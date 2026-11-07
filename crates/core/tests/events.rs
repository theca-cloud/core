//! The event feed (docs/decisions.md, Sync; sprint 08): every change writes
//! its event, numbered per library in commit order; `POST /events` streams
//! them collapsed and resolved, follows access, and ends when its credential
//! falls; a client applying the feed ends with the server's tree. Account 1
//! (a@x.ch) owns organization one, where account 2 (b@x.ch) is a member;
//! account 3 (c@x.ch) owns organization two.

#![allow(dead_code, unused_imports)] // the shared harness has more than these tests use

include!("common/harness.rs");

use std::collections::{BTreeMap, HashMap, HashSet};
use std::time::Duration;

use base64::Engine;
use futures::StreamExt;
use theca_core::events::Registry;

/// A message of the stream.
#[derive(Debug)]
enum Msg {
    Libraries(Vec<api::FeedLibrary>),
    Changes(api::Changes),
}

/// An open `POST /events` stream, read message by message.
struct Feed {
    body: axum::body::BodyDataStream,
    buf: String,
    /// `: ping` comments seen.
    pings: usize,
}

impl Feed {
    /// The next message, comments skipped; `None` if none comes within
    /// `wait` or the stream ended.
    async fn within(&mut self, wait: Duration) -> Option<Msg> {
        let deadline = tokio::time::Instant::now() + wait;
        loop {
            if let Some(end) = self.buf.find("\n\n") {
                let frame: String = self.buf.drain(..end + 2).collect();
                let (mut event, mut data) = (None, String::new());
                for line in frame.lines() {
                    if line.starts_with(':') {
                        self.pings += 1;
                    } else if let Some(e) = line.strip_prefix("event: ") {
                        event = Some(e.to_owned());
                    } else if let Some(d) = line.strip_prefix("data: ") {
                        data.push_str(d);
                    }
                }
                match event.as_deref() {
                    Some("libraries") => {
                        return Some(Msg::Libraries(serde_json::from_str(&data).unwrap()));
                    }
                    Some("changes") => {
                        return Some(Msg::Changes(serde_json::from_str(&data).unwrap()));
                    }
                    Some(other) => panic!("unknown event {other}"),
                    None => continue,
                }
            }
            match tokio::time::timeout_at(deadline, self.body.next()).await {
                Ok(Some(chunk)) => self
                    .buf
                    .push_str(std::str::from_utf8(&chunk.unwrap()).unwrap()),
                Ok(None) | Err(_) => return None,
            }
        }
    }

    async fn next(&mut self) -> Msg {
        self.within(Duration::from_secs(5))
            .await
            .expect("a message")
    }

    async fn libraries(&mut self) -> Vec<api::FeedLibrary> {
        match self.next().await {
            Msg::Libraries(l) => l,
            other => panic!("expected libraries, got {other:?}"),
        }
    }

    async fn changes(&mut self) -> api::Changes {
        match self.next().await {
            Msg::Changes(c) => c,
            other => panic!("expected changes, got {other:?}"),
        }
    }

    /// Nothing comes for a moment.
    async fn quiet(&mut self) -> bool {
        self.within(Duration::from_millis(300)).await.is_none()
    }

    /// The stream ends (within `wait`), whatever it sends before.
    async fn ends_within(&mut self, wait: Duration) -> bool {
        let deadline = tokio::time::Instant::now() + wait;
        loop {
            match tokio::time::timeout_at(deadline, self.body.next()).await {
                Ok(None) => return true,
                Ok(Some(_)) => {}
                Err(_) => return false,
            }
        }
    }
}

impl Client {
    /// Opens the event feed from `after` (numbers, or numbers and IDs).
    async fn feed(&self, after: &impl serde::Serialize) -> Result<Feed, (StatusCode, Value)> {
        let mut request =
            Request::post("/api/v1/events").header("content-type", "application/json");
        request = match &self.bearer {
            Some(password) => request.header("authorization", format!("Bearer {password}")),
            None => request.header("cookie", &self.cookies),
        };
        let body = serde_json::to_vec(&json!({ "after": after })).unwrap();
        let response = self
            .app
            .clone()
            .oneshot(request.body(Body::from(body)).unwrap())
            .await
            .unwrap();
        let status = response.status();
        if !status.is_success() {
            let bytes = to_bytes(response.into_body(), usize::MAX).await.unwrap();
            return Err((
                status,
                serde_json::from_slice(&bytes).unwrap_or(Value::Null),
            ));
        }
        assert_eq!(response.headers()["content-type"], "text/event-stream");
        assert_eq!(response.headers()["cache-control"], "no-cache");
        assert_eq!(response.headers()["x-accel-buffering"], "no");
        Ok(Feed {
            body: response.into_body().into_data_stream(),
            buf: String::new(),
            pings: 0,
        })
    }

    async fn feed_start(&self) -> Feed {
        self.feed(&BTreeMap::<Uuid, i64>::new()).await.unwrap()
    }

    async fn put_file(
        &mut self,
        library: &api::Library,
        parent: Uuid,
        name: &str,
        data: &[u8],
    ) -> Uuid {
        self.upload(library, json!({ "parent": parent, "name": name }), data)
            .await
            .unwrap()
            .file
    }

    async fn mkdir(&self, folder: Uuid, paths: &[&str]) -> Vec<api::FolderPath> {
        self.json(
            "POST",
            &format!("/api/v1/nodes/{folder}/folders"),
            &json!({ "paths": paths }),
        )
        .await
        .unwrap()
    }

    /// A folder's subtree, as `(id, parent, name, current version)`.
    async fn tree(&self, folder: Uuid) -> Vec<(Uuid, Uuid, String, Option<Uuid>)> {
        self.tree_if_any(folder).await.expect("a reachable folder")
    }

    /// `None` if the folder is out of reach (gone since a message said it
    /// existed: a later one says it's gone).
    async fn tree_if_any(&self, folder: Uuid) -> Option<Vec<(Uuid, Uuid, String, Option<Uuid>)>> {
        let (status, bytes) = self
            .call("GET", &format!("/api/v1/nodes/{folder}/tree"), None, true)
            .await;
        if status == StatusCode::NOT_FOUND {
            return None;
        }
        assert_eq!(status, StatusCode::OK, "tree of {folder}");
        let mut ids: HashMap<String, Uuid> = HashMap::new();
        let mut out = Vec::new();
        for line in bytes.split(|b| *b == b'\n').filter(|l| !l.is_empty()) {
            match serde_json::from_slice::<api::TreeLine>(line).unwrap() {
                api::TreeLine::Node(n) => {
                    let (parent, name) = match n.path.rsplit_once('/') {
                        Some((dir, name)) => (ids[dir], name.to_owned()),
                        None => (folder, n.path.clone()),
                    };
                    ids.insert(n.path.clone(), n.id);
                    out.push((n.id, parent, name, n.current_version));
                }
                api::TreeLine::End { .. } => return Some(out),
                api::TreeLine::Error(e) => panic!("{e:?}"),
            }
        }
        panic!("tree cut");
    }
}

/// A library's events: `(seq, resource, kind, action, actor)`.
async fn events(pool: &PgPool, library: Uuid) -> Vec<(i64, Uuid, String, String, Option<i64>)> {
    sqlx::query_as(
        "SELECT e.seq, e.resource, e.resource_kind, e.action, e.actor
         FROM event e JOIN library l ON l.pk = e.library WHERE l.uuid = $1 ORDER BY e.seq",
    )
    .bind(library)
    .fetch_all(pool)
    .await
    .unwrap()
}

/// The events after `seq`, as `(resource, action)`.
async fn since(pool: &PgPool, library: Uuid, seq: i64) -> Vec<(Uuid, String)> {
    events(pool, library)
        .await
        .into_iter()
        .filter(|e| e.0 > seq)
        .map(|e| (e.1, e.3))
        .collect()
}

async fn last_seq(pool: &PgPool, library: Uuid) -> i64 {
    events(pool, library).await.last().map_or(0, |e| e.0)
}

fn act(id: Uuid, action: &str) -> (Uuid, String) {
    (id, action.to_owned())
}

/// A router whose streams ping and whose revocation check runs every 200 ms,
/// and its state.
async fn quick(pool: &PgPool) -> (Router, AppState) {
    let mut state = state(pool).await;
    state.events = Registry::new(Duration::from_millis(200));
    (router(state.clone()), state)
}

async fn dav(client: &Client, scope: Value) -> String {
    let mut scope = scope;
    scope["name"] = json!("dav");
    let created: api::NewAppPassword = client
        .json("POST", "/api/v1/auth/app-passwords", &scope)
        .await
        .unwrap();
    let basic =
        base64::engine::general_purpose::STANDARD.encode(format!("me:{}", created.password));
    format!("Basic {basic}")
}

async fn dav_call(
    app: &Router,
    auth: &str,
    method: &str,
    path: &str,
    headers: &[(&str, &str)],
    body: &[u8],
) -> StatusCode {
    let mut request = Request::builder()
        .method(method)
        .uri(format!("/dav{path}"))
        .header("authorization", auth);
    for (k, v) in headers {
        request = request.header(*k, *v);
    }
    app.clone()
        .oneshot(request.body(Body::from(body.to_vec())).unwrap())
        .await
        .unwrap()
        .status()
}

async fn node_id(pool: &PgPool, library: Uuid, name: &str) -> Uuid {
    sqlx::query_scalar(
        "SELECT n.uuid FROM node n JOIN library l ON l.pk = n.library WHERE l.uuid = $1 AND n.name = $2",
    )
    .bind(library)
    .bind(name)
    .fetch_one(pool)
    .await
    .unwrap()
}

// Writing: each change, its event.

#[sqlx::test]
async fn each_change_writes_its_event(pool: PgPool) {
    let w = world(Kind::Filesystem, pool).await;
    let mut alice = w.as_user(1).await;
    let docs = alice.create_library("Docs").await;
    let photos = alice.create_library("Photos").await;
    // Creating a library writes nothing: it enters `libraries`.
    assert!(events(&w.pool, docs.id).await.is_empty());

    // Commits: created, versioned, touched only when the time changes.
    let a = alice.put_file(&docs, docs.root, "a.txt", b"one").await;
    let mut seq = last_seq(&w.pool, docs.id).await;
    assert_eq!(
        events(&w.pool, docs.id).await,
        [(1, a, "node".into(), "created".into(), Some(1))]
    );
    let v1 = alice.manifest(a, None).await.unwrap().version;
    let same = |modified: &str| json!({ "file": a, "base_version": v1, "modified": modified });
    alice
        .upload(&docs, same("2026-01-01T00:00:00Z"), b"one")
        .await
        .unwrap();
    assert_eq!(since(&w.pool, docs.id, seq).await, [act(a, "touched")]);
    seq += 1;
    alice
        .upload(&docs, same("2026-01-01T00:00:00Z"), b"one")
        .await
        .unwrap();
    assert_eq!(
        since(&w.pool, docs.id, seq).await,
        [],
        "same chunks, same time: nothing"
    );
    alice
        .upload(&docs, json!({ "file": a, "base_version": v1 }), b"two")
        .await
        .unwrap();
    assert_eq!(since(&w.pool, docs.id, seq).await, [act(a, "versioned")]);
    seq += 1;
    // A version restored.
    let restore = format!("/api/v1/files/{a}/versions/{v1}/restore");
    assert!(
        alice
            .call("POST", &restore, None, true)
            .await
            .0
            .is_success()
    );
    assert_eq!(
        since(&w.pool, docs.id, seq).await,
        [act(a, "version_restored")]
    );
    seq += 1;

    // mkdir -p: one event per folder it created, none for those existing.
    let made = alice.mkdir(docs.root, &["x/y", "x/z"]).await;
    let z = made[1].id;
    let x = node_id(&w.pool, docs.id, "x").await;
    let y = node_id(&w.pool, docs.id, "y").await;
    assert_eq!(
        since(&w.pool, docs.id, seq).await,
        [act(x, "created"), act(y, "created"), act(z, "created")]
    );
    seq += 3;
    alice.mkdir(docs.root, &["x/y"]).await;
    assert_eq!(since(&w.pool, docs.id, seq).await, []);

    // Renamed (same folder), moved (another folder, renamed or not).
    alice
        .patch_node(a, json!({ "name": "b.txt" }))
        .await
        .unwrap();
    alice.patch_node(a, json!({ "parent": x })).await.unwrap();
    alice
        .patch_node(a, json!({ "parent": y, "name": "c.txt" }))
        .await
        .unwrap();
    alice
        .patch_node(a, json!({ "parent": y, "name": "d.txt" }))
        .await
        .unwrap();
    assert_eq!(
        since(&w.pool, docs.id, seq).await,
        [
            act(a, "renamed"),
            act(a, "moved"),
            act(a, "moved"),
            act(a, "renamed")
        ]
    );
    seq += 4;

    // To another library: moved in both.
    alice
        .patch_node(z, json!({ "parent": photos.root }))
        .await
        .unwrap();
    assert_eq!(since(&w.pool, docs.id, seq).await, [act(z, "moved")]);
    seq += 1;
    assert_eq!(since(&w.pool, photos.id, 0).await, [act(z, "moved")]);

    // Copies: created; onto a file, versioned.
    let copy = alice
        .copy_node(a, json!({ "parent": docs.root }))
        .await
        .unwrap()
        .id;
    assert_eq!(since(&w.pool, docs.id, seq).await, [act(copy, "created")]);
    seq += 1;
    let other = alice
        .put_file(&docs, docs.root, "other.txt", b"other")
        .await;
    seq += 1;
    alice
        .copy_node(
            other,
            json!({ "parent": docs.root, "name": "d.txt", "replace": true }),
        )
        .await
        .unwrap();
    assert_eq!(since(&w.pool, docs.id, seq).await, [act(copy, "versioned")]);
    seq += 1;
    // A move onto a file (graft): versioned there, the source purged.
    alice
        .patch_node(other, json!({ "name": "d.txt", "replace": true }))
        .await
        .unwrap();
    assert_eq!(
        since(&w.pool, docs.id, seq).await,
        [act(copy, "versioned"), act(other, "purged")]
    );
    seq += 2;

    // The trash: one event for a folder, whatever its subtree.
    assert_eq!(alice.trash_node(x, false).await, StatusCode::NO_CONTENT);
    alice.restore_node(x).await.unwrap();
    assert_eq!(alice.trash_node(x, false).await, StatusCode::NO_CONTENT);
    assert_eq!(alice.trash_node(x, true).await, StatusCode::NO_CONTENT);
    assert_eq!(
        since(&w.pool, docs.id, seq).await,
        [
            act(x, "trashed"),
            act(x, "restored"),
            act(x, "trashed"),
            act(x, "purged")
        ]
    );
    seq += 4;

    // A refused change writes nothing, and leaves no gap.
    let taken = alice
        .upload(&docs, json!({ "parent": docs.root, "name": "d.txt" }), b"x")
        .await
        .unwrap_err();
    assert_eq!(taken.0, StatusCode::CONFLICT);
    assert_eq!(since(&w.pool, docs.id, seq).await, []);
    let e = alice.put_file(&docs, docs.root, "e.txt", b"e").await;
    assert_eq!(since(&w.pool, docs.id, seq).await, [act(e, "created")]);
    seq += 1;

    // Libraries: renamed, trashed, restored.
    let lib = format!("/api/v1/libraries/{}", docs.id);
    alice
        .json::<Value>("PATCH", &lib, &json!({ "name": "Docs 2" }))
        .await
        .unwrap();
    assert!(alice.call("DELETE", &lib, None, true).await.0.is_success());
    assert!(
        alice
            .call("POST", &format!("{lib}/restore"), None, true)
            .await
            .0
            .is_success()
    );
    assert_eq!(
        since(&w.pool, docs.id, seq).await,
        [
            act(docs.id, "renamed"),
            act(docs.id, "trashed"),
            act(docs.id, "restored")
        ]
    );
    let kinds: Vec<String> = events(&w.pool, docs.id)
        .await
        .into_iter()
        .rev()
        .take(3)
        .map(|e| e.2)
        .collect();
    assert_eq!(kinds, ["library"; 3]);

    // Numbered from 1, without gaps.
    let seqs: Vec<i64> = events(&w.pool, docs.id).await.iter().map(|e| e.0).collect();
    assert_eq!(seqs, (1..=seqs.len() as i64).collect::<Vec<_>>());
    let counter: i64 = sqlx::query_scalar("SELECT event_seq FROM library WHERE uuid = $1")
        .bind(docs.id)
        .fetch_one(&w.pool)
        .await
        .unwrap();
    assert_eq!(counter, seqs.len() as i64);

    // A purged library takes its events.
    assert!(alice.call("DELETE", &lib, None, true).await.0.is_success());
    assert!(
        alice
            .call("DELETE", &format!("{lib}?permanent=true"), None, true)
            .await
            .0
            .is_success()
    );
    let left: i64 = sqlx::query_scalar("SELECT count(*) FROM event e WHERE NOT EXISTS (SELECT 1 FROM library l WHERE l.pk = e.library)")
        .fetch_one(&w.pool)
        .await
        .unwrap();
    assert_eq!(left, 0);
    assert_eq!(since(&w.pool, photos.id, 0).await.len(), 1);
}

#[sqlx::test]
async fn webdav_writes_events(pool: PgPool) {
    let w = world(Kind::Filesystem, pool).await;
    let alice = w.as_user(1).await;
    let docs = alice.create_library("Docs").await;
    let auth = dav(&alice, json!({ "library": docs.id })).await;
    let call = |method: &'static str,
                path: &'static str,
                headers: &'static [(&'static str, &'static str)],
                body: &'static [u8]| {
        let (app, auth) = (w.app.clone(), auth.clone());
        async move { dav_call(&app, &auth, method, path, headers, body).await }
    };

    assert!(call("PUT", "/a.txt", &[], b"one").await.is_success());
    let a = node_id(&w.pool, docs.id, "a.txt").await;
    assert!(call("PUT", "/a.txt", &[], b"two").await.is_success());
    assert!(call("MKCOL", "/f/", &[], b"").await.is_success());
    let f = node_id(&w.pool, docs.id, "f").await;
    assert!(
        call("MOVE", "/a.txt", &[("destination", "/dav/f/a.txt")], b"")
            .await
            .is_success()
    );
    assert!(
        call("MOVE", "/f/a.txt", &[("destination", "/dav/f/b.txt")], b"")
            .await
            .is_success()
    );
    assert!(
        call("COPY", "/f/b.txt", &[("destination", "/dav/c.txt")], b"")
            .await
            .is_success()
    );
    let c = node_id(&w.pool, docs.id, "c.txt").await;
    assert_eq!(
        since(&w.pool, docs.id, 0).await,
        [
            act(a, "created"),
            act(a, "versioned"),
            act(f, "created"),
            act(a, "moved"),
            act(a, "renamed"),
            act(c, "created"),
        ]
    );
    // A folder's DELETE: dav-server removes the children first, then the
    // folder; the database keeps one trash entry (the folder's), and the
    // feed reports them all gone.
    let seq = last_seq(&w.pool, docs.id).await;
    assert!(call("DELETE", "/f/", &[], b"").await.is_success());
    assert_eq!(
        since(&w.pool, docs.id, seq).await,
        [act(a, "trashed"), act(f, "trashed")]
    );
    let trashed: Vec<String> =
        sqlx::query_scalar("SELECT name FROM node WHERE deleted_at IS NOT NULL")
            .fetch_all(&w.pool)
            .await
            .unwrap();
    assert_eq!(trashed, ["f"]);
    // An ignored file writes nothing.
    let seq = last_seq(&w.pool, docs.id).await;
    assert!(call("PUT", "/.DS_Store", &[], b"os").await.is_success());
    assert_eq!(since(&w.pool, docs.id, seq).await, []);
}

#[sqlx::test]
async fn the_retention_purges_without_an_actor(pool: PgPool) {
    let w = world(Kind::Filesystem, pool).await;
    let mut alice = w.as_user(1).await;
    let docs = alice.create_library("Docs").await;
    let a = alice.put_file(&docs, docs.root, "a.txt", b"a").await;
    assert_eq!(alice.trash_node(a, false).await, StatusCode::NO_CONTENT);
    w.sql("UPDATE node SET deleted_at = now() - interval '400 days' WHERE deleted_at IS NOT NULL")
        .await;
    let state = state(&w.pool).await;
    theca_core::jobs::Job::TrashPurge.run(&state).await.unwrap();
    let last = events(&w.pool, docs.id).await.pop().unwrap();
    assert_eq!((last.1, last.3.as_str(), last.4), (a, "purged", None));

    // A library purged by the job leaves its members' `libraries` at once.
    let bob = w.as_user_on(router(state.clone()), 2).await;
    w.add_member(docs.id, 2, "read").await;
    let mut feed = bob.feed_start().await;
    assert!(feed.libraries().await.iter().any(|l| l.id == docs.id));
    let lib = format!("/api/v1/libraries/{}", docs.id);
    assert!(alice.call("DELETE", &lib, None, true).await.0.is_success());
    w.sql(
        "UPDATE library SET deleted_at = now() - interval '400 days' WHERE deleted_at IS NOT NULL",
    )
    .await;
    theca_core::jobs::Job::TrashPurge.run(&state).await.unwrap();
    assert!(!feed.libraries().await.iter().any(|l| l.id == docs.id));
}

#[sqlx::test]
async fn joining_reaches_the_memberships_prepared(pool: PgPool) {
    let w = world(Kind::Filesystem, pool).await;
    let alice = w.as_user(1).await;
    let carol = w.as_user(3).await;
    let docs = alice.create_library("Docs").await;
    // Carol invited to organization one earlier, Docs prepared for her.
    w.sql(
        r#"INSERT INTO "user" (account, organization, invited_at) VALUES (3, 1, now());
           INSERT INTO library_member (organization, library, "user", role)
           SELECT 1, l.pk, u.pk, 'read' FROM library l, "user" u
           WHERE l.name = 'Docs' AND u.account = 3 AND u.organization = 1"#,
    )
    .await;
    let mut feed = carol.feed_start().await;
    assert!(!feed.libraries().await.iter().any(|l| l.id == docs.id));
    // Added again, now that she can log in: she joins at once.
    let members = format!("/api/v1/organizations/{}/members", w.organizations[0]);
    let member: api::Member = alice
        .json("POST", &members, &json!({ "email": "c@x.ch" }))
        .await
        .unwrap();
    assert!(member.joined);
    assert!(feed.libraries().await.iter().any(|l| l.id == docs.id));
}

// Order.

#[sqlx::test]
async fn commits_into_a_library_number_in_commit_order(pool: PgPool) {
    let w = world(Kind::Filesystem, pool).await;
    let mut alice = w.as_user(1).await;
    let docs = alice.create_library("Docs").await;
    let photos = alice.create_library("Photos").await;
    let a = alice.put_file(&docs, docs.root, "a.txt", b"a").await;
    let mut feed = alice.feed_start().await;
    feed.libraries().await;

    // A transaction numbering its events, as actions::commit does, and held
    // open: a second commit into the library waits for it.
    let mut held = w.pool.begin().await.unwrap();
    let seq: i64 = sqlx::query_scalar(
        "UPDATE library SET event_seq = event_seq + 1 WHERE uuid = $1 RETURNING event_seq",
    )
    .bind(docs.id)
    .fetch_one(&mut *held)
    .await
    .unwrap();
    assert_eq!(seq, 2);
    let second = {
        let alice = w.as_user(1).await;
        tokio::spawn(async move {
            alice
                .patch_node(a, json!({ "name": "b.txt" }))
                .await
                .unwrap()
        })
    };
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert!(
        !second.is_finished(),
        "the second commit waits for the first"
    );
    // Another library doesn't wait.
    alice.put_file(&photos, photos.root, "p.txt", b"p").await;
    let changes = feed.changes().await;
    assert_eq!(changes.positions.keys().collect::<Vec<_>>(), [&photos.id]);
    assert!(
        feed.quiet().await,
        "nothing of Docs before the first commits"
    );

    sqlx::query(
        "INSERT INTO event (library, seq, resource, resource_kind, action)
         SELECT pk, 2, $2, 'node', 'touched' FROM library WHERE uuid = $1",
    )
    .bind(docs.id)
    .bind(a)
    .execute(&mut *held)
    .await
    .unwrap();
    held.commit().await.unwrap();
    second.await.unwrap();
    assert_eq!(
        since(&w.pool, docs.id, 1).await,
        [act(a, "touched"), act(a, "renamed")]
    );
    // The stream reads both, Docs at 3.
    let mut position = 0;
    while position < 3 {
        position = feed.changes().await.positions[&docs.id];
    }
}

#[sqlx::test]
async fn crossing_moves_both_go_through(pool: PgPool) {
    let w = world(Kind::Filesystem, pool).await;
    let mut alice = w.as_user(1).await;
    let docs = alice.create_library("Docs").await;
    let photos = alice.create_library("Photos").await;
    let (one, two) = (w.as_user(1).await, w.as_user(1).await);
    for round in 0..20 {
        let a = alice
            .put_file(&docs, docs.root, &format!("a{round}"), b"a")
            .await;
        let p = alice
            .put_file(&photos, photos.root, &format!("p{round}"), b"p")
            .await;
        let (x, y) = tokio::join!(
            one.patch_node(a, json!({ "parent": photos.root })),
            two.patch_node(p, json!({ "parent": docs.root })),
        );
        x.unwrap();
        y.unwrap();
    }
    for library in [docs.id, photos.id] {
        let seqs: Vec<i64> = events(&w.pool, library).await.iter().map(|e| e.0).collect();
        assert_eq!(seqs, (1..=seqs.len() as i64).collect::<Vec<_>>());
    }
}

// Reading.

#[sqlx::test]
async fn a_batch_is_collapsed_and_ordered(pool: PgPool) {
    let w = world(Kind::Filesystem, pool).await;
    let mut alice = w.as_user(1).await;
    let docs = alice.create_library("Docs").await;
    let mut feed = alice.feed_start().await;
    let libraries = feed.libraries().await;
    let after = BTreeMap::from([(docs.id, libraries[0].position)]);
    assert_eq!(after[&docs.id], 0);

    // Created, renamed twice, moved: one entry, the final state.
    let a = alice.put_file(&docs, docs.root, "a.txt", b"a").await;
    let made = alice.mkdir(docs.root, &["f/g"]).await;
    let g = made[0].id;
    let f = node_id(&w.pool, docs.id, "f").await;
    alice
        .patch_node(a, json!({ "name": "b.txt" }))
        .await
        .unwrap();
    alice
        .patch_node(a, json!({ "name": "c.txt" }))
        .await
        .unwrap();
    alice.patch_node(a, json!({ "parent": g })).await.unwrap();
    // Created then purged: gone.
    let t = alice.put_file(&docs, docs.root, "t.txt", b"t").await;
    alice.trash_node(t, true).await;
    alice.trash_node(t, false).await;
    assert_eq!(alice.trash_node(t, true).await, StatusCode::NO_CONTENT);
    // Two folders swapped: g out of f, then f into g.
    alice
        .patch_node(g, json!({ "parent": docs.root }))
        .await
        .unwrap();
    alice.patch_node(f, json!({ "parent": g })).await.unwrap();

    // Read from the start in one batch (a new stream).
    let mut feed = alice.feed(&after).await.unwrap();
    feed.libraries().await;
    let changes = feed.changes().await;
    let ids: Vec<Uuid> = changes.entries.iter().map(|c| c.id).collect();
    assert_eq!(
        ids,
        [g, a, f, t],
        "parents first (g now holds a and f), then gone"
    );
    let entry = |id: Uuid| changes.entries.iter().find(|c| c.id == id).unwrap();
    let state = entry(a).state.as_ref().unwrap();
    assert_eq!(
        (state.name.as_str(), state.parent, state.is_file),
        ("c.txt", Some(g), true)
    );
    assert_eq!(entry(a).action, api::EventAction::Moved);
    assert_eq!(entry(f).state.as_ref().unwrap().parent, Some(g));
    assert_eq!(entry(g).state.as_ref().unwrap().parent, Some(docs.root));
    assert!(entry(t).state.is_none());
    assert_eq!(entry(t).action, api::EventAction::Purged);
    assert_eq!(
        changes.positions[&docs.id],
        last_seq(&w.pool, docs.id).await
    );
    assert!(feed.quiet().await);

    // A trashed folder: gone; restored: back with its state.
    alice.trash_node(g, false).await;
    let c = feed.changes().await;
    assert_eq!((c.entries[0].id, c.entries[0].state.is_none()), (g, true));
    alice.restore_node(g).await.unwrap();
    let c = feed.changes().await;
    assert_eq!(c.entries[0].action, api::EventAction::Restored);
    assert!(c.entries[0].state.is_some());
}

#[sqlx::test]
async fn a_move_between_libraries_for_each_viewer(pool: PgPool) {
    let w = world(Kind::Filesystem, pool).await;
    let mut alice = w.as_user(1).await;
    let photos = alice.create_library("Photos").await;
    let docs = alice.create_library("Docs").await;
    let a = alice.put_file(&photos, photos.root, "a.jpg", b"a").await;
    // Both libraries; the destination only; the source only.
    let both = alice.with_app_password(json!({})).await;
    let dest = alice.with_app_password(json!({ "library": docs.id })).await;
    let source = alice
        .with_app_password(json!({ "library": photos.id }))
        .await;
    let mut feeds = Vec::new();
    for client in [&both, &dest, &source] {
        let mut feed = client.feed_start().await;
        feed.libraries().await;
        feeds.push(feed);
    }
    alice
        .patch_node(a, json!({ "parent": docs.root }))
        .await
        .unwrap();
    let mut seen = Vec::new();
    for feed in &mut feeds {
        let changes = feed.changes().await;
        assert_eq!(changes.entries.len(), 1);
        seen.push(
            changes.entries[0]
                .state
                .as_ref()
                .map(|s| (s.library, s.parent)),
        );
    }
    assert_eq!(
        seen,
        [
            Some((docs.id, Some(docs.root))),
            Some((docs.id, Some(docs.root))),
            None
        ]
    );
}

#[sqlx::test]
async fn batches_of_a_thousand(pool: PgPool) {
    let w = world(Kind::Filesystem, pool).await;
    let alice = w.as_user(1).await;
    let docs = alice.create_library("Docs").await;
    for (start, count) in [(0, 1000), (1000, 1000), (2000, 500)] {
        let paths: Vec<String> = (start..start + count).map(|i| format!("f{i}")).collect();
        let paths: Vec<&str> = paths.iter().map(String::as_str).collect();
        alice.mkdir(docs.root, &paths).await;
    }
    let mut feed = alice.feed(&BTreeMap::from([(docs.id, 0)])).await.unwrap();
    feed.libraries().await;
    let mut positions = Vec::new();
    for _ in 0..3 {
        let changes = feed.changes().await;
        positions.push((changes.entries.len(), changes.positions[&docs.id]));
    }
    assert_eq!(positions, [(1000, 1000), (1000, 2000), (500, 2500)]);
    assert!(feed.quiet().await);
}

// Access.

#[sqlx::test]
async fn access_follows_the_stream(pool: PgPool) {
    let w = world(Kind::Filesystem, pool).await;
    let mut alice = w.as_user(1).await;
    let bob = w.as_user(2).await;
    let carol = w.as_user(3).await;
    let docs = alice.create_library("Docs").await;
    let mut feed = bob.feed_start().await;
    let ids = |l: Vec<api::FeedLibrary>| -> Vec<Uuid> { l.into_iter().map(|l| l.id).collect() };
    let personal = ids(feed.libraries().await);

    // Added: woken, Docs listed with its position, then followed from there.
    alice.put_file(&docs, docs.root, "before.txt", b"x").await;
    let members = format!("/api/v1/libraries/{}/members/{}", docs.id, w.users[1]);
    alice
        .json::<Value>("PUT", &members, &json!({ "role": "read" }))
        .await
        .unwrap();
    let listed = feed.libraries().await;
    let entry = listed.iter().find(|l| l.id == docs.id).unwrap();
    assert_eq!(
        (entry.position, entry.writable, entry.reset),
        (1, false, false)
    );
    let after = alice.put_file(&docs, docs.root, "after.txt", b"y").await;
    let changes = feed.changes().await;
    assert_eq!(
        changes.entries.iter().map(|c| c.id).collect::<Vec<_>>(),
        [after]
    );

    // A role change: `writable` changes.
    alice
        .json::<Value>("PUT", &members, &json!({ "role": "write" }))
        .await
        .unwrap();
    let listed = feed.libraries().await;
    assert!(listed.iter().find(|l| l.id == docs.id).unwrap().writable);
    // Removed: dropped from `libraries`, its events no longer read.
    assert!(
        alice
            .call("DELETE", &members, None, true)
            .await
            .0
            .is_success()
    );
    let listed = ids(feed.libraries().await);
    assert!(!listed.contains(&docs.id));
    alice.put_file(&docs, docs.root, "unseen.txt", b"z").await;
    assert!(feed.quiet().await);

    // A library created: in its creator's `libraries`; purged: out of its
    // members'.
    let own = bob.create_library("Bob's").await;
    assert!(ids(feed.libraries().await).contains(&own.id));
    let lib = format!("/api/v1/libraries/{}", own.id);
    assert!(bob.call("DELETE", &lib, None, true).await.0.is_success());
    let changes = feed.changes().await;
    let state = changes.entries[0].state.as_ref().unwrap();
    assert_eq!(
        (changes.entries[0].kind, state.trashed),
        (api::ResourceKind::Library, true)
    );
    assert!(
        bob.call("DELETE", &format!("{lib}?permanent=true"), None, true)
            .await
            .0
            .is_success()
    );
    assert!(!ids(feed.libraries().await).contains(&own.id));

    // Nothing from another organization.
    let mut carol = carol;
    let theirs = carol.create_library("Theirs").await;
    carol.put_file(&theirs, theirs.root, "c.txt", b"c").await;
    assert!(feed.quiet().await);

    // Removed from the organization: its libraries leave.
    alice
        .json::<Value>("PUT", &members, &json!({ "role": "read" }))
        .await
        .unwrap();
    assert!(ids(feed.libraries().await).contains(&docs.id));
    let member = format!(
        "/api/v1/organizations/{}/members/{}",
        w.organizations[0], w.users[1]
    );
    assert!(
        alice
            .call("DELETE", &member, None, true)
            .await
            .0
            .is_success()
    );
    assert_eq!(ids(feed.libraries().await), personal);
}

#[sqlx::test]
async fn a_deleted_organization_leaves(pool: PgPool) {
    let w = world(Kind::Filesystem, pool).await;
    let alice = w.as_user(1).await;
    let bob = w.as_user(2).await;
    let docs = alice.create_library("Docs").await;
    w.add_member(docs.id, 2, "read").await;
    let mut feed = bob.feed_start().await;
    assert!(feed.libraries().await.iter().any(|l| l.id == docs.id));
    let org = format!("/api/v1/organizations/{}", w.organizations[0]);
    let (status, body) = alice.call("DELETE", &org, None, true).await;
    assert!(
        status.is_success(),
        "{status} {}",
        String::from_utf8_lossy(&body)
    );
    assert!(!feed.libraries().await.iter().any(|l| l.id == docs.id));
}

#[sqlx::test]
async fn a_suspended_organization_leaves_and_comes_back(pool: PgPool) {
    let w = world(Kind::Filesystem, pool).await;
    let alice = w.as_user(1).await;
    let bob = w.as_user(2).await;
    let docs = alice.create_library("Docs").await;
    w.add_member(docs.id, 2, "read").await;
    w.sql("UPDATE account SET instance_admin = true WHERE pk = 3")
        .await;
    let admin = w.as_user(3).await;
    let mut feed = bob.feed_start().await;
    assert!(feed.libraries().await.iter().any(|l| l.id == docs.id));
    let org = format!("/api/v1/admin/organizations/{}", w.organizations[0]);
    for (suspended, listed) in [(true, false), (false, true)] {
        admin
            .json::<Value>("PATCH", &org, &json!({ "suspended": suspended }))
            .await
            .unwrap();
        assert_eq!(
            feed.libraries().await.iter().any(|l| l.id == docs.id),
            listed
        );
    }
}

#[sqlx::test]
async fn a_scoped_app_password_sees_its_library(pool: PgPool) {
    let w = world(Kind::Filesystem, pool).await;
    let mut alice = w.as_user(1).await;
    let docs = alice.create_library("Docs").await;
    let photos = alice.create_library("Photos").await;
    let scoped = alice
        .with_app_password(json!({ "library": docs.id, "read_only": true }))
        .await;
    let mut feed = scoped.feed_start().await;
    let listed = feed.libraries().await;
    assert_eq!(listed.iter().map(|l| l.id).collect::<Vec<_>>(), [docs.id]);
    assert!(!listed[0].writable);
    alice.put_file(&photos, photos.root, "p.txt", b"p").await;
    assert!(feed.quiet().await);
    let d = alice.put_file(&docs, docs.root, "d.txt", b"d").await;
    assert_eq!(feed.changes().await.entries[0].id, d);
}

// Protocol.

#[sqlx::test]
async fn positions_and_resets(pool: PgPool) {
    let w = world(Kind::Filesystem, pool).await;
    let mut alice = w.as_user(1).await;
    let docs = alice.create_library("Docs").await;
    let photos = alice.create_library("Photos").await;
    let a = alice.put_file(&docs, docs.root, "a.txt", b"a").await;
    let b = alice.put_file(&docs, docs.root, "b.txt", b"b").await;
    let p = alice.put_file(&photos, photos.root, "p.txt", b"p").await;

    // An empty map: every library announced with its position, nothing replayed.
    let mut feed = alice.feed_start().await;
    let listed = feed.libraries().await;
    let position = |id: Uuid| listed.iter().find(|l| l.id == id).unwrap().position;
    assert_eq!((position(docs.id), position(photos.id)), (2, 1));
    assert!(feed.quiet().await);

    // From a position: what follows; beyond the library's last event (a
    // restored backup): that library reset, the others untouched; a library
    // out of reach: ignored.
    let after = BTreeMap::from([(docs.id, 1), (photos.id, 7), (Uuid::now_v7(), 3)]);
    let mut feed = alice.feed(&after).await.unwrap();
    let listed = feed.libraries().await;
    let reset: Vec<(Uuid, bool)> = listed.iter().map(|l| (l.id, l.reset)).collect();
    assert!(reset.contains(&(docs.id, false)) && reset.contains(&(photos.id, true)));
    let changes = feed.changes().await;
    assert_eq!(
        changes.entries.iter().map(|c| c.id).collect::<Vec<_>>(),
        [b]
    );
    assert_eq!(changes.positions, BTreeMap::from([(docs.id, 2)]));
    let _ = (a, p);

    // Cut, then resumed from the last positions: nothing lost.
    drop(feed);
    let c = alice.put_file(&docs, docs.root, "c.txt", b"c").await;
    let mut feed = alice
        .feed(&BTreeMap::from([(docs.id, 2), (photos.id, 1)]))
        .await
        .unwrap();
    feed.libraries().await;
    assert_eq!(feed.changes().await.entries[0].id, c);

    // Another organization's library is not followed.
    let mut carol = w.as_user(3).await;
    let theirs = carol.create_library("Theirs").await;
    carol.put_file(&theirs, theirs.root, "t.txt", b"t").await;
    let mut feed = alice.feed(&BTreeMap::from([(theirs.id, 0)])).await.unwrap();
    assert!(!feed.libraries().await.iter().any(|l| l.id == theirs.id));
    assert!(feed.quiet().await);
}

/// A database restored from a backup, then given new events past a client's
/// position: the numbers came back, the events didn't. The client's position
/// names its event, which that number no longer is: reset. A number alone (a
/// client before core 1.4.0) can't tell.
#[sqlx::test]
async fn a_restore_past_new_events_is_a_reset(pool: PgPool) {
    let w = world(Kind::Filesystem, pool).await;
    let mut alice = w.as_user(1).await;
    let docs = alice.create_library("Docs").await;
    for name in ["a.txt", "b.txt", "c.txt"] {
        alice.put_file(&docs, docs.root, name, b"x").await;
    }
    let mut feed = alice.feed_start().await;
    let listed = feed.libraries().await;
    let at = listed.iter().find(|l| l.id == docs.id).unwrap();
    let (seq, event) = (at.position, at.event.expect("an event at 3"));
    assert_eq!(seq, 3);
    drop(feed);

    // The backup was taken after the first event; three new ones since.
    sqlx::query(
        "DELETE FROM event e USING library l WHERE e.library = l.pk AND l.uuid = $1 AND e.seq > 1",
    )
    .bind(docs.id)
    .execute(&w.pool)
    .await
    .unwrap();
    sqlx::query("UPDATE library SET event_seq = 1 WHERE uuid = $1")
        .bind(docs.id)
        .execute(&w.pool)
        .await
        .unwrap();
    for name in ["d.txt", "e.txt", "f.txt"] {
        alice.put_file(&docs, docs.root, name, b"y").await;
    }

    let named = json!({ docs.id.to_string(): { "seq": seq, "event": event } });
    let mut feed = alice.feed(&named).await.unwrap();
    let now = feed.libraries().await;
    let now = now.iter().find(|l| l.id == docs.id).unwrap();
    assert!(now.reset, "the restore is told");
    assert_eq!(now.position, 4);
    assert!(feed.quiet().await, "followed from the last event");
    let last = now.event.unwrap();

    // A number alone: today's rule, which can't tell.
    let mut feed = alice.feed(&BTreeMap::from([(docs.id, seq)])).await.unwrap();
    assert!(!feed.libraries().await[0].reset);
    drop(feed);

    // The same event: followed, and a batch names its new position's event.
    let same = json!({ docs.id.to_string(): { "seq": 4, "event": last } });
    let mut feed = alice.feed(&same).await.unwrap();
    assert!(!feed.libraries().await[0].reset);
    alice.put_file(&docs, docs.root, "g.txt", b"z").await;
    let changes = feed.changes().await;
    assert_eq!(changes.positions[&docs.id], 5);
    let fifth = changes.events[&docs.id];
    drop(feed);
    let resumed = json!({ docs.id.to_string(): { "seq": 5, "event": fifth } });
    let mut feed = alice.feed(&resumed).await.unwrap();
    assert!(!feed.libraries().await[0].reset);
    assert!(feed.quiet().await);
}

#[sqlx::test]
async fn pings(pool: PgPool) {
    let w = world(Kind::Filesystem, pool).await;
    let (app, _) = quick(&w.pool).await;
    let alice = w.as_user_on(app, 1).await;
    let mut feed = alice.feed_start().await;
    feed.libraries().await;
    assert!(feed.within(Duration::from_millis(700)).await.is_none());
    assert!(feed.pings >= 2, "{} pings", feed.pings);
}

/// At most ten streams per credential: the eleventh closes the oldest, which
/// is a client gone without closing (asleep, a network change, a proxy holding
/// the upstream) more often than not. A client still there reconnects.
#[sqlx::test]
async fn at_most_ten_streams_per_credential(pool: PgPool) {
    let w = world(Kind::Filesystem, pool).await;
    let alice = w.as_user(1).await;
    let mut feeds = Vec::new();
    for _ in 0..10 {
        let mut feed = alice.feed_start().await;
        feed.libraries().await;
        feeds.push(feed);
    }
    let mut eleventh = alice.feed(&BTreeMap::<Uuid, i64>::new()).await.unwrap();
    eleventh.libraries().await;
    assert!(
        feeds[0].ends_within(Duration::from_secs(5)).await,
        "the oldest ends"
    );
    for feed in &mut feeds[1..] {
        assert!(feed.quiet().await, "the others go on");
    }
    // Another credential of the same account has its own ten.
    let other = alice.with_app_password(json!({})).await;
    other.feed_start().await;
}

#[sqlx::test]
async fn a_fallen_credential_ends_its_streams(pool: PgPool) {
    let w = world(Kind::Filesystem, pool).await;
    let (app, state) = quick(&w.pool).await;
    tokio::spawn(theca_core::events::watch_revocations(state));
    let mut alice = w.as_user_on(app.clone(), 1).await;
    let docs = alice.create_library("Docs").await;

    // An app password revoked: nothing more is read, the stream ends.
    let created: api::NewAppPassword = alice
        .json(
            "POST",
            "/api/v1/auth/app-passwords",
            &json!({ "name": "sync" }),
        )
        .await
        .unwrap();
    let sync = Client {
        app: app.clone(),
        account: alice.account,
        org: alice.org,
        cookies: String::new(),
        bearer: Some(created.password),
        puts: 0,
    };
    let mut feed = sync.feed_start().await;
    feed.libraries().await;
    let revoke = format!("/api/v1/auth/app-passwords/{}", created.app_password.id);
    assert!(
        alice
            .call("DELETE", &revoke, None, true)
            .await
            .0
            .is_success()
    );
    alice.put_file(&docs, docs.root, "after.txt", b"x").await;
    assert!(feed.ends_within(Duration::from_secs(2)).await);

    // A session logged out, a session expired, an account suspended.
    for end in ["logout", "expired", "suspended"] {
        let bob = w.as_user_on(app.clone(), 2).await;
        let mut feed = bob.feed_start().await;
        feed.libraries().await;
        match end {
            "logout" => assert!(bob.call("POST", "/api/v1/auth/logout", None, true).await.0.is_success()),
            "expired" => w.sql("UPDATE token SET last_seen = now() - interval '400 days' WHERE usage = 'session' AND resource = (SELECT uuid FROM account WHERE pk = 2)").await,
            _ => w.sql("UPDATE account SET suspended_at = now() WHERE pk = 2").await,
        }
        assert!(feed.ends_within(Duration::from_secs(2)).await, "{end}");
        w.sql("UPDATE account SET suspended_at = NULL WHERE pk = 2")
            .await;
    }
}

#[sqlx::test]
async fn commits_during_a_read_are_all_caught_up(pool: PgPool) {
    let w = world(Kind::Filesystem, pool).await;
    let alice = w.as_user(1).await;
    let docs = alice.create_library("Docs").await;
    let mut feed = alice.feed_start().await;
    feed.libraries().await;
    let writer = {
        let alice = w.as_user(1).await;
        tokio::spawn(async move {
            for i in 0..100 {
                alice.mkdir(docs.root, &[&format!("f{i}")]).await;
            }
        })
    };
    let mut seen = HashSet::new();
    let mut messages = 0;
    while seen.len() < 100 {
        let changes = feed.changes().await;
        messages += 1;
        seen.extend(changes.entries.iter().map(|c| c.id));
    }
    writer.await.unwrap();
    assert_eq!(seen.len(), 100);
    assert!(feed.quiet().await);
    // Wake-ups merging while a stream reads: the registry's own test.
    let _ = messages;
}

// Convergence: a client applying the feed ends with the server's tree.

/// A client's copy of the trees it follows: node → (parent, name, version);
/// the libraries' roots.
#[derive(Default, Clone)]
struct Mirror {
    nodes: HashMap<Uuid, (Uuid, String, Option<Uuid>)>,
    roots: HashMap<Uuid, Uuid>,
    positions: BTreeMap<Uuid, i64>,
}

impl Mirror {
    fn remove_subtree(&mut self, id: Uuid) {
        let children: Vec<Uuid> = self
            .nodes
            .iter()
            .filter(|(_, n)| n.0 == id)
            .map(|(c, _)| *c)
            .collect();
        for child in children {
            self.remove_subtree(child);
        }
        self.nodes.remove(&id);
    }

    async fn list(&mut self, client: &Client, folder: Uuid) {
        for (id, parent, name, version) in client.tree_if_any(folder).await.unwrap_or_default() {
            self.nodes.insert(id, (parent, name, version));
        }
    }

    /// The client rules (sprint 08, Reading).
    async fn apply(&mut self, client: &Client, msg: Msg) {
        match msg {
            Msg::Libraries(libraries) => {
                let listed: HashMap<Uuid, &api::FeedLibrary> =
                    libraries.iter().map(|l| (l.id, l)).collect();
                let gone: Vec<Uuid> = self
                    .roots
                    .keys()
                    .filter(|l| !listed.contains_key(l))
                    .copied()
                    .collect();
                for library in gone {
                    let root = self.roots.remove(&library).unwrap();
                    self.remove_subtree(root);
                    self.positions.remove(&library);
                }
                for l in libraries.iter().filter(|l| !l.trashed) {
                    if !self.roots.contains_key(&l.id) || l.reset {
                        self.roots.insert(l.id, l.root);
                        self.positions.insert(l.id, l.position);
                        self.list(client, l.root).await;
                    }
                }
            }
            Msg::Changes(changes) => {
                for change in changes.entries {
                    if change.kind == api::ResourceKind::Library {
                        continue;
                    }
                    match change.state {
                        None => self.remove_subtree(change.id),
                        Some(state) => {
                            let known = self.nodes.contains_key(&change.id);
                            self.nodes.insert(
                                change.id,
                                (state.parent.unwrap(), state.name, state.current_version),
                            );
                            if !known && !state.is_file {
                                self.list(client, change.id).await;
                            }
                        }
                    }
                }
                self.positions.extend(changes.positions);
            }
        }
    }

    /// Equal to the server's tree, library by library.
    async fn assert_matches(&self, client: &Client) {
        for (library, root) in &self.roots {
            let mut server: Vec<_> = client
                .tree(*root)
                .await
                .into_iter()
                .map(|(id, p, n, v)| (id, (p, n, v)))
                .collect();
            server.sort();
            let mut mine: Vec<_> = self
                .nodes
                .iter()
                .filter(|(id, _)| self.root_of(**id) == Some(*root))
                .map(|(id, n)| (*id, n.clone()))
                .collect();
            mine.sort();
            assert_eq!(mine, server, "library {library}");
        }
        let followed: usize = self
            .roots
            .values()
            .map(|r| {
                self.nodes
                    .keys()
                    .filter(|n| self.root_of(**n) == Some(*r))
                    .count()
            })
            .sum();
        assert_eq!(followed, self.nodes.len(), "no stray node");
    }

    fn root_of(&self, mut id: Uuid) -> Option<Uuid> {
        for _ in 0..1000 {
            match self.nodes.get(&id) {
                Some((parent, _, _)) => id = *parent,
                None => return self.roots.values().any(|r| *r == id).then_some(id),
            }
        }
        None
    }
}

/// Applies the feed until it goes quiet.
async fn drain(mirror: &mut Mirror, client: &Client, feed: &mut Feed) {
    while let Some(msg) = feed.within(Duration::from_millis(500)).await {
        mirror.apply(client, msg).await;
    }
}

#[sqlx::test]
async fn a_client_applying_the_feed_converges(pool: PgPool) {
    let w = world(Kind::Filesystem, pool).await;
    let mut alice = w.as_user(1).await;
    let bob = w.as_user(2).await;
    let a = alice.create_library("A").await;
    let b = alice.create_library("B").await;
    w.add_member(a.id, 2, "read").await;
    let auth = dav(&alice, json!({ "library": a.id })).await;
    let davs = |method: &'static str,
                path: String,
                headers: Vec<(&'static str, String)>,
                body: &'static [u8]| {
        let (app, auth) = (w.app.clone(), auth.clone());
        async move {
            let headers: Vec<(&str, &str)> =
                headers.iter().map(|(k, v)| (*k, v.as_str())).collect();
            let status = dav_call(&app, &auth, method, &path, &headers, body).await;
            assert!(status.is_success(), "{method} {path}: {status}");
        }
    };

    // Before the client: a tree in A, through the API and WebDAV.
    let docs = alice.mkdir(a.root, &["docs/old", "photos"]).await;
    let docs_id = node_id(&w.pool, a.id, "docs").await;
    let old = docs[0].id;
    let photos = docs[1].id;
    let f1 = alice.put_file(&a, docs_id, "one.txt", b"one").await;
    alice.put_file(&a, old, "two.txt", b"two").await;
    davs("PUT", "/photos/p.jpg".into(), vec![], b"photo").await;

    // Bob's client starts: an empty map, the trees listed.
    let mut mirror = Mirror::default();
    let mut feed = bob.feed_start().await;
    drain(&mut mirror, &bob, &mut feed).await;
    mirror.assert_matches(&bob).await;
    let middle = mirror.clone();

    // Every kind of change, the client applying and checking after each
    // group (a missing event can hide in a batch that also holds a later one).
    let v1 = alice.manifest(f1, None).await.unwrap().version;
    alice
        .upload(&a, json!({ "file": f1, "base_version": v1 }), b"one, v2")
        .await
        .unwrap();
    let v2 = alice.manifest(f1, None).await.unwrap().version;
    alice
        .upload(
            &a,
            json!({ "file": f1, "base_version": v2, "modified": "2025-05-05T00:00:00Z" }),
            b"one, v2",
        )
        .await
        .unwrap();
    let restore = format!("/api/v1/files/{f1}/versions/{v1}/restore");
    assert!(
        alice
            .call("POST", &restore, None, true)
            .await
            .0
            .is_success()
    );
    alice.mkdir(a.root, &["new/deep/er"]).await;
    drain(&mut mirror, &bob, &mut feed).await;
    mirror.assert_matches(&bob).await;
    alice
        .patch_node(f1, json!({ "name": "uno.txt" }))
        .await
        .unwrap();
    alice
        .patch_node(f1, json!({ "parent": photos }))
        .await
        .unwrap();
    // A folder with children to B (out of Bob's reach), another back from B.
    alice
        .patch_node(old, json!({ "parent": b.root }))
        .await
        .unwrap();
    let back = alice.mkdir(b.root, &["back/inside"]).await;
    let back_id = node_id(&w.pool, b.id, "back").await;
    alice.put_file(&b, back[0].id, "deep.txt", b"deep").await;
    alice
        .patch_node(back_id, json!({ "parent": a.root }))
        .await
        .unwrap();
    drain(&mut mirror, &bob, &mut feed).await;
    mirror.assert_matches(&bob).await;
    // Copies, a graft, the trash.
    alice
        .copy_node(docs_id, json!({ "parent": a.root, "name": "docs copy" }))
        .await
        .unwrap();
    let g = alice.put_file(&a, photos, "g.txt", b"graft").await;
    alice
        .patch_node(g, json!({ "name": "uno.txt", "replace": true }))
        .await
        .unwrap();
    drain(&mut mirror, &bob, &mut feed).await;
    mirror.assert_matches(&bob).await;
    alice.trash_node(docs_id, false).await;
    drain(&mut mirror, &bob, &mut feed).await;
    mirror.assert_matches(&bob).await;
    alice.restore_node(docs_id).await.unwrap();
    drain(&mut mirror, &bob, &mut feed).await;
    mirror.assert_matches(&bob).await;
    alice.trash_node(photos, false).await;
    // WebDAV, with a concurrent writer.
    let writer = {
        let mut alice = w.as_user(1).await;
        let a = a.clone();
        tokio::spawn(async move {
            for i in 0..20 {
                alice
                    .put_file(&a, a.root, &format!("w{i}.txt"), format!("w{i}").as_bytes())
                    .await;
            }
        })
    };
    davs("PUT", "/docs/dav.txt".into(), vec![], b"dav").await;
    davs("MKCOL", "/davdir/".into(), vec![], b"").await;
    davs(
        "MOVE",
        "/docs/dav.txt".into(),
        vec![("destination", "/dav/davdir/dav.txt".into())],
        b"",
    )
    .await;
    davs(
        "COPY",
        "/davdir/".into(),
        vec![("destination", "/dav/davcopy/".into())],
        b"",
    )
    .await;
    davs("DELETE", "/davdir/".into(), vec![], b"").await;
    writer.await.unwrap();

    // Midway, Bob gains B.
    let members = format!("/api/v1/libraries/{}/members/{}", b.id, w.users[1]);
    alice
        .json::<Value>("PUT", &members, &json!({ "role": "read" }))
        .await
        .unwrap();
    alice.put_file(&b, b.root, "after-joining.txt", b"b").await;

    drain(&mut mirror, &bob, &mut feed).await;
    mirror.assert_matches(&bob).await;
    assert_eq!(mirror.roots.len(), 2, "A, then B");

    // From the middle (a client that was offline), across a cut.
    let mut late = middle;
    let mut feed = bob.feed(&late.positions).await.unwrap();
    for _ in 0..2 {
        if let Some(msg) = feed.within(Duration::from_millis(500)).await {
            late.apply(&bob, msg).await;
        }
    }
    drop(feed);
    let mut feed = bob.feed(&late.positions).await.unwrap();
    drain(&mut late, &bob, &mut feed).await;
    late.assert_matches(&bob).await;
}
