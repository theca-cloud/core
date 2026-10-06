//! `/api/v1`: libraries, folders, and the upload/download protocol
//! (see docs/decisions.md, Upload flow).

use axum::body::Bytes;
use axum::extract::{DefaultBodyLimit, Path, Query, State};
use axum::http::StatusCode;
use axum::routing::{get, patch, post};
use axum::{Json, Router};
use serde::Deserialize;
use theca_common::api;
use theca_common::chunking::MAX_SIZE;
use uuid::Uuid;

use super::AppState;
use super::auth::{ClientIp, CurrentAccount};
use super::error::{ApiJson, HttpError};
use crate::db;
use crate::db::library::Access;
use crate::storage::{Backend, Operation};
use theca_common::hash::Sha256;

pub fn router() -> Router<AppState> {
    Router::new()
        .route("/limits", get(limits))
        .route("/libraries", post(create_library).get(list_libraries))
        .route(
            "/libraries/{id}",
            patch(update_library).delete(delete_library),
        )
        .route("/libraries/{id}/restore", post(restore_library))
        .route("/organizations/{id}/trash", get(organization_trash))
        .route("/nodes/{id}/children", get(children))
        .route("/chunks/lookup", post(lookup))
        .route("/chunks/upload-urls", post(upload_urls))
        .route("/chunks/announce", post(announce))
        .route("/chunks/cancel", post(cancel))
        .route("/files/commit", post(commit))
        .route("/files/download", post(download))
        .route(
            "/blobs/{chunk}",
            // A chunk is at most MAX_SIZE plaintext; compression can grow it slightly.
            get(get_blob)
                .put(put_blob)
                .layer(DefaultBodyLimit::max(MAX_SIZE + MAX_SIZE / 64 + 1024)),
        )
}

/// The instance's limits, as they stand (they change live): clients size their
/// work by them, e.g. an upload engine's window.
#[utoipa::path(get, path = "/api/v1/limits", responses((status = 200, body = api::Limits)))]
async fn limits(State(state): State<AppState>, _user: CurrentAccount) -> Json<api::Limits> {
    let settings = state.settings.get();
    Json(api::Limits {
        max_pending_uploads: u32::try_from(settings.max_pending_uploads).unwrap_or(u32::MAX),
    })
}

pub(super) async fn library_access(
    state: &AppState,
    user: &CurrentAccount,
    library: Uuid,
) -> Result<Access, HttpError> {
    db::library::access(&state.pool, user, library)
        .await?
        .ok_or_else(|| HttpError::not_found("library"))
}

/// Rename a library (a `write` member).
#[utoipa::path(patch, path = "/api/v1/libraries/{id}", params(("id" = Uuid, Path)), request_body = api::UpdateLibrary, responses(
    (status = 200, body = api::Library),
    (status = 404, body = api::ApiError),
    (status = 409, description = "`library_name_taken`", body = api::ApiError),
))]
async fn update_library(
    State(state): State<AppState>,
    user: CurrentAccount,
    Path(id): Path<Uuid>,
    ApiJson(req): ApiJson<api::UpdateLibrary>,
) -> Result<Json<api::Library>, HttpError> {
    let access = library_access(&state, &user, id).await?;
    crate::files::rename_library(&state, &access, req.name.as_str()).await?;
    Ok(Json(db::library::get(&state.pool, access.library).await?))
}

#[derive(Debug, Deserialize, utoipa::IntoParams)]
struct DeleteLibraryParams {
    /// Delete for good (a library already in the trash only).
    #[serde(default)]
    permanent: bool,
}

/// Put a library, with everything in it, in its organization's trash (a
/// `write` member); or (`permanent`, from the trash) delete it for good.
#[utoipa::path(delete, path = "/api/v1/libraries/{id}", params(("id" = Uuid, Path), DeleteLibraryParams), responses(
    (status = 204),
    (status = 404, body = api::ApiError),
))]
async fn delete_library(
    State(state): State<AppState>,
    user: CurrentAccount,
    Path(id): Path<Uuid>,
    Query(params): Query<DeleteLibraryParams>,
) -> Result<StatusCode, HttpError> {
    if params.permanent {
        let access = trashed_library(&state, &user, id).await?;
        crate::files::purge_library(&state, &access).await?;
    } else {
        let access = library_access(&state, &user, id).await?;
        crate::files::trash_library(&state, &access).await?;
    }
    Ok(StatusCode::NO_CONTENT)
}

/// Take a library out of the trash (renamed ` (restored)` if its name was
/// taken since).
#[utoipa::path(post, path = "/api/v1/libraries/{id}/restore", params(("id" = Uuid, Path)), responses(
    (status = 200, body = api::Library),
    (status = 404, body = api::ApiError),
))]
async fn restore_library(
    State(state): State<AppState>,
    user: CurrentAccount,
    Path(id): Path<Uuid>,
) -> Result<Json<api::Library>, HttpError> {
    let access = trashed_library(&state, &user, id).await?;
    crate::files::restore_library(&state, &access).await?;
    Ok(Json(db::library::get(&state.pool, access.library).await?))
}

/// An organization's trashed libraries (those the account is a member of).
#[utoipa::path(get, path = "/api/v1/organizations/{id}/trash", params(("id" = Uuid, Path)), responses(
    (status = 200, body = Vec<api::TrashedLibrary>),
))]
async fn organization_trash(
    State(state): State<AppState>,
    user: CurrentAccount,
    Path(id): Path<Uuid>,
) -> Result<Json<Vec<api::TrashedLibrary>>, HttpError> {
    Ok(Json(db::library::trash_list(&state.pool, &user, id).await?))
}

async fn trashed_library(
    state: &AppState,
    user: &CurrentAccount,
    library: Uuid,
) -> Result<Access, HttpError> {
    db::library::trashed_access(&state.pool, user, library)
        .await?
        .ok_or_else(|| HttpError::not_found("trashed library"))
}

pub(super) fn require_write(access: &Access) -> Result<(), HttpError> {
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
    user: CurrentAccount,
    ApiJson(req): ApiJson<api::CreateLibrary>,
) -> Result<(StatusCode, Json<api::Library>), HttpError> {
    use db::library::CreateError;
    let mut tx = state.pool.begin().await?;
    let library = db::library::create(
        &mut tx,
        &user,
        req.organization,
        req.name.as_str(),
        &req.namespaces,
    )
    .await
    .map_err(|e| match e {
        CreateError::NotMember => HttpError::not_found("organization"),
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
    user: CurrentAccount,
) -> Result<Json<Vec<api::Library>>, HttpError> {
    Ok(Json(db::library::list(&state.pool, &user).await?))
}

/// A folder's children.
#[utoipa::path(get, path = "/api/v1/nodes/{id}/children", params(("id" = Uuid, Path)), responses(
    (status = 200, body = Vec<api::Node>),
    (status = 404, body = api::ApiError),
))]
async fn children(
    State(state): State<AppState>,
    user: CurrentAccount,
    Path(id): Path<Uuid>,
) -> Result<Json<Vec<api::Node>>, HttpError> {
    let node = db::library::node_access(&state.pool, &user, id)
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
    user: CurrentAccount,
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
    user: CurrentAccount,
    ApiJson(req): ApiJson<Vec<api::Declare>>,
) -> Result<Json<Vec<api::ChunkUrl>>, HttpError> {
    use db::chunk::DeclareError;
    let lifetime = state.settings.get().presigned_url_lifetime;
    let max_uploads = state.settings.get().max_pending_uploads;
    let declared = db::chunk::declare(&state.pool, &user, &req, lifetime, max_uploads)
        .await
        .map_err(|e| match e {
            DeclareError::NotAllowed(chunks) => {
                tracing::warn!(anomaly = "upload_url_not_allowed", account = %user.account_uuid, ?chunks,
                    "upload URL requested for chunks not pending or not writable");
                HttpError::forbidden("chunks not pending, or not in a library you can write")
                    .into_code("chunk_not_allowed")
                    .with_chunks(chunks)
            }
            DeclareError::Mismatch(chunks) => {
                tracing::warn!(anomaly = "declaration_mismatch", account = %user.account_uuid, ?chunks,
                    "chunk declared again with another size, compression or checksum");
                HttpError::conflict(
                    "declaration_mismatch",
                    "these chunks were declared with other values",
                )
                .with_chunks(chunks)
            }
            DeclareError::Invalid(chunks) => {
                tracing::warn!(anomaly = "invalid_declaration", account = %user.account_uuid, ?chunks,
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
            DeclareError::MixedOrganizations => HttpError::bad_request(
                "mixed_organizations",
                "one request declares chunks of a single organization",
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
    user: CurrentAccount,
    ApiJson(req): ApiJson<api::Chunks>,
) -> Result<Json<api::Chunks>, HttpError> {
    let allowed = db::chunk::announceable(&state.pool, &user, &req.chunks).await?;
    let refused: Vec<Uuid> = req
        .chunks
        .iter()
        .filter(|c| !allowed.iter().any(|(a, _, _)| a == *c))
        .copied()
        .collect();
    if !refused.is_empty() {
        tracing::warn!(anomaly = "announce_not_allowed", account = %user.account_uuid, chunks = ?refused,
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
                tracing::warn!(anomaly = "announce_size_mismatch", account = %user.account_uuid, %chunk, declared, size,
                    "uploaded object doesn't have the declared size");
                missing.push(chunk);
            }
            None => {
                tracing::warn!(anomaly = "announce_missing_object", account = %user.account_uuid, %chunk,
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
    user: CurrentAccount,
    ApiJson(req): ApiJson<api::Chunks>,
) -> Result<StatusCode, HttpError> {
    db::chunk::cancel(&state.pool, &user, &req.chunks).await?;
    Ok(StatusCode::NO_CONTENT)
}

/// Most commits in one batch.
const COMMIT_BATCH: usize = 100;

/// Commit versions: each a new file (`parent` + `name`) or a new version
/// (`file`), in its own transaction, so one failing never undoes the others.
/// The outcomes come in the request's order; one commit is a batch of one.
/// An outcome's error: `name_taken`, `chunk_missing` (with the chunks to
/// upload again), `chunk_not_allowed`, `not_found` (file, folder, base
/// version), `invalid_target`.
#[utoipa::path(post, path = "/api/v1/files/commit", request_body = api::Commits, responses(
    (status = 200, body = Vec<api::CommitOutcome>),
    (status = 400, description = "`batch_too_large`", body = api::ApiError),
))]
async fn commit(
    State(state): State<AppState>,
    user: CurrentAccount,
    ApiJson(req): ApiJson<api::Commits>,
) -> Result<Json<Vec<api::CommitOutcome>>, HttpError> {
    if req.commits.len() > COMMIT_BATCH {
        return Err(HttpError::bad_request(
            "batch_too_large",
            format!("at most {COMMIT_BATCH} commits per batch"),
        ));
    }
    let mut outcomes = Vec::with_capacity(req.commits.len());
    for commit in &req.commits {
        outcomes.push(match commit_one(&state, &user, commit).await {
            Ok(committed) => api::CommitOutcome {
                committed: Some(committed),
                error: None,
                status: None,
            },
            Err(e) => {
                let (status, error) = e.into_parts();
                api::CommitOutcome {
                    committed: None,
                    error: Some(error),
                    status: Some(status.as_u16()),
                }
            }
        });
    }
    Ok(Json(outcomes))
}

/// One commit, in its own transaction.
async fn commit_one(
    state: &AppState,
    user: &CurrentAccount,
    req: &api::Commit,
) -> Result<api::Committed, HttpError> {
    use db::file::{CommitError, Target};
    let access = library_access(state, user, req.library).await?;
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
        CommitError::ChunksMissing(chunks) => HttpError::conflict(
            "chunk_missing",
            "chunks not uploaded (pending) or deleted: upload them again",
        )
        .with_chunks(chunks),
        CommitError::ChunksNotAllowed(chunks) => {
            tracing::warn!(anomaly = "commit_chunk_not_allowed", account = %user.account_uuid, library = %req.library,
                ?chunks, "commit with chunks outside the library's dedup scope");
            HttpError::forbidden("chunks outside this library's dedup scope")
                .with_chunks(chunks)
                .into_code("chunk_not_allowed")
        }
        CommitError::Db(e) => e.into(),
    })?;
    tx.commit().await?;
    Ok(api::Committed {
        file: committed.file,
        version: committed.version,
        conflict: committed.conflict,
        unchanged: committed.unchanged,
    })
}

/// Most files in one download request.
const DOWNLOAD_BATCH: usize = 100;

/// Files' chunks, in order, with presigned download URLs: each file's current
/// version, or the one asked. The outcomes come in the request's order.
#[utoipa::path(post, path = "/api/v1/files/download", request_body = api::Downloads, responses(
    (status = 200, body = Vec<api::DownloadOutcome>),
    (status = 400, description = "`batch_too_large`", body = api::ApiError),
))]
async fn download(
    State(state): State<AppState>,
    user: CurrentAccount,
    ApiJson(req): ApiJson<api::Downloads>,
) -> Result<Json<Vec<api::DownloadOutcome>>, HttpError> {
    if req.files.len() > DOWNLOAD_BATCH {
        return Err(HttpError::bad_request(
            "batch_too_large",
            format!("at most {DOWNLOAD_BATCH} files per request"),
        ));
    }
    let mut outcomes = Vec::with_capacity(req.files.len());
    for file in &req.files {
        outcomes.push(match download_one(&state, &user, file).await {
            Ok(download) => api::DownloadOutcome {
                download: Some(download),
                error: None,
                status: None,
            },
            Err(e) => {
                let (status, error) = e.into_parts();
                api::DownloadOutcome {
                    download: None,
                    error: Some(error),
                    status: Some(status.as_u16()),
                }
            }
        });
    }
    Ok(Json(outcomes))
}

/// One file's version, its chunks with their URLs.
async fn download_one(
    state: &AppState,
    user: &CurrentAccount,
    req: &api::DownloadFile,
) -> Result<api::Download, HttpError> {
    let node = db::library::node_access(&state.pool, user, req.file)
        .await?
        .filter(|n| n.is_file)
        .ok_or_else(|| HttpError::not_found("file"))?;
    let version = match req.version {
        Some(version) => db::node::version_of(&state.pool, node.node, version).await?,
        None => node.current_version,
    }
    .ok_or_else(|| HttpError::not_found("version"))?;
    let (version, chunks) = db::file::version_chunks(&state.pool, version).await?;
    let lifetime = state.settings.get().presigned_url_lifetime;
    let mut out = Vec::with_capacity(chunks.len());
    for c in chunks {
        let url = backend(state, c.storage)
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
    Ok(api::Download {
        version,
        chunks: out,
    })
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
    metrics::counter!("theca_bytes_total", "direction" => "in").increment(body.len() as u64);
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
    let bytes = backend(&state, storage).await?.get(chunk).await?;
    metrics::counter!("theca_bytes_total", "direction" => "out").increment(bytes.len() as u64);
    Ok(bytes)
}
