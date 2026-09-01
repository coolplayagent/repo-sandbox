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
