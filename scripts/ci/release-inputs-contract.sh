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
[[ $# == 8 ]]
[[ $1 == -C ]]
[[ $2 == "$expected_root" ]]
[[ $3 == -c ]]
[[ $4 == "safe.directory=$expected_root" ]]
[[ $5 == merge-base ]]
[[ $6 == --is-ancestor ]]
[[ $7 == "$EXPECTED_SHA" ]]
[[ $8 == origin/main ]]
exit "${EXPECTED_STATUS:-0}"
SH
chmod +x "$temporary/git"

sha=165d179aaacd082eca58494509f2c9c0537ca488
version=$("$root/scripts/ci/workspace-version.sh")

# The mock exits 70 only after observing the command-scoped exact-workspace
# exception and the expected immutable SHA. A bare git invocation, wildcard
# safe.directory, or argument drift therefore makes this contract fail.
if EXPECTED_ROOT=$root EXPECTED_SHA=$sha EXPECTED_STATUS=70 PATH="$temporary:$PATH" \
  GITHUB_WORKSPACE=$root GITHUB_SHA=$sha \
  "$root/scripts/ci/validate-release-inputs.sh" "v$version" >/dev/null 2>&1; then
  echo 'release input validator did not execute the ownership-safe Git gate' >&2
  exit 1
else
  status=$?
  [[ $status == 70 ]]
fi

# Exercise the real ancestry and tag/version gates in a self-contained clone.
# The PR checkout is intentionally shallow and need not contain origin/main;
# only the disposable fixture receives the comparison ref.
fixture="$temporary/repository"
"$real_git" clone --quiet --no-local "$root" "$fixture"
fixture=$(cd "$fixture" && pwd -P)
cp "$root/scripts/ci/validate-release-inputs.sh" \
  "$fixture/scripts/ci/validate-release-inputs.sh"
fixture_head=$("$real_git" -C "$fixture" rev-parse HEAD)
"$real_git" -C "$fixture" update-ref refs/remotes/origin/main "$fixture_head"
GITHUB_WORKSPACE=$fixture GITHUB_SHA=$fixture_head \
  "$fixture/scripts/ci/validate-release-inputs.sh" "v$version" >/dev/null

for bad_sha in 'HEAD' '165d179;id' 'ABCDEF0123456789ABCDEF0123456789ABCDEF01'; do
  if EXPECTED_ROOT=$root EXPECTED_SHA=$bad_sha EXPECTED_STATUS=0 PATH="$temporary:$PATH" \
    GITHUB_WORKSPACE=$root GITHUB_SHA=$bad_sha \
    "$root/scripts/ci/validate-release-inputs.sh" "v$version" >/dev/null 2>&1; then
    echo "unsafe or non-ancestor release SHA accepted: $bad_sha" >&2
    exit 1
  fi
done

non_ancestor=0000000000000000000000000000000000000000
if EXPECTED_ROOT=$root EXPECTED_SHA=$non_ancestor EXPECTED_STATUS=1 PATH="$temporary:$PATH" \
  GITHUB_WORKSPACE=$root GITHUB_SHA=$non_ancestor \
  "$root/scripts/ci/validate-release-inputs.sh" "v$version" >/dev/null 2>&1; then
  echo "non-ancestor release SHA accepted: $non_ancestor" >&2
  exit 1
fi

fixture_tree=$("$real_git" -C "$fixture" rev-parse 'HEAD^{tree}')
unrelated_main=$(printf '%s\n' 'unrelated main fixture' | \
  GIT_AUTHOR_NAME=contract GIT_AUTHOR_EMAIL=contract@example.invalid \
  GIT_AUTHOR_DATE=2000-01-01T00:00:00Z \
  GIT_COMMITTER_NAME=contract GIT_COMMITTER_EMAIL=contract@example.invalid \
  GIT_COMMITTER_DATE=2000-01-01T00:00:00Z \
  "$real_git" -C "$fixture" commit-tree "$fixture_tree")
"$real_git" -C "$fixture" update-ref refs/remotes/origin/main "$unrelated_main"
if GITHUB_WORKSPACE=$fixture GITHUB_SHA=$fixture_head \
  "$fixture/scripts/ci/validate-release-inputs.sh" "v$version" >/dev/null 2>&1; then
  echo "real Git ancestry gate accepted $fixture_head against an unrelated root" >&2
  exit 1
fi

if EXPECTED_ROOT=$root EXPECTED_SHA=$sha EXPECTED_STATUS=0 PATH="$temporary:$PATH" \
  GITHUB_WORKSPACE=$temporary GITHUB_SHA=$sha \
  "$root/scripts/ci/validate-release-inputs.sh" "v$version" >/dev/null 2>&1; then
  echo 'release validator accepted a different workspace' >&2
  exit 1
fi

echo 'release input ownership, workspace, SHA, ancestry, and version contracts passed'
