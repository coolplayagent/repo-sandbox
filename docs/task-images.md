# Task images

`repo_sandbox_adapters::task_image::TaskImageBuilder` combines a previously
built environment image with a `SourceSnapshot`. It pins the parent with its
BuildKit digest and copies only the materialized snapshot to the fixed
`/workspace` directory. Task images intentionally declare neither an
`ENTRYPOINT` nor a `CMD`; orchestration supplies build and test commands.

The generated tag is `sha256-<identity>`. The identity is a versioned,
length-delimited SHA-256 hash of the environment digest, snapshot digest and
resolved commit (when remote), template ID/version, and normalized
configuration digest and OCI creation timestamp. Including the timestamp in
identity prevents two different image configurations from ever sharing an
immutable tag; callers that require byte-stable rebuilds supply the same
reproducible source/build epoch. Source, configuration, resolved environment,
or creation-time changes select a new tag. The builder only loads the image
locally; registry push and retention policy remain later orchestration concerns.
When the environment is multi-platform, the pinned index digest still resolves
the child manifest selected by the task image's OCI platform.

The OCI labels record creation time, source commit and content digest, template
ID/version, configuration digest, environment digest, and task identity.

Each build uses an automatically removed temporary directory containing only:

- a generated Dockerfile;
- a deny-by-default `.dockerignore`; and
- the snapshot under `source/`.

The generated Dockerfile names the pinned parent alias `environment` and the
only exported final stage `task`. The adapter always supplies `--target task`;
there is no caller-supplied target string. `COPY --link source/ /workspace/` is
the sole source-dependent layer, so changing only repository content reuses the
toolchain/environment image while producing a new snapshot digest, task label,
identity and COPY layer.

The adapter walks the snapshot without following symlinks, recomputes the exact
#5 normalized path/mode/content manifest while copying, rejects digest or file
count mismatches, and fails before Docker sees common Git or
credential paths (`.git`, `.env*`, `.netrc`, SSH/AWS/Docker credential
directories, and related files). Git authentication remains solely in the
snapshot adapter. Unit tests inject prohibited names and inspect the context;
an ignored Docker integration test is available for hosts with Linux Docker,
Buildx, and BusyBox.
