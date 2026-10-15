//! Shared building blocks of verbarelay.
//!
//! * [`messages`]: the JSON control messages exchanged over the peer WebSocket.
//! * [`crypto`]: per-job end-to-end encryption between the server and one peer.
//! * [`frames`]: the binary WebSocket frames that carry encrypted audio.
//! * [`models`]: the catalog of Whisper models a server can hand out.
//! * [`transcript`]: transcription results.

pub mod crypto;
pub mod frames;
pub mod messages;
pub mod models;
pub mod transcript;

/// Version of the peer protocol. Bumped on incompatible changes.
pub const PROTOCOL_VERSION: u32 = 1;

/// Sample rate of all audio sent to peers.
pub const SAMPLE_RATE: u32 = 16_000;

/// HTTP path of the peer WebSocket endpoint.
pub const PEER_CONNECT_PATH: &str = "/api/v1/peers/connect";

/// HTTP path of the enrollment endpoint.
pub const PEER_ENROLL_PATH: &str = "/api/v1/peers/enroll";

/// Prefix of tokens issued to peers.
pub const PEER_TOKEN_PREFIX: &str = "vrp_";

/// Prefix of tokens issued to API callers.
pub const API_TOKEN_PREFIX: &str = "vra_";
