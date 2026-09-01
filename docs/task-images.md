# Task images

`repo_sandbox_adapters::task_image::TaskImageBuilder` combines a previously
built environment image with a `SourceSnapshot`. It pins the parent with its
BuildKit digest and copies only the materialized snapshot to the fixed
`/workspace` directory. Task images intentionally declare neither an
`ENTRYPOINT` nor a `CMD`; orchestration supplies build and test commands.

The generated tag is `sha256-<identity>`. The identity is a versioned,
length-delimited SHA-256 hash of the environment digest, snapshot digest and
resolved commit (when remote), template ID/version, and normalized
configuration digest. The build timestamp is metadata, not identity, so the
same immutable inputs always select the same tag. Source, configuration, or
resolved environment changes select a new tag. The builder only loads the
image locally; registry push and retention policy remain later orchestration
concerns.

The OCI labels record creation time, source commit and content digest, template
ID/version, configuration digest, environment digest, and task identity.

Each build uses an automatically removed temporary directory containing only:

- a generated Dockerfile;
- a deny-by-default `.dockerignore`; and
- the snapshot under `source/`.

The adapter walks the snapshot without following symlinks, rejects special
files and file-count mismatches, and fails before Docker sees common Git or
credential paths (`.git`, `.env*`, `.netrc`, SSH/AWS/Docker credential
directories, and related files). Git authentication remains solely in the
snapshot adapter. Unit tests inject prohibited names and inspect the context;
an ignored Docker integration test is available for hosts with Linux Docker,
Buildx, and BusyBox.
