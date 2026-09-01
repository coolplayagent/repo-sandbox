#!/usr/bin/env bash
set -euo pipefail

[[ $# == 1 ]] || { echo "usage: $0 INSTALL_DIRECTORY" >&2; exit 64; }
install_directory=$1
version=1.29.0

case "$(uname -m)" in
  x86_64)
    artifact=bazelisk-linux-amd64
    expected=5a408715e932c0250d28bd84555f12edbf70117de42f9181691c736eacc4a992
    ;;
  aarch64|arm64)
    artifact=bazelisk-linux-arm64
    expected=e20e8b0f4f240091b7a55bf17b9398bd4f40ee70ae0208dff95dd4c445fb4010
    ;;
  *) echo "unsupported Bazelisk architecture: $(uname -m)" >&2; exit 64 ;;
esac

mkdir -p -- "$install_directory"
destination="$install_directory/bazelisk"
curl --fail --location --proto '=https' --tlsv1.2 \
  --output "$destination" \
  "https://github.com/bazelbuild/bazelisk/releases/download/v${version}/${artifact}"
printf '%s  %s\n' "$expected" "$destination" | sha256sum --check --strict
chmod 0755 "$destination"
echo "$install_directory" >> "${GITHUB_PATH:-/dev/null}"
