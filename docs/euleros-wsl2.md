# EulerOS 2.10.7 x86_64 on WSL2

This deployment path supports only the vendor EulerOS 2.10.7 x86_64 rootfs on
WSL2. It does not provide or promise a native Windows container backend, and it
does not support openEuler, other EulerOS baselines, ARM64 Windows hosts, or
WSL1.

## Trusted rootfs and import

Obtain the EulerOS 2.10.7 x86_64 rootfs and its SHA-256 digest from the image
publisher through your organization's approved distribution channel. This
repository deliberately does not guess a mutable download URL or bless an
unverified mirror. The digest is mandatory for both local and HTTPS inputs.

From an elevated PowerShell terminal at the repository root:

```powershell
.\scripts\wsl\bootstrap-euleros.ps1 `
  -RootfsPath C:\images\EulerOS-2.10.7-x86_64-rootfs.tar `
  -RootfsSha256 '<64 lowercase or uppercase hex characters>'
```

For a publisher-provided HTTPS URL, replace `-RootfsPath` with `-RootfsUri`.
The script verifies the content before import. It imports with `--version 2`,
enables systemd without replacing other `/etc/wsl.conf` sections, restarts only
the named distribution when required, and invokes the Linux installer as root.
The distro name is restricted to characters accepted as a single native
argument; paths and Linux installer arguments are never concatenated into a
shell command.

An existing distro is never imported over or changed by default. To target an
already-created instance intentionally, add `-UseExisting`; the Linux preflight
still rejects anything other than the exact EulerOS 2.10.7 x86_64 WSL baseline.
The installer never unregisters a distro, edits Docker `daemon.json`, prunes
Docker state, changes an existing buildx builder, or grants a non-root user
Docker access.

## What is installed

Each operation checks existing state first and is safe to repeat:

- required EulerOS packages and `docker-engine.x86_64` from the distro's
  already-configured yum/dnf repositories;
- Docker's existing systemd unit, enabled and started only when needed;
- pinned Bazelisk and buildx binaries, downloaded over TLS and checked against
  hard-coded upstream SHA-256 digests only when missing;
- `qemu-user-static` and the systemd binfmt registration only when the
  `qemu-aarch64` handler is absent;
- `repo-sandbox`, built from the supplied checkout with Bazel and installed to
  `/usr/local/bin`.

The installer stops on missing vendor repositories or capabilities instead of
adding third-party package repositories or silently substituting another
distribution package.

## Verification in the target environment

Run the installer a second time, then run the acceptance script:

```powershell
.\scripts\wsl\bootstrap-euleros.ps1 `
  -RootfsPath C:\images\EulerOS-2.10.7-x86_64-rootfs.tar `
  -RootfsSha256 '<same digest>' -UseExisting
wsl.exe -d EulerOS-2.10.7 --user root --exec bash `
  /mnt/d/path/to/repo-sandbox/scripts/wsl/smoke-euleros.sh `
  /mnt/d/path/to/repo-sandbox
```

The smoke script requires real Docker/BuildKit access. It runs `doctor`, the
full Bazel build/test suite, a native `linux/amd64` image build/run, and a
`linux/arm64` build/run whose `uname` assertion demonstrates QEMU execution.
It exits nonzero on any missing capability; do not record a pass from static
contract tests alone.

On any Windows development host, the non-mutating static checks are available:

```powershell
pwsh -NoProfile -File scripts/wsl/tests/contracts.ps1
```
