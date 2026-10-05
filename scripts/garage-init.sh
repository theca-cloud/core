#!/usr/bin/env sh
# Initializes a single-node Garage for dev and CI: layout, bucket "theca", and an
# access key written to $1/s3_access_key and $1/s3_secret_key. Idempotent.
# Usage: garage-init.sh <secrets dir> <garage command...>
#   e.g. garage-init.sh data/secrets docker compose exec -T garage /garage
set -eu
secrets=$1
shift

# Wait for the node (up to 30 s).
attempt=0
until "$@" status >/dev/null 2>&1; do
    attempt=$((attempt + 1))
    [ "$attempt" -lt 30 ] || { echo "garage is not responding" >&2; exit 1; }
    sleep 1
done

node=$("$@" status 2>/dev/null | awk '/NO ROLE ASSIGNED/ {print $1; exit}')
if [ -n "$node" ]; then
    "$@" layout assign -z dc1 -c 1G "$node" >/dev/null 2>&1
    "$@" layout apply --version 1 >/dev/null 2>&1
fi

"$@" bucket info theca >/dev/null 2>&1 || "$@" bucket create theca >/dev/null 2>&1

if [ ! -s "$secrets/s3_access_key" ] || [ ! -s "$secrets/s3_secret_key" ]; then
    mkdir -p "$secrets"
    out=$("$@" key create theca-dev 2>/dev/null)
    printf '%s\n' "$out" | awk '/Key ID/ {print $3}' > "$secrets/s3_access_key"
    printf '%s\n' "$out" | awk '/Secret key/ {print $3}' > "$secrets/s3_secret_key"
    "$@" bucket allow --read --write --owner theca --key "$(cat "$secrets/s3_access_key")" >/dev/null 2>&1
fi
