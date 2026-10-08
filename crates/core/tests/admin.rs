//! The admin API (13a): live settings, organizations, accounts, storages,
//! identity providers and the SSRF guard, admins only.

#![allow(dead_code, unused_imports)] // the shared harness has more than these tests use

include!("common/harness.rs");

/// Account 1 (a@x.ch, owner of organization one) is the instance admin.
async fn admin_world(pool: PgPool) -> World {
    let w = world(Kind::Filesystem, pool).await;
    w.sql("UPDATE account SET instance_admin = true WHERE pk = 1")
        .await;
    w
}

fn code(failure: Failure) -> String {
    failure.1["code"].as_str().unwrap_or_default().to_owned()
}

#[sqlx::test]
async fn settings_apply_at_once(pool: PgPool) {
    let w = admin_world(pool).await;
    let admin = w.as_user(1).await;
    let bob = w.as_user(2).await;
    assert_eq!(
        code(
            bob.get::<Value>("/api/v1/admin/settings")
                .await
                .unwrap_err()
        ),
        "admin_required"
    );

    let listed: Vec<api::SettingInfo> = admin.get("/api/v1/admin/settings").await.unwrap();
    assert!(
        listed
            .iter()
            .any(|s| s.key == "registration" && s.default == "closed" && s.value.is_none())
    );

    // Registration closed, then open (without SMTP: the next refusal differs).
    let register = json!({ "email": "new@x.ch", "password": "a long password" });
    let refused = admin
        .json::<Value>("POST", "/api/v1/auth/register", &register)
        .await
        .unwrap_err();
    assert_eq!(code(refused), "registration_closed");
    admin
        .json::<Value>(
            "PATCH",
            "/api/v1/admin/settings",
            &json!({ "registration": "open" }),
        )
        .await
        .unwrap();
    let refused = admin
        .json::<Value>("POST", "/api/v1/auth/register", &register)
        .await
        .unwrap_err();
    assert_eq!(
        code(refused),
        "email_unavailable",
        "applied without a restart"
    );

    // All or nothing: one invalid value refuses the whole change.
    let bad = json!({ "instance_name": "Renamed", "session_idle_lifetime_secs": "5" });
    let refused = admin
        .json::<Value>("PATCH", "/api/v1/admin/settings", &bad)
        .await
        .unwrap_err();
    assert_eq!(code(refused), "invalid_setting");
    let unknown = admin
        .json::<Value>("PATCH", "/api/v1/admin/settings", &json!({ "typo": "x" }))
        .await
        .unwrap_err();
    assert_eq!(code(unknown), "unknown_setting");
    let listed: Vec<api::SettingInfo> = admin.get("/api/v1/admin/settings").await.unwrap();
    assert!(
        listed
            .iter()
            .any(|s| s.key == "instance_name" && s.value.is_none()),
        "nothing stored"
    );

    // null: back to the default.
    admin
        .json::<Value>(
            "PATCH",
            "/api/v1/admin/settings",
            &json!({ "registration": null }),
        )
        .await
        .unwrap();
    let refused = admin
        .json::<Value>("POST", "/api/v1/auth/register", &register)
        .await
        .unwrap_err();
    assert_eq!(code(refused), "registration_closed");
}

#[sqlx::test]
async fn organizations_and_suspension(pool: PgPool) {
    let w = admin_world(pool).await;
    let admin = w.as_user(1).await;
    let bob = w.as_user(2).await;
    admin.create_library("Docs").await;
    let one = w.organizations[0];

    let created: api::AdminOrganization = admin
        .json(
            "POST",
            "/api/v1/admin/organizations",
            &json!({ "name": "Carol Inc", "owner": "c@x.ch" }),
        )
        .await
        .unwrap();
    assert_eq!((created.owner.as_str(), created.members), ("c@x.ch", 1));
    let carol = w.as_user(3).await;
    let me: api::Me = carol.get("/api/v1/me").await.unwrap();
    assert!(
        me.organizations
            .iter()
            .any(|o| o.organization == created.id)
    );
    let nobody = admin
        .json::<Value>(
            "POST",
            "/api/v1/admin/organizations",
            &json!({ "name": "X", "owner": "nobody@x.ch" }),
        )
        .await
        .unwrap_err();
    assert_eq!(nobody.0, StatusCode::NOT_FOUND);

    // Quota, then suspension: nothing of it is reachable, members still log in.
    let path = format!("/api/v1/admin/organizations/{one}");
    let updated: api::AdminOrganization = admin
        .json("PATCH", &path, &json!({ "quota": 1000 }))
        .await
        .unwrap();
    assert_eq!(updated.quota, Some(1000));
    admin
        .json::<Value>("PATCH", &path, &json!({ "suspended": true }))
        .await
        .unwrap();
    let libraries: Vec<api::Library> = bob.get("/api/v1/libraries").await.unwrap();
    assert!(libraries.is_empty());
    let me: api::Me = bob.get("/api/v1/me").await.unwrap();
    assert!(
        me.organizations
            .iter()
            .any(|o| o.organization == one && o.suspended)
    );
    let updated: api::AdminOrganization = admin
        .json(
            "PATCH",
            &path,
            &json!({ "suspended": false, "quota": null }),
        )
        .await
        .unwrap();
    assert_eq!((updated.suspended, updated.quota), (false, None));
    let libraries: Vec<api::Library> = admin.get("/api/v1/libraries").await.unwrap();
    assert_eq!(libraries.len(), 1);
}

#[sqlx::test]
async fn accounts_and_admins(pool: PgPool) {
    let w = admin_world(pool).await;
    let admin = w.as_user(1).await;
    let bob = w.as_user(2).await;
    let accounts: Vec<api::AdminAccount> =
        admin.get("/api/v1/admin/accounts?q=b%40x").await.unwrap();
    assert_eq!(accounts.len(), 1);
    let bob_id = accounts[0].id;

    // Suspended: its session stops working, it can't log in.
    let path = format!("/api/v1/admin/accounts/{bob_id}");
    admin
        .json::<Value>("PATCH", &path, &json!({ "suspended": true }))
        .await
        .unwrap();
    assert_eq!(
        bob.get::<Value>("/api/v1/me").await.unwrap_err().0,
        StatusCode::UNAUTHORIZED
    );
    let login = json!({ "email": "b@x.ch", "password": PASSWORD });
    let (status, body) = admin
        .call(
            "POST",
            "/api/v1/auth/login",
            Some(serde_json::to_vec(&login).unwrap()),
            false,
        )
        .await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    assert!(String::from_utf8_lossy(&body).contains("account_suspended"));
    admin
        .json::<Value>("PATCH", &path, &json!({ "suspended": false }))
        .await
        .unwrap();
    assert_eq!(
        admin
            .call(
                "POST",
                "/api/v1/auth/login",
                Some(serde_json::to_vec(&login).unwrap()),
                false
            )
            .await
            .0,
        StatusCode::OK
    );

    // Never without an active admin.
    let me = admin.account;
    let last = admin
        .json::<Value>(
            "PATCH",
            &format!("/api/v1/admin/accounts/{me}"),
            &json!({ "instance_admin": false }),
        )
        .await
        .unwrap_err();
    assert_eq!(code(last), "last_admin");
    let last = admin
        .json::<Value>(
            "PATCH",
            &format!("/api/v1/admin/accounts/{me}"),
            &json!({ "suspended": true }),
        )
        .await
        .unwrap_err();
    assert_eq!(code(last), "last_admin");
    admin
        .json::<Value>("PATCH", &path, &json!({ "instance_admin": true }))
        .await
        .unwrap();
    admin
        .json::<Value>(
            "PATCH",
            &format!("/api/v1/admin/accounts/{me}"),
            &json!({ "instance_admin": false }),
        )
        .await
        .unwrap();
}

/// The SaaS app drives the instance with an admin's account-wide app password.
#[sqlx::test]
async fn app_passwords_for_the_saas(pool: PgPool) {
    let w = admin_world(pool).await;
    let admin = w.as_user(1).await;
    let bearer = |scope: Value| {
        let admin = &admin;
        async move {
            let mut scope = scope;
            scope["name"] = json!("saas");
            let created: api::NewAppPassword = admin
                .json("POST", "/api/v1/auth/app-passwords", &scope)
                .await
                .unwrap();
            created.password
        }
    };
    let call = |token: String, method: &'static str, body: Option<Value>| {
        let app = w.app.clone();
        async move {
            let mut request = Request::builder()
                .method(method)
                .uri("/api/v1/admin/organizations?q=one")
                .header("authorization", format!("Bearer {token}"));
            if body.is_some() {
                request = request.header("content-type", "application/json");
            }
            let body = body.map_or_else(Body::empty, |b| Body::from(b.to_string()));
            app.oneshot(request.body(body).unwrap())
                .await
                .unwrap()
                .status()
        }
    };
    assert_eq!(
        call(bearer(json!({})).await, "GET", None).await,
        StatusCode::OK
    );
    let read_only = bearer(json!({ "read_only": true })).await;
    assert_eq!(call(read_only.clone(), "GET", None).await, StatusCode::OK);
    let create = json!({ "name": "SaaS customer", "owner": "c@x.ch" });
    assert_eq!(
        call(read_only, "POST", Some(create.clone())).await,
        StatusCode::FORBIDDEN
    );
    let scoped = bearer(json!({ "organization": w.organizations[0] })).await;
    assert_eq!(call(scoped, "GET", None).await, StatusCode::FORBIDDEN);
}

#[sqlx::test]
async fn storages(pool: PgPool) {
    let w = admin_world(pool).await;
    let admin = w.as_user(1).await;
    let dir = tempfile::tempdir().unwrap();
    let new = json!({ "name": "Fast", "implementation": "filesystem", "path": dir.path() });
    let added: api::StorageInfo = admin
        .json("POST", "/api/v1/admin/storages", &new)
        .await
        .unwrap();
    assert!(!added.default);
    let file = tempfile::NamedTempFile::new().unwrap();
    let broken =
        json!({ "name": "Broken", "implementation": "filesystem", "path": file.path().join("x") });
    let refused = admin
        .json::<Value>("POST", "/api/v1/admin/storages", &broken)
        .await
        .unwrap_err();
    assert_eq!(code(refused), "storage_unusable");
    let taken = admin
        .json::<Value>("POST", "/api/v1/admin/storages", &new)
        .await
        .unwrap_err();
    assert_eq!(code(taken), "storage_name_taken");

    // The new default gets new libraries.
    let (status, _) = admin
        .call(
            "POST",
            &format!("/api/v1/admin/storages/{}/default", added.id),
            Some(b"{}".to_vec()),
            true,
        )
        .await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    admin.create_library("New").await;
    let storage: String = sqlx::query_scalar(
        "SELECT s.name FROM library l JOIN storage s ON s.pk = l.storage WHERE l.name = 'New'",
    )
    .fetch_one(&w.pool)
    .await
    .unwrap();
    assert_eq!(storage, "Fast");
    let listed: Vec<api::StorageInfo> = admin.get("/api/v1/admin/storages").await.unwrap();
    assert_eq!(listed.iter().filter(|s| s.default).count(), 1);
}

#[sqlx::test]
async fn identity_providers(pool: PgPool) {
    let w = admin_world(pool).await;
    let admin = w.as_user(1).await;
    let secret_file = tempfile::NamedTempFile::new().unwrap();

    // Instance-wide: the admin, a secret file allowed.
    let google = json!({ "name": "Google", "issuer": "https://accounts.google.com", "client_id": "theca",
                         "client_secret_file": secret_file.path() });
    let created: api::ProviderInfo = admin
        .json("POST", "/api/v1/admin/identity-providers", &google)
        .await
        .unwrap();
    assert_eq!(created.organization, None);

    // An organization's: the owner, the secret as a value (stored encrypted,
    // never shown), the SSRF guard on the issuer.
    let org = format!(
        "/api/v1/organizations/{}/identity-providers",
        w.organizations[0]
    );
    let file = json!({ "name": "SSO", "issuer": "https://example.com", "client_id": "c",
                       "client_secret_file": "/etc/passwd" });
    assert_eq!(
        code(admin.json::<Value>("POST", &org, &file).await.unwrap_err()),
        "invalid_provider"
    );
    for issuer in [
        "http://example.com",
        "https://localhost",
        "https://127.0.0.1",
        "https://169.254.169.254",
        "https://10.0.0.1",
    ] {
        let request =
            json!({ "name": "SSO", "issuer": issuer, "client_id": "c", "client_secret": "s" });
        let refused = admin
            .json::<Value>("POST", &org, &request)
            .await
            .unwrap_err();
        assert_eq!(code(refused), "issuer_refused", "{issuer}");
    }
    // Trusted organizations: the instance may allow internal issuers.
    admin
        .json::<Value>(
            "PATCH",
            "/api/v1/admin/settings",
            &json!({ "oidc_allow_internal_issuers": "true" }),
        )
        .await
        .unwrap();
    let internal = json!({ "name": "SSO", "issuer": "http://keycloak.internal:8080", "client_id": "c", "client_secret": "s3cret" });
    let created: api::ProviderInfo = admin.json("POST", &org, &internal).await.unwrap();
    assert!(created.client_secret_file.is_none());
    let stored: String =
        sqlx::query_scalar("SELECT client_secret_encrypted FROM identity_provider WHERE uuid = $1")
            .bind(created.id)
            .fetch_one(&w.pool)
            .await
            .unwrap();
    assert!(!stored.contains("s3cret"));

    // Only the owner.
    let bob = w.as_user(2).await;
    assert_eq!(
        bob.get::<Value>(&org).await.unwrap_err().0,
        StatusCode::NOT_FOUND
    );
    let listed: Vec<api::ProviderInfo> = admin.get(&org).await.unwrap();
    assert_eq!(listed.len(), 1);
    assert_eq!(
        admin
            .call("DELETE", &format!("{org}/{}", created.id), None, true)
            .await
            .0,
        StatusCode::NO_CONTENT
    );
}

#[sqlx::test]
async fn organizations_by_their_users(pool: PgPool) {
    let w = admin_world(pool).await;
    let mut admin = w.as_user(1).await;
    let bob = w.as_user(2).await;
    let carol = w.as_user(3).await;

    // Any account creates one, and owns it.
    let created: api::Organization = bob
        .json(
            "POST",
            "/api/v1/organizations",
            &json!({ "name": " Bob Ltd " }),
        )
        .await
        .unwrap();
    assert_eq!(
        (
            created.name.as_str(),
            created.owner,
            created.personal,
            created.used
        ),
        ("Bob Ltd", true, false, 0)
    );
    let me: api::Me = bob.get("/api/v1/me").await.unwrap();
    assert!(
        me.organizations
            .iter()
            .any(|o| o.organization == created.id && o.owner)
    );
    let empty = bob
        .json::<Value>("POST", "/api/v1/organizations", &json!({ "name": " " }))
        .await
        .unwrap_err();
    assert_eq!(code(empty), "invalid_name");

    // Its owner renames it; others don't see it.
    let path = format!("/api/v1/organizations/{}", created.id);
    let renamed: api::Organization = bob
        .json("PATCH", &path, &json!({ "name": "Bob SA" }))
        .await
        .unwrap();
    assert_eq!(renamed.name, "Bob SA");
    assert_eq!(
        carol.get::<Value>(&path).await.unwrap_err().0,
        StatusCode::NOT_FOUND
    );
    let refused = carol
        .json::<Value>("PATCH", &path, &json!({ "name": "Mine" }))
        .await
        .unwrap_err();
    assert_eq!(refused.0, StatusCode::NOT_FOUND);

    // Every member sees the quota and the usage; only the owner can rename.
    let one = w.organizations[0];
    let docs = admin.create_library("Docs").await;
    admin
        .upload(
            &docs,
            json!({ "parent": docs.root, "name": "a" }),
            &data(5000, 1),
        )
        .await
        .unwrap();
    admin
        .json::<Value>(
            "PATCH",
            &format!("/api/v1/admin/organizations/{one}"),
            &json!({ "quota": 1_000_000 }),
        )
        .await
        .unwrap();
    let seen: api::Organization = bob
        .get(&format!("/api/v1/organizations/{one}"))
        .await
        .unwrap();
    assert_eq!((seen.quota, seen.owner), (Some(1_000_000), false));
    assert!(seen.used > 0);
    let refused = bob
        .json::<Value>(
            "PATCH",
            &format!("/api/v1/organizations/{one}"),
            &json!({ "name": "x" }),
        )
        .await
        .unwrap_err();
    assert_eq!(refused.0, StatusCode::NOT_FOUND);

    // `organization_creation = admins`: instance admins only, and `Me` says so.
    let me: api::Me = bob.get("/api/v1/me").await.unwrap();
    assert!(me.can_create_organizations);
    admin
        .json::<Value>(
            "PATCH",
            "/api/v1/admin/settings",
            &json!({ "organization_creation": "admins" }),
        )
        .await
        .unwrap();
    let closed = bob
        .json::<Value>(
            "POST",
            "/api/v1/organizations",
            &json!({ "name": "Another" }),
        )
        .await
        .unwrap_err();
    assert_eq!(
        (closed.0, code(closed.clone())),
        (StatusCode::FORBIDDEN, "organization_creation_closed".into())
    );
    let me: api::Me = bob.get("/api/v1/me").await.unwrap();
    assert!(!me.can_create_organizations);
    let me: api::Me = admin.get("/api/v1/me").await.unwrap();
    assert!(me.can_create_organizations);
    admin
        .json::<api::Organization>(
            "POST",
            "/api/v1/organizations",
            &json!({ "name": "Admin Co" }),
        )
        .await
        .unwrap();
}

#[sqlx::test]
async fn the_instance_to_anyone(pool: PgPool) {
    let w = admin_world(pool).await;
    let admin = w.as_user(1).await;
    let anyone = Client {
        app: w.app.clone(),
        account: Uuid::nil(),
        org: Uuid::nil(),
        cookies: String::new(),
        puts: 0,
    };
    let instance: api::Instance = anyone.get("/api/v1/instance").await.unwrap();
    assert_eq!(
        (instance.name.as_str(), instance.registration),
        ("Theca", false)
    );

    // Open, but no SMTP: nobody can register yet (the email can't go out).
    admin
        .json::<Value>(
            "PATCH",
            "/api/v1/admin/settings",
            &json!({ "instance_name": "Acme Cloud", "registration": "open" }),
        )
        .await
        .unwrap();
    let instance: api::Instance = anyone.get("/api/v1/instance").await.unwrap();
    assert_eq!(
        (instance.name.as_str(), instance.registration),
        ("Acme Cloud", false)
    );
    admin
        .json::<Value>(
            "PATCH",
            "/api/v1/admin/settings",
            &json!({ "smtp_host": "localhost", "smtp_from": "theca@x.ch" }),
        )
        .await
        .unwrap();
    let instance: api::Instance = anyone.get("/api/v1/instance").await.unwrap();
    assert!(instance.registration);
}
