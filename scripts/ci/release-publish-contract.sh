#!/usr/bin/env bash
set -euo pipefail

root=$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)
temporary=$(mktemp -d)
trap 'rm -rf -- "$temporary"' EXIT
mkdir -p "$temporary/bin" "$temporary/current" "$temporary/existing"

version=$("$root/scripts/ci/workspace-version.sh")
for asset in \
  "repo-sandbox-${version}-linux-amd64.tar.gz" \
  "repo-sandbox-${version}-linux-amd64.tar.gz.sha256" \
  "repo-sandbox-${version}-linux-arm64.tar.gz" \
  "repo-sandbox-${version}-linux-arm64.tar.gz.sha256" \
  SHA256SUMS; do
  printf 'deterministic:%s\n' "$asset" > "$temporary/current/$asset"
  cp "$temporary/current/$asset" "$temporary/existing/$asset"
done

cat > "$temporary/bin/git" <<'MOCK'
#!/usr/bin/env bash
set -euo pipefail
case ${1:-} in
  fetch) exit 0 ;;
  rev-parse)
    count=0
    [[ ! -f $MOCK_GIT_COUNT ]] || count=$(cat "$MOCK_GIT_COUNT")
    count=$((count + 1))
    printf '%s\n' "$count" > "$MOCK_GIT_COUNT"
    if [[ $count -gt 1 && -n ${MOCK_SECOND_REMOTE_SHA:-} ]]; then
      printf '%s\n' "$MOCK_SECOND_REMOTE_SHA"
    else
      printf '%s\n' "$MOCK_REMOTE_SHA"
    fi
    ;;
  *) exit 2 ;;
esac
MOCK
cat > "$temporary/bin/gh" <<'MOCK'
#!/usr/bin/env bash
set -euo pipefail
if [[ ${1:-} == api ]]; then
  endpoint=
  for argument in "$@"; do
    [[ $argument != repos/* ]] || endpoint=$argument
  done
  if [[ " $* " == *' --method DELETE '* ]]; then
    : > "$MOCK_DELETE_MARKER"
    exit 0
  fi
  case $endpoint in
    */releases/tags/*)
      if [[ " $* " == *' --jq '* ]]; then
        printf '123\tfalse\tv%s\n' "$MOCK_WORKSPACE_VERSION"
        exit 0
      fi
      if [[ $MOCK_RELEASE_STATE == existing ]]; then
        printf 'HTTP/2.0 200 OK\n\n{}\n'
        exit 0
      fi
      if [[ $MOCK_RELEASE_STATE == error ]]; then
        printf 'HTTP/2.0 500 Internal Server Error\n' >&2
        exit 1
      fi
      printf 'HTTP/2.0 404 Not Found\n' >&2
      exit 1
      ;;
    *'/releases?per_page=100')
      [[ $MOCK_RELEASE_STATE != error-list ]] || { echo 'list failed' >&2; exit 1; }
      case $MOCK_RELEASE_STATE in
        draft|draft-flips|wrong-detail) printf '123\ttrue\tv%s\n' "$MOCK_WORKSPACE_VERSION" ;;
        multiple)
          printf '123\ttrue\tv%s\n' "$MOCK_WORKSPACE_VERSION"
          printf '124\ttrue\tv%s\n' "$MOCK_WORKSPACE_VERSION"
          ;;
        non-draft) printf '123\tfalse\tv%s\n' "$MOCK_WORKSPACE_VERSION" ;;
        wrong-tag) printf '123\ttrue\tv9.9.9\n' ;;
        absent) : ;;
      esac
      exit 0
      ;;
    */releases/123)
      case $MOCK_RELEASE_STATE in
        draft) printf '123\ttrue\tv%s\n' "$MOCK_WORKSPACE_VERSION" ;;
        draft-flips) printf '123\tfalse\tv%s\n' "$MOCK_WORKSPACE_VERSION" ;;
        wrong-detail) printf '123\ttrue\tv9.9.9\n' ;;
        *) exit 2 ;;
      esac
      exit 0
      ;;
    *) exit 2 ;;
  esac
fi
if [[ ${1:-} == release && ${2:-} == download ]]; then
  while [[ $# -gt 0 ]]; do
    if [[ $1 == --dir ]]; then destination=$2; shift 2; else shift; fi
  done
  cp "$MOCK_EXISTING_DIR"/* "$destination/"
  exit 0
fi
if [[ ${1:-} == release && ${2:-} == create ]]; then
  : > "$MOCK_CREATE_MARKER"
  exit 0
fi
exit 2
MOCK
chmod +x "$temporary/bin/git" "$temporary/bin/gh"

sha=0123456789abcdef0123456789abcdef01234567
export PATH="$temporary/bin:$PATH" MOCK_REMOTE_SHA=$sha
export MOCK_EXISTING_DIR="$temporary/existing" MOCK_CREATE_MARKER="$temporary/created"
export MOCK_DELETE_MARKER="$temporary/deleted" MOCK_GIT_COUNT="$temporary/git-count"
export MOCK_WORKSPACE_VERSION=$version

# A moved tag is rejected before any GitHub release API or write is attempted.
export MOCK_REMOTE_SHA=1123456789abcdef0123456789abcdef01234567 MOCK_RELEASE_STATE=absent
rm -f "$MOCK_GIT_COUNT"
if "$root/scripts/ci/publish-release.sh" "v$version" owner/repository "$temporary/current" "$sha" \
  >/dev/null 2>&1; then
  echo "moved tag was accepted" >&2; exit 1
fi
[[ ! -e $MOCK_CREATE_MARKER ]]

# A tag moved after the existence check is caught by the immediate second fetch.
export MOCK_REMOTE_SHA=$sha MOCK_SECOND_REMOTE_SHA=2123456789abcdef0123456789abcdef01234567
rm -f "$MOCK_GIT_COUNT"
if "$root/scripts/ci/publish-release.sh" "v$version" owner/repository "$temporary/current" "$sha" \
  >/dev/null 2>&1; then
  echo "tag move immediately before create was accepted" >&2; exit 1
fi
[[ ! -e $MOCK_CREATE_MARKER ]]
unset MOCK_SECOND_REMOTE_SHA

# An interrupted partial draft is mutable. Delete only its numeric release object,
# retain the verified tag, and recreate from this run's complete asset set.
export MOCK_RELEASE_STATE=draft
rm -f "$MOCK_GIT_COUNT" "$MOCK_CREATE_MARKER" "$MOCK_DELETE_MARKER"
"$root/scripts/ci/publish-release.sh" "v$version" owner/repository "$temporary/current" "$sha" >/dev/null
[[ -f $MOCK_DELETE_MARKER && -f $MOCK_CREATE_MARKER ]]
rm -f "$MOCK_CREATE_MARKER" "$MOCK_DELETE_MARKER"

# The numeric detail endpoint must still match the listed draft exactly.
export MOCK_RELEASE_STATE=wrong-detail
rm -f "$MOCK_GIT_COUNT"
if "$root/scripts/ci/publish-release.sh" "v$version" owner/repository "$temporary/current" "$sha" \
  >/dev/null 2>&1; then
  echo "draft detail with mismatched tag_name was accepted" >&2; exit 1
fi
[[ ! -e $MOCK_CREATE_MARKER && ! -e $MOCK_DELETE_MARKER ]]

# A draft that becomes published/changes during recovery is never deleted.
export MOCK_RELEASE_STATE=draft-flips
rm -f "$MOCK_GIT_COUNT"
if "$root/scripts/ci/publish-release.sh" "v$version" owner/repository "$temporary/current" "$sha" \
  >/dev/null 2>&1; then
  echo "draft state change before delete was accepted" >&2; exit 1
fi
[[ ! -e $MOCK_CREATE_MARKER && ! -e $MOCK_DELETE_MARKER ]]

for unsafe_state in multiple non-draft error-list; do
  export MOCK_RELEASE_STATE=$unsafe_state
  rm -f "$MOCK_GIT_COUNT"
  if "$root/scripts/ci/publish-release.sh" "v$version" owner/repository "$temporary/current" "$sha" \
    >/dev/null 2>&1; then
    echo "unsafe release-list state was accepted: $unsafe_state" >&2; exit 1
  fi
  [[ ! -e $MOCK_CREATE_MARKER && ! -e $MOCK_DELETE_MARKER ]]
done

# An unrelated draft is ignored; zero exact matches permits one fresh create.
export MOCK_RELEASE_STATE=wrong-tag
rm -f "$MOCK_GIT_COUNT"
"$root/scripts/ci/publish-release.sh" "v$version" owner/repository "$temporary/current" "$sha" >/dev/null
[[ -f $MOCK_CREATE_MARKER && ! -e $MOCK_DELETE_MARKER ]]
rm -f "$MOCK_CREATE_MARKER"

# An API/auth/network failure is not treated as proof that a release is absent.
export MOCK_RELEASE_STATE=error
rm -f "$MOCK_GIT_COUNT"
if "$root/scripts/ci/publish-release.sh" "v$version" owner/repository "$temporary/current" "$sha" \
  >/dev/null 2>&1; then
  echo "release API failure was treated as absence" >&2; exit 1
fi
[[ ! -e $MOCK_CREATE_MARKER ]]
[[ ! -e $MOCK_DELETE_MARKER ]]

# An existing release must be byte-for-byte identical; it is never overwritten.
export MOCK_REMOTE_SHA=$sha MOCK_RELEASE_STATE=existing
rm -f "$MOCK_GIT_COUNT"
printf 'different\n' > "$temporary/existing/SHA256SUMS"
if "$root/scripts/ci/publish-release.sh" "v$version" owner/repository "$temporary/current" "$sha" \
  >/dev/null 2>&1; then
  echo "different existing release was accepted" >&2; exit 1
fi
[[ ! -e $MOCK_CREATE_MARKER ]]
[[ ! -e $MOCK_DELETE_MARKER ]]

cp "$temporary/current/SHA256SUMS" "$temporary/existing/SHA256SUMS"
rm -f "$MOCK_GIT_COUNT"
"$root/scripts/ci/publish-release.sh" "v$version" owner/repository "$temporary/current" "$sha" >/dev/null
[[ ! -e $MOCK_CREATE_MARKER ]]

# Only an explicit API 404 permits one create with the validated asset set.
export MOCK_RELEASE_STATE=absent
rm -f "$MOCK_GIT_COUNT"
"$root/scripts/ci/publish-release.sh" "v$version" owner/repository "$temporary/current" "$sha" >/dev/null
[[ -f $MOCK_CREATE_MARKER ]]

echo "release rerun, immutable asset, and moved-tag contracts passed"
