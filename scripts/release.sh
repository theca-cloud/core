#!/usr/bin/env sh
# Prepares a release on `dev`, or `prod` for a hotfix (docs/CONTRIBUTING.md,
# Releases): the next
# version from the Conventional Commits since the last release (git-cliff),
# written to Cargo.toml (the workspace's version and its own crates'),
# openapi.json (it carries the version), CHANGELOG.md, committed as
# `chore(release): X.Y.Z`. Then merge `dev` into
# `prod` (git merge --no-ff dev): its pipeline publishes.
#   scripts/release.sh            prepare and commit
#   scripts/release.sh --dry-run  show the version and the changelog only
set -eu
cd "$(dirname "$0")/.."

die() {
    echo "$1" >&2
    exit 1
}
command -v git-cliff >/dev/null 2>&1 || die "git-cliff is missing (brew install git-cliff)"
case "$(git branch --show-current)" in
dev | prod) ;; # prod: a hotfix
*) die "releases are prepared on dev (or prod, for a hotfix)" ;;
esac
[ -z "$(git status --porcelain)" ] || die "commit or stash your changes first"
git fetch -q --tags origin

current=$(sed -n 's/^version = "\(.*\)"$/\1/p' Cargo.toml | head -1)
if git tag --list 'v*' | grep -q .; then
    next=$(git cliff --bumped-version | sed 's/^v//')
else
    next=$current # the first release: the version Cargo.toml already has
fi
git tag --list | grep -qx "v$next" && die "v$next is already released: nothing new to release"

if [ "${1:-}" = --dry-run ]; then
    echo "Next version: $next (now $current)"
    git cliff --unreleased --tag "v$next" --strip header
    exit 0
fi

# The workspace's version, and its crates' versions as dependencies.
sed -i.bak \
    -e "s/^version = \"$current\"$/version = \"$next\"/" \
    -e "s/^\(theca-[a-z]* = {.*version = \"\)$current\"/\1$next\"/" \
    Cargo.toml
rm Cargo.toml.bak
cargo update -q --workspace
UPDATE_OPENAPI=1 cargo test -q -p theca-core --lib openapi_is_up_to_date >/dev/null
git cliff --tag "v$next" -o CHANGELOG.md
git add Cargo.toml Cargo.lock openapi.json CHANGELOG.md
git commit -q -m "chore(release): $next"
echo "Release $next prepared on dev. Next: push dev, wait for its CI, then"
echo "  git checkout prod && git merge --no-ff dev && git push"
