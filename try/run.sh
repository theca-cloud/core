#!/usr/bin/env sh
# A local Theca to try as a user (README.md): the released image, set up with
# an admin, a "Documents" library and an app password for WebDAV.
set -eu
cd "$(dirname "$0")"

url=http://localhost:8090
email=admin@example.com
password=theca-try-password
credentials=data/credentials.txt

case "${1:-}" in
"") ;;
--down)
    HOST_USER= docker compose down
    exit 0
    ;;
--clean)
    HOST_USER= docker compose down
    rm -rf data
    exit 0
    ;;
--help | -h)
    echo "usage: ./run.sh [--down | --clean]"
    echo "  (none)   start (and set up on the first run), print the credentials"
    echo "  --down   stop, keeping the data"
    echo "  --clean  stop and delete everything (database, files, secrets)"
    exit 0
    ;;
*)
    echo "unknown option: $1 (see --help)" >&2
    exit 2
    ;;
esac

mkdir -p data/secrets data/db data/files
export HOST_USER="$(id -u):$(id -g)"
for key in server.key data.key; do
    [ -s "data/secrets/$key" ] || openssl rand -hex 32 >"data/secrets/$key"
done
printf postgres://theca:theca@postgres:5432/theca >data/secrets/database_url
chmod 644 data/secrets/*

docker compose pull -q theca
docker compose up -d --wait

api() { # method path [json]
    curl -fsS -X "$1" -b data/cookies -c data/cookies -H 'Content-Type: application/json' \
        ${3:+-d "$3"} "$url/api/v1$2"
}

if api GET /setup | jq -e .required >/dev/null; then
    echo "Setting up..."
    api POST /setup "$(jq -n --arg e "$email" --arg p "$password" '{admin_email: $e,
        admin_password: $p, organization: "Demo",
        storages: [{name: "local", implementation: "filesystem", path: "/files"}]}')" >/dev/null
    org=$(api POST /auth/login "$(jq -n --arg e "$email" --arg p "$password" '{email: $e, password: $p}')" |
        jq -r '.organizations[0].organization')
    api POST /libraries "$(jq -n --arg o "$org" '{organization: $o, name: "Documents"}')" >/dev/null
    app=$(api POST /auth/app-passwords '{"name": "WebDAV"}' | jq -r .password)
    cat >"$credentials" <<EOF
Web API:  $url/api/v1
Admin:    $email / $password
WebDAV:   $url/dav/ (any username, password: $app)
EOF
fi
cat "$credentials"
