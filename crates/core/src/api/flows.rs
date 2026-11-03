//! Login flows: a device (the sync client, a CLI) logs in through the browser,
//! so it works for password, TOTP and OIDC accounts alike and never sees a
//! password. The device starts a flow and polls; the account approves it on
//! the web app's `/connect/<flow>` from a full session; the poll then returns
//! an app password for the whole account, once (docs/decisions.md,
//! Authentication).

use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use sha2::Digest;
use theca_common::api;
use uuid::Uuid;

use super::AppState;
use super::auth::{ClientIp, CurrentAccount};
use super::error::{ApiJson, HttpError};
use crate::auth::OpaqueToken;
use crate::db;
use crate::db::flow::Polled;
use crate::limits::Limit;

pub fn router() -> Router<AppState> {
    Router::new()
        .route("/auth/flows", post(start))
        .route("/auth/flows/poll", post(poll))
        .route("/auth/flows/{id}", get(show))
        .route("/auth/flows/{id}/approve", post(approve))
}

/// Upper-case letters and digits nobody confuses (no 0/O, 1/I), 32 of them.
const CODE_ALPHABET: &[u8; 32] = b"ABCDEFGHJKLMNPQRSTUVWXYZ23456789";

/// `ABC-DEF`: 30 bits, read aloud and compared by eye.
fn code() -> String {
    let mut bytes = [0u8; 6];
    getrandom::fill(&mut bytes).expect("system randomness");
    let letters: String = bytes
        .iter()
        .map(|b| CODE_ALPHABET[usize::from(b % 32)] as char)
        .collect();
    format!("{}-{}", &letters[..3], &letters[3..])
}

/// Start a login flow: anyone may, limited per address like logins.
#[utoipa::path(post, path = "/api/v1/auth/flows", operation_id = "start_login_flow", request_body = api::StartLoginFlow, responses(
    (status = 201, body = api::LoginFlow),
    (status = 400, description = "`invalid_name`", body = api::ApiError),
    (status = 429, body = api::ApiError),
))]
pub async fn start(
    State(state): State<AppState>,
    ip: ClientIp,
    ApiJson(req): ApiJson<api::StartLoginFlow>,
) -> Result<(StatusCode, Json<api::LoginFlow>), HttpError> {
    if let Some(ip) = ip.0 {
        let settings = state.settings.get();
        state
            .limits
            .hit(&settings, Limit::LoginIp, &ip.to_string())
            .map_err(HttpError::too_many)?;
    }
    let name = req.name.trim();
    if name.is_empty() || name.chars().count() > 200 {
        return Err(HttpError::bad_request(
            "invalid_name",
            "the name must have 1 to 200 characters",
        ));
    }
    let poll = OpaqueToken::generate();
    let code = code();
    let id = db::flow::start(&state.pool, name, &code, &poll.hash).await?;
    Ok((
        StatusCode::CREATED,
        Json(api::LoginFlow {
            url: format!(
                "{}/connect/{id}",
                state.signer.public_url().trim_end_matches('/')
            ),
            poll: poll.token,
            code,
        }),
    ))
}

/// A flow waiting for approval: what the approval page shows.
#[utoipa::path(get, path = "/api/v1/auth/flows/{id}", operation_id = "login_flow", params(("id" = Uuid, Path)), responses(
    (status = 200, body = api::PendingLoginFlow),
    (status = 403, description = "`full_session_required`", body = api::ApiError),
    (status = 404, description = "Unknown, expired or already approved", body = api::ApiError),
))]
pub async fn show(
    State(state): State<AppState>,
    account: CurrentAccount,
    Path(id): Path<Uuid>,
) -> Result<Json<api::PendingLoginFlow>, HttpError> {
    account.require_full_session()?;
    let flow = db::flow::pending(&state.pool, id)
        .await?
        .ok_or_else(|| HttpError::not_found("login flow"))?;
    Ok(Json(api::PendingLoginFlow {
        name: flow.name,
        code: flow.code,
        created_at: flow.created_at,
    }))
}

/// Approve a flow: its device gets an app password for this account at its
/// next poll. A full session only: an app password for the whole account.
#[utoipa::path(post, path = "/api/v1/auth/flows/{id}/approve", operation_id = "approve_login_flow", params(("id" = Uuid, Path)), responses(
    (status = 204),
    (status = 403, description = "`full_session_required`", body = api::ApiError),
    (status = 404, description = "Unknown, expired or already approved", body = api::ApiError),
))]
pub async fn approve(
    State(state): State<AppState>,
    account: CurrentAccount,
    Path(id): Path<Uuid>,
) -> Result<StatusCode, HttpError> {
    account.require_full_session()?;
    if db::flow::approve(&state.pool, id, account.account).await? {
        Ok(StatusCode::NO_CONTENT)
    } else {
        Err(HttpError::not_found("login flow"))
    }
}

/// Poll a flow: `202` while it waits, the app password once approved (the
/// flow then gone), `404` when expired or unknown.
#[utoipa::path(post, path = "/api/v1/auth/flows/poll", operation_id = "poll_login_flow", request_body = api::PollLoginFlow, responses(
    (status = 200, body = api::NewAppPassword),
    (status = 202, description = "Not approved yet"),
    (status = 404, description = "Unknown, expired, or already returned", body = api::ApiError),
))]
pub async fn poll(
    State(state): State<AppState>,
    ApiJson(req): ApiJson<api::PollLoginFlow>,
) -> Result<Response, HttpError> {
    let hash: [u8; 32] = sha2::Sha256::digest(req.poll.as_bytes()).into();
    match db::flow::poll(&state.pool, &hash).await? {
        Polled::Gone => Err(HttpError::not_found("login flow")),
        Polled::Pending => Ok(StatusCode::ACCEPTED.into_response()),
        Polled::Approved {
            token,
            uuid,
            name,
            created_at,
        } => Ok(Json(api::NewAppPassword {
            password: token.token,
            app_password: api::AppPassword {
                id: uuid,
                name,
                organization: None,
                library: None,
                read_only: false,
                created_at,
                last_seen: None,
            },
        })
        .into_response()),
    }
}
