#!/usr/bin/env bash
set -euo pipefail

expected=
repo_root=${REPO_SANDBOX_SOURCE:-$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)}
while (($#)); do
  case $1 in
    --expected-arch) expected=${2-}; shift 2 ;;
    --source) repo_root=${2-}; shift 2 ;;
    *) echo "unknown argument: $1" >&2; exit 64 ;;
  esac
done
[[ $expected == amd64 || $expected == arm64 ]] || { echo '--expected-arch must be amd64 or arm64' >&2; exit 64; }
case $(uname -m) in x86_64) actual=amd64 ;; aarch64) actual=arm64 ;; *) actual=unsupported ;; esac
[[ $actual == "$expected" ]] || { echo "native architecture mismatch: expected $expected, got $actual" >&2; exit 1; }
case ${DOCKER_HOST:-unix:///var/run/docker.sock} in unix://*|ssh://*) ;; *) echo 'unsafe DOCKER_HOST refused' >&2; exit 1 ;; esac

repo-sandbox doctor
(cd "$repo_root" && bazelisk build //... && bazelisk test //...)
(cd "$repo_root" && cargo test -p repo-sandbox-adapters docker_buildx_cold_and_warm_smoke -- --ignored --nocapture)
(cd "$repo_root" && cargo test -p repo-sandbox-adapters docker_one_shot_job_smoke -- --ignored --nocapture)

echo "EulerOS/HCE native $expected install, doctor, build, test, one-shot runner and owned cleanup passed"
