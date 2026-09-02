#!/usr/bin/env bash
set -euo pipefail

rpm -q gcc-c++ >/dev/null

libstdcxx=$(gcc -print-file-name=libstdc++.so)
[[ $libstdcxx != libstdc++.so && -f $libstdcxx ]] || {
  echo "gcc did not resolve libstdc++.so to a real file" >&2
  exit 1
}

owner=$(rpm -qf -- "$libstdcxx")
[[ $owner =~ ^(gcc-c\+\+|libstdc\+\+-devel)-[A-Za-z0-9._+~:-]+$ ]] || {
  echo "unexpected libstdc++.so package owner: $owner" >&2
  exit 1
}
