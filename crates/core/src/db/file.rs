use sqlx::{PgPool, Postgres, Transaction};
use theca_common::compression::Compression;
use theca_common::hash::ChunkHash;
use uuid::Uuid;

use super::library::Access;

/// Where a commit goes.
pub enum Target<'a> {
    /// A new version of an existing file.
    File {
        file: Uuid,
        base_version: Option<Uuid>,
    },
    /// A new file.
    New { parent: Uuid, name: &'a str },
}

/// Why a commit is refused (besides database refusals, mapped in `api::error`).
pub enum CommitError {
    FileNotFound,
    ParentNotFound,
    BaseNotFound,
    /// Chunk IDs outside the library's dedup scope and not from a version of the
    /// same file (or unknown): they could be guessed from another library.
    ChunksNotAllowed(Vec<Uuid>),
    Db(sqlx::Error),
}

impl From<sqlx::Error> for CommitError {
    fn from(e: sqlx::Error) -> Self {
        Self::Db(e)
    }
}

/// The committed version: `(file, version, conflict)`.
pub struct Committed {
    pub file: Uuid,
    pub version: Uuid,
    pub conflict: bool,
}

/// Creates the file if needed and a new version with the given chunks (in
/// order), and makes it current. `conflict`: the base wasn't the current version.
pub async fn commit(
    tx: &mut Transaction<'_, Postgres>,
    access: &Access,
    target: Target<'_>,
    chunks: &[Uuid],
    message: Option<&str>,
) -> Result<Committed, CommitError> {
    let (file_pk, file_uuid, base, current) = match target {
        Target::File { file, base_version } => {
            let node = sqlx::query!(
                "SELECT pk, current_version FROM node
                 WHERE uuid = $1 AND library = $2 AND is_file AND deleted_at IS NULL",
                file,
                access.library
            )
            .fetch_optional(&mut **tx)
            .await?
            .ok_or(CommitError::FileNotFound)?;
            let base = match base_version {
                None => None,
                Some(b) => Some(
                    sqlx::query_scalar!(
                        "SELECT pk FROM version WHERE uuid = $1 AND file = $2",
                        b,
                        node.pk
                    )
                    .fetch_optional(&mut **tx)
                    .await?
                    .ok_or(CommitError::BaseNotFound)?,
                ),
            };
            (node.pk, file, base, node.current_version)
        }
        Target::New { parent, name } => {
            let parent = sqlx::query_scalar!(
                "SELECT pk FROM node WHERE uuid = $1 AND library = $2 AND NOT is_file AND deleted_at IS NULL",
                parent,
                access.library
            )
            .fetch_optional(&mut **tx)
            .await?
            .ok_or(CommitError::ParentNotFound)?;
            let node = sqlx::query!(
                "INSERT INTO node (organization, library, parent, name, is_file)
                 VALUES ($1, $2, $3, $4, true) RETURNING pk, uuid",
                access.organization,
                access.library,
                parent,
                name
            )
            .fetch_one(&mut **tx)
            .await?;
            (node.pk, node.uuid, None, None)
        }
    };

    // Chunk IDs must be in the library's dedup scope, or come from a version of
    // this file (unchanged chunks, restores). Code-checked: the database only
    // checks organization and readiness.
    let mut unique: Vec<Uuid> = chunks.to_vec();
    unique.sort();
    unique.dedup();
    let allowed = sqlx::query!(
        r#"WITH scope AS (
               SELECT $1::bigint AS library
               UNION
               SELECT other.library FROM library_namespace mine
               JOIN library_namespace other ON other.namespace = mine.namespace
               WHERE mine.library = $1
           )
           SELECT c.pk, c.uuid FROM chunk c
           WHERE c.uuid = ANY($2) AND c.organization = $3
             AND (c.library = $1 OR EXISTS (
                     SELECT 1 FROM chunk_in_version civ
                     JOIN version v ON v.pk = civ.version
                     JOIN node n ON n.pk = v.file
                     WHERE civ.chunk = c.pk
                       AND (n.library IN (SELECT library FROM scope) OR v.file = $4)))"#,
        access.library,
        &unique,
        access.organization,
        file_pk
    )
    .fetch_all(&mut **tx)
    .await?;
    let refused: Vec<Uuid> = unique
        .iter()
        .filter(|u| !allowed.iter().any(|a| a.uuid == **u))
        .copied()
        .collect();
    if !refused.is_empty() {
        return Err(CommitError::ChunksNotAllowed(refused));
    }
    let pks: Vec<i64> = chunks
        .iter()
        .map(|u| allowed.iter().find(|a| a.uuid == *u).expect("allowed").pk)
        .collect();

    let version = sqlx::query!(
        "INSERT INTO version (organization, file, parent_version, message)
         VALUES ($1, $2, $3, $4) RETURNING pk, uuid",
        access.organization,
        file_pk,
        base,
        message
    )
    .fetch_one(&mut **tx)
    .await?;
    sqlx::query!(
        "INSERT INTO chunk_in_version (organization, version, chunk, position)
         SELECT $1, $2, chunk, (position - 1)::int
         FROM unnest($3::bigint[]) WITH ORDINALITY AS t (chunk, position)",
        access.organization,
        version.pk,
        &pks
    )
    .execute(&mut **tx)
    .await?;
    sqlx::query!(
        "UPDATE node SET current_version = $1 WHERE pk = $2",
        version.pk,
        file_pk
    )
    .execute(&mut **tx)
    .await?;

    Ok(Committed {
        file: file_uuid,
        version: version.uuid,
        conflict: current != base,
    })
}

/// One chunk of a version, in order.
pub struct VersionChunk {
    pub uuid: Uuid,
    pub hash: ChunkHash,
    pub compression: Compression,
    pub plain_size: u64,
    pub storage: i64,
}

/// A version's uuid and ordered chunks.
pub async fn version_chunks(
    pool: &PgPool,
    version: i64,
) -> sqlx::Result<(Uuid, Vec<VersionChunk>)> {
    let uuid = sqlx::query_scalar!("SELECT uuid FROM version WHERE pk = $1", version)
        .fetch_one(pool)
        .await?;
    let rows = sqlx::query!(
        r#"SELECT c.uuid, c.hash, c.compression AS "compression!", c.plain_size, c.storage
           FROM chunk_in_version civ JOIN chunk c ON c.pk = civ.chunk
           WHERE civ.version = $1 ORDER BY civ.position"#,
        version
    )
    .fetch_all(pool)
    .await?;
    let chunks = rows
        .into_iter()
        .map(|r| VersionChunk {
            uuid: r.uuid,
            hash: ChunkHash(r.hash.as_slice().try_into().expect("16-byte hash (CHECK)")),
            compression: match r.compression.as_str() {
                "none" => Compression::None,
                "lz4" => Compression::Lz4,
                _ => Compression::Unknown,
            },
            plain_size: u64::try_from(r.plain_size).unwrap_or(0),
            storage: r.storage,
        })
        .collect();
    Ok((uuid, chunks))
}
