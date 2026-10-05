//! Storage backends (built from a `storage` row) and presigned URLs.
//!
//! The filesystem backend has no presigning of its own, so the server signs its
//! own URLs (`/api/v1/blobs/{chunk}`), with the same shape as S3 presigned URLs:
//! clients never know which backend they talk to. Object key = the chunk's uuid.

use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use hmac::{KeyInit, Mac};
use object_store::local::LocalFileSystem;
use object_store::path::Path;
use object_store::{ObjectStore, ObjectStoreExt, PutPayload};
use uuid::Uuid;

/// How long a presigned URL stays valid.
pub const URL_LIFETIME: Duration = Duration::from_secs(3600);

/// A storage backend failure (unknown implementation, bad details, I/O).
#[derive(Debug, thiserror::Error)]
pub enum BackendError {
    #[error("unsupported storage implementation {0:?}")]
    Unsupported(String),
    #[error("invalid storage details: {0}")]
    Details(String),
    #[error("storage error: {0}")]
    Store(#[from] object_store::Error),
}

/// One storage backend.
#[derive(Clone)]
pub struct Backend {
    store: Arc<dyn ObjectStore>,
}

impl Backend {
    /// Builds the backend described by a `storage` row.
    pub fn new(implementation: &str, details: &serde_json::Value) -> Result<Self, BackendError> {
        match implementation {
            "filesystem" => {
                let path = details["path"]
                    .as_str()
                    .ok_or_else(|| BackendError::Details("filesystem needs a \"path\"".into()))?;
                std::fs::create_dir_all(path).map_err(|e| BackendError::Details(e.to_string()))?;
                Ok(Self {
                    store: Arc::new(LocalFileSystem::new_with_prefix(path)?),
                })
            }
            other => Err(BackendError::Unsupported(other.into())),
        }
    }

    /// Stores a chunk's object (atomic: the filesystem writes a temp file and renames).
    pub async fn put(&self, chunk: Uuid, data: Vec<u8>) -> Result<(), BackendError> {
        self.store.put(&key(chunk), PutPayload::from(data)).await?;
        Ok(())
    }

    pub async fn get(&self, chunk: Uuid) -> Result<Vec<u8>, BackendError> {
        Ok(self.store.get(&key(chunk)).await?.bytes().await?.to_vec())
    }

    /// The real stored size, or `None` if the object doesn't exist.
    pub async fn size(&self, chunk: Uuid) -> Result<Option<u64>, BackendError> {
        match self.store.head(&key(chunk)).await {
            Ok(meta) => Ok(Some(meta.size)),
            Err(object_store::Error::NotFound { .. }) => Ok(None),
            Err(e) => Err(e.into()),
        }
    }

    /// Deletes a chunk's object; deleting a missing object is fine.
    pub async fn delete(&self, chunk: Uuid) -> Result<(), BackendError> {
        match self.store.delete(&key(chunk)).await {
            Ok(()) | Err(object_store::Error::NotFound { .. }) => Ok(()),
            Err(e) => Err(e.into()),
        }
    }
}

fn key(chunk: Uuid) -> Path {
    Path::from(chunk.to_string())
}

/// Upload or download, part of what a presigned URL signs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Operation {
    Put,
    Get,
}

impl Operation {
    fn as_str(self) -> &'static str {
        match self {
            Self::Put => "put",
            Self::Get => "get",
        }
    }
}

/// Signs and verifies the server's own presigned URLs.
#[derive(Clone)]
pub struct UrlSigner {
    key: [u8; 32],
    public_url: String,
}

impl UrlSigner {
    pub fn new(key: [u8; 32], public_url: &str) -> Self {
        Self {
            key,
            public_url: public_url.trim_end_matches('/').to_owned(),
        }
    }

    /// A URL allowing `operation` on `chunk` until [`URL_LIFETIME`] from now.
    pub fn url(&self, operation: Operation, chunk: Uuid) -> String {
        let expires = now() + URL_LIFETIME.as_secs();
        let sig = self.signature(operation, chunk, expires);
        format!(
            "{}/api/v1/blobs/{chunk}?op={}&expires={expires}&sig={sig}",
            self.public_url,
            operation.as_str()
        )
    }

    /// Whether a URL's signature is valid and not expired.
    pub fn verify(&self, operation: Operation, chunk: Uuid, expires: u64, sig: &str) -> bool {
        expires >= now() && constant_time_eq(&self.signature(operation, chunk, expires), sig)
    }

    fn signature(&self, operation: Operation, chunk: Uuid, expires: u64) -> String {
        let mut mac =
            hmac::Hmac::<sha2::Sha256>::new_from_slice(&self.key).expect("any key length");
        mac.update(format!("{}:{chunk}:{expires}", operation.as_str()).as_bytes());
        mac.finalize()
            .into_bytes()
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect()
    }
}

fn constant_time_eq(a: &str, b: &str) -> bool {
    a.len() == b.len()
        && a.bytes()
            .zip(b.bytes())
            .fold(0, |acc, (x, y)| acc | (x ^ y))
            == 0
}

fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn signed_urls() {
        let signer = UrlSigner::new([7; 32], "http://localhost:8080/");
        let chunk = Uuid::nil();
        let url = signer.url(Operation::Put, chunk);
        let query: std::collections::HashMap<_, _> = url
            .split_once('?')
            .unwrap()
            .1
            .split('&')
            .filter_map(|kv| kv.split_once('='))
            .collect();
        let expires: u64 = query["expires"].parse().unwrap();
        assert!(signer.verify(Operation::Put, chunk, expires, query["sig"]));
        assert!(
            !signer.verify(Operation::Get, chunk, expires, query["sig"]),
            "other operation"
        );
        assert!(
            !signer.verify(Operation::Put, Uuid::max(), expires, query["sig"]),
            "other chunk"
        );
        assert!(
            !signer.verify(Operation::Put, chunk, expires + 1, query["sig"]),
            "tampered expiry"
        );
        assert!(
            !signer.verify(
                Operation::Put,
                chunk,
                1,
                &signer.signature(Operation::Put, chunk, 1)
            ),
            "expired"
        );
    }
}
