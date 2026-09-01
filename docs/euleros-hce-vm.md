# EulerOS/HCE-compatible virtual machines

The VM deployment path is capability-driven. It supports a native `x86_64`
or `aarch64` Linux VM when all of these are present: RPM, `dnf` or `yum`, a
running systemd, an available Docker engine package (or an existing Docker
installation), and `qemu-user-static` in repositories already configured by
the operator. It does not infer compatibility from a hard-coded distribution
name or create the VM.

## Security and state boundary

The installer adds no package repository and downloads only architecture-matched
Bazelisk and buildx release artifacts over HTTPS/TLS 1.2 after checking pinned
SHA-256 digests. Run it from a trusted checkout. A non-root local invocation
uses `sudo`; remote invocation requires root or passwordless `sudo`.

The installer uses Docker only through a local Unix socket or Docker's SSH
transport and rejects unsafe `DOCKER_HOST` values and active Docker contexts.
Before enabling or starting Docker, it also refuses a `tcp://` listener found in
the systemd unit, standard yum-family daemon configuration, or running process.
It never
writes `daemon.json`, creates an unauthenticated TCP listener, grants socket
access to a user, changes the selected builder, or prunes images, containers,
volumes, builders, or shared cache. Existing daemon settings are left intact;
operators must separately remove any pre-existing unsafe listener.

Package installation, systemd enable/start, downloads, symlink publication and
CLI publication are state-aware. Repeating the installer preserves an existing
daemon configuration, selected buildx builder, and build cache.

## Local VM installation

On the VM, from the repository root:

```console
scripts/vm/install-yum.sh --source "$PWD"
scripts/vm/install-yum.sh --source "$PWD"
sudo -- scripts/vm/smoke-euleros.sh --expected-arch amd64 --source "$PWD"
```

Use `arm64` on an aarch64 VM. The smoke command rejects emulation masquerading
as the requested native VM. It runs `doctor`, the Bazel build/test suite, the
existing BuildKit cold/warm integration test, and the existing one-shot Docker
runner test. Those adapters remove only their uniquely owned test resources and
do not prune shared state.

## SSH installation and acceptance

Host keys are strict by default. Without `--known-hosts`, OpenSSH's normal
known-hosts files are used; first-contact auto-acceptance is never enabled.
Host, user, port, identity, and known-hosts values are validated and supplied as
separate OpenSSH arguments. To avoid OpenSSH configuration-token ambiguity,
known-hosts paths cannot contain whitespace or shell metacharacters. No value is
interpolated into the fixed remote shell program. IPv6 literals are passed
without brackets, for example:

```console
scripts/vm/ssh-euleros.sh \
  --host 2001:db8::10 --user vm_operator --port 2222 \
  --known-hosts /secure/known_hosts --identity /secure/id_ed25519 \
  --source "$PWD" --acceptance-arch arm64
```

The source archive is streamed over SSH, not placed through a shell-parsed scp
path. Only committed files from `git archive HEAD` are sent, so unrelated
untracked workspace files and local credentials cannot enter the archive. The
remote account creates a private temporary directory, extracts and runs there,
and removes exactly that directory through an exit/signal trap.
Private-key contents are never copied or printed. Exit status `70` means SSH
connection/transport failure, `71` authentication failure, and `72` a remote
install or acceptance failure. Invalid local input exits `64`.

For the required two-native-VM closure, copy
`scripts/vm/fixtures/targets.tsv.example`, fill in one amd64 and one arm64 VM,
then run:

```console
scripts/vm/acceptance-matrix.sh /secure/euler-vm-targets.tsv
```

This repository deliberately contains no fake successful VM result. Static and
fake-SSH contract tests validate command boundaries but cannot replace the real
matrix:

```console
bash scripts/vm/tests/contracts.sh
```

## Known constraints

- Docker and `qemu-user-static` must be available from repositories the operator
  already trusts and configured; the installer does not add vendor or Docker
  repositories.
- The SSH account needs passwordless sudo unless it logs in as root.
- Registry access is required by `doctor` and integration smoke tests.
- A pre-existing daemon configured with TCP listeners is not rewritten or
  started; it must be remediated by the VM owner before using this deployment
  path.
