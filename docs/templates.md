# Central environment templates

All reusable environment definitions live under `templates/`. A template
manifest explicitly declares its `id`, semantic `version`, `base_image`,
component selection, supported `target_platforms`, `build_context`, and
parameters. Each selected component carries an explicit `depends_on` list.
Component manifests independently declare ID, version, platforms, and a central
build context.

`TemplateCatalog` validates both kinds of manifests before planning. Duplicate
IDs, empty or unsafe contexts, invalid manifests, missing components, missing
dependency selections, platform mismatches, and dependency cycles are reported
with YAML-style paths. Topological sorting uses component IDs to break ties, so
the same inputs always produce the same `TemplatePlan` stage order.

The bootstrap catalog contains `rust-bazel@1.0.1` and the `base-tools`, `bazel`,
and `rust` components. Its manifests and Dockerfiles are Bazel compile inputs,
and core tests statically parse and plan the embedded catalog. Central paths are
ordinary checked-in files; no symlinks or init-dev-compatible filename
conventions are required.

The central Rust+Bazel Dockerfile has four named stages. `toolchain-build` owns
the large upstream assembly image, the downloader, optional BuildKit secret,
and transient installation state. `environment-base` starts from Debian slim,
installs only task-runtime build dependencies, and copies the Rust/Cargo,
fixed Bazel, and optional Bazelisk executables across an explicit
`COPY --from=toolchain-build` boundary. `offline-seed` resolves a fixed,
centrally owned `genrule` plus `cc_test` fixture without access to repository
source or the optional GitHub token. The final `environment` copies only the
resulting content-addressed Bazel repository closure. The actual Bazel binary is pinned by
the trusted central template to Bazel 8.3.1, so repositories cannot override
`bazel_version` or `bazelisk_version`, and the normal `bazel` command does not
need Bazelisk to resolve `latest` or download a second executable at task
runtime.

The root-owned `bazel` wrapper ignores repository, user, and system rc files,
clears Bazelisk override variables, selects the fixed binary, and disables all
repository downloads. Task containers use Docker network `none`. When a source
snapshot has no `MODULE.bazel.lock`, the task image supplies the checksum-pinned
baseline lock for Bazel 8.3.1's built-in C++, Java, shell, and platform module
mapping. The read-only image closure contains both registry metadata and source
archives, rather than relying on a BuildKit cache mount. Version 1 seeds only
the centrally defined baseline module closure; any additional dependency or
extension closure that requires a download fails closed at runtime. Apt and Cargo
paths use locked, architecture-specific BuildKit cache mounts; those mounts and
`/run/secrets` are never committed to a layer.

Inspect a repository selection without building or pulling an image:

```console
repo-sandbox plan --repository path/to/repository
```

The output identifies the resolved template, image, platform, central build
context, every dependency edge, and the mandatory versioned execution profile
in stable order. Repository YAML cannot replace profile commands or limits.

## BuildKit adapter

`repo_sandbox_adapters::buildkit::BuildKit` converts a `TemplatePlan` into a
structured `docker buildx build` invocation. Callers supply an `ImageRef` and
may select build arguments, standard HTTP/HTTPS proxy arguments, cache imports
and exports, target progress format, load or push output, and an existing or
task-owned ephemeral builder. The result is a core `BuiltImage` containing the
requested image reference and the BuildKit `sha256` digest.

Build targets always use OCI platform names. `BuildOptions::platforms` accepts
`linux/amd64`, `linux/arm64`, or both; an empty list keeps the platform resolved
by `TemplatePlan`. Every target must be supported by the template and every
selected component. Native-only builds proceed directly. Before a
cross-architecture build step, the adapter bootstraps and inspects the selected
builder and requires it to advertise the other platform. Missing QEMU/binfmt
(or a native builder node) therefore fails explicitly before
`docker buildx build`, consistent with the `qemu_binfmt` doctor conclusion.

Docker cannot load a multi-platform image into its classic image store, so the
adapter rejects multi-platform `ImageOutput::Load` before invoking Docker.
Callers explicitly choose `Push` or `OciDirectory`. Push uses the caller's
existing Docker credentials and registry choice; registry authentication and
lifecycle remain Issue #11 concerns. OCI output writes an unpacked OCI image
layout and needs no registry.

After a multi-platform push, the adapter runs `docker buildx imagetools inspect
--raw`; for OCI output it reads the generated `index.json`. It rejects an output
that is not a manifest list or omits a requested platform. `BuiltImage` contains
the index digest and a stable `platform_digests` list with each concrete image
manifest digest, and is serializable for build reports.

The adapter passes every option as a separate process argument; it never
constructs a shell command. A stable plan digest covers the template version,
resolved parameters, platform, contexts, component versions, and dependency
edges. The Dockerfile records this digest as an OCI label, so changed template
inputs invalidate the final image configuration while BuildKit remains free to
reuse unchanged installation layers.

Environment requests always pass the structured `--target environment`. The
target is not a public build option and cannot be replaced by a build argument,
so a caller cannot export `toolchain-build`. Task image construction has the
only internal path to `--target task`.

`scripts/docker/multistage-acceptance.sh` is the CI and local acceptance entry
point. For amd64 and arm64 it runs Cargo tests and a Bazel build in both the
generated task image and the checked-in pre-change single-stage baseline,
checks layer/filesystem content and source digest labels, demonstrates warm
environment and source-only task cache reuse, and prints compressed `docker
save | gzip` plus unpacked engine sizes. The job fails unless the final task
image is at least 10% smaller on each architecture. The baseline preserves the
same toolchain and commands and is not padded with synthetic data.

When an ephemeral builder is requested, the adapter first inspects the name and
rejects an existing builder as unowned. It then creates the builder without
changing the globally selected builder and, only after creation succeeds, always attempts
`docker buildx rm --force <owned-name>` after success, failure, or interruption.
Create failures never trigger name-based removal, including an `already exists`
race after inspection. Existing builders are never removed. The implementation deliberately never
runs `docker system prune` or `docker buildx prune`.

The ignored `docker_two_architecture_smoke_and_oci_manifest` integration test
loads and runs both single-platform images and verifies a two-platform OCI
index. `docker_pushed_tag_contains_and_runs_both_platforms` additionally checks
a real manifest tag and both runtime paths when
`REPO_SANDBOX_MULTIARCH_TEST_IMAGE` names a writable disposable tag. These
tests require native amd64/arm64 nodes or working QEMU/binfmt; the registry test
uses preconfigured Docker authentication and does not provision credentials.
