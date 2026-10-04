//! The Theca server: API, WebDAV, background jobs, migrations.
//!
//! The binary (`theca-server`) only calls [`run`], so other binaries (e.g. the
//! enterprise edition) can embed the server.

pub mod api;
pub mod config;
pub mod db;
mod error;
pub mod jwt;
pub mod settings;

use std::sync::Arc;

pub use error::Error;

use crate::api::AppState;
use crate::config::Config;
use crate::jwt::JwtKey;
use crate::settings::Settings;

/// Starts the server and returns when it stops (SIGTERM or Ctrl-C).
pub async fn run() -> Result<(), Error> {
    let config = Config::from_env()?;
    let _jwt_key = JwtKey::load_or_generate(&config.jwt_key_file)?;
    let pool = db::connect(&config.database_url).await?;
    let settings = Arc::new(Settings::load(&pool).await?);

    // Only listen once the database is migrated and settings are loaded, so the
    // health probe fails during startup.
    let listener = tokio::net::TcpListener::bind(config.listen_addr).await?;
    tracing::info!(addr = %config.listen_addr, public_url = config.public_url, "listening");
    axum::serve(listener, api::router(AppState { pool, settings }))
        .with_graceful_shutdown(shutdown_signal())
        .await?;
    tracing::info!("stopped");
    Ok(())
}

/// Probes `/api/health` on `LISTEN_ADDR`. For Docker's `HEALTHCHECK`: production
/// images don't ship `curl`.
pub async fn healthcheck() -> Result<bool, Error> {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    let mut addr = config::listen_addr()?;
    if addr.ip().is_unspecified() {
        addr.set_ip(std::net::Ipv4Addr::LOCALHOST.into());
    }
    let mut stream = tokio::net::TcpStream::connect(addr).await?;
    stream
        .write_all(b"GET /api/health HTTP/1.0\r\nHost: localhost\r\n\r\n")
        .await?;
    let mut response = Vec::new();
    stream.read_to_end(&mut response).await?;
    Ok(response.starts_with(b"HTTP/1.0 200") || response.starts_with(b"HTTP/1.1 200"))
}

async fn shutdown_signal() {
    let ctrl_c = async {
        let _ = tokio::signal::ctrl_c().await;
    };
    #[cfg(unix)]
    let terminate = async {
        if let Ok(mut s) = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
        {
            s.recv().await;
        }
    };
    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();
    tokio::select! { () = ctrl_c => {}, () = terminate => {} }
    tracing::info!("shutting down");
}
