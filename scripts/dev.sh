#!/usr/bin/env sh
# Starts the dev environment, writes .env and the dev secret files, migrates the DB.
# Idempotent: run it as often as needed. Start over: rm -rf data/ .env
set -eu
cd "$(dirname "$0")/.."

if ! command -v sqlx >/dev/null 2>&1; then
    echo "sqlx-cli is missing: cargo install --locked sqlx-cli --no-default-features --features postgres,rustls" >&2
    exit 1
fi

# The password is generated once, then reused from .env.
if [ -f .env ]; then
    password=$(sed -n 's/^POSTGRES_PASSWORD=//p' .env)
elif [ -d data/postgres ]; then
    echo "data/postgres exists but .env is missing: its password is lost. Run: rm -rf data/" >&2
    exit 1
else
    password=$(openssl rand -hex 24)
fi
url="postgres://theca:$password@localhost:5432/theca"

mkdir -p data/secrets
printf '%s\n' "$url" > data/secrets/database_url
[ -f data/secrets/jwt_key ] || : > data/secrets/jwt_key  # empty: the server generates the key

cat > .env <<ENV
# Written by scripts/dev.sh.
POSTGRES_USER=theca
POSTGRES_PASSWORD=$password
POSTGRES_DB=theca
# Read by sqlx's macros at compile time only; the server reads DATABASE_URL__FILE.
DATABASE_URL=$url
DATABASE_URL__FILE=data/secrets/database_url
JWT_KEY__FILE=data/secrets/jwt_key
PUBLIC_URL=http://localhost:8080
ENV

docker compose up -d --wait
sqlx migrate run --source crates/core/migrations
echo "Dev environment ready, see .env"
