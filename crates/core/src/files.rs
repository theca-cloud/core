//! File operations shared by the API and WebDAV: one function per action, with
//! its checks (docs/decisions.md, Plugins: "each action goes through a single
//! core function"). Callers resolve the nodes (by uuid, or by path) and map
//! [`FileError`] to their protocol.

use std::path::Path;

use theca_common::api;
use theca_common::chunking::chunks;
use theca_common::compression::{CompressionPolicy, compress};
use theca_common::hash::{ChunkHash, Sha256};
use uuid::Uuid;

use crate::api::AppState;
use crate::db;
use crate::db::account::Credential;
use crate::db::chunk::DeclareError;
use crate::db::library::{Access, NodeAccess};
use crate::storage::BackendError;

/// Why an operation is refused.
#[derive(Debug, thiserror::Error)]
pub enum FileError {
    #[error("{0} not found")]
    NotFound(&'static str),
    #[error("read-only access to this library")]
    ReadOnly,
    #[error("a library's root can't be renamed, moved or deleted")]
    Root,
    #[error("files can't move or be copied to another organization (each pays for its own)")]
    OtherOrganization,
    #[error("a file or folder with this name exists")]
    NameTaken,
    #[error("a library with this name exists")]
    LibraryNameTaken,
    #[error("a folder can't move into itself")]
    MoveIntoItself,
    #[error("invalid name")]
    InvalidName,
    #[error("the organization's quota is exceeded")]
    QuotaExceeded,
    /// The protocol refused a step (should not happen for a server-side upload).
    #[error("refused: {0}")]
    Refused(&'static str),
    #[error("database: {0}")]
    Db(sqlx::Error),
    #[error("storage: {0}")]
    Backend(#[from] BackendError),
    #[error("reading the upload: {0}")]
    Io(#[from] std::io::Error),
}

impl From<sqlx::Error> for FileError {
    fn from(e: sqlx::Error) -> Self {
        match e.as_database_error().and_then(|d| d.constraint()) {
            Some("node_name") => FileError::NameTaken,
            Some("library_name") => FileError::LibraryNameTaken,
            Some("node_no_cycle") => FileError::MoveIntoItself,
            Some("node_name_valid") => FileError::InvalidName,
            _ => FileError::Db(e),
        }
    }
}

fn writable(access: &Access) -> Result<(), FileError> {
    if access.can_write {
        Ok(())
    } else {
        Err(FileError::ReadOnly)
    }
}

fn folder(node: &NodeAccess) -> Result<(), FileError> {
    if node.is_file {
        Err(FileError::NotFound("folder"))
    } else {
        Ok(())
    }
}

fn not_root(node: &NodeAccess) -> Result<(), FileError> {
    if node.parent.is_some() {
        Ok(())
    } else {
        Err(FileError::Root)
    }
}

fn same_organization(a: &NodeAccess, b: &NodeAccess) -> Result<(), FileError> {
    if a.access.organization == b.access.organization {
        Ok(())
    } else {
        Err(FileError::OtherOrganization)
    }
}

/// Creates a folder.
pub async fn create_folder(
    state: &AppState,
    parent: &NodeAccess,
    name: &str,
) -> Result<api::Node, FileError> {
    folder(parent)?;
    writable(&parent.access)?;
    Ok(db::node::create_folder(&state.pool, parent, name).await?)
}

/// Renames and/or moves a node (into any library of the same organization).
/// `replace`: a file onto a file of that name merges into it (decisions.md,
/// Moves and copies). Returns the node it now is.
pub async fn rename_move(
    state: &AppState,
    node: &NodeAccess,
    name: Option<&str>,
    parent: Option<&NodeAccess>,
    replace: bool,
) -> Result<i64, FileError> {
    writable(&node.access)?;
    not_root(node)?;
    if let Some(parent) = parent {
        folder(parent)?;
        writable(&parent.access)?;
        same_organization(node, parent)?;
    }
    if replace && node.is_file {
        let folder = parent
            .map_or(node.parent, |p| Some(p.node))
            .expect("not the root");
        let name = match name {
            Some(name) => name.to_owned(),
            None => db::node::info(&state.pool, node.node).await?.name,
        };
        if let Some((dest, true, _)) = db::node::child_named(&state.pool, folder, &name).await?
            && dest != node.node
        {
            let mut tx = state.pool.begin().await?;
            db::node::graft(&mut tx, node.node, dest).await?;
            tx.commit().await?;
            return Ok(dest);
        }
    }
    db::node::update(&state.pool, node, name, parent).await?;
    Ok(node.node)
}

/// Copies a node into a folder (same organization), reusing its chunks.
/// `replace`: a file onto a file of that name becomes a new version of it.
/// Returns the copy (or the replaced file).
pub async fn copy(
    state: &AppState,
    source: &NodeAccess,
    parent: &NodeAccess,
    name: Option<&str>,
    replace: bool,
) -> Result<i64, FileError> {
    not_root(source)?;
    folder(parent)?;
    writable(&parent.access)?;
    same_organization(source, parent)?;
    if replace && source.is_file {
        let name = match name {
            Some(name) => name.to_owned(),
            None => db::node::info(&state.pool, source.node).await?.name,
        };
        if let Some((dest, true, _)) =
            db::node::child_named(&state.pool, parent.node, &name).await?
            && dest != source.node
        {
            let mut tx = state.pool.begin().await?;
            let content = source.current_version.expect("a file has a version");
            db::node::add_version(&mut tx, &parent.access, dest, content).await?;
            tx.commit().await?;
            return Ok(dest);
        }
    }
    let mut tx = state.pool.begin().await?;
    let copy = db::node::copy(&mut tx, source, parent, name).await?;
    tx.commit().await?;
    Ok(copy)
}

/// Renames a library.
pub async fn rename_library(
    state: &AppState,
    library: &Access,
    name: &str,
) -> Result<(), FileError> {
    writable(library)?;
    Ok(db::library::rename(&state.pool, library.library, name).await?)
}

/// Puts a library and everything in it in its organization's trash.
pub async fn trash_library(state: &AppState, library: &Access) -> Result<(), FileError> {
    writable(library)?;
    Ok(db::library::trash(&state.pool, library.library).await?)
}

/// Takes a trashed library out of the trash (renamed if its name was taken).
pub async fn restore_library(state: &AppState, library: &Access) -> Result<(), FileError> {
    writable(library)?;
    let mut tx = state.pool.begin().await?;
    db::library::restore(&mut tx, library.library).await?;
    tx.commit().await?;
    Ok(())
}

/// Deletes a trashed library for good (its chunks go with the GC).
pub async fn purge_library(state: &AppState, library: &Access) -> Result<(), FileError> {
    writable(library)?;
    let mut tx = state.pool.begin().await?;
    db::library::purge(&mut tx, library.library).await?;
    tx.commit().await?;
    Ok(())
}

/// Puts a node in the trash.
pub async fn trash(state: &AppState, node: &NodeAccess) -> Result<(), FileError> {
    writable(&node.access)?;
    not_root(node)?;
    Ok(db::node::trash(&state.pool, node.node).await?)
}

/// Where uploaded bytes go.
pub enum Target<'a> {
    /// A new file in a folder.
    New {
        parent: &'a NodeAccess,
        name: &'a str,
    },
    /// A new version of a file, based on its current one (last write wins).
    Existing(&'a NodeAccess),
}

/// Stores a whole file the server received (WebDAV): chunked, hashed and
/// deduplicated here with the clients' own code, each new chunk going through
/// the protocol's steps (lookup, declaration with the quota check, the object,
/// then `uploaded_at`), so a row exists before its object, as always. Then
/// committed. Returns the file.
// ponytail: reads, hashes and compresses on the async thread (a chunk at a
// time, from a local file); spawn_blocking if it shows up in latency.
pub async fn store(
    state: &AppState,
    who: &Credential,
    target: Target<'_>,
    path: &Path,
) -> Result<Uuid, FileError> {
    let access = match &target {
        Target::New { parent, .. } => {
            folder(parent)?;
            parent.access
        }
        Target::Existing(file) => file.access,
    };
    writable(&access)?;
    let backend = state.backends.get(access.storage).await?;
    let policy = CompressionPolicy::default();
    let mut ids = Vec::new();
    for chunk in chunks(std::fs::File::open(path)?) {
        let plain = chunk?.data;
        metrics::counter!("theca_bytes_total", "direction" => "in").increment(plain.len() as u64);
        let hash = ChunkHash::of(&plain);
        let entry = db::chunk::lookup(
            &state.pool,
            &access,
            &[api::LookupChunk {
                hash,
                plain_size: plain.len() as u64,
            }],
        )
        .await?
        .remove(0);
        if entry.state == api::ChunkState::Upload {
            let (compression, stored) =
                compress(&policy, &plain).map_err(|e| std::io::Error::other(e.to_string()))?;
            let declare = api::Declare {
                chunk: entry.chunk,
                stored_size: stored.len() as u64,
                compression,
                stored_sha256: Sha256::of(&stored),
            };
            let lifetime = state.settings.get().presigned_url_lifetime;
            db::chunk::declare(&state.pool, who, &[declare], lifetime, i64::MAX)
                .await
                .map_err(|e| match e {
                    DeclareError::QuotaExceeded { .. } => FileError::QuotaExceeded,
                    DeclareError::Db(e) => FileError::Db(e),
                    _ => FileError::Refused("chunk declaration"),
                })?;
            backend.put(entry.chunk, stored).await?;
            db::chunk::mark_uploaded(&state.pool, entry.chunk).await?;
        }
        ids.push(entry.chunk);
    }
    let target = match target {
        Target::New { parent, name } => db::file::Target::New {
            parent: parent_uuid(state, parent).await?,
            name,
        },
        Target::Existing(file) => db::file::Target::File {
            file: node_uuid(state, file.node).await?,
            base_version: match file.current_version {
                Some(v) => Some(version_uuid(state, v).await?),
                None => None,
            },
        },
    };
    let mut tx = state.pool.begin().await?;
    let committed = db::file::commit(&mut tx, &access, target, &ids, None)
        .await
        .map_err(|e| match e {
            db::file::CommitError::Db(e) => FileError::from(e),
            db::file::CommitError::FileNotFound => FileError::NotFound("file"),
            db::file::CommitError::ParentNotFound => FileError::NotFound("folder"),
            db::file::CommitError::BaseNotFound => FileError::Refused("base version"),
            db::file::CommitError::ChunksMissing(_) => FileError::Refused("chunks missing"),
            db::file::CommitError::ChunksNotAllowed(_) => {
                FileError::Refused("chunks outside the dedup scope")
            }
        })?;
    tx.commit().await?;
    Ok(committed.file)
}

async fn node_uuid(state: &AppState, node: i64) -> sqlx::Result<Uuid> {
    sqlx::query_scalar!("SELECT uuid FROM node WHERE pk = $1", node)
        .fetch_one(&state.pool)
        .await
}

async fn parent_uuid(state: &AppState, parent: &NodeAccess) -> sqlx::Result<Uuid> {
    node_uuid(state, parent.node).await
}

async fn version_uuid(state: &AppState, version: i64) -> sqlx::Result<Uuid> {
    sqlx::query_scalar!("SELECT uuid FROM version WHERE pk = $1", version)
        .fetch_one(&state.pool)
        .await
}
