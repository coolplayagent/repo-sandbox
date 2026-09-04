#!/usr/bin/env python3
"""Validate a release tag against every version authority."""

import argparse

from release_lib import ReleaseError, validate_tag


parser = argparse.ArgumentParser()
parser.add_argument("--tag", required=True)
arguments = parser.parse_args()
try:
    print(validate_tag(arguments.tag))
except (OSError, ReleaseError) as error:
    raise SystemExit(f"validate-release: {error}") from error
