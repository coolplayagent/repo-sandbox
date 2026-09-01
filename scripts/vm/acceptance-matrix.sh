#!/usr/bin/env bash
set -euo pipefail

[[ $# == 1 && -f $1 ]] || { echo "usage: $0 TARGETS.tsv" >&2; exit 64; }
target_file=$1
root=$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)
seen_amd64=false seen_arm64=false

while IFS=$'\t' read -r architecture host user port known_hosts identity extra; do
  [[ -z $architecture || $architecture == \#* ]] && continue
  [[ -z ${extra:-} ]] || { echo 'target row has too many fields' >&2; exit 64; }
  [[ $architecture == amd64 || $architecture == arm64 ]] || { echo 'invalid target architecture' >&2; exit 64; }
  args=(--host "$host" --user "$user" --port "$port" --source "$root" --acceptance-arch "$architecture")
  [[ -z $known_hosts ]] || args+=(--known-hosts "$known_hosts")
  [[ -z $identity ]] || args+=(--identity "$identity")
  "$root/scripts/vm/ssh-euleros.sh" "${args[@]}"
  if [[ $architecture == amd64 ]]; then seen_amd64=true; else seen_arm64=true; fi
done <"$target_file"

$seen_amd64 && $seen_arm64 || { echo 'target file must contain successful amd64 and arm64 rows' >&2; exit 1; }
echo 'EulerOS/HCE amd64 and arm64 VM acceptance matrix passed'
