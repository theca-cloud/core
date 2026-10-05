# Theca core

The Theca server and the Rust libraries shared by its clients:

- `common`: API types, chunking, hashing, name rules (native + WASM).
- `core`: the server (API, WebDAV, background jobs, migrations).
- `client`: the engine every Rust client is built on (auth, upload, download).

Configuring a server: [CONFIGURATION.md](CONFIGURATION.md). Design and decisions: [theca/docs](https://git.pband.ch/theca/docs). Contributing: see [CONTRIBUTING.md](https://git.pband.ch/theca/docs/src/branch/prod/CONTRIBUTING.md).

## Development

Prerequisites: Rust (the toolchain is pinned in `rust-toolchain.toml`), Docker, and:

```sh
cargo install --locked sqlx-cli --no-default-features --features postgres,rustls
cargo install --locked cargo-deny
```

```sh
scripts/dev.sh                 # start Postgres, write .env and dev secrets, migrate (idempotent)
cargo run -p theca-core        # run the server on :8080 (binary: theca-server)
cargo run -p theca-core -- healthcheck   # probe /api/health, exit 0 or 1
cargo test --workspace         # needs scripts/dev.sh (tests use the dev Postgres and Garage)
UPDATE_OPENAPI=1 cargo test    # regenerate openapi.json after changing the API
cargo sqlx prepare --workspace -- --all-targets   # refresh .sqlx/ after changing a query
sqlx database reset -y -f --source crates/core/migrations   # after editing the migration (rewritten in place until 1.0.0)
scripts/ci.sh [lint|test|wasm|deny]   # every CI check natively, or one step
scripts/ci-docker.sh           # the real Woodpecker pipeline, in Docker (needs woodpecker-cli)
scripts/install-hooks.sh       # run scripts/ci.sh before every push (bypass: git push --no-verify)
scripts/release.sh --dry-run   # the next version and its changelog (releases: see CONTRIBUTING.md)
cargo fmt --all                # fix formatting
```

Dev explorers: database (pgweb, no login) http://localhost:8081, S3 (garage-webui) http://localhost:3909, emails (Mailpit) http://localhost:8025 (SMTP on `localhost:1025`).

Start over: `rm -rf data/ .env`, then `scripts/dev.sh`.

CI runs in `git.pband.ch/theca/ci-rust`, built from `ci/Dockerfile` and pushed by `scripts/ci-image.sh` (after `docker login git.pband.ch`). Bumping Rust: update `rust-toolchain.toml`, run `scripts/ci-image.sh`, then update the image tag in `.woodpecker/ci.yaml`.

Releases: `scripts/release.sh` on `dev`, then a merge into `prod` publishes the server image (`git.pband.ch/theca/core`, amd64 and arm64), the crates and `@theca/client` on the Gitea registries, and the Gitea release (see the release process in CONTRIBUTING.md).

License: AGPL-3.0.
