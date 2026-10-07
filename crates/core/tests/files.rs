//! File operations through the router: folders, renames, moves, copies, the
//! trash, versions and search (filesystem backend; storage is irrelevant here).

#![allow(dead_code, unused_imports)] // the shared harness has more than these tests use

include!("common/harness.rs");

async fn fs_world(pool: PgPool) -> World {
    world(Kind::Filesystem, pool).await
}

impl Client {
    async fn mkdir(&self, parent: Uuid, name: &str) -> Uuid {
        self.folders(parent, &[name]).await.unwrap()[0].id
    }

    async fn folders(&self, parent: Uuid, paths: &[&str]) -> Result<Vec<api::FolderPath>, Failure> {
        self.json(
            "POST",
            &format!("/api/v1/nodes/{parent}/folders"),
            &json!({ "paths": paths }),
        )
        .await
    }

    /// `GET /nodes/{id}/tree`, its NDJSON lines.
    async fn tree(&self, folder: Uuid) -> Result<Vec<api::TreeLine>, Failure> {
        let (status, bytes) = self
            .call("GET", &format!("/api/v1/nodes/{folder}/tree"), None, true)
            .await;
        if !status.is_success() {
            return Err((
                status,
                serde_json::from_slice(&bytes).unwrap_or(Value::Null),
            ));
        }
        Ok(bytes
            .split(|b| *b == b'\n')
            .filter(|line| !line.is_empty())
            .map(|line| serde_json::from_slice(line).unwrap())
            .collect())
    }

    async fn info(&self, node: Uuid) -> Result<api::NodeInfo, Failure> {
        self.get(&format!("/api/v1/nodes/{node}")).await
    }

    async fn patch(&self, node: Uuid, body: Value) -> Result<api::NodeInfo, Failure> {
        self.json("PATCH", &format!("/api/v1/nodes/{node}"), &body)
            .await
    }

    async fn delete(&self, path: &str) -> StatusCode {
        self.call("DELETE", path, None, true).await.0
    }

    async fn trash(&self, node: Uuid) {
        assert_eq!(
            self.delete(&format!("/api/v1/nodes/{node}")).await,
            StatusCode::NO_CONTENT
        );
    }

    async fn restore(&self, node: Uuid) -> Result<api::Restored, Failure> {
        self.json("POST", &format!("/api/v1/nodes/{node}/restore"), &json!({}))
            .await
    }

    async fn children(&self, folder: Uuid) -> Vec<String> {
        let nodes: Vec<api::Node> = self
            .get(&format!("/api/v1/nodes/{folder}/children"))
            .await
            .unwrap();
        nodes.into_iter().map(|n| n.name).collect()
    }

    async fn file(
        &mut self,
        library: &api::Library,
        parent: Uuid,
        name: &str,
        data: &[u8],
    ) -> Uuid {
        let target = json!({ "parent": parent, "name": name });
        self.upload(library, target, data).await.unwrap().file
    }

    /// The size of a file's current version.
    async fn size(&self, file: Uuid) -> i64 {
        self.info(file).await.unwrap().size.unwrap()
    }

    async fn versions(&self, file: Uuid) -> Vec<api::Version> {
        self.get(&format!("/api/v1/files/{file}/versions"))
            .await
            .unwrap()
    }

    async fn search(&self, q: &str) -> Result<Vec<String>, Failure> {
        let q = q.replace('%', "%25").replace(' ', "%20");
        let found: Vec<api::SearchResult> = self.get(&format!("/api/v1/search?q={q}")).await?;
        Ok(found.into_iter().map(|r| r.path).collect())
    }
}

fn code(failure: Failure) -> (StatusCode, String) {
    (
        failure.0,
        failure.1["code"].as_str().unwrap_or_default().to_owned(),
    )
}

#[sqlx::test]
async fn folders_renames_moves(pool: PgPool) {
    let w = fs_world(pool).await;
    let mut alice = w.as_user(1).await;
    let docs = alice.create_library("Docs").await;
    let a = alice.mkdir(docs.root, "A").await;
    let b = alice.mkdir(docs.root, "B").await;
    // An existing folder (ignoring case) is kept, like `mkdir -p`.
    let again = alice.folders(docs.root, &["a"]).await.unwrap();
    assert_eq!((again[0].id, again[0].created), (a, false));
    let f = alice.file(&docs, a, "x.txt", &data(1000, 1)).await;

    // Rename, then move a folder: its contents follow.
    assert_eq!(
        alice
            .patch(f, json!({ "name": "y.txt" }))
            .await
            .unwrap()
            .path,
        "/A/y.txt"
    );
    assert_eq!(
        alice.patch(a, json!({ "parent": b })).await.unwrap().path,
        "/B/A"
    );
    assert_eq!(alice.info(f).await.unwrap().path, "/B/A/y.txt");
    let cycle = alice.patch(b, json!({ "parent": a })).await.unwrap_err();
    assert_eq!(
        code(cycle),
        (StatusCode::CONFLICT, "move_into_itself".into())
    );
    let root = alice
        .patch(docs.root, json!({ "name": "x" }))
        .await
        .unwrap_err();
    assert_eq!(code(root), (StatusCode::BAD_REQUEST, "root".into()));
    assert_eq!(
        alice.delete(&format!("/api/v1/nodes/{}", docs.root)).await,
        StatusCode::BAD_REQUEST
    );

    // Members only; read-only members can't change anything.
    let bob = w.as_user(2).await;
    assert_eq!(bob.info(a).await.unwrap_err().0, StatusCode::NOT_FOUND);
    w.add_member(docs.id, 2, "read").await;
    assert_eq!(bob.info(a).await.unwrap().path, "/B/A");
    assert_eq!(
        bob.patch(a, json!({ "name": "Z" })).await.unwrap_err().0,
        StatusCode::FORBIDDEN
    );

    // To another library: the subtree and its history follow.
    let other = alice.create_library("Other").await;
    let moved = alice
        .patch(a, json!({ "parent": other.root }))
        .await
        .unwrap();
    assert_eq!((moved.library, moved.path.as_str()), (other.id, "/A"));
    let file = alice.info(f).await.unwrap();
    assert_eq!((file.library, file.path.as_str()), (other.id, "/A/y.txt"));
    assert_eq!(alice.download(f).await.unwrap(), data(1000, 1));
    assert_eq!(alice.versions(f).await.len(), 1);
    assert!(alice.children(b).await.is_empty());

    // Never to another organization.
    w.sql(r#"INSERT INTO "user" (pk, account, organization, joined_at) OVERRIDING SYSTEM VALUE VALUES (4, 1, 2, now())"#)
        .await;
    let theirs: api::Library = alice
        .json(
            "POST",
            "/api/v1/libraries",
            &json!({ "name": "Theirs", "organization": w.organizations[1] }),
        )
        .await
        .unwrap();
    let away = alice
        .patch(a, json!({ "parent": theirs.root }))
        .await
        .unwrap_err();
    assert_eq!(
        code(away),
        (StatusCode::CONFLICT, "other_organization".into())
    );
    let copy = alice
        .json::<Value>(
            "POST",
            &format!("/api/v1/nodes/{a}/copy"),
            &json!({ "parent": theirs.root }),
        )
        .await
        .unwrap_err();
    assert_eq!(
        code(copy),
        (StatusCode::CONFLICT, "other_organization".into())
    );
}

#[sqlx::test]
async fn copies_share_chunks(pool: PgPool) {
    let w = fs_world(pool).await;
    let mut alice = w.as_user(1).await;
    let docs = alice.create_library("Docs").await;
    let src = alice.mkdir(docs.root, "src").await;
    let sub = alice.mkdir(src, "sub").await;
    let big = data(3 * MIB, 2);
    alice.file(&docs, src, "big.bin", &big).await;
    alice.file(&docs, sub, "small.txt", &data(100, 3)).await;
    let gone = alice.file(&docs, src, "gone.txt", &data(100, 4)).await;
    alice.trash(gone).await;
    let chunks = |w: &World| {
        let pool = w.pool.clone();
        async move {
            sqlx::query_scalar::<_, i64>("SELECT count(*) FROM chunk")
                .fetch_one(&pool)
                .await
                .unwrap()
        }
    };
    let before = chunks(&w).await;

    let other = alice.create_library("Other").await;
    let copy: api::NodeInfo = alice
        .json(
            "POST",
            &format!("/api/v1/nodes/{src}/copy"),
            &json!({ "parent": other.root, "name": "dst" }),
        )
        .await
        .unwrap();
    assert_eq!(copy.path, "/dst");
    assert_eq!(chunks(&w).await, before, "no chunk duplicated");
    assert_eq!(
        alice.children(copy.id).await,
        ["sub", "big.bin"],
        "trashed files skipped"
    );
    let copied: Vec<api::Node> = alice
        .get(&format!("/api/v1/nodes/{}/children", copy.id))
        .await
        .unwrap();
    let copied_big = copied.iter().find(|n| n.name == "big.bin").unwrap().id;
    assert_eq!(alice.download(copied_big).await.unwrap(), big);
    assert_eq!(alice.versions(copied_big).await.len(), 1, "a fresh history");
    assert_eq!(alice.size(copied_big).await, big.len() as i64);

    // Into its own subtree: a snapshot, so it terminates.
    let nested: api::NodeInfo = alice
        .json(
            "POST",
            &format!("/api/v1/nodes/{src}/copy"),
            &json!({ "parent": sub }),
        )
        .await
        .unwrap();
    assert_eq!(nested.path, "/src/sub/src");
    assert_eq!(alice.children(nested.id).await, ["sub", "big.bin"]);

    // Not into a library the user only reads.
    w.add_member(other.id, 2, "write").await;
    let bob = w.as_user(2).await;
    w.add_member(docs.id, 2, "read").await;
    let refused = bob
        .json::<Value>(
            "POST",
            &format!("/api/v1/nodes/{}/copy", other.root),
            &json!({ "parent": docs.root }),
        )
        .await
        .unwrap_err();
    assert_eq!(refused.0, StatusCode::BAD_REQUEST, "the root isn't copied");
    let refused = bob
        .json::<Value>(
            "POST",
            &format!("/api/v1/nodes/{}/copy", copy.id),
            &json!({ "parent": docs.root }),
        )
        .await
        .unwrap_err();
    assert_eq!(refused.0, StatusCode::FORBIDDEN);
}

#[sqlx::test]
async fn trash(pool: PgPool) {
    let w = fs_world(pool).await;
    let mut alice = w.as_user(1).await;
    let docs = alice.create_library("Docs").await;
    let t = alice.mkdir(docs.root, "T").await;
    let sub = alice.mkdir(t, "sub").await;
    let a = alice.file(&docs, t, "a.txt", &data(100, 1)).await;
    let b = alice.file(&docs, sub, "b.txt", &data(100, 2)).await;

    // b first, then its ancestor T: the whole subtree is unreachable.
    alice.trash(b).await;
    alice.trash(t).await;
    assert!(alice.children(docs.root).await.is_empty());
    assert_eq!(alice.info(a).await.unwrap_err().0, StatusCode::NOT_FOUND);
    assert_eq!(
        alice.download(a).await.unwrap_err().0,
        StatusCode::NOT_FOUND
    );
    let edit = alice
        .upload(&docs, json!({ "file": a }), &data(100, 9))
        .await
        .unwrap_err();
    assert_eq!(edit.0, StatusCode::NOT_FOUND);
    let inside = alice.folders(sub, &["x"]).await.unwrap_err();
    assert_eq!(inside.0, StatusCode::NOT_FOUND);
    assert!(alice.search("a.txt").await.unwrap().is_empty());
    let listed: Vec<api::TrashEntry> = alice
        .get(&format!("/api/v1/libraries/{}/trash", docs.id))
        .await
        .unwrap();
    let mut paths: Vec<&str> = listed.iter().map(|e| e.path.as_str()).collect();
    paths.sort();
    assert_eq!(paths, ["/T", "/T/sub/b.txt"]);

    // Its name is free again; restoring T renames it, and brings back what
    // was trashed with it, not b (trashed earlier).
    alice.mkdir(docs.root, "T").await;
    let restored = alice.restore(t).await.unwrap();
    assert_eq!(
        (restored.name.as_str(), restored.path.as_str()),
        ("T (restored)", "/T (restored)")
    );
    assert_eq!(alice.info(a).await.unwrap().path, "/T (restored)/a.txt");
    assert_eq!(alice.info(b).await.unwrap_err().0, StatusCode::NOT_FOUND);
    assert_eq!(
        alice.restore(b).await.unwrap().path,
        "/T (restored)/sub/b.txt"
    );

    // A node whose folder is trashed comes back into the root.
    alice.trash(b).await;
    alice.trash(sub).await;
    let back = alice.restore(b).await.unwrap();
    assert_eq!((back.parent, back.path.as_str()), (docs.root, "/b.txt"));
    assert_eq!(
        alice.restore(b).await.unwrap_err().0,
        StatusCode::NOT_FOUND,
        "not in the trash"
    );

    // Deleted for good: only from the trash; nodes and history go, chunks stay (GC).
    let tr = format!("/api/v1/nodes/{t}?permanent=true");
    assert_eq!(alice.delete(&tr).await, StatusCode::NOT_FOUND);
    alice.trash(t).await;
    assert_eq!(alice.delete(&tr).await, StatusCode::NO_CONTENT);
    let count = |sql: &'static str| {
        let pool = w.pool.clone();
        async move {
            sqlx::query_scalar::<_, i64>(sql)
                .fetch_one(&pool)
                .await
                .unwrap()
        }
    };
    assert_eq!(
        count("SELECT count(*) FROM node WHERE name IN ('a.txt', 'sub', 'T (restored)')").await,
        0
    );
    assert_eq!(
        count("SELECT count(*) FROM version").await,
        1,
        "only b.txt's"
    );
    // b.txt's, a.txt's, and the pending one of the refused edit.
    assert_eq!(count("SELECT count(*) FROM chunk").await, 3);
}

#[sqlx::test]
async fn versions(pool: PgPool) {
    let w = fs_world(pool).await;
    let mut alice = w.as_user(1).await;
    let docs = alice.create_library("Docs").await;
    let (one, two, three) = (data(500, 1), data(500, 2), data(500, 3));
    let c1 = alice
        .upload(&docs, new_file(&docs, "f.txt"), &one)
        .await
        .unwrap();
    let f = c1.file;
    let c2 = alice
        .upload(
            &docs,
            json!({ "file": f, "base_version": c1.version }),
            &two,
        )
        .await
        .unwrap();
    let c3 = alice
        .upload(
            &docs,
            json!({ "file": f, "base_version": c1.version }),
            &three,
        )
        .await
        .unwrap();
    assert!(c3.conflict);

    let tree = alice.versions(f).await;
    let parent = |v: Uuid| tree.iter().find(|x| x.id == v).unwrap().parent;
    assert_eq!(
        (parent(c1.version), parent(c2.version), parent(c3.version)),
        (None, Some(c1.version), Some(c1.version))
    );
    assert!(tree.iter().find(|x| x.id == c3.version).unwrap().current);
    assert!(tree.iter().all(|v| v.author.as_deref() == Some("a@x.ch")));
    let old = alice.manifest(f, Some(c1.version)).await.unwrap();
    assert_eq!(old.version, c1.version);

    // Restoring only moves the current version; the next commit branches from it.
    let restore = format!("/api/v1/files/{f}/versions/{}/restore", c2.version);
    let (status, _) = alice
        .call("POST", &restore, Some(b"{}".to_vec()), true)
        .await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    assert_eq!(alice.download(f).await.unwrap(), two);
    assert_eq!(alice.versions(f).await.len(), 3, "nothing created");
    let c4 = alice
        .upload(
            &docs,
            json!({ "file": f, "base_version": c2.version }),
            &one,
        )
        .await
        .unwrap();
    assert!(!c4.conflict);
    assert_eq!(
        alice
            .versions(f)
            .await
            .iter()
            .find(|v| v.id == c4.version)
            .unwrap()
            .parent,
        Some(c2.version)
    );

    // RCS-style numbers: c2 continues c1's line, c3 branches, c4 follows c2.
    let numbers: Vec<(Uuid, String)> = alice
        .versions(f)
        .await
        .into_iter()
        .map(|v| (v.id, v.number))
        .collect();
    assert_eq!(
        numbers,
        [
            (c1.version, "1".to_owned()),
            (c2.version, "2".to_owned()),
            (c4.version, "3".to_owned()),
            (c3.version, "1.1.1".to_owned()),
        ],
        "tree order"
    );

    // Another file's version: unknown here.
    let g = alice
        .upload(&docs, new_file(&docs, "g.txt"), &one)
        .await
        .unwrap();
    let wrong = format!("/api/v1/files/{f}/versions/{}/restore", g.version);
    assert_eq!(
        alice
            .call("POST", &wrong, Some(b"{}".to_vec()), true)
            .await
            .0,
        StatusCode::NOT_FOUND
    );
    assert_eq!(
        alice.manifest(f, Some(g.version)).await.unwrap_err().0,
        StatusCode::NOT_FOUND
    );
}

#[sqlx::test]
async fn search(pool: PgPool) {
    let w = fs_world(pool).await;
    let mut alice = w.as_user(1).await;
    let docs = alice.create_library("Docs").await;
    let reports = alice.mkdir(docs.root, "Reports").await;
    for name in ["Q1 report.pdf", "100% done.txt", "notes.txt"] {
        alice.file(&docs, reports, name, &data(10, 1)).await;
    }
    let old = alice
        .file(&docs, docs.root, "old report.txt", &data(10, 2))
        .await;
    alice.trash(old).await;
    let mut carol = w.as_user(3).await;
    let theirs = carol.create_library("Theirs").await;
    carol
        .file(&theirs, theirs.root, "secret report.txt", &data(10, 3))
        .await;

    assert_eq!(
        alice.search("REPORT").await.unwrap(),
        ["/Reports/Q1 report.pdf", "/Reports"]
    );
    assert_eq!(
        alice.search("100%").await.unwrap(),
        ["/Reports/100% done.txt"]
    );
    assert_eq!(
        alice.search("0% d").await.unwrap(),
        ["/Reports/100% done.txt"]
    );
    assert_eq!(
        alice.search("_._").await.unwrap(),
        Vec::<String>::new(),
        "wildcards are literal"
    );
    assert_eq!(
        carol.search("report").await.unwrap(),
        ["/secret report.txt"]
    );
    assert_eq!(
        code(alice.search("ab").await.unwrap_err()),
        (StatusCode::BAD_REQUEST, "query_too_short".into())
    );
}

#[sqlx::test]
async fn moves_onto_files_merge_histories(pool: PgPool) {
    let w = fs_world(pool).await;
    let mut alice = w.as_user(1).await;
    let docs = alice.create_library("Docs").await;
    let other = alice.create_library("Other").await;
    let b1 = alice
        .upload(&docs, new_file(&docs, "b.txt"), &data(500, 1))
        .await
        .unwrap();
    let a1 = alice
        .upload(&docs, new_file(&docs, "a.txt"), &data(500, 2))
        .await
        .unwrap();
    let a2 = alice
        .upload(
            &docs,
            json!({ "file": a1.file, "base_version": a1.version }),
            &data(500, 3),
        )
        .await
        .unwrap();

    // Without `replace`: refused.
    assert_eq!(
        code(
            alice
                .patch(a1.file, json!({ "name": "b.txt" }))
                .await
                .unwrap_err()
        ),
        (StatusCode::CONFLICT, "name_taken".into())
    );
    let merged = alice
        .patch(a1.file, json!({ "name": "B.TXT", "replace": true }))
        .await
        .unwrap();
    assert_eq!(merged.id, b1.file, "it is b.txt now");
    assert_eq!(alice.download(b1.file).await.unwrap(), data(500, 3));
    let tree: Vec<(Uuid, String, bool)> = alice
        .versions(b1.file)
        .await
        .into_iter()
        .map(|v| (v.id, v.number, v.current))
        .collect();
    assert_eq!(
        tree,
        [
            (b1.version, "1".to_owned(), false),
            (a1.version, "2".to_owned(), false),
            (a2.version, "3".to_owned(), true),
        ]
    );
    assert_eq!(
        code(alice.info(a1.file).await.unwrap_err()).0,
        StatusCode::NOT_FOUND
    );
    assert_eq!(alice.children(docs.root).await, ["b.txt"]);

    // Across libraries of the organization, the same.
    let c = alice
        .upload(&other, new_file(&other, "b.txt"), &data(500, 4))
        .await
        .unwrap();
    let merged = alice
        .patch(b1.file, json!({ "parent": other.root, "replace": true }))
        .await
        .unwrap();
    assert_eq!((merged.id, merged.library), (c.file, other.id));
    assert_eq!(alice.versions(c.file).await.len(), 4);
    assert_eq!(alice.download(c.file).await.unwrap(), data(500, 3));

    // Folders are never replaced.
    let folder = alice.mkdir(other.root, "f").await;
    assert_eq!(
        code(
            alice
                .patch(folder, json!({ "name": "b.txt", "replace": true }))
                .await
                .unwrap_err()
        ),
        (StatusCode::CONFLICT, "name_taken".into())
    );
}

#[sqlx::test]
async fn copies_onto_files_add_a_version(pool: PgPool) {
    let w = fs_world(pool).await;
    let mut alice = w.as_user(1).await;
    let docs = alice.create_library("Docs").await;
    let b = alice
        .upload(&docs, new_file(&docs, "b.txt"), &data(500, 1))
        .await
        .unwrap();
    let a = alice
        .upload(&docs, new_file(&docs, "a.txt"), &data(500, 2))
        .await
        .unwrap();
    let copy: api::NodeInfo = alice
        .json(
            "POST",
            &format!("/api/v1/nodes/{}/copy", a.file),
            &json!({ "parent": docs.root, "name": "b.txt", "replace": true }),
        )
        .await
        .unwrap();
    assert_eq!(copy.id, b.file);
    assert_eq!(alice.download(b.file).await.unwrap(), data(500, 2));
    let tree = alice.versions(b.file).await;
    assert_eq!(tree.len(), 2);
    assert_eq!(tree[1].parent, Some(b.version), "based on its current one");
    assert_eq!(alice.size(b.file).await, 500);
    let empty = alice
        .upload(&docs, new_file(&docs, "empty.txt"), &[])
        .await
        .unwrap();
    assert_eq!(alice.size(empty.file).await, 0);
    assert_eq!(
        alice.download(a.file).await.unwrap(),
        data(500, 2),
        "the source stays"
    );
}

#[sqlx::test]
async fn library_trash(pool: PgPool) {
    let w = fs_world(pool).await;
    let mut alice = w.as_user(1).await;
    let photos = alice.create_library("Photos").await;
    let file = alice
        .upload(&photos, new_file(&photos, "a.jpg"), &data(500, 1))
        .await
        .unwrap();
    let names = |libraries: Vec<api::Library>| -> Vec<String> {
        libraries.into_iter().map(|l| l.name).collect()
    };

    // Renamed, then a taken name refused.
    let renamed: api::Library = alice
        .json(
            "PATCH",
            &format!("/api/v1/libraries/{}", photos.id),
            &json!({ "name": "Pictures" }),
        )
        .await
        .unwrap();
    assert_eq!(renamed.name, "Pictures");
    alice.create_library("Taken").await;
    let refused = alice
        .json::<api::Library>(
            "PATCH",
            &format!("/api/v1/libraries/{}", photos.id),
            &json!({ "name": "taken" }),
        )
        .await
        .unwrap_err();
    assert_eq!(
        code(refused),
        (StatusCode::CONFLICT, "library_name_taken".into())
    );

    // Trashed: out of reach, its name free.
    assert_eq!(
        alice
            .delete(&format!("/api/v1/libraries/{}", photos.id))
            .await,
        StatusCode::NO_CONTENT
    );
    let listed: Vec<api::Library> = alice.get("/api/v1/libraries").await.unwrap();
    assert_eq!(names(listed), ["Taken"]);
    assert_eq!(
        code(alice.info(file.file).await.unwrap_err()).0,
        StatusCode::NOT_FOUND
    );
    let trash: Vec<api::TrashedLibrary> = alice
        .get(&format!("/api/v1/organizations/{}/trash", alice.org))
        .await
        .unwrap();
    assert_eq!((trash.len(), trash[0].name.as_str()), (1, "Pictures"));
    alice.create_library("Pictures").await;

    // Restored under another name, its files back.
    let back: api::Library = alice
        .json(
            "POST",
            &format!("/api/v1/libraries/{}/restore", photos.id),
            &json!({}),
        )
        .await
        .unwrap();
    assert_eq!(back.name, "Pictures (restored)");
    assert_eq!(alice.download(file.file).await.unwrap(), data(500, 1));

    // Purged: gone for good, its chunks left to the GC.
    alice
        .delete(&format!("/api/v1/libraries/{}", photos.id))
        .await;
    assert_eq!(
        alice
            .delete(&format!("/api/v1/libraries/{}?permanent=true", photos.id))
            .await,
        StatusCode::NO_CONTENT
    );
    let trash: Vec<api::TrashedLibrary> = alice
        .get(&format!("/api/v1/organizations/{}/trash", alice.org))
        .await
        .unwrap();
    assert!(trash.is_empty());
    let left: i64 = sqlx::query_scalar("SELECT count(*) FROM library WHERE uuid = $1")
        .bind(photos.id)
        .fetch_one(&w.pool)
        .await
        .unwrap();
    assert_eq!(left, 0);
}

#[sqlx::test]
async fn commit_batches(pool: PgPool) {
    let w = fs_world(pool).await;
    let mut alice = w.as_user(1).await;
    let docs = alice.create_library("Docs").await;
    let taken = alice
        .upload(&docs, new_file(&docs, "taken.txt"), &data(100, 1))
        .await
        .unwrap();
    let commit =
        |name: &str| json!({ "library": docs.id, "parent": docs.root, "name": name, "chunks": [] });
    // Each commit its own outcome, in order: one refused, the others committed.
    let outcomes: Vec<api::CommitOutcome> = alice
        .json(
            "POST",
            "/api/v1/files/commit",
            &json!({ "commits": [commit("a.txt"), commit("taken.txt"), commit("b.txt")] }),
        )
        .await
        .unwrap();
    assert!(outcomes[0].committed.is_some() && outcomes[2].committed.is_some());
    let refused = &outcomes[1];
    assert_eq!(
        (
            refused.error.as_ref().unwrap().code.as_str(),
            refused.status
        ),
        ("name_taken", Some(409))
    );
    assert_eq!(
        alice.children(docs.root).await,
        ["a.txt", "b.txt", "taken.txt"]
    );
    assert_eq!(alice.versions(taken.file).await.len(), 1);

    let too_many: Vec<Value> = (0..101).map(|i| commit(&format!("f{i}"))).collect();
    let refused = alice
        .json::<Vec<api::CommitOutcome>>(
            "POST",
            "/api/v1/files/commit",
            &json!({ "commits": too_many }),
        )
        .await
        .unwrap_err();
    assert_eq!(
        code(refused),
        (StatusCode::BAD_REQUEST, "batch_too_large".into())
    );
}

#[sqlx::test]
async fn download_lists(pool: PgPool) {
    let w = fs_world(pool).await;
    let alice = w.as_user(1).await;
    let files: Vec<Value> = (0..101).map(|_| json!({ "file": Uuid::nil() })).collect();
    let refused = alice
        .json::<Vec<api::DownloadOutcome>>(
            "POST",
            "/api/v1/files/download",
            &json!({ "files": files }),
        )
        .await
        .unwrap_err();
    assert_eq!(
        code(refused),
        (StatusCode::BAD_REQUEST, "batch_too_large".into())
    );
}

#[sqlx::test]
async fn folder_trees(pool: PgPool) {
    let w = fs_world(pool).await;
    let mut alice = w.as_user(1).await;
    let docs = alice.create_library("Docs").await;
    let made = alice
        .folders(docs.root, &["a", "a/b", "c/d"])
        .await
        .unwrap();
    let shown: Vec<(&str, bool)> = made.iter().map(|f| (f.path.as_str(), f.created)).collect();
    assert_eq!(shown, [("a", true), ("a/b", true), ("c/d", true)]);
    assert_eq!(alice.children(docs.root).await, ["a", "c"]);
    let again = alice.folders(docs.root, &["A/B", "c/d/e"]).await.unwrap();
    assert_eq!((again[0].id, again[0].created), (made[1].id, false));
    assert!(again[1].created);

    alice
        .upload(&docs, new_file(&docs, "f.txt"), &data(100, 1))
        .await
        .unwrap();
    let blocked = alice
        .folders(docs.root, &["new", "f.txt/x"])
        .await
        .unwrap_err();
    assert_eq!(code(blocked), (StatusCode::CONFLICT, "name_taken".into()));
    assert!(
        !alice.children(docs.root).await.contains(&"new".to_owned()),
        "one transaction: nothing created"
    );
    let invalid = alice.folders(docs.root, &["bad:name"]).await.unwrap_err();
    assert_eq!(code(invalid).0, StatusCode::BAD_REQUEST);
    let many: Vec<String> = (0..1001).map(|i| format!("f{i}")).collect();
    let many: Vec<&str> = many.iter().map(String::as_str).collect();
    assert_eq!(
        code(alice.folders(docs.root, &many).await.unwrap_err()),
        (StatusCode::BAD_REQUEST, "batch_too_large".into())
    );
}

#[sqlx::test]
async fn streamed_trees(pool: PgPool) {
    let w = fs_world(pool).await;
    let mut alice = w.as_user(1).await;
    let docs = alice.create_library("Docs").await;
    let made = alice
        .folders(docs.root, &["a/b", "a-c", "gone/inside"])
        .await
        .unwrap();
    let b = made[0].id;
    alice.file(&docs, b, "f.txt", &data(100, 1)).await;
    alice.file(&docs, docs.root, "top.txt", &data(100, 2)).await;
    let gone = alice.folders(docs.root, &["gone"]).await.unwrap()[0].id;
    alice.trash(gone).await;

    let lines = alice.tree(docs.root).await.unwrap();
    let (nodes, end) = lines.split_at(lines.len() - 1);
    let paths: Vec<String> = nodes
        .iter()
        .map(|l| match l {
            api::TreeLine::Node(n) => n.path.clone(),
            other => panic!("{other:?}"),
        })
        .collect();
    // Parents before their children; the trash left out.
    assert_eq!(paths, ["a", "a-c", "a/b", "a/b/f.txt", "top.txt"]);
    assert!(matches!(end[0], api::TreeLine::End { count: 5 }));
    let file = nodes.iter().find_map(|l| match l {
        api::TreeLine::Node(n) if n.path == "top.txt" => Some(n.clone()),
        _ => None,
    });
    let file = file.unwrap();
    assert!(file.is_file && file.current_version.is_some());

    // A subfolder's own tree; a file has none.
    let sub = alice.tree(made[0].id).await.unwrap();
    assert_eq!(sub.len(), 2, "f.txt and the end");
    assert_eq!(
        code(alice.tree(file.id).await.unwrap_err()),
        (StatusCode::BAD_REQUEST, "not_a_folder".into())
    );
}

/// Every response describing a file carries its size; folders have none.
#[sqlx::test]
async fn sizes(pool: PgPool) {
    let w = fs_world(pool).await;
    let mut alice = w.as_user(1).await;
    let docs = alice.create_library("Docs").await;
    let folder = alice.mkdir(docs.root, "folder").await;
    let first = alice
        .upload(
            &docs,
            json!({ "parent": folder, "name": "a.txt" }),
            &data(1000, 1),
        )
        .await
        .unwrap();
    alice
        .upload(
            &docs,
            json!({ "file": first.file, "base_version": first.version }),
            &data(300, 2),
        )
        .await
        .unwrap();

    let children: Vec<api::Node> = alice
        .get(&format!("/api/v1/nodes/{folder}/children"))
        .await
        .unwrap();
    assert_eq!(children[0].size, Some(300), "the current version's");
    let root: Vec<api::Node> = alice
        .get(&format!("/api/v1/nodes/{}/children", docs.root))
        .await
        .unwrap();
    assert_eq!(root[0].size, None, "a folder has none");
    assert_eq!(alice.info(folder).await.unwrap().size, None);
    assert_eq!(alice.size(first.file).await, 300);
    let sizes: Vec<Option<i64>> = alice
        .tree(docs.root)
        .await
        .unwrap()
        .into_iter()
        .filter_map(|line| match line {
            api::TreeLine::Node(n) => Some(n.size),
            _ => None,
        })
        .collect();
    assert_eq!(sizes, [None, Some(300)]);
    let versions: Vec<i64> = alice
        .versions(first.file)
        .await
        .iter()
        .map(|v| v.size)
        .collect();
    assert_eq!(versions, [1000, 300]);
    // Dates on the wire: RFC 3339 in UTC, whatever the database's time zone.
    let raw: Value = alice
        .get(&format!("/api/v1/files/{}/versions", first.file))
        .await
        .unwrap();
    let date = raw[0]["created_at"].as_str().unwrap();
    assert!(
        date.len() > 20 && date.as_bytes()[10] == b'T' && date.ends_with('Z'),
        "{date}"
    );
    let found: Vec<api::SearchResult> = alice.get("/api/v1/search?q=a.txt").await.unwrap();
    assert_eq!(found[0].size, Some(300));

    alice.trash(first.file).await;
    let trash: Vec<api::TrashEntry> = alice
        .get(&format!("/api/v1/libraries/{}/trash", docs.id))
        .await
        .unwrap();
    assert_eq!(trash[0].size, Some(300));
}
