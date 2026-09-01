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

The bootstrap catalog contains `rust-bazel@1.0.0` and the `base-tools`, `bazel`,
and `rust` components. Its manifests and Dockerfiles are Bazel compile inputs,
and core tests statically parse and plan the embedded catalog. Central paths are
ordinary checked-in files; no symlinks or init-dev-compatible filename
conventions are required.

Inspect a repository selection without building or pulling an image:

```console
repo-sandbox plan --repository path/to/repository
```

The output identifies the resolved template, image, platform, central build
context, and every dependency edge in stable execution order.

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
