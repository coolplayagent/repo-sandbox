ARG BASE_IMAGE=docker.io/library/rust:1.97.0-bookworm
FROM ${BASE_IMAGE}

# These values are part of the final image configuration. BuildKit can reuse
# unchanged installation layers, while a template, component, or parameter
# change still produces a distinct environment image.
ARG REPO_SANDBOX_TEMPLATE_ID
ARG REPO_SANDBOX_TEMPLATE_VERSION
ARG REPO_SANDBOX_PLAN_DIGEST
LABEL org.opencontainers.image.title="${REPO_SANDBOX_TEMPLATE_ID}" \
      org.opencontainers.image.version="${REPO_SANDBOX_TEMPLATE_VERSION}" \
      io.repo-sandbox.plan-digest="${REPO_SANDBOX_PLAN_DIGEST}"

ARG BAZELISK_VERSION=1.27.0
RUN apt-get update \
    && apt-get install --yes --no-install-recommends ca-certificates curl git \
    && curl --fail --location --show-error \
      "https://github.com/bazelbuild/bazelisk/releases/download/v${BAZELISK_VERSION}/bazelisk-linux-$(dpkg --print-architecture)" \
      --output /usr/local/bin/bazel \
    && chmod 0755 /usr/local/bin/bazel \
    && rm -rf /var/lib/apt/lists/*
