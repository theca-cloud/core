@../docs/CLAUDE.md

# This repo: `core`

Cargo workspace with `common` (lib, native + WASM), `core` (server: lib + thin binary), `client` (lib used by every Rust client), and the SQL migrations.

Crates: `crates/common` (`theca-common`), `crates/core` (`theca-core`, binary `theca-server`), `crates/client` (`theca-client`).

Build, run and test commands: see the README.

- **reqwest's `read_timeout` counts from the request's start until the answer's headers** (then per read of the body): it cut every slow upload (sprint 11, fixed in 1.3.3). Requests sending long bodies go through `Client::sending()`, which has none.
- **A paused tokio clock (`start_paused`) fires every timer against real I/O**, the connect timeout too: a timeout test passed for the wrong one. Timeout tests run in real time, with the client's timeouts short under `cfg(test)`.
- **Switching to a branch with fewer migrations** leaves the dev database ahead of the code (`sqlx`: "migration … was previously applied but is missing"). Reset it from that branch: `set -a; . ./.env; set +a; cargo sqlx database reset -y --source crates/core/migrations`.

@README.md
