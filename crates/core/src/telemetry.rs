//! Prometheus metrics, served on their own listener (`METRICS_ADDR`), never
//! on the public port. Labels are bounded: route templates, never paths,
//! organizations or accounts (usage per organization is the admin API's).

use std::sync::OnceLock;
use std::time::Instant;

use axum::Router;
use axum::extract::{Request, State};
use axum::middleware::Next;
use axum::response::Response;
use axum::routing::get;
use metrics_exporter_prometheus::{PrometheusBuilder, PrometheusHandle};
use sqlx::PgPool;

/// Installs the global recorder (once). Without it, recording is a no-op.
pub fn install_recorder() -> PrometheusHandle {
    static HANDLE: OnceLock<PrometheusHandle> = OnceLock::new();
    HANDLE
        .get_or_init(|| {
            PrometheusBuilder::new()
                .install_recorder()
                .expect("no other metrics recorder")
        })
        .clone()
}

/// `GET /metrics`, for the metrics listener.
pub fn router(handle: PrometheusHandle, pool: PgPool) -> Router {
    Router::new()
        .route("/metrics", get(render))
        .with_state((handle, pool))
}

async fn render(State((handle, pool)): State<(PrometheusHandle, PgPool)>) -> String {
    let idle = pool.num_idle() as f64;
    metrics::gauge!("theca_db_connections", "state" => "idle").set(idle);
    metrics::gauge!("theca_db_connections", "state" => "in_use").set(f64::from(pool.size()) - idle);
    handle.render()
}

/// Middleware: counts requests and their duration, by method, route template
/// and status.
pub async fn track(request: Request, next: Next) -> Response {
    let method = request.method().to_string();
    let route = route(request.uri().path());
    let started = Instant::now();
    let response = next.run(request).await;
    let status = response.status().as_u16();
    // Unknown paths (scanners probing /api/...) mustn't each become a label.
    let route = if matches!(status, 404 | 405) {
        "other".to_owned()
    } else {
        route
    };
    let status = status.to_string();
    metrics::counter!("theca_http_requests_total", "method" => method.clone(), "route" => route.clone(), "status" => status)
        .increment(1);
    metrics::histogram!("theca_http_request_duration_seconds", "method" => method, "route" => route)
        .record(started.elapsed().as_secs_f64());
    response
}

/// A path's template: IDs become `{id}`, and WebDAV paths (users' own names)
/// are all `/dav`. Bounded, whatever the requests.
pub fn route(path: &str) -> String {
    if path == "/dav" || path.starts_with("/dav/") {
        return "/dav".into();
    }
    let segments: Vec<&str> = path
        .split('/')
        .map(|s| {
            if uuid::Uuid::parse_str(s).is_ok() {
                "{id}"
            } else {
                s
            }
        })
        .collect();
    // Unknown routes (scanners) collapse too.
    if segments.len() > 7 || !path.starts_with("/api/") {
        return "other".into();
    }
    segments.join("/")
}

/// A tracing layer counting anomalies (`anomaly` field) by type.
pub struct AnomalyCounter;

impl<S: tracing::Subscriber> tracing_subscriber::Layer<S> for AnomalyCounter {
    fn on_event(&self, event: &tracing::Event<'_>, _: tracing_subscriber::layer::Context<'_, S>) {
        struct Visitor(Option<String>);
        impl tracing::field::Visit for Visitor {
            fn record_str(&mut self, field: &tracing::field::Field, value: &str) {
                if field.name() == "anomaly" {
                    self.0 = Some(value.to_owned());
                }
            }
            fn record_debug(&mut self, _: &tracing::field::Field, _: &dyn std::fmt::Debug) {}
        }
        let mut visitor = Visitor(None);
        event.record(&mut visitor);
        if let Some(anomaly) = visitor.0 {
            metrics::counter!("theca_anomalies_total", "anomaly" => anomaly).increment(1);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::route;

    #[test]
    fn routes_are_templates() {
        assert_eq!(
            route("/api/v1/nodes/0192d7f0-0000-7000-8000-000000000001/children"),
            "/api/v1/nodes/{id}/children"
        );
        assert_eq!(route("/dav/one/Docs/secret plans.txt"), "/dav");
        assert_eq!(route("/wp-admin/install.php"), "other");
        assert_eq!(route("/api/health"), "/api/health");
    }
}
