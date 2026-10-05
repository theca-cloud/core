//! Request and response types of the HTTP API (`/api/v1`), shared by the server
//! and every client. The server derives their OpenAPI schemas (feature `openapi`).

use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::compression::Compression;
use crate::hash::ChunkHash;
use crate::name::Name;

/// An API error: a stable machine-readable `code` and a human message.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
pub struct ApiError {
    /// Stable identifier, e.g. `name_taken`, `chunk_missing`.
    pub code: String,
    /// Explanation for humans.
    pub message: String,
    /// Chunks concerned, for `chunk_missing` and `chunk_not_allowed`.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub chunks: Vec<Uuid>,
}

/// `POST /libraries`.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
pub struct CreateLibrary {
    /// The library name (same rules as file names).
    #[cfg_attr(feature = "openapi", schema(value_type = String))]
    pub name: Name,
    /// Existing namespaces of the organization to put the library in. Empty:
    /// the library only dedups with itself.
    #[serde(default)]
    pub namespaces: Vec<Uuid>,
}

/// A library.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
pub struct Library {
    /// The library.
    pub id: Uuid,
    /// Its name.
    pub name: String,
    /// Its root folder.
    pub root: Uuid,
}

/// A file or folder.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
pub struct Node {
    /// The node.
    pub id: Uuid,
    /// Its name.
    pub name: String,
    /// A file (true) or a folder (false).
    pub is_file: bool,
}

/// `POST /chunks/lookup`.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
pub struct Lookup {
    /// The library being uploaded into.
    pub library: Uuid,
    /// Hashes of the file's chunks.
    #[cfg_attr(feature = "openapi", schema(value_type = Vec<String>))]
    pub hashes: Vec<ChunkHash>,
}

/// Whether a chunk must be uploaded.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
#[serde(rename_all = "lowercase")]
pub enum ChunkState {
    /// Already stored: skip it.
    Ready,
    /// Pending: upload it, then announce it.
    Upload,
    /// A state this version doesn't know.
    #[serde(other)]
    Unknown,
}

/// One hash of a lookup.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
pub struct LookupEntry {
    /// The hash looked up.
    #[cfg_attr(feature = "openapi", schema(value_type = String))]
    pub hash: ChunkHash,
    /// The chunk to reference in the commit.
    pub chunk: Uuid,
    /// Upload it or not.
    pub state: ChunkState,
}

/// A list of chunks (`/chunks/upload-urls`, `/chunks/cancel`).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
pub struct Chunks {
    /// The chunks.
    pub chunks: Vec<Uuid>,
}

/// A presigned URL for one chunk.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
pub struct ChunkUrl {
    /// The chunk.
    pub chunk: Uuid,
    /// Where to `PUT` (upload) or `GET` (download) it, without authentication.
    pub url: String,
}

/// One chunk of `POST /chunks/announce`.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
pub struct Announce {
    /// The uploaded chunk.
    pub chunk: Uuid,
    /// Its size before compression and encryption.
    pub plain_size: u64,
    /// How it was compressed.
    #[cfg_attr(feature = "openapi", schema(value_type = String))]
    pub compression: Compression,
}

/// `POST /files/commit`: either `file` (a new version of an existing file) or
/// `parent` + `name` (a new file).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
pub struct Commit {
    /// The library.
    pub library: Uuid,
    /// An existing file.
    #[serde(default)]
    pub file: Option<Uuid>,
    /// The folder of a new file.
    #[serde(default)]
    pub parent: Option<Uuid>,
    /// The name of a new file.
    #[serde(default)]
    #[cfg_attr(feature = "openapi", schema(value_type = Option<String>))]
    pub name: Option<Name>,
    /// The version the client started from (none for a new file).
    #[serde(default)]
    pub base_version: Option<Uuid>,
    /// The file's chunks, in order.
    pub chunks: Vec<Uuid>,
    /// Description of the version.
    #[serde(default)]
    pub message: Option<String>,
}

/// The result of a commit.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
pub struct Committed {
    /// The file.
    pub file: Uuid,
    /// The new version, now current.
    pub version: Uuid,
    /// The base wasn't the current version: another branch exists.
    pub conflict: bool,
}

/// One chunk of a download, in order.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
pub struct DownloadChunk {
    /// The chunk.
    pub chunk: Uuid,
    /// Its hash, to verify the downloaded plaintext.
    #[cfg_attr(feature = "openapi", schema(value_type = String))]
    pub hash: ChunkHash,
    /// How it's compressed.
    #[cfg_attr(feature = "openapi", schema(value_type = String))]
    pub compression: Compression,
    /// Its size once decompressed.
    pub plain_size: u64,
    /// Presigned `GET` URL.
    pub url: String,
}

/// `GET /files/{id}/download`.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
pub struct Download {
    /// The version downloaded (the current one).
    pub version: Uuid,
    /// Its chunks, in order.
    pub chunks: Vec<DownloadChunk>,
}
