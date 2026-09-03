#!/usr/bin/env bash
set -euo pipefail

[[ $# == 2 ]] || { echo "usage: $0 SCENARIO RESULT_DIRECTORY" >&2; exit 64; }
scenario=$1
result_directory=$2
root=$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)
mkdir -p -- "$result_directory"

run_test() {
  local name=$1
  cargo test -p repo-sandbox-adapters "$name" -- --ignored --exact --nocapture
}

case "$scenario" in
  cli-build-success|cli-verify-success|cli-build-failure|cli-test-failure|cli-clean-owned-only)
    fixture=$(mktemp -d)
    cleanup_cli_fixture() { rm -rf -- "$fixture"; }
    trap cleanup_cli_fixture EXIT
    cp "$root/.repo-sandbox.yaml.example" "$fixture/.repo-sandbox.yaml"
    git -C "$fixture" init -q
    git -C "$fixture" config user.email e2e@example.invalid
    git -C "$fixture" config user.name repo-sandbox-e2e
    if [[ "$scenario" == cli-build-failure || "$scenario" == cli-clean-owned-only ]]; then
      printf 'this is not valid bazel syntax !!!\n' >"$fixture/BUILD.bazel"
    else
      cat >"$fixture/BUILD.bazel" <<'EOF'
genrule(name = "build_ok", outs = ["built.txt"], cmd = "echo built > $@")
sh_test(name = "tests", srcs = ["test.sh"])
EOF
      if [[ "$scenario" == cli-test-failure ]]; then
        printf '#!/bin/sh\nexit 23\n' >"$fixture/test.sh"
      else
        printf '#!/bin/sh\nexit 0\n' >"$fixture/test.sh"
      fi
      chmod +x "$fixture/test.sh"
    fi
    git -C "$fixture" add .
    git -C "$fixture" commit -qm fixture
    cargo build -p repo-sandbox-cli
    cli="$root/target/debug/repo-sandbox"
    report="$fixture/report.json"
    case "$scenario" in
      cli-build-success) "$cli" build --repository "$fixture" --report-path "$report" ;;
      cli-verify-success) "$cli" verify --repository "$fixture" --report-path "$report" ;;
      cli-build-failure)
        set +e; "$cli" build --repository "$fixture" --report-path "$report"; status=$?; set -e
        [[ $status -eq 10 ]]
        ;;
      cli-test-failure)
        set +e; "$cli" verify --repository "$fixture" --report-path "$report"; status=$?; set -e
        [[ $status -eq 11 ]]
        ;;
      cli-clean-owned-only)
        foreign=$(docker create --label io.repo-sandbox.task-id=foreign busybox:1.36 true)
        set +e; "$cli" build --repository "$fixture" --keep-on-failure --report-path "$report"; status=$?; set -e
        [[ $status -eq 10 ]]
        before=$(docker inspect --format '{{.Id}}' "$foreign")
        "$cli" clean --repository "$fixture" --dry-run --include-images --include-cache
        "$cli" clean --repository "$fixture" --yes --include-images --include-cache
        "$cli" clean --repository "$fixture" --yes --include-images --include-cache
        [[ $(docker inspect --format '{{.Id}}' "$foreign") == "$before" ]]
        docker rm "$foreign" >/dev/null
        ;;
    esac
    [[ -s "$report" ]]
    grep -q '"task_id"' "$report"
    echo "$scenario=passed"
    printf 'passed\n' >"$result_directory/$scenario.passed"
    ;;
  adapters)
    run_test 'task_image::tests::docker_task_image_contains_only_snapshot_source'
    echo 'stage=snapshot-task-image status=passed'
    run_test 'buildkit::tests::docker_buildx_cold_and_warm_smoke'
    echo 'stage=buildkit-cache status=passed'
    run_test 'docker_runner::tests::docker_one_shot_job_smoke'
    echo 'stage=runner-artifact-export status=passed'
    printf 'passed\n' >"$result_directory/adapter-smoke.passed"
    ;;
  failures)
    run_test 'docker_runner::tests::docker_build_test_timeout_and_keep_on_failure_matrix'
    printf 'passed\n' >"$result_directory/failure-matrix.passed"
    ;;
  architecture-mismatch)
    temporary=$(mktemp -d)
    cleanup() { rm -rf -- "$temporary"; }
    trap cleanup EXIT
    cat >"$temporary/Dockerfile" <<'EOF'
FROM busybox:1.36 AS environment
ARG TARGETARCH
RUN echo stage=architecture-check expected=amd64 actual="$TARGETARCH" && test "$TARGETARCH" = amd64
EOF
    set +e
    docker buildx build --platform linux/arm64 --progress plain --target environment "$temporary" \
      >"$temporary/build.log" 2>&1
    status=$?
    set -e
    cat "$temporary/build.log"
    [[ $status -ne 0 ]]
    grep -Eq 'stage=architecture-check expected=amd64 actual=("?arm64"?)|exec format error' \
      "$temporary/build.log"
    echo 'stage=architecture-check architecture_mismatch=observed'
    printf 'passed\n' >"$result_directory/architecture-mismatch.passed"
    ;;
  dogfood)
    "$root/scripts/docker/multistage-acceptance.sh"
    echo 'dogfood=passed'
    printf 'passed\n' >"$result_directory/dogfood.passed"
    ;;
  *)
    echo "unknown Docker E2E scenario: $scenario" >&2
    exit 64
    ;;
esac
