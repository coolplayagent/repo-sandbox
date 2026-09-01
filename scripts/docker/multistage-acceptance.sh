#!/usr/bin/env bash
set -euo pipefail

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
    >/dev/null 2>&1 || true
  rm -rf "$temporary"
}
trap cleanup EXIT

cp -R "$repo_root/tests/multistage/." "$temporary/context"
printf '%s\n' 'issue1-secret-marker-must-not-leak' >"$temporary/github-token"

printf '%-8s %15s %15s %15s %10s\n' platform compressed_single compressed_multi unpacked_multi reduction
for architecture in amd64 arm64; do
  platform="linux/$architecture"
  environment="repo-sandbox-$run_id-environment-$architecture"
  task="repo-sandbox-$run_id-task-$architecture"
  baseline="repo-sandbox-$run_id-baseline-$architecture"
  cache_cold="$temporary/cache-$architecture-cold"
  cache_warm="$temporary/cache-$architecture-warm"
  cold_log="$temporary/environment-$architecture-cold.log"
  warm_log="$temporary/environment-$architecture-warm.log"

  docker buildx build "${builder_args[@]}" --platform "$platform" --target environment \
    --secret "id=github_token,src=$temporary/github-token" \
    --cache-to "type=local,dest=$cache_cold,mode=max" --progress plain --load \
    --tag "$environment" "$repo_root/templates/rust-bazel/context" 2>&1 | tee "$cold_log"
  docker buildx build "${builder_args[@]}" --platform "$platform" --target environment \
    --secret "id=github_token,src=$temporary/github-token" \
    --cache-from "type=local,src=$cache_cold" --cache-to "type=local,dest=$cache_warm,mode=max" \
    --progress plain --load --tag "$environment" \
    "$repo_root/templates/rust-bazel/context" 2>&1 | tee "$warm_log"
  grep -q 'CACHED' "$warm_log"

  source_digest=$(tar -C "$temporary/context/source" --sort=name --mtime='UTC 1970-01-01' \
    --owner=0 --group=0 --numeric-owner -cf - . | sha256sum | awk '{print "sha256:"$1}')
  docker buildx build "${builder_args[@]}" --platform "$platform" --target task \
    --build-arg "ENVIRONMENT_IMAGE=$environment" --build-arg "SOURCE_DIGEST=$source_digest" \
    --progress plain --load --tag "$task" -f "$temporary/context/Dockerfile.task" "$temporary/context"

  # Only business source changes. The immutable environment remains identical,
  # while task COPY/label work is invalidated and rebuilt.
  printf '\n// cache-boundary-change\n' >>"$temporary/context/source/src/main.rs"
  changed_digest=$(tar -C "$temporary/context/source" --sort=name --mtime='UTC 1970-01-01' \
    --owner=0 --group=0 --numeric-owner -cf - . | sha256sum | awk '{print "sha256:"$1}')
  task_log="$temporary/task-$architecture-source-change.log"
  docker buildx build "${builder_args[@]}" --platform "$platform" --target task \
    --build-arg "ENVIRONMENT_IMAGE=$environment" --build-arg "SOURCE_DIGEST=$changed_digest" \
    --progress plain --load --tag "$task" -f "$temporary/context/Dockerfile.task" \
    "$temporary/context" 2>&1 | tee "$task_log"
  grep -q 'CACHED' "$task_log"
  sed -i '$d' "$temporary/context/source/src/main.rs"
  sed -i '$d' "$temporary/context/source/src/main.rs"

  docker buildx build "${builder_args[@]}" --platform "$platform" --target task \
    --build-arg "SOURCE_DIGEST=$source_digest" --progress plain --load --tag "$baseline" \
    -f "$temporary/context/Dockerfile.single-stage" "$temporary/context"

  for image in "$task" "$baseline"; do
    docker run --rm --platform "$platform" "$image" cargo test --locked
    docker run --rm --platform "$platform" "$image" bazel --batch build //:rust_binary
  done

  labels=$(docker image inspect "$task" --format '{{json .Config.Labels}}')
  grep -Fq "\"io.repo-sandbox.source.digest\":\"$changed_digest\"" <<<"$labels"
  history=$(docker history --no-trunc "$task")
  ! grep -Fq 'issue1-secret-marker-must-not-leak' <<<"$history"
  docker run --rm --platform "$platform" "$task" sh -ec '
    test ! -e /toolchain
    test ! -e /run/secrets/github_token
    test ! -e /root/.cache/bazel
    test ! -e /usr/local/cargo/registry
    test ! -e /usr/local/cargo/git
    test -z "$(find /var/lib/apt/lists -mindepth 1 -print -quit 2>/dev/null)"
    ! command -v curl >/dev/null
    command -v rustc >/dev/null
    command -v cargo >/dev/null
    command -v bazel >/dev/null
    command -v git >/dev/null
    command -v cc >/dev/null
  '

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
