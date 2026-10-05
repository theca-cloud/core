use std::process::ExitCode;

use tracing_subscriber::EnvFilter;

#[tokio::main]
async fn main() -> ExitCode {
    // Dev convenience (written by scripts/dev.sh). Release builds read the real environment only.
    #[cfg(debug_assertions)]
    let _ = dotenvy::dotenv();

    theca_core::install_crypto_provider();
    use tracing_subscriber::layer::SubscriberExt;
    use tracing_subscriber::util::SubscriberInitExt;
    tracing_subscriber::registry()
        .with(tracing_subscriber::fmt::layer())
        .with(EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()))
        .with(theca_core::telemetry::AnomalyCounter)
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
