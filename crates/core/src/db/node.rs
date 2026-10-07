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
                  node_path(n.pk) AS "path!", v.uuid AS "current_version?", v.size AS "size?"
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
           RETURNING uuid AS id, name, is_file, NULL::timestamptz AS modified,
                     NULL::uuid AS current_version, NULL::bigint AS size"#,
        parent.access.organization,
        parent.access.library,
        parent.node,
        name
    )
    .fetch_one(pool)
    .await
}

/// A folder's subtree outside the trash, as it streams from the database:
/// paths below the folder, parents before their children (byte order puts a
/// path before its extensions).
pub fn tree(
    pool: &PgPool,
    folder: i64,
) -> impl futures::Stream<Item = sqlx::Result<api::TreeNode>> + '_ {
    sqlx::query_as!(
        api::TreeNode,
        r#"WITH RECURSIVE tree (pk, path) AS (
               SELECT pk, name FROM node WHERE parent = $1 AND deleted_at IS NULL
               UNION ALL
               SELECT n.pk, t.path || '/' || n.name
               FROM node n JOIN tree t ON n.parent = t.pk
               WHERE n.deleted_at IS NULL
           )
           SELECT n.uuid AS id, t.path AS "path!", n.is_file,
                  v.created_at AS "modified?", v.uuid AS "current_version?", v.size AS "size?"
           FROM tree t
           JOIN node n ON n.pk = t.pk
           LEFT JOIN version v ON v.pk = n.current_version
           ORDER BY t.path"#,
        folder
    )
    .fetch(pool)
}

/// The child of `parent` named `name` (ignoring case, outside the trash), or
/// a new folder of that name: `(pk, uuid, created)`. A file of that name:
/// `None`.
pub async fn folder_child(
    tx: &mut Transaction<'_, Postgres>,
    parent: &NodeAccess,
    under: i64,
    name: &str,
) -> sqlx::Result<Option<(i64, Uuid, bool)>> {
    let existing = sqlx::query!(
        "SELECT pk, uuid, is_file FROM node
         WHERE parent = $1 AND lower(name) = lower($2) AND deleted_at IS NULL",
        under,
        name
    )
    .fetch_optional(&mut **tx)
    .await?;
    if let Some(node) = existing {
        return Ok((!node.is_file).then_some((node.pk, node.uuid, false)));
    }
    let created = sqlx::query!(
        "INSERT INTO node (organization, library, parent, name, is_file)
         VALUES ($1, $2, $3, $4, false) RETURNING pk, uuid",
        parent.access.organization,
        parent.access.library,
        under,
        name
    )
    .fetch_one(&mut **tx)
    .await?;
    Ok(Some((created.pk, created.uuid, true)))
}

/// Renames and/or moves a node. To another library (same organization,
/// checked by the caller), its `library` changes and the database cascades it
/// to the subtree (`node_parent`), so the history follows.
pub async fn update(
    pool: &PgPool,
    node: &NodeAccess,
    name: Option<&str>,
    parent: Option<&NodeAccess>,
) -> sqlx::Result<()> {
    let library = parent.map_or(node.access.library, |p| p.access.library);
    let parent = parent.map_or(node.parent, |p| Some(p.node));
    sqlx::query!(
        "UPDATE node SET name = coalesce($2, name), parent = $3, library = $4 WHERE pk = $1",
        node.node,
        name,
        parent,
        library
    )
    .execute(pool)
    .await?;
    Ok(())
}

/// The reachable child of `folder` named `name` (ignoring case), if any:
/// `(pk, is_file, current_version)`.
pub async fn child_named(
    pool: &PgPool,
    folder: i64,
    name: &str,
) -> sqlx::Result<Option<(i64, bool, Option<i64>)>> {
    let row = sqlx::query!(
        "SELECT pk, is_file, current_version FROM node
         WHERE parent = $1 AND lower(name) = lower($2) AND deleted_at IS NULL",
        folder,
        name
    )
    .fetch_optional(pool)
    .await?;
    Ok(row.map(|r| (r.pk, r.is_file, r.current_version)))
}

/// Merges the file `source` into the file `dest` (a move onto it): the
/// source's versions join the destination's, its root(s) based on the
/// destination's current version; the source's current version becomes the
/// destination's; the source node goes.
pub async fn graft(tx: &mut Transaction<'_, Postgres>, source: i64, dest: i64) -> sqlx::Result<()> {
    let current = sqlx::query_scalar!("SELECT current_version FROM node WHERE pk = $1", source)
        .fetch_one(&mut **tx)
        .await?;
    sqlx::query!(
        "UPDATE node SET current_version = NULL WHERE pk = $1",
        source
    )
    .execute(&mut **tx)
    .await?;
    // One statement: `version_parent` (same file) holds at its end.
    sqlx::query!(
        "UPDATE version SET file = $2,
             parent_version = coalesce(parent_version, (SELECT current_version FROM node WHERE pk = $2))
         WHERE file = $1",
        source,
        dest
    )
    .execute(&mut **tx)
    .await?;
    sqlx::query!(
        "UPDATE node SET current_version = $2 WHERE pk = $1",
        dest,
        current
    )
    .execute(&mut **tx)
    .await?;
    sqlx::query!("DELETE FROM node WHERE pk = $1", source)
        .execute(&mut **tx)
        .await?;
    Ok(())
}

/// A new version of the file `dest`, based on its current one, with the
/// chunks of the version `content` (a copy onto an existing file).
pub async fn add_version(
    tx: &mut Transaction<'_, Postgres>,
    access: &Access,
    dest: i64,
    content: i64,
) -> sqlx::Result<()> {
    let new = sqlx::query_scalar!(
        "INSERT INTO version (organization, file, parent_version, author, size)
         SELECT $1, pk, current_version, $3, (SELECT size FROM version WHERE pk = $4)
         FROM node WHERE pk = $2 RETURNING pk",
        access.organization,
        dest,
        access.user,
        content
    )
    .fetch_one(&mut **tx)
    .await?;
    sqlx::query!(
        "INSERT INTO chunk_in_version (organization, version, chunk, position)
         SELECT organization, $2, chunk, position FROM chunk_in_version WHERE version = $1",
        content,
        new
    )
    .execute(&mut **tx)
    .await?;
    sqlx::query!(
        "UPDATE node SET current_version = $2 WHERE pk = $1",
        dest,
        new
    )
    .execute(&mut **tx)
    .await?;
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
) -> sqlx::Result<i64> {
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
        top.get_or_insert(copy.pk);
        if let Some(version) = n.current_version {
            let new = sqlx::query_scalar!(
                "INSERT INTO version (organization, file, author, size)
                 SELECT $1, $2, $3, size FROM version WHERE pk = $4 RETURNING pk",
                to.organization,
                copy.pk,
                to.user,
                version
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
        r#"SELECT n.uuid AS id, n.name, n.is_file, node_path(n.pk) AS "path!", v.size AS "size?",
                  n.deleted_at AS "deleted_at!"
           FROM node n LEFT JOIN version v ON v.pk = n.current_version
           WHERE n.library = $1 AND n.deleted_at IS NOT NULL ORDER BY n.deleted_at DESC"#,
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
pub(crate) fn restored_name(name: &str, is_file: bool, attempt: u32) -> String {
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

/// A file's versions, numbered RCS-style (`numbers`), in tree order.
pub async fn versions(pool: &PgPool, file: i64) -> sqlx::Result<Vec<api::Version>> {
    let rows = sqlx::query!(
        r#"SELECT v.pk, v.parent_version, v.uuid AS id, p.uuid AS "parent?", a.email AS "author?",
                  v.created_at, v.message, v.size,
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
    .await?;
    let tree: Vec<(i64, Option<i64>)> = rows.iter().map(|r| (r.pk, r.parent_version)).collect();
    Ok(numbers(&tree)
        .into_iter()
        .map(|(i, number)| {
            let r = &rows[i];
            api::Version {
                id: r.id,
                number,
                parent: r.parent,
                author: r.author.clone(),
                created_at: r.created_at,
                message: r.message.clone(),
                size: r.size,
                current: r.current,
            }
        })
        .collect())
}

/// RCS-style numbers for a version tree given as `(pk, parent)` rows in
/// creation order (pk order): `(row index, number)` in tree order (depth
/// first). Along a line numbers increment (1, 2, 3); a version's oldest child
/// continues its line, each later child starts a branch `<version>.<k>.1`.
/// Extra roots (none in practice) count as branches of a virtual `0`.
fn numbers(rows: &[(i64, Option<i64>)]) -> Vec<(usize, String)> {
    let children = |parent: Option<i64>| -> Vec<usize> {
        let mut found: Vec<usize> = (0..rows.len()).filter(|&i| rows[i].1 == parent).collect();
        found.sort_by_key(|&i| rows[i].0);
        found
    };
    let mut out = Vec::with_capacity(rows.len());
    // (row, its number), depth first, children in reverse so the oldest pops first.
    let mut stack: Vec<(usize, Vec<u64>)> = Vec::new();
    for (k, &root) in children(None).iter().enumerate().rev() {
        let number = if k == 0 {
            vec![1]
        } else {
            vec![0, k as u64, 1]
        };
        stack.push((root, number));
    }
    while let Some((i, number)) = stack.pop() {
        for (k, &child) in children(Some(rows[i].0)).iter().enumerate().rev() {
            let next = if k == 0 {
                let mut n = number.clone();
                *n.last_mut().expect("never empty") += 1;
                n
            } else {
                let mut n = number.clone();
                n.extend([k as u64, 1]);
                n
            };
            stack.push((child, next));
        }
        let text: Vec<String> = number.iter().map(u64::to_string).collect();
        out.push((i, text.join(".")));
    }
    out
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
        r#"SELECT n.uuid AS id, l.uuid AS library, n.name, n.is_file, node_path(n.pk) AS "path!",
                  v.size AS "size?"
           FROM node n
           JOIN library_access($1) a ON a.library = n.library
           JOIN library l ON l.pk = n.library
           LEFT JOIN version v ON v.pk = n.current_version
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

#[cfg(test)]
mod numbering {
    use super::numbers;

    /// Numbers by pk, for readable assertions.
    fn numbered(rows: &[(i64, Option<i64>)]) -> Vec<(i64, String)> {
        numbers(rows)
            .into_iter()
            .map(|(i, n)| (rows[i].0, n))
            .collect()
    }

    #[test]
    fn a_line() {
        let rows = [(1, None), (2, Some(1)), (3, Some(2))];
        assert_eq!(
            numbered(&rows),
            [(1, "1".into()), (2, "2".into()), (3, "3".into())]
        );
    }

    #[test]
    fn branches() {
        // 1 - 2 - 3, a conflict on 2 (5), a second branch from 2 (7) after a
        // restore, a branch from the branch (6 then 8 on 5).
        let rows = [
            (1, None),
            (2, Some(1)),
            (3, Some(2)),
            (5, Some(2)),
            (6, Some(5)),
            (7, Some(2)),
            (8, Some(5)),
            (9, Some(3)),
        ];
        assert_eq!(
            numbered(&rows),
            [
                (1, "1".into()),
                (2, "2".into()),
                (3, "3".into()),
                (9, "4".into()),
                (5, "2.1.1".into()),
                (6, "2.1.2".into()),
                (8, "2.1.1.1.1".into()),
                (7, "2.2.1".into()),
            ]
        );
    }

    #[test]
    fn a_graft_continues_the_line() {
        // Destination 1 - 2; the grafted file's versions 3 - 4 hang below 2.
        let rows = [(1, None), (2, Some(1)), (3, Some(2)), (4, Some(3))];
        let last = numbered(&rows).pop().unwrap();
        assert_eq!(last, (4, "4".into()));
    }
}
