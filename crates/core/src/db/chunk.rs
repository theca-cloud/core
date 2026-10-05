use sqlx::PgPool;
use theca_common::api::{ChunkState, LookupEntry};
use theca_common::compression::Compression;
use theca_common::hash::ChunkHash;
use uuid::Uuid;

use super::library::Access;

/// Looks up hashes in the library's dedup scope; creates pending chunks for the
/// misses; touches every chunk returned (keepalive).
///
/// A hash matches a chunk of the library's organization, on the library's
/// storage, not tombstoned, and either:
/// - uploaded into this library (pending or announced, even if not committed yet), or
/// - ready and referenced by a version of this library or of a library sharing
///   a namespace with it (computed now, so namespace changes apply immediately).
///
/// Ready chunks win over pending ones.
pub async fn lookup(
    pool: &PgPool,
    access: &Access,
    hashes: &[ChunkHash],
) -> sqlx::Result<Vec<LookupEntry>> {
    let mut unique: Vec<Vec<u8>> = hashes.iter().map(|h| h.0.to_vec()).collect();
    unique.sort();
    unique.dedup();

    let mut tx = pool.begin().await?;
    let found = sqlx::query!(
        r#"WITH scope AS (
               SELECT $1::bigint AS library
               UNION
               SELECT other.library FROM library_namespace mine
               JOIN library_namespace other ON other.namespace = mine.namespace
               WHERE mine.library = $1
           )
           SELECT DISTINCT ON (c.hash) c.pk, c.uuid, c.hash, c.ready
           FROM chunk c
           WHERE c.organization = $2 AND c.storage = $3 AND c.hash = ANY($4)
             AND c.deleting_at IS NULL
             AND (c.library = $1 OR (c.ready AND EXISTS (
                     SELECT 1 FROM chunk_in_version civ
                     JOIN version v ON v.pk = civ.version
                     JOIN node n ON n.pk = v.file
                     WHERE civ.chunk = c.pk AND n.library IN (SELECT library FROM scope))))
           ORDER BY c.hash, c.ready DESC, c.pk"#,
        access.library,
        access.organization,
        access.storage,
        &unique
    )
    .fetch_all(&mut *tx)
    .await?;

    let found_pks: Vec<i64> = found.iter().map(|f| f.pk).collect();
    sqlx::query!(
        "UPDATE chunk SET touched_at = now() WHERE pk = ANY($1)",
        &found_pks
    )
    .execute(&mut *tx)
    .await?;

    let missing: Vec<Vec<u8>> = unique
        .iter()
        .filter(|h| !found.iter().any(|f| &f.hash == *h))
        .cloned()
        .collect();
    let created = sqlx::query!(
        "INSERT INTO chunk (organization, library, hash, storage)
         SELECT $1, $2, h, $3 FROM unnest($4::bytea[]) AS h
         RETURNING uuid, hash",
        access.organization,
        access.library,
        access.storage,
        &missing
    )
    .fetch_all(&mut *tx)
    .await?;
    tx.commit().await?;

    let entry = |hash: &[u8], chunk: Uuid, ready: bool| LookupEntry {
        hash: ChunkHash(hash.try_into().expect("16-byte hash (CHECK)")),
        chunk,
        state: if ready {
            ChunkState::Ready
        } else {
            ChunkState::Upload
        },
    };
    Ok(found
        .iter()
        .map(|f| entry(&f.hash, f.uuid, f.ready))
        .chain(created.iter().map(|c| entry(&c.hash, c.uuid, false)))
        .collect())
}

/// Pending chunks the user may upload: uploaded into a library they can write.
/// Returns `(uuid, storage)` for those allowed.
pub async fn writable_pending(
    pool: &PgPool,
    user: i64,
    chunks: &[Uuid],
) -> sqlx::Result<Vec<(Uuid, i64)>> {
    let rows = sqlx::query!(
        r#"SELECT c.uuid, c.storage FROM chunk c
           JOIN library_member m ON m.library = c.library AND m."user" = $1 AND m.role = 'write'
           WHERE c.uuid = ANY($2) AND c.uploaded_at IS NULL AND c.deleting_at IS NULL"#,
        user,
        chunks
    )
    .fetch_all(pool)
    .await?;
    Ok(rows.into_iter().map(|r| (r.uuid, r.storage)).collect())
}

/// A storage row: `(implementation, details)`.
pub async fn storage(pool: &PgPool, storage: i64) -> sqlx::Result<(String, serde_json::Value)> {
    let row = sqlx::query!(
        "SELECT implementation, details FROM storage WHERE pk = $1",
        storage
    )
    .fetch_one(pool)
    .await?;
    Ok((row.implementation, row.details))
}

/// The storage of a chunk a presigned URL points to: pending for an upload, not
/// tombstoned for a download.
pub async fn blob_storage(pool: &PgPool, chunk: Uuid, upload: bool) -> sqlx::Result<Option<i64>> {
    sqlx::query_scalar!(
        "SELECT storage FROM chunk WHERE uuid = $1 AND deleting_at IS NULL
         AND ($2 = (uploaded_at IS NULL) OR NOT $2)",
        chunk,
        upload
    )
    .fetch_optional(pool)
    .await
}

/// Marks a pending chunk as uploaded, with the real size read from the backend.
pub async fn announce(
    pool: &PgPool,
    chunk: Uuid,
    stored_size: u64,
    plain_size: u64,
    compression: Compression,
) -> sqlx::Result<()> {
    let compression = match compression {
        Compression::None => "none",
        Compression::Lz4 => "lz4",
        Compression::Unknown => "unknown", // refused by the CHECK
    };
    sqlx::query!(
        "UPDATE chunk SET uploaded_at = now(), touched_at = now(),
                stored_size = $2, plain_size = $3, compression = $4
         WHERE uuid = $1 AND uploaded_at IS NULL",
        chunk,
        i64::try_from(stored_size).unwrap_or(i64::MAX),
        i64::try_from(plain_size).unwrap_or(i64::MAX),
        compression
    )
    .execute(pool)
    .await?;
    Ok(())
}

/// Deletes the user's chunks that no version references (rows first; the
/// caller then deletes the objects). Returns `(uuid, storage)` of those deleted.
pub async fn cancel(pool: &PgPool, user: i64, chunks: &[Uuid]) -> sqlx::Result<Vec<(Uuid, i64)>> {
    let rows = sqlx::query!(
        r#"DELETE FROM chunk c USING library_member m
           WHERE c.uuid = ANY($2) AND m.library = c.library AND m."user" = $1 AND m.role = 'write'
             AND NOT EXISTS (SELECT 1 FROM chunk_in_version WHERE chunk = c.pk)
           RETURNING c.uuid, c.storage"#,
        user,
        chunks
    )
    .fetch_all(pool)
    .await?;
    Ok(rows.into_iter().map(|r| (r.uuid, r.storage)).collect())
}
