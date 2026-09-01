#!/usr/bin/env bash
set -euo pipefail

[[ $# == 1 ]] || { echo "usage: $0 TAG" >&2; exit 64; }
tag=$1
root=$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)

python_command=python3
if ! python3 -c 'import sys' >/dev/null 2>&1; then
  python_command=python
fi
"$python_command" - "$root" "$tag" <<'PY'
import pathlib
import re
import sys
import tomllib

root = pathlib.Path(sys.argv[1])
tag = sys.argv[2]
match = re.fullmatch(r"v(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)", tag)
if not match:
    raise SystemExit("release tag must be canonical vMAJOR.MINOR.PATCH")
version = tag[1:]

with (root / "Cargo.toml").open("rb") as source:
    cargo_version = tomllib.load(source)["workspace"]["package"]["version"]
if cargo_version != version:
    raise SystemExit(f"tag {tag} does not match Cargo workspace version {cargo_version}")

module = (root / "MODULE.bazel").read_text(encoding="utf-8")
module_match = re.search(r'module\(\s*name\s*=\s*"repo_sandbox",\s*version\s*=\s*"([^"]+)"', module)
if not module_match or module_match.group(1) != version:
    raise SystemExit("tag version does not match MODULE.bazel")

cli_build = (root / "crates/cli/BUILD.bazel").read_text(encoding="utf-8")
declared = re.findall(r'^\s*version\s*=\s*"([^"]+)"', cli_build, re.MULTILINE)
if not declared or any(item != version for item in declared):
    raise SystemExit("tag version does not match every CLI Bazel target")
print(f"validated release {tag}")
PY
