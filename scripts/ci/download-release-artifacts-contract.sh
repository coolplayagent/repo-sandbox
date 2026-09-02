#!/usr/bin/env bash
set -euo pipefail

root=$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)
temporary=$(mktemp -d)
trap 'rm -rf -- "$temporary"' EXIT
mkdir -p "$temporary/bin" "$temporary/zips"
version=$("$root/scripts/ci/workspace-version.sh")

python_command=python3
if ! python3 -c 'import sys' >/dev/null 2>&1; then python_command=python; fi
"$python_command" - "$temporary/zips" "$version" <<'PY'
import pathlib
import sys
import zipfile

root = pathlib.Path(sys.argv[1])
version = sys.argv[2]
for artifact_id, platform, marker in (
    (101, "linux-amd64", "amd64-attempt-1"),
    (102, "linux-amd64", "amd64-attempt-2"),
    (201, "linux-arm64", "arm64-attempt-1"),
    (202, "linux-arm64", "arm64-attempt-2"),
):
    archive = f"repo-sandbox-{version}-{platform}.tar.gz"
    with zipfile.ZipFile(root / f"{artifact_id}.zip", "w") as bundle:
        bundle.writestr(archive, marker)
        bundle.writestr(f"{archive}.sha256", f"checksum:{marker}\n")
PY

cat > "$temporary/bin/gh" <<'MOCK'
#!/usr/bin/env bash
set -euo pipefail
endpoint=
for argument in "$@"; do [[ $argument != repos/* ]] || endpoint=$argument; done
case $endpoint in
  */actions/runs/77/artifacts?per_page=100)
    case $MOCK_ARTIFACT_STATE in
      mixed)
        printf '%s\n' \
          $'101\tcli-linux-amd64-77-1\tfalse\t77' \
          $'102\tcli-linux-amd64-77-3\tfalse\t77' \
          $'201\tcli-linux-arm64-77-1\ttrue\t77' \
          $'202\tcli-linux-arm64-77-2\tfalse\t77' \
          $'999\tcli-linux-arm64-77-2\tfalse\t78'
        ;;
      missing) printf '%s\n' $'101\tcli-linux-amd64-77-1\tfalse\t77' ;;
      duplicate)
        printf '%s\n' \
          $'101\tcli-linux-amd64-77-1\tfalse\t77' \
          $'102\tcli-linux-amd64-77-1\tfalse\t77' \
          $'202\tcli-linux-arm64-77-2\tfalse\t77'
        ;;
      cross-run)
        printf '%s\n' \
          $'101\tcli-linux-amd64-77-1\tfalse\t78' \
          $'202\tcli-linux-arm64-77-2\tfalse\t78'
        ;;
      error) exit 1 ;;
    esac
    ;;
  */actions/artifacts/*/zip)
    artifact_id=${endpoint%/zip}
    artifact_id=${artifact_id##*/}
    cat "$MOCK_ZIP_DIRECTORY/$artifact_id.zip"
    ;;
  *) exit 2 ;;
esac
MOCK
chmod +x "$temporary/bin/gh"
export PATH="$temporary/bin:$PATH" MOCK_ZIP_DIRECTORY="$temporary/zips"

export MOCK_ARTIFACT_STATE=mixed
mkdir "$temporary/output"
"$root/scripts/ci/download-release-artifacts.sh" "v$version" owner/repository 77 2 \
  "$temporary/output" >/dev/null
grep -Fqx 'amd64-attempt-1' "$temporary/output/repo-sandbox-${version}-linux-amd64.tar.gz"
grep -Fqx 'arm64-attempt-2' "$temporary/output/repo-sandbox-${version}-linux-arm64.tar.gz"
[[ $(find "$temporary/output" -type f | wc -l) -eq 4 ]]

for unsafe_state in missing duplicate cross-run error; do
  export MOCK_ARTIFACT_STATE=$unsafe_state
  output="$temporary/output-$unsafe_state"
  if "$root/scripts/ci/download-release-artifacts.sh" "v$version" owner/repository 77 2 "$output" \
    >/dev/null 2>&1; then
    echo "unsafe artifact selection accepted: $unsafe_state" >&2; exit 1
  fi
done

if "$root/scripts/ci/download-release-artifacts.sh" "v$version" owner/repository '77;id' 2 \
  "$temporary/injected" >/dev/null 2>&1; then
  echo "injected run ID was accepted" >&2; exit 1
fi

echo "run-scoped cross-attempt artifact selection contracts passed"
