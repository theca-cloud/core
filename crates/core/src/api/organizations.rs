//! An organization's members: the owner adds (an existing account joins at
//! once, another address is invited) and removes them.

use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::routing::{delete, get};
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
        .route("/organizations/{id}/members", get(members).post(add))
        .route("/organizations/{id}/members/{user}", delete(remove))
}

/// The owner, from a full session.
async fn owned(state: &AppState, account: &CurrentAccount, id: Uuid) -> Result<Owned, HttpError> {
    account.require_full_session()?;
    db::organization::owned(&state.pool, account, id)
        .await?
        .ok_or_else(|| HttpError::not_found("organization"))
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
        Added::Invited(_) if state.settings.smtp.is_none() => return Err(email_unavailable()),
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
