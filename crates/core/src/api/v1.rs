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
use super::auth::CurrentUser;
use super::error::{ApiJson, HttpError};
use crate::db;
use crate::db::library::Access;
use crate::storage::{Backend, Operation};

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
    let (implementation, details) = db::chunk::storage(&state.pool, storage).await?;
    Ok(Backend::new(&implementation, &details)?)
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
        db::chunk::lookup(&state.pool, &access, &req.hashes).await?,
    ))
}

/// Presigned upload URLs for pending chunks (request them in batches).
#[utoipa::path(post, path = "/api/v1/chunks/upload-urls", request_body = api::Chunks, responses(
    (status = 200, body = Vec<api::ChunkUrl>),
    (status = 403, description = "Chunks not pending or not writable", body = api::ApiError),
))]
async fn upload_urls(
    State(state): State<AppState>,
    user: CurrentUser,
    ApiJson(req): ApiJson<api::Chunks>,
) -> Result<Json<Vec<api::ChunkUrl>>, HttpError> {
    let allowed = writable_pending(&state, &user, &req.chunks).await?;
    Ok(Json(
        allowed
            .iter()
            .map(|(chunk, _)| api::ChunkUrl {
                chunk: *chunk,
                url: state.signer.url(Operation::Put, *chunk),
            })
            .collect(),
    ))
}

/// All requested chunks must be pending and uploaded into a library the user can write.
async fn writable_pending(
    state: &AppState,
    user: &CurrentUser,
    chunks: &[Uuid],
) -> Result<Vec<(Uuid, i64)>, HttpError> {
    let allowed = db::chunk::writable_pending(&state.pool, user.pk, chunks).await?;
    let refused: Vec<Uuid> = chunks
        .iter()
        .filter(|c| !allowed.iter().any(|(a, _)| a == *c))
        .copied()
        .collect();
    if refused.is_empty() {
        Ok(allowed)
    } else {
        Err(
            HttpError::forbidden("chunks not pending, or not in a library you can write")
                .with_chunks(refused),
        )
    }
}

/// Mark uploaded chunks as ready. The real size is read from the backend; a
/// chunk whose object is missing stays pending (`upload`).
#[utoipa::path(post, path = "/api/v1/chunks/announce", request_body = Vec<api::Announce>, responses(
    (status = 200, description = "Chunks still to upload (object missing), with fresh URLs", body = Vec<api::ChunkUrl>),
))]
async fn announce(
    State(state): State<AppState>,
    user: CurrentUser,
    ApiJson(req): ApiJson<Vec<api::Announce>>,
) -> Result<Json<Vec<api::ChunkUrl>>, HttpError> {
    let ids: Vec<Uuid> = req.iter().map(|a| a.chunk).collect();
    let allowed = writable_pending(&state, &user, &ids).await?;
    let mut missing = Vec::new();
    for item in &req {
        let storage = allowed
            .iter()
            .find(|(c, _)| *c == item.chunk)
            .expect("checked")
            .1;
        match backend(&state, storage).await?.size(item.chunk).await? {
            Some(size) => {
                db::chunk::announce(
                    &state.pool,
                    item.chunk,
                    size,
                    item.plain_size,
                    item.compression,
                )
                .await?
            }
            None => missing.push(item.chunk),
        }
    }
    // The chunks still to upload, with a fresh URL.
    Ok(Json(
        missing
            .into_iter()
            .map(|chunk| api::ChunkUrl {
                chunk,
                url: state.signer.url(Operation::Put, chunk),
            })
            .collect(),
    ))
}

/// Cancel an upload: deletes the given chunks that no version references.
#[utoipa::path(post, path = "/api/v1/chunks/cancel", request_body = api::Chunks, responses((status = 204)))]
async fn cancel(
    State(state): State<AppState>,
    user: CurrentUser,
    ApiJson(req): ApiJson<api::Chunks>,
) -> Result<StatusCode, HttpError> {
    // Rows first, then objects: a crash in between leaves an orphan object
    // (wasted space), never a row pointing to a missing object.
    for (chunk, storage) in db::chunk::cancel(&state.pool, user.pk, &req.chunks).await? {
        backend(&state, storage).await?.delete(chunk).await?;
    }
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
        CommitError::ChunksNotAllowed(c) => {
            HttpError::forbidden("chunks outside this library's dedup scope")
                .with_chunks(c)
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
    Ok(Json(api::Download {
        version,
        chunks: chunks
            .into_iter()
            .map(|c| api::DownloadChunk {
                chunk: c.uuid,
                hash: c.hash,
                compression: c.compression,
                plain_size: c.plain_size,
                url: state.signer.url(Operation::Get, c.uuid),
            })
            .collect(),
    }))
}

#[derive(Deserialize)]
struct Signed {
    op: String,
    expires: u64,
    sig: String,
}

fn check_signature(
    state: &AppState,
    op: Operation,
    chunk: Uuid,
    q: &Signed,
) -> Result<(), HttpError> {
    let expected = match op {
        Operation::Put => "put",
        Operation::Get => "get",
    };
    if q.op == expected && state.signer.verify(op, chunk, q.expires, &q.sig) {
        Ok(())
    } else {
        Err(HttpError::forbidden("invalid or expired URL"))
    }
}

/// Presigned upload (filesystem backend). No authentication: the URL is the authorization.
async fn put_blob(
    State(state): State<AppState>,
    Path(chunk): Path<Uuid>,
    Query(q): Query<Signed>,
    body: Bytes,
) -> Result<StatusCode, HttpError> {
    check_signature(&state, Operation::Put, chunk, &q)?;
    let storage = db::chunk::blob_storage(&state.pool, chunk, true)
        .await?
        .ok_or_else(|| HttpError::not_found("pending chunk"))?;
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
) -> Result<Vec<u8>, HttpError> {
    check_signature(&state, Operation::Get, chunk, &q)?;
    let storage = db::chunk::blob_storage(&state.pool, chunk, false)
        .await?
        .ok_or_else(|| HttpError::not_found("chunk"))?;
    Ok(backend(&state, storage).await?.get(chunk).await?)
}
