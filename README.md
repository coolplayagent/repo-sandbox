# repo-sandbox

`repo-sandbox` is a Rust CLI built and tested exclusively through Bazel with
Bzlmod. The initial skeleton separates the CLI, domain core, and infrastructure
adapters so future integrations do not leak into the domain layer.

## Build and test

Install [Bazelisk](https://github.com/bazelbuild/bazelisk), then use the checked-in
Bazel version and module graph:

```console
bazelisk build //...
bazelisk test //...
bazelisk run //:repo-sandbox -- --help
bazelisk run //:repo-sandbox -- --version
```

The `doctor`, `plan`, `build`, `verify`, and `clean` routes are intentionally
reserved; their behavior belongs to follow-up issues.

`Cargo.toml` and `Cargo.lock` describe Rust packages and third-party dependency
resolution for Bzlmod's crate_universe. Cargo is not a supported build entrypoint.
