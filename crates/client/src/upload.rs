//! Uploads: the protocol of docs/decisions.md (Upload flow), once for every
//! client, as a staged engine. Files are submitted; the engine schedules
//! everything, across files and chunks:
//!
//! 1. **Hash**: a few files at once, each chunked (FastCDC) and hashed in one
//!    pass; only hashes and sizes kept.
//! 2. **Lookup**: hashes from any files, in batches: which chunks the server
//!    lacks.
//! 3. **Declare, upload, announce**: a sliding window of chunks declared and
//!    not yet announced (the server's `max_pending_uploads` per account); a
//!    chunk is read again, compressed, declared, sent to its presigned URL
//!    (several at once), announced (which frees its slot).
//! 4. **Commit**: a file once all its chunks are ready, in batches of files;
//!    chunks the server lost meanwhile (`chunk_missing`) go round again.
//!
//! Every batching stage sends on the first of: a full batch, or its oldest
//! item waiting `flush`. Stages pass references (file, offset, length, hash),
//! never bytes: a chunk is read again only to be uploaded. A chunk two files
//! share is uploaded once. A failing file never blocks the others.
//!
//! Resuming is starting again: the lookup skips what's already there.

use std::collections::{HashMap, VecDeque};
use std::io;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use futures::channel::mpsc::TryRecvError;
use futures::channel::{mpsc, oneshot};
use futures::future::{Either, select};
use futures::stream::FuturesUnordered;
use futures::{StreamExt, TryStreamExt};
use theca_common::api;
use theca_common::chunking::AsyncChunker;
use theca_common::compression::{CompressionPolicy, compress};
use theca_common::hash::{ChunkHash, Sha256};
use theca_common::name::Name;
use uuid::Uuid;

use crate::api::{Client, Error};
use crate::rt::{self, MaybeSync, next_batch};
use crate::source::Source;

/// How uploads behave.
#[derive(Debug, Clone)]
pub struct UploadOptions {
    /// Chunks sent at once (each at most 8 MiB in memory).
    pub parallel: usize,
    /// Chunks declared and not yet announced, at most: the server's
    /// `max_pending_uploads` (15 by default), shared by an account's clients.
    pub window: usize,
    /// Files read and hashed at once.
    pub hashing: usize,
    /// How long a batch waits to fill before it's sent anyway.
    pub flush: Duration,
    /// How often uploaded chunks are looked up again during a long upload, so
    /// the server's garbage collection never takes them (it waits 24 h).
    pub keepalive: Duration,
    /// Retries of a request failing on the network or with a 5xx.
    pub retries: u32,
    pub compression: CompressionPolicy,
    /// The versions' description.
    pub message: Option<String>,
}

impl Default for UploadOptions {
    fn default() -> Self {
        Self {
            parallel: 4,
            window: 15,
            hashing: 4,
            flush: Duration::from_millis(20),
            keepalive: Duration::from_secs(6 * 3600),
            retries: 4,
            compression: CompressionPolicy::default(),
            message: None,
        }
    }
}

/// Where a file goes.
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

/// Where the uploads stand, in bytes and files.
#[derive(Debug, Clone, Default)]
pub struct UploadProgress {
    pub total: u64,
    pub hashed: u64,
    pub uploaded: u64,
    /// Already on the server (deduplicated): nothing to send.
    pub skipped: u64,
    /// Files submitted.
    pub files: u64,
    /// Files committed or failed.
    pub files_done: u64,
}

/// Cancels uploads from elsewhere (another task, a UI button).
#[derive(Debug, Clone, Default)]
pub struct Cancel(Arc<AtomicBool>);

impl Cancel {
    pub fn cancel(&self) {
        self.0.store(true, Ordering::Relaxed);
    }

    pub(crate) fn is_cancelled(&self) -> bool {
        self.0.load(Ordering::Relaxed)
    }

    fn check(&self) -> Result<(), Error> {
        if self.is_cancelled() {
            Err(Error::Cancelled)
        } else {
            Ok(())
        }
    }
}

/// A chunk of a source.
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
/// Files per commit request (the server's limit).
const COMMIT_BATCH: usize = 100;
/// Rounds of re-uploading chunks the server lost before committing.
const COMMIT_ROUNDS: usize = 3;
/// How often a cancel is noticed.
const CANCEL_POLL: Duration = Duration::from_millis(100);

/// Uploads `source` to `target`: the engine with one file. `progress` is
/// called as it advances; `cancel` stops it (the pending chunks are cancelled
/// on the server).
pub async fn upload(
    client: &Client,
    source: &impl Source,
    target: Target,
    options: &UploadOptions,
    progress: impl FnMut(&UploadProgress) + MaybeSync,
    cancel: &Cancel,
) -> Result<api::Committed, Error> {
    let (uploader, run) = Uploader::new(client, options, cancel, progress);
    let committed = uploader.submit(source, target);
    drop(uploader);
    futures::join!(run, committed).1
}

/// A chunk, by library and hash (dedup never crosses libraries here).
type Key = (Uuid, ChunkHash);

#[derive(PartialEq)]
enum Status {
    LookingUp,
    Uploading,
    Ready,
}

struct Chunk {
    id: Option<Uuid>,
    status: Status,
    /// Files waiting for it to be ready.
    waiters: Vec<usize>,
    /// Where to read it: a file and its part.
    from: (usize, Part),
    /// The lookup asked us to upload it: a cancel frees it on the server
    /// (unless a committed version uses it).
    ours: bool,
    /// Open files that need it: none left after a cancel, it's dropped.
    users: usize,
}

struct File<'a> {
    source: &'a dyn Source,
    target: Target,
    parts: Vec<Part>,
    /// Its distinct chunks not ready yet.
    waiting: usize,
    rounds: usize,
    /// `None` once resolved.
    done: Option<oneshot::Sender<Result<api::Committed, Error>>>,
}

/// The stages' queues; dropped (closing them, ending the stages) once every
/// submitted file is resolved and nothing more can come.
#[derive(Default)]
struct Queues {
    lookup: Option<mpsc::UnboundedSender<Key>>,
    upload: Option<mpsc::UnboundedSender<Key>>,
    commit: Option<mpsc::UnboundedSender<usize>>,
    // Never sent on: dropping them stops the keepalive and the cancel watch.
    #[allow(dead_code)]
    stop_keepalive: Option<mpsc::UnboundedSender<()>>,
    #[allow(dead_code)]
    stop_cancel: Option<mpsc::UnboundedSender<()>>,
}

struct State<'a, P> {
    files: Vec<File<'a>>,
    chunks: HashMap<Key, Chunk>,
    /// Files not resolved yet.
    open: usize,
    input_closed: bool,
    cancelled: bool,
    /// Chunks of cancelled files to free on the server (the cancel watch
    /// sends them).
    to_free: Vec<Uuid>,
    progress: UploadProgress,
    report: P,
    queues: Queues,
}

impl<P: FnMut(&UploadProgress)> State<'_, P> {
    fn notify(&mut self) {
        (self.report)(&self.progress);
    }

    fn resolve(&mut self, f: usize, result: Result<api::Committed, Error>) {
        if let Some(done) = self.files[f].done.take() {
            let cancelled = matches!(result, Err(Error::Cancelled));
            let _ = done.send(result);
            self.release(f, cancelled);
            self.open -= 1;
            self.progress.files_done += 1;
            self.notify();
            self.finish_if_done();
        }
    }

    /// A file resolved no longer needs its chunks. Cancelled: those no other
    /// open file needs are dropped, and freed on the server if we uploaded
    /// them. (Failed otherwise, they stay: committing them under another name
    /// needs no upload.)
    fn release(&mut self, f: usize, cancelled: bool) {
        let library = self.files[f].target.library();
        let mut seen = Vec::new();
        for part in &self.files[f].parts {
            let key = (library, part.hash);
            if seen.contains(&key) {
                continue;
            }
            seen.push(key);
            let Some(chunk) = self.chunks.get_mut(&key) else {
                continue;
            };
            chunk.users = chunk.users.saturating_sub(1);
            chunk.waiters.retain(|w| *w != f);
            if cancelled && chunk.users == 0 {
                if chunk.ours
                    && let Some(id) = chunk.id
                {
                    self.to_free.push(id);
                }
                self.chunks.remove(&key);
            }
        }
    }

    fn finish_if_done(&mut self) {
        if (self.input_closed && self.open == 0) || self.cancelled {
            self.queues = Queues::default();
        }
    }

    /// A chunk is on the server: the files waiting for it may commit.
    fn ready(&mut self, key: Key) {
        let Some(chunk) = self.chunks.get_mut(&key) else {
            return;
        };
        chunk.status = Status::Ready;
        for f in std::mem::take(&mut chunk.waiters) {
            let file = &mut self.files[f];
            if file.done.is_some() {
                file.waiting -= 1;
                if file.waiting == 0
                    && let Some(queue) = &self.queues.commit
                {
                    let _ = queue.unbounded_send(f);
                }
            }
        }
    }

    /// A chunk can't be uploaded: the files waiting for it fail.
    fn fail(&mut self, key: Key, error: &Error) {
        let waiters = self
            .chunks
            .remove(&key)
            .map(|c| c.waiters)
            .unwrap_or_default();
        for f in waiters {
            self.resolve(f, Err(again(error)));
        }
    }

    /// `key` looked up again (lost by the server, or newly needed by `f`).
    fn look_up(&mut self, key: Key, f: usize, part: Part) {
        let chunk = self.chunks.entry(key).or_insert(Chunk {
            id: None,
            status: Status::Ready,
            waiters: Vec::new(),
            from: (f, part),
            ours: false,
            users: 0,
        });
        chunk.waiters.push(f);
        self.files[f].waiting += 1;
        if chunk.status == Status::Ready {
            chunk.status = Status::LookingUp;
            chunk.id = None;
            if let Some(queue) = &self.queues.lookup {
                let _ = queue.unbounded_send(key);
            }
        }
    }
}

/// The same error again, for every file a failure touches.
fn again(error: &Error) -> Error {
    match error {
        Error::Api { status, error } => Error::Api {
            status: *status,
            error: error.clone(),
        },
        Error::Cancelled => Error::Cancelled,
        Error::SourceChanged => Error::SourceChanged,
        Error::Version => Error::Version,
        Error::Corrupted(id) => Error::Corrupted(*id),
        Error::Storage { chunk, status } => Error::Storage {
            chunk: *chunk,
            status: *status,
        },
        other => Error::Decode(other.to_string()),
    }
}

/// The upload engine: files go in with `submit`, the engine runs them all
/// together while its `run` future is polled (alongside the submissions'
/// results). Dropping the `Uploader` says no more files come: `run` ends once
/// every submitted file is committed or failed.
pub struct Uploader<'a, P> {
    state: Arc<Mutex<State<'a, P>>>,
    files: mpsc::UnboundedSender<usize>,
}

impl<'a, P: FnMut(&UploadProgress) + MaybeSync + 'a> Uploader<'a, P> {
    /// The engine, and the future that runs it.
    pub fn new(
        client: &'a Client,
        options: &'a UploadOptions,
        cancel: &'a Cancel,
        progress: P,
    ) -> (Self, impl Future<Output = ()> + 'a) {
        let (files, hash_rx) = mpsc::unbounded();
        let (lookup, lookup_rx) = mpsc::unbounded();
        let (upload, upload_rx) = mpsc::unbounded();
        let (commit, commit_rx) = mpsc::unbounded();
        let (stop_keepalive, keepalive_rx) = mpsc::unbounded();
        let (stop_cancel, cancel_rx) = mpsc::unbounded();
        let state = Arc::new(Mutex::new(State {
            files: Vec::new(),
            chunks: HashMap::new(),
            open: 0,
            input_closed: false,
            cancelled: false,
            to_free: Vec::new(),
            progress: UploadProgress::default(),
            report: progress,
            queues: Queues {
                lookup: Some(lookup),
                upload: Some(upload),
                commit: Some(commit),
                stop_keepalive: Some(stop_keepalive),
                stop_cancel: Some(stop_cancel),
            },
        }));
        let run = {
            let state = state.clone();
            async move {
                let s = &state;
                futures::join!(
                    hash_stage(s, hash_rx, options, cancel),
                    lookup_stage(s, lookup_rx, client, options),
                    window_stage(s, upload_rx, client, options, cancel),
                    commit_stage(s, commit_rx, client, options),
                    keepalive_stage(s, keepalive_rx, client, options),
                    cancel_watch(s, cancel_rx, client, cancel),
                );
            }
        };
        (Self { state, files }, run)
    }

    /// Adds a file: a handle to await its result (once committed, or
    /// failed), or to cancel it alone.
    pub fn submit(&self, source: &'a dyn Source, target: Target) -> Submitted<'a, P> {
        let (done, result) = oneshot::channel();
        let mut state = self.state.lock().expect("engine state");
        let mut file = None;
        if state.cancelled {
            let _ = done.send(Err(Error::Cancelled));
        } else {
            let f = state.files.len();
            file = Some(f);
            state.files.push(File {
                source,
                target,
                parts: Vec::new(),
                waiting: 0,
                rounds: 0,
                done: Some(done),
            });
            state.open += 1;
            state.progress.files += 1;
            state.progress.total += source.len();
            state.notify();
            let _ = self.files.unbounded_send(f);
        }
        drop(state);
        Submitted {
            state: self.state.clone(),
            file,
            result,
        }
    }
}

/// A submitted file: await it for its result, or cancel it alone (the others
/// go on; its chunks no other file needs are freed on the server).
pub struct Submitted<'a, P> {
    state: Shared<'a, P>,
    file: Option<usize>,
    result: oneshot::Receiver<Result<api::Committed, Error>>,
}

impl<P: FnMut(&UploadProgress)> Submitted<'_, P> {
    pub fn cancel(&self) {
        if let Some(f) = self.file {
            lock(&self.state).resolve(f, Err(Error::Cancelled));
        }
    }
}

impl<P> Future for Submitted<'_, P> {
    type Output = Result<api::Committed, Error>;

    fn poll(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Self::Output> {
        std::pin::Pin::new(&mut self.result)
            .poll(cx)
            .map(|r| r.unwrap_or(Err(Error::Cancelled)))
    }
}

type Shared<'a, P> = Arc<Mutex<State<'a, P>>>;

fn lock<'s, 'a, P>(state: &'s Shared<'a, P>) -> std::sync::MutexGuard<'s, State<'a, P>> {
    state.lock().expect("engine state")
}

/// Stage 1: reads and hashes a few files at once, then registers their chunks.
// ponytail: hashing runs on the engine's task (overlapping the network, not
// other cores); threads if the CPU ever shows in the numbers.
async fn hash_stage<P: FnMut(&UploadProgress)>(
    state: &Shared<'_, P>,
    files: mpsc::UnboundedReceiver<usize>,
    options: &UploadOptions,
    cancel: &Cancel,
) {
    files
        .map(|f| async move {
            let source = lock(state).files[f].source;
            (f, hash(source, state, cancel).await)
        })
        .buffer_unordered(options.hashing.max(1))
        .for_each(|(f, parts)| {
            let mut s = lock(state);
            match parts {
                Err(e) => s.resolve(f, Err(e)),
                // Cancelled while it was being read.
                Ok(_) if s.files[f].done.is_none() => {}
                Ok(parts) => {
                    let library = s.files[f].target.library();
                    s.files[f].parts = parts.clone();
                    let mut seen = Vec::new();
                    for part in parts {
                        let key = (library, part.hash);
                        if seen.contains(&key) {
                            continue;
                        }
                        seen.push(key);
                        match s.chunks.get_mut(&key) {
                            Some(chunk) if chunk.status == Status::Ready => {}
                            Some(chunk) => {
                                chunk.waiters.push(f);
                                s.files[f].waiting += 1;
                            }
                            None => s.look_up(key, f, part),
                        }
                        if let Some(chunk) = s.chunks.get_mut(&key) {
                            chunk.users += 1;
                        }
                    }
                    if s.files[f].waiting == 0
                        && let Some(queue) = &s.queues.commit
                    {
                        let _ = queue.unbounded_send(f);
                    }
                }
            }
            std::future::ready(())
        })
        .await;
    let mut s = lock(state);
    s.input_closed = true;
    s.finish_if_done();
}

/// The hash pass of one file: its chunks, read through the chunker.
async fn hash<P: FnMut(&UploadProgress)>(
    source: &dyn Source,
    state: &Shared<'_, P>,
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
        let mut s = lock(state);
        s.progress.hashed += chunk.data.len() as u64;
        s.notify();
    }
    Ok(parts)
}

/// Stage 2: looks hashes up in batches; what the server lacks goes to the
/// window, what it has is ready.
async fn lookup_stage<P: FnMut(&UploadProgress)>(
    state: &Shared<'_, P>,
    mut keys: mpsc::UnboundedReceiver<Key>,
    client: &Client,
    options: &UploadOptions,
) {
    while let Some(batch) = next_batch(&mut keys, LOOKUP_BATCH, options.flush).await {
        let mut by_library: HashMap<Uuid, Vec<(ChunkHash, u64)>> = HashMap::new();
        {
            let s = lock(state);
            for (library, hash) in batch {
                if let Some(chunk) = s.chunks.get(&(library, hash))
                    && chunk.status == Status::LookingUp
                {
                    by_library
                        .entry(library)
                        .or_default()
                        .push((hash, chunk.from.1.len as u64));
                }
            }
        }
        for (library, hashes) in by_library {
            let request = api::Lookup {
                library,
                chunks: hashes
                    .iter()
                    .map(|(hash, plain_size)| api::LookupChunk {
                        hash: *hash,
                        plain_size: *plain_size,
                    })
                    .collect(),
            };
            let answer = rt::retry(options.retries, || client.lookup(&request)).await;
            let mut s = lock(state);
            match answer {
                Err(e) => {
                    for (hash, _) in hashes {
                        s.fail((library, hash), &e);
                    }
                }
                Ok(entries) => {
                    for (hash, size) in hashes {
                        let key = (library, hash);
                        let Some(entry) = entries.iter().find(|e| e.hash == hash) else {
                            s.fail(key, &Error::Decode("a hash missing from the lookup".into()));
                            continue;
                        };
                        let Some(chunk) = s.chunks.get_mut(&key) else {
                            continue;
                        };
                        chunk.id = Some(entry.chunk);
                        if entry.state == api::ChunkState::Upload {
                            chunk.status = Status::Uploading;
                            chunk.ours = true;
                            if let Some(queue) = &s.queues.upload {
                                let _ = queue.unbounded_send(key);
                            }
                        } else {
                            s.progress.skipped += size;
                            s.ready(key);
                        }
                    }
                    s.notify();
                }
            }
        }
    }
}

/// A chunk declared, waiting for its turn to be sent.
struct Declared {
    key: Key,
    url: api::ChunkUrl,
    declare: api::Declare,
    bytes: Vec<u8>,
    plain: u64,
}

/// Stage 3: the window. Chunks are declared into the free slots, sent several
/// at once, and announced early (an announce frees their slots).
async fn window_stage<P: FnMut(&UploadProgress)>(
    state: &Shared<'_, P>,
    mut keys: mpsc::UnboundedReceiver<Key>,
    client: &Client,
    options: &UploadOptions,
    cancel: &Cancel,
) {
    let window = options.window.max(1);
    let parallel = options.parallel.clamp(1, window);
    let mut in_flight = 0usize;
    let mut closed = false;
    let mut incoming: Vec<Key> = Vec::new();
    let mut waiting: VecDeque<Declared> = VecDeque::new();
    let mut puts = FuturesUnordered::new();
    let mut sent: Vec<(Key, Uuid, u64)> = Vec::new();
    loop {
        if cancel.is_cancelled() {
            return;
        }
        // New chunks into the free slots: wait for some only when idle.
        let free = window.saturating_sub(in_flight + incoming.len());
        if free > 0 && !closed {
            if in_flight == 0 && incoming.is_empty() {
                match next_batch(&mut keys, free, options.flush).await {
                    Some(batch) => incoming.extend(batch),
                    None => closed = true,
                }
            } else {
                while incoming.len() < window - in_flight {
                    match keys.try_recv() {
                        Ok(key) => incoming.push(key),
                        Err(TryRecvError::Closed) => {
                            closed = true;
                            break;
                        }
                        Err(TryRecvError::Empty) => break,
                    }
                }
            }
        }
        if !incoming.is_empty() {
            let batch = std::mem::take(&mut incoming);
            in_flight += batch.len();
            match declare(state, client, &batch, options, cancel).await {
                Ok(declared) => {
                    let accepted: Vec<Key> = declared.iter().map(|d| d.key).collect();
                    in_flight -= batch.iter().filter(|k| !accepted.contains(k)).count();
                    waiting.extend(declared);
                }
                Err(e) => {
                    in_flight -= batch.len();
                    let mut s = lock(state);
                    for key in batch {
                        s.fail(key, &e);
                    }
                }
            }
        }
        while puts.len() < parallel
            && let Some(d) = waiting.pop_front()
        {
            puts.push(async move {
                let result = put(client, d.url, d.declare, &d.bytes, options).await;
                (d.key, d.declare.chunk, d.plain, result)
            });
        }
        if puts.is_empty() {
            if !sent.is_empty() {
                in_flight -= announce(state, client, &mut sent, options).await;
                continue;
            }
            if closed && in_flight == 0 {
                return;
            }
            continue;
        }
        // A send finishing, or (with free slots) new chunks arriving.
        let finished = if window > in_flight && !closed {
            match select(puts.next(), keys.next()).await {
                Either::Left((finished, _)) => finished,
                Either::Right((Some(key), _)) => {
                    incoming.push(key);
                    continue;
                }
                Either::Right((None, _)) => {
                    closed = true;
                    continue;
                }
            }
        } else {
            puts.next().await
        };
        if let Some((key, id, plain, result)) = finished {
            match result {
                Ok(()) => sent.push((key, id, plain)),
                Err(e) => {
                    in_flight -= 1;
                    lock(state).fail(key, &e);
                }
            }
        }
        // Announce early: the slots it frees are the window's throughput.
        if sent.len() >= parallel.div_ceil(2) || puts.is_empty() {
            in_flight -= announce(state, client, &mut sent, options).await;
        }
    }
}

/// Reads, checks and compresses chunks, and declares them (waiting while the
/// account's other uploads fill the server's window). The declared ones.
async fn declare<P: FnMut(&UploadProgress)>(
    state: &Shared<'_, P>,
    client: &Client,
    keys: &[Key],
    options: &UploadOptions,
    cancel: &Cancel,
) -> Result<Vec<Declared>, Error> {
    let mut ready = Vec::new();
    for key in keys {
        let (source, part, id) = {
            let s = lock(state);
            // Gone, or no open file needs it anymore (cancelled).
            let Some(chunk) = s.chunks.get(key).filter(|c| c.users > 0) else {
                continue;
            };
            let (f, part) = chunk.from;
            (s.files[f].source, part, chunk.id.expect("looked up"))
        };
        let plain = match source.read(part.offset, part.len).await {
            Ok(plain) if plain.len() == part.len && ChunkHash::of(&plain) == part.hash => plain,
            Ok(_) => {
                lock(state).fail(*key, &Error::SourceChanged);
                continue;
            }
            Err(e) => {
                lock(state).fail(*key, &Error::Io(e));
                continue;
            }
        };
        let (compression, bytes) = compress(&options.compression, &plain)
            .map_err(|e| Error::Io(io::Error::other(e.to_string())))?;
        let declare = api::Declare {
            chunk: id,
            stored_size: bytes.len() as u64,
            compression,
            stored_sha256: Sha256::of(&bytes),
        };
        ready.push((*key, declare, bytes, part.len as u64));
    }
    if ready.is_empty() {
        return Ok(Vec::new());
    }
    let declarations: Vec<api::Declare> = ready.iter().map(|(_, d, _, _)| *d).collect();
    let urls = loop {
        cancel.check()?;
        match rt::retry(options.retries, || client.upload_urls(&declarations)).await {
            // The account's other uploads hold the window: wait for slots.
            Err(Error::Api { status: 429, .. }) => rt::sleep(Duration::from_secs(1)).await,
            other => break other?,
        }
    };
    let mut declared = Vec::with_capacity(ready.len());
    for (key, declare, bytes, plain) in ready {
        match urls.iter().find(|u| u.chunk == declare.chunk) {
            Some(url) => declared.push(Declared {
                key,
                url: url.clone(),
                declare,
                bytes,
                plain,
            }),
            None => lock(state).fail(key, &Error::Decode("a chunk without an upload URL".into())),
        }
    }
    Ok(declared)
}

/// Announces the sent chunks; ready ones free their files. How many slots it
/// freed.
async fn announce<P: FnMut(&UploadProgress)>(
    state: &Shared<'_, P>,
    client: &Client,
    sent: &mut Vec<(Key, Uuid, u64)>,
    options: &UploadOptions,
) -> usize {
    let batch = std::mem::take(sent);
    if batch.is_empty() {
        return 0;
    }
    let ids: Vec<Uuid> = batch.iter().map(|(_, id, _)| *id).collect();
    let answer = rt::retry(options.retries, || client.announce(&ids)).await;
    let mut s = lock(state);
    match answer {
        Ok(missing) => {
            for (key, id, plain) in &batch {
                if missing.contains(id) {
                    s.fail(
                        *key,
                        &Error::Decode(format!("the storage doesn't have {id} after its upload")),
                    );
                } else {
                    // Uploaded means confirmed: a resumed upload reuses it.
                    s.progress.uploaded += plain;
                    s.ready(*key);
                }
            }
            s.notify();
        }
        Err(e) => {
            for (key, _, _) in &batch {
                s.fail(*key, &e);
            }
        }
    }
    batch.len()
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
        let result = rt::retry(options.retries, || async {
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

/// Stage 4: commits ready files in batches. Chunks the server lost go round
/// again (new IDs).
async fn commit_stage<P: FnMut(&UploadProgress)>(
    state: &Shared<'_, P>,
    mut files: mpsc::UnboundedReceiver<usize>,
    client: &Client,
    options: &UploadOptions,
) {
    while let Some(batch) = next_batch(&mut files, COMMIT_BATCH, options.flush).await {
        let commits: Vec<(usize, api::Commit)> = {
            let s = lock(state);
            batch
                .into_iter()
                .filter(|f| s.files[*f].done.is_some())
                .map(|f| (f, commit_request(&s, f, options)))
                .collect()
        };
        if commits.is_empty() {
            continue;
        }
        let requests: Vec<api::Commit> = commits.iter().map(|(_, c)| c.clone()).collect();
        let answer = rt::retry(options.retries, || client.commit(&requests)).await;
        let mut s = lock(state);
        let outcomes = match answer {
            Ok(outcomes) => outcomes,
            Err(e) => {
                for (f, _) in commits {
                    s.resolve(f, Err(again(&e)));
                }
                continue;
            }
        };
        for ((f, commit), outcome) in commits.into_iter().zip(outcomes) {
            match outcome {
                Err(Error::Api { error, .. })
                    if error.code == "chunk_missing" && s.files[f].rounds + 1 < COMMIT_ROUNDS =>
                {
                    // Lost meanwhile (garbage-collected, or cancelled by a
                    // parallel upload of the same content): those again;
                    // without a list, every chunk of the file.
                    s.files[f].rounds += 1;
                    let library = s.files[f].target.library();
                    let lost: Vec<Part> = s.files[f]
                        .parts
                        .iter()
                        .zip(&commit.chunks)
                        .filter(|(_, id)| error.chunks.is_empty() || error.chunks.contains(id))
                        .map(|(p, _)| *p)
                        .collect();
                    let mut again = Vec::new();
                    for part in lost {
                        if !again.iter().any(|p: &Part| p.hash == part.hash) {
                            again.push(part);
                        }
                    }
                    for part in again {
                        let key = (library, part.hash);
                        if let Some(chunk) = s.chunks.get_mut(&key)
                            && chunk.status == Status::Ready
                        {
                            chunk.from = (f, part);
                        }
                        s.look_up(key, f, part);
                    }
                }
                result => s.resolve(f, result),
            }
        }
    }
}

fn commit_request<P>(state: &State<'_, P>, f: usize, options: &UploadOptions) -> api::Commit {
    let file = &state.files[f];
    let library = file.target.library();
    let chunks = file
        .parts
        .iter()
        .map(|p| state.chunks[&(library, p.hash)].id.expect("ready"))
        .collect();
    match &file.target {
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

/// During long uploads: the chunks of files not committed yet are looked up
/// again every `keepalive`, so the server's garbage collection never takes
/// them.
async fn keepalive_stage<P: FnMut(&UploadProgress)>(
    state: &Shared<'_, P>,
    mut stop: mpsc::UnboundedReceiver<()>,
    client: &Client,
    options: &UploadOptions,
) {
    loop {
        match select(Box::pin(rt::sleep(options.keepalive)), stop.next()).await {
            Either::Left(_) => {}
            Either::Right(_) => return,
        }
        let mut by_library: HashMap<Uuid, Vec<api::LookupChunk>> = HashMap::new();
        {
            let s = lock(state);
            for file in s.files.iter().filter(|f| f.done.is_some()) {
                for part in &file.parts {
                    let key = (file.target.library(), part.hash);
                    if s.chunks
                        .get(&key)
                        .is_some_and(|c| c.status == Status::Ready)
                    {
                        by_library.entry(key.0).or_default().push(api::LookupChunk {
                            hash: part.hash,
                            plain_size: part.len as u64,
                        });
                    }
                }
            }
        }
        for (library, chunks) in by_library {
            for batch in chunks.chunks(LOOKUP_BATCH) {
                let request = api::Lookup {
                    library,
                    chunks: batch.to_vec(),
                };
                let _ = client.lookup(&request).await;
            }
        }
    }
}

/// Notices a cancel: every open file fails, the stages close, and the chunks
/// still pending are cancelled on the server (freeing them and their quota).
async fn cancel_watch<P: FnMut(&UploadProgress)>(
    state: &Shared<'_, P>,
    mut stop: mpsc::UnboundedReceiver<()>,
    client: &Client,
    cancel: &Cancel,
) {
    // ponytail: polled; a waker in `Cancel` if a tenth of a second matters.
    loop {
        free(state, client).await;
        if cancel.is_cancelled() {
            break;
        }
        match select(Box::pin(rt::sleep(CANCEL_POLL)), stop.next()).await {
            Either::Left(_) => {}
            Either::Right(_) => {
                free(state, client).await;
                return;
            }
        }
    }
    let pending: Vec<Uuid> = {
        let mut s = lock(state);
        s.cancelled = true;
        let pending = s
            .chunks
            .values()
            .filter(|c| c.ours)
            .filter_map(|c| c.id)
            .collect();
        for f in 0..s.files.len() {
            s.resolve(f, Err(Error::Cancelled));
        }
        s.finish_if_done();
        pending
    };
    if !pending.is_empty() {
        let _ = client.cancel(&pending).await;
    }
    lock(state).to_free.clear();
}

/// Frees the chunks of cancelled files on the server.
async fn free<P>(state: &Shared<'_, P>, client: &Client) {
    let ids = std::mem::take(&mut lock(state).to_free);
    if !ids.is_empty() {
        let _ = client.cancel(&ids).await;
    }
}
