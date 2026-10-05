//! Rate limits, the client's address behind trusted proxies, and metrics (13b).

#![allow(dead_code, unused_imports)] // the shared harness has more than these tests use

include!("common/harness.rs");

use std::net::SocketAddr;

use axum::extract::ConnectInfo;

/// Small limits, set before the app starts.
async fn limited_world(pool: PgPool, limits: &[(&str, &str)]) -> World {
    for (key, value) in limits {
        sqlx::query("INSERT INTO setting (key, value) VALUES ($1, $2)")
            .bind(key)
            .bind(value)
            .execute(&pool)
            .await
            .unwrap();
    }
    world(Kind::Filesystem, pool).await
}

/// A login from `peer` (with an optional `X-Forwarded-For`): `(status, Retry-After)`.
async fn login(
    app: &Router,
    peer: &str,
    forwarded: Option<&str>,
    email: &str,
    password: &str,
) -> (StatusCode, Option<String>) {
    let body = json!({ "email": email, "password": password }).to_string();
    let mut request =
        Request::post("/api/v1/auth/login").header("content-type", "application/json");
    if let Some(forwarded) = forwarded {
        request = request.header("x-forwarded-for", forwarded);
    }
    let mut request = request.body(Body::from(body)).unwrap();
    let peer: SocketAddr = format!("{peer}:4000").parse().unwrap();
    request.extensions_mut().insert(ConnectInfo(peer));
    let response = app.clone().oneshot(request).await.unwrap();
    let retry = response
        .headers()
        .get("retry-after")
        .map(|v| v.to_str().unwrap().to_owned());
    (response.status(), retry)
}

#[sqlx::test]
async fn failed_logins_slow_one_account(pool: PgPool) {
    let w = limited_world(pool, &[("rate_limit_login_account", "3/300")]).await;
    // A user logging in normally is never slowed down.
    for _ in 0..5 {
        assert_eq!(
            login(&w.app, "1.1.1.1", None, "a@x.ch", PASSWORD).await.0,
            StatusCode::OK
        );
    }
    for _ in 0..3 {
        assert_eq!(
            login(&w.app, "1.1.1.1", None, "a@x.ch", "wrong").await.0,
            StatusCode::UNAUTHORIZED
        );
    }
    // Then even the right password waits, from anywhere; other accounts don't.
    let (status, retry) = login(&w.app, "2.2.2.2", None, "A@x.ch", PASSWORD).await;
    assert_eq!(status, StatusCode::TOO_MANY_REQUESTS);
    let retry: u64 = retry.unwrap().parse().unwrap();
    assert!((1..=300).contains(&retry));
    assert_eq!(
        login(&w.app, "2.2.2.2", None, "b@x.ch", PASSWORD).await.0,
        StatusCode::OK
    );
}

#[sqlx::test]
async fn logins_per_address(pool: PgPool) {
    let w = limited_world(pool, &[("rate_limit_login_ip", "2/300")]).await;
    for _ in 0..2 {
        assert_eq!(
            login(&w.app, "1.2.3.4", None, "a@x.ch", PASSWORD).await.0,
            StatusCode::OK
        );
    }
    assert_eq!(
        login(&w.app, "1.2.3.4", None, "a@x.ch", PASSWORD).await.0,
        StatusCode::TOO_MANY_REQUESTS
    );
    assert_eq!(
        login(&w.app, "5.6.7.8", None, "a@x.ch", PASSWORD).await.0,
        StatusCode::OK
    );
}

/// Behind a declared proxy, the client is the rightmost address it forwarded
/// that isn't itself a proxy; an undeclared peer's header is ignored.
#[sqlx::test]
async fn forwarded_only_through_trusted_proxies(pool: PgPool) {
    let w = limited_world(pool, &[("rate_limit_login_ip", "1/300")]).await;
    let mut state = state(&w.pool).await;
    state.proxies = std::sync::Arc::new(vec!["10.0.0.0/8".parse().unwrap()]);
    let app = router(state);

    // Through the proxy: "6.6.6.6" was written by the client, 1.2.3.4 is it.
    assert_eq!(
        login(
            &app,
            "10.0.0.5",
            Some("6.6.6.6, 1.2.3.4"),
            "a@x.ch",
            PASSWORD
        )
        .await
        .0,
        StatusCode::OK
    );
    assert_eq!(
        login(
            &app,
            "10.0.0.9",
            Some("1.2.3.4, 10.0.0.7"),
            "a@x.ch",
            PASSWORD
        )
        .await
        .0,
        StatusCode::TOO_MANY_REQUESTS,
        "1.2.3.4 again, through two proxies"
    );
    // Not through a proxy: the header is the client's own claim, ignored.
    assert_eq!(
        login(&app, "9.9.9.9", Some("1.2.3.4"), "a@x.ch", PASSWORD)
            .await
            .0,
        StatusCode::OK
    );
}

#[sqlx::test]
async fn emails_per_address(pool: PgPool) {
    let w = limited_world(pool, &[("rate_limit_email_address", "2/3600")]).await;
    let anonymous = Client {
        app: w.app.clone(),
        account: Uuid::nil(),
        org: Uuid::nil(),
        cookies: String::new(),
        puts: 0,
    };
    for _ in 0..2 {
        anonymous
            .json::<Value>(
                "POST",
                "/api/v1/auth/password/forgot",
                &json!({ "email": "a@x.ch" }),
            )
            .await
            .unwrap();
    }
    let refused = anonymous
        .json::<Value>(
            "POST",
            "/api/v1/auth/password/forgot",
            &json!({ "email": "A@X.ch" }),
        )
        .await
        .unwrap_err();
    assert_eq!(
        (refused.0, refused.1["code"].as_str()),
        (StatusCode::TOO_MANY_REQUESTS, Some("rate_limited"))
    );
    let queued: i64 = sqlx::query_scalar("SELECT count(*) FROM email")
        .fetch_one(&w.pool)
        .await
        .unwrap();
    assert_eq!(queued, 2, "the inbox isn't flooded");
}

/// Metrics: served by their own router (the METRICS_ADDR listener), with
/// bounded labels; never on the public router.
#[sqlx::test]
async fn metrics(pool: PgPool) {
    let handle = theca_core::telemetry::install_recorder();
    let w = world(Kind::Filesystem, pool).await;
    let alice = w.as_user(1).await;
    let docs = alice.create_library("Docs").await;
    alice
        .get::<Value>(&format!("/api/v1/nodes/{}/children", docs.root))
        .await
        .unwrap();

    let metrics = theca_core::telemetry::router(handle, w.pool.clone());
    let response = metrics
        .oneshot(Request::get("/metrics").body(Body::empty()).unwrap())
        .await
        .unwrap();
    let text = String::from_utf8(
        to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap()
            .to_vec(),
    )
    .unwrap();
    assert!(
        text.contains(r#"route="/api/v1/nodes/{id}/children""#),
        "{text}"
    );
    assert!(text.contains("theca_db_connections"));
    assert!(!text.contains(&docs.root.to_string()), "no IDs in labels");

    let public = w
        .app
        .clone()
        .oneshot(Request::get("/metrics").body(Body::empty()).unwrap())
        .await
        .unwrap();
    assert_eq!(public.status(), StatusCode::NOT_FOUND);
}
