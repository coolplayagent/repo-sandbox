#!/usr/bin/env bash
set -euo pipefail

root=$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)
temporary=$(mktemp -d)
trap 'rm -rf "$temporary"' EXIT
mkdir -p "$temporary/bin"

cat >"$temporary/bin/bazelisk" <<'EOF'
#!/usr/bin/env bash
set -euo pipefail
case $1 in
  build)
    [[ $* == 'build --action_env=PATH --config=release //:repo-sandbox' ]]
    touch "$MOCK_STATE/build"
    ;;
  query)
    [[ $2 == 'kind(".*_test rule", //...)' && $3 == --output=label && $# == 3 ]]
    [[ ${MOCK_QUERY_FAIL:-no} != yes ]]
    printf '%s' "${MOCK_QUERY_OUTPUT:-}"
    touch "$MOCK_STATE/query"
    ;;
  test)
    shift
    [[ $1 == --action_env=PATH ]]
    shift
    [[ $# == 3 ]]
    [[ $1 == //crates/core:core_test ]]
    [[ $2 == //apps/cli:cli_test ]]
    [[ $3 == //:root_test ]]
    touch "$MOCK_STATE/test"
    ;;
  *) exit 91 ;;
esac
EOF
chmod +x "$temporary/bin/bazelisk"

valid_targets=$'//crates/core:core_test\n//apps/cli:cli_test\n//:root_test\n'
PATH="$temporary/bin:$PATH" MOCK_STATE="$temporary" MOCK_QUERY_OUTPUT="$valid_targets" \
  "$root/scripts/ci/release-bazel.sh" >/dev/null
[[ -f $temporary/build && -f $temporary/query && -f $temporary/test ]]

for invalid in '' $'//crates/core:core_test\nnot-a-label\n' \
  $'//tests/multistage/source:rust_binary\n'; do
  rm -f "$temporary/build" "$temporary/query" "$temporary/test"
  if PATH="$temporary/bin:$PATH" MOCK_STATE="$temporary" MOCK_QUERY_OUTPUT="$invalid" \
    "$root/scripts/ci/release-bazel.sh" >/dev/null 2>&1; then
    echo 'invalid release Bazel query output was accepted' >&2
    exit 1
  fi
  [[ ! -f $temporary/test ]]
done

if PATH="$temporary/bin:$PATH" MOCK_STATE="$temporary" MOCK_QUERY_FAIL=yes \
  MOCK_QUERY_OUTPUT="$valid_targets" "$root/scripts/ci/release-bazel.sh" >/dev/null 2>&1; then
  echo 'failed Bazel query was accepted' >&2
  exit 1
fi

echo 'Release Bazel test-selection contract passed'
