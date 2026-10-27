//! Deleting organizations and accounts (GDPR erasure: immediate hard deletion).
//!
//! An organization's data goes at once; its chunks are tombstoned for the GC
//! (rows outlive their objects: decisions.md, Garbage collection), so the
//! organization row stays, marked deleted and ownerless, until the cleanup
//! job finds it without chunks.

use sqlx::{PgConnection, PgPool};
use uuid::Uuid;

/// Deletes an organization's data: libraries (nodes, versions, chunk
/// references), members, namespaces, identity providers, restricted
/// sessions and scoped app passwords; tombstones its chunks.
pub async fn organization(db: &mut PgConnection, organization: i64) -> sqlx::Result<()> {
    sqlx::query!(
        "UPDATE organization SET deleted_at = now(), owner = NULL, personal_of = NULL, default_storage = NULL
         WHERE pk = $1",
        organization
    )
    .execute(&mut *db)
    .await?;
    sqlx::query!(
        "UPDATE node SET current_version = NULL WHERE organization = $1 AND current_version IS NOT NULL",
        organization
    )
    .execute(&mut *db)
    .await?;
    sqlx::query!(
        "DELETE FROM chunk_in_version WHERE organization = $1",
        organization
    )
    .execute(&mut *db)
    .await?;
    sqlx::query!("DELETE FROM version WHERE organization = $1", organization)
        .execute(&mut *db)
        .await?;
    sqlx::query!("DELETE FROM node WHERE organization = $1", organization)
        .execute(&mut *db)
        .await?;
    sqlx::query!(
        "DELETE FROM library_namespace WHERE organization = $1",
        organization
    )
    .execute(&mut *db)
    .await?;
    // Members of its libraries go with them (cascade).
    sqlx::query!("DELETE FROM library WHERE owner = $1", organization)
        .execute(&mut *db)
        .await?;
    sqlx::query!(
        "DELETE FROM namespace WHERE organization = $1",
        organization
    )
    .execute(&mut *db)
    .await?;
    sqlx::query!(
        "DELETE FROM identity_provider WHERE organization = $1",
        organization
    )
    .execute(&mut *db)
    .await?;
    sqlx::query!("DELETE FROM token WHERE organization = $1", organization)
        .execute(&mut *db)
        .await?;
    // Invitation tokens point to the (pending) users.
    sqlx::query!(
        r#"DELETE FROM token WHERE usage = 'invitation'
             AND resource IN (SELECT uuid FROM "user" WHERE organization = $1)"#,
        organization
    )
    .execute(&mut *db)
    .await?;
    let invited: Vec<i64> = sqlx::query_scalar!(
        r#"DELETE FROM "user" WHERE organization = $1 RETURNING account"#,
        organization
    )
    .fetch_all(&mut *db)
    .await?;
    for account in invited {
        super::organization::delete_if_phantom(db, account).await?;
    }
    sqlx::query!(
        "UPDATE chunk SET deleting_at = coalesce(deleting_at, now()) WHERE organization = $1",
        organization
    )
    .execute(&mut *db)
    .await?;
    Ok(())
}

/// The company organizations an account owns (deleting it needs them
/// transferred or deleted first).
pub async fn owned_organizations(pool: &PgPool, account: i64) -> sqlx::Result<Vec<String>> {
    sqlx::query_scalar!(
        r#"SELECT o.name FROM organization o JOIN "user" u ON u.pk = o.owner
           WHERE u.account = $1 AND o.personal_of IS NULL AND o.deleted_at IS NULL
           ORDER BY lower(o.name)"#,
        account
    )
    .fetch_all(pool)
    .await
}

/// Deletes an account: its personal organization, its memberships,
/// identities, tokens, queued and sent emails, then the account (its versions
/// stay, authorless: `version.author` is the account). The caller checked it owns no company organization.
pub async fn account(db: &mut PgConnection, account: i64) -> sqlx::Result<()> {
    let personal: Option<i64> = sqlx::query_scalar!(
        "SELECT pk FROM organization WHERE personal_of = $1",
        account
    )
    .fetch_optional(&mut *db)
    .await?;
    if let Some(personal) = personal {
        organization(db, personal).await?;
    }
    let row = sqlx::query!("SELECT uuid, email FROM account WHERE pk = $1", account)
        .fetch_one(&mut *db)
        .await?;
    sqlx::query!(
        r#"DELETE FROM token WHERE resource = $1
             OR resource IN (SELECT uuid FROM "user" WHERE account = $2)"#,
        row.uuid,
        account
    )
    .execute(&mut *db)
    .await?;
    sqlx::query!(r#"DELETE FROM "user" WHERE account = $1"#, account)
        .execute(&mut *db)
        .await?;
    sqlx::query!(
        "DELETE FROM email WHERE lower(recipient) = lower($1)",
        row.email
    )
    .execute(&mut *db)
    .await?;
    // Identities go with it (cascade).
    sqlx::query!("DELETE FROM account WHERE pk = $1", account)
        .execute(&mut *db)
        .await?;
    Ok(())
}

/// Moves an organization to another of its joined members.
pub async fn transfer(
    db: &mut PgConnection,
    organization: i64,
    member: Uuid,
) -> sqlx::Result<bool> {
    let done = sqlx::query!(
        r#"UPDATE organization o SET owner = u.pk
           FROM "user" u
           WHERE o.pk = $1 AND u.uuid = $2 AND u.organization = o.pk AND u.joined_at IS NOT NULL"#,
        organization,
        member
    )
    .execute(db)
    .await?;
    Ok(done.rows_affected() == 1)
}
