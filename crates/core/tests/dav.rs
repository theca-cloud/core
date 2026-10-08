//! WebDAV and previews: the tree per app password scope, reads with ranges,
//! writes deduplicated by the server, and the litmus conformance suite against
//! a real port (filesystem backend).

#![allow(dead_code, unused_imports)] // the shared harness has more than these tests use

include!("common/harness.rs");

use axum::http::HeaderMap;
use base64::Engine;

async fn fs_world(pool: PgPool) -> World {
    world(Kind::Filesystem, pool).await
}

/// A WebDAV client: Basic auth with an app password.
struct Dav {
    app: Router,
    auth: String,
}

impl Dav {
    async fn request(
        &self,
        method: &str,
        path: &str,
        headers: &[(&str, &str)],
        body: Vec<u8>,
    ) -> (StatusCode, HeaderMap, Vec<u8>) {
        let mut request = Request::builder()
            .method(method)
            .uri(format!("/dav{path}"))
            .header("authorization", &self.auth);
        for (k, v) in headers {
            request = request.header(*k, *v);
        }
        let response = self
            .app
            .clone()
            .oneshot(request.body(Body::from(body)).unwrap())
            .await
            .unwrap();
        let (parts, body) = response.into_parts();
        (
            parts.status,
            parts.headers,
            to_bytes(body, usize::MAX).await.unwrap().to_vec(),
        )
    }

    async fn status(&self, method: &str, path: &str, headers: &[(&str, &str)]) -> StatusCode {
        self.request(method, path, headers, Vec::new()).await.0
    }

    async fn put(&self, path: &str, data: &[u8]) -> StatusCode {
        self.request("PUT", path, &[], data.to_vec()).await.0
    }

    async fn get(&self, path: &str, range: Option<&str>) -> (StatusCode, Vec<u8>) {
        let headers: Vec<(&str, &str)> = range.map(|r| ("range", r)).into_iter().collect();
        let (status, _, body) = self.request("GET", path, &headers, Vec::new()).await;
        (status, body)
    }

    /// The names `PROPFIND` (depth 1) lists under a collection, sorted.
    async fn list(&self, path: &str) -> Vec<String> {
        let (status, _, body) = self
            .request("PROPFIND", path, &[("depth", "1")], Vec::new())
            .await;
        assert_eq!(
            status,
            StatusCode::MULTI_STATUS,
            "{path}: {}",
            String::from_utf8_lossy(&body)
        );
        let xml = String::from_utf8(body).unwrap();
        let prefix = format!("/dav{path}");
        let mut names: Vec<String> = xml
            .split("<D:href>")
            .skip(1)
            .filter_map(|s| s.split("</D:href>").next())
            .map(|href| href.trim_end_matches('/').to_owned())
            .filter(|href| *href != prefix.trim_end_matches('/'))
            .map(|href| percent_decode(href.rsplit('/').next().unwrap()))
            .collect();
        names.sort();
        names
    }
}

fn percent_decode(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = Vec::new();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() + 1 {
            out.push(u8::from_str_radix(&s[i + 1..i + 3], 16).unwrap());
            i += 3;
        } else {
            out.push(bytes[i]);
            i += 1;
        }
    }
    String::from_utf8(out).unwrap()
}

impl World {
    /// An app password of account `n` with `scope` (JSON of `CreateAppPassword`).
    async fn dav(&self, n: usize, scope: Value) -> Dav {
        let client = self.as_user(n).await;
        let mut scope = scope;
        scope["name"] = json!("dav");
        let created: api::NewAppPassword = client
            .json("POST", "/api/v1/auth/app-passwords", &scope)
            .await
            .unwrap();
        let basic =
            base64::engine::general_purpose::STANDARD.encode(format!("me:{}", created.password));
        Dav {
            app: self.app.clone(),
            auth: format!("Basic {basic}"),
        }
    }
}

#[sqlx::test]
async fn tree_follows_the_scope(pool: PgPool) {
    let w = fs_world(pool).await;
    let mut alice = w.as_user(1).await;
    let docs = alice.create_library("Docs").await;
    alice.create_library("Photos").await;
    alice
        .upload(&docs, new_file(&docs, "a.txt"), &data(100, 1))
        .await
        .unwrap();

    let account = w.dav(1, json!({})).await;
    assert_eq!(account.list("/").await, ["one"]);
    assert_eq!(account.list("/one/").await, ["Docs", "Photos"]);
    assert_eq!(account.list("/one/Docs/").await, ["a.txt"]);

    let org = w
        .dav(1, json!({ "organization": w.organizations[0] }))
        .await;
    assert_eq!(org.list("/").await, ["Docs", "Photos"]);

    let lib = w.dav(1, json!({ "library": docs.id })).await;
    assert_eq!(lib.list("/").await, ["a.txt"]);
    assert_eq!(
        lib.get("/a.txt", None).await,
        (StatusCode::OK, data(100, 1))
    );
    assert_eq!(
        lib.status("PROPFIND", "/Photos/", &[("depth", "0")]).await,
        StatusCode::NOT_FOUND
    );

    // No password, a wrong one, or a session: refused, with a Basic challenge.
    let anonymous = Dav {
        app: w.app.clone(),
        auth: String::new(),
    };
    let (status, headers, _) = anonymous.request("PROPFIND", "/", &[], Vec::new()).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    assert!(
        headers["www-authenticate"]
            .to_str()
            .unwrap()
            .starts_with("Basic")
    );
    let session = Dav {
        app: w.app.clone(),
        auth: String::new(),
    };
    let mut request = Request::builder()
        .method("PROPFIND")
        .uri("/dav/")
        .header("cookie", &alice.cookies);
    request = request.header("depth", "0");
    let response = session
        .app
        .clone()
        .oneshot(request.body(Body::empty()).unwrap())
        .await
        .unwrap();
    assert_eq!(
        response.status(),
        StatusCode::UNAUTHORIZED,
        "sessions don't open WebDAV"
    );
}

#[sqlx::test]
async fn reads_and_writes(pool: PgPool) {
    let w = fs_world(pool).await;
    let alice = w.as_user(1).await;
    let docs = alice.create_library("Docs").await;
    let dav = w.dav(1, json!({ "library": docs.id })).await;
    let chunks = || async {
        sqlx::query_scalar::<_, i64>("SELECT count(*) FROM chunk")
            .fetch_one(&w.pool)
            .await
            .unwrap()
    };

    // A multi-chunk file: written, read back, ranges across chunk boundaries.
    let big = data(3 * MIB, 7);
    assert_eq!(dav.put("/big.bin", &big).await, StatusCode::CREATED);
    assert_eq!(
        dav.get("/big.bin", None).await,
        (StatusCode::OK, big.clone())
    );
    let (status, part) = dav.get("/big.bin", Some("bytes=1048000-1049000")).await;
    assert_eq!(
        (status, part.as_slice()),
        (StatusCode::PARTIAL_CONTENT, &big[1_048_000..=1_049_000])
    );
    let (_, tail) = dav.get("/big.bin", Some("bytes=-10")).await;
    assert_eq!(tail, &big[big.len() - 10..]);

    // The same bytes elsewhere: deduplicated, no new chunk.
    let before = chunks().await;
    assert_eq!(
        dav.status("MKCOL", "/copies/", &[]).await,
        StatusCode::CREATED
    );
    assert_eq!(
        dav.put("/copies/again.bin", &big).await,
        StatusCode::CREATED
    );
    assert_eq!(chunks().await, before);

    // Overwriting makes a new version; the API sees the files.
    assert_eq!(
        dav.put("/big.bin", b"small now").await,
        StatusCode::NO_CONTENT
    );
    assert_eq!(dav.get("/big.bin", None).await.1, b"small now");
    let nodes: Vec<api::Node> = alice
        .get(&format!("/api/v1/nodes/{}/children", docs.root))
        .await
        .unwrap();
    let file = nodes.iter().find(|n| n.name == "big.bin").unwrap().id;
    let versions: Vec<api::Version> = alice
        .get(&format!("/api/v1/files/{file}/versions"))
        .await
        .unwrap();
    assert_eq!(versions.len(), 2);

    // Moves, copies, deletes (to the trash).
    let dest = [("destination", "/dav/copies/renamed.bin")];
    assert_eq!(
        dav.status("MOVE", "/big.bin", &dest).await,
        StatusCode::CREATED
    );
    assert_eq!(
        dav.status(
            "COPY",
            "/copies/renamed.bin",
            &[("destination", "/dav/copy.bin")]
        )
        .await,
        StatusCode::CREATED
    );
    assert_eq!(dav.list("/copies/").await, ["again.bin", "renamed.bin"]);
    assert_eq!(
        dav.status("DELETE", "/copies/", &[]).await,
        StatusCode::NO_CONTENT
    );
    assert_eq!(dav.list("/").await, ["copy.bin"]);
    let trash: Vec<api::TrashEntry> = alice
        .get(&format!("/api/v1/libraries/{}/trash", docs.id))
        .await
        .unwrap();
    assert_eq!(
        trash.len(),
        1,
        "{:?}",
        trash.iter().map(|t| &t.path).collect::<Vec<_>>()
    );

    // An empty file.
    assert_eq!(dav.put("/empty", b"").await, StatusCode::CREATED);
    assert_eq!(dav.get("/empty", None).await, (StatusCode::OK, Vec::new()));

    // Names follow our rules: refused, never renamed.
    assert_eq!(dav.put("/bad%3Cname", b"x").await, StatusCode::FORBIDDEN);
}

/// Each version keeps its client's modification time (`X-OC-Mtime`, as rclone
/// sends it): shown by WebDAV and the API; the same bytes with another time
/// update the current version (a touch, no new version); a restored version
/// shows its own; a copy keeps it.
#[sqlx::test]
async fn modification_times(pool: PgPool) {
    let w = fs_world(pool).await;
    let alice = w.as_user(1).await;
    let docs = alice.create_library("Docs").await;
    let dav = w.dav(1, json!({ "library": docs.id })).await;
    let put = |data: &'static [u8], mtime: Option<&'static str>| {
        let headers: Vec<(&str, &str)> = mtime.map(|t| ("x-oc-mtime", t)).into_iter().collect();
        let dav = &dav;
        async move {
            dav.request("PUT", "/notes.txt", &headers, data.to_vec())
                .await
        }
    };
    let file = || async {
        let nodes: Vec<api::Node> = alice
            .get(&format!("/api/v1/nodes/{}/children", docs.root))
            .await
            .unwrap();
        nodes.into_iter().find(|n| n.name == "notes.txt").unwrap()
    };
    let versions = |id: uuid::Uuid| {
        let alice = &alice;
        async move {
            alice
                .get::<Vec<api::Version>>(&format!("/api/v1/files/{id}/versions"))
                .await
                .unwrap()
        }
    };
    let lastmodified = |path: &'static str| {
        let dav = &dav;
        async move {
            let (_, _, body) = dav
                .request("PROPFIND", path, &[("depth", "0")], Vec::new())
                .await;
            let xml = String::from_utf8(body).unwrap();
            xml.split("getlastmodified>")
                .nth(1)
                .unwrap()
                .split('<')
                .next()
                .unwrap()
                .to_owned()
        }
    };
    let at = |secs: i64| chrono::DateTime::from_timestamp(secs, 0).unwrap();

    // The client's time, accepted, shown everywhere.
    let (status, headers, _) = put(b"version one", Some("1700000000")).await;
    assert_eq!(status, StatusCode::CREATED);
    assert_eq!(headers["x-oc-mtime"], "accepted");
    assert_eq!(
        lastmodified("/notes.txt").await,
        "Tue, 14 Nov 2023 22:13:20 GMT"
    );
    let node = file().await;
    assert_eq!(node.modified, Some(at(1_700_000_000)));
    let info: api::NodeInfo = alice
        .get(&format!("/api/v1/nodes/{}", node.id))
        .await
        .unwrap();
    assert_eq!(info.modified, Some(at(1_700_000_000)));
    let first = versions(node.id).await;
    assert_eq!(first.len(), 1);
    assert_eq!(first[0].modified, at(1_700_000_000));

    // A touch: the same bytes, another time. No new version.
    put(b"version one", Some("1700000100")).await;
    let touched = versions(node.id).await;
    assert_eq!(touched.len(), 1);
    assert_eq!(touched[0].modified, at(1_700_000_100));

    // New content without a time: the upload's.
    let (status, headers, _) = put(b"version two", None).await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    assert!(headers.get("x-oc-mtime").is_none());
    let node = file().await;
    assert!(node.modified.unwrap() > at(1_700_000_100));

    // A restored version shows its own time.
    let restore = format!("/api/v1/files/{}/versions/{}/restore", node.id, first[0].id);
    let (status, _) = alice
        .call("POST", &restore, Some(b"{}".to_vec()), true)
        .await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    assert_eq!(file().await.modified, Some(at(1_700_000_100)));

    // A copy keeps it.
    let status = dav
        .status("COPY", "/notes.txt", &[("destination", "/dav/copy.txt")])
        .await;
    assert_eq!(status, StatusCode::CREATED);
    assert_eq!(
        lastmodified("/copy.txt").await,
        "Tue, 14 Nov 2023 22:15:00 GMT"
    );

    // A time that isn't one.
    let (status, _, _) = put(b"x", Some("soon")).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
}

#[sqlx::test]
async fn read_only_and_quota(pool: PgPool) {
    let w = fs_world(pool).await;
    let mut alice = w.as_user(1).await;
    let docs = alice.create_library("Docs").await;
    alice
        .upload(&docs, new_file(&docs, "a.txt"), &data(100, 1))
        .await
        .unwrap();
    let read_only = w
        .dav(1, json!({ "library": docs.id, "read_only": true }))
        .await;
    assert_eq!(read_only.get("/a.txt", None).await.0, StatusCode::OK);
    assert_eq!(read_only.put("/b.txt", b"x").await, StatusCode::FORBIDDEN);
    assert_eq!(
        read_only.status("DELETE", "/a.txt", &[]).await,
        StatusCode::FORBIDDEN
    );
    assert_eq!(
        read_only.status("MKCOL", "/new/", &[]).await,
        StatusCode::FORBIDDEN
    );

    w.sql("UPDATE organization SET quota = 2000 WHERE pk = 1")
        .await;
    let dav = w.dav(1, json!({ "library": docs.id })).await;
    assert_eq!(
        dav.put("/big.bin", &data(MIB, 2)).await,
        StatusCode::INSUFFICIENT_STORAGE
    );
}

#[sqlx::test]
async fn previews(pool: PgPool) {
    let w = fs_world(pool).await;
    let mut alice = w.as_user(1).await;
    let docs = alice.create_library("Docs").await;
    let page = alice
        .upload(
            &docs,
            new_file(&docs, "page.html"),
            b"<script>alert(1)</script>",
        )
        .await
        .unwrap();
    let movie = data(2 * MIB, 3);
    let video = alice
        .upload(&docs, new_file(&docs, "movie.mp4"), &movie)
        .await
        .unwrap();
    let fetch = |file: Uuid, range: Option<&'static str>| {
        let alice = &alice;
        async move {
            let mut request = Request::get(format!("/api/v1/files/{file}/content"))
                .header("cookie", &alice.cookies);
            if let Some(range) = range {
                request = request.header("range", range);
            }
            let response = alice
                .app
                .clone()
                .oneshot(request.body(Body::empty()).unwrap())
                .await
                .unwrap();
            let (parts, body) = response.into_parts();
            (
                parts.status,
                parts.headers,
                to_bytes(body, usize::MAX).await.unwrap().to_vec(),
            )
        }
    };

    let (status, headers, body) = fetch(page.file, None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        headers["content-type"], "text/plain; charset=utf-8",
        "never rendered as HTML"
    );
    assert_eq!(headers["content-security-policy"], "sandbox");
    assert_eq!(headers["x-content-type-options"], "nosniff");
    assert_eq!(body, b"<script>alert(1)</script>");

    let (status, headers, body) = fetch(video.file, Some("bytes=1000-1999")).await;
    assert_eq!(status, StatusCode::PARTIAL_CONTENT);
    assert_eq!(headers["content-type"], "video/mp4");
    assert_eq!(
        headers["content-range"],
        format!("bytes 1000-1999/{}", movie.len())
    );
    assert_eq!(body, &movie[1000..2000]);
    let (status, ..) = fetch(video.file, Some("bytes=99999999-")).await;
    assert_eq!(status, StatusCode::RANGE_NOT_SATISFIABLE);
}

/// litmus, the RFC 4918 conformance suite, against a real port. Every group
/// runs; only the tests in EXPECTED_FAILURES may fail (dead properties aren't
/// stored), and they must fail (the list can't go stale).
#[sqlx::test]
async fn litmus(pool: PgPool) {
    // Reading back dead properties (they're accepted, not stored).
    const EXPECTED_FAILURES: &[&str] = &["propget"];
    let w = fs_world(pool).await;
    let alice = w.as_user(1).await;
    let docs = alice.create_library("Docs").await;
    let created: api::NewAppPassword = alice
        .json(
            "POST",
            "/api/v1/auth/app-passwords",
            &json!({ "name": "litmus", "library": docs.id }),
        )
        .await
        .unwrap();
    let listener = tokio::net::TcpListener::bind("0.0.0.0:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let app = w.app.clone();
    tokio::spawn(async move {
        axum::serve(
            listener,
            app.into_make_service_with_connect_info::<std::net::SocketAddr>(),
        )
        .await
        .unwrap()
    });

    let output = run_litmus(port, &created.password).await;
    let mut failed: Vec<String> = Vec::new();
    for line in output.lines() {
        // " 7. propget......  7. propget...... FAIL (reason)": the test's name
        // follows its number, and the line ends with its result.
        let Some((_, after)) = line.trim_start().split_once(". ") else {
            continue;
        };
        if !line.trim_start().starts_with(|c: char| c.is_ascii_digit()) || !line.contains(" FAIL") {
            continue;
        }
        failed.push(
            after
                .split('.')
                .next()
                .unwrap_or_default()
                .trim()
                .to_owned(),
        );
    }
    failed.dedup();
    // The full report, for reading when something changes.
    let _ = std::fs::write(
        concat!(env!("CARGO_MANIFEST_DIR"), "/../../target/litmus.log"),
        &output,
    );
    let unexpected: Vec<&String> = failed
        .iter()
        .filter(|f| !EXPECTED_FAILURES.contains(&f.as_str()))
        .collect();
    let fixed: Vec<&&str> = EXPECTED_FAILURES
        .iter()
        .filter(|e| !failed.iter().any(|f| f == *e))
        .collect();
    assert!(
        unexpected.is_empty(),
        "unexpected litmus failures {unexpected:?}:\n{output}"
    );
    assert!(
        fixed.is_empty(),
        "now passing, remove from EXPECTED_FAILURES: {fixed:?}"
    );
    for group in ["basic", "copymove", "props", "locks", "http"] {
        assert!(
            output.contains(&format!("summary for `{group}'")),
            "litmus didn't run {group}:\n{output}"
        );
    }
}

/// litmus from the PATH (the CI image), or in Docker (built from
/// ci/litmus.Dockerfile on first use). On a blocking thread: the server it
/// tests runs on this runtime.
async fn run_litmus(port: u16, password: &str) -> String {
    let password = password.to_owned();
    tokio::task::spawn_blocking(move || {
        use std::process::Command;
        let local = Command::new("litmus").arg("--version").output().is_ok();
        let output = if local {
            // litmus writes debug.log into its working directory.
            Command::new("litmus")
                .current_dir(std::env::temp_dir())
                .args([
                    "--keep-going",
                    &format!("http://127.0.0.1:{port}/dav/"),
                    "me",
                    &password,
                ])
                .output()
                .unwrap()
        } else {
            let root = concat!(env!("CARGO_MANIFEST_DIR"), "/../..");
            let image = Command::new("docker")
                .args(["image", "inspect", "theca-litmus"])
                .output()
                .unwrap();
            if !image.status.success() {
                let built = Command::new("docker")
                    .args([
                        "build",
                        "-q",
                        "-t",
                        "theca-litmus",
                        "-f",
                        "ci/litmus.Dockerfile",
                        "ci",
                    ])
                    .current_dir(root)
                    .status()
                    .unwrap();
                assert!(built.success(), "building the litmus image");
            }
            Command::new("docker")
                .args([
                    "run",
                    "--rm",
                    "--add-host",
                    "host.docker.internal:host-gateway",
                    "theca-litmus",
                ])
                .args([
                    "--keep-going",
                    &format!("http://host.docker.internal:{port}/dav/"),
                    "me",
                    &password,
                ])
                .output()
                .unwrap()
        };
        String::from_utf8_lossy(&output.stdout).into_owned()
            + &String::from_utf8_lossy(&output.stderr)
    })
    .await
    .unwrap()
}
