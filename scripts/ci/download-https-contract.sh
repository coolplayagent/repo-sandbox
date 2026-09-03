#!/usr/bin/env bash
set -euo pipefail

root=$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)
temporary=$(mktemp -d)
trap 'rm -rf -- "$temporary"' EXIT
mkdir -p "$temporary/bin" "$temporary/output"

cat >"$temporary/bin/curl" <<'MOCK'
#!/usr/bin/env bash
set -euo pipefail
[[ $# == 8 ]]
[[ $1 == --fail && $2 == --location && $3 == --proto && $4 == '=https' ]]
[[ $5 == --tlsv1.2 && $6 == --output ]]
output=$7
url=$8
[[ ! -e $output ]]
attempt=0
[[ ! -f $MOCK_COUNTER ]] || attempt=$(cat "$MOCK_COUNTER")
attempt=$((attempt + 1))
printf '%s\n' "$attempt" >"$MOCK_COUNTER"
printf '%s\n' "$url" >>"$MOCK_URL_LOG"
printf 'partial attempt %s' "$attempt" >"$output"
if [[ $attempt -le ${MOCK_FAILURES:-0} ]]; then
  exit 22
fi
printf 'downloaded %s' "$url" >"$output"
MOCK
cat >"$temporary/bin/sleep" <<'MOCK'
#!/usr/bin/env bash
set -euo pipefail
[[ $# == 1 && $1 =~ ^(1|2|4|8|16)$ ]]
printf '%s\n' "$1" >>"$MOCK_SLEEP_LOG"
MOCK
chmod +x "$temporary/bin/curl" "$temporary/bin/sleep"

export PATH="$temporary/bin:$PATH"
export MOCK_COUNTER="$temporary/counter"
export MOCK_URL_LOG="$temporary/urls"
export MOCK_SLEEP_LOG="$temporary/sleeps"

# Simulate curl 7.61: the mock accepts the portable options but has no
# --retry-all-errors support. Three failed partial responses must be removed.
export MOCK_FAILURES=3
output="$temporary/output/retried"
"$root/scripts/ci/download-https.sh" https://example.test/retried "$output"
[[ $(cat "$MOCK_COUNTER") == 4 ]]
[[ $(cat "$output") == 'downloaded https://example.test/retried' ]]
[[ ! -e ${output}.partial ]]
[[ $(cat "$MOCK_SLEEP_LOG") == $'1\n2\n4' ]]

# Both release assets use the same protected downloader.
rm -f "$MOCK_COUNTER" "$MOCK_URL_LOG" "$MOCK_SLEEP_LOG"
export MOCK_FAILURES=0
for asset in cli.tar.gz cli.tar.gz.sha256; do
  "$root/scripts/ci/download-https.sh" "https://example.test/$asset" \
    "$temporary/output/$asset"
done
[[ $(cat "$MOCK_COUNTER") == 2 ]]
[[ $(cat "$MOCK_URL_LOG") == $'https://example.test/cli.tar.gz\nhttps://example.test/cli.tar.gz.sha256' ]]

# A permanent failure is bounded, returns curl's error, and leaves no bytes.
rm -f "$MOCK_COUNTER" "$MOCK_URL_LOG" "$MOCK_SLEEP_LOG"
export MOCK_FAILURES=99
output="$temporary/output/permanent"
set +e
"$root/scripts/ci/download-https.sh" https://example.test/permanent "$output" \
  >/dev/null 2>&1
status=$?
set -e
if [[ $status -eq 0 ]]; then
  echo 'permanent curl failure was accepted' >&2
  exit 1
fi
[[ $status == 22 ]]
[[ $(cat "$MOCK_COUNTER") == 6 ]]
[[ $(cat "$MOCK_SLEEP_LOG") == $'1\n2\n4\n8\n16' ]]
[[ ! -e $output && ! -e ${output}.partial ]]

echo 'Portable HTTPS download retry contract passed'
