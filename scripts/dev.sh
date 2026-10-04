#!/usr/bin/env sh
# Starts the dev environment and writes .env. Idempotent: run it as often as needed.
# Start over: rm -rf data/ .env
set -eu
cd "$(dirname "$0")/.."

if [ ! -f .env ]; then
    if [ -d data/postgres ]; then
        echo "data/postgres exists but .env is missing: its password is lost. Run: rm -rf data/" >&2
        exit 1
    fi
    password=$(openssl rand -hex 24)
    cat > .env <<ENV
POSTGRES_USER=theca
POSTGRES_PASSWORD=$password
POSTGRES_DB=theca
DATABASE_URL=postgres://theca:$password@localhost:5432/theca
ENV
fi

docker compose up -d --wait
echo "Dev environment ready, see .env"
