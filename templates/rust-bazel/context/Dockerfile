# syntax=docker/dockerfile:1.7@sha256:a57df69d0ea827fb7266491f2813635de6f17269be881f696fbfdf2d83dda33e
ARG BASE_IMAGE=docker.io/library/rust:1.97.0-bookworm

# The official Rust image is intentionally confined to the assembly stage. Its
# download/build utilities and image metadata are not inherited by the runtime.
FROM ${BASE_IMAGE} AS toolchain-build

ARG TARGETARCH
ARG BAZELISK_VERSION=1.27.0
ARG BAZEL_VERSION=8.3.1
RUN --mount=type=cache,id=repo-sandbox-apt-${TARGETARCH},target=/var/cache/apt,sharing=locked \
    --mount=type=cache,id=repo-sandbox-apt-lists-${TARGETARCH},target=/var/lib/apt/lists,sharing=locked \
    --mount=type=cache,id=repo-sandbox-toolchain-downloads-${TARGETARCH},target=/toolchain-downloads,sharing=locked \
    --mount=type=secret,id=github_token,required=false \
    set -eu; \
    token_header=''; \
    if [ -s /run/secrets/github_token ]; then \
      token=$(cat /run/secrets/github_token); \
      case "$token" in github_pat_*|ghp_*) token_header="Authorization: Bearer $token" ;; esac; \
    fi; \
    apt-get update; \
    apt-get install --yes --no-install-recommends ca-certificates curl; \
    mkdir -p /toolchain/bin; \
    case "$TARGETARCH" in amd64) bazel_arch=x86_64 ;; arm64) bazel_arch=arm64 ;; *) exit 64 ;; esac; \
    if [ -n "$token_header" ]; then header_args="--header=$token_header"; else header_args=''; fi; \
    bazelisk_download="/toolchain-downloads/bazelisk-${BAZELISK_VERSION}-${TARGETARCH}"; \
    bazel_download="/toolchain-downloads/bazel-${BAZEL_VERSION}-${bazel_arch}"; \
    if [ ! -s "$bazelisk_download" ]; then \
      curl --fail --location --show-error $header_args \
        "https://github.com/bazelbuild/bazelisk/releases/download/v${BAZELISK_VERSION}/bazelisk-linux-${TARGETARCH}" \
        --output "$bazelisk_download"; \
    fi; \
    if [ ! -s "$bazel_download" ]; then \
      curl --fail --location --show-error $header_args \
        "https://github.com/bazelbuild/bazel/releases/download/${BAZEL_VERSION}/bazel-${BAZEL_VERSION}-linux-${bazel_arch}" \
        --output "$bazel_download"; \
    fi; \
    install -m 0755 "$bazel_download" /toolchain/bin/bazel; \
    install -m 0755 "$bazelisk_download" /toolchain/bin/bazelisk; \
    rustc --version; \
    cargo --version

FROM debian:bookworm-slim AS environment

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
    PATH=/usr/local/cargo/bin:${PATH}

# Cargo and Bazel network state are reusable BuildKit caches, never image
# layers. This step validates the copied tools without downloading project data.
RUN --mount=type=cache,id=repo-sandbox-cargo-registry-${TARGETARCH},target=/usr/local/cargo/registry,sharing=locked \
    --mount=type=cache,id=repo-sandbox-cargo-git-${TARGETARCH},target=/usr/local/cargo/git,sharing=locked \
    --mount=type=cache,id=repo-sandbox-bazel-${TARGETARCH},target=/root/.cache/bazel,sharing=locked \
    set -eu; \
    rustup default "$RUST_VERSION"; \
    ln -sf "$(rustup which --toolchain "$RUST_VERSION" rustc)" /usr/local/cargo/bin/rustc; \
    ln -sf "$(rustup which --toolchain "$RUST_VERSION" rustdoc)" /usr/local/cargo/bin/rustdoc; \
    rustc --version; \
    cargo --version; \
    bazel --help >/dev/null

# These values are part of the final image configuration. BuildKit can reuse
# unchanged installation layers, while a template, component, or parameter
# change still produces a distinct environment image.
ARG REPO_SANDBOX_TEMPLATE_ID
ARG REPO_SANDBOX_TEMPLATE_VERSION
ARG REPO_SANDBOX_PLAN_DIGEST
LABEL org.opencontainers.image.title="${REPO_SANDBOX_TEMPLATE_ID}" \
      org.opencontainers.image.version="${REPO_SANDBOX_TEMPLATE_VERSION}" \
      io.repo-sandbox.plan-digest="${REPO_SANDBOX_PLAN_DIGEST}"
