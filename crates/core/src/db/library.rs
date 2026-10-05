use sqlx::{PgPool, Postgres, Transaction};
use theca_common::api;
use uuid::Uuid;

/// A user's access to a library, through `library_member`.
#[derive(Debug, Clone, Copy)]
pub struct Access {
    pub library: i64,
    pub organization: i64,
    pub storage: i64,
    pub can_write: bool,
}

/// The user's access to a library, if they're a member.
pub async fn access(pool: &PgPool, user: i64, library: Uuid) -> sqlx::Result<Option<Access>> {
    let row = sqlx::query!(
        r#"SELECT l.pk, l.owner, l.storage, m.role
           FROM library l JOIN library_member m ON m.library = l.pk AND m."user" = $1
           WHERE l.uuid = $2"#,
        user,
        library
    )
    .fetch_optional(pool)
    .await?;
    Ok(row.map(|r| Access {
        library: r.pk,
        organization: r.owner,
        storage: r.storage,
        can_write: r.role == "write",
    }))
}

/// A node (not in the trash) and the user's access to its library.
#[derive(Debug, Clone, Copy)]
pub struct NodeAccess {
    pub access: Access,
    pub node: i64,
    pub is_file: bool,
    pub current_version: Option<i64>,
}

pub async fn node_access(pool: &PgPool, user: i64, node: Uuid) -> sqlx::Result<Option<NodeAccess>> {
    let row = sqlx::query!(
        r#"SELECT n.pk, n.is_file, n.current_version, l.pk AS library, l.owner, l.storage, m.role
           FROM node n
           JOIN library l ON l.pk = n.library
           JOIN library_member m ON m.library = l.pk AND m."user" = $1
           WHERE n.uuid = $2 AND n.deleted_at IS NULL"#,
        user,
        node
    )
    .fetch_optional(pool)
    .await?;
    Ok(row.map(|r| NodeAccess {
        access: Access {
            library: r.library,
            organization: r.owner,
            storage: r.storage,
            can_write: r.role == "write",
        },
        node: r.pk,
        is_file: r.is_file,
        current_version: r.current_version,
    }))
}

/// Why a library can't be created (besides database refusals).
pub enum CreateError {
    NoStorage,
    UnknownNamespaces(Vec<Uuid>),
    Db(sqlx::Error),
}

impl From<sqlx::Error> for CreateError {
    fn from(e: sqlx::Error) -> Self {
        Self::Db(e)
    }
}

/// Creates a library with its root, the creator as `write` member, in the given
/// namespaces. Storage: the organization's default, else the instance default.
pub async fn create(
    tx: &mut Transaction<'_, Postgres>,
    organization: i64,
    creator: i64,
    name: &str,
    namespaces: &[Uuid],
) -> Result<api::Library, CreateError> {
    let storage = sqlx::query_scalar!(
        r#"SELECT COALESCE(
               (SELECT default_storage FROM organization WHERE pk = $1),
               (SELECT pk FROM storage WHERE is_default)
           ) AS "storage?""#,
        organization
    )
    .fetch_one(&mut **tx)
    .await?
    .ok_or(CreateError::NoStorage)?;

    let found = sqlx::query!(
        "SELECT pk, uuid FROM namespace WHERE organization = $1 AND uuid = ANY($2)",
        organization,
        namespaces
    )
    .fetch_all(&mut **tx)
    .await?;
    let unknown: Vec<Uuid> = namespaces
        .iter()
        .filter(|u| !found.iter().any(|f| f.uuid == **u))
        .copied()
        .collect();
    if !unknown.is_empty() {
        return Err(CreateError::UnknownNamespaces(unknown));
    }

    let library = sqlx::query!(
        "INSERT INTO library (name, owner, storage) VALUES ($1, $2, $3) RETURNING pk, uuid",
        name,
        organization,
        storage
    )
    .fetch_one(&mut **tx)
    .await?;
    let root = sqlx::query_scalar!(
        "INSERT INTO node (organization, library, parent, name, is_file)
         VALUES ($1, $2, NULL, '', false) RETURNING uuid",
        organization,
        library.pk
    )
    .fetch_one(&mut **tx)
    .await?;
    sqlx::query!(
        r#"INSERT INTO library_member (organization, library, "user", role) VALUES ($1, $2, $3, 'write')"#,
        organization,
        library.pk,
        creator
    )
    .execute(&mut **tx)
    .await?;
    let namespace_pks: Vec<i64> = found.iter().map(|f| f.pk).collect();
    sqlx::query!(
        "INSERT INTO library_namespace (organization, library, namespace)
         SELECT $1, $2, unnest($3::bigint[])",
        organization,
        library.pk,
        &namespace_pks
    )
    .execute(&mut **tx)
    .await?;

    Ok(api::Library {
        id: library.uuid,
        name: name.to_owned(),
        root,
    })
}

/// The libraries the user is a member of.
pub async fn list(pool: &PgPool, user: i64) -> sqlx::Result<Vec<api::Library>> {
    sqlx::query_as!(
        api::Library,
        r#"SELECT l.uuid AS id, l.name, n.uuid AS root
           FROM library l
           JOIN library_member m ON m.library = l.pk AND m."user" = $1
           JOIN node n ON n.library = l.pk AND n.parent IS NULL
           ORDER BY lower(l.name)"#,
        user
    )
    .fetch_all(pool)
    .await
}

/// A folder's children, not in the trash.
pub async fn children(pool: &PgPool, folder: i64) -> sqlx::Result<Vec<api::Node>> {
    sqlx::query_as!(
        api::Node,
        "SELECT uuid AS id, name, is_file FROM node
         WHERE parent = $1 AND deleted_at IS NULL ORDER BY is_file, lower(name)",
        folder
    )
    .fetch_all(pool)
    .await
}
