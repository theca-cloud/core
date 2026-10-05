//! App passwords: credentials for the CLI, API clients and WebDAV, limited to
//! data (no login, no account actions), optionally to an organization or a
//! library, optionally read-only. Managed from a session only.

use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::routing::{delete, post};
use axum::{Json, Router};
use theca_common::api;
use uuid::Uuid;

use super::AppState;
use super::auth::CurrentAccount;
use super::error::{ApiJson, HttpError};
use crate::db;

pub fn router() -> Router<AppState> {
    Router::new()
        .route("/auth/app-passwords", post(create).get(list))
        .route("/auth/app-passwords/{id}", delete(revoke))
}

/// App passwords can't manage app passwords: a leaked one can't mint more.
fn require_session(account: &CurrentAccount) -> Result<(), HttpError> {
    if account.session {
        Ok(())
    } else {
        Err(
            HttpError::forbidden("app passwords are managed from a session")
                .into_code("session_required"),
        )
    }
}

/// Create an app password, within what this session reaches (a restricted
/// session: inside its organization). The password is shown only once.
#[utoipa::path(post, path = "/api/v1/auth/app-passwords", request_body = api::CreateAppPassword, responses(
    (status = 201, body = api::NewAppPassword),
    (status = 400, description = "`invalid_name`", body = api::ApiError),
    (status = 403, description = "`session_required`", body = api::ApiError),
    (status = 404, description = "Organization or library not reachable", body = api::ApiError),
))]
pub async fn create(
    State(state): State<AppState>,
    account: CurrentAccount,
    ApiJson(req): ApiJson<api::CreateAppPassword>,
) -> Result<(StatusCode, Json<api::NewAppPassword>), HttpError> {
    require_session(&account)?;
    let name = req.name.trim();
    if name.is_empty() || name.chars().count() > 200 {
        return Err(HttpError::bad_request(
            "invalid_name",
            "the name must have 1 to 200 characters",
        ));
    }
    let scope = db::account::resolve_scope(&state.pool, &account, req.organization, req.library)
        .await?
        .ok_or_else(|| {
            HttpError::not_found(if req.library.is_some() {
                "library"
            } else {
                "organization"
            })
        })?;
    let (token, app_password) =
        db::account::create_app_password(&state.pool, &account, name, scope, req.read_only).await?;
    Ok((
        StatusCode::CREATED,
        Json(api::NewAppPassword {
            password: token.token,
            app_password,
        }),
    ))
}

/// The account's app passwords.
#[utoipa::path(get, path = "/api/v1/auth/app-passwords", responses(
    (status = 200, body = Vec<api::AppPassword>),
    (status = 403, description = "`session_required`", body = api::ApiError),
))]
pub async fn list(
    State(state): State<AppState>,
    account: CurrentAccount,
) -> Result<Json<Vec<api::AppPassword>>, HttpError> {
    require_session(&account)?;
    Ok(Json(
        db::account::app_passwords(&state.pool, &account).await?,
    ))
}

/// Revoke an app password: refused at once.
#[utoipa::path(delete, path = "/api/v1/auth/app-passwords/{id}", params(("id" = Uuid, Path)), responses(
    (status = 204),
    (status = 403, description = "`session_required`", body = api::ApiError),
    (status = 404, body = api::ApiError),
))]
pub async fn revoke(
    State(state): State<AppState>,
    account: CurrentAccount,
    Path(id): Path<Uuid>,
) -> Result<StatusCode, HttpError> {
    require_session(&account)?;
    if db::account::revoke_app_password(&state.pool, &account, id).await? {
        Ok(StatusCode::NO_CONTENT)
    } else {
        Err(HttpError::not_found("app password"))
    }
}
