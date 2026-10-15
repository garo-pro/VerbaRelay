//! The background peer: keeps the connection to the server and does the work.
//!
//! One job at a time: while a job (or a benchmark) runs, offers are declined
//! with "busy". Audio arrives encrypted for this device only, is decrypted into
//! memory, handed to the worker process and wiped afterwards. Live segments and
//! the final result go back encrypted with the job's result key.

use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::Context;
use futures_util::{SinkExt, StreamExt};
use tokio::sync::{Mutex, mpsc, oneshot, watch};
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::tungstenite::http::{HeaderValue, StatusCode};
use tokio_tungstenite::tungstenite::protocol::WebSocketConfig;
use tokio_tungstenite::tungstenite::{Error as WsError, Message};
use tokio_util::sync::CancellationToken;
use tracing::{debug, error, info, warn};
use uuid::Uuid;
use verbarelay_protocol::PROTOCOL_VERSION;
use verbarelay_protocol::crypto::{self, PeerKeypair, SessionKeys};
use verbarelay_protocol::frames::{AudioChunk, bytes_to_pcm};
use verbarelay_protocol::messages::{
    Backend, BenchmarkResult, DECLINE_BUSY, DECLINE_MODEL_MISSING, ModelPolicy, PeerMessage, ServerMessage,
    encode_sealed,
};
use verbarelay_protocol::models::ModelSpec;
use verbarelay_protocol::transcript::TranscriptResult;
use zeroize::Zeroizing;

use crate::config::{ClientConfig, Paths, UpdateSettings};
use crate::engine::EngineDevice;
use crate::ipc::{self, SharedStatus};
use crate::models::ModelStore;
use crate::updater::StagedUpdate;
use crate::worker::{WorkerError, WorkerHandle};

/// Why the agent stopped.
#[derive(Debug)]
pub enum AgentExit {
    /// The token is invalid or was revoked: setup must run again.
    NeedsEnrollment,
    /// An update was applied; restart the executable.
    Restart,
}

/// How a single connection ended.
enum SessionEnd {
    Disconnected(String),
    Reconnect(String),
    Unauthorized,
    Exit(AgentExit),
}

/// Consecutive worker failures on a GPU backend before falling back to the CPU.
const MAX_GPU_FAILURES: u32 = 3;
const PING_INTERVAL: Duration = Duration::from_secs(30);
const IDLE_TIMEOUT: Duration = Duration::from_secs(90);

enum Internal {
    JobDone {
        job_id: Uuid,
        worker_failed: bool,
        succeeded: bool,
    },
    ModelReady(String),
    ModelFailed(String),
    BenchmarkDone(anyhow::Result<BenchmarkResult>),
}

/// Decrypted audio and the job's keys, handed from the connection to the job task.
type AudioDelivery = (Zeroizing<Vec<i16>>, Arc<SessionKeys>);

struct CurrentJob {
    id: Uuid,
    cancel: CancellationToken,
    audio_tx: Option<oneshot::Sender<AudioDelivery>>,
    assembly: Option<AudioAssembly>,
}

struct AudioAssembly {
    keys: Arc<SessionKeys>,
    chunk_count: u32,
    sample_count: u64,
    next_seq: u32,
    buffer: Zeroizing<Vec<u8>>,
}

pub struct Agent {
    paths: Paths,
    config: ClientConfig,
    keys: PeerKeypair,
    store: ModelStore,
    http: reqwest::Client,
    status: SharedStatus,
    worker: Arc<Mutex<Option<WorkerHandle>>>,
    devices: Vec<EngineDevice>,
    gpu_failures: u32,
    settings_tx: watch::Sender<UpdateSettings>,
    staged_rx: mpsc::Receiver<StagedUpdate>,
    staged: Option<StagedUpdate>,
    commands_rx: mpsc::Receiver<ipc::Command>,
}

impl Agent {
    pub fn new(
        paths: Paths,
        config: ClientConfig,
        status: SharedStatus,
        settings_tx: watch::Sender<UpdateSettings>,
        staged_rx: mpsc::Receiver<StagedUpdate>,
        commands_rx: mpsc::Receiver<ipc::Command>,
    ) -> anyhow::Result<Self> {
        let keys = paths.load_or_create_keypair()?;
        Ok(Self {
            store: ModelStore::new(paths.models_dir.clone()),
            http: crate::net::download_client()?,
            paths,
            config,
            keys,
            status,
            worker: Arc::new(Mutex::new(None)),
            devices: Vec::new(),
            gpu_failures: 0,
            settings_tx,
            staged_rx,
            staged: None,
            commands_rx,
        })
    }

    fn set_status(&self, f: impl FnOnce(&mut ipc::AgentStatus)) {
        f(&mut self.status.lock().unwrap());
    }

    /// Runs until the agent has to stop. Reconnects with backoff.
    pub async fn run(mut self) -> AgentExit {
        if self.config.backend == Backend::Cuda && !crate::packages::cuda_installed(&self.paths.install_dir) {
            info!("the CUDA package is missing; installing it");
            if let Err(e) = crate::packages::install_cuda(&self.paths.install_dir, &self.paths.data_dir).await {
                warn!("installing the CUDA package failed: {e:#}");
            }
        }
        self.devices = crate::worker::detect_devices().await.unwrap_or_else(|e| {
            warn!("device detection failed: {e:#}");
            Vec::new()
        });

        let mut backoff = Duration::from_secs(1);
        loop {
            let started = Instant::now();
            let end = self.session().await;
            self.set_status(|s| s.connected = false);
            match end {
                SessionEnd::Exit(exit) => {
                    self.stop_worker().await;
                    return exit;
                }
                SessionEnd::Unauthorized => {
                    error!("the server no longer accepts this device's token; setup has to run again");
                    self.stop_worker().await;
                    return AgentExit::NeedsEnrollment;
                }
                SessionEnd::Reconnect(reason) => {
                    info!(%reason, "reconnecting");
                    backoff = Duration::from_secs(1);
                    continue;
                }
                SessionEnd::Disconnected(reason) => {
                    if started.elapsed() > Duration::from_secs(60) {
                        backoff = Duration::from_secs(1);
                    }
                    warn!(%reason, retry_in = backoff.as_secs(), "connection lost");
                    self.set_status(|s| s.last_error = Some(reason));
                }
            }
            // Wait, but keep handling commands and updates meanwhile.
            let deadline = tokio::time::sleep(backoff);
            tokio::pin!(deadline);
            loop {
                tokio::select! {
                    () = &mut deadline => break,
                    Some(cmd) = self.commands_rx.recv() => { let _ = self.handle_command(cmd); }
                    Some(staged) = self.staged_rx.recv() => {
                        // Not connected means not working: apply right away.
                        if let Some(exit) = self.apply_update(staged).await {
                            return exit;
                        }
                    }
                }
            }
            backoff = (backoff * 2).min(Duration::from_secs(60));
        }
    }

    async fn stop_worker(&self) {
        if let Some(worker) = self.worker.lock().await.take() {
            worker.shutdown().await;
        }
    }

    /// Reloads the configuration. Returns a reason to reconnect if needed.
    fn handle_command(&mut self, command: ipc::Command) -> Option<String> {
        match command {
            ipc::Command::Reload => match ClientConfig::load(&self.paths.config_file) {
                Ok(new) => {
                    let reconnect = new.server_url != self.config.server_url
                        || new.backend != self.config.backend
                        || new.device_name != self.config.device_name
                        || new.credentials.as_ref().map(|c| &c.token)
                            != self.config.credentials.as_ref().map(|c| &c.token);
                    let _ = self.settings_tx.send(new.updates.clone());
                    info!("configuration reloaded");
                    self.config = new;
                    reconnect.then(|| "configuration changed".to_string())
                }
                Err(e) => {
                    warn!("reloading the configuration failed: {e:#}");
                    None
                }
            },
        }
    }

    async fn apply_update(&mut self, staged: StagedUpdate) -> Option<AgentExit> {
        self.stop_worker().await;
        match crate::updater::apply(&staged, &self.paths.install_dir) {
            Ok(()) => Some(AgentExit::Restart),
            Err(e) => {
                error!("applying the update failed: {e:#}");
                None
            }
        }
    }

    fn model_names(&self, policy: &ModelPolicy) -> Vec<String> {
        self.store.available(&policy.allowed)
    }

    /// The model this device uses when the server does not dictate one.
    fn own_model(&self, policy: &ModelPolicy) -> Option<ModelSpec> {
        policy
            .allowed
            .iter()
            .find(|m| m.name == self.config.model)
            .cloned()
            .or_else(|| verbarelay_protocol::models::find(&self.config.model))
    }

    fn start_download(&self, spec: ModelSpec, internal: &mpsc::UnboundedSender<Internal>) {
        if self.store.has(&spec) || !self.store.claim(&spec.name) {
            return;
        }
        info!(model = %spec.name, size_mb = spec.size / 1_000_000, "downloading model");
        let store = self.store.clone();
        let client = self.http.clone();
        let internal = internal.clone();
        tokio::spawn(async move {
            match store.download_claimed(&client, &spec).await {
                Ok(_) => {
                    let _ = internal.send(Internal::ModelReady(spec.name));
                }
                Err(e) => {
                    warn!(model = %spec.name, "model download failed: {e:#}");
                    let _ = internal.send(Internal::ModelFailed(spec.name));
                }
            }
        });
    }

    async fn session(&mut self) -> SessionEnd {
        let Some(credentials) = self.config.credentials.clone() else {
            return SessionEnd::Unauthorized;
        };
        let url = match self.config.connect_url() {
            Ok(u) => u,
            Err(e) => return SessionEnd::Disconnected(format!("{e:#}")),
        };
        let mut request = match url.as_str().into_client_request() {
            Ok(r) => r,
            Err(e) => return SessionEnd::Disconnected(e.to_string()),
        };
        let Ok(auth) = HeaderValue::from_str(&format!("Bearer {}", credentials.token)) else {
            return SessionEnd::Unauthorized;
        };
        request.headers_mut().insert("Authorization", auth);
        if let Ok(ua) = HeaderValue::from_str(&crate::net::user_agent()) {
            request.headers_mut().insert("User-Agent", ua);
        }
        let tls = match crate::net::tls_config(self.config.ca_certificate.as_deref()) {
            Ok(t) => t,
            Err(e) => return SessionEnd::Disconnected(format!("{e:#}")),
        };
        let mut ws_config = WebSocketConfig::default();
        ws_config.max_message_size = Some(8 << 20);
        ws_config.max_frame_size = Some(8 << 20);
        let connect = tokio_tungstenite::connect_async_tls_with_config(
            request,
            Some(ws_config),
            false,
            Some(tokio_tungstenite::Connector::Rustls(tls)),
        );
        let (socket, _) = match tokio::time::timeout(Duration::from_secs(30), connect).await {
            Err(_) => return SessionEnd::Disconnected("connecting timed out".into()),
            Ok(Err(WsError::Http(response))) if response.status() == StatusCode::UNAUTHORIZED => {
                return SessionEnd::Unauthorized;
            }
            Ok(Err(e)) => return SessionEnd::Disconnected(e.to_string()),
            Ok(Ok(s)) => s,
        };
        let (mut sink, mut stream) = socket.split();

        // Hello / welcome.
        let device = crate::detect::device_info(&self.devices, self.config.backend);
        let accelerator = device.accelerator.clone();
        let hello = PeerMessage::Hello {
            protocol_version: PROTOCOL_VERSION,
            client_version: env!("CARGO_PKG_VERSION").to_string(),
            device,
            models: self.store.available(&[]),
            busy: false,
        };
        if sink.send(json_message(&hello)).await.is_err() {
            return SessionEnd::Disconnected("sending hello failed".into());
        }
        let welcome = match tokio::time::timeout(Duration::from_secs(15), stream.next()).await {
            Ok(Some(Ok(Message::Text(text)))) => serde_json::from_str::<ServerMessage>(&text).ok(),
            Ok(Some(Ok(Message::Close(frame)))) => {
                return SessionEnd::Disconnected(format!("server closed the connection: {frame:?}"));
            }
            _ => None,
        };
        let Some(ServerMessage::Welcome {
            peer_id,
            server_version,
            policy,
            benchmark,
        }) = welcome
        else {
            return SessionEnd::Disconnected("no welcome from the server".into());
        };
        info!(%peer_id, %server_version, backend = %self.config.backend, %accelerator, "connected to the server");
        let server = self.config.server_url.clone();
        let backend = self.config.backend.to_string();
        self.set_status(|s| {
            s.connected = true;
            s.server = server;
            s.backend = backend;
            s.accelerator = accelerator;
            s.last_error = None;
            s.model = policy.enforced.as_ref().map(|m| m.name.clone());
        });

        // Outgoing messages go through one channel so job tasks can send too.
        let (out_tx, mut out_rx) = mpsc::unbounded_channel::<Message>();
        let writer = tokio::spawn(async move {
            while let Some(message) = out_rx.recv().await {
                let closing = matches!(message, Message::Close(_));
                if sink.send(message).await.is_err() || closing {
                    break;
                }
            }
            let _ = sink.close().await;
        });

        let (internal_tx, mut internal_rx) = mpsc::unbounded_channel::<Internal>();
        let mut policy = policy;
        let mut current: Option<CurrentJob> = None;
        let mut benchmarking = false;
        let mut pending_benchmark = benchmark;

        // Make sure the model is (being) downloaded.
        let needed = policy.enforced.clone().or_else(|| self.own_model(&policy));
        if let Some(spec) = needed {
            self.start_download(spec, &internal_tx);
        }

        let mut ping = tokio::time::interval(PING_INTERVAL);
        let end = loop {
            // Start a pending benchmark when idle and the model is there.
            if !benchmarking
                && current.is_none()
                && let Some(request) = pending_benchmark.clone()
            {
                let spec = request.model.clone().or_else(|| self.own_model(&policy));
                match spec {
                    Some(spec) if self.store.has(&spec) => {
                        pending_benchmark = None;
                        benchmarking = true;
                        self.set_status(|s| s.working = true);
                        self.spawn_benchmark(spec, internal_tx.clone());
                    }
                    Some(spec) => self.start_download(spec, &internal_tx),
                    None => pending_benchmark = None,
                }
            }
            // Apply a staged update as soon as nothing is running.
            if self.staged.is_some() && !benchmarking && current.is_none() {
                let _ = out_tx.send(Message::Close(None));
                break SessionEnd::Exit(AgentExit::Restart);
            }

            tokio::select! {
                incoming = tokio::time::timeout(IDLE_TIMEOUT, stream.next()) => {
                    let message = match incoming {
                        Err(_) => break SessionEnd::Disconnected("the server stopped responding".into()),
                        Ok(None) => break SessionEnd::Disconnected("the server closed the connection".into()),
                        Ok(Some(Err(e))) => break SessionEnd::Disconnected(e.to_string()),
                        Ok(Some(Ok(m))) => m,
                    };
                    match message {
                        Message::Text(text) => {
                            let msg = match serde_json::from_str::<ServerMessage>(&text) {
                                Ok(m) => m,
                                Err(e) => { warn!("invalid server message: {e}"); continue; }
                            };
                            match msg {
                                ServerMessage::Offer { job_id, model, language, translate, audio_ms } => {
                                    let busy = current.is_some() || benchmarking || self.staged.is_some();
                                    let spec = model.or_else(|| self.own_model(&policy));
                                    let reply = |accept: bool, reason: Option<&str>| PeerMessage::OfferReply {
                                        job_id, accept, reason: reason.map(str::to_string),
                                    };
                                    if busy {
                                        let _ = out_tx.send(json_message(&reply(false, Some(DECLINE_BUSY))));
                                        continue;
                                    }
                                    let Some(spec) = spec else {
                                        let _ = out_tx.send(json_message(&reply(false, Some("no model configured"))));
                                        continue;
                                    };
                                    if !self.store.has(&spec) {
                                        let _ = out_tx.send(json_message(&reply(false, Some(DECLINE_MODEL_MISSING))));
                                        self.start_download(spec, &internal_tx);
                                        continue;
                                    }
                                    let _ = out_tx.send(json_message(&reply(true, None)));
                                    debug!(%job_id, audio_ms, model = %spec.name, "job accepted");
                                    let cancel = CancellationToken::new();
                                    let (audio_tx, audio_rx) = oneshot::channel();
                                    current = Some(CurrentJob { id: job_id, cancel: cancel.clone(), audio_tx: Some(audio_tx), assembly: None });
                                    self.set_status(|s| s.working = true);
                                    self.spawn_job(JobSpec {
                                        job_id,
                                        model_path: self.store.path_of(&spec),
                                        language,
                                        translate,
                                        cancel,
                                        audio_rx,
                                        out_tx: out_tx.clone(),
                                        internal_tx: internal_tx.clone(),
                                    });
                                }
                                ServerMessage::Audio { job_id, ephemeral_public_key, chunk_count, sample_count } => {
                                    let Some(job) = current.as_mut().filter(|j| j.id == job_id) else {
                                        warn!(%job_id, "audio for an unknown job");
                                        continue;
                                    };
                                    let keys = crypto::decode_key(&ephemeral_public_key)
                                        .map_err(anyhow::Error::from)
                                        .and_then(|eph| self.keys.open_session(eph, job_id).map_err(anyhow::Error::from));
                                    match keys {
                                        Ok(keys) => {
                                            let capacity = usize::try_from(sample_count.saturating_mul(2)).unwrap_or(0).min(1 << 31);
                                            job.assembly = Some(AudioAssembly {
                                                keys: Arc::new(keys),
                                                chunk_count,
                                                sample_count,
                                                next_seq: 0,
                                                buffer: Zeroizing::new(Vec::with_capacity(capacity)),
                                            });
                                        }
                                        Err(e) => fail_job(&out_tx, job, &format!("key exchange failed: {e:#}")),
                                    }
                                }
                                ServerMessage::Cancel { job_id } => {
                                    if let Some(job) = current.as_ref().filter(|j| j.id == job_id) {
                                        info!(%job_id, "job cancelled by the server");
                                        job.cancel.cancel();
                                    }
                                }
                                ServerMessage::RunBenchmark { request } => pending_benchmark = Some(request),
                                ServerMessage::Revoked => break SessionEnd::Unauthorized,
                                ServerMessage::Welcome { policy: new_policy, .. } => policy = new_policy,
                            }
                        }
                        Message::Binary(frame) => {
                            let Some(chunk) = AudioChunk::decode(&frame) else { continue };
                            let Some(job) = current.as_mut().filter(|j| j.id == chunk.job_id) else { continue };
                            if let Err(e) = receive_chunk(job, chunk) {
                                fail_job(&out_tx, job, &e);
                            }
                        }
                        Message::Close(frame) => break SessionEnd::Disconnected(format!("closed by the server: {frame:?}")),
                        _ => {}
                    }
                }
                Some(event) = internal_rx.recv() => match event {
                    Internal::JobDone { job_id, worker_failed, succeeded } => {
                        if current.as_ref().is_some_and(|j| j.id == job_id) {
                            current = None;
                        }
                        self.set_status(|s| { s.working = benchmarking; if succeeded { s.jobs_done += 1; } });
                        if let Some(end) = self.track_worker_health(worker_failed, succeeded) {
                            break end;
                        }
                    }
                    Internal::ModelReady(name) => {
                        info!(model = %name, "model ready");
                        let _ = out_tx.send(json_message(&PeerMessage::Models { models: self.model_names(&policy) }));
                    }
                    Internal::ModelFailed(name) => {
                        self.set_status(|s| s.last_error = Some(format!("downloading model {name} failed")));
                    }
                    Internal::BenchmarkDone(result) => {
                        benchmarking = false;
                        self.set_status(|s| s.working = current.is_some());
                        match result {
                            Ok(result) => {
                                let speed = result.speed();
                                info!(model = %result.model, backend = %result.backend, speed = format!("{speed:.1}"), "benchmark finished");
                                self.set_status(|s| s.score = Some(speed));
                                let _ = out_tx.send(json_message(&PeerMessage::Benchmark { result }));
                                if let Some(end) = self.track_worker_health(false, true) { break end; }
                            }
                            Err(e) => {
                                warn!("benchmark failed: {e:#}");
                                if let Some(end) = self.track_worker_health(true, false) { break end; }
                            }
                        }
                    }
                },
                Some(command) = self.commands_rx.recv() => {
                    if let Some(reason) = self.handle_command(command) {
                        if let Some(job) = &current { job.cancel.cancel(); }
                        break SessionEnd::Reconnect(reason);
                    }
                }
                Some(staged) = self.staged_rx.recv() => {
                    info!(update = %staged.description, "update ready; it is installed when the current work is done");
                    let description = staged.description.clone();
                    self.set_status(|s| s.pending_update = Some(description));
                    self.staged = Some(staged);
                }
                _ = ping.tick() => {
                    let _ = out_tx.send(Message::Ping(Vec::new().into()));
                }
            }
        };

        if let Some(job) = current {
            job.cancel.cancel();
        }
        drop(out_tx);
        let _ = tokio::time::timeout(Duration::from_secs(5), writer).await;

        if matches!(end, SessionEnd::Exit(AgentExit::Restart))
            && let Some(staged) = self.staged.take()
        {
            return match self.apply_update(staged).await {
                Some(exit) => SessionEnd::Exit(exit),
                None => SessionEnd::Reconnect("update failed".into()),
            };
        }
        end
    }

    /// Counts worker failures; after several on a GPU backend, falls back to the CPU.
    fn track_worker_health(&mut self, failed: bool, succeeded: bool) -> Option<SessionEnd> {
        if succeeded {
            self.gpu_failures = 0;
            return None;
        }
        if !failed || self.config.backend == Backend::Cpu {
            return None;
        }
        self.gpu_failures += 1;
        if self.gpu_failures < MAX_GPU_FAILURES {
            return None;
        }
        warn!(backend = %self.config.backend, failures = self.gpu_failures, "the GPU backend keeps failing; switching to the CPU");
        self.config.backend = Backend::Cpu;
        self.gpu_failures = 0;
        if let Err(e) = self.config.save(&self.paths.config_file) {
            warn!("saving the configuration failed: {e:#}");
        }
        self.set_status(|s| s.last_error = Some("the GPU kept failing; switched to the CPU".into()));
        Some(SessionEnd::Reconnect("backend changed to CPU".into()))
    }

    fn spawn_benchmark(&self, spec: ModelSpec, internal: mpsc::UnboundedSender<Internal>) {
        let worker = self.worker.clone();
        let path = self.store.path_of(&spec);
        let backend = self.config.backend;
        let threads = self.config.threads;
        tokio::spawn(async move {
            let mut guard = worker.lock().await;
            let result = match ensure_worker(&mut guard, &path, backend, threads).await {
                Ok(handle) => crate::benchmark::measure(handle, &spec.name).await,
                Err(e) => Err(e),
            };
            if result.is_err() {
                // A failed run may have left the worker in a bad state.
                if let Some(w) = guard.take() {
                    w.shutdown().await;
                }
            }
            drop(guard);
            let _ = internal.send(Internal::BenchmarkDone(result));
        });
    }

    fn spawn_job(&self, job: JobSpec) {
        let worker = self.worker.clone();
        let backend = self.config.backend;
        let threads = self.config.threads;
        tokio::spawn(run_job(job, worker, backend, threads));
    }
}

struct JobSpec {
    job_id: Uuid,
    model_path: std::path::PathBuf,
    language: Option<String>,
    translate: bool,
    cancel: CancellationToken,
    audio_rx: oneshot::Receiver<AudioDelivery>,
    out_tx: mpsc::UnboundedSender<Message>,
    internal_tx: mpsc::UnboundedSender<Internal>,
}

fn json_message<T: serde::Serialize>(msg: &T) -> Message {
    Message::Text(serde_json::to_string(msg).expect("messages serialize").into())
}

fn fail_job(out_tx: &mpsc::UnboundedSender<Message>, job: &mut CurrentJob, error: &str) {
    warn!(job_id = %job.id, %error, "job failed");
    let _ = out_tx.send(json_message(&PeerMessage::JobFailed {
        job_id: job.id,
        error: error.to_string(),
        retryable: true,
    }));
    job.cancel.cancel();
    job.assembly = None;
    job.audio_tx = None;
}

/// Decrypts one audio chunk; hands the audio to the job when complete.
fn receive_chunk(job: &mut CurrentJob, chunk: AudioChunk) -> Result<(), String> {
    let assembly = job.assembly.as_mut().ok_or("audio chunk before the audio header")?;
    if chunk.seq != assembly.next_seq || chunk.seq >= assembly.chunk_count {
        return Err(format!("unexpected audio chunk {}", chunk.seq));
    }
    let expected_last = chunk.seq + 1 == assembly.chunk_count;
    if chunk.last != expected_last {
        return Err("audio chunk with a wrong end marker".into());
    }
    let plain = Zeroizing::new(
        assembly
            .keys
            .open_audio(chunk.seq, chunk.last, &chunk.ciphertext)
            .map_err(|e| format!("decrypting audio failed: {e}"))?,
    );
    assembly.buffer.extend_from_slice(&plain);
    assembly.next_seq += 1;
    if chunk.last {
        let assembly = job.assembly.take().expect("checked above");
        let pcm = Zeroizing::new(bytes_to_pcm(&assembly.buffer));
        if pcm.len() as u64 != assembly.sample_count {
            return Err("audio length does not match the header".into());
        }
        if let Some(tx) = job.audio_tx.take() {
            let _ = tx.send((pcm, assembly.keys));
        }
    }
    Ok(())
}

/// Makes sure the worker runs with the right model and backend.
async fn ensure_worker<'a>(
    guard: &'a mut Option<WorkerHandle>,
    model_path: &std::path::Path,
    backend: Backend,
    threads: u32,
) -> anyhow::Result<&'a mut WorkerHandle> {
    let reusable = guard
        .as_ref()
        .is_some_and(|w| w.model_path == model_path && w.backend == backend);
    if !reusable {
        if let Some(old) = guard.take() {
            old.shutdown().await;
        }
        *guard = Some(
            WorkerHandle::start(model_path, backend, threads)
                .await
                .context("starting the transcription worker")?,
        );
    }
    Ok(guard.as_mut().expect("worker was just started"))
}

async fn run_job(job: JobSpec, worker: Arc<Mutex<Option<WorkerHandle>>>, backend: Backend, threads: u32) {
    let JobSpec {
        job_id,
        model_path,
        language,
        translate,
        cancel,
        audio_rx,
        out_tx,
        internal_tx,
    } = job;
    let send_failed = |error: String| {
        let _ = out_tx.send(json_message(&PeerMessage::JobFailed {
            job_id,
            error,
            retryable: true,
        }));
    };
    let mut worker_failed = false;
    let mut succeeded = false;

    let mut guard = worker.lock().await;
    // Load the model while the audio is still arriving.
    let handle = match ensure_worker(&mut guard, &model_path, backend, threads).await {
        Ok(h) => Some(h),
        Err(e) => {
            send_failed(format!("{e:#}"));
            worker_failed = true;
            None
        }
    };

    if let Some(handle) = handle {
        let audio = tokio::select! {
            audio = audio_rx => audio.ok(),
            () = cancel.cancelled() => None,
            () = tokio::time::sleep(Duration::from_secs(600)) => None,
        };
        if let Some((pcm, keys)) = audio {
            let audio_ready = Instant::now();
            let mut seq: u32 = 0;
            let mut first_segment_ms = None;
            let result = handle
                .transcribe(&pcm, language, translate, &cancel, |segment| {
                    first_segment_ms.get_or_insert_with(|| audio_ready.elapsed().as_millis() as u64);
                    let Ok(plain) = serde_json::to_vec(&segment) else {
                        return;
                    };
                    if let Ok(sealed) = keys.seal_result(seq, false, &plain) {
                        let _ = out_tx.send(json_message(&PeerMessage::Segment {
                            job_id,
                            seq,
                            sealed: encode_sealed(&sealed),
                        }));
                        seq += 1;
                    }
                })
                .await;
            drop(pcm);
            match result {
                Ok(output) => {
                    let result = TranscriptResult {
                        language: output.language,
                        segments: output.segments,
                        processing_ms: output.processing_ms,
                        first_segment_ms,
                    };
                    match serde_json::to_vec(&result)
                        .map_err(anyhow::Error::from)
                        .and_then(|plain| keys.seal_result(seq, true, &plain).map_err(anyhow::Error::from))
                    {
                        Ok(sealed) => {
                            let _ = out_tx.send(json_message(&PeerMessage::JobFinished {
                                job_id,
                                seq,
                                sealed: encode_sealed(&sealed),
                            }));
                            info!(%job_id, processing_ms = result.processing_ms, segments = result.segments.len(), "job finished");
                            succeeded = true;
                        }
                        Err(e) => send_failed(format!("sealing the result failed: {e:#}")),
                    }
                }
                Err(WorkerError::Cancelled) => debug!(%job_id, "job cancelled"),
                Err(WorkerError::Crashed) => {
                    warn!(%job_id, "the worker process crashed");
                    guard.take();
                    worker_failed = true;
                    send_failed("the transcription worker crashed".into());
                }
                Err(WorkerError::Failed(message)) => {
                    warn!(%job_id, %message, "transcription failed");
                    send_failed(message);
                }
            }
        }
    }
    drop(guard);
    let _ = internal_tx.send(Internal::JobDone {
        job_id,
        worker_failed,
        succeeded,
    });
}
