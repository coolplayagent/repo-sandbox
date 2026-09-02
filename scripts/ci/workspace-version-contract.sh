#!/usr/bin/env bash
set -euo pipefail

root=$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)
temporary=$(mktemp -d)
trap 'rm -rf -- "$temporary"' EXIT
mkdir -p "$temporary/crates/cli"
cp "$root/Cargo.toml" "$temporary/Cargo.toml"
cp "$root/MODULE.bazel" "$temporary/MODULE.bazel"
cp "$root/crates/cli/BUILD.bazel" "$temporary/crates/cli/BUILD.bazel"

expected=$("$root/scripts/ci/workspace-version.sh")
[[ $("$root/scripts/ci/workspace-version.sh" "$temporary") == "$expected" ]]

sed -i '0,/version = "[^"]*"/s//version = "01.2.3"/' "$temporary/Cargo.toml"
if "$root/scripts/ci/workspace-version.sh" "$temporary" >/dev/null 2>&1; then
  echo "non-canonical workspace version was accepted" >&2; exit 1
fi
cp "$root/Cargo.toml" "$temporary/Cargo.toml"

sed -i '0,/version = "[^"]*"/s//version = "9.9.9"/' "$temporary/MODULE.bazel"
if "$root/scripts/ci/workspace-version.sh" "$temporary" >/dev/null 2>&1; then
  echo "divergent Bazel module version was accepted" >&2; exit 1
fi

echo "canonical synchronized workspace version contracts passed"
