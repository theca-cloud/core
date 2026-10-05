#!/usr/bin/env sh
# Uses the versioned hooks in .githooks/ (pre-push runs scripts/ci.sh). Idempotent.
set -eu
cd "$(dirname "$0")/.."
git config core.hooksPath .githooks
echo "Git hooks installed from .githooks/"
