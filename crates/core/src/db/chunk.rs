use std::time::Duration;

use sqlx::PgPool;
use theca_common::api::{ChunkState, Declare, LookupChunk, LookupEntry};
use theca_common::compression::Compression;
use theca_common::hash::{ChunkHash, Sha256};
use uuid::Uuid;

use super::account::Credential;
use super::library::Access;

/// Looks up hashes in the library's dedup scope; creates pending chunks (with
/// their declared `plain_size`) for the misses; touches every chunk returned
/// (keepalive).
///
/// A hash matches a chunk of the library's organization, on the library's
/// storage, not tombstoned, and either:
/// - uploaded into this library (pending or announced, even if not committed yet), or
/// - ready and referenced by a version of a library in the dedup scope
///   (`dedup_scope`: this library, the libraries sharing a namespace with it,
///   and every library the credential can access in the organization, trash
///   included; computed now, so changes apply immediately).
///
/// Ready chunks win over pending ones.
pub async fn lookup(
    pool: &PgPool,
    access: &Access,
    chunks: &[LookupChunk],
) -> sqlx::Result<Vec<LookupEntry>> {
    let mut unique: Vec<&LookupChunk> = Vec::new();
    for c in chunks {
        if !unique.iter().any(|u| u.hash == c.hash) {
            unique.push(c);
        }
    }
    let hashes: Vec<Vec<u8>> = unique.iter().map(|c| c.hash.0.to_vec()).collect();

    let mut tx = pool.begin().await?;
    let found = sqlx::query!(
        r#"SELECT DISTINCT ON (c.hash) c.pk, c.uuid, c.hash, c.ready
           FROM chunk c
           WHERE c.organization = $2 AND c.storage = $3 AND c.hash = ANY($4)
             AND c.deleting_at IS NULL
             AND (c.library = $1 OR (c.ready AND EXISTS (
                     SELECT 1 FROM chunk_in_version civ
                     JOIN version v ON v.pk = civ.version
                     JOIN node n ON n.pk = v.file
                     WHERE civ.chunk = c.pk AND n.library IN (SELECT library FROM dedup_scope($1, $5)))))
           ORDER BY c.hash, c.ready DESC, c.pk"#,
        access.library,
        access.organization,
        access.storage,
        &hashes,
        access.token
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

    let missing: Vec<&&LookupChunk> = unique
        .iter()
        .filter(|c| !found.iter().any(|f| f.hash == c.hash.0))
        .collect();
    let missing_hashes: Vec<Vec<u8>> = missing.iter().map(|c| c.hash.0.to_vec()).collect();
    let missing_sizes: Vec<i64> = missing
        .iter()
        .map(|c| i64::try_from(c.plain_size).unwrap_or(i64::MAX))
        .collect();
    // plain_size over 8 MiB: refused by the chunk_plain_size CHECK.
    let created = sqlx::query!(
        "INSERT INTO chunk (organization, library, hash, storage, plain_size)
         SELECT $1, $2, h, $3, s FROM unnest($4::bytea[], $5::bigint[]) AS t (h, s)
         RETURNING uuid, hash",
        access.organization,
        access.library,
        access.storage,
        &missing_hashes,
        &missing_sizes
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

/// Why a declaration is refused.
pub enum DeclareError {
    /// Not pending, tombstoned, unknown, or not in a library the account can write.
    NotAllowed(Vec<Uuid>),
    /// Chunks of several organizations in one request (one file, one library).
    MixedOrganizations,
    /// Already declared with other values (size, compression or checksum).
    Mismatch(Vec<Uuid>),
    /// `stored_size` above `plain_size`, or an unknown compression.
    Invalid(Vec<Uuid>),
    /// The user already has too many uploads in progress (`max_pending_uploads`).
    TooManyUploads {
        in_progress: i64,
        requested: i64,
        max: i64,
    },
    /// The organization's quota would be exceeded; nothing was changed.
    QuotaExceeded {
        quota: i64,
        needed: i64,
    },
    Db(sqlx::Error),
}

impl From<sqlx::Error> for DeclareError {
    fn from(e: sqlx::Error) -> Self {
        Self::Db(e)
    }
}

/// A declared chunk, ready to get an upload URL: `(chunk, storage, sha256)`.
pub struct Declared {
    pub chunk: Uuid,
    pub storage: i64,
    pub sha256: Sha256,
}

fn compression_str(c: Compression) -> Option<&'static str> {
    match c {
        Compression::None => Some("none"),
        Compression::Lz4 => Some("lz4"),
        Compression::Unknown => None,
    }
}

/// Records what the client will upload (fixed at the first declaration; a
/// later one must repeat it exactly), checks the user's uploads in progress
/// and the organization's quota, and
/// records when the URLs about to be issued expire (before they're returned).
/// All or nothing.
pub async fn declare(
    pool: &PgPool,
    who: &Credential,
    declarations: &[Declare],
    url_lifetime: Duration,
    max_uploads: i64,
) -> Result<Vec<Declared>, DeclareError> {
    let mut tx = pool.begin().await?;
    let ids: Vec<Uuid> = declarations.iter().map(|d| d.chunk).collect();
    let rows = sqlx::query!(
        r#"SELECT c.pk, c.uuid, c.storage, c.organization, c.plain_size, c.stored_size, c.compression,
                  c.stored_sha256, a."user" AS "user!"
           FROM chunk c
           JOIN library_access($1) a ON a.library = c.library AND a.can_write
           WHERE c.uuid = ANY($2) AND c.uploaded_at IS NULL AND c.deleting_at IS NULL
           FOR UPDATE OF c"#,
        who.token,
        &ids
    )
    .fetch_all(&mut *tx)
    .await?;

    let refused: Vec<Uuid> = ids
        .iter()
        .filter(|id| !rows.iter().any(|r| r.uuid == **id))
        .copied()
        .collect();
    if !refused.is_empty() {
        return Err(DeclareError::NotAllowed(refused));
    }
    let Some(first) = rows.first() else {
        return Ok(Vec::new());
    };
    let (organization, user) = (first.organization, first.user);
    if rows.iter().any(|r| r.organization != organization) {
        return Err(DeclareError::MixedOrganizations);
    }
    // Serializes declarations within the organization, so two concurrent
    // uploads can't both fit under the quota.
    let quota = sqlx::query_scalar!(
        "SELECT quota FROM organization WHERE pk = $1 FOR NO KEY UPDATE",
        organization
    )
    .fetch_one(&mut *tx)
    .await?;

    let (mut invalid, mut mismatch) = (Vec::new(), Vec::new());
    for d in declarations {
        let row = rows.iter().find(|r| r.uuid == d.chunk).expect("checked");
        let (Some(compression), Ok(size)) =
            (compression_str(d.compression), i64::try_from(d.stored_size))
        else {
            invalid.push(d.chunk);
            continue;
        };
        if size > row.plain_size {
            invalid.push(d.chunk);
        } else if row.stored_size.is_some()
            && (row.stored_size != Some(size)
                || row.compression.as_deref() != Some(compression)
                || row.stored_sha256.as_deref() != Some(&d.stored_sha256.0[..]))
        {
            mismatch.push(d.chunk);
        }
    }
    if !invalid.is_empty() {
        return Err(DeclareError::Invalid(invalid));
    }
    if !mismatch.is_empty() {
        return Err(DeclareError::Mismatch(mismatch));
    }

    // Uploads in progress: the account's other chunks (in any organization)
    // with a valid URL, not uploaded yet. The chunks of this request are all
    // about to be in progress (renewing a URL doesn't take a second slot).
    let in_progress = sqlx::query_scalar!(
        r#"SELECT count(*) AS "count!" FROM chunk
           WHERE declared_by IN (SELECT pk FROM "user" WHERE account = $1)
             AND uploaded_at IS NULL AND deleting_at IS NULL
             AND url_expires_at > now() AND NOT (uuid = ANY($2))"#,
        who.account,
        &ids
    )
    .fetch_one(&mut *tx)
    .await?;
    let requested = ids.len() as i64;
    if in_progress + requested > max_uploads {
        return Err(DeclareError::TooManyUploads {
            in_progress,
            requested,
            max: max_uploads,
        });
    }

    for d in declarations {
        let lifetime = i64::try_from(url_lifetime.as_secs()).unwrap_or(i64::MAX);
        sqlx::query!(
            "UPDATE chunk SET stored_size = $2, compression = $3, stored_sha256 = $4, touched_at = now(),
                    declared_by = $6,
                    url_expires_at = GREATEST(url_expires_at, now() + make_interval(secs => $5 + 60))
             WHERE uuid = $1",
            d.chunk,
            i64::try_from(d.stored_size).expect("checked"),
            compression_str(d.compression).expect("checked"),
            &d.stored_sha256.0[..],
            lifetime as f64,
            user
        )
        .execute(&mut *tx)
        .await?;
    }

    if let Some(quota) = quota {
        let used = sqlx::query_scalar!(
            r#"SELECT COALESCE(sum(stored_size), 0)::bigint AS "used!" FROM chunk WHERE organization = $1"#,
            organization
        )
        .fetch_one(&mut *tx)
        .await?;
        if used > quota {
            return Err(DeclareError::QuotaExceeded {
                quota,
                needed: used,
            });
        }
    }
    tx.commit().await?;

    Ok(declarations
        .iter()
        .map(|d| Declared {
            chunk: d.chunk,
            storage: rows
                .iter()
                .find(|r| r.uuid == d.chunk)
                .expect("checked")
                .storage,
            sha256: d.stored_sha256,
        })
        .collect())
}

/// A pending, declared chunk the account may announce: `(chunk, storage, declared size)`.
pub async fn announceable(
    pool: &PgPool,
    who: &Credential,
    chunks: &[Uuid],
) -> sqlx::Result<Vec<(Uuid, i64, i64)>> {
    let rows = sqlx::query!(
        r#"SELECT c.uuid, c.storage, c.stored_size AS "stored_size!" FROM chunk c
           JOIN library_access($1) a ON a.library = c.library AND a.can_write
           WHERE c.uuid = ANY($2) AND c.uploaded_at IS NULL AND c.deleting_at IS NULL
             AND c.stored_size IS NOT NULL"#,
        who.token,
        chunks
    )
    .fetch_all(pool)
    .await?;
    Ok(rows
        .into_iter()
        .map(|r| (r.uuid, r.storage, r.stored_size))
        .collect())
}

/// Marks an uploaded chunk as ready (its object was checked).
pub async fn mark_uploaded(pool: &PgPool, chunk: Uuid) -> sqlx::Result<()> {
    sqlx::query!(
        "UPDATE chunk SET uploaded_at = now(), touched_at = now() WHERE uuid = $1 AND uploaded_at IS NULL",
        chunk
    )
    .execute(pool)
    .await?;
    Ok(())
}

/// What a server-signed upload URL may write: the storage and declared SHA-256
/// of a pending chunk. `None` once uploaded or tombstoned.
pub async fn pending_upload(pool: &PgPool, chunk: Uuid) -> sqlx::Result<Option<(i64, Vec<u8>)>> {
    let row = sqlx::query!(
        r#"SELECT storage, stored_sha256 AS "sha!" FROM chunk
           WHERE uuid = $1 AND uploaded_at IS NULL AND deleting_at IS NULL AND stored_sha256 IS NOT NULL"#,
        chunk
    )
    .fetch_optional(pool)
    .await?;
    Ok(row.map(|r| (r.storage, r.sha)))
}

/// The storage of a chunk a download URL points to (not tombstoned).
pub async fn download_storage(pool: &PgPool, chunk: Uuid) -> sqlx::Result<Option<i64>> {
    sqlx::query_scalar!(
        "SELECT storage FROM chunk WHERE uuid = $1 AND deleting_at IS NULL",
        chunk
    )
    .fetch_optional(pool)
    .await
}

/// Cancels the account's chunks that no version references: tombstones them (they
/// leave lookups and can't be committed). Their objects are deleted later by
/// the GC, once their upload URLs have expired. Returns the chunks tombstoned.
pub async fn cancel(pool: &PgPool, who: &Credential, chunks: &[Uuid]) -> sqlx::Result<Vec<Uuid>> {
    sqlx::query_scalar!(
        r#"UPDATE chunk c SET deleting_at = now() FROM library_access($1) a
           WHERE c.uuid = ANY($2) AND a.library = c.library AND a.can_write
             AND c.deleting_at IS NULL
             AND NOT EXISTS (SELECT 1 FROM chunk_in_version WHERE chunk = c.pk)
           RETURNING c.uuid"#,
        who.token,
        chunks
    )
    .fetch_all(pool)
    .await
}
