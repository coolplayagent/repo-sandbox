#!/usr/bin/env bash
set -euo pipefail

[[ $# == 5 ]] || { echo "usage: $0 TAG REPOSITORY RUN_ID RUN_ATTEMPT OUTPUT_DIRECTORY" >&2; exit 64; }
tag=$1
repository=$2
run_id=$3
run_attempt=$4
output_directory=$5
root=$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)

"$root/scripts/ci/validate-release.sh" "$tag" >/dev/null
[[ $repository =~ ^[A-Za-z0-9_.-]+/[A-Za-z0-9_.-]+$ ]] || {
  echo "repository must be a canonical owner/name" >&2; exit 64;
}
[[ $run_id =~ ^[1-9][0-9]*$ && $run_attempt =~ ^[1-9][0-9]*$ ]] || {
  echo "run ID and attempt must be positive decimal integers" >&2; exit 64;
}
mkdir -p -- "$output_directory"
[[ -z $(find "$output_directory" -mindepth 1 -maxdepth 1 -print -quit) ]] || {
  echo "artifact output directory must be empty" >&2; exit 1;
}

rows=$(gh api --paginate "repos/${repository}/actions/runs/${run_id}/artifacts?per_page=100" \
  --jq '.artifacts[] | [.id, .name, .expired, .workflow_run.id] | @tsv')

amd64_id= amd64_attempt=0
arm64_id= arm64_attempt=0
while IFS=$'\t' read -r artifact_id artifact_name artifact_expired artifact_run_id; do
  [[ -z $artifact_id && -z $artifact_name && -z $artifact_expired && -z $artifact_run_id ]] && continue
  [[ $artifact_id =~ ^[1-9][0-9]*$ && $artifact_run_id == "$run_id" && \
    $artifact_expired =~ ^(true|false)$ ]] || continue
  [[ $artifact_expired == false ]] || continue
  [[ $artifact_name =~ ^cli-(linux-amd64|linux-arm64)-${run_id}-([1-9][0-9]*)$ ]] || continue
  platform=${BASH_REMATCH[1]}
  attempt=${BASH_REMATCH[2]}
  (( attempt <= run_attempt )) || continue

  if [[ $platform == linux-amd64 ]]; then
    if (( attempt > amd64_attempt )); then amd64_id=$artifact_id; amd64_attempt=$attempt
    elif (( attempt == amd64_attempt )); then echo "duplicate latest amd64 artifact" >&2; exit 1
    fi
  else
    if (( attempt > arm64_attempt )); then arm64_id=$artifact_id; arm64_attempt=$attempt
    elif (( attempt == arm64_attempt )); then echo "duplicate latest arm64 artifact" >&2; exit 1
    fi
  fi
done <<< "$rows"

[[ -n $amd64_id && -n $arm64_id ]] || {
  echo "this workflow run does not contain one usable artifact per release platform" >&2; exit 1;
}

temporary=$(mktemp -d)
trap 'rm -rf -- "$temporary"' EXIT
for platform in linux-amd64 linux-arm64; do
  if [[ $platform == linux-amd64 ]]; then artifact_id=$amd64_id; else artifact_id=$arm64_id; fi
  gh api "repos/${repository}/actions/artifacts/${artifact_id}/zip" > "$temporary/$platform.zip"
done

python_command=python3
if ! python3 -c 'import sys' >/dev/null 2>&1; then python_command=python; fi
"$python_command" - "$temporary" "$output_directory" "${tag#v}" <<'PY'
import pathlib
import shutil
import sys
import zipfile

source = pathlib.Path(sys.argv[1])
output = pathlib.Path(sys.argv[2])
version = sys.argv[3]
for platform in ("linux-amd64", "linux-arm64"):
    archive_name = f"repo-sandbox-{version}-{platform}.tar.gz"
    expected = {archive_name, f"{archive_name}.sha256"}
    with zipfile.ZipFile(source / f"{platform}.zip") as bundle:
        names = bundle.namelist()
        if len(names) != len(expected) or set(names) != expected:
            raise SystemExit(f"artifact zip for {platform} has an unexpected file set")
        for name in sorted(expected):
            with bundle.open(name) as incoming, (output / name).open("xb") as outgoing:
                shutil.copyfileobj(incoming, outgoing)
PY

echo "downloaded latest run-scoped artifacts: amd64 attempt $amd64_attempt, arm64 attempt $arm64_attempt"
