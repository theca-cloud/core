use sqlx::{PgPool, Postgres, Transaction};
use theca_common::api;
use uuid::Uuid;

use super::account::Credential;

// Access is decided by the SQL functions `library_access(token)` and
// `organization_access(token)`: the account's joined memberships, within the
// credential's scope. Never join memberships by hand here.

/// An account's access to a library, through its membership and `library_member`.
#[derive(Debug, Clone, Copy)]
pub struct Access {
    pub library: i64,
    pub organization: i64,
    pub storage: i64,
    /// The account's membership in the library's organization.
    pub user: i64,
    pub can_write: bool,
}

/// The account's access to a library, if it's a member.
pub async fn access(
    pool: &PgPool,
    who: &Credential,
    library: Uuid,
) -> sqlx::Result<Option<Access>> {
    let row = sqlx::query!(
        r#"SELECT l.pk, l.owner, l.storage, a."user" AS "user!", a.can_write AS "can_write!"
           FROM library l JOIN library_access($1) a ON a.library = l.pk
           WHERE l.uuid = $2"#,
        who.token,
        library
    )
    .fetch_optional(pool)
    .await?;
    Ok(row.map(|r| Access {
        library: r.pk,
        organization: r.owner,
        storage: r.storage,
        user: r.user,
        can_write: r.can_write,
    }))
}

/// A reachable node (neither it nor an ancestor in the trash) and the user's
/// access to its library.
#[derive(Debug, Clone, Copy)]
pub struct NodeAccess {
    pub access: Access,
    pub node: i64,
    /// `None`: the library's root.
    pub parent: Option<i64>,
    pub is_file: bool,
    pub current_version: Option<i64>,
}

pub async fn node_access(
    pool: &PgPool,
    who: &Credential,
    node: Uuid,
) -> sqlx::Result<Option<NodeAccess>> {
    let row = sqlx::query!(
        r#"SELECT n.pk, n.parent, n.is_file, n.current_version, l.pk AS library, l.owner, l.storage,
                  a."user" AS "user!", a.can_write AS "can_write!"
           FROM node n
           JOIN library l ON l.pk = n.library
           JOIN library_access($1) a ON a.library = l.pk
           WHERE n.uuid = $2 AND node_reachable(n.pk)"#,
        who.token,
        node
    )
    .fetch_optional(pool)
    .await?;
    Ok(row.map(|r| NodeAccess {
        access: Access {
            library: r.library,
            organization: r.owner,
            storage: r.storage,
            user: r.user,
            can_write: r.can_write,
        },
        node: r.pk,
        parent: r.parent,
        is_file: r.is_file,
        current_version: r.current_version,
    }))
}

/// Why a library can't be created (besides database refusals).
pub enum CreateError {
    /// Not a (joined) member of the organization, outside the credential's
    /// scope, or read-only.
    NotMember,
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
    who: &Credential,
    organization: Uuid,
    name: &str,
    namespaces: &[Uuid],
) -> Result<api::Library, CreateError> {
    let member = sqlx::query!(
        r#"SELECT a."user" AS "pk!", a.organization AS "organization!"
           FROM organization_access($1) a JOIN organization o ON o.pk = a.organization
           WHERE o.uuid = $2 AND a.library IS NULL AND a.can_write"#,
        who.token,
        organization
    )
    .fetch_optional(&mut **tx)
    .await?
    .ok_or(CreateError::NotMember)?;
    let (creator, organization_uuid, organization) = (member.pk, organization, member.organization);

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
        organization: organization_uuid,
        name: name.to_owned(),
        root,
    })
}

/// The libraries the account is a member of, in every organization it has
/// joined, within the credential's scope.
pub async fn list(pool: &PgPool, who: &Credential) -> sqlx::Result<Vec<api::Library>> {
    sqlx::query_as!(
        api::Library,
        r#"SELECT l.uuid AS id, o.uuid AS organization, l.name, n.uuid AS root
           FROM library_access($1) a
           JOIN library l ON l.pk = a.library
           JOIN organization o ON o.pk = l.owner
           JOIN node n ON n.library = l.pk AND n.parent IS NULL
           ORDER BY lower(o.name), lower(l.name)"#,
        who.token
    )
    .fetch_all(pool)
    .await
}

/// A folder's children, not in the trash.
pub async fn children(pool: &PgPool, folder: i64) -> sqlx::Result<Vec<api::Node>> {
    sqlx::query_as!(
        api::Node,
        r#"SELECT n.uuid AS id, n.name, n.is_file, v.created_at AS "modified?",
                  v.uuid AS "current_version?", v.size AS "size?"
           FROM node n LEFT JOIN version v ON v.pk = n.current_version
           WHERE n.parent = $1 AND n.deleted_at IS NULL ORDER BY n.is_file, lower(n.name)"#,
        folder
    )
    .fetch_all(pool)
    .await
}

/// A library as the API shows it.
pub async fn get(pool: &PgPool, library: i64) -> sqlx::Result<api::Library> {
    sqlx::query_as!(
        api::Library,
        r#"SELECT l.uuid AS id, o.uuid AS organization, l.name, n.uuid AS root
           FROM library l
           JOIN organization o ON o.pk = l.owner
           JOIN node n ON n.library = l.pk AND n.parent IS NULL
           WHERE l.pk = $1"#,
        library
    )
    .fetch_one(pool)
    .await
}

/// A library in the trash the account reaches (`library_access_any`).
pub async fn trashed_access(
    pool: &PgPool,
    who: &Credential,
    library: Uuid,
) -> sqlx::Result<Option<Access>> {
    let row = sqlx::query!(
        r#"SELECT l.pk, l.owner, l.storage, a."user" AS "user!", a.can_write AS "can_write!"
           FROM library l JOIN library_access_any($1) a ON a.library = l.pk
           WHERE l.uuid = $2 AND l.deleted_at IS NOT NULL"#,
        who.token,
        library
    )
    .fetch_optional(pool)
    .await?;
    Ok(row.map(|r| Access {
        library: r.pk,
        organization: r.owner,
        storage: r.storage,
        user: r.user,
        can_write: r.can_write,
    }))
}

pub async fn rename(pool: &PgPool, library: i64, name: &str) -> sqlx::Result<()> {
    sqlx::query!("UPDATE library SET name = $2 WHERE pk = $1", library, name)
        .execute(pool)
        .await?;
    Ok(())
}

/// Puts a library in its organization's trash: it and everything in it are
/// out of reach (`library_access`), its name free.
pub async fn trash(pool: &PgPool, library: i64) -> sqlx::Result<()> {
    sqlx::query!(
        "UPDATE library SET deleted_at = now() WHERE pk = $1",
        library
    )
    .execute(pool)
    .await?;
    Ok(())
}

/// Takes a library out of the trash, renamed ` (restored)` (then
/// ` (restored 2)`, ...) if its name was taken since.
pub async fn restore(tx: &mut Transaction<'_, Postgres>, library: i64) -> sqlx::Result<()> {
    let row = sqlx::query!("SELECT owner, name FROM library WHERE pk = $1", library)
        .fetch_one(&mut **tx)
        .await?;
    let taken: Vec<String> = sqlx::query_scalar!(
        r#"SELECT lower(name) AS "name!" FROM library WHERE owner = $1 AND deleted_at IS NULL"#,
        row.owner
    )
    .fetch_all(&mut **tx)
    .await?;
    let name = (0..)
        .map(|n| super::node::restored_name(&row.name, false, n))
        .find(|candidate| !taken.contains(&candidate.to_lowercase()))
        .expect("a free name");
    sqlx::query!(
        "UPDATE library SET deleted_at = NULL, name = $2 WHERE pk = $1",
        library,
        name
    )
    .execute(&mut **tx)
    .await?;
    Ok(())
}

/// Deletes a library for good: its nodes, versions and chunk references
/// (the chunks go with the GC), memberships, namespaces and scoped tokens.
pub async fn purge(tx: &mut Transaction<'_, Postgres>, library: i64) -> sqlx::Result<()> {
    let root = sqlx::query_scalar!(
        "SELECT pk FROM node WHERE library = $1 AND parent IS NULL",
        library
    )
    .fetch_one(&mut **tx)
    .await?;
    super::node::purge(tx, root).await?;
    // Members, namespaces and tokens go by cascade; pending chunks forget it.
    sqlx::query!("DELETE FROM library WHERE pk = $1", library)
        .execute(&mut **tx)
        .await?;
    Ok(())
}

/// An organization's trashed libraries the account reaches.
pub async fn trash_list(
    pool: &PgPool,
    who: &Credential,
    organization: Uuid,
) -> sqlx::Result<Vec<api::TrashedLibrary>> {
    sqlx::query_as!(
        api::TrashedLibrary,
        r#"SELECT l.uuid AS id, l.name, l.deleted_at AS "deleted_at!"
           FROM library_access_any($1) a
           JOIN library l ON l.pk = a.library
           JOIN organization o ON o.pk = l.owner
           WHERE o.uuid = $2 AND l.deleted_at IS NOT NULL
           ORDER BY l.deleted_at DESC"#,
        who.token,
        organization
    )
    .fetch_all(pool)
    .await
}
