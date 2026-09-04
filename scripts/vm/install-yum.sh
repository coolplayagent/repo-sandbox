#!/usr/bin/env bash
set -euo pipefail

readonly BAZELISK_VERSION=v1.29.0
readonly BAZELISK_AMD64_SHA256=5a408715e932c0250d28bd84555f12edbf70117de42f9181691c736eacc4a992
readonly BAZELISK_ARM64_SHA256=e20e8b0f4f240091b7a55bf17b9398bd4f40ee70ae0208dff95dd4c445fb4010
readonly BUILDX_VERSION=v0.15.1
readonly BUILDX_AMD64_SHA256=8d486f0088b7407a90ad675525ba4a17d0a537741b9b33fe3391a88cafa2dd0b
readonly BUILDX_ARM64_SHA256=13f4ffd2b6922e941d6b6a9faee73ec9b8cab5b309ef90dfadf48142c2a47f34

if ((EUID != 0)); then
  command -v sudo >/dev/null 2>&1 || { echo 'installation requires root or sudo' >&2; exit 77; }
  exec sudo -- "$0" "$@"
fi

source_dir=
while (($#)); do
  case $1 in
    --source)
      (($# >= 2)) || { echo '--source requires a value' >&2; exit 64; }
      source_dir=$2
      shift 2
      ;;
    *) echo "unknown argument: $1" >&2; exit 64 ;;
  esac
done

[[ -n $source_dir && $source_dir == /* && -f $source_dir/MODULE.bazel ]] || {
  echo '--source must be an absolute repo-sandbox source directory' >&2; exit 64
}
[[ $(uname -s) == Linux ]] || { echo 'a Linux VM is required' >&2; exit 1; }
case $(uname -m) in
  x86_64) artifact_arch=amd64; foreign_binfmt=qemu-aarch64; bazelisk_sha=$BAZELISK_AMD64_SHA256; buildx_sha=$BUILDX_AMD64_SHA256 ;;
  aarch64) artifact_arch=arm64; foreign_binfmt=qemu-x86_64; bazelisk_sha=$BAZELISK_ARM64_SHA256; buildx_sha=$BUILDX_ARM64_SHA256 ;;
  *) echo 'unsupported architecture: expected x86_64 or aarch64' >&2; exit 1 ;;
esac

command -v rpm >/dev/null 2>&1 || { echo 'an RPM-based VM is required' >&2; exit 1; }
package_manager=$(command -v dnf || command -v yum || true)
[[ -n $package_manager ]] || { echo 'dnf or yum is required' >&2; exit 1; }
command -v systemctl >/dev/null 2>&1 && [[ -d /run/systemd/system ]] || {
  echo 'a running systemd instance is required' >&2; exit 1
}

package_exists() {
  rpm -q -- "$1" >/dev/null 2>&1 || "$package_manager" -q list --available "$1" >/dev/null 2>&1
}

install_packages() {
  local missing=() package
  for package in "$@"; do
    rpm -q -- "$package" >/dev/null 2>&1 || missing+=("$package")
  done
  ((${#missing[@]} == 0)) || "$package_manager" install -y -- "${missing[@]}"
}

install_packages ca-certificates curl git tar gzip

if ! command -v docker >/dev/null 2>&1; then
  docker_package=
  for candidate in docker-engine docker-ce docker; do
    if package_exists "$candidate"; then docker_package=$candidate; break; fi
  done
  [[ -n $docker_package ]] || {
    echo 'no Docker engine package is available in the already-configured repositories' >&2; exit 1
  }
  "$package_manager" install -y -- "$docker_package"
fi

systemctl cat docker.service >/dev/null 2>&1 || {
  echo 'Docker is installed but docker.service is unavailable' >&2; exit 1
}

unsafe_docker_listener_configured() {
  systemctl cat docker.service 2>/dev/null | grep -Eqi 'tcp://' && return 0
  local file
  for file in /etc/docker/daemon.json /etc/sysconfig/docker /etc/sysconfig/docker-storage; do
    [[ ! -f $file ]] || ! grep -Eqi 'tcp://' "$file" || return 0
  done
  local pid
  for pid in $(pidof dockerd 2>/dev/null || true); do
    ! tr '\0' ' ' <"/proc/$pid/cmdline" | grep -Eqi 'tcp://' || return 0
  done
  return 1
}
unsafe_docker_listener_configured && {
  echo 'refusing to start or use a Docker daemon configured with a TCP listener' >&2; exit 1
}
systemctl is-enabled --quiet docker.service || systemctl enable docker.service >/dev/null
systemctl is-active --quiet docker.service || systemctl start docker.service

case ${DOCKER_HOST:-unix:///var/run/docker.sock} in
  unix://*|ssh://*) ;;
  *) echo 'refusing Docker access outside a local Unix socket or SSH transport' >&2; exit 1 ;;
esac
if [[ -z ${DOCKER_HOST:-} ]]; then
  resolved_docker_host=$(docker context inspect --format '{{.Endpoints.docker.Host}}')
  case $resolved_docker_host in
    unix://*|ssh://*) ;;
    *) echo 'active Docker context is not a local Unix socket or SSH transport' >&2; exit 1 ;;
  esac
fi
docker info >/dev/null

install_verified_binary() {
  local url=$1 expected=$2 destination=$3 temporary actual
  [[ $url == https://* && $expected =~ ^[a-f0-9]{64}$ ]] || {
    echo "refusing unverified or non-HTTPS download: $url" >&2; return 1
  }
  temporary=$(mktemp "${destination}.repo-sandbox.XXXXXX")
  if ! curl --proto '=https' --tlsv1.2 --fail --location --silent --show-error \
    --output "$temporary" "$url"; then
    rm -f -- "$temporary"; return 1
  fi
  actual=$(sha256sum "$temporary" | awk '{print $1}')
  if [[ $actual != "$expected" ]]; then
    rm -f -- "$temporary"
    echo "SHA-256 mismatch for $url" >&2
    return 1
  fi
  install -m 0755 "$temporary" "$destination"
  rm -f -- "$temporary"
}

if ! command -v bazelisk >/dev/null 2>&1; then
  install_verified_binary \
    "https://github.com/bazelbuild/bazelisk/releases/download/$BAZELISK_VERSION/bazelisk-linux-$artifact_arch" \
    "$bazelisk_sha" /usr/local/bin/bazelisk
fi
[[ -e /usr/local/bin/bazel ]] || ln -s bazelisk /usr/local/bin/bazel

if ! docker buildx version >/dev/null 2>&1; then
  install -d -m 0755 /usr/local/lib/docker/cli-plugins
  install_verified_binary \
    "https://github.com/docker/buildx/releases/download/$BUILDX_VERSION/buildx-$BUILDX_VERSION.linux-$artifact_arch" \
    "$buildx_sha" /usr/local/lib/docker/cli-plugins/docker-buildx
fi
docker buildx version >/dev/null

# doctor requires the opposite architecture to be executable. Use only a
# package already exposed by the operator-configured repositories.
if [[ ! -f /proc/sys/fs/binfmt_misc/$foreign_binfmt ]] ||
  ! grep -qs '^enabled' "/proc/sys/fs/binfmt_misc/$foreign_binfmt"; then
  package_exists qemu-user-static || {
    echo 'qemu-user-static is required but unavailable in configured repositories' >&2; exit 1
  }
  install_packages qemu-user-static
  systemctl try-restart systemd-binfmt.service >/dev/null 2>&1 || true
fi
[[ -f /proc/sys/fs/binfmt_misc/$foreign_binfmt ]] &&
  grep -qs '^enabled' "/proc/sys/fs/binfmt_misc/$foreign_binfmt" || {
    echo "$foreign_binfmt is not enabled after installing qemu-user-static" >&2; exit 1
  }

# Preserve the selected builder and all cache. Inspecting/bootstraping the
# selected builder is state-local; this installer never creates, removes or
# switches builders and never prunes Docker resources.
docker buildx inspect --bootstrap >/dev/null

(cd "$source_dir" && bazelisk build //:repo-sandbox)
cli_source=$(cd "$source_dir" && bazelisk cquery --output=files //:repo-sandbox | head -n1)
[[ -x $source_dir/$cli_source ]] || { echo 'Bazel did not produce repo-sandbox' >&2; exit 1; }
if [[ ! -x /usr/local/bin/repo-sandbox ]] || ! cmp -s "$source_dir/$cli_source" /usr/local/bin/repo-sandbox; then
  install -m 0755 "$source_dir/$cli_source" /usr/local/bin/repo-sandbox
fi

repo-sandbox --version
repo-sandbox doctor
