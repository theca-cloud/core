//! Streaming a version's content from its chunks: an offset maps to a chunk
//! through the cumulative plaintext sizes; the chunk is fetched from its
//! backend, decompressed, checked against its hash, and the right slice served.
//! The server streams (WebDAV, previews): a redirect can't split one response
//! across chunks nor translate a `Range` (docs/decisions.md).

use bytes::Bytes;
use futures::Stream;
use theca_common::compression::decompress;
use theca_common::hash::ChunkHash;

use crate::api::AppState;
use crate::db::file::{VersionChunk, version_chunks};

/// Why a read failed.
#[derive(Debug, thiserror::Error)]
pub enum StreamError {
    #[error("database: {0}")]
    Db(#[from] sqlx::Error),
    #[error("storage: {0}")]
    Backend(#[from] crate::storage::BackendError),
    #[error("chunk {0} is corrupted (decompression or hash)")]
    Corrupted(uuid::Uuid),
}

/// A seekable reader over one version.
pub struct VersionReader {
    state: AppState,
    chunks: Vec<VersionChunk>,
    /// Where each chunk starts in the file.
    starts: Vec<u64>,
    len: u64,
    /// The last chunk read (sequential reads hit it): `(index, plaintext)`.
    cache: Option<(usize, Bytes)>,
}

impl VersionReader {
    pub async fn open(state: &AppState, version: i64) -> Result<Self, StreamError> {
        let (_, chunks) = version_chunks(&state.pool, version).await?;
        let mut starts = Vec::with_capacity(chunks.len());
        let mut len = 0;
        for c in &chunks {
            starts.push(len);
            len += c.plain_size;
        }
        Ok(Self {
            state: state.clone(),
            chunks,
            starts,
            len,
            cache: None,
        })
    }

    /// The file's size.
    pub fn len(&self) -> u64 {
        self.len
    }

    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    /// Up to `max` bytes from `offset` (never across a chunk boundary; empty at
    /// the end of the file).
    pub async fn read(&mut self, offset: u64, max: usize) -> Result<Bytes, StreamError> {
        if offset >= self.len || max == 0 {
            return Ok(Bytes::new());
        }
        let index = self.starts.partition_point(|&s| s <= offset) - 1;
        let plain = match &self.cache {
            Some((i, data)) if *i == index => data.clone(),
            _ => {
                let data = self.fetch(index).await?;
                self.cache = Some((index, data.clone()));
                data
            }
        };
        let from = (offset - self.starts[index]) as usize;
        let to = plain.len().min(from + max);
        Ok(plain.slice(from..to))
    }

    async fn fetch(&self, index: usize) -> Result<Bytes, StreamError> {
        let c = &self.chunks[index];
        let stored = self
            .state
            .backends
            .get(c.storage)
            .await?
            .get(c.uuid)
            .await?;
        let plain =
            decompress(c.compression, &stored).map_err(|_| StreamError::Corrupted(c.uuid))?;
        if ChunkHash::of(&plain) != c.hash {
            tracing::error!(chunk = %c.uuid, "a stored chunk doesn't match its hash");
            return Err(StreamError::Corrupted(c.uuid));
        }
        Ok(Bytes::from(plain))
    }

    /// The bytes `start..end` as a stream, for an HTTP body.
    pub fn into_stream(
        self,
        start: u64,
        end: u64,
    ) -> impl Stream<Item = Result<Bytes, std::io::Error>> + Send {
        futures::stream::unfold((self, start), move |(mut reader, at)| async move {
            if at >= end {
                return None;
            }
            let max = (end - at).min(usize::MAX as u64) as usize;
            match reader.read(at, max).await {
                Ok(bytes) if bytes.is_empty() => None,
                Ok(bytes) => {
                    let next = at + bytes.len() as u64;
                    Some((Ok(bytes), (reader, next)))
                }
                Err(e) => Some((Err(std::io::Error::other(e.to_string())), (reader, end))),
            }
        })
    }
}

/// A single byte range from a `Range` header, within a file of `len` bytes:
/// `Some(Ok(start..end))`, `Some(Err(()))` if unsatisfiable, `None` to serve
/// the whole file (no header, several ranges, or unparsable: allowed by RFC 9110).
pub fn parse_range(header: Option<&str>, len: u64) -> Option<Result<(u64, u64), ()>> {
    let spec = header?.strip_prefix("bytes=")?;
    if spec.contains(',') {
        return None;
    }
    let (start, end) = spec.split_once('-')?;
    let (start, end) = match (start.trim(), end.trim()) {
        ("", suffix) => {
            let n: u64 = suffix.parse().ok()?;
            (len.saturating_sub(n), len)
        }
        (start, "") => (start.parse().ok()?, len),
        (start, end) => (
            start.parse().ok()?,
            end.parse::<u64>().ok()?.saturating_add(1).min(len),
        ),
    };
    Some(if start < end && start < len {
        Ok((start, end))
    } else {
        Err(())
    })
}

#[cfg(test)]
mod tests {
    use super::parse_range;

    #[test]
    fn ranges() {
        assert_eq!(parse_range(None, 100), None);
        assert_eq!(parse_range(Some("bytes=0-9"), 100), Some(Ok((0, 10))));
        assert_eq!(parse_range(Some("bytes=90-"), 100), Some(Ok((90, 100))));
        assert_eq!(parse_range(Some("bytes=-10"), 100), Some(Ok((90, 100))));
        assert_eq!(parse_range(Some("bytes=50-1000"), 100), Some(Ok((50, 100))));
        assert_eq!(parse_range(Some("bytes=100-"), 100), Some(Err(())));
        assert_eq!(parse_range(Some("bytes=0-1,5-6"), 100), None);
        assert_eq!(parse_range(Some("items=0-1"), 100), None);
    }
}
