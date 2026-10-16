// The real `theca-server` binary as a process (`include!`d by the tests that
// drive it through `theca-client`).

use std::net::TcpListener;
use std::process::{Child, Command, Stdio};
use std::time::Duration;

/// The server process, killed when dropped.
struct Server {
    child: Child,
    url: String,
    /// Its working directory: its keys (`server_key`, `data_key`) and a
    /// filesystem storage's relative path. Only the upgrade test reads it
    /// (its dump); the others just keep it alive.
    #[allow(dead_code)]
    dir: tempfile::TempDir,
}

impl Drop for Server {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// The keys the tests' servers run with (written into `dir` when missing).
const SERVER_KEY: &str = "1111111111111111111111111111111111111111111111111111111111111111";
const DATA_KEY: &str = "2222222222222222222222222222222222222222222222222222222222222222";

/// Starts `theca-server` on `database_url` (it migrates at startup), working
/// in `dir`, on a free port, and waits until it's healthy.
async fn start_server(database_url: &str, dir: tempfile::TempDir, setup_open: bool) -> Server {
    theca_core::install_crypto_provider();
    let file = |name: &str, content: &str| {
        let path = dir.path().join(name);
        if !path.exists() {
            std::fs::write(&path, content).unwrap();
        }
        path
    };
    let port = TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port();
    let public = format!("http://127.0.0.1:{port}");
    let mut command = Command::new(env!("CARGO_BIN_EXE_theca-server"));
    command
        .current_dir(dir.path())
        .env("DATABASE_URL__FILE", file("database_url", database_url))
        .env("SERVER_KEY__FILE", file("server_key", SERVER_KEY))
        .env("DATA_KEY__FILE", file("data_key", DATA_KEY))
        .env("PUBLIC_URL", &public)
        .env("LISTEN_ADDR", format!("127.0.0.1:{port}"))
        .env("RUST_LOG", "warn")
        .stdout(Stdio::null());
    if setup_open {
        command.env("SETUP_OPEN", "true");
    }
    let mut server = Server {
        child: command.spawn().expect("the server binary"),
        url: public,
        dir,
    };
    for _ in 0..300 {
        if let Ok(response) = reqwest::get(format!("{}/api/health", server.url)).await
            && response.status().is_success()
        {
            return server;
        }
        if let Ok(Some(status)) = server.child.try_wait() {
            panic!("the server exited: {status}");
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    panic!("the server didn't become healthy");
}

/// The database URL of the test's own database (`DATABASE_URL`'s server).
fn database_url(database: &str) -> String {
    dotenvy::dotenv().ok();
    let base = std::env::var("DATABASE_URL").expect("DATABASE_URL: run scripts/dev.sh");
    format!("{}/{database}", base.rsplit_once('/').unwrap().0)
}
