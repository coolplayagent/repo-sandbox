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

assert_oci_manifest_platform() {
  local layout=$1 architecture=$2 document compact
  while IFS= read -r document; do
    compact=$(tr -d '\r\n' <"$document")
    if grep -Eq '"mediaType"[[:space:]]*:[[:space:]]*"application/vnd\.(oci\.image\.manifest|docker\.distribution\.manifest)\.[^\"]*"' \
        <<<"$compact" \
      && grep -Eq "\"platform\"[[:space:]]*:[[:space:]]*\\{[^}]*\"architecture\"[[:space:]]*:[[:space:]]*\"$architecture\"[^}]*\"os\"[[:space:]]*:[[:space:]]*\"linux\"|\"platform\"[[:space:]]*:[[:space:]]*\\{[^}]*\"os\"[[:space:]]*:[[:space:]]*\"linux\"[^}]*\"architecture\"[[:space:]]*:[[:space:]]*\"$architecture\"" \
        <<<"$compact"; then
      return 0
    fi
  done < <(find "$layout" -type f \( -name index.json -o -path '*/blobs/sha256/*' \) -print)
  echo "OCI layout has no image manifest descriptor for linux/$architecture" >&2
  return 1
}

case "$scenario" in
  cli-build-success|cli-verify-success|cli-build-failure|cli-test-failure|cli-clean-owned-only|cli-interrupt-cleanup|cli-multi-platform-oci)
    fixture=$(mktemp -d)
    cleanup_cli_fixture() {
      [[ -z ${foreign:-} ]] || docker rm --force "$foreign" >/dev/null 2>&1 || true
      rm -rf -- "$fixture"
    }
    trap cleanup_cli_fixture EXIT
    cp "$root/.repo-sandbox.yaml.example" "$fixture/.repo-sandbox.yaml"
    printf '.repo-sandbox/\nreport*.json\n.*.repo-sandbox-reservation\n' >"$fixture/.gitignore"
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
      elif [[ "$scenario" == cli-interrupt-cleanup ]]; then
        printf '#!/bin/sh\ntouch /workspace/cancel-ready\nsleep 600\n' >"$fixture/test.sh"
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
      cli-interrupt-cleanup)
        foreign_labeled=
        foreign_unlabeled=
        task_container=
        cli_pid=
        cleanup_cli_fixture() {
          if [[ -n ${cli_pid:-} ]] && kill -0 "$cli_pid" 2>/dev/null; then
            kill -TERM "$cli_pid" 2>/dev/null || true
            wait "$cli_pid" 2>/dev/null || true
          fi
          [[ -z ${task_container:-} ]] || docker rm --force "$task_container" >/dev/null 2>&1 || true
          [[ -z ${foreign_labeled:-} ]] || docker rm --force "$foreign_labeled" >/dev/null 2>&1 || true
          [[ -z ${foreign_unlabeled:-} ]] || docker rm --force "$foreign_unlabeled" >/dev/null 2>&1 || true
          rm -rf -- "$fixture"
        }
        foreign_labeled=$(docker create --label io.repo-sandbox.task-id=foreign busybox:1.36 true)
        foreign_unlabeled=$(docker create busybox:1.36 true)
        sentinel="$result_directory/ctrl-c-unrelated.keep"
        printf 'must remain byte-identical\n' >"$sentinel"
        sentinel_before=$(sha256sum "$sentinel")

        interrupt_log="$result_directory/cli-interrupt.log"
        "$cli" verify --repository "$fixture" --report-path "$report" \
          >"$interrupt_log" 2>&1 &
        cli_pid=$!
        for _ in $(seq 1 600); do
          container_manifest=$(grep -Fl '"kind": "container"' \
            "$fixture"/.repo-sandbox/tasks/*.json 2>/dev/null | head -n 1 || true)
          if [[ -n $container_manifest ]]; then
            task_container=$(sed -nE \
              's/^[[:space:]]*"identifier": "([^"]+)",?$/\1/p' "$container_manifest")
          fi
          if [[ -n $task_container ]] \
            && docker exec "$task_container" test -f /workspace/cancel-ready 2>/dev/null; then
            break
          fi
          sleep 0.25
        done
        [[ -n $task_container ]]
        docker exec "$task_container" test -f /workspace/cancel-ready
        kill -INT "$cli_pid"
        set +e
        wait "$cli_pid"
        interrupt_status=$?
        set -e
        cli_pid=
        cat "$interrupt_log"
        [[ $interrupt_status -eq 3 ]]
        assert_report_common "$report" removed
        grep -Fq '"status": "cancelled"' "$report"
        assert_step "$report" test bazel-test cancelled
        ! docker inspect "$task_container" >/dev/null 2>&1
        latest_container_event=$(grep -Fl "\"identifier\": \"$task_container\"" \
          "$fixture"/.repo-sandbox/tasks/*.json | sort | tail -n 1)
        grep -Fq '"state": "cleaned"' "$latest_container_event"
        docker inspect "$foreign_labeled" >/dev/null
        docker inspect "$foreign_unlabeled" >/dev/null
        [[ $(sha256sum "$sentinel") == "$sentinel_before" ]]
        echo 'interrupt_exit=3 phase=test cleanup=removed journal_container=cleaned unrelated=unchanged'
        ;;
      cli-multi-platform-oci)
        layout="$result_directory/task-layout"
        "$cli" verify --repository "$fixture" --report-path "$report" \
          --platform linux/amd64 --platform linux/arm64 --oci-layout "$layout"
        assert_report_common "$report" removed
        assert_step "$report" build bazel-build succeeded
        assert_step "$report" test bazel-test succeeded
        [[ -s $layout/oci-layout ]]
        [[ -s $layout/index.json ]]
        grep -Fq '"mediaType": "application/vnd.oci.image.index.v1+json"' \
          "$layout/index.json"
        [[ $(grep -c '"platform": {' "$layout/index.json") -eq 2 ]]
        assert_oci_manifest_platform "$layout" amd64
        assert_oci_manifest_platform "$layout" arm64
        grep -Eq '"digest"[[:space:]]*:[[:space:]]*"sha256:[0-9a-f]{64}"' \
          "$layout/index.json"
        echo 'multi_platform=linux/amd64,linux/arm64 output=oci-layout runner=verified'
        ;;
    esac
    echo "$scenario=passed"
    printf 'passed\n' >"$result_directory/$scenario.passed"
    ;;
  cli-public-file-remote)
    fixture=$(mktemp -d)
    remote_state=$(mktemp -d)
    cleanup_remote_fixture() { rm -rf -- "$fixture" "$remote_state"; }
    trap cleanup_remote_fixture EXIT
    cp "$root/.repo-sandbox.yaml.example" "$fixture/.repo-sandbox.yaml"
    printf '.repo-sandbox/\nreport*.json\n.*.repo-sandbox-reservation\n' >"$fixture/.gitignore"
    cat >"$fixture/BUILD.bazel" <<'EOF'
genrule(name = "build_ok", outs = ["built.txt"], cmd = "echo built > $@")
sh_test(name = "tests", srcs = ["test.sh"])
EOF
    printf '#!/bin/sh\nexit 0\n' >"$fixture/test.sh"
    chmod +x "$fixture/test.sh"
    git -C "$fixture" init -q
    git -C "$fixture" config user.email e2e@example.invalid
    git -C "$fixture" config user.name repo-sandbox-e2e
    git -C "$fixture" add .
    git -C "$fixture" commit -qm fixture
    expected_commit=$(git -C "$fixture" rev-parse HEAD)
    bare="$remote_state/public.git"
    git clone -q --bare "$fixture" "$bare"
    remote_url="file://$(cd "$(dirname "$bare")" && pwd)/$(basename "$bare")"
    cargo build -p repo-sandbox-cli
    cli="$root/target/debug/repo-sandbox"
    report="$remote_state/report.json"
    (
      cd "$remote_state"
      "$cli" verify --repository "$remote_url" --git-ref "$expected_commit" \
        --report-path "$report"
    )
    assert_report_common "$report" removed
    grep -Fq '"kind": "remote_git"' "$report"
    grep -Fq "\"commit\": \"$expected_commit\"" "$report"
    assert_step "$report" build bazel-build succeeded
    assert_step "$report" test bazel-test succeeded
    echo 'remote_cli=file:// commit=pinned transport=offline'
    printf 'passed\n' >"$result_directory/cli-public-file-remote.passed"
    ;;
  cli-private-https-remote|cli-private-ssh-remote)
    remote_state=$(mktemp -d)
    cleanup_private_remote() { rm -rf -- "$remote_state"; }
    trap cleanup_private_remote EXIT
    cargo build -p repo-sandbox-cli
    cli="$root/target/debug/repo-sandbox"
    report="$remote_state/report.json"
    if [[ $scenario == cli-private-https-remote ]]; then
      remote_url=$REPO_SANDBOX_E2E_HTTPS_URL
      remote_ref=$REPO_SANDBOX_E2E_HTTPS_REF
      credential_marker=$REPO_SANDBOX_E2E_HTTPS_TOKEN
    else
      remote_url=$REPO_SANDBOX_E2E_SSH_URL
      remote_ref=$REPO_SANDBOX_E2E_SSH_REF
      credential_marker=$(awk '!/^---/ && NF { print; exit }' \
        "$REPO_SANDBOX_E2E_SSH_KEY")
    fi
    [[ -n $credential_marker ]]
    (
      cd "$remote_state"
      "$cli" verify --repository "$remote_url" --git-ref "$remote_ref" \
        --report-path "$report"
    )
    assert_report_common "$report" removed
    grep -Fq '"kind": "remote_git"' "$report"
    grep -Eq '"commit": "[0-9a-f]{40}([0-9a-f]{24})?"' "$report"
    assert_step "$report" build bazel-build succeeded
    assert_step "$report" test bazel-test succeeded
    ! grep -R -Fq -- "$credential_marker" "$remote_state"
    echo "private_remote=${scenario#cli-private-} credential_scan=passed"
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
