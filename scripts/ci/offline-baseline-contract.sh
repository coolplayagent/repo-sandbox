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
import pathlib, sys, tomllib
root = pathlib.Path(sys.argv[1])
seed = root / 'templates/rust-bazel/context/offline-baseline/BUILD.rust.seed'
build = seed.read_text()
for member in tomllib.loads((root / 'Cargo.toml').read_text())['workspace']['members']:
    for name, spec in tomllib.loads((root / member / 'Cargo.toml').read_text()).get('dependencies', {}).items():
        if isinstance(spec, dict) and spec.get('workspace'):
            assert f'"{name}"' in build, f'offline Rust fixture must depend on {name}'
PY
printf '%s\n' 'Offline Rust dependency closure contract passed'
