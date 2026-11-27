#!/usr/bin/env python3
"""Unit tests for scripts/package_release_asset.py."""

from __future__ import annotations

import os
import pathlib
import shlex
import stat
import subprocess
import sys
import tarfile
import tempfile
import unittest
import zipfile

sys.dont_write_bytecode = True

SCRIPTS_DIR = pathlib.Path(__file__).resolve().parent
REPO_ROOT = SCRIPTS_DIR.parent
sys.path.insert(0, str(SCRIPTS_DIR))

import package_release_asset as pra  # noqa: E402


def _make_fake_wheel(
    wheels_dir: pathlib.Path,
    wheel_name: str,
    version: str,
    exe_name: str,
    payload: bytes,
) -> pathlib.Path:
    wheels_dir.mkdir(parents=True, exist_ok=True)
    wheel_path = wheels_dir / wheel_name
    with zipfile.ZipFile(wheel_path, "w") as zf:
        zf.writestr("agy_companion/__init__.py", "__all__ = []\n")
        zf.writestr(f"agy_companion-{version}.data/scripts/{exe_name}", payload)
    return wheel_path


class PackageReleaseAssetTests(unittest.TestCase):
    def test_package_unix_wheel_and_launcher_verification(self) -> None:
        version = "0.0.12"
        payload = f"#!/bin/sh\necho 'agy-companion {version}'\n".encode("utf-8")

        with tempfile.TemporaryDirectory() as td:
            root = pathlib.Path(td)
            wheels_dir = root / "wheels"
            bin_dir = root / "bin"
            release_dir = root / "release"
            _make_fake_wheel(
                wheels_dir,
                f"agy_companion-{version}-py3-none-linux_x86_64.whl",
                version,
                "agy-companion",
                payload,
            )

            binary_path, archive_path, sha_path = pra.package_wheel(
                wheels_dir=wheels_dir,
                target="linux-x64",
                version=version,
                bin_out_dir=bin_dir,
                release_out_dir=release_dir,
                skip_smoke_test=(os.name == "nt"),
            )

            self.assertEqual(binary_path, bin_dir / "linux-x64" / "agy-companion")
            self.assertEqual(archive_path.name, f"agy-companion-v{version}-linux-x64.tar.gz")
            self.assertEqual(sha_path.name, f"agy-companion-v{version}-linux-x64.tar.gz.sha256")

            with tarfile.open(archive_path, "r:gz") as tf:
                names = tf.getnames()
                self.assertEqual(names, ["agy-companion"])
                member = tf.getmember("agy-companion")
                self.assertEqual(member.mode, 0o755)
                self.assertEqual(tf.extractfile(member).read(), payload)  # type: ignore[union-attr]

            # On POSIX, verify plugin/scripts/run-companion.sh accepts the archive & checksums format.
            if os.name != "nt":
                checksums_file = release_dir / "checksums.txt"
                checksums_file.write_text(sha_path.read_text(encoding="utf-8"), encoding="utf-8")
                extract_dir = root / "extract"
                launcher = REPO_ROOT / "plugin" / "scripts" / "run-companion.sh"
                script = (
                    f"source {shlex.quote(str(launcher))}\n"
                    f"verify_sha256 {shlex.quote(str(archive_path))} "
                    f"{shlex.quote(str(checksums_file))} {shlex.quote(archive_path.name)}\n"
                    f"extracted=$(extract_verified_binary {shlex.quote(str(archive_path))} "
                    f"{shlex.quote(str(extract_dir))})\n"
                    f'test -f "$extracted"\n'
                )
                proc = subprocess.run(
                    ["bash", "-euo", "pipefail", "-c", script],
                    capture_output=True,
                    text=True,
                    check=False,
                )
                self.assertEqual(proc.returncode, 0, msg=proc.stderr)

    def test_package_windows_wheel_and_assemble_all_five_checksums(self) -> None:
        version = "0.0.12"
        with tempfile.TemporaryDirectory() as td:
            root = pathlib.Path(td)
            release_dir = root / "release"
            bin_dir = root / "bin"

            for target, (exe_name, _ext) in pra.SUPPORTED_TARGETS.items():
                wdir = root / f"wheels-{target}"
                _make_fake_wheel(
                    wdir,
                    f"agy_companion-{version}-py3-none-{target.replace('-', '_')}.whl",
                    version,
                    exe_name,
                    f"fake-binary-{target}".encode("utf-8"),
                )
                pra.package_wheel(
                    wheels_dir=wdir,
                    target=target,
                    version=version,
                    bin_out_dir=bin_dir,
                    release_out_dir=release_dir,
                    skip_smoke_test=True,
                )

            win_zip = release_dir / f"agy-companion-v{version}-win32-x64.zip"
            with zipfile.ZipFile(win_zip, "r") as zf:
                self.assertEqual(zf.namelist(), ["agy-companion.exe"])

            out_file = release_dir / "checksums.txt"
            pra.assemble_checksums(release_dir=release_dir, version=version, out_file=out_file)
            lines = out_file.read_text(encoding="utf-8").splitlines()
            self.assertEqual(len(lines), 5)
            for line in lines:
                self.assertRegex(line, pra.SHA256_LINE_RE)

    def test_assemble_checksums_rejects_missing_target_and_tampered_hash(self) -> None:
        version = "0.0.12"
        with tempfile.TemporaryDirectory() as td:
            root = pathlib.Path(td)
            release_dir = root / "release"
            wdir = root / "wheels"
            _make_fake_wheel(
                wdir,
                f"agy_companion-{version}-py3-none-linux_x86_64.whl",
                version,
                "agy-companion",
                b"fake",
            )
            _, archive_path, sha_path = pra.package_wheel(
                wheels_dir=wdir,
                target="linux-x64",
                version=version,
                bin_out_dir=root / "bin",
                release_out_dir=release_dir,
                skip_smoke_test=True,
            )

            with self.assertRaisesRegex(ValueError, "missing expected release archive"):
                pra.assemble_checksums(
                    release_dir=release_dir,
                    version=version,
                    out_file=release_dir / "checksums.txt",
                )

            sha_path.write_text(f"{'0' * 64}  {archive_path.name}\n", encoding="utf-8")
            with self.assertRaisesRegex(ValueError, "SHA-256 mismatch"):
                pra.assemble_checksums(
                    release_dir=release_dir,
                    version=version,
                    out_file=release_dir / "checksums.txt",
                )

    def test_archive_determinism_tar_gz_and_zip(self) -> None:
        payload = b"deterministic-binary-payload-1234567890"
        with tempfile.TemporaryDirectory() as td:
            root = pathlib.Path(td)
            bin_path = root / "agy-companion"
            bin_path.write_bytes(payload)

            tar_a = root / "run1" / "agy-companion-v0.0.12-linux-x64.tar.gz"
            tar_b = root / "run2" / "other-dir-name.tar.gz"
            pra.create_release_archive(bin_path, tar_a, "agy-companion", "tar.gz")
            pra.create_release_archive(bin_path, tar_b, "agy-companion", "tar.gz")

            self.assertEqual(tar_a.read_bytes(), tar_b.read_bytes())
            self.assertEqual(pra.sha256_file(tar_a), pra.sha256_file(tar_b))

            zip_a = root / "run1" / "agy-companion-v0.0.12-win32-x64.zip"
            zip_b = root / "run2" / "other-dir-name.zip"
            pra.create_release_archive(bin_path, zip_a, "agy-companion.exe", "zip")
            pra.create_release_archive(bin_path, zip_b, "agy-companion.exe", "zip")

            self.assertEqual(zip_a.read_bytes(), zip_b.read_bytes())
            self.assertEqual(pra.sha256_file(zip_a), pra.sha256_file(zip_b))

    def test_invalid_semver_rejected_in_package_wheel_and_checksums(self) -> None:
        bad_versions = ("v0.0.12", "0.0.12-rc.1", "../0.0.12", "0.0", "")
        with tempfile.TemporaryDirectory() as td:
            root = pathlib.Path(td)
            wdir = root / "wheels"
            _make_fake_wheel(
                wdir,
                "agy_companion-0.0.12-py3-none-linux_x86_64.whl",
                "0.0.12",
                "agy-companion",
                b"fake",
            )
            for bad in bad_versions:
                with self.subTest(fn="package_wheel", version=bad):
                    with self.assertRaisesRegex(ValueError, "invalid SemVer"):
                        pra.package_wheel(
                            wheels_dir=wdir,
                            target="linux-x64",
                            version=bad,
                            bin_out_dir=root / "bin",
                            release_out_dir=root / "release",
                            skip_smoke_test=True,
                        )
                with self.subTest(fn="assemble_checksums", version=bad):
                    with self.assertRaisesRegex(ValueError, "invalid SemVer"):
                        pra.assemble_checksums(
                            release_dir=root / "release",
                            version=bad,
                            out_file=root / "release" / "checksums.txt",
                        )

    def test_wheel_extraction_edge_cases_and_security(self) -> None:
        version = "0.0.12"
        with tempfile.TemporaryDirectory() as td:
            root = pathlib.Path(td)

            # 0 wheels
            empty_dir = root / "empty_wheels"
            empty_dir.mkdir()
            with self.assertRaisesRegex(ValueError, "expected exactly 1 .whl"):
                pra.package_wheel(
                    wheels_dir=empty_dir,
                    target="linux-x64",
                    version=version,
                    bin_out_dir=root / "bin",
                    release_out_dir=root / "release",
                    skip_smoke_test=True,
                )

            # 2 wheels
            two_dir = root / "two_wheels"
            _make_fake_wheel(
                two_dir, "agy_companion-0.0.12-py3-none-a.whl", version, "agy-companion", b"a"
            )
            _make_fake_wheel(
                two_dir, "agy_companion-0.0.12-py3-none-b.whl", version, "agy-companion", b"b"
            )
            with self.assertRaisesRegex(ValueError, "expected exactly 1 .whl"):
                pra.package_wheel(
                    wheels_dir=two_dir,
                    target="linux-x64",
                    version=version,
                    bin_out_dir=root / "bin",
                    release_out_dir=root / "release",
                    skip_smoke_test=True,
                )

            # Missing binary entry
            missing_whl = root / "missing.whl"
            with zipfile.ZipFile(missing_whl, "w") as zf:
                zf.writestr("agy_companion/__init__.py", "__all__ = []\n")
            with self.assertRaisesRegex(ValueError, "expected exactly 1 binary entry"):
                pra.extract_binary_from_wheel(
                    missing_whl, root / "out" / "bin", "agy-companion", expected_version=version
                )

            # Empty binary entry
            empty_bin_dir = root / "empty_bin_wheels"
            _make_fake_wheel(
                empty_bin_dir,
                "agy_companion-0.0.12-py3-none-linux_x86_64.whl",
                version,
                "agy-companion",
                b"",
            )
            with self.assertRaisesRegex(ValueError, "is empty"):
                pra.package_wheel(
                    wheels_dir=empty_bin_dir,
                    target="linux-x64",
                    version=version,
                    bin_out_dir=root / "bin",
                    release_out_dir=root / "release",
                    skip_smoke_test=True,
                )

            # Wrong wheel version prefix
            wrong_ver_dir = root / "wrong_ver_wheels"
            _make_fake_wheel(
                wrong_ver_dir,
                "agy_companion-0.0.12-py3-none-linux_x86_64.whl",
                "9.9.9",
                "agy-companion",
                b"fake",
            )
            with self.assertRaisesRegex(ValueError, "agy_companion-0.0.12.data/scripts/agy-companion"):
                pra.package_wheel(
                    wheels_dir=wrong_ver_dir,
                    target="linux-x64",
                    version=version,
                    bin_out_dir=root / "bin",
                    release_out_dir=root / "release",
                    skip_smoke_test=True,
                )

            # Nested path / traversal prefix must not match
            nested_whl = root / "nested.whl"
            with zipfile.ZipFile(nested_whl, "w") as zf:
                zf.writestr(
                    f"nested/agy_companion-{version}.data/scripts/agy-companion",
                    b"fake",
                )
            with self.assertRaisesRegex(ValueError, "expected exactly 1 binary entry"):
                pra.extract_binary_from_wheel(
                    nested_whl, root / "out" / "bin", "agy-companion", expected_version=version
                )

            # Zip symlink entry must be rejected
            symlink_whl = root / "symlink.whl"
            with zipfile.ZipFile(symlink_whl, "w") as zf:
                zi = zipfile.ZipInfo(f"agy_companion-{version}.data/scripts/agy-companion")
                zi.external_attr = (stat.S_IFLNK | 0o777) << 16
                zf.writestr(zi, b"/etc/passwd")
            with self.assertRaisesRegex(ValueError, "symlink"):
                pra.extract_binary_from_wheel(
                    symlink_whl, root / "out" / "bin", "agy-companion", expected_version=version
                )

    def test_smoke_test_binary_failure_paths(self) -> None:
        if os.name == "nt":
            self.skipTest("POSIX shell script fixture only")
        with tempfile.TemporaryDirectory() as td:
            root = pathlib.Path(td)

            fail_bin = root / "fail-bin"
            fail_bin.write_text("#!/bin/sh\necho 'boom' >&2\nexit 2\n", encoding="utf-8")
            fail_bin.chmod(0o755)
            with self.assertRaisesRegex(RuntimeError, "exit 2"):
                pra.smoke_test_binary(fail_bin, "0.0.12")

            wrong_ver = root / "wrong-ver"
            wrong_ver.write_text("#!/bin/sh\necho 'agy-companion 0.0.11'\n", encoding="utf-8")
            wrong_ver.chmod(0o755)
            with self.assertRaisesRegex(RuntimeError, "unexpected --version output"):
                pra.smoke_test_binary(wrong_ver, "0.0.12")

    def test_assemble_checksums_negative_and_stray_archive_cases(self) -> None:
        version = "0.0.12"

        def _populate_all_five(base: pathlib.Path) -> pathlib.Path:
            rdir = base / "release"
            bdir = base / "bin"
            for target, (exe_name, _ext) in pra.SUPPORTED_TARGETS.items():
                wdir = base / f"wheels-{target}"
                _make_fake_wheel(
                    wdir,
                    f"agy_companion-{version}-py3-none-{target.replace('-', '_')}.whl",
                    version,
                    exe_name,
                    f"fake-{target}".encode("utf-8"),
                )
                pra.package_wheel(
                    wheels_dir=wdir,
                    target=target,
                    version=version,
                    bin_out_dir=bdir,
                    release_out_dir=rdir,
                    skip_smoke_test=True,
                )
            return rdir

        # Stray unhashed .tar.gz
        with tempfile.TemporaryDirectory() as td:
            rdir = _populate_all_five(pathlib.Path(td))
            (rdir / "stray-unhashed.tar.gz").write_bytes(b"stray")
            with self.assertRaisesRegex(ValueError, "stray|unexpected"):
                pra.assemble_checksums(
                    release_dir=rdir, version=version, out_file=rdir / "checksums.txt"
                )

        # Stray unhashed .zip
        with tempfile.TemporaryDirectory() as td:
            rdir = _populate_all_five(pathlib.Path(td))
            (rdir / "stray-unhashed.zip").write_bytes(b"stray")
            with self.assertRaisesRegex(ValueError, "stray|unexpected"):
                pra.assemble_checksums(
                    release_dir=rdir, version=version, out_file=rdir / "checksums.txt"
                )

        # Mismatched .sha256 filename
        with tempfile.TemporaryDirectory() as td:
            rdir = _populate_all_five(pathlib.Path(td))
            orig = rdir / f"agy-companion-v{version}-linux-x64.tar.gz.sha256"
            renamed = rdir / "wrong-name.tar.gz.sha256"
            orig.rename(renamed)
            with self.assertRaisesRegex(ValueError, "does not match"):
                pra.assemble_checksums(
                    release_dir=rdir, version=version, out_file=rdir / "checksums.txt"
                )

        # Extra .sha256 for unknown archive
        with tempfile.TemporaryDirectory() as td:
            rdir = _populate_all_five(pathlib.Path(td))
            extra_arc = rdir / f"agy-companion-v{version}-freebsd-x64.tar.gz"
            extra_arc.write_bytes(b"extra")
            digest = pra.sha256_file(extra_arc)
            (rdir / f"{extra_arc.name}.sha256").write_text(
                f"{digest}  {extra_arc.name}\n", encoding="utf-8"
            )
            with self.assertRaisesRegex(ValueError, "unexpected extra release archive"):
                pra.assemble_checksums(
                    release_dir=rdir, version=version, out_file=rdir / "checksums.txt"
                )

        # Malformed checksum line
        with tempfile.TemporaryDirectory() as td:
            rdir = _populate_all_five(pathlib.Path(td))
            sha_file = rdir / f"agy-companion-v{version}-linux-x64.tar.gz.sha256"
            sha_file.write_text("not-a-valid-sha256-line\n", encoding="utf-8")
            with self.assertRaisesRegex(ValueError, "invalid checksum line format"):
                pra.assemble_checksums(
                    release_dir=rdir, version=version, out_file=rdir / "checksums.txt"
                )

        # Multi-line .sha256
        with tempfile.TemporaryDirectory() as td:
            rdir = _populate_all_five(pathlib.Path(td))
            sha_file = rdir / f"agy-companion-v{version}-linux-x64.tar.gz.sha256"
            line = sha_file.read_text(encoding="utf-8").strip()
            sha_file.write_text(f"{line}\n{line}\n", encoding="utf-8")
            with self.assertRaisesRegex(ValueError, "expected exactly 1 checksum line"):
                pra.assemble_checksums(
                    release_dir=rdir, version=version, out_file=rdir / "checksums.txt"
                )

        # Missing referenced archive
        with tempfile.TemporaryDirectory() as td:
            rdir = _populate_all_five(pathlib.Path(td))
            (rdir / f"agy-companion-v{version}-linux-x64.tar.gz").unlink()
            with self.assertRaisesRegex(ValueError, "missing archive"):
                pra.assemble_checksums(
                    release_dir=rdir, version=version, out_file=rdir / "checksums.txt"
                )

        # Symlinked archive rejected (POSIX)
        if os.name != "nt":
            with tempfile.TemporaryDirectory() as td:
                base = pathlib.Path(td)
                rdir = _populate_all_five(base)
                arc = rdir / f"agy-companion-v{version}-linux-x64.tar.gz"
                real_copy = base / "real.tar.gz"
                real_copy.write_bytes(arc.read_bytes())
                arc.unlink()
                arc.symlink_to(real_copy)
                with self.assertRaisesRegex(ValueError, "symlink"):
                    pra.assemble_checksums(
                        release_dir=rdir, version=version, out_file=rdir / "checksums.txt"
                    )

    def test_cli_main_subcommands(self) -> None:
        version = "0.0.12"
        with tempfile.TemporaryDirectory() as td:
            root = pathlib.Path(td)
            release_dir = root / "release"
            bin_dir = root / "bin"
            for target, (exe_name, _ext) in pra.SUPPORTED_TARGETS.items():
                wdir = root / f"wheels-{target}"
                _make_fake_wheel(
                    wdir,
                    f"agy_companion-{version}-py3-none-{target.replace('-', '_')}.whl",
                    version,
                    exe_name,
                    f"cli-binary-{target}".encode("utf-8"),
                )
                rc = pra.main(
                    [
                        "package-wheel",
                        "--wheels-dir",
                        str(wdir),
                        "--target",
                        target,
                        "--version",
                        version,
                        "--bin-out-dir",
                        str(bin_dir),
                        "--release-out-dir",
                        str(release_dir),
                        "--skip-smoke-test",
                    ]
                )
                self.assertEqual(rc, 0)

            out_file = release_dir / "checksums.txt"
            rc = pra.main(
                [
                    "checksums",
                    "--release-dir",
                    str(release_dir),
                    "--version",
                    version,
                    "--out-file",
                    str(out_file),
                ]
            )
            self.assertEqual(rc, 0)
            self.assertEqual(len(out_file.read_text(encoding="utf-8").splitlines()), 5)


if __name__ == "__main__":
    unittest.main()
