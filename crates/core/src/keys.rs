//! Secret keys read from files (`SERVER_KEY__FILE`, `DATA_KEY__FILE`), never
//! written (decisions.md, Configuration), and the cipher for secrets stored in
//! the database.

use std::path::Path;

use base64::Engine;
use base64::engine::general_purpose::STANDARD as BASE64;
use chacha20poly1305::aead::{Aead, KeyInit, Payload};
use chacha20poly1305::{XChaCha20Poly1305, XNonce};

use crate::Error;

/// A 32-byte secret from a file. Only used to derive keys (see [`SecretKey::derive`]).
#[derive(Clone)]
pub struct SecretKey([u8; 32]);

impl std::fmt::Debug for SecretKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("SecretKey(<redacted>)")
    }
}

impl SecretKey {
    /// Loads the key from `path` (64 hex characters).
    pub fn load(name: &'static str, path: &Path) -> Result<Self, Error> {
        let content = crate::config::read_secret(name, path.to_owned())?;
        let content = content.trim();
        let invalid = || Error::InvalidKey(name, path.to_owned());
        if content.len() != 64 {
            return Err(invalid());
        }
        let mut key = [0u8; 32];
        for (i, byte) in key.iter_mut().enumerate() {
            *byte = u8::from_str_radix(&content[2 * i..2 * i + 2], 16).map_err(|_| invalid())?;
        }
        Ok(Self(key))
    }
}

impl SecretKey {
    /// A key for another purpose, derived from this one (HMAC-SHA256 of `label`),
    /// so the server has a single secret to manage.
    pub fn derive(&self, label: &str) -> [u8; 32] {
        use hmac::{KeyInit, Mac};
        let mut mac = hmac::Hmac::<sha2::Sha256>::new_from_slice(&self.0).expect("any key length");
        mac.update(label.as_bytes());
        mac.finalize().into_bytes().into()
    }
}

/// Encrypts secrets stored in the database (TOTP secrets) with a key derived
/// from `DATA_KEY__FILE`: a database dump alone doesn't reveal them.
/// XChaCha20-Poly1305 (random 24-byte nonces); the associated data binds each
/// ciphertext to its row (e.g. the account), so it can't be moved to another.
#[derive(Clone)]
pub struct DataCipher(XChaCha20Poly1305);

impl std::fmt::Debug for DataCipher {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("DataCipher(<redacted>)")
    }
}

impl DataCipher {
    pub fn new(key: &[u8; 32]) -> Self {
        Self(XChaCha20Poly1305::new(&(*key).into()))
    }

    /// `base64(nonce ‖ ciphertext)`.
    pub fn encrypt(&self, plaintext: &[u8], aad: &[u8]) -> String {
        let mut nonce = [0u8; 24];
        getrandom::fill(&mut nonce).expect("system randomness");
        let ciphertext = self
            .0
            .encrypt(
                &XNonce::from(nonce),
                Payload {
                    msg: plaintext,
                    aad,
                },
            )
            .expect("encryption can't fail");
        BASE64.encode([&nonce[..], &ciphertext].concat())
    }

    /// `None` if tampered with, encrypted with another key, or for another row.
    pub fn decrypt(&self, encrypted: &str, aad: &[u8]) -> Option<Vec<u8>> {
        let bytes = BASE64.decode(encrypted).ok()?;
        if bytes.len() < 24 {
            return None;
        }
        let (nonce, ciphertext) = bytes.split_at(24);
        let nonce: [u8; 24] = nonce.try_into().ok()?;
        self.0
            .decrypt(
                &XNonce::from(nonce),
                Payload {
                    msg: ciphertext,
                    aad,
                },
            )
            .ok()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_file(name: &str, content: &str) -> std::path::PathBuf {
        let dir =
            std::env::temp_dir().join(format!("theca-server-key-{name}-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("key");
        std::fs::write(&path, content).unwrap();
        path
    }

    #[test]
    fn loads_a_key_and_refuses_an_empty_file() {
        let path = temp_file("load", &format!("{}\n", "ab".repeat(32)));
        assert_eq!(SecretKey::load("TEST__FILE", &path).unwrap().0, [0xab; 32]);
        let path = temp_file("empty", "");
        let err = SecretKey::load("SERVER_KEY__FILE", &path).unwrap_err();
        assert!(matches!(err, Error::EmptySecret { .. }), "{err}");
        assert!(err.to_string().contains("openssl rand -hex 32"), "{err}");
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "", "never written");
    }

    #[test]
    fn data_cipher() {
        let cipher = DataCipher::new(&[1; 32]);
        let sealed = cipher.encrypt(b"secret", b"account 1");
        assert_eq!(cipher.decrypt(&sealed, b"account 1").unwrap(), b"secret");
        assert_ne!(
            cipher.encrypt(b"secret", b"account 1"),
            sealed,
            "random nonces"
        );
        assert!(
            cipher.decrypt(&sealed, b"account 2").is_none(),
            "bound to its row"
        );
        assert!(
            DataCipher::new(&[2; 32])
                .decrypt(&sealed, b"account 1")
                .is_none()
        );
        assert!(cipher.decrypt("garbage", b"account 1").is_none());
    }

    #[test]
    fn rejects_invalid_content() {
        let path = temp_file("bad", "not a key");
        assert!(matches!(
            SecretKey::load("TEST__FILE", &path),
            Err(Error::InvalidKey(..))
        ));
    }
}
