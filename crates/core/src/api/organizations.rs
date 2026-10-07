//! Organizations: any account creates company ones (unless the setting
//! `organization_creation` keeps it to admins); their owner renames them and
//! manages their members (an existing account joins at once, another address
//! is invited).

use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::routing::{delete, get, post};
use axum::{Json, Router};
use theca_common::api;
use uuid::Uuid;

use super::AppState;
use super::accounts::email_unavailable;
use super::auth::CurrentAccount;
use super::error::{ApiJson, HttpError};
use crate::db;
use crate::db::organization::{Added, Owned, RemoveError};

pub fn router() -> Router<AppState> {
    Router::new()
        .route("/organizations", post(create))
        .route("/organizations/{id}/members", get(members).post(add))
        .route("/organizations/{id}/members/{user}", delete(remove))
        .route(
            "/organizations/{id}",
            get(organization).patch(rename).delete(delete_organization),
        )
        .route("/organizations/{id}/owner", axum::routing::post(transfer))
        .route(
            "/organizations/{id}/identity-providers",
            get(providers).post(create_provider),
        )
        .route(
            "/organizations/{id}/identity-providers/{provider}",
            axum::routing::patch(update_provider).delete(delete_provider),
        )
}

/// The owner, from a full session.
pub(super) async fn owned(
    state: &AppState,
    account: &CurrentAccount,
    id: Uuid,
) -> Result<Owned, HttpError> {
    account.require_full_session()?;
    db::organization::owned(&state.pool, account, id)
        .await?
        .ok_or_else(|| HttpError::not_found("organization"))
}

/// An organization's name: trimmed, not empty.
pub(super) fn name(name: &str) -> Result<&str, HttpError> {
    let name = name.trim();
    if name.is_empty() {
        return Err(HttpError::bad_request(
            "invalid_name",
            "the organization needs a name",
        ));
    }
    Ok(name)
}

/// Create a company organization, owned by the account (from a full session;
/// instance admins only when `organization_creation` is `admins`).
#[utoipa::path(post, path = "/api/v1/organizations", request_body = api::OrganizationName, responses(
    (status = 201, body = api::Organization),
    (status = 400, description = "`invalid_name`", body = api::ApiError),
    (status = 403, description = "`organization_creation_closed`", body = api::ApiError),
))]
pub async fn create(
    State(state): State<AppState>,
    account: CurrentAccount,
    ApiJson(request): ApiJson<api::OrganizationName>,
) -> Result<(StatusCode, Json<api::Organization>), HttpError> {
    account.require_full_session()?;
    let name = name(&request.name)?;
    if !state.settings.get().organization_creation_open {
        let admin = sqlx::query_scalar!(
            "SELECT instance_admin FROM account WHERE pk = $1",
            account.account
        )
        .fetch_one(&state.pool)
        .await?;
        if !admin {
            return Err(
                HttpError::forbidden("only instance admins create organizations")
                    .into_code("organization_creation_closed"),
            );
        }
    }
    let mut tx = state.pool.begin().await?;
    let pk = db::organization::create(&mut tx, account.account, name, false).await?;
    tx.commit().await?;
    Ok((
        StatusCode::CREATED,
        Json(db::organization::details(&state.pool, pk, account.account).await?),
    ))
}

/// An organization, with its quota and usage (any member).
#[utoipa::path(get, path = "/api/v1/organizations/{id}", params(("id" = Uuid, Path)), responses(
    (status = 200, body = api::Organization),
    (status = 404, body = api::ApiError),
))]
pub async fn organization(
    State(state): State<AppState>,
    account: CurrentAccount,
    Path(id): Path<Uuid>,
) -> Result<Json<api::Organization>, HttpError> {
    let pk = db::organization::joined(&state.pool, &account, id)
        .await?
        .ok_or_else(|| HttpError::not_found("organization"))?;
    Ok(Json(
        db::organization::details(&state.pool, pk, account.account).await?,
    ))
}

/// Rename a company organization (the owner).
#[utoipa::path(patch, path = "/api/v1/organizations/{id}", params(("id" = Uuid, Path)), request_body = api::OrganizationName, responses(
    (status = 200, body = api::Organization),
    (status = 400, description = "`invalid_name`", body = api::ApiError),
    (status = 404, body = api::ApiError),
    (status = 409, description = "`personal_organization`", body = api::ApiError),
))]
pub async fn rename(
    State(state): State<AppState>,
    account: CurrentAccount,
    Path(id): Path<Uuid>,
    ApiJson(request): ApiJson<api::OrganizationName>,
) -> Result<Json<api::Organization>, HttpError> {
    let organization = owned(&state, &account, id).await?;
    if organization.personal {
        return Err(personal());
    }
    db::organization::rename(&state.pool, organization.pk, name(&request.name)?).await?;
    Ok(Json(
        db::organization::details(&state.pool, organization.pk, account.account).await?,
    ))
}

/// The organization's members and pending invitations (any member).
#[utoipa::path(get, path = "/api/v1/organizations/{id}/members", params(("id" = Uuid, Path)), responses(
    (status = 200, body = Vec<api::Member>),
    (status = 404, body = api::ApiError),
))]
pub async fn members(
    State(state): State<AppState>,
    account: CurrentAccount,
    Path(id): Path<Uuid>,
) -> Result<Json<Vec<api::Member>>, HttpError> {
    let organization = db::organization::joined(&state.pool, &account, id)
        .await?
        .ok_or_else(|| HttpError::not_found("organization"))?;
    Ok(Json(
        db::organization::members(&state.pool, organization).await?,
    ))
}

/// Add a member by email (the owner): an existing account joins at once (and
/// is told by email); another address gets an invitation.
#[utoipa::path(post, path = "/api/v1/organizations/{id}/members", params(("id" = Uuid, Path)), request_body = api::AddMember, responses(
    (status = 201, body = api::Member),
    (status = 404, body = api::ApiError),
    (status = 409, description = "`already_member`, `personal_organization`", body = api::ApiError),
    (status = 503, description = "`email_unavailable` (an invitation needs SMTP)", body = api::ApiError),
))]
pub async fn add(
    State(state): State<AppState>,
    account: CurrentAccount,
    Path(id): Path<Uuid>,
    ApiJson(req): ApiJson<api::AddMember>,
) -> Result<(StatusCode, Json<api::Member>), HttpError> {
    let organization = owned(&state, &account, id).await?;
    if organization.personal {
        return Err(personal());
    }
    let settings = state.settings.get();
    state
        .limits
        .hit(
            &settings,
            crate::limits::Limit::Invitations,
            &organization.pk.to_string(),
        )
        .map_err(HttpError::too_many)?;
    let email = req.email.trim();
    if !email.contains('@') {
        return Err(HttpError::bad_request(
            "invalid_email",
            "invalid email address",
        ));
    }
    let inviter = db::account::me(&state.pool, account.account).await?.email;
    let mut tx = state.pool.begin().await?;
    let user = match db::organization::add(&mut tx, &organization, &inviter, email).await? {
        Added::Joined(user) => user,
        Added::Invited(_) if state.settings.get().smtp.is_none() => return Err(email_unavailable()),
        Added::Invited(user) => user,
        Added::AlreadyMember => {
            return Err(HttpError::conflict("already_member", "already a member"));
        }
    };
    tx.commit().await?;
    let member = db::organization::members(&state.pool, organization.pk)
        .await?
        .into_iter()
        .find(|m| m.id == user)
        .expect("just added");
    Ok((StatusCode::CREATED, Json(member)))
}

pub(super) fn personal() -> HttpError {
    HttpError::conflict(
        "personal_organization",
        "a personal organization has no other member",
    )
}

/// Remove a member, or cancel an invitation (the owner; never the owner).
#[utoipa::path(delete, path = "/api/v1/organizations/{id}/members/{user}", params(("id" = Uuid, Path), ("user" = Uuid, Path)), responses(
    (status = 204),
    (status = 404, body = api::ApiError),
    (status = 409, description = "`owner`", body = api::ApiError),
))]
pub async fn remove(
    State(state): State<AppState>,
    account: CurrentAccount,
    Path((id, user)): Path<(Uuid, Uuid)>,
) -> Result<StatusCode, HttpError> {
    let organization = owned(&state, &account, id).await?;
    let mut tx = state.pool.begin().await?;
    match db::organization::remove(&mut tx, organization.pk, user).await {
        Ok(()) => {}
        Err(RemoveError::NotFound) => return Err(HttpError::not_found("member")),
        Err(RemoveError::Owner) => {
            return Err(HttpError::conflict("owner", "the owner can't be removed"));
        }
        Err(RemoveError::Db(e)) => return Err(e.into()),
    }
    tx.commit().await?;
    Ok(StatusCode::NO_CONTENT)
}

/// The organization's identity providers (the owner).
#[utoipa::path(get, path = "/api/v1/organizations/{id}/identity-providers", params(("id" = Uuid, Path)), responses(
    (status = 200, body = Vec<api::ProviderInfo>),
    (status = 404, body = api::ApiError),
))]
pub async fn providers(
    State(state): State<AppState>,
    account: CurrentAccount,
    Path(id): Path<Uuid>,
) -> Result<Json<Vec<api::ProviderInfo>>, HttpError> {
    let organization = owned(&state, &account, id).await?;
    Ok(Json(
        db::admin::providers(&state.pool, Some(organization.pk)).await?,
    ))
}

/// Add an identity provider for the organization (the owner): its logins open
/// sessions restricted to the organization. `https` issuers on public
/// addresses only; the secret as a value (stored encrypted).
#[utoipa::path(post, path = "/api/v1/organizations/{id}/identity-providers", params(("id" = Uuid, Path)), request_body = api::ProviderRequest, responses(
    (status = 201, body = api::ProviderInfo),
    (status = 400, description = "`invalid_provider`, `issuer_refused`", body = api::ApiError),
))]
pub async fn create_provider(
    State(state): State<AppState>,
    account: CurrentAccount,
    Path(id): Path<Uuid>,
    ApiJson(request): ApiJson<api::ProviderRequest>,
) -> Result<(StatusCode, Json<api::ProviderInfo>), HttpError> {
    let organization = owned(&state, &account, id).await?;
    let created = super::providers::create(&state, Some(organization.pk), &request).await?;
    Ok((StatusCode::CREATED, Json(created)))
}

/// Change one of the organization's identity providers (the owner).
#[utoipa::path(patch, path = "/api/v1/organizations/{id}/identity-providers/{provider}", params(("id" = Uuid, Path), ("provider" = Uuid, Path)), request_body = api::ProviderRequest, responses(
    (status = 200, body = api::ProviderInfo),
    (status = 404, body = api::ApiError),
))]
pub async fn update_provider(
    State(state): State<AppState>,
    account: CurrentAccount,
    Path((id, provider)): Path<(Uuid, Uuid)>,
    ApiJson(request): ApiJson<api::ProviderRequest>,
) -> Result<Json<api::ProviderInfo>, HttpError> {
    let organization = owned(&state, &account, id).await?;
    Ok(Json(
        super::providers::update(&state, Some(organization.pk), provider, &request).await?,
    ))
}

/// Delete one of the organization's identity providers, and the identities
/// linked through it (the owner).
#[utoipa::path(delete, path = "/api/v1/organizations/{id}/identity-providers/{provider}", params(("id" = Uuid, Path), ("provider" = Uuid, Path)), responses(
    (status = 204),
    (status = 404, body = api::ApiError),
))]
pub async fn delete_provider(
    State(state): State<AppState>,
    account: CurrentAccount,
    Path((id, provider)): Path<(Uuid, Uuid)>,
) -> Result<StatusCode, HttpError> {
    let organization = owned(&state, &account, id).await?;
    super::providers::delete(&state, Some(organization.pk), provider).await
}

/// Give the organization to another joined member (the owner).
#[utoipa::path(post, path = "/api/v1/organizations/{id}/owner", params(("id" = Uuid, Path)), request_body = api::TransferOwnership, responses(
    (status = 204),
    (status = 404, description = "Not the owner, or not a joined member", body = api::ApiError),
    (status = 409, description = "`personal_organization`", body = api::ApiError),
))]
pub async fn transfer(
    State(state): State<AppState>,
    account: CurrentAccount,
    Path(id): Path<Uuid>,
    ApiJson(request): ApiJson<api::TransferOwnership>,
) -> Result<StatusCode, HttpError> {
    let organization = owned(&state, &account, id).await?;
    if organization.personal {
        return Err(personal());
    }
    let mut tx = state.pool.begin().await?;
    if !db::deletion::transfer(&mut tx, organization.pk, request.member).await? {
        return Err(HttpError::not_found("member"));
    }
    tx.commit().await?;
    Ok(StatusCode::NO_CONTENT)
}

/// Delete the organization (the owner): every library, file and version at
/// once; the stored data follows with the garbage collection, started now.
#[utoipa::path(delete, path = "/api/v1/organizations/{id}", params(("id" = Uuid, Path)), responses(
    (status = 204),
    (status = 404, body = api::ApiError),
    (status = 409, description = "`personal_organization` (deleted with the account)", body = api::ApiError),
))]
pub async fn delete_organization(
    State(state): State<AppState>,
    account: CurrentAccount,
    Path(id): Path<Uuid>,
) -> Result<StatusCode, HttpError> {
    let organization = owned(&state, &account, id).await?;
    if organization.personal {
        return Err(personal());
    }
    erase(&state, organization.pk).await
}

/// Deletes an organization and starts the GC (also for the admin API).
pub(super) async fn erase(state: &AppState, organization: i64) -> Result<StatusCode, HttpError> {
    let mut tx = state.pool.begin().await?;
    db::deletion::organization(&mut tx, organization).await?;
    tx.commit().await?;
    crate::jobs::trigger(state, crate::jobs::Job::ChunkGc).await;
    tracing::info!(organization, "organization deleted");
    Ok(StatusCode::NO_CONTENT)
}
