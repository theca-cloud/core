//! The upgrade test (decisions.md, Schema and migrations): every release
//! leaves a dump of a server filled by its own code in `tests/upgrade/<version>/`
//! (`dump.sql`, the filesystem storage's `files/`, the keys, `manifest.json`).
//! `every_dump_upgrades` restores each one, starts today's server on it (its
//! migrations apply what's new), and reads back what the manifest lists.
//!
//! `fill` writes a dump: `scripts/upgrade-dump.sh <version>`, from
//! `scripts/release.sh`. A change that stores a new kind of data extends the
//! fill, the manifest (an optional field: older dumps don't have it) and the
//! read-back, in the same change.

include!("common/server.rs");

use std::path::Path;

use serde::{Deserialize, Serialize};
use serde_json::json;
use theca_client::{Cancel, Client, MemorySink, MemorySource, Target, UploadOptions, upload};
use theca_common::api;
use theca_common::name::Name;
use uuid::Uuid;

/// What a dump holds, as its fill created it.
#[derive(Debug, Serialize, Deserialize)]
struct Manifest {
    /// The `core` version that wrote it.
    version: String,
    /// The instance admin, owner of the company organization.
    admin: Account,
    /// A member of the organization, with TOTP.
    member: Account,
    /// The company organization.
    organization: Uuid,
    /// One of the admin's app passwords.
    app_password: String,
    libraries: Vec<Library>,
}

#[derive(Debug, Serialize, Deserialize)]
struct Account {
    email: String,
    password: String,
    /// The TOTP secret (base32), when it's on.
    totp: Option<String>,
}

#[derive(Debug, Serialize, Deserialize)]
struct Library {
    id: Uuid,
    name: String,
    root: Uuid,
    /// The dedup namespace it's in, if any.
    namespace: Option<Uuid>,
    /// The member's role in it, if given one.
    member_role: Option<api::Role>,
    /// The root folder's children's names.
    listing: Vec<String>,
    files: Vec<File>,
    /// Names of the trashed files.
    trash: Vec<String>,
}

#[derive(Debug, Serialize, Deserialize)]
struct File {
    id: Uuid,
    name: String,
    /// Its folder.
    parent: Uuid,
    /// Oldest first; the last one is current.
    versions: Vec<Version>,
}

#[derive(Debug, Serialize, Deserialize)]
struct Version {
    id: Uuid,
    number: String,
    author: Option<String>,
    message: Option<String>,
    content: String,
}

fn dumps() -> std::path::PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/upgrade")
}

/// Copies a directory tree.
fn copy(from: &Path, to: &Path) {
    std::fs::create_dir_all(to).unwrap();
    for entry in std::fs::read_dir(from).unwrap() {
        let entry = entry.unwrap();
        let target = to.join(entry.file_name());
        if entry.file_type().unwrap().is_dir() {
            copy(&entry.path(), &target);
        } else {
            std::fs::copy(entry.path(), target).unwrap();
        }
    }
}

async fn fetch(client: &Client, file: Uuid, version: Option<Uuid>) -> String {
    let out = MemorySink::default();
    theca_client::download(
        client,
        file,
        version,
        &out,
        &Default::default(),
        |_| {},
        &Cancel::default(),
    )
    .await
    .unwrap();
    String::from_utf8(out.0.into_inner().unwrap()).unwrap()
}

async fn put(client: &Client, target: Target, content: &str, message: &str) -> api::Committed {
    let options = UploadOptions {
        message: Some(message.to_owned()),
        ..Default::default()
    };
    let source = MemorySource(content.as_bytes().to_vec());
    upload(
        client,
        &source,
        target,
        &options,
        |_| {},
        &Cancel::default(),
    )
    .await
    .unwrap()
}

fn new_file(library: &api::Library, parent: Uuid, name: &str) -> Target {
    Target::New {
        library: library.id,
        parent,
        name: Name::new(name).unwrap(),
    }
}

/// The token in an email's link, from Mailpit (`MAILPIT_TEST_HOST`).
async fn emailed_token(to: &str) -> String {
    let host = std::env::var("MAILPIT_TEST_HOST").expect("MAILPIT_TEST_HOST: run scripts/dev.sh");
    let base = format!("http://{host}:8025/api/v1");
    let query = format!("to%3A%22{}%22", to.replace('@', "%40"));
    for _ in 0..100 {
        let search = reqwest::get(format!("{base}/search?query={query}"))
            .await
            .unwrap()
            .bytes()
            .await
            .unwrap();
        let search: serde_json::Value = serde_json::from_slice(&search).unwrap();
        if let Some(id) = search["messages"][0]["ID"].as_str() {
            let message = reqwest::get(format!("{base}/message/{id}"))
                .await
                .unwrap()
                .bytes()
                .await
                .unwrap();
            let message: serde_json::Value = serde_json::from_slice(&message).unwrap();
            let text = message["Text"].as_str().unwrap();
            let start = text.find("token=").expect("a link with a token") + "token=".len();
            return text[start..].split_whitespace().next().unwrap().to_owned();
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    panic!("no email to {to}");
}

/// The code an authenticator app shows `offset` seconds from now.
fn totp_code(base32: &str, offset: i64) -> String {
    let secret = totp_rs::Secret::try_from_base32(base32).unwrap();
    let totp = totp_rs::Builder::new().with_secret(secret).build().unwrap();
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs();
    totp.generate(now.saturating_add_signed(offset)).to_string()
}

/// Fills a fresh instance through the API and writes its files, keys and
/// manifest into `UPGRADE_DUMP`; the database (`UPGRADE_DATABASE`, created
/// empty) is dumped by `scripts/upgrade-dump.sh` afterwards.
#[tokio::test]
#[ignore = "writes a release's dump: scripts/upgrade-dump.sh"]
async fn fill() {
    let out = std::env::var("UPGRADE_DUMP").expect("UPGRADE_DUMP: the dump's directory");
    let database = std::env::var("UPGRADE_DATABASE").expect("UPGRADE_DATABASE: an empty database");
    let server = start_server(&database_url(&database), tempfile::tempdir().unwrap(), true).await;

    // Setup: the admin and the company organization, a filesystem storage in
    // the server's directory (relative: the dump moves).
    let mut admin = Client::new(&server.url).unwrap();
    let setup: api::Setup = serde_json::from_value(json!({
        "admin_email": "admin@upgrade.theca",
        "admin_password": "admin's long password",
        "organization": "Acme",
        "storages": [{ "name": "main", "implementation": "filesystem", "path": "files" }],
    }))
    .unwrap();
    let me = admin.setup(&setup, None).await.unwrap();
    let organization = me.organizations[0].organization;
    let host = std::env::var("MAILPIT_TEST_HOST").expect("MAILPIT_TEST_HOST: run scripts/dev.sh");
    // Emails through Mailpit; the trash kept for ten years, so the purge never
    // empties an old dump's.
    let _: serde_json::Value = admin
        .call(
            reqwest::Method::PATCH,
            "/api/v1/admin/settings",
            Some(&json!({
                "smtp_host": host, "smtp_port": "1025", "smtp_tls": "none",
                "smtp_from": "Theca <theca@upgrade.theca>", "trash_retention_secs": "315360000",
            })),
        )
        .await
        .unwrap();

    // A member, invited and joined.
    let email = format!("member-{}@upgrade.theca", Uuid::now_v7());
    let added: api::Member = admin
        .call(
            reqwest::Method::POST,
            &format!("/api/v1/organizations/{organization}/members"),
            Some(&json!({ "email": email })),
        )
        .await
        .unwrap();
    let _: serde_json::Value = admin
        .call(
            reqwest::Method::POST,
            "/api/v1/admin/jobs/email_send/run",
            None::<&()>,
        )
        .await
        .unwrap();
    let password = "member's long password";
    let _: Option<api::Me> = Client::new(&server.url)
        .unwrap()
        .call(
            reqwest::Method::POST,
            "/api/v1/invitations/accept",
            Some(&json!({ "token": emailed_token(&email).await, "password": password })),
        )
        .await
        .unwrap();
    let mut member = Client::new(&server.url).unwrap();
    member.login(&email, password, None).await.unwrap();

    // Two libraries, one in a dedup namespace, the member writing in it.
    let namespace: api::Namespace = admin
        .call(
            reqwest::Method::POST,
            &format!("/api/v1/organizations/{organization}/namespaces"),
            Some(&json!({ "name": "Shared" })),
        )
        .await
        .unwrap();
    let docs: api::Library = admin
        .call(
            reqwest::Method::POST,
            "/api/v1/libraries",
            Some(&json!({ "organization": organization, "name": "Docs", "namespaces": [namespace.id] })),
        )
        .await
        .unwrap();
    let photos = admin.create_library(organization, "Photos").await.unwrap();
    let _: serde_json::Value = admin
        .call(
            reqwest::Method::PUT,
            &format!("/api/v1/libraries/{}/members/{}", docs.id, added.id),
            Some(&json!({ "role": "write" })),
        )
        .await
        .unwrap();

    // Files: two versions by two authors with messages, one in a folder, one
    // trashed.
    let readme = put(
        &admin,
        new_file(&docs, docs.root, "readme.txt"),
        "Hello, version 1\n",
        "first",
    )
    .await;
    let target = Target::Version {
        library: docs.id,
        file: readme.file,
        base: Some(readme.version),
    };
    put(&member, target, "Hello, version 2\n", "second").await;
    let notes = admin
        .create_folders(docs.root, &["notes".to_owned()])
        .await
        .unwrap()[0]
        .id;
    let plan = put(
        &member,
        new_file(&docs, notes, "plan.txt"),
        "The plan.\n",
        "a plan",
    )
    .await;
    let old = put(
        &admin,
        new_file(&docs, docs.root, "old.txt"),
        "Old.\n",
        "soon gone",
    )
    .await;
    admin.trash_nodes(&[old.file], false).await.unwrap()[0]
        .as_ref()
        .unwrap();
    let cat = put(
        &admin,
        new_file(&photos, photos.root, "cat.txt"),
        "A cat.\n",
        "meow",
    )
    .await;

    // TOTP for the member, an app password for the admin.
    let totp: api::TotpSetup = member
        .call(reqwest::Method::POST, "/api/v1/auth/totp", Some(&json!({})))
        .await
        .unwrap();
    let _: api::RecoveryCodes = member
        .call(
            reqwest::Method::POST,
            "/api/v1/auth/totp/confirm",
            // The previous step's code (still accepted): the read-back's code,
            // perhaps within the same 30 s, isn't refused as reused.
            Some(&json!({ "code": totp_code(&totp.secret, -30) })),
        )
        .await
        .unwrap();
    let app_password = admin
        .create_app_password(&serde_json::from_value(json!({ "name": "upgrade test" })).unwrap())
        .await
        .unwrap()
        .password;

    // The manifest, from the server's own answers.
    let mut contents = std::collections::HashMap::new();
    for (file, texts) in [
        (
            readme.file,
            vec!["Hello, version 1\n", "Hello, version 2\n"],
        ),
        (plan.file, vec!["The plan.\n"]),
        (cat.file, vec!["A cat.\n"]),
    ] {
        contents.insert(file, texts);
    }
    let mut libraries = Vec::new();
    for (library, namespace, member_role, files) in [
        (
            &docs,
            Some(namespace.id),
            Some(api::Role::Write),
            vec![
                (readme.file, docs.root, "readme.txt"),
                (plan.file, notes, "plan.txt"),
            ],
        ),
        (
            &photos,
            None,
            None,
            vec![(cat.file, photos.root, "cat.txt")],
        ),
    ] {
        let mut listing: Vec<String> = admin
            .children(library.root)
            .await
            .unwrap()
            .into_iter()
            .map(|n| n.name)
            .collect();
        listing.sort();
        let mut manifest_files = Vec::new();
        for (id, parent, name) in files {
            let mut versions: Vec<api::Version> = admin.versions(id).await.unwrap();
            versions.sort_by_key(|v| v.created_at);
            manifest_files.push(File {
                id,
                name: name.to_owned(),
                parent,
                versions: versions
                    .into_iter()
                    .zip(&contents[&id])
                    .map(|(v, content)| Version {
                        id: v.id,
                        number: v.number,
                        author: v.author,
                        message: v.message,
                        content: (*content).to_owned(),
                    })
                    .collect(),
            });
        }
        let trash = admin
            .library_trash(library.id)
            .await
            .unwrap()
            .into_iter()
            .map(|t| t.name)
            .collect();
        libraries.push(Library {
            id: library.id,
            name: library.name.clone(),
            root: library.root,
            namespace,
            member_role,
            listing,
            files: manifest_files,
            trash,
        });
    }
    let manifest = Manifest {
        version: env!("CARGO_PKG_VERSION").to_owned(),
        admin: Account {
            email: "admin@upgrade.theca".to_owned(),
            password: "admin's long password".to_owned(),
            totp: None,
        },
        member: Account {
            email,
            password: password.to_owned(),
            totp: Some(totp.secret),
        },
        organization,
        app_password,
        libraries,
    };

    let out = Path::new(&out);
    copy(&server.dir.path().join("files"), &out.join("files"));
    for key in ["server_key", "data_key"] {
        std::fs::copy(server.dir.path().join(key), out.join(key)).unwrap();
    }
    std::fs::write(
        out.join("manifest.json"),
        serde_json::to_string_pretty(&manifest).unwrap() + "\n",
    )
    .unwrap();
}

/// Every kept dump, restored and upgraded by today's server, reads back as
/// its manifest says.
#[tokio::test]
async fn every_dump_upgrades() {
    let mut dumps: Vec<_> = std::fs::read_dir(dumps())
        .unwrap()
        .map(|e| e.unwrap().path())
        .collect();
    dumps.sort();
    assert!(!dumps.is_empty(), "no dump in tests/upgrade");
    for dump in dumps {
        read_back(&dump).await;
    }
}

async fn read_back(dump: &Path) {
    let manifest: Manifest =
        serde_json::from_slice(&std::fs::read(dump.join("manifest.json")).unwrap()).unwrap();
    let context = |what: &str| format!("dump {}: {what}", manifest.version);

    // A database of its own, restored from the dump (psql's meta-commands
    // left out: no psql here).
    let database = format!("upgrade_{}", Uuid::now_v7().simple());
    let admin_pool = sqlx::PgPool::connect(&database_url("postgres"))
        .await
        .unwrap();
    sqlx::raw_sql(&format!(r#"CREATE DATABASE "{database}""#))
        .execute(&admin_pool)
        .await
        .unwrap();
    let pool = sqlx::PgPool::connect(&database_url(&database))
        .await
        .unwrap();
    let sql: String = std::fs::read_to_string(dump.join("dump.sql"))
        .unwrap()
        .lines()
        .filter(|line| !line.starts_with('\\'))
        .collect::<Vec<_>>()
        .join("\n");
    sqlx::raw_sql(&sql).execute(&pool).await.unwrap();
    pool.close().await;

    let dir = tempfile::tempdir().unwrap();
    copy(dump, dir.path());
    let server = start_server(&database_url(&database), dir, false).await;
    check(&server, &manifest, &context).await;
    drop(server);

    sqlx::raw_sql(&format!(r#"DROP DATABASE "{database}" WITH (FORCE)"#))
        .execute(&admin_pool)
        .await
        .unwrap();
}

async fn check(server: &Server, manifest: &Manifest, context: &dyn Fn(&str) -> String) {
    let mut admin = Client::new(&server.url).unwrap();
    admin
        .login(&manifest.admin.email, &manifest.admin.password, None)
        .await
        .unwrap_or_else(|e| panic!("{}: {e}", context("the admin's login")));

    // The member: the password alone isn't enough, the TOTP code is.
    let mut member = Client::new(&server.url).unwrap();
    let refused = member
        .login(&manifest.member.email, &manifest.member.password, None)
        .await
        .unwrap_err();
    assert_eq!(
        refused.code(),
        Some("totp_required"),
        "{}",
        context("TOTP on")
    );
    let secret = manifest.member.totp.as_deref().unwrap();
    member
        .login(
            &manifest.member.email,
            &manifest.member.password,
            Some(&totp_code(secret, 0)),
        )
        .await
        .unwrap_or_else(|e| panic!("{}: {e}", context("the member's login")));

    // The app password still opens the admin's account.
    let me = Client::new(&server.url)
        .unwrap()
        .with_token(&manifest.app_password)
        .me()
        .await
        .unwrap_or_else(|e| panic!("{}: {e}", context("the app password")));
    assert_eq!(
        me.email,
        manifest.admin.email,
        "{}",
        context("the app password")
    );

    let namespaces: Vec<api::Namespace> = admin
        .call(
            reqwest::Method::GET,
            &format!("/api/v1/organizations/{}/namespaces", manifest.organization),
            None::<&()>,
        )
        .await
        .unwrap();
    for library in &manifest.libraries {
        let what = |w: &str| context(&format!("library {}: {w}", library.name));
        let mut listing: Vec<String> = admin
            .children(library.root)
            .await
            .unwrap_or_else(|e| panic!("{}: {e}", what("listing")))
            .into_iter()
            .map(|n| n.name)
            .collect();
        listing.sort();
        assert_eq!(listing, library.listing, "{}", what("listing"));
        if let Some(namespace) = library.namespace {
            let found = namespaces.iter().find(|n| n.id == namespace);
            assert!(
                found.is_some_and(|n| n.libraries.contains(&library.id)),
                "{}",
                what("namespace")
            );
        }
        if let Some(role) = library.member_role {
            let members: Vec<api::LibraryMember> = admin
                .call(
                    reqwest::Method::GET,
                    &format!("/api/v1/libraries/{}/members", library.id),
                    None::<&()>,
                )
                .await
                .unwrap();
            assert!(
                members
                    .iter()
                    .any(|m| m.email == manifest.member.email && m.role == role),
                "{}",
                what("member")
            );
        }
        for file in &library.files {
            let what = |w: &str| what(&format!("{}: {w}", file.name));
            let children = admin.children(file.parent).await.unwrap();
            assert!(
                children
                    .iter()
                    .any(|n| n.id == file.id && n.name == file.name),
                "{}",
                what("in its folder")
            );
            let mut versions = admin.versions(file.id).await.unwrap();
            versions.sort_by_key(|v| v.created_at);
            let found: Vec<_> = versions
                .iter()
                .map(|v| {
                    (
                        v.id,
                        v.number.as_str(),
                        v.author.as_deref(),
                        v.message.as_deref(),
                    )
                })
                .collect();
            let expected: Vec<_> = file
                .versions
                .iter()
                .map(|v| {
                    (
                        v.id,
                        v.number.as_str(),
                        v.author.as_deref(),
                        v.message.as_deref(),
                    )
                })
                .collect();
            assert_eq!(found, expected, "{}", what("versions"));
            for version in &file.versions {
                assert_eq!(
                    fetch(&admin, file.id, Some(version.id)).await,
                    version.content,
                    "{}",
                    what(&format!("version {}", version.number))
                );
            }
            assert_eq!(
                fetch(&admin, file.id, None).await,
                file.versions.last().unwrap().content,
                "{}",
                what("current content")
            );
        }
        let trash: Vec<String> = admin
            .library_trash(library.id)
            .await
            .unwrap()
            .into_iter()
            .map(|t| t.name)
            .collect();
        assert_eq!(trash, library.trash, "{}", what("trash"));
    }
}
