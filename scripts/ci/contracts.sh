#!/usr/bin/env bash
set -euo pipefail

root=$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)
ci="$root/.github/workflows/ci.yml"
release="$root/.github/workflows/release.yml"
adapters_build="$root/crates/adapters/BUILD.bazel"
cli_build="$root/crates/cli/BUILD.bazel"

for required in "$ci" "$release" "$root/scripts/ci/validate-release.sh" \
  "$root/scripts/ci/package-cli.sh" "$root/scripts/ci/verify-release.sh"; do
  [[ -f $required ]] || { echo "missing CI contract: $required" >&2; exit 1; }
done

grep -Eq '^  pull_request:$' "$ci"
! grep -Rq 'pull_request_target' "$root/.github/workflows"
! grep -Rq '\${{ *secrets\.' "$root/.github/workflows"
grep -Fq 'permissions:' "$ci"
grep -Fq 'contents: read' "$ci"
grep -Fq 'persist-credentials: false' "$ci"
grep -Fq 'bazel-ci-v1' "$ci"
grep -Fq 'ci-quota-docker.sh builder-name' "$ci"
grep -Fq 'builder=$(builder_name)' "$root/scripts/e2e/ci-quota-docker.sh"
[[ $("$root/scripts/e2e/ci-quota-docker.sh" builder-name "${TMPDIR:-/tmp}" 12345 6) == \
  buildkit-e2e-v1-12345-6 ]]
grep -Fq 'contents: write' "$release"
[[ $(grep -c 'contents: write' "$release") -eq 1 ]]
grep -Fq 'environment: release' "$release"
grep -Fq 'ubuntu-24.04-arm' "$release"
grep -Fq 'SHA256SUMS' "$release"
grep -Fq 'container: ubuntu:24.04' "$release"

# The adapter's include_str! tests must stay hermetic without broad workspace data.
grep -Fq '"//scripts/docker:multistage-acceptance"' "$adapters_build"
grep -Fq '"//templates:rust-bazel-dockerfile"' "$adapters_build"
grep -Fq 'srcs = ["multistage-acceptance.sh"]' "$root/scripts/docker/BUILD.bazel"
grep -Fq 'srcs = ["rust-bazel/context/Dockerfile"]' "$root/templates/BUILD.bazel"

# Both the CLI library and binary directly import clap; keep exactly those direct deps.
[[ $(grep -c '"@crates//:clap"' "$cli_build") -eq 2 ]]
! grep -Fq 'all_crate_deps' "$cli_build"

while IFS= read -r action; do
  [[ $action =~ @([a-f0-9]{40})([[:space:]]|$) ]] || {
    echo "GitHub Action is not pinned to a full commit: $action" >&2
    exit 1
  }
done < <(grep -RhE '^\s*uses:' "$root/.github/workflows")

"$root/scripts/ci/validate-release.sh" v0.1.0 >/dev/null
for malicious in 'v0.1.0;id' 'v0.1.0/../../escape' 'v0.1.0-rc.1' 'V0.1.0' 'v00.1.0'; do
  if "$root/scripts/ci/validate-release.sh" "$malicious" >/dev/null 2>&1; then
    echo "unsafe release tag accepted: $malicious" >&2
    exit 1
  fi
done

if "$root/scripts/ci/package-cli.sh" v0.1.0 'linux-amd64/../../escape' /does/not/exist /tmp/out \
  >/dev/null 2>&1; then
  echo 'unsafe release platform accepted' >&2
  exit 1
fi

echo 'CI permission, fork, cache, tag, platform, checksum, and fresh-machine contracts passed'
