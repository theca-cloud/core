//! Every change to files, libraries and library access, one function per
//! change, shared by the API, WebDAV and the jobs (docs/decisions.md, Sync;
//! Plugins). Callers resolve the nodes (by uuid, or by path) and map
//! [`FileError`] to their protocol; they never write through `db::*`
//! themselves. Each change records its events in [`Changes`], and [`commit`]
//! writes them, commits, and wakes the event feed's streams.

pub mod library;
pub mod member;
pub mod node;

use std::collections::BTreeMap;

use sqlx::{Postgres, Transaction};
use theca_common::api::{EventAction, ResourceKind};
use uuid::Uuid;

use crate::api::AppState;
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

struct Event {
    resource: Uuid,
    kind: ResourceKind,
    action: EventAction,
}

/// What a transaction changed: the events to write, per library, and the
/// accounts whose access changed (their streams resend `libraries`).
#[derive(Default)]
pub struct Changes {
    /// By library `pk`: the order [`commit`] takes their rows in.
    events: BTreeMap<i64, Vec<Event>>,
    accounts: Vec<i64>,
    /// Who acted; `None` for a job.
    actor: Option<i64>,
}

impl Changes {
    /// Changes made by `actor` (an account), or by a job (`None`).
    pub fn by(actor: Option<i64>) -> Self {
        Self {
            actor,
            ..Self::default()
        }
    }

    pub fn node(&mut self, library: i64, node: Uuid, action: EventAction) {
        self.events.entry(library).or_default().push(Event {
            resource: node,
            kind: ResourceKind::Node,
            action,
        });
    }

    /// The library's own event (the database refuses a node action for it).
    pub fn library(&mut self, library: i64, uuid: Uuid, action: EventAction) {
        self.events.entry(library).or_default().push(Event {
            resource: uuid,
            kind: ResourceKind::Library,
            action,
        });
    }

    /// `account`'s access to libraries changed.
    pub fn access(&mut self, account: i64) {
        self.accounts.push(account);
    }
}

/// Writes the events, commits, then wakes the streams of the libraries and
/// accounts concerned. The only way to commit a transaction that changed
/// files, libraries or access.
///
/// Each library's counter goes up by its number of events, libraries taken in
/// `pk` order (against deadlocks between two commits), just before the commit:
/// the row lock it takes lasts until the commit, so commits into one library
/// number their events in commit order, and a rollback leaves no gap.
pub async fn commit(
    state: &AppState,
    mut tx: Transaction<'_, Postgres>,
    changes: Changes,
) -> sqlx::Result<()> {
    for (library, events) in &changes.events {
        let n = events.len() as i64;
        let last = sqlx::query_scalar!(
            "UPDATE library SET event_seq = event_seq + $2 WHERE pk = $1 RETURNING event_seq",
            library,
            n
        )
        .fetch_one(&mut *tx)
        .await?;
        let resources: Vec<Uuid> = events.iter().map(|e| e.resource).collect();
        let kinds: Vec<&str> = events.iter().map(|e| e.kind.as_str()).collect();
        let actions: Vec<&str> = events.iter().map(|e| e.action.as_str()).collect();
        sqlx::query!(
            "INSERT INTO event (library, seq, resource, resource_kind, action, actor)
             SELECT $1, $2 + ordinality, resource, kind, action, $6
             FROM unnest($3::uuid[], $4::text[], $5::text[]) WITH ORDINALITY AS t (resource, kind, action)",
            library,
            last - n,
            &resources,
            &kinds as &[&str],
            &actions as &[&str],
            changes.actor
        )
        .execute(&mut *tx)
        .await?;
    }
    tx.commit().await?;
    let libraries: Vec<i64> = changes.events.into_keys().collect();
    state.events.wake(&libraries, &changes.accounts);
    Ok(())
}
