# syntax=docker/dockerfile:1.7@sha256:a57df69d0ea827fb7266491f2813635de6f17269be881f696fbfdf2d83dda33e
ARG BASE_IMAGE=docker.io/library/rust:1.97.0-bookworm

# The official Rust image is intentionally confined to the assembly stage. Its
# download/build utilities and image metadata are not inherited by the runtime.
FROM ${BASE_IMAGE} AS toolchain-build

ARG TARGETARCH
RUN --mount=type=cache,id=repo-sandbox-apt-${TARGETARCH},target=/var/cache/apt,sharing=locked \
    --mount=type=cache,id=repo-sandbox-apt-lists-${TARGETARCH},target=/var/lib/apt/lists,sharing=locked \
    --mount=type=cache,id=repo-sandbox-toolchain-downloads-${TARGETARCH},target=/toolchain-downloads,sharing=locked \
    --mount=type=secret,id=github_token,required=false \
    set -eu; \
    bazelisk_version=1.27.0; \
    bazel_version=8.3.1; \
    token_header=''; \
    if [ -s /run/secrets/github_token ]; then \
      token=$(cat /run/secrets/github_token); \
      case "$token" in github_pat_*|ghp_*) token_header="Authorization: Bearer $token" ;; esac; \
    fi; \
    apt-get update; \
    apt-get install --yes --no-install-recommends ca-certificates curl; \
    mkdir -p /toolchain/bin; \
    case "$TARGETARCH" in \
      amd64) bazel_arch=x86_64; bazel_sha=17247e8a84245f59d3bc633d0cfe0a840992a7760a11af1a30012d03da31604c; bazelisk_sha=e1508323f347ad1465a887bc5d2bfb91cffc232d11e8e997b623227c6b32fb76 ;; \
      arm64) bazel_arch=arm64; bazel_sha=52bb74f0880ee87b17e3a1aa41257d0dba9e9cb92df4e83f8f6c656a46df152d; bazelisk_sha=bb608519a440d45d10304eb684a73a2b6bb7699c5b0e5434361661b25f113a5d ;; \
      *) exit 64 ;; \
    esac; \
    if [ -n "$token_header" ]; then header_args="--header=$token_header"; else header_args=''; fi; \
    bazelisk_download="/toolchain-downloads/bazelisk-${bazelisk_version}-${TARGETARCH}"; \
    bazel_download="/toolchain-downloads/bazel-${bazel_version}-${bazel_arch}"; \
    if ! printf '%s  %s\n' "$bazelisk_sha" "$bazelisk_download" | sha256sum --check --strict >/dev/null 2>&1; then \
      curl --fail --location --show-error $header_args \
        "https://github.com/bazelbuild/bazelisk/releases/download/v${bazelisk_version}/bazelisk-linux-${TARGETARCH}" \
        --output "$bazelisk_download"; \
    fi; \
    if ! printf '%s  %s\n' "$bazel_sha" "$bazel_download" | sha256sum --check --strict >/dev/null 2>&1; then \
      curl --fail --location --show-error $header_args \
        "https://github.com/bazelbuild/bazel/releases/download/${bazel_version}/bazel-${bazel_version}-linux-${bazel_arch}" \
        --output "$bazel_download"; \
    fi; \
    printf '%s  %s\n' "$bazelisk_sha" "$bazelisk_download" | sha256sum --check --strict; \
    printf '%s  %s\n' "$bazel_sha" "$bazel_download" | sha256sum --check --strict; \
    install -m 0755 "$bazel_download" /toolchain/bin/bazel; \
    install -m 0755 "$bazelisk_download" /toolchain/bin/bazelisk; \
    rustc --version; \
    cargo --version

FROM debian:bookworm-slim AS environment-base

ARG TARGETARCH
ARG RUST_VERSION=1.97.0
# These are task-runtime dependencies. Downloader/build-only packages stay in
# toolchain-build; apt indexes and archives live only in BuildKit cache mounts.
RUN --mount=type=cache,id=repo-sandbox-apt-${TARGETARCH},target=/var/cache/apt,sharing=locked \
    --mount=type=cache,id=repo-sandbox-apt-lists-${TARGETARCH},target=/var/lib/apt/lists,sharing=locked \
    set -eu; \
    apt-get update; \
    apt-get install --yes --no-install-recommends build-essential ca-certificates git; \
    rm -rf /var/lib/apt/lists/* /var/cache/apt/archives/*.deb

COPY --from=toolchain-build /usr/local/cargo/ /usr/local/cargo/
COPY --from=toolchain-build /usr/local/rustup/ /usr/local/rustup/
COPY --from=toolchain-build /toolchain/bin/bazel /usr/local/bin/bazel
COPY --from=toolchain-build /toolchain/bin/bazelisk /usr/local/bin/bazelisk
ENV CARGO_HOME=/usr/local/cargo \
    RUSTUP_HOME=/usr/local/rustup \
    REPO_SANDBOX_BAZEL_VERSION=8.3.1 \
    PATH=/usr/local/cargo/bin:${PATH}

# Cargo network state is a reusable BuildKit cache, never an image layer. This
# step validates the copied tools without downloading project data.
RUN --mount=type=cache,id=repo-sandbox-cargo-registry-${TARGETARCH},target=/usr/local/cargo/registry,sharing=locked \
    --mount=type=cache,id=repo-sandbox-cargo-git-${TARGETARCH},target=/usr/local/cargo/git,sharing=locked \
    set -eu; \
    rustup default "$RUST_VERSION"; \
    ln -sf "$(rustup which --toolchain "$RUST_VERSION" rustc)" /usr/local/cargo/bin/rustc; \
    ln -sf "$(rustup which --toolchain "$RUST_VERSION" rustdoc)" /usr/local/cargo/bin/rustdoc; \
    rustc --version; \
    cargo --version; \
    bazel --output_user_root=/tmp/repo-sandbox-bazel-check --help >/dev/null; \
    rm -rf /tmp/repo-sandbox-bazel-check

# Resolve only this centrally owned fixture. Its lock pins every BCR metadata
# byte and every selected source archive carries an integrity hash. The GitHub
# token is intentionally unavailable in this stage. After pruning workspace and
# install state, the remaining global repository CAS is architecture-neutral.
FROM environment-base AS offline-seed
COPY offline-baseline/ /opt/repo-sandbox/offline-baseline/
RUN set -eu; \
    cd /opt/repo-sandbox/offline-baseline; \
    cp BUILD.seed BUILD.bazel; \
    cp MODULE.bazel MODULE.cpp; \
    cp MODULE.bazel.lock MODULE.cpp.lock; \
    mkdir -p apps/cli/src crates/core/src crates/adapters/src crates/adapters/e2e; \
    for source in apps/cli/src/lib.rs apps/cli/src/main.rs crates/core/src/lib.rs crates/adapters/src/lib.rs crates/adapters/e2e/e2e_matrix.rs; do \
      printf 'fn main() {}\n' > "$source"; \
    done; \
    mkdir -p /toolchain/bazel-seed; \
    cp MODULE.bazel.lock /tmp/repo-sandbox-baseline.lock; \
    bazel --batch --output_user_root=/toolchain/bazel-seed mod graph >/dev/null; \
    sed -n '/^  "registryFileHashes": {$/,/^  },$/p' /tmp/repo-sandbox-baseline.lock > /tmp/repo-sandbox-expected-hashes; \
    sed -n '/^  "registryFileHashes": {$/,/^  },$/p' MODULE.bazel.lock > /tmp/repo-sandbox-actual-hashes; \
    test -s /tmp/repo-sandbox-expected-hashes; \
    cmp /tmp/repo-sandbox-expected-hashes /tmp/repo-sandbox-actual-hashes; \
    bazel --batch --output_user_root=/toolchain/bazel-seed test //...; \
    cp MODULE.rust MODULE.bazel; \
    cp MODULE.rust.lock MODULE.bazel.lock; \
    cp BUILD.rust.seed BUILD.bazel; \
    bazel --batch --output_user_root=/toolchain/bazel-seed fetch @crates//...; \
    bazel --batch --output_user_root=/toolchain/bazel-seed test //...; \
    rm /tmp/repo-sandbox-baseline.lock /tmp/repo-sandbox-expected-hashes /tmp/repo-sandbox-actual-hashes; \
    find /toolchain/bazel-seed -mindepth 1 -maxdepth 1 ! -name cache -exec rm -rf '{}' +; \
    find /toolchain/bazel-seed/cache -mindepth 1 -maxdepth 1 ! -name repos -exec rm -rf '{}' +

# Prove that the repository CAS alone is sufficient after discarding the seed's
# installed toolchains, extracted repositories and action outputs.
RUN --network=none set -eu; \
    cd /opt/repo-sandbox/offline-baseline; \
    bazel --batch --output_user_root=/toolchain/bazel-seed test //... --repository_disable_download; \
    find /toolchain/bazel-seed -mindepth 1 -maxdepth 1 ! -name cache -exec rm -rf '{}' +; \
    find /toolchain/bazel-seed/cache -mindepth 1 -maxdepth 1 ! -name repos -exec rm -rf '{}' +; \
    cp MODULE.cpp MODULE.bazel; \
    cp MODULE.cpp.lock MODULE.bazel.lock; \
    cp BUILD.seed BUILD.bazel; \
    bazel --batch --output_user_root=/toolchain/bazel-seed test //... --repository_disable_download; \
    find /toolchain/bazel-seed -mindepth 1 -maxdepth 1 ! -name cache -exec rm -rf '{}' +; \
    find /toolchain/bazel-seed/cache -mindepth 1 -maxdepth 1 ! -name repos -exec rm -rf '{}' +

FROM environment-base AS environment
COPY --from=offline-seed /toolchain/bazel-seed/cache/repos/ /var/cache/repo-sandbox/bazel/cache/repos/
COPY offline-baseline/MODULE.bazel.lock /usr/local/share/repo-sandbox/offline-baseline/MODULE.bazel.lock
COPY bazel /tmp/repo-sandbox-bazel-wrapper
RUN set -eu; \
    mkdir -p /usr/local/libexec/repo-sandbox; \
    mv /usr/local/bin/bazel /usr/local/libexec/repo-sandbox/bazel-8.3.1; \
    install -m 0755 /tmp/repo-sandbox-bazel-wrapper /usr/local/bin/bazel; \
    rm /tmp/repo-sandbox-bazel-wrapper; \
    bazel version | grep -Fx 'Build label: 8.3.1'

# These values are part of the final image configuration. BuildKit can reuse
# unchanged installation layers, while a template, component, or parameter
# change still produces a distinct environment image.
ARG REPO_SANDBOX_TEMPLATE_ID
ARG REPO_SANDBOX_TEMPLATE_VERSION
ARG REPO_SANDBOX_PLAN_DIGEST
ARG REPO_SANDBOX_REPOSITORY_ID
LABEL org.opencontainers.image.title="${REPO_SANDBOX_TEMPLATE_ID}" \
      org.opencontainers.image.version="${REPO_SANDBOX_TEMPLATE_VERSION}" \
      io.repo-sandbox.plan-digest="${REPO_SANDBOX_PLAN_DIGEST}" \
      io.repo-sandbox.owner="${REPO_SANDBOX_PLAN_DIGEST}" \
      io.repo-sandbox.repository-id="${REPO_SANDBOX_REPOSITORY_ID}"
