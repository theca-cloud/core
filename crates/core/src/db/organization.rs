//! Organizations and their members.

use sqlx::{PgConnection, PgPool};
use theca_common::api;
use uuid::Uuid;

use super::account::Credential;
use crate::email::{self, Email};

/// Creates an organization owned by `account` (joined), personal or not.
/// Returns its pk.
pub async fn create(
    db: &mut PgConnection,
    account: i64,
    name: &str,
    personal: bool,
) -> sqlx::Result<i64> {
    // The owner FK is deferred: reserve the owner's pk, insert the
    // organization, then its owner.
    let owner =
        sqlx::query_scalar!(r#"SELECT nextval(pg_get_serial_sequence('"user"', 'pk')) AS "pk!""#)
            .fetch_one(&mut *db)
            .await?;
    let org = sqlx::query_scalar!(
        "INSERT INTO organization (name, owner, personal_of) VALUES ($1, $2, $3) RETURNING pk",
        name,
        owner,
        personal.then_some(account)
    )
    .fetch_one(&mut *db)
    .await?;
    sqlx::query!(
        r#"INSERT INTO "user" (pk, account, organization, joined_at) OVERRIDING SYSTEM VALUE
           VALUES ($1, $2, $3, now())"#,
        owner,
        account,
        org
    )
    .execute(&mut *db)
    .await?;
    Ok(org)
}

/// An organization the credential's account owns: `(pk, name, personal)`.
pub struct Owned {
    pub pk: i64,
    pub name: String,
    pub personal: bool,
}

/// The organization, if the credential's account owns it (and reaches it).
pub async fn owned(
    pool: &PgPool,
    who: &Credential,
    organization: Uuid,
) -> sqlx::Result<Option<Owned>> {
    sqlx::query_as!(
        Owned,
        r#"SELECT o.pk, o.name, o.personal_of IS NOT NULL AS "personal!"
           FROM organization o JOIN "user" u ON u.pk = o.owner
           WHERE o.uuid = $1 AND u.account = $2 AND ($3::bigint IS NULL OR o.pk = $3)"#,
        organization,
        who.account,
        who.organization
    )
    .fetch_optional(pool)
    .await
}

/// The organization, if the credential's account is a joined member of it.
pub async fn joined(
    pool: &PgPool,
    who: &Credential,
    organization: Uuid,
) -> sqlx::Result<Option<i64>> {
    sqlx::query_scalar!(
        r#"SELECT a.organization AS "organization!" FROM organization_access($1) a
           JOIN organization o ON o.pk = a.organization WHERE o.uuid = $2"#,
        who.token,
        organization
    )
    .fetch_optional(pool)
    .await
}

/// The organization's members, pending invitations included.
pub async fn members(pool: &PgPool, organization: i64) -> sqlx::Result<Vec<api::Member>> {
    sqlx::query_as!(
        api::Member,
        r#"SELECT u.uuid AS id, a.email, u.joined_at IS NOT NULL AS "joined!", o.owner = u.pk AS "owner!"
           FROM "user" u JOIN account a ON a.pk = u.account JOIN organization o ON o.pk = u.organization
           WHERE u.organization = $1 ORDER BY lower(a.email)"#,
        organization
    )
    .fetch_all(pool)
    .await
}

/// What adding an address did.
pub enum Added {
    /// An account with a way to log in: joined at once.
    Joined(Uuid),
    /// No such account: invited (pending), the invitation emailed.
    Invited(Uuid),
    AlreadyMember,
}

/// Adds an address to an organization: an existing account (one that can log
/// in) joins directly; otherwise an account without identity is created (or
/// reused, if invited elsewhere) with a pending user, and an invitation sent.
pub async fn add(
    db: &mut PgConnection,
    organization: &Owned,
    inviter: &str,
    email: &str,
) -> sqlx::Result<Added> {
    let account = sqlx::query!(
        r#"SELECT a.pk, EXISTS (SELECT 1 FROM identity i WHERE i.account = a.pk) AS "can_log_in!",
                  u.uuid AS "user?", u.joined_at IS NOT NULL AS "joined?"
           FROM account a LEFT JOIN "user" u ON u.account = a.pk AND u.organization = $2
           WHERE lower(a.email) = lower($1)"#,
        email,
        organization.pk
    )
    .fetch_optional(&mut *db)
    .await?;
    if let Some(a) = &account {
        if a.joined == Some(true) {
            return Ok(Added::AlreadyMember);
        }
        if a.can_log_in {
            let user = sqlx::query_scalar!(
                r#"INSERT INTO "user" (account, organization, joined_at) VALUES ($1, $2, now())
                   ON CONFLICT (account, organization) DO UPDATE SET joined_at = now()
                   RETURNING uuid"#,
                a.pk,
                organization.pk
            )
            .fetch_one(&mut *db)
            .await?;
            let notice = Email::MemberAdded {
                organization: organization.name.clone(),
                inviter: inviter.to_owned(),
            };
            email::enqueue(db, email, &notice).await?;
            return Ok(Added::Joined(user));
        }
    }
    let account = match account {
        Some(a) => a.pk,
        None => {
            sqlx::query_scalar!(
                "INSERT INTO account (email) VALUES ($1) RETURNING pk",
                email
            )
            .fetch_one(&mut *db)
            .await?
        }
    };
    let user = sqlx::query_scalar!(
        r#"INSERT INTO "user" (account, organization, invited_at) VALUES ($1, $2, now())
           ON CONFLICT (account, organization) DO UPDATE SET invited_at = now()
           RETURNING uuid"#,
        account,
        organization.pk
    )
    .fetch_one(&mut *db)
    .await?;
    let invitation = Email::Invitation {
        user,
        organization: organization.name.clone(),
        inviter: inviter.to_owned(),
    };
    email::enqueue(db, email, &invitation).await?;
    Ok(Added::Invited(user))
}

/// Why a member can't be removed.
pub enum RemoveError {
    NotFound,
    Owner,
    Db(sqlx::Error),
}

impl From<sqlx::Error> for RemoveError {
    fn from(e: sqlx::Error) -> Self {
        Self::Db(e)
    }
}

/// Removes a member (or cancels an invitation). An invited account left with
/// no identity and no other membership goes too.
pub async fn remove(
    db: &mut PgConnection,
    organization: i64,
    user: Uuid,
) -> Result<(), RemoveError> {
    let row = sqlx::query!(
        r#"SELECT u.pk, u.account, o.owner = u.pk AS "owner!" FROM "user" u
           JOIN organization o ON o.pk = u.organization WHERE u.uuid = $1 AND u.organization = $2"#,
        user,
        organization
    )
    .fetch_optional(&mut *db)
    .await?
    .ok_or(RemoveError::NotFound)?;
    if row.owner {
        return Err(RemoveError::Owner);
    }
    sqlx::query!(r#"DELETE FROM "user" WHERE pk = $1"#, row.pk)
        .execute(&mut *db)
        .await?;
    delete_if_phantom(db, row.account).await?;
    Ok(())
}

/// Deletes an account created by an invitation that never got a way to log
/// in, once it has no membership left.
pub async fn delete_if_phantom(db: &mut PgConnection, account: i64) -> sqlx::Result<()> {
    sqlx::query!(
        r#"DELETE FROM account a WHERE a.pk = $1
             AND NOT EXISTS (SELECT 1 FROM identity WHERE account = a.pk)
             AND NOT EXISTS (SELECT 1 FROM "user" WHERE account = a.pk)"#,
        account
    )
    .execute(db)
    .await?;
    Ok(())
}
