#!/usr/bin/env bash
set -euo pipefail

assert_cached_step() {
  local log=$1
  local description=$2
  # The total instruction count and optional platform prefix are presentation
  # details. Keep the ordinal so distinct RUN instructions remain required.
  awk -v description="$description" '
    function stable(text) {
      gsub(/\[linux\/[^ ]+ /, "[", text)
      gsub(/\/[0-9]+\]/, "]", text)
      return text
    }
    {
      line=stable($0)
      wanted=stable(description)
      offset=index(line, wanted)
      next_char=substr(line, offset + length(wanted), 1)
      if (offset && (next_char == "" || next_char ~ /[[:space:]]/)) expected[$1]=1
    }
    $2 == "CACHED" { cached[$1]=1 }
    END {
      count=0
      for (step in expected) {
        count++
        if (!cached[step]) {
          print "required cache vertex not cached: " step " for " description > "/dev/stderr"
          exit 1
        }
      }
      if (count == 0) print "required cache operation missing: " description > "/dev/stderr"
      exit count == 0
    }
  ' "$log"
}

assert_environment_cached() {
  local warm_log=$1
  assert_cached_step "$warm_log" '[toolchain-build 2/2] RUN' || return 1
  assert_cached_step "$warm_log" '[environment-base 2/7] RUN' || return 1
  assert_cached_step "$warm_log" '[environment-base 3/7] COPY --from=toolchain-build /usr/local/cargo/' || return 1
  assert_cached_step "$warm_log" '[environment-base 4/7] COPY --from=toolchain-build /usr/local/rustup/' || return 1
  assert_cached_step "$warm_log" '[environment-base 5/7] COPY --from=toolchain-build /toolchain/bin/bazel' || return 1
  assert_cached_step "$warm_log" '[environment-base 6/7] COPY --from=toolchain-build /toolchain/bin/bazelisk' || return 1
  assert_cached_step "$warm_log" '[environment-base 7/7] RUN' || return 1
  assert_cached_step "$warm_log" '[offline-seed 2/2] RUN' || return 1
  assert_cached_step "$warm_log" '[offline-seed 3/3] RUN' || return 1
  assert_cached_step "$warm_log" '[environment 1/4] COPY --from=offline-seed /toolchain/bazel-seed/cache/repos/' || return 1
  assert_cached_step "$warm_log" '[environment 4/4] RUN' || return 1
}

if [[ ${1-} == --self-test-cache-assertions ]]; then
  cache_fixture=$(mktemp)
  trap 'rm -f "$cache_fixture" "$cache_fixture.miss"' EXIT
  printf '%s\n' '#17 [offline-seed 2/3] RUN online-seed' '#17 CACHED' \
    '#19 [offline-seed 3/3] RUN offline-verify' '#19 CACHED' >"$cache_fixture"
  assert_cached_step "$cache_fixture" '[offline-seed 2/2] RUN'
  cat >"$cache_fixture" <<'CACHELOG'
#11 [linux/amd64 toolchain-build 2/2] RUN install-tools
#11 CACHED
#12 [linux/amd64 environment-base 2/7] RUN install-runtime
#12 CACHED
#13 [linux/amd64 environment-base 3/7] COPY --from=toolchain-build /usr/local/cargo/
#13 CACHED
#14 [linux/amd64 environment-base 4/7] COPY --from=toolchain-build /usr/local/rustup/
#14 CACHED
#15 [linux/amd64 environment-base 5/7] COPY --from=toolchain-build /toolchain/bin/bazel
#15 CACHED
#16 [linux/amd64 environment-base 6/7] COPY --from=toolchain-build /toolchain/bin/bazelisk
#16 CACHED
#17 [linux/amd64 environment-base 7/7] RUN verify-tools
#17 CACHED
#18 [offline-seed 2/3] RUN online-seed
#18 CACHED
#19 [offline-seed 3/3] RUN offline-verify
#19 CACHED
#20 [environment 1/4] COPY --from=offline-seed /toolchain/bazel-seed/cache/repos/
#20 CACHED
#21 [environment 4/4] RUN install-wrapper
#21 CACHED
CACHELOG
  assert_environment_cached "$cache_fixture"
  for vertex in $(seq 11 21); do
    sed "s/^#$vertex CACHED$/#$vertex DONE 0.1s/" "$cache_fixture" >"$cache_fixture.miss"
    if assert_environment_cached "$cache_fixture.miss" 2>/dev/null; then echo "uncached vertex $vertex was accepted" >&2; exit 1; fi
    sed "/^#$vertex /d" "$cache_fixture" >"$cache_fixture.miss"
    if assert_environment_cached "$cache_fixture.miss" 2>/dev/null; then echo "missing required vertex $vertex was accepted" >&2; exit 1; fi
  done
  sed '/^#15 /s|/toolchain/bin/bazel$|/toolchain/bin/bazelisk|' "$cache_fixture" >"$cache_fixture.miss"
  if assert_environment_cached "$cache_fixture.miss" 2>/dev/null; then echo 'bazelisk substituted for bazel was accepted' >&2; exit 1; fi
  sed '/^#11 /s/] RUN /] RUNNER /' "$cache_fixture" >"$cache_fixture.miss"
  if assert_environment_cached "$cache_fixture.miss" 2>/dev/null; then echo 'operation prefix collision was accepted' >&2; exit 1; fi
  rm -f "$cache_fixture.miss"
  if assert_cached_step "$cache_fixture" '[missing-stage 1/4] COPY' 2>/dev/null; then exit 1; fi
  printf '%s\n' '#17 [offline-seed 2/3] RUN online-seed' '#17 CACHED' \
    '#19 [offline-seed 3/3] RUN offline-verify' '#19 DONE 0.1s' >"$cache_fixture"
  if assert_cached_step "$cache_fixture" '[offline-seed 3/3] RUN' 2>/dev/null; then exit 1; fi
  exit 0
fi

repo_root=$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)
temporary=$(mktemp -d)
run_id="issue1-$$"
builder=${REPO_SANDBOX_BUILDER:-}
builder_args=()
[[ -z $builder ]] || builder_args=(--builder "$builder")

cleanup() {
  docker image rm --force \
    "repo-sandbox-$run_id-environment-amd64" "repo-sandbox-$run_id-environment-arm64" \
    "repo-sandbox-$run_id-task-amd64" "repo-sandbox-$run_id-task-arm64" \
    "repo-sandbox-$run_id-baseline-amd64" "repo-sandbox-$run_id-baseline-arm64" \
    "repo-sandbox-$run_id-acceptance-task-amd64" "repo-sandbox-$run_id-acceptance-task-arm64" \
    "repo-sandbox-$run_id-acceptance-baseline-amd64" "repo-sandbox-$run_id-acceptance-baseline-arm64" \
    >/dev/null 2>&1 || true
  rm -rf "$temporary"
}
trap cleanup EXIT

retry_external_build() {
  local attempt=1
  local maximum=3
  while true; do
    if "$@"; then
      return 0
    fi
    if (( attempt >= maximum )); then
      echo "external build failed after $maximum attempts" >&2
      return 1
    fi
    echo "external build attempt $attempt failed; retrying" >&2
    sleep $((attempt * 5))
    attempt=$((attempt + 1))
  done
}

cp -R "$repo_root/tests/multistage/." "$temporary/context"
cp -R "$repo_root/templates/rust-bazel/context/offline-baseline" "$temporary/context/"
cp "$repo_root/templates/rust-bazel/context/bazel" "$temporary/context/bazel"
cp "$temporary/context/source/src/main.rs" "$temporary/main.rs.original"
printf '%s\n' 'issue1-secret-marker-must-not-leak' >"$temporary/github-token"


printf '%-8s %15s %15s %15s %10s\n' platform compressed_single compressed_multi unpacked_multi reduction
for architecture in amd64 arm64; do
  platform="linux/$architecture"
  environment="repo-sandbox-$run_id-environment-$architecture"
  task="repo-sandbox-$run_id-task-$architecture"
  baseline="repo-sandbox-$run_id-baseline-$architecture"
  acceptance_task="repo-sandbox-$run_id-acceptance-task-$architecture"
  acceptance_baseline="repo-sandbox-$run_id-acceptance-baseline-$architecture"
  cache_cold="$temporary/cache-$architecture-cold"
  cache_warm="$temporary/cache-$architecture-warm"
  cold_log="$temporary/environment-$architecture-cold.log"
  warm_log="$temporary/environment-$architecture-warm.log"

  retry_external_build docker buildx build "${builder_args[@]}" --provenance=false \
    --platform "$platform" --target environment \
    --secret "id=github_token,src=$temporary/github-token" \
    --cache-to "type=local,dest=$cache_cold,mode=max" --progress plain --load \
    --tag "$environment" "$repo_root/templates/rust-bazel/context" 2>&1 | tee "$cold_log"
  cold_environment_identity=$(docker image inspect "$environment" --format '{{.Id}}')
  retry_external_build docker buildx build "${builder_args[@]}" --provenance=false \
    --platform "$platform" --target environment \
    --secret "id=github_token,src=$temporary/github-token" \
    --cache-from "type=local,src=$cache_cold" --cache-to "type=local,dest=$cache_warm,mode=max" \
    --progress plain --load --tag "$environment" \
    "$repo_root/templates/rust-bazel/context" 2>&1 | tee "$warm_log"
  warm_environment_identity=$(docker image inspect "$environment" --format '{{.Id}}')
  test "$cold_environment_identity" = "$warm_environment_identity"
  printf '%s environment cold/warm identity: %s\n' \
    "$architecture" "$warm_environment_identity"
  assert_environment_cached "$warm_log"

  source_digest=$(tar -C "$temporary/context/source" --sort=name --mtime='UTC 1970-01-01' \
    --owner=0 --group=0 --numeric-owner -cf - . | sha256sum | awk '{print "sha256:"$1}')
  retry_external_build docker build --provenance=false --platform "$platform" --target task \
    --build-arg "ENVIRONMENT_IMAGE=$environment" --build-arg "SOURCE_DIGEST=$source_digest" \
    --progress plain --tag "$task" -f "$temporary/context/Dockerfile.task" "$temporary/context"
  environment_before_source_change=$(docker image inspect "$environment" --format '{{.Id}}')
  original_task_identity=$(docker image inspect "$task" --format '{{.Id}}')

  # Only business source changes. The immutable environment remains identical,
  # while task COPY/label work is invalidated and rebuilt.
  printf '\n// cache-boundary-change\n' >>"$temporary/context/source/src/main.rs"
  changed_digest=$(tar -C "$temporary/context/source" --sort=name --mtime='UTC 1970-01-01' \
    --owner=0 --group=0 --numeric-owner -cf - . | sha256sum | awk '{print "sha256:"$1}')
  task_log="$temporary/task-$architecture-source-change.log"
  retry_external_build docker build --provenance=false --platform "$platform" --target task \
    --build-arg "ENVIRONMENT_IMAGE=$environment" --build-arg "SOURCE_DIGEST=$changed_digest" \
    --progress plain --tag "$task" -f "$temporary/context/Dockerfile.task" \
    "$temporary/context" 2>&1 | tee "$task_log"
  environment_after_source_change=$(docker image inspect "$environment" --format '{{.Id}}')
  changed_task_identity=$(docker image inspect "$task" --format '{{.Id}}')
  test "$environment_before_source_change" = "$environment_after_source_change"
  printf '%s source-only environment identity: before=%s after=%s\n' \
    "$architecture" "$environment_before_source_change" "$environment_after_source_change"
  test "$original_task_identity" != "$changed_task_identity"
  changed_labels=$(docker image inspect "$task" --format '{{json .Config.Labels}}')
  grep -Fq "\"io.repo-sandbox.source.digest\":\"$changed_digest\"" <<<"$changed_labels"

  # Restore the exact original source and rebuild the final task. From here on,
  # task and baseline contain byte-identical business input and digest labels.
  cp "$temporary/main.rs.original" "$temporary/context/source/src/main.rs"
  retry_external_build docker build --provenance=false --platform "$platform" --target task \
    --build-arg "ENVIRONMENT_IMAGE=$environment" --build-arg "SOURCE_DIGEST=$source_digest" \
    --progress plain --tag "$task" -f "$temporary/context/Dockerfile.task" "$temporary/context"
  labels=$(docker image inspect "$task" --format '{{json .Config.Labels}}')
  restored_task_identity=$(docker image inspect "$task" --format '{{.Id}}')
  test "$restored_task_identity" = "$original_task_identity"
  grep -Fq "\"io.repo-sandbox.source.digest\":\"$source_digest\"" <<<"$labels"
  printf '%s restored task identity=%s source_digest=%s\n' \
    "$architecture" "$restored_task_identity" "$source_digest"

  retry_external_build docker buildx build "${builder_args[@]}" --provenance=false \
    --platform "$platform" --target task \
    --build-arg "SOURCE_DIGEST=$source_digest" --progress plain --load --tag "$baseline" \
    -f "$temporary/context/Dockerfile.single-stage" "$temporary/context"

  # Only these disposable build-stage checks receive host networking so Cargo
  # and Bazel can fetch public dependencies. Final task containers retain the
  # runner's isolated default network contract.
  retry_external_build docker build --network host --provenance=false \
    --platform "$platform" --target acceptance \
    --build-arg "ENVIRONMENT_IMAGE=$environment" --build-arg "SOURCE_DIGEST=$source_digest" \
    --progress plain --tag "$acceptance_task" -f "$temporary/context/Dockerfile.task" \
    "$temporary/context"
  # Reuse the exact loaded baseline. Building its full Dockerfile here would
  # repeat its seed on the Engine builder, including emulated ARM toolchains.
  retry_external_build docker build --network host --provenance=false \
    --platform "$platform" --target acceptance --build-arg "ACCEPTANCE_IMAGE=$baseline" \
    --progress plain --tag "$acceptance_baseline" \
    -f "$temporary/context/Dockerfile.acceptance" "$temporary/context"
  printf '%s build-stage acceptance network=host scope=public-dependency-download passed\n' \
    "$architecture"

  history=$(docker history --no-trunc "$task")
  ! grep -Fq 'issue1-secret-marker-must-not-leak' <<<"$history"
  docker run --rm --platform "$platform" "$task" sh -ec '
    test ! -e /toolchain
    test ! -e /run/secrets/github_token
    test -z "$(find /root/.cache -mindepth 1 -print -quit 2>/dev/null)"
    test -d /var/cache/repo-sandbox/bazel/cache/repos/v1
    test -s /usr/local/share/repo-sandbox/offline-baseline/MODULE.bazel.lock
    test -x /usr/local/libexec/repo-sandbox/bazel-9.2.0
    test ! -e /root/.cache/bazel
    test ! -e /root/.cache/bazelisk
    test ! -e /toolchain-downloads
    test ! -e /usr/local/cargo/registry
    test ! -e /usr/local/cargo/git
    test -z "$(find /var/lib/apt/lists -mindepth 1 -print -quit 2>/dev/null)"
    ! command -v curl >/dev/null
    command -v rustc >/dev/null
    command -v cargo >/dev/null
    command -v bazel >/dev/null
    command -v git >/dev/null
    command -v cc >/dev/null
    ! grep -R -F issue1-secret-marker-must-not-leak \
      /var/cache/repo-sandbox /usr/local/share/repo-sandbox >/dev/null 2>&1
  '
  printf '%s final-image history/filesystem security scan: passed\n' "$architecture"

  docker run --rm --network none --platform "$platform" \
    --env USE_BAZEL_VERSION=latest --env BAZEL_OPTS=--bazelrc=/workspace/.bazelrc \
    "$task" sh -ec '
      bazel version | grep -Fx "Build label: 9.2.0"
      bazel --batch build //:rust_binary
      bazel --batch test //...
    '
  printf '%s final-task Bazel closure: network=none pinned=9.2.0 rc=ignored passed\n' \
    "$architecture"

  if [[ $architecture == amd64 ]]; then
    missing_log="$temporary/offline-closure-missing.log"
    if docker run --rm --network none --platform "$platform" "$task" sh -ec '
      rm -rf /var/cache/repo-sandbox/bazel/cache/repos
      bazel --batch build //...
    ' >"$missing_log" 2>&1; then
      echo 'Bazel unexpectedly succeeded without the embedded repository closure' >&2
      exit 1
    fi
    grep -Eqi 'registry|repository|download' "$missing_log"

    corrupt_log="$temporary/offline-closure-corrupt.log"
    if docker run --rm --network none --platform "$platform" "$task" sh -ec '
      sed -i "0,/8a28e4a/{s/8a28e4a/ffffffff/}" MODULE.bazel.lock
      bazel --batch build //...
    ' >"$corrupt_log" 2>&1; then
      echo 'Bazel unexpectedly accepted a corrupt registry-content pin' >&2
      exit 1
    fi
    grep -Eqi 'registry|checksum|download|hash' "$corrupt_log"

    foreign_log="$temporary/offline-closure-foreign-module.log"
    if docker run --rm --network none --platform "$platform" "$task" sh -ec '
      printf '\''%s\n'\'' '\''bazel_dep(name = "rules_go", version = "0.50.1")'\'' \
        >> MODULE.bazel
      bazel --batch build //...
    ' >"$foreign_log" 2>&1; then
      echo 'Bazel unexpectedly downloaded a module outside the central baseline' >&2
      exit 1
    fi
    grep -Eqi 'registry|repository|download' "$foreign_log"
    echo 'amd64 missing/corrupt/foreign offline closure fail-closed: passed'
  fi

  docker image save "$task" -o "$temporary/multi-$architecture.tar"
  docker image save "$baseline" -o "$temporary/single-$architecture.tar"
  gzip -n -9 -c "$temporary/multi-$architecture.tar" >"$temporary/multi-$architecture.tar.gz"
  gzip -n -9 -c "$temporary/single-$architecture.tar" >"$temporary/single-$architecture.tar.gz"
  multi_compressed=$(wc -c <"$temporary/multi-$architecture.tar.gz")
  single_compressed=$(wc -c <"$temporary/single-$architecture.tar.gz")
  multi_unpacked=$(docker image inspect "$task" --format '{{.Size}}')
  reduction=$(awk -v single="$single_compressed" -v multi="$multi_compressed" \
    'BEGIN { printf "%.2f", (single-multi)*100/single }')
  printf '%-8s %15d %15d %15d %9s%%\n' "$architecture" "$single_compressed" \
    "$multi_compressed" "$multi_unpacked" "$reduction"
  awk -v reduction="$reduction" 'BEGIN { exit !(reduction >= 10.0) }'
done
