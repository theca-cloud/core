@../docs/CLAUDE.md

# This repo: `core`

Cargo workspace with `common` (lib, native + WASM), `core` (server: lib + thin binary), `client` (lib used by every Rust client), and the SQL migrations.

Crates: `crates/common` (`theca-common`), `crates/core` (`theca-core`, binary `theca-server`), `crates/client` (`theca-client`).

Build, run and test commands: see the README.

- **Switching to a branch with fewer migrations** leaves the dev database ahead of the code (`sqlx`: "migration … was previously applied but is missing"). Reset it from that branch: `set -a; . ./.env; set +a; cargo sqlx database reset -y --source crates/core/migrations`.

@README.md
