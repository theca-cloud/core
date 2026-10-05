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

# S3 tests: locally, .env points to the dev Garage (scripts/dev.sh). In CI,
# start a throwaway Garage (its binary is in the CI image).
if [ -z "${S3_TEST_HOST:-}" ]; then
    if ! command -v garage >/dev/null 2>&1; then
        echo "no S3 for the tests: run scripts/dev.sh (or use the CI image)" >&2
        exit 1
    fi
    mkdir -p /tmp/garage
    garage -c ci/garage.toml server >/tmp/garage/server.log 2>&1 &
    scripts/garage-init.sh /tmp/garage/secrets garage -c ci/garage.toml
    export S3_TEST_HOST=http://127.0.0.1:3900 S3_TEST_REGION=garage S3_TEST_BUCKET=theca \
        S3_TEST_ACCESS_KEY_FILE=/tmp/garage/secrets/s3_access_key \
        S3_TEST_SECRET_KEY_FILE=/tmp/garage/secrets/s3_secret_key
fi

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
