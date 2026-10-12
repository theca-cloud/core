//! Emails and accounts: registration, verification, password reset, members
//! and invitations, the sending job and its retries. Emails really go out over
//! SMTP to Mailpit (`MAILPIT_TEST_HOST`, written by scripts/dev.sh, set by CI),
//! and are read back through its API. Addresses are unique per test: Mailpit
//! is shared by every test and run.

#![allow(dead_code, unused_imports)] // the shared harness has more than these tests use

include!("common/harness.rs");

use theca_core::jobs::Job;

fn mailpit() -> String {
    dotenvy::dotenv().ok();
    std::env::var("MAILPIT_TEST_HOST").expect("MAILPIT_TEST_HOST is required: run scripts/dev.sh")
}

/// A fresh address.
fn address(name: &str) -> String {
    format!("{name}-{}@test.theca", Uuid::now_v7())
}

/// Settings: SMTP to Mailpit, and registration open or not.
async fn configure(pool: &PgPool, registration: &str, smtp_port: u16) {
    sqlx::query(
        "INSERT INTO setting (key, value) VALUES
            ('registration', $1), ('smtp_host', $2), ('smtp_port', $3), ('smtp_tls', 'none'),
            ('smtp_from', 'Theca <theca@test.theca>')",
    )
    .bind(registration)
    .bind(mailpit())
    .bind(smtp_port.to_string())
    .execute(pool)
    .await
    .unwrap();
}

async fn mail_world(pool: PgPool) -> World {
    configure(&pool, "open", 1025).await;
    world(Kind::Filesystem, pool).await
}

/// A received email: `(subject, text)`, oldest first.
async fn inbox(to: &str) -> Vec<(String, String)> {
    let base = format!("http://{}:8025/api/v1", mailpit());
    let get = |url: String| async move {
        let bytes = reqwest::get(url).await.unwrap().bytes().await.unwrap();
        serde_json::from_slice::<Value>(&bytes).unwrap()
    };
    // Mailpit's search syntax: to:"address" (quotes and @ percent-encoded).
    let query = format!("to%3A%22{}%22", to.replace('@', "%40"));
    let mut found: Vec<Value> = Vec::new();
    for _ in 0..50 {
        let search = get(format!("{base}/search?query={query}")).await;
        found = search["messages"].as_array().cloned().unwrap_or_default();
        if !found.is_empty() {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }
    let mut out = Vec::new();
    for message in found.iter().rev() {
        let full = get(format!(
            "{base}/message/{}",
            message["ID"].as_str().unwrap()
        ))
        .await;
        out.push((
            full["Subject"].as_str().unwrap().to_owned(),
            full["Text"].as_str().unwrap().to_owned(),
        ));
    }
    out
}

/// The token in an email's link.
fn token(text: &str) -> String {
    let start = text.find("token=").expect("a link with a token") + "token=".len();
    text[start..].split_whitespace().next().unwrap().to_owned()
}

impl World {
    async fn send_emails(&self) -> i64 {
        Job::EmailSend.run(&state(&self.pool).await).await.unwrap()
    }

    async fn cleanup(&self) {
        Job::Cleanup.run(&state(&self.pool).await).await.unwrap();
    }

    /// A client without a session, on a router with the current settings.
    async fn anonymous(&self) -> Client {
        Client {
            app: app(&self.pool).await,
            account: Uuid::nil(),
            org: Uuid::nil(),
            cookies: String::new(),
            puts: 0,
        }
    }

    /// A client logged in with an email and password.
    async fn logged_in(&self, email: &str, password: &str) -> Client {
        let mut client = self.anonymous().await;
        let body = serde_json::to_vec(&json!({ "email": email, "password": password })).unwrap();
        let request = Request::post("/api/v1/auth/login")
            .header("content-type", "application/json")
            .body(Body::from(body))
            .unwrap();
        let response = client.app.clone().oneshot(request).await.unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        client.cookies = response
            .headers()
            .get_all("set-cookie")
            .iter()
            .map(|v| v.to_str().unwrap().split(';').next().unwrap().to_owned())
            .collect::<Vec<_>>()
            .join("; ");
        client
    }

    /// A verified account with the test password, without organization.
    async fn account(&self, email: &str) {
        let hash = password_hash();
        sqlx::raw_sql(&format!(
            "WITH a AS (INSERT INTO account (email, verified) VALUES ('{email}', true) RETURNING pk)
             INSERT INTO identity (account, kind, details)
             SELECT pk, 'password', jsonb_build_object('hash', '{hash}') FROM a"
        ))
        .execute(&self.pool)
        .await
        .unwrap();
    }
}

async fn login(c: &Client, email: &str, password: &str) -> (StatusCode, Value) {
    let body = json!({ "email": email, "password": password });
    let (status, bytes) = c
        .call(
            "POST",
            "/api/v1/auth/login",
            Some(serde_json::to_vec(&body).unwrap()),
            false,
        )
        .await;
    (
        status,
        serde_json::from_slice(&bytes).unwrap_or(Value::Null),
    )
}

#[sqlx::test]
async fn registration(pool: PgPool) {
    let w = mail_world(pool).await;
    let anon = w.anonymous().await;
    let email = address("new");
    let register = json!({ "email": email, "password": "a long password" });
    anon.json::<Value>("POST", "/api/v1/auth/register", &register)
        .await
        .unwrap();

    // Unverified: no login (after the password check), until the link.
    assert_eq!(
        login(&anon, &email, "wrong password").await.0,
        StatusCode::UNAUTHORIZED
    );
    let (status, refused) = login(&anon, &email, "a long password").await;
    assert_eq!(
        (status, refused["code"].as_str()),
        (StatusCode::FORBIDDEN, Some("email_not_verified"))
    );
    assert_eq!(w.send_emails().await, 1);
    let mail = inbox(&email).await;
    assert!(
        mail[0].0.starts_with("Confirm your email address"),
        "{mail:?}"
    );
    let link = json!({ "token": token(&mail[0].1) });
    let me: api::Me = anon
        .json("POST", "/api/v1/auth/verify", &link)
        .await
        .unwrap();
    assert_eq!(me.email, email);
    assert_eq!(me.organizations.len(), 1);
    assert!(me.organizations[0].personal && me.organizations[0].joined);
    assert_eq!(
        login(&anon, &email, "a long password").await.0,
        StatusCode::OK
    );
    let reused = anon
        .json::<Value>("POST", "/api/v1/auth/verify", &link)
        .await
        .unwrap_err();
    assert_eq!(reused.1["code"], "invalid_token");

    // The same address again: the same answer, a different email.
    anon.json::<Value>("POST", "/api/v1/auth/register", &register)
        .await
        .unwrap();
    w.send_emails().await;
    let mail = inbox(&email).await;
    assert!(
        mail[1].0.starts_with("Someone tried to sign up"),
        "{mail:?}"
    );

    let weak = json!({ "email": address("weak"), "password": "short" });
    let refused = anon
        .json::<Value>("POST", "/api/v1/auth/register", &weak)
        .await
        .unwrap_err();
    assert_eq!(refused.1["code"], "weak_password");
}

#[sqlx::test]
async fn registration_needs_to_be_open(pool: PgPool) {
    let w = world(Kind::Filesystem, pool).await;
    let register = json!({ "email": address("x"), "password": "a long password" });
    let refused = w
        .anonymous()
        .await
        .json::<Value>("POST", "/api/v1/auth/register", &register)
        .await
        .unwrap_err();
    assert_eq!(refused.1["code"], "registration_closed");
    w.sql("INSERT INTO setting (key, value) VALUES ('registration', 'open')")
        .await;
    let refused = w
        .anonymous()
        .await
        .json::<Value>("POST", "/api/v1/auth/register", &register)
        .await
        .unwrap_err();
    assert_eq!(refused.1["code"], "email_unavailable", "no SMTP");
}

#[sqlx::test]
async fn unverified_accounts_expire(pool: PgPool) {
    let w = mail_world(pool).await;
    let anon = w.anonymous().await;
    let email = address("squatter");
    let register = json!({ "email": email, "password": "a long password" });
    anon.json::<Value>("POST", "/api/v1/auth/register", &register)
        .await
        .unwrap();
    w.sql("UPDATE account SET created_at = now() - interval '8 days' WHERE NOT verified")
        .await;
    w.cleanup().await;
    let left: i64 = sqlx::query_scalar("SELECT count(*) FROM account WHERE email = $1")
        .bind(&email)
        .fetch_one(&w.pool)
        .await
        .unwrap();
    assert_eq!(left, 0, "the address is free again");
    anon.json::<Value>("POST", "/api/v1/auth/register", &register)
        .await
        .unwrap();
}

#[sqlx::test]
async fn password_reset(pool: PgPool) {
    let w = mail_world(pool).await;
    let email = address("forgetful");
    w.account(&email).await;
    let anon = w.anonymous().await;
    let (_, before) = login(&anon, &email, PASSWORD).await;
    assert!(before["account"].is_string());
    let session_count = || async {
        sqlx::query_scalar::<_, i64>(
            "SELECT count(*) FROM token WHERE usage = 'session' AND revoked_at IS NULL",
        )
        .fetch_one(&w.pool)
        .await
        .unwrap()
    };
    assert_eq!(session_count().await, 1);

    // Unknown addresses: the same answer, no email.
    let unknown = address("nobody");
    anon.json::<Value>(
        "POST",
        "/api/v1/auth/password/forgot",
        &json!({ "email": unknown }),
    )
    .await
    .unwrap();
    for _ in 0..2 {
        anon.json::<Value>(
            "POST",
            "/api/v1/auth/password/forgot",
            &json!({ "email": email }),
        )
        .await
        .unwrap();
    }
    assert_eq!(w.send_emails().await, 2);
    assert!(inbox(&unknown).await.is_empty());
    let mail = inbox(&email).await;
    let (old, new) = (token(&mail[0].1), token(&mail[1].1));

    // Only the latest link works, once; it logs every session out.
    let reset = |token: &str, password: &str| json!({ "token": token, "password": password });
    let refused = anon
        .json::<Value>(
            "POST",
            "/api/v1/auth/password/reset",
            &reset(&old, "a new password"),
        )
        .await
        .unwrap_err();
    assert_eq!(refused.1["code"], "invalid_token");
    // The page asks which address the link is for, without using it.
    let peek = json!({ "token": new });
    let asked = anon
        .json::<Value>("POST", "/api/v1/auth/token", &peek)
        .await
        .unwrap();
    assert_eq!(asked["email"], email);
    for weak in ["short", email.as_str()] {
        let refused = anon
            .json::<Value>("POST", "/api/v1/auth/password/reset", &reset(&new, weak))
            .await
            .unwrap_err();
        assert_eq!(refused.1["code"], "weak_password", "{weak}");
    }
    anon.json::<Value>(
        "POST",
        "/api/v1/auth/password/reset",
        &reset(&new, "a new password"),
    )
    .await
    .unwrap();
    assert_eq!(session_count().await, 0);
    assert_eq!(
        login(&anon, &email, PASSWORD).await.0,
        StatusCode::UNAUTHORIZED
    );
    assert_eq!(
        login(&anon, &email, "a new password").await.0,
        StatusCode::OK
    );
    let reused = anon
        .json::<Value>(
            "POST",
            "/api/v1/auth/password/reset",
            &reset(&new, "again again"),
        )
        .await
        .unwrap_err();
    assert_eq!(reused.1["code"], "invalid_token");
    let used = anon
        .json::<Value>("POST", "/api/v1/auth/token", &peek)
        .await
        .unwrap_err();
    assert_eq!(used.1["code"], "invalid_token");
}

#[sqlx::test]
async fn failed_sends_are_retried_then_given_up(pool: PgPool) {
    configure(&pool, "open", 1).await; // nothing listens on port 1
    let w = world(Kind::Filesystem, pool).await;
    let email = address("unlucky");
    w.account(&email).await;
    let anon = w.anonymous().await;
    anon.json::<Value>(
        "POST",
        "/api/v1/auth/password/forgot",
        &json!({ "email": email }),
    )
    .await
    .unwrap();
    assert_eq!(w.send_emails().await, 0);
    let (attempts, delay): (i32, f64) = sqlx::query_as(
        "SELECT attempts, extract(epoch FROM next_attempt_at - now())::float8 FROM email",
    )
    .fetch_one(&w.pool)
    .await
    .unwrap();
    assert_eq!(attempts, 1);
    assert!((50.0..=60.0).contains(&delay), "{delay}");
    let tokens: i64 =
        sqlx::query_scalar("SELECT count(*) FROM token WHERE usage = 'password_reset'")
            .fetch_one(&w.pool)
            .await
            .unwrap();
    assert_eq!(tokens, 0, "the token was rolled back with the failed send");

    // After the last retry: given up.
    w.sql("UPDATE email SET attempts = 5, next_attempt_at = now()")
        .await;
    w.send_emails().await;
    let given_up: bool = sqlx::query_scalar("SELECT given_up_at IS NOT NULL FROM email")
        .fetch_one(&w.pool)
        .await
        .unwrap();
    assert!(given_up);
}

#[sqlx::test]
async fn members_and_invitations(pool: PgPool) {
    let w = mail_world(pool).await;
    let alice = w.as_user(1).await;
    let bob = w.as_user(2).await;
    let org = w.organizations[0];
    let members = format!("/api/v1/organizations/{org}/members");
    let add = |email: &str| json!({ "email": email });

    // An existing account joins at once, and is told.
    let existing = address("existing");
    w.account(&existing).await;
    let joined: api::Member = alice.json("POST", &members, &add(&existing)).await.unwrap();
    assert!(joined.joined);
    let again = alice
        .json::<Value>("POST", &members, &add(&existing))
        .await
        .unwrap_err();
    assert_eq!(again.1["code"], "already_member");

    // A new address is invited; accepting sets its password and logs it in.
    let newcomer = address("newcomer");
    let invited: api::Member = alice.json("POST", &members, &add(&newcomer)).await.unwrap();
    assert!(!invited.joined);
    assert_eq!(w.send_emails().await, 2);
    assert!(inbox(&existing).await[0].0.starts_with("You joined one"));
    let mail = inbox(&newcomer).await;
    assert!(mail[0].0.starts_with("Join one"), "{mail:?}");
    let anon = w.anonymous().await;
    assert_eq!(
        login(&anon, &newcomer, "a long password").await.0,
        StatusCode::UNAUTHORIZED
    );
    // The invitation's address, for the page; a password made of it refused.
    let link = json!({ "token": token(&mail[0].1) });
    let asked = anon
        .json::<Value>("POST", "/api/v1/auth/token", &link)
        .await
        .unwrap();
    assert_eq!(asked["email"], newcomer);
    let weak = json!({ "token": token(&mail[0].1), "password": newcomer });
    let refused = anon
        .json::<Value>("POST", "/api/v1/invitations/accept", &weak)
        .await
        .unwrap_err();
    assert_eq!(refused.1["code"], "weak_password");
    let accept = json!({ "token": token(&mail[0].1), "password": "a long password" });
    let me: Option<api::Me> = anon
        .json("POST", "/api/v1/invitations/accept", &accept)
        .await
        .unwrap();
    let me = me.expect("a new account is logged in");
    assert_eq!(me.organizations.len(), 1);
    assert!(me.organizations[0].joined && !me.organizations[0].personal);
    assert_eq!(
        login(&anon, &newcomer, "a long password").await.0,
        StatusCode::OK
    );
    let reused = anon
        .json::<Value>("POST", "/api/v1/invitations/accept", &accept)
        .await
        .unwrap_err();
    assert_eq!(reused.1["code"], "invalid_token");

    // Members see each other; only the owner manages.
    let listed: Vec<api::Member> = bob.get(&members).await.unwrap();
    assert_eq!(listed.len(), 4);
    assert!(listed.iter().all(|m| m.joined));
    assert_eq!(
        bob.json::<Value>("POST", &members, &add(&address("x")))
            .await
            .unwrap_err()
            .0,
        StatusCode::NOT_FOUND
    );
    let owner = listed.iter().find(|m| m.owner).unwrap().id;
    let refused = alice
        .call("DELETE", &format!("{members}/{owner}"), None, true)
        .await
        .0;
    assert_eq!(refused, StatusCode::CONFLICT);

    // A cancelled invitation takes its account with it; an accepted one stays.
    let cancelled = address("cancelled");
    let pending: api::Member = alice
        .json("POST", &members, &add(&cancelled))
        .await
        .unwrap();
    assert_eq!(
        alice
            .call("DELETE", &format!("{members}/{}", pending.id), None, true)
            .await
            .0,
        StatusCode::NO_CONTENT
    );
    let accounts: i64 = sqlx::query_scalar("SELECT count(*) FROM account WHERE email = $1")
        .bind(&cancelled)
        .fetch_one(&w.pool)
        .await
        .unwrap();
    assert_eq!(accounts, 0);

    // Expired invitations too.
    let late = address("late");
    alice
        .json::<api::Member>("POST", &members, &add(&late))
        .await
        .unwrap();
    w.sql("UPDATE \"user\" SET invited_at = now() - interval '15 days' WHERE joined_at IS NULL")
        .await;
    w.cleanup().await;
    let accounts: i64 = sqlx::query_scalar("SELECT count(*) FROM account WHERE email = $1")
        .bind(&late)
        .fetch_one(&w.pool)
        .await
        .unwrap();
    assert_eq!(accounts, 0);
}

#[sqlx::test]
async fn personal_organizations_stay_personal(pool: PgPool) {
    let w = mail_world(pool).await;
    let anon = w.anonymous().await;
    let email = address("solo");
    anon.json::<Value>(
        "POST",
        "/api/v1/auth/register",
        &json!({ "email": email, "password": "a long password" }),
    )
    .await
    .unwrap();
    w.send_emails().await;
    let link = json!({ "token": token(&inbox(&email).await[0].1) });
    let me: api::Me = anon
        .json("POST", "/api/v1/auth/verify", &link)
        .await
        .unwrap();
    let personal = me.organizations[0].organization;

    let solo = w.logged_in(&email, "a long password").await;
    let refused = solo
        .json::<Value>(
            "POST",
            &format!("/api/v1/organizations/{personal}/members"),
            &json!({ "email": "a@x.ch" }),
        )
        .await
        .unwrap_err();
    assert_eq!(refused.1["code"], "personal_organization");
    // Even directly in the database.
    let direct = sqlx::query(
        r#"INSERT INTO "user" (account, organization, joined_at)
                                SELECT 1, pk, now() FROM organization WHERE uuid = $1"#,
    )
    .bind(personal)
    .execute(&w.pool)
    .await
    .unwrap_err();
    assert_eq!(
        direct.as_database_error().unwrap().constraint(),
        Some("personal_organization")
    );
}

#[sqlx::test]
async fn invitations_need_smtp(pool: PgPool) {
    let w = world(Kind::Filesystem, pool).await;
    let alice = w.as_user(1).await;
    let members = format!("/api/v1/organizations/{}/members", w.organizations[0]);
    let refused = alice
        .json::<Value>("POST", &members, &json!({ "email": address("x") }))
        .await
        .unwrap_err();
    assert_eq!(refused.1["code"], "email_unavailable");
    // An existing account still joins (its notice waits for SMTP).
    let carol: api::Member = alice
        .json("POST", &members, &json!({ "email": "c@x.ch" }))
        .await
        .unwrap();
    assert!(carol.joined);
}

/// The admin's SMTP tests, with the dialog's values over the saved ones:
/// connecting, a test email to the admin (Mailpit), errors as answers, an
/// invalid value refused like a save, nothing stored.
#[sqlx::test]
async fn smtp_tests(pool: PgPool) {
    configure(&pool, "closed", 1025).await;
    let w = world(Kind::Filesystem, pool).await;
    // Logged in first: the address then changes to one unique to this test.
    let admin = w.as_user(1).await;
    let me = address("admin");
    sqlx::query("UPDATE account SET instance_admin = true, email = $1 WHERE pk = 1")
        .bind(&me)
        .execute(&w.pool)
        .await
        .unwrap();
    let test = |path: &'static str, body: Value| {
        let admin = &admin;
        async move { admin.json::<api::ConnectionTest>("POST", path, &body).await }
    };

    let ok = test("/api/v1/admin/smtp/test", json!({})).await.unwrap();
    assert!(ok.ok, "{}", ok.message);
    // A wrong port, as typed in the dialog: an answer, at once.
    let started = std::time::Instant::now();
    let wrong = test("/api/v1/admin/smtp/test", json!({ "smtp_port": "1" }))
        .await
        .unwrap();
    assert!(!wrong.ok && !wrong.message.is_empty());
    assert!(started.elapsed() < std::time::Duration::from_secs(12));
    // Refused like a save; only SMTP settings.
    let invalid = test("/api/v1/admin/smtp/test", json!({ "smtp_port": "nope" }))
        .await
        .unwrap_err();
    assert_eq!(invalid.1["code"], "invalid_setting");
    let other = test("/api/v1/admin/smtp/test", json!({ "registration": "open" }))
        .await
        .unwrap_err();
    assert_eq!(other.1["code"], "unknown_setting");
    // Without a server.
    let none = test("/api/v1/admin/smtp/test", json!({ "smtp_host": null }))
        .await
        .unwrap();
    assert!(!none.ok);

    // A test email, to the admin, at once.
    let sent = test("/api/v1/admin/smtp/send-test", json!({}))
        .await
        .unwrap();
    assert!(sent.ok, "{}", sent.message);
    let received = inbox(&me).await;
    assert_eq!(received.len(), 1);
    assert_eq!(received[0].0, "Theca: a test email");

    // Nothing stored, and admins only.
    let port: String = sqlx::query_scalar("SELECT value FROM setting WHERE key = 'smtp_port'")
        .fetch_one(&w.pool)
        .await
        .unwrap();
    assert_eq!(port, "1025");
    let bob = w.as_user(2).await;
    let refused = bob
        .json::<api::ConnectionTest>("POST", "/api/v1/admin/smtp/test", &json!({}))
        .await
        .unwrap_err();
    assert_eq!(refused.1["code"], "admin_required");
}
