# OCI registry distribution

`repo-sandbox-adapters::registry` provides a vendor-neutral `OciRegistry`
boundary and a Docker implementation suitable for Harbor, GHCR, Docker Hub and
other OCI Distribution registries. Cloud-specific adapters such as SWR can
implement the same boundary without changing orchestration or report types.

The adapter does not provision, configure, retain or delete registry-side
resources. Repository names are fully qualified (`registry/repository`) and OCI
validated. Publishing always creates the immutable
`sha256-<64 lowercase hex>` content tag first. Configured aliases such as
`latest` are additional mutable pointers and never replace that content tag.

Authentication is runtime-only. `RegistryCredential::CredentialHelper` asks
Docker to use its externally configured credential store/helper and validates
access with a caller-selected probe image. `RegistryCredential::Password`
selects a helper and runs `docker --config <temporary-helper-only-config> login
... --password-stdin`; that config contains only `credsStore`, preventing
Docker's config-file auth fallback. The secret stays out of argv, Debug output,
serializable reports, and repository configuration. Errors are sanitized before classification as
authentication, network, command, interruption, digest, or manifest failures.

Publishing uses `docker buildx imagetools create`, which copies a pinned source
manifest and all of its children. The adapter reads the destination descriptor's
reported `Digest:` and uses raw manifest JSON only to check requested child digests.
Buildx v0.15.1 is the minimum supported release: its `--prefer-index` option is
required to copy a single-image manifest without silently wrapping it in an OCI
index and changing its digest. Before any registry write, the CLI verifies this
capability from `docker buildx imagetools create --help` and reports an actionable
environment error when the installed plugin is too old. The checked-in EulerOS
installers pin and checksum that minimum release.
Pulling pins the root digest, pulls every requested platform separately, checks
each local image's OS/architecture, and then rechecks the original tag to detect
a mutable-tag race. A raw single-image manifest is accepted for exactly one
requested platform, which must match the pulled local image.

The CLI seeds and publishes only after every requested build/verify step has
succeeded. Missing Registry configuration fails before Docker build side
effects, and seed, immutable-tag, manifest, or alias errors always return
non-zero rather than being ignored.

## Configurable integration test

No external credentials or registry are assumed by the default test suite. To
exercise a standard registry already configured in Docker's credential helper,
select a readable multi-platform source and a writable disposable destination:

```text
REPO_SANDBOX_REGISTRY_TEST_SOURCE=registry.example/team/source:multiarch
REPO_SANDBOX_REGISTRY_TEST_REPOSITORY=registry.example/team/disposable
cargo test -p repo-sandbox-adapters docker_registry_login_publish_pull_and_multiarch_digest_consistency -- --ignored --exact
```

The test verifies helper-backed access, copies the complete multi-platform
manifest under a content tag and mutable test alias, pulls every platform by
digest, and compares the root digest. It intentionally does not create or
destroy the registry or delete server-side content.
