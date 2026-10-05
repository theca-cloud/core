//! HTTP API. Unversioned probes (`/api/health`, `/api/version`) and `/api/v1`.

mod app_password;
pub mod auth;
pub mod error;
mod nodes;
mod oidc;
mod session;
pub mod setup;
mod totp;
mod v1;

use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::Duration;

use axum::extract::State;
use axum::http::StatusCode;
use axum::routing::get;
use axum::{Json, Router};
use serde::Serialize;
use sqlx::PgPool;
use utoipa::{OpenApi, ToSchema};

use crate::keys::DataCipher;
use crate::settings::Settings;
use crate::storage::{Backends, UrlSigner};

/// Shared by every handler.
#[derive(Clone)]
pub struct AppState {
    pub pool: PgPool,
    pub settings: Arc<Settings>,
    /// Signs and verifies the server's own presigned URLs (filesystem backend).
    pub signer: UrlSigner,
    /// Storage backends, built once per storage.
    pub backends: Backends,
    /// Encrypts secrets stored in the database.
    pub cipher: DataCipher,
    /// Whether the first-run setup is still to be done.
    pub setup: Arc<setup::SetupState>,
}

/// The OpenAPI description of the API, committed as `openapi.json`.
#[derive(OpenApi)]
#[openapi(
    info(title = "Theca"),
    paths(
        health,
        version,
        setup::status,
        setup::run,
        session::login,
        session::logout,
        session::sessions,
        session::revoke,
        session::me,
        app_password::create,
        app_password::list,
        app_password::revoke,
        totp::start,
        totp::confirm,
        totp::disable,
        totp::recovery_codes,
        oidc::providers,
        oidc::start,
        oidc::link,
        oidc::callback,
        oidc::identities,
        oidc::unlink,
        v1::create_library,
        v1::list_libraries,
        v1::children,
        v1::lookup,
        v1::upload_urls,
        v1::announce,
        v1::cancel,
        v1::commit,
        v1::download,
        nodes::info,
        nodes::create_folder,
        nodes::update,
        nodes::delete,
        nodes::copy,
        nodes::trash,
        nodes::restore,
        nodes::versions,
        nodes::restore_version,
        nodes::search,
    ),
    components(schemas(theca_common::api::ApiError, theca_common::api::ChunkState))
)]
pub struct ApiDoc;

/// Builds the router.
pub fn router(state: AppState) -> Router {
    Router::new()
        .route("/api/health", get(health))
        .route("/api/version", get(version))
        .route("/api/v1/setup", get(setup::status).post(setup::run))
        .nest(
            "/api/v1",
            v1::router()
                .merge(nodes::router())
                .merge(session::router())
                .merge(app_password::router())
                .merge(totp::router())
                .merge(oidc::router())
                .layer(axum::middleware::from_fn_with_state(
                    state.clone(),
                    auth::authenticate,
                ))
                .layer(axum::middleware::from_fn_with_state(
                    state.clone(),
                    setup::require_setup,
                )),
        )
        .with_state(state)
}

/// Each health check gets at most this long, so a hung dependency fails the probe.
const CHECK_TIMEOUT: Duration = Duration::from_secs(2);

/// Health report. The contract is the HTTP status (200 / 503); the body is informational.
#[derive(Serialize, ToSchema)]
pub struct Health {
    /// `ok` or `failing`.
    status: &'static str,
    /// Each check: `ok` or `failing`. Details go to the logs, never here.
    checks: BTreeMap<&'static str, &'static str>,
}

/// Readiness probe: 200 when the server can serve requests, 503 otherwise.
#[utoipa::path(get, path = "/api/health", responses(
    (status = 200, description = "Ready", body = Health),
    (status = 503, description = "Not ready", body = Health),
))]
async fn health(State(state): State<AppState>) -> (StatusCode, Json<Health>) {
    let database =
        tokio::time::timeout(CHECK_TIMEOUT, sqlx::query("SELECT 1").execute(&state.pool))
            .await
            .map_err(|_| "timed out".to_owned())
            .and_then(|r| r.map_err(|e| e.to_string()));
    if let Err(reason) = &database {
        tracing::warn!(reason, "health check failed: database");
    }

    let checks = BTreeMap::from([("database", if database.is_ok() { "ok" } else { "failing" })]);
    let healthy = checks.values().all(|c| *c == "ok");
    let status = if healthy {
        StatusCode::OK
    } else {
        StatusCode::SERVICE_UNAVAILABLE
    };
    (
        status,
        Json(Health {
            status: if healthy { "ok" } else { "failing" },
            checks,
        }),
    )
}

/// Server and API versions, for clients to check compatibility.
#[derive(Serialize, ToSchema)]
pub struct Version {
    /// Version of this server.
    server: &'static str,
    /// Supported major API versions.
    api: Vec<&'static str>,
}

/// Server version and supported API majors.
#[utoipa::path(get, path = "/api/version", responses((status = 200, body = Version)))]
async fn version() -> Json<Version> {
    Json(Version {
        server: env!("CARGO_PKG_VERSION"),
        api: vec!["v1"],
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::{Body, to_bytes};
    use axum::http::Request;
    use sqlx::postgres::PgPoolOptions;
    use tower::ServiceExt;

    async fn get(app: Router, path: &str) -> (StatusCode, serde_json::Value) {
        let response = app
            .oneshot(Request::get(path).body(Body::empty()).unwrap())
            .await
            .unwrap();
        let status = response.status();
        let body = to_bytes(response.into_body(), usize::MAX).await.unwrap();
        (status, serde_json::from_slice(&body).unwrap())
    }

    fn app(pool: PgPool) -> Router {
        let settings = Arc::new(Settings {
            instance_name: "Theca".into(),
            gc_grace_period: Duration::from_secs(1),
            presigned_url_lifetime: Duration::from_secs(60),
            max_pending_uploads: 100,
            session_idle_lifetime: Duration::from_secs(3600),
            session_max_lifetime: None,
            oidc_link_by_email: false,
        });
        router(AppState {
            backends: Backends::new(pool.clone()),
            pool,
            settings,
            signer: UrlSigner::new([0; 32], "http://test"),
            cipher: DataCipher::new(&[0; 32]),
            setup: setup::SetupState::done(),
        })
    }

    #[sqlx::test]
    async fn healthy(pool: PgPool) {
        let (status, body) = get(app(pool), "/api/health").await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["checks"]["database"], "ok");
    }

    #[tokio::test]
    async fn database_down() {
        let pool = PgPoolOptions::new()
            .acquire_timeout(Duration::from_millis(200))
            .connect_lazy("postgres://nobody@127.0.0.1:1/none")
            .unwrap();
        let (status, body) = get(app(pool), "/api/health").await;
        assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
        assert_eq!(body["checks"]["database"], "failing");
    }

    #[tokio::test]
    async fn version() {
        let pool = PgPoolOptions::new()
            .connect_lazy("postgres://unused@127.0.0.1:1/x")
            .unwrap();
        let (status, body) = get(app(pool), "/api/version").await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["api"][0], "v1");
    }

    /// `openapi.json` must match the code. Regenerate with `UPDATE_OPENAPI=1 cargo test`.
    #[test]
    fn openapi_is_up_to_date() {
        let path = concat!(env!("CARGO_MANIFEST_DIR"), "/../../openapi.json");
        let generated = ApiDoc::openapi().to_pretty_json().unwrap() + "\n";
        if std::env::var_os("UPDATE_OPENAPI").is_some() {
            std::fs::write(path, &generated).unwrap();
        }
        let committed = std::fs::read_to_string(path).unwrap_or_default();
        assert!(
            committed == generated,
            "openapi.json is stale: run UPDATE_OPENAPI=1 cargo test"
        );
    }
}
