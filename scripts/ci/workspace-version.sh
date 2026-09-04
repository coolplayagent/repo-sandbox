#!/usr/bin/env bash
set -euo pipefail

[[ $# -le 1 ]] || { echo "usage: $0 [REPOSITORY_ROOT]" >&2; exit 64; }
root=${1:-$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)}

python_command=python3
if ! python3 -c 'import sys' >/dev/null 2>&1; then
  python_command=python
fi
"$python_command" - "$root" <<'PY'
import pathlib
import re
import sys

root = pathlib.Path(sys.argv[1])
cargo = (root / "Cargo.toml").read_text(encoding="utf-8")
workspace_package = re.search(
    r'^\[workspace\.package\]\s*$\n(.*?)(?=^\[|\Z)', cargo, re.MULTILINE | re.DOTALL
)
declared_workspace_versions = [] if not workspace_package else re.findall(
    r'^\s*version\s*=\s*"([^"]+)"\s*$', workspace_package.group(1), re.MULTILINE
)
if len(declared_workspace_versions) != 1:
    raise SystemExit("Cargo.toml must declare exactly one workspace package version")
version = declared_workspace_versions[0]

canonical = r"(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)"
if not isinstance(version, str) or not re.fullmatch(canonical, version):
    raise SystemExit("Cargo workspace version must be canonical MAJOR.MINOR.PATCH")

module = (root / "MODULE.bazel").read_text(encoding="utf-8")
module_match = re.search(
    r'module\(\s*name\s*=\s*"repo_sandbox",\s*version\s*=\s*"([^"]+)"', module
)
if not module_match or module_match.group(1) != version:
    raise SystemExit("Cargo workspace version does not match MODULE.bazel")

cli_build = (root / "apps/cli/BUILD.bazel").read_text(encoding="utf-8")
declared = re.findall(r'^\s*version\s*=\s*"([^"]+)"', cli_build, re.MULTILINE)
if not declared or any(item != version for item in declared):
    raise SystemExit("Cargo workspace version does not match every CLI Bazel target")

print(version)
PY
