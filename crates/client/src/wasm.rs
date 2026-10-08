//! The browser package (`wasm-pack`): a thin API over the engines for the web
//! app, which runs it in a Web Worker. The session cookie goes with every
//! request (`credentials: include`); JavaScript never sees it. Errors reach
//! JavaScript as `{ code, message }` (`Error::kind`).

use std::cell::RefCell;
use std::collections::HashMap;
use std::io;

use futures::channel::{mpsc, oneshot};
use futures::future::{Either, join_all, select};
use futures::{StreamExt, TryStreamExt};
use serde::Serialize;
use uuid::Uuid;
use wasm_bindgen::prelude::*;
use wasm_bindgen_futures::JsFuture;

use crate::Client;
use crate::api::Error;
use crate::download::{DownloadOptions, stream};
use crate::place::place;
use crate::rt::BoxFuture;
use crate::source::Source;
use crate::upload::{Cancel, UploadOptions, UploadProgress, Uploader};

/// A browser `Blob` (a `File` from an `<input>` or a drop), read by slices.
struct BlobSource(web_sys::Blob);

impl Source for BlobSource {
    fn len(&self) -> u64 {
        self.0.size() as u64
    }

    /// A `File`'s `lastModified` (milliseconds); a bare `Blob` has none.
    fn modified(&self) -> Option<chrono::DateTime<chrono::Utc>> {
        use wasm_bindgen::JsCast;
        let file = self.0.dyn_ref::<web_sys::File>()?;
        chrono::DateTime::from_timestamp_millis(file.last_modified() as i64)
    }

    fn read(&self, offset: u64, len: usize) -> BoxFuture<'_, io::Result<Vec<u8>>> {
        Box::pin(async move {
            let end = (offset + len as u64).min(self.len());
            let slice = self
                .0
                .slice_with_f64_and_f64(offset as f64, end as f64)
                .map_err(|e| io::Error::other(format!("{e:?}")))?;
            let buffer = JsFuture::from(slice.array_buffer())
                .await
                .map_err(|e| io::Error::other(format!("{e:?}")))?;
            Ok(js_sys::Uint8Array::new(&buffer).to_vec())
        })
    }
}

/// What `upload` reports to its `on_event`.
#[derive(Serialize)]
#[serde(tag = "kind", rename_all = "lowercase")]
enum Event<'a> {
    /// Every file's bytes together.
    Progress {
        total: u64,
        hashed: u64,
        uploaded: u64,
        skipped: u64,
    },
    /// A file committed.
    Done {
        path: &'a str,
        file: Uuid,
        version: Uuid,
        conflict: bool,
        unchanged: bool,
    },
    /// A file failed (the others go on).
    Failed {
        path: &'a str,
        code: &'a str,
        message: String,
    },
}

fn emit(on_event: &js_sys::Function, event: &Event) {
    if let Ok(json) = serde_json::to_string(event)
        && let Ok(value) = js_sys::JSON::parse(&json)
    {
        let _ = on_event.call1(&JsValue::NULL, &value);
    }
}

fn failed(on_event: &js_sys::Function, path: &str, e: &Error) {
    emit(
        on_event,
        &Event::Failed {
            path,
            code: e.kind(),
            message: e.to_string(),
        },
    );
}

/// An error as JavaScript gets it: `{ code, message }`.
fn js_error(e: &Error) -> JsValue {
    let json = serde_json::json!({ "code": e.kind(), "message": e.to_string() }).to_string();
    js_sys::JSON::parse(&json).unwrap_or(JsValue::NULL)
}

fn uuid(value: &str) -> Result<Uuid, JsValue> {
    Uuid::parse_str(value).map_err(|e| js_error(&Error::Decode(e.to_string())))
}

/// A cancel for an upload run: one file (`Some(path)`), or all (`None`).
type CancelSender = mpsc::UnboundedSender<Option<String>>;

/// The client for the web app.
#[wasm_bindgen]
pub struct ThecaClient {
    client: Client,
    uploads: RefCell<HashMap<u32, CancelSender>>,
}

#[wasm_bindgen]
impl ThecaClient {
    /// A client for the server at `base_url` (usually the page's origin).
    #[wasm_bindgen(constructor)]
    pub fn new(base_url: &str) -> Result<ThecaClient, JsValue> {
        Ok(Self {
            client: Client::new(base_url).map_err(|e| js_error(&e))?,
            uploads: RefCell::default(),
        })
    }

    /// Uploads `entries` (`[{ path, file }]`, `path` below the folder: `a.txt`,
    /// or `a/b/c.txt` for a folder's files) into the folder `folder` of
    /// `library`, as the run `id` (to cancel it). `folders`: empty folders to
    /// create too (`a/empty`). A name already there gets a new version.
    /// `message`: the versions' description.
    ///
    /// `on_event` gets `{ kind: "progress", total, hashed, uploaded, skipped }`
    /// (bytes, every file together), and each file's outcome: `{ kind: "done",
    /// path, file, version, conflict, unchanged }` or `{ kind: "failed", path,
    /// code, message }`. Resolves once every file is done or failed; rejects
    /// with `{ code, message }` when nothing could start (folders not created).
    #[allow(clippy::too_many_arguments)]
    pub async fn upload(
        &self,
        id: u32,
        entries: js_sys::Array,
        folders: Vec<String>,
        library: String,
        folder: String,
        message: Option<String>,
        on_event: js_sys::Function,
    ) -> Result<(), JsValue> {
        let (library, folder) = (uuid(&library)?, uuid(&folder)?);
        let mut paths = Vec::new();
        let mut sources = Vec::new();
        for entry in entries.iter() {
            let path = js_sys::Reflect::get(&entry, &"path".into())?
                .as_string()
                .ok_or("an entry without a path")?;
            let file: web_sys::File = js_sys::Reflect::get(&entry, &"file".into())?.dyn_into()?;
            paths.push(path);
            sources.push(BlobSource(file.into()));
        }
        let (cancels, mut cancelled) = mpsc::unbounded();
        self.uploads.borrow_mut().insert(id, cancels);
        let result = async {
            let targets = place(&self.client, library, folder, &folders, &paths)
                .await
                .map_err(|e| js_error(&e))?;
            let cancel = Cancel::default();
            let options = UploadOptions {
                message,
                ..UploadOptions::default()
            };
            let report = |p: &UploadProgress| {
                emit(
                    &on_event,
                    &Event::Progress {
                        total: p.total,
                        hashed: p.hashed,
                        uploaded: p.uploaded,
                        skipped: p.skipped,
                    },
                );
            };
            let (uploader, run) = Uploader::new(&self.client, &options, &cancel, report);
            // Each file: its outcome, or its own cancel (it then fails as cancelled).
            let mut per_file: HashMap<&str, oneshot::Sender<()>> = HashMap::new();
            let mut files = Vec::new();
            for ((path, source), target) in paths.iter().zip(&sources).zip(targets) {
                let target = match target {
                    Ok(target) => target,
                    Err(e) => {
                        failed(&on_event, path, &e);
                        continue;
                    }
                };
                let submitted = uploader.submit(source, target);
                let (stop, stopped) = oneshot::channel();
                per_file.insert(path.as_str(), stop);
                let on_event = &on_event;
                files.push(async move {
                    let result = match select(submitted, stopped).await {
                        Either::Left((result, _)) => result,
                        Either::Right((_, submitted)) => {
                            submitted.cancel();
                            submitted.await
                        }
                    };
                    match result {
                        Ok(c) => emit(
                            on_event,
                            &Event::Done {
                                path,
                                file: c.file,
                                version: c.version,
                                conflict: c.conflict,
                                unchanged: c.unchanged,
                            },
                        ),
                        Err(e) => failed(on_event, path, &e),
                    }
                });
            }
            drop(uploader);
            // Cancels are listened to until every file is done or failed.
            let cancels = async {
                while let Some(path) = cancelled.next().await {
                    match path {
                        Some(path) => {
                            if let Some(stop) = per_file.remove(path.as_str()) {
                                let _ = stop.send(());
                            }
                        }
                        None => cancel.cancel(),
                    }
                }
            };
            select(
                Box::pin(async { futures::join!(run, join_all(files)) }),
                Box::pin(cancels),
            )
            .await;
            Ok(())
        }
        .await;
        self.uploads.borrow_mut().remove(&id);
        result
    }

    /// Cancels one file of the upload run `id` (`path`), or all of it.
    /// Cancelled files fail with the code `cancelled`; their pending chunks
    /// are freed on the server.
    pub fn cancel(&self, id: u32, path: Option<String>) {
        if let Some(run) = self.uploads.borrow().get(&id) {
            let _ = run.unbounded_send(path);
        }
    }

    /// The content of `file` (its current version, or `version`), in order,
    /// as a `ReadableStream` of `Uint8Array`s: fetched as it's read (a few
    /// chunks ahead), so the reader's pace bounds the memory. A failure errors
    /// the stream with `{ code, message }`.
    pub fn download(
        &self,
        file: String,
        version: Option<String>,
    ) -> Result<web_sys::ReadableStream, JsValue> {
        let file = uuid(&file)?;
        let version = version.as_deref().map(uuid).transpose()?;
        let chunks = stream(
            self.client.clone(),
            file,
            version,
            DownloadOptions::default(),
        )
        .map_ok(|bytes| JsValue::from(js_sys::Uint8Array::from(bytes.as_slice())))
        .map_err(|e| js_error(&e));
        Ok(wasm_streams::ReadableStream::from_stream(chunks).into_raw())
    }
}
