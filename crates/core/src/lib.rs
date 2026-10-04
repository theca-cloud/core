//! The Theca server: API, WebDAV, background jobs, migrations.
//!
//! The binary (`theca-server`) only calls [`run`], so other binaries (e.g. the
//! enterprise edition) can embed the server.

#[derive(Debug, thiserror::Error)]
pub enum Error {}

/// Starts the server and returns when it stops.
pub async fn run() -> Result<(), Error> {
    tracing::info!("starting");
    Ok(())
}
