//! The browser package (`wasm-pack`): a thin API over the engine for the web
//! app. The session cookie goes with every request (`credentials: include`);
//! JavaScript never sees it.

use std::cell::RefCell;
use std::io;

use theca_common::name::Name;
use uuid::Uuid;
use wasm_bindgen::prelude::*;
use wasm_bindgen_futures::JsFuture;

use crate::Client;
use crate::rt::BoxFuture;
use crate::source::Source;
use crate::upload::{Cancel, Target, UploadOptions, UploadProgress, upload};

/// A browser `Blob` (a `File` from an `<input>` or a drop), read by slices.
struct BlobSource(web_sys::Blob);

impl Source for BlobSource {
    fn len(&self) -> u64 {
        self.0.size() as u64
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

/// The client for the web app.
#[wasm_bindgen]
pub struct ThecaClient {
    client: Client,
    cancel: RefCell<Cancel>,
}

#[wasm_bindgen]
impl ThecaClient {
    /// A client for the server at `base_url` (usually the page's origin).
    #[wasm_bindgen(constructor)]
    pub fn new(base_url: &str) -> Result<ThecaClient, JsError> {
        Ok(Self {
            client: Client::new(base_url)?,
            cancel: RefCell::default(),
        })
    }

    /// Uploads `file` as `name` into the folder `parent` of `library`.
    /// `on_progress` gets `{ total, hashed, uploaded, skipped }` (bytes).
    /// Resolves to `{ file, version, conflict }`.
    pub async fn upload(
        &self,
        file: web_sys::File,
        library: String,
        parent: String,
        name: String,
        on_progress: js_sys::Function,
    ) -> Result<JsValue, JsError> {
        let cancel = Cancel::default();
        *self.cancel.borrow_mut() = cancel.clone();
        let target = Target::New {
            library: Uuid::parse_str(&library)?,
            parent: Uuid::parse_str(&parent)?,
            name: Name::new(&name).map_err(|e| JsError::new(&e.to_string()))?,
        };
        let report = |p: &UploadProgress| {
            let json = format!(
                r#"{{"total":{},"hashed":{},"uploaded":{},"skipped":{}}}"#,
                p.total, p.hashed, p.uploaded, p.skipped
            );
            if let Ok(value) = js_sys::JSON::parse(&json) {
                let _ = on_progress.call1(&JsValue::NULL, &value);
            }
        };
        let source = BlobSource(file.into());
        let committed = upload(
            &self.client,
            &source,
            target,
            &UploadOptions::default(),
            report,
            &cancel,
        )
        .await?;
        let json = serde_json::to_string(&committed).map_err(|e| JsError::new(&e.to_string()))?;
        js_sys::JSON::parse(&json).map_err(|_| JsError::new("unreadable answer"))
    }

    /// Stops the upload in progress (its pending chunks are cancelled on the server).
    pub fn cancel(&self) {
        self.cancel.borrow().cancel();
    }
}
