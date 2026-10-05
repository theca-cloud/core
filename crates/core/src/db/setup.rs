//! The first-run setup: an instance without an admin gets one, with its first
//! organization and storages, in one transaction.

use sqlx::PgPool;
use uuid::Uuid;

/// Fresh: no instance admin yet.
pub async fn is_fresh(pool: &PgPool) -> sqlx::Result<bool> {
    sqlx::query_scalar!(
        r#"SELECT NOT EXISTS (SELECT 1 FROM account WHERE instance_admin) AS "fresh!""#
    )
    .fetch_one(pool)
    .await
}

pub enum SetupError {
    AlreadySetUp,
    Db(sqlx::Error),
}

impl From<sqlx::Error> for SetupError {
    fn from(e: sqlx::Error) -> Self {
        Self::Db(e)
    }
}

/// Creates the admin (password identity), the organization they own, and the
/// storages (`(name, implementation, details)`, the first as the instance
/// default). Returns the admin's account `(pk, uuid)`.
pub async fn run(
    pool: &PgPool,
    email: &str,
    password_hash: &str,
    organization: &str,
    storages: &[(String, &'static str, serde_json::Value)],
) -> Result<(i64, Uuid), SetupError> {
    let mut tx = pool.begin().await?;
    // Concurrent setups queue here, and only the first finds the instance fresh.
    sqlx::query!("SELECT pg_advisory_xact_lock(hashtext('theca setup'))")
        .execute(&mut *tx)
        .await?;
    let fresh = sqlx::query_scalar!(
        r#"SELECT NOT EXISTS (SELECT 1 FROM account WHERE instance_admin) AS "fresh!""#
    )
    .fetch_one(&mut *tx)
    .await?;
    if !fresh {
        return Err(SetupError::AlreadySetUp);
    }

    let account = sqlx::query!(
        "INSERT INTO account (email, instance_admin, verified) VALUES ($1, true, true) RETURNING pk, uuid",
        email
    )
    .fetch_one(&mut *tx)
    .await?;
    sqlx::query!(
        "INSERT INTO identity (account, kind, details) VALUES ($1, 'password', jsonb_build_object('hash', $2::text))",
        account.pk,
        password_hash
    )
    .execute(&mut *tx)
    .await?;
    super::organization::create(&mut tx, account.pk, organization, false).await?;
    for (i, (name, implementation, details)) in storages.iter().enumerate() {
        sqlx::query!(
            "INSERT INTO storage (name, implementation, details, is_default) VALUES ($1, $2, $3, $4)",
            name,
            implementation,
            details,
            i == 0
        )
        .execute(&mut *tx)
        .await?;
    }
    tx.commit().await?;
    Ok((account.pk, account.uuid))
}
