//! Background jobs: each job called directly, the runner's supervision, and
//! the admin API (filesystem backend).

#![allow(dead_code, unused_imports)] // the shared harness has more than these tests use

include!("common/harness.rs");

use std::time::Duration;

use theca_core::jobs::{self, Job};

async fn fs_world(pool: PgPool) -> World {
    world(Kind::Filesystem, pool).await
}

impl World {
    async fn run(&self, job: Job) -> i64 {
        job.run(&state(&self.pool).await).await.unwrap()
    }

    async fn rows(&self, sql: &str) -> i64 {
        sqlx::query_scalar(sql).fetch_one(&self.pool).await.unwrap()
    }

    async fn stored(&self, chunk: Uuid) -> bool {
        let backend = state(&self.pool).await.backends.get(1).await.unwrap();
        backend.size(chunk).await.unwrap().is_some()
    }

    async fn exists(&self, chunk: Uuid) -> bool {
        sqlx::query_scalar::<_, bool>("SELECT EXISTS (SELECT 1 FROM chunk WHERE uuid = $1)")
            .bind(chunk)
            .fetch_one(&self.pool)
            .await
            .unwrap()
    }
}

#[sqlx::test]
async fn chunk_gc(pool: PgPool) {
    let w = fs_world(pool).await;
    let mut alice = w.as_user(1).await;
    let docs = alice.create_library("Docs").await;
    let kept_file = alice
        .upload(&docs, new_file(&docs, "kept.txt"), &data(1000, 1))
        .await
        .unwrap()
        .file;
    let kept = alice.file_chunks(kept_file).await[0];
    let gone = alice
        .upload(&docs, new_file(&docs, "gone.txt"), &data(1000, 2))
        .await
        .unwrap();
    let gone_chunk = alice.file_chunks(gone.file).await[0];
    let abandoned = alice.looked_up(docs.id, &data(1000, 3)).await.declare.chunk;
    let (in_flight, _) = alice.pending(docs.id, &data(1000, 4)).await;
    let in_flight = in_flight.declare.chunk;
    // gone.txt deleted for good: its chunk is unreferenced, its object stored.
    alice.trash_node(gone.file, false).await;
    assert_eq!(
        alice.trash_node(gone.file, true).await,
        StatusCode::NO_CONTENT
    );

    // Within the grace period: nothing goes.
    assert_eq!(w.run(Job::ChunkGc).await, 0);

    // Past it: unreferenced chunks go, unless an upload URL for them is still
    // valid (it could still write the object): never referenced ones.
    w.sql("UPDATE chunk SET touched_at = now() - interval '2 days'")
        .await;
    assert_eq!(w.run(Job::ChunkGc).await, 1);
    assert!(!w.exists(abandoned).await, "never declared: no URL");
    assert!(
        w.exists(in_flight).await && w.exists(gone_chunk).await,
        "URLs still valid"
    );

    // Once the URLs expired, objects first, then rows.
    w.sql("UPDATE chunk SET url_expires_at = now() - interval '1 minute' WHERE url_expires_at IS NOT NULL").await;
    assert!(w.stored(gone_chunk).await);
    assert_eq!(w.run(Job::ChunkGc).await, 2);
    assert!(!w.exists(gone_chunk).await && !w.stored(gone_chunk).await);
    assert!(!w.exists(in_flight).await);
    assert!(w.exists(kept).await && w.stored(kept).await);
    assert_eq!(alice.download(kept_file).await.unwrap(), data(1000, 1));

    // An interrupted run (tombstone, object still there) is finished by the next.
    let tomb = alice
        .upload(&docs, new_file(&docs, "tomb.txt"), &data(1000, 5))
        .await
        .unwrap();
    let tomb_chunk = alice.file_chunks(tomb.file).await[0];
    w.sql(&format!(
        "DELETE FROM chunk_in_version WHERE chunk = (SELECT pk FROM chunk WHERE uuid = '{tomb_chunk}');
         UPDATE node SET current_version = NULL WHERE uuid = '{0}';
         DELETE FROM version WHERE uuid = '{1}';
         DELETE FROM node WHERE uuid = '{0}';
         UPDATE chunk SET deleting_at = now(), url_expires_at = now() - interval '1 minute'
           WHERE uuid = '{tomb_chunk}'",
        tomb.file, tomb.version
    ))
    .await;
    assert_eq!(w.run(Job::ChunkGc).await, 1);
    assert!(!w.exists(tomb_chunk).await && !w.stored(tomb_chunk).await);
}

/// The GC can't delete what a commit references, and a commit can't reference
/// what the GC marked: the schema enforces both, whatever the timing.
#[sqlx::test]
async fn gc_and_commits_cannot_race(pool: PgPool) {
    let w = fs_world(pool).await;
    let mut alice = w.as_user(1).await;
    let docs = alice.create_library("Docs").await;
    let file = alice
        .upload(&docs, new_file(&docs, "a.txt"), &data(1000, 1))
        .await
        .unwrap();
    let referenced = alice.file_chunks(file.file).await[0];
    let marking = sqlx::query("UPDATE chunk SET deleting_at = now() WHERE uuid = $1")
        .bind(referenced)
        .execute(&w.pool)
        .await
        .unwrap_err();
    assert_eq!(
        marking.as_database_error().unwrap().constraint(),
        Some("chunk_in_version_chunk")
    );

    // A marked chunk can't be committed (the client uploads it again).
    let other = alice
        .upload(&docs, new_file(&docs, "b.txt"), &data(1000, 2))
        .await
        .unwrap();
    let marked = alice.file_chunks(other.file).await[0];
    w.sql(&format!(
        "DELETE FROM chunk_in_version WHERE chunk = (SELECT pk FROM chunk WHERE uuid = '{marked}');
         UPDATE chunk SET deleting_at = now() WHERE uuid = '{marked}'"
    ))
    .await;
    let commit =
        json!({ "library": docs.id, "parent": docs.root, "name": "c.txt", "chunks": [marked] });
    let refused = alice.commit(&commit).await.unwrap_err();
    assert_eq!(refused.1["code"], "chunk_missing");
}

#[sqlx::test]
async fn trash_purge(pool: PgPool) {
    let w = fs_world(pool).await;
    let mut alice = w.as_user(1).await;
    let docs = alice.create_library("Docs").await;
    let file = alice
        .upload(&docs, new_file(&docs, "a.txt"), &data(1000, 1))
        .await
        .unwrap();
    alice.trash_node(file.file, false).await;

    // 30 days by default.
    assert_eq!(w.run(Job::TrashPurge).await, 0);
    w.sql("UPDATE node SET deleted_at = now() - interval '31 days' WHERE deleted_at IS NOT NULL")
        .await;
    assert_eq!(w.run(Job::TrashPurge).await, 1);
    assert_eq!(w.rows("SELECT count(*) FROM version").await, 0);
    assert_eq!(
        w.rows("SELECT count(*) FROM chunk").await,
        1,
        "left to the GC"
    );

    // 0: at the next run.
    w.sql("INSERT INTO setting (key, value) VALUES ('trash_retention_secs', '0')")
        .await;
    let other = alice
        .upload(&docs, new_file(&docs, "b.txt"), &data(1000, 2))
        .await
        .unwrap();
    alice.trash_node(other.file, false).await;
    assert_eq!(w.run(Job::TrashPurge).await, 1);

    // A trashed library, whole.
    let photos = alice.create_library("Photos").await;
    alice
        .upload(&photos, new_file(&photos, "c.txt"), &data(1000, 3))
        .await
        .unwrap();
    alice
        .call(
            "DELETE",
            &format!("/api/v1/libraries/{}", photos.id),
            None,
            true,
        )
        .await;
    assert_eq!(w.run(Job::TrashPurge).await, 1);
    assert_eq!(
        w.rows("SELECT count(*) FROM library WHERE deleted_at IS NOT NULL")
            .await,
        0
    );
}

#[sqlx::test]
async fn cleanup(pool: PgPool) {
    let w = fs_world(pool).await;
    let _alive = w.as_user(1).await;
    let _expired = w.as_user(2).await;
    let _revoked = w.as_user(3).await;
    w.sql(
        "UPDATE token SET last_seen = now() - interval '8 days'
           WHERE resource = (SELECT uuid FROM account WHERE pk = 2);
         UPDATE token SET revoked_at = now(), revocation_reason = 'logout'
           WHERE resource = (SELECT uuid FROM account WHERE pk = 3);
         INSERT INTO job_run (job, started_at, finished_at, status)
           VALUES ('cleanup', now() - interval '40 days', now() - interval '40 days', 'success'),
                  ('cleanup', now() - interval '1 day', now() - interval '1 day', 'success')",
    )
    .await;
    assert_eq!(w.run(Job::Cleanup).await, 3);
    assert_eq!(w.rows("SELECT count(*) FROM token").await, 1);
    assert_eq!(w.rows("SELECT count(*) FROM job_run").await, 1);
}

#[sqlx::test]
async fn orphan_check(pool: PgPool) {
    let anomalies = Anomalies::capture();
    let w = fs_world(pool).await;
    let mut alice = w.as_user(1).await;
    let docs = alice.create_library("Docs").await;
    let file = alice
        .upload(&docs, new_file(&docs, "a.txt"), &data(1000, 1))
        .await
        .unwrap();
    let real = alice.file_chunks(file.file).await[0];
    let dir = w._dirs[0].path().to_owned();
    let backend = state(&w.pool).await.backends.get(1).await.unwrap();
    let (old, young) = (Uuid::now_v7(), Uuid::now_v7());
    backend.put(old, b"leaked".to_vec()).await.unwrap();
    backend.put(young, b"in flight".to_vec()).await.unwrap();
    // Not chunks: outside `chunks/`, and inside it under a name no chunk has.
    let stray = dir.join("chunks/00/00/notes.txt");
    std::fs::create_dir_all(stray.parent().unwrap()).unwrap();
    std::fs::write(&stray, "not a chunk").unwrap();
    std::fs::write(dir.join("notes.txt"), "not a chunk").unwrap();
    let object = |chunk: Uuid| dir.join(theca_core::storage::key(chunk).as_ref());
    let two_days_ago = std::time::SystemTime::now() - Duration::from_secs(2 * 86400);
    for path in [
        object(old),
        object(real),
        stray.clone(),
        dir.join("notes.txt"),
    ] {
        let f = std::fs::File::options().write(true).open(path).unwrap();
        f.set_modified(two_days_ago).unwrap();
    }

    assert_eq!(w.run(Job::OrphanCheck).await, 1);
    assert!(!w.stored(old).await);
    assert!(anomalies.logged("orphan_object", old));
    assert!(
        w.stored(young).await,
        "a young object may be an upload in flight"
    );
    assert!(w.stored(real).await);
    assert!(dir.join("notes.txt").exists());
    assert!(stray.exists());
}

#[sqlx::test]
async fn runner(pool: PgPool) {
    let status = |pool: PgPool, run: Uuid| async move {
        sqlx::query_as::<_, (String, Option<String>)>(
            "SELECT status, error FROM job_run WHERE uuid = $1",
        )
        .bind(run)
        .fetch_one(&pool)
        .await
        .unwrap()
    };
    let job = Job::Cleanup;

    // Success, failure, panic, timeout: all recorded.
    let run = jobs::start(&pool, job).await.unwrap().unwrap();
    assert!(
        jobs::start(&pool, job).await.unwrap().is_none(),
        "already running"
    );
    jobs::supervise(&pool, "cleanup", run, Duration::from_secs(5), async {
        Ok(3)
    })
    .await;
    assert_eq!(status(pool.clone(), run.1).await, ("success".into(), None));
    let run = jobs::start(&pool, job).await.unwrap().unwrap();
    jobs::supervise(&pool, "cleanup", run, Duration::from_secs(5), async {
        panic!("boom")
    })
    .await;
    assert_eq!(
        status(pool.clone(), run.1).await,
        ("failure".into(), Some("panicked: boom".into()))
    );
    let run = jobs::start(&pool, job).await.unwrap().unwrap();
    let slow = async {
        tokio::time::sleep(Duration::from_secs(60)).await;
        Ok(0)
    };
    jobs::supervise(&pool, "cleanup", run, Duration::from_millis(50), slow).await;
    assert!(
        status(pool.clone(), run.1)
            .await
            .1
            .unwrap()
            .starts_with("timed out")
    );

    // A run left running by a crash is interrupted at startup, and doesn't
    // count for the next wait (it resumes at once); finished runs do.
    let run = jobs::start(&pool, Job::ChunkGc).await.unwrap().unwrap();
    assert_eq!(jobs::mark_interrupted(&pool).await.unwrap(), 1);
    assert_eq!(status(pool.clone(), run.1).await.0, "interrupted");
    assert_eq!(
        jobs::next_wait(&pool, Job::ChunkGc, std::time::Duration::from_secs(3600))
            .await
            .unwrap(),
        Duration::ZERO
    );
    sqlx::query("INSERT INTO job_run (job, started_at, finished_at, status) VALUES ('chunk_gc', now() - interval '10 minutes', now(), 'success')")
        .execute(&pool)
        .await
        .unwrap();
    let wait = jobs::next_wait(&pool, Job::ChunkGc, std::time::Duration::from_secs(3600))
        .await
        .unwrap();
    assert!((49 * 60..=50 * 60).contains(&wait.as_secs()), "{wait:?}");

    // Three failures in a row: reported by the health check.
    assert!(jobs::failing(&pool).await.unwrap().is_empty());
    for _ in 0..2 {
        let run = jobs::start(&pool, job).await.unwrap().unwrap();
        jobs::supervise(&pool, "cleanup", run, Duration::from_secs(5), async {
            panic!("again")
        })
        .await;
    }
    assert_eq!(jobs::failing(&pool).await.unwrap(), ["cleanup"]);
}

#[sqlx::test]
async fn admin_api(pool: PgPool) {
    let w = fs_world(pool).await;
    let alice = w.as_user(1).await;
    let refused = alice.get::<Value>("/api/v1/admin/jobs").await.unwrap_err();
    assert_eq!(refused.1["code"], "admin_required");
    w.sql("UPDATE account SET instance_admin = true WHERE pk = 1")
        .await;

    let listed: Vec<api::JobStatus> = alice.get("/api/v1/admin/jobs").await.unwrap();
    let names: Vec<&str> = listed.iter().map(|j| j.job.as_str()).collect();
    assert_eq!(
        names,
        [
            "chunk_gc",
            "trash_purge",
            "cleanup",
            "orphan_check",
            "email_send"
        ]
    );

    let run: api::JobRun = alice
        .json("POST", "/api/v1/admin/jobs/cleanup/run", &json!({}))
        .await
        .unwrap();
    assert_eq!(run.status, "running");
    let mut finished = None;
    for _ in 0..100 {
        let listed: Vec<api::JobStatus> = alice.get("/api/v1/admin/jobs").await.unwrap();
        let last = listed[2].runs[0].clone();
        if last.status != "running" {
            finished = Some(last);
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    let finished = finished.expect("the run finishes");
    assert_eq!((finished.id, finished.status.as_str()), (run.id, "success"));
    let unknown = alice
        .json::<Value>("POST", "/api/v1/admin/jobs/nope/run", &json!({}))
        .await
        .unwrap_err();
    assert_eq!(unknown.0, StatusCode::NOT_FOUND);
}
