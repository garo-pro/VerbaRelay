//! Binary WebSocket frames.
//!
//! Layout of an audio chunk frame:
//!
//! | bytes | content                           |
//! |-------|-----------------------------------|
//! | 1     | frame kind (1 = audio chunk)      |
//! | 16    | job id                            |
//! | 4     | sequence number (big endian)      |
//! | 1     | 1 if this is the last chunk       |
//! | rest  | ChaCha20-Poly1305 ciphertext      |

use uuid::Uuid;

/// Frame kind byte of an encrypted audio chunk.
pub const KIND_AUDIO_CHUNK: u8 = 1;

/// Size of the plaintext audio carried per chunk (1 MiB).
pub const AUDIO_CHUNK_SIZE: usize = 1024 * 1024;

const HEADER_LEN: usize = 1 + 16 + 4 + 1;

/// An encrypted audio chunk.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AudioChunk {
    pub job_id: Uuid,
    pub seq: u32,
    pub last: bool,
    pub ciphertext: Vec<u8>,
}

impl AudioChunk {
    /// Serializes the chunk into a binary frame.
    pub fn encode(&self) -> Vec<u8> {
        let mut out = Vec::with_capacity(HEADER_LEN + self.ciphertext.len());
        out.push(KIND_AUDIO_CHUNK);
        out.extend_from_slice(self.job_id.as_bytes());
        out.extend_from_slice(&self.seq.to_be_bytes());
        out.push(u8::from(self.last));
        out.extend_from_slice(&self.ciphertext);
        out
    }

    /// Parses a binary frame. Returns `None` for malformed or unknown frames.
    pub fn decode(frame: &[u8]) -> Option<Self> {
        if frame.len() < HEADER_LEN || frame[0] != KIND_AUDIO_CHUNK {
            return None;
        }
        let job_id = Uuid::from_slice(&frame[1..17]).ok()?;
        let seq = u32::from_be_bytes(frame[17..21].try_into().ok()?);
        let last = match frame[21] {
            0 => false,
            1 => true,
            _ => return None,
        };
        Some(Self {
            job_id,
            seq,
            last,
            ciphertext: frame[HEADER_LEN..].to_vec(),
        })
    }
}

/// Converts 16-bit PCM samples to little-endian bytes.
pub fn pcm_to_bytes(samples: &[i16]) -> Vec<u8> {
    samples.iter().flat_map(|s| s.to_le_bytes()).collect()
}

/// Converts little-endian bytes back to 16-bit PCM samples.
pub fn bytes_to_pcm(bytes: &[u8]) -> Vec<i16> {
    bytes
        .as_chunks::<2>()
        .0
        .iter()
        .map(|b| i16::from_le_bytes(*b))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn chunk_round_trip() {
        let chunk = AudioChunk {
            job_id: Uuid::new_v4(),
            seq: 7,
            last: true,
            ciphertext: vec![1, 2, 3],
        };
        assert_eq!(AudioChunk::decode(&chunk.encode()), Some(chunk));
    }

    #[test]
    fn rejects_garbage() {
        assert_eq!(AudioChunk::decode(&[1, 2, 3]), None);
        assert_eq!(AudioChunk::decode(&[9; 40]), None);
    }

    #[test]
    fn pcm_round_trip() {
        let samples = vec![0, -1, i16::MAX, i16::MIN, 1234];
        assert_eq!(bytes_to_pcm(&pcm_to_bytes(&samples)), samples);
    }
}
