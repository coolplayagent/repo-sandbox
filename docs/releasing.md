# CI and CLI releases

Every pull request runs two stable required checks. `Bazel quality and runner
smoke` builds and tests the complete Bazel graph, plans the checked-in
`.repo-sandbox.yaml` sample with the Bazel-built executable, and executes its
`--version` path on the hosted runner. `Required Docker E2E` runs the real
descriptor-driven Docker scenarios introduced by issue 16. Repository branch
protection should require both job names before merging.

The PR workflow uses the `pull_request` event with `contents: read`, does not
reference Actions secrets, and checks out without persisted credentials. The
same behavior therefore applies to same-repository and fork pull requests;
neither can publish a release. Bazel cache keys begin with `bazel-ci-v1`.
BuildKit uses a task-owned daemon and builder whose name begins with
`buildkit-e2e-v1`; the same validated helper derives that exact name for both
creation and ownership-scoped cleanup, and it never restores from or writes to
the Bazel namespace.
Release Bazel caches use a third prefix, `bazel-release-v1`, so untrusted PR
cache entries cannot become release inputs.

## Publishing

1. Update the workspace version in `Cargo.toml`, `MODULE.bazel`, and both CLI
   targets in `crates/cli/BUILD.bazel`.
2. Merge the version change after both required checks pass.
3. Create a tag in the exact form `vMAJOR.MINOR.PATCH`, pointing at that commit,
   and push it. Configure the `release` GitHub Environment and tag ruleset so
   only release maintainers can approve or create matching tags.

The tag workflow validates that the tagged commit is already on `origin/main`,
the tag is canonical, and all version declarations match before it builds
anything. Native GitHub runners build `linux-amd64` and
`linux-arm64` with Bazel inside a digest-pinned Rocky Linux 8.10 container,
run the Bazel tests, enforce a maximum GLIBC 2.28 symbol requirement, execute
each binary, create a
deterministic archive, and attach both per-asset checksums and `SHA256SUMS` to a
GitHub Release with generated notes. Only the `Publish protected tag release`
job receives `contents: write`; build and fresh-machine jobs remain read-only.
All third-party Actions are pinned to full commits and checkout never persists
the workflow token.

The generic Linux archives support GLIBC 2.28 or newer. This baseline is both
built and tested on Rocky Linux 8 rather than inferred from a newer hosted
runner. After publication, native amd64 and arm64 jobs start from the same clean,
digest-pinned Rocky Linux 8.10 container. They use no release token: each downloads the public archive and
checksum, validates the checksum filename before invoking `sha256sum`, rejects
unexpected archive members, independently rechecks the GLIBC symbol ceiling,
and executes `repo-sandbox --version`. The workflow
is not successful until both public-download checks pass.

## Consumer verification

Choose one platform, then download its archive plus `SHA256SUMS` from the
Release page. Verify before extraction:

```console
sha256sum --check --ignore-missing SHA256SUMS
tar -xzf repo-sandbox-0.1.0-linux-amd64.tar.gz
./repo-sandbox --version
```

Release filenames are derived only after the tag and platform have matched
finite allowlists. Build artifacts are immutable, run-scoped Actions artifacts
and are accepted by the publish job only when the exact two-platform file set
and both checksums validate. The final public checks provide an independent
download boundary; GitHub Releases remains the only artifact service.
Release reruns compare every already-published asset byte-for-byte and never
overwrite it. An interrupted, unpublished draft is the only mutable case: after
revalidating its numeric ID, exact tag, draft state, and remote tag commit, the
workflow replaces that partial draft without deleting or moving the Git tag.
