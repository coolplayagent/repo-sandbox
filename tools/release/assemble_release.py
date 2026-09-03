#!/usr/bin/env python3
"""Validate the exact two-platform release asset set and write SHA256SUMS."""

import argparse
from pathlib import Path

from release_lib import ROOT, ReleaseError, parse_checksum, sha256, validate_tag


def assemble(tag: str, directory: Path, root: Path = ROOT) -> None:
    version = validate_tag(tag, root)
    directory = directory.resolve()
    archives = [
        f"repo-sandbox-{version}-linux-amd64.tar.gz",
        f"repo-sandbox-{version}-linux-arm64.tar.gz",
    ]
    expected = {name for archive in archives for name in (archive, archive + ".sha256")}
    actual = {path.name for path in directory.iterdir() if path.is_file()}
    if actual != expected:
        raise ReleaseError(f"release directory has an unexpected file set: {sorted(actual)}")
    lines = []
    for archive in archives:
        digest = parse_checksum(directory / f"{archive}.sha256", archive)
        if sha256(directory / archive) != digest:
            raise ReleaseError(f"checksum mismatch for {archive}")
        lines.append(f"{digest}  {archive}\n")
    with (directory / "SHA256SUMS").open("w", encoding="ascii", newline="\n") as checksums:
        checksums.write("".join(lines))


def main() -> None:
    parser = argparse.ArgumentParser()
    parser.add_argument("--tag", required=True)
    parser.add_argument("--directory", required=True, type=Path)
    arguments = parser.parse_args()
    assemble(arguments.tag, arguments.directory)


if __name__ == "__main__":
    try:
        main()
    except (OSError, ReleaseError) as error:
        raise SystemExit(f"assemble-release: {error}") from error
