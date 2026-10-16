#!/usr/bin/env sh
# The pipelines' steps the YAML can't say (.woodpecker/rc.yaml, release.yaml).
# The version is Cargo.toml's (written by scripts/release.sh).
#   binaries  static binaries for amd64 and arm64 in dist/ (cargo zigbuild), for
#             the core image: web's dev and test server, never shipped (the CE
#             image compiles its own, natively per architecture)
#   npm       @theca/client on the Gitea npm registry (needs GITEA_TOKEN)
#             DRY_RUN=1: packs it, publishes nothing (scripts/release-local.sh)
set -eu
cd "$(dirname "$0")/.."

gitea=https://git.pband.ch
version=$(sed -n 's/^version = "\(.*\)"$/\1/p' Cargo.toml | head -1)

case "${1:-}" in
binaries)
    cargo zigbuild --release --locked -p theca-core --bin theca-server \
        --target x86_64-unknown-linux-musl --target aarch64-unknown-linux-musl
    mkdir -p dist/amd64 dist/arm64
    cp target/x86_64-unknown-linux-musl/release/theca-server dist/amd64/
    cp target/aarch64-unknown-linux-musl/release/theca-server dist/arm64/
    # amd64 runs here (an unknown command exits 2 after printing the usage);
    # arm64 must at least be an aarch64 ELF (machine 0xB7 at offset 18).
    status=0
    dist/amd64/theca-server version-check 2>/dev/null || status=$?
    [ "$status" = 2 ] || { echo "the amd64 binary doesn't run" >&2; exit 1; }
    [ "$(od -An -tx1 -j18 -N1 dist/arm64/theca-server | tr -d ' ')" = b7 ] ||
        { echo "the arm64 binary isn't aarch64" >&2; exit 1; }
    ls -l dist/*/theca-server
    ;;
npm)
    # The browser package, as @theca/client.
    wasm-pack build crates/client --release --target web --scope theca --out-dir ../../dist/npm
    sed -i 's|"name": "@theca/theca-client"|"name": "@theca/client"|' dist/npm/package.json
    # The password scorer beside it, its own WASM (zxcvbn's weight stays out of
    # the engine): @theca/client/password/theca_password.js.
    wasm-pack build crates/password --release --target web --out-dir ../../dist/npm/password
    rm dist/npm/password/package.json dist/npm/password/.gitignore
    jq '.files += ["password"]' dist/npm/package.json >dist/npm/package.json.new
    mv dist/npm/package.json.new dist/npm/package.json
    registry=${gitea#https:}/api/packages/theca/npm/
    printf '%s:_authToken=%s\n' "$registry" "$GITEA_TOKEN" >dist/npm/.npmrc
    # Already published by a failed run: skipped, so the pipeline can be re-run.
    if curl -fsS -o /dev/null -H "Authorization: token $GITEA_TOKEN" \
        "$gitea/api/v1/packages/theca/npm/%40theca%2Fclient/$version" 2>/dev/null; then
        echo "@theca/client $version: already published"
    else
        (cd dist/npm && npm publish --registry "https:$registry" ${DRY_RUN:+--dry-run})
    fi
    ;;
*)
    echo "usage: scripts/release-ci.sh binaries|npm" >&2
    exit 2
    ;;
esac
