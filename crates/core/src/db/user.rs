use sqlx::PgPool;
use uuid::Uuid;

/// A user who has joined their organization: `(pk, organization)`.
pub async fn joined(pool: &PgPool, uuid: Uuid) -> sqlx::Result<Option<(i64, i64)>> {
    let row = sqlx::query!(
        r#"SELECT pk, organization FROM "user" WHERE uuid = $1 AND joined_at IS NOT NULL"#,
        uuid
    )
    .fetch_optional(pool)
    .await?;
    Ok(row.map(|r| (r.pk, r.organization)))
}
