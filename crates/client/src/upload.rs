//! Uploads: the protocol of docs/decisions.md (Upload flow), once for every
//! client.
//!
//! 1. Hash pass: the source chunked (FastCDC) and hashed; only hashes and
//!    sizes kept.
//! 2. Lookup: which chunks the server lacks.
//! 3. Upload pass: only those chunks read again, compressed, declared, sent to
//!    their presigned URLs a few at a time, announced.
//! 4. Commit; chunks the server lost meanwhile (`chunk_missing`) go again.
//!
//! Resuming is starting again: the lookup skips what's already there.

use std::collections::HashMap;
use std::io;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use futures::{StreamExt, TryStreamExt};
use theca_common::api;
use theca_common::chunking::AsyncChunker;
use theca_common::compression::{CompressionPolicy, compress};
use theca_common::hash::{ChunkHash, Sha256};
use theca_common::name::Name;
use uuid::Uuid;
use web_time::Instant;

use crate::api::{Client, Error};
use crate::rt;
use crate::source::Source;

/// How an upload behaves.
#[derive(Debug, Clone)]
pub struct UploadOptions {
    /// Chunks sent at once (each at most 8 MiB in memory).
    pub parallel: usize,
    /// How often uploaded chunks are looked up again during a long upload, so
    /// the server's garbage collection never takes them (it waits 24 h).
    pub keepalive: Duration,
    /// Retries of a request failing on the network or with a 5xx.
    pub retries: u32,
    pub compression: CompressionPolicy,
    /// The version's description.
    pub message: Option<String>,
}

impl Default for UploadOptions {
    fn default() -> Self {
        Self {
            parallel: 4,
            keepalive: Duration::from_secs(6 * 3600),
            retries: 4,
            compression: CompressionPolicy::default(),
            message: None,
        }
    }
}

/// Where the file goes.
#[derive(Debug, Clone)]
pub enum Target {
    /// A new file in a folder.
    New {
        library: Uuid,
        parent: Uuid,
        name: Name,
    },
    /// A new version of a file, based on `base` (the version the edit started
    /// from; a conflict if it's no longer current).
    Version {
        library: Uuid,
        file: Uuid,
        base: Option<Uuid>,
    },
}

impl Target {
    fn library(&self) -> Uuid {
        match self {
            Target::New { library, .. } | Target::Version { library, .. } => *library,
        }
    }
}

/// Where an upload stands, in bytes of the file.
#[derive(Debug, Clone, Default)]
pub struct UploadProgress {
    pub total: u64,
    pub hashed: u64,
    pub uploaded: u64,
    /// Already on the server (deduplicated): nothing to send.
    pub skipped: u64,
}

/// Cancels an upload from elsewhere (another task, a UI button).
#[derive(Debug, Clone, Default)]
pub struct Cancel(Arc<AtomicBool>);

impl Cancel {
    pub fn cancel(&self) {
        self.0.store(true, Ordering::Relaxed);
    }

    fn check(&self) -> Result<(), Error> {
        if self.0.load(Ordering::Relaxed) {
            Err(Error::Cancelled)
        } else {
            Ok(())
        }
    }
}

/// A chunk of the source.
#[derive(Debug, Clone, Copy)]
struct Part {
    offset: u64,
    len: usize,
    hash: ChunkHash,
}

/// How much the hash pass reads at once.
const BLOCK: usize = 4 * 1024 * 1024;
/// Hashes per lookup request.
const LOOKUP_BATCH: usize = 1000;
/// Rounds of re-uploading chunks the server lost before committing.
const COMMIT_ROUNDS: usize = 3;

/// Uploads `source` to `target`. `progress` is called as it advances;
/// `cancel` stops it (the pending chunks are cancelled on the server).
pub async fn upload(
    client: &Client,
    source: &impl Source,
    target: Target,
    options: &UploadOptions,
    mut progress: impl FnMut(&UploadProgress),
    cancel: &Cancel,
) -> Result<api::Committed, Error> {
    let mut state = UploadProgress {
        total: source.len(),
        ..Default::default()
    };
    let parts = hash(source, &mut state, &mut progress, cancel).await?;
    let mut ids: HashMap<ChunkHash, Uuid> = HashMap::new();
    let mut needed: Vec<Part> = parts.clone();
    for round in 0..COMMIT_ROUNDS {
        send(
            client,
            source,
            target.library(),
            &needed,
            options,
            &mut state,
            &mut progress,
            cancel,
            &mut ids,
        )
        .await?;
        let commit = commit_request(&target, &parts, &ids, options);
        match retry(options.retries, || client.commit(&commit)).await {
            Ok(committed) => return Ok(committed),
            // Lost meanwhile (garbage-collected, or cancelled by a parallel
            // upload of the same content): those again, with new IDs.
            Err(Error::Api { error, .. })
                if error.code == "chunk_missing" && round + 1 < COMMIT_ROUNDS =>
            {
                // Without a list (lost between the server's check and its
                // insert): look everything up again (ready chunks are skipped).
                needed = parts
                    .iter()
                    .filter(|p| error.chunks.is_empty() || error.chunks.contains(&ids[&p.hash]))
                    .copied()
                    .collect();
                for p in &needed {
                    ids.remove(&p.hash);
                }
            }
            Err(e) => return Err(e),
        }
    }
    unreachable!("the last round returns")
}

/// The hash pass: reads the source in blocks through the chunker.
async fn hash(
    source: &impl Source,
    state: &mut UploadProgress,
    progress: &mut impl FnMut(&UploadProgress),
    cancel: &Cancel,
) -> Result<Vec<Part>, Error> {
    let len = source.len();
    let blocks = futures::stream::unfold(0u64, move |offset| async move {
        if offset >= len {
            return None;
        }
        match source.read(offset, BLOCK).await {
            Ok(data) if data.is_empty() => None,
            Ok(data) => {
                let next = offset + data.len() as u64;
                Some((Ok::<_, io::Error>(data), next))
            }
            Err(e) => Some((Err(e), len)),
        }
    });
    let mut chunker = AsyncChunker::new(Box::pin(blocks).into_async_read());
    let mut chunks = std::pin::pin!(chunker.stream());
    let mut parts = Vec::new();
    while let Some(chunk) = chunks.next().await {
        cancel.check()?;
        let chunk = chunk?;
        parts.push(Part {
            offset: chunk.offset,
            len: chunk.data.len(),
            hash: ChunkHash::of(&chunk.data),
        });
        state.hashed += chunk.data.len() as u64;
        progress(state);
    }
    Ok(parts)
}

/// Looks `parts` up, and uploads those the server lacks. Fills `ids`.
#[allow(clippy::too_many_arguments)]
async fn send(
    client: &Client,
    source: &impl Source,
    library: Uuid,
    parts: &[Part],
    options: &UploadOptions,
    state: &mut UploadProgress,
    progress: &mut impl FnMut(&UploadProgress),
    cancel: &Cancel,
    ids: &mut HashMap<ChunkHash, Uuid>,
) -> Result<(), Error> {
    // Each distinct chunk once.
    let mut unique: Vec<Part> = Vec::new();
    for p in parts {
        if !unique.iter().any(|u| u.hash == p.hash) && !ids.contains_key(&p.hash) {
            unique.push(*p);
        }
    }
    let mut to_upload: Vec<(Uuid, Part)> = Vec::new();
    for batch in unique.chunks(LOOKUP_BATCH) {
        let entries = lookup(client, library, batch, options).await?;
        for (part, entry) in batch.iter().zip(entries) {
            ids.insert(part.hash, entry.chunk);
            match entry.state {
                api::ChunkState::Upload => to_upload.push((entry.chunk, *part)),
                _ => state.skipped += part.len as u64,
            }
        }
    }
    progress(state);

    let mut uploaded: Vec<Part> = Vec::new();
    let mut touched = Instant::now();
    for batch in to_upload.chunks(options.parallel.max(1)) {
        if let Err(cancelled) = cancel.check() {
            let pending: Vec<Uuid> = to_upload.iter().map(|(id, _)| *id).collect();
            let _ = client.cancel(&pending).await;
            return Err(cancelled);
        }
        if touched.elapsed() >= options.keepalive && !uploaded.is_empty() {
            lookup(client, library, &uploaded, options).await?;
            touched = Instant::now();
        }
        upload_batch(client, source, batch, options).await?;
        for (_, part) in batch {
            state.uploaded += part.len as u64;
            uploaded.push(*part);
        }
        progress(state);
    }
    Ok(())
}

async fn lookup(
    client: &Client,
    library: Uuid,
    parts: &[Part],
    options: &UploadOptions,
) -> Result<Vec<api::LookupEntry>, Error> {
    let request = api::Lookup {
        library,
        chunks: parts
            .iter()
            .map(|p| api::LookupChunk {
                hash: p.hash,
                plain_size: p.len as u64,
            })
            .collect(),
    };
    let entries = retry(options.retries, || client.lookup(&request)).await?;
    // In the request's order (the server answers per hash).
    parts
        .iter()
        .map(|p| {
            entries
                .iter()
                .find(|e| e.hash == p.hash)
                .cloned()
                .ok_or_else(|| Error::Decode("a hash missing from the lookup".into()))
        })
        .collect()
}

/// Reads, compresses, declares, sends and announces a batch of chunks.
async fn upload_batch(
    client: &Client,
    source: &impl Source,
    batch: &[(Uuid, Part)],
    options: &UploadOptions,
) -> Result<(), Error> {
    let mut stored: Vec<(api::Declare, Vec<u8>)> = Vec::with_capacity(batch.len());
    for (id, part) in batch {
        let plain = source.read(part.offset, part.len).await?;
        if plain.len() != part.len || ChunkHash::of(&plain) != part.hash {
            return Err(Error::SourceChanged);
        }
        let (compression, bytes) = compress(&options.compression, &plain)
            .map_err(|e| Error::Io(io::Error::other(e.to_string())))?;
        let declare = api::Declare {
            chunk: *id,
            stored_size: bytes.len() as u64,
            compression,
            stored_sha256: Sha256::of(&bytes),
        };
        stored.push((declare, bytes));
    }
    let declarations: Vec<api::Declare> = stored.iter().map(|(d, _)| *d).collect();
    let urls = retry(options.retries, || client.upload_urls(&declarations)).await?;
    let puts = urls.into_iter().map(|url| {
        let (declare, bytes) = stored
            .iter()
            .find(|(d, _)| d.chunk == url.chunk)
            .expect("declared");
        put(client, url, *declare, bytes, options)
    });
    futures::stream::iter(puts)
        .buffer_unordered(options.parallel.max(1))
        .try_collect::<Vec<()>>()
        .await?;
    let ids: Vec<Uuid> = declarations.iter().map(|d| d.chunk).collect();
    let missing = retry(options.retries, || client.announce(&ids)).await?;
    if missing.is_empty() {
        Ok(())
    } else {
        Err(Error::Decode(format!(
            "the storage doesn't have {missing:?} after their upload"
        )))
    }
}

/// Sends one chunk to its presigned URL (with its signed headers). `412`: it
/// was already there. An expired URL (`403`) is requested again, once.
async fn put(
    client: &Client,
    mut url: api::ChunkUrl,
    declare: api::Declare,
    bytes: &[u8],
    options: &UploadOptions,
) -> Result<(), Error> {
    let mut renewed = false;
    loop {
        let result = retry(options.retries, || async {
            let mut request = client.http().put(&url.url).body(bytes.to_vec());
            for (name, value) in &url.headers {
                request = request.header(name, value);
            }
            let status = request.send().await?.status();
            if status.is_success() || status.as_u16() == 412 {
                Ok(())
            } else {
                Err(Error::Storage {
                    chunk: url.chunk,
                    status: status.as_u16(),
                })
            }
        })
        .await;
        match result {
            Err(Error::Storage { status: 403, .. }) if !renewed => {
                renewed = true;
                url = client.upload_urls(&[declare]).await?.remove(0);
            }
            other => return other,
        }
    }
}

fn commit_request(
    target: &Target,
    parts: &[Part],
    ids: &HashMap<ChunkHash, Uuid>,
    options: &UploadOptions,
) -> api::Commit {
    let chunks = parts.iter().map(|p| ids[&p.hash]).collect();
    match target {
        Target::New {
            library,
            parent,
            name,
        } => api::Commit {
            library: *library,
            file: None,
            parent: Some(*parent),
            name: Some(name.clone()),
            base_version: None,
            chunks,
            message: options.message.clone(),
        },
        Target::Version {
            library,
            file,
            base,
        } => api::Commit {
            library: *library,
            file: Some(*file),
            parent: None,
            name: None,
            base_version: *base,
            chunks,
            message: options.message.clone(),
        },
    }
}

/// Runs `request` again on network errors and 5xx, with growing delays.
pub(crate) async fn retry<T, F, Fut>(retries: u32, mut request: F) -> Result<T, Error>
where
    F: FnMut() -> Fut,
    Fut: Future<Output = Result<T, Error>>,
{
    let mut delay = Duration::from_millis(500);
    let mut attempt = 0;
    loop {
        match request().await {
            Err(e) if e.transient() && attempt < retries => {
                attempt += 1;
                rt::sleep(delay).await;
                delay *= 2;
            }
            result => return result,
        }
    }
}
