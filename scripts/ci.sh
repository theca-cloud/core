#!/usr/bin/env sh
# Every CI check, run natively. Woodpecker runs this same script.
# Locally it uses the dev environment (scripts/dev.sh); CI sets DATABASE_URL itself.
set -eu
cd "$(dirname "$0")/.."

for tool in cargo-deny sqlx; do
    if ! command -v "$tool" >/dev/null 2>&1; then
        echo "$tool is missing, see the README prerequisites" >&2
        exit 1
    fi
done
# Only when the environment doesn't provide DATABASE_URL (CI does; a local .env
# may be present in the workspace, e.g. with scripts/ci-docker.sh).
if [ -z "${DATABASE_URL:-}" ] && [ -f .env ]; then
    set -a
    . ./.env
    set +a
fi
: "${DATABASE_URL:?DATABASE_URL is required: run scripts/dev.sh}"

# The database may still be starting (CI service): retry for up to 30 s.
attempt=0
until sqlx migrate run --source crates/core/migrations; do
    attempt=$((attempt + 1))
    [ "$attempt" -lt 30 ] || exit 1
    sleep 1
done

cargo fmt --all --check
cargo sqlx prepare --workspace --check -- --all-targets
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
cargo build -p theca-common -p theca-client --target wasm32-unknown-unknown
cargo deny check
echo "CI checks passed"
