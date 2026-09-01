#!/usr/bin/env bash
set -euo pipefail
root=$(cd "$(dirname "${BASH_SOURCE[0]}")/../../.." && pwd)
installer=$root/scripts/vm/install-yum.sh
ssh_driver=$root/scripts/vm/ssh-euleros.sh
smoke=$root/scripts/vm/smoke-euleros.sh

grep -q "command -v dnf || command -v yum" "$installer"
grep -q "x86_64).*artifact_arch=amd64" "$installer"
grep -q "aarch64).*artifact_arch=arm64" "$installer"
grep -q "curl --proto '=https' --tlsv1.2" "$installer"
grep -q 'sha256sum' "$installer"
grep -q 'systemctl is-active.*systemctl start' "$installer"
grep -q 'unix://\*|ssh://\*' "$installer"
grep -q 'unsafe_docker_listener_configured' "$installer"
grep -q "docker context inspect --format" "$installer"
! grep -Eq 'docker (system )?prune|buildx (create|rm|use)' "$installer"
! grep -Eq '(install|tee|printf|cat).*>?[^#]*daemon\.json' "$installer"
grep -q 'StrictHostKeyChecking=yes' "$ssh_driver"
grep -q 'BatchMode=yes' "$ssh_driver"
grep -q 'ssh "${ssh_args\[@\]}"' "$ssh_driver"
! grep -Eq 'ssh .*(\$host|\$user|\$identity)' "$ssh_driver"
grep -q 'expected == amd64.*expected == arm64' "$smoke"
grep -q 'docker_one_shot_job_smoke' "$smoke"

for script in "$installer" "$ssh_driver" "$smoke"; do bash -n "$script"; done

fake=$(mktemp -d)
trap 'rm -rf -- "$fake"' EXIT
cat >"$fake/ssh" <<'EOF'
#!/usr/bin/env bash
printf '%s\n' "$@" >"${SSH_CAPTURE:?}"
case ${SSH_MODE:-auth} in
  auth) echo 'Permission denied (publickey).' >&2; exit 255 ;;
  connection) echo 'Connection timed out' >&2; exit 255 ;;
  remote)
    [[ ${*: -1} == true ]] && exit 0
    cat >/dev/null
    echo 'fixture remote failure' >&2
    exit 42
    ;;
  remote_preflight) echo 'forced command rejected' >&2; exit 42 ;;
esac
EOF
chmod +x "$fake/ssh"
capture=$fake/argv
set +e
PATH="$fake:$PATH" SSH_CAPTURE=$capture "$ssh_driver" --host '2001:db8::10' --user vm_user \
  --port 2222 --source "$root" --known-hosts "$ssh_driver" >/dev/null 2>&1
status=$?
set -e
[[ $status == 71 ]]
grep -Fx -- '2001:db8::10' "$capture"
grep -Fx -- '2222' "$capture"
grep -Fx -- 'StrictHostKeyChecking=yes' "$capture"
set +e
PATH="$fake:$PATH" SSH_CAPTURE=$capture SSH_MODE=connection "$ssh_driver" --host example.test \
  --user vm_user --source "$root" >/dev/null 2>&1
connection_status=$?
PATH="$fake:$PATH" SSH_CAPTURE=$capture SSH_MODE=remote "$ssh_driver" --host example.test \
  --user vm_user --source "$root" >/dev/null 2>&1
remote_status=$?
PATH="$fake:$PATH" SSH_CAPTURE=$capture SSH_MODE=remote_preflight "$ssh_driver" --host example.test \
  --user vm_user --source "$root" >/dev/null 2>&1
remote_preflight_status=$?
set -e
[[ $connection_status == 70 ]]
[[ $remote_status == 72 ]]
[[ $remote_preflight_status == 72 ]]
if PATH="$fake:$PATH" "$ssh_driver" --host 'host;touch injected' --user vm_user --source "$root" >/dev/null 2>&1; then
  echo 'injection-shaped host was accepted' >&2; exit 1
fi

echo 'EulerOS/HCE VM executable contracts passed'
