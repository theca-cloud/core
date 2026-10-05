//! The first-run setup, through the router, on a fresh database.

use std::sync::Arc;

use axum::Router;
use axum::body::{Body, to_bytes};
use axum::http::{Request, StatusCode};
use serde_json::{Value, json};
use sqlx::PgPool;
use theca_core::api::setup::SetupState;
use theca_core::api::{AppState, router};
use theca_core::settings::Settings;
use theca_core::storage::{Backends, UrlSigner};
use tower::ServiceExt;

const TOKEN: &str = "0123456789abcdef0123456789abcdef";

async fn app(pool: &PgPool, token: Option<&str>) -> Router {
    router(AppState {
        pool: pool.clone(),
        settings: Arc::new(Settings::load(pool).await.unwrap()),
        signer: UrlSigner::new([9; 32], "http://test"),
        backends: Backends::new(pool.clone()),
        cipher: theca_core::keys::DataCipher::new(&[7; 32]),
        setup: SetupState::pending(token.map(str::to_owned)),
    })
}

/// `(status, body, Cookie header from Set-Cookie)`.
async fn send(
    app: &Router,
    method: &str,
    path: &str,
    headers: &[(&str, &str)],
    body: Option<Value>,
) -> (StatusCode, Value, String) {
    let mut request = Request::builder().method(method).uri(path);
    for (k, v) in headers {
        request = request.header(*k, *v);
    }
    if body.is_some() {
        request = request.header("content-type", "application/json");
    }
    let body = body.map_or_else(Body::empty, |b| Body::from(b.to_string()));
    let response = app
        .clone()
        .oneshot(request.body(body).unwrap())
        .await
        .unwrap();
    let status = response.status();
    let cookies = response
        .headers()
        .get_all("set-cookie")
        .iter()
        .map(|v| v.to_str().unwrap().split(';').next().unwrap().to_owned())
        .collect::<Vec<_>>()
        .join("; ");
    let bytes = to_bytes(response.into_body(), usize::MAX).await.unwrap();
    (
        status,
        serde_json::from_slice(&bytes).unwrap_or(Value::Null),
        cookies,
    )
}

fn setup_body(storages: Value) -> Value {
    json!({
        "admin_email": "admin@x.ch",
        "admin_password": "a long password",
        "organization": "Acme",
        "storages": storages,
    })
}

fn filesystem(name: &str, dir: &tempfile::TempDir) -> Value {
    json!({ "name": name, "implementation": "filesystem", "path": dir.path() })
}

async fn setup(app: &Router, token: Option<&str>, body: Value) -> (StatusCode, Value, String) {
    let headers: Vec<(&str, &str)> = token
        .map(|t| ("x-theca-setup-token", t))
        .into_iter()
        .collect();
    send(app, "POST", "/api/v1/setup", &headers, Some(body)).await
}

async fn is_fresh(pool: &PgPool) -> bool {
    theca_core::db::setup::is_fresh(pool).await.unwrap()
}

#[sqlx::test]
async fn fresh_instance_serves_only_the_setup(pool: PgPool) {
    let app = app(&pool, Some(TOKEN)).await;
    let (status, body, _) = send(&app, "GET", "/api/v1/setup", &[], None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body, json!({ "required": true, "token_required": true }));
    let (status, body, _) = send(&app, "GET", "/api/v1/libraries", &[], None).await;
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(body["code"], "setup_required");
    let (status, _, _) = send(
        &app,
        "POST",
        "/api/v1/auth/login",
        &[],
        Some(json!({ "email": "a", "password": "b" })),
    )
    .await;
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
}

#[sqlx::test]
async fn setup_once(pool: PgPool) {
    let app = app(&pool, Some(TOKEN)).await;
    let (main, other) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
    let body = setup_body(json!([
        filesystem("Main", &main),
        filesystem("Other", &other)
    ]));

    // The token is required.
    for token in [None, Some("wrong")] {
        let (status, refused, _) = setup(&app, token, body.clone()).await;
        assert_eq!(status, StatusCode::UNAUTHORIZED);
        assert_eq!(refused["code"], "invalid_setup_token");
    }
    assert!(is_fresh(&pool).await);

    let (status, me, cookies) = setup(&app, Some(TOKEN), body.clone()).await;
    assert_eq!(status, StatusCode::OK, "{me}");
    assert_eq!(me["email"], "admin@x.ch");
    assert_eq!(me["organizations"][0]["name"], "Acme");
    let (admin, verified): (bool, bool) =
        sqlx::query_as("SELECT instance_admin, verified FROM account")
            .fetch_one(&pool)
            .await
            .unwrap();
    assert!(admin && verified);
    let storages: Vec<(String, bool)> =
        sqlx::query_as("SELECT name, is_default FROM storage ORDER BY pk")
            .fetch_all(&pool)
            .await
            .unwrap();
    assert_eq!(storages, [("Main".into(), true), ("Other".into(), false)]);

    // Set up: every route answers, no restart; the setup is closed.
    let (_, status_body, _) = send(&app, "GET", "/api/v1/setup", &[], None).await;
    assert_eq!(
        status_body,
        json!({ "required": false, "token_required": false })
    );
    let (status, again, _) = setup(&app, Some(TOKEN), body).await;
    assert_eq!(status, StatusCode::CONFLICT);
    assert_eq!(again["code"], "already_set_up");

    // The admin is logged in, and libraries land on the default storage.
    let organization = me["organizations"][0]["organization"].clone();
    let create = json!({ "name": "Docs", "organization": organization });
    let (status, library, _) = send(
        &app,
        "POST",
        "/api/v1/libraries",
        &[("cookie", &cookies)],
        Some(create),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{library}");
    let storage: String =
        sqlx::query_scalar("SELECT s.name FROM library l JOIN storage s ON s.pk = l.storage")
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(storage, "Main");
    let login = json!({ "email": "admin@x.ch", "password": "a long password" });
    let (status, _, _) = send(&app, "POST", "/api/v1/auth/login", &[], Some(login)).await;
    assert_eq!(status, StatusCode::OK);
}

#[sqlx::test]
async fn broken_setups_change_nothing(pool: PgPool) {
    let app = app(&pool, Some(TOKEN)).await;
    let dir = tempfile::tempdir().unwrap();
    let file = tempfile::NamedTempFile::new().unwrap();
    let under_a_file =
        json!({ "name": "Bad", "implementation": "filesystem", "path": file.path().join("x") });
    let no_key = json!({
        "name": "S3", "implementation": "s3", "host": "http://localhost:1", "region": "r", "bucket": "b",
        "access_key_file": "/nonexistent/key", "secret_key_file": "/nonexistent/secret",
    });
    for (storages, code) in [
        (
            json!([filesystem("Good", &dir), under_a_file]),
            "storage_unusable",
        ),
        (json!([no_key]), "storage_unusable"),
        (
            json!([filesystem("Same", &dir), filesystem("SAME", &dir)]),
            "storage_name_taken",
        ),
        (json!([]), "invalid_setup"),
    ] {
        let (_, refused, _) = setup(&app, Some(TOKEN), setup_body(storages)).await;
        assert_eq!(refused["code"], code, "{refused}");
        assert!(is_fresh(&pool).await);
    }
    let mut short = setup_body(json!([filesystem("Good", &dir)]));
    short["admin_password"] = json!("short");
    let (_, refused, _) = setup(&app, Some(TOKEN), short).await;
    assert_eq!(refused["code"], "invalid_setup");
    let count: i64 = sqlx::query_scalar(
        "SELECT (SELECT count(*) FROM account) + (SELECT count(*) FROM storage)",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(count, 0);
}

#[sqlx::test]
async fn open_setup_needs_no_token(pool: PgPool) {
    let app = app(&pool, None).await;
    let (_, status, _) = send(&app, "GET", "/api/v1/setup", &[], None).await;
    assert_eq!(status["token_required"], false);
    let dir = tempfile::tempdir().unwrap();
    let (status, _, _) = setup(&app, None, setup_body(json!([filesystem("Main", &dir)]))).await;
    assert_eq!(status, StatusCode::OK);
}

#[sqlx::test]
async fn concurrent_setups_one_wins(pool: PgPool) {
    let app = app(&pool, Some(TOKEN)).await;
    let dir = tempfile::tempdir().unwrap();
    let body = setup_body(json!([filesystem("Main", &dir)]));
    let (a, b) = tokio::join!(
        setup(&app, Some(TOKEN), body.clone()),
        setup(&app, Some(TOKEN), body)
    );
    let mut statuses = [a.0, b.0];
    statuses.sort();
    assert_eq!(statuses, [StatusCode::OK, StatusCode::CONFLICT]);
    let admins: i64 = sqlx::query_scalar("SELECT count(*) FROM account")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(admins, 1);
}
