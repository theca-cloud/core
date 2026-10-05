//! The storage backend contract, the same on every backend: the filesystem and
//! S3 (Garage, `S3_TEST_*` variables from `scripts/dev.sh` or CI).

use serde_json::json;
use theca_core::storage::Backend;
use uuid::Uuid;

fn filesystem() -> (Backend, tempfile::TempDir) {
    let dir = tempfile::tempdir().unwrap();
    (
        Backend::new("filesystem", &json!({ "path": dir.path() })).unwrap(),
        dir,
    )
}

fn s3() -> Backend {
    theca_core::install_crypto_provider();
    dotenvy::dotenv().ok();
    let var = |name: &str| {
        std::env::var(name).unwrap_or_else(|_| panic!("{name} is required: run scripts/dev.sh"))
    };
    let details = json!({
        "host": var("S3_TEST_HOST"),
        "region": var("S3_TEST_REGION"),
        "bucket": var("S3_TEST_BUCKET"),
        "access_key_file": var("S3_TEST_ACCESS_KEY_FILE"),
        "secret_key_file": var("S3_TEST_SECRET_KEY_FILE"),
        "path_style": true,
    });
    Backend::new("s3", &details).unwrap()
}

async fn contract(backend: Backend) {
    let chunk = Uuid::now_v7();
    assert_eq!(backend.size(chunk).await.unwrap(), None, "missing object");
    backend.delete(chunk).await.unwrap(); // deleting a missing object is fine

    backend.put(chunk, b"hello".to_vec()).await.unwrap();
    assert_eq!(backend.size(chunk).await.unwrap(), Some(5));
    assert_eq!(backend.get(chunk).await.unwrap(), b"hello");

    backend.delete(chunk).await.unwrap();
    assert_eq!(backend.size(chunk).await.unwrap(), None);
    backend.delete(chunk).await.unwrap(); // twice
}

#[tokio::test]
async fn filesystem_contract() {
    let (backend, _dir) = filesystem();
    contract(backend).await;
}

#[tokio::test]
async fn s3_contract() {
    contract(s3()).await;
}

#[test]
fn unknown_implementation_and_missing_details() {
    assert!(Backend::new("ftp", &json!({})).is_err());
    assert!(Backend::new("filesystem", &json!({})).is_err());
    assert!(Backend::new("s3", &json!({ "host": "http://x" })).is_err());
}
