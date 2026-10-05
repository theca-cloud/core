//! Authentication primitives: password hashing, opaque tokens.
//! See docs/decisions.md, Authentication.

use std::time::Duration;

use argon2::Argon2;
use argon2::password_hash::phc::PasswordHash;
use argon2::password_hash::{PasswordHasher, PasswordVerifier};
use sha2::Digest;

/// `last_seen` is written at most this often (not on every request).
pub const LAST_SEEN_PRECISION: Duration = Duration::from_secs(5 * 60);

/// Hashes a password with Argon2id (PHC string: parameters included, so they can evolve).
pub fn hash_password(password: &str) -> String {
    // Random salt from the system.
    let hash: PasswordHash = Argon2::default()
        .hash_password(password.as_bytes())
        .expect("valid parameters");
    hash.to_string()
}

/// Checks a password against a stored hash. With `None` (unknown account),
/// still spends the same time on a dummy hash, so responses don't reveal
/// whether an email exists.
pub fn verify_password(hash: Option<&str>, password: &str) -> bool {
    static DUMMY: std::sync::OnceLock<String> = std::sync::OnceLock::new();
    let dummy = DUMMY.get_or_init(|| hash_password("dummy password, never matches"));
    let Ok(parsed) = PasswordHash::new(hash.unwrap_or(dummy.as_str())) else {
        return false;
    };
    let ok = Argon2::default()
        .verify_password(password.as_bytes(), &parsed)
        .is_ok();
    ok && hash.is_some()
}

/// An opaque token: `th_<public>.<private>`. The database stores the public
/// part (lookup) and the SHA-256 of the whole string, never the token itself.
pub struct OpaqueToken {
    /// The whole token, given once to the client.
    pub token: String,
    /// `th_<public>`, stored and indexed.
    pub prefix: String,
    /// SHA-256 of `token`, stored.
    pub hash: [u8; 32],
}

impl OpaqueToken {
    pub fn generate() -> Self {
        let mut public = [0u8; 12];
        let mut private = [0u8; 32];
        getrandom::fill(&mut public).expect("system randomness");
        getrandom::fill(&mut private).expect("system randomness");
        let prefix = format!("th_{}", hex(&public));
        let token = format!("{prefix}.{}", hex(&private));
        let hash = sha2::Sha256::digest(token.as_bytes()).into();
        Self {
            token,
            prefix,
            hash,
        }
    }

    /// The public part and SHA-256 of a token presented by a client, if well-formed.
    pub fn parse(token: &str) -> Option<(&str, [u8; 32])> {
        let (prefix, private) = token.split_once('.')?;
        if !prefix.starts_with("th_") || private.is_empty() {
            return None;
        }
        Some((prefix, sha2::Sha256::digest(token.as_bytes()).into()))
    }
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn passwords() {
        let hash = hash_password("correct horse");
        assert!(hash.starts_with("$argon2id$"));
        assert!(verify_password(Some(&hash), "correct horse"));
        assert!(!verify_password(Some(&hash), "wrong"));
        assert!(
            !verify_password(None, "dummy password, never matches"),
            "unknown account never matches"
        );
    }

    #[test]
    fn opaque_tokens() {
        let t = OpaqueToken::generate();
        let (prefix, hash) = OpaqueToken::parse(&t.token).unwrap();
        assert_eq!((prefix, hash), (t.prefix.as_str(), t.hash));
        assert!(OpaqueToken::parse("garbage").is_none());
        assert_ne!(OpaqueToken::generate().token, t.token);
    }
}
