//! The upload protocol end to end, through the router, on a real Postgres:
//! lookup, declaration and presigned upload, announce, commit, download. Plus
//! the dedup scope, authorization, sizes, quota and anomalies.
//!
//! Every scenario runs on both backends: the filesystem (server-signed URLs,
//! through the router) and S3 (Garage, presigned by S3, real HTTP). S3 needs
//! the `S3_TEST_*` variables, written by `scripts/dev.sh` and set by CI.

#![allow(dead_code)] // the shared harness has more than these tests use

include!("common/harness.rs");

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
    unchanged_content_creates_no_version,
    commit_keeps_the_client_modified_time,
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
    dedup_follows_what_the_uploader_can_access,
    no_dedup_with_what_the_uploader_cannot_access,
    narrowed_app_passwords_dedup_within_their_scope,
    authors_survive_leaving_and_go_with_their_account,
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
    purges_free_the_space_at_once,
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

async fn unchanged_content_creates_no_version(kind: Kind, pool: PgPool) {
    let w = world(kind, pool).await;
    let mut alice = w.as_user(1).await;
    let lib = alice.create_library("docs").await;
    let content = data(3 * MIB, 7);
    let v1 = alice
        .upload(&lib, new_file(&lib, "doc.bin"), &content)
        .await
        .unwrap();
    assert!(!v1.unchanged);
    let listed: Vec<api::Node> = alice
        .get(&format!("/api/v1/nodes/{}/children", lib.root))
        .await
        .unwrap();
    assert_eq!(
        listed[0].current_version,
        Some(v1.version),
        "listings carry it"
    );

    let on = |base: Uuid| json!({ "file": v1.file, "base_version": base, "message": "again" });
    let again = alice.upload(&lib, on(v1.version), &content).await.unwrap();
    assert!(again.unchanged);
    assert_eq!(again.version, v1.version, "the current version");
    let count = || async {
        sqlx::query_scalar::<_, i64>(
            "SELECT count(*) FROM version v JOIN node n ON n.pk = v.file WHERE n.uuid = $1",
        )
        .bind(v1.file)
        .fetch_one(&w.pool)
        .await
        .unwrap()
    };
    assert_eq!(count().await, 1);

    // From a base no longer current, the same content is a real version.
    let v2 = alice
        .upload(&lib, on(v1.version), &data(MIB, 8))
        .await
        .unwrap();
    let stale = alice.upload(&lib, on(v1.version), &content).await.unwrap();
    assert!(!stale.unchanged && stale.conflict);
    assert_ne!(stale.version, v2.version);
    assert_eq!(count().await, 3);
}

/// The commit's `modified` is the file's time everywhere; the same chunks with
/// another time update the current version (a touch) instead of creating one.
async fn commit_keeps_the_client_modified_time(kind: Kind, pool: PgPool) {
    let w = world(kind, pool).await;
    let mut alice = w.as_user(1).await;
    let lib = alice.create_library("docs").await;
    let content = data(MIB, 9);
    let mut target = new_file(&lib, "dated.bin");
    target["modified"] = json!("2020-01-02T03:04:05Z");
    let v1 = alice.upload(&lib, target, &content).await.unwrap();
    let at = |s: &str| s.parse::<chrono::DateTime<chrono::Utc>>().unwrap();
    let info: api::NodeInfo = alice
        .get(&format!("/api/v1/nodes/{}", v1.file))
        .await
        .unwrap();
    assert_eq!(info.modified, Some(at("2020-01-02T03:04:05Z")));

    let touch =
        json!({ "file": v1.file, "base_version": v1.version, "modified": "2021-06-07T08:09:10Z" });
    let touched = alice.upload(&lib, touch, &content).await.unwrap();
    assert!(touched.unchanged);
    let versions: Vec<api::Version> = alice
        .get(&format!("/api/v1/files/{}/versions", v1.file))
        .await
        .unwrap();
    assert_eq!(versions.len(), 1, "no new version");
    assert_eq!(versions[0].modified, at("2021-06-07T08:09:10Z"));
    assert!(
        versions[0].created_at > at("2026-01-01T00:00:00Z"),
        "the commit's own time"
    );

    let mut absurd = new_file(&lib, "absurd.bin");
    absurd["modified"] = json!("0000-06-01T00:00:00Z");
    let refused = alice.upload(&lib, absurd, &content).await.unwrap_err();
    assert_eq!(refused.1["code"], "invalid_modified");
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
    assert!(alice.commit(&commit).await.is_err());
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
    // Bob can't access a: only the group lets his library b dedup with it.
    let w = world(kind, pool).await;
    let mut alice = w.as_user(1).await;
    let mut bob = w.as_user(2).await;
    let a = alice.create_library("a").await;
    let b = bob.create_library("b").await;
    let outside = bob.create_library("outside").await;
    let ns = w.namespace(&[a.id, b.id]).await;
    let content = data(2 * MIB, 18);
    alice
        .upload(&a, new_file(&a, "f.bin"), &content)
        .await
        .unwrap();

    let before = bob.puts;
    bob.upload(&b, new_file(&b, "f.bin"), &content)
        .await
        .unwrap();
    assert_eq!(bob.puts, before, "b dedups with a (common namespace)");

    let only_in_a = data(2 * MIB, 23);
    alice
        .upload(&a, new_file(&a, "h.bin"), &only_in_a)
        .await
        .unwrap();
    let before = bob.puts;
    bob.upload(&outside, new_file(&outside, "h.bin"), &only_in_a)
        .await
        .unwrap();
    assert!(
        bob.puts > before,
        "outside isn't in the group, and bob can't access a"
    );

    // Removing the namespace: b's file is intact, new uploads don't see a anymore.
    let b_file = bob
        .get::<Vec<api::Node>>(&format!("/api/v1/nodes/{}/children", b.root))
        .await
        .unwrap()[0]
        .id;
    w.sql(&format!("DELETE FROM library_namespace WHERE namespace = (SELECT pk FROM namespace WHERE uuid = '{ns}')"))
        .await;
    assert_eq!(
        bob.download(b_file).await.unwrap(),
        content,
        "b keeps the chunks it borrowed"
    );
    let only_in_a = data(MIB, 19);
    alice
        .upload(&a, new_file(&a, "g.bin"), &only_in_a)
        .await
        .unwrap();
    let before = bob.puts;
    bob.upload(&b, new_file(&b, "g.bin"), &only_in_a)
        .await
        .unwrap();
    assert!(
        bob.puts > before,
        "after the removal, b uploads blocks only present in a"
    );
}

async fn dedup_follows_what_the_uploader_can_access(kind: Kind, pool: PgPool) {
    // No group anywhere: Alice's libraries dedup together because she can
    // access them all (ADR 0002), a trashed one included.
    let w = world(kind, pool).await;
    let mut alice = w.as_user(1).await;
    let a = alice.create_library("a").await;
    let b = alice.create_library("b").await;
    let content = data(2 * MIB, 24);
    alice
        .upload(&a, new_file(&a, "f.bin"), &content)
        .await
        .unwrap();
    let before = alice.puts;
    alice
        .upload(&b, new_file(&b, "f.bin"), &content)
        .await
        .unwrap();
    assert_eq!(alice.puts, before, "b dedups with a: alice can access both");

    let trashed = data(2 * MIB, 25);
    let in_a = alice
        .upload(&a, new_file(&a, "t.bin"), &trashed)
        .await
        .unwrap()
        .file;
    let ids = alice.file_chunks(in_a).await;
    w.sql(&format!(
        "UPDATE library SET deleted_at = now() WHERE uuid = '{}'",
        a.id
    ))
    .await;
    let before = alice.puts;
    alice
        .upload(&b, new_file(&b, "t.bin"), &trashed)
        .await
        .unwrap();
    assert_eq!(
        alice.puts, before,
        "a trashed library still counts: alice can restore and read it"
    );
    // The commit check agrees: the trashed library's chunk IDs are reusable.
    let commit = json!({ "library": b.id, "parent": b.root, "name": "by-id.bin", "chunks": ids });
    alice.commit(&commit).await.unwrap();
}

async fn no_dedup_with_what_the_uploader_cannot_access(kind: Kind, pool: PgPool) {
    let w = world(kind, pool).await;
    let mut alice = w.as_user(1).await;
    let mut bob = w.as_user(2).await;
    let a = alice.create_library("a").await;
    let b = bob.create_library("b").await;
    let content = data(2 * MIB, 26);
    alice
        .upload(&a, new_file(&a, "f.bin"), &content)
        .await
        .unwrap();
    bob.upload(&b, new_file(&b, "f.bin"), &content)
        .await
        .unwrap();
    assert!(bob.puts > 0, "bob can't access a: no oracle on it");

    // Read access is enough: bob could copy a's files by hand.
    w.add_member(a.id, 2, "read").await;
    let c = bob.create_library("c").await;
    let before = bob.puts;
    bob.upload(&c, new_file(&c, "f.bin"), &content)
        .await
        .unwrap();
    assert_eq!(bob.puts, before, "bob reads a now: c dedups with it");
}

async fn narrowed_app_passwords_dedup_within_their_scope(kind: Kind, pool: PgPool) {
    let w = world(kind, pool).await;
    let mut alice = w.as_user(1).await;
    let a = alice.create_library("a").await;
    let b = alice.create_library("b").await;
    let c = alice.create_library("c").await;
    let content = data(2 * MIB, 27);
    alice
        .upload(&a, new_file(&a, "f.bin"), &content)
        .await
        .unwrap();

    let in_a = alice
        .get::<Vec<api::Node>>(&format!("/api/v1/nodes/{}/children", a.root))
        .await
        .unwrap()[0]
        .id;
    let ids = alice.file_chunks(in_a).await;
    let mut only_b = alice.with_app_password(json!({ "library": b.id })).await;
    only_b
        .upload(&b, new_file(&b, "f.bin"), &content)
        .await
        .unwrap();
    assert!(
        only_b.puts > 0,
        "narrowed to b, it can't access a: its account's access doesn't count"
    );
    let commit = json!({ "library": b.id, "parent": b.root, "name": "by-id.bin", "chunks": ids });
    let (status, error) = only_b.commit(&commit).await.unwrap_err();
    assert_eq!(
        (status, error["code"].as_str()),
        (StatusCode::FORBIDDEN, Some("chunk_not_allowed")),
        "nor reuse a's chunk IDs at commit"
    );

    let org = w.organizations[0];
    let mut whole_org = alice
        .with_app_password(json!({ "organization": org }))
        .await;
    whole_org
        .upload(&c, new_file(&c, "f.bin"), &content)
        .await
        .unwrap();
    assert_eq!(
        whole_org.puts, 0,
        "narrowed to the organization: c dedups with a"
    );
    let commit = json!({ "library": c.id, "parent": c.root, "name": "by-id.bin", "chunks": ids });
    whole_org.commit(&commit).await.unwrap();
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

async fn authors_survive_leaving_and_go_with_their_account(kind: Kind, pool: PgPool) {
    // A version's author is the account, not the membership (sprint 06).
    let w = world(kind, pool).await;
    let alice = w.as_user(1).await;
    let mut bob = w.as_user(2).await;
    let a = alice.create_library("a").await;
    w.add_member(a.id, 2, "write").await;
    let file = bob
        .upload(&a, new_file(&a, "f.bin"), &data(MIB, 28))
        .await
        .unwrap()
        .file;
    let author = || async {
        let versions: Vec<api::Version> = alice
            .get(&format!("/api/v1/files/{file}/versions"))
            .await
            .unwrap();
        versions[0].author.clone()
    };
    assert_eq!(author().await.as_deref(), Some("b@x.ch"));

    let (org, user) = (w.organizations[0], w.users[1]);
    let (status, _) = alice
        .call(
            "DELETE",
            &format!("/api/v1/organizations/{org}/members/{user}"),
            None,
            true,
        )
        .await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    assert_eq!(
        author().await.as_deref(),
        Some("b@x.ch"),
        "the author survives leaving the organization"
    );

    let (status, _) = bob
        .call(
            "DELETE",
            "/api/v1/me",
            Some(json!({ "password": PASSWORD }).to_string().into_bytes()),
            true,
        )
        .await;
    assert_eq!(status, StatusCode::NO_CONTENT, "bob deletes his account");
    assert_eq!(author().await, None, "and with it, the trace");
}

async fn commit_refuses_chunks_outside_the_dedup_scope(kind: Kind, pool: PgPool) {
    let w = world(kind, pool).await;
    let mut alice = w.as_user(1).await;
    let bob = w.as_user(2).await;
    let secret = alice.create_library("secret").await;
    let other = bob.create_library("other").await;
    let anomalies = Anomalies::capture();
    let file = alice
        .upload(&secret, new_file(&secret, "s.bin"), &data(MIB, 22))
        .await
        .unwrap()
        .file;
    let stolen = alice.file_chunks(file).await;
    let commit =
        json!({ "library": other.id, "parent": other.root, "name": "x.bin", "chunks": stolen });
    let (status, error) = bob.commit(&commit).await.unwrap_err();
    assert_eq!(
        (status, error["code"].as_str()),
        (StatusCode::FORBIDDEN, Some("chunk_not_allowed"))
    );
    assert!(anomalies.logged("commit_chunk_not_allowed", bob.account));

    // The same scope as the lookup's: once bob reads secret, he may reuse them.
    w.add_member(secret.id, 2, "read").await;
    bob.commit(&commit).await.unwrap();
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
    let (status, error) = alice.commit(&commit).await.unwrap_err();
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
        // Every file, wherever it is: each must be a known chunk at its key.
        for dir in &w._dirs {
            let mut todo = vec![dir.path().to_owned()];
            while let Some(folder) = todo.pop() {
                for entry in std::fs::read_dir(folder).unwrap() {
                    let path = entry.unwrap().path();
                    if path.is_dir() {
                        todo.push(path);
                        continue;
                    }
                    let name = path.strip_prefix(dir.path()).unwrap().to_string_lossy();
                    let known = rows
                        .iter()
                        .find(|(chunk, _, _)| theca_core::storage::key(*chunk).as_ref() == name);
                    let (_, _, size) =
                        known.unwrap_or_else(|| panic!("object {name} has no chunk row"));
                    assert_eq!(Some(path.metadata().unwrap().len() as i64), *size);
                }
            }
        }
    }
}

/// Emptying the trash gives the space back at once, not after the GC's grace
/// (the chunks were just uploaded: well inside it). A chunk another file still
/// uses stays. A chunk whose upload URL is still valid is marked and stays
/// counted, its object kept until the URL expires (it could write it again).
async fn purges_free_the_space_at_once(kind: Kind, pool: PgPool) {
    let w = world(kind, pool).await;
    let mut alice = w.as_user(1).await;
    let lib = alice.create_library("docs").await;
    let reader = w.as_user(1).await;
    let used = || async {
        let org: api::Organization = reader
            .get(&format!("/api/v1/organizations/{}", lib.organization))
            .await
            .unwrap();
        org.used
    };
    let pool = &w.pool;
    let count = |sql: &'static str| async move {
        sqlx::query_scalar::<_, i64>(sql)
            .fetch_one(pool)
            .await
            .unwrap()
    };
    // Waits for the GC the purge started (in the background): up to 10 s.
    let settled = |expected: i64| async move {
        for _ in 0..100 {
            if used().await == expected {
                return;
            }
            tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        }
        panic!("the usage stayed at {}, not {expected}", used().await);
    };
    // As uploaded a while ago (URLs expired), still inside the grace.
    let urls_expire = || w.sql("UPDATE chunk SET url_expires_at = now() - interval '1 second'");

    let kept = alice
        .upload(&lib, new_file(&lib, "kept.bin"), &data(3 * MIB, 41))
        .await
        .unwrap();
    let (kept_used, kept_chunks) = (used().await, count("SELECT count(*) FROM chunk").await);
    let gone = alice
        .upload(&lib, new_file(&lib, "gone.bin"), &data(3 * MIB, 42))
        .await
        .unwrap();
    // The same content as kept.bin: its chunks, shared.
    let twin = alice
        .upload(&lib, new_file(&lib, "twin.bin"), &data(3 * MIB, 41))
        .await
        .unwrap();
    assert!(used().await > kept_used);
    urls_expire().await;
    for file in [gone.file, twin.file] {
        assert_eq!(alice.trash_node(file, false).await, StatusCode::NO_CONTENT);
        assert_eq!(alice.trash_node(file, true).await, StatusCode::NO_CONTENT);
    }
    settled(kept_used).await;
    assert_eq!(
        count("SELECT count(*) FROM chunk").await,
        kept_chunks,
        "gone.bin's chunks deleted, kept.bin's not"
    );
    assert_eq!(alice.download(kept.file).await.unwrap(), data(3 * MIB, 41));

    // Just uploaded (its URLs still valid): marked, still counted, kept.
    let fresh = alice
        .upload(&lib, new_file(&lib, "fresh.bin"), &data(2 * MIB, 43))
        .await
        .unwrap();
    let with_fresh = used().await;
    assert_eq!(
        alice.trash_node(fresh.file, false).await,
        StatusCode::NO_CONTENT
    );
    assert_eq!(
        alice.trash_node(fresh.file, true).await,
        StatusCode::NO_CONTENT
    );
    tokio::time::sleep(std::time::Duration::from_millis(500)).await;
    assert!(count("SELECT count(*) FROM chunk WHERE deleting_at IS NOT NULL").await > 0);
    assert_eq!(
        used().await,
        with_fresh,
        "counted until its object is deleted"
    );
    // Once the URLs expire, the GC (woken then) deletes them.
    urls_expire().await;
    w.sql("UPDATE account SET instance_admin = true WHERE pk = 1")
        .await;
    let status = alice
        .call(
            "POST",
            "/api/v1/admin/jobs/chunk_gc/run",
            Some(b"{}".to_vec()),
            true,
        )
        .await
        .0;
    assert!(status.is_success(), "{status}");
    settled(kept_used).await;

    // A whole library, from the trash.
    let (status, _) = alice
        .call(
            "DELETE",
            &format!("/api/v1/libraries/{}", lib.id),
            None,
            true,
        )
        .await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    let (status, _) = alice
        .call(
            "DELETE",
            &format!("/api/v1/libraries/{}?permanent=true", lib.id),
            None,
            true,
        )
        .await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    settled(0).await;
}
