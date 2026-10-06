#!/usr/bin/env sh
# The release pipeline's steps (.woodpecker/release.yaml), on `dev` (release
# candidates) and `prod` (releases). The version is Cargo.toml's (written by
# scripts/release.sh). Needs GITEA_TOKEN for `check` and `publish`.
#   binaries  static binaries for amd64 and arm64 in dist/ (cargo zigbuild)
#   tags      the image tags in .tags (refuses an already released version)
#   publish   crates, browser package, tag and Gitea release (prod)
#             DRY_RUN=1: packages only, prints the release (scripts/release-local.sh)
set -eu
cd "$(dirname "$0")/.."

gitea=https://git.pband.ch
repo=theca/core
version=$(sed -n 's/^version = "\(.*\)"$/\1/p' Cargo.toml | head -1)
branch=${CI_COMMIT_BRANCH:-$(git branch --show-current)}

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
tags)
    major=${version%%.*}
    minor=${version#*.}
    minor=$major.${minor%%.*}
    if [ "$branch" = prod ]; then
        if curl -fsS -o /dev/null -H "Authorization: token $GITEA_TOKEN" \
            "$gitea/api/v1/repos/$repo/releases/tags/v$version"; then
            echo "v$version is already released: run scripts/release.sh on dev first" >&2
            exit 1
        fi
        printf '%s\n' latest "$major" "$minor" "$version" >.tags
    else
        printf '%s\n' rc "$major-rc" "$minor-rc" "$version-rc" >.tags
    fi
    cat .tags
    ;;
publish)
    # Crates, in dependency order (already tested: no rebuild).
    export CARGO_REGISTRIES_GITEA_TOKEN="Bearer $GITEA_TOKEN"
    # What a failed run already published is skipped, so the pipeline can be re-run.
    published() {
        curl -fsS -o /dev/null -H "Authorization: token $GITEA_TOKEN" \
            "$gitea/api/v1/packages/theca/$1/$2/$version" 2>/dev/null
    }
    for crate in theca-common theca-client theca-core; do
        if published cargo "$crate"; then
            echo "$crate $version: already published"
        else
            cargo publish -q --locked --no-verify --registry gitea -p "$crate" ${DRY_RUN:+--dry-run}
        fi
    done
    # The browser package, as @theca/client.
    wasm-pack build crates/client --release --target web --scope theca --out-dir ../../dist/npm
    sed -i 's|"name": "@theca/theca-client"|"name": "@theca/client"|' dist/npm/package.json
    registry=${gitea#https:}/api/packages/theca/npm/
    printf '%s:_authToken=%s\n' "$registry" "$GITEA_TOKEN" >dist/npm/.npmrc
    if published npm %40theca%2Fclient; then
        echo "@theca/client $version: already published"
    else
        (cd dist/npm && npm publish --registry "https:$registry" ${DRY_RUN:+--dry-run})
    fi
    # The tag and the release, with this version's changelog and openapi.json.
    notes=$(awk -v v="## [$version]" 'index($0, v) == 1 {on = 1; next} /^## \[/ {on = 0} on' CHANGELOG.md)
    release=$(jq -n --arg tag "v$version" --arg sha "${CI_COMMIT_SHA:-$(git rev-parse HEAD)}" --arg body "$notes" \
        '{tag_name: $tag, target_commitish: $sha, name: $tag, body: $body}')
    if [ -n "${DRY_RUN:-}" ]; then
        echo "$release"
        exit 0
    fi
    id=$(echo "$release" |
        curl -fsS -X POST -H "Authorization: token $GITEA_TOKEN" -H 'Content-Type: application/json' \
            --data-binary @- "$gitea/api/v1/repos/$repo/releases" | jq -r .id)
    curl -fsS -X POST -H "Authorization: token $GITEA_TOKEN" -F attachment=@openapi.json \
        "$gitea/api/v1/repos/$repo/releases/$id/assets?name=openapi.json" >/dev/null
    echo "Released v$version"
    ;;
*)
    echo "usage: scripts/release-ci.sh binaries|tags|publish" >&2
    exit 2
    ;;
esac
