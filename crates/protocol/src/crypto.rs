//! End-to-end encryption between the server and a single peer.
//!
//! Every peer owns a long-term X25519 key pair that is created on the peer and
//! registered during enrollment; the private key never leaves the peer. For each
//! job the server creates an ephemeral X25519 key pair, performs a Diffie-Hellman
//! exchange with the peer's public key and derives two independent
//! ChaCha20-Poly1305 keys with HKDF-SHA256 (salt = job id):
//!
//! * the audio key (server -> peer)
//! * the result key (peer -> server)
//!
//! Because every job uses fresh key material, nonces are simple counters. The job
//! id, the chunk sequence number and a "last chunk" flag are bound as associated
//! data, so chunks cannot be reordered, replayed into another job or truncated.
//!
//! This layer is in addition to TLS: audio stays encrypted even if TLS is
//! terminated by a reverse proxy, and only the selected peer can decrypt it.

use chacha20poly1305::aead::{Aead, KeyInit, Payload};
use chacha20poly1305::{ChaCha20Poly1305, Key, Nonce};
use hkdf::Hkdf;
use rand::rngs::OsRng;
use sha2::Sha256;
use uuid::Uuid;
use x25519_dalek::{EphemeralSecret, PublicKey, StaticSecret};
use zeroize::{Zeroize, ZeroizeOnDrop};

const AUDIO_INFO: &[u8] = b"verbarelay/v1/audio";
const RESULT_INFO: &[u8] = b"verbarelay/v1/result";

/// Errors of the crypto layer.
#[derive(Debug, thiserror::Error)]
pub enum CryptoError {
    #[error("the key exchange produced a non-contributory shared secret")]
    WeakKey,
    #[error("decryption failed: the data was tampered with or the key is wrong")]
    Decrypt,
    #[error("encryption failed")]
    Encrypt,
    #[error("invalid key encoding")]
    InvalidKey,
}

/// The long-term key pair of a peer.
#[derive(Clone)]
pub struct PeerKeypair {
    secret: StaticSecret,
}

impl PeerKeypair {
    /// Creates a fresh random key pair.
    pub fn generate() -> Self {
        Self {
            secret: StaticSecret::random_from_rng(OsRng),
        }
    }

    /// Restores a key pair from its 32 secret bytes.
    pub fn from_secret_bytes(bytes: [u8; 32]) -> Self {
        Self {
            secret: StaticSecret::from(bytes),
        }
    }

    /// The 32 secret bytes (store them with restrictive permissions).
    pub fn secret_bytes(&self) -> [u8; 32] {
        self.secret.to_bytes()
    }

    /// The public key that is registered with the server.
    pub fn public_bytes(&self) -> [u8; 32] {
        PublicKey::from(&self.secret).to_bytes()
    }

    /// Derives the session keys of a job from the server's ephemeral public key.
    pub fn open_session(&self, ephemeral_public: [u8; 32], job_id: Uuid) -> Result<SessionKeys, CryptoError> {
        let shared = self.secret.diffie_hellman(&PublicKey::from(ephemeral_public));
        if !shared.was_contributory() {
            return Err(CryptoError::WeakKey);
        }
        SessionKeys::derive(shared.as_bytes(), job_id)
    }
}

/// Creates the session keys for a job on the server side.
///
/// Returns the ephemeral public key that has to be sent to the peer.
pub fn create_session(peer_public: [u8; 32], job_id: Uuid) -> Result<([u8; 32], SessionKeys), CryptoError> {
    let ephemeral = EphemeralSecret::random_from_rng(OsRng);
    let ephemeral_public = PublicKey::from(&ephemeral).to_bytes();
    let shared = ephemeral.diffie_hellman(&PublicKey::from(peer_public));
    if !shared.was_contributory() {
        return Err(CryptoError::WeakKey);
    }
    Ok((ephemeral_public, SessionKeys::derive(shared.as_bytes(), job_id)?))
}

/// The two symmetric keys of one job.
#[derive(Zeroize, ZeroizeOnDrop)]
pub struct SessionKeys {
    audio: [u8; 32],
    result: [u8; 32],
    #[zeroize(skip)]
    job_id: Uuid,
}

impl SessionKeys {
    fn derive(shared: &[u8; 32], job_id: Uuid) -> Result<Self, CryptoError> {
        let hk = Hkdf::<Sha256>::new(Some(job_id.as_bytes()), shared);
        let mut audio = [0u8; 32];
        let mut result = [0u8; 32];
        hk.expand(AUDIO_INFO, &mut audio).map_err(|_| CryptoError::InvalidKey)?;
        hk.expand(RESULT_INFO, &mut result)
            .map_err(|_| CryptoError::InvalidKey)?;
        Ok(Self { audio, result, job_id })
    }

    /// Encrypts an audio chunk (server side).
    pub fn seal_audio(&self, seq: u32, last: bool, plaintext: &[u8]) -> Result<Vec<u8>, CryptoError> {
        seal(&self.audio, self.job_id, seq, last, plaintext)
    }

    /// Decrypts an audio chunk (peer side).
    pub fn open_audio(&self, seq: u32, last: bool, ciphertext: &[u8]) -> Result<Vec<u8>, CryptoError> {
        open(&self.audio, self.job_id, seq, last, ciphertext)
    }

    /// Encrypts a result message (peer side).
    pub fn seal_result(&self, seq: u32, last: bool, plaintext: &[u8]) -> Result<Vec<u8>, CryptoError> {
        seal(&self.result, self.job_id, seq, last, plaintext)
    }

    /// Decrypts a result message (server side).
    pub fn open_result(&self, seq: u32, last: bool, ciphertext: &[u8]) -> Result<Vec<u8>, CryptoError> {
        open(&self.result, self.job_id, seq, last, ciphertext)
    }
}

fn nonce_for(seq: u32) -> [u8; 12] {
    let mut nonce = [0u8; 12];
    nonce[8..].copy_from_slice(&seq.to_be_bytes());
    nonce
}

fn aad_for(job_id: Uuid, seq: u32, last: bool) -> [u8; 21] {
    let mut aad = [0u8; 21];
    aad[..16].copy_from_slice(job_id.as_bytes());
    aad[16..20].copy_from_slice(&seq.to_be_bytes());
    aad[20] = u8::from(last);
    aad
}

fn seal(key: &[u8; 32], job_id: Uuid, seq: u32, last: bool, plaintext: &[u8]) -> Result<Vec<u8>, CryptoError> {
    let cipher = ChaCha20Poly1305::new(Key::from_slice(key));
    let nonce = nonce_for(seq);
    let aad = aad_for(job_id, seq, last);
    cipher
        .encrypt(
            Nonce::from_slice(&nonce),
            Payload {
                msg: plaintext,
                aad: &aad,
            },
        )
        .map_err(|_| CryptoError::Encrypt)
}

fn open(key: &[u8; 32], job_id: Uuid, seq: u32, last: bool, ciphertext: &[u8]) -> Result<Vec<u8>, CryptoError> {
    let cipher = ChaCha20Poly1305::new(Key::from_slice(key));
    let nonce = nonce_for(seq);
    let aad = aad_for(job_id, seq, last);
    cipher
        .decrypt(
            Nonce::from_slice(&nonce),
            Payload {
                msg: ciphertext,
                aad: &aad,
            },
        )
        .map_err(|_| CryptoError::Decrypt)
}

/// Encodes a 32-byte key as unpadded URL-safe base64.
pub fn encode_key(key: &[u8; 32]) -> String {
    use base64::Engine;
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(key)
}

/// Decodes a 32-byte key from unpadded URL-safe base64.
pub fn decode_key(text: &str) -> Result<[u8; 32], CryptoError> {
    use base64::Engine;
    let bytes = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .decode(text.trim())
        .map_err(|_| CryptoError::InvalidKey)?;
    bytes.try_into().map_err(|_| CryptoError::InvalidKey)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trip_between_server_and_peer() {
        let peer = PeerKeypair::generate();
        let job = Uuid::new_v4();
        let (eph, server_keys) = create_session(peer.public_bytes(), job).unwrap();
        let peer_keys = peer.open_session(eph, job).unwrap();

        let sealed = server_keys.seal_audio(0, true, b"pcm data").unwrap();
        assert_eq!(peer_keys.open_audio(0, true, &sealed).unwrap(), b"pcm data");

        let result = peer_keys.seal_result(3, false, b"hello").unwrap();
        assert_eq!(server_keys.open_result(3, false, &result).unwrap(), b"hello");
    }

    #[test]
    fn tampering_is_detected() {
        let peer = PeerKeypair::generate();
        let job = Uuid::new_v4();
        let (eph, server_keys) = create_session(peer.public_bytes(), job).unwrap();
        let peer_keys = peer.open_session(eph, job).unwrap();
        let sealed = server_keys.seal_audio(1, false, b"chunk").unwrap();

        // Wrong sequence number, wrong last flag, other job, other direction.
        assert!(peer_keys.open_audio(2, false, &sealed).is_err());
        assert!(peer_keys.open_audio(1, true, &sealed).is_err());
        assert!(
            peer.open_session(eph, Uuid::new_v4())
                .unwrap()
                .open_audio(1, false, &sealed)
                .is_err()
        );
        assert!(peer_keys.open_result(1, false, &sealed).is_err());
    }

    #[test]
    fn other_peers_cannot_decrypt() {
        let peer = PeerKeypair::generate();
        let attacker = PeerKeypair::generate();
        let job = Uuid::new_v4();
        let (eph, server_keys) = create_session(peer.public_bytes(), job).unwrap();
        let sealed = server_keys.seal_audio(0, true, b"secret").unwrap();
        assert!(
            attacker
                .open_session(eph, job)
                .unwrap()
                .open_audio(0, true, &sealed)
                .is_err()
        );
    }

    #[test]
    fn low_order_points_are_rejected() {
        let peer = PeerKeypair::generate();
        assert!(matches!(
            peer.open_session([0u8; 32], Uuid::new_v4()),
            Err(CryptoError::WeakKey)
        ));
    }

    #[test]
    fn key_encoding_round_trip() {
        let key = PeerKeypair::generate().public_bytes();
        assert_eq!(decode_key(&encode_key(&key)).unwrap(), key);
        assert!(decode_key("short").is_err());
    }
}
