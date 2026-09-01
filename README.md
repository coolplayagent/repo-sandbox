# repo-sandbox

`repo-sandbox` is a Rust CLI built and tested exclusively through Bazel with
Bzlmod. The initial skeleton separates the CLI, domain core, and infrastructure
adapters so future integrations do not leak into the domain layer.

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
```

Private Git snapshots support SSH agent/key references and HTTPS token/credential
helper authentication without storing credentials. See
[`docs/config-v1.md`](docs/config-v1.md#private-git-authentication) for the
security contract.

`doctor` checks the host OS/CPU, Docker daemon, BuildKit, buildx,
cross-architecture QEMU/binfmt support, repository filesystem space, and Docker
Hub registry connectivity. It never installs software or changes host/Docker
configuration. A failed capability includes suggested operator actions and exits
with the environment exit code (`3`). The `plan`, `build`, `verify`, and `clean`
routes remain reserved for follow-up issues.

The versioned repository configuration and finite CLI override contract are
documented in [`docs/config-v1.md`](docs/config-v1.md). A complete configuration
is available as [`.repo-sandbox.yaml.example`](.repo-sandbox.yaml.example).

Source inputs are materialized into private temporary directories. Local Git
worktrees include tracked files plus untracked files accepted by Git's ignore
rules; remote refs are resolved to a full commit object ID before checkout. The
snapshot identity hashes normalized paths, Git-compatible file modes, and file
contents. Git metadata is never copied, recursive submodules are opt-in, and Git
LFS sources are rejected in v1.

`Cargo.toml` and `Cargo.lock` describe Rust packages and third-party dependency
resolution for Bzlmod's crate_universe. Cargo is not a supported build entrypoint.
