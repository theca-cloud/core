# Theca core

The Theca server and the Rust libraries shared by its clients:

- `common`: API types, chunking, hashing, name rules (native + WASM).
- `core`: the server (API, WebDAV, background jobs, migrations).
- `client`: the engine every Rust client is built on (auth, upload, download).

Design and decisions: [theca/docs](https://git.pband.ch/theca/docs). Contributing: see [CONTRIBUTING.md](https://git.pband.ch/theca/docs/src/branch/prod/CONTRIBUTING.md).

## Development

Prerequisites: Rust (the toolchain is pinned in `rust-toolchain.toml`), Docker.

```sh
scripts/dev.sh                 # start Postgres, write .env (idempotent)
cargo run -p theca-core        # run the server (binary: theca-server)
scripts/ci.sh                  # every CI check, natively (needs: cargo install --locked cargo-deny)
scripts/ci-docker.sh           # the real Woodpecker pipeline, in Docker (needs woodpecker-cli)
scripts/install-hooks.sh       # run scripts/ci.sh before every push (bypass: git push --no-verify)
cargo fmt --all                # fix formatting
```

Start over: `rm -rf data/ .env`, then `scripts/dev.sh`.

CI runs in `git.pband.ch/theca/ci-rust`, built from `ci/Dockerfile` and pushed by `scripts/ci-image.sh` (after `docker login git.pband.ch`). Bumping Rust: update `rust-toolchain.toml`, run `scripts/ci-image.sh`, then update the image tag in `.woodpecker/ci.yaml`.

License: AGPL-3.0.
