#!/usr/bin/env python3
"""Validate and deterministically package one native repo-sandbox binary."""

import argparse
from pathlib import Path

from release_lib import ReleaseError, check_glibc, create_archive, sha256, validate_binary, validate_tag


def main() -> None:
    parser = argparse.ArgumentParser()
    parser.add_argument("--tag", required=True)
    parser.add_argument("--platform", required=True)
    parser.add_argument("--binary", required=True, type=Path)
    parser.add_argument("--output", required=True, type=Path)
    arguments = parser.parse_args()

    version = validate_tag(arguments.tag)
    binary = arguments.binary.resolve()
    validate_binary(binary, arguments.platform, version)
    check_glibc(binary)
    name = f"repo-sandbox-{version}-{arguments.platform}.tar.gz"
    destination = arguments.output.resolve() / name
    create_archive(binary, destination)
    digest = sha256(destination)
    with destination.with_name(name + ".sha256").open(
        "w", encoding="ascii", newline="\n"
    ) as checksum:
        checksum.write(f"{digest}  {name}\n")
    print(destination)


if __name__ == "__main__":
    try:
        main()
    except (OSError, ReleaseError) as error:
        raise SystemExit(f"package-cli: {error}") from error
