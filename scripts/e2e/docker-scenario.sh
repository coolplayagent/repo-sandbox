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

assert_report_digest() {
  local report=$1 field=$2
  grep -Eq "\"${field}\": \"sha256:[0-9a-f]{64}\"" "$report"
}

assert_report_common() {
  local report=$1 expected_cleanup=$2
  [[ -s $report ]]
  grep -Eq '"task_id": "[0-9]+-[0-9]+"' "$report"
  assert_report_digest "$report" plan_digest
  assert_report_digest "$report" image_digest
  grep -Eq '"id": "[0-9a-f]{64}"' "$report"
  grep -Fq "\"cleanup\": \"$expected_cleanup\"" "$report"
}

report_snapshot_id() {
  sed -nE 's/^[[:space:]]*"id": "([0-9a-f]{64})",?$/\1/p' "$1" | head -n 1
}

assert_step() {
  local report=$1 phase=$2 name=$3 status=$4
  awk -v expected_phase="$phase" -v expected_name="$name" -v expected_status="$status" '
    $0 ~ "\\\"phase\\\": \\\"" expected_phase "\\\"" { phase_found = 1 }
    phase_found && $0 ~ "\\\"name\\\": \\\"" expected_name "\\\"" { name_found = 1 }
    phase_found && name_found && $0 ~ "\\\"status\\\": \\\"" expected_status "\\\"" { found = 1 }
    phase_found && /}/ { phase_found = 0; name_found = 0 }
    END { exit !found }
  ' "$report"
}

run_expect_status() {
  local expected=$1
  shift
  set +e
  "$@"
  local actual=$?
  set -e
  [[ $actual -eq $expected ]] || {
    echo "expected exit $expected, got $actual: $*" >&2
    return 1
  }
}

case "$scenario" in
  cli-build-success|cli-verify-success|cli-build-failure|cli-test-failure|cli-clean-owned-only)
    fixture=$(mktemp -d)
    cleanup_cli_fixture() {
      [[ -z ${foreign:-} ]] || docker rm --force "$foreign" >/dev/null 2>&1 || true
      rm -rf -- "$fixture"
    }
    trap cleanup_cli_fixture EXIT
    cp "$root/.repo-sandbox.yaml.example" "$fixture/.repo-sandbox.yaml"
    printf '.repo-sandbox/\nreport*.json\n' >"$fixture/.gitignore"
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
      cli-build-success)
        cache_index="$fixture/.repo-sandbox/cache/environment/index.json"
        [[ ! -e $cache_index ]]
        "$cli" build --repository "$fixture" --report-path "$report"
        assert_report_common "$report" removed
        grep -Fq '"status": "succeeded"' "$report"
        assert_step "$report" build bazel-build succeeded
        [[ -f $cache_index ]]
        first_source=$(report_snapshot_id "$report")
        [[ -n $first_source ]]

        warm_report="$fixture/report-warm.json"
        "$cli" build --repository "$fixture" --report-path "$warm_report"
        assert_report_common "$warm_report" removed
        assert_step "$warm_report" build bazel-build succeeded
        [[ $(report_snapshot_id "$warm_report") == "$first_source" ]]
        [[ -f $cache_index ]]

        printf '# source-change\n' >>"$fixture/BUILD.bazel"
        git -C "$fixture" add BUILD.bazel
        git -C "$fixture" commit -qm source-change
        changed_report="$fixture/report-changed.json"
        "$cli" build --repository "$fixture" --report-path "$changed_report"
        assert_report_common "$changed_report" removed
        assert_step "$changed_report" build bazel-build succeeded
        [[ $(report_snapshot_id "$changed_report") != "$first_source" ]]
        echo 'cache=cold_then_warm source_digest=changed'
        ;;
      cli-verify-success)
        "$cli" verify --repository "$fixture" --report-path "$report"
        assert_report_common "$report" removed
        grep -Fq '"status": "succeeded"' "$report"
        assert_step "$report" build bazel-build succeeded
        assert_step "$report" test bazel-test succeeded
        [[ $(grep -c '"phase": "build"' "$report") -eq 1 ]]
        [[ $(grep -c '"phase": "test"' "$report") -eq 1 ]]
        ;;
      cli-build-failure)
        run_expect_status 10 "$cli" build --repository "$fixture" --report-path "$report"
        assert_report_common "$report" removed
        grep -Fq '"status": "command_failed"' "$report"
        grep -Fq '"phase": "build"' "$report"
        grep -Fq '"step": "bazel-build"' "$report"
        assert_step "$report" build bazel-build command_failed
        grep -Eq '"exit_code": [1-9][0-9]*' "$report"
        [[ $(grep -c '"phase": "test"' "$report") -eq 0 ]]
        ;;
      cli-test-failure)
        push_args=()
        if [[ -n ${REPO_SANDBOX_E2E_REGISTRY_REPOSITORY:-} ]]; then
          [[ $REPO_SANDBOX_E2E_REGISTRY_REPOSITORY =~ ^[A-Za-z0-9._:/-]+$ ]]
          sed -i "s|bazelisk_version: \"1.27.0\"|bazelisk_version: \"1.27.0\"\n    registry_repository: \"${REPO_SANDBOX_E2E_REGISTRY_REPOSITORY}\"|" \
            "$fixture/.repo-sandbox.yaml"
          git -C "$fixture" add .repo-sandbox.yaml
          git -C "$fixture" commit -qm registry-policy
          push_args=(--push)
        fi
        run_expect_status 11 "$cli" verify --repository "$fixture" \
          --report-path "$report" "${push_args[@]}"
        assert_report_common "$report" removed
        grep -Fq '"status": "command_failed"' "$report"
        grep -Fq '"phase": "test"' "$report"
        grep -Fq '"step": "bazel-test"' "$report"
        assert_step "$report" build bazel-build succeeded
        assert_step "$report" test bazel-test command_failed
        grep -Fq '"exit_code": 23' "$report"
        ! grep -Fq '"published"' "$report"
        ;;
      cli-clean-owned-only)
        foreign=$(docker create --label io.repo-sandbox.task-id=foreign busybox:1.36 true)
        run_expect_status 10 "$cli" build --repository "$fixture" --keep-on-failure \
          --report-path "$report"
        assert_report_common "$report" retained_on_failure
        grep -Fq '"phase": "build"' "$report"
        grep -Fq '"step": "bazel-build"' "$report"
        before=$(docker inspect --format '{{.Id}}' "$foreign")
        retained=$(sed -nE 's/^[[:space:]]*"container_id": "([^"]+)",?$/\1/p' "$report")
        [[ -n $retained ]]
        retained_before=$(docker inspect --format '{{.Id}}' "$retained")
        cache_marker="$fixture/.repo-sandbox/cache/.repo-sandbox-owner"
        [[ -s $cache_marker ]]
        marker_before=$(sha256sum "$cache_marker")

        dry_run_output=$("$cli" clean --repository "$fixture" --dry-run \
          --include-images --include-cache)
        grep -Fq '(dry-run)' <<<"$dry_run_output"
        grep -Fq 'candidate Container' <<<"$dry_run_output"
        [[ $(docker inspect --format '{{.Id}}' "$retained") == "$retained_before" ]]
        [[ $(sha256sum "$cache_marker") == "$marker_before" ]]

        container_manifest=$(grep -Fl '"kind": "container"' \
          "$fixture"/.repo-sandbox/tasks/*.json | head -n 1)
        container_manifest_name=$(basename "$container_manifest")
        mv "$fixture/.repo-sandbox/tasks" "$fixture/.repo-sandbox/tasks-owned"
        mkdir "$fixture/.repo-sandbox/tasks"
        forged_manifest="$fixture/.repo-sandbox/tasks/forged-owner-mismatch.json"
        sed "s|$retained|$foreign|g" \
          "$fixture/.repo-sandbox/tasks-owned/$container_manifest_name" >"$forged_manifest"
        run_expect_status 3 "$cli" clean --repository "$fixture" --yes \
          --include-images --include-cache
        [[ $(docker inspect --format '{{.Id}}' "$foreign") == "$before" ]]
        [[ $(docker inspect --format '{{.Id}}' "$retained") == "$retained_before" ]]
        rm -rf -- "$fixture/.repo-sandbox/tasks"
        mv "$fixture/.repo-sandbox/tasks-owned" "$fixture/.repo-sandbox/tasks"

        "$cli" clean --repository "$fixture" --yes --include-images --include-cache
        ! docker inspect "$retained" >/dev/null 2>&1
        [[ $(docker inspect --format '{{.Id}}' "$foreign") == "$before" ]]
        idempotent_output=$("$cli" clean --repository "$fixture" --yes \
          --include-images --include-cache)
        grep -Fq '0 succeeded' <<<"$idempotent_output"
        [[ $(docker inspect --format '{{.Id}}' "$foreign") == "$before" ]]
        docker rm "$foreign" >/dev/null
        foreign=
        echo 'dry_run=unchanged owner_mismatch=refused foreign_resource=preserved cleanup=idempotent'
        ;;
    esac
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
