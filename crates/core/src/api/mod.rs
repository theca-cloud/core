//! HTTP API. Unversioned probes (`/api/health`, `/api/version`) and `/api/v1`.

mod accounts;
mod admin;
mod app_password;
pub mod auth;
mod content;
pub mod error;
mod nodes;
mod oidc;
mod organizations;
mod providers;
mod session;
pub mod setup;
mod sharing;
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
use crate::settings::LiveSettings;
use crate::storage::{Backends, UrlSigner};

/// Shared by every handler.
#[derive(Clone)]
pub struct AppState {
    pub pool: PgPool,
    /// The instance settings, live (a change through the admin API applies at once).
    pub settings: LiveSettings,
    /// Signs and verifies the server's own presigned URLs (filesystem backend).
    pub signer: UrlSigner,
    /// Storage backends, built once per storage.
    pub backends: Backends,
    /// Encrypts secrets stored in the database.
    pub cipher: DataCipher,
    /// Whether the first-run setup is still to be done.
    pub setup: Arc<setup::SetupState>,
    /// Rate limit counters.
    pub limits: crate::limits::Limits,
    /// Reverse proxies whose `X-Forwarded-For` is believed (`TRUSTED_PROXIES`).
    pub proxies: Arc<Vec<sqlx::types::ipnetwork::IpNetwork>>,
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
        session::delete_me,
        session::export,
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
        v1::limits,
        v1::instance,
        v1::create_library,
        v1::list_libraries,
        v1::update_library,
        v1::delete_library,
        v1::restore_library,
        v1::organization_trash,
        v1::children,
        v1::lookup,
        v1::upload_urls,
        v1::announce,
        v1::cancel,
        v1::commit,
        v1::download,
        content::content,
        nodes::info,
        nodes::create_folder,
        nodes::tree,
        nodes::update,
        nodes::trash_nodes,
        nodes::copy,
        nodes::trash,
        nodes::restore,
        nodes::versions,
        nodes::restore_version,
        nodes::search,
        admin::jobs,
        accounts::register,
        accounts::verify,
        accounts::resend,
        accounts::forgot,
        accounts::reset,
        accounts::accept,
        organizations::create,
        organizations::organization,
        organizations::rename,
        organizations::members,
        sharing::libraries,
        sharing::members,
        sharing::set_member,
        sharing::remove_member,
        sharing::namespaces,
        sharing::create_namespace,
        sharing::rename_namespace,
        sharing::delete_namespace,
        sharing::add_library,
        sharing::remove_library,
        organizations::add,
        organizations::remove,
        admin::run_job,
        admin::settings,
        admin::change_settings,
        admin::organizations,
        admin::create_organization,
        admin::update_organization,
        admin::accounts,
        admin::update_account,
        admin::storages,
        admin::add_storage,
        admin::default_storage,
        admin::providers,
        admin::create_provider,
        admin::update_provider,
        admin::delete_provider,
        admin::delete_organization,
        admin::delete_account,
        organizations::transfer,
        organizations::delete_organization,
        organizations::providers,
        organizations::create_provider,
        organizations::update_provider,
        organizations::delete_provider,
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
                .merge(admin::router())
                .merge(accounts::router())
                .merge(organizations::router())
                .merge(sharing::router())
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
        .merge(dav_routes(state.clone()))
        .layer(axum::middleware::from_fn(crate::telemetry::track))
        .with_state(state)
}

/// `/dav/...`: every method, authenticated like the API.
fn dav_routes(state: AppState) -> Router<AppState> {
    let dav = crate::dav::handler(state.clone());
    let serve = move |request: axum::extract::Request| crate::dav::serve(dav.clone(), request);
    Router::new()
        .route("/dav", axum::routing::any(serve.clone()))
        .route("/dav/", axum::routing::any(serve.clone()))
        .route("/dav/{*path}", axum::routing::any(serve))
        .layer(axum::middleware::from_fn_with_state(
            state.clone(),
            auth::authenticate,
        ))
        .layer(axum::middleware::from_fn_with_state(
            state,
            setup::require_setup,
        ))
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
    /// Background jobs whose last 3 runs failed (reported, not a failed probe:
    /// restarting the server wouldn't fix them).
    failing_jobs: Vec<String>,
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
    let failing_jobs = match database {
        Ok(_) => crate::jobs::failing(&state.pool).await.unwrap_or_default(),
        Err(_) => Vec::new(),
    };
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
            failing_jobs,
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
        let settings = crate::settings::LiveSettings::new(crate::settings::Settings {
            instance_name: "Theca".into(),
            gc_grace_period: Duration::from_secs(1),
            presigned_url_lifetime: Duration::from_secs(60),
            max_pending_uploads: 100,
            session_idle_lifetime: Duration::from_secs(3600),
            session_max_lifetime: None,
            oidc_link_by_email: false,
            oidc_allow_internal_issuers: false,
            trash_retention: Duration::from_secs(0),
            registration_open: false,
            organization_creation_open: true,
            smtp: None,
            job_intervals: Default::default(),
            rate_limits: Default::default(),
        });
        router(AppState {
            backends: Backends::new(pool.clone()),
            pool,
            settings,
            signer: UrlSigner::new([0; 32], "http://test"),
            cipher: DataCipher::new(&[0; 32]),
            setup: setup::SetupState::done(),
            limits: Default::default(),
            proxies: Default::default(),
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

    /// Every path parameter is in its path: a query struct derived as
    /// `IntoParams` without `parameter_in = Query` lands in the path, which
    /// typed clients (the web app's) then put there.
    #[test]
    fn path_parameters_are_in_their_paths() {
        let spec = serde_json::to_value(ApiDoc::openapi()).unwrap();
        let mut misplaced = Vec::new();
        for (path, item) in spec["paths"].as_object().unwrap() {
            for operation in item.as_object().unwrap().values() {
                for parameter in operation["parameters"].as_array().into_iter().flatten() {
                    let name = parameter["name"].as_str().unwrap();
                    if parameter["in"] == "path" && !path.contains(&format!("{{{name}}}")) {
                        misplaced.push(format!("{path} {name}"));
                    }
                }
            }
        }
        assert!(
            misplaced.is_empty(),
            "query parameters declared in the path: {misplaced:?}"
        );
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
