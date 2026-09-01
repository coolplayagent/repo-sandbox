#!/usr/bin/env bash
set -euo pipefail

[[ $EUID -eq 0 ]] || exec sudo -- "$0" "$@"
[[ $(uname -m) == x86_64 && -n ${WSL_INTEROP:-} ]] || {
  echo "smoke requires an x86_64 WSL2 host" >&2; exit 1
}
repo_root=${1:-$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)}

repo-sandbox doctor
(cd "$repo_root" && bazelisk build //... && bazelisk test //...)

temporary=$(mktemp -d)
trap 'rm -rf -- "$temporary"' EXIT
printf 'FROM busybox:1.36\nRUN uname -m | grep -Eq "^(aarch64|arm64)$"\n' >"$temporary/Dockerfile"

docker buildx build --platform linux/amd64 --load --tag repo-sandbox/issue-14-amd64-smoke "$temporary"
docker run --rm --platform linux/amd64 repo-sandbox/issue-14-amd64-smoke true
docker buildx build --platform linux/arm64 --load --tag repo-sandbox/issue-14-arm64-smoke "$temporary"
docker run --rm --platform linux/arm64 repo-sandbox/issue-14-arm64-smoke sh -c 'uname -m | grep -Eq "^(aarch64|arm64)$"'

echo "amd64 native build and arm64 QEMU smoke passed"
