#!/usr/bin/env sh
# Releases `dev` into `prod` (or a hotfix already on `prod`): runs scripts/ci.sh,
# merges with --no-ff, bumps the version in Cargo.toml, regenerates openapi.json
# (it carries the version), writes the upgrade test's dump of this release
# (scripts/upgrade-dump.sh), prepends the release to CHANGELOG.md (git-cliff,
# cliff.toml), commits it all as the version (`1.2.3`), tags it, and
# fast-forwards `dev`. Never pushes. The tag's pipeline publishes.
#
# The version comes from the Conventional Commits on `dev` (cliff.toml's [bump]),
# except 1.0.0: going to 1.0.0 is a decision, so it is never automatic.
#   scripts/release.sh            the next version, from the commits
#   scripts/release.sh 1.0.0      that version
#   scripts/release.sh --dry-run  show the version and the changelog only
# Needs the dev environment (scripts/dev.sh): the checks and the dump use it.
set -eu
cd "$(dirname "$0")/.."

die() {
    echo "$1" >&2
    exit 1
}
command -v git-cliff >/dev/null 2>&1 || die "git-cliff is missing (brew install git-cliff)"
branch=$(git branch --show-current)
case "$branch" in
dev | prod) ;; # prod: a hotfix squashed into prod
*) die "releases start from dev (or prod, for a hotfix)" ;;
esac
[ -z "$(git status --porcelain)" ] || die "commit or stash your changes first"
git remote | grep -qx origin && git fetch -q --tags origin

dry=
next=
for arg in "$@"; do
    case "$arg" in
    --dry-run) dry=1 ;;
    *) next=$arg ;;
    esac
done

current=$(sed -n 's/^version = "\(.*\)"$/\1/p' Cargo.toml | head -1)
if [ -z "$next" ]; then
    # 0.x's tags carry a `v` (git-cliff keeps it in the version it prints).
    next=$(git cliff --bumped-version | sed 's/^v//')
    [ "$next" != 1.0.0 ] || die "1.0.0 is your call: scripts/release.sh 1.0.0"
fi
echo "$next" | grep -Eqx '[0-9]+\.[0-9]+\.[0-9]+' || die "not a version: $next"
for tag in "$next" "v$next"; do
    git rev-parse -q --verify "refs/tags/$tag" >/dev/null && die "$next is already released"
done

# The changelog section, written now: dev's commits aren't on prod until the merge.
changes=$(mktemp)
git cliff --unreleased --tag "$next" --strip header >"$changes"

if [ -n "$dry" ]; then
    echo "Next version: $next (now $current)"
    cat "$changes"
    exit 0
fi

scripts/ci.sh || die "scripts/ci.sh failed: nothing released"

if [ "$branch" = dev ]; then
    git checkout -q prod
    git merge -q --no-ff --no-commit dev >/dev/null || die "merge conflict: resolve it, or git merge --abort"
fi

sed -i.bak "s/^version = \"$current\"$/version = \"$next\"/" Cargo.toml
rm Cargo.toml.bak
cargo update -q --workspace
UPDATE_OPENAPI=1 cargo test -q -p theca-core --lib openapi_is_up_to_date >/dev/null
# After the bump: the dump's manifest records the version Cargo.toml says.
scripts/upgrade-dump.sh "$next"
git add "crates/core/tests/upgrade/$next"
if [ -s "$changes" ]; then # empty when only skipped commits (chore, test...)
    # After the title and the line describing the file.
    { head -n 3 CHANGELOG.md; cat "$changes"; echo; tail -n +4 CHANGELOG.md; } >"$changes.new"
    mv "$changes.new" CHANGELOG.md
fi
rm -f "$changes"

git add -u
git commit -q -m "$next"
git tag -a "$next" -m "$next"
git checkout -q dev
if [ "$branch" = dev ]; then
    git merge -q --ff-only prod
    echo "Released $next. Push: git push origin prod dev $next"
else
    echo "Released $next on prod. Bring it into dev: git merge prod, then push prod, dev and $next"
fi
