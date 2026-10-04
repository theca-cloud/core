#!/usr/bin/env sh
# Every CI check, run natively. Woodpecker runs this same script.
set -eu
cd "$(dirname "$0")/.."

if ! command -v cargo-deny >/dev/null 2>&1; then
    echo "cargo-deny is missing: cargo install --locked cargo-deny" >&2
    exit 1
fi

cargo fmt --all --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
cargo build -p theca-common -p theca-client --target wasm32-unknown-unknown
cargo deny check
echo "CI checks passed"
