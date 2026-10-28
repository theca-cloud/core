//! End to end: the real `theca-server` binary as a process (environment
//! configuration, migrations at startup, a real port), driven through
//! `theca-client` like any client: setup, login, upload, download, an edit
//! that sends only new chunks, a cancelled upload resumed. On the filesystem
//! and on S3 (Garage).

include!("common/server.rs");

use futures::TryStreamExt;
use serde_json::json;
use sqlx::PgPool;
use theca_client::{
    Cancel, Client, DownloadOptions, Downloader, Error, FileSink, MemorySink, MemorySource, Target,
    UploadOptions, UploadProgress, Uploader, download, place, stream, upload,
};
use theca_common::api;
use theca_common::name::Name;

/// Starts `theca-server` on the test's database, with an open setup.
async fn start(pool: &PgPool) -> Server {
    let database = pool
        .connect_options()
        .get_database()
        .expect("a test database")
        .to_owned();
    start_server(&database_url(&database), tempfile::tempdir().unwrap(), true).await
}

/// Deterministic pseudo-random bytes (incompressible, distinct chunks).
fn data(len: usize, seed: u64) -> Vec<u8> {
    let mut s = seed * 2 + 1;
    (0..len)
        .map(|_| {
            s ^= s << 13;
            s ^= s >> 7;
            s ^= s << 17;
            s as u8
        })
        .collect()
}

async fn fetch(client: &Client, file: uuid::Uuid, version: Option<uuid::Uuid>) -> Vec<u8> {
    let out = MemorySink::default();
    let options = DownloadOptions::default();
    download(
        client,
        file,
        version,
        &out,
        &options,
        |_| {},
        &Cancel::default(),
    )
    .await
    .unwrap();
    out.0.into_inner().unwrap()
}

async fn scenario(pool: PgPool, storage: serde_json::Value) {
    let server = start(&pool).await;
    let mut client = Client::new(&server.url).unwrap();
    client.check_version().await.unwrap();
    let setup: api::Setup = serde_json::from_value(json!({
        "admin_email": "admin@test.theca",
        "admin_password": "a long password",
        "organization": "Acme",
        "storages": [storage],
    }))
    .unwrap();
    let me = client.setup(&setup, None).await.unwrap();
    let library = client
        .create_library(me.organizations[0].organization, "Docs")
        .await
        .unwrap();

    // A multi-chunk file, up and down.
    let original = data(5 * 1024 * 1024, 1);
    let target = Target::New {
        library: library.id,
        parent: library.root,
        name: Name::new("big.bin").unwrap(),
    };
    let options = UploadOptions::default();
    let committed = upload(
        &client,
        &MemorySource(original.clone()),
        target,
        &options,
        |_| {},
        &Cancel::default(),
    )
    .await
    .unwrap();
    assert_eq!(fetch(&client, committed.file, None).await, original);

    // The source's modification time reaches the file.
    let when = chrono::DateTime::from_timestamp(1_600_000_000, 0).unwrap();
    let dated = upload(
        &client,
        &Dated(MemorySource(b"dated".to_vec()), when),
        Target::New {
            library: library.id,
            parent: library.root,
            name: Name::new("dated.txt").unwrap(),
        },
        &options,
        |_| {},
        &Cancel::default(),
    )
    .await
    .unwrap();
    assert_eq!(client.node(dated.file).await.unwrap().modified, Some(when));

    // An edit at the end: only the new chunks travel.
    let mut edited = original.clone();
    let end = edited.len();
    edited[end - 1000..].copy_from_slice(&data(1000, 2));
    let mut last = Default::default();
    let target = Target::Version {
        library: library.id,
        file: committed.file,
        base: Some(committed.version),
    };
    let v2 = upload(
        &client,
        &MemorySource(edited.clone()),
        target,
        &options,
        |p| last = p.clone(),
        &Cancel::default(),
    )
    .await
    .unwrap();
    assert!(!v2.conflict);
    assert!(last.skipped > last.uploaded, "deduplicated: {last:?}");
    assert_eq!(fetch(&client, committed.file, None).await, edited);
    assert_eq!(
        fetch(&client, committed.file, Some(committed.version)).await,
        original
    );

    let small = UploadOptions {
        parallel: 1,
        ..Default::default()
    };
    let third = data(6 * 1024 * 1024, 3);
    let target = |name: &str| Target::New {
        library: library.id,
        parent: library.root,
        name: Name::new(name).unwrap(),
    };

    // Interrupted after its first chunks (a crash, a lost connection: the
    // upload is just dropped), then resumed: what was sent isn't sent again.
    let (sent, mut first_batch) = futures::channel::mpsc::unbounded::<()>();
    let (source, never) = (MemorySource(third.clone()), Cancel::default());
    let interrupted = upload(
        &client,
        &source,
        target("third.bin"),
        &small,
        |p| {
            if p.uploaded > 0 {
                let _ = sent.unbounded_send(());
            }
        },
        &never,
    );
    tokio::select! {
        result = interrupted => panic!("finished before the interruption: {result:?}"),
        _ = futures::StreamExt::next(&mut first_batch) => {}
    }
    let mut resumed = Default::default();
    let done = upload(
        &client,
        &MemorySource(third.clone()),
        target("third.bin"),
        &small,
        |p| resumed = p.clone(),
        &Cancel::default(),
    )
    .await
    .unwrap();
    assert!(
        resumed.skipped > 0,
        "the chunks sent before the interruption are reused: {resumed:?}"
    );
    assert_eq!(fetch(&client, done.file, None).await, third);

    // Cancelled: the upload is abandoned and its chunks freed on the server, so
    // a new attempt sends everything again.
    let fourth = data(6 * 1024 * 1024, 4);
    let cancel = Cancel::default();
    let stopped = upload(
        &client,
        &MemorySource(fourth.clone()),
        target("fourth.bin"),
        &small,
        |p| {
            if p.uploaded > 0 {
                cancel.cancel();
            }
        },
        &cancel,
    )
    .await;
    assert!(
        matches!(stopped, Err(theca_client::Error::Cancelled)),
        "{stopped:?}"
    );
    let mut again = Default::default();
    upload(
        &client,
        &MemorySource(fourth),
        target("fourth.bin"),
        &small,
        |p| again = p.clone(),
        &Cancel::default(),
    )
    .await
    .unwrap();
    assert_eq!(again.skipped, 0, "cancelled chunks were freed: {again:?}");

    // A plain login works too.
    let mut again = Client::new(&server.url).unwrap();
    let me = again
        .login("admin@test.theca", "a long password", None)
        .await
        .unwrap();
    assert_eq!(me.email, "admin@test.theca");
    let listed = again.children(library.root).await.unwrap();
    assert_eq!(listed.len(), 4);

    // Logging out revokes that session.
    again.logout().await.unwrap();
    assert_eq!(
        again.me().await.unwrap_err().code(),
        Some("unauthenticated")
    );
}

#[sqlx::test]
async fn filesystem(pool: PgPool) {
    let dir = tempfile::tempdir().unwrap();
    scenario(
        pool,
        json!({ "name": "main", "implementation": "filesystem", "path": dir.path() }),
    )
    .await;
}

#[sqlx::test]
async fn s3(pool: PgPool) {
    dotenvy::dotenv().ok();
    let var =
        |name: &str| std::env::var(name).unwrap_or_else(|_| panic!("{name}: run scripts/dev.sh"));
    scenario(
        pool,
        json!({
            "name": "main", "implementation": "s3",
            "host": var("S3_TEST_HOST"), "region": var("S3_TEST_REGION"), "bucket": var("S3_TEST_BUCKET"),
            "access_key_file": var("S3_TEST_ACCESS_KEY_FILE"), "secret_key_file": var("S3_TEST_SECRET_KEY_FILE"),
        }),
    )
    .await;
}

/// Bytes with a modification time, as a file on disk has one.
struct Dated(MemorySource, chrono::DateTime<chrono::Utc>);

impl theca_client::Source for Dated {
    fn len(&self) -> u64 {
        theca_client::Source::len(&self.0)
    }

    fn read(
        &self,
        offset: u64,
        len: usize,
    ) -> std::pin::Pin<Box<dyn Future<Output = std::io::Result<Vec<u8>>> + Send + '_>> {
        self.0.read(offset, len)
    }

    fn modified(&self) -> Option<chrono::DateTime<chrono::Utc>> {
        Some(self.1)
    }
}

/// A source that pauses when the upload pass reads its last chunk (the hash
/// pass reads in blocks, never at a chunk's offset), until released.
struct Gated {
    inner: MemorySource,
    last: u64,
    reads: std::sync::Mutex<u32>,
    reached: tokio::sync::Notify,
    release: tokio::sync::Notify,
}

impl theca_client::Source for Gated {
    fn len(&self) -> u64 {
        theca_client::Source::len(&self.inner)
    }

    fn read(
        &self,
        offset: u64,
        len: usize,
    ) -> std::pin::Pin<Box<dyn Future<Output = std::io::Result<Vec<u8>>> + Send + '_>> {
        Box::pin(async move {
            let first = offset == self.last && {
                let mut reads = self.reads.lock().unwrap();
                *reads += 1;
                *reads == 1
            };
            if first {
                self.reached.notify_one();
                self.release.notified().await;
            }
            self.inner.read(offset, len).await
        })
    }
}

/// A chunk lost between its upload and the commit (here cancelled; the GC or
/// a parallel cancel in real life): the commit answers `chunk_missing`, and
/// the engine uploads it again and commits.
#[sqlx::test]
async fn commit_recovers_lost_chunks(pool: PgPool) {
    let server = start(&pool).await;
    let dir = tempfile::tempdir().unwrap();
    let mut client = Client::new(&server.url).unwrap();
    let setup: api::Setup = serde_json::from_value(json!({
        "admin_email": "admin@test.theca", "admin_password": "a long password", "organization": "Acme",
        "storages": [{ "name": "main", "implementation": "filesystem", "path": dir.path() }],
    }))
    .unwrap();
    let me = client.setup(&setup, None).await.unwrap();
    let library = client
        .create_library(me.organizations[0].organization, "Docs")
        .await
        .unwrap();

    let bytes = data(6 * 1024 * 1024, 5);
    let last = theca_common::chunking::chunks(&bytes[..])
        .last()
        .unwrap()
        .unwrap()
        .offset;
    let source = Gated {
        inner: MemorySource(bytes.clone()),
        last,
        reads: Default::default(),
        reached: Default::default(),
        release: Default::default(),
    };
    let target = Target::New {
        library: library.id,
        parent: library.root,
        name: Name::new("lost.bin").unwrap(),
    };
    // One chunk at a time: the first is announced before the last is read.
    let options = UploadOptions {
        parallel: 1,
        window: Some(1),
        ..Default::default()
    };
    let never = Cancel::default();
    let uploading = upload(&client, &source, target, &options, |_| {}, &never);
    let interfere = async {
        source.reached.notified().await;
        // The first chunk is uploaded, not committed yet: cancel it.
        let first: uuid::Uuid = sqlx::query_scalar(
            "SELECT uuid FROM chunk WHERE uploaded_at IS NOT NULL AND deleting_at IS NULL ORDER BY pk LIMIT 1",
        )
        .fetch_one(&pool)
        .await
        .unwrap();
        client.cancel(&[first]).await.unwrap();
        source.release.notify_one();
    };
    // A timeout: a bug here must fail, not hang the suite.
    let (committed, ()) = tokio::time::timeout(Duration::from_secs(60), async {
        tokio::join!(uploading, interfere)
    })
    .await
    .expect("the upload and the interference finish");
    let committed = committed.unwrap();
    assert_eq!(fetch(&client, committed.file, None).await, bytes);
}

/// Many files through one engine: each committed (or failing on its own),
/// identical content uploaded once, the results each file's own.
#[sqlx::test]
async fn many_files_at_once(pool: PgPool) {
    let server = start(&pool).await;
    let dir = tempfile::tempdir().unwrap();
    let mut client = Client::new(&server.url).unwrap();
    let setup: api::Setup = serde_json::from_value(json!({
        "admin_email": "admin@test.theca", "admin_password": "a long password", "organization": "Acme",
        "storages": [{ "name": "main", "implementation": "filesystem", "path": dir.path() }],
    }))
    .unwrap();
    let me = client.setup(&setup, None).await.unwrap();
    let library = client
        .create_library(me.organizations[0].organization, "Docs")
        .await
        .unwrap();
    let target = |name: &str| Target::New {
        library: library.id,
        parent: library.root,
        name: Name::new(name).unwrap(),
    };
    // A name taken before the batch: that file alone fails.
    upload(
        &client,
        &MemorySource(b"first".to_vec()),
        target("taken.txt"),
        &UploadOptions::default(),
        |_| {},
        &Cancel::default(),
    )
    .await
    .unwrap();

    let mut sources: Vec<(String, MemorySource)> = (0..30)
        .map(|i| {
            (
                format!("small-{i}.bin"),
                MemorySource(data(10_000 + i, i as u64 + 10)),
            )
        })
        .collect();
    let big = data(5 * 1024 * 1024, 7);
    sources.push(("big.bin".into(), MemorySource(big.clone())));
    sources.push(("big-copy.bin".into(), MemorySource(big.clone())));
    sources.push(("empty.txt".into(), MemorySource(Vec::new())));
    sources.push(("taken.txt".into(), MemorySource(b"second".to_vec())));

    let options = UploadOptions::default();
    let cancel = Cancel::default();
    let mut last = Default::default();
    let (uploader, run) = Uploader::new(&client, &options, &cancel, |p: &UploadProgress| {
        last = p.clone()
    });
    let results: Vec<_> = sources
        .iter()
        .map(|(name, source)| uploader.submit(source, target(name)))
        .collect();
    drop(uploader);
    let ((), results) = tokio::time::timeout(Duration::from_secs(60), async {
        tokio::join!(run, futures::future::join_all(results))
    })
    .await
    .expect("the engine finishes");

    for ((name, source), result) in sources.iter().zip(results) {
        if name == "taken.txt" {
            assert_eq!(result.unwrap_err().code(), Some("name_taken"));
            continue;
        }
        let committed = result.unwrap_or_else(|e| panic!("{name}: {e}"));
        assert_eq!(
            fetch(&client, committed.file, None).await,
            source.0,
            "{name}"
        );
    }
    assert_eq!((last.files, last.files_done), (34, 34));
    assert_eq!(
        last.uploaded + last.skipped,
        last.total - big.len() as u64,
        "the copy's chunks were never sent twice: {last:?}"
    );

    // All of them down through one engine, into files; a file that doesn't
    // exist fails alone. Then again: every file already identical.
    let committed: Vec<(String, uuid::Uuid, Vec<u8>)> = {
        let listed = client.children(library.root).await.unwrap();
        sources
            .iter()
            .filter(|(name, _)| name != "taken.txt")
            .map(|(name, source)| {
                let node = listed.iter().find(|n| &n.name == name).unwrap();
                (name.clone(), node.id, source.0.clone())
            })
            .collect()
    };
    let out = tempfile::tempdir().unwrap();
    for round in ["fresh", "again"] {
        let options = DownloadOptions {
            skip_identical: true,
            ..Default::default()
        };
        let sinks: Vec<FileSink> = committed
            .iter()
            .map(|(name, _, _)| FileSink::new(&out.path().join(name)))
            .collect();
        let missing = FileSink::new(&out.path().join("missing"));
        let (downloader, run) = Downloader::new(
            &client,
            &options,
            &cancel,
            |_: &theca_client::DownloadProgress| {},
        );
        let results: Vec<_> = committed
            .iter()
            .zip(&sinks)
            .map(|((_, id, _), sink)| downloader.submit(*id, None, sink))
            .collect();
        let gone = downloader.submit(uuid::Uuid::nil(), None, &missing);
        drop(downloader);
        let ((), results, gone) = tokio::time::timeout(Duration::from_secs(60), async {
            tokio::join!(run, futures::future::join_all(results), gone)
        })
        .await
        .expect("the engine finishes");
        assert_eq!(gone.unwrap_err().code(), Some("not_found"));
        for ((name, _, bytes), result) in committed.iter().zip(results) {
            let downloaded = result.unwrap_or_else(|e| panic!("{name}: {e}"));
            assert_eq!(downloaded.unchanged, round == "again", "{round}: {name}");
            assert_eq!(
                &std::fs::read(out.path().join(name)).unwrap(),
                bytes,
                "{name}"
            );
        }
    }
    let leftovers: Vec<_> = std::fs::read_dir(out.path())
        .unwrap()
        .filter_map(Result::ok)
        .filter(|e| e.file_name().to_string_lossy().ends_with(".theca-part"))
        .collect();
    assert!(leftovers.is_empty(), "no partial file left");
}

/// One file cancelled among others: it alone ends `cancelled`, the others
/// commit, and the chunks it had uploaded are freed on the server.
#[sqlx::test]
async fn cancel_one_file(pool: PgPool) {
    let server = start(&pool).await;
    let dir = tempfile::tempdir().unwrap();
    let mut client = Client::new(&server.url).unwrap();
    let setup: api::Setup = serde_json::from_value(json!({
        "admin_email": "admin@test.theca", "admin_password": "a long password", "organization": "Acme",
        "storages": [{ "name": "main", "implementation": "filesystem", "path": dir.path() }],
    }))
    .unwrap();
    let me = client.setup(&setup, None).await.unwrap();
    let library = client
        .create_library(me.organizations[0].organization, "Docs")
        .await
        .unwrap();
    let target = |name: &str| Target::New {
        library: library.id,
        parent: library.root,
        name: Name::new(name).unwrap(),
    };
    let big = MemorySource(data(6 * 1024 * 1024, 8));
    let small = MemorySource(data(20_000, 9));
    // One chunk at a time: the big file is still going when its first chunk
    // is confirmed.
    let options = UploadOptions {
        parallel: 1,
        window: Some(1),
        ..Default::default()
    };
    let cancel = Cancel::default();
    let confirmed = std::sync::atomic::AtomicBool::new(false);
    let (uploader, run) = Uploader::new(&client, &options, &cancel, |p: &UploadProgress| {
        if p.uploaded > 0 {
            confirmed.store(true, std::sync::atomic::Ordering::Relaxed);
        }
    });
    let big_upload = uploader.submit(&big, target("big.bin"));
    let small_upload = uploader.submit(&small, target("small.bin"));
    drop(uploader);
    let cancel_big = async {
        while !confirmed.load(std::sync::atomic::Ordering::Relaxed) {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        big_upload.cancel();
        big_upload.await
    };
    let ((), big_result, small_result) = tokio::time::timeout(Duration::from_secs(60), async {
        tokio::join!(run, cancel_big, small_upload)
    })
    .await
    .expect("the engine finishes");
    assert!(
        matches!(big_result, Err(theca_client::Error::Cancelled)),
        "{big_result:?}"
    );
    let small_committed = small_result.unwrap();
    assert_eq!(fetch(&client, small_committed.file, None).await, small.0);

    // Its confirmed chunks were freed: everything goes again.
    let mut again = UploadProgress::default();
    upload(
        &client,
        &big,
        target("big.bin"),
        &UploadOptions::default(),
        |p| again = p.clone(),
        &Cancel::default(),
    )
    .await
    .unwrap();
    assert_eq!(
        again.skipped, 0,
        "the cancelled file's chunks were freed: {again:?}"
    );
}

/// The upload engine sizes its window by the server's `max_pending_uploads`
/// (`GET /limits`, live): a window larger than the cap would have every
/// declaration refused, forever.
#[sqlx::test]
async fn the_window_follows_the_server(pool: PgPool) {
    let server = start(&pool).await;
    let dir = tempfile::tempdir().unwrap();
    let mut client = Client::new(&server.url).unwrap();
    let setup: api::Setup = serde_json::from_value(json!({
        "admin_email": "admin@test.theca", "admin_password": "a long password", "organization": "Acme",
        "storages": [{ "name": "main", "implementation": "filesystem", "path": dir.path() }],
    }))
    .unwrap();
    let me = client.setup(&setup, None).await.unwrap();
    let library = client
        .create_library(me.organizations[0].organization, "Docs")
        .await
        .unwrap();
    assert_eq!(client.limits().await.unwrap().max_pending_uploads, 15);
    let _: serde_json::Value = client
        .call(
            reqwest::Method::PATCH,
            "/api/v1/admin/settings",
            Some(&json!({ "max_pending_uploads": "2" })),
        )
        .await
        .unwrap();
    assert_eq!(client.limits().await.unwrap().max_pending_uploads, 2);

    let bytes = data(8 * 1024 * 1024, 11);
    let target = Target::New {
        library: library.id,
        parent: library.root,
        name: Name::new("big.bin").unwrap(),
    };
    let committed = tokio::time::timeout(
        Duration::from_secs(60),
        upload(
            &client,
            &MemorySource(bytes.clone()),
            target,
            &UploadOptions::default(),
            |_| {},
            &Cancel::default(),
        ),
    )
    .await
    .expect("declarations within the server's window are never refused")
    .unwrap();
    assert_eq!(fetch(&client, committed.file, None).await, bytes);
}

#[sqlx::test]
async fn placing_and_streaming(pool: PgPool) {
    let server = start(&pool).await;
    let dir = tempfile::tempdir().unwrap();
    let mut client = Client::new(&server.url).unwrap();
    let setup: api::Setup = serde_json::from_value(json!({
        "admin_email": "admin@test.theca", "admin_password": "a long password", "organization": "Acme",
        "storages": [{ "name": "main", "implementation": "filesystem", "path": dir.path() }],
    }))
    .unwrap();
    let me = client.setup(&setup, None).await.unwrap();
    let library = client
        .create_library(me.organizations[0].organization, "Docs")
        .await
        .unwrap();
    let (id, root) = (library.id, library.root);

    // A tree: its folders created (an empty one too), each file a new one; a
    // folder with an invalid name fails its files alone; what the operating
    // system left is skipped.
    let files: Vec<String> = [
        "a.txt",
        "Docs/b.txt",
        "Docs/sub/c.txt",
        "bad:name/d.txt",
        "Docs/.DS_Store",
        "._a.txt",
        "Docs/sub/Thumbs.db",
    ]
    .map(String::from)
    .into();
    let targets = place(&client, id, root, &["Docs/empty".into()], &files)
        .await
        .unwrap();
    assert!(matches!(&targets[0], Ok(Target::New { parent, .. }) if *parent == root));
    assert!(matches!(&targets[2], Ok(Target::New { name, .. }) if name.as_str() == "c.txt"));
    assert!(matches!(&targets[3], Err(Error::InvalidName(_))));
    for ignored in &targets[4..] {
        assert!(matches!(ignored, Err(Error::Ignored(_))));
    }
    let docs = client
        .children(root)
        .await
        .unwrap()
        .into_iter()
        .find(|n| n.name == "Docs")
        .unwrap();
    let inside: Vec<String> = client
        .children(docs.id)
        .await
        .unwrap()
        .into_iter()
        .map(|n| n.name)
        .collect();
    assert_eq!(inside, ["empty", "sub"]);
    let sources = [MemorySource(data(3000, 20)), MemorySource(data(4000, 21))];
    let options = UploadOptions::default();
    let mut committed = Vec::new();
    for (source, target) in sources.iter().zip(targets.into_iter().take(2)) {
        let done = upload(
            &client,
            source,
            target.unwrap(),
            &options,
            |_| {},
            &Cancel::default(),
        )
        .await
        .unwrap();
        committed.push(done);
    }

    // Again, names in another case: new versions of the files there, based
    // on their current versions; a file where a folder is refused.
    let files: Vec<String> = ["A.TXT", "docs/B.txt", "Docs"].map(String::from).into();
    let targets = place(&client, id, root, &[], &files).await.unwrap();
    for (target, done) in targets.iter().zip(&committed) {
        assert!(matches!(target,
            Ok(Target::Version { file, base, .. }) if *file == done.file && *base == Some(done.version)));
    }
    assert!(matches!(&targets[2], Err(Error::FolderExists(_))));

    // A file streamed in order, a few chunks ahead, as a browser download.
    let big = data(6 * 1024 * 1024, 22);
    let target = Target::New {
        library: id,
        parent: root,
        name: Name::new("big.bin").unwrap(),
    };
    let done = upload(
        &client,
        &MemorySource(big.clone()),
        target,
        &options,
        |_| {},
        &Cancel::default(),
    )
    .await
    .unwrap();
    let options = DownloadOptions {
        parallel: 2,
        ..Default::default()
    };
    let chunks: Vec<Vec<u8>> = stream(client.clone(), done.file, None, options)
        .try_collect()
        .await
        .unwrap();
    assert!(chunks.len() > 2);
    assert_eq!(chunks.concat(), big);
}
