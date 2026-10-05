//! The upload protocol end to end, through the router, on a real Postgres and a
//! temporary filesystem backend: lookup, presigned upload, announce, commit,
//! download. Plus the dedup scope and authorization rules the code enforces
//! (docs/db.md, code-checked invariants).

use std::sync::Arc;

use axum::Router;
use axum::body::{Body, to_bytes};
use axum::http::{Request, StatusCode};
use serde::Serialize;
use serde::de::DeserializeOwned;
use serde_json::{Value, json};
use sqlx::PgPool;
use theca_common::api::{self, ChunkState};
use theca_common::chunking::chunks;
use theca_common::compression::{CompressionPolicy, compress, decompress};
use theca_common::hash::ChunkHash;
use theca_core::api::{AppState, router};
use theca_core::settings::Settings;
use theca_core::storage::UrlSigner;
use tower::ServiceExt;
use uuid::Uuid;

const PUBLIC_URL: &str = "http://test";

/// Two organizations, users and storages:
/// - organization 1: users 1 (owner) and 2; organization 2: user 3 (owner)
/// - storage 1 (shared, instance default) and storage 2 (shared), each a temp dir
struct World {
    app: Router,
    pool: PgPool,
    users: [Uuid; 3],
    _dirs: [tempfile::TempDir; 2],
}

async fn world(pool: PgPool) -> World {
    let dirs = [tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap()];
    let details = |d: &tempfile::TempDir| json!({ "path": d.path() }).to_string();
    sqlx::raw_sql(&format!(
        r#"
        BEGIN;
        INSERT INTO account (pk, email) OVERRIDING SYSTEM VALUE VALUES (1, 'a@x.ch'), (2, 'b@x.ch'), (3, 'c@x.ch');
        INSERT INTO organization (pk, name, owner) OVERRIDING SYSTEM VALUE VALUES (1, 'one', 1), (2, 'two', 3);
        INSERT INTO "user" (pk, account, organization, joined_at) OVERRIDING SYSTEM VALUE
            VALUES (1, 1, 1, now()), (2, 2, 1, now()), (3, 3, 2, now());
        INSERT INTO storage (pk, name, implementation, details, is_default) OVERRIDING SYSTEM VALUE
            VALUES (1, 'main', 'filesystem', '{}', true), (2, 'other', 'filesystem', '{}', false);
        COMMIT;
        "#,
        details(&dirs[0]),
        details(&dirs[1]),
    ))
    .execute(&pool)
    .await
    .unwrap();
    let users: Vec<Uuid> = sqlx::query_scalar(r#"SELECT uuid FROM "user" ORDER BY pk"#)
        .fetch_all(&pool)
        .await
        .unwrap();
    let settings = Arc::new(Settings::load(&pool).await.unwrap());
    let state = AppState {
        pool: pool.clone(),
        settings,
        signer: UrlSigner::new([9; 32], PUBLIC_URL),
    };
    World {
        app: router(state),
        pool,
        users: [users[0], users[1], users[2]],
        _dirs: dirs,
    }
}

impl World {
    fn as_user(&self, n: usize) -> Client {
        Client {
            app: self.app.clone(),
            user: self.users[n - 1],
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
}

/// A test client acting as one user. Counts presigned uploads (`puts`).
struct Client {
    app: Router,
    user: Uuid,
    puts: usize,
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
            request = request.header("x-theca-user", self.user.to_string());
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
    ) -> Result<T, (StatusCode, Value)> {
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

    async fn get<T: DeserializeOwned>(&self, path: &str) -> Result<T, (StatusCode, Value)> {
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

    /// A presigned URL: no authentication header, only the signature.
    async fn blob(&self, method: &str, url: &str, body: Option<Vec<u8>>) -> (StatusCode, Vec<u8>) {
        self.call(method, url.strip_prefix(PUBLIC_URL).unwrap(), body, false)
            .await
    }

    async fn create_library(&self, name: &str) -> api::Library {
        self.json("POST", "/api/v1/libraries", &json!({ "name": name }))
            .await
            .unwrap()
    }

    /// The whole protocol: chunk, hash, compress, lookup, upload, announce, commit.
    async fn upload(
        &mut self,
        library: &api::Library,
        target: Value,
        data: &[u8],
    ) -> Result<api::Committed, (StatusCode, Value)> {
        let parts: Vec<(ChunkHash, Vec<u8>)> = chunks(data)
            .map(|c| {
                let c = c.unwrap();
                (ChunkHash::of(&c.data), c.data)
            })
            .collect();
        let hashes: Vec<ChunkHash> = parts.iter().map(|(h, _)| *h).collect();
        let entries: Vec<api::LookupEntry> = self
            .json(
                "POST",
                "/api/v1/chunks/lookup",
                &json!({ "library": library.id, "hashes": hashes }),
            )
            .await?;
        let chunk_of = |h: &ChunkHash| entries.iter().find(|e| e.hash == *h).unwrap();

        let to_upload: Vec<Uuid> = {
            let mut v: Vec<Uuid> = entries
                .iter()
                .filter(|e| e.state == ChunkState::Upload)
                .map(|e| e.chunk)
                .collect();
            v.dedup();
            v
        };
        if !to_upload.is_empty() {
            let urls: Vec<api::ChunkUrl> = self
                .json(
                    "POST",
                    "/api/v1/chunks/upload-urls",
                    &json!({ "chunks": to_upload }),
                )
                .await?;
            let mut announces = Vec::new();
            for url in &urls {
                let (hash, plain) = parts
                    .iter()
                    .find(|(h, _)| chunk_of(h).chunk == url.chunk)
                    .unwrap();
                let (compression, stored) = compress(&CompressionPolicy::default(), plain).unwrap();
                assert_eq!(
                    self.blob("PUT", &url.url, Some(stored)).await.0,
                    StatusCode::OK,
                    "{hash}"
                );
                self.puts += 1;
                announces.push(api::Announce {
                    chunk: url.chunk,
                    plain_size: plain.len() as u64,
                    compression,
                });
            }
            let missing: Vec<api::ChunkUrl> = self
                .json("POST", "/api/v1/chunks/announce", &announces)
                .await?;
            assert!(missing.is_empty(), "every uploaded chunk is announced");
        }

        let mut commit = target;
        commit["library"] = json!(library.id);
        commit["chunks"] = json!(hashes.iter().map(|h| chunk_of(h).chunk).collect::<Vec<_>>());
        self.json("POST", "/api/v1/files/commit", &commit).await
    }

    /// Downloads a file's current version, checking every chunk's hash.
    async fn download(&self, file: Uuid) -> Result<Vec<u8>, (StatusCode, Value)> {
        let download: api::Download = self.get(&format!("/api/v1/files/{file}/download")).await?;
        let mut data = Vec::new();
        for chunk in download.chunks {
            let (status, stored) = self.blob("GET", &chunk.url, None).await;
            assert_eq!(status, StatusCode::OK);
            let plain = decompress(chunk.compression, &stored).unwrap();
            assert_eq!(ChunkHash::of(&plain), chunk.hash);
            data.extend(plain);
        }
        Ok(data)
    }
}

fn new_file(library: &api::Library, name: &str) -> Value {
    json!({ "parent": library.root, "name": name })
}

/// Deterministic pseudo-random bytes (incompressible, so chunks are distinct).
fn data(len: usize, seed: u64) -> Vec<u8> {
    let mut s = seed | 1;
    (0..len)
        .map(|_| {
            s ^= s << 13;
            s ^= s >> 7;
            s ^= s << 17;
            s as u8
        })
        .collect()
}

const MIB: usize = 1024 * 1024;

// Protocol.

#[sqlx::test]
async fn upload_then_download_identical(pool: PgPool) {
    let w = world(pool).await;
    let mut alice = w.as_user(1);
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

#[sqlx::test]
async fn reupload_uploads_nothing(pool: PgPool) {
    let w = world(pool).await;
    let mut alice = w.as_user(1);
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

#[sqlx::test]
async fn edit_uploads_only_new_chunks(pool: PgPool) {
    let w = world(pool).await;
    let mut alice = w.as_user(1);
    let lib = alice.create_library("docs").await;
    let mut content = data(8 * MIB, 3);
    let v1 = alice
        .upload(&lib, new_file(&lib, "doc.bin"), &content)
        .await
        .unwrap();
    let total = alice.puts;
    content[4 * MIB..4 * MIB + 100].fill(0xAB); // edit in the middle
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

#[sqlx::test]
async fn concurrent_edits_conflict(pool: PgPool) {
    let w = world(pool).await;
    let mut alice = w.as_user(1);
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

#[sqlx::test]
async fn empty_file(pool: PgPool) {
    let w = world(pool).await;
    let mut alice = w.as_user(1);
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

#[sqlx::test]
async fn cancel_deletes_only_unreferenced_chunks(pool: PgPool) {
    let w = world(pool).await;
    let mut alice = w.as_user(1);
    let lib = alice.create_library("misc").await;
    let committed = alice
        .upload(&lib, new_file(&lib, "kept.bin"), &data(MIB, 7))
        .await
        .unwrap();
    let kept: Vec<Uuid> = alice
        .get::<api::Download>(&format!("/api/v1/files/{}/download", committed.file))
        .await
        .unwrap()
        .chunks
        .iter()
        .map(|c| c.chunk)
        .collect();
    // A pending chunk, uploaded but never committed.
    let entries: Vec<api::LookupEntry> = alice
        .json(
            "POST",
            "/api/v1/chunks/lookup",
            &json!({ "library": lib.id, "hashes": [ChunkHash::of(b"x")] }),
        )
        .await
        .unwrap();
    let pending = entries[0].chunk;
    let urls: Vec<api::ChunkUrl> = alice
        .json(
            "POST",
            "/api/v1/chunks/upload-urls",
            &json!({ "chunks": [pending] }),
        )
        .await
        .unwrap();
    alice.blob("PUT", &urls[0].url, Some(b"x".to_vec())).await;

    let all: Vec<Uuid> = kept.iter().copied().chain([pending]).collect();
    let _: () = alice
        .json("POST", "/api/v1/chunks/cancel", &json!({ "chunks": all }))
        .await
        .unwrap();
    let remaining: Vec<Uuid> = sqlx::query_scalar("SELECT uuid FROM chunk")
        .fetch_all(&w.pool)
        .await
        .unwrap();
    assert!(!remaining.contains(&pending), "the pending chunk is gone");
    assert!(
        kept.iter().all(|k| remaining.contains(k)),
        "referenced chunks stay"
    );
    assert_eq!(
        alice
            .blob("GET", &urls[0].url.replace("op=put", "op=get"), None)
            .await
            .0,
        StatusCode::FORBIDDEN
    );
    assert_eq!(alice.download(committed.file).await.unwrap(), data(MIB, 7));
}

#[sqlx::test]
async fn presigned_urls_cannot_be_tampered_with(pool: PgPool) {
    let w = world(pool).await;
    let alice = w.as_user(1);
    let lib = alice.create_library("misc").await;
    let entries: Vec<api::LookupEntry> = alice
        .json(
            "POST",
            "/api/v1/chunks/lookup",
            &json!({ "library": lib.id, "hashes": [ChunkHash::of(b"y")] }),
        )
        .await
        .unwrap();
    let urls: Vec<api::ChunkUrl> = alice
        .json(
            "POST",
            "/api/v1/chunks/upload-urls",
            &json!({ "chunks": [entries[0].chunk] }),
        )
        .await
        .unwrap();
    let url = &urls[0].url;
    let (head, sig) = url.rsplit_once("sig=").unwrap();
    let tampered = format!(
        "{head}sig={}{}",
        if sig.starts_with('0') { '1' } else { '0' },
        &sig[1..]
    );
    assert_eq!(
        alice.blob("PUT", &tampered, Some(b"y".to_vec())).await.0,
        StatusCode::FORBIDDEN
    );
    let other_expiry = url.replace("expires=", "expires=1");
    assert_eq!(
        alice
            .blob("PUT", &other_expiry, Some(b"y".to_vec()))
            .await
            .0,
        StatusCode::FORBIDDEN
    );
    assert_eq!(
        alice.blob("PUT", url, Some(b"y".to_vec())).await.0,
        StatusCode::OK
    );
}

// Dedup scope.

#[sqlx::test]
async fn namespaces_decide_who_dedups_with_whom(pool: PgPool) {
    let w = world(pool).await;
    let mut alice = w.as_user(1);
    let a = alice.create_library("a").await;
    let b = alice.create_library("b").await;
    let outside = alice.create_library("outside").await;
    let ns = w.namespace(&[a.id, b.id]).await;
    let content = data(2 * MIB, 8);
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

    // Removing b from the namespace: its file is intact, new uploads don't see a anymore.
    let b_file = alice
        .get::<Vec<api::Node>>(&format!("/api/v1/nodes/{}/children", b.root))
        .await
        .unwrap()[0]
        .id;
    w.sql(&format!(
        "DELETE FROM library_namespace WHERE namespace = (SELECT pk FROM namespace WHERE uuid = '{ns}')"
    ))
    .await;
    assert_eq!(
        alice.download(b_file).await.unwrap(),
        content,
        "b keeps the chunks it borrowed"
    );
    let only_in_a = data(MIB, 9);
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

#[sqlx::test]
async fn pending_chunks_only_match_within_their_library(pool: PgPool) {
    let w = world(pool).await;
    let alice = w.as_user(1);
    let a = alice.create_library("a").await;
    let b = alice.create_library("b").await;
    w.namespace(&[a.id, b.id]).await;
    let hash = ChunkHash::of(b"pending");
    let lookup = |lib: Uuid| json!({ "library": lib, "hashes": [hash] });
    let in_a: Vec<api::LookupEntry> = alice
        .json("POST", "/api/v1/chunks/lookup", &lookup(a.id))
        .await
        .unwrap();
    let again: Vec<api::LookupEntry> = alice
        .json("POST", "/api/v1/chunks/lookup", &lookup(a.id))
        .await
        .unwrap();
    assert_eq!(
        in_a[0].chunk, again[0].chunk,
        "the same pending chunk within a"
    );
    let in_b: Vec<api::LookupEntry> = alice
        .json("POST", "/api/v1/chunks/lookup", &lookup(b.id))
        .await
        .unwrap();
    assert_ne!(
        in_a[0].chunk, in_b[0].chunk,
        "b doesn't see a's pending chunk, even in a common namespace"
    );
}

#[sqlx::test]
async fn dedup_never_crosses_storages_or_organizations(pool: PgPool) {
    let w = world(pool).await;
    let mut alice = w.as_user(1);
    let a = alice.create_library("a").await;
    let b = alice.create_library("b").await;
    w.namespace(&[a.id, b.id]).await;
    // b moves to storage 2: same namespace, other storage.
    w.sql(&format!(
        "UPDATE library SET storage = 2 WHERE uuid = '{}'",
        b.id
    ))
    .await;
    let content = data(MIB, 10);
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

    let mut carol = w.as_user(3);
    let c = carol.create_library("c").await;
    carol
        .upload(&c, new_file(&c, "f.bin"), &content)
        .await
        .unwrap();
    assert!(carol.puts > 0, "no dedup across organizations");
}

// Authorization.

#[sqlx::test]
async fn members_only(pool: PgPool) {
    let w = world(pool).await;
    let mut alice = w.as_user(1);
    let lib = alice.create_library("private").await;
    let file = alice
        .upload(&lib, new_file(&lib, "f.bin"), &data(MIB, 11))
        .await
        .unwrap()
        .file;

    // Bob, same organization, not a member: sees nothing.
    let mut bob = w.as_user(2);
    let libs: Vec<api::Library> = bob.get("/api/v1/libraries").await.unwrap();
    assert!(libs.is_empty());
    assert_eq!(
        bob.get::<Value>(&format!("/api/v1/nodes/{}/children", lib.root))
            .await
            .unwrap_err()
            .0,
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

    // Read member: lists and downloads, can't upload.
    w.add_member(lib.id, 2, "read").await;
    assert_eq!(
        bob.get::<Vec<api::Library>>("/api/v1/libraries")
            .await
            .unwrap()
            .len(),
        1
    );
    assert_eq!(bob.download(file).await.unwrap(), data(MIB, 11));
    assert_eq!(
        bob.upload(&lib, new_file(&lib, "g.bin"), b"x")
            .await
            .unwrap_err()
            .0,
        StatusCode::FORBIDDEN
    );

    // No user header at all.
    assert_eq!(
        bob.call("GET", "/api/v1/libraries", None, false).await.0,
        StatusCode::UNAUTHORIZED
    );
}

#[sqlx::test]
async fn commit_refuses_chunks_outside_the_dedup_scope(pool: PgPool) {
    let w = world(pool).await;
    let mut alice = w.as_user(1);
    let secret = alice.create_library("secret").await;
    let other = alice.create_library("other").await; // not in a common namespace
    let file = alice
        .upload(&secret, new_file(&secret, "s.bin"), &data(MIB, 12))
        .await
        .unwrap()
        .file;
    let stolen: Vec<Uuid> = alice
        .get::<api::Download>(&format!("/api/v1/files/{file}/download"))
        .await
        .unwrap()
        .chunks
        .iter()
        .map(|c| c.chunk)
        .collect();
    let commit =
        json!({ "library": other.id, "parent": other.root, "name": "x.bin", "chunks": stolen });
    let (status, error) = alice
        .json::<api::Committed>("POST", "/api/v1/files/commit", &commit)
        .await
        .unwrap_err();
    assert_eq!(status, StatusCode::FORBIDDEN);
    assert_eq!(error["code"], "chunk_not_allowed");
}

#[sqlx::test]
async fn commit_errors(pool: PgPool) {
    let w = world(pool).await;
    let mut alice = w.as_user(1);
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
    let entries: Vec<api::LookupEntry> = alice
        .json(
            "POST",
            "/api/v1/chunks/lookup",
            &json!({ "library": lib.id, "hashes": [ChunkHash::of(b"never")] }),
        )
        .await
        .unwrap();
    let commit = json!({ "library": lib.id, "parent": lib.root, "name": "p.bin", "chunks": [entries[0].chunk] });
    let (status, error) = alice
        .json::<api::Committed>("POST", "/api/v1/files/commit", &commit)
        .await
        .unwrap_err();
    assert_eq!(
        (status, error["code"].as_str()),
        (StatusCode::CONFLICT, Some("chunk_missing"))
    );

    // Library names: taken (ignoring case) and invalid.
    let (status, error) = alice
        .json::<api::Library>("POST", "/api/v1/libraries", &json!({ "name": "MISC" }))
        .await
        .unwrap_err();
    assert_eq!(
        (status, error["code"].as_str()),
        (StatusCode::CONFLICT, Some("library_name_taken"))
    );
    let (status, _) = alice
        .json::<api::Library>("POST", "/api/v1/libraries", &json!({ "name": "a/b" }))
        .await
        .unwrap_err();
    assert_eq!(status, StatusCode::BAD_REQUEST);
}
