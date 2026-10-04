use std::process::ExitCode;

use tracing_subscriber::EnvFilter;

#[tokio::main]
async fn main() -> ExitCode {
    // Dev convenience (written by scripts/dev.sh). Release builds read the real environment only.
    #[cfg(debug_assertions)]
    let _ = dotenvy::dotenv();

    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()))
        .init();

    let result = match std::env::args().nth(1).as_deref() {
        None => theca_core::run().await.map(|()| true),
        Some("healthcheck") => theca_core::healthcheck().await,
        Some(other) => {
            eprintln!("unknown command {other:?}; usage: theca-server [healthcheck]");
            return ExitCode::from(2);
        }
    };
    match result {
        Ok(true) => ExitCode::SUCCESS,
        Ok(false) => ExitCode::FAILURE,
        Err(e) => {
            tracing::error!("{e}");
            ExitCode::FAILURE
        }
    }
}
