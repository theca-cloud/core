//! The web app's files (`WEB_DIR`): served for every path the API doesn't
//! know, with `index.html` for unknown ones (a single-page app routes them
//! itself). Unknown `/api/` and `/dav/` paths stay `404`s.

use std::path::Path;

use axum::Router;
use axum::extract::Request;
use axum::http::StatusCode;
use axum::response::IntoResponse;
use tower::ServiceExt;
use tower_http::services::{ServeDir, ServeFile};

/// The router, also serving `dir`.
pub fn serve(router: Router, dir: &Path) -> Router {
    let files = ServeDir::new(dir).fallback(ServeFile::new(dir.join("index.html")));
    router.fallback(move |request: Request| {
        let files = files.clone();
        async move {
            let path = request.uri().path();
            if path.starts_with("/api/") || path == "/api" || path.starts_with("/dav") {
                return StatusCode::NOT_FOUND.into_response();
            }
            match files.oneshot(request).await {
                Ok(response) => response.map(axum::body::Body::new),
                Err(e) => match e {},
            }
        }
    })
}

#[cfg(test)]
mod tests {
    use axum::body::{Body, to_bytes};
    use axum::http::{Request, StatusCode};
    use axum::routing::get;
    use tower::ServiceExt;

    use super::*;

    #[tokio::test]
    async fn files_and_spa_fallback() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("index.html"), "<app>").unwrap();
        std::fs::write(dir.path().join("app.js"), "js").unwrap();
        let api = Router::new().route("/api/health", get(|| async { "ok" }));
        let app = serve(api, dir.path());
        let get = |path: &'static str| {
            let app = app.clone();
            async move {
                let response = app
                    .oneshot(Request::get(path).body(Body::empty()).unwrap())
                    .await
                    .unwrap();
                let status = response.status();
                let body = to_bytes(response.into_body(), usize::MAX).await.unwrap();
                (status, String::from_utf8(body.to_vec()).unwrap())
            }
        };
        assert_eq!(get("/api/health").await, (StatusCode::OK, "ok".into()));
        assert_eq!(get("/app.js").await, (StatusCode::OK, "js".into()));
        assert_eq!(
            get("/libraries/123").await,
            (StatusCode::OK, "<app>".into()),
            "the SPA routes it"
        );
        assert_eq!(
            get("/api/v1/nope").await.0,
            StatusCode::NOT_FOUND,
            "never the SPA for the API"
        );
        assert_eq!(get("/dav/x").await.0, StatusCode::NOT_FOUND);
    }
}
