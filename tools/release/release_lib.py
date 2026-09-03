"""Shared, dependency-free release validation and archive helpers."""

import gzip
import hashlib
import io
import os
import platform as host_platform
import re
import struct
import subprocess
import tarfile
from pathlib import Path


ROOT = Path(__file__).resolve().parents[2]
VERSION_PATTERN = re.compile(r"(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)")
PLATFORMS = {
    "linux-amd64": ("x86_64", 62),
    "linux-arm64": ("aarch64", 183),
}


class ReleaseError(RuntimeError):
    """A release input or artifact violates the release contract."""


def sha256(path: Path) -> str:
    digest = hashlib.sha256()
    with path.open("rb") as source:
        for chunk in iter(lambda: source.read(1024 * 1024), b""):
            digest.update(chunk)
    return digest.hexdigest()


def workspace_version(root: Path = ROOT) -> str:
    cargo = (root / "Cargo.toml").read_text(encoding="utf-8")
    section = re.search(
        r"^\[workspace\.package\]\s*$\n(.*?)(?=^\[|\Z)",
        cargo,
        re.MULTILINE | re.DOTALL,
    )
    declared = [] if section is None else re.findall(
        r'^\s*version\s*=\s*"([^"]+)"\s*$', section.group(1), re.MULTILINE
    )
    if len(declared) != 1 or VERSION_PATTERN.fullmatch(declared[0]) is None:
        raise ReleaseError("Cargo.toml must declare one canonical workspace version")
    version = declared[0]

    module = (root / "MODULE.bazel").read_text(encoding="utf-8")
    module_version = re.search(
        r'module\(\s*name\s*=\s*"repo_sandbox",\s*version\s*=\s*"([^"]+)"',
        module,
    )
    if module_version is None or module_version.group(1) != version:
        raise ReleaseError("Cargo workspace version does not match MODULE.bazel")

    cli_build = (root / "apps" / "cli" / "BUILD.bazel").read_text(encoding="utf-8")
    bazel_versions = re.findall(r'^\s*version\s*=\s*"([^"]+)"', cli_build, re.MULTILINE)
    if not bazel_versions or any(item != version for item in bazel_versions):
        raise ReleaseError("Cargo workspace version does not match every CLI Bazel target")
    return version


def validate_tag(tag: str, root: Path = ROOT) -> str:
    if not tag.startswith("v") or VERSION_PATTERN.fullmatch(tag[1:]) is None:
        raise ReleaseError("release tag must be canonical vMAJOR.MINOR.PATCH")
    version = workspace_version(root)
    if tag[1:] != version:
        raise ReleaseError(f"tag {tag} does not match workspace version {version}")
    return version


def elf_machine(path: Path) -> int:
    with path.open("rb") as binary:
        header = binary.read(20)
    if len(header) < 20 or header[:4] != b"\x7fELF":
        raise ReleaseError(f"release binary is not ELF: {path}")
    if header[5] == 1:
        return struct.unpack_from("<H", header, 18)[0]
    if header[5] == 2:
        return struct.unpack_from(">H", header, 18)[0]
    raise ReleaseError(f"release binary has an invalid ELF byte order: {path}")


def validate_binary(binary: Path, release_platform: str, version: str) -> None:
    if release_platform not in PLATFORMS:
        raise ReleaseError(f"unsupported release platform: {release_platform}")
    if not binary.is_file() or not os.access(binary, os.X_OK):
        raise ReleaseError(f"CLI binary is not executable: {binary}")
    expected_host, expected_machine = PLATFORMS[release_platform]
    actual_host = host_platform.machine().lower()
    aliases = {"x86_64": {"x86_64", "amd64"}, "aarch64": {"aarch64", "arm64"}}
    if actual_host not in aliases[expected_host]:
        raise ReleaseError(
            f"native runner mismatch for {release_platform}: {host_platform.machine()}"
        )
    if elf_machine(binary) != expected_machine:
        raise ReleaseError(f"ELF architecture does not match {release_platform}")
    completed = subprocess.run(
        [str(binary), "--version"],
        check=False,
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
        universal_newlines=True,
        timeout=30,
    )
    expected_version = f"repo-sandbox {version}"
    if completed.returncode != 0 or completed.stdout.strip() != expected_version:
        raise ReleaseError(f"CLI --version does not match v{version}")


def create_archive(binary: Path, destination: Path) -> None:
    contents = binary.read_bytes()
    destination.parent.mkdir(parents=True, exist_ok=True)
    with destination.open("xb") as raw:
        with gzip.GzipFile(filename="", mode="wb", fileobj=raw, mtime=0, compresslevel=9) as zipped:
            with tarfile.open(fileobj=zipped, mode="w", format=tarfile.PAX_FORMAT) as archive:
                member = tarfile.TarInfo("repo-sandbox")
                member.size = len(contents)
                member.mode = 0o755
                member.uid = 0
                member.gid = 0
                member.uname = ""
                member.gname = ""
                member.mtime = 0
                archive.addfile(member, io.BytesIO(contents))


def archive_binary(archive_path: Path) -> bytes:
    try:
        gzip_header = archive_path.read_bytes()[:10]
        if len(gzip_header) != 10 or gzip_header[:3] != b"\x1f\x8b\x08" or gzip_header[4:8] != b"\0\0\0\0":
            raise ReleaseError("release archive gzip header is not canonical")
        with tarfile.open(archive_path, mode="r:gz") as archive:
            members = archive.getmembers()
            if len(members) != 1:
                raise ReleaseError("release archive must contain exactly one member")
            member = members[0]
            if member.name != "repo-sandbox" or not member.isfile():
                raise ReleaseError("release archive member must be the regular file repo-sandbox")
            if member.mode != 0o755 or member.uid != 0 or member.gid != 0 or member.mtime != 0:
                raise ReleaseError("release archive metadata is not canonical")
            if member.uname or member.gname or member.pax_headers:
                raise ReleaseError("release archive identity metadata is not canonical")
            extracted = archive.extractfile(member)
            if extracted is None:
                raise ReleaseError("release archive binary cannot be read")
            return extracted.read()
    except (tarfile.TarError, OSError) as error:
        raise ReleaseError(f"invalid release archive: {error}") from error


def parse_checksum(checksum_path: Path, archive_name: str) -> str:
    line = checksum_path.read_text(encoding="ascii")
    match = re.fullmatch(r"([0-9a-f]{64})  ([^\r\n]+)\n", line)
    if match is None or match.group(2) != archive_name:
        raise ReleaseError(f"invalid checksum file: {checksum_path.name}")
    return match.group(1)


def required_glibc_versions(binary: Path):
    completed = subprocess.run(
        ["readelf", "--version-info", str(binary)],
        check=False,
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
        universal_newlines=True,
        timeout=30,
    )
    if completed.returncode != 0:
        raise ReleaseError(f"readelf rejected release binary: {completed.stderr.strip()}")
    return {
        (int(major), int(minor))
        for major, minor in re.findall(r"GLIBC_(\d+)\.(\d+)", completed.stdout)
    }


def check_glibc(binary: Path, maximum=(2, 28)) -> None:
    too_new = sorted(version for version in required_glibc_versions(binary) if version > maximum)
    if too_new:
        version = too_new[-1]
        raise ReleaseError(
            f"CLI requires GLIBC_{version[0]}.{version[1]}, newer than supported "
            f"GLIBC_{maximum[0]}.{maximum[1]}"
        )
