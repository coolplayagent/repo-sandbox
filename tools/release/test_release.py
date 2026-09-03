import gzip
import io
import stat
import struct
import tarfile
import tempfile
import unittest
from pathlib import Path
from unittest import mock

from assemble_release import assemble
from release_lib import (
    ReleaseError,
    archive_binary,
    check_glibc,
    create_archive,
    elf_machine,
    parse_checksum,
    sha256,
    validate_binary,
    validate_tag,
    workspace_version,
)


def fake_elf(machine: int, payload: bytes = b"") -> bytes:
    header = bytearray(64)
    header[:6] = b"\x7fELF\x02\x01"
    struct.pack_into("<H", header, 18, machine)
    return bytes(header) + payload


class ReleaseTests(unittest.TestCase):
    def authority(self, root: Path, version: str = "1.2.3") -> None:
        (root / "apps/cli").mkdir(parents=True)
        (root / "Cargo.toml").write_text(
            f'[workspace.package]\nversion = "{version}"\n', encoding="utf-8"
        )
        (root / "MODULE.bazel").write_text(
            f'module(\n    name = "repo_sandbox",\n    version = "{version}",\n)\n', encoding="utf-8"
        )
        (root / "apps/cli/BUILD.bazel").write_text(
            f'rust_binary(\n    version = "{version}",\n)\n', encoding="utf-8"
        )

    def test_version_authorities_and_tag_must_match(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            self.authority(root)
            self.assertEqual(workspace_version(root), "1.2.3")
            self.assertEqual(validate_tag("v1.2.3", root), "1.2.3")
            with self.assertRaises(ReleaseError):
                validate_tag("1.2.3", root)
            with self.assertRaises(ReleaseError):
                validate_tag("v1.2.4", root)

    def test_elf_architecture_is_read_from_header(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            binary = Path(temporary) / "repo-sandbox"
            binary.write_bytes(fake_elf(62))
            self.assertEqual(elf_machine(binary), 62)
            binary.write_bytes(b"not an elf")
            with self.assertRaises(ReleaseError):
                elf_machine(binary)

    def test_binary_validation_rejects_unknown_platform_and_mismatch(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            binary = Path(temporary) / "repo-sandbox"
            binary.write_bytes(fake_elf(62))
            binary.chmod(binary.stat().st_mode | stat.S_IXUSR)
            with self.assertRaises(ReleaseError):
                validate_binary(binary, "linux-riscv64", "1.2.3")
            with mock.patch("release_lib.host_platform.machine", return_value="x86_64"):
                with self.assertRaises(ReleaseError):
                    validate_binary(binary, "linux-arm64", "1.2.3")
            with self.assertRaises(ReleaseError):
                validate_binary(Path(temporary) / "missing", "linux-amd64", "1.2.3")

    def test_binary_validation_executes_matching_native_binary(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            binary = Path(temporary) / "repo-sandbox"
            binary.write_bytes(fake_elf(62))
            completed = mock.Mock(returncode=0, stdout="repo-sandbox 1.2.3\n")
            with mock.patch("release_lib.os.access", return_value=True):
                with mock.patch("release_lib.host_platform.machine", return_value="x86_64"):
                    with mock.patch("release_lib.subprocess.run", return_value=completed):
                        validate_binary(binary, "linux-amd64", "1.2.3")
                        completed.stdout = "repo-sandbox 9.9.9\n"
                        with self.assertRaises(ReleaseError):
                            validate_binary(binary, "linux-amd64", "1.2.3")

    def test_glibc_ceiling_is_enforced(self) -> None:
        accepted = mock.Mock(returncode=0, stdout="GLIBC_2.17 GLIBC_2.28", stderr="")
        rejected = mock.Mock(returncode=0, stdout="GLIBC_2.17 GLIBC_2.29", stderr="")
        with mock.patch("release_lib.subprocess.run", return_value=accepted):
            check_glibc(Path("repo-sandbox"))
        with mock.patch("release_lib.subprocess.run", return_value=rejected):
            with self.assertRaises(ReleaseError):
                check_glibc(Path("repo-sandbox"))

    def test_archive_is_reproducible_and_contains_only_binary(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            binary = root / "input"
            binary.write_bytes(fake_elf(62, b"payload"))
            first, second = root / "first.tar.gz", root / "second.tar.gz"
            create_archive(binary, first)
            create_archive(binary, second)
            self.assertEqual(first.read_bytes(), second.read_bytes())
            self.assertEqual(archive_binary(first), binary.read_bytes())
            with gzip.open(first, "rb") as compressed:
                with tarfile.open(fileobj=io.BytesIO(compressed.read())) as archive:
                    member = archive.getmembers()[0]
                    self.assertEqual(member.name, "repo-sandbox")
                    self.assertEqual(member.mode, 0o755)
            changed_header = bytearray(first.read_bytes())
            changed_header[4] = 1
            first.write_bytes(changed_header)
            with self.assertRaises(ReleaseError):
                archive_binary(first)

    def test_archive_rejects_extra_members(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            path = Path(temporary) / "bad.tar.gz"
            with tarfile.open(path, "w:gz") as archive:
                for name in ("repo-sandbox", "README.md"):
                    member = tarfile.TarInfo(name)
                    member.size = 1
                    archive.addfile(member, io.BytesIO(b"x"))
            with self.assertRaises(ReleaseError):
                archive_binary(path)

    def test_checksum_filename_and_digest_are_strict(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            archive = root / "a.tar.gz"
            archive.write_bytes(b"archive")
            checksum = root / "a.tar.gz.sha256"
            checksum.write_text(f"{sha256(archive)}  a.tar.gz\n", encoding="ascii")
            self.assertEqual(parse_checksum(checksum, "a.tar.gz"), sha256(archive))
            with self.assertRaises(ReleaseError):
                parse_checksum(checksum, "other.tar.gz")

    def test_release_set_rejects_tampering_and_extra_assets(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary) / "authority"
            release = Path(temporary) / "release"
            release.mkdir()
            self.authority(root)
            for platform in ("linux-amd64", "linux-arm64"):
                name = f"repo-sandbox-1.2.3-{platform}.tar.gz"
                archive = release / name
                archive.write_bytes(platform.encode("ascii"))
                (release / f"{name}.sha256").write_text(
                    f"{sha256(archive)}  {name}\n", encoding="ascii"
                )
            assemble("v1.2.3", release, root)
            self.assertTrue((release / "SHA256SUMS").is_file())

            (release / "repo-sandbox-1.2.3-linux-amd64.tar.gz").write_bytes(b"tampered")
            (release / "SHA256SUMS").unlink()
            with self.assertRaises(ReleaseError):
                assemble("v1.2.3", release, root)

            archive = release / "repo-sandbox-1.2.3-linux-amd64.tar.gz"
            checksum = release / f"{archive.name}.sha256"
            checksum.write_text(f"{sha256(archive)}  {archive.name}\n", encoding="ascii")
            (release / "unexpected.txt").write_text("unexpected", encoding="utf-8")
            with self.assertRaises(ReleaseError):
                assemble("v1.2.3", release, root)


if __name__ == "__main__":
    unittest.main()
