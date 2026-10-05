//! File operations: folders, renames, moves, copies, the trash, versions and
//! search. Reads need any access to the library, writes `write`; a node in the
//! trash (or under a trashed folder) is unreachable except from the trash.

use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::routing::{get, post};
use axum::{Json, Router};
use serde::Deserialize;
use theca_common::api;
use uuid::Uuid;

use super::AppState;
use super::auth::CurrentAccount;
use super::error::{ApiJson, HttpError};
use super::v1::{library_access, require_write};
use crate::db;
use crate::db::library::NodeAccess;
use crate::files;

pub fn router() -> Router<AppState> {
    Router::new()
        .route("/nodes/{id}", get(info).patch(update).delete(delete))
        .route("/nodes/{id}/folders", post(create_folder))
        .route("/nodes/{id}/copy", post(copy))
        .route("/nodes/{id}/restore", post(restore))
        .route("/libraries/{id}/trash", get(trash))
        .route("/files/{id}/versions", get(versions))
        .route("/files/{id}/content", get(super::content::content))
        .route(
            "/files/{id}/versions/{version}/restore",
            post(restore_version),
        )
        .route("/search", get(search))
}

async fn node(state: &AppState, user: &CurrentAccount, id: Uuid) -> Result<NodeAccess, HttpError> {
    db::library::node_access(&state.pool, user, id)
        .await?
        .ok_or_else(|| HttpError::not_found("node"))
}

async fn file(state: &AppState, user: &CurrentAccount, id: Uuid) -> Result<NodeAccess, HttpError> {
    let file = node(state, user, id).await?;
    if !file.is_file {
        return Err(HttpError::not_found("file"));
    }
    Ok(file)
}

/// A file or folder, with its library and path.
#[utoipa::path(get, path = "/api/v1/nodes/{id}", params(("id" = Uuid, Path)), responses(
    (status = 200, body = api::NodeInfo),
    (status = 404, body = api::ApiError),
))]
pub async fn info(
    State(state): State<AppState>,
    user: CurrentAccount,
    Path(id): Path<Uuid>,
) -> Result<Json<api::NodeInfo>, HttpError> {
    let node = node(&state, &user, id).await?;
    Ok(Json(db::node::info(&state.pool, node.node).await?))
}

/// Create a folder.
#[utoipa::path(post, path = "/api/v1/nodes/{id}/folders", params(("id" = Uuid, Path)), request_body = api::CreateFolder, responses(
    (status = 201, body = api::Node),
    (status = 404, body = api::ApiError),
    (status = 409, description = "`name_taken`", body = api::ApiError),
))]
pub async fn create_folder(
    State(state): State<AppState>,
    user: CurrentAccount,
    Path(id): Path<Uuid>,
    ApiJson(req): ApiJson<api::CreateFolder>,
) -> Result<(StatusCode, Json<api::Node>), HttpError> {
    let parent = node(&state, &user, id).await?;
    let created = files::create_folder(&state, &parent, req.name.as_str()).await?;
    Ok((StatusCode::CREATED, Json(created)))
}

/// Rename and/or move a file or folder. Within a library a move is one row;
/// to another library of the same organization, the subtree (and its
/// history) follows.
#[utoipa::path(patch, path = "/api/v1/nodes/{id}", params(("id" = Uuid, Path)), request_body = api::UpdateNode, responses(
    (status = 200, body = api::NodeInfo),
    (status = 400, description = "`root`", body = api::ApiError),
    (status = 404, body = api::ApiError),
    (status = 409, description = "`name_taken`, `move_into_itself`, `other_organization`", body = api::ApiError),
))]
pub async fn update(
    State(state): State<AppState>,
    user: CurrentAccount,
    Path(id): Path<Uuid>,
    ApiJson(req): ApiJson<api::UpdateNode>,
) -> Result<Json<api::NodeInfo>, HttpError> {
    let node = node(&state, &user, id).await?;
    let parent = match req.parent {
        Some(parent) => Some(super::nodes::node(&state, &user, parent).await?),
        None => None,
    };
    let name = req.name.as_ref().map(|n| n.as_str());
    files::rename_move(&state, &node, name, parent.as_ref()).await?;
    Ok(Json(db::node::info(&state.pool, node.node).await?))
}

#[derive(Debug, Deserialize, utoipa::IntoParams)]
pub struct DeleteParams {
    /// Delete for good (a node already in the trash only).
    #[serde(default)]
    permanent: bool,
}

/// Put a file or folder in the trash, or (`permanent`, from the trash) delete
/// it for good with its subtree and history.
#[utoipa::path(delete, path = "/api/v1/nodes/{id}", params(("id" = Uuid, Path), DeleteParams), responses(
    (status = 204),
    (status = 400, description = "`root`", body = api::ApiError),
    (status = 404, body = api::ApiError),
))]
pub async fn delete(
    State(state): State<AppState>,
    user: CurrentAccount,
    Path(id): Path<Uuid>,
    Query(params): Query<DeleteParams>,
) -> Result<StatusCode, HttpError> {
    if params.permanent {
        let node = db::node::trashed_access(&state.pool, &user, id)
            .await?
            .ok_or_else(|| HttpError::not_found("trashed node"))?;
        require_write(&node.access)?;
        let mut tx = state.pool.begin().await?;
        db::node::purge(&mut tx, node.node).await?;
        tx.commit().await?;
    } else {
        let node = node(&state, &user, id).await?;
        files::trash(&state, &node).await?;
    }
    Ok(StatusCode::NO_CONTENT)
}

/// Copy a file or folder into a folder of any library of the same
/// organization. Files keep their current version only, sharing its chunks.
#[utoipa::path(post, path = "/api/v1/nodes/{id}/copy", params(("id" = Uuid, Path)), request_body = api::CopyNode, responses(
    (status = 201, body = api::NodeInfo),
    (status = 404, body = api::ApiError),
    (status = 409, description = "`name_taken`, `other_organization`", body = api::ApiError),
))]
pub async fn copy(
    State(state): State<AppState>,
    user: CurrentAccount,
    Path(id): Path<Uuid>,
    ApiJson(req): ApiJson<api::CopyNode>,
) -> Result<(StatusCode, Json<api::NodeInfo>), HttpError> {
    let source = node(&state, &user, id).await?;
    let dest = node(&state, &user, req.parent).await?;
    let name = req.name.as_ref().map(|n| n.as_str());
    let copy = files::copy(&state, &source, &dest, name).await?;
    let copy = node(&state, &user, copy).await?;
    Ok((
        StatusCode::CREATED,
        Json(db::node::info(&state.pool, copy.node).await?),
    ))
}

/// A library's trash.
#[utoipa::path(get, path = "/api/v1/libraries/{id}/trash", params(("id" = Uuid, Path)), responses(
    (status = 200, body = Vec<api::TrashEntry>),
    (status = 404, body = api::ApiError),
))]
pub async fn trash(
    State(state): State<AppState>,
    user: CurrentAccount,
    Path(id): Path<Uuid>,
) -> Result<Json<Vec<api::TrashEntry>>, HttpError> {
    let access = library_access(&state, &user, id).await?;
    Ok(Json(
        db::node::trash_list(&state.pool, access.library).await?,
    ))
}

/// Take a file or folder out of the trash: back in its folder, or into the
/// library's root if that one is trashed too; renamed if its name is taken.
#[utoipa::path(post, path = "/api/v1/nodes/{id}/restore", params(("id" = Uuid, Path)), responses(
    (status = 200, body = api::Restored),
    (status = 404, body = api::ApiError),
))]
pub async fn restore(
    State(state): State<AppState>,
    user: CurrentAccount,
    Path(id): Path<Uuid>,
) -> Result<Json<api::Restored>, HttpError> {
    let node = db::node::trashed_access(&state.pool, &user, id)
        .await?
        .ok_or_else(|| HttpError::not_found("trashed node"))?;
    require_write(&node.access)?;
    let mut tx = state.pool.begin().await?;
    let restored = db::node::restore(&mut tx, &node).await?;
    tx.commit().await?;
    Ok(Json(restored))
}

/// A file's versions: a tree (branches after conflicts and restores).
#[utoipa::path(get, path = "/api/v1/files/{id}/versions", params(("id" = Uuid, Path)), responses(
    (status = 200, body = Vec<api::Version>),
    (status = 404, body = api::ApiError),
))]
pub async fn versions(
    State(state): State<AppState>,
    user: CurrentAccount,
    Path(id): Path<Uuid>,
) -> Result<Json<Vec<api::Version>>, HttpError> {
    let file = file(&state, &user, id).await?;
    Ok(Json(db::node::versions(&state.pool, file.node).await?))
}

/// Make an old version current. The next commit based on it branches the tree.
#[utoipa::path(post, path = "/api/v1/files/{id}/versions/{version}/restore", params(("id" = Uuid, Path), ("version" = Uuid, Path)), responses(
    (status = 204),
    (status = 404, body = api::ApiError),
))]
pub async fn restore_version(
    State(state): State<AppState>,
    user: CurrentAccount,
    Path((id, version)): Path<(Uuid, Uuid)>,
) -> Result<StatusCode, HttpError> {
    let file = file(&state, &user, id).await?;
    require_write(&file.access)?;
    let version = db::node::version_of(&state.pool, file.node, version)
        .await?
        .ok_or_else(|| HttpError::not_found("version"))?;
    db::node::set_current(&state.pool, file.node, version).await?;
    Ok(StatusCode::NO_CONTENT)
}

#[derive(Debug, Deserialize, utoipa::IntoParams)]
pub struct SearchParams {
    /// Part of a name (at least 3 characters).
    q: String,
    /// Only this library.
    library: Option<Uuid>,
}

/// Files and folders by name (case-insensitive), in the libraries the user
/// reaches, outside the trash. At most 50.
#[utoipa::path(get, path = "/api/v1/search", params(SearchParams), responses(
    (status = 200, body = Vec<api::SearchResult>),
    (status = 400, description = "`query_too_short`", body = api::ApiError),
))]
pub async fn search(
    State(state): State<AppState>,
    user: CurrentAccount,
    Query(params): Query<SearchParams>,
) -> Result<Json<Vec<api::SearchResult>>, HttpError> {
    let q = params.q.trim();
    if q.chars().count() < 3 {
        return Err(HttpError::bad_request(
            "query_too_short",
            "search needs at least 3 characters",
        ));
    }
    Ok(Json(
        db::node::search(&state.pool, &user, q, params.library).await?,
    ))
}
