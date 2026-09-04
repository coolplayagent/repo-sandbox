#!/usr/bin/env bash

# The caller provides install_verified_binary so the same checksum-enforcing
# installer is used on every supported host.
buildx_has_carbon_copy_capability() {
  docker buildx imagetools create --help 2>&1 | grep -Fq -- '--prefer-index'
}

ensure_buildx_carbon_copy_capability() {
  local url=$1 expected=$2 destination=$3
  if buildx_has_carbon_copy_capability; then
    return 0
  fi

  install -d -m 0755 "$(dirname -- "$destination")"
  install_verified_binary "$url" "$expected" "$destination"
  if ! buildx_has_carbon_copy_capability; then
    echo "installed Buildx does not provide imagetools create --prefer-index; remove any shadowing Docker CLI plugin and rerun the installer" >&2
    return 1
  fi
}
