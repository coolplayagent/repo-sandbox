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

Steps run in declared build-then-test order. Reports retain each executed
step's phase, name, start/end wall-clock time, elapsed duration, exit code,
stdout, stderr, and outcome. `fail_fast` stops after the first command failure;
when disabled, later build and test commands continue and the first command
failure remains the job status. Total timeout, resource exhaustion, and
infrastructure errors always stop execution. Memory OOM and temporary-storage
exhaustion are distinct from ordinary command failures.

`RunReport` and all nested result/status types implement `serde::Serialize` and
remain available after the container exits for a later reporting layer. Clock
and Docker execution traits are injectable, making timing, timeout, failure,
and cleanup behavior deterministic in unit tests. An ignored Docker smoke test
is available as `docker_runner::tests::docker_one_shot_job_smoke` for hosts with
a Linux Docker daemon and `busybox:1.36`.
