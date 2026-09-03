#!/usr/bin/env bash
set -euo pipefail

[[ $# == 2 ]] || { echo "usage: $0 HTTPS_URL OUTPUT" >&2; exit 64; }
url=$1
output=$2
[[ $url == https://* && $url != *$'\n'* && $url != *$'\r'* ]] || {
  echo 'download URL must be single-line HTTPS' >&2
  exit 64
}
[[ -n $output ]] || { echo 'download output must be non-empty' >&2; exit 64; }

partial="${output}.partial"
rm -f -- "$output" "$partial"
last_status=1
delays=(1 2 4 8 16)
for attempt in 1 2 3 4 5 6; do
  rm -f -- "$partial"
  if curl --fail --location --proto '=https' --tlsv1.2 \
    --output "$partial" "$url"; then
    mv -- "$partial" "$output"
    exit 0
  else
    last_status=$?
  fi
  rm -f -- "$partial"
  if [[ $attempt -lt 6 ]]; then
    sleep "${delays[attempt - 1]}"
  fi
done

echo "HTTPS download failed after 6 attempts: $url" >&2
exit "$last_status"
