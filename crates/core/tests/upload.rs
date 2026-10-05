//! The upload protocol end to end, through the router, on a real Postgres:
//! lookup, declaration and presigned upload, announce, commit, download. Plus
//! the dedup scope, authorization, sizes, quota and anomalies.
//!
//! Every scenario runs on both backends: the filesystem (server-signed URLs,
//! through the router) and S3 (Garage, presigned by S3, real HTTP). S3 needs
//! the `S3_TEST_*` variables, written by `scripts/dev.sh` and set by CI.

use std::sync::{Arc, Mutex};

use axum::Router;
use axum::body::{Body, to_bytes};
use axum::http::{Request, StatusCode};
use serde::Serialize;
use serde::de::DeserializeOwned;
use serde_json::{Value, json};
use sqlx::PgPool;
use theca_common::api::{self, ChunkState};
use theca_common::chunking::chunks;
use theca_common::compression::{Compression, CompressionPolicy, compress, decompress};
use theca_common::hash::{ChunkHash, Sha256};
use theca_core::api::{AppState, router};
use theca_core::settings::Settings;
use theca_core::storage::{Backends, UrlSigner};
use tower::ServiceExt;
use uuid::Uuid;

const PUBLIC_URL: &str = "http://test";
const MIB: usize = 1024 * 1024;

#[derive(Clone, Copy, Debug)]
enum Kind {
    Filesystem,
    S3,
}

/// Two organizations, users and storages:
/// - organization 1: users 1 (owner) and 2; organization 2: user 3 (owner)
/// - storage 1 (shared, instance default) and storage 2 (shared), of `kind`
struct World {
    app: Router,
    pool: PgPool,
    users: [Uuid; 3],
    organizations: [Uuid; 2],
    _dirs: Vec<tempfile::TempDir>,
}

fn storage_details(kind: Kind) -> (&'static str, Value, Option<tempfile::TempDir>) {
    match kind {
        Kind::Filesystem => {
            let dir = tempfile::tempdir().unwrap();
            let details = json!({ "path": dir.path() });
            ("filesystem", details, Some(dir))
        }
        Kind::S3 => {
            dotenvy::dotenv().ok();
            let var = |name: &str| {
                std::env::var(name).unwrap_or_else(|_| {
                    panic!("{name} is required for S3 tests: run scripts/dev.sh")
                })
            };
            let details = json!({
                "host": var("S3_TEST_HOST"),
                "region": var("S3_TEST_REGION"),
                "bucket": var("S3_TEST_BUCKET"),
                "access_key_file": var("S3_TEST_ACCESS_KEY_FILE"),
                "secret_key_file": var("S3_TEST_SECRET_KEY_FILE"),
                "path_style": true,
            });
            ("s3", details, None)
        }
    }
}

async fn world(kind: Kind, pool: PgPool) -> World {
    theca_core::install_crypto_provider();
    let hash = password_hash();
    let (implementation, first, dir1) = storage_details(kind);
    let (_, second, dir2) = storage_details(kind);
    sqlx::raw_sql(&format!(
        r#"
        BEGIN;
        INSERT INTO account (pk, email) OVERRIDING SYSTEM VALUE VALUES (1, 'a@x.ch'), (2, 'b@x.ch'), (3, 'c@x.ch');
        INSERT INTO identity (account, kind, details)
            SELECT pk, 'password', jsonb_build_object('hash', '{hash}') FROM account;
        INSERT INTO organization (pk, name, owner) OVERRIDING SYSTEM VALUE VALUES (1, 'one', 1), (2, 'two', 3);
        INSERT INTO "user" (pk, account, organization, joined_at) OVERRIDING SYSTEM VALUE
            VALUES (1, 1, 1, now()), (2, 2, 1, now()), (3, 3, 2, now());
        INSERT INTO storage (pk, name, implementation, details, is_default) OVERRIDING SYSTEM VALUE
            VALUES (1, 'main', '{implementation}', '{first}', true), (2, 'other', '{implementation}', '{second}', false);
        COMMIT;
        "#,
    ))
    .execute(&pool)
    .await
    .unwrap();
    let users: Vec<Uuid> = sqlx::query_scalar(r#"SELECT uuid FROM "user" ORDER BY pk"#)
        .fetch_all(&pool)
        .await
        .unwrap();
    let organizations: Vec<Uuid> = sqlx::query_scalar("SELECT uuid FROM organization ORDER BY pk")
        .fetch_all(&pool)
        .await
        .unwrap();
    World {
        app: app(&pool).await,
        pool,
        users: [users[0], users[1], users[2]],
        organizations: [organizations[0], organizations[1]],
        _dirs: dir1.into_iter().chain(dir2).collect(),
    }
}

async fn app(pool: &PgPool) -> Router {
    let settings = Arc::new(Settings::load(pool).await.unwrap());
    router(AppState {
        pool: pool.clone(),
        settings,
        signer: UrlSigner::new([9; 32], PUBLIC_URL),
        backends: Backends::new(pool.clone()),
        cipher: theca_core::keys::DataCipher::new(&[7; 32]),
        setup: theca_core::api::setup::SetupState::done(),
    })
}

/// Every test account's password, hashed once (Argon2 is deliberately slow).
const PASSWORD: &str = "correct horse battery staple";

fn password_hash() -> &'static str {
    static HASH: std::sync::OnceLock<String> = std::sync::OnceLock::new();
    HASH.get_or_init(|| theca_core::auth::hash_password(PASSWORD))
}

impl World {
    /// Logs in as account `n` (1: a@x.ch in organization one, 2: b@x.ch in
    /// organization one, 3: c@x.ch in organization two).
    async fn as_user(&self, n: usize) -> Client {
        self.as_user_on(self.app.clone(), n).await
    }

    async fn as_user_on(&self, app: Router, n: usize) -> Client {
        let email = ["a@x.ch", "b@x.ch", "c@x.ch"][n - 1];
        let request = Request::post("/api/v1/auth/login")
            .header("content-type", "application/json")
            .body(Body::from(
                json!({ "email": email, "password": PASSWORD }).to_string(),
            ))
            .unwrap();
        let response = app.clone().oneshot(request).await.unwrap();
        assert_eq!(response.status(), StatusCode::OK, "login");
        let cookies: Vec<String> = response
            .headers()
            .get_all("set-cookie")
            .iter()
            .map(|v| v.to_str().unwrap().split(';').next().unwrap().to_owned())
            .collect();
        let me: api::Me =
            serde_json::from_slice(&to_bytes(response.into_body(), usize::MAX).await.unwrap())
                .unwrap();
        Client {
            app,
            account: me.account,
            org: self.organizations[if n == 3 { 1 } else { 0 }],
            cookies: cookies.join("; "),
            puts: 0,
        }
    }

    /// A namespace in organization 1 containing the given libraries.
    async fn namespace(&self, libraries: &[Uuid]) -> Uuid {
        let ns: Uuid = sqlx::query_scalar(
            "INSERT INTO namespace (organization, name) VALUES (1, gen_random_uuid()::text) RETURNING uuid",
        )
        .fetch_one(&self.pool)
        .await
        .unwrap();
        for library in libraries {
            sqlx::query(
                "INSERT INTO library_namespace (organization, library, namespace)
                 SELECT 1, l.pk, n.pk FROM library l, namespace n WHERE l.uuid = $1 AND n.uuid = $2",
            )
            .bind(library)
            .bind(ns)
            .execute(&self.pool)
            .await
            .unwrap();
        }
        ns
    }

    async fn add_member(&self, library: Uuid, user: usize, role: &str) {
        sqlx::query(
            r#"INSERT INTO library_member (organization, library, "user", role)
               SELECT l.owner, l.pk, u.pk, $3 FROM library l, "user" u WHERE l.uuid = $1 AND u.uuid = $2"#,
        )
        .bind(library)
        .bind(self.users[user - 1])
        .bind(role)
        .execute(&self.pool)
        .await
        .unwrap();
    }

    async fn sql(&self, sql: &str) {
        sqlx::raw_sql(sql).execute(&self.pool).await.unwrap();
    }

    async fn chunk_column<T>(&self, chunk: Uuid, column: &str) -> T
    where
        T: for<'r> sqlx::Decode<'r, sqlx::Postgres> + sqlx::Type<sqlx::Postgres> + Send + Unpin,
    {
        sqlx::query_scalar(&format!("SELECT {column} FROM chunk WHERE uuid = $1"))
            .bind(chunk)
            .fetch_one(&self.pool)
            .await
            .unwrap()
    }
}

/// A test client logged in as one account (session cookies). Counts uploads (`puts`).
struct Client {
    app: Router,
    account: Uuid,
    /// The organization its libraries are created in.
    org: Uuid,
    cookies: String,
    puts: usize,
}

type Failure = (StatusCode, Value);

/// A chunk ready to upload: its declaration and stored bytes.
struct Prepared {
    declare: api::Declare,
    stored: Vec<u8>,
}

impl Client {
    async fn call(
        &self,
        method: &str,
        path: &str,
        body: Option<Vec<u8>>,
        auth: bool,
    ) -> (StatusCode, Vec<u8>) {
        let mut request = Request::builder()
            .method(method)
            .uri(path)
            .header("content-type", "application/json");
        if auth {
            request = request.header("cookie", &self.cookies);
        }
        let response = self
            .app
            .clone()
            .oneshot(
                request
                    .body(body.map_or_else(Body::empty, Body::from))
                    .unwrap(),
            )
            .await
            .unwrap();
        let status = response.status();
        (
            status,
            to_bytes(response.into_body(), usize::MAX)
                .await
                .unwrap()
                .to_vec(),
        )
    }

    async fn json<T: DeserializeOwned>(
        &self,
        method: &str,
        path: &str,
        body: &impl Serialize,
    ) -> Result<T, Failure> {
        let (status, bytes) = self
            .call(method, path, Some(serde_json::to_vec(body).unwrap()), true)
            .await;
        if status.is_success() {
            Ok(serde_json::from_slice(if bytes.is_empty() { b"null" } else { &bytes }).unwrap())
        } else {
            Err((
                status,
                serde_json::from_slice(&bytes).unwrap_or(Value::Null),
            ))
        }
    }

    async fn get<T: DeserializeOwned>(&self, path: &str) -> Result<T, Failure> {
        let (status, bytes) = self.call("GET", path, None, true).await;
        if status.is_success() {
            Ok(serde_json::from_slice(&bytes).unwrap())
        } else {
            Err((
                status,
                serde_json::from_slice(&bytes).unwrap_or(Value::Null),
            ))
        }
    }

    /// A presigned URL: through the router (filesystem) or real HTTP (S3).
    /// No authentication header, only the signature and the signed headers.
    async fn blob(
        &self,
        method: &str,
        url: &str,
        headers: &[(&str, &str)],
        body: Option<Vec<u8>>,
    ) -> StatusCode {
        if let Some(path) = url.strip_prefix(PUBLIC_URL) {
            let mut request = Request::builder().method(method).uri(path);
            for (k, v) in headers {
                request = request.header(*k, *v);
            }
            let response = self
                .app
                .clone()
                .oneshot(
                    request
                        .body(body.map_or_else(Body::empty, Body::from))
                        .unwrap(),
                )
                .await
                .unwrap();
            return response.status();
        }
        let client = reqwest::Client::new();
        let mut request = client.request(method.parse().unwrap(), url);
        for (k, v) in headers {
            request = request.header(*k, *v);
        }
        if let Some(body) = body {
            request = request.body(body);
        }
        request.send().await.unwrap().status()
    }

    async fn fetch(&self, url: &str) -> Vec<u8> {
        if let Some(path) = url.strip_prefix(PUBLIC_URL) {
            let (status, bytes) = self.call("GET", path, None, false).await;
            assert_eq!(status, StatusCode::OK);
            return bytes;
        }
        let response = reqwest::get(url).await.unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        response.bytes().await.unwrap().to_vec()
    }

    async fn put_url(&self, url: &api::ChunkUrl, body: Vec<u8>) -> StatusCode {
        let headers: Vec<(&str, &str)> = url
            .headers
            .iter()
            .map(|(k, v)| (k.as_str(), v.as_str()))
            .collect();
        self.blob("PUT", &url.url, &headers, Some(body)).await
    }

    async fn create_library(&self, name: &str) -> api::Library {
        self.json(
            "POST",
            "/api/v1/libraries",
            &json!({ "name": name, "organization": self.org }),
        )
        .await
        .unwrap()
    }

    async fn lookup(
        &self,
        library: Uuid,
        chunks: &[(ChunkHash, usize)],
    ) -> Result<Vec<api::LookupEntry>, Failure> {
        let chunks: Vec<_> = chunks
            .iter()
            .map(|(hash, size)| json!({ "hash": hash, "plain_size": size }))
            .collect();
        self.json(
            "POST",
            "/api/v1/chunks/lookup",
            &json!({ "library": library, "chunks": chunks }),
        )
        .await
    }

    async fn upload_urls(
        &self,
        declarations: &[api::Declare],
    ) -> Result<Vec<api::ChunkUrl>, Failure> {
        self.json("POST", "/api/v1/chunks/upload-urls", &declarations)
            .await
    }

    async fn announce(&self, chunks: &[Uuid]) -> Result<Vec<Uuid>, Failure> {
        let missing: api::Chunks = self
            .json(
                "POST",
                "/api/v1/chunks/announce",
                &json!({ "chunks": chunks }),
            )
            .await?;
        Ok(missing.chunks)
    }

    async fn cancel(&self, chunks: &[Uuid]) {
        let _: () = self
            .json(
                "POST",
                "/api/v1/chunks/cancel",
                &json!({ "chunks": chunks }),
            )
            .await
            .unwrap();
    }

    /// The whole protocol: chunk, hash, lookup, compress, declare, upload, announce, commit.
    async fn upload(
        &mut self,
        library: &api::Library,
        target: Value,
        data: &[u8],
    ) -> Result<api::Committed, Failure> {
        let parts: Vec<(ChunkHash, Vec<u8>)> = chunks(data)
            .map(|c| {
                let c = c.unwrap();
                (ChunkHash::of(&c.data), c.data)
            })
            .collect();
        let sizes: Vec<(ChunkHash, usize)> = parts.iter().map(|(h, d)| (*h, d.len())).collect();
        let entries = self.lookup(library.id, &sizes).await?;
        let entry = |h: &ChunkHash| entries.iter().find(|e| e.hash == *h).unwrap();

        let mut prepared: Vec<Prepared> = Vec::new();
        for (hash, plain) in &parts {
            let e = entry(hash);
            if e.state == ChunkState::Upload && !prepared.iter().any(|p| p.declare.chunk == e.chunk)
            {
                prepared.push(prepare(e.chunk, plain));
            }
        }
        if !prepared.is_empty() {
            let declarations: Vec<api::Declare> = prepared.iter().map(|p| p.declare).collect();
            let urls = self.upload_urls(&declarations).await?;
            for url in &urls {
                let p = prepared
                    .iter()
                    .find(|p| p.declare.chunk == url.chunk)
                    .unwrap();
                assert!(
                    self.put_url(url, p.stored.clone()).await.is_success(),
                    "upload of {}",
                    url.chunk
                );
                self.puts += 1;
            }
            let ids: Vec<Uuid> = prepared.iter().map(|p| p.declare.chunk).collect();
            assert!(
                self.announce(&ids).await?.is_empty(),
                "every uploaded chunk is announced"
            );
        }

        let mut commit = target;
        commit["library"] = json!(library.id);
        commit["chunks"] = json!(
            parts
                .iter()
                .map(|(h, _)| entry(h).chunk)
                .collect::<Vec<_>>()
        );
        self.json("POST", "/api/v1/files/commit", &commit).await
    }

    /// Downloads a file's current version, checking every chunk's hash.
    async fn download(&self, file: Uuid) -> Result<Vec<u8>, Failure> {
        let download: api::Download = self.get(&format!("/api/v1/files/{file}/download")).await?;
        let mut data = Vec::new();
        for chunk in download.chunks {
            let plain = decompress(chunk.compression, &self.fetch(&chunk.url).await).unwrap();
            assert_eq!(ChunkHash::of(&plain), chunk.hash);
            data.extend(plain);
        }
        Ok(data)
    }

    /// The chunks of a file's current version.
    async fn file_chunks(&self, file: Uuid) -> Vec<Uuid> {
        let download: api::Download = self
            .get(&format!("/api/v1/files/{file}/download"))
            .await
            .unwrap();
        download.chunks.iter().map(|c| c.chunk).collect()
    }

    /// A single pending chunk of `data`, declared, with its URL.
    async fn pending(&self, library: Uuid, data: &[u8]) -> (Prepared, api::ChunkUrl) {
        let prepared = self.looked_up(library, data).await;
        let urls = self.upload_urls(&[prepared.declare]).await.unwrap();
        (prepared, urls.into_iter().next().unwrap())
    }

    /// A single chunk of `data`, looked up (pending, not declared yet).
    async fn looked_up(&self, library: Uuid, data: &[u8]) -> Prepared {
        let entries = self
            .lookup(library, &[(ChunkHash::of(data), data.len())])
            .await
            .unwrap();
        prepare(entries[0].chunk, data)
    }
}

fn prepare(chunk: Uuid, plain: &[u8]) -> Prepared {
    let (compression, stored) = compress(&CompressionPolicy::default(), plain).unwrap();
    Prepared {
        declare: api::Declare {
            chunk,
            stored_size: stored.len() as u64,
            compression,
            stored_sha256: Sha256::of(&stored),
        },
        stored,
    }
}

fn new_file(library: &api::Library, name: &str) -> Value {
    json!({ "parent": library.root, "name": name })
}

/// Deterministic pseudo-random bytes (incompressible, so chunks are distinct).
fn data(len: usize, seed: u64) -> Vec<u8> {
    let mut s = seed * 2 + 1; // distinct and odd (xorshift needs non-zero)
    (0..len)
        .map(|_| {
            s ^= s << 13;
            s ^= s >> 7;
            s ^= s << 17;
            s as u8
        })
        .collect()
}

/// Anomaly events, recorded by one global subscriber (tracing caches callsite
/// interest globally, so per-test subscribers can miss events). Each test looks
/// for its own events through a unique ID (user or chunk uuid).
struct Anomalies;

static RECORDED: Mutex<Vec<(String, String)>> = Mutex::new(Vec::new());

impl Anomalies {
    fn capture() -> Self {
        use tracing_subscriber::layer::SubscriberExt;
        static INSTALL: std::sync::Once = std::sync::Once::new();
        INSTALL.call_once(|| {
            let _ = tracing::subscriber::set_global_default(
                tracing_subscriber::registry().with(Recorder),
            );
        });
        Self
    }

    /// Whether `anomaly` was logged with `id` (a uuid) among its fields.
    fn logged(&self, anomaly: &str, id: Uuid) -> bool {
        let id = id.to_string();
        RECORDED
            .lock()
            .unwrap()
            .iter()
            .any(|(a, fields)| a == anomaly && fields.contains(&id))
    }
}

struct Recorder;

impl<S: tracing::Subscriber> tracing_subscriber::Layer<S> for Recorder {
    fn on_event(&self, event: &tracing::Event<'_>, _: tracing_subscriber::layer::Context<'_, S>) {
        #[derive(Default)]
        struct Visitor(Option<String>, String);
        impl tracing::field::Visit for Visitor {
            fn record_str(&mut self, field: &tracing::field::Field, value: &str) {
                if field.name() == "anomaly" {
                    self.0 = Some(value.to_owned());
                } else {
                    self.1.push_str(value);
                }
            }
            fn record_debug(&mut self, _: &tracing::field::Field, value: &dyn std::fmt::Debug) {
                self.1.push_str(&format!("{value:?} "));
            }
        }
        let mut visitor = Visitor::default();
        event.record(&mut visitor);
        if let Some(anomaly) = visitor.0 {
            RECORDED.lock().unwrap().push((anomaly, visitor.1));
        }
    }
}

/// Runs each scenario on both backends: `filesystem::name` and `s3::name`.
macro_rules! on_both_backends {
    ($($name:ident),* $(,)?) => {
        mod filesystem {
            $( #[sqlx::test(migrations = "./migrations")]
               async fn $name(pool: sqlx::PgPool) { super::$name(super::Kind::Filesystem, pool).await } )*
        }
        mod s3 {
            $( #[sqlx::test(migrations = "./migrations")]
               async fn $name(pool: sqlx::PgPool) { super::$name(super::Kind::S3, pool).await } )*
        }
    };
}

on_both_backends!(
    upload_then_download_identical,
    reupload_uploads_nothing,
    edit_uploads_only_new_chunks,
    concurrent_edits_conflict,
    empty_file,
    other_bytes_are_refused,
    declarations_are_fixed,
    announce_checks_the_object,
    sizes_are_bounded,
    cancel_tombstones,
    quota,
    url_lifetime_setting,
    presigned_urls_cannot_be_tampered_with,
    namespaces_decide_who_dedups_with_whom,
    pending_chunks_only_match_within_their_library,
    dedup_never_crosses_storages_or_organizations,
    members_only,
    commit_refuses_chunks_outside_the_dedup_scope,
    commit_errors,
    attack_url_for_another_chunk,
    attack_other_methods,
    attack_upload_after_cancel_is_billed,
    attack_concurrent_declarations,
    attack_unsigned_headers,
    attack_pending_upload_cap,
    every_stored_byte_is_billed,
);

// Protocol.

async fn upload_then_download_identical(kind: Kind, pool: PgPool) {
    let w = world(kind, pool).await;
    let mut alice = w.as_user(1).await;
    let lib = alice.create_library("photos").await;
    let content = data(6 * MIB, 1);
    let committed = alice
        .upload(&lib, new_file(&lib, "big.bin"), &content)
        .await
        .unwrap();
    assert!(alice.puts > 1, "multi-chunk file");
    assert!(!committed.conflict);
    assert_eq!(alice.download(committed.file).await.unwrap(), content);
    let listed: Vec<api::Node> = alice
        .get(&format!("/api/v1/nodes/{}/children", lib.root))
        .await
        .unwrap();
    assert_eq!(listed.len(), 1);
    assert_eq!(listed[0].name, "big.bin");
}

async fn reupload_uploads_nothing(kind: Kind, pool: PgPool) {
    let w = world(kind, pool).await;
    let mut alice = w.as_user(1).await;
    let lib = alice.create_library("photos").await;
    let content = data(4 * MIB, 2);
    alice
        .upload(&lib, new_file(&lib, "a.bin"), &content)
        .await
        .unwrap();
    let before = alice.puts;
    alice
        .upload(&lib, new_file(&lib, "copy.bin"), &content)
        .await
        .unwrap();
    assert_eq!(alice.puts, before, "every chunk was ready");
}

async fn edit_uploads_only_new_chunks(kind: Kind, pool: PgPool) {
    let w = world(kind, pool).await;
    let mut alice = w.as_user(1).await;
    let lib = alice.create_library("docs").await;
    let mut content = data(8 * MIB, 3);
    let v1 = alice
        .upload(&lib, new_file(&lib, "doc.bin"), &content)
        .await
        .unwrap();
    let total = alice.puts;
    content[4 * MIB..4 * MIB + 100].fill(0xAB);
    let before = alice.puts;
    let v2 = alice
        .upload(
            &lib,
            json!({ "file": v1.file, "base_version": v1.version }),
            &content,
        )
        .await
        .unwrap();
    assert!(alice.puts - before < total, "unchanged chunks were reused");
    assert!(!v2.conflict);
    assert_eq!(alice.download(v1.file).await.unwrap(), content);
    let parent: Option<Uuid> = sqlx::query_scalar(
        "SELECT p.uuid FROM version v LEFT JOIN version p ON p.pk = v.parent_version WHERE v.uuid = $1",
    )
    .bind(v2.version)
    .fetch_one(&w.pool)
    .await
    .unwrap();
    assert_eq!(
        parent,
        Some(v1.version),
        "the new version's parent is the base"
    );
}

async fn concurrent_edits_conflict(kind: Kind, pool: PgPool) {
    let w = world(kind, pool).await;
    let mut alice = w.as_user(1).await;
    let lib = alice.create_library("docs").await;
    let v1 = alice
        .upload(&lib, new_file(&lib, "doc.bin"), &data(MIB, 4))
        .await
        .unwrap();
    let edit = || json!({ "file": v1.file, "base_version": v1.version });
    let a = alice.upload(&lib, edit(), &data(MIB, 5)).await.unwrap();
    let b = alice.upload(&lib, edit(), &data(MIB, 6)).await.unwrap();
    assert!(!a.conflict);
    assert!(
        b.conflict,
        "the second edit started from a version that was no longer current"
    );
    let versions: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM version v JOIN node n ON n.pk = v.file WHERE n.uuid = $1",
    )
    .bind(v1.file)
    .fetch_one(&w.pool)
    .await
    .unwrap();
    assert_eq!(versions, 3, "both branches are kept");
}

async fn empty_file(kind: Kind, pool: PgPool) {
    let w = world(kind, pool).await;
    let mut alice = w.as_user(1).await;
    let lib = alice.create_library("misc").await;
    let committed = alice
        .upload(&lib, new_file(&lib, "empty.txt"), &[])
        .await
        .unwrap();
    assert_eq!(
        alice.download(committed.file).await.unwrap(),
        Vec::<u8>::new()
    );
}

// Sizes, checksum, immutability.

/// The upload URL only accepts exactly the declared bytes.
async fn other_bytes_are_refused(kind: Kind, pool: PgPool) {
    let w = world(kind, pool).await;
    let alice = w.as_user(1).await;
    let lib = alice.create_library("misc").await;
    let (prepared, url) = alice.pending(lib.id, &data(1000, 7)).await;

    let mut same_size = prepared.stored.clone();
    same_size[0] ^= 1;
    assert!(
        !alice.put_url(&url, same_size).await.is_success(),
        "same size, other content"
    );
    assert!(
        !alice.put_url(&url, data(2000, 8)).await.is_success(),
        "other size"
    );
    let without_checksum: Vec<(&str, &str)> = url
        .headers
        .iter()
        .filter(|(k, _)| k.as_str() != "x-amz-checksum-sha256")
        .map(|(k, v)| (k.as_str(), v.as_str()))
        .collect();
    let omitted = alice
        .blob(
            "PUT",
            &url.url,
            &without_checksum,
            Some(prepared.stored.clone()),
        )
        .await;
    // The filesystem endpoint checks the body itself (the header is informative there).
    if let Kind::S3 = kind {
        assert!(
            !omitted.is_success(),
            "the signed checksum header can't be omitted"
        );
    }

    assert!(
        alice
            .put_url(&url, prepared.stored.clone())
            .await
            .is_success(),
        "the declared bytes"
    );
    assert!(
        alice
            .announce(&[prepared.declare.chunk])
            .await
            .unwrap()
            .is_empty()
    );
}

/// The declaration is fixed: no URL can ever carry other bytes for a chunk.
async fn declarations_are_fixed(kind: Kind, pool: PgPool) {
    let w = world(kind, pool).await;
    let alice = w.as_user(1).await;
    let lib = alice.create_library("misc").await;
    let anomalies = Anomalies::capture();
    let (prepared, url) = alice.pending(lib.id, &data(1000, 9)).await;

    // Same declaration again (e.g. the URL expired): a new URL, fine.
    assert!(alice.upload_urls(&[prepared.declare]).await.is_ok());
    // Another declaration: refused.
    let other = data(1000, 10);
    let mut changed = prepared.declare;
    changed.stored_sha256 = Sha256::of(&other);
    let (status, error) = alice.upload_urls(&[changed]).await.unwrap_err();
    assert_eq!(
        (status, error["code"].as_str()),
        (StatusCode::CONFLICT, Some("declaration_mismatch"))
    );
    assert!(anomalies.logged("declaration_mismatch", alice.account));

    // Upload, announce, then try to overwrite with other bytes through a still valid URL.
    assert!(
        alice
            .put_url(&url, prepared.stored.clone())
            .await
            .is_success()
    );
    assert!(
        alice
            .announce(&[prepared.declare.chunk])
            .await
            .unwrap()
            .is_empty()
    );
    assert!(
        !alice.put_url(&url, other).await.is_success(),
        "no overwrite with other bytes"
    );
    // Rewriting the identical bytes changes nothing (S3: accepted or refused, same content either way).
    let _ = alice.put_url(&url, prepared.stored.clone()).await;
    // A new declaration after the upload is refused too (not pending anymore).
    assert_eq!(
        alice.upload_urls(&[prepared.declare]).await.unwrap_err().0,
        StatusCode::FORBIDDEN
    );
    assert!(anomalies.logged("upload_url_not_allowed", alice.account));
}

async fn announce_checks_the_object(kind: Kind, pool: PgPool) {
    let w = world(kind, pool).await;
    let alice = w.as_user(1).await;
    let lib = alice.create_library("misc").await;
    let anomalies = Anomalies::capture();
    let (prepared, url) = alice.pending(lib.id, &data(500, 11)).await;
    let chunk = prepared.declare.chunk;
    assert_eq!(
        alice.announce(&[chunk]).await.unwrap(),
        vec![chunk],
        "not uploaded yet"
    );
    assert!(anomalies.logged("announce_missing_object", chunk));
    let uploaded: Option<String> = w.chunk_column(chunk, "uploaded_at::text").await;
    assert_eq!(uploaded, None, "still pending");
    assert!(alice.put_url(&url, prepared.stored).await.is_success());
    assert!(alice.announce(&[chunk]).await.unwrap().is_empty());
    // A chunk that was never declared can't be announced.
    let undeclared = alice.looked_up(lib.id, b"undeclared").await;
    assert_eq!(
        alice
            .announce(&[undeclared.declare.chunk])
            .await
            .unwrap_err()
            .0,
        StatusCode::FORBIDDEN
    );
}

async fn sizes_are_bounded(kind: Kind, pool: PgPool) {
    let w = world(kind, pool).await;
    let alice = w.as_user(1).await;
    let lib = alice.create_library("misc").await;
    let (status, error) = alice
        .lookup(lib.id, &[(ChunkHash::of(b"huge"), 8 * MIB + 1)])
        .await
        .unwrap_err();
    assert_eq!(
        (status, error["code"].as_str()),
        (StatusCode::BAD_REQUEST, Some("invalid_plain_size"))
    );

    let mut declare = alice.looked_up(lib.id, b"small").await.declare; // plain size 5
    declare.stored_size = 6;
    declare.compression = Compression::None;
    let (status, error) = alice.upload_urls(&[declare]).await.unwrap_err();
    assert_eq!(
        (status, error["code"].as_str()),
        (StatusCode::BAD_REQUEST, Some("invalid_declaration"))
    );
}

async fn cancel_tombstones(kind: Kind, pool: PgPool) {
    let w = world(kind, pool).await;
    let mut alice = w.as_user(1).await;
    let lib = alice.create_library("misc").await;
    let committed = alice
        .upload(&lib, new_file(&lib, "kept.bin"), &data(MIB, 12))
        .await
        .unwrap();
    let kept = alice.file_chunks(committed.file).await;
    let (prepared, url) = alice.pending(lib.id, b"cancelled").await;
    let pending = prepared.declare.chunk;

    alice
        .cancel(&kept.iter().copied().chain([pending]).collect::<Vec<_>>())
        .await;

    // Tombstoned, not deleted: its URL is still valid, so the row stays until the GC.
    let deleting: Option<String> = w.chunk_column(pending, "deleting_at::text").await;
    assert!(deleting.is_some());
    let referenced: Option<String> = w.chunk_column(kept[0], "deleting_at::text").await;
    assert!(referenced.is_none(), "referenced chunks aren't tombstoned");
    // It left the lookups, and can't be committed.
    let again = alice.looked_up(lib.id, b"cancelled").await;
    assert_ne!(again.declare.chunk, pending);
    let commit =
        json!({ "library": lib.id, "parent": lib.root, "name": "c.bin", "chunks": [pending] });
    assert!(
        alice
            .json::<api::Committed>("POST", "/api/v1/files/commit", &commit)
            .await
            .is_err()
    );
    // The filesystem endpoint refuses uploads to a tombstoned chunk; on S3 the
    // object may land, and the GC deletes it once the URL has expired.
    if let Kind::Filesystem = kind {
        assert!(!alice.put_url(&url, prepared.stored).await.is_success());
    }
    assert_eq!(alice.download(committed.file).await.unwrap(), data(MIB, 12));
}

async fn quota(kind: Kind, pool: PgPool) {
    let w = world(kind, pool).await;
    let mut alice = w.as_user(1).await;
    let lib = alice.create_library("misc").await;
    w.sql("UPDATE organization SET quota = 3000 WHERE pk = 1")
        .await;

    // 2000 bytes (incompressible): fits.
    alice
        .upload(&lib, new_file(&lib, "a.bin"), &data(2000, 13))
        .await
        .unwrap();
    // 2000 more: over the quota, refused at upload-urls, nothing declared.
    let over = alice.looked_up(lib.id, &data(2000, 14)).await;
    let (status, error) = alice.upload_urls(&[over.declare]).await.unwrap_err();
    assert_eq!(
        (status, error["code"].as_str()),
        (StatusCode::CONFLICT, Some("quota_exceeded"))
    );
    let declared: Option<i64> = w.chunk_column(over.declare.chunk, "stored_size").await;
    assert_eq!(declared, None, "nothing changed");

    // Tombstoned chunks still count until actually deleted: 2000 + 900 used.
    let (small, _) = alice.pending(lib.id, &data(900, 15)).await;
    alice.cancel(&[small.declare.chunk]).await;
    let tiny = alice.looked_up(lib.id, &data(200, 16)).await;
    assert_eq!(
        alice.upload_urls(&[tiny.declare]).await.unwrap_err().1["code"],
        "quota_exceeded"
    );

    // Unlimited again, and other organizations were never affected.
    w.sql("UPDATE organization SET quota = NULL WHERE pk = 1")
        .await;
    assert!(alice.upload_urls(&[tiny.declare]).await.is_ok());
    let mut carol = w.as_user(3).await;
    let c = carol.create_library("c").await;
    carol
        .upload(&c, new_file(&c, "big.bin"), &data(5000, 17))
        .await
        .unwrap();
}

async fn url_lifetime_setting(kind: Kind, pool: PgPool) {
    let w = world(kind, pool).await;
    w.sql("INSERT INTO setting (key, value) VALUES ('presigned_url_lifetime_secs', '120')")
        .await;
    let alice = w.as_user_on(app(&w.pool).await, 1).await;
    let lib = alice.create_library("misc").await;
    let (prepared, _) = alice.pending(lib.id, b"lifetime").await;
    // url_expires_at = now + lifetime (120 s) + margin (60 s).
    let remaining: f64 = w
        .chunk_column(
            prepared.declare.chunk,
            "extract(epoch FROM url_expires_at - now())::float8",
        )
        .await;
    assert!((170.0..=181.0).contains(&remaining), "{remaining}");
}

async fn presigned_urls_cannot_be_tampered_with(kind: Kind, pool: PgPool) {
    let w = world(kind, pool).await;
    let alice = w.as_user(1).await;
    let lib = alice.create_library("misc").await;
    let anomalies = Anomalies::capture();
    let (prepared, url) = alice.pending(lib.id, b"tamper").await;
    let headers: Vec<(&str, &str)> = url
        .headers
        .iter()
        .map(|(k, v)| (k.as_str(), v.as_str()))
        .collect();
    let tampered = format!("{}0", url.url);
    assert!(
        !alice
            .blob("PUT", &tampered, &headers, Some(prepared.stored.clone()))
            .await
            .is_success()
    );
    if let Kind::Filesystem = kind {
        assert!(anomalies.logged("invalid_signature", prepared.declare.chunk));
    }
    assert!(alice.put_url(&url, prepared.stored).await.is_success());
}

// Dedup scope.

async fn namespaces_decide_who_dedups_with_whom(kind: Kind, pool: PgPool) {
    let w = world(kind, pool).await;
    let mut alice = w.as_user(1).await;
    let a = alice.create_library("a").await;
    let b = alice.create_library("b").await;
    let outside = alice.create_library("outside").await;
    let ns = w.namespace(&[a.id, b.id]).await;
    let content = data(2 * MIB, 18);
    alice
        .upload(&a, new_file(&a, "f.bin"), &content)
        .await
        .unwrap();

    let before = alice.puts;
    alice
        .upload(&b, new_file(&b, "f.bin"), &content)
        .await
        .unwrap();
    assert_eq!(alice.puts, before, "b dedups with a (common namespace)");

    let before = alice.puts;
    alice
        .upload(&outside, new_file(&outside, "f.bin"), &content)
        .await
        .unwrap();
    assert!(alice.puts > before, "outside sees neither a nor b");

    // Removing the namespace: b's file is intact, new uploads don't see a anymore.
    let b_file = alice
        .get::<Vec<api::Node>>(&format!("/api/v1/nodes/{}/children", b.root))
        .await
        .unwrap()[0]
        .id;
    w.sql(&format!("DELETE FROM library_namespace WHERE namespace = (SELECT pk FROM namespace WHERE uuid = '{ns}')"))
        .await;
    assert_eq!(
        alice.download(b_file).await.unwrap(),
        content,
        "b keeps the chunks it borrowed"
    );
    let only_in_a = data(MIB, 19);
    alice
        .upload(&a, new_file(&a, "g.bin"), &only_in_a)
        .await
        .unwrap();
    let before = alice.puts;
    alice
        .upload(&b, new_file(&b, "g.bin"), &only_in_a)
        .await
        .unwrap();
    assert!(
        alice.puts > before,
        "after the removal, b uploads blocks only present in a"
    );
}

async fn pending_chunks_only_match_within_their_library(kind: Kind, pool: PgPool) {
    let w = world(kind, pool).await;
    let alice = w.as_user(1).await;
    let a = alice.create_library("a").await;
    let b = alice.create_library("b").await;
    w.namespace(&[a.id, b.id]).await;
    let in_a = alice.looked_up(a.id, b"pending").await.declare.chunk;
    let again = alice.looked_up(a.id, b"pending").await.declare.chunk;
    assert_eq!(in_a, again, "the same pending chunk within a");
    let in_b = alice.looked_up(b.id, b"pending").await.declare.chunk;
    assert_ne!(
        in_a, in_b,
        "b doesn't see a's pending chunk, even in a common namespace"
    );
}

async fn dedup_never_crosses_storages_or_organizations(kind: Kind, pool: PgPool) {
    let w = world(kind, pool).await;
    let mut alice = w.as_user(1).await;
    let a = alice.create_library("a").await;
    let b = alice.create_library("b").await;
    w.namespace(&[a.id, b.id]).await;
    w.sql(&format!(
        "UPDATE library SET storage = 2 WHERE uuid = '{}'",
        b.id
    ))
    .await;
    let content = data(MIB, 20);
    alice
        .upload(&a, new_file(&a, "f.bin"), &content)
        .await
        .unwrap();
    let before = alice.puts;
    alice
        .upload(&b, new_file(&b, "f.bin"), &content)
        .await
        .unwrap();
    assert!(alice.puts > before, "no dedup across storages");

    let mut carol = w.as_user(3).await;
    let c = carol.create_library("c").await;
    carol
        .upload(&c, new_file(&c, "f.bin"), &content)
        .await
        .unwrap();
    assert!(carol.puts > 0, "no dedup across organizations");
}

// Authorization.

async fn members_only(kind: Kind, pool: PgPool) {
    let w = world(kind, pool).await;
    let mut alice = w.as_user(1).await;
    let lib = alice.create_library("private").await;
    let file = alice
        .upload(&lib, new_file(&lib, "f.bin"), &data(MIB, 21))
        .await
        .unwrap()
        .file;

    let mut bob = w.as_user(2).await;
    assert!(
        bob.get::<Vec<api::Library>>("/api/v1/libraries")
            .await
            .unwrap()
            .is_empty()
    );
    let children = format!("/api/v1/nodes/{}/children", lib.root);
    assert_eq!(
        bob.get::<Value>(&children).await.unwrap_err().0,
        StatusCode::NOT_FOUND
    );
    assert_eq!(
        bob.download(file).await.unwrap_err().0,
        StatusCode::NOT_FOUND
    );
    assert_eq!(
        bob.upload(&lib, new_file(&lib, "g.bin"), b"x")
            .await
            .unwrap_err()
            .0,
        StatusCode::NOT_FOUND
    );

    w.add_member(lib.id, 2, "read").await;
    assert_eq!(
        bob.get::<Vec<api::Library>>("/api/v1/libraries")
            .await
            .unwrap()
            .len(),
        1
    );
    assert_eq!(bob.download(file).await.unwrap(), data(MIB, 21));
    assert_eq!(
        bob.upload(&lib, new_file(&lib, "g.bin"), b"x")
            .await
            .unwrap_err()
            .0,
        StatusCode::FORBIDDEN
    );

    assert_eq!(
        bob.call("GET", "/api/v1/libraries", None, false).await.0,
        StatusCode::UNAUTHORIZED
    );
}

async fn commit_refuses_chunks_outside_the_dedup_scope(kind: Kind, pool: PgPool) {
    let w = world(kind, pool).await;
    let mut alice = w.as_user(1).await;
    let secret = alice.create_library("secret").await;
    let other = alice.create_library("other").await;
    let anomalies = Anomalies::capture();
    let file = alice
        .upload(&secret, new_file(&secret, "s.bin"), &data(MIB, 22))
        .await
        .unwrap()
        .file;
    let stolen = alice.file_chunks(file).await;
    let commit =
        json!({ "library": other.id, "parent": other.root, "name": "x.bin", "chunks": stolen });
    let (status, error) = alice
        .json::<api::Committed>("POST", "/api/v1/files/commit", &commit)
        .await
        .unwrap_err();
    assert_eq!(
        (status, error["code"].as_str()),
        (StatusCode::FORBIDDEN, Some("chunk_not_allowed"))
    );
    assert!(anomalies.logged("commit_chunk_not_allowed", alice.account));
}

async fn commit_errors(kind: Kind, pool: PgPool) {
    let w = world(kind, pool).await;
    let mut alice = w.as_user(1).await;
    let lib = alice.create_library("misc").await;
    alice
        .upload(&lib, new_file(&lib, "taken.txt"), b"a")
        .await
        .unwrap();

    let (status, error) = alice
        .upload(&lib, new_file(&lib, "TAKEN.TXT"), b"b")
        .await
        .unwrap_err();
    assert_eq!(
        (status, error["code"].as_str()),
        (StatusCode::CONFLICT, Some("name_taken"))
    );
    let (status, error) = alice
        .upload(&lib, new_file(&lib, "con.txt"), b"c")
        .await
        .unwrap_err();
    assert_eq!(
        (status, error["code"].as_str()),
        (StatusCode::BAD_REQUEST, Some("invalid_request"))
    );

    // A pending chunk (looked up, never uploaded) can't be committed.
    let pending = alice.looked_up(lib.id, b"never").await.declare.chunk;
    let commit =
        json!({ "library": lib.id, "parent": lib.root, "name": "p.bin", "chunks": [pending] });
    let (status, error) = alice
        .json::<api::Committed>("POST", "/api/v1/files/commit", &commit)
        .await
        .unwrap_err();
    assert_eq!(
        (status, error["code"].as_str()),
        (StatusCode::CONFLICT, Some("chunk_missing"))
    );

    let taken = json!({ "name": "MISC", "organization": alice.org });
    let (status, error) = alice
        .json::<api::Library>("POST", "/api/v1/libraries", &taken)
        .await
        .unwrap_err();
    assert_eq!(
        (status, error["code"].as_str()),
        (StatusCode::CONFLICT, Some("library_name_taken"))
    );
    let invalid = json!({ "name": "a/b", "organization": alice.org });
    let (status, _) = alice
        .json::<api::Library>("POST", "/api/v1/libraries", &invalid)
        .await
        .unwrap_err();
    assert_eq!(status, StatusCode::BAD_REQUEST);
}

// Attacks: storing more than the database knows (and bills).

/// Organization 1's billed bytes: the quota's sum of stored sizes.
async fn billed(w: &World) -> i64 {
    sqlx::query_scalar(
        "SELECT COALESCE(sum(stored_size), 0)::bigint FROM chunk WHERE organization = 1",
    )
    .fetch_one(&w.pool)
    .await
    .unwrap()
}

/// A URL signed for chunk A can't write chunk B's key.
async fn attack_url_for_another_chunk(kind: Kind, pool: PgPool) {
    let w = world(kind, pool).await;
    let alice = w.as_user(1).await;
    let lib = alice.create_library("misc").await;
    let (a, url_a) = alice.pending(lib.id, &data(300, 30)).await;
    let (b, _) = alice.pending(lib.id, &data(300, 31)).await;
    let (a_id, b_id) = (a.declare.chunk.to_string(), b.declare.chunk.to_string());
    let forged = api::ChunkUrl {
        chunk: b.declare.chunk,
        url: url_a.url.replace(&a_id, &b_id),
        headers: url_a.headers.clone(),
    };
    assert!(!alice.put_url(&forged, a.stored.clone()).await.is_success());
    assert_eq!(
        alice.announce(&[b.declare.chunk]).await.unwrap(),
        vec![b.declare.chunk],
        "nothing written for b"
    );
}

/// The PUT signature doesn't allow another method (e.g. starting a multipart upload).
async fn attack_other_methods(kind: Kind, pool: PgPool) {
    let w = world(kind, pool).await;
    let alice = w.as_user(1).await;
    let lib = alice.create_library("misc").await;
    let (prepared, url) = alice.pending(lib.id, &data(300, 32)).await;
    let headers: Vec<(&str, &str)> = url
        .headers
        .iter()
        .map(|(k, v)| (k.as_str(), v.as_str()))
        .collect();
    let multipart = format!("{}&uploads", url.url);
    assert!(
        !alice
            .blob("POST", &multipart, &headers, None)
            .await
            .is_success(),
        "multipart initiation"
    );
    assert!(
        !alice
            .blob("POST", &url.url, &headers, Some(prepared.stored.clone()))
            .await
            .is_success(),
        "POST"
    );
    assert_eq!(
        alice.announce(&[prepared.declare.chunk]).await.unwrap(),
        vec![prepared.declare.chunk]
    );
}

/// Cancelling then uploading anyway gains nothing: the row stays and is billed
/// until the GC deletes the object (once the URL has expired) and then the row.
async fn attack_upload_after_cancel_is_billed(kind: Kind, pool: PgPool) {
    let w = world(kind, pool).await;
    let alice = w.as_user(1).await;
    let lib = alice.create_library("misc").await;
    let (prepared, url) = alice.pending(lib.id, &data(700, 33)).await;
    let before = billed(&w).await;
    alice.cancel(&[prepared.declare.chunk]).await;
    let _ = alice.put_url(&url, prepared.stored.clone()).await; // S3 may accept it
    assert_eq!(billed(&w).await, before, "still billed after the cancel");
    let size: Option<i64> = w.chunk_column(prepared.declare.chunk, "stored_size").await;
    assert_eq!(size, Some(prepared.stored.len() as i64));
}

/// Two declarations racing for the last bytes of the quota: exactly one wins.
async fn attack_concurrent_declarations(kind: Kind, pool: PgPool) {
    let w = world(kind, pool).await;
    let alice = w.as_user(1).await;
    let lib = alice.create_library("misc").await;
    w.sql("UPDATE organization SET quota = 3000 WHERE pk = 1")
        .await;
    let a = alice.looked_up(lib.id, &data(2000, 34)).await;
    let b = alice.looked_up(lib.id, &data(2000, 35)).await;
    let (da, db) = ([a.declare], [b.declare]);
    let (ra, rb) = tokio::join!(alice.upload_urls(&da), alice.upload_urls(&db));
    assert_eq!(
        ra.is_ok() as u8 + rb.is_ok() as u8,
        1,
        "exactly one declaration fits"
    );
    assert!(billed(&w).await <= 3000);
}

/// Unsigned headers (metadata, tagging, storage class) can't inflate an object.
async fn attack_unsigned_headers(kind: Kind, pool: PgPool) {
    let w = world(kind, pool).await;
    let alice = w.as_user(1).await;
    let lib = alice.create_library("misc").await;
    let (prepared, url) = alice.pending(lib.id, &data(400, 36)).await;
    let junk = "x".repeat(1500);
    let mut headers: Vec<(&str, &str)> = url
        .headers
        .iter()
        .map(|(k, v)| (k.as_str(), v.as_str()))
        .collect();
    headers.push(("x-amz-meta-junk", &junk));
    headers.push(("x-amz-tagging", "a=b"));
    headers.push(("x-amz-storage-class", "GLACIER"));
    let status = alice
        .blob("PUT", &url.url, &headers, Some(prepared.stored.clone()))
        .await;
    // Refused (unsigned x-amz-* headers), or accepted with exactly the declared bytes.
    if status.is_success() {
        assert!(
            alice
                .announce(&[prepared.declare.chunk])
                .await
                .unwrap()
                .is_empty(),
            "size as declared"
        );
    }
}

/// At most `max_pending_uploads` chunks being uploaded per user (declared, URL
/// still valid, not uploaded): clients upload in batches.
async fn attack_pending_upload_cap(kind: Kind, pool: PgPool) {
    let w = world(kind, pool).await;
    w.sql("INSERT INTO setting (key, value) VALUES ('max_pending_uploads', '3')")
        .await;
    let app = app(&w.pool).await;
    let alice = w.as_user_on(app.clone(), 1).await;
    let bob = w.as_user_on(app, 2).await;
    let lib = alice.create_library("misc").await;
    w.add_member(lib.id, 2, "write").await;

    // Lookups are not limited.
    let mut chunks = Vec::new();
    for i in 0..6 {
        chunks.push(alice.looked_up(lib.id, &data(100, 50 + i)).await);
    }
    let declares: Vec<api::Declare> = chunks.iter().map(|c| c.declare).collect();
    let (status, error) = alice.upload_urls(&declares[..4]).await.unwrap_err();
    assert_eq!(
        (status, error["code"].as_str()),
        (
            StatusCode::TOO_MANY_REQUESTS,
            Some("too_many_pending_uploads")
        )
    );
    let urls = alice.upload_urls(&declares[..3]).await.unwrap();
    assert!(
        alice.upload_urls(&declares[3..4]).await.is_err(),
        "the 4th waits"
    );
    assert!(
        alice.upload_urls(&declares[..3]).await.is_ok(),
        "renewing URLs takes no new slot"
    );

    // Per user: Bob has his own slots.
    assert!(bob.upload_urls(&declares[3..4]).await.is_ok());

    // Uploading and announcing frees a slot; so does cancelling.
    assert!(
        alice
            .put_url(&urls[0], chunks[0].stored.clone())
            .await
            .is_success()
    );
    assert!(
        alice
            .announce(&[declares[0].chunk])
            .await
            .unwrap()
            .is_empty()
    );
    assert!(alice.upload_urls(&declares[4..5]).await.is_ok());
    alice.cancel(&[declares[1].chunk]).await;
    assert!(alice.upload_urls(&declares[5..6]).await.is_ok());
}

/// After every attack above, the database knows every stored byte: each object
/// has a chunk row whose stored size is its real size (and, on the filesystem,
/// where the directory can be listed, there is no object without a row).
async fn every_stored_byte_is_billed(kind: Kind, pool: PgPool) {
    let w = world(kind, pool.clone()).await;
    let mut alice = w.as_user(1).await;
    let lib = alice.create_library("misc").await;
    alice
        .upload(&lib, new_file(&lib, "ok.bin"), &data(3 * MIB, 37))
        .await
        .unwrap();
    let (a, url_a) = alice.pending(lib.id, &data(500, 38)).await;
    let (b, url_b) = alice.pending(lib.id, &data(500, 39)).await;
    let _ = alice.put_url(&url_a, data(5000, 40)).await; // bigger than declared
    let mut altered = a.stored.clone();
    altered[0] ^= 1;
    let _ = alice.put_url(&url_a, altered).await; // same size, other bytes
    let forged = api::ChunkUrl {
        chunk: b.declare.chunk,
        url: url_a
            .url
            .replace(&a.declare.chunk.to_string(), &b.declare.chunk.to_string()),
        headers: url_a.headers.clone(),
    };
    let _ = alice.put_url(&forged, a.stored.clone()).await; // a's URL on b's key
    alice.cancel(&[b.declare.chunk]).await;
    let _ = alice.put_url(&url_b, b.stored.clone()).await; // after cancel
    let _ = alice.put_url(&url_a, a.stored.clone()).await; // the honest upload

    let backends = Backends::new(pool.clone());
    let rows: Vec<(Uuid, i64, Option<i64>)> =
        sqlx::query_as("SELECT uuid, storage, stored_size FROM chunk")
            .fetch_all(&pool)
            .await
            .unwrap();
    for (chunk, storage, stored_size) in &rows {
        if let Some(size) = backends
            .get(*storage)
            .await
            .unwrap()
            .size(*chunk)
            .await
            .unwrap()
        {
            assert_eq!(
                Some(size as i64),
                *stored_size,
                "object {chunk} has {size} bytes, the DB knows {stored_size:?}"
            );
        }
    }
    if let Kind::Filesystem = kind {
        for dir in &w._dirs {
            for entry in std::fs::read_dir(dir.path()).unwrap() {
                let entry = entry.unwrap();
                let name = entry.file_name().to_string_lossy().into_owned();
                let known = rows.iter().find(|(chunk, _, _)| chunk.to_string() == name);
                let (_, _, size) =
                    known.unwrap_or_else(|| panic!("object {name} has no chunk row"));
                assert_eq!(Some(entry.metadata().unwrap().len() as i64), *size);
            }
        }
    }
}
