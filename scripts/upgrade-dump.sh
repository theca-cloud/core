#!/usr/bin/env sh
# Writes the upgrade test's dump of this tree's release into
# crates/core/tests/upgrade/<version>/ (decisions.md, Schema and migrations):
# a fresh server filled through the API by the `fill` test, then its database
# dumped. scripts/release.sh runs it before the version commit.
#   scripts/upgrade-dump.sh 1.2.3
# Needs the dev environment (scripts/dev.sh): Postgres and Mailpit.
set -eu
cd "$(dirname "$0")/.."

version=${1:?usage: scripts/upgrade-dump.sh <version>}
out=crates/core/tests/upgrade/$version
database=upgrade_dump

set -a
. ./.env
set +a
psql() { docker compose exec -T postgres psql -q -U "$POSTGRES_USER" -d postgres -c "$1"; }

rm -rf "$out"
mkdir -p "$out"
psql "DROP DATABASE IF EXISTS $database WITH (FORCE)"
psql "CREATE DATABASE $database"
UPGRADE_DUMP=$PWD/$out UPGRADE_DATABASE=$database \
    cargo test -q -p theca-core --test upgrade fill -- --ignored --exact
# Plain SQL with INSERTs: the test restores it without psql.
docker compose exec -T postgres pg_dump -U "$POSTGRES_USER" --no-owner --no-privileges --inserts \
    "$database" >"$out/dump.sql"
psql "DROP DATABASE $database WITH (FORCE)"
echo "Dump written: $out"
