//! Startup configuration, from environment variables only.
//!
//! Secrets only exist as `X__FILE` (a path to a file holding the value); every
//! other setting only as a plain variable.

use std::env;
use std::net::SocketAddr;
use std::path::PathBuf;

use crate::Error;

/// What the server needs before it can reach its database.
#[derive(Debug, Clone)]
pub struct Config {
    pub database_url: String,
    pub listen_addr: SocketAddr,
    pub public_url: String,
    pub server_key_file: PathBuf,
    pub data_key_file: PathBuf,
}

impl Config {
    /// Reads the configuration from the environment.
    pub fn from_env() -> Result<Self, Error> {
        Ok(Self {
            database_url: secret("DATABASE_URL__FILE")?,
            listen_addr: listen_addr()?,
            public_url: required("PUBLIC_URL")?,
            server_key_file: required("SERVER_KEY__FILE")?.into(),
            data_key_file: required("DATA_KEY__FILE")?.into(),
        })
    }
}

/// `LISTEN_ADDR`, `0.0.0.0:8080` by default.
pub fn listen_addr() -> Result<SocketAddr, Error> {
    let raw = env::var("LISTEN_ADDR").unwrap_or_else(|_| "0.0.0.0:8080".into());
    raw.parse().map_err(|e| Error::InvalidEnv {
        name: "LISTEN_ADDR",
        reason: format!("{e}"),
    })
}

fn required(name: &'static str) -> Result<String, Error> {
    env::var(name).map_err(|_| Error::MissingEnv(name))
}

/// Reads a secret from the file named by the `name` variable (trailing newline trimmed).
pub fn secret(name: &'static str) -> Result<String, Error> {
    read_secret(name, PathBuf::from(required(name)?))
}

fn read_secret(name: &'static str, path: PathBuf) -> Result<String, Error> {
    let value = std::fs::read_to_string(&path).map_err(|source| Error::SecretFile {
        name,
        path,
        source,
    })?;
    Ok(value.trim_end_matches(['\n', '\r']).to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn secret_file_is_read_and_trimmed() {
        let path = std::env::temp_dir().join(format!("theca-secret-{}", std::process::id()));
        std::fs::write(&path, "postgres://u:p@h/db\n").unwrap();
        assert_eq!(read_secret("X__FILE", path).unwrap(), "postgres://u:p@h/db");
    }

    #[test]
    fn missing_secret_file_names_the_variable() {
        let err = read_secret("X__FILE", "/nonexistent/theca".into()).unwrap_err();
        assert!(err.to_string().contains("X__FILE"), "{err}");
    }

    #[test]
    fn missing_variable() {
        assert!(matches!(
            secret("THECA_TEST_UNSET__FILE"),
            Err(Error::MissingEnv(_))
        ));
    }
}
