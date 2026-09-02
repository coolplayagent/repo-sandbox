#!/usr/bin/env bash
set -euo pipefail

root=$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)
temporary=$(mktemp -d)
trap 'rm -rf -- "$temporary"' EXIT
printf '#!/bin/sh\nexit 0\n' > "$temporary/candidate"
chmod +x "$temporary/candidate"
cat > "$temporary/readelf" <<'MOCK'
#!/usr/bin/env bash
set -euo pipefail
case $1 in
  --file-header) [[ $MOCK_ELF_MODE != nonelf ]] ;;
  --version-info)
    case $MOCK_ELF_MODE in
      baseline) echo 'Name: GLIBC_2.28  Flags: none' ;;
      legacy) echo 'Name: GLIBC_2.17  Flags: none' ;;
      newer) echo 'Name: GLIBC_2.29  Flags: none' ;;
      static) : ;;
    esac
    ;;
  *) exit 2 ;;
esac
MOCK
chmod +x "$temporary/readelf"
export PATH="$temporary:$PATH"

for mode in baseline legacy static; do
  export MOCK_ELF_MODE=$mode
  "$root/scripts/ci/verify-glibc-baseline.sh" "$temporary/candidate" 2.28
done
for mode in nonelf newer; do
  export MOCK_ELF_MODE=$mode
  if "$root/scripts/ci/verify-glibc-baseline.sh" "$temporary/candidate" 2.28 >/dev/null 2>&1; then
    echo "incompatible ELF mode accepted: $mode" >&2; exit 1
  fi
done

echo "glibc 2.28 compatibility contracts passed"
