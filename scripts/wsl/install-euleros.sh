#!/usr/bin/env bash
set -euo pipefail

readonly EXPECTED_VERSION="2.10.7"
readonly BAZELISK_URL="https://github.com/bazelbuild/bazelisk/releases/download/v1.29.0/bazelisk-linux-amd64"
readonly BAZELISK_SHA256="5a408715e932c0250d28bd84555f12edbf70117de42f9181691c736eacc4a992"
readonly BUILDX_URL="https://github.com/docker/buildx/releases/download/v0.14.1/buildx-v0.14.1.linux-amd64"
readonly BUILDX_SHA256="68e4f8895331ade982de8085a8c137b8af65f3ef95040b6c6113552243638508"

source_dir=""
configure_systemd_only=false
while (($#)); do
  case "$1" in
    --source)
      (($# >= 2)) || { echo "--source requires a value" >&2; exit 64; }
      source_dir=$2
      shift 2
      ;;
    --configure-systemd-only)
      configure_systemd_only=true
      shift
      ;;
    *) echo "unknown argument: $1" >&2; exit 64 ;;
  esac
done

[[ $EUID -eq 0 ]] || { echo "run this installer as root (the PowerShell bootstrap does this)" >&2; exit 1; }
[[ -n $source_dir && $source_dir == /* && -f "$source_dir/MODULE.bazel" ]] || {
  echo "--source must be an absolute repo-sandbox source directory" >&2
  exit 64
}
[[ $(uname -s) == Linux && -n ${WSL_INTEROP:-} ]] || {
  echo "unsupported host: this installer runs only inside WSL2" >&2
  exit 1
}
grep -Eqi 'microsoft-standard-WSL2|WSL2' /proc/sys/kernel/osrelease || {
  echo "unsupported WSL generation: WSL2 is required" >&2
  exit 1
}
[[ $(uname -m) == x86_64 ]] || { echo "unsupported architecture: expected x86_64" >&2; exit 1; }
grep -qi '^NAME=.*EulerOS' /etc/os-release 2>/dev/null || {
  echo "unsupported distribution: expected EulerOS" >&2
  exit 1
}

# EulerOS 2.10 images identify the release in VERSION_ID; the approved rootfs
# baseline must additionally expose 2.10.7 in os-release or its vendor release
# metadata. This intentionally rejects newer/older EulerOS images.
release_metadata=$(cat /etc/os-release /etc/euleros-release /etc/euleros-base-release 2>/dev/null || true)
grep -Eq '(^|[^0-9])2\.10([^0-9]|$)' <<<"$release_metadata" || {
  echo "unsupported EulerOS release: expected 2.10" >&2
  exit 1
}
grep -Eq '(^|[^0-9])2\.10\.7([^0-9]|$)' <<<"$release_metadata" || {
  echo "unsupported EulerOS baseline: expected exactly $EXPECTED_VERSION in vendor release metadata" >&2
  exit 1
}

ensure_systemd_wsl_config() {
  local current=/etc/wsl.conf temporary
  temporary=$(mktemp /etc/wsl.conf.repo-sandbox.XXXXXX)
  if [[ -f $current ]]; then
    awk '
      BEGIN { in_boot=0; saw_boot=0; wrote=0 }
      /^\[[^]]+\][[:space:]]*$/ {
        if (in_boot && !wrote) { print "systemd=true"; wrote=1 }
        in_boot=($0 ~ /^\[boot\][[:space:]]*$/)
        if (in_boot) saw_boot=1
        print; next
      }
      in_boot && /^[[:space:]]*systemd[[:space:]]*=/ {
        if (!wrote) print "systemd=true"
        wrote=1; next
      }
      { print }
      END {
        if (in_boot && !wrote) print "systemd=true"
        if (!saw_boot) { print ""; print "[boot]"; print "systemd=true" }
      }
    ' "$current" >"$temporary"
  else
    printf '[boot]\nsystemd=true\n' >"$temporary"
  fi
  chmod 0644 "$temporary"
  if [[ ! -f $current ]] || ! cmp -s "$temporary" "$current"; then
    install -m 0644 "$temporary" "$current"
    : >/run/repo-sandbox-wsl-restart-required
  else
    rm -f /run/repo-sandbox-wsl-restart-required
  fi
  rm -f "$temporary"
}

ensure_systemd_wsl_config
if $configure_systemd_only; then
  [[ $(ps -p 1 -o comm=) == systemd ]] || : >/run/repo-sandbox-wsl-restart-required
  exit 0
fi
[[ $(ps -p 1 -o comm=) == systemd ]] || {
  echo "systemd is configured but is not PID 1; terminate and restart this WSL distribution" >&2
  exit 1
}

package_manager=$(command -v dnf || command -v yum || true)
[[ -n $package_manager ]] || { echo "EulerOS dnf/yum is required" >&2; exit 1; }

install_packages() {
  local missing=() package
  for package in "$@"; do
    rpm -q "$package" >/dev/null 2>&1 || missing+=("$package")
  done
  ((${#missing[@]} == 0)) || "$package_manager" install -y -- "${missing[@]}"
}

install_packages ca-certificates curl git tar gzip

if ! command -v docker >/dev/null 2>&1; then
  "$package_manager" install -y -- docker-engine.x86_64
fi
systemctl is-enabled --quiet docker.service || systemctl enable docker.service >/dev/null
systemctl is-active --quiet docker.service || systemctl start docker.service

install_verified_binary() {
  local url=$1 expected=$2 destination=$3 mode=${4:-0755} temporary actual
  [[ $url == https://* && $expected =~ ^[a-f0-9]{64}$ ]] || {
    echo "refusing unverified or non-HTTPS download: $url" >&2; return 1
  }
  temporary=$(mktemp "${destination}.repo-sandbox.XXXXXX")
  if ! curl --proto '=https' --tlsv1.2 --fail --location --silent --show-error "$url" --output "$temporary"; then
    rm -f "$temporary"; return 1
  fi
  actual=$(sha256sum "$temporary" | awk '{print $1}')
  if [[ $actual != "$expected" ]]; then
    rm -f "$temporary"
    echo "SHA-256 mismatch for $url" >&2
    return 1
  fi
  install -m "$mode" "$temporary" "$destination"
  rm -f "$temporary"
}

if ! command -v bazelisk >/dev/null 2>&1; then
  install_verified_binary "$BAZELISK_URL" "$BAZELISK_SHA256" /usr/local/bin/bazelisk
fi
if [[ ! -e /usr/local/bin/bazel ]]; then
  ln -s bazelisk /usr/local/bin/bazel
fi

if ! docker buildx version >/dev/null 2>&1; then
  install -d -m 0755 /usr/local/lib/docker/cli-plugins
  install_verified_binary "$BUILDX_URL" "$BUILDX_SHA256" /usr/local/lib/docker/cli-plugins/docker-buildx
fi
docker buildx version >/dev/null

binfmt_arm64_enabled() {
  [[ -f /proc/sys/fs/binfmt_misc/qemu-aarch64 ]] &&
    grep -q '^enabled' /proc/sys/fs/binfmt_misc/qemu-aarch64
}

if ! binfmt_arm64_enabled; then
  if ! rpm -q qemu-user-static >/dev/null 2>&1; then
    "$package_manager" install -y -- qemu-user-static
  fi
  systemctl restart systemd-binfmt.service
fi
binfmt_arm64_enabled || {
  echo "qemu-aarch64 binfmt is not enabled after installing qemu-user-static" >&2
  exit 1
}

# Build into Bazel's private output tree, then atomically publish only this CLI.
(cd "$source_dir" && bazelisk build //:repo-sandbox)
cli_source=$(cd "$source_dir" && bazelisk cquery --output=files //:repo-sandbox | head -n1)
[[ -x "$source_dir/$cli_source" ]] || { echo "Bazel did not produce repo-sandbox" >&2; exit 1; }
if [[ ! -x /usr/local/bin/repo-sandbox ]] || ! cmp -s "$source_dir/$cli_source" /usr/local/bin/repo-sandbox; then
  install -m 0755 "$source_dir/$cli_source" /usr/local/bin/repo-sandbox
fi

repo-sandbox --version
repo-sandbox doctor
