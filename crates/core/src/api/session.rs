//! Sessions: login, logout, the current account, and the account's sessions.

use axum::extract::{Path, State};
use axum::http::{HeaderMap, StatusCode};
use axum::routing::{delete, get, post};
use axum::{Json, Router};
use theca_common::api;
use uuid::Uuid;

use super::AppState;
use super::auth::{ClientIp, CurrentAccount, UserAgent, clear_session_cookie, set_session_cookie};
use super::error::{ApiJson, HttpError};
use crate::auth::verify_password;
use crate::db;

pub fn router() -> Router<AppState> {
    Router::new()
        .route("/auth/login", post(login))
        .route("/auth/logout", post(logout))
        .route("/auth/sessions", get(sessions))
        .route("/auth/sessions/{id}", delete(revoke))
        .route("/me", get(me))
}

/// Log in with email and password. Sets the session cookie. Unknown email and
/// wrong password give the same error, in the same time.
#[utoipa::path(post, path = "/api/v1/auth/login", request_body = api::Login, responses(
    (status = 200, body = api::Me),
    (status = 401, description = "`invalid_credentials`, `totp_required`, `invalid_totp`", body = api::ApiError),
    (status = 403, description = "`email_not_verified`", body = api::ApiError),
))]
pub async fn login(
    State(state): State<AppState>,
    ip: ClientIp,
    agent: UserAgent,
    ApiJson(req): ApiJson<api::Login>,
) -> Result<(HeaderMap, Json<api::Me>), HttpError> {
    let found = db::account::password_of(&state.pool, &req.email).await?;
    let hash = found.as_ref().and_then(|(_, _, hash, _)| hash.as_deref());
    if !verify_password(hash, &req.password) {
        let account = found.map(|(_, uuid, _, _)| uuid);
        tracing::warn!(anomaly = "login_failed", ?account, ip = ?ip.0, "failed login");
        return Err(HttpError::new(
            StatusCode::UNAUTHORIZED,
            "invalid_credentials",
            "wrong email or password",
        ));
    }
    let (pk, account, _, verified) = found.expect("a password matched");
    // Self-registered accounts log in once their address is verified (else a
    // stranger could have registered it first). Checked after the password:
    // only someone who knows it learns the account is unverified.
    if !verified {
        return Err(
            HttpError::forbidden("confirm your email address first (see your inbox)")
                .into_code("email_not_verified"),
        );
    }
    // The second factor. `totp_required` only tells someone who already knows
    // the password that a code is needed.
    if let Some(identity) = db::account::totp_of(&state.pool, pk)
        .await?
        .filter(|t| t.confirmed)
    {
        let Some(code) = req.totp.as_deref() else {
            return Err(HttpError::new(
                StatusCode::UNAUTHORIZED,
                "totp_required",
                "send the TOTP code (or a recovery code) with the password",
            ));
        };
        if !super::totp::check_code(&state, account, &identity, code, true, ip).await? {
            return Err(HttpError::new(
                StatusCode::UNAUTHORIZED,
                "invalid_totp",
                "wrong or reused code",
            ));
        }
    }
    let (token, _) =
        db::account::create_session(&state.pool, account, None, agent.0.as_deref(), ip.0).await?;
    let mut headers = HeaderMap::new();
    set_session_cookie(&mut headers, &token.token);
    Ok((headers, Json(db::account::me(&state.pool, pk).await?)))
}

/// Log out: revokes this session (at once) and clears the cookie.
#[utoipa::path(post, path = "/api/v1/auth/logout", responses((status = 204)))]
pub async fn logout(
    State(state): State<AppState>,
    account: CurrentAccount,
) -> Result<(StatusCode, HeaderMap), HttpError> {
    if !account.session {
        return Err(HttpError::forbidden("app passwords can't log out").into_code("not_a_session"));
    }
    db::account::revoke_session(
        &state.pool,
        account.account_uuid,
        account.token_uuid,
        "logout",
    )
    .await?;
    let mut headers = HeaderMap::new();
    clear_session_cookie(&mut headers);
    Ok((StatusCode::NO_CONTENT, headers))
}

/// The account's live sessions.
#[utoipa::path(get, path = "/api/v1/auth/sessions", responses((status = 200, body = Vec<api::Session>)))]
pub async fn sessions(
    State(state): State<AppState>,
    account: CurrentAccount,
) -> Result<Json<Vec<api::Session>>, HttpError> {
    account.require_full_session()?;
    Ok(Json(
        db::account::sessions(
            &state.pool,
            account.account_uuid,
            account.token_uuid,
            state.settings.session_lifetime(),
        )
        .await?,
    ))
}

/// Revoke one of the account's sessions (e.g. a lost device).
#[utoipa::path(delete, path = "/api/v1/auth/sessions/{id}", params(("id" = Uuid, Path)), responses(
    (status = 204),
    (status = 404, body = api::ApiError),
))]
pub async fn revoke(
    State(state): State<AppState>,
    account: CurrentAccount,
    Path(id): Path<Uuid>,
) -> Result<StatusCode, HttpError> {
    account.require_full_session()?;
    if db::account::revoke_session(&state.pool, account.account_uuid, id, "revoked").await? {
        Ok(StatusCode::NO_CONTENT)
    } else {
        Err(HttpError::not_found("session"))
    }
}

/// The logged-in account and its organizations.
#[utoipa::path(get, path = "/api/v1/me", responses((status = 200, body = api::Me)))]
pub async fn me(
    State(state): State<AppState>,
    account: CurrentAccount,
) -> Result<Json<api::Me>, HttpError> {
    Ok(Json(db::account::me(&state.pool, account.account).await?))
}
