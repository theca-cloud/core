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
    #[error("the key file {1} (from {0}) is empty and not writable: {2}")]
    KeyNotWritable(&'static str, PathBuf, io::Error),
    #[error("invalid key in {1} (from {0}): expected 64 hex characters")]
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
