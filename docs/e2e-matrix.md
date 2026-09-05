# End-to-end scenario matrix

`tests/e2e/scenarios.yaml` is the single source of truth for the P1 Docker,
WSL and VM matrix. Each scenario fixes its execution tier, targets, capability
coverage, required environment, wall-clock timeout, argv (without shell
interpolation), expected exit code, required/forbidden log text, and output
artifacts. The Rust runner executes the commands; the YAML is not a collection
of regular expressions standing in for end-to-end work.

Run the host-required Docker tier with:

```console
cargo run -p repo-sandbox-adapters --bin e2e-matrix -- --target docker
```

Use `--list`, `--scenario ID`, and `--output PATH` to inspect or narrow a run.
The default output is a unique `target/e2e/<pid>-<timestamp>/` directory. Every
executed scenario gets its own directory containing the complete
`scenario.log`, assertion-bearing `report.json`, and its declared artifacts.
An existing scenario directory is never reused or overwritten.
On Linux the runner starts each command in a new session and requires
[`pidfd_open`](https://man7.org/linux/man-pages/man2/pidfd_open.2.html)/
[`pidfd_send_signal`](https://man7.org/linux/man-pages/man2/pidfd_send_signal.2.html)
support (Linux 5.3+ or equivalent backports). It
probes those APIs before spawning a scenario. Cleanup binds each process by
pidfd, verifies its session, and removes remaining live session members even
when they use another process group or the scenario leader exits first. The
leader is reaped only after cleanup, keeping its session identity reserved.
This requirement applies to the matrix harness, not the installed CLI. On
Windows the harness terminates the tree rooted at the exact scenario PID.

## Required host matrix

The Docker target is CI-required. It covers:

| Scenario | Real boundary | Assertions |
| --- | --- | --- |
| `public-git-snapshot` | task-owned localhost unauthenticated smart-HTTP `git http-backend` service and `GitSnapshotter` | network clone, fixed commit materialization, joined server thread/closed port teardown |
| `docker-adapters` | `GitSnapshotter`, `TaskImageBuilder`, `BuildKit`, `DockerRunner` | local source, task image contents, cold/warm cache, artifact export |
| `docker-failures` | task-owned Docker containers through `DockerRunner` | build exit 41, test exit 42, timeout, stage names, failed-container retention |
| `docker-architecture-mismatch` | a real arm64 BuildKit execution | architecture stage fails and is visible in the log |
| `rust-bazel-dogfood` | repo-sandbox Rust+Bazel image fixture | cold/warm cache, source-only change, amd64/arm64, multistage, Cargo and Bazel tests |

The hosted CI job does not weaken the runner's writable-layer limit when the
host daemon lacks overlay-on-XFS project quotas. It creates a task-unique sparse
XFS filesystem mounted with `pquota`, starts a separate Docker daemon with an
isolated data root, socket, exec root and bridge, and first proves that a
`busybox:1.36` container can be created and run with
`--storage-opt size=32M`. BuildKit and disposable dogfood acceptance build
stages that fetch public dependencies use the host network; final task
containers and runner scenarios use Docker network `none`. The isolated bridge
belongs only to the task-owned test daemon and its supporting fixtures. The job removes
its exact Buildx builder, verifies the
daemon PID and command line before terminating it, unmounts the task filesystem,
its exec-root network namespace and loop device, removes the task bridge and
directory, and asserts those resources are gone.

CI prepares the arm64 environment in an explicit cache-only Buildx step before
running the matrix. It gives builder bootstrap 60 seconds and emulated offline
seed preparation 45 minutes, preserving their plain progress logs under
`target/e2e/preparation/` even on failure. A preparation failure fails the job.
Preparation also exports a complete local cache into a fresh task-owned directory
under `RUNNER_TEMP`. Dogfood explicitly imports this cache for its first ARM
environment build, so BuildKit garbage collection cannot discard the only copy
of the expensive preparation. Its own cold/warm exports and every cache-vertex
assertion still run. Completed architecture images, archives and caches are removed
before the next architecture, and CI removes the preparation cache on every exit.
CI uploads compact diagnostics separately from the complete results archive.
The compact archive omits only `task-layout/blobs/`; logs, reports, OCI indexes
and registry manifests remain available without downloading image layers.
The original full archive and all runtime blob-digest assertions are retained.
The native cold CLI build remains in the matrix; all scenario deadlines and
publication, offline, cache, and size assertions remain required. This separates
emulated toolchain setup from the multi-platform CLI contract checks. The cold/warm
CLI scenario uses a separate builder, so clearing its internal cache cannot
remove the shared ARM preparation. Single-stage acceptance inherits the exact
loaded comparison image and runs only its Cargo/Bazel checks, avoiding another
complete seed on the Docker Engine builder. The size gate still compares the
original task and baseline images.

Failure cleanup is ownership-scoped. Images and containers use unique run IDs.
The runner failure test checks the retained container's
`io.repo-sandbox.task-id` label before removing that exact container ID. The
dogfood script removes only its uniquely tagged images and task-local cache
directories. The cache-boundary scenario prunes only its newly created builder,
removes that exact builder on exit, and preserves the caller's selected builder
and shared images/caches.

## Opt-in targets

WSL and VM are represented by the same scenario description and assertion
model, but are not reported as passing unless real targets are supplied:

```console
REPO_SANDBOX_WSL_DISTRO=EulerOS cargo run -p repo-sandbox-adapters --bin e2e-matrix -- --target wsl
REPO_SANDBOX_VM_TARGETS=/secure/targets.tsv cargo run -p repo-sandbox-adapters --bin e2e-matrix -- --target vm
```

The WSL driver reuses `scripts/wsl/smoke-euleros.sh`. The VM driver reuses
`scripts/vm/acceptance-matrix.sh` and therefore requires explicit successful
amd64 and arm64 rows. Missing variables produce an explicit `SKIP` for opt-in
scenarios; they never produce synthetic success records.

Private HTTPS and SSH Git are also opt-in so CI never depends on an external
cloud repository. Point the variables below at a disposable local fixture or
operator-controlled temporary service:

- HTTPS success: `REPO_SANDBOX_E2E_HTTPS_URL`, `_REF`, `_USER`, `_TOKEN`;
  invalid-auth injection additionally uses `_INVALID_TOKEN`.
- SSH: `REPO_SANDBOX_E2E_SSH_URL`, `_REF`, `_KEY`, `_KNOWN_HOSTS`.
- Registry: `REPO_SANDBOX_REGISTRY_TEST_SOURCE` and
  `REPO_SANDBOX_REGISTRY_TEST_REPOSITORY`.

HTTPS tokens are resolved only through the adapter's ephemeral askpass helper;
SSH uses an external key plus strict known-hosts file. Scenario logs assert the
HTTPS token is absent, adapter errors redact resolved secrets, and task-image
history/filesystem scans reject credential material. Before logs or reports are
written, the matrix runner also replaces declared secret environment values and
both the original and trailing-newline-trimmed SSH key file contents with
`<redacted>`; assertion messages never echo a forbidden value. The SSH boundary
is enforced by the runner and its LF/CRLF stdout/stderr tests rather than by
copying private key material into a YAML assertion. Never put secret values in
the YAML or a Git URL.

## Exit and failure contract

The matrix command exits zero only when every selected required scenario and
every enabled opt-in scenario satisfies all fixed assertions. A command
failure, matrix timeout, missing required log marker, forbidden credential
marker, or missing artifact is a failure. The JSON report retains the observed
exit code and timeout state, while component reports retain the precise build
or test phase and step name.
