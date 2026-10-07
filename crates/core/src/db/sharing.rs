//! What an organization's owner manages: its libraries' members and its dedup
//! namespaces (decisions.md, Authorization and Trust boundary).

use sqlx::PgPool;
use theca_common::api;
use uuid::Uuid;

use super::account::Credential;

/// The organization of a library or namespace whose organization the
/// credential's account owns (and reaches).
#[derive(Debug, Clone, Copy)]
pub struct Managed {
    /// The library or namespace.
    pub pk: i64,
    pub organization: i64,
}

/// A library (not in the trash) of an organization the account owns.
pub async fn library(
    pool: &PgPool,
    who: &Credential,
    library: Uuid,
) -> sqlx::Result<Option<Managed>> {
    sqlx::query_as!(
        Managed,
        r#"SELECT l.pk, l.owner AS organization
           FROM library l JOIN organization o ON o.pk = l.owner JOIN "user" u ON u.pk = o.owner
           WHERE l.uuid = $1 AND l.deleted_at IS NULL AND u.account = $2
             AND ($3::bigint IS NULL OR o.pk = $3)"#,
        library,
        who.account,
        who.organization
    )
    .fetch_optional(pool)
    .await
}

/// A namespace of an organization the account owns.
pub async fn namespace(
    pool: &PgPool,
    who: &Credential,
    namespace: Uuid,
) -> sqlx::Result<Option<Managed>> {
    sqlx::query_as!(
        Managed,
        r#"SELECT n.pk, n.organization
           FROM namespace n JOIN organization o ON o.pk = n.organization JOIN "user" u ON u.pk = o.owner
           WHERE n.uuid = $1 AND u.account = $2 AND ($3::bigint IS NULL OR o.pk = $3)"#,
        namespace,
        who.account,
        who.organization
    )
    .fetch_optional(pool)
    .await
}

/// Every library of the organization (not in the trash), members or not.
pub async fn libraries(
    pool: &PgPool,
    organization: i64,
) -> sqlx::Result<Vec<api::OrganizationLibrary>> {
    sqlx::query_as!(
        api::OrganizationLibrary,
        r#"SELECT l.uuid AS id, l.name,
                  (SELECT count(*) FROM library_member m WHERE m.library = l.pk) AS "members!"
           FROM library l WHERE l.owner = $1 AND l.deleted_at IS NULL ORDER BY lower(l.name)"#,
        organization
    )
    .fetch_all(pool)
    .await
}

pub async fn members(pool: &PgPool, library: i64) -> sqlx::Result<Vec<api::LibraryMember>> {
    let rows = sqlx::query!(
        r#"SELECT u.uuid, a.email, u.joined_at IS NOT NULL AS "joined!", m.role
           FROM library_member m JOIN "user" u ON u.pk = m."user" JOIN account a ON a.pk = u.account
           WHERE m.library = $1 ORDER BY lower(a.email)"#,
        library
    )
    .fetch_all(pool)
    .await?;
    Ok(rows
        .into_iter()
        .map(|r| api::LibraryMember {
            user: r.uuid,
            email: r.email,
            joined: r.joined,
            role: if r.role == "write" {
                api::Role::Write
            } else {
                api::Role::Read
            },
        })
        .collect())
}

/// Adds a member of the organization to the library, or changes its role.
/// `false`: no such member in the organization.
pub async fn set_member(
    pool: &PgPool,
    library: Managed,
    user: Uuid,
    write: bool,
) -> sqlx::Result<bool> {
    let role = if write { "write" } else { "read" };
    let done = sqlx::query!(
        r#"INSERT INTO library_member (organization, library, "user", role)
           SELECT $1, $2, u.pk, $4 FROM "user" u WHERE u.uuid = $3 AND u.organization = $1
           ON CONFLICT (library, "user") DO UPDATE SET role = EXCLUDED.role"#,
        library.organization,
        library.pk,
        user,
        role
    )
    .execute(pool)
    .await?;
    Ok(done.rows_affected() == 1)
}

/// `false`: not a member of the library.
pub async fn remove_member(pool: &PgPool, library: i64, user: Uuid) -> sqlx::Result<bool> {
    let done = sqlx::query!(
        r#"DELETE FROM library_member m USING "user" u
           WHERE m.library = $1 AND m."user" = u.pk AND u.uuid = $2"#,
        library,
        user
    )
    .execute(pool)
    .await?;
    Ok(done.rows_affected() == 1)
}

pub async fn namespaces(pool: &PgPool, organization: i64) -> sqlx::Result<Vec<api::Namespace>> {
    sqlx::query_as!(
        api::Namespace,
        r#"SELECT n.uuid AS id, n.name,
                  ARRAY(SELECT l.uuid FROM library_namespace ln JOIN library l ON l.pk = ln.library
                        WHERE ln.namespace = n.pk ORDER BY lower(l.name)) AS "libraries!"
           FROM namespace n WHERE n.organization = $1 ORDER BY lower(n.name)"#,
        organization
    )
    .fetch_all(pool)
    .await
}

pub async fn get_namespace(pool: &PgPool, namespace: i64) -> sqlx::Result<api::Namespace> {
    sqlx::query_as!(
        api::Namespace,
        r#"SELECT n.uuid AS id, n.name,
                  ARRAY(SELECT l.uuid FROM library_namespace ln JOIN library l ON l.pk = ln.library
                        WHERE ln.namespace = n.pk ORDER BY lower(l.name)) AS "libraries!"
           FROM namespace n WHERE n.pk = $1"#,
        namespace
    )
    .fetch_one(pool)
    .await
}

pub async fn create_namespace(pool: &PgPool, organization: i64, name: &str) -> sqlx::Result<i64> {
    sqlx::query_scalar!(
        "INSERT INTO namespace (organization, name) VALUES ($1, $2) RETURNING pk",
        organization,
        name
    )
    .fetch_one(pool)
    .await
}

pub async fn rename_namespace(pool: &PgPool, namespace: i64, name: &str) -> sqlx::Result<()> {
    sqlx::query!(
        "UPDATE namespace SET name = $2 WHERE pk = $1",
        namespace,
        name
    )
    .execute(pool)
    .await?;
    Ok(())
}

/// The libraries leave it (trust revoked at once); their chunks stay theirs.
pub async fn delete_namespace(pool: &PgPool, namespace: i64) -> sqlx::Result<()> {
    sqlx::query!("DELETE FROM namespace WHERE pk = $1", namespace)
        .execute(pool)
        .await?;
    Ok(())
}

/// Puts a library of the namespace's organization in it (idempotent).
/// `false`: no such library there.
pub async fn add_library(pool: &PgPool, namespace: Managed, library: Uuid) -> sqlx::Result<bool> {
    let found = sqlx::query_scalar!(
        "SELECT pk FROM library WHERE uuid = $1 AND owner = $2 AND deleted_at IS NULL",
        library,
        namespace.organization
    )
    .fetch_optional(pool)
    .await?;
    let Some(library) = found else {
        return Ok(false);
    };
    sqlx::query!(
        "INSERT INTO library_namespace (organization, library, namespace) VALUES ($1, $2, $3)
         ON CONFLICT (library, namespace) DO NOTHING",
        namespace.organization,
        library,
        namespace.pk
    )
    .execute(pool)
    .await?;
    Ok(true)
}

/// `false`: the library wasn't in it.
pub async fn remove_library(pool: &PgPool, namespace: i64, library: Uuid) -> sqlx::Result<bool> {
    let done = sqlx::query!(
        "DELETE FROM library_namespace ln USING library l
         WHERE ln.namespace = $1 AND ln.library = l.pk AND l.uuid = $2",
        namespace,
        library
    )
    .execute(pool)
    .await?;
    Ok(done.rows_affected() == 1)
}
