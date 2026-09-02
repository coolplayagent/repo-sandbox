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
  published=$(gh api "repos/${repository}/releases/tags/${tag}" \
    --jq '[.id, .draft, .tag_name] | @tsv')
  IFS=$'\t' read -r release_id release_is_draft release_tag <<< "$published"
  [[ $release_id =~ ^[0-9]+$ && $release_is_draft == false && $release_tag == "$tag" ]] || {
    echo "published release endpoint returned invalid identity, state, or tag" >&2; exit 1;
  }
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

if ! grep -Eq '^HTTP/[^ ]+ 404([[:space:]]|$)' <<< "$release_response"; then
  printf '%s\n' "$release_response" >&2
  exit "$release_status"
fi

# The tag endpoint intentionally excludes drafts. Only after its explicit 404,
# enumerate every release visible to the authenticated write token and compare
# tag_name in Bash, never by interpolating the tag into a jq program.
release_list=$(gh api --paginate "repos/${repository}/releases?per_page=100" \
  --jq '.[] | [.id, .draft, .tag_name] | @tsv')
draft_matches=()
while IFS=$'\t' read -r candidate_id candidate_is_draft candidate_tag; do
  [[ -z $candidate_id && -z $candidate_is_draft && -z $candidate_tag ]] && continue
  if [[ $candidate_tag == "$tag" ]]; then
    [[ $candidate_id =~ ^[0-9]+$ && $candidate_is_draft =~ ^(true|false)$ ]] || {
      echo "release list returned invalid matching metadata" >&2; exit 1;
    }
    draft_matches+=("${candidate_id}"$'\t'"${candidate_is_draft}"$'\t'"${candidate_tag}")
  fi
done <<< "$release_list"

[[ ${#draft_matches[@]} -le 1 ]] || {
  echo "multiple release objects match the canonical tag" >&2; exit 1;
}
if [[ ${#draft_matches[@]} -eq 1 ]]; then
  metadata=${draft_matches[0]}
  IFS=$'\t' read -r release_id release_is_draft release_tag <<< "$metadata"
  [[ $release_is_draft == true && $release_tag == "$tag" ]] || {
    echo "tag endpoint was absent but release list match is not the expected draft" >&2; exit 1;
  }
  git fetch --force --no-tags origin "refs/tags/${tag}:refs/repo-sandbox/publish-tag"
  remote_sha=$(git rev-parse --verify 'refs/repo-sandbox/publish-tag^{commit}')
  [[ $remote_sha == "$expected_sha" ]] || {
    echo "remote tag moved before partial-draft recovery" >&2; exit 1;
  }
  confirmed_metadata=$(gh api "repos/${repository}/releases/${release_id}" \
    --jq '[.id, .draft, .tag_name] | @tsv')
  [[ $confirmed_metadata == "$metadata" ]] || {
    echo "draft identity or state changed before recovery" >&2; exit 1;
  }
  gh api --method DELETE "repos/${repository}/releases/${release_id}"
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
