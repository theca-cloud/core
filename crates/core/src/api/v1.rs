//! `/api/v1`: libraries, folders, and the upload/download protocol
//! (see docs/decisions.md, Upload flow).

use axum::body::Bytes;
use axum::extract::{DefaultBodyLimit, Path, Query, State};
use axum::http::StatusCode;
use axum::routing::{get, post};
use axum::{Json, Router};
use serde::Deserialize;
use theca_common::api;
use theca_common::chunking::MAX_SIZE;
use uuid::Uuid;

use super::AppState;
use super::auth::{ClientIp, CurrentUser};
use super::error::{ApiJson, HttpError};
use crate::db;
use crate::db::library::Access;
use crate::storage::{Backend, Operation};
use theca_common::hash::Sha256;

pub fn router() -> Router<AppState> {
    Router::new()
        .route("/libraries", post(create_library).get(list_libraries))
        .route("/nodes/{id}/children", get(children))
        .route("/chunks/lookup", post(lookup))
        .route("/chunks/upload-urls", post(upload_urls))
        .route("/chunks/announce", post(announce))
        .route("/chunks/cancel", post(cancel))
        .route("/files/commit", post(commit))
        .route("/files/{id}/download", get(download))
        .route(
            "/blobs/{chunk}",
            // A chunk is at most MAX_SIZE plaintext; compression can grow it slightly.
            get(get_blob)
                .put(put_blob)
                .layer(DefaultBodyLimit::max(MAX_SIZE + MAX_SIZE / 64 + 1024)),
        )
}

async fn library_access(
    state: &AppState,
    user: &CurrentUser,
    library: Uuid,
) -> Result<Access, HttpError> {
    db::library::access(&state.pool, user.pk, library)
        .await?
        .ok_or_else(|| HttpError::not_found("library"))
}

fn require_write(access: &Access) -> Result<(), HttpError> {
    if access.can_write {
        Ok(())
    } else {
        Err(HttpError::forbidden("read-only access to this library"))
    }
}

async fn backend(state: &AppState, storage: i64) -> Result<Backend, HttpError> {
    Ok(state.backends.get(storage).await?)
}

/// Create a library: its root folder, the creator as `write` member, in the given
/// namespaces. Storage: the organization's default, else the instance default.
#[utoipa::path(post, path = "/api/v1/libraries", request_body = api::CreateLibrary, responses(
    (status = 201, body = api::Library),
    (status = 404, description = "Unknown namespace", body = api::ApiError),
    (status = 409, description = "Name taken, or no storage configured", body = api::ApiError),
))]
async fn create_library(
    State(state): State<AppState>,
    user: CurrentUser,
    ApiJson(req): ApiJson<api::CreateLibrary>,
) -> Result<(StatusCode, Json<api::Library>), HttpError> {
    use db::library::CreateError;
    let mut tx = state.pool.begin().await?;
    let library = db::library::create(
        &mut tx,
        user.organization,
        user.pk,
        req.name.as_str(),
        &req.namespaces,
    )
    .await
    .map_err(|e| match e {
        CreateError::NoStorage => {
            HttpError::conflict("no_storage", "no default storage configured")
        }
        CreateError::UnknownNamespaces(n) => HttpError::not_found("namespace").with_chunks(n),
        CreateError::Db(e) => e.into(),
    })?;
    tx.commit().await?;
    Ok((StatusCode::CREATED, Json(library)))
}

/// The libraries the user is a member of.
#[utoipa::path(get, path = "/api/v1/libraries", responses((status = 200, body = Vec<api::Library>)))]
async fn list_libraries(
    State(state): State<AppState>,
    user: CurrentUser,
) -> Result<Json<Vec<api::Library>>, HttpError> {
    Ok(Json(db::library::list(&state.pool, user.pk).await?))
}

/// A folder's children.
#[utoipa::path(get, path = "/api/v1/nodes/{id}/children", params(("id" = Uuid, Path)), responses(
    (status = 200, body = Vec<api::Node>),
    (status = 404, body = api::ApiError),
))]
async fn children(
    State(state): State<AppState>,
    user: CurrentUser,
    Path(id): Path<Uuid>,
) -> Result<Json<Vec<api::Node>>, HttpError> {
    let node = db::library::node_access(&state.pool, user.pk, id)
        .await?
        .filter(|n| !n.is_file)
        .ok_or_else(|| HttpError::not_found("folder"))?;
    Ok(Json(db::library::children(&state.pool, node.node).await?))
}

/// Which chunks to upload. Misses become pending chunks; every chunk returned is
/// touched (repeat the lookup as a keepalive during long uploads).
#[utoipa::path(post, path = "/api/v1/chunks/lookup", request_body = api::Lookup, responses(
    (status = 200, body = Vec<api::LookupEntry>),
))]
async fn lookup(
    State(state): State<AppState>,
    user: CurrentUser,
    ApiJson(req): ApiJson<api::Lookup>,
) -> Result<Json<Vec<api::LookupEntry>>, HttpError> {
    let access = library_access(&state, &user, req.library).await?;
    require_write(&access)?;
    Ok(Json(
        db::chunk::lookup(&state.pool, &access, &req.chunks).await?,
    ))
}

/// Declare what will be uploaded and get presigned upload URLs (in batches).
/// The declaration is fixed at the first request; the URL only accepts exactly
/// the declared bytes (their SHA-256 is signed). Checks the organization's quota.
#[utoipa::path(post, path = "/api/v1/chunks/upload-urls", request_body = Vec<api::Declare>, responses(
    (status = 200, body = Vec<api::ChunkUrl>),
    (status = 400, description = "`invalid_declaration`", body = api::ApiError),
    (status = 403, description = "`chunk_not_allowed`: not pending, or not writable", body = api::ApiError),
    (status = 409, description = "`declaration_mismatch`, `quota_exceeded`", body = api::ApiError),
))]
async fn upload_urls(
    State(state): State<AppState>,
    user: CurrentUser,
    ApiJson(req): ApiJson<Vec<api::Declare>>,
) -> Result<Json<Vec<api::ChunkUrl>>, HttpError> {
    use db::chunk::DeclareError;
    let lifetime = state.settings.presigned_url_lifetime;
    let max_uploads = state.settings.max_pending_uploads;
    let declared = db::chunk::declare(&state.pool, user.pk, user.organization, &req, lifetime, max_uploads)
        .await
        .map_err(|e| match e {
            DeclareError::NotAllowed(chunks) => {
                tracing::warn!(anomaly = "upload_url_not_allowed", user = %user.uuid, ?chunks,
                    "upload URL requested for chunks not pending or not writable");
                HttpError::forbidden("chunks not pending, or not in a library you can write")
                    .into_code("chunk_not_allowed")
                    .with_chunks(chunks)
            }
            DeclareError::Mismatch(chunks) => {
                tracing::warn!(anomaly = "declaration_mismatch", user = %user.uuid, ?chunks,
                    "chunk declared again with another size, compression or checksum");
                HttpError::conflict(
                    "declaration_mismatch",
                    "these chunks were declared with other values",
                )
                .with_chunks(chunks)
            }
            DeclareError::Invalid(chunks) => {
                tracing::warn!(anomaly = "invalid_declaration", user = %user.uuid, ?chunks,
                    "stored size above plain size, or unknown compression");
                HttpError::bad_request(
                    "invalid_declaration",
                    "stored size above plain size, or unknown compression",
                )
                .with_chunks(chunks)
            }
            DeclareError::TooManyUploads { in_progress, requested, max } => HttpError::new(
                StatusCode::TOO_MANY_REQUESTS,
                "too_many_pending_uploads",
                format!("{in_progress} uploads in progress and {requested} more requested, the limit is {max}: upload and announce first"),
            ),
            DeclareError::QuotaExceeded { quota, needed } => HttpError::conflict(
                "quota_exceeded",
                format!(
                    "the organization's quota ({quota} bytes) would be exceeded ({needed} bytes)"
                ),
            ),
            DeclareError::Db(e) => e.into(),
        })?;
    let mut urls = Vec::with_capacity(declared.len());
    for d in declared {
        let presigned = backend(&state, d.storage)
            .await?
            .upload_url(&state.signer, d.chunk, d.sha256, lifetime)
            .await?;
        urls.push(api::ChunkUrl {
            chunk: d.chunk,
            url: presigned.url,
            headers: presigned.headers,
        });
    }
    Ok(Json(urls))
}

/// Mark uploaded chunks as ready, after checking each object exists with the
/// declared size. Returns the chunks still to upload (object missing): request
/// new URLs for them (same declaration).
#[utoipa::path(post, path = "/api/v1/chunks/announce", request_body = api::Chunks, responses(
    (status = 200, description = "Chunks not uploaded yet", body = api::Chunks),
    (status = 403, description = "`chunk_not_allowed`", body = api::ApiError),
))]
async fn announce(
    State(state): State<AppState>,
    user: CurrentUser,
    ApiJson(req): ApiJson<api::Chunks>,
) -> Result<Json<api::Chunks>, HttpError> {
    let allowed = db::chunk::announceable(&state.pool, user.pk, &req.chunks).await?;
    let refused: Vec<Uuid> = req
        .chunks
        .iter()
        .filter(|c| !allowed.iter().any(|(a, _, _)| a == *c))
        .copied()
        .collect();
    if !refused.is_empty() {
        tracing::warn!(anomaly = "announce_not_allowed", user = %user.uuid, chunks = ?refused,
            "announce of chunks not pending, not declared, or not writable");
        return Err(
            HttpError::forbidden("chunks not pending, not declared, or not writable")
                .into_code("chunk_not_allowed")
                .with_chunks(refused),
        );
    }
    let mut missing = Vec::new();
    for (chunk, storage, declared) in allowed {
        match backend(&state, storage).await?.size(chunk).await? {
            Some(size) if i64::try_from(size).ok() == Some(declared) => {
                db::chunk::mark_uploaded(&state.pool, chunk).await?;
            }
            Some(size) => {
                // The signed checksum should make this impossible: a backend
                // ignoring it, or an attempt. The chunk stays pending.
                tracing::warn!(anomaly = "announce_size_mismatch", user = %user.uuid, %chunk, declared, size,
                    "uploaded object doesn't have the declared size");
                missing.push(chunk);
            }
            None => {
                tracing::warn!(anomaly = "announce_missing_object", user = %user.uuid, %chunk,
                    "announced chunk has no object");
                missing.push(chunk);
            }
        }
    }
    Ok(Json(api::Chunks { chunks: missing }))
}

/// Cancel an upload: tombstones the given chunks that no version references.
/// Their objects are deleted later, once their upload URLs have expired.
#[utoipa::path(post, path = "/api/v1/chunks/cancel", request_body = api::Chunks, responses((status = 204)))]
async fn cancel(
    State(state): State<AppState>,
    user: CurrentUser,
    ApiJson(req): ApiJson<api::Chunks>,
) -> Result<StatusCode, HttpError> {
    db::chunk::cancel(&state.pool, user.pk, &req.chunks).await?;
    Ok(StatusCode::NO_CONTENT)
}

/// Commit a version: a new file (`parent` + `name`) or a new version (`file`).
#[utoipa::path(post, path = "/api/v1/files/commit", request_body = api::Commit, responses(
    (status = 200, body = api::Committed),
    (status = 403, description = "Chunks not allowed (`chunk_not_allowed`)", body = api::ApiError),
    (status = 409, description = "`name_taken`, `chunk_missing`", body = api::ApiError),
))]
async fn commit(
    State(state): State<AppState>,
    user: CurrentUser,
    ApiJson(req): ApiJson<api::Commit>,
) -> Result<Json<api::Committed>, HttpError> {
    use db::file::{CommitError, Target};
    let access = library_access(&state, &user, req.library).await?;
    require_write(&access)?;
    let target = match (req.file, req.parent, req.name.as_ref()) {
        (Some(file), None, None) => Target::File {
            file,
            base_version: req.base_version,
        },
        (None, Some(parent), Some(name)) => Target::New {
            parent,
            name: name.as_str(),
        },
        _ => {
            return Err(HttpError::bad_request(
                "invalid_target",
                "give either `file`, or `parent` and `name`",
            ));
        }
    };
    let mut tx = state.pool.begin().await?;
    let committed = db::file::commit(
        &mut tx,
        &access,
        target,
        &req.chunks,
        req.message.as_deref(),
    )
    .await
    .map_err(|e| match e {
        CommitError::FileNotFound => HttpError::not_found("file"),
        CommitError::ParentNotFound => HttpError::not_found("folder"),
        CommitError::BaseNotFound => HttpError::not_found("base version"),
        CommitError::ChunksNotAllowed(chunks) => {
            tracing::warn!(anomaly = "commit_chunk_not_allowed", user = %user.uuid, library = %req.library,
                ?chunks, "commit with chunks outside the library's dedup scope");
            HttpError::forbidden("chunks outside this library's dedup scope")
                .with_chunks(chunks)
                .into_code("chunk_not_allowed")
        }
        CommitError::Db(e) => e.into(),
    })?;
    tx.commit().await?;
    Ok(Json(api::Committed {
        file: committed.file,
        version: committed.version,
        conflict: committed.conflict,
    }))
}

/// The current version's chunks, in order, with presigned download URLs.
#[utoipa::path(get, path = "/api/v1/files/{id}/download", params(("id" = Uuid, Path)), responses(
    (status = 200, body = api::Download),
    (status = 404, body = api::ApiError),
))]
async fn download(
    State(state): State<AppState>,
    user: CurrentUser,
    Path(id): Path<Uuid>,
) -> Result<Json<api::Download>, HttpError> {
    let node = db::library::node_access(&state.pool, user.pk, id)
        .await?
        .filter(|n| n.is_file)
        .ok_or_else(|| HttpError::not_found("file"))?;
    let version = node
        .current_version
        .ok_or_else(|| HttpError::not_found("version"))?;
    let (version, chunks) = db::file::version_chunks(&state.pool, version).await?;
    let lifetime = state.settings.presigned_url_lifetime;
    let mut out = Vec::with_capacity(chunks.len());
    for c in chunks {
        let url = backend(&state, c.storage)
            .await?
            .download_url(&state.signer, c.uuid, lifetime)
            .await?;
        out.push(api::DownloadChunk {
            chunk: c.uuid,
            hash: c.hash,
            compression: c.compression,
            plain_size: c.plain_size,
            url,
        });
    }
    Ok(Json(api::Download {
        version,
        chunks: out,
    }))
}

#[derive(Deserialize)]
struct Signed {
    op: String,
    expires: u64,
    #[serde(default)]
    sha256: String,
    sig: String,
}

fn check_signature(
    state: &AppState,
    op: Operation,
    chunk: Uuid,
    q: &Signed,
    ip: ClientIp,
) -> Result<(), HttpError> {
    let expected = match op {
        Operation::Put => "put",
        Operation::Get => "get",
    };
    if q.op == expected && state.signer.verify(op, chunk, q.expires, &q.sha256, &q.sig) {
        Ok(())
    } else {
        tracing::warn!(anomaly = "invalid_signature", %chunk, ip = ?ip.0, "invalid or expired presigned URL");
        Err(HttpError::forbidden("invalid or expired URL"))
    }
}

/// Presigned upload (filesystem backend). No authentication: the URL is the
/// authorization. Accepts exactly the declared bytes (SHA-256), while pending.
async fn put_blob(
    State(state): State<AppState>,
    Path(chunk): Path<Uuid>,
    Query(q): Query<Signed>,
    ip: ClientIp,
    body: Bytes,
) -> Result<StatusCode, HttpError> {
    check_signature(&state, Operation::Put, chunk, &q, ip)?;
    let Some((storage, declared)) = db::chunk::pending_upload(&state.pool, chunk).await? else {
        tracing::warn!(anomaly = "upload_not_pending", %chunk, ip = ?ip.0, "upload to a chunk no longer pending");
        return Err(HttpError::conflict(
            "not_pending",
            "this chunk is already uploaded, or cancelled",
        ));
    };
    if Sha256::of(&body).0[..] != declared[..] {
        tracing::warn!(anomaly = "upload_checksum_mismatch", %chunk, ip = ?ip.0, "uploaded bytes don't match the declared SHA-256");
        return Err(HttpError::bad_request(
            "bad_digest",
            "the uploaded bytes don't match the declared SHA-256",
        ));
    }
    backend(&state, storage)
        .await?
        .put(chunk, body.to_vec())
        .await?;
    Ok(StatusCode::OK)
}

/// Presigned download (filesystem backend).
async fn get_blob(
    State(state): State<AppState>,
    Path(chunk): Path<Uuid>,
    Query(q): Query<Signed>,
    ip: ClientIp,
) -> Result<Vec<u8>, HttpError> {
    check_signature(&state, Operation::Get, chunk, &q, ip)?;
    let storage = db::chunk::download_storage(&state.pool, chunk)
        .await?
        .ok_or_else(|| HttpError::not_found("chunk"))?;
    Ok(backend(&state, storage).await?.get(chunk).await?)
}
