FROM debian:bookworm-slim
ARG BAZELISK_VERSION=1.27.0
RUN apt-get update \
    && apt-get install --yes --no-install-recommends ca-certificates curl \
    && curl --fail --location --show-error \
      "https://github.com/bazelbuild/bazelisk/releases/download/v${BAZELISK_VERSION}/bazelisk-linux-$(dpkg --print-architecture)" \
      --output /usr/local/bin/bazel \
    && chmod 0755 /usr/local/bin/bazel \
    && rm -rf /var/lib/apt/lists/*
