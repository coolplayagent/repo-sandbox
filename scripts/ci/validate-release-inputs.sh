#!/usr/bin/env bash
set -euo pipefail

[[ $# == 1 ]] || { echo "usage: $0 TAG" >&2; exit 64; }
tag=$1
root=$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd -P)

: "${GITHUB_WORKSPACE:?GITHUB_WORKSPACE is required}"
: "${GITHUB_SHA:?GITHUB_SHA is required}"
workspace=$(cd "$GITHUB_WORKSPACE" && pwd -P)
[[ $workspace == "$root" ]] || {
  echo "GITHUB_WORKSPACE does not identify the checked-out release repository" >&2
  exit 64
}
[[ $GITHUB_SHA =~ ^[0-9a-f]{40}$ ]] || {
  echo "GITHUB_SHA must be a lowercase 40-character commit ID" >&2
  exit 64
}

# A job container can see the checkout as owned by the host runner's uid. Keep
# the ownership exception command-scoped and limited to this exact checkout;
# never persist it or trust every directory via a wildcard exception.
git -C "$root" -c "safe.directory=$root" \
  merge-base --is-ancestor "$GITHUB_SHA" origin/main
"$root/scripts/ci/validate-release.sh" "$tag"
