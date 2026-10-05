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
use crate::limits::Limit;

pub fn router() -> Router<AppState> {
    Router::new()
        .route("/auth/login", post(login))
        .route("/auth/logout", post(logout))
        .route("/auth/sessions", get(sessions))
        .route("/auth/sessions/{id}", delete(revoke))
        .route("/me", get(me).delete(delete_me))
        .route("/me/export", get(export))
}

/// Log in with email and password. Sets the session cookie. Unknown email and
/// wrong password give the same error, in the same time.
#[utoipa::path(post, path = "/api/v1/auth/login", request_body = api::Login, responses(
    (status = 200, body = api::Me),
    (status = 401, description = "`invalid_credentials`, `totp_required`, `invalid_totp`", body = api::ApiError),
    (status = 403, description = "`email_not_verified`, `account_suspended`", body = api::ApiError),
))]
pub async fn login(
    State(state): State<AppState>,
    ip: ClientIp,
    agent: UserAgent,
    ApiJson(req): ApiJson<api::Login>,
) -> Result<(HeaderMap, Json<api::Me>), HttpError> {
    let settings = state.settings.get();
    if let Some(ip) = ip.0 {
        state
            .limits
            .hit(&settings, Limit::LoginIp, &ip.to_string())
            .map_err(HttpError::too_many)?;
    }
    state
        .limits
        .check(&settings, Limit::LoginAccount, &req.email)
        .map_err(HttpError::too_many)?;
    let found = db::account::password_of(&state.pool, &req.email).await?;
    let hash = found
        .as_ref()
        .and_then(|(_, _, hash, _, _)| hash.as_deref());
    if !verify_password(hash, &req.password) {
        let account = found.map(|(_, uuid, _, _, _)| uuid);
        tracing::warn!(anomaly = "login_failed", ?account, ip = ?ip.0, "failed login");
        state
            .limits
            .fail(&settings, Limit::LoginAccount, &req.email);
        return Err(HttpError::new(
            StatusCode::UNAUTHORIZED,
            "invalid_credentials",
            "wrong email or password",
        ));
    }
    let (pk, account, _, verified, suspended) = found.expect("a password matched");
    if suspended {
        return Err(
            HttpError::forbidden("this account is suspended").into_code("account_suspended")
        );
    }
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
            state.settings.get().session_lifetime(),
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

/// Deletes an account, if it owns no company organization and isn't the
/// last active instance admin; starts the GC for its personal data.
pub(super) async fn erase_account(state: &AppState, account: i64) -> Result<(), HttpError> {
    let owned = db::deletion::owned_organizations(&state.pool, account).await?;
    if !owned.is_empty() {
        return Err(HttpError::conflict(
            "owns_organizations",
            format!(
                "transfer or delete these organizations first: {}",
                owned.join(", ")
            ),
        ));
    }
    let mut tx = state.pool.begin().await?;
    sqlx::query!("SELECT pg_advisory_xact_lock(hashtext('theca instance admins'))")
        .execute(&mut *tx)
        .await?;
    let others = sqlx::query_scalar!(
        r#"SELECT count(*) AS "count!" FROM account
           WHERE instance_admin AND suspended_at IS NULL AND pk <> $1"#,
        account
    )
    .fetch_one(&mut *tx)
    .await?;
    let admin = sqlx::query_scalar!("SELECT instance_admin FROM account WHERE pk = $1", account)
        .fetch_optional(&mut *tx)
        .await?
        .ok_or_else(|| HttpError::not_found("account"))?;
    if admin && others == 0 {
        return Err(HttpError::conflict(
            "last_admin",
            "the instance needs at least one active admin",
        ));
    }
    db::deletion::account(&mut tx, account).await?;
    tx.commit().await?;
    crate::jobs::trigger(state, crate::jobs::Job::ChunkGc).await;
    Ok(())
}

/// Delete this account (a full session, confirmed with its password if it has
/// one and its TOTP code if on): its personal organization, memberships,
/// identities, tokens and emails, at once. Refused while it owns a company
/// organization (transfer or delete it first).
#[utoipa::path(delete, path = "/api/v1/me", request_body = api::DeleteAccount, responses(
    (status = 204),
    (status = 401, description = "`invalid_credentials`, `totp_required`, `invalid_totp`", body = api::ApiError),
    (status = 409, description = "`owns_organizations`, `last_admin`", body = api::ApiError),
))]
pub async fn delete_me(
    State(state): State<AppState>,
    account: CurrentAccount,
    ip: ClientIp,
    ApiJson(request): ApiJson<api::DeleteAccount>,
) -> Result<(StatusCode, HeaderMap), HttpError> {
    account.require_full_session()?;
    let hash = sqlx::query_scalar!(
        r#"SELECT details->>'hash' AS "hash!" FROM identity WHERE account = $1 AND kind = 'password'"#,
        account.account
    )
    .fetch_optional(&state.pool)
    .await?;
    if let Some(hash) = hash
        && !verify_password(Some(&hash), request.password.as_deref().unwrap_or_default())
    {
        return Err(HttpError::new(
            StatusCode::UNAUTHORIZED,
            "invalid_credentials",
            "wrong password",
        ));
    }
    if let Some(identity) = db::account::totp_of(&state.pool, account.account)
        .await?
        .filter(|t| t.confirmed)
    {
        let code = request.totp.as_deref().ok_or_else(|| {
            HttpError::new(
                StatusCode::UNAUTHORIZED,
                "totp_required",
                "send a TOTP code",
            )
        })?;
        if !super::totp::check_code(&state, account.account_uuid, &identity, code, true, ip).await?
        {
            return Err(HttpError::new(
                StatusCode::UNAUTHORIZED,
                "invalid_totp",
                "wrong or reused code",
            ));
        }
    }
    erase_account(&state, account.account).await?;
    tracing::info!(account = %account.account_uuid, "account deleted by itself");
    let mut headers = HeaderMap::new();
    clear_session_cookie(&mut headers);
    Ok((StatusCode::NO_CONTENT, headers))
}

/// Everything the server holds about this account, never secrets (a full
/// session). Files come through the usual paths (CLI, WebDAV).
#[utoipa::path(get, path = "/api/v1/me/export", responses((status = 200, body = api::Export)))]
pub async fn export(
    State(state): State<AppState>,
    account: CurrentAccount,
) -> Result<Json<api::Export>, HttpError> {
    account.require_full_session()?;
    let row = sqlx::query!(
        r#"SELECT uuid, email, verified, created_at::text AS "created_at!",
                  EXISTS (SELECT 1 FROM identity WHERE account = $1 AND kind = 'totp'
                          AND details->>'confirmed' = 'true') AS "totp!"
           FROM account WHERE pk = $1"#,
        account.account
    )
    .fetch_one(&state.pool)
    .await?;
    let files = sqlx::query_as!(
        api::ExportedFile,
        r#"SELECT n.uuid AS id, l.name AS library, node_path(n.pk) AS "path!",
                  (SELECT count(*) FROM version v WHERE v.file = n.pk) AS "versions!"
           FROM node n JOIN library l ON l.pk = n.library
           JOIN organization o ON o.pk = l.owner
           WHERE o.personal_of = $1 AND n.is_file AND node_reachable(n.pk)
           ORDER BY l.name, node_path(n.pk)"#,
        account.account
    )
    .fetch_all(&state.pool)
    .await?;
    Ok(Json(api::Export {
        account: api::ExportedAccount {
            id: row.uuid,
            email: row.email,
            verified: row.verified,
            totp: row.totp,
            created_at: row.created_at,
        },
        organizations: db::account::me(&state.pool, account.account)
            .await?
            .organizations,
        sessions: db::account::sessions(
            &state.pool,
            account.account_uuid,
            account.token_uuid,
            state.settings.get().session_lifetime(),
        )
        .await?,
        identities: db::identity::identities(&state.pool, account.account).await?,
        app_passwords: db::account::app_passwords(&state.pool, &account).await?,
        files,
    }))
}
