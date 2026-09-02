#!/usr/bin/env bash
set -euo pipefail

[[ $# == 4 ]] || { echo "usage: $0 TAG REPOSITORY RELEASE_DIR COMMIT_SHA" >&2; exit 64; }
tag=$1
repository=$2
release_dir=$3
expected_sha=$4
root=$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)

"$root/scripts/ci/validate-release.sh" "$tag" >/dev/null
[[ $repository =~ ^[A-Za-z0-9_.-]+/[A-Za-z0-9_.-]+$ ]] || {
  echo "repository must be a canonical owner/name" >&2; exit 64;
}
[[ $expected_sha =~ ^[a-f0-9]{40}$ ]] || { echo "commit SHA must be 40 lowercase hex characters" >&2; exit 64; }
[[ -d $release_dir ]] || { echo "release directory does not exist" >&2; exit 1; }

version=${tag#v}
expected_assets=$(printf '%s\n' \
  "repo-sandbox-${version}-linux-amd64.tar.gz" \
  "repo-sandbox-${version}-linux-amd64.tar.gz.sha256" \
  "repo-sandbox-${version}-linux-arm64.tar.gz" \
  "repo-sandbox-${version}-linux-arm64.tar.gz.sha256" \
  'SHA256SUMS' | LC_ALL=C sort)

assert_asset_set() {
  local directory=$1 actual
  actual=$(find "$directory" -maxdepth 1 -type f -printf '%f\n' | LC_ALL=C sort)
  [[ $actual == "$expected_assets" ]] || {
    echo "release asset set is not the exact two-platform contract" >&2
    return 1
  }
}

assert_asset_set "$release_dir"

# Establish the remote tag target before inspecting release state. Fetch into a
# constant local ref; strict tag validation above prevents refspec injection.
git fetch --force --no-tags origin "refs/tags/${tag}:refs/repo-sandbox/publish-tag"
remote_sha=$(git rev-parse --verify 'refs/repo-sandbox/publish-tag^{commit}')
[[ $remote_sha == "$expected_sha" ]] || {
  echo "remote tag target changed: expected $expected_sha, got $remote_sha" >&2
  exit 1
}

set +e
release_response=$(gh api --include "repos/${repository}/releases/tags/${tag}" 2>&1)
release_status=$?
set -e

if [[ $release_status -eq 0 ]]; then
  metadata=$(gh api "repos/${repository}/releases/tags/${tag}" --jq '[.id, .draft, .tag_name] | @tsv')
  IFS=$'\t' read -r release_id release_is_draft release_tag <<< "$metadata"
  [[ $release_id =~ ^[0-9]+$ && $release_is_draft =~ ^(true|false)$ && $release_tag == "$tag" ]] || {
    echo "release API returned invalid identity, draft state, or tag" >&2; exit 1;
  }

  if [[ $release_is_draft == true ]]; then
    # A partial draft is unpublished and mutable. Delete only that numeric release
    # object (never the tag), then recreate it from the exact current asset set.
    git fetch --force --no-tags origin "refs/tags/${tag}:refs/repo-sandbox/publish-tag"
    remote_sha=$(git rev-parse --verify 'refs/repo-sandbox/publish-tag^{commit}')
    [[ $remote_sha == "$expected_sha" ]] || {
      echo "remote tag moved before partial-draft recovery" >&2; exit 1;
    }
    confirmed_metadata=$(gh api "repos/${repository}/releases/tags/${tag}" \
      --jq '[.id, .draft, .tag_name] | @tsv')
    [[ $confirmed_metadata == "$metadata" ]] || {
      echo "draft identity or state changed before recovery" >&2; exit 1;
    }
    gh api --method DELETE "repos/${repository}/releases/${release_id}"
    release_status=1
    release_response='HTTP/2.0 404 Not Found'
  else
    existing=$(mktemp -d)
    trap 'rm -rf -- "$existing"' EXIT
    gh release download "$tag" --repo "$repository" --dir "$existing"
    assert_asset_set "$existing"
    while IFS= read -r asset; do
      cmp --silent "$release_dir/$asset" "$existing/$asset" || {
        echo "existing release asset differs from this deterministic build: $asset" >&2
        exit 1
      }
    done <<< "$expected_assets"
    echo "existing published release assets exactly match this run; publication is already complete"
    exit 0
  fi
fi

if ! grep -Eq '^HTTP/[^ ]+ 404([[:space:]]|$)' <<< "$release_response"; then
  printf '%s\n' "$release_response" >&2
  exit "$release_status"
fi

# Close the approval/API window: re-fetch and peel the remote tag immediately
# before the write, then refuse publication if it moved since this run was built.
git fetch --force --no-tags origin "refs/tags/${tag}:refs/repo-sandbox/publish-tag"
remote_sha=$(git rev-parse --verify 'refs/repo-sandbox/publish-tag^{commit}')
[[ $remote_sha == "$expected_sha" ]] || {
  echo "remote tag target changed immediately before publication" >&2
  exit 1
}

assets=()
while IFS= read -r asset; do assets+=("$release_dir/$asset"); done <<< "$expected_assets"
gh release create "$tag" "${assets[@]}" \
  --repo "$repository" \
  --verify-tag \
  --generate-notes \
  --notes 'Download the archive for your platform and verify it against SHA256SUMS before execution.'
