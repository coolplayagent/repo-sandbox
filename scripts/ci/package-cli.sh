#!/usr/bin/env bash
set -euo pipefail

[[ $# == 4 ]] || { echo "usage: $0 TAG PLATFORM BINARY OUTPUT_DIRECTORY" >&2; exit 64; }
root=$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)
exec python3 "$root/tools/release/package_cli.py" \
  --tag "$1" --platform "$2" --binary "$3" --output "$4"
