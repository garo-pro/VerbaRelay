//! The transcription worker process.
//!
//! All native code (whisper.cpp, GPU drivers) runs in a separate process started
//! from the same executable (`verbarelay-client worker ...`). If a driver
//! crashes, only the worker dies; the client stays connected, reports the failure
//! and starts a new worker.
//!
//! Parent and worker talk over stdin/stdout with frames of
//! `[kind: u8][length: u32 LE][payload]`, kind 1 = JSON, kind 2 = PCM bytes.
//! Audio is passed through the pipe only and never written to disk.

use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::Arc;
use std::sync::atomic::{AtomicI32, Ordering};
use std::time::{Duration, Instant};

use anyhow::{Context, bail};
use serde::{Deserialize, Serialize};
use tokio::io::{AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, ChildStdin, ChildStdout};
use tokio::sync::Mutex;
use tokio_util::sync::CancellationToken;
use tracing::{debug, info, warn};
use verbarelay_protocol::frames::bytes_to_pcm;
use verbarelay_protocol::messages::Backend;
use verbarelay_protocol::transcript::Segment;

use crate::engine::{Engine, EngineDevice};

const KIND_JSON: u8 = 1;
const KIND_PCM: u8 = 2;
const MAX_FRAME: usize = 1 << 31;

/// Requests from the parent.
#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum WorkerRequest {
    /// A PCM frame with `samples` 16-bit samples follows.
    Transcribe {
        language: Option<String>,
        translate: bool,
        samples: u64,
    },
    Cancel,
    Shutdown,
}

/// Events from the worker.
#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "event", rename_all = "snake_case")]
pub enum WorkerEvent {
    Ready { whisper_version: String, load_ms: u64 },
    Segment { start_ms: u64, end_ms: u64, text: String },
    Done { language: String, processing_ms: u64 },
    Error { message: String },
}

// ---------------------------------------------------------------------------
// Worker side (runs in the child process, synchronous)
// ---------------------------------------------------------------------------

fn write_frame(out: &mut impl Write, kind: u8, payload: &[u8]) -> std::io::Result<()> {
    out.write_all(&[kind])?;
    out.write_all(&(payload.len() as u32).to_le_bytes())?;
    out.write_all(payload)?;
    out.flush()
}

fn read_frame(input: &mut impl Read) -> std::io::Result<(u8, Vec<u8>)> {
    let mut header = [0u8; 5];
    input.read_exact(&mut header)?;
    let len = u32::from_le_bytes(header[1..5].try_into().expect("4 bytes")) as usize;
    if len > MAX_FRAME {
        return Err(std::io::Error::new(std::io::ErrorKind::InvalidData, "frame too large"));
    }
    let mut payload = vec![0u8; len];
    input.read_exact(&mut payload)?;
    Ok((header[0], payload))
}

fn emit(event: &WorkerEvent) {
    let json = serde_json::to_vec(event).expect("worker events serialize");
    let mut out = std::io::stdout().lock();
    if write_frame(&mut out, KIND_JSON, &json).is_err() {
        std::process::exit(0); // Parent is gone.
    }
}

enum Incoming {
    Job {
        language: Option<String>,
        translate: bool,
        pcm: Vec<i16>,
    },
    Shutdown,
}

/// Entry point of `verbarelay-client worker`.
pub fn run_worker(install_dir: &Path, model_path: &Path, backend: Backend, threads: u32) -> anyhow::Result<()> {
    let started = Instant::now();
    let engine = match Engine::load(install_dir) {
        Ok(e) => e,
        Err(e) => {
            emit(&WorkerEvent::Error {
                message: format!("{e:#}"),
            });
            return Err(e);
        }
    };
    let model = match engine.load_model(model_path, backend) {
        Ok(m) => m,
        Err(e) => {
            emit(&WorkerEvent::Error {
                message: format!("{e:#}"),
            });
            return Err(e);
        }
    };
    emit(&WorkerEvent::Ready {
        whisper_version: engine.whisper_version(),
        load_ms: started.elapsed().as_millis() as u64,
    });

    // A reader thread receives jobs and cancel requests while the main thread transcribes.
    let abort = Arc::new(AtomicI32::new(0));
    let (jobs_tx, jobs_rx) = std::sync::mpsc::channel::<Incoming>();
    let reader_abort = abort.clone();
    std::thread::spawn(move || {
        let mut stdin = std::io::stdin().lock();
        loop {
            let Ok((kind, payload)) = read_frame(&mut stdin) else {
                let _ = jobs_tx.send(Incoming::Shutdown);
                return;
            };
            if kind != KIND_JSON {
                continue;
            }
            match serde_json::from_slice::<WorkerRequest>(&payload) {
                Ok(WorkerRequest::Transcribe {
                    language,
                    translate,
                    samples,
                }) => {
                    let Ok((KIND_PCM, pcm)) = read_frame(&mut stdin) else {
                        let _ = jobs_tx.send(Incoming::Shutdown);
                        return;
                    };
                    let pcm = bytes_to_pcm(&pcm);
                    if pcm.len() as u64 != samples {
                        emit(&WorkerEvent::Error {
                            message: "sample count mismatch".into(),
                        });
                        continue;
                    }
                    reader_abort.store(0, Ordering::SeqCst);
                    let _ = jobs_tx.send(Incoming::Job {
                        language,
                        translate,
                        pcm,
                    });
                }
                Ok(WorkerRequest::Cancel) => reader_abort.store(1, Ordering::SeqCst),
                Ok(WorkerRequest::Shutdown) | Err(_) => {
                    reader_abort.store(1, Ordering::SeqCst);
                    let _ = jobs_tx.send(Incoming::Shutdown);
                    return;
                }
            }
        }
    });

    while let Ok(Incoming::Job {
        language,
        translate,
        pcm,
    }) = jobs_rx.recv()
    {
        let samples: Vec<f32> = pcm.iter().map(|&s| f32::from(s) / 32768.0).collect();
        drop(pcm);
        let started = Instant::now();
        let mut on_segment =
            |start_ms: u64, end_ms: u64, text: String| emit(&WorkerEvent::Segment { start_ms, end_ms, text });
        match model.transcribe(
            &samples,
            language.as_deref(),
            translate,
            threads,
            &abort,
            &mut on_segment,
        ) {
            Ok(language) => emit(&WorkerEvent::Done {
                language,
                processing_ms: started.elapsed().as_millis() as u64,
            }),
            Err(e) => emit(&WorkerEvent::Error {
                message: format!("{e:#}"),
            }),
        }
    }
    Ok(())
}

/// Entry point of `verbarelay-client detect-devices`: prints the devices as JSON.
pub fn run_detect(install_dir: &Path) -> anyhow::Result<()> {
    let engine = Engine::load(install_dir)?;
    println!("{}", serde_json::to_string(&engine.devices()?)?);
    Ok(())
}

// ---------------------------------------------------------------------------
// Parent side (async)
// ---------------------------------------------------------------------------

/// Starts a hidden child process of this executable.
fn command(args: &[&str]) -> anyhow::Result<tokio::process::Command> {
    let exe = std::env::current_exe()?;
    let mut cmd = tokio::process::Command::new(exe);
    cmd.args(args);
    #[cfg(windows)]
    {
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        cmd.creation_flags(CREATE_NO_WINDOW);
    }
    Ok(cmd)
}

/// Lists the compute devices in a child process (a crashing driver cannot hurt us).
pub async fn detect_devices() -> anyhow::Result<Vec<EngineDevice>> {
    let output = tokio::time::timeout(
        Duration::from_secs(60),
        command(&["detect-devices"])?
            .stdin(Stdio::null())
            .kill_on_drop(true)
            .output(),
    )
    .await
    .context("device detection timed out")??;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        bail!(
            "device detection failed: {}",
            stderr.lines().last().unwrap_or("unknown error")
        );
    }
    let stdout = String::from_utf8_lossy(&output.stdout);
    let json = stdout
        .lines()
        .rev()
        .find(|l| l.trim_start().starts_with('['))
        .context("no device list received")?;
    Ok(serde_json::from_str(json)?)
}

/// Result of one transcription in the worker.
#[derive(Debug)]
pub struct WorkerOutput {
    pub language: String,
    pub processing_ms: u64,
    pub segments: Vec<Segment>,
}

/// Errors of a worker run.
#[derive(Debug, thiserror::Error)]
pub enum WorkerError {
    #[error("cancelled")]
    Cancelled,
    #[error("the worker process stopped unexpectedly")]
    Crashed,
    #[error("{0}")]
    Failed(String),
}

/// A running worker with a loaded model.
pub struct WorkerHandle {
    child: Child,
    stdin: Arc<Mutex<ChildStdin>>,
    stdout: BufReader<ChildStdout>,
    pub backend: Backend,
    pub model_path: PathBuf,
}

async fn write_frame_async(stdin: &Mutex<ChildStdin>, kind: u8, payload: &[u8]) -> std::io::Result<()> {
    let mut stdin = stdin.lock().await;
    stdin.write_all(&[kind]).await?;
    stdin.write_all(&(payload.len() as u32).to_le_bytes()).await?;
    stdin.write_all(payload).await?;
    stdin.flush().await
}

async fn read_event(stdout: &mut BufReader<ChildStdout>) -> Result<WorkerEvent, WorkerError> {
    loop {
        let mut header = [0u8; 5];
        stdout.read_exact(&mut header).await.map_err(|_| WorkerError::Crashed)?;
        let len = u32::from_le_bytes(header[1..5].try_into().expect("4 bytes")) as usize;
        if len > MAX_FRAME {
            return Err(WorkerError::Crashed);
        }
        let mut payload = vec![0u8; len];
        stdout
            .read_exact(&mut payload)
            .await
            .map_err(|_| WorkerError::Crashed)?;
        if header[0] == KIND_JSON {
            return serde_json::from_slice(&payload)
                .map_err(|e| WorkerError::Failed(format!("invalid worker message: {e}")));
        }
    }
}

impl WorkerHandle {
    /// Starts a worker and waits until the model is loaded.
    pub async fn start(model_path: &Path, backend: Backend, threads: u32) -> anyhow::Result<Self> {
        let model = model_path.to_string_lossy().to_string();
        let threads_arg = threads.to_string();
        let mut child = command(&[
            "worker",
            "--model",
            &model,
            "--backend",
            backend.as_str(),
            "--threads",
            &threads_arg,
        ])?
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .context("starting the worker process")?;
        let stdin = Arc::new(Mutex::new(child.stdin.take().context("worker stdin")?));
        let mut stdout = BufReader::new(child.stdout.take().context("worker stdout")?);

        // Native log lines (warnings from whisper.cpp / ggml) go to our log.
        if let Some(stderr) = child.stderr.take() {
            tokio::spawn(async move {
                use tokio::io::AsyncBufReadExt;
                let mut lines = BufReader::new(stderr).lines();
                while let Ok(Some(line)) = lines.next_line().await {
                    if !line.trim().is_empty() {
                        debug!(target: "worker", "{line}");
                    }
                }
            });
        }

        match tokio::time::timeout(Duration::from_secs(300), read_event(&mut stdout)).await {
            Ok(Ok(WorkerEvent::Ready {
                whisper_version,
                load_ms,
            })) => {
                info!(%backend, model = %model, load_ms, whisper = %whisper_version, "worker ready");
            }
            Ok(Ok(WorkerEvent::Error { message })) => bail!("{message}"),
            Ok(Ok(other)) => bail!("unexpected worker message: {other:?}"),
            Ok(Err(e)) => bail!("{e}"),
            Err(_) => bail!("loading the model took too long"),
        }
        Ok(Self {
            child,
            stdin,
            stdout,
            backend,
            model_path: model_path.to_path_buf(),
        })
    }

    /// Transcribes audio, reporting segments as they come.
    pub async fn transcribe(
        &mut self,
        pcm: &[i16],
        language: Option<String>,
        translate: bool,
        cancel: &CancellationToken,
        mut on_segment: impl FnMut(Segment),
    ) -> Result<WorkerOutput, WorkerError> {
        let request = WorkerRequest::Transcribe {
            language,
            translate,
            samples: pcm.len() as u64,
        };
        let json = serde_json::to_vec(&request).expect("requests serialize");
        let bytes = zeroize::Zeroizing::new(verbarelay_protocol::frames::pcm_to_bytes(pcm));
        write_frame_async(&self.stdin, KIND_JSON, &json)
            .await
            .map_err(|_| WorkerError::Crashed)?;
        write_frame_async(&self.stdin, KIND_PCM, &bytes)
            .await
            .map_err(|_| WorkerError::Crashed)?;
        drop(bytes);

        let mut segments = Vec::new();
        let mut cancel_sent = false;
        loop {
            let event = tokio::select! {
                event = read_event(&mut self.stdout) => event?,
                () = cancel.cancelled(), if !cancel_sent => {
                    cancel_sent = true;
                    let json = serde_json::to_vec(&WorkerRequest::Cancel).expect("requests serialize");
                    write_frame_async(&self.stdin, KIND_JSON, &json).await.map_err(|_| WorkerError::Crashed)?;
                    continue;
                }
            };
            match event {
                WorkerEvent::Segment { start_ms, end_ms, text } => {
                    let segment = Segment { start_ms, end_ms, text };
                    on_segment(segment.clone());
                    segments.push(segment);
                }
                WorkerEvent::Done {
                    language,
                    processing_ms,
                } => {
                    if cancel_sent {
                        return Err(WorkerError::Cancelled);
                    }
                    return Ok(WorkerOutput {
                        language,
                        processing_ms,
                        segments,
                    });
                }
                WorkerEvent::Error { message } => {
                    return Err(if cancel_sent {
                        WorkerError::Cancelled
                    } else {
                        WorkerError::Failed(message)
                    });
                }
                WorkerEvent::Ready { .. } => {}
            }
        }
    }

    /// Stops the worker.
    pub async fn shutdown(mut self) {
        let json = serde_json::to_vec(&WorkerRequest::Shutdown).expect("requests serialize");
        let _ = write_frame_async(&self.stdin, KIND_JSON, &json).await;
        if tokio::time::timeout(Duration::from_secs(5), self.child.wait())
            .await
            .is_err()
        {
            warn!("worker did not stop; killing it");
            let _ = self.child.kill().await;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn frames_round_trip() {
        let mut buf = Vec::new();
        write_frame(&mut buf, KIND_JSON, b"{\"op\":\"cancel\"}").unwrap();
        let (kind, payload) = read_frame(&mut buf.as_slice()).unwrap();
        assert_eq!(kind, KIND_JSON);
        let request: WorkerRequest = serde_json::from_slice(&payload).unwrap();
        assert!(matches!(request, WorkerRequest::Cancel));
    }
}
