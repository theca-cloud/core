//! Storage backends (built from `storage` rows, cached) and presigned URLs.
//!
//! - S3: presigned URLs from S3 itself (`object_store`'s `Signer`). The server
//!   never sees chunk data. Upload URLs sign the chunk's SHA-256
//!   (`x-amz-checksum-sha256`): the backend only accepts exactly the declared
//!   bytes, hence the declared size. They also sign `If-None-Match: *` (no
//!   overwrite) where the backend supports it.
//! - Filesystem: no presigning of its own, so the server signs its own URLs
//!   (`/api/v1/blobs/{chunk}`), the SHA-256 included, and checks them itself.
//!
//! Clients never know which backend they talk to. Object key = the chunk's uuid.

use std::collections::{BTreeMap, HashMap};
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use base64::Engine;
use hmac::{KeyInit, Mac};
use http::{HeaderName, HeaderValue, Method};
use object_store::aws::{AmazonS3, AmazonS3Builder};
use object_store::local::LocalFileSystem;
use object_store::path::Path;
use object_store::signer::{SignedUrlOptions, Signer};
use object_store::{ClientOptions, RetryConfig};
use object_store::{ObjectStore, ObjectStoreExt, PutPayload};
use sqlx::PgPool;
use theca_common::hash::Sha256;
use tokio::sync::RwLock;
use uuid::Uuid;

/// A storage backend failure (unknown implementation, bad details, I/O).
#[derive(Debug, thiserror::Error)]
pub enum BackendError {
    #[error("unsupported storage implementation {0:?}")]
    Unsupported(String),
    #[error("invalid storage details: {0}")]
    Details(String),
    #[error("storage error: {0}")]
    Store(#[from] object_store::Error),
    #[error("database error: {0}")]
    Database(#[from] sqlx::Error),
}

/// One storage backend.
#[derive(Clone, Debug)]
pub enum Backend {
    Filesystem(Arc<LocalFileSystem>),
    S3(Arc<AmazonS3>),
}

/// A presigned request: URL plus the headers the client must send (signed).
pub struct Presigned {
    pub url: String,
    pub headers: BTreeMap<String, String>,
}

impl Backend {
    /// Builds the backend described by a `storage` row.
    pub fn new(implementation: &str, details: &serde_json::Value) -> Result<Self, BackendError> {
        Self::build(implementation, details, false)
    }

    /// `quick`: for probes, S3 requests fail in seconds (short timeouts, one
    /// retry) instead of `object_store`'s default policy, which retries for
    /// minutes (made for transient errors, not a wrong address).
    fn build(
        implementation: &str,
        details: &serde_json::Value,
        quick: bool,
    ) -> Result<Self, BackendError> {
        let field = |name: &str| {
            details[name]
                .as_str()
                .ok_or_else(|| BackendError::Details(format!("{implementation} needs \"{name}\"")))
        };
        match implementation {
            "filesystem" => {
                let path = field("path")?;
                std::fs::create_dir_all(path).map_err(|e| BackendError::Details(e.to_string()))?;
                Ok(Self::Filesystem(Arc::new(
                    LocalFileSystem::new_with_prefix(path)?,
                )))
            }
            "s3" => {
                // Credentials are files (never stored in the DB).
                let secret = |name: &str| -> Result<String, BackendError> {
                    let path = field(name)?;
                    std::fs::read_to_string(path)
                        .map(|v| v.trim().to_owned())
                        .map_err(|e| BackendError::Details(format!("{name} ({path}): {e}")))
                };
                let host = field("host")?;
                let mut builder = AmazonS3Builder::new();
                if quick {
                    builder = builder
                        .with_client_options(
                            ClientOptions::new()
                                .with_connect_timeout(Duration::from_secs(5))
                                .with_timeout(Duration::from_secs(15)),
                        )
                        .with_retry(RetryConfig {
                            max_retries: 1,
                            retry_timeout: Duration::from_secs(20),
                            ..Default::default()
                        });
                }
                let store = builder
                    .with_endpoint(host)
                    .with_region(field("region")?)
                    .with_bucket_name(field("bucket")?)
                    .with_access_key_id(secret("access_key_file")?)
                    .with_secret_access_key(secret("secret_key_file")?)
                    // Self-hosted S3 servers usually need path-style URLs (host/bucket/key).
                    .with_virtual_hosted_style_request(
                        !details["path_style"].as_bool().unwrap_or(true),
                    )
                    .with_allow_http(host.starts_with("http://"))
                    .build()?;
                Ok(Self::S3(Arc::new(store)))
            }
            other => Err(BackendError::Unsupported(other.into())),
        }
    }

    fn store(&self) -> &dyn ObjectStore {
        match self {
            Self::Filesystem(s) => s.as_ref(),
            Self::S3(s) => s.as_ref(),
        }
    }

    /// Stores a chunk's object (filesystem uploads; atomic: temp file + rename).
    pub async fn put(&self, chunk: Uuid, data: Vec<u8>) -> Result<(), BackendError> {
        self.store()
            .put(&key(chunk), PutPayload::from(data))
            .await?;
        Ok(())
    }

    pub async fn get(&self, chunk: Uuid) -> Result<Vec<u8>, BackendError> {
        Ok(self.store().get(&key(chunk)).await?.bytes().await?.to_vec())
    }

    /// The real stored size, or `None` if the object doesn't exist.
    pub async fn size(&self, chunk: Uuid) -> Result<Option<u64>, BackendError> {
        match self.store().head(&key(chunk)).await {
            Ok(meta) => Ok(Some(meta.size)),
            Err(object_store::Error::NotFound { .. }) => Ok(None),
            Err(e) => Err(e.into()),
        }
    }

    /// Every chunk object: `(key, last modified in Unix seconds)`, as a stream
    /// (a backend can hold millions).
    pub fn list(&self) -> futures::stream::BoxStream<'static, Result<(String, i64), BackendError>> {
        use futures::StreamExt;
        self.store()
            .list(Some(&Path::from(CHUNKS)))
            .map(|meta| {
                let meta = meta?;
                Ok((meta.location.to_string(), meta.last_modified.timestamp()))
            })
            .boxed()
    }

    /// Deletes a chunk's object; deleting a missing object is fine.
    pub async fn delete(&self, chunk: Uuid) -> Result<(), BackendError> {
        match self.store().delete(&key(chunk)).await {
            Ok(()) | Err(object_store::Error::NotFound { .. }) => Ok(()),
            Err(e) => Err(e.into()),
        }
    }

    /// A URL to upload exactly the bytes whose SHA-256 is `sha256`.
    pub async fn upload_url(
        &self,
        signer: &UrlSigner,
        chunk: Uuid,
        sha256: Sha256,
        lifetime: Duration,
    ) -> Result<Presigned, BackendError> {
        let checksum = base64::engine::general_purpose::STANDARD.encode(sha256.0);
        let headers = BTreeMap::from([
            ("x-amz-checksum-sha256".to_owned(), checksum.clone()),
            ("if-none-match".to_owned(), "*".to_owned()),
        ]);
        let url = match self {
            Self::Filesystem(_) => signer.url(Operation::Put, chunk, Some(sha256), lifetime),
            Self::S3(s3) => {
                let options = SignedUrlOptions::default()
                    .with_signed_header(
                        HeaderName::from_static("x-amz-checksum-sha256"),
                        HeaderValue::from_str(&checksum).expect("base64 is a valid header"),
                    )
                    .with_signed_header(
                        HeaderName::from_static("if-none-match"),
                        HeaderValue::from_static("*"),
                    );
                s3.signed_url_opts(Method::PUT, &key(chunk), lifetime, &options)
                    .await?
                    .to_string()
            }
        };
        Ok(Presigned { url, headers })
    }

    /// A URL to download a chunk's object.
    pub async fn download_url(
        &self,
        signer: &UrlSigner,
        chunk: Uuid,
        lifetime: Duration,
    ) -> Result<String, BackendError> {
        Ok(match self {
            Self::Filesystem(_) => signer.url(Operation::Get, chunk, None, lifetime),
            Self::S3(s3) => s3
                .signed_url(Method::GET, &key(chunk), lifetime)
                .await?
                .to_string(),
        })
    }
}

/// The chunk an object key is for, if it is one.
pub fn chunk_of(key: &str) -> Option<Uuid> {
    let chunk = Uuid::parse_str(key.rsplit('/').next()?).ok()?;
    (self::key(chunk).as_ref() == key).then_some(chunk)
}

/// Where a chunk's object lives: `chunks/<aa>/<bb>/<uuid>`, `aa` and `bb` the
/// UUID's last four hex digits (UUIDv7's random part: recent chunks spread
/// evenly), so no directory grows huge (decisions.md, Storage backends).
pub fn key(chunk: Uuid) -> Path {
    let hex = chunk.simple().to_string();
    Path::from(format!("{CHUNKS}/{}/{}/{chunk}", &hex[28..30], &hex[30..]))
}

/// The prefix of every chunk object; nothing else is ever listed.
const CHUNKS: &str = "chunks";

/// The backends, built once per storage (an S3 client holds connections).
#[derive(Clone)]
pub struct Backends {
    pool: PgPool,
    cache: Arc<RwLock<HashMap<i64, Backend>>>,
}

impl Backends {
    pub fn new(pool: PgPool) -> Self {
        Self {
            pool,
            cache: Arc::default(),
        }
    }

    /// The backend of a storage row.
    // ponytail: never invalidated; when admins can edit a storage's details,
    // drop its entry on update.
    pub async fn get(&self, storage: i64) -> Result<Backend, BackendError> {
        if let Some(backend) = self.cache.read().await.get(&storage) {
            return Ok(backend.clone());
        }
        let row = sqlx::query!(
            "SELECT implementation, details FROM storage WHERE pk = $1",
            storage
        )
        .fetch_one(&self.pool)
        .await?;
        let backend = Backend::new(&row.implementation, &row.details)?;
        self.cache.write().await.insert(storage, backend.clone());
        Ok(backend)
    }
}

/// Upload or download, part of what a server-signed URL signs.
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

/// Signs and verifies the server's own presigned URLs (filesystem backend).
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

    /// The URL clients reach the server at, without a trailing `/`.
    pub fn public_url(&self) -> &str {
        &self.public_url
    }

    /// A URL allowing `operation` on `chunk` for `lifetime`. Uploads also sign
    /// the SHA-256 of the only bytes they accept.
    pub fn url(
        &self,
        operation: Operation,
        chunk: Uuid,
        sha256: Option<Sha256>,
        lifetime: Duration,
    ) -> String {
        let expires = now() + lifetime.as_secs();
        let sha = sha256.map(|s| s.to_string()).unwrap_or_default();
        let sig = self.signature(operation, chunk, expires, &sha);
        let sha_param = if sha.is_empty() {
            String::new()
        } else {
            format!("&sha256={sha}")
        };
        format!(
            "{}/api/v1/blobs/{chunk}?op={}&expires={expires}{sha_param}&sig={sig}",
            self.public_url,
            operation.as_str()
        )
    }

    /// Whether a URL's signature is valid and not expired.
    pub fn verify(
        &self,
        operation: Operation,
        chunk: Uuid,
        expires: u64,
        sha256: &str,
        sig: &str,
    ) -> bool {
        expires >= now()
            && constant_time_eq(&self.signature(operation, chunk, expires, sha256), sig)
    }

    fn signature(&self, operation: Operation, chunk: Uuid, expires: u64, sha256: &str) -> String {
        let mut mac =
            hmac::Hmac::<sha2::Sha256>::new_from_slice(&self.key).expect("any key length");
        mac.update(format!("{}:{chunk}:{expires}:{sha256}", operation.as_str()).as_bytes());
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

/// A `storage` row's implementation and details.
pub fn details(backend: &theca_common::api::StorageBackend) -> (&'static str, serde_json::Value) {
    let mut details = serde_json::to_value(backend).expect("serializable");
    let implementation = match backend {
        theca_common::api::StorageBackend::Filesystem { .. } => "filesystem",
        theca_common::api::StorageBackend::S3 { .. } => "s3",
    };
    details
        .as_object_mut()
        .expect("an object")
        .remove("implementation");
    (implementation, details)
}

/// Writes, reads back and deletes a small object: the storage works. Fails
/// in seconds on S3 (a quick client), not after minutes of retries.
pub async fn probe(implementation: &str, details: &serde_json::Value) -> Result<(), String> {
    let backend = Backend::build(implementation, details, true).map_err(|e| e.to_string())?;
    let mut id = [0u8; 16];
    getrandom::fill(&mut id).expect("system randomness");
    let key = Uuid::from_bytes(id);
    let data = b"theca storage probe".to_vec();
    backend
        .put(key, data.clone())
        .await
        .map_err(|e| e.to_string())?;
    let read = backend.get(key).await.map_err(|e| e.to_string());
    backend.delete(key).await.map_err(|e| e.to_string())?;
    if read? != data {
        return Err("read back different bytes".into());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn chunk_keys() {
        let chunk: Uuid = "01a10e63-d658-7be1-8a89-e852b2e921d5".parse().unwrap();
        let key = super::key(chunk).to_string();
        assert_eq!(key, "chunks/21/d5/01a10e63-d658-7be1-8a89-e852b2e921d5");
        assert_eq!(chunk_of(&key), Some(chunk));
        // Anything else in the backend is never taken for a chunk.
        assert_eq!(chunk_of("01a10e63-d658-7be1-8a89-e852b2e921d5"), None);
        assert_eq!(
            chunk_of("chunks/00/00/01a10e63-d658-7be1-8a89-e852b2e921d5"),
            None
        );
        assert_eq!(chunk_of("chunks/21/d5/notes.txt"), None);
    }

    #[test]
    fn signed_urls() {
        let signer = UrlSigner::new([7; 32], "http://localhost:8080/");
        let chunk = Uuid::nil();
        let sha = Sha256::of(b"x");
        let url = signer.url(Operation::Put, chunk, Some(sha), Duration::from_secs(60));
        let query: HashMap<_, _> = url
            .split_once('?')
            .unwrap()
            .1
            .split('&')
            .filter_map(|kv| kv.split_once('='))
            .collect();
        let expires: u64 = query["expires"].parse().unwrap();
        let (s, sig) = (query["sha256"], query["sig"]);
        assert!(signer.verify(Operation::Put, chunk, expires, s, sig));
        assert!(
            !signer.verify(Operation::Get, chunk, expires, s, sig),
            "other operation"
        );
        assert!(
            !signer.verify(Operation::Put, Uuid::max(), expires, s, sig),
            "other chunk"
        );
        assert!(
            !signer.verify(Operation::Put, chunk, expires + 1, s, sig),
            "tampered expiry"
        );
        assert!(
            !signer.verify(
                Operation::Put,
                chunk,
                expires,
                &Sha256::of(b"y").to_string(),
                sig
            ),
            "other bytes"
        );
        let old = signer.signature(Operation::Put, chunk, 1, s);
        assert!(!signer.verify(Operation::Put, chunk, 1, s, &old), "expired");
    }
}
