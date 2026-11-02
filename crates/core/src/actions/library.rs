//! Libraries: creation, renaming, the trash.

use theca_common::api;
use uuid::Uuid;

use theca_common::api::EventAction;

use super::{Changes, FileError, commit, writable};
use crate::api::AppState;
use crate::db;
use crate::db::account::Credential;
use crate::db::library::{Access, CreateError};

/// Creates a library with its root, its creator as `write` member, in the
/// given namespaces. No event: it enters its creator's `libraries`.
pub async fn create(
    state: &AppState,
    who: &Credential,
    organization: Uuid,
    name: &str,
    namespaces: &[Uuid],
) -> Result<api::Library, CreateError> {
    let mut tx = state.pool.begin().await?;
    let library = db::library::create(&mut tx, who, organization, name, namespaces).await?;
    let mut changes = Changes::by(Some(who.account));
    changes.access(who.account);
    commit(state, tx, changes).await?;
    Ok(library)
}

/// Renames a library.
pub async fn rename(state: &AppState, library: &Access, name: &str) -> Result<(), FileError> {
    writable(library)?;
    let uuid = db::library::uuid_of(&state.pool, library.library).await?;
    let mut tx = state.pool.begin().await?;
    db::library::rename(&mut tx, library.library, name).await?;
    let mut changes = Changes::by(Some(library.account));
    changes.library(library.library, uuid, EventAction::Renamed);
    commit(state, tx, changes).await?;
    Ok(())
}

/// Puts a library and everything in it in its organization's trash.
pub async fn trash(state: &AppState, library: &Access) -> Result<(), FileError> {
    writable(library)?;
    let uuid = db::library::uuid_of(&state.pool, library.library).await?;
    let mut tx = state.pool.begin().await?;
    db::library::trash(&mut tx, library.library).await?;
    let mut changes = Changes::by(Some(library.account));
    changes.library(library.library, uuid, EventAction::Trashed);
    commit(state, tx, changes).await?;
    Ok(())
}

/// Takes a trashed library out of the trash (renamed if its name was taken).
pub async fn restore(state: &AppState, library: &Access) -> Result<(), FileError> {
    writable(library)?;
    let uuid = db::library::uuid_of(&state.pool, library.library).await?;
    let mut tx = state.pool.begin().await?;
    db::library::restore(&mut tx, library.library).await?;
    let mut changes = Changes::by(Some(library.account));
    changes.library(library.library, uuid, EventAction::Restored);
    commit(state, tx, changes).await?;
    Ok(())
}

/// Deletes a trashed library for good; its chunks are released at once. No
/// event (its events go with it): it leaves its members' `libraries`.
pub async fn purge(state: &AppState, library: &Access) -> Result<(), FileError> {
    writable(library)?;
    let freed = purge_library(state, library.library, Some(library.account)).await?;
    crate::jobs::release(state, &freed).await;
    Ok(())
}

/// The trash retention's purge of a library (a job: the chunks wait for the
/// GC's grace, as decisions.md says).
pub async fn purge_expired(state: &AppState, library: i64) -> sqlx::Result<()> {
    purge_library(state, library, None).await.map(|_| ())
}

async fn purge_library(
    state: &AppState,
    library: i64,
    actor: Option<i64>,
) -> sqlx::Result<Vec<i64>> {
    let mut tx = state.pool.begin().await?;
    let mut changes = Changes::by(actor);
    for account in db::library::member_accounts(&mut tx, library).await? {
        changes.access(account);
    }
    let freed = db::library::purge(&mut tx, library).await?;
    commit(state, tx, changes).await?;
    Ok(freed)
}
