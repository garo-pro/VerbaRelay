//! Tokens and the master password.
//!
//! Tokens are 32 random bytes, shown once and stored only as SHA-256 hashes. The
//! master password is either compared in constant time (plain configuration) or
//! verified against an Argon2 hash.

use argon2::{Argon2, PasswordHash, PasswordHasher, PasswordVerifier};
use base64::Engine;
use rand::TryRng;
use sha2::{Digest, Sha256};
use subtle::ConstantTimeEq;

use crate::config::EnrollmentSection;

/// Creates a new random token with the given prefix.
pub fn generate_token(prefix: &str) -> String {
    let mut bytes = [0u8; 32];
    rand::rngs::SysRng
        .try_fill_bytes(&mut bytes)
        .expect("the operating system random number generator failed");
    format!(
        "{prefix}{}",
        base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(bytes)
    )
}

/// The hash under which a token is stored.
pub fn hash_token(token: &str) -> String {
    let digest = Sha256::digest(token.trim().as_bytes());
    digest.iter().map(|b| format!("{b:02x}")).collect()
}

/// A short, non-secret prefix of a token for log messages.
pub fn redact(token: &str) -> String {
    let visible: String = token.chars().take(8).collect();
    format!("{visible}…")
}

/// Extracts the token of an `Authorization: Bearer <token>` header value.
pub fn bearer(header: Option<&str>) -> Option<&str> {
    let value = header?.trim();
    let (scheme, token) = value.split_once(' ')?;
    if scheme.eq_ignore_ascii_case("bearer") && !token.trim().is_empty() {
        Some(token.trim())
    } else {
        None
    }
}

/// Creates an Argon2id hash for the configuration file.
pub fn hash_password(password: &str) -> anyhow::Result<String> {
    Argon2::default()
        .hash_password(password.as_bytes())
        .map(|h| h.to_string())
        .map_err(|e| anyhow::anyhow!("hashing failed: {e}"))
}

/// Checks a master password attempt.
pub fn verify_master_password(config: &EnrollmentSection, attempt: &str) -> bool {
    if let Some(hash) = &config.master_password_hash {
        return match PasswordHash::new(hash) {
            Ok(parsed) => Argon2::default().verify_password(attempt.as_bytes(), &parsed).is_ok(),
            Err(_) => false,
        };
    }
    if let Some(plain) = &config.master_password {
        // Compare digests so the comparison time does not depend on the length.
        let a = Sha256::digest(plain.as_bytes());
        let b = Sha256::digest(attempt.as_bytes());
        return bool::from(a.ct_eq(&b));
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tokens_are_unique_and_hashable() {
        let a = generate_token("vra_");
        let b = generate_token("vra_");
        assert_ne!(a, b);
        assert!(a.starts_with("vra_"));
        assert_eq!(hash_token(&a), hash_token(&format!(" {a} ")));
        assert_eq!(hash_token(&a).len(), 64);
        assert!(!redact(&a).contains(&a[10..]));
    }

    #[test]
    fn bearer_parsing() {
        assert_eq!(bearer(Some("Bearer abc")), Some("abc"));
        assert_eq!(bearer(Some("bearer  abc ")), Some("abc"));
        assert_eq!(bearer(Some("Basic abc")), None);
        assert_eq!(bearer(Some("Bearer ")), None);
        assert_eq!(bearer(None), None);
    }

    #[test]
    fn master_password_plain_and_hashed() {
        let plain = EnrollmentSection {
            master_password: Some("correct horse battery".into()),
            master_password_hash: None,
            peer_token_ttl_days: 14,
        };
        assert!(verify_master_password(&plain, "correct horse battery"));
        assert!(!verify_master_password(&plain, "wrong"));

        let hashed = EnrollmentSection {
            master_password: None,
            master_password_hash: Some(hash_password("correct horse battery").unwrap()),
            peer_token_ttl_days: 14,
        };
        assert!(verify_master_password(&hashed, "correct horse battery"));
        assert!(!verify_master_password(&hashed, "wrong"));
    }
}
