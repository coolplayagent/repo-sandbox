#!/usr/bin/env python3
"""Verify one published archive and optionally execute its binary."""

import argparse
import os
import subprocess
import tempfile
from pathlib import Path

from release_lib import (
    ReleaseError,
    archive_binary,
    check_glibc,
    parse_checksum,
    sha256,
    validate_binary,
    validate_tag,
)


def main() -> None:
    parser = argparse.ArgumentParser()
    parser.add_argument("--tag", required=True)
    parser.add_argument("--platform", required=True)
    parser.add_argument("--archive", required=True, type=Path)
    parser.add_argument("--checksum", required=True, type=Path)
    arguments = parser.parse_args()

    version = validate_tag(arguments.tag)
    archive = arguments.archive.resolve()
    expected = parse_checksum(arguments.checksum.resolve(), archive.name)
    if sha256(archive) != expected:
        raise ReleaseError("release archive SHA-256 does not match its checksum")
    contents = archive_binary(archive)
    with tempfile.TemporaryDirectory(prefix="repo-sandbox-verify-") as temporary:
        binary = Path(temporary) / "repo-sandbox"
        binary.write_bytes(contents)
        binary.chmod(0o755)
        validate_binary(binary, arguments.platform, version)
        check_glibc(binary)
        subprocess.run([str(binary), "--help"], check=True, timeout=30)


if __name__ == "__main__":
    try:
        main()
    except (OSError, ReleaseError, subprocess.SubprocessError) as error:
        raise SystemExit(f"verify-archive: {error}") from error
