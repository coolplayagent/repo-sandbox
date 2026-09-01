#!/usr/bin/env bash
set -euo pipefail

action=${1:-}
runner_temp=${2:-}
run_id=${3:-}
run_attempt=${4:-}

if [[ $EUID -ne 0 ]]; then
  echo "ci-quota-docker must run as root" >&2
  exit 64
fi
if [[ -z $runner_temp || -z $run_id || -z $run_attempt ]]; then
  echo "usage: ci-quota-docker.sh start|stop RUNNER_TEMP RUN_ID RUN_ATTEMPT" >&2
  exit 64
fi
if [[ ! $run_id =~ ^[0-9]+$ || ! $run_attempt =~ ^[0-9]+$ ]]; then
  echo "run ID and attempt must be numeric" >&2
  exit 64
fi

runner_temp=$(realpath "$runner_temp")
task_root="$runner_temp/repo-sandbox-quota-docker-$run_id-$run_attempt"
mountpoint="$task_root/xfs"
state="$task_root/state"
socket="$task_root/docker.sock"
bridge="rsq$(printf '%x' "$((run_id % 1048575))")"
bridge=${bridge:0:15}
subnet_octet=$((run_id % 180 + 40))

validate_task_root() {
  case "$task_root" in
    "$runner_temp"/repo-sandbox-quota-docker-*) ;;
    *) echo "refuse unsafe quota Docker root: $task_root" >&2; exit 65 ;;
  esac
  [[ $task_root != "$runner_temp" ]]
}

owned_daemon_pid() {
  local pid
  pid=$(<"$state/dockerd.pid")
  [[ $pid =~ ^[0-9]+$ ]]
  [[ -e /proc/$pid/exe ]]
  [[ $(basename "$(readlink -f "/proc/$pid/exe")") == dockerd ]]
  tr '\0' '\n' <"/proc/$pid/cmdline" | grep -Fx -- "--data-root=$mountpoint/docker" >/dev/null
  tr '\0' '\n' <"/proc/$pid/cmdline" | grep -Fx -- "--host=unix://$socket" >/dev/null
  printf '%s\n' "$pid"
}

start() {
  validate_task_root
  if [[ -e $task_root ]]; then
    echo "refuse to reuse quota Docker root: $task_root" >&2
    exit 66
  fi
  mkdir -p "$state" "$mountpoint" "$task_root/exec"
  chmod 0711 "$task_root"
  truncate -s 32G "$task_root/xfs.img"
  mkfs.xfs -q -f -n ftype=1 "$task_root/xfs.img"
  local loop_device
  loop_device=$(losetup --find --show "$task_root/xfs.img")
  echo "$loop_device" >"$state/loop-device"
  mount -o pquota "$loop_device" "$mountpoint"
  findmnt -rn -M "$mountpoint" -o OPTIONS | grep -Eq '(^|,)(pquota|prjquota)(,|$)'
  mkdir -p "$mountpoint/docker"
  ip link add "$bridge" type bridge
  ip address add "172.31.$subnet_octet.1/24" dev "$bridge"
  ip link set "$bridge" up

  local runner_uid=${SUDO_UID:-1001}
  local runner_gid=${SUDO_GID:-121}
  RUNNER_TRACKING_ID= nohup dockerd \
    "--host=unix://$socket" \
    "--data-root=$mountpoint/docker" \
    "--exec-root=$task_root/exec" \
    "--pidfile=$state/dockerd.pid" \
    --storage-driver=overlay2 \
    "--bridge=$bridge" \
    --iptables=false \
    --ip-forward=false \
    --ip-masq=false \
    >"$state/dockerd.log" 2>&1 &
  echo "$!" >"$state/launcher.pid"

  local ready=false
  for _ in $(seq 1 60); do
    if [[ -S $socket ]] && DOCKER_HOST="unix://$socket" docker info >/dev/null 2>&1; then
      ready=true
      break
    fi
    sleep 1
  done
  if [[ $ready != true ]]; then
    tail -n 200 "$state/dockerd.log" >&2 || true
    exit 67
  fi
  owned_daemon_pid >/dev/null
  chown "$runner_uid:$runner_gid" "$socket"
  chmod 0660 "$socket"

  DOCKER_HOST="unix://$socket" docker pull busybox:1.36 >/dev/null
  local probe="repo-sandbox-quota-probe-$run_id-$run_attempt"
  DOCKER_HOST="unix://$socket" docker container create \
    --name "$probe" --storage-opt size=32M busybox:1.36 true >/dev/null
  DOCKER_HOST="unix://$socket" docker container start --attach "$probe" >/dev/null
  DOCKER_HOST="unix://$socket" docker container rm "$probe" >/dev/null
  echo "quota_probe=passed storage_opt=size=32M" >&2
  printf 'unix://%s\n' "$socket"
}

stop() {
  validate_task_root
  [[ -d $task_root ]] || return 0
  local docker_host="unix://$socket"
  local builder="repo-sandbox-ci-$run_id-$run_attempt"
  if [[ -S $socket ]]; then
    DOCKER_HOST="$docker_host" docker buildx inspect "$builder" >/dev/null 2>&1 \
      && DOCKER_HOST="$docker_host" docker buildx rm "$builder" >/dev/null
  fi
  if [[ -f $state/dockerd.pid ]]; then
    local pid
    pid=$(<"$state/dockerd.pid")
    if kill -0 "$pid" 2>/dev/null; then
      pid=$(owned_daemon_pid)
      kill -TERM "$pid"
      for _ in $(seq 1 30); do
        kill -0 "$pid" 2>/dev/null || break
        sleep 1
      done
      if kill -0 "$pid" 2>/dev/null; then
        kill -KILL "$pid"
      fi
      for _ in $(seq 1 10); do
        kill -0 "$pid" 2>/dev/null || break
        sleep 1
      done
      ! kill -0 "$pid" 2>/dev/null
    fi
  fi
  local netns_mount="$task_root/exec/netns/default"
  mountpoint -q "$netns_mount" && umount "$netns_mount"
  ! mountpoint -q "$netns_mount"
  mountpoint -q "$mountpoint" && umount "$mountpoint"
  if [[ -f $state/loop-device ]]; then
    local loop_device backing_file
    loop_device=$(<"$state/loop-device")
    if losetup "$loop_device" >/dev/null 2>&1; then
      backing_file=$(losetup -n -O BACK-FILE "$loop_device")
      [[ $(realpath "$backing_file") == "$task_root/xfs.img" ]]
      losetup --detach "$loop_device"
    fi
    ! losetup "$loop_device" >/dev/null 2>&1
  fi
  ip link show "$bridge" >/dev/null 2>&1 && ip link delete "$bridge"
  rm -f -- "$socket"
  [[ ! -S $socket ]]
  ! mountpoint -q "$mountpoint"
  ! ip link show "$bridge" >/dev/null 2>&1
  rm -rf -- "$task_root"
  [[ ! -e $task_root ]]
}

case "$action" in
  start) start ;;
  stop) stop ;;
  *) echo "unknown action: $action" >&2; exit 64 ;;
esac
