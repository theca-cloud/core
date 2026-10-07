//! Client engine shared by every Theca client (the CLI natively, the web app
//! through WASM): the API over the `common` types, uploads and downloads,
//! written once (docs/decisions.md, Architecture).

mod api;
mod download;
mod place;
mod rt;
mod source;
mod upload;

#[cfg(target_arch = "wasm32")]
mod wasm;

pub use api::{Client, Error};
#[cfg(not(target_arch = "wasm32"))]
pub use download::FileSink;
pub use download::{
    DownloadOptions, DownloadProgress, Downloaded, Downloader, Fetched, MemorySink, Sink, download,
    stream,
};
pub use place::{Existing, path_key, place, target};
#[cfg(not(target_arch = "wasm32"))]
pub use source::FileSource;
pub use source::{MemorySource, Source};
pub use upload::{Cancel, Submitted, Target, UploadOptions, UploadProgress, Uploader, upload};
