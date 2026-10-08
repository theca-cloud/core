//! What an organization's owner manages, from a full session: its libraries'
//! members and its dedup namespaces (decisions.md, Authorization and Trust
//! boundary).

use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::routing::{get, patch, put};
use axum::{Json, Router};
use theca_common::api;
use uuid::Uuid;

use super::AppState;
use super::auth::CurrentAccount;
use super::error::{ApiJson, HttpError};
use super::organizations::owned;
use crate::db;
use crate::db::sharing::Managed;

pub fn router() -> Router<AppState> {
    Router::new()
        .route("/organizations/{id}/libraries", get(libraries))
        .route("/libraries/{id}/members", get(members))
        .route(
            "/libraries/{id}/members/{user}",
            put(set_member).delete(remove_member),
        )
        .route(
            "/organizations/{id}/namespaces",
            get(namespaces).post(create_namespace),
        )
        .route(
            "/namespaces/{id}",
            patch(rename_namespace).delete(delete_namespace),
        )
        .route(
            "/namespaces/{id}/libraries/{library}",
            put(add_library).delete(remove_library),
        )
}

async fn managed_library(
    state: &AppState,
    account: &CurrentAccount,
    id: Uuid,
) -> Result<Managed, HttpError> {
    account.require_full_session()?;
    db::sharing::library(&state.pool, account, id)
        .await?
        .ok_or_else(|| HttpError::not_found("library"))
}

async fn managed_namespace(
    state: &AppState,
    account: &CurrentAccount,
    id: Uuid,
) -> Result<Managed, HttpError> {
    account.require_full_session()?;
    db::sharing::namespace(&state.pool, account, id)
        .await?
        .ok_or_else(|| HttpError::not_found("namespace"))
}

fn namespace_name(name: &str) -> Result<&str, HttpError> {
    let name = name.trim();
    if name.is_empty() {
        return Err(HttpError::bad_request(
            "invalid_name",
            "the namespace needs a name",
        ));
    }
    Ok(name)
}

/// Every library of the organization, whether the owner is a member or not
/// (the owner).
#[utoipa::path(get, path = "/api/v1/organizations/{id}/libraries", params(("id" = Uuid, Path)), responses(
    (status = 200, body = Vec<api::OrganizationLibrary>),
    (status = 404, body = api::ApiError),
))]
pub async fn libraries(
    State(state): State<AppState>,
    account: CurrentAccount,
    Path(id): Path<Uuid>,
) -> Result<Json<Vec<api::OrganizationLibrary>>, HttpError> {
    let organization = owned(&state, &account, id).await?;
    Ok(Json(
        db::sharing::libraries(&state.pool, organization.pk).await?,
    ))
}

/// A library's members (its members, and the organization's owner).
#[utoipa::path(get, path = "/api/v1/libraries/{id}/members", operation_id = "library_members", params(("id" = Uuid, Path)), responses(
    (status = 200, body = Vec<api::LibraryMember>),
    (status = 404, body = api::ApiError),
))]
pub async fn members(
    State(state): State<AppState>,
    account: CurrentAccount,
    Path(id): Path<Uuid>,
) -> Result<Json<Vec<api::LibraryMember>>, HttpError> {
    let library = match db::library::access(&state.pool, &account, id).await? {
        Some(access) => access.library,
        None => managed_library(&state, &account, id).await?.pk,
    };
    Ok(Json(db::sharing::members(&state.pool, library).await?))
}

/// Give a member of the organization access to the library, or change its
/// role (the owner). Invited members not joined yet can be added: their
/// access is ready when they accept.
#[utoipa::path(put, path = "/api/v1/libraries/{id}/members/{user}", params(("id" = Uuid, Path), ("user" = Uuid, Path)), request_body = api::SetRole, responses(
    (status = 200, body = Vec<api::LibraryMember>),
    (status = 400, description = "`invalid_role`", body = api::ApiError),
    (status = 404, description = "No such library, or no such member of its organization", body = api::ApiError),
))]
pub async fn set_member(
    State(state): State<AppState>,
    account: CurrentAccount,
    Path((id, user)): Path<(Uuid, Uuid)>,
    ApiJson(request): ApiJson<api::SetRole>,
) -> Result<Json<Vec<api::LibraryMember>>, HttpError> {
    let library = managed_library(&state, &account, id).await?;
    let write = match request.role {
        api::Role::Read => false,
        api::Role::Write => true,
        api::Role::Unknown => {
            return Err(HttpError::bad_request(
                "invalid_role",
                "the role is read or write",
            ));
        }
    };
    if !db::sharing::set_member(&state.pool, library, user, write).await? {
        return Err(HttpError::not_found("member"));
    }
    Ok(Json(db::sharing::members(&state.pool, library.pk).await?))
}

/// Take a member's access to the library away (the owner). A library may be
/// left without members: nobody reaches it until it's given some.
#[utoipa::path(delete, path = "/api/v1/libraries/{id}/members/{user}", params(("id" = Uuid, Path), ("user" = Uuid, Path)), responses(
    (status = 204),
    (status = 404, body = api::ApiError),
))]
pub async fn remove_member(
    State(state): State<AppState>,
    account: CurrentAccount,
    Path((id, user)): Path<(Uuid, Uuid)>,
) -> Result<StatusCode, HttpError> {
    let library = managed_library(&state, &account, id).await?;
    if !db::sharing::remove_member(&state.pool, library.pk, user).await? {
        return Err(HttpError::not_found("member"));
    }
    Ok(StatusCode::NO_CONTENT)
}

/// The organization's dedup namespaces and their libraries (the owner).
#[utoipa::path(get, path = "/api/v1/organizations/{id}/namespaces", params(("id" = Uuid, Path)), responses(
    (status = 200, body = Vec<api::Namespace>),
    (status = 404, body = api::ApiError),
))]
pub async fn namespaces(
    State(state): State<AppState>,
    account: CurrentAccount,
    Path(id): Path<Uuid>,
) -> Result<Json<Vec<api::Namespace>>, HttpError> {
    let organization = owned(&state, &account, id).await?;
    Ok(Json(
        db::sharing::namespaces(&state.pool, organization.pk).await?,
    ))
}

/// Create a dedup namespace, empty (the owner).
#[utoipa::path(post, path = "/api/v1/organizations/{id}/namespaces", params(("id" = Uuid, Path)), request_body = api::NamespaceName, responses(
    (status = 201, body = api::Namespace),
    (status = 400, description = "`invalid_name`", body = api::ApiError),
    (status = 404, body = api::ApiError),
    (status = 409, description = "`namespace_name_taken`", body = api::ApiError),
))]
pub async fn create_namespace(
    State(state): State<AppState>,
    account: CurrentAccount,
    Path(id): Path<Uuid>,
    ApiJson(request): ApiJson<api::NamespaceName>,
) -> Result<(StatusCode, Json<api::Namespace>), HttpError> {
    let organization = owned(&state, &account, id).await?;
    let name = namespace_name(&request.name)?;
    let pk = db::sharing::create_namespace(&state.pool, organization.pk, name).await?;
    Ok((
        StatusCode::CREATED,
        Json(db::sharing::get_namespace(&state.pool, pk).await?),
    ))
}

/// Rename a namespace (the owner).
#[utoipa::path(patch, path = "/api/v1/namespaces/{id}", params(("id" = Uuid, Path)), request_body = api::NamespaceName, responses(
    (status = 200, body = api::Namespace),
    (status = 404, body = api::ApiError),
    (status = 409, description = "`namespace_name_taken`", body = api::ApiError),
))]
pub async fn rename_namespace(
    State(state): State<AppState>,
    account: CurrentAccount,
    Path(id): Path<Uuid>,
    ApiJson(request): ApiJson<api::NamespaceName>,
) -> Result<Json<api::Namespace>, HttpError> {
    let namespace = managed_namespace(&state, &account, id).await?;
    let name = namespace_name(&request.name)?;
    db::sharing::rename_namespace(&state.pool, namespace.pk, name).await?;
    Ok(Json(
        db::sharing::get_namespace(&state.pool, namespace.pk).await?,
    ))
}

/// Delete a namespace (the owner): its libraries stop deduplicating with each
/// other at once, and keep every chunk they already share.
#[utoipa::path(delete, path = "/api/v1/namespaces/{id}", params(("id" = Uuid, Path)), responses(
    (status = 204),
    (status = 404, body = api::ApiError),
))]
pub async fn delete_namespace(
    State(state): State<AppState>,
    account: CurrentAccount,
    Path(id): Path<Uuid>,
) -> Result<StatusCode, HttpError> {
    let namespace = managed_namespace(&state, &account, id).await?;
    db::sharing::delete_namespace(&state.pool, namespace.pk).await?;
    Ok(StatusCode::NO_CONTENT)
}

/// Put one of the organization's libraries in the namespace (the owner):
/// from now on, it dedups with the namespace's other libraries, both ways.
/// Every member of each can then probe the others' content: a security
/// decision (decisions.md, Trust boundary).
#[utoipa::path(put, path = "/api/v1/namespaces/{id}/libraries/{library}", params(("id" = Uuid, Path), ("library" = Uuid, Path)), responses(
    (status = 200, body = api::Namespace),
    (status = 404, body = api::ApiError),
))]
pub async fn add_library(
    State(state): State<AppState>,
    account: CurrentAccount,
    Path((id, library)): Path<(Uuid, Uuid)>,
) -> Result<Json<api::Namespace>, HttpError> {
    let namespace = managed_namespace(&state, &account, id).await?;
    if !db::sharing::add_library(&state.pool, namespace, library).await? {
        return Err(HttpError::not_found("library"));
    }
    Ok(Json(
        db::sharing::get_namespace(&state.pool, namespace.pk).await?,
    ))
}

/// Take a library out of the namespace (the owner): the trust is revoked at
/// once; the library keeps the chunks it already shares.
#[utoipa::path(delete, path = "/api/v1/namespaces/{id}/libraries/{library}", params(("id" = Uuid, Path), ("library" = Uuid, Path)), responses(
    (status = 200, body = api::Namespace),
    (status = 404, body = api::ApiError),
))]
pub async fn remove_library(
    State(state): State<AppState>,
    account: CurrentAccount,
    Path((id, library)): Path<(Uuid, Uuid)>,
) -> Result<Json<api::Namespace>, HttpError> {
    let namespace = managed_namespace(&state, &account, id).await?;
    if !db::sharing::remove_library(&state.pool, namespace.pk, library).await? {
        return Err(HttpError::not_found("library"));
    }
    Ok(Json(
        db::sharing::get_namespace(&state.pool, namespace.pk).await?,
    ))
}
