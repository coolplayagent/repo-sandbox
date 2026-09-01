#!/usr/bin/env bash
set -euo pipefail

readonly EXIT_CONNECTION=70
readonly EXIT_AUTHENTICATION=71
readonly EXIT_REMOTE_TASK=72

host= user= source_dir= port=22 identity= known_hosts= acceptance_arch=
while (($#)); do
  case $1 in
    --host|--user|--source|--port|--identity|--known-hosts|--acceptance-arch)
      (($# >= 2)) || { echo "$1 requires a value" >&2; exit 64; }
      option=$1 value=$2
      case $option in
        --host) host=$value ;; --user) user=$value ;; --source) source_dir=$value ;;
        --port) port=$value ;; --identity) identity=$value ;; --known-hosts) known_hosts=$value ;;
        --acceptance-arch) acceptance_arch=$value ;;
      esac
      shift 2
      ;;
    *) echo "unknown argument: $1" >&2; exit 64 ;;
  esac
done

[[ $host =~ ^[A-Za-z0-9._:%-]+$ && $host != -* ]] || { echo 'invalid SSH host' >&2; exit 64; }
[[ $user =~ ^[A-Za-z_][A-Za-z0-9._-]*$ ]] || { echo 'invalid SSH user' >&2; exit 64; }
[[ $port =~ ^[0-9]+$ ]] && ((10#$port >= 1 && 10#$port <= 65535)) || { echo 'invalid SSH port' >&2; exit 64; }
[[ -d $source_dir && -f $source_dir/MODULE.bazel ]] || { echo 'invalid source directory' >&2; exit 64; }
[[ -z $identity || -f $identity ]] || { echo 'SSH identity is not a regular file' >&2; exit 64; }
[[ -z $known_hosts || -f $known_hosts ]] || { echo 'known_hosts is not a regular file' >&2; exit 64; }
[[ -z $known_hosts || $known_hosts =~ ^[A-Za-z0-9._/:@%+-]+$ ]] || {
  echo 'known_hosts path contains unsupported characters' >&2; exit 64
}
[[ -z $acceptance_arch || $acceptance_arch == amd64 || $acceptance_arch == arm64 ]] || {
  echo 'acceptance architecture must be amd64 or arm64' >&2; exit 64
}

ssh_args=(-o BatchMode=yes -o StrictHostKeyChecking=yes -o ConnectTimeout=10 -p "$port" -l "$user")
if [[ -n $known_hosts ]]; then ssh_args+=(-o "UserKnownHostsFile=$known_hosts"); fi
if [[ -n $identity ]]; then ssh_args+=(-o IdentitiesOnly=yes -i "$identity"); fi
ssh_args+=(-- "$host")

error_file=$(mktemp)
archive_file=$(mktemp)
cleanup() { rm -f -- "$error_file" "$archive_file"; }
trap cleanup EXIT
trap 'exit 129' HUP
trap 'exit 130' INT
trap 'exit 143' TERM

classify_transport_failure() {
  if grep -Eqi 'permission denied|authentication failed|no supported authentication methods|too many authentication failures' "$error_file"; then
    echo 'ssh authentication failed' >&2
    return "$EXIT_AUTHENTICATION"
  fi
  echo 'ssh connection failed' >&2
  return "$EXIT_CONNECTION"
}

set +e
ssh "${ssh_args[@]}" true 2>"$error_file"
preflight_status=$?
set -e
if ((preflight_status == 255)); then
  classify_transport_failure
  exit $?
fi
if ((preflight_status != 0)); then
  sed 's/^/remote: /' "$error_file" >&2
  echo "remote preflight failed (remote exit $preflight_status)" >&2
  exit "$EXIT_REMOTE_TASK"
fi

# A local archive avoids scp's remote-path parsing. The remote command is a
# constant: no host/user/path/key value is ever interpreted by the remote shell.
git -C "$source_dir" rev-parse --verify HEAD^{commit} >/dev/null
git -C "$source_dir" archive --format=tar HEAD >"$archive_file"
case $acceptance_arch in
  amd64) readonly remote_command='umask 077; d=$(mktemp -d "${TMPDIR:-/tmp}/repo-sandbox-vm.XXXXXXXX") || exit 120; as_root() { if [ "$(id -u)" -eq 0 ]; then "$@"; else sudo -n -- "$@"; fi; }; cleanup() { rm -rf -- "$d" 2>/dev/null || sudo -n -- rm -rf -- "$d"; }; trap cleanup EXIT; trap "exit 129" HUP; trap "exit 130" INT; trap "exit 143" TERM; tar -xf - -C "$d" || exit 121; as_root bash "$d/scripts/vm/install-yum.sh" --source "$d" && as_root bash "$d/scripts/vm/install-yum.sh" --source "$d" && as_root bash "$d/scripts/vm/smoke-euleros.sh" --expected-arch amd64 --source "$d"; r=$?; [ "$r" -eq 255 ] && exit 254; exit "$r"' ;;
  arm64) readonly remote_command='umask 077; d=$(mktemp -d "${TMPDIR:-/tmp}/repo-sandbox-vm.XXXXXXXX") || exit 120; as_root() { if [ "$(id -u)" -eq 0 ]; then "$@"; else sudo -n -- "$@"; fi; }; cleanup() { rm -rf -- "$d" 2>/dev/null || sudo -n -- rm -rf -- "$d"; }; trap cleanup EXIT; trap "exit 129" HUP; trap "exit 130" INT; trap "exit 143" TERM; tar -xf - -C "$d" || exit 121; as_root bash "$d/scripts/vm/install-yum.sh" --source "$d" && as_root bash "$d/scripts/vm/install-yum.sh" --source "$d" && as_root bash "$d/scripts/vm/smoke-euleros.sh" --expected-arch arm64 --source "$d"; r=$?; [ "$r" -eq 255 ] && exit 254; exit "$r"' ;;
  *) readonly remote_command='umask 077; d=$(mktemp -d "${TMPDIR:-/tmp}/repo-sandbox-vm.XXXXXXXX") || exit 120; as_root() { if [ "$(id -u)" -eq 0 ]; then "$@"; else sudo -n -- "$@"; fi; }; cleanup() { rm -rf -- "$d" 2>/dev/null || sudo -n -- rm -rf -- "$d"; }; trap cleanup EXIT; trap "exit 129" HUP; trap "exit 130" INT; trap "exit 143" TERM; tar -xf - -C "$d" || exit 121; as_root bash "$d/scripts/vm/install-yum.sh" --source "$d"; r=$?; [ "$r" -eq 255 ] && exit 254; exit "$r"' ;;
esac

set +e
ssh "${ssh_args[@]}" "$remote_command" <"$archive_file" 2>"$error_file"
status=$?
set -e
if ((status == 255)); then
  classify_transport_failure
  exit $?
fi
if ((status != 0)); then
  sed 's/^/remote: /' "$error_file" >&2
  echo "remote task failed (remote exit $status)" >&2
  exit "$EXIT_REMOTE_TASK"
fi
[[ ! -s $error_file ]] || sed 's/^/remote: /' "$error_file" >&2
