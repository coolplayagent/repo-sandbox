#!/usr/bin/env bash
set -euo pipefail

root=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/../.." && pwd -P)
# shellcheck source=../lib/buildx-capability.sh
source "$root/scripts/lib/buildx-capability.sh"

temporary=$(mktemp -d)
trap 'rm -rf -- "$temporary"' EXIT
events=$temporary/events
plugin_state=v0.14.1
installed_state=capable

docker() {
  printf 'docker:%s\n' "$*" >>"$events"
  [[ $* == 'buildx imagetools create --help' ]] || return 91
  case $plugin_state in
    capable) printf '      --prefer-index  When source is an image, output a manifest list\n' ;;
    v0.14.1) printf 'Usage: docker buildx imagetools create [OPTIONS]\n' ;;
    broken) return 1 ;;
  esac
}

install() {
  printf 'install:%s\n' "$*" >>"$events"
}

install_verified_binary() {
  printf 'download:%s|%s|%s\n' "$1" "$2" "$3" >>"$events"
  plugin_state=$installed_state
}

url=https://github.com/docker/buildx/releases/download/v0.15.1/buildx-v0.15.1.linux-amd64
sha=8d486f0088b7407a90ad675525ba4a17d0a537741b9b33fe3391a88cafa2dd0b
destination=$temporary/docker-buildx

# An already capable plugin is left untouched.
plugin_state=capable
: >"$events"
ensure_buildx_carbon_copy_capability "$url" "$sha" "$destination"
[[ $(grep -c '^docker:' "$events") == 1 ]]
! grep -q '^download:' "$events"

# An installed v0.14-style plugin without --prefer-index is replaced by the
# pinned binary and the capability is checked again afterwards.
plugin_state=v0.14.1
installed_state=capable
: >"$events"
ensure_buildx_carbon_copy_capability "$url" "$sha" "$destination"
[[ $(grep -c '^docker:' "$events") == 2 ]]
grep -Fxq "download:$url|$sha|$destination" "$events"

# A downloaded plugin that is still shadowed or incapable fails closed with an
# actionable error after the second capability probe.
plugin_state=v0.14.1
installed_state=v0.14.1
: >"$events"
error=$temporary/error
if ensure_buildx_carbon_copy_capability "$url" "$sha" "$destination" 2>"$error"; then
  echo 'incapable installed Buildx unexpectedly passed' >&2
  exit 1
fi
[[ $(grep -c '^docker:' "$events") == 2 ]]
grep -Fq 'remove any shadowing Docker CLI plugin and rerun the installer' "$error"

echo 'Buildx installer capability fixtures passed'
