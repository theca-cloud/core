#!/usr/bin/env sh
# Every CI check, run natively: `scripts/ci.sh` runs them all (locally, and the
# pre-push hook); `scripts/ci.sh <step>` runs one, as the Woodpecker steps do:
#   lint  formatting, .sqlx/ up to date, clippy (needs the database)
#   test  every test (database, S3, SMTP, litmus)
#   wasm  the crates built for the browser
#   deny  licenses, advisories, sources
# Locally it uses the dev environment (scripts/dev.sh); CI sets DATABASE_URL itself.
set -eu
cd "$(dirname "$0")/.."

step=${1:-all}
case "$step" in
    all | lint | test | wasm | deny) ;;
    *)
        echo "usage: scripts/ci.sh [lint|test|wasm|deny]" >&2
        exit 2
        ;;
esac

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

database() {
    : "${DATABASE_URL:?DATABASE_URL is required: run scripts/dev.sh}"
    # The database may still be starting (CI service): retry for up to 30 s.
    attempt=0
    until sqlx migrate run --source crates/core/migrations; do
        attempt=$((attempt + 1))
        [ "$attempt" -lt 30 ] || exit 1
        sleep 1
    done
}

s3() {
    # Locally, .env points to the dev Garage (scripts/dev.sh). In CI, start a
    # throwaway Garage (its binary is in the CI image).
    [ -n "${S3_TEST_HOST:-}" ] && return
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
}

# openapi.json against the last release's: no breaking change to /api/v1 (a
# client built against any 1.x keeps working). From 1.0 on: betas may break.
api_compatibility() {
    if ! command -v oasdiff >/dev/null 2>&1; then
        echo "oasdiff missing: API compatibility not checked here (the CI checks it)"
        return
    fi
    git fetch -q --tags origin 2>/dev/null || true
    last=$(git describe --tags --abbrev=0 --match 'v*' 2>/dev/null) || {
        echo "no release yet: no API to stay compatible with"
        return
    }
    case "$last" in
    v0.*)
        # Beta (decisions.md, Versions): the API may still break until 1.0.
        echo "last release $last is beta (0.x): breaking API changes allowed until 1.0"
        return
        ;;
    esac
    git show "$last:openapi.json" >target/released-openapi.json
    oasdiff breaking target/released-openapi.json openapi.json --fail-on ERR
}

if [ "$step" = all ] || [ "$step" = lint ]; then
    database
    cargo fmt --all --check
    cargo sqlx prepare --workspace --check -- --all-targets
    cargo clippy --workspace --all-targets -- -D warnings
    api_compatibility
fi
if [ "$step" = all ] || [ "$step" = test ]; then
    database
    s3
    cargo test --workspace
fi
if [ "$step" = all ] || [ "$step" = wasm ]; then
    cargo build -p theca-common -p theca-client -p theca-password --target wasm32-unknown-unknown
fi
if [ "$step" = all ] || [ "$step" = deny ]; then
    cargo deny check
fi
echo "CI checks passed ($step)"
