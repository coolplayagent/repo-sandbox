#!/usr/bin/env bash
set -euo pipefail

root=$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd -P)
temporary=$(mktemp -d)
trap 'rm -rf "$temporary"' EXIT
real_git=$(command -v git)

cat >"$temporary/git" <<'SH'
#!/usr/bin/env bash
set -euo pipefail
expected_root=${EXPECTED_ROOT:?}
[[ $# == 6 ]]
[[ $1 == -c ]]
[[ $2 == "safe.directory=$expected_root" ]]
[[ $3 == merge-base ]]
[[ $4 == --is-ancestor ]]
[[ $5 == "$EXPECTED_SHA" ]]
[[ $6 == origin/main ]] && exit 70
SH
chmod +x "$temporary/git"

sha=165d179aaacd082eca58494509f2c9c0537ca488
version=$("$root/scripts/ci/workspace-version.sh")

# The mock exits 70 only after observing the command-scoped exact-workspace
# exception and the expected immutable SHA. A bare git invocation, wildcard
# safe.directory, or argument drift therefore makes this contract fail.
if EXPECTED_ROOT=$root EXPECTED_SHA=$sha PATH="$temporary:$PATH" \
  GITHUB_WORKSPACE=$root GITHUB_SHA=$sha \
  "$root/scripts/ci/validate-release-inputs.sh" "v$version" >/dev/null 2>&1; then
  echo 'release input validator did not execute the ownership-safe Git gate' >&2
  exit 1
else
  status=$?
  [[ $status == 70 ]]
fi

# Exercise the real Git ancestry gate as well as the tag/version validation.
head=$($real_git -C "$root" rev-parse HEAD)
EXPECTED_ROOT=$root EXPECTED_SHA=unused PATH=$PATH \
  GITHUB_WORKSPACE=$root GITHUB_SHA=$head \
  "$root/scripts/ci/validate-release-inputs.sh" "v$version" >/dev/null

for bad_sha in 'HEAD' '165d179;id' 'ABCDEF0123456789ABCDEF0123456789ABCDEF01' \
  '000000000000000000000000000000000000000'; do
  if EXPECTED_ROOT=$root EXPECTED_SHA=$bad_sha PATH="$temporary:$PATH" \
    GITHUB_WORKSPACE=$root GITHUB_SHA=$bad_sha \
    "$root/scripts/ci/validate-release-inputs.sh" "v$version" >/dev/null 2>&1; then
    echo "unsafe or non-ancestor release SHA accepted: $bad_sha" >&2
    exit 1
  fi
done

if EXPECTED_ROOT=$root EXPECTED_SHA=$sha PATH="$temporary:$PATH" \
  GITHUB_WORKSPACE=$temporary GITHUB_SHA=$sha \
  "$root/scripts/ci/validate-release-inputs.sh" "v$version" >/dev/null 2>&1; then
  echo 'release validator accepted a different workspace' >&2
  exit 1
fi

echo 'release input ownership, workspace, SHA, ancestry, and version contracts passed'
