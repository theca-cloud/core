//! Request and response types of the HTTP API (`/api/v1`), shared by the server
//! and every client. The server derives their OpenAPI schemas (feature `openapi`).

use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::compression::Compression;
use std::collections::BTreeMap;

use crate::hash::{ChunkHash, Sha256};
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
    /// The organization that will own it (the account must be a member).
    pub organization: Uuid,
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
    /// The organization owning it.
    pub organization: Uuid,
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
    /// A file's current version date.
    pub modified: Option<String>,
}

/// `POST /chunks/lookup`.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
pub struct Lookup {
    /// The library being uploaded into.
    pub library: Uuid,
    /// The file's chunks.
    pub chunks: Vec<LookupChunk>,
}

/// One chunk of a lookup.
#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
pub struct LookupChunk {
    /// Hash of the plaintext.
    #[cfg_attr(feature = "openapi", schema(value_type = String))]
    pub hash: ChunkHash,
    /// Size of the plaintext (at most 8 MiB).
    pub plain_size: u64,
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

/// One chunk of `POST /chunks/upload-urls`: what the client will upload. Fixed
/// at the first request; a later request (expired URL) must repeat it exactly.
#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
pub struct Declare {
    /// A pending chunk from the lookup.
    pub chunk: Uuid,
    /// Size of the bytes to upload (after compression), at most `plain_size`.
    pub stored_size: u64,
    /// How those bytes are compressed.
    #[cfg_attr(feature = "openapi", schema(value_type = String))]
    pub compression: Compression,
    /// SHA-256 of those bytes: the upload URL only accepts exactly them.
    #[cfg_attr(feature = "openapi", schema(value_type = String))]
    pub stored_sha256: Sha256,
}

/// A list of chunks (`/chunks/announce`, `/chunks/cancel`).
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
    /// Headers to send with the request, exactly (they are signed).
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub headers: BTreeMap<String, String>,
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

/// `POST /auth/login`.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
pub struct Login {
    /// The account's email.
    pub email: String,
    /// Its password.
    pub password: String,
    /// The current TOTP code (or a recovery code), if TOTP is on. Without it,
    /// a right password gets `totp_required`: ask for the code, send again.
    #[serde(default)]
    pub totp: Option<String>,
}

/// The logged-in account and its organizations.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
pub struct Me {
    /// The account.
    pub account: Uuid,
    /// Its email.
    pub email: String,
    /// Whether logging in with the password also asks for a TOTP code.
    pub totp: bool,
    /// Its memberships.
    pub organizations: Vec<Membership>,
}

/// An account's membership in an organization.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
pub struct Membership {
    /// The organization.
    pub organization: Uuid,
    /// Its name.
    pub name: String,
    /// False while it's an invitation not accepted yet.
    pub joined: bool,
}

/// A live session of the account.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
pub struct Session {
    /// The session.
    pub id: Uuid,
    /// Its name (from the user agent).
    pub name: String,
    /// The browser or client that opened it.
    pub user_agent: Option<String>,
    /// The address it was opened from.
    pub ip: Option<String>,
    /// When it was opened.
    pub created_at: String,
    /// When it was last used.
    pub last_seen: Option<String>,
    /// The organization it is restricted to, if opened through an organization's identity provider.
    pub restricted_to: Option<Uuid>,
    /// This session.
    pub current: bool,
}

/// Create an app password (CLI, API clients, WebDAV): data access only.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
pub struct CreateAppPassword {
    /// Shown in lists, e.g. "laptop CLI" (1 to 200 characters).
    pub name: String,
    /// Limit it to one organization (none: the whole account; a restricted
    /// session: always its own organization).
    #[serde(default)]
    pub organization: Option<Uuid>,
    /// Limit it to one library (its organization implied).
    #[serde(default)]
    pub library: Option<Uuid>,
    /// Read only, whatever the account's role in the libraries.
    #[serde(default)]
    pub read_only: bool,
}

/// An app password (never the secret itself, except once at creation).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
pub struct AppPassword {
    /// The app password.
    pub id: Uuid,
    /// Its name.
    pub name: String,
    /// The organization it's limited to, if any.
    pub organization: Option<Uuid>,
    /// The library it's limited to, if any.
    pub library: Option<Uuid>,
    /// Read only.
    pub read_only: bool,
    /// When it was created.
    pub created_at: String,
    /// Last used (precise to a few minutes), if ever.
    pub last_seen: Option<String>,
}

/// A new app password: the secret is shown only this once.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
pub struct NewAppPassword {
    /// Send it as `Authorization: Bearer <password>` (or WebDAV Basic auth).
    pub password: String,
    /// What it is.
    pub app_password: AppPassword,
}

/// A TOTP secret being set up (not required at login until confirmed).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
pub struct TotpSetup {
    /// The secret in base32, to type into an authenticator app.
    pub secret: String,
    /// The `otpauth://` URI, to show as a QR code.
    pub uri: String,
}

/// A TOTP code (6 digits), or a recovery code where accepted.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
pub struct TotpCode {
    /// The code.
    pub code: String,
}

/// One-time recovery codes, each usable once instead of a TOTP code. Shown
/// only now: the server only keeps their hashes.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
pub struct RecoveryCodes {
    /// The codes.
    pub codes: Vec<String>,
}

/// An OIDC provider users can log in with.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
pub struct IdentityProvider {
    /// The provider: log in at `/api/v1/auth/oidc/{id}/start`.
    pub id: Uuid,
    /// Its name, e.g. "Google".
    pub name: String,
}

/// One of the account's ways to log in.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
pub struct Identity {
    /// The identity.
    pub id: Uuid,
    /// `password` or `oidc`.
    pub kind: String,
    /// The OIDC provider.
    pub provider: Option<Uuid>,
    /// The OIDC provider's name.
    pub provider_name: Option<String>,
}

/// Where to send the browser to continue with an OIDC provider.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
pub struct OidcRedirect {
    /// The provider's authorization URL.
    pub url: String,
}

/// Whether the instance still has to be set up.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
pub struct SetupStatus {
    /// Not set up yet: the server only serves the setup.
    pub required: bool,
    /// The setup needs the token printed in the server's logs.
    pub token_required: bool,
}

/// Sets up a fresh instance: its admin, first organization and storages.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
pub struct Setup {
    /// The instance admin's email.
    pub admin_email: String,
    /// The instance admin's password (at least 8 characters).
    pub admin_password: String,
    /// The first organization, owned by the admin.
    pub organization: String,
    /// One or more storages, shared by the instance. The first one is the
    /// default (new libraries go there).
    pub storages: Vec<NewStorage>,
}

/// A storage backend. Credentials are paths to files on the server, never values.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
pub struct NewStorage {
    /// Its name (unique, ignoring case).
    pub name: String,
    /// What it is.
    #[serde(flatten)]
    pub backend: StorageBackend,
}

/// A storage implementation and its settings.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
#[serde(tag = "implementation", rename_all = "lowercase")]
pub enum StorageBackend {
    /// A directory on the server.
    Filesystem {
        /// Its path (created if missing).
        path: String,
    },
    /// An S3-compatible bucket.
    S3 {
        /// The endpoint, e.g. `https://s3.example.com`.
        host: String,
        /// The region.
        region: String,
        /// The bucket.
        bucket: String,
        /// Path to a file holding the access key.
        access_key_file: String,
        /// Path to a file holding the secret key.
        secret_key_file: String,
        /// Path-style URLs (`host/bucket/key`), what self-hosted servers usually need.
        #[serde(default = "yes")]
        path_style: bool,
    },
}

fn yes() -> bool {
    true
}

/// A file or folder, with where it is.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
pub struct NodeInfo {
    /// The node.
    pub id: Uuid,
    /// Its library.
    pub library: Uuid,
    /// Its name (empty for the root).
    pub name: String,
    /// A file (true) or a folder (false).
    pub is_file: bool,
    /// Its folder (none for the root).
    pub parent: Option<Uuid>,
    /// Its path in the library, e.g. `/a/b.txt` (`/` for the root).
    pub path: String,
    /// A file's current version.
    pub current_version: Option<Uuid>,
}

/// `POST /nodes/{parent}/folders`.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
pub struct CreateFolder {
    /// Its name.
    #[cfg_attr(feature = "openapi", schema(value_type = String))]
    pub name: Name,
}

/// `PATCH /nodes/{id}`: rename and/or move.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
pub struct UpdateNode {
    /// The new name.
    #[serde(default)]
    #[cfg_attr(feature = "openapi", schema(value_type = Option<String>))]
    pub name: Option<Name>,
    /// The new folder (in any library of the same organization).
    #[serde(default)]
    pub parent: Option<Uuid>,
}

/// `POST /nodes/{id}/copy`.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
pub struct CopyNode {
    /// The folder to copy into (in any library of the same organization).
    pub parent: Uuid,
    /// The copy's name (default: the source's).
    #[serde(default)]
    #[cfg_attr(feature = "openapi", schema(value_type = Option<String>))]
    pub name: Option<Name>,
}

/// A trashed file or folder.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
pub struct TrashEntry {
    /// The node.
    pub id: Uuid,
    /// Its name.
    pub name: String,
    /// A file (true) or a folder (false).
    pub is_file: bool,
    /// Where it was.
    pub path: String,
    /// When it was trashed.
    pub deleted_at: String,
}

/// Where a restored node landed.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
pub struct Restored {
    /// Its folder: its old one, or the library's root if that one is in the trash.
    pub parent: Uuid,
    /// Its name (` (restored)` added if the old one was taken).
    pub name: String,
    /// Its path.
    pub path: String,
}

/// A version of a file.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
pub struct Version {
    /// The version.
    pub id: Uuid,
    /// The version it was based on (a tree: branches after conflicts and restores).
    pub parent: Option<Uuid>,
    /// Who committed it (their email), unless they left the organization.
    pub author: Option<String>,
    /// When.
    pub created_at: String,
    /// Its description.
    pub message: Option<String>,
    /// The file's current version.
    pub current: bool,
}

/// A search result.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
pub struct SearchResult {
    /// The node.
    pub id: Uuid,
    /// Its library.
    pub library: Uuid,
    /// Its name.
    pub name: String,
    /// A file (true) or a folder (false).
    pub is_file: bool,
    /// Its path in the library.
    pub path: String,
}

/// A background job and its last runs (admin).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
pub struct JobStatus {
    /// The job: `chunk_gc`, `trash_purge`, `cleanup`, `orphan_check`.
    pub job: String,
    /// How often it runs, in seconds.
    pub interval_secs: u64,
    /// Its last runs, newest first.
    pub runs: Vec<JobRun>,
}

/// One run of a background job.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
pub struct JobRun {
    /// The run.
    pub id: Uuid,
    /// When it started.
    pub started_at: String,
    /// When it ended (none while running).
    pub finished_at: Option<String>,
    /// `running`, `success`, `failure` or `interrupted` (cut off by a stop).
    pub status: String,
    /// How many items it handled.
    pub items: i32,
    /// Why it failed.
    pub error: Option<String>,
}
