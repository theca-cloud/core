//! Downloads (native): a staged engine, like uploads. Files are submitted;
//! their manifests are fetched in batches, their chunks several at once from
//! any files (presigned URLs), decompressed, checked against their hash, and
//! written at their offset: they may arrive in any order. A file is complete
//! once all its chunks are written. (The web app downloads through the
//! server's streaming endpoint in V1.)

use std::io::{self, BufReader, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use futures::StreamExt;
use futures::channel::{mpsc, oneshot};
use futures::future::{Either, select};
use theca_common::api;
use theca_common::chunking::chunks;
use theca_common::compression::decompress;
use theca_common::hash::ChunkHash;
use uuid::Uuid;

use crate::api::{Client, Error};
use crate::rt::{self, MaybeSync, next_batch, retry};
use crate::upload::Cancel;

/// How downloads behave.
#[derive(Debug, Clone)]
pub struct DownloadOptions {
    /// Chunks fetched at once (each at most 8 MiB in memory).
    pub parallel: usize,
    /// Check every chunk against its hash: a lying server or storage is
    /// caught. Costs little (`xxh3_128` runs at tens of GB/s).
    pub verify: bool,
    /// Leave a destination that already holds the version untouched (it's
    /// read and hashed, nothing is downloaded).
    pub skip_identical: bool,
    /// How long a batch of files waits to fill before it's sent anyway.
    pub flush: Duration,
    /// Retries of a request failing on the network or with a 5xx.
    pub retries: u32,
}

impl Default for DownloadOptions {
    fn default() -> Self {
        Self {
            parallel: 8,
            verify: true,
            skip_identical: false,
            flush: Duration::from_millis(20),
            retries: 4,
        }
    }
}

/// Where the downloads stand, in bytes and files.
#[derive(Debug, Clone, Default)]
pub struct DownloadProgress {
    /// The versions' sizes, as their manifests arrive.
    pub total: u64,
    pub written: u64,
    /// Already identical at their destination: nothing to download.
    pub skipped: u64,
    pub files: u64,
    pub files_done: u64,
}

/// A file downloaded.
#[derive(Debug, Clone, Copy)]
pub struct Downloaded {
    /// The version written.
    pub version: Uuid,
    /// The destination already held it: nothing was written.
    pub unchanged: bool,
}

/// Where a download goes. Chunks are written at their offset, in any order.
pub trait Sink: MaybeSync {
    /// Whether it already holds this content (chunk hashes and sizes, in
    /// order): then nothing is downloaded (`skip_identical`).
    fn holds(&self, _chunks: &[(ChunkHash, u64)]) -> io::Result<bool> {
        Ok(false)
    }

    fn write_at(&self, offset: u64, data: &[u8]) -> io::Result<()>;

    /// Every chunk written: the content is complete.
    fn finish(&self) -> io::Result<()>;

    /// Failed or cancelled: what was written goes.
    fn abort(&self);
}

/// A local file, written as `<name>.theca-part` and renamed over `<name>` once
/// complete: an interrupted download never leaves a truncated file under the
/// real name. Opened at the first write, closed at the end.
pub struct FileSink {
    target: PathBuf,
    part: PathBuf,
    file: Mutex<Option<std::fs::File>>,
}

impl FileSink {
    pub fn new(target: &Path) -> Self {
        let mut part = target
            .file_name()
            .map_or_else(Default::default, |n| n.to_owned());
        part.push(".theca-part");
        Self {
            target: target.to_owned(),
            part: target.with_file_name(part),
            file: Mutex::new(None),
        }
    }
}

impl Sink for FileSink {
    /// The local file chunked and hashed as an upload would be, compared.
    fn holds(&self, expected: &[(ChunkHash, u64)]) -> io::Result<bool> {
        let Ok(meta) = std::fs::metadata(&self.target) else {
            return Ok(false);
        };
        let size: u64 = expected.iter().map(|(_, size)| size).sum();
        if !meta.is_file() || meta.len() != size {
            return Ok(false);
        }
        let mut expected = expected.iter();
        for chunk in chunks(BufReader::new(std::fs::File::open(&self.target)?)) {
            let chunk = chunk?;
            match expected.next() {
                Some((hash, _)) if *hash == ChunkHash::of(&chunk.data) => {}
                _ => return Ok(false),
            }
        }
        Ok(expected.next().is_none())
    }

    fn write_at(&self, offset: u64, data: &[u8]) -> io::Result<()> {
        let mut file = self.file.lock().map_err(|_| io::Error::other("poisoned"))?;
        if file.is_none() {
            *file = Some(std::fs::File::create(&self.part)?);
        }
        let file = file.as_mut().expect("opened above");
        file.seek(SeekFrom::Start(offset))?;
        file.write_all(data)
    }

    fn finish(&self) -> io::Result<()> {
        let mut file = self.file.lock().map_err(|_| io::Error::other("poisoned"))?;
        match file.take() {
            Some(mut file) => file.flush()?,
            // An empty file: nothing was written.
            None => drop(std::fs::File::create(&self.part)?),
        }
        std::fs::rename(&self.part, &self.target)
    }

    fn abort(&self) {
        if let Ok(mut file) = self.file.lock()
            && file.take().is_some()
        {
            let _ = std::fs::remove_file(&self.part);
        }
    }
}

/// Bytes in memory (tests).
#[derive(Default)]
pub struct MemorySink(pub Mutex<Vec<u8>>);

impl Sink for MemorySink {
    fn write_at(&self, offset: u64, data: &[u8]) -> io::Result<()> {
        let mut bytes = self.0.lock().map_err(|_| io::Error::other("poisoned"))?;
        let end = offset as usize + data.len();
        if bytes.len() < end {
            bytes.resize(end, 0);
        }
        bytes[offset as usize..end].copy_from_slice(data);
        Ok(())
    }

    fn finish(&self) -> io::Result<()> {
        Ok(())
    }

    fn abort(&self) {}
}

/// Downloads `file` (its current version, or `version`) into `sink`: the
/// engine with one file.
pub async fn download(
    client: &Client,
    file: Uuid,
    version: Option<Uuid>,
    sink: &impl Sink,
    options: &DownloadOptions,
    progress: impl FnMut(&DownloadProgress) + MaybeSync,
    cancel: &Cancel,
) -> Result<Downloaded, Error> {
    let (downloader, run) = Downloader::new(client, options, cancel, progress);
    let downloaded = downloader.submit(file, version, sink);
    drop(downloader);
    futures::join!(run, downloaded).1
}

struct File<'a> {
    request: api::DownloadFile,
    sink: &'a dyn Sink,
    version: Option<Uuid>,
    /// Chunks not written yet.
    remaining: usize,
    /// `None` once resolved.
    done: Option<oneshot::Sender<Result<Downloaded, Error>>>,
}

/// A chunk to fetch, and where it goes.
struct Job {
    file: usize,
    offset: u64,
    chunk: api::DownloadChunk,
}

#[derive(Default)]
struct Queues {
    fetch: Option<mpsc::UnboundedSender<Job>>,
    // Never sent on: dropping it stops the cancel watch.
    #[allow(dead_code)]
    stop: Option<mpsc::UnboundedSender<()>>,
}

struct State<'a, P> {
    files: Vec<File<'a>>,
    open: usize,
    input_closed: bool,
    cancelled: bool,
    progress: DownloadProgress,
    report: P,
    queues: Queues,
}

impl<P: FnMut(&DownloadProgress)> State<'_, P> {
    fn notify(&mut self) {
        (self.report)(&self.progress);
    }

    fn resolve(&mut self, f: usize, result: Result<Downloaded, Error>) {
        let file = &mut self.files[f];
        if let Some(done) = file.done.take() {
            if result.is_err() {
                file.sink.abort();
            }
            let _ = done.send(result);
            self.open -= 1;
            self.progress.files_done += 1;
            self.notify();
            if (self.input_closed && self.open == 0) || self.cancelled {
                self.queues = Queues::default();
            }
        }
    }
}

type Shared<'a, P> = Arc<Mutex<State<'a, P>>>;

fn lock<'s, 'a, P>(state: &'s Shared<'a, P>) -> std::sync::MutexGuard<'s, State<'a, P>> {
    state.lock().expect("engine state")
}

/// The download engine: files go in with `submit`, the engine runs them all
/// together while its `run` future is polled. Dropping the `Downloader` says
/// no more files come: `run` ends once every file is written or failed.
pub struct Downloader<'a, P> {
    state: Shared<'a, P>,
    files: mpsc::UnboundedSender<usize>,
}

impl<'a, P: FnMut(&DownloadProgress) + MaybeSync + 'a> Downloader<'a, P> {
    /// The engine, and the future that runs it.
    pub fn new(
        client: &'a Client,
        options: &'a DownloadOptions,
        cancel: &'a Cancel,
        progress: P,
    ) -> (Self, impl Future<Output = ()> + 'a) {
        let (files, manifest_rx) = mpsc::unbounded();
        let (fetch, fetch_rx) = mpsc::unbounded();
        let (stop, stop_rx) = mpsc::unbounded();
        let state = Arc::new(Mutex::new(State {
            files: Vec::new(),
            open: 0,
            input_closed: false,
            cancelled: false,
            progress: DownloadProgress::default(),
            report: progress,
            queues: Queues {
                fetch: Some(fetch),
                stop: Some(stop),
            },
        }));
        let run = {
            let state = state.clone();
            async move {
                let s = &state;
                futures::join!(
                    manifest_stage(s, manifest_rx, client, options),
                    fetch_stage(s, fetch_rx, client, options),
                    cancel_watch(s, stop_rx, cancel),
                );
            }
        };
        (Self { state, files }, run)
    }

    /// Adds a file (its current version, or `version`): a handle to await its
    /// result, or to cancel it alone.
    pub fn submit(&self, file: Uuid, version: Option<Uuid>, sink: &'a dyn Sink) -> Fetched<'a, P> {
        let (done, result) = oneshot::channel();
        let mut state = lock(&self.state);
        let mut index = None;
        if state.cancelled {
            let _ = done.send(Err(Error::Cancelled));
        } else {
            let f = state.files.len();
            index = Some(f);
            state.files.push(File {
                request: api::DownloadFile { file, version },
                sink,
                version: None,
                remaining: 0,
                done: Some(done),
            });
            state.open += 1;
            state.progress.files += 1;
            state.notify();
            let _ = self.files.unbounded_send(f);
        }
        drop(state);
        Fetched {
            state: self.state.clone(),
            file: index,
            result,
        }
    }
}

/// A submitted file: await it for its result, or cancel it alone.
pub struct Fetched<'a, P> {
    state: Shared<'a, P>,
    file: Option<usize>,
    result: oneshot::Receiver<Result<Downloaded, Error>>,
}

impl<P: FnMut(&DownloadProgress)> Fetched<'_, P> {
    pub fn cancel(&self) {
        if let Some(f) = self.file {
            lock(&self.state).resolve(f, Err(Error::Cancelled));
        }
    }
}

impl<P> Future for Fetched<'_, P> {
    type Output = Result<Downloaded, Error>;

    fn poll(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Self::Output> {
        std::pin::Pin::new(&mut self.result)
            .poll(cx)
            .map(|r| r.unwrap_or(Err(Error::Cancelled)))
    }
}

/// Most files per manifest request (the server's limit).
const MANIFEST_BATCH: usize = 100;
/// How often a cancel is noticed.
const CANCEL_POLL: Duration = Duration::from_millis(100);

/// Stage 1: manifests in batches. A file already identical at its destination
/// is done; the others' chunks go to the fetch stage.
// ponytail: the identical check hashes on the engine's task, one file after
// another; threads if "down again" ever shows it.
async fn manifest_stage<P: FnMut(&DownloadProgress)>(
    state: &Shared<'_, P>,
    mut files: mpsc::UnboundedReceiver<usize>,
    client: &Client,
    options: &DownloadOptions,
) {
    while let Some(batch) = next_batch(&mut files, MANIFEST_BATCH, options.flush).await {
        let requests: Vec<(usize, api::DownloadFile)> = {
            let s = lock(state);
            batch
                .into_iter()
                .filter(|f| s.files[*f].done.is_some())
                .map(|f| (f, s.files[f].request))
                .collect()
        };
        if requests.is_empty() {
            continue;
        }
        let list: Vec<api::DownloadFile> = requests.iter().map(|(_, r)| *r).collect();
        let outcomes = match retry(options.retries, || client.manifests(&list)).await {
            Ok(outcomes) => outcomes,
            Err(e) => {
                let mut s = lock(state);
                for (f, _) in requests {
                    s.resolve(f, Err(again(&e)));
                }
                continue;
            }
        };
        for ((f, _), outcome) in requests.into_iter().zip(outcomes) {
            let manifest = match outcome {
                Ok(manifest) => manifest,
                Err(e) => {
                    lock(state).resolve(f, Err(e));
                    continue;
                }
            };
            let size: u64 = manifest.chunks.iter().map(|c| c.plain_size).sum();
            let sink = lock(state).files[f].sink;
            if options.skip_identical {
                let expected: Vec<(ChunkHash, u64)> = manifest
                    .chunks
                    .iter()
                    .map(|c| (c.hash, c.plain_size))
                    .collect();
                if sink.holds(&expected).unwrap_or(false) {
                    let mut s = lock(state);
                    s.progress.total += size;
                    s.progress.skipped += size;
                    s.resolve(
                        f,
                        Ok(Downloaded {
                            version: manifest.version,
                            unchanged: true,
                        }),
                    );
                    continue;
                }
            }
            let mut s = lock(state);
            if s.files[f].done.is_none() {
                continue;
            }
            s.progress.total += size;
            s.files[f].version = Some(manifest.version);
            s.files[f].remaining = manifest.chunks.len();
            if manifest.chunks.is_empty() {
                let result = sink.finish().map_err(Error::Io).map(|()| Downloaded {
                    version: manifest.version,
                    unchanged: false,
                });
                s.resolve(f, result);
                continue;
            }
            s.notify();
            let mut offset = 0;
            for chunk in manifest.chunks {
                let len = chunk.plain_size;
                if let Some(queue) = &s.queues.fetch {
                    let _ = queue.unbounded_send(Job {
                        file: f,
                        offset,
                        chunk,
                    });
                }
                offset += len;
            }
        }
    }
    let mut s = lock(state);
    s.input_closed = true;
    if s.open == 0 {
        s.queues = Queues::default();
    }
}

/// Stage 2: chunks fetched several at once, from any files, each checked and
/// written at its offset; a file is complete once all its chunks are written.
async fn fetch_stage<P: FnMut(&DownloadProgress)>(
    state: &Shared<'_, P>,
    jobs: mpsc::UnboundedReceiver<Job>,
    client: &Client,
    options: &DownloadOptions,
) {
    jobs.map(|job| async move {
        let open = lock(state).files[job.file].done.is_some();
        let result = if open {
            fetch(state, client, &job, options).await
        } else {
            Ok(Vec::new())
        };
        (job, open, result)
    })
    .buffer_unordered(options.parallel.max(1))
    .for_each(|(job, open, result)| {
        if open {
            let mut s = lock(state);
            let f = job.file;
            let written = result.and_then(|plain| {
                s.files[f]
                    .sink
                    .write_at(job.offset, &plain)
                    .map_err(Error::Io)
                    .map(|()| plain.len() as u64)
            });
            match written {
                Ok(len) if s.files[f].done.is_some() => {
                    s.progress.written += len;
                    s.files[f].remaining -= 1;
                    if s.files[f].remaining == 0 {
                        let version = s.files[f].version.expect("from its manifest");
                        let result =
                            s.files[f]
                                .sink
                                .finish()
                                .map_err(Error::Io)
                                .map(|()| Downloaded {
                                    version,
                                    unchanged: false,
                                });
                        s.resolve(f, result);
                    } else {
                        s.notify();
                    }
                }
                Ok(_) => {}
                Err(e) => s.resolve(f, Err(e)),
            }
        }
        std::future::ready(())
    })
    .await;
}

/// One chunk: fetched from its presigned URL (an expired one is asked again
/// with the file's manifest, once), decompressed, checked.
async fn fetch<P: FnMut(&DownloadProgress)>(
    state: &Shared<'_, P>,
    client: &Client,
    job: &Job,
    options: &DownloadOptions,
) -> Result<Vec<u8>, Error> {
    let mut url = job.chunk.url.clone();
    let mut renewed = false;
    let stored = loop {
        let result = retry(options.retries, || async {
            let response = client.http().get(&url).send().await?;
            let status = response.status();
            if !status.is_success() {
                return Err(Error::Storage {
                    chunk: job.chunk.chunk,
                    status: status.as_u16(),
                });
            }
            Ok(response.bytes().await?)
        })
        .await;
        match result {
            // Expired (the manifest was fetched long ago): a fresh one.
            Err(Error::Storage { status: 403, .. }) if !renewed => {
                renewed = true;
                let request = {
                    let s = lock(state);
                    let file = &s.files[job.file];
                    api::DownloadFile {
                        file: file.request.file,
                        version: file.version,
                    }
                };
                let manifest = client.manifests(&[request]).await?.remove(0)?;
                url = manifest
                    .chunks
                    .into_iter()
                    .find(|c| c.chunk == job.chunk.chunk)
                    .ok_or_else(|| Error::Decode("a chunk gone from its manifest".into()))?
                    .url;
            }
            other => break other?,
        }
    };
    let plain = decompress(job.chunk.compression, &stored)
        .map_err(|_| Error::Corrupted(job.chunk.chunk))?;
    if options.verify
        && (plain.len() as u64 != job.chunk.plain_size || ChunkHash::of(&plain) != job.chunk.hash)
    {
        return Err(Error::Corrupted(job.chunk.chunk));
    }
    Ok(plain)
}

/// Notices a cancel: every open file fails (its partial content dropped) and
/// the stages close.
// ponytail: polled; a waker in `Cancel` if a tenth of a second matters.
async fn cancel_watch<P: FnMut(&DownloadProgress)>(
    state: &Shared<'_, P>,
    mut stop: mpsc::UnboundedReceiver<()>,
    cancel: &Cancel,
) {
    loop {
        if cancel.is_cancelled() {
            break;
        }
        match select(Box::pin(rt::sleep(CANCEL_POLL)), stop.next()).await {
            Either::Left(_) => {}
            Either::Right(_) => return,
        }
    }
    let mut s = lock(state);
    s.cancelled = true;
    for f in 0..s.files.len() {
        s.resolve(f, Err(Error::Cancelled));
    }
    s.queues = Queues::default();
}

/// The same error again, for every file a failure touches.
fn again(error: &Error) -> Error {
    match error {
        Error::Api { status, error } => Error::Api {
            status: *status,
            error: error.clone(),
        },
        Error::Cancelled => Error::Cancelled,
        other => Error::Decode(other.to_string()),
    }
}
