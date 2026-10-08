use std::io;
use std::path::PathBuf;

/// Everything that can stop the server. Startup errors say what to fix.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("missing environment variable {0}")]
    MissingEnv(&'static str),
    #[error("invalid environment variable {name}: {reason}")]
    InvalidEnv { name: &'static str, reason: String },
    #[error("cannot read secret file {path} (from {name}): {source}")]
    SecretFile {
        name: &'static str,
        path: PathBuf,
        source: io::Error,
    },
    #[error("the secret file {path} (from {name}) is empty{hint}", hint = key_hint(name))]
    EmptySecret { name: &'static str, path: PathBuf },
    #[error("invalid key in {1} (from {0}): expected 64 hex characters{hint}", hint = key_hint(.0))]
    InvalidKey(&'static str, PathBuf),
    #[error("database error: {0}")]
    Database(#[from] sqlx::Error),
    #[error("database migration failed: {0}")]
    Migration(#[from] sqlx::migrate::MigrateError),
    #[error("unsupported database: {0}")]
    UnsupportedDatabase(String),
    #[error("invalid setting {key}: {reason}")]
    InvalidSetting { key: &'static str, reason: String },
    #[error("{0}")]
    Io(#[from] io::Error),
}

/// How to make a key, for the errors a first start without one meets.
fn key_hint(name: &str) -> &'static str {
    if name.ends_with("KEY__FILE") {
        " (make one with `openssl rand -hex 32`)"
    } else {
        ""
    }
}
