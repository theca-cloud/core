//! TOTP, the second factor of password logins: setup, confirmation (which
//! hands out recovery codes), disabling, new recovery codes. Account-level:
//! full sessions only.

use axum::extract::State;
use axum::http::StatusCode;
use axum::routing::post;
use axum::{Json, Router};
use theca_common::api;
use uuid::Uuid;

use super::AppState;
use super::auth::{ClientIp, CurrentAccount};
use super::error::{ApiJson, HttpError};
use crate::db;
use crate::db::account::TotpIdentity;
use crate::limits::Limit;
use crate::totp;

pub fn router() -> Router<AppState> {
    Router::new()
        .route("/auth/totp", post(start))
        .route("/auth/totp/confirm", post(confirm))
        .route("/auth/totp/disable", post(disable))
        .route("/auth/totp/recovery-codes", post(recovery_codes))
}

/// Checks a TOTP code (accepted once) or, if `recovery` allows it, a recovery
/// code (used up). Failures are logged as anomalies.
pub(super) async fn check_code(
    state: &AppState,
    account: Uuid,
    identity: &TotpIdentity,
    code: &str,
    recovery: bool,
    ip: ClientIp,
) -> Result<bool, HttpError> {
    let settings = state.settings.get();
    let key = account.to_string();
    state
        .limits
        .check(&settings, Limit::Totp, &key)
        .map_err(HttpError::too_many)?;
    let ok = if totp::is_recovery_code(code) {
        let (prefix, hash) = totp::recovery_code_hash(account, code);
        recovery && db::account::use_recovery_code(&state.pool, account, &prefix, &hash).await?
    } else {
        let secret = state
            .cipher
            .decrypt(&identity.secret, account.as_bytes())
            .ok_or_else(|| {
                HttpError::internal(format_args!(
                    "account {account}: TOTP secret doesn't decrypt (DATA_KEY__FILE changed?)"
                ))
            })?;
        match totp::check(&secret, code) {
            Some(step) => db::account::use_totp_step(&state.pool, identity.pk, step).await?,
            None => false,
        }
    };
    if !ok {
        state.limits.fail(&settings, Limit::Totp, &key);
        tracing::warn!(anomaly = "totp_failed", %account, ip = ?ip.0,
            "wrong, reused or expired TOTP or recovery code");
    }
    Ok(ok)
}

fn invalid_code() -> HttpError {
    HttpError::new(
        StatusCode::UNAUTHORIZED,
        "invalid_totp",
        "wrong or reused code",
    )
}

/// Start setting up TOTP: a new secret, not required at login until
/// confirmed. Starting again replaces an unconfirmed secret.
#[utoipa::path(post, path = "/api/v1/auth/totp", operation_id = "totp_start", responses(
    (status = 200, body = api::TotpSetup),
    (status = 403, description = "`full_session_required`", body = api::ApiError),
    (status = 409, description = "`totp_enabled`", body = api::ApiError),
))]
pub async fn start(
    State(state): State<AppState>,
    account: CurrentAccount,
) -> Result<Json<api::TotpSetup>, HttpError> {
    account.require_full_session()?;
    let secret = totp::generate_secret();
    let sealed = state
        .cipher
        .encrypt(&secret, account.account_uuid.as_bytes());
    if !db::account::start_totp(&state.pool, account.account, &sealed).await? {
        return Err(HttpError::conflict("totp_enabled", "TOTP is already on"));
    }
    let email = db::account::email_of(&mut *state.pool.acquire().await?, account.account).await?;
    let (secret, uri) = totp::setup(&secret, &state.settings.get().instance_name, &email);
    Ok(Json(api::TotpSetup { secret, uri }))
}

/// Confirm TOTP with a first code: from now on, password logins ask for a
/// code. Returns the recovery codes (shown only now).
#[utoipa::path(post, path = "/api/v1/auth/totp/confirm", request_body = api::TotpCode, responses(
    (status = 200, body = api::RecoveryCodes),
    (status = 401, description = "`invalid_totp`", body = api::ApiError),
    (status = 404, description = "No TOTP being set up", body = api::ApiError),
))]
pub async fn confirm(
    State(state): State<AppState>,
    account: CurrentAccount,
    ip: ClientIp,
    ApiJson(req): ApiJson<api::TotpCode>,
) -> Result<Json<api::RecoveryCodes>, HttpError> {
    account.require_full_session()?;
    let identity = db::account::totp_of(&state.pool, account.account)
        .await?
        .filter(|t| !t.confirmed)
        .ok_or_else(|| HttpError::not_found("TOTP setup"))?;
    if !check_code(
        &state,
        account.account_uuid,
        &identity,
        &req.code,
        false,
        ip,
    )
    .await?
    {
        return Err(invalid_code());
    }
    Ok(Json(
        new_recovery_codes(&state, account.account_uuid, Some(identity.pk)).await?,
    ))
}

/// Turn TOTP off, with a current code (or a recovery code).
#[utoipa::path(post, path = "/api/v1/auth/totp/disable", request_body = api::TotpCode, responses(
    (status = 204),
    (status = 401, description = "`invalid_totp`", body = api::ApiError),
    (status = 404, description = "TOTP is off", body = api::ApiError),
))]
pub async fn disable(
    State(state): State<AppState>,
    account: CurrentAccount,
    ip: ClientIp,
    ApiJson(req): ApiJson<api::TotpCode>,
) -> Result<StatusCode, HttpError> {
    let identity = confirmed(&state, &account).await?;
    if !check_code(&state, account.account_uuid, &identity, &req.code, true, ip).await? {
        return Err(invalid_code());
    }
    db::account::disable_totp(&state.pool, account.account, account.account_uuid).await?;
    Ok(StatusCode::NO_CONTENT)
}

/// New recovery codes, with a current code (or a recovery code): the
/// previous ones stop working.
#[utoipa::path(post, path = "/api/v1/auth/totp/recovery-codes", request_body = api::TotpCode, responses(
    (status = 200, body = api::RecoveryCodes),
    (status = 401, description = "`invalid_totp`", body = api::ApiError),
    (status = 404, description = "TOTP is off", body = api::ApiError),
))]
pub async fn recovery_codes(
    State(state): State<AppState>,
    account: CurrentAccount,
    ip: ClientIp,
    ApiJson(req): ApiJson<api::TotpCode>,
) -> Result<Json<api::RecoveryCodes>, HttpError> {
    let identity = confirmed(&state, &account).await?;
    if !check_code(&state, account.account_uuid, &identity, &req.code, true, ip).await? {
        return Err(invalid_code());
    }
    Ok(Json(
        new_recovery_codes(&state, account.account_uuid, None).await?,
    ))
}

async fn confirmed(state: &AppState, account: &CurrentAccount) -> Result<TotpIdentity, HttpError> {
    account.require_full_session()?;
    db::account::totp_of(&state.pool, account.account)
        .await?
        .filter(|t| t.confirmed)
        .ok_or_else(|| HttpError::not_found("TOTP"))
}

async fn new_recovery_codes(
    state: &AppState,
    account: Uuid,
    confirm: Option<i64>,
) -> Result<api::RecoveryCodes, HttpError> {
    let codes: Vec<String> = (0..totp::RECOVERY_CODES)
        .map(|_| totp::generate_recovery_code())
        .collect();
    let hashes: Vec<_> = codes
        .iter()
        .map(|c| totp::recovery_code_hash(account, c))
        .collect();
    db::account::set_recovery_codes(&state.pool, account, confirm, &hashes).await?;
    Ok(api::RecoveryCodes { codes })
}
