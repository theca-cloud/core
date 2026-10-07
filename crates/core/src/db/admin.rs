//! The instance admin's queries: organizations, accounts, storages, identity
//! providers.

use sqlx::{PgConnection, PgPool};
use theca_common::api;
use uuid::Uuid;

/// Organizations whose name or owner's email contains `query` (all if none).
pub async fn organizations(
    pool: &PgPool,
    query: Option<&str>,
) -> sqlx::Result<Vec<api::AdminOrganization>> {
    sqlx::query_as!(
        api::AdminOrganization,
        r#"SELECT o.uuid AS id, o.name, a.email AS owner, o.personal_of IS NOT NULL AS "personal!",
                  (SELECT count(*) FROM "user" m WHERE m.organization = o.pk) AS "members!",
                  o.quota,
                  (SELECT coalesce(sum(c.stored_size), 0)::bigint FROM chunk c WHERE c.organization = o.pk) AS "used!",
                  o.suspended_at IS NOT NULL AS "suspended!",
                  s.uuid AS "default_storage?"
           FROM organization o
           JOIN "user" u ON u.pk = o.owner JOIN account a ON a.pk = u.account
           LEFT JOIN storage s ON s.pk = o.default_storage
           WHERE $1::text IS NULL OR o.name ILIKE '%' || $1 || '%' OR a.email ILIKE '%' || $1 || '%'
           ORDER BY lower(o.name) LIMIT 200"#,
        query
    )
    .fetch_all(pool)
    .await
}

pub async fn organization_pk(pool: &PgPool, uuid: Uuid) -> sqlx::Result<Option<i64>> {
    sqlx::query_scalar!("SELECT pk FROM organization WHERE uuid = $1", uuid)
        .fetch_optional(pool)
        .await
}

/// Applies the fields given. `false` if the default storage is unknown.
pub async fn update_organization(
    db: &mut PgConnection,
    pk: i64,
    update: &api::UpdateOrganization,
) -> sqlx::Result<bool> {
    if let Some(name) = &update.name {
        sqlx::query!("UPDATE organization SET name = $2 WHERE pk = $1", pk, name)
            .execute(&mut *db)
            .await?;
    }
    if let Some(quota) = update.quota {
        sqlx::query!(
            "UPDATE organization SET quota = $2 WHERE pk = $1",
            pk,
            quota
        )
        .execute(&mut *db)
        .await?;
    }
    if let Some(suspended) = update.suspended {
        sqlx::query!(
            "UPDATE organization SET suspended_at = CASE WHEN $2 THEN coalesce(suspended_at, now()) END WHERE pk = $1",
            pk,
            suspended
        )
        .execute(&mut *db)
        .await?;
    }
    if let Some(storage) = update.default_storage {
        let storage = match storage {
            Some(uuid) => match sqlx::query_scalar!("SELECT pk FROM storage WHERE uuid = $1", uuid)
                .fetch_optional(&mut *db)
                .await?
            {
                Some(pk) => Some(pk),
                None => return Ok(false),
            },
            None => None,
        };
        sqlx::query!(
            "UPDATE organization SET default_storage = $2 WHERE pk = $1",
            pk,
            storage
        )
        .execute(&mut *db)
        .await?;
    }
    Ok(true)
}

/// Accounts whose email contains `query` (all if none).
pub async fn accounts(pool: &PgPool, query: Option<&str>) -> sqlx::Result<Vec<api::AdminAccount>> {
    sqlx::query_as!(
        api::AdminAccount,
        r#"SELECT uuid AS id, email, verified, instance_admin, suspended_at IS NOT NULL AS "suspended!",
                  created_at
           FROM account WHERE $1::text IS NULL OR email ILIKE '%' || $1 || '%'
           ORDER BY lower(email) LIMIT 200"#,
        query
    )
    .fetch_all(pool)
    .await
}

/// Why an account update is refused.
pub enum AccountError {
    NotFound,
    /// It would leave no active instance admin.
    LastAdmin,
    Db(sqlx::Error),
}

impl From<sqlx::Error> for AccountError {
    fn from(e: sqlx::Error) -> Self {
        Self::Db(e)
    }
}

/// Applies the fields given, never leaving the instance without an active admin.
pub async fn update_account(
    pool: &PgPool,
    uuid: Uuid,
    update: &api::UpdateAccount,
) -> Result<(), AccountError> {
    let mut tx = pool.begin().await?;
    // Serializes admin changes: two can't each remove "the other" last admin.
    sqlx::query!("SELECT pg_advisory_xact_lock(hashtext('theca instance admins'))")
        .execute(&mut *tx)
        .await?;
    let found = sqlx::query_scalar!(
        "UPDATE account SET instance_admin = coalesce($2, instance_admin),
             suspended_at = CASE WHEN $3::boolean IS NULL THEN suspended_at
                                 WHEN $3 THEN coalesce(suspended_at, now()) END
         WHERE uuid = $1 RETURNING pk",
        uuid,
        update.instance_admin,
        update.suspended
    )
    .fetch_optional(&mut *tx)
    .await?;
    if found.is_none() {
        return Err(AccountError::NotFound);
    }
    let admins = sqlx::query_scalar!(
        r#"SELECT count(*) AS "count!" FROM account WHERE instance_admin AND suspended_at IS NULL"#
    )
    .fetch_one(&mut *tx)
    .await?;
    if admins == 0 {
        return Err(AccountError::LastAdmin);
    }
    tx.commit().await?;
    Ok(())
}

pub async fn storages(pool: &PgPool) -> sqlx::Result<Vec<api::StorageInfo>> {
    sqlx::query_as!(
        api::StorageInfo,
        r#"SELECT s.uuid AS id, s.name, s.implementation, s.is_default AS "default",
                  count(c.pk) AS "chunks!", coalesce(sum(c.stored_size), 0)::bigint AS "used!"
           FROM storage s LEFT JOIN chunk c ON c.storage = s.pk
           WHERE s.organization IS NULL
           GROUP BY s.pk ORDER BY lower(s.name)"#
    )
    .fetch_all(pool)
    .await
}

/// Adds a shared storage (already probed).
pub async fn add_storage(
    pool: &PgPool,
    name: &str,
    implementation: &str,
    details: &serde_json::Value,
) -> sqlx::Result<Uuid> {
    sqlx::query_scalar!(
        "INSERT INTO storage (name, implementation, details) VALUES ($1, $2, $3) RETURNING uuid",
        name,
        implementation,
        details
    )
    .fetch_one(pool)
    .await
}

/// Makes a shared storage the instance default. `false` if unknown.
pub async fn set_default_storage(pool: &PgPool, uuid: Uuid) -> sqlx::Result<bool> {
    let mut tx = pool.begin().await?;
    sqlx::query!("UPDATE storage SET is_default = false WHERE is_default")
        .execute(&mut *tx)
        .await?;
    let found = sqlx::query!(
        "UPDATE storage SET is_default = true WHERE uuid = $1 AND organization IS NULL",
        uuid
    )
    .execute(&mut *tx)
    .await?
    .rows_affected();
    if found == 0 {
        return Ok(false);
    }
    tx.commit().await?;
    Ok(true)
}

/// The providers of an organization, or the instance-wide ones (`None`).
pub async fn providers(
    pool: &PgPool,
    organization: Option<i64>,
) -> sqlx::Result<Vec<api::ProviderInfo>> {
    sqlx::query_as!(
        api::ProviderInfo,
        r#"SELECT p.uuid AS id, o.uuid AS "organization?", p.name, p.issuer, p.client_id,
                  p.client_secret_file, p.enabled
           FROM identity_provider p LEFT JOIN organization o ON o.pk = p.organization
           WHERE p.organization IS NOT DISTINCT FROM $1 ORDER BY lower(p.name)"#,
        organization
    )
    .fetch_all(pool)
    .await
}
