// The test harness shared by the API tests (`include!`d: a module would
// need every item public). A fixture world, logged-in clients, the upload
// protocol, downloads, anomalies.

use std::sync::Mutex;

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
        INSERT INTO account (pk, email, verified) OVERRIDING SYSTEM VALUE VALUES (1, 'a@x.ch', true), (2, 'b@x.ch', true), (3, 'c@x.ch', true);
        INSERT INTO identity (account, kind, details)
            SELECT pk, 'password', jsonb_build_object('hash', '{hash}') FROM account;
        INSERT INTO organization (pk, name, owner) OVERRIDING SYSTEM VALUE VALUES (1, 'one', 1), (2, 'two', 3);
        INSERT INTO "user" (pk, account, organization, joined_at) OVERRIDING SYSTEM VALUE
            VALUES (1, 1, 1, now()), (2, 2, 1, now()), (3, 3, 2, now());
        INSERT INTO storage (pk, name, implementation, details, is_default) OVERRIDING SYSTEM VALUE
            VALUES (1, 'main', '{implementation}', '{first}', true), (2, 'other', '{implementation}', '{second}', false);
        -- The rows above have explicit keys: move the sequences past them.
        SELECT setval(pg_get_serial_sequence(t, 'pk'), 100) FROM unnest(ARRAY['account', 'organization', '"user"', 'storage']) AS t;
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
    router(state(pool).await)
}

/// The server's state, with the settings currently in the database.
async fn state(pool: &PgPool) -> AppState {
    AppState {
        pool: pool.clone(),
        settings: theca_core::settings::LiveSettings::new(Settings::load(pool).await.unwrap()),
        signer: UrlSigner::new([9; 32], PUBLIC_URL),
        backends: Backends::new(pool.clone()),
        cipher: theca_core::keys::DataCipher::new(&[7; 32]),
        setup: theca_core::api::setup::SetupState::done(),
        limits: Default::default(),
        proxies: Default::default(),
    }
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
