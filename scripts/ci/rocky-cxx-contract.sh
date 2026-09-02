#!/usr/bin/env bash
set -euo pipefail

root=$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)
temporary=$(mktemp -d)
trap 'rm -rf "$temporary"' EXIT
mkdir -p "$temporary/bin"
touch "$temporary/libstdc++.so"

cat >"$temporary/bin/gcc" <<'EOF'
#!/usr/bin/env bash
[[ $1 == -print-file-name=libstdc++.so ]]
printf '%s\n' "$MOCK_LIBSTDCXX"
EOF
cat >"$temporary/bin/rpm" <<'EOF'
#!/usr/bin/env bash
if [[ $1 == -q && $2 == gcc-c++ && $# == 2 ]]; then
  [[ ${MOCK_GCC_CXX_INSTALLED:-yes} == yes ]]
elif [[ $1 == -qf && $2 == -- && $3 == "$MOCK_LIBSTDCXX" && $# == 3 ]]; then
  printf '%s\n' "$MOCK_OWNER"
else
  exit 90
fi
EOF
chmod +x "$temporary/bin/gcc" "$temporary/bin/rpm"

run_gate() {
  PATH="$temporary/bin:$PATH" MOCK_LIBSTDCXX="$temporary/libstdc++.so" \
    MOCK_OWNER=$1 MOCK_GCC_CXX_INSTALLED=${2:-yes} \
    "$root/scripts/ci/verify-rocky-cxx.sh" >/dev/null 2>&1
}

run_gate 'gcc-c++-8.5.0-28.el8_10.x86_64'
run_gate 'libstdc++-devel-8.5.0-28.el8_10.aarch64'

for owner in \
  'gcc-8.5.0-28.el8_10.x86_64' \
  'libstdc++-8.5.0-28.el8_10.aarch64' \
  'evil-gcc-c++-8.5.0' \
  'gcc-c++-' \
  $'gcc-c++-8.5.0\nlibstdc++-devel-8.5.0'; do
  if run_gate "$owner"; then
    echo "unexpected RPM owner accepted: $owner" >&2
    exit 1
  fi
done

if run_gate 'gcc-c++-8.5.0-28.el8_10.x86_64' no; then
  echo 'missing explicitly requested gcc-c++ package was accepted' >&2
  exit 1
fi

rm "$temporary/libstdc++.so"
if run_gate 'gcc-c++-8.5.0-28.el8_10.x86_64'; then
  echo 'nonexistent libstdc++.so path was accepted' >&2
  exit 1
fi

echo 'Rocky C++ package ownership contract passed'
