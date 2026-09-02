#!/usr/bin/env bash
set -euo pipefail

temporary=$(mktemp)
trap 'rm -f "$temporary"' EXIT

bazelisk build --action_env=PATH --config=release //:repo-sandbox
bazelisk query 'kind(".*_test rule", //...)' --output=label >"$temporary"

mapfile -t test_targets <"$temporary"
[[ ${#test_targets[@]} -gt 0 ]] || {
  echo 'Bazel query returned no release test targets' >&2
  exit 1
}

for target in "${test_targets[@]}"; do
  [[ $target =~ ^//[A-Za-z0-9_./+-]*:[A-Za-z0-9_./+-]+$ ]] || {
    echo "Bazel query returned a non-canonical test label: $target" >&2
    exit 1
  }
  [[ $target != //tests/multistage/source:rust_binary ]] || {
    echo 'Docker fixture genrule cannot be run as a release test' >&2
    exit 1
  }
done

bazelisk test --action_env=PATH "${test_targets[@]}"
