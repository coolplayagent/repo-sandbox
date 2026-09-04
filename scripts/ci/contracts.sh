#!/usr/bin/env bash
set -euo pipefail

root=$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)
ci="$root/.github/workflows/ci.yml"
release="$root/.github/workflows/release.yml"
adapters_build="$root/crates/adapters/BUILD.bazel"
cli_build="$root/apps/cli/BUILD.bazel"
environment_dockerfile="$root/templates/rust-bazel/context/Dockerfile"
bazel_wrapper="$root/templates/rust-bazel/context/bazel"
baseline_lock="$root/templates/rust-bazel/context/offline-baseline/MODULE.bazel.lock"

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
grep -Fq 'tools/release/package_cli.py' "$root/scripts/ci/package-cli.sh"
grep -Fq 'check_glibc(binary)' "$root/tools/release/package_cli.py"
grep -Fq 'verify-glibc-baseline.sh" "$temporary/repo-sandbox" 2.28' \
  "$root/scripts/ci/verify-release.sh"
grep -Fq 'raw.githubusercontent.com/${GITHUB_REPOSITORY}/${RELEASE_COMMIT}/scripts/ci/verify-glibc-baseline.sh' \
  "$release"
grep -Fq 'version=$(scripts/ci/workspace-version.sh)' "$ci"
grep -Fq 'docker buildx inspect "$builder" --bootstrap' "$ci"
grep -Fq "Platforms:.*linux/amd64" "$ci"
grep -Fq 'REPO_SANDBOX_E2E_PROFILE_SECRET: repo-sandbox-ci-profile-secret-not-a-credential' "$ci"
! grep -Fq "grep -Fx 'repo-sandbox 0.1.0'" "$ci"
grep -Fq 'bash -n scripts/ci/*.sh scripts/e2e/*.sh scripts/docker/multistage-acceptance.sh' "$ci"
grep -Fq 'publish-release.sh "${{ needs.prepare.outputs.tag }}" "$GITHUB_REPOSITORY" release "${{ needs.prepare.outputs.commit }}"' "$release"
publish_line=$(grep -nF 'publish-release.sh "${{ needs.prepare.outputs.tag }}" "$GITHUB_REPOSITORY" release "${{ needs.prepare.outputs.commit }}"' "$release" | cut -d: -f1)
attest_line=$(grep -nF 'uses: actions/attest-build-provenance@' "$release" | cut -d: -f1)
(( publish_line < attest_line ))
grep -Fq 'actions: read' "$release"
grep -Fq 'download-release-artifacts.sh "${{ needs.prepare.outputs.tag }}" "$GITHUB_REPOSITORY" "$GITHUB_RUN_ID" "$GITHUB_RUN_ATTEMPT" release' "$release"
[[ $(grep -c 'validate-release-inputs.sh' "$release") -eq 3 ]]
grep -Fq 'workflow_dispatch:' "$release"
grep -Fq "group: release-\${{ github.event_name == 'workflow_dispatch' && inputs.tag || github.ref_name }}" "$release"
grep -Fq 'ref: refs/tags/${{ steps.requested.outputs.tag }}' "$release"
grep -Fq '[[ $GITHUB_EVENT_NAME == workflow_dispatch && $GITHUB_SHA != "$commit" ]]' "$release"
grep -Fq 'manual release must be dispatched with --ref $RELEASE_TAG' "$release"
grep -Fq 'merge-base --is-ancestor "$commit" origin/main' "$release"
grep -Fq 'uses: actions/attest-build-provenance@a2bbfa25375fe432b6a289bc6b6cd05ecd0c4c32 # v4.1.0' "$release"
grep -Fq 'subject-path: release/repo-sandbox-*.tar.gz' "$release"
grep -Fq 'artifact-metadata: write' "$release"
grep -Fq 'cmp "release-a/$archive" "release-b/$archive"' "$release"
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
grep -Fq '"//templates:rust-bazel-offline-baseline"' "$adapters_build"
grep -Fq 'srcs = ["multistage-acceptance.sh"]' "$root/scripts/docker/BUILD.bazel"
grep -Fq 'srcs = ["rust-bazel/context/Dockerfile"]' "$root/templates/BUILD.bazel"
grep -Fq 'name = "rust-bazel-offline-baseline"' "$root/templates/BUILD.bazel"
grep -Fq 'FROM environment-base AS offline-seed' "$environment_dockerfile"
grep -Fq 'mod graph >/dev/null' "$environment_dockerfile"
grep -Fq 'cmp /tmp/repo-sandbox-expected-hashes /tmp/repo-sandbox-actual-hashes' \
  "$environment_dockerfile"
grep -Fq 'bazel --batch --output_user_root=/toolchain/bazel-seed test //...' \
  "$environment_dockerfile"
grep -Fq 'COPY --from=offline-seed /toolchain/bazel-seed/cache/repos/' \
  "$environment_dockerfile"
! sed -n '/FROM environment-base AS offline-seed/,/FROM environment-base AS environment/p' \
  "$environment_dockerfile" | grep -Fq 'github_token'
grep -Fq '/usr/local/libexec/repo-sandbox/bazel-8.3.1' "$bazel_wrapper"
grep -Fq -- '--output_user_root=/var/cache/repo-sandbox/bazel' "$bazel_wrapper"
grep -Fq -- '--ignore_all_rc_files' "$bazel_wrapper"
grep -Fq -- '--repository_disable_download' "$bazel_wrapper"
grep -Fq '"https://bcr.bazel.build/modules/platforms/0.0.7/MODULE.bazel"' \
  "$baseline_lock"
grep -Fq '"https://bcr.bazel.build/modules/rules_shell/0.2.0/MODULE.bazel"' \
  "$baseline_lock"
grep -Fq '"https://bcr.bazel.build/modules/rules_java/8.12.0/MODULE.bazel"' \
  "$baseline_lock"
grep -Fq '["--network", "none"]' "$root/crates/adapters/src/docker_runner.rs"
grep -Fxq 'templates/rust-bazel/context/Dockerfile text eol=lf' "$root/.gitattributes"
grep -Fxq 'templates/rust-bazel/context/bazel text eol=lf' "$root/.gitattributes"
grep -Fxq 'templates/rust-bazel/context/offline-baseline/* text eol=lf' \
  "$root/.gitattributes"
grep -Fq "Template: rust-bazel@1.0.1" "$ci"
! grep -Rq -F --exclude=contracts.sh 'rust-bazel@1.0.0' \
  "$root/.github" "$root/scripts" "$root/docs"
grep -Fq 'Bazel unexpectedly downloaded a module outside the central baseline' \
  "$root/scripts/docker/multistage-acceptance.sh"
grep -Fq 'templates/rust-bazel/context/offline-baseline' \
  "$root/scripts/docker/multistage-acceptance.sh"
grep -Fq 'COPY offline-baseline/' "$root/tests/multistage/Dockerfile.single-stage"
grep -Fq '/usr/local/share/repo-sandbox/offline-baseline/MODULE.bazel.lock' \
  "$root/tests/multistage/Dockerfile.single-stage"
grep -Fq 'rm -rf /tmp/repo-sandbox-bazel-check' "$environment_dockerfile"
bash "$root/scripts/e2e/docker-scenario.sh" --self-test-task-id
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

# Keep every scenario budget explicit. The required Docker job is sequential, so
# its outer timeout must cover the sum of every required Docker scenario plus teardown.
scenario_timeouts=$(awk '
  /- id: / { id=$3 }
  /timeout_seconds:/ { print id "=" $2 }
' "$root/tests/e2e/scenarios.yaml")
expected_scenario_timeouts=$(printf '%s\n' \
  'cli-build-success=900' \
  'cli-verify-success=600' \
  'cli-build-failure=600' \
  'cli-test-failure=600' \
  'cli-clean-owned-only=600' \
  'cli-interrupt-cleanup=600' \
  'cli-multi-platform-oci=1800' \
  'cli-registry-publish=1200' \
  'cli-public-file-remote=600' \
  'cli-private-https-remote=1800' \
  'cli-private-ssh-remote=1800' \
  'cli-profile-contracts=1200' \
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

docker_job_minutes=$(awk '/^  docker-required:/{found=1} found && /timeout-minutes:/{print $2; exit}' \
  "$ci")
required_docker_budget=$(awk '
  /- id: / { id=$3; tier=""; docker=0 }
  /tier: required/ { tier="required" }
  /targets: \[[^]]*docker/ { docker=1 }
  /timeout_seconds:/ { if (tier == "required" && docker) total += $2 }
  END { print total }
' "$root/tests/e2e/scenarios.yaml")
[[ $((docker_job_minutes * 60)) -ge $((required_docker_budget + 600)) ]]

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
