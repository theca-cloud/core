//! OIDC logins and the account's identities.
//!
//! The provider decides the session's scope: an instance-wide provider opens a
//! full session, an organization's provider a session restricted to that
//! organization (it could assert anyone's email: see docs/decisions.md,
//! Authentication). The start and the callback are browser navigations: they
//! answer with redirects, to the web app (`/`, `/account` after a link, or
//! `/login?error=...`).

use axum::extract::{Path, Query, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::Redirect;
use axum::routing::{delete, get, post};
use axum::{Json, Router};
use serde::Deserialize;
use theca_common::api;
use uuid::Uuid;

use super::AppState;
use super::auth::{ClientIp, CurrentAccount, UserAgent, cookie, set_cookie, set_session_cookie};
use super::error::HttpError;
use crate::db;
use crate::db::identity::{Provider, UnlinkError};
use crate::oidc::{self, Flow};

const FLOW_COOKIE: &str = "theca_oidc";
const FLOW_AAD: &[u8] = b"theca oidc flow";

pub fn router() -> Router<AppState> {
    Router::new()
        .route("/auth/oidc/providers", get(providers))
        .route("/auth/oidc/{provider}/start", get(start))
        .route("/auth/oidc/{provider}/link", post(link))
        .route("/auth/oidc/{provider}/callback", get(callback))
        .route("/auth/identities", get(identities))
        .route("/auth/identities/{id}", delete(unlink))
}

/// The instance-wide providers, for the login page. An organization's
/// provider isn't listed: the organization shares its start link.
#[utoipa::path(get, path = "/api/v1/auth/oidc/providers", operation_id = "login_providers", responses(
    (status = 200, body = Vec<api::IdentityProvider>),
))]
pub async fn providers(
    State(state): State<AppState>,
) -> Result<Json<Vec<api::IdentityProvider>>, HttpError> {
    Ok(Json(db::identity::instance_providers(&state.pool).await?))
}

/// Start logging in with a provider: redirects to it.
#[utoipa::path(get, path = "/api/v1/auth/oidc/{provider}/start", operation_id = "oidc_start", params(("provider" = Uuid, Path)), responses(
    (status = 303, description = "To the provider, or to `/login?error=...`"),
))]
pub async fn start(
    State(state): State<AppState>,
    Path(provider): Path<Uuid>,
) -> (HeaderMap, Redirect) {
    let mut headers = HeaderMap::new();
    match begin(&state, &mut headers, provider, None).await {
        Ok(url) => (headers, Redirect::to(&url)),
        Err(code) => (headers, failed(&state, code)),
    }
}

/// Start linking a provider's identity to this account: returns the URL to
/// send the browser to. A POST (CSRF-protected), not a link: another site must
/// not be able to start a linking flow in the user's browser.
#[utoipa::path(post, path = "/api/v1/auth/oidc/{provider}/link", params(("provider" = Uuid, Path)), responses(
    (status = 200, body = api::OidcRedirect),
    (status = 403, description = "`full_session_required`", body = api::ApiError),
    (status = 404, body = api::ApiError),
))]
pub async fn link(
    State(state): State<AppState>,
    account: CurrentAccount,
    Path(provider): Path<Uuid>,
) -> Result<(HeaderMap, Json<api::OidcRedirect>), HttpError> {
    account.require_full_session()?;
    let mut headers = HeaderMap::new();
    match begin(&state, &mut headers, provider, Some(account.account_uuid)).await {
        Ok(url) => Ok((headers, Json(api::OidcRedirect { url }))),
        Err("unknown_provider") => Err(HttpError::not_found("provider")),
        Err(code) => Err(HttpError::new(
            StatusCode::BAD_GATEWAY,
            code,
            "the provider can't be reached",
        )),
    }
}

/// The provider's authorization URL; the flow goes into an encrypted cookie.
async fn begin(
    state: &AppState,
    headers: &mut HeaderMap,
    provider: Uuid,
    link: Option<Uuid>,
) -> Result<String, &'static str> {
    let provider = load(state, provider).await.ok_or("unknown_provider")?;
    let (url, flow) = oidc::start(
        &state.cipher,
        state.settings.get().oidc_allow_internal_issuers,
        &provider,
        &redirect_uri(state, &provider),
        link,
    )
    .await
    .map_err(|e| {
        tracing::warn!(provider = %provider.uuid, error = %e, "OIDC start failed");
        "oidc_failed"
    })?;
    let sealed = state
        .cipher
        .encrypt(&serde_json::to_vec(&flow).expect("serializable"), FLOW_AAD);
    let max_age = oidc::FLOW_LIFETIME.as_secs();
    set_cookie(
        headers,
        &format!("{FLOW_COOKIE}={sealed}; Max-Age={max_age}"),
    );
    Ok(url)
}

#[derive(Debug, Deserialize, utoipa::IntoParams)]
#[into_params(parameter_in = Query)]
pub struct CallbackParams {
    code: Option<String>,
    state: Option<String>,
    /// Set by the provider when the user refused, or on errors.
    error: Option<String>,
}

/// The provider sends the user back here: opens a session (or links the
/// identity), then redirects to the web app.
#[utoipa::path(get, path = "/api/v1/auth/oidc/{provider}/callback", params(("provider" = Uuid, Path), CallbackParams), responses(
    (status = 303, description = "To `/` (logged in), `/account` (linked), or `/login?error=...`"),
))]
pub async fn callback(
    State(state): State<AppState>,
    request_headers: HeaderMap,
    ip: ClientIp,
    agent: UserAgent,
    Path(provider): Path<Uuid>,
    Query(params): Query<CallbackParams>,
) -> (HeaderMap, Redirect) {
    let mut headers = HeaderMap::new();
    set_cookie(&mut headers, &format!("{FLOW_COOKIE}=; Max-Age=0"));
    match finish(&state, &request_headers, ip, agent, provider, params).await {
        Ok(Some(token)) => {
            set_session_cookie(&mut headers, &token);
            (
                headers,
                Redirect::to(&format!("{}/", state.signer.public_url())),
            )
        }
        // An identity linked: back to the account's page.
        Ok(None) => (
            headers,
            Redirect::to(&format!("{}/account", state.signer.public_url())),
        ),
        Err(code) => (headers, failed(&state, code)),
    }
}

/// The callback's work. `Some(token)`: a new session; `None`: an identity linked.
async fn finish(
    state: &AppState,
    headers: &HeaderMap,
    ip: ClientIp,
    agent: UserAgent,
    provider: Uuid,
    params: CallbackParams,
) -> Result<Option<String>, &'static str> {
    if let Some(error) = params.error {
        tracing::info!(%provider, error, "the provider refused the login");
        return Err("oidc_refused");
    }
    let flow: Flow = cookie(headers, FLOW_COOKIE)
        .and_then(|c| state.cipher.decrypt(c, FLOW_AAD))
        .and_then(|bytes| serde_json::from_slice(&bytes).ok())
        .ok_or("oidc_expired")?;
    // The state ties the callback to the browser that started the flow (CSRF).
    if flow.is_expired()
        || flow.provider != provider
        || params.state.as_deref() != Some(&flow.state)
    {
        tracing::warn!(anomaly = "oidc_state_mismatch", %provider, ip = ?ip.0,
            "OIDC callback without a matching flow");
        return Err("oidc_expired");
    }
    let provider = load(state, provider).await.ok_or("unknown_provider")?;
    let code = params.code.ok_or("oidc_failed")?;
    let link = flow.link;
    let asserted = oidc::finish(
        &state.cipher,
        state.settings.get().oidc_allow_internal_issuers,
        &provider,
        &redirect_uri(state, &provider),
        flow,
        code,
    )
    .await
    .map_err(|e| {
        tracing::warn!(provider = %provider.uuid, error = %e, "OIDC callback failed");
        "oidc_failed"
    })?;
    let db_error = |e: sqlx::Error| {
        tracing::error!(error = %e, "database error in an OIDC callback");
        "internal"
    };

    let linked = db::identity::account_of(&state.pool, provider.pk, &asserted.subject)
        .await
        .map_err(db_error)?;
    if let Some(account) = link {
        // Linking from a full session: the identity must be new, or already this account's.
        let (pk, uuid) = db::identity::account_by_uuid(&state.pool, account)
            .await
            .map_err(db_error)?
            .ok_or("internal")?;
        return match linked {
            Some((_, owner)) if owner == uuid => Ok(None),
            Some(_) => Err("identity_taken"),
            None => {
                db::identity::link(&state.pool, pk, provider.pk, &asserted.subject)
                    .await
                    .map_err(db_error)?;
                Ok(None)
            }
        };
    }
    let account = match linked {
        Some((_, uuid)) => uuid,
        None => link_by_email(state, &provider, &asserted).await?,
    };
    let (token, _) = db::account::create_session(
        &state.pool,
        account,
        provider.organization,
        agent.0.as_deref(),
        ip.0,
    )
    .await
    .map_err(db_error)?;
    Ok(Some(token.token))
}

/// An unknown identity: linked to the account with its email, if the instance
/// allows it and the provider verified the email.
async fn link_by_email(
    state: &AppState,
    provider: &Provider,
    asserted: &oidc::Asserted,
) -> Result<Uuid, &'static str> {
    let verified_email = asserted
        .email
        .as_deref()
        .filter(|_| asserted.email_verified);
    // A verified account with the address the provider verified: linked, if
    // allowed. An organization's provider only links that organization's
    // members: never beyond its bounds (an invitation isn't membership).
    if let (Some(email), true) = (verified_email, state.settings.get().oidc_link_by_email) {
        let found =
            db::identity::verified_account_by_email(&state.pool, email, provider.organization)
                .await
                .map_err(|_| "internal")?;
        if let Some((pk, uuid)) = found {
            db::identity::link(&state.pool, pk, provider.pk, &asserted.subject)
                .await
                .map_err(|_| "internal")?;
            tracing::info!(account = %uuid, provider = %provider.uuid, "OIDC identity linked by email");
            return Ok(uuid);
        }
    }
    // Sign-up: an instance-wide provider, registration open, an address
    // without an account. (An organization's provider can't create members.)
    let email = asserted.email.as_deref().filter(|e| e.contains('@'));
    let (Some(email), None, true) = (
        email,
        provider.organization,
        state.settings.get().registration_open,
    ) else {
        return Err("unknown_identity");
    };
    let account = db::identity::sign_up(
        &state.pool,
        email,
        asserted.email_verified,
        provider.pk,
        &asserted.subject,
    )
    .await
    .map_err(|_| "internal")?
    .ok_or("unknown_identity")?;
    tracing::info!(%account, provider = %provider.uuid, "account created through OIDC");
    Ok(account)
}

async fn load(state: &AppState, provider: Uuid) -> Option<Provider> {
    db::identity::provider(&state.pool, provider)
        .await
        .ok()
        .flatten()
}

fn redirect_uri(state: &AppState, provider: &Provider) -> String {
    format!(
        "{}/api/v1/auth/oidc/{}/callback",
        state.signer.public_url(),
        provider.uuid
    )
}

fn failed(state: &AppState, code: &str) -> Redirect {
    Redirect::to(&format!("{}/login?error={code}", state.signer.public_url()))
}

/// The account's ways to log in.
#[utoipa::path(get, path = "/api/v1/auth/identities", responses(
    (status = 200, body = Vec<api::Identity>),
    (status = 403, description = "`full_session_required`", body = api::ApiError),
))]
pub async fn identities(
    State(state): State<AppState>,
    account: CurrentAccount,
) -> Result<Json<Vec<api::Identity>>, HttpError> {
    account.require_full_session()?;
    Ok(Json(
        db::identity::identities(&state.pool, account.account).await?,
    ))
}

/// Remove one of the account's ways to log in (never the last one).
#[utoipa::path(delete, path = "/api/v1/auth/identities/{id}", params(("id" = Uuid, Path)), responses(
    (status = 204),
    (status = 404, body = api::ApiError),
    (status = 409, description = "`last_identity`", body = api::ApiError),
))]
pub async fn unlink(
    State(state): State<AppState>,
    account: CurrentAccount,
    Path(id): Path<Uuid>,
) -> Result<StatusCode, HttpError> {
    account.require_full_session()?;
    match db::identity::unlink(&state.pool, account.account, id).await {
        Ok(()) => Ok(StatusCode::NO_CONTENT),
        Err(UnlinkError::NotFound) => Err(HttpError::not_found("identity")),
        Err(UnlinkError::Last) => Err(HttpError::conflict(
            "last_identity",
            "the account's last way to log in",
        )),
        Err(UnlinkError::Db(e)) => Err(e.into()),
    }
}
