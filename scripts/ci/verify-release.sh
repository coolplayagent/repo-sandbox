#!/usr/bin/env bash
set -euo pipefail

[[ $# == 3 ]] || { echo "usage: $0 TAG PLATFORM RELEASE_BASE_URL" >&2; exit 64; }
tag=$1
platform=$2
base_url=$3
[[ $tag =~ ^v(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)$ ]] || {
  echo 'release tag must be canonical vMAJOR.MINOR.PATCH' >&2; exit 64;
}
case "$platform" in linux-amd64|linux-arm64) ;; *) echo 'unsupported release platform' >&2; exit 64 ;; esac
[[ $base_url =~ ^https://github\.com/[A-Za-z0-9_.-]+/[A-Za-z0-9_.-]+/releases/download$ ]] || {
  echo 'release base URL must be a canonical GitHub repository URL' >&2; exit 64;
}

version=${tag#v}
archive="repo-sandbox-${version}-${platform}.tar.gz"
checksum="${archive}.sha256"
temporary=$(mktemp -d)
cleanup() { rm -rf -- "$temporary"; }
trap cleanup EXIT
for asset in "$archive" "$checksum"; do
  curl --fail --location --proto '=https' --tlsv1.2 --retry 5 --retry-all-errors \
    --output "$temporary/$asset" "$base_url/$tag/$asset"
done

checksum_line=$(cat "$temporary/$checksum")
[[ $checksum_line =~ ^[a-f0-9]{64}\ \ repo-sandbox-[0-9]+\.[0-9]+\.[0-9]+-linux-(amd64|arm64)\.tar\.gz$ ]] || {
  echo 'malformed release checksum manifest' >&2; exit 1;
}
[[ ${checksum_line#*  } == "$archive" ]] || { echo 'checksum names a different asset' >&2; exit 1; }
(cd "$temporary" && printf '%s\n' "$checksum_line" | sha256sum --check --strict)
[[ $(tar -tzf "$temporary/$archive") == repo-sandbox ]] || {
  echo 'archive must contain exactly the repo-sandbox executable' >&2; exit 1;
}
tar -xzf "$temporary/$archive" -C "$temporary" -- repo-sandbox
[[ $("$temporary/repo-sandbox" --version) == "repo-sandbox $version" ]] || {
  echo 'downloaded CLI version does not match release tag' >&2; exit 1;
}
echo "verified $archive on a fresh machine"
