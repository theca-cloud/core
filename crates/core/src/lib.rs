//! The Theca server: API, WebDAV, background jobs, migrations.
//!
//! The binary (`theca-server`) only calls [`run`], so other binaries (e.g. the
//! enterprise edition) can embed the server.

pub mod api;
pub mod auth;
pub mod config;
pub mod dav;
pub mod db;
pub mod email;
mod error;
pub mod files;
pub mod jobs;
pub mod keys;
pub mod limits;
pub mod oidc;
pub mod settings;
pub mod storage;
pub mod stream;
pub mod telemetry;
pub mod totp;
pub mod web;

use std::time::Duration;

pub use error::Error;

use crate::api::AppState;
use crate::config::Config;
use crate::keys::{DataCipher, SecretKey};
use crate::settings::Settings;

/// Installs `ring` as the TLS crypto provider (S3, and later OIDC). Call once
/// at startup, before any HTTP client is built. See docs: Rust stack.
pub fn install_crypto_provider() {
    // Fails only if a provider is already installed, which is fine.
    let _ = rustls::crypto::ring::default_provider().install_default();
}

/// Starts the server and returns when it stops (SIGTERM or Ctrl-C).
pub async fn run() -> Result<(), Error> {
    let config = Config::from_env()?;
    let server_key = SecretKey::load("SERVER_KEY__FILE", &config.server_key_file)?;
    let data_key = SecretKey::load("DATA_KEY__FILE", &config.data_key_file)?;
    let cipher = DataCipher::new(&data_key.derive("theca data v1"));
    let signer =
        storage::UrlSigner::new(server_key.derive("theca blob url v1"), &config.public_url);
    let pool = db::connect(&config.database_url).await?;
    let settings = settings::LiveSettings::new(Settings::load(&pool).await?);

    // Only listen once the database is migrated and settings are loaded, so the
    // health probe fails during startup.
    let listener = tokio::net::TcpListener::bind(config.listen_addr).await?;
    tracing::info!(addr = %config.listen_addr, public_url = config.public_url, "listening");
    let backends = storage::Backends::new(pool.clone());
    let setup = if db::setup::is_fresh(&pool).await? {
        let token = (!config.setup_open).then(api::setup::generate_token);
        match &token {
            Some(token) => tracing::info!(
                "not set up yet: open {}/setup?token={token}",
                config.public_url.trim_end_matches('/')
            ),
            None => tracing::warn!(
                "not set up yet, and SETUP_OPEN is set: the first visitor of {}/setup sets it up",
                config.public_url.trim_end_matches('/')
            ),
        }
        api::setup::SetupState::pending(token)
    } else {
        api::setup::SetupState::done()
    };
    let state = AppState {
        pool,
        settings,
        signer,
        backends,
        cipher,
        setup,
        limits: Default::default(),
        proxies: std::sync::Arc::new(config.trusted_proxies.clone()),
    };

    let interrupted = jobs::mark_interrupted(&state.pool).await?;
    if interrupted > 0 {
        tracing::warn!(
            interrupted,
            "job runs cut off by the last stop: they resume now"
        );
    }
    let (stop, stopping) = tokio::sync::watch::channel(false);
    let mut job_tasks = jobs::spawn_all(state.clone(), stopping);

    if let Some(addr) = config.metrics_addr {
        let handle = telemetry::install_recorder();
        let metrics = tokio::net::TcpListener::bind(addr).await?;
        tracing::info!(%addr, "metrics on /metrics");
        tokio::spawn(
            axum::serve(metrics, telemetry::router(handle, state.pool.clone())).into_future(),
        );
    }
    let app = api::router(state);
    let app = match &config.web_dir {
        Some(dir) => web::serve(app, dir),
        None => app,
    };
    // Connect info: the client's IP, logged with anomalies.
    axum::serve(
        listener,
        app.into_make_service_with_connect_info::<std::net::SocketAddr>(),
    )
    .with_graceful_shutdown(async move {
        shutdown_signal().await;
        let _ = stop.send(true);
    })
    .await?;
    // Jobs: no new runs; current ones get a few seconds, then are dropped (the
    // next start marks them interrupted, and they resume).
    let drain = async { while job_tasks.join_next().await.is_some() {} };
    if tokio::time::timeout(Duration::from_secs(5), drain)
        .await
        .is_err()
    {
        tracing::warn!("job runs still going: dropped, they resume at the next start");
    }
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
