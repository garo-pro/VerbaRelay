//! JSON control messages of the peer WebSocket and the enrollment endpoint.
//!
//! Flow of one job:
//!
//! 1. server -> peer: [`ServerMessage::Offer`] ("are you there and free?")
//! 2. peer -> server: [`PeerMessage::OfferReply`] (accept or decline)
//! 3. server -> peer: [`ServerMessage::Audio`] followed by binary audio frames
//! 4. peer -> server: any number of [`PeerMessage::Segment`] (live results)
//! 5. peer -> server: [`PeerMessage::JobFinished`] or [`PeerMessage::JobFailed`]

use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::models::ModelSpec;

/// The compute backend a peer uses.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Backend {
    Cpu,
    Vulkan,
    Cuda,
    Metal,
}

impl Backend {
    pub const ALL: [Backend; 4] = [Backend::Cuda, Backend::Metal, Backend::Vulkan, Backend::Cpu];

    pub fn as_str(self) -> &'static str {
        match self {
            Backend::Cpu => "cpu",
            Backend::Vulkan => "vulkan",
            Backend::Cuda => "cuda",
            Backend::Metal => "metal",
        }
    }

    pub fn display_name(self) -> &'static str {
        match self {
            Backend::Cpu => "CPU",
            Backend::Vulkan => "Vulkan (GPU)",
            Backend::Cuda => "CUDA (NVIDIA GPU)",
            Backend::Metal => "Metal (Apple GPU)",
        }
    }
}

impl std::fmt::Display for Backend {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

impl std::str::FromStr for Backend {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s.to_ascii_lowercase().as_str() {
            "cpu" => Ok(Backend::Cpu),
            "vulkan" => Ok(Backend::Vulkan),
            "cuda" => Ok(Backend::Cuda),
            "metal" => Ok(Backend::Metal),
            other => Err(format!("unknown backend: {other}")),
        }
    }
}

/// A graphics device as seen by the peer.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct GpuInfo {
    pub name: String,
    /// ggml backend that reported the device, e.g. "Vulkan" or "CUDA".
    pub backend: String,
    /// True for integrated GPUs.
    pub integrated: bool,
    pub memory_total: u64,
}

/// Hardware description sent by a peer.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DeviceInfo {
    pub hostname: String,
    pub os: String,
    pub arch: String,
    pub cpu_name: String,
    pub cpu_threads: u32,
    pub memory_total: u64,
    pub gpus: Vec<GpuInfo>,
    /// Backend the peer transcribes with.
    pub backend: Backend,
    /// Human readable name of the device doing the work, e.g. "NVIDIA GeForce RTX 4070".
    pub accelerator: String,
}

/// Result of a benchmark run on the peer.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct BenchmarkResult {
    pub model: String,
    pub backend: Backend,
    /// Length of the benchmark audio.
    pub audio_ms: u64,
    /// Time needed to transcribe it (model already loaded).
    pub processing_ms: u64,
}

impl BenchmarkResult {
    /// How many times faster than real time the peer is. This is the basis of
    /// the transcription score.
    pub fn speed(&self) -> f64 {
        self.audio_ms as f64 / self.processing_ms.max(1) as f64
    }
}

/// The server's rules for which model to use.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ModelPolicy {
    /// When set, every peer must use this model unless a request overrides it.
    pub enforced: Option<ModelSpec>,
    /// Models requests may ask for (peers download them on demand).
    pub allowed: Vec<ModelSpec>,
}

/// Asks a peer to measure its speed.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct BenchmarkRequest {
    /// Model to benchmark; `None` means the peer's own configured model.
    pub model: Option<ModelSpec>,
}

/// Request body of the enrollment endpoint.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EnrollRequest {
    pub master_password: String,
    pub name: String,
    /// The peer's X25519 public key (URL-safe base64).
    pub public_key: String,
    pub client_version: String,
}

/// Response body of the enrollment endpoint.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EnrollResponse {
    pub peer_id: Uuid,
    pub token: String,
    pub token_ttl_days: u32,
}

/// Messages sent from a peer to the server.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum PeerMessage {
    /// First message after connecting.
    Hello {
        protocol_version: u32,
        client_version: String,
        device: DeviceInfo,
        /// Names of the models available on disk.
        models: Vec<String>,
        busy: bool,
    },
    /// The set of models on disk changed (download finished).
    Models { models: Vec<String> },
    /// A benchmark finished.
    Benchmark { result: BenchmarkResult },
    /// Answer to [`ServerMessage::Offer`].
    OfferReply {
        job_id: Uuid,
        accept: bool,
        reason: Option<String>,
    },
    /// A live segment, encrypted with the job's result key.
    Segment {
        job_id: Uuid,
        seq: u32,
        /// Sealed JSON of a [`crate::transcript::Segment`] (URL-safe base64).
        sealed: String,
    },
    /// The job is done. `sealed` holds a [`crate::transcript::TranscriptResult`].
    JobFinished { job_id: Uuid, seq: u32, sealed: String },
    /// The job failed on this peer.
    JobFailed {
        job_id: Uuid,
        error: String,
        /// False when the audio itself is the problem and retrying elsewhere is pointless.
        retryable: bool,
    },
}

/// Reason a peer gives when it declines an offer because the model is missing.
/// The peer starts downloading the model at the same time.
pub const DECLINE_MODEL_MISSING: &str = "model_missing";

/// Reason a peer gives when it is already working.
pub const DECLINE_BUSY: &str = "busy";

/// Messages sent from the server to a peer.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ServerMessage {
    /// Answer to [`PeerMessage::Hello`].
    Welcome {
        peer_id: Uuid,
        server_version: String,
        policy: ModelPolicy,
        /// The server has no valid score for this peer yet.
        benchmark: Option<BenchmarkRequest>,
    },
    /// "Are you there and free?" for a job.
    Offer {
        job_id: Uuid,
        /// `None` means the peer's own configured model (server does not enforce one).
        model: Option<ModelSpec>,
        language: Option<String>,
        translate: bool,
        audio_ms: u64,
    },
    /// Audio of an accepted job follows in `chunk_count` binary frames.
    Audio {
        job_id: Uuid,
        /// Server's ephemeral X25519 public key (URL-safe base64).
        ephemeral_public_key: String,
        chunk_count: u32,
        sample_count: u64,
    },
    /// Stop working on a job.
    Cancel { job_id: Uuid },
    /// Please run a benchmark.
    RunBenchmark { request: BenchmarkRequest },
    /// The peer was revoked and must enroll again.
    Revoked,
}

/// Encodes sealed bytes for a JSON field.
pub fn encode_sealed(bytes: &[u8]) -> String {
    use base64::Engine;
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(bytes)
}

/// Decodes a sealed JSON field.
pub fn decode_sealed(text: &str) -> Option<Vec<u8>> {
    use base64::Engine;
    base64::engine::general_purpose::URL_SAFE_NO_PAD.decode(text).ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn messages_are_tagged() {
        let msg = PeerMessage::OfferReply {
            job_id: Uuid::nil(),
            accept: true,
            reason: None,
        };
        let json = serde_json::to_string(&msg).unwrap();
        assert!(json.contains("\"type\":\"offer_reply\""));
        let back: PeerMessage = serde_json::from_str(&json).unwrap();
        assert!(matches!(back, PeerMessage::OfferReply { accept: true, .. }));
    }

    #[test]
    fn backend_parsing() {
        assert_eq!("CUDA".parse::<Backend>().unwrap(), Backend::Cuda);
        assert_eq!("metal".parse::<Backend>().unwrap(), Backend::Metal);
    }

    #[test]
    fn benchmark_speed() {
        let b = BenchmarkResult {
            model: "tiny".into(),
            backend: Backend::Cpu,
            audio_ms: 11_000,
            processing_ms: 1_000,
        };
        assert!((b.speed() - 11.0).abs() < f64::EPSILON);
    }
}
