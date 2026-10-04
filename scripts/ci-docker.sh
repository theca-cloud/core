#!/usr/bin/env sh
# Runs the real Woodpecker pipeline locally in Docker (same images as the server).
# Use it when CI fails but scripts/ci.sh passes.
set -eu
cd "$(dirname "$0")/.."

if ! command -v woodpecker-cli >/dev/null 2>&1; then
    echo "woodpecker-cli is missing: see https://woodpecker-ci.org/docs/cli" >&2
    exit 1
fi

# Use the active Docker context (Docker Desktop, OrbStack, ...): woodpecker-cli only
# looks at /var/run/docker.sock otherwise.
export DOCKER_HOST="${DOCKER_HOST:-$(docker context inspect --format '{{.Endpoints.docker.Host}}')}"

woodpecker-cli exec --backend-engine docker .woodpecker/ci.yaml
