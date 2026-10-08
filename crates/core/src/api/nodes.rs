//! File operations: folders, renames, moves, copies, the trash, versions and
//! search. Reads need any access to the library, writes `write`; a node in the
//! trash (or under a trashed folder) is unreachable except from the trash.

use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::routing::{get, patch, post};
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
        .route("/nodes", patch(update))
        .route("/nodes/copy", post(copy))
        .route("/nodes/trash", post(trash_nodes))
        .route("/nodes/restore", post(restore))
        .route("/nodes/{id}", get(info))
        .route("/nodes/{id}/folders", post(create_folder))
        .route("/nodes/{id}/tree", get(tree))
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

/// Most paths in one folders request.
const FOLDERS_BATCH: usize = 1000;

/// Create folders below a folder, like `mkdir -p`: each path's missing
/// folders created, existing ones kept, all in one transaction. Each path's
/// folder, in order.
#[utoipa::path(post, path = "/api/v1/nodes/{id}/folders", params(("id" = Uuid, Path)), request_body = api::CreateFolders, responses(
    (status = 200, body = Vec<api::FolderPath>),
    (status = 400, description = "`name_invalid`, `batch_too_large`", body = api::ApiError),
    (status = 404, body = api::ApiError),
    (status = 409, description = "`name_taken` (a file has a path's name)", body = api::ApiError),
))]
pub async fn create_folder(
    State(state): State<AppState>,
    user: CurrentAccount,
    Path(id): Path<Uuid>,
    ApiJson(req): ApiJson<api::CreateFolders>,
) -> Result<Json<Vec<api::FolderPath>>, HttpError> {
    if req.paths.len() > FOLDERS_BATCH {
        return Err(HttpError::bad_request(
            "batch_too_large",
            format!("at most {FOLDERS_BATCH} paths per request"),
        ));
    }
    let parent = node(&state, &user, id).await?;
    Ok(Json(
        files::create_folders(&state, &parent, &req.paths).await?,
    ))
}

/// How long the tree's stream waits on a client not reading before giving up
/// (freeing its database connection).
const TREE_STALL: std::time::Duration = std::time::Duration::from_secs(30);

/// A folder's whole subtree (outside the trash) in one streamed answer: NDJSON,
/// one `{"node": ...}` line per node, parents before their children, then
/// `{"end": {"count": n}}`, or `{"error": ...}` if it fails midway. A stream
/// without its end line was cut.
#[utoipa::path(get, path = "/api/v1/nodes/{id}/tree", params(("id" = Uuid, Path)), responses(
    (status = 200, description = "NDJSON lines", body = api::TreeLine, content_type = "application/x-ndjson"),
    (status = 400, description = "`not_a_folder`", body = api::ApiError),
    (status = 404, body = api::ApiError),
))]
pub async fn tree(
    State(state): State<AppState>,
    user: CurrentAccount,
    Path(id): Path<Uuid>,
) -> Result<axum::response::Response, HttpError> {
    use axum::response::IntoResponse;
    use futures::{SinkExt, StreamExt};

    let folder = node(&state, &user, id).await?;
    if folder.is_file {
        return Err(HttpError::bad_request("not_a_folder", "a file has no tree"));
    }
    // Bounded: a slow client slows the query down, never fills memory.
    let (mut lines, body) = futures::channel::mpsc::channel::<axum::body::Bytes>(64);
    let pool = state.pool.clone();
    tokio::spawn(async move {
        let mut send = async |line: api::TreeLine| -> bool {
            let mut bytes = serde_json::to_vec(&line).expect("serializable");
            bytes.push(b'\n');
            matches!(
                tokio::time::timeout(TREE_STALL, lines.send(bytes.into())).await,
                Ok(Ok(()))
            )
        };
        let mut rows = std::pin::pin!(db::node::tree(&pool, folder.node));
        let mut count = 0;
        let end = loop {
            match rows.next().await {
                Some(Ok(node)) => {
                    count += 1;
                    // Gone, or stalled: the query stops here.
                    if !send(api::TreeLine::Node(node)).await {
                        return;
                    }
                }
                Some(Err(e)) => break api::TreeLine::Error(HttpError::internal(e).into_parts().1),
                None => break api::TreeLine::End { count },
            }
        };
        send(end).await;
    });
    Ok((
        [(axum::http::header::CONTENT_TYPE, "application/x-ndjson")],
        axum::body::Body::from_stream(body.map(Ok::<_, std::convert::Infallible>)),
    )
        .into_response())
}

/// The most nodes one batch takes.
const NODE_BATCH: usize = 1000;

fn batch_size(len: usize) -> Result<(), HttpError> {
    if len > NODE_BATCH {
        return Err(HttpError::bad_request(
            "batch_too_large",
            format!("at most {NODE_BATCH} nodes per batch"),
        ));
    }
    Ok(())
}

/// One item's result as its outcome's parts: the value, or the error and the
/// status a call of its own would have answered.
fn outcome<T>(result: Result<T, HttpError>) -> (Option<T>, Option<api::ApiError>, Option<u16>) {
    match result {
        Ok(value) => (Some(value), None, None),
        Err(e) => {
            let (status, error) = e.into_parts();
            (None, Some(error), Some(status.as_u16()))
        }
    }
}

fn node_outcome(result: Result<Option<api::NodeInfo>, HttpError>) -> api::NodeOutcome {
    let (node, error, status) = outcome(result);
    api::NodeOutcome {
        node: node.flatten(),
        error,
        status,
    }
}

/// Rename and/or move files and folders, each in its own transaction, each
/// its own outcome. Within a library a move is one row; to another library of
/// the same organization, the subtree (and its history) follows.
#[utoipa::path(patch, path = "/api/v1/nodes", request_body = api::UpdateNodes, responses(
    (status = 200, description = "Each node's outcome, in order: errors `root`, `not_found`, `name_taken`, `move_into_itself`, `other_organization`", body = Vec<api::NodeOutcome>),
    (status = 400, description = "`batch_too_large`", body = api::ApiError),
))]
pub async fn update(
    State(state): State<AppState>,
    user: CurrentAccount,
    ApiJson(req): ApiJson<api::UpdateNodes>,
) -> Result<Json<Vec<api::NodeOutcome>>, HttpError> {
    batch_size(req.nodes.len())?;
    let mut outcomes = Vec::with_capacity(req.nodes.len());
    for change in &req.nodes {
        outcomes.push(node_outcome(
            update_one(&state, &user, change).await.map(Some),
        ));
    }
    Ok(Json(outcomes))
}

async fn update_one(
    state: &AppState,
    user: &CurrentAccount,
    change: &api::UpdateNode,
) -> Result<api::NodeInfo, HttpError> {
    let node = node(state, user, change.id).await?;
    let parent = match change.parent {
        Some(parent) => Some(super::nodes::node(state, user, parent).await?),
        None => None,
    };
    let name = change.name.as_ref().map(|n| n.as_str());
    let now = files::rename_move(state, &node, name, parent.as_ref(), change.replace).await?;
    Ok(db::node::info(&state.pool, now).await?)
}

/// Put files and folders in the trash, or (`permanent`, from the trash)
/// delete them for good with their subtrees and histories; each in its own
/// transaction, each its own outcome.
#[utoipa::path(post, path = "/api/v1/nodes/trash", request_body = api::TrashNodes, responses(
    (status = 200, description = "Each node's outcome, in order (no `node`): errors `root`, `not_found`", body = Vec<api::NodeOutcome>),
    (status = 400, description = "`batch_too_large`", body = api::ApiError),
))]
pub async fn trash_nodes(
    State(state): State<AppState>,
    user: CurrentAccount,
    ApiJson(req): ApiJson<api::TrashNodes>,
) -> Result<Json<Vec<api::NodeOutcome>>, HttpError> {
    batch_size(req.nodes.len())?;
    let mut outcomes = Vec::with_capacity(req.nodes.len());
    for id in &req.nodes {
        let result = trash_one(&state, &user, *id, req.permanent).await;
        outcomes.push(node_outcome(result.map(|()| None)));
    }
    Ok(Json(outcomes))
}

async fn trash_one(
    state: &AppState,
    user: &CurrentAccount,
    id: Uuid,
    permanent: bool,
) -> Result<(), HttpError> {
    if permanent {
        let node = db::node::trashed_access(&state.pool, user, id)
            .await?
            .ok_or_else(|| HttpError::not_found("trashed node"))?;
        require_write(&node.access)?;
        let mut tx = state.pool.begin().await?;
        db::node::purge(&mut tx, node.node).await?;
        tx.commit().await?;
    } else {
        let node = node(state, user, id).await?;
        files::trash(state, &node).await?;
    }
    Ok(())
}

/// Copy files and folders into folders of any library of the same
/// organization, each in its own transaction, each its own outcome. Files
/// keep their current version only, sharing its chunks.
#[utoipa::path(post, path = "/api/v1/nodes/copy", request_body = api::CopyNodes, responses(
    (status = 200, description = "Each copy's outcome, in order (`node`: the copy): errors `not_found`, `name_taken`, `other_organization`", body = Vec<api::NodeOutcome>),
    (status = 400, description = "`batch_too_large`", body = api::ApiError),
))]
pub async fn copy(
    State(state): State<AppState>,
    user: CurrentAccount,
    ApiJson(req): ApiJson<api::CopyNodes>,
) -> Result<Json<Vec<api::NodeOutcome>>, HttpError> {
    batch_size(req.nodes.len())?;
    let mut outcomes = Vec::with_capacity(req.nodes.len());
    for copy in &req.nodes {
        outcomes.push(node_outcome(copy_one(&state, &user, copy).await.map(Some)));
    }
    Ok(Json(outcomes))
}

async fn copy_one(
    state: &AppState,
    user: &CurrentAccount,
    copy: &api::CopyNode,
) -> Result<api::NodeInfo, HttpError> {
    let source = node(state, user, copy.id).await?;
    let dest = node(state, user, copy.parent).await?;
    let name = copy.name.as_ref().map(|n| n.as_str());
    let made = files::copy(state, &source, &dest, name, copy.replace).await?;
    Ok(db::node::info(&state.pool, made).await?)
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

/// Take files and folders out of the trash, each in its own transaction,
/// each its own outcome: back in their folder, or into the library's root if
/// that one is trashed too; renamed if their name is taken.
#[utoipa::path(post, path = "/api/v1/nodes/restore", request_body = api::RestoreNodes, responses(
    (status = 200, description = "Each node's outcome, in order: errors `not_found`", body = Vec<api::RestoreOutcome>),
    (status = 400, description = "`batch_too_large`", body = api::ApiError),
))]
pub async fn restore(
    State(state): State<AppState>,
    user: CurrentAccount,
    ApiJson(req): ApiJson<api::RestoreNodes>,
) -> Result<Json<Vec<api::RestoreOutcome>>, HttpError> {
    batch_size(req.nodes.len())?;
    let mut outcomes = Vec::with_capacity(req.nodes.len());
    for id in &req.nodes {
        let (restored, error, status) = outcome(restore_one(&state, &user, *id).await);
        outcomes.push(api::RestoreOutcome {
            restored,
            error,
            status,
        });
    }
    Ok(Json(outcomes))
}

async fn restore_one(
    state: &AppState,
    user: &CurrentAccount,
    id: Uuid,
) -> Result<api::Restored, HttpError> {
    let node = db::node::trashed_access(&state.pool, user, id)
        .await?
        .ok_or_else(|| HttpError::not_found("trashed node"))?;
    require_write(&node.access)?;
    let mut tx = state.pool.begin().await?;
    let restored = db::node::restore(&mut tx, &node).await?;
    tx.commit().await?;
    Ok(restored)
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
