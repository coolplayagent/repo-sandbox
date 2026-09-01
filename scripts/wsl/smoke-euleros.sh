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
run_id=$(cat /proc/sys/kernel/random/uuid)
amd64_tag="repo-sandbox/issue-14-smoke:amd64-$run_id"
arm64_tag="repo-sandbox/issue-14-smoke:arm64-$run_id"

cleanup() {
  local tag owner
  for tag in "$amd64_tag" "$arm64_tag"; do
    owner=$(docker image inspect --format '{{ index .Config.Labels "io.repo-sandbox.smoke.issue-14" }}' "$tag" 2>/dev/null || true)
    if [[ $owner == "$run_id" ]]; then
      docker image rm -- "$tag" >/dev/null || true
    fi
  done
  rm -rf -- "$temporary"
}
trap cleanup EXIT

cat >"$temporary/Dockerfile" <<'EOF'
FROM busybox:1.36
ARG TARGETARCH
ARG SMOKE_RUN_ID
LABEL io.repo-sandbox.smoke.issue-14=$SMOKE_RUN_ID
RUN case "$TARGETARCH" in \
      amd64) test "$(uname -m)" = x86_64 ;; \
      arm64) test "$(uname -m)" = aarch64 ;; \
      *) exit 1 ;; \
    esac
EOF

docker buildx build --platform linux/amd64 --build-arg "SMOKE_RUN_ID=$run_id" --load --tag "$amd64_tag" "$temporary"
docker run --rm --platform linux/amd64 "$amd64_tag" sh -c 'test "$(uname -m)" = x86_64'
docker buildx build --platform linux/arm64 --build-arg "SMOKE_RUN_ID=$run_id" --load --tag "$arm64_tag" "$temporary"
docker run --rm --platform linux/arm64 "$arm64_tag" sh -c 'test "$(uname -m)" = aarch64'

echo "amd64 native build and arm64 QEMU smoke passed"
