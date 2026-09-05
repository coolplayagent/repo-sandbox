FROM debian:bookworm-slim
RUN bazelisk_version=1.27.0 \
    && apt-get update \
    && apt-get install --yes --no-install-recommends ca-certificates curl \
    && curl --fail --location --show-error \
      "https://github.com/bazelbuild/bazelisk/releases/download/v${bazelisk_version}/bazelisk-linux-$(dpkg --print-architecture)" \
      --output /usr/local/bin/bazel \
    && chmod 0755 /usr/local/bin/bazel \
    && rm -rf /var/lib/apt/lists/*
