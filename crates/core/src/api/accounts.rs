//! Accounts by email: registration, verification, password reset, and
//! accepting invitations. Answers never say whether an address has an account.

use axum::extract::State;
use axum::http::{HeaderMap, StatusCode};
use axum::routing::post;
use axum::{Json, Router};
use theca_common::api;

use super::AppState;
use super::auth::{ClientIp, UserAgent, set_session_cookie};
use super::error::{ApiJson, HttpError};
use crate::auth::hash_password;
use crate::db;
use crate::email::{self, Email};
use crate::limits::Limit;

pub fn router() -> Router<AppState> {
    Router::new()
        .route("/auth/register", post(register))
        .route("/auth/verify", post(verify))
        .route("/auth/verify/resend", post(resend))
        .route("/auth/password/forgot", post(forgot))
        .route("/auth/password/reset", post(reset))
        .route("/invitations/accept", post(accept))
}

/// The password rules: at least 8 characters.
pub(super) fn check_password(password: &str) -> Result<(), HttpError> {
    if password.chars().count() < 8 {
        return Err(HttpError::bad_request(
            "weak_password",
            "a password needs at least 8 characters",
        ));
    }
    Ok(())
}

/// Emails asked for an address, and from an address: limited (spam, and
/// someone's inbox flooded).
fn email_limits(state: &AppState, ip: ClientIp, email: &str) -> Result<(), HttpError> {
    let settings = state.settings.get();
    state
        .limits
        .hit(&settings, Limit::EmailAddress, email)
        .map_err(HttpError::too_many)?;
    if let Some(ip) = ip.0 {
        state
            .limits
            .hit(&settings, Limit::EmailIp, &ip.to_string())
            .map_err(HttpError::too_many)?;
    }
    Ok(())
}

fn check_email(email: &str) -> Result<&str, HttpError> {
    let email = email.trim();
    if email.len() < 3 || !email.contains('@') || email.contains(char::is_whitespace) {
        return Err(HttpError::bad_request(
            "invalid_email",
            "invalid email address",
        ));
    }
    Ok(email)
}

fn invalid_token() -> HttpError {
    HttpError::bad_request(
        "invalid_token",
        "this link is invalid, used or expired: ask for a new one",
    )
}

/// Logs an account in: a session cookie and `Me`.
async fn log_in(
    state: &AppState,
    pk: i64,
    account: uuid::Uuid,
    ip: ClientIp,
    agent: UserAgent,
) -> Result<(HeaderMap, Json<api::Me>), HttpError> {
    let (token, _) =
        db::account::create_session(&state.pool, account, None, agent.0.as_deref(), ip.0).await?;
    let mut headers = HeaderMap::new();
    set_session_cookie(&mut headers, &token.token);
    Ok((headers, Json(db::account::me(&state.pool, pk).await?)))
}

/// Create an account (when registration is open): it gets a personal
/// organization, and logs in once its address is confirmed by email. Always
/// `202`: a taken address gets an email saying so instead.
#[utoipa::path(post, path = "/api/v1/auth/register", request_body = api::Register, responses(
    (status = 202),
    (status = 400, description = "`invalid_email`, `weak_password`", body = api::ApiError),
    (status = 403, description = "`registration_closed`", body = api::ApiError),
    (status = 503, description = "`email_unavailable`", body = api::ApiError),
))]
pub async fn register(
    State(state): State<AppState>,
    ip: ClientIp,
    ApiJson(req): ApiJson<api::Register>,
) -> Result<StatusCode, HttpError> {
    if !state.settings.get().registration_open {
        return Err(HttpError::forbidden("registration is closed").into_code("registration_closed"));
    }
    if state.settings.get().smtp.is_none() {
        return Err(email_unavailable());
    }
    let email = check_email(&req.email)?;
    email_limits(&state, ip, email)?;
    check_password(&req.password)?;
    // Hashed whether or not the address is free: same time either way.
    let hash = hash_password(&req.password);
    let mut tx = state.pool.begin().await?;
    match db::account::register(&mut tx, email, &hash).await? {
        Some(account) => email::enqueue(&mut tx, email, &Email::Verification { account }).await?,
        None => email::enqueue(&mut tx, email, &Email::RegistrationAttempt).await?,
    }
    tx.commit().await?;
    Ok(StatusCode::ACCEPTED)
}

pub(super) fn email_unavailable() -> HttpError {
    HttpError::new(
        StatusCode::SERVICE_UNAVAILABLE,
        "email_unavailable",
        "this instance can't send emails (no SMTP configured)",
    )
}

/// Confirm an email address (the link's token): the account can log in, and is.
#[utoipa::path(post, path = "/api/v1/auth/verify", request_body = api::LinkToken, responses(
    (status = 200, body = api::Me),
    (status = 400, description = "`invalid_token`", body = api::ApiError),
))]
pub async fn verify(
    State(state): State<AppState>,
    ip: ClientIp,
    agent: UserAgent,
    ApiJson(req): ApiJson<api::LinkToken>,
) -> Result<(HeaderMap, Json<api::Me>), HttpError> {
    let mut tx = state.pool.begin().await?;
    let account = db::account::consume_one_off(
        &mut tx,
        "email_verification",
        &req.token,
        email::VERIFICATION_LIFETIME,
    )
    .await?
    .ok_or_else(invalid_token)?;
    let pk = db::account::set_verified(&mut tx, account)
        .await?
        .ok_or_else(invalid_token)?;
    tx.commit().await?;
    log_in(&state, pk, account, ip, agent).await
}

/// Send the confirmation email again (unverified accounts). Always `202`.
#[utoipa::path(post, path = "/api/v1/auth/verify/resend", request_body = api::EmailAddress, responses((status = 202)))]
pub async fn resend(
    State(state): State<AppState>,
    ip: ClientIp,
    ApiJson(req): ApiJson<api::EmailAddress>,
) -> Result<StatusCode, HttpError> {
    email_limits(&state, ip, req.email.trim())?;
    if let Some((account, false, true)) =
        db::account::by_email(&state.pool, req.email.trim()).await?
    {
        let mut tx = state.pool.begin().await?;
        email::enqueue(&mut tx, req.email.trim(), &Email::Verification { account }).await?;
        tx.commit().await?;
    }
    Ok(StatusCode::ACCEPTED)
}

/// Email a link to choose a new password. Always `202`.
#[utoipa::path(post, path = "/api/v1/auth/password/forgot", request_body = api::EmailAddress, responses((status = 202)))]
pub async fn forgot(
    State(state): State<AppState>,
    ip: ClientIp,
    ApiJson(req): ApiJson<api::EmailAddress>,
) -> Result<StatusCode, HttpError> {
    email_limits(&state, ip, req.email.trim())?;
    if let Some((account, _, _)) = db::account::by_email(&state.pool, req.email.trim()).await? {
        let mut tx = state.pool.begin().await?;
        email::enqueue(&mut tx, req.email.trim(), &Email::PasswordReset { account }).await?;
        tx.commit().await?;
    }
    Ok(StatusCode::ACCEPTED)
}

/// Choose a new password (the link's token). Also confirms the address, and
/// logs out every session. TOTP, if on, stays required.
#[utoipa::path(post, path = "/api/v1/auth/password/reset", request_body = api::SetPassword, responses(
    (status = 204),
    (status = 400, description = "`invalid_token`, `weak_password`", body = api::ApiError),
))]
pub async fn reset(
    State(state): State<AppState>,
    ApiJson(req): ApiJson<api::SetPassword>,
) -> Result<StatusCode, HttpError> {
    let password = req.password.as_deref().unwrap_or_default();
    check_password(password)?;
    let hash = hash_password(password);
    let mut tx = state.pool.begin().await?;
    let account =
        db::account::consume_one_off(&mut tx, "password_reset", &req.token, email::RESET_LIFETIME)
            .await?
            .ok_or_else(invalid_token)?;
    let pk = db::account::set_verified(&mut tx, account)
        .await?
        .ok_or_else(invalid_token)?;
    db::account::set_password(&mut tx, pk, &hash).await?;
    db::account::revoke_sessions(&mut tx, account, "password_reset").await?;
    tx.commit().await?;
    Ok(StatusCode::NO_CONTENT)
}

/// Accept an invitation (the link's token): joins the organization. A new
/// account chooses its password here, and is logged in; an account that can
/// already log in just joins (no login: its own password and TOTP apply).
#[utoipa::path(post, path = "/api/v1/invitations/accept", request_body = api::SetPassword, responses(
    (status = 200, description = "Joined; `Me` if logged in", body = Option<api::Me>),
    (status = 400, description = "`invalid_token`, `weak_password`", body = api::ApiError),
))]
pub async fn accept(
    State(state): State<AppState>,
    ip: ClientIp,
    agent: UserAgent,
    ApiJson(req): ApiJson<api::SetPassword>,
) -> Result<(HeaderMap, Json<Option<api::Me>>), HttpError> {
    let hash = req.password.as_deref().map(hash_password);
    let mut tx = state.pool.begin().await?;
    let user = db::account::consume_one_off(
        &mut tx,
        "invitation",
        &req.token,
        email::INVITATION_LIFETIME,
    )
    .await?
    .ok_or_else(invalid_token)?;
    let invited = sqlx::query!(
        r#"SELECT u.pk, a.pk AS account, a.uuid AS account_uuid,
                  EXISTS (SELECT 1 FROM identity WHERE account = a.pk) AS "can_log_in!"
           FROM "user" u JOIN account a ON a.pk = u.account WHERE u.uuid = $1"#,
        user
    )
    .fetch_optional(&mut *tx)
    .await?
    .ok_or_else(invalid_token)?; // the invitation was cancelled
    let new_password = !invited.can_log_in;
    if new_password {
        let password = req.password.as_deref().unwrap_or_default();
        check_password(password)?;
        db::account::set_password(&mut tx, invited.account, hash.as_deref().expect("checked"))
            .await?;
    }
    db::account::set_verified(&mut tx, invited.account_uuid).await?;
    sqlx::query!(
        r#"UPDATE "user" SET joined_at = now(), invited_at = NULL WHERE pk = $1"#,
        invited.pk
    )
    .execute(&mut *tx)
    .await?;
    tx.commit().await?;
    if new_password {
        let (headers, Json(me)) =
            log_in(&state, invited.account, invited.account_uuid, ip, agent).await?;
        Ok((headers, Json(Some(me))))
    } else {
        Ok((HeaderMap::new(), Json(None)))
    }
}
