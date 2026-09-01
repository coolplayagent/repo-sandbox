# repo-sandbox

`repo-sandbox` is a Rust CLI built and tested exclusively through Bazel with
Bzlmod. The CLI separates domain planning from infrastructure adapters and
includes a central, versioned environment template catalog.

## Build and test

Install [Bazelisk](https://github.com/bazelbuild/bazelisk), then use the checked-in
Bazel version and module graph:

```console
bazelisk build //...
bazelisk test //...
bazelisk run //:repo-sandbox -- --help
bazelisk run //:repo-sandbox -- --version
```

Run the read-only prerequisite inspection in a terminal or CI job:

```console
repo-sandbox doctor
repo-sandbox doctor --json
repo-sandbox plan --repository .
```

Private Git snapshots support SSH agent/key references and HTTPS token/credential
helper authentication without storing credentials. See
[`docs/config-v1.md`](docs/config-v1.md#private-git-authentication) for the
security contract.

`doctor` checks the host OS/CPU, Docker daemon, BuildKit, buildx,
cross-architecture QEMU/binfmt support, repository filesystem space, and Docker
Hub registry connectivity. It never installs software or changes host/Docker
configuration. A failed capability includes suggested operator actions and exits
with the environment exit code (`3`). `plan` resolves the selected central
template and displays its stable dependency graph; `build`, `verify`, and
`clean` remain reserved for follow-up issues.

Environment builds use canonical `linux/amd64` and `linux/arm64` platform names.
Cross-architecture and multi-platform builds require a builder that advertises
the non-native platform through a native node or QEMU/binfmt; missing capability
fails before Dockerfile execution. Multi-platform outputs use an explicit
registry push or OCI layout because Docker `--load` cannot represent a manifest
list. See [`docs/templates.md`](docs/templates.md).

OCI registry distribution uses Docker credential helpers or password stdin,
publishes content-addressed immutable tags plus optional aliases, and verifies
complete multi-platform manifests on push and pull. See
[`docs/registry.md`](docs/registry.md) for the security contract and configurable
integration test.

The versioned repository configuration and finite CLI override contract are
documented in [`docs/config-v1.md`](docs/config-v1.md). A complete configuration
is available as [`.repo-sandbox.yaml.example`](.repo-sandbox.yaml.example).
The central manifest and component graph are documented in
[`docs/templates.md`](docs/templates.md); business repositories select only a
template ID and parameters and do not carry a catalog Dockerfile.

Source inputs are materialized into private temporary directories. Local Git
worktrees include tracked files plus untracked files accepted by Git's ignore
rules; remote refs are resolved to a full commit object ID before checkout. The
snapshot identity hashes normalized paths, Git-compatible file modes, and file
contents. Git metadata is never copied, recursive submodules are opt-in, and Git
LFS sources are rejected in v1.

Task images combine a resolved environment image with exactly one immutable
source snapshot at `/workspace`. Their tag is derived from environment, source,
template, configuration, and creation-time metadata; OCI labels retain those
inputs. See [`docs/task-images.md`](docs/task-images.md) for the
security and identity contract.

`Cargo.toml` and `Cargo.lock` describe Rust packages and third-party dependency
resolution for Bzlmod's crate_universe. Cargo is not a supported build entrypoint.

The supported EulerOS 2.10.7 x86_64 WSL2 bootstrap and its explicit platform
boundary are documented in [`docs/euleros-wsl2.md`](docs/euleros-wsl2.md).

Capability-driven deployment to native amd64/arm64 EulerOS/HCE-compatible yum
VMs, including strict SSH triggering and the real two-VM acceptance contract,
is documented in [`docs/euleros-hce-vm.md`](docs/euleros-hce-vm.md).
