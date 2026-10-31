//! Speed measurement with a bundled speech sample.
//!
//! The sample is the first two minutes of John F. Kennedy's inaugural address
//! (1961, public domain, original recording of the JFK Library). It is long
//! enough to cover several 30 second Whisper windows without repeating itself,
//! so the score is not skewed by caching or repeated text.

use std::path::Path;
use std::time::Instant;

use anyhow::{Context, bail};
use tokio_util::sync::CancellationToken;
use verbarelay_protocol::messages::{Backend, BenchmarkResult};

use crate::worker::WorkerHandle;

const SAMPLE_WAV: &[u8] = include_bytes!("../assets/benchmark.wav");
/// Length of the uncounted warm-up run: the start of the sample.
const WARMUP_SECONDS: usize = 11;

/// Decodes the bundled 16 kHz mono 16-bit WAV file.
pub fn sample_pcm() -> anyhow::Result<Vec<i16>> {
    parse_wav_16k_mono(SAMPLE_WAV)
}

fn parse_wav_16k_mono(bytes: &[u8]) -> anyhow::Result<Vec<i16>> {
    if bytes.len() < 12 || &bytes[0..4] != b"RIFF" || &bytes[8..12] != b"WAVE" {
        bail!("not a WAV file");
    }
    let mut pos = 12;
    let mut format_ok = false;
    while pos + 8 <= bytes.len() {
        let id = &bytes[pos..pos + 4];
        let len = u32::from_le_bytes(bytes[pos + 4..pos + 8].try_into()?) as usize;
        let body = bytes.get(pos + 8..pos + 8 + len).context("truncated WAV chunk")?;
        if id == b"fmt " {
            let channels = u16::from_le_bytes(body[2..4].try_into()?);
            let rate = u32::from_le_bytes(body[4..8].try_into()?);
            let bits = u16::from_le_bytes(body[14..16].try_into()?);
            format_ok = channels == 1 && rate == verbarelay_protocol::SAMPLE_RATE && bits == 16;
        } else if id == b"data" {
            if !format_ok {
                bail!("the benchmark sample must be 16 kHz mono 16-bit");
            }
            return Ok(verbarelay_protocol::frames::bytes_to_pcm(body));
        }
        pos += 8 + len + (len & 1);
    }
    bail!("WAV file without data")
}

/// Measures the speed of a loaded worker. One warm-up run is not counted.
pub async fn measure(worker: &mut WorkerHandle, model_name: &str) -> anyhow::Result<BenchmarkResult> {
    let pcm = sample_pcm()?;
    let audio_ms = pcm.len() as u64 * 1000 / u64::from(verbarelay_protocol::SAMPLE_RATE);
    let cancel = CancellationToken::new();
    // Warm-up: the first run includes one-time GPU setup costs.
    let warmup_len = (WARMUP_SECONDS * verbarelay_protocol::SAMPLE_RATE as usize).min(pcm.len());
    let warmup = &pcm[..warmup_len];
    worker
        .transcribe(warmup, Some("en".into()), false, &cancel, |_| {})
        .await?;
    let started = Instant::now();
    let output = worker
        .transcribe(&pcm, Some("en".into()), false, &cancel, |_| {})
        .await?;
    let wall_ms = started.elapsed().as_millis() as u64;
    Ok(BenchmarkResult {
        model: model_name.to_string(),
        backend: worker.backend,
        audio_ms,
        processing_ms: output.processing_ms.max(1).min(wall_ms.max(1)),
    })
}

/// Starts a worker, measures and stops it again.
pub async fn run(
    model_path: &Path,
    model_name: &str,
    backend: Backend,
    threads: u32,
) -> anyhow::Result<BenchmarkResult> {
    let mut worker = WorkerHandle::start(model_path, backend, threads).await?;
    let result = measure(&mut worker, model_name).await;
    worker.shutdown().await;
    result
}

#[cfg(test)]
mod tests {
    #[test]
    fn bundled_sample_is_valid() {
        let pcm = super::sample_pcm().unwrap();
        let seconds = pcm.len() / 16_000;
        assert!((100..=130).contains(&seconds), "sample is {seconds} s long");
    }
}
