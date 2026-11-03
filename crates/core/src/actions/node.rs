//! Files and folders: folders, commits, renames, moves, copies, the trash,
//! versions.

use std::collections::HashMap;
use std::path::Path;

use chrono::{DateTime, Utc};
use theca_common::api;
use theca_common::api::EventAction;
use theca_common::chunking::chunks;
use theca_common::compression::{CompressionPolicy, compress};
use theca_common::hash::{ChunkHash, Sha256};
use theca_common::name::Name;
use uuid::Uuid;

use super::{Changes, FileError, commit, folder, not_root, same_organization, writable};
use crate::api::AppState;
use crate::db;
use crate::db::account::Credential;
use crate::db::chunk::DeclareError;
use crate::db::file::{CommitError, Committed};
use crate::db::library::{Access, NodeAccess};

/// Folders below `parent`, like `mkdir -p`: each path's missing folders
/// created, existing ones kept, in one transaction. Each path's folder, and
/// whether this created it.
pub async fn create_folders(
    state: &AppState,
    parent: &NodeAccess,
    paths: &[String],
) -> Result<Vec<api::FolderPath>, FileError> {
    folder(parent)?;
    writable(&parent.access)?;
    let mut tx = state.pool.begin().await?;
    let mut changes = Changes::by(Some(parent.access.account));
    // Folders already walked, by their path in lower case.
    let mut known: HashMap<String, (i64, Uuid, bool)> = HashMap::new();
    let mut out = Vec::with_capacity(paths.len());
    for path in paths {
        let mut under = parent.node;
        let mut walked: Vec<String> = Vec::new();
        let mut last = None;
        for segment in path.split('/').filter(|s| !s.is_empty()) {
            let name = Name::new(segment).map_err(|_| FileError::InvalidName)?;
            walked.push(name.as_str().to_owned());
            let key = walked.join("/").to_lowercase();
            let folder = match known.get(&key) {
                Some(folder) => *folder,
                None => {
                    let folder = db::node::folder_child(&mut tx, parent, under, name.as_str())
                        .await?
                        .ok_or(FileError::NameTaken)?;
                    if folder.2 {
                        changes.node(parent.access.library, folder.1, EventAction::Created);
                    }
                    known.insert(key, folder);
                    folder
                }
            };
            under = folder.0;
            last = Some(folder);
        }
        let (_, id, created) = last.ok_or(FileError::InvalidName)?;
        out.push(api::FolderPath {
            path: walked.join("/"),
            id,
            created,
        });
    }
    commit(state, tx, changes).await?;
    Ok(out)
}

/// Where a file commit goes.
pub use db::file::Target as CommitTarget;

/// Commits chunks as a new file or a new version (decisions.md, Upload flow).
pub async fn commit_file(
    state: &AppState,
    access: &Access,
    target: CommitTarget<'_>,
    chunks: &[Uuid],
    message: Option<&str>,
    modified: Option<DateTime<Utc>>,
) -> Result<Committed, CommitError> {
    let new = matches!(target, CommitTarget::New { .. });
    let mut tx = state.pool.begin().await?;
    let committed = db::file::commit(&mut tx, access, target, chunks, message, modified).await?;
    let mut changes = Changes::by(Some(access.account));
    let action = if new {
        Some(EventAction::Created)
    } else if !committed.unchanged {
        Some(EventAction::Versioned)
    } else if committed.touched {
        Some(EventAction::Touched)
    } else {
        None
    };
    if let Some(action) = action {
        changes.node(access.library, committed.file, action);
    }
    commit(state, tx, changes).await?;
    Ok(committed)
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
    let mut changes = Changes::by(Some(node.access.account));
    if replace && node.is_file {
        let folder = parent
            .map_or(node.parent, |p| Some(p.node))
            .expect("not the root");
        let name = match name {
            Some(name) => name.to_owned(),
            None => db::node::info(&state.pool, node.node).await?.name,
        };
        if let Some(dest) = db::node::child_named(&state.pool, folder, &name).await?
            && dest.is_file
            && dest.pk != node.node
        {
            let mut tx = state.pool.begin().await?;
            db::node::graft(&mut tx, node.node, dest.pk).await?;
            let library = parent.map_or(node.access.library, |p| p.access.library);
            changes.node(library, dest.uuid, EventAction::Versioned);
            changes.node(node.access.library, node.uuid, EventAction::Purged);
            commit(state, tx, changes).await?;
            return Ok(dest.pk);
        }
    }
    let mut tx = state.pool.begin().await?;
    if let Some(p) = parent.filter(|p| p.access.library != node.access.library) {
        // Both libraries, in pk order, before the move's trigger locks the
        // destination (node_no_cycle) and the commit both: two moves crossing
        // between the same libraries would otherwise deadlock.
        db::library::lock(&mut tx, &[node.access.library, p.access.library]).await?;
    }
    db::node::update(&mut tx, node, name, parent).await?;
    match parent {
        Some(p) if p.access.library != node.access.library => {
            changes.node(node.access.library, node.uuid, EventAction::Moved);
            changes.node(p.access.library, node.uuid, EventAction::Moved);
        }
        Some(p) if Some(p.node) != node.parent => {
            changes.node(node.access.library, node.uuid, EventAction::Moved);
        }
        _ => changes.node(node.access.library, node.uuid, EventAction::Renamed),
    }
    commit(state, tx, changes).await?;
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
    let mut changes = Changes::by(Some(parent.access.account));
    if replace && source.is_file {
        let name = match name {
            Some(name) => name.to_owned(),
            None => db::node::info(&state.pool, source.node).await?.name,
        };
        if let Some(dest) = db::node::child_named(&state.pool, parent.node, &name).await?
            && dest.is_file
            && dest.pk != source.node
        {
            let mut tx = state.pool.begin().await?;
            let content = source.current_version.expect("a file has a version");
            db::node::add_version(&mut tx, &parent.access, dest.pk, content).await?;
            changes.node(parent.access.library, dest.uuid, EventAction::Versioned);
            commit(state, tx, changes).await?;
            return Ok(dest.pk);
        }
    }
    let mut tx = state.pool.begin().await?;
    let (copy, uuid) = db::node::copy(&mut tx, source, parent, name).await?;
    changes.node(parent.access.library, uuid, EventAction::Created);
    commit(state, tx, changes).await?;
    Ok(copy)
}

/// Puts a node in the trash (its subtree follows it: one event).
pub async fn trash(state: &AppState, node: &NodeAccess) -> Result<(), FileError> {
    writable(&node.access)?;
    not_root(node)?;
    let mut tx = state.pool.begin().await?;
    db::node::trash(&mut tx, node.node).await?;
    let mut changes = Changes::by(Some(node.access.account));
    changes.node(node.access.library, node.uuid, EventAction::Trashed);
    commit(state, tx, changes).await?;
    Ok(())
}

/// Takes nodes trashed earlier out of the trash without an event: WebDAV
/// removes a folder's children before the folder (`dav-server` goes bottom
/// up), then keeps one trash entry, the folder's. Unreachable below it, they
/// are already `gone` in the feed.
pub async fn untrash_below_trashed(state: &AppState, nodes: &[i64]) -> Result<(), FileError> {
    let mut tx = state.pool.begin().await?;
    db::node::untrash(&mut tx, nodes).await?;
    commit(state, tx, Changes::default()).await?;
    Ok(())
}

/// Takes a trashed node out of the trash: back in its folder if that one is
/// reachable, else into the library's root; renamed if its name is taken.
pub async fn restore(state: &AppState, node: &NodeAccess) -> Result<api::Restored, FileError> {
    writable(&node.access)?;
    let mut tx = state.pool.begin().await?;
    let restored = db::node::restore(&mut tx, node).await?;
    let mut changes = Changes::by(Some(node.access.account));
    changes.node(node.access.library, node.uuid, EventAction::Restored);
    commit(state, tx, changes).await?;
    Ok(restored)
}

/// Deletes a trashed node for good, with its subtree and their histories.
/// Returns the chunks they referenced, for `jobs::release`.
pub async fn purge(state: &AppState, node: &NodeAccess) -> Result<Vec<i64>, FileError> {
    writable(&node.access)?;
    let mut tx = state.pool.begin().await?;
    let freed = db::node::purge(&mut tx, node.node).await?;
    let mut changes = Changes::by(Some(node.access.account));
    changes.node(node.access.library, node.uuid, EventAction::Purged);
    commit(state, tx, changes).await?;
    Ok(freed)
}

/// The trash retention's purge of a node (a job: no actor).
pub async fn purge_expired(state: &AppState, node: i64) -> sqlx::Result<()> {
    let mut tx = state.pool.begin().await?;
    let Some((library, uuid)) = db::node::library_of(&mut tx, node).await? else {
        // Inside a folder purged earlier in the same run.
        return Ok(());
    };
    db::node::purge(&mut tx, node).await?;
    let mut changes = Changes::by(None);
    changes.node(library, uuid, EventAction::Purged);
    commit(state, tx, changes).await
}

/// Makes an old version current: the next commit based on it branches the tree.
pub async fn restore_version(
    state: &AppState,
    file: &NodeAccess,
    version: i64,
) -> Result<(), FileError> {
    writable(&file.access)?;
    let mut tx = state.pool.begin().await?;
    db::node::set_current(&mut tx, file.node, version).await?;
    let mut changes = Changes::by(Some(file.access.account));
    changes.node(file.access.library, file.uuid, EventAction::VersionRestored);
    commit(state, tx, changes).await?;
    Ok(())
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
/// committed, with the client's modification time if it gave one. Returns the
/// file.
// ponytail: reads, hashes and compresses on the async thread (a chunk at a
// time, from a local file); spawn_blocking if it shows up in latency.
pub async fn store(
    state: &AppState,
    who: &Credential,
    target: Target<'_>,
    path: &Path,
    modified: Option<DateTime<Utc>>,
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
        Target::New { parent, name } => CommitTarget::New {
            parent: parent.uuid,
            name,
        },
        Target::Existing(file) => CommitTarget::File {
            file: file.uuid,
            base_version: match file.current_version {
                Some(v) => Some(db::node::version_uuid(&state.pool, v).await?),
                None => None,
            },
        },
    };
    let committed = commit_file(state, &access, target, &ids, None, modified)
        .await
        .map_err(|e| match e {
            CommitError::Db(e) => FileError::from(e),
            CommitError::FileNotFound => FileError::NotFound("file"),
            CommitError::ParentNotFound => FileError::NotFound("folder"),
            CommitError::BaseNotFound => FileError::Refused("base version"),
            CommitError::ChunksMissing(_) => FileError::Refused("chunks missing"),
            CommitError::ChunksNotAllowed(_) => {
                FileError::Refused("chunks outside the dedup scope")
            }
        })?;
    Ok(committed.file)
}
