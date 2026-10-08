//! Startup configuration, from environment variables only.
//!
//! Secrets only exist as `X__FILE` (a path to a file holding the value); every
//! other setting only as a plain variable.

use std::env;
use std::net::SocketAddr;
use std::path::PathBuf;

use sqlx::types::ipnetwork::IpNetwork;

use crate::Error;

/// What the server needs before it can reach its database.
#[derive(Debug, Clone)]
pub struct Config {
    pub database_url: String,
    pub listen_addr: SocketAddr,
    pub public_url: String,
    pub server_key_file: PathBuf,
    pub data_key_file: PathBuf,
    /// `SETUP_OPEN=true`: a fresh instance's setup needs no token.
    pub setup_open: bool,
    /// `TRUSTED_PROXIES`: reverse proxies whose `X-Forwarded-For` is believed.
    pub trusted_proxies: Vec<IpNetwork>,
    /// `METRICS_ADDR`: where Prometheus metrics are served (never the public port).
    pub metrics_addr: Option<SocketAddr>,
    /// `WEB_DIR`: the web app's files, served for paths the API doesn't know.
    pub web_dir: Option<PathBuf>,
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
            setup_open: setup_open()?,
            trusted_proxies: trusted_proxies()?,
            web_dir: env::var("WEB_DIR").ok().map(PathBuf::from),
            metrics_addr: match env::var("METRICS_ADDR") {
                Err(_) => None,
                Ok(v) => Some(v.parse().map_err(|e| Error::InvalidEnv {
                    name: "METRICS_ADDR",
                    reason: format!("{e}"),
                })?),
            },
        })
    }
}

/// `TRUSTED_PROXIES`: comma-separated addresses or CIDRs (e.g.
/// `10.0.0.0/8, 172.17.0.1`). None by default: the peer is the client.
fn trusted_proxies() -> Result<Vec<IpNetwork>, Error> {
    let Ok(raw) = env::var("TRUSTED_PROXIES") else {
        return Ok(Vec::new());
    };
    raw.split(',')
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(|s| {
            s.parse().map_err(|e| Error::InvalidEnv {
                name: "TRUSTED_PROXIES",
                reason: format!("{s:?}: {e}"),
            })
        })
        .collect()
}

/// `SETUP_OPEN`, false by default.
fn setup_open() -> Result<bool, Error> {
    match env::var("SETUP_OPEN") {
        Err(_) => Ok(false),
        Ok(v) => v.parse().map_err(|_| Error::InvalidEnv {
            name: "SETUP_OPEN",
            reason: format!("{v:?} isn't true or false"),
        }),
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

/// Reads a secret file, never writing it: a missing or empty one is an error
/// naming the variable.
pub(crate) fn read_secret(name: &'static str, path: PathBuf) -> Result<String, Error> {
    let value = match std::fs::read_to_string(&path) {
        Ok(value) => value,
        Err(source) => return Err(Error::SecretFile { name, path, source }),
    };
    if value.trim().is_empty() {
        return Err(Error::EmptySecret { name, path });
    }
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
    fn empty_secret_file_names_the_variable() {
        let path = std::env::temp_dir().join(format!("theca-secret-empty-{}", std::process::id()));
        std::fs::write(&path, "\n").unwrap();
        let err = read_secret("X__FILE", path).unwrap_err();
        assert!(matches!(err, Error::EmptySecret { .. }), "{err}");
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
