#!/usr/bin/env bash
set -euo pipefail

[[ $# == 1 ]] || { echo "usage: $0 TAG" >&2; exit 64; }
tag=$1
root=$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)
version=$("$root/scripts/ci/workspace-version.sh")

python_command=python3
if ! python3 -c 'import sys' >/dev/null 2>&1; then python_command=python; fi
"$python_command" - "$tag" "$version" <<'PY'
import re
import sys

tag = sys.argv[1]
version = sys.argv[2]
match = re.fullmatch(r"v(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)", tag)
if not match:
    raise SystemExit("release tag must be canonical vMAJOR.MINOR.PATCH")
if tag[1:] != version:
    raise SystemExit(f"tag {tag} does not match workspace version {version}")
print(f"validated release {tag}")
PY
