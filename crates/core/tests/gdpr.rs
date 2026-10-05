//! Deleting organizations and accounts, ownership transfers, and the export
//! (13c).

#![allow(dead_code, unused_imports)] // the shared harness has more than these tests use

include!("common/harness.rs");

use theca_core::jobs::Job;

async fn fs_world(pool: PgPool) -> World {
    world(Kind::Filesystem, pool).await
}

impl World {
    async fn count(&self, sql: &str) -> i64 {
        sqlx::query_scalar(sql).fetch_one(&self.pool).await.unwrap()
    }

    async fn run(&self, job: Job) {
        job.run(&state(&self.pool).await).await.unwrap();
    }
}

#[sqlx::test]
async fn deleting_an_organization(pool: PgPool) {
    let w = fs_world(pool).await;
    let mut alice = w.as_user(1).await;
    let bob = w.as_user(2).await;
    let docs = alice.create_library("Docs").await;
    let file = alice
        .upload(&docs, new_file(&docs, "a.txt"), &data(2000, 1))
        .await
        .unwrap();
    let chunk = alice.file_chunks(file.file).await[0];
    let one = w.organizations[0];

    // Only the owner; then ownership moves to a joined member.
    assert_eq!(
        bob.call(
            "DELETE",
            &format!("/api/v1/organizations/{one}"),
            None,
            true
        )
        .await
        .0,
        StatusCode::NOT_FOUND
    );
    let members: Vec<api::Member> = alice
        .get(&format!("/api/v1/organizations/{one}/members"))
        .await
        .unwrap();
    let bob_member = members.iter().find(|m| m.email == "b@x.ch").unwrap().id;
    let transfer = json!({ "member": bob_member });
    let (status, _) = alice
        .call(
            "POST",
            &format!("/api/v1/organizations/{one}/owner"),
            Some(transfer.to_string().into_bytes()),
            true,
        )
        .await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    assert_eq!(
        alice
            .call(
                "DELETE",
                &format!("/api/v1/organizations/{one}"),
                None,
                true
            )
            .await
            .0,
        StatusCode::NOT_FOUND
    );

    // Deleted: nothing reachable at once, chunks tombstoned.
    assert_eq!(
        bob.call(
            "DELETE",
            &format!("/api/v1/organizations/{one}"),
            None,
            true
        )
        .await
        .0,
        StatusCode::NO_CONTENT
    );
    let me: api::Me = alice.get("/api/v1/me").await.unwrap();
    assert!(me.organizations.iter().all(|o| o.organization != one));
    assert_eq!(
        alice.download(file.file).await.unwrap_err().0,
        StatusCode::NOT_FOUND
    );
    assert_eq!(w.count("SELECT count(*) FROM node").await, 0);
    assert_eq!(
        w.count("SELECT count(*) FROM chunk WHERE deleting_at IS NULL")
            .await,
        0
    );

    // The GC removes the objects (once their upload URLs expired), then the
    // cleanup the organization row.
    w.sql("UPDATE chunk SET url_expires_at = now() - interval '1 minute'")
        .await;
    w.run(Job::ChunkGc).await;
    w.run(Job::Cleanup).await;
    assert_eq!(w.count("SELECT count(*) FROM chunk").await, 0);
    let backend = state(&w.pool).await.backends.get(1).await.unwrap();
    assert!(
        backend.size(chunk).await.unwrap().is_none(),
        "the object is gone"
    );
    assert_eq!(
        w.count(&format!(
            "SELECT count(*) FROM organization WHERE uuid = '{one}'"
        ))
        .await,
        0
    );
}

#[sqlx::test]
async fn deleting_an_account(pool: PgPool) {
    let w = fs_world(pool).await;
    let mut alice = w.as_user(1).await;
    let bob = w.as_user(2).await;
    let docs = alice.create_library("Docs").await;
    alice
        .upload(&docs, new_file(&docs, "a.txt"), &data(100, 1))
        .await
        .unwrap();
    w.add_member(docs.id, 2, "read").await;
    // A personal organization for alice too.
    let mut tx = w.pool.begin().await.unwrap();
    theca_core::db::organization::create(&mut tx, 1, "alice", true)
        .await
        .unwrap();
    tx.commit().await.unwrap();
    let delete = |password: &str| json!({ "password": password }).to_string().into_bytes();

    // Owner of a company organization: transfer it first.
    let (status, body) = alice
        .call("DELETE", "/api/v1/me", Some(delete(PASSWORD)), true)
        .await;
    assert_eq!(status, StatusCode::CONFLICT);
    assert!(String::from_utf8_lossy(&body).contains("owns_organizations"));
    let one = w.organizations[0];
    let members: Vec<api::Member> = alice
        .get(&format!("/api/v1/organizations/{one}/members"))
        .await
        .unwrap();
    let bob_member = members.iter().find(|m| m.email == "b@x.ch").unwrap().id;
    alice
        .call(
            "POST",
            &format!("/api/v1/organizations/{one}/owner"),
            Some(json!({ "member": bob_member }).to_string().into_bytes()),
            true,
        )
        .await;

    // Confirmed with the password; then nothing of the account is left.
    assert_eq!(
        alice
            .call("DELETE", "/api/v1/me", Some(delete("wrong")), true)
            .await
            .0,
        StatusCode::UNAUTHORIZED
    );
    assert_eq!(
        alice
            .call("DELETE", "/api/v1/me", Some(delete(PASSWORD)), true)
            .await
            .0,
        StatusCode::NO_CONTENT
    );
    assert_eq!(
        w.count("SELECT count(*) FROM account WHERE email = 'a@x.ch'")
            .await,
        0
    );
    assert_eq!(
        w.count("SELECT count(*) FROM token WHERE resource NOT IN (SELECT uuid FROM account)")
            .await,
        0
    );
    assert_eq!(
        w.count("SELECT count(*) FROM organization WHERE name = 'alice' AND deleted_at IS NULL")
            .await,
        0
    );
    let login = json!({ "email": "a@x.ch", "password": PASSWORD })
        .to_string()
        .into_bytes();
    assert_eq!(
        bob.call("POST", "/api/v1/auth/login", Some(login), false)
            .await
            .0,
        StatusCode::UNAUTHORIZED
    );

    // Its files in the company organization stay, authorless.
    let nodes: Vec<api::Node> = bob
        .get(&format!("/api/v1/nodes/{}/children", docs.root))
        .await
        .unwrap();
    let versions: Vec<api::Version> = bob
        .get(&format!("/api/v1/files/{}/versions", nodes[0].id))
        .await
        .unwrap();
    assert_eq!(versions[0].author, None);
}

#[sqlx::test]
async fn admin_deletions(pool: PgPool) {
    let w = fs_world(pool).await;
    w.sql("UPDATE account SET instance_admin = true WHERE pk = 1")
        .await;
    let alice = w.as_user(1).await;
    let me = alice.account;
    let (status, body) = alice
        .call(
            "DELETE",
            &format!("/api/v1/admin/accounts/{me}"),
            None,
            true,
        )
        .await;
    assert_eq!(
        status,
        StatusCode::CONFLICT,
        "{}",
        String::from_utf8_lossy(&body)
    );

    // Carol owns organization two: deleted with it first.
    let accounts: Vec<api::AdminAccount> =
        alice.get("/api/v1/admin/accounts?q=c%40x").await.unwrap();
    let carol = accounts[0].id;
    let (status, body) = alice
        .call(
            "DELETE",
            &format!("/api/v1/admin/accounts/{carol}"),
            None,
            true,
        )
        .await;
    assert_eq!(status, StatusCode::CONFLICT);
    assert!(String::from_utf8_lossy(&body).contains("owns_organizations"));
    let two = w.organizations[1];
    assert_eq!(
        alice
            .call(
                "DELETE",
                &format!("/api/v1/admin/organizations/{two}"),
                None,
                true
            )
            .await
            .0,
        StatusCode::NO_CONTENT
    );
    assert_eq!(
        alice
            .call(
                "DELETE",
                &format!("/api/v1/admin/accounts/{carol}"),
                None,
                true
            )
            .await
            .0,
        StatusCode::NO_CONTENT
    );
    let organizations: Vec<api::AdminOrganization> =
        alice.get("/api/v1/admin/organizations").await.unwrap();
    assert!(
        organizations.iter().all(|o| o.id != two),
        "deleted organizations aren't listed"
    );
}

#[sqlx::test]
async fn export(pool: PgPool) {
    let w = fs_world(pool).await;
    let alice = w.as_user(1).await;
    let mut tx = w.pool.begin().await.unwrap();
    let personal = theca_core::db::organization::create(&mut tx, 1, "alice", true)
        .await
        .unwrap();
    tx.commit().await.unwrap();
    // A file in the personal organization.
    let personal: Uuid = sqlx::query_scalar("SELECT uuid FROM organization WHERE pk = $1")
        .bind(personal)
        .fetch_one(&w.pool)
        .await
        .unwrap();
    let mut mine = alice;
    mine.org = personal;
    let lib = mine.create_library("Mine").await;
    mine.upload(&lib, new_file(&lib, "diary.txt"), &data(100, 1))
        .await
        .unwrap();
    let app: api::NewAppPassword = mine
        .json(
            "POST",
            "/api/v1/auth/app-passwords",
            &json!({ "name": "cli" }),
        )
        .await
        .unwrap();

    let (status, body) = mine.call("GET", "/api/v1/me/export", None, true).await;
    assert_eq!(status, StatusCode::OK);
    let text = String::from_utf8(body).unwrap();
    let export: api::Export = serde_json::from_str(&text).unwrap();
    assert_eq!(export.account.email, "a@x.ch");
    assert_eq!(export.files.len(), 1);
    assert_eq!(export.files[0].path, "/diary.txt");
    assert_eq!(export.app_passwords.len(), 1);
    assert!(!text.contains("$argon2"), "no password hash");
    assert!(!text.contains(&app.password), "no app password");
    assert!(
        !text.contains(mine.cookies.trim_start_matches("theca_session=")),
        "no session token"
    );
}
