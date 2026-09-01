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
