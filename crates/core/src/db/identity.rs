//! Identity providers (OIDC) and the identities linked through them.

use sqlx::PgPool;
use theca_common::api;
use uuid::Uuid;

/// An enabled OIDC provider.
#[derive(Debug, Clone)]
pub struct Provider {
    pub pk: i64,
    pub uuid: Uuid,
    /// Its organization (sessions it opens are restricted to it), or `None`:
    /// instance-wide (full sessions).
    pub organization: Option<i64>,
    pub issuer: String,
    pub client_id: String,
    pub client_secret_file: String,
}

/// An enabled provider.
pub async fn provider(pool: &PgPool, uuid: Uuid) -> sqlx::Result<Option<Provider>> {
    sqlx::query_as!(
        Provider,
        "SELECT pk, uuid, organization, issuer, client_id, client_secret_file
         FROM identity_provider WHERE uuid = $1 AND enabled",
        uuid
    )
    .fetch_optional(pool)
    .await
}

/// The enabled instance-wide providers (the login page's buttons; an
/// organization's provider is reached through its own link).
pub async fn instance_providers(pool: &PgPool) -> sqlx::Result<Vec<api::IdentityProvider>> {
    sqlx::query_as!(
        api::IdentityProvider,
        "SELECT uuid AS id, name FROM identity_provider
         WHERE organization IS NULL AND enabled ORDER BY lower(name)"
    )
    .fetch_all(pool)
    .await
}

/// The account linked to `(provider, subject)`: `(pk, uuid)`.
pub async fn account_of(
    pool: &PgPool,
    provider: i64,
    subject: &str,
) -> sqlx::Result<Option<(i64, Uuid)>> {
    let row = sqlx::query!(
        "SELECT a.pk, a.uuid FROM identity i JOIN account a ON a.pk = i.account
         WHERE i.kind = 'oidc' AND i.provider = $1 AND i.details->>'subject' = $2",
        provider,
        subject
    )
    .fetch_optional(pool)
    .await?;
    Ok(row.map(|r| (r.pk, r.uuid)))
}

/// The account with this email (case-insensitive): `(pk, uuid)`.
pub async fn account_by_email(pool: &PgPool, email: &str) -> sqlx::Result<Option<(i64, Uuid)>> {
    let row = sqlx::query!(
        "SELECT pk, uuid FROM account WHERE lower(email) = lower($1)",
        email
    )
    .fetch_optional(pool)
    .await?;
    Ok(row.map(|r| (r.pk, r.uuid)))
}

/// Links `(provider, subject)` to an account. `false` if it's already linked
/// (to any account).
pub async fn link(pool: &PgPool, account: i64, provider: i64, subject: &str) -> sqlx::Result<bool> {
    let done = sqlx::query!(
        "INSERT INTO identity (account, kind, provider, details)
         VALUES ($1, 'oidc', $2, jsonb_build_object('subject', $3::text))
         ON CONFLICT (provider, (details->>'subject')) WHERE kind = 'oidc' DO NOTHING",
        account,
        provider,
        subject
    )
    .execute(pool)
    .await?;
    Ok(done.rows_affected() == 1)
}

/// The account's ways to log in (passwords and OIDC identities; TOTP is
/// managed on its own).
pub async fn identities(pool: &PgPool, account: i64) -> sqlx::Result<Vec<api::Identity>> {
    sqlx::query_as!(
        api::Identity,
        r#"SELECT i.uuid AS id, i.kind, p.uuid AS "provider?", p.name AS "provider_name?"
           FROM identity i LEFT JOIN identity_provider p ON p.pk = i.provider
           WHERE i.account = $1 AND i.kind IN ('password', 'oidc')
           ORDER BY i.kind DESC, lower(p.name)"#,
        account
    )
    .fetch_all(pool)
    .await
}

/// Why an identity can't be removed.
pub enum UnlinkError {
    NotFound,
    /// The account's last way to log in.
    Last,
    Db(sqlx::Error),
}

impl From<sqlx::Error> for UnlinkError {
    fn from(e: sqlx::Error) -> Self {
        Self::Db(e)
    }
}

/// Removes one of the account's ways to log in, never the last one.
pub async fn unlink(pool: &PgPool, account: i64, identity: Uuid) -> Result<(), UnlinkError> {
    let mut tx = pool.begin().await?;
    // Serializes the account's unlinks: two at once can't remove the last two.
    sqlx::query!("SELECT pk FROM account WHERE pk = $1 FOR UPDATE", account)
        .fetch_one(&mut *tx)
        .await?;
    let deleted = sqlx::query!(
        "DELETE FROM identity WHERE account = $1 AND uuid = $2 AND kind IN ('password', 'oidc')",
        account,
        identity
    )
    .execute(&mut *tx)
    .await?;
    if deleted.rows_affected() == 0 {
        return Err(UnlinkError::NotFound);
    }
    let left = sqlx::query_scalar!(
        r#"SELECT count(*) AS "count!" FROM identity WHERE account = $1 AND kind IN ('password', 'oidc')"#,
        account
    )
    .fetch_one(&mut *tx)
    .await?;
    if left == 0 {
        return Err(UnlinkError::Last);
    }
    tx.commit().await?;
    Ok(())
}

/// The account with this uuid: `(pk, uuid)`.
pub async fn account_by_uuid(pool: &PgPool, uuid: Uuid) -> sqlx::Result<Option<(i64, Uuid)>> {
    let row = sqlx::query!("SELECT pk, uuid FROM account WHERE uuid = $1", uuid)
        .fetch_optional(pool)
        .await?;
    Ok(row.map(|r| (r.pk, r.uuid)))
}
