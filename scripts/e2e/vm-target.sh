#!/usr/bin/env bash
set -euo pipefail

[[ $# == 2 && -f $1 ]] || { echo "usage: $0 TARGETS.tsv RESULT_DIRECTORY" >&2; exit 64; }
targets=$1
result_directory=$2
root=$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)
"$root/scripts/vm/acceptance-matrix.sh" "$targets"
mkdir -p -- "$result_directory"
printf 'passed\n' >"$result_directory/vm-matrix.passed"
echo 'vm_matrix=passed'
