#!/usr/bin/env sh
# The release pipeline's npm step, locally and without publishing: in the
# CI image (built for this machine), with DRY_RUN=1. Reads Gitea with your token.
#   GITEA_TOKEN=... scripts/release-local.sh
set -eu
cd "$(dirname "$0")/.."
: "${GITEA_TOKEN:?set GITEA_TOKEN (a Gitea token with package read)}"
rust=$(sed -n 's/^channel = "\(.*\)"$/\1/p' rust-toolchain.toml)
docker build -q -t theca-ci-local --build-arg RUST_VERSION="$rust" ci >/dev/null
docker run --rm -v "$PWD":/src -w /src -v theca-release-target:/target \
    -e CARGO_TARGET_DIR=/target -e SQLX_OFFLINE=true -e DRY_RUN=1 -e GITEA_TOKEN \
    -e CI_COMMIT_BRANCH="$(git branch --show-current)" \
    theca-ci-local scripts/release-ci.sh npm
rm -f dist/npm/.npmrc
