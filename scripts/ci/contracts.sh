#!/usr/bin/env bash
set -euo pipefail

root=$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)
ci="$root/.github/workflows/ci.yml"
release="$root/.github/workflows/release.yml"
adapters_build="$root/crates/adapters/BUILD.bazel"
cli_build="$root/crates/cli/BUILD.bazel"

for required in "$ci" "$release" "$root/scripts/ci/workspace-version.sh" \
  "$root/scripts/ci/download-release-artifacts.sh" \
  "$root/scripts/ci/validate-release.sh" "$root/scripts/ci/validate-release-inputs.sh" \
  "$root/scripts/ci/package-cli.sh" \
  "$root/scripts/ci/publish-release.sh" "$root/scripts/ci/verify-release.sh" \
  "$root/scripts/ci/verify-rocky-cxx.sh" "$root/scripts/ci/release-bazel.sh"; do
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
rocky_image='rockylinux/rockylinux:8.10@sha256:e8a49c5403b687db05d4d67333fa45808fbe74f36e683cec7abb1f7d0f2338c6'
[[ $(grep -c "container: $rocky_image" "$release") -eq 2 ]]
rocky_prerequisites='dnf install --assumeyes binutils ca-certificates curl findutils gcc gcc-c++ git gzip python3 tar zstd'
[[ $(grep -Fc "$rocky_prerequisites" "$release") -eq 1 ]]
grep -Fq 'run: scripts/ci/verify-rocky-cxx.sh' "$release"
grep -Fq 'rpm -q gcc-c++' "$root/scripts/ci/verify-rocky-cxx.sh"
grep -Fq '[[ $libstdcxx != libstdc++.so && -f $libstdcxx ]]' \
  "$root/scripts/ci/verify-rocky-cxx.sh"
grep -Fq '^(gcc-c\+\+|libstdc\+\+-devel)-' "$root/scripts/ci/verify-rocky-cxx.sh"
! grep -Fq 'cargo build' "$release"
grep -Fq 'binary=$(bazelisk cquery --config=release //:repo-sandbox --output=files' "$release"
grep -Fq 'verify-glibc-baseline.sh" "$binary" 2.28' "$root/scripts/ci/package-cli.sh"
grep -Fq 'verify-glibc-baseline.sh" "$temporary/repo-sandbox" 2.28' \
  "$root/scripts/ci/verify-release.sh"
grep -Fq 'raw.githubusercontent.com/${GITHUB_REPOSITORY}/${GITHUB_SHA}/scripts/ci/verify-glibc-baseline.sh' \
  "$release"
grep -Fq 'version=$(scripts/ci/workspace-version.sh)' "$ci"
! grep -Fq "grep -Fx 'repo-sandbox 0.1.0'" "$ci"
grep -Fq 'bash -n scripts/ci/*.sh scripts/e2e/*.sh scripts/docker/multistage-acceptance.sh' "$ci"
grep -Fq 'publish-release.sh "$GITHUB_REF_NAME" "$GITHUB_REPOSITORY" release "$GITHUB_SHA"' "$release"
grep -Fq 'actions: read' "$release"
grep -Fq 'download-release-artifacts.sh "$GITHUB_REF_NAME" "$GITHUB_REPOSITORY" "$GITHUB_RUN_ID" "$GITHUB_RUN_ATTEMPT" release' "$release"
[[ $(grep -c 'validate-release-inputs.sh "$GITHUB_REF_NAME"' "$release") -eq 2 ]]
! grep -Fq 'git merge-base --is-ancestor "$GITHUB_SHA" origin/main' "$release"
grep -Fq 'git -C "$root" -c "safe.directory=$root"' \
  "$root/scripts/ci/validate-release-inputs.sh"
if grep -Rq --exclude=contracts.sh 'safe.directory=\*' "$root/.github" "$root/scripts/ci"; then
  echo 'wildcard safe.directory exception is forbidden' >&2
  exit 1
fi
if grep -REq --exclude=contracts.sh \
  'git[[:space:]]+config[[:space:]]+--global.*safe\.directory' \
  "$root/.github" "$root/scripts/ci"; then
  echo 'persistent global safe.directory configuration is forbidden' >&2
  exit 1
fi
! grep -Fq 'pattern: cli-*-${{ github.run_id }}-${{ github.run_attempt }}' "$release"
! grep -Fq 'gh release create' "$release"
grep -Fq 'refs/tags/${tag}:refs/repo-sandbox/publish-tag' "$root/scripts/ci/publish-release.sh"
[[ $(grep -c "remote_sha == \"\$expected_sha\"" "$root/scripts/ci/publish-release.sh") -eq 3 ]]
grep -Fq 'cmp --silent' "$root/scripts/ci/publish-release.sh"
grep -Fq "404([[:space:]]|\$)" "$root/scripts/ci/publish-release.sh"
grep -Fq 'release_is_draft == true' "$root/scripts/ci/publish-release.sh"
grep -Fq 'release_tag == "$tag"' "$root/scripts/ci/publish-release.sh"
grep -Fq 'confirmed_metadata == "$metadata"' "$root/scripts/ci/publish-release.sh"
grep -Fq 'gh api --paginate "repos/${repository}/releases?per_page=100"' \
  "$root/scripts/ci/publish-release.sh"
grep -Fq -- "--jq '.[] | [.id, .draft, .tag_name] | @tsv'" "$root/scripts/ci/publish-release.sh"
! grep -Eq -- '--jq.*\$tag' "$root/scripts/ci/publish-release.sh"
grep -Fq 'gh api --method DELETE "repos/${repository}/releases/${release_id}"' \
  "$root/scripts/ci/publish-release.sh"
! grep -Fq -- '--cleanup-tag' "$root/scripts/ci/publish-release.sh"

# The adapter's include_str! tests must stay hermetic without broad workspace data.
grep -Fq '"//scripts/docker:multistage-acceptance"' "$adapters_build"
grep -Fq '"//templates:rust-bazel-dockerfile"' "$adapters_build"
grep -Fq 'srcs = ["multistage-acceptance.sh"]' "$root/scripts/docker/BUILD.bazel"
grep -Fq 'srcs = ["rust-bazel/context/Dockerfile"]' "$root/templates/BUILD.bazel"
grep -A5 -F 'name = "e2e_matrix_test"' "$adapters_build" | grep -Fq 'srcs = ["e2e/e2e_matrix.rs"]'
grep -Fq 'bazelisk test --action_env=PATH //...' "$ci"
grep -Fq -- '- name: Release Bazel target selection' "$ci"
[[ $(grep -Fhc 'run: scripts/ci/release-bazel.sh' "$ci" "$release" | \
  awk '{ total += $1 } END { print total }') -eq 2 ]]
grep -Fq "bazelisk query 'kind(\".*_test rule\", //...)' --output=label" \
  "$root/scripts/ci/release-bazel.sh"
grep -Fq '[[ ${#test_targets[@]} -gt 0 ]]' "$root/scripts/ci/release-bazel.sh"
grep -Fq 'bazelisk test --action_env=PATH "${test_targets[@]}"' \
  "$root/scripts/ci/release-bazel.sh"
! grep -Fq 'bazelisk test --action_env=PATH //...' "$release" "$root/scripts/ci/release-bazel.sh"
if grep -Eq '(^|[[:space:]])(cargo|rustc)([[:space:]]|$)' \
  "$release" "$root/scripts/ci/release-bazel.sh"; then
  echo 'release build must not install or invoke host Cargo/Rust' >&2
  exit 1
fi

# Both the CLI library and binary directly import clap; keep exactly those direct deps.
[[ $(grep -c '"@crates//:clap"' "$cli_build") -eq 2 ]]
! grep -Fq 'all_crate_deps' "$cli_build"

# Keep every required scenario budget stable: this change deliberately grants only
# the dual-architecture dogfood scenario more time on hosted QEMU runners.
scenario_timeouts=$(awk '
  /- id: / { id=$3 }
  /timeout_seconds:/ { print id "=" $2 }
' "$root/tests/e2e/scenarios.yaml")
expected_scenario_timeouts=$(printf '%s\n' \
  'public-git-snapshot=60' \
  'private-https-snapshot=120' \
  'private-ssh-snapshot=120' \
  'private-https-invalid-auth=120' \
  'docker-adapters=600' \
  'docker-failures=180' \
  'docker-architecture-mismatch=120' \
  'rust-bazel-dogfood=3300' \
  'registry-publish-pull=600' \
  'euleros-wsl-dogfood=1800' \
  'euleros-hce-vm-matrix=3600')
[[ "$scenario_timeouts" == "$expected_scenario_timeouts" ]]

dogfood_timeout=3300
docker_job_minutes=$(awk '/^  docker-required:/{found=1} found && /timeout-minutes:/{print $2; exit}' \
  "$ci")
[[ $((docker_job_minutes * 60)) -ge $((dogfood_timeout + 600)) ]]

while IFS= read -r action; do
  [[ $action =~ @([a-f0-9]{40})([[:space:]]|$) ]] || {
    echo "GitHub Action is not pinned to a full commit: $action" >&2
    exit 1
  }
done < <(grep -RhE '^\s*uses:' "$root/.github/workflows")

workspace_version=$("$root/scripts/ci/workspace-version.sh")
"$root/scripts/ci/validate-release.sh" "v$workspace_version" >/dev/null
for malicious in "v${workspace_version};id" "v${workspace_version}/../../escape" \
  "v${workspace_version}-rc.1" "V$workspace_version" 'v00.1.0'; do
  if "$root/scripts/ci/validate-release.sh" "$malicious" >/dev/null 2>&1; then
    echo "unsafe release tag accepted: $malicious" >&2
    exit 1
  fi
done

if "$root/scripts/ci/package-cli.sh" "v$workspace_version" 'linux-amd64/../../escape' /does/not/exist /tmp/out \
  >/dev/null 2>&1; then
  echo 'unsafe release platform accepted' >&2
  exit 1
fi

"$root/scripts/ci/workspace-version-contract.sh" >/dev/null
"$root/scripts/ci/release-publish-contract.sh" >/dev/null
"$root/scripts/ci/glibc-baseline-contract.sh" >/dev/null
"$root/scripts/ci/download-release-artifacts-contract.sh" >/dev/null
"$root/scripts/ci/release-inputs-contract.sh" >/dev/null
"$root/scripts/ci/rocky-cxx-contract.sh" >/dev/null
"$root/scripts/ci/release-bazel-contract.sh" >/dev/null

echo 'CI permission, fork, cache, tag, platform, checksum, and fresh-machine contracts passed'
