//! Client engine shared by every Theca client (the CLI natively, the web app
//! through WASM): the API over the `common` types, uploads and downloads,
//! written once (docs/decisions.md, Architecture).

mod api;
mod rt;
mod source;
mod upload;

#[cfg(not(target_arch = "wasm32"))]
mod download;
#[cfg(target_arch = "wasm32")]
mod wasm;

pub use api::{Client, Error};
#[cfg(not(target_arch = "wasm32"))]
pub use download::{DownloadOptions, DownloadProgress, download};
#[cfg(not(target_arch = "wasm32"))]
pub use source::FileSource;
pub use source::{MemorySource, Source};
pub use upload::{Cancel, Submitted, Target, UploadOptions, UploadProgress, Uploader, upload};
