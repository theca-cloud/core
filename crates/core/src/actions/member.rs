//! Who can access libraries: library members, organization members. These
//! write no event; the accounts concerned get a new `libraries` on their
//! streams.

use sqlx::{Postgres, Transaction};
use theca_common::api;
use uuid::Uuid;

use super::{Changes, commit};
use crate::api::AppState;
use crate::db;
use crate::db::organization::{Added, Owned, RemoveError};
use crate::db::sharing::Managed;

/// Adds a member of the organization to the library, or changes its role
/// (`writable` changes). `false`: no such member in the organization.
pub async fn set(
    state: &AppState,
    library: Managed,
    user: Uuid,
    write: bool,
) -> sqlx::Result<bool> {
    let mut tx = state.pool.begin().await?;
    let Some(account) = db::sharing::set_member(&mut tx, library, user, write).await? else {
        return Ok(false);
    };
    let mut changes = Changes::default();
    changes.access(account);
    commit(state, tx, changes).await?;
    Ok(true)
}

/// Takes a member's access to the library away. `false`: not a member.
pub async fn remove(state: &AppState, library: i64, user: Uuid) -> sqlx::Result<bool> {
    let mut tx = state.pool.begin().await?;
    let Some(account) = db::sharing::remove_member(&mut tx, library, user).await? else {
        return Ok(false);
    };
    let mut changes = Changes::default();
    changes.access(account);
    commit(state, tx, changes).await?;
    Ok(true)
}

/// Adds an address to an organization (`db::organization::add`). An account
/// that joins at once reaches the library memberships prepared for an
/// earlier invitation. `None`: an invitation is needed but `can_invite` is
/// false (no email): nothing is done.
pub async fn add(
    state: &AppState,
    organization: &Owned,
    inviter: &str,
    email: &str,
    can_invite: bool,
) -> sqlx::Result<Option<Added>> {
    let mut tx = state.pool.begin().await?;
    let added = db::organization::add(&mut tx, organization, inviter, email).await?;
    let mut changes = Changes::default();
    match added {
        Added::Invited(_) if !can_invite => return Ok(None),
        Added::Joined(user) => changes.access(
            db::organization::account_of(&mut tx, user)
                .await?
                .expect("just joined"),
        ),
        _ => {}
    }
    commit(state, tx, changes).await?;
    Ok(Some(added))
}

/// An invitation accepted, in the transaction that checked it: the
/// memberships prepared for it become reachable.
pub async fn join(
    state: &AppState,
    mut tx: Transaction<'_, Postgres>,
    user: i64,
    account: i64,
) -> sqlx::Result<()> {
    db::organization::join(&mut tx, user).await?;
    let mut changes = Changes::default();
    changes.access(account);
    commit(state, tx, changes).await
}

/// Removes a member from an organization, with its library memberships.
pub async fn leave(state: &AppState, organization: i64, user: Uuid) -> Result<(), RemoveError> {
    let mut tx = state.pool.begin().await?;
    let account = db::organization::account_of(&mut tx, user).await?;
    db::organization::remove(&mut tx, organization, user).await?;
    let mut changes = Changes::default();
    changes.access(account.expect("found by remove"));
    commit(state, tx, changes).await?;
    Ok(())
}

/// Deletes an organization: every library, at once.
pub async fn delete_organization(state: &AppState, organization: i64) -> sqlx::Result<()> {
    let mut tx = state.pool.begin().await?;
    let mut changes = Changes::default();
    for account in db::organization::member_accounts(&mut tx, organization).await? {
        changes.access(account);
    }
    db::deletion::organization(&mut tx, organization).await?;
    commit(state, tx, changes).await
}

/// Why the admin's update of an organization is refused.
pub enum UpdateError {
    /// The new owner isn't a joined member.
    Member,
    /// No such storage.
    Storage,
    Db(sqlx::Error),
}

impl From<sqlx::Error> for UpdateError {
    fn from(e: sqlx::Error) -> Self {
        Self::Db(e)
    }
}

/// The admin's update of an organization (`db::admin`): suspended or lifted,
/// its libraries leave or come back for every member.
pub async fn update_organization(
    state: &AppState,
    organization: i64,
    update: &api::UpdateOrganization,
) -> Result<(), UpdateError> {
    let mut tx = state.pool.begin().await?;
    if let Some(owner) = &update.owner
        && !db::admin::set_owner(&mut tx, organization, owner).await?
    {
        return Err(UpdateError::Member);
    }
    if !db::admin::update_organization(&mut tx, organization, update).await? {
        return Err(UpdateError::Storage);
    }
    let mut changes = Changes::default();
    if update.suspended.is_some() {
        for account in db::organization::member_accounts(&mut tx, organization).await? {
            changes.access(account);
        }
    }
    commit(state, tx, changes).await?;
    Ok(())
}
