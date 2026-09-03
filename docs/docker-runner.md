# Docker one-shot runner

`repo_sandbox_adapters::docker_runner::DockerRunner` converts a core `RunSpec`
into a bounded local Docker job. `plan(&RunSpec)` returns the structured Docker
argv without executing it, so callers can inspect the security and resource
policy before starting a container.

The default plan uses Docker's `bridge` network, runs without `--privileged`,
does not mount the Docker socket, enables `no-new-privileges`, drops all Linux
capabilities, and allocates explicit CPU, memory, memory-swap, writable-layer,
and `/tmp` tmpfs limits. It starts no TTY or interactive session. A small non-interactive keeper
process exists only for the lifetime of the job so ordered build and test steps
share `/workspace`; it is removed after the last step or any terminal failure.

Every task has a validated unique `io.repo-sandbox.task-id` label. The runner
rejects a pre-existing matching label before creation. Ownership begins only
when `docker container create` succeeds and returns an ID; cleanup then uses
that exact ID. The runner never removes by a caller-supplied name, never prunes
Docker state, and never removes an image, volume, network, or shared container.

Steps run in declared build-then-test order. The system executor tees each raw
stdout/stderr chunk to the console as it arrives and appends the same bytes to
the corresponding step's `stdout_bytes`/`stderr_bytes`, so invalid UTF-8 and
code points split across read boundaries remain losslessly recoverable. The
`stdout`/`stderr` strings are explicitly only human-readable lossy views.
Reports retain each executed
step's phase, name, start/end wall-clock time, elapsed duration, exit code,
stdout, stderr, and outcome. `fail_fast` stops after the first command failure;
when disabled, later build and test commands continue and the first command
failure remains the job status. Total timeout, resource exhaustion, and
infrastructure errors always stop execution. Memory OOM and temporary-storage
exhaustion are distinct from ordinary command failures.

`RunReport` also records the source snapshot identity and origin, a secret-free
configuration summary, the image reference and content digest, total timing,
cleanup disposition, and cleanup errors. `write_report_json` writes both success
and failure reports through a unique `create_new` same-directory temporary file,
flush plus `sync_all`, and an atomic create-if-absent hard-link publication.
Existing reports are never overwritten; concurrent writers produce one winner,
and failed writers clean their private temporary file.

By default the exact container ID returned by this task's successful create is
removed on success and failure. `keep_on_failure` retains that container only on
a non-success outcome and marks the report `retained_on_failure`. Snapshot
materializations use delete-on-drop by default; after a failed run a caller may
invoke `MaterializedSnapshot::retain_on_failure` when the CLI flag is present.
Neither path issues Docker image removal, builder removal, or any prune command,
so shared images and global BuildKit cache are outside the cleanup boundary.

Ordinary allowlisted environment variables are inherited by name. Secrets are
private temporary files mounted read-only and loaded only inside `docker exec`,
so their values do not enter argv or persistent `Config.Env`. Secret-bearing
step output is buffered and value-redacted before console and report emission.

Artifact export is declaration based: every request must exactly equal a
configured relative directory. Portable validation rejects both slash styles,
absolute/drive/UNC forms, `..`, links, and Windows reparse points. Every source
entry is rechecked against the canonical workspace boundary and regular files
are copied from an already-open handle. Existing export destinations are not
overwritten.

`RunReport` and all nested result/status types implement `serde::Serialize` and
remain available after the container exits. Clock
and Docker execution traits are injectable, making timing, timeout, failure,
and cleanup behavior deterministic in unit tests. An ignored Docker smoke test
is available as `docker_runner::tests::docker_one_shot_job_smoke` for hosts with
a Linux Docker daemon and `busybox:1.36`.
