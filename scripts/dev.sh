#!/usr/bin/env sh
# Starts the dev environment, writes .env and the dev secret files, migrates the DB.
# Idempotent: run it as often as needed. Start over: rm -rf data/ .env
set -eu
cd "$(dirname "$0")/.."

if ! command -v sqlx >/dev/null 2>&1; then
    echo "sqlx-cli is missing: cargo install --locked sqlx-cli --no-default-features --features postgres,rustls" >&2
    exit 1
fi

# A host port: the one .env has, else `default` or the next free one (another
# project may hold it), kept in .env from then on.
port() {
    value=$(sed -n "s/^$1=//p" .env 2>/dev/null)
    if [ -z "$value" ]; then
        value=$2
        while nc -z 127.0.0.1 "$value" 2>/dev/null; do
            value=$((value + 1))
        done
    fi
    echo "$value"
}
postgres_port=$(port POSTGRES_PORT 5432)
garage_port=$(port S3_PORT 3900)
garage_webui_port=$(port GARAGE_WEBUI_PORT 3909)
mailpit_smtp_port=$(port MAILPIT_SMTP_PORT 1025)
mailpit_port=$(port MAILPIT_PORT 8025)
pgweb_port=$(port PGWEB_PORT 8081)

# The password is generated once, then reused from .env.
if [ -f .env ]; then
    password=$(sed -n 's/^POSTGRES_PASSWORD=//p' .env)
elif [ -d data/postgres ]; then
    echo "data/postgres exists but .env is missing: its password is lost. Run: rm -rf data/" >&2
    exit 1
else
    password=$(openssl rand -hex 24)
fi
url="postgres://theca:$password@localhost:$postgres_port/theca"

mkdir -p data/secrets data/garage
printf '%s\n' "$url" > data/secrets/database_url
# Generated once: the server only reads its keys.
for key in server_key data_key; do
    [ -s "data/secrets/$key" ] || openssl rand -hex 32 > "data/secrets/$key"
done

# Garage config, secrets generated once.
if [ ! -f data/garage/garage.toml ]; then
    cat > data/garage/garage.toml <<TOML
metadata_dir = "/var/lib/garage/meta"
data_dir = "/var/lib/garage/data"
db_engine = "sqlite"
replication_factor = 1
rpc_bind_addr = "[::]:3901"
rpc_public_addr = "127.0.0.1:3901"
rpc_secret = "$(openssl rand -hex 32)"

[s3_api]
s3_region = "garage"
api_bind_addr = "[::]:3900"
root_domain = ".s3.garage.localhost"

[admin]
api_bind_addr = "[::]:3903"
admin_token = "$(openssl rand -base64 32)"
TOML
fi

cat > .env <<ENV
# Written by scripts/dev.sh. Ports: edit them here, then run it again.
POSTGRES_PORT=$postgres_port
S3_PORT=$garage_port
GARAGE_WEBUI_PORT=$garage_webui_port
MAILPIT_SMTP_PORT=$mailpit_smtp_port
MAILPIT_PORT=$mailpit_port
PGWEB_PORT=$pgweb_port
POSTGRES_USER=theca
POSTGRES_PASSWORD=$password
POSTGRES_DB=theca
# Read by sqlx's macros at compile time only; the server reads DATABASE_URL__FILE.
DATABASE_URL=$url
DATABASE_URL__FILE=data/secrets/database_url
SERVER_KEY__FILE=data/secrets/server_key
DATA_KEY__FILE=data/secrets/data_key
PUBLIC_URL=http://localhost:8080
# SMTP for tests (Mailpit).
MAILPIT_TEST_HOST=localhost
MAILPIT_TEST_SMTP_PORT=$mailpit_smtp_port
MAILPIT_TEST_API_PORT=$mailpit_port
# S3 backend for tests (Garage).
S3_TEST_HOST=http://localhost:$garage_port
S3_TEST_REGION=garage
S3_TEST_BUCKET=theca
S3_TEST_ACCESS_KEY_FILE=$PWD/data/secrets/s3_access_key
S3_TEST_SECRET_KEY_FILE=$PWD/data/secrets/s3_secret_key
ENV

docker compose up -d --wait
scripts/garage-init.sh data/secrets docker compose exec -T garage /garage
sqlx migrate run --source crates/core/migrations
echo "Dev environment ready: database explorer http://localhost:$pgweb_port, S3 explorer http://localhost:$garage_webui_port, emails http://localhost:$mailpit_port (see .env)"
