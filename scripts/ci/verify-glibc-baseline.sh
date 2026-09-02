#!/usr/bin/env bash
set -euo pipefail

[[ $# == 2 ]] || { echo "usage: $0 BINARY MAX_GLIBC_MAJOR.MINOR" >&2; exit 64; }
binary=$1
baseline=$2
[[ $baseline =~ ^(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)$ ]] || {
  echo "glibc baseline must be canonical MAJOR.MINOR" >&2; exit 64;
}
[[ -f $binary && -x $binary ]] || { echo "ELF candidate is not executable" >&2; exit 1; }
command -v readelf >/dev/null || { echo "readelf is required for glibc verification" >&2; exit 1; }
readelf --file-header "$binary" >/dev/null || { echo "CLI binary is not ELF" >&2; exit 1; }

baseline_major=${baseline%%.*}
baseline_minor=${baseline#*.}
while IFS= read -r required; do
  [[ -z $required ]] && continue
  version=${required#GLIBC_}
  major=${version%%.*}
  minor=${version#*.}
  if (( major > baseline_major || (major == baseline_major && minor > baseline_minor) )); then
    echo "CLI requires $required, newer than supported GLIBC_$baseline" >&2
    exit 1
  fi
done < <(readelf --version-info "$binary" | grep -oE 'GLIBC_[0-9]+\.[0-9]+' | LC_ALL=C sort -u || true)
