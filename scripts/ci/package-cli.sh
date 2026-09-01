#!/usr/bin/env bash
set -euo pipefail

[[ $# == 4 ]] || { echo "usage: $0 TAG PLATFORM BINARY OUTPUT_DIRECTORY" >&2; exit 64; }
tag=$1
platform=$2
binary=$3
output_directory=$4
root=$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)
"$root/scripts/ci/validate-release.sh" "$tag"

case "$platform" in
  linux-amd64) expected_machine=x86_64 ;;
  linux-arm64) expected_machine=aarch64 ;;
  *) echo "unsupported release platform: $platform" >&2; exit 64 ;;
esac
[[ $(uname -m) == "$expected_machine" ]] || {
  echo "native runner mismatch for $platform: $(uname -m)" >&2
  exit 1
}
[[ -f "$binary" && -x "$binary" ]] || { echo "CLI binary is not executable: $binary" >&2; exit 1; }

version=${tag#v}
[[ $("$binary" --version) == "repo-sandbox $version" ]] || {
  echo "CLI --version does not match $tag" >&2
  exit 1
}

mkdir -p -- "$output_directory"
stage=$(mktemp -d)
cleanup() { rm -rf -- "$stage"; }
trap cleanup EXIT
install -m 0755 -- "$binary" "$stage/repo-sandbox"
archive="repo-sandbox-${version}-${platform}.tar.gz"
tar --sort=name --mtime='UTC 1970-01-01' --owner=0 --group=0 --numeric-owner \
  -czf "$output_directory/$archive" -C "$stage" repo-sandbox
(cd "$output_directory" && sha256sum "$archive" > "${archive}.sha256")
