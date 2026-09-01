# syntax=docker/dockerfile:1.7
ARG BASE_IMAGE=docker.io/library/rust:1.97.0-bookworm

# The official Rust image is intentionally confined to the assembly stage. Its
# download/build utilities and image metadata are not inherited by the runtime.
FROM ${BASE_IMAGE} AS toolchain-build

ARG TARGETARCH
ARG BAZELISK_VERSION=1.27.0
RUN --mount=type=cache,id=repo-sandbox-apt-${TARGETARCH},target=/var/cache/apt,sharing=locked \
    --mount=type=cache,id=repo-sandbox-apt-lists-${TARGETARCH},target=/var/lib/apt/lists,sharing=locked \
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
    if [ -n "$token_header" ]; then header_args="--header=$token_header"; else header_args=''; fi; \
    curl --fail --location --show-error $header_args \
      "https://github.com/bazelbuild/bazelisk/releases/download/v${BAZELISK_VERSION}/bazelisk-linux-${TARGETARCH}" \
      --output /toolchain/bin/bazel; \
    chmod 0755 /toolchain/bin/bazel; \
    rustc --version; \
    cargo --version

FROM debian:bookworm-slim AS environment

ARG TARGETARCH
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
ENV CARGO_HOME=/usr/local/cargo \
    RUSTUP_HOME=/usr/local/rustup \
    PATH=/usr/local/cargo/bin:${PATH}

# Cargo and Bazel network state are reusable BuildKit caches, never image
# layers. This step validates the copied tools without downloading project data.
RUN --mount=type=cache,id=repo-sandbox-cargo-registry-${TARGETARCH},target=/usr/local/cargo/registry,sharing=locked \
    --mount=type=cache,id=repo-sandbox-cargo-git-${TARGETARCH},target=/usr/local/cargo/git,sharing=locked \
    --mount=type=cache,id=repo-sandbox-bazel-${TARGETARCH},target=/root/.cache/bazel,sharing=locked \
    rustc --version && cargo --version && bazel --help >/dev/null

# These values are part of the final image configuration. BuildKit can reuse
# unchanged installation layers, while a template, component, or parameter
# change still produces a distinct environment image.
ARG REPO_SANDBOX_TEMPLATE_ID
ARG REPO_SANDBOX_TEMPLATE_VERSION
ARG REPO_SANDBOX_PLAN_DIGEST
LABEL org.opencontainers.image.title="${REPO_SANDBOX_TEMPLATE_ID}" \
      org.opencontainers.image.version="${REPO_SANDBOX_TEMPLATE_VERSION}" \
      io.repo-sandbox.plan-digest="${REPO_SANDBOX_PLAN_DIGEST}"
