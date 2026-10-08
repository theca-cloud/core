//! Request and response types of the HTTP API (`/api/v1`), shared by the server
//! and every client. The server derives their OpenAPI schemas (feature `openapi`).

use chrono::{DateTime, Utc};
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

/// `PATCH /libraries/{id}`: rename.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
pub struct UpdateLibrary {
    /// The new name (same rules as file names).
    #[cfg_attr(feature = "openapi", schema(value_type = String))]
    pub name: Name,
}

/// A library in its organization's trash.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
pub struct TrashedLibrary {
    /// The library.
    pub id: Uuid,
    /// Its name.
    pub name: String,
    /// When it was trashed.
    pub deleted_at: DateTime<Utc>,
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
    /// Whether the account may write in it: its role, within its credential's
    /// scope and read-only cap.
    #[serde(default)]
    pub writable: bool,
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
    pub modified: Option<DateTime<Utc>>,
    /// A file's current version (the base of a new one).
    #[serde(default)]
    pub current_version: Option<Uuid>,
    /// A file's size in bytes (its current version's); none for a folder or a
    /// file without a version yet.
    #[serde(default)]
    pub size: Option<i64>,
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

/// One commit of `POST /files/commit`: either `file` (a new version of an existing file) or
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
    /// The same content as the base, still current: no version was created,
    /// `version` is the current one.
    #[serde(default)]
    pub unchanged: bool,
}

/// `POST /files/commit`: commits, each in its own transaction (one
/// commit is a batch of one).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
pub struct Commits {
    /// At most 100.
    pub commits: Vec<Commit>,
}

/// One commit's outcome in a batch: committed, or why not.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
pub struct CommitOutcome {
    /// Committed (or `unchanged`).
    #[serde(default)]
    pub committed: Option<Committed>,
    /// Refused, and why.
    #[serde(default)]
    pub error: Option<ApiError>,
    /// With `error`: its HTTP status, as a call of its own would have answered.
    #[serde(default)]
    pub status: Option<u16>,
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

/// `GET /limits`: the instance's limits clients size their work by.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
pub struct Limits {
    /// Chunks an account may have declared and not yet announced
    /// (`max_pending_uploads`): an upload engine's window.
    pub max_pending_uploads: u32,
}

/// `POST /files/download`: files whose chunks to download (at most 100).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
pub struct Downloads {
    /// The files, at most 100.
    pub files: Vec<DownloadFile>,
}

/// A file to download: its current version, or another one.
#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
pub struct DownloadFile {
    /// The file.
    pub file: Uuid,
    /// One of its versions (default: the current one).
    #[serde(default)]
    pub version: Option<Uuid>,
}

/// One file's outcome in `POST /files/download`: its chunks, or why not.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
pub struct DownloadOutcome {
    /// The version's chunks.
    #[serde(default)]
    pub download: Option<Download>,
    /// Refused, and why (`not_found`: the file or the version).
    #[serde(default)]
    pub error: Option<ApiError>,
    /// With `error`: its HTTP status.
    #[serde(default)]
    pub status: Option<u16>,
}

/// A file's version to download (an outcome of `POST /files/download`).
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
    /// An instance admin (organizations, storages, settings).
    #[serde(default)]
    pub instance_admin: bool,
    /// Its memberships.
    pub organizations: Vec<Membership>,
}

/// `GET /instance`: public, for a login page.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
pub struct Instance {
    /// Its name (the setting `instance_name`).
    pub name: String,
    /// Accounts can be created (`registration = open`, and SMTP configured).
    pub registration: bool,
    /// The zxcvbn score a new password needs, 0 (none: 8 characters only) to 4
    /// (the setting `password_strength`).
    pub password_strength: u8,
}

/// A member's role in a library.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
#[serde(rename_all = "lowercase")]
pub enum Role {
    /// Reads.
    Read,
    /// Reads, uploads, moves, deletes.
    Write,
    /// A role this version doesn't know.
    #[serde(other)]
    Unknown,
}

/// A library's member.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
pub struct LibraryMember {
    /// The organization's member (as `GET /organizations/{id}/members` lists them).
    pub user: Uuid,
    /// Its email.
    pub email: String,
    /// False while it's an invitation not accepted yet.
    pub joined: bool,
    /// Its role.
    pub role: Role,
}

/// `PUT /libraries/{id}/members/{user}`: adds the member, or changes its role.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
pub struct SetRole {
    /// The role.
    pub role: Role,
}

/// One of an organization's libraries, to its owner.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
pub struct OrganizationLibrary {
    /// The library.
    pub id: Uuid,
    /// Its name.
    pub name: String,
    /// How many members it has (none: nobody reaches it until given some).
    pub members: i64,
}

/// A dedup namespace: its libraries' members trust each other.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
pub struct Namespace {
    /// The namespace.
    pub id: Uuid,
    /// Its name.
    pub name: String,
    /// The libraries in it.
    pub libraries: Vec<Uuid>,
}

/// `POST /organizations/{id}/namespaces`, `PATCH /namespaces/{id}`.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
pub struct NamespaceName {
    /// Its name.
    pub name: String,
}

/// An organization, to one of its members.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
pub struct Organization {
    /// The organization.
    pub id: Uuid,
    /// Its name.
    pub name: String,
    /// The account's personal organization (shown as "My files").
    pub personal: bool,
    /// The account owns it.
    pub owner: bool,
    /// Its quota in bytes; none: unlimited.
    pub quota: Option<i64>,
    /// What it stores, in bytes (counted against the quota).
    pub used: i64,
}

/// `POST /organizations`, `PATCH /organizations/{id}`.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
pub struct OrganizationName {
    /// Its name.
    pub name: String,
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
    /// The account's personal organization (shown as "My files").
    pub personal: bool,
    /// Suspended by the instance admin: nothing in it is reachable.
    pub suspended: bool,
    /// The account owns it (its administrator: members, settings).
    #[serde(default)]
    pub owner: bool,
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
    pub created_at: DateTime<Utc>,
    /// When it was last used.
    pub last_seen: Option<DateTime<Utc>>,
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
    pub created_at: DateTime<Utc>,
    /// Last used (precise to a few minutes), if ever.
    pub last_seen: Option<DateTime<Utc>>,
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
    /// The folders above it, from the library's root (named `""`) to its
    /// folder; none for the root.
    #[serde(default)]
    pub ancestors: Vec<Ancestor>,
    /// A file's current version.
    pub current_version: Option<Uuid>,
    /// A file's size in bytes (its current version's); none for a folder or a
    /// file without a version yet.
    #[serde(default)]
    pub size: Option<i64>,
}

/// A folder above a node.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
pub struct Ancestor {
    /// The folder.
    pub id: Uuid,
    /// Its name (empty for the library's root).
    pub name: String,
}

/// `POST /nodes/{parent}/folders`: folders to create in a folder, like
/// `mkdir -p` (missing ones created, existing ones kept).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
pub struct CreateFolders {
    /// Paths relative to the folder, `/`-separated (`a`, `a/b`); at most 1000.
    pub paths: Vec<String>,
}

/// A folder of `POST /nodes/{parent}/folders`, in the request's order.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
pub struct FolderPath {
    /// The path asked (names as stored).
    pub path: String,
    /// The folder.
    pub id: Uuid,
    /// Created by this request (false: it existed).
    pub created: bool,
}

/// A node of `GET /nodes/{id}/tree`.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
pub struct TreeNode {
    /// The node.
    pub id: Uuid,
    /// Its path below the listed folder (`a`, `a/b.txt`).
    pub path: String,
    /// A file (true) or a folder (false).
    pub is_file: bool,
    /// A file's current version date.
    pub modified: Option<DateTime<Utc>>,
    /// A file's current version.
    pub current_version: Option<Uuid>,
    /// A file's size in bytes (its current version's); none for a folder or a
    /// file without a version yet.
    #[serde(default)]
    pub size: Option<i64>,
}

/// One line of `GET /nodes/{id}/tree` (NDJSON): a node, then the end (with
/// how many nodes came) or an error. A stream without its end line is cut.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
#[serde(rename_all = "snake_case")]
pub enum TreeLine {
    /// A node, parents before their children.
    Node(TreeNode),
    /// Every node was sent.
    End {
        /// How many.
        count: u64,
    },
    /// The listing failed midway.
    Error(ApiError),
}

/// `PATCH /nodes`: renames and/or moves, each in its own transaction (at
/// most 1000).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
pub struct UpdateNodes {
    /// The changes, each its own outcome.
    pub nodes: Vec<UpdateNode>,
}

/// A rename and/or move.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
pub struct UpdateNode {
    /// The file or folder.
    pub id: Uuid,
    /// The new name.
    #[serde(default)]
    #[cfg_attr(feature = "openapi", schema(value_type = Option<String>))]
    pub name: Option<Name>,
    /// The new folder (in any library of the same organization).
    #[serde(default)]
    pub parent: Option<Uuid>,
    /// A file of that name exists there: merge into it (its history gains
    /// this file's versions after its current one, this file's current version
    /// becomes current, this node goes). Otherwise `name_taken`. Folders are
    /// never replaced.
    #[serde(default)]
    pub replace: bool,
}

/// `POST /nodes/copy`: copies, each in its own transaction (at most 1000).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
pub struct CopyNodes {
    /// The copies, each its own outcome.
    pub nodes: Vec<CopyNode>,
}

/// A copy.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
pub struct CopyNode {
    /// The file or folder to copy.
    pub id: Uuid,
    /// The folder to copy into (in any library of the same organization).
    pub parent: Uuid,
    /// The copy's name (default: the source's).
    #[serde(default)]
    #[cfg_attr(feature = "openapi", schema(value_type = Option<String>))]
    pub name: Option<Name>,
    /// A file of that name exists there: the copy is a new version of it
    /// (based on its current one). Otherwise `name_taken`. Folders are never
    /// replaced.
    #[serde(default)]
    pub replace: bool,
}

/// `POST /nodes/trash`: files and folders to the trash, or (`permanent`,
/// already in the trash) deleted for good; each in its own transaction (at
/// most 1000).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
pub struct TrashNodes {
    /// The files and folders, each its own outcome.
    pub nodes: Vec<Uuid>,
    /// Delete for good (nodes already in the trash only).
    #[serde(default)]
    pub permanent: bool,
}

/// `POST /nodes/restore`: files and folders out of the trash, each in its own
/// transaction (at most 1000).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
pub struct RestoreNodes {
    /// The trashed files and folders, each its own outcome.
    pub nodes: Vec<Uuid>,
}

/// One node's outcome in a batch: the node as it is now (none after a trash),
/// or why not.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
pub struct NodeOutcome {
    /// Done: the node now (renamed, moved, the copy); none for a trash.
    #[serde(default)]
    pub node: Option<NodeInfo>,
    /// Refused, and why.
    #[serde(default)]
    pub error: Option<ApiError>,
    /// With `error`: its HTTP status, as a call of its own would have answered.
    #[serde(default)]
    pub status: Option<u16>,
}

/// One restore's outcome in a batch.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
pub struct RestoreOutcome {
    /// Where it went.
    #[serde(default)]
    pub restored: Option<Restored>,
    /// Refused, and why.
    #[serde(default)]
    pub error: Option<ApiError>,
    /// With `error`: its HTTP status, as a call of its own would have answered.
    #[serde(default)]
    pub status: Option<u16>,
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
    /// A file's size in bytes (its current version's); none for a folder or a
    /// file without a version yet.
    #[serde(default)]
    pub size: Option<i64>,
    /// When it was trashed.
    pub deleted_at: DateTime<Utc>,
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
    /// Its number in the tree, RCS-style: `1`, `2`, `3` along a line; a
    /// version's oldest child continues its line, later ones start branches
    /// (`2.1.1`, `2.1.2`, `2.2.1`). Never changes.
    #[serde(default)]
    pub number: String,
    /// The version it was based on (a tree: branches after conflicts and restores).
    pub parent: Option<Uuid>,
    /// Who committed it (their email), unless they left the organization.
    pub author: Option<String>,
    /// When.
    pub created_at: DateTime<Utc>,
    /// Its description.
    pub message: Option<String>,
    /// The file's size in bytes at this version.
    #[serde(default)]
    pub size: i64,
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
    /// A file's size in bytes (its current version's); none for a folder or a
    /// file without a version yet.
    #[serde(default)]
    pub size: Option<i64>,
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
    pub started_at: DateTime<Utc>,
    /// When it ended (none while running).
    pub finished_at: Option<DateTime<Utc>>,
    /// `running`, `success`, `failure` or `interrupted` (cut off by a stop).
    pub status: String,
    /// How many items it handled.
    pub items: i32,
    /// Why it failed.
    pub error: Option<String>,
}

/// A member of an organization (or a pending invitation).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
pub struct Member {
    /// The member (their user in this organization).
    pub id: Uuid,
    /// Their email.
    pub email: String,
    /// Joined (false: invited, pending).
    pub joined: bool,
    /// The organization's owner.
    pub owner: bool,
}

/// `POST /organizations/{id}/members`.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
pub struct AddMember {
    /// An existing account joins at once; another address gets an invitation.
    pub email: String,
}

/// An email and a password (registration).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
pub struct Register {
    /// The email (verified before the account can log in).
    pub email: String,
    /// The password (at least 8 characters).
    pub password: String,
}

/// An email address.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
pub struct EmailAddress {
    /// The address.
    pub email: String,
}

/// A token from an email's link.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
pub struct LinkToken {
    /// The token.
    pub token: String,
}

/// The account an email's link is for.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
pub struct TokenEmail {
    /// Its email.
    pub email: String,
}

/// A password change: the current one, and the new one.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
pub struct ChangePassword {
    /// The current password.
    pub current: String,
    /// The new password (at least 8 characters, and strong enough).
    pub new: String,
}

/// A token from an email's link, and a password to set.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
pub struct SetPassword {
    /// The token.
    pub token: String,
    /// The new password (at least 8 characters, and strong enough: the
    /// instance's `password_strength`). Optional to accept an
    /// invitation for an account that can already log in.
    #[serde(default)]
    pub password: Option<String>,
}

/// `Some(None)` for an explicit `null` (clear), `None` when absent (keep).
fn double_option<'de, D, T>(deserializer: D) -> Result<Option<Option<T>>, D::Error>
where
    D: serde::Deserializer<'de>,
    T: Deserialize<'de>,
{
    Option::<T>::deserialize(deserializer).map(Some)
}

/// An instance setting (admin).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
pub struct SettingInfo {
    /// The key.
    pub key: String,
    /// Its stored value (none: the default applies).
    pub value: Option<String>,
    /// Its default.
    pub default: String,
    /// What it does.
    pub description: String,
}

/// An organization, for the instance admin.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
pub struct AdminOrganization {
    /// The organization.
    pub id: Uuid,
    /// Its name.
    pub name: String,
    /// Its owner's email.
    pub owner: String,
    /// An account's personal organization.
    pub personal: bool,
    /// Joined members and pending invitations.
    pub members: i64,
    /// Its quota in bytes (none: unlimited).
    pub quota: Option<i64>,
    /// Bytes it stores (the quota's measure).
    pub used: i64,
    /// Suspended: nothing in it is reachable.
    pub suspended: bool,
    /// Its default storage (none: the instance's).
    pub default_storage: Option<Uuid>,
}

/// `POST /admin/organizations`.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
pub struct CreateOrganization {
    /// Its name.
    pub name: String,
    /// The email of an existing account, its owner.
    pub owner: String,
}

/// `PATCH /admin/organizations/{id}`: only the fields given change.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
pub struct UpdateOrganization {
    /// A new name.
    #[serde(default)]
    pub name: Option<String>,
    /// A quota in bytes, or `null` for unlimited.
    #[serde(default, deserialize_with = "double_option")]
    #[cfg_attr(feature = "openapi", schema(value_type = Option<i64>))]
    pub quota: Option<Option<i64>>,
    /// Suspend or lift the suspension.
    #[serde(default)]
    pub suspended: Option<bool>,
    /// A default storage, or `null` for the instance's.
    #[serde(default, deserialize_with = "double_option")]
    #[cfg_attr(feature = "openapi", schema(value_type = Option<Uuid>))]
    pub default_storage: Option<Option<Uuid>>,
}

/// An account, for the instance admin.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
pub struct AdminAccount {
    /// The account.
    pub id: Uuid,
    /// Its email.
    pub email: String,
    /// Its email is verified.
    pub verified: bool,
    /// An instance admin.
    pub instance_admin: bool,
    /// Suspended: it can't log in, its tokens are refused.
    pub suspended: bool,
    /// When it was created.
    pub created_at: DateTime<Utc>,
}

/// `PATCH /admin/accounts/{id}`: only the fields given change.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
pub struct UpdateAccount {
    /// Make or unmake an instance admin (never the last one).
    #[serde(default)]
    pub instance_admin: Option<bool>,
    /// Suspend or lift the suspension.
    #[serde(default)]
    pub suspended: Option<bool>,
}

/// A storage backend, for the instance admin.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
pub struct StorageInfo {
    /// The storage.
    pub id: Uuid,
    /// Its name.
    pub name: String,
    /// `filesystem` or `s3`.
    pub implementation: String,
    /// The instance default (new libraries go there).
    pub default: bool,
    /// Chunks stored there.
    pub chunks: i64,
    /// Bytes stored there.
    pub used: i64,
}

/// An OIDC identity provider (never its secret).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
pub struct ProviderInfo {
    /// The provider: its login starts at `/api/v1/auth/oidc/{id}/start`.
    pub id: Uuid,
    /// Its organization (none: instance-wide).
    pub organization: Option<Uuid>,
    /// Shown on the login screen.
    pub name: String,
    /// The issuer (its discovery document is under it).
    pub issuer: String,
    /// The client ID registered there.
    pub client_id: String,
    /// The client secret's file (instance-wide providers using one).
    pub client_secret_file: Option<String>,
    /// Offered for logins.
    pub enabled: bool,
}

/// Creates or changes an OIDC identity provider (changes: only the fields given).
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
pub struct ProviderRequest {
    /// Shown on the login screen.
    #[serde(default)]
    pub name: Option<String>,
    /// The issuer URL (an organization's: `https`, public addresses only).
    #[serde(default)]
    pub issuer: Option<String>,
    /// The client ID registered there.
    #[serde(default)]
    pub client_id: Option<String>,
    /// The client secret's value (stored encrypted). An organization's
    /// provider takes only this.
    #[serde(default)]
    pub client_secret: Option<String>,
    /// A path to a file holding the client secret (instance-wide only).
    #[serde(default)]
    pub client_secret_file: Option<String>,
    /// Offered for logins.
    #[serde(default)]
    pub enabled: Option<bool>,
}

/// `POST /organizations/{id}/owner`.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
pub struct TransferOwnership {
    /// The joined member becoming the owner.
    pub member: Uuid,
}

/// `DELETE /me`: the account confirms (its password if it has one, its TOTP
/// code if on).
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
pub struct DeleteAccount {
    /// The account's password.
    #[serde(default)]
    pub password: Option<String>,
    /// A TOTP or recovery code.
    #[serde(default)]
    pub totp: Option<String>,
}

/// Everything the server holds about an account (`GET /me/export`), never
/// secrets. Files come through the usual paths (CLI, WebDAV).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
pub struct Export {
    /// The account.
    pub account: ExportedAccount,
    /// Its organizations.
    pub organizations: Vec<Membership>,
    /// Its live sessions.
    pub sessions: Vec<Session>,
    /// Its ways to log in.
    pub identities: Vec<Identity>,
    /// Its app passwords (names and scopes).
    pub app_passwords: Vec<AppPassword>,
    /// The files of its personal organization.
    pub files: Vec<ExportedFile>,
}

/// The account part of an export.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
pub struct ExportedAccount {
    /// The account.
    pub id: Uuid,
    /// Its email.
    pub email: String,
    /// Its email is verified.
    pub verified: bool,
    /// TOTP is on.
    pub totp: bool,
    /// When it was created.
    pub created_at: DateTime<Utc>,
}

/// A file of an export.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
pub struct ExportedFile {
    /// The file.
    pub id: Uuid,
    /// Its library.
    pub library: String,
    /// Its path.
    pub path: String,
    /// How many versions it has.
    pub versions: i64,
}
