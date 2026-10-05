//! Sessions through the router: login, transparent refresh, logout, the
//! sessions list, CSRF, accounts in several organizations, restricted sessions.

use std::sync::Arc;

use axum::Router;
use axum::body::{Body, to_bytes};
use axum::http::{Request, StatusCode};
use serde_json::{Value, json};
use sqlx::PgPool;
use theca_common::api;
use theca_core::api::{AppState, router};
use theca_core::auth::hash_password;
use theca_core::settings::Settings;
use theca_core::storage::{Backends, UrlSigner};
use tower::ServiceExt;
use uuid::Uuid;

const PASSWORD: &str = "correct horse battery staple";

/// Accounts a (joined in organizations one and two) and b (joined in one,
/// only invited to two); a filesystem storage for libraries.
struct World {
    app: Router,
    pool: PgPool,
    organizations: [Uuid; 2],
    _dir: tempfile::TempDir,
}

async fn world(pool: PgPool) -> World {
    theca_core::install_crypto_provider();
    let dir = tempfile::tempdir().unwrap();
    let hash = hash_password(PASSWORD);
    sqlx::raw_sql(&format!(
        r#"
        BEGIN;
        INSERT INTO account (pk, email) OVERRIDING SYSTEM VALUE VALUES (1, 'a@x.ch'), (2, 'b@x.ch');
        INSERT INTO identity (account, kind, details)
            SELECT pk, 'password', jsonb_build_object('hash', '{hash}') FROM account;
        INSERT INTO organization (pk, name, owner) OVERRIDING SYSTEM VALUE VALUES (1, 'one', 1), (2, 'two', 2);
        INSERT INTO "user" (pk, account, organization, joined_at) OVERRIDING SYSTEM VALUE
            VALUES (1, 1, 1, now()), (2, 1, 2, now()), (3, 2, 1, now()), (4, 2, 2, NULL);
        INSERT INTO storage (name, implementation, details, is_default)
            VALUES ('main', 'filesystem', '{{"path": "{}"}}', true);
        COMMIT;
        "#,
        dir.path().display()
    ))
    .execute(&pool)
    .await
    .unwrap();
    let organizations: Vec<Uuid> = sqlx::query_scalar("SELECT uuid FROM organization ORDER BY pk")
        .fetch_all(&pool)
        .await
        .unwrap();
    World {
        app: router(AppState {
            pool: pool.clone(),
            settings: Arc::new(Settings::load(&pool).await.unwrap()),
            signer: UrlSigner::new([9; 32], "http://test"),
            backends: Backends::new(pool.clone()),
            cipher: theca_core::keys::DataCipher::new(&[7; 32]),
            setup: theca_core::api::setup::SetupState::done(),
        }),
        pool,
        organizations: [organizations[0], organizations[1]],
        _dir: dir,
    }
}

#[derive(Debug)]
struct Response {
    status: StatusCode,
    location: Option<String>,
    /// `name=value` of each `Set-Cookie`.
    cookies: Vec<String>,
    body: Value,
}

impl Response {
    fn cookie(&self, name: &str) -> Option<&str> {
        self.cookies
            .iter()
            .find_map(|c| c.strip_prefix(name)?.strip_prefix('='))
    }

    /// The `Cookie` header a browser would send back.
    fn session(&self) -> String {
        self.cookies.join("; ")
    }

    fn code(&self) -> &str {
        self.body["code"].as_str().unwrap_or_default()
    }
}

impl World {
    async fn send(&self, request: axum::http::request::Builder, body: Option<Value>) -> Response {
        let body = body.map_or_else(Body::empty, |b| Body::from(b.to_string()));
        let response = self
            .app
            .clone()
            .oneshot(request.body(body).unwrap())
            .await
            .unwrap();
        let status = response.status();
        let location = response
            .headers()
            .get("location")
            .map(|v| v.to_str().unwrap().to_owned());
        let cookies = response
            .headers()
            .get_all("set-cookie")
            .iter()
            .map(|v| v.to_str().unwrap().split(';').next().unwrap().to_owned())
            .collect();
        let bytes = to_bytes(response.into_body(), usize::MAX).await.unwrap();
        Response {
            status,
            location,
            cookies,
            body: serde_json::from_slice(&bytes).unwrap_or(Value::Null),
        }
    }

    async fn login(&self, email: &str, password: &str) -> Response {
        let request =
            Request::post("/api/v1/auth/login").header("content-type", "application/json");
        self.send(
            request,
            Some(json!({ "email": email, "password": password })),
        )
        .await
    }

    /// A request with a `Cookie` header (or an `Authorization` header).
    async fn with(
        &self,
        method: &str,
        path: &str,
        header: (&str, &str),
        body: Option<Value>,
    ) -> Response {
        let mut request = Request::builder()
            .method(method)
            .uri(path)
            .header(header.0, header.1);
        if body.is_some() {
            request = request.header("content-type", "application/json");
        }
        self.send(request, body).await
    }

    async fn get(&self, path: &str, cookies: &str) -> Response {
        self.with("GET", path, ("cookie", cookies), None).await
    }

    async fn create_library(&self, cookies: &str, organization: Uuid, name: &str) -> Response {
        let body = json!({ "name": name, "organization": organization });
        self.with("POST", "/api/v1/libraries", ("cookie", cookies), Some(body))
            .await
    }
}

#[sqlx::test]
async fn login_and_wrong_credentials(pool: PgPool) {
    let w = world(pool).await;
    let ok = w.login("A@x.ch", PASSWORD).await;
    assert_eq!(ok.status, StatusCode::OK);
    assert!(ok.cookie("theca_session").is_some());
    let me: api::Me = serde_json::from_value(ok.body.clone()).unwrap();
    assert_eq!(me.email, "a@x.ch");
    assert_eq!(me.organizations.len(), 2);

    let me = w.get("/api/v1/me", &ok.session()).await;
    assert_eq!(me.status, StatusCode::OK);
    assert_eq!(me.body["email"], "a@x.ch");

    // Wrong password and unknown email: the same error.
    for (email, password) in [("a@x.ch", "wrong"), ("nobody@x.ch", PASSWORD)] {
        let refused = w.login(email, password).await;
        assert_eq!(refused.status, StatusCode::UNAUTHORIZED);
        assert_eq!(refused.code(), "invalid_credentials");
        assert!(refused.cookies.is_empty());
    }

    // No cookies, garbage cookies.
    assert_eq!(
        w.get("/api/v1/me", "").await.status,
        StatusCode::UNAUTHORIZED
    );
    let garbage = "theca_session=th_nope.nope";
    assert_eq!(
        w.get("/api/v1/me", garbage).await.status,
        StatusCode::UNAUTHORIZED
    );
}

#[sqlx::test]
async fn sliding_expiry(pool: PgPool) {
    let w = world(pool).await;
    let session = w.login("a@x.ch", PASSWORD).await.session();
    let pool = &w.pool;
    let age = || async move {
        sqlx::query_scalar::<_, f64>(
            "SELECT extract(epoch FROM now() - last_seen)::float8 FROM token",
        )
        .fetch_one(pool)
        .await
        .unwrap()
    };
    let set_age = |interval: &'static str| async move {
        sqlx::query(&format!(
            "UPDATE token SET last_seen = now() - interval '{interval}'"
        ))
        .execute(pool)
        .await
        .unwrap();
    };

    // Used a minute ago: last_seen isn't rewritten on every request.
    set_age("1 minute").await;
    assert_eq!(w.get("/api/v1/me", &session).await.status, StatusCode::OK);
    assert!(age().await >= 60.0);

    // Used 6 days ago: still valid, and slides to now.
    set_age("6 days").await;
    assert_eq!(w.get("/api/v1/me", &session).await.status, StatusCode::OK);
    assert!(age().await < 60.0);

    // Unused for more than 7 days: expired.
    set_age("8 days").await;
    assert_eq!(
        w.get("/api/v1/me", &session).await.status,
        StatusCode::UNAUTHORIZED
    );
}

#[sqlx::test]
async fn logout_and_sessions(pool: PgPool) {
    let w = world(pool).await;
    let laptop = w.login("a@x.ch", PASSWORD).await;
    let phone = w.login("a@x.ch", PASSWORD).await;

    let sessions = w.get("/api/v1/auth/sessions", &laptop.session()).await;
    let sessions: Vec<api::Session> = serde_json::from_value(sessions.body).unwrap();
    assert_eq!(sessions.len(), 2);
    assert_eq!(sessions.iter().filter(|s| s.current).count(), 1);
    let phone_id = sessions.iter().find(|s| !s.current).unwrap().id;

    // Revoke the phone from the laptop: it stops working at once.
    let revoked = w
        .with(
            "DELETE",
            &format!("/api/v1/auth/sessions/{phone_id}"),
            ("cookie", &laptop.session()),
            None,
        )
        .await;
    assert_eq!(revoked.status, StatusCode::NO_CONTENT);
    assert_eq!(
        w.get("/api/v1/me", &phone.session()).await.status,
        StatusCode::UNAUTHORIZED
    );
    let again = w
        .with(
            "DELETE",
            &format!("/api/v1/auth/sessions/{phone_id}"),
            ("cookie", &laptop.session()),
            None,
        )
        .await;
    assert_eq!(again.status, StatusCode::NOT_FOUND);

    // b can't revoke a's sessions.
    let b = w.login("b@x.ch", PASSWORD).await;
    let laptop_id = sessions.iter().find(|s| s.current).unwrap().id;
    let foreign = w
        .with(
            "DELETE",
            &format!("/api/v1/auth/sessions/{laptop_id}"),
            ("cookie", &b.session()),
            None,
        )
        .await;
    assert_eq!(foreign.status, StatusCode::NOT_FOUND);

    // Logout: cookie cleared, the session revoked at once.
    let logout = w
        .with(
            "POST",
            "/api/v1/auth/logout",
            ("cookie", &laptop.session()),
            Some(json!({})),
        )
        .await;
    assert_eq!(logout.status, StatusCode::NO_CONTENT);
    assert_eq!(logout.cookie("theca_session"), Some(""));
    assert_eq!(
        w.get("/api/v1/me", &laptop.session()).await.status,
        StatusCode::UNAUTHORIZED
    );
}

#[sqlx::test]
async fn csrf(pool: PgPool) {
    let w = world(pool).await;
    let a = w.login("a@x.ch", PASSWORD).await;
    let body = json!({ "name": "Docs", "organization": w.organizations[0] }).to_string();
    let form = |extra: Option<&str>| {
        let mut request = Request::post("/api/v1/libraries")
            .header("cookie", a.session())
            .header("content-type", "text/plain");
        if let Some(header) = extra {
            request = request.header(header, "1");
        }
        request
    };

    // What a cross-site form can send: refused before the handler.
    let forged = w.send(form(None), None).await;
    assert_eq!(forged.status, StatusCode::FORBIDDEN);
    assert_eq!(forged.code(), "csrf");

    // Our header gets past the check (then the handler wants JSON).
    let request = form(Some("x-theca-csrf")).body(Body::from(body)).unwrap();
    let status = w.app.clone().oneshot(request).await.unwrap().status();
    assert_ne!(status, StatusCode::FORBIDDEN);

    // Bearer requests can't be forged by another site: no check.
    let token = a.cookie("theca_session").unwrap();
    let bearer = w
        .send(
            Request::post("/api/v1/libraries")
                .header("authorization", format!("Bearer {token}"))
                .header("content-type", "text/plain"),
            None,
        )
        .await;
    assert_ne!(bearer.code(), "csrf");
}

#[sqlx::test]
async fn one_session_several_organizations(pool: PgPool) {
    let w = world(pool).await;
    let a = w.login("a@x.ch", PASSWORD).await.session();
    let b = w.login("b@x.ch", PASSWORD).await.session();
    let [one, two] = w.organizations;

    // a works on both organizations with the same session.
    assert_eq!(
        w.create_library(&a, one, "Docs").await.status,
        StatusCode::CREATED
    );
    assert_eq!(
        w.create_library(&a, two, "Docs").await.status,
        StatusCode::CREATED
    );
    let listed: Vec<api::Library> =
        serde_json::from_value(w.get("/api/v1/libraries", &a).await.body).unwrap();
    let mut orgs: Vec<Uuid> = listed.iter().map(|l| l.organization).collect();
    orgs.sort();
    let mut expected = vec![one, two];
    expected.sort();
    assert_eq!(orgs, expected);

    // b is only invited to two: nothing there is reachable.
    let refused = w.create_library(&b, two, "Mine").await;
    assert_eq!(refused.status, StatusCode::NOT_FOUND);
    let library_of_two = listed.iter().find(|l| l.organization == two).unwrap();
    let children = w
        .get(
            &format!("/api/v1/nodes/{}/children", library_of_two.root),
            &b,
        )
        .await;
    assert_eq!(children.status, StatusCode::NOT_FOUND);
    let listed: Vec<api::Library> =
        serde_json::from_value(w.get("/api/v1/libraries", &b).await.body).unwrap();
    assert!(listed.iter().all(|l| l.organization == one));
}

#[sqlx::test]
async fn restricted_session(pool: PgPool) {
    let w = world(pool).await;
    let a = w.login("a@x.ch", PASSWORD).await.session();
    let [one, two] = w.organizations;
    assert_eq!(
        w.create_library(&a, one, "Docs").await.status,
        StatusCode::CREATED
    );
    assert_eq!(
        w.create_library(&a, two, "Docs").await.status,
        StatusCode::CREATED
    );

    // A session opened through organization two's provider (step 06 part 4
    // opens them; here, directly).
    let account: Uuid = sqlx::query_scalar("SELECT uuid FROM account WHERE pk = 1")
        .fetch_one(&w.pool)
        .await
        .unwrap();
    let (token, _) = theca_core::db::account::create_session(&w.pool, account, Some(2), None, None)
        .await
        .unwrap();
    let bearer = format!("Bearer {}", token.token);
    let auth = ("authorization", bearer.as_str());

    // Only organization two is reachable.
    let listed = w.with("GET", "/api/v1/libraries", auth, None).await;
    let listed: Vec<api::Library> = serde_json::from_value(listed.body).unwrap();
    assert_eq!(listed.len(), 1);
    assert_eq!(listed[0].organization, two);
    let body = |org: Uuid| Some(json!({ "name": "New", "organization": org }));
    assert_eq!(
        w.with("POST", "/api/v1/libraries", auth, body(two))
            .await
            .status,
        StatusCode::CREATED
    );
    assert_eq!(
        w.with("POST", "/api/v1/libraries", auth, body(one))
            .await
            .status,
        StatusCode::NOT_FOUND
    );

    // No account-level actions.
    let sessions = w.with("GET", "/api/v1/auth/sessions", auth, None).await;
    assert_eq!(sessions.status, StatusCode::FORBIDDEN);
    assert_eq!(sessions.code(), "full_session_required");
}

impl World {
    /// Creates an app password with the given session; returns the Bearer
    /// header value (or the refusal).
    async fn app_password(&self, session: (&str, &str), scope: Value) -> Result<String, Response> {
        let created = self
            .with("POST", "/api/v1/auth/app-passwords", session, Some(scope))
            .await;
        if created.status != StatusCode::CREATED {
            return Err(created);
        }
        Ok(format!(
            "Bearer {}",
            created.body["password"].as_str().unwrap()
        ))
    }

    /// The names of the libraries a credential lists.
    async fn library_names(&self, auth: (&str, &str)) -> Vec<String> {
        let listed = self.with("GET", "/api/v1/libraries", auth, None).await;
        let listed: Vec<api::Library> = serde_json::from_value(listed.body).unwrap();
        let mut names: Vec<String> = listed.into_iter().map(|l| l.name).collect();
        names.sort();
        names
    }

    /// Whether a credential can write into a library (an empty lookup needs `write`).
    async fn can_write(&self, auth: (&str, &str), library: Uuid) -> StatusCode {
        let body = json!({ "library": library, "chunks": [] });
        self.with("POST", "/api/v1/chunks/lookup", auth, Some(body))
            .await
            .status
    }
}

#[sqlx::test]
async fn app_password_scopes(pool: PgPool) {
    let w = world(pool).await;
    let session = w.login("a@x.ch", PASSWORD).await.session();
    let a = ("cookie", session.as_str());
    let [one, two] = w.organizations;
    let mut libraries = Vec::new();
    for (org, name) in [(one, "A1"), (one, "A2"), (two, "B1")] {
        let created = w.create_library(&session, org, name).await;
        libraries.push(
            serde_json::from_value::<api::Library>(created.body)
                .unwrap()
                .id,
        );
    }
    let names = |n: &[&str]| n.iter().map(|s| s.to_string()).collect::<Vec<_>>();

    // The whole account.
    let all = w.app_password(a, json!({ "name": "cli" })).await.unwrap();
    assert_eq!(
        w.library_names(("authorization", &all)).await,
        names(&["A1", "A2", "B1"])
    );
    assert_eq!(
        w.can_write(("authorization", &all), libraries[2]).await,
        StatusCode::OK
    );

    // One organization.
    let org = w
        .app_password(a, json!({ "name": "one", "organization": one }))
        .await
        .unwrap();
    let org = ("authorization", org.as_str());
    assert_eq!(w.library_names(org).await, names(&["A1", "A2"]));
    assert_eq!(w.can_write(org, libraries[2]).await, StatusCode::NOT_FOUND);
    assert_eq!(
        w.create_library(&session, one, "A3").await.status,
        StatusCode::CREATED
    );
    let body = json!({ "name": "A4", "organization": one });
    let created = w.with("POST", "/api/v1/libraries", org, Some(body)).await;
    assert_eq!(created.status, StatusCode::CREATED);

    // One library: nothing else, and no new libraries.
    let lib = w
        .app_password(a, json!({ "name": "backup", "library": libraries[0] }))
        .await
        .unwrap();
    let lib = ("authorization", lib.as_str());
    assert_eq!(w.library_names(lib).await, names(&["A1"]));
    assert_eq!(w.can_write(lib, libraries[0]).await, StatusCode::OK);
    assert_eq!(w.can_write(lib, libraries[1]).await, StatusCode::NOT_FOUND);
    let body = json!({ "name": "A5", "organization": one });
    let refused = w.with("POST", "/api/v1/libraries", lib, Some(body)).await;
    assert_eq!(refused.status, StatusCode::NOT_FOUND);

    // Read-only: reads, never writes.
    let ro = w
        .app_password(
            a,
            json!({ "name": "ro", "organization": one, "read_only": true }),
        )
        .await
        .unwrap();
    let ro = ("authorization", ro.as_str());
    assert_eq!(w.library_names(ro).await.len(), 4);
    assert_eq!(w.can_write(ro, libraries[0]).await, StatusCode::FORBIDDEN);
    let body = json!({ "name": "A6", "organization": one });
    let refused = w.with("POST", "/api/v1/libraries", ro, Some(body)).await;
    assert_eq!(refused.status, StatusCode::NOT_FOUND);

    // A library and an organization that don't match, or out of reach.
    let mismatch = json!({ "name": "x", "organization": two, "library": libraries[0] });
    assert_eq!(
        w.app_password(a, mismatch).await.unwrap_err().status,
        StatusCode::NOT_FOUND
    );
    let b = w.login("b@x.ch", PASSWORD).await.session();
    let b = ("cookie", b.as_str());
    let foreign = json!({ "name": "x", "library": libraries[2] });
    assert_eq!(
        w.app_password(b, foreign).await.unwrap_err().status,
        StatusCode::NOT_FOUND
    );
    let invited = json!({ "name": "x", "organization": two });
    assert_eq!(
        w.app_password(b, invited).await.unwrap_err().status,
        StatusCode::NOT_FOUND
    );
    let unnamed = json!({ "name": "  " });
    assert_eq!(
        w.app_password(b, unnamed).await.unwrap_err().code(),
        "invalid_name"
    );
}

#[sqlx::test]
async fn app_passwords_only_reach_data(pool: PgPool) {
    let w = world(pool).await;
    let session = w.login("a@x.ch", PASSWORD).await.session();
    let a = ("cookie", session.as_str());
    let password = w.app_password(a, json!({ "name": "cli" })).await.unwrap();
    let cli = ("authorization", password.as_str());
    assert_eq!(
        w.with("GET", "/api/v1/me", cli, None).await.status,
        StatusCode::OK
    );

    // No account actions, no more app passwords, no logout.
    let sessions = w.with("GET", "/api/v1/auth/sessions", cli, None).await;
    assert_eq!(sessions.code(), "full_session_required");
    let more = w
        .app_password(cli, json!({ "name": "more" }))
        .await
        .unwrap_err();
    assert_eq!(more.code(), "session_required");
    let listed = w.with("GET", "/api/v1/auth/app-passwords", cli, None).await;
    assert_eq!(listed.code(), "session_required");
    let logout = w
        .with("POST", "/api/v1/auth/logout", cli, Some(json!({})))
        .await;
    assert_eq!(logout.status, StatusCode::FORBIDDEN);

    // Not a login password either.
    let token = password.strip_prefix("Bearer ").unwrap();
    assert_eq!(
        w.login("a@x.ch", token).await.status,
        StatusCode::UNAUTHORIZED
    );

    // Listed (never with the secret), then revoked: refused at once.
    let listed = w.with("GET", "/api/v1/auth/app-passwords", a, None).await;
    let listed: Vec<api::AppPassword> = serde_json::from_value(listed.body).unwrap();
    assert_eq!(listed.len(), 1);
    assert_eq!(listed[0].name, "cli");
    let path = format!("/api/v1/auth/app-passwords/{}", listed[0].id);
    let b = w.login("b@x.ch", PASSWORD).await.session();
    let foreign = w.with("DELETE", &path, ("cookie", &b), None).await;
    assert_eq!(foreign.status, StatusCode::NOT_FOUND);
    assert_eq!(
        w.with("DELETE", &path, a, None).await.status,
        StatusCode::NO_CONTENT
    );
    assert_eq!(
        w.with("GET", "/api/v1/me", cli, None).await.status,
        StatusCode::UNAUTHORIZED
    );
}

#[sqlx::test]
async fn app_passwords_from_a_restricted_session(pool: PgPool) {
    let w = world(pool).await;
    let full = w.login("a@x.ch", PASSWORD).await.session();
    let [one, two] = w.organizations;
    w.create_library(&full, one, "A1").await;
    w.create_library(&full, two, "B1").await;
    let account: Uuid = sqlx::query_scalar("SELECT uuid FROM account WHERE pk = 1")
        .fetch_one(&w.pool)
        .await
        .unwrap();
    let (token, _) = theca_core::db::account::create_session(&w.pool, account, Some(2), None, None)
        .await
        .unwrap();
    let bearer = format!("Bearer {}", token.token);
    let restricted = ("authorization", bearer.as_str());

    // Unscoped: limited to the session's organization anyway.
    let password = w
        .app_password(restricted, json!({ "name": "sso cli" }))
        .await
        .unwrap();
    assert_eq!(
        w.library_names(("authorization", &password)).await,
        vec!["B1".to_owned()]
    );
    let outside = json!({ "name": "x", "organization": one });
    assert_eq!(
        w.app_password(restricted, outside)
            .await
            .unwrap_err()
            .status,
        StatusCode::NOT_FOUND
    );

    // It only sees app passwords of its organization; the full session sees all.
    w.app_password(("cookie", &full), json!({ "name": "account-wide" }))
        .await
        .unwrap();
    let count = |auth: (&'static str, String)| {
        let w = &w;
        async move {
            let listed = w
                .with("GET", "/api/v1/auth/app-passwords", (auth.0, &auth.1), None)
                .await;
            listed.body.as_array().unwrap().len()
        }
    };
    assert_eq!(count(("authorization", bearer.clone())).await, 1);
    assert_eq!(count(("cookie", full.clone())).await, 2);
}

#[sqlx::test]
async fn session_lifetime_settings(pool: PgPool) {
    sqlx::query(
        "INSERT INTO setting (key, value) VALUES
            ('session_idle_lifetime_secs', '3600'), ('session_max_lifetime_secs', '86400')",
    )
    .execute(&pool)
    .await
    .unwrap();
    let w = world(pool).await;
    let session = w.login("a@x.ch", PASSWORD).await.session();
    let age = |last_seen: &str, created: &str| {
        let sql = format!(
            "UPDATE token SET last_seen = now() - interval '{last_seen}', \
                              created_at = now() - interval '{created}'"
        );
        let pool = &w.pool;
        async move { sqlx::query(&sql).execute(pool).await.unwrap() }
    };
    let me = || w.get("/api/v1/me", &session);

    // Idle for 2 hours: past the 1-hour idle lifetime.
    age("2 hours", "2 hours").await;
    assert_eq!(me().await.status, StatusCode::UNAUTHORIZED);

    // Used every few minutes, but logged in 2 days ago: past the 1-day cap.
    age("1 minute", "23 hours").await;
    assert_eq!(me().await.status, StatusCode::OK);
    age("1 minute", "2 days").await;
    assert_eq!(me().await.status, StatusCode::UNAUTHORIZED);
}

/// The code an authenticator app shows `offset` seconds from now.
fn totp_code(base32: &str, offset: i64) -> String {
    let secret = totp_rs::Secret::try_from_base32(base32).unwrap();
    let totp = totp_rs::Builder::new().with_secret(secret).build().unwrap();
    totp.generate(now_secs().saturating_add_signed(offset))
        .to_string()
}

fn now_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs()
}

impl World {
    async fn login_with(&self, password: &str, totp: Option<&str>) -> Response {
        let request =
            Request::post("/api/v1/auth/login").header("content-type", "application/json");
        let body = json!({ "email": "a@x.ch", "password": password, "totp": totp });
        self.send(request, Some(body)).await
    }
}

#[sqlx::test]
async fn totp(pool: PgPool) {
    let w = world(pool).await;
    // Codes for the previous, current and next 30-second steps are all valid:
    // start early in a step, so the server's step doesn't change mid-test.
    if now_secs() % 30 > 20 {
        tokio::time::sleep(std::time::Duration::from_secs(31 - now_secs() % 30)).await;
    }
    let session = w.login("a@x.ch", PASSWORD).await.session();
    let a = ("cookie", session.as_str());
    let post = |path: &'static str, body: Value| {
        let w = &w;
        async move { w.with("POST", path, a, Some(body)).await }
    };

    // Set up: not required until confirmed.
    let setup: api::TotpSetup =
        serde_json::from_value(post("/api/v1/auth/totp", json!({})).await.body).unwrap();
    assert!(setup.uri.starts_with("otpauth://totp/Theca:a%40x.ch?"));
    assert_eq!(w.login_with(PASSWORD, None).await.status, StatusCode::OK);
    let wrong = post("/api/v1/auth/totp/confirm", json!({ "code": "000000" })).await;
    if totp_code(&setup.secret, 0) != "000000" {
        assert_eq!(wrong.code(), "invalid_totp");
    }
    let confirmed = post(
        "/api/v1/auth/totp/confirm",
        json!({ "code": totp_code(&setup.secret, -30) }),
    )
    .await;
    let recovery: api::RecoveryCodes = serde_json::from_value(confirmed.body).unwrap();
    assert_eq!(recovery.codes.len(), 10);
    assert_eq!(w.get("/api/v1/me", &session).await.body["totp"], true);
    assert_eq!(
        post("/api/v1/auth/totp", json!({})).await.code(),
        "totp_enabled"
    );

    // Stored encrypted: the database alone doesn't reveal the secret.
    let stored: String =
        sqlx::query_scalar("SELECT details->>'secret' FROM identity WHERE kind = 'totp'")
            .fetch_one(&w.pool)
            .await
            .unwrap();
    let raw = totp_rs::Secret::try_from_base32(&setup.secret).unwrap();
    use base64::Engine;
    let stored = base64::engine::general_purpose::STANDARD
        .decode(stored)
        .unwrap();
    assert!(
        !stored
            .windows(raw.as_bytes().len())
            .any(|w| w == raw.as_bytes())
    );

    // Login: the password alone isn't enough; codes work once.
    let required = w.login_with(PASSWORD, None).await;
    assert_eq!(required.code(), "totp_required");
    assert!(required.cookies.is_empty());
    let wrong_password = w
        .login_with("wrong", Some(&totp_code(&setup.secret, 0)))
        .await;
    assert_eq!(wrong_password.code(), "invalid_credentials");
    let replayed = w
        .login_with(PASSWORD, Some(&totp_code(&setup.secret, -30)))
        .await;
    assert_eq!(
        replayed.code(),
        "invalid_totp",
        "the confirmation code was used"
    );
    assert_eq!(
        w.login_with(PASSWORD, Some(&totp_code(&setup.secret, 0)))
            .await
            .status,
        StatusCode::OK
    );
    let replayed = w
        .login_with(PASSWORD, Some(&totp_code(&setup.secret, 0)))
        .await;
    assert_eq!(replayed.code(), "invalid_totp");

    // Recovery codes: once each, typed loosely.
    let first = recovery.codes[0].to_uppercase().replace('-', " ");
    assert_eq!(
        w.login_with(PASSWORD, Some(&first)).await.status,
        StatusCode::OK
    );
    assert_eq!(
        w.login_with(PASSWORD, Some(&first)).await.code(),
        "invalid_totp"
    );

    // New recovery codes: the old ones stop working.
    let renewed = post(
        "/api/v1/auth/totp/recovery-codes",
        json!({ "code": totp_code(&setup.secret, 30) }),
    )
    .await;
    let renewed: api::RecoveryCodes = serde_json::from_value(renewed.body).unwrap();
    let old = w.login_with(PASSWORD, Some(&recovery.codes[1])).await;
    assert_eq!(old.code(), "invalid_totp");

    // Disable (a lost phone: with a recovery code).
    let disabled = post(
        "/api/v1/auth/totp/disable",
        json!({ "code": renewed.codes[0] }),
    )
    .await;
    assert_eq!(disabled.status, StatusCode::NO_CONTENT);
    assert_eq!(w.login_with(PASSWORD, None).await.status, StatusCode::OK);
    assert_eq!(w.get("/api/v1/me", &session).await.body["totp"], false);
    let gone = w.login_with(PASSWORD, Some(&renewed.codes[1])).await;
    assert_eq!(gone.status, StatusCode::OK, "no TOTP: the code is ignored");
    let left: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM token WHERE usage = 'recovery_code' AND revoked_at IS NULL",
    )
    .fetch_one(&w.pool)
    .await
    .unwrap();
    assert_eq!(left, 0);
}

#[sqlx::test]
async fn totp_needs_a_full_session(pool: PgPool) {
    let w = world(pool).await;
    let session = w.login("a@x.ch", PASSWORD).await.session();
    let password = w
        .app_password(("cookie", &session), json!({ "name": "cli" }))
        .await
        .unwrap();
    let refused = w
        .with(
            "POST",
            "/api/v1/auth/totp",
            ("authorization", &password),
            Some(json!({})),
        )
        .await;
    assert_eq!(refused.code(), "full_session_required");
}

// OIDC, against a fake provider served by the test (HS256 ID tokens signed
// with the client secret, as the spec allows).

const IDP_SECRET: &str = "the client secret";

/// What the fake provider asserts at the next token request.
#[derive(Default)]
struct Assertion {
    subject: String,
    email: String,
    verified: bool,
    nonce: String,
}

struct FakeIdp {
    issuer: String,
    assertion: Arc<std::sync::Mutex<Assertion>>,
    secret_file: tempfile::NamedTempFile,
}

async fn fake_idp() -> FakeIdp {
    use axum::routing::{get, post};
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let issuer = format!("http://{}", listener.local_addr().unwrap());
    let assertion = Arc::new(std::sync::Mutex::new(Assertion::default()));
    let discovery = json!({
        "issuer": issuer,
        "authorization_endpoint": format!("{issuer}/authorize"),
        "token_endpoint": format!("{issuer}/token"),
        "jwks_uri": format!("{issuer}/jwks"),
        "response_types_supported": ["code"],
        "subject_types_supported": ["public"],
        "id_token_signing_alg_values_supported": ["HS256"],
    });
    let (iss, shared) = (issuer.clone(), assertion.clone());
    let app = Router::new()
        .route(
            "/.well-known/openid-configuration",
            get(move || async move { axum::Json(discovery) }),
        )
        .route("/jwks", get(|| async { axum::Json(json!({ "keys": [] })) }))
        .route(
            "/token",
            post(move || async move {
                let a = shared.lock().unwrap();
                let claims = json!({
                    "iss": iss, "sub": a.subject, "aud": "theca", "nonce": a.nonce,
                    "iat": now_secs(), "exp": now_secs() + 300,
                    "email": a.email, "email_verified": a.verified,
                });
                axum::Json(json!({
                    "access_token": "x", "token_type": "Bearer", "expires_in": 300,
                    "id_token": hs256(&claims),
                }))
            }),
        );
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    let secret_file = tempfile::NamedTempFile::new().unwrap();
    std::fs::write(secret_file.path(), IDP_SECRET).unwrap();
    FakeIdp {
        issuer,
        assertion,
        secret_file,
    }
}

fn hs256(claims: &Value) -> String {
    use base64::Engine;
    use hmac::{KeyInit, Mac};
    let b64 = |bytes: &[u8]| base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(bytes);
    let header = b64(br#"{"alg":"HS256","typ":"JWT"}"#);
    let payload = b64(claims.to_string().as_bytes());
    let mut mac = hmac::Hmac::<sha2::Sha256>::new_from_slice(IDP_SECRET.as_bytes()).unwrap();
    mac.update(format!("{header}.{payload}").as_bytes());
    format!("{header}.{payload}.{}", b64(&mac.finalize().into_bytes()))
}

impl World {
    /// Registers the fake provider, instance-wide or for an organization (pk).
    async fn provider(&self, idp: &FakeIdp, name: &str, organization: Option<i64>) -> Uuid {
        sqlx::query_scalar(
            "INSERT INTO identity_provider (organization, name, issuer, client_id, client_secret_file)
             VALUES ($1, $2, $3, 'theca', $4) RETURNING uuid",
        )
        .bind(organization)
        .bind(name)
        .bind(&idp.issuer)
        .bind(idp.secret_file.path().to_str().unwrap())
        .fetch_one(&self.pool)
        .await
        .unwrap()
    }

    /// Goes to the provider (`flow`: the start or link response), which
    /// asserts `(subject, email, verified)`, and comes back to the callback.
    async fn oidc_return(
        &self,
        provider: Uuid,
        idp: &FakeIdp,
        flow: &Response,
        url: &str,
        who: (&str, &str, bool),
    ) -> Response {
        let param = |name: &str| {
            url.split(['?', '&'])
                .find_map(|p| p.strip_prefix(&format!("{name}=")))
                .unwrap()
                .to_owned()
        };
        assert!(url.starts_with(&format!("{}/authorize?", idp.issuer)));
        *idp.assertion.lock().unwrap() = Assertion {
            subject: who.0.into(),
            email: who.1.into(),
            verified: who.2,
            nonce: param("nonce"),
        };
        let callback = format!(
            "/api/v1/auth/oidc/{provider}/callback?code=c&state={}",
            param("state")
        );
        self.get(&callback, &flow.session()).await
    }

    /// A full OIDC login: start, provider, callback.
    async fn oidc_login(&self, provider: Uuid, idp: &FakeIdp, who: (&str, &str, bool)) -> Response {
        let start = self
            .get(&format!("/api/v1/auth/oidc/{provider}/start"), "")
            .await;
        assert_eq!(start.status, StatusCode::SEE_OTHER);
        let url = start.location.clone().unwrap();
        self.oidc_return(provider, idp, &start, &url, who).await
    }

    /// Links an identity to the account of `session`.
    async fn oidc_link(
        &self,
        session: &str,
        provider: Uuid,
        idp: &FakeIdp,
        who: (&str, &str, bool),
    ) -> Response {
        let path = format!("/api/v1/auth/oidc/{provider}/link");
        let flow = self
            .with("POST", &path, ("cookie", session), Some(json!({})))
            .await;
        assert_eq!(flow.status, StatusCode::OK, "{flow:?}");
        let url = flow.body["url"].as_str().unwrap().to_owned();
        self.oidc_return(provider, idp, &flow, &url, who).await
    }
}

const HOME: &str = "http://test/";

#[sqlx::test]
async fn oidc_login_and_linking(pool: PgPool) {
    let w = world(pool).await;
    let idp = fake_idp().await;
    let provider = w.provider(&idp, "Google", None).await;
    let listed = w.get("/api/v1/auth/oidc/providers", "").await;
    assert_eq!(listed.body[0]["name"], "Google");

    // Unknown identity, linking by email off: refused.
    let unknown = w.oidc_login(provider, &idp, ("g-a", "a@x.ch", true)).await;
    assert_eq!(
        unknown.location.as_deref(),
        Some("http://test/login?error=unknown_identity")
    );
    assert!(unknown.cookie("theca_session").is_none());

    // Linked from a's session; then a logs in with it: a full session.
    let a = w.login("a@x.ch", PASSWORD).await.session();
    let linked = w
        .oidc_link(&a, provider, &idp, ("g-a", "whatever", false))
        .await;
    assert_eq!(linked.location.as_deref(), Some(HOME));
    let login = w.oidc_login(provider, &idp, ("g-a", "", false)).await;
    assert_eq!(login.location.as_deref(), Some(HOME));
    let session = format!("theca_session={}", login.cookie("theca_session").unwrap());
    assert_eq!(w.get("/api/v1/me", &session).await.body["email"], "a@x.ch");
    assert_eq!(
        w.get("/api/v1/auth/sessions", &session).await.status,
        StatusCode::OK
    );

    // An identity belongs to one account.
    let b = w.login("b@x.ch", PASSWORD).await.session();
    let taken = w.oidc_link(&b, provider, &idp, ("g-a", "", false)).await;
    assert_eq!(
        taken.location.as_deref(),
        Some("http://test/login?error=identity_taken")
    );

    // A callback that doesn't match the browser's flow (CSRF, replay).
    let start = w
        .get(&format!("/api/v1/auth/oidc/{provider}/start"), "")
        .await;
    let forged = format!("/api/v1/auth/oidc/{provider}/callback?code=c&state=forged");
    let refused = w.get(&forged, &start.session()).await;
    assert_eq!(
        refused.location.as_deref(),
        Some("http://test/login?error=oidc_expired")
    );
    assert_eq!(
        w.get(&forged, "").await.location.as_deref(),
        Some("http://test/login?error=oidc_expired")
    );

    // A token not signed with this client's secret: refused.
    let mut other = fake_idp().await;
    other.secret_file = tempfile::NamedTempFile::new().unwrap();
    std::fs::write(other.secret_file.path(), "another secret").unwrap();
    let forger = w.provider(&other, "Forger", None).await;
    let forged = w.oidc_login(forger, &other, ("g-a", "", false)).await;
    assert_eq!(
        forged.location.as_deref(),
        Some("http://test/login?error=oidc_failed")
    );

    // Identities: never remove the last way to log in.
    let identities: Vec<api::Identity> =
        serde_json::from_value(w.get("/api/v1/auth/identities", &a).await.body).unwrap();
    assert_eq!(identities.len(), 2);
    let remove = |id: Uuid| {
        let (w, a) = (&w, &a);
        async move {
            w.with(
                "DELETE",
                &format!("/api/v1/auth/identities/{id}"),
                ("cookie", a),
                None,
            )
            .await
        }
    };
    let password = identities.iter().find(|i| i.kind == "password").unwrap().id;
    let google = identities.iter().find(|i| i.kind == "oidc").unwrap().id;
    assert_eq!(remove(password).await.status, StatusCode::NO_CONTENT);
    assert_eq!(remove(google).await.code(), "last_identity");
    assert_eq!(
        w.login("a@x.ch", PASSWORD).await.status,
        StatusCode::UNAUTHORIZED
    );
}

#[sqlx::test]
async fn oidc_link_by_email(pool: PgPool) {
    sqlx::query("INSERT INTO setting (key, value) VALUES ('oidc_link_by_email', 'true')")
        .execute(&pool)
        .await
        .unwrap();
    let w = world(pool).await;
    let idp = fake_idp().await;
    let provider = w.provider(&idp, "Google", None).await;

    // Only a verified email links.
    let unverified = w.oidc_login(provider, &idp, ("g-a", "a@x.ch", false)).await;
    assert_eq!(
        unverified.location.as_deref(),
        Some("http://test/login?error=unknown_identity")
    );
    let verified = w.oidc_login(provider, &idp, ("g-a", "A@x.ch", true)).await;
    assert_eq!(verified.location.as_deref(), Some(HOME));
    let session = format!(
        "theca_session={}",
        verified.cookie("theca_session").unwrap()
    );
    assert_eq!(w.get("/api/v1/me", &session).await.body["email"], "a@x.ch");
    let nobody = w
        .oidc_login(provider, &idp, ("g-x", "nobody@x.ch", true))
        .await;
    assert_eq!(
        nobody.location.as_deref(),
        Some("http://test/login?error=unknown_identity")
    );
}

/// An organization's admin runs their own provider and asserts another
/// account's (verified) email: they only get a session restricted to their
/// organization, which reaches neither the account's other organizations nor
/// the account itself.
#[sqlx::test]
async fn oidc_takeover_is_contained(pool: PgPool) {
    sqlx::query("INSERT INTO setting (key, value) VALUES ('oidc_link_by_email', 'true')")
        .execute(&pool)
        .await
        .unwrap();
    let w = world(pool).await;
    let [one, two] = w.organizations;
    let a = w.login("a@x.ch", PASSWORD).await.session();
    w.create_library(&a, one, "A1").await;
    w.create_library(&a, two, "B1").await;
    let idp = fake_idp().await;
    let evil = w.provider(&idp, "Two's SSO", Some(2)).await;
    assert_eq!(
        w.get("/api/v1/auth/oidc/providers", "").await.body,
        json!([]),
        "not listed"
    );

    let login = w.oidc_login(evil, &idp, ("anything", "a@x.ch", true)).await;
    assert_eq!(login.location.as_deref(), Some(HOME));
    let stolen = format!("theca_session={}", login.cookie("theca_session").unwrap());

    // Organization two only.
    let libraries: Vec<api::Library> =
        serde_json::from_value(w.get("/api/v1/libraries", &stolen).await.body).unwrap();
    assert_eq!(libraries.len(), 1);
    assert_eq!(libraries[0].organization, two);
    let into_one = w.create_library(&stolen, one, "Exfiltrate").await;
    assert_eq!(into_one.status, StatusCode::NOT_FOUND);

    // Nothing on the account: sessions, identities, TOTP, linking.
    let s = ("cookie", stolen.as_str());
    for (method, path) in [
        ("GET", "/api/v1/auth/sessions".to_owned()),
        ("GET", "/api/v1/auth/identities".to_owned()),
        ("POST", "/api/v1/auth/totp".to_owned()),
        ("POST", format!("/api/v1/auth/oidc/{evil}/link")),
    ] {
        let refused = w.with(method, &path, s, Some(json!({}))).await;
        assert_eq!(refused.code(), "full_session_required", "{method} {path}");
    }
    // App passwords stay inside organization two.
    let outside = json!({ "name": "x", "organization": one });
    assert_eq!(
        w.app_password(s, outside).await.unwrap_err().status,
        StatusCode::NOT_FOUND
    );
}
