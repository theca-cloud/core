//! Downloads (native): the manifest, chunks fetched in parallel from their
//! presigned URLs, decompressed, checked against their hash (by default), and
//! written in order. (The web app downloads through the server's streaming
//! endpoint in V1.)

use std::io::Write;

use futures::StreamExt;
use theca_common::api;
use theca_common::compression::decompress;
use theca_common::hash::ChunkHash;
use uuid::Uuid;

use crate::api::{Client, Error};
use crate::rt::retry;

/// How a download behaves.
#[derive(Debug, Clone)]
pub struct DownloadOptions {
    /// A version of the file (default: the current one).
    pub version: Option<Uuid>,
    /// Chunks fetched at once (each at most 8 MiB in memory, plus the order
    /// they're written in).
    pub parallel: usize,
    /// Check every chunk against its hash: a lying server or storage is
    /// caught. Costs little (`xxh3_128` runs at tens of GB/s).
    pub verify: bool,
    /// Retries of a fetch failing on the network or with a 5xx.
    pub retries: u32,
}

impl Default for DownloadOptions {
    fn default() -> Self {
        Self {
            version: None,
            parallel: 4,
            verify: true,
            retries: 4,
        }
    }
}

/// Where a download stands, in bytes.
#[derive(Debug, Clone, Default)]
pub struct DownloadProgress {
    pub total: u64,
    pub written: u64,
}

/// Downloads a file's version into `sink`. Returns the version.
pub async fn download(
    client: &Client,
    file: Uuid,
    sink: &mut impl Write,
    options: &DownloadOptions,
    mut progress: impl FnMut(&DownloadProgress),
) -> Result<Uuid, Error> {
    let manifest = client.download_manifest(file, options.version).await?;
    let mut state = DownloadProgress {
        total: manifest.chunks.iter().map(|c| c.plain_size).sum(),
        written: 0,
    };
    progress(&state);
    let mut chunks =
        futures::stream::iter(manifest.chunks.iter().map(|c| fetch(client, c, options)))
            .buffered(options.parallel.max(1));
    while let Some(plain) = chunks.next().await {
        let plain = plain?;
        sink.write_all(&plain)?;
        state.written += plain.len() as u64;
        progress(&state);
    }
    sink.flush()?;
    Ok(manifest.version)
}

async fn fetch(
    client: &Client,
    chunk: &api::DownloadChunk,
    options: &DownloadOptions,
) -> Result<Vec<u8>, Error> {
    let stored = retry(options.retries, || async {
        let response = client.http().get(&chunk.url).send().await?;
        let status = response.status();
        if !status.is_success() {
            return Err(Error::Storage {
                chunk: chunk.chunk,
                status: status.as_u16(),
            });
        }
        Ok(response.bytes().await?)
    })
    .await?;
    let plain =
        decompress(chunk.compression, &stored).map_err(|_| Error::Corrupted(chunk.chunk))?;
    if options.verify
        && (plain.len() as u64 != chunk.plain_size || ChunkHash::of(&plain) != chunk.hash)
    {
        return Err(Error::Corrupted(chunk.chunk));
    }
    Ok(plain)
}
