#!/usr/bin/env bash
set -euo pipefail
root=$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)
seed="$root/templates/rust-bazel/context/offline-baseline"
# Keep the trusted offline closure aligned with the repository we dogfood.
cmp "$root/MODULE.bazel" "$seed/MODULE.rust"
cmp "$root/MODULE.bazel.lock" "$seed/MODULE.rust.lock"
for manifest in Cargo.toml Cargo.lock \
  apps/cli/Cargo.toml crates/core/Cargo.toml crates/adapters/Cargo.toml; do
  cmp "$root/$manifest" "$seed/$manifest"
done
python3 - "$root" <<'PY'
import json, pathlib, re, sys, tomllib
root = pathlib.Path(sys.argv[1])
version = (root / '.bazelversion').read_text().strip()
# A new Bazel release must explicitly update its supported lock schema. Changing
# only the lockFileVersion field does not migrate generated repository records.
supported_schemas = {'9.2.0': 28}
assert version in supported_schemas, f'update the offline lock schema contract for Bazel {version}'
for relative in ('MODULE.bazel.lock',
                 'templates/rust-bazel/context/offline-baseline/MODULE.rust.lock',
                 'templates/rust-bazel/context/offline-baseline/MODULE.bazel.lock'):
    actual = json.loads((root / relative).read_text())['lockFileVersion']
    assert actual == supported_schemas[version], f'{relative}: schema {actual} is incompatible with Bazel {version}'
for relative, pattern in (
    ('templates/rust-bazel/context/Dockerfile', r'bazel_version=([0-9.]+)'),
    ('templates/rust-bazel/context/Dockerfile', r'REPO_SANDBOX_BAZEL_VERSION=([0-9.]+)'),
    ('tests/multistage/Dockerfile.single-stage', r'ARG BAZEL_VERSION=([0-9.]+)'),
    ('templates/rust-bazel/context/Dockerfile', r'/libexec/repo-sandbox/bazel-([0-9.]+)'),
    ('tests/multistage/Dockerfile.single-stage', r'/libexec/repo-sandbox/bazel-([0-9.]+)'),
    ('templates/rust-bazel/context/bazel', r'/libexec/repo-sandbox/bazel-([0-9.]+)'),
):
    versions = re.findall(pattern, (root / relative).read_text())
    assert versions and set(versions) == {version}, f'{relative}: Bazel versions {versions} differ from .bazelversion {version}'
for relative in ('templates/rust-bazel/context/Dockerfile', 'tests/multistage/Dockerfile.single-stage'):
    fetches = [line for line in (root / relative).read_text().splitlines()
               if 'fetch @crates//... //:rust_dependencies_full' in line]
    assert len(fetches) == 2 and all('--lockfile_mode=error' in line for line in fetches), f'{relative}: Rust seed fetch must reject stale locks online and offline'
seed = root / 'templates/rust-bazel/context/offline-baseline/BUILD.rust.seed'
build = seed.read_text()
for member in tomllib.loads((root / 'Cargo.toml').read_text())['workspace']['members']:
    for name, spec in tomllib.loads((root / member / 'Cargo.toml').read_text()).get('dependencies', {}).items():
        if isinstance(spec, dict) and spec.get('workspace'):
            assert f'"{name}"' in build, f'offline Rust fixture must depend on {name}'
PY
printf '%s\n' 'Offline Rust dependency closure contract passed'
