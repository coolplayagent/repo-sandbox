# Issue #1 validation record

Validation was attempted on 2026-09-01 from Windows Docker Desktop 4.87.0.
The engine was Linux/amd64, Buildx was v0.36.1, BuildKit was v0.32.2, and
`docker buildx inspect --bootstrap` advertised both `linux/amd64` and
`linux/arm64`, so the local daemon and QEMU capability checks passed.

The full `scripts/docker/multistage-acceptance.sh` run could not reach an image
export in the available validation window because external downloads were
unstable and exceptionally slow. Docker Hub returned repeated HTTP 500 errors
for Rust image blobs (BuildKit retried them). After about eleven minutes the two
largest amd64 Rust layers had reached 216.01/217.91 MB and 211.63/211.63 MB.
The Debian package index separately spent about five minutes in retry/backoff
before succeeding, after which the required build dependency download remained
in progress. The owned run was interrupted and its trap removed its temporary
tags and directory. Consequently there is no honest local compressed-size
percentage, warm-cache result, or container behavior result to report from this
attempt.

CI runs the exact same checked-in script after installing QEMU and Buildx. It
does not permit a partial pass: both architectures must run Cargo tests and a
Bazel build in both images, the final filesystem/history/labels must pass the
security assertions, warm environment and source-only task builds must contain
BuildKit `CACHED` evidence, and the measured `docker save` plus deterministic
gzip size must be at least 10% below the equivalent pre-change single-stage
baseline on each architecture. The engine's unpacked task image size is printed
beside the two compressed sizes. The baseline is the previous central
single-stage Dockerfile with the same source and required Rust, Cargo, compiler,
Git, CA and Bazel functionality; it contains no synthetic padding.

Host-side Rust validation completed successfully:

- `cargo fmt --all -- --check`
- `cargo clippy --workspace --all-targets --all-features -- -D warnings`
- `cargo test --workspace` (105 passed, 0 failed, 6 existing Docker tests ignored)

The host did not have a Bazel executable, so `bazel test //...` failed at
process launch with `bazel: The term 'bazel' is not recognized`. Bazel behavior
remains an explicit, non-optional part of the container acceptance job.
