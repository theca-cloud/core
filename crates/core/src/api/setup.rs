//! The first-run setup. A fresh instance (no admin) only serves this; the
//! setup token, printed in the logs at startup, protects it unless
//! `SETUP_OPEN` is set.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use axum::Json;
use axum::extract::{Request, State};
use axum::http::{HeaderMap, StatusCode};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use theca_common::api;

use super::AppState;
use super::auth::{ClientIp, UserAgent, set_session_cookie};
use super::error::{ApiJson, HttpError};
use crate::auth::hash_password;
use crate::db;
use crate::db::setup::SetupError;

/// Whether the instance still has to be set up, and the token it needs.
#[derive(Debug)]
pub struct SetupState {
    pending: AtomicBool,
    /// `None`: open setup (`SETUP_OPEN`), or already set up.
    token: Option<String>,
}

impl SetupState {
    pub fn done() -> Arc<Self> {
        Arc::new(Self {
            pending: AtomicBool::new(false),
            token: None,
        })
    }

    pub fn pending(token: Option<String>) -> Arc<Self> {
        Arc::new(Self {
            pending: AtomicBool::new(true),
            token,
        })
    }

    pub fn is_pending(&self) -> bool {
        self.pending.load(Ordering::Acquire)
    }

    fn accepts(&self, token: Option<&str>) -> bool {
        match &self.token {
            None => true,
            Some(expected) => token.is_some_and(|t| subtle_eq(t.as_bytes(), expected.as_bytes())),
        }
    }
}

/// Constant-time comparison (the token is a secret).
fn subtle_eq(a: &[u8], b: &[u8]) -> bool {
    a.len() == b.len() && a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

/// A new setup token: 128 bits, hex.
pub fn generate_token() -> String {
    let mut bytes = [0u8; 16];
    getrandom::fill(&mut bytes).expect("system randomness");
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// Middleware: a fresh instance only serves the setup.
pub async fn require_setup(
    State(state): State<AppState>,
    request: Request,
    next: Next,
) -> Response {
    if state.setup.is_pending() {
        return HttpError::new(
            StatusCode::SERVICE_UNAVAILABLE,
            "setup_required",
            "this instance isn't set up yet: see the setup URL in the server's logs",
        )
        .into_response();
    }
    next.run(request).await
}

/// Whether the instance still has to be set up.
#[utoipa::path(get, path = "/api/v1/setup", responses((status = 200, body = api::SetupStatus)))]
pub async fn status(State(state): State<AppState>) -> Json<api::SetupStatus> {
    let required = state.setup.is_pending();
    Json(api::SetupStatus {
        required,
        token_required: required && state.setup.token.is_some(),
    })
}

/// Set up a fresh instance, once: the admin (logged in by the answer), the
/// first organization, and the storages, each tested first (write, read back,
/// delete). The token goes in `X-Theca-Setup-Token`.
#[utoipa::path(post, path = "/api/v1/setup", request_body = api::Setup, responses(
    (status = 200, body = api::Me),
    (status = 400, description = "`invalid_setup`, `storage_unusable`", body = api::ApiError),
    (status = 401, description = "`invalid_setup_token`", body = api::ApiError),
    (status = 409, description = "`already_set_up`, `storage_name_taken`", body = api::ApiError),
))]
pub async fn run(
    State(state): State<AppState>,
    headers: HeaderMap,
    ip: ClientIp,
    agent: UserAgent,
    ApiJson(req): ApiJson<api::Setup>,
) -> Result<(HeaderMap, Json<api::Me>), HttpError> {
    let already = || HttpError::conflict("already_set_up", "this instance is already set up");
    if !state.setup.is_pending() {
        return Err(already());
    }
    let token = headers
        .get("x-theca-setup-token")
        .and_then(|v| v.to_str().ok());
    if !state.setup.accepts(token) {
        tracing::warn!(anomaly = "setup_token_invalid", ip = ?ip.0, "setup with a wrong token");
        return Err(HttpError::new(
            StatusCode::UNAUTHORIZED,
            "invalid_setup_token",
            "wrong or missing setup token (see the server's logs)",
        ));
    }

    let invalid = |message: &str| HttpError::bad_request("invalid_setup", message.to_owned());
    if !req.admin_email.contains('@') {
        return Err(invalid("the admin's email is invalid"));
    }
    super::accounts::check_password(&req.admin_password)?;
    if req.organization.trim().is_empty() {
        return Err(invalid("the organization needs a name"));
    }
    if req.storages.is_empty() || req.storages.iter().any(|s| s.name.trim().is_empty()) {
        return Err(invalid("at least one storage, each with a name"));
    }

    let mut storages = Vec::new();
    for storage in &req.storages {
        let (implementation, details) = crate::storage::details(&storage.backend);
        crate::storage::probe(implementation, &details)
            .await
            .map_err(|e| {
                HttpError::bad_request(
                    "storage_unusable",
                    format!("storage {:?}: {e}", storage.name),
                )
            })?;
        storages.push((storage.name.trim().to_owned(), implementation, details));
    }

    let hash = hash_password(&req.admin_password);
    let (pk, account) = db::setup::run(
        &state.pool,
        req.admin_email.trim(),
        &hash,
        req.organization.trim(),
        &storages,
    )
    .await
    .map_err(|e| match e {
        SetupError::AlreadySetUp => already(),
        SetupError::Db(e) if constraint(&e) == Some("storage_name") => {
            HttpError::conflict("storage_name_taken", "two storages have the same name")
        }
        SetupError::Db(e) => e.into(),
    })?;
    state.setup.pending.store(false, Ordering::Release);
    tracing::info!(%account, "instance set up");

    let (token, _) =
        db::account::create_session(&state.pool, account, None, agent.0.as_deref(), ip.0).await?;
    let mut headers = HeaderMap::new();
    set_session_cookie(&mut headers, &token.token);
    Ok((headers, Json(db::account::me(&state.pool, pk).await?)))
}

fn constraint(e: &sqlx::Error) -> Option<&str> {
    e.as_database_error().and_then(|d| d.constraint())
}
