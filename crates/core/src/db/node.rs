//! File operations on nodes: folders, renames, moves, copies, the trash,
//! versions and search. Callers check access first (`library::node_access`,
//! [`trashed_access`]); a node is reachable when neither it nor an ancestor is
//! trashed (`node_reachable` in SQL).

use std::collections::HashMap;

use sqlx::{PgPool, Postgres, Transaction};
use theca_common::api;
use uuid::Uuid;

use super::account::Credential;
use super::library::{Access, NodeAccess};

/// A node with its library and path.
pub async fn info(pool: &PgPool, node: i64) -> sqlx::Result<api::NodeInfo> {
    sqlx::query_as!(
        api::NodeInfo,
        r#"SELECT n.uuid AS id, l.uuid AS library, n.name, n.is_file, p.uuid AS "parent?",
                  node_path(n.pk) AS "path!", v.uuid AS "current_version?"
           FROM node n
           JOIN library l ON l.pk = n.library
           LEFT JOIN node p ON p.pk = n.parent
           LEFT JOIN version v ON v.pk = n.current_version
           WHERE n.pk = $1"#,
        node
    )
    .fetch_one(pool)
    .await
}

/// Creates a folder.
pub async fn create_folder(
    pool: &PgPool,
    parent: &NodeAccess,
    name: &str,
) -> sqlx::Result<api::Node> {
    sqlx::query_as!(
        api::Node,
        r#"INSERT INTO node (organization, library, parent, name, is_file)
           VALUES ($1, $2, $3, $4, false)
           RETURNING uuid AS id, name, is_file, NULL::text AS modified"#,
        parent.access.organization,
        parent.access.library,
        parent.node,
        name
    )
    .fetch_one(pool)
    .await
}

/// Renames and/or moves a node. Within its library, a move only changes
/// `parent`; to another library (same organization, checked by the caller), it
/// also rewrites `library` on the whole subtree, in the same statement (the
/// composite FKs are checked at its end), so the history follows.
// ponytail: one UPDATE of the subtree; run huge cross-library moves as a
// background job (step 09) if they become a problem.
pub async fn update(
    pool: &PgPool,
    node: &NodeAccess,
    name: Option<&str>,
    parent: Option<&NodeAccess>,
) -> sqlx::Result<()> {
    let library = parent.map_or(node.access.library, |p| p.access.library);
    let parent = parent.map_or(node.parent, |p| Some(p.node));
    if library == node.access.library {
        sqlx::query!(
            "UPDATE node SET name = coalesce($2, name), parent = $3 WHERE pk = $1",
            node.node,
            name,
            parent
        )
        .execute(pool)
        .await?;
    } else {
        sqlx::query!(
            "WITH RECURSIVE subtree (pk) AS (
                 SELECT $1::bigint
                 UNION ALL
                 SELECT n.pk FROM node n JOIN subtree s ON n.parent = s.pk
             )
             UPDATE node SET library = $4,
                 name = CASE WHEN pk = $1 THEN coalesce($2, name) ELSE name END,
                 parent = CASE WHEN pk = $1 THEN $3 ELSE parent END
             WHERE pk IN (SELECT pk FROM subtree)",
            node.node,
            name,
            parent,
            library
        )
        .execute(pool)
        .await?;
    }
    Ok(())
}

/// Copies a node into a folder (same organization, checked by the caller):
/// new nodes, and for each file one version with the same chunks in the same
/// order (reuse by ID; no byte duplicated). Trashed descendants are skipped.
/// The subtree is read first, so copying a folder into itself terminates.
// ponytail: one insert per node; set-based SQL if huge copies matter.
pub async fn copy(
    tx: &mut Transaction<'_, Postgres>,
    source: &NodeAccess,
    dest: &NodeAccess,
    name: Option<&str>,
) -> sqlx::Result<Uuid> {
    let nodes = sqlx::query!(
        r#"WITH RECURSIVE subtree (pk, parent, name, is_file, current_version, depth) AS (
               SELECT pk, parent, name, is_file, current_version, 0 FROM node WHERE pk = $1
               UNION ALL
               SELECT n.pk, n.parent, n.name, n.is_file, n.current_version, s.depth + 1
               FROM node n JOIN subtree s ON n.parent = s.pk
               WHERE n.deleted_at IS NULL
           )
           SELECT pk AS "pk!", parent, name AS "name!", is_file AS "is_file!", current_version
           FROM subtree ORDER BY depth"#,
        source.node
    )
    .fetch_all(&mut **tx)
    .await?;
    let to = dest.access;
    let mut copies: HashMap<i64, i64> = HashMap::new();
    let mut top = None;
    for (i, n) in nodes.iter().enumerate() {
        let (parent, name) = if i == 0 {
            (dest.node, name.unwrap_or(&n.name))
        } else {
            (
                copies[&n.parent.expect("below the copied node")],
                n.name.as_str(),
            )
        };
        let copy = sqlx::query!(
            "INSERT INTO node (organization, library, parent, name, is_file)
             VALUES ($1, $2, $3, $4, $5) RETURNING pk, uuid",
            to.organization,
            to.library,
            parent,
            name,
            n.is_file
        )
        .fetch_one(&mut **tx)
        .await?;
        copies.insert(n.pk, copy.pk);
        top.get_or_insert(copy.uuid);
        if let Some(version) = n.current_version {
            let new = sqlx::query_scalar!(
                "INSERT INTO version (organization, file, author) VALUES ($1, $2, $3) RETURNING pk",
                to.organization,
                copy.pk,
                to.user
            )
            .fetch_one(&mut **tx)
            .await?;
            sqlx::query!(
                "INSERT INTO chunk_in_version (organization, version, chunk, position)
                 SELECT organization, $2, chunk, position FROM chunk_in_version WHERE version = $1",
                version,
                new
            )
            .execute(&mut **tx)
            .await?;
            sqlx::query!(
                "UPDATE node SET current_version = $2 WHERE pk = $1",
                copy.pk,
                new
            )
            .execute(&mut **tx)
            .await?;
        }
    }
    Ok(top.expect("the copied node itself"))
}

/// Puts a node in the trash: only that node is flagged.
pub async fn trash(pool: &PgPool, node: i64) -> sqlx::Result<()> {
    sqlx::query!("UPDATE node SET deleted_at = now() WHERE pk = $1", node)
        .execute(pool)
        .await?;
    Ok(())
}

/// A trashed node, if the user reaches its library.
pub async fn trashed_access(
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
           WHERE n.uuid = $2 AND n.deleted_at IS NOT NULL"#,
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

/// A library's trash.
pub async fn trash_list(pool: &PgPool, library: i64) -> sqlx::Result<Vec<api::TrashEntry>> {
    sqlx::query_as!(
        api::TrashEntry,
        r#"SELECT uuid AS id, name, is_file, node_path(pk) AS "path!", deleted_at::text AS "deleted_at!"
           FROM node WHERE library = $1 AND deleted_at IS NOT NULL ORDER BY deleted_at DESC"#,
        library
    )
    .fetch_all(pool)
    .await
}

/// Takes a node out of the trash: back in its folder if that one is
/// reachable, else into the library's root; renamed ` (restored)` (then
/// ` (restored 2)`, ...) if its name is taken there.
pub async fn restore(
    tx: &mut Transaction<'_, Postgres>,
    node: &NodeAccess,
) -> sqlx::Result<api::Restored> {
    let row = sqlx::query!(
        r#"SELECT n.name,
                  CASE WHEN node_reachable(n.parent) THEN n.parent
                       ELSE (SELECT r.pk FROM node r WHERE r.library = n.library AND r.parent IS NULL)
                  END AS "parent!"
           FROM node n WHERE n.pk = $1"#,
        node.node
    )
    .fetch_one(&mut **tx)
    .await?;
    let taken: Vec<String> = sqlx::query_scalar!(
        r#"SELECT lower(name) AS "name!" FROM node WHERE parent = $1 AND deleted_at IS NULL"#,
        row.parent
    )
    .fetch_all(&mut **tx)
    .await?;
    let name = (0..)
        .map(|n| restored_name(&row.name, node.is_file, n))
        .find(|candidate| !taken.contains(&candidate.to_lowercase()))
        .expect("a free name");
    // An overlong name ("... (restored)" past 255 bytes) is refused by the
    // database (`node_name_valid`), and the restore with it.
    sqlx::query!(
        "UPDATE node SET deleted_at = NULL, parent = $2, name = $3 WHERE pk = $1",
        node.node,
        row.parent,
        name
    )
    .execute(&mut **tx)
    .await?;
    let parent = sqlx::query_scalar!("SELECT uuid FROM node WHERE pk = $1", row.parent)
        .fetch_one(&mut **tx)
        .await?;
    let path = sqlx::query_scalar!(r#"SELECT node_path($1) AS "path!""#, node.node)
        .fetch_one(&mut **tx)
        .await?;
    Ok(api::Restored { parent, name, path })
}

/// `a.txt`, then `a (restored).txt`, `a (restored 2).txt`, ... (folders and
/// dotfiles: the suffix goes at the end).
fn restored_name(name: &str, is_file: bool, attempt: u32) -> String {
    let suffix = match attempt {
        0 => return name.to_owned(),
        1 => " (restored)".to_owned(),
        n => format!(" (restored {n})"),
    };
    match name.rfind('.').filter(|&i| is_file && i > 0) {
        Some(i) => format!("{}{suffix}{}", &name[..i], &name[i..]),
        None => format!("{name}{suffix}"),
    }
}

/// Deletes a trashed node for good: its subtree, their versions and chunk
/// references. The chunks themselves are left to the GC.
pub async fn purge(tx: &mut Transaction<'_, Postgres>, node: i64) -> sqlx::Result<()> {
    let subtree: Vec<i64> = sqlx::query_scalar!(
        r#"WITH RECURSIVE subtree (pk) AS (
               SELECT $1::bigint
               UNION ALL
               SELECT n.pk FROM node n JOIN subtree s ON n.parent = s.pk
           )
           SELECT pk AS "pk!" FROM subtree"#,
        node
    )
    .fetch_all(&mut **tx)
    .await?;
    sqlx::query!(
        "UPDATE node SET current_version = NULL WHERE pk = ANY($1)",
        &subtree
    )
    .execute(&mut **tx)
    .await?;
    sqlx::query!(
        "DELETE FROM chunk_in_version WHERE version IN (SELECT pk FROM version WHERE file = ANY($1))",
        &subtree
    )
    .execute(&mut **tx)
    .await?;
    sqlx::query!("DELETE FROM version WHERE file = ANY($1)", &subtree)
        .execute(&mut **tx)
        .await?;
    sqlx::query!("DELETE FROM node WHERE pk = ANY($1)", &subtree)
        .execute(&mut **tx)
        .await?;
    Ok(())
}

/// A file's versions (the tree, oldest first).
pub async fn versions(pool: &PgPool, file: i64) -> sqlx::Result<Vec<api::Version>> {
    sqlx::query_as!(
        api::Version,
        r#"SELECT v.uuid AS id, p.uuid AS "parent?", a.email AS "author?",
                  v.created_at::text AS "created_at!", v.message,
                  v.pk = n.current_version AS "current!"
           FROM version v
           JOIN node n ON n.pk = v.file
           LEFT JOIN version p ON p.pk = v.parent_version
           LEFT JOIN "user" u ON u.pk = v.author
           LEFT JOIN account a ON a.pk = u.account
           WHERE v.file = $1 ORDER BY v.pk"#,
        file
    )
    .fetch_all(pool)
    .await
}

/// A version of a file, by its uuid.
pub async fn version_of(pool: &PgPool, file: i64, version: Uuid) -> sqlx::Result<Option<i64>> {
    sqlx::query_scalar!(
        "SELECT pk FROM version WHERE file = $1 AND uuid = $2",
        file,
        version
    )
    .fetch_optional(pool)
    .await
}

/// Makes a version current: the next commit based on it branches the tree.
pub async fn set_current(pool: &PgPool, file: i64, version: i64) -> sqlx::Result<()> {
    sqlx::query!(
        "UPDATE node SET current_version = $2 WHERE pk = $1",
        file,
        version
    )
    .execute(pool)
    .await?;
    Ok(())
}

/// Files and folders whose name contains `query` (case-insensitive), reachable,
/// in the libraries the credential reaches (or one of them). At most 50.
pub async fn search(
    pool: &PgPool,
    who: &Credential,
    query: &str,
    library: Option<Uuid>,
) -> sqlx::Result<Vec<api::SearchResult>> {
    let escaped = query
        .replace('\\', "\\\\")
        .replace('%', "\\%")
        .replace('_', "\\_");
    sqlx::query_as!(
        api::SearchResult,
        r#"SELECT n.uuid AS id, l.uuid AS library, n.name, n.is_file, node_path(n.pk) AS "path!"
           FROM node n
           JOIN library_access($1) a ON a.library = n.library
           JOIN library l ON l.pk = n.library
           WHERE n.name ILIKE '%' || $2 || '%' AND n.parent IS NOT NULL
             AND ($3::uuid IS NULL OR l.uuid = $3) AND node_reachable(n.pk)
           ORDER BY lower(n.name), n.pk
           LIMIT 50"#,
        who.token,
        escaped,
        library
    )
    .fetch_all(pool)
    .await
}

#[cfg(test)]
mod tests {
    use super::restored_name;

    #[test]
    fn restored_names() {
        assert_eq!(restored_name("a.txt", true, 0), "a.txt");
        assert_eq!(restored_name("a.txt", true, 1), "a (restored).txt");
        assert_eq!(restored_name("a.tar.gz", true, 2), "a.tar (restored 2).gz");
        assert_eq!(restored_name(".bashrc", true, 1), ".bashrc (restored)");
        assert_eq!(restored_name("v1.0", false, 1), "v1.0 (restored)");
    }
}
