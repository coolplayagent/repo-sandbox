#!/usr/bin/env bash
set -euo pipefail

valid_task_id_value() {
  local value=${1-}
  [[ -n $value && ${#value} -le 48 && $value =~ ^[a-z0-9_-]+$ ]]
}

task_source_rebuilt_stream() {
  awk '
    /\[(task )?[0-9]+\/[0-9]+\] COPY .*source/ { vertex=$1 }
    vertex != "" && $1 == vertex && /[[:space:]]CACHED$/ { cached=1 }
    vertex != "" && $1 == vertex && /[[:space:]]DONE/ { done=1 }
    END { exit !done || cached }
  '
}

# Fold journal events before selecting a live container: quota probes and the
# runner both have Container records, and earlier registrations may be cleaned.
journal_container_records() {
  python3 - "$1" <<'PYRECORDS'
import json, pathlib, sys
latest = {}
for path in sorted(pathlib.Path(sys.argv[1]).glob("*.json")):
    for item in json.loads(path.read_text()):
        if item.get("kind") == "container":
            latest[item["identifier"]] = (item, path)
for item, path in latest.values():
    if item.get("state") != "cleaned":
        print(item["identifier"], item["task_id"], path, sep="\t")
PYRECORDS
}

select_task_container() {
  local identifier task_id manifest name
  while IFS=$'\t' read -r identifier task_id manifest; do
    name=$(docker inspect --format '{{.Name}}' "$identifier" 2>/dev/null || true)
    if [[ $name == "/repo-sandbox-$task_id" ]]; then
      printf '%s\n' "$identifier"
      return 0
    fi
  done < <(journal_container_records "$1")
}

assert_report_exports_file() {
  python3 - "$1" "$2" <<'PYEXPORT'
import json, pathlib, sys
with open(sys.argv[1]) as source:
    report = json.load(source)
artifact = pathlib.Path(sys.argv[2]).resolve()
roots = [pathlib.Path(value).resolve() for value in report["exported_artifacts"]]
assert any(artifact != root and artifact.is_relative_to(root) for root in roots), "artifact is not beneath a reported export directory"
PYEXPORT
}

start_cache_boundary_builder() {
  cache_boundary_builder="repo-sandbox-cache-$(python3 -c 'import uuid; print(uuid.uuid4().hex)')"
  cache_boundary_owned=
  cache_boundary_saved_buildx=${BUILDX_BUILDER-}
  cache_boundary_had_buildx=${BUILDX_BUILDER+x}
  cache_boundary_saved_repo=${REPO_SANDBOX_BUILDER-}
  cache_boundary_had_repo=${REPO_SANDBOX_BUILDER+x}
  # Do not select this builder globally: other matrix scenarios retain the
  # shared builder and its expensive ARM preparation cache.
  docker buildx create --name "$cache_boundary_builder" --driver docker-container \
    --driver-opt network=host >/dev/null || return
  cache_boundary_owned=$cache_boundary_builder
  export BUILDX_BUILDER=$cache_boundary_owned
  export REPO_SANDBOX_BUILDER=$cache_boundary_owned
  docker buildx inspect "$cache_boundary_owned" --bootstrap >/dev/null
}

prune_cache_boundary_builder() {
  [[ -n ${cache_boundary_owned:-} ]] || return 1
  docker buildx prune --builder "$cache_boundary_owned" --all --force >/dev/null
}

cleanup_cache_boundary_builder() {
  [[ -n ${cache_boundary_owned:-} ]] || return 0
  docker buildx rm --force "$cache_boundary_owned" >/dev/null 2>&1 || true
  cache_boundary_owned=
  if [[ $cache_boundary_had_buildx ]]; then
    export BUILDX_BUILDER=$cache_boundary_saved_buildx
  else
    unset BUILDX_BUILDER
  fi
  if [[ $cache_boundary_had_repo ]]; then
    export REPO_SANDBOX_BUILDER=$cache_boundary_saved_repo
  else
    unset REPO_SANDBOX_BUILDER
  fi
}

if [[ ${1-} == --self-test-task-id ]]; then
  [[ $# == 1 ]]
  valid_task_id_value '1234-0123456789abcdef-987654321'
  for malformed in '' 'UPPERCASE' 'contains/slash' 'contains space' \
    '1234567890123456789012345678901234567890123456789'; do
    ! valid_task_id_value "$malformed"
  done
  printf '%s\n' '#8 [task 1/4] COPY --link source/ /workspace/' '#8 DONE 0.1s' | \
    task_source_rebuilt_stream
  printf '%s\n' '#8 [1/4] COPY --link source/ /workspace/' '#8 DONE 0.1s' | \
    task_source_rebuilt_stream
  ! printf '%s\n' '#8 [1/4] COPY --link source/ /workspace/' '#8 CACHED' | \
    task_source_rebuilt_stream
  selection_fixture=$(mktemp -d)
  trap 'rm -rf "$selection_fixture"' EXIT
  python3 - "$selection_fixture" <<'PYFIXTURE'
import json, pathlib, sys
root = pathlib.Path(sys.argv[1])
records = [
    {"kind": "container", "identifier": "old-probe", "task_id": "task", "state": "registered"},
    {"kind": "container", "identifier": "old-probe", "task_id": "task", "state": "cleaned"},
    {"kind": "container", "identifier": "live-probe", "task_id": "task", "state": "registered"},
    {"kind": "container", "identifier": "runner", "task_id": "task", "state": "registered"},
]
for index, item in enumerate(records):
    (root / f"event-{index:020}.json").write_text(json.dumps([item]))
PYFIXTURE
  docker() {
    case ${4-} in
      live-probe) printf '%s\n' /repo-sandbox-quota-probe-task ;;
      runner) printf '%s\n' /repo-sandbox-task ;;
      *) return 1 ;;
    esac
  }
  [[ $(journal_container_records "$selection_fixture" | wc -l) -eq 2 ]]
  [[ $(select_task_container "$selection_fixture") == runner ]]
  [[ $(journal_container_records "$selection_fixture" | awk -F '\t' '$1 == "runner" {print $3}') ==     "$selection_fixture/event-00000000000000000003.json" ]]
  printf '%s\n' '{"exported_artifacts":["/task/artifacts/profile-artifacts"]}' >"$selection_fixture/export.json"
  assert_report_exports_file "$selection_fixture/export.json" /task/artifacts/profile-artifacts/profile-artifact.txt
  for invalid_artifact in /task/artifacts/other/profile-artifact.txt /task/artifacts/profile-artifacts \
    /task/artifacts/profile-artifacts/../unreported.txt; do
    if assert_report_exports_file "$selection_fixture/export.json" "$invalid_artifact" 2>/dev/null; then
      echo "unreported artifact was accepted: $invalid_artifact" >&2
      exit 1
    fi
  done
  builder_calls="$selection_fixture/builders.log"
  builder_failure=
  docker() {
    printf '%s\n' "$*" >>"$builder_calls"
    [[ ${2-} != "$builder_failure" ]]
  }
  export BUILDX_BUILDER=shared-buildx REPO_SANDBOX_BUILDER=shared-repo
  start_cache_boundary_builder
  owned=$cache_boundary_owned
  [[ $owned == repo-sandbox-cache-* && $owned != shared-buildx ]]
  [[ $BUILDX_BUILDER == "$owned" && $REPO_SANDBOX_BUILDER == "$owned" ]]
  prune_cache_boundary_builder
  cleanup_cache_boundary_builder
  [[ $BUILDX_BUILDER == shared-buildx && $REPO_SANDBOX_BUILDER == shared-repo ]]
  grep -Fxq "buildx create --name $owned --driver docker-container --driver-opt network=host" "$builder_calls"
  grep -Fxq "buildx inspect $owned --bootstrap" "$builder_calls"
  grep -Fxq "buildx prune --builder $owned --all --force" "$builder_calls"
  grep -Fxq "buildx rm --force $owned" "$builder_calls"
  [[ $(wc -l <"$builder_calls") -eq 4 ]]

  : >"$builder_calls"
  builder_failure=create
  if start_cache_boundary_builder; then exit 1; fi
  cleanup_cache_boundary_builder
  [[ $(wc -l <"$builder_calls") -eq 1 ]]
  [[ $BUILDX_BUILDER == shared-buildx && $REPO_SANDBOX_BUILDER == shared-repo ]]

  : >"$builder_calls"
  builder_failure=inspect
  if start_cache_boundary_builder; then exit 1; fi
  owned=$cache_boundary_owned
  [[ -n $owned ]]
  cleanup_cache_boundary_builder
  grep -Fxq "buildx rm --force $owned" "$builder_calls"
  [[ $(wc -l <"$builder_calls") -eq 3 ]]
  [[ $BUILDX_BUILDER == shared-buildx && $REPO_SANDBOX_BUILDER == shared-repo ]]
  exit 0
fi

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
  local report=$1 expected_cleanup=$2 task_id
  [[ -s $report ]]
  task_id=$(python3 - "$report" <<'PY'
import json
import sys

with open(sys.argv[1], encoding="utf-8") as stream:
    value = json.load(stream).get("task_id")
if not isinstance(value, str):
    raise SystemExit("report task_id is not a string")
print(value)
PY
  )
  valid_task_id_value "$task_id"
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
    index($0, "\"phase\": \"" expected_phase "\"") { phase_found = 1 }
    phase_found && index($0, "\"name\": \"" expected_name "\"") { name_found = 1 }
    phase_found && name_found && index($0, "\"status\": \"" expected_status "\"") { found = 1 }
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
  local layout=$1 architecture=$2
  python3 - "$layout" "$architecture" <<'PY'
import json
import pathlib
import sys

layout = pathlib.Path(sys.argv[1])
architecture = sys.argv[2]
document = json.loads((layout / "index.json").read_text(encoding="utf-8"))
seen = set()

def descriptors(value):
    for descriptor in value.get("manifests", []):
        platform = descriptor.get("platform", {})
        media_type = descriptor.get("mediaType", "")
        if (platform.get("os"), platform.get("architecture")) == ("linux", architecture) \
                and ("image.manifest" in media_type or "distribution.manifest" in media_type):
            return True
        digest = descriptor.get("digest", "")
        if digest.startswith("sha256:") and digest not in seen:
            seen.add(digest)
            blob = layout / "blobs" / "sha256" / digest.removeprefix("sha256:")
            try:
                nested = json.loads(blob.read_text(encoding="utf-8"))
            except (OSError, UnicodeDecodeError, json.JSONDecodeError):
                continue
            if descriptors(nested):
                return True
    return False

if not descriptors(document):
    raise SystemExit(f"OCI layout has no image manifest descriptor for linux/{architecture}")
PY
}

assert_oci_blob_digests() {
  local layout=$1 descriptors digest size blob actual_digest actual_size
  descriptors=$(awk '
    /"digest"[[:space:]]*:/ {
      digest = $0
      sub(/^.*"digest"[[:space:]]*:[[:space:]]*"/, "", digest)
      sub(/".*$/, "", digest)
    }
    digest != "" && /"size"[[:space:]]*:/ {
      size = $0
      sub(/^.*"size"[[:space:]]*:[[:space:]]*/, "", size)
      sub(/[^0-9].*$/, "", size)
      print digest, size
      digest = ""
    }
  ' "$layout/index.json")
  [[ -n $descriptors ]]
  while read -r digest size; do
    [[ $digest =~ ^sha256:[0-9a-f]{64}$ ]]
    [[ $size =~ ^[0-9]+$ ]]
    blob="$layout/blobs/sha256/${digest#sha256:}"
    [[ -f $blob ]]
    actual_digest=$(sha256sum "$blob" | awk '{print "sha256:" $1}')
    actual_size=$(wc -c <"$blob" | tr -d '[:space:]')
    [[ $actual_digest == "$digest" ]]
    [[ $actual_size == "$size" ]]
  done <<<"$descriptors"
  while IFS= read -r blob; do
    [[ $(sha256sum "$blob" | awk '{print $1}') == $(basename "$blob") ]]
  done < <(find "$layout/blobs/sha256" -type f -print)
}

assert_report_phase() {
  local report=$1 phase=$2 exit_code=$3
  grep -Fq "\"phase\": \"$phase\"" "$report"
  grep -Fq "\"exit_code\": $exit_code" "$report"
}

assert_environment_cache_hit() {
  local log=$1
  grep -Eq 'importing cache manifest from local' "$log"
  awk '
    /\[[^]]*environment [0-9]+\/[0-9]+\] RUN/ { vertex=$1 }
    vertex != "" && $1 == vertex && /[[:space:]]CACHED$/ { found=1 }
    END { exit !found }
  ' "$log"
}

assert_task_source_rebuilt() {
  local log=$1
  task_source_rebuilt_stream <"$log"
}

case "$scenario" in
  cli-build-success|cli-verify-success|cli-build-failure|cli-test-failure|cli-clean-owned-only|cli-interrupt-cleanup|cli-multi-platform-oci|cli-registry-publish)
    fixture=$(mktemp -d)
    registry_container=
    cache_boundary_owned=
    cleanup_cli_fixture() {
      cleanup_cache_boundary_builder
      [[ -z ${foreign:-} ]] || docker rm --force "$foreign" >/dev/null 2>&1 || true
      [[ -z ${owned_image_reference:-} ]] || docker rm --force "$owned_image_reference" >/dev/null 2>&1 || true
      [[ -z ${registry_container:-} ]] || docker rm --force "$registry_container" >/dev/null 2>&1 || true
      rm -rf -- "$fixture"
    }
    trap cleanup_cli_fixture EXIT
    cp "$root/.repo-sandbox.yaml.example" "$fixture/.repo-sandbox.yaml"
    if [[ $scenario == cli-registry-publish ]]; then
      registry_container=$(docker run --detach --publish 127.0.0.1::5000 registry:2)
      registry_port=$(docker port "$registry_container" 5000/tcp | head -n 1)
      registry_port=${registry_port##*:}
      [[ $registry_port =~ ^[0-9]+$ ]]
      for _ in $(seq 1 60); do
        curl --fail --silent "http://127.0.0.1:$registry_port/v2/" >/dev/null && break
        sleep 0.25
      done
      curl --fail --silent "http://127.0.0.1:$registry_port/v2/" >/dev/null
      registry_repository="127.0.0.1:$registry_port/repo-sandbox/e2e"
      sed -i "s|rust_version: \"1.97.0\"|rust_version: \"1.97.0\"\n    registry_repository: \"$registry_repository\"|" \
        "$fixture/.repo-sandbox.yaml"
    fi
    printf '.repo-sandbox/\nreport*.json\n.*.repo-sandbox-reservation\n' >"$fixture/.gitignore"
    printf 'module(name = "repo_sandbox_e2e_fixture")\nbazel_dep(name = "rules_cc", version = "0.2.17")\n' >"$fixture/MODULE.bazel"
    git -C "$fixture" init -q
    git -C "$fixture" config user.email e2e@example.invalid
    git -C "$fixture" config user.name repo-sandbox-e2e
    if [[ "$scenario" == cli-build-failure || "$scenario" == cli-clean-owned-only ]]; then
      printf 'this is not valid bazel syntax !!!\n' >"$fixture/BUILD.bazel"
    else
      cat >"$fixture/BUILD.bazel" <<'EOF'
load("@rules_cc//cc:cc_test.bzl", "cc_test")

genrule(name = "build_ok", outs = ["built.txt"], cmd = "echo built > $@")
cc_test(name = "tests", srcs = ["test.cc"])
EOF
      if [[ "$scenario" == cli-test-failure ]]; then
        printf 'int main() { return 23; }\n' >"$fixture/test.cc"
      elif [[ "$scenario" == cli-interrupt-cleanup ]]; then
        printf '#include <fstream>\n#include <unistd.h>\nint main() { std::ofstream("/workspace/cancel-ready"); sleep(600); }\n' >"$fixture/test.cc"
      else
        printf 'int main() { return 0; }\n' >"$fixture/test.cc"
      fi
    fi
    git -C "$fixture" add .
    git -C "$fixture" commit -qm fixture
    cargo build -p repo-sandbox-cli
    cli="$root/target/debug/repo-sandbox"
    report="$fixture/report.json"
    case "$scenario" in
      cli-build-success)
        start_cache_boundary_builder
        cache_index="$fixture/.repo-sandbox/cache/environment/index.json"
        [[ ! -e $cache_index ]]
        "$cli" build --repository "$fixture" --report-path "$report"
        assert_report_common "$report" removed
        grep -Fq '"status": "succeeded"' "$report"
        assert_step "$report" build bazel-build succeeded
        [[ -f $cache_index ]]
        first_source=$(report_snapshot_id "$report")
        [[ -n $first_source ]]
        # Remove the builder's internal cache: the warm run can now succeed as cached
        # only by importing the repository-owned local cache exported above.
        prune_cache_boundary_builder

        warm_report="$fixture/report-warm.json"
        warm_log="$result_directory/cli-build-warm.log"
        "$cli" build --repository "$fixture" --report-path "$warm_report" 2>&1 | tee "$warm_log"
        assert_report_common "$warm_report" removed
        assert_step "$warm_report" build bazel-build succeeded
        [[ $(report_snapshot_id "$warm_report") == "$first_source" ]]
        [[ -f $cache_index ]]
        assert_environment_cache_hit "$warm_log"

        printf '# source-change\n' >>"$fixture/BUILD.bazel"
        git -C "$fixture" add BUILD.bazel
        git -C "$fixture" commit -qm source-change
        changed_report="$fixture/report-changed.json"
        changed_log="$result_directory/cli-build-source-change.log"
        "$cli" build --repository "$fixture" --report-path "$changed_report" 2>&1 | tee "$changed_log"
        assert_report_common "$changed_report" removed
        assert_step "$changed_report" build bazel-build succeeded
        [[ $(report_snapshot_id "$changed_report") != "$first_source" ]]
        assert_environment_cache_hit "$changed_log"
        assert_task_source_rebuilt "$changed_log"
        echo 'cache=cold_then_warm cache_hit=verified source_digest=changed'
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
          sed -i "s|rust_version: \"1.97.0\"|rust_version: \"1.97.0\"\n    registry_repository: \"${REPO_SANDBOX_E2E_REGISTRY_REPOSITORY}\"|" \
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
        grep -Eq '"exit_code": [1-9][0-9]*' "$report"
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

        container_manifest=$(journal_container_records "$fixture/.repo-sandbox/tasks" | \
          awk -F '\t' -v retained="$retained" '$1 == retained { print $3; exit }')
        [[ -n $container_manifest && -f $container_manifest ]]
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

        owned_image=$(docker inspect --format '{{.Image}}' "$retained")
        [[ $owned_image =~ ^sha256:[0-9a-f]{64}$ ]]
        owned_image_reference=$(docker create "$owned_image" true)
        referenced_output="$result_directory/clean-referenced.log"
        run_expect_status 3 "$cli" clean --repository "$fixture" --yes \
          --include-images --include-cache >"$referenced_output" 2>&1
        grep -Fq 'unfinished' "$referenced_output"
        grep -Fq 'still referenced' "$referenced_output"
        docker image inspect "$owned_image" >/dev/null
        docker rm "$owned_image_reference" >/dev/null
        owned_image_reference=

        "$cli" clean --repository "$fixture" --yes --include-images --include-cache
        ! docker inspect "$retained" >/dev/null 2>&1
        [[ $(docker inspect --format '{{.Id}}' "$foreign") == "$before" ]]
        idempotent_output=$("$cli" clean --repository "$fixture" --yes \
          --include-images --include-cache)
        grep -Fq '0 succeeded' <<<"$idempotent_output"
        [[ $(docker inspect --format '{{.Id}}' "$foreign") == "$before" ]]
        docker rm "$foreign" >/dev/null
        foreign=
        echo 'dry_run=unchanged owner_mismatch=refused referenced=exit3 foreign_resource=preserved cleanup=idempotent'
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
          task_container=$(select_task_container "$fixture/.repo-sandbox/tasks")
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
        assert_oci_blob_digests "$layout"
        report_image_digest=$(sed -nE 's/^[[:space:]]*"image_digest": "(sha256:[0-9a-f]{64})",?$/\1/p' \
          "$report" | head -n 1)
        primary_descriptor=$(awk '
          /"digest"[[:space:]]*:/ {
            digest=$0; sub(/^.*"digest"[[:space:]]*:[[:space:]]*"/, "", digest); sub(/".*$/, "", digest)
          }
          /"architecture"[[:space:]]*:[[:space:]]*"amd64"/ { print digest; exit }
        ' "$layout/index.json")
        [[ $primary_descriptor == "$report_image_digest" ]]
        grep -Eq '"digest"[[:space:]]*:[[:space:]]*"sha256:[0-9a-f]{64}"' \
          "$layout/index.json"
        echo 'multi_platform=linux/amd64,linux/arm64 output=oci-layout runner=verified'
        ;;
      cli-registry-publish)
        "$cli" verify --repository "$fixture" --report-path "$report" --push
        assert_report_common "$report" removed
        assert_step "$report" build bazel-build succeeded
        assert_step "$report" test bazel-test succeeded
        digest=$(sed -nE 's/^[[:space:]]*"digest": "(sha256:[0-9a-f]{64})",?$/\1/p' \
          "$report" | head -n 1)
        [[ -n $digest ]]
        immutable="$registry_repository:sha256-${digest#sha256:}"
        alias="$registry_repository:verified"
        grep -Fq "\"immutable\": \"$immutable\"" "$report"
        grep -Fq "\"$alias\"" "$report"
        docker buildx imagetools inspect "$immutable" --raw >"$result_directory/immutable.json"
        docker buildx imagetools inspect "$alias" --raw >"$result_directory/alias.json"
        [[ $(sha256sum "$result_directory/immutable.json" | awk '{print $1}') == \
          $(sha256sum "$result_directory/alias.json" | awk '{print $1}') ]]
        python3 - "$result_directory/immutable.json" <<'PYMANIFEST'
import json, sys
with open(sys.argv[1]) as source:
    manifest = json.load(source)
assert manifest["schemaVersion"] == 2
assert manifest["mediaType"] in {
    "application/vnd.oci.image.manifest.v1+json",
    "application/vnd.docker.distribution.manifest.v2+json",
}
PYMANIFEST
        ! docker image inspect "$immutable" >/dev/null 2>&1
        docker pull --platform linux/amd64 "$immutable" >/dev/null
        docker image inspect --format '{{join .RepoDigests "\n"}}' "$immutable" | \
          grep -Fq "$registry_repository@$digest"

        multi_report="$fixture/report-multi-push.json"
        "$cli" verify --repository "$fixture" --report-path "$multi_report" \
          --platform linux/amd64 --platform linux/arm64 --push
        assert_report_common "$multi_report" removed
        assert_step "$multi_report" build bazel-build succeeded
        assert_step "$multi_report" test bazel-test succeeded
        multi_immutable=$(sed -nE 's/^[[:space:]]*"immutable": "([^"]+)",?$/\1/p' \
          "$multi_report")
        [[ -n $multi_immutable ]]
        multi_primary=$(sed -nE 's/^[[:space:]]*"image_digest": "(sha256:[0-9a-f]{64})",?$/\1/p' \
          "$multi_report")
        docker buildx imagetools inspect "$multi_immutable" --format '{{json .Manifest}}' \
          >"$result_directory/multi-manifest.json"
        grep -Fq "$multi_primary" "$result_directory/multi-manifest.json"
        [[ $(grep -o '"architecture":"amd64"' "$result_directory/multi-manifest.json" | wc -l) -eq 1 ]]
        [[ $(grep -o '"architecture":"arm64"' "$result_directory/multi-manifest.json" | wc -l) -eq 1 ]]
        docker buildx imagetools inspect "$alias" --format '{{json .Manifest}}' \
          >"$result_directory/multi-alias.json"
        [[ $(sha256sum "$result_directory/multi-manifest.json" | awk '{print $1}') == \
          $(sha256sum "$result_directory/multi-alias.json" | awk '{print $1}') ]]
        docker pull --platform linux/amd64 "$multi_immutable" >/dev/null
        docker pull --platform linux/arm64 "$multi_immutable" >/dev/null
        tags_before=$(curl --fail --silent \
          "http://127.0.0.1:$registry_port/v2/repo-sandbox/e2e/tags/list")

        printf 'int main() { return 23; }\n' >"$fixture/test.cc"
        git -C "$fixture" add test.cc
        git -C "$fixture" commit -qm failing-test
        failure_report="$fixture/report-push-failure.json"
        run_expect_status 11 "$cli" verify --repository "$fixture" \
          --report-path "$failure_report" --push
        assert_report_common "$failure_report" removed
        assert_step "$failure_report" test bazel-test command_failed
        ! grep -Fq '"published"' "$failure_report"
        tags_after=$(curl --fail --silent \
          "http://127.0.0.1:$registry_port/v2/repo-sandbox/e2e/tags/list")
        python3 - "$tags_before" "$tags_after" <<'PYTAGS'
import json, sys
before, after = [set(json.loads(value).get("tags") or []) for value in sys.argv[1:]]
assert {tag for tag in before if not tag.startswith("preflight-")} == {tag for tag in after if not tag.startswith("preflight-")}
PYTAGS
        echo 'registry_cli=published immutable=verified multi_platform=verified primary_digest=runner-verified alias=verified pullback=verified failed_verify_publish=none'
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
    printf 'module(name = "repo_sandbox_remote_fixture")\nbazel_dep(name = "rules_cc", version = "0.2.17")\n' >"$fixture/MODULE.bazel"
    cat >"$fixture/BUILD.bazel" <<'EOF'
load("@rules_cc//cc:cc_test.bzl", "cc_test")

genrule(name = "build_ok", outs = ["built.txt"], cmd = "echo built > $@")
cc_test(name = "tests", srcs = ["test.cc"])
EOF
    printf 'int main() { return 0; }\n' >"$fixture/test.cc"
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
      auth_args=(--git-https-username "$REPO_SANDBOX_E2E_HTTPS_USER" \
        --git-https-token-env REPO_SANDBOX_E2E_HTTPS_TOKEN)
    else
      remote_url=$REPO_SANDBOX_E2E_SSH_URL
      remote_ref=$REPO_SANDBOX_E2E_SSH_REF
      credential_marker=$(awk '!/^---/ && NF { print; exit }' \
        "$REPO_SANDBOX_E2E_SSH_KEY")
      auth_args=(--git-ssh-private-key "$REPO_SANDBOX_E2E_SSH_KEY" \
        --git-ssh-known-hosts "$REPO_SANDBOX_E2E_SSH_KNOWN_HOSTS")
    fi
    [[ -n $credential_marker ]]
    (
      cd "$remote_state"
      "$cli" verify --repository "$remote_url" --git-ref "$remote_ref" \
        --report-path "$report" "${auth_args[@]}"
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
  cli-profile-contracts)
    profile_root=$(mktemp -d)
    profile_secret=${REPO_SANDBOX_E2E_PROFILE_SECRET:-repo-sandbox-profile-secret-$RANDOM-$RANDOM}
    cleanup_profile_fixtures() { rm -rf -- "$profile_root"; }
    trap cleanup_profile_fixtures EXIT
    export REPO_SANDBOX_ENABLE_ACCEPTANCE_PROFILES=1
    cargo build -p repo-sandbox-cli
    cli="$root/target/debug/repo-sandbox"
    for profile in timeout memory temporary-storage architecture secret-artifact; do
      repository="$profile_root/$profile"
      mkdir -p "$repository"
      cat >"$repository/.repo-sandbox.yaml" <<EOF
version: 1
template:
  id: rust-bazel-acceptance-$profile
  parameters:
    platform: linux/amd64
EOF
      printf 'module(name = "repo_sandbox_profile_%s")\n' "${profile//-/_}" >"$repository/MODULE.bazel"
      printf 'exports_files(["fixture.txt"])\n' >"$repository/BUILD.bazel"
      printf 'fixture\n' >"$repository/fixture.txt"
      printf '.repo-sandbox/\n' >"$repository/.gitignore"
      git -C "$repository" init -q
      git -C "$repository" config user.email e2e@example.invalid
      git -C "$repository" config user.name repo-sandbox-e2e
      git -C "$repository" add .
      git -C "$repository" commit -qm fixture
    done
    for profile in timeout memory temporary-storage architecture; do
      repository="$profile_root/$profile"
      report="$result_directory/profile-$profile.json"
      expected_cleanup=removed
      case "$profile" in
        timeout)
          expected_exit=3; expected_status=timed_out; expected_phase=runner
          expected_step_status=timed_out
          ;;
        memory)
          expected_exit=3; expected_status=resource_exceeded; expected_phase=test
          expected_step_status=resource_exceeded
          ;;
        temporary-storage)
          expected_exit=3; expected_status=resource_exceeded; expected_phase=test
          expected_step_status=resource_exceeded
          ;;
        architecture)
          expected_exit=3; expected_status=infrastructure_failed; expected_phase=runner
          expected_step_status=
          expected_cleanup=not_needed
          ;;
      esac
      run_expect_status "$expected_exit" "$cli" verify --repository "$repository" \
        --report-path "$report"
      # Architecture fails at runner infrastructure validation, after snapshot
      # and immutable task-image construction, so its identities remain part of
      # the same auditable report schema as the other execution failures.
      assert_report_common "$report" "$expected_cleanup"
      grep -Fq "\"status\": \"$expected_status\"" "$report"
      assert_report_phase "$report" "$expected_phase" "$expected_exit"
      if [[ -n $expected_step_status ]]; then
        assert_step "$report" test "acceptance-$profile" "$expected_step_status"
      fi
      if [[ $profile == architecture ]]; then
        python3 - "$report" <<'PYARCHITECTURE'
import json, sys
with open(sys.argv[1]) as source:
    report = json.load(source)
assert report["status"]["operation"] == "validate runner platform"
assert "expected linux/arm64, inspected linux/amd64" in report["status"]["message"]
assert report["container_id"] is None
assert not report["steps"]
PYARCHITECTURE
      elif [[ $profile == memory ]]; then
        grep -Fq '"limit": "memory"' "$report"
      elif [[ $profile == temporary-storage ]]; then
        grep -Fq '"limit": "temporary_storage"' "$report"
      fi
    done

    artifact_repository="$profile_root/secret-artifact"
    artifact_report="$result_directory/profile-secret-artifact.json"
    REPO_SANDBOX_E2E_PROFILE_SECRET="$profile_secret" \
      "$cli" verify --repository "$artifact_repository" --report-path "$artifact_report"
    assert_report_common "$artifact_report" removed
    assert_report_phase "$artifact_report" complete 0
    artifact=$(find "$artifact_repository/.repo-sandbox/artifacts" -type f \
      -name profile-artifact.txt -print -quit)
    [[ -n $artifact ]]
    grep -Fxq 'artifact-ok' "$artifact"
    assert_report_exports_file "$artifact_report" "$artifact"
    ! grep -R -Fq -- "$profile_secret" "$artifact_repository/.repo-sandbox" "$artifact_report"
    echo 'profile_cli=timeout,memory,temporary-storage,architecture,secret-artifact status=verified'
    printf 'passed\n' >"$result_directory/cli-profile-contracts.passed"
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
