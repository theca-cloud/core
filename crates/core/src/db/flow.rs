//! Login flows: a device logging in through the browser (docs/decisions.md,
//! Authentication). A flow lives 15 minutes; the poll that finds it approved
//! creates the device's app password and deletes it.

use chrono::{DateTime, Utc};
use sqlx::PgPool;
use uuid::Uuid;

use crate::auth::OpaqueToken;
use crate::db::account::{Scope, insert_app_password};

/// A flow waiting for approval, as the approval page shows it.
pub struct Pending {
    pub name: String,
    pub code: String,
    pub created_at: DateTime<Utc>,
}

/// Starts a flow; its UUID.
pub async fn start(pool: &PgPool, name: &str, code: &str, poll_hash: &[u8]) -> sqlx::Result<Uuid> {
    sqlx::query_scalar!(
        "INSERT INTO login_flow (name, code, poll_hash) VALUES ($1, $2, $3) RETURNING uuid",
        name,
        code,
        poll_hash
    )
    .fetch_one(pool)
    .await
}

/// A flow still waiting for approval, and not expired.
pub async fn pending(pool: &PgPool, flow: Uuid) -> sqlx::Result<Option<Pending>> {
    sqlx::query_as!(
        Pending,
        "SELECT name, code, created_at FROM login_flow
         WHERE uuid = $1 AND account IS NULL AND created_at > now() - interval '15 minutes'",
        flow
    )
    .fetch_optional(pool)
    .await
}

/// Approves a pending flow for the account. False: no such pending flow.
pub async fn approve(pool: &PgPool, flow: Uuid, account: i64) -> sqlx::Result<bool> {
    let done = sqlx::query!(
        "UPDATE login_flow SET account = $2
         WHERE uuid = $1 AND account IS NULL AND created_at > now() - interval '15 minutes'",
        flow,
        account
    )
    .execute(pool)
    .await?;
    Ok(done.rows_affected() == 1)
}

/// What a poll finds.
pub enum Polled {
    /// Unknown, expired, or already picked up.
    Gone,
    Pending,
    /// The new app password (the flow is gone).
    Approved {
        token: OpaqueToken,
        uuid: Uuid,
        name: String,
        created_at: DateTime<Utc>,
    },
}

/// The flow whose poll secret hashes to `poll_hash`: approved, its app
/// password is created and the flow deleted, in one transaction (a second poll
/// finds nothing).
pub async fn poll(pool: &PgPool, poll_hash: &[u8]) -> sqlx::Result<Polled> {
    let mut tx = pool.begin().await?;
    let Some(flow) = sqlx::query!(
        r#"SELECT f.pk, f.name, a.uuid AS "account?" FROM login_flow f
           LEFT JOIN account a ON a.pk = f.account
           WHERE f.poll_hash = $1 AND f.created_at > now() - interval '15 minutes'
           FOR UPDATE OF f"#,
        poll_hash
    )
    .fetch_optional(&mut *tx)
    .await?
    else {
        return Ok(Polled::Gone);
    };
    let Some(account) = flow.account else {
        return Ok(Polled::Pending);
    };
    let (token, uuid, created_at) =
        insert_app_password(&mut *tx, account, &flow.name, Scope::Account, false).await?;
    sqlx::query!("DELETE FROM login_flow WHERE pk = $1", flow.pk)
        .execute(&mut *tx)
        .await?;
    tx.commit().await?;
    Ok(Polled::Approved {
        token,
        uuid,
        name: flow.name,
        created_at,
    })
}
