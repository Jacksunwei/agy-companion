#!/usr/bin/env python3
"""Release packaging helpers for agy-companion.

Subcommands:
  package-wheel  Extract the native binary from a maturin-built wheel, smoke-test
                 `--version`, build the deterministic GitHub Release archive
                 (`.tar.gz` on Unix, `.zip` on Windows), and write `<archive>.sha256`.
  checksums      Assemble and strictly validate `checksums.txt` across all 5 targets.
"""

from __future__ import annotations

import argparse
import gzip
import hashlib
import io
import os
import pathlib
import re
import stat
import subprocess
import sys
import tarfile
import zipfile

sys.dont_write_bytecode = True

BINARY_NAME = "agy-companion"

SUPPORTED_TARGETS: dict[str, tuple[str, str]] = {
    "darwin-arm64": ("agy-companion", "tar.gz"),
    "darwin-x64": ("agy-companion", "tar.gz"),
    "linux-x64": ("agy-companion", "tar.gz"),
    "linux-arm64": ("agy-companion", "tar.gz"),
    "win32-x64": ("agy-companion.exe", "zip"),
}

SEMVER_RE = re.compile(r"^\d+\.\d+\.\d+$")
SHA256_LINE_RE = re.compile(r"^([0-9a-f]{64})  ([^\s/\\]+)$")


def _validate_semver(version: str) -> None:
    if not SEMVER_RE.fullmatch(version):
        raise ValueError(f"invalid SemVer version {version!r} (expected X.Y.Z)")


def sha256_file(path: pathlib.Path) -> str:
    h = hashlib.sha256()
    with path.open("rb") as fh:
        for chunk in iter(lambda: fh.read(65536), b""):
            h.update(chunk)
    return h.hexdigest()


def extract_binary_from_wheel(
    wheel_path: pathlib.Path,
    dest_path: pathlib.Path,
    expected_exe_name: str,
    *,
    expected_version: str | None = None,
) -> pathlib.Path:
    """Extract `*.data/scripts/<expected_exe_name>` from a maturin bin wheel."""
    if expected_version is not None:
        _validate_semver(expected_version)

    entry_re = re.compile(r"^[^/\\]+\.data/scripts/" + re.escape(expected_exe_name) + r"$")
    with zipfile.ZipFile(wheel_path, "r") as zf:
        matches = [info for info in zf.infolist() if entry_re.fullmatch(info.filename)]
        if len(matches) != 1:
            found = [info.filename for info in matches]
            raise ValueError(
                f"expected exactly 1 binary entry matching *.data/scripts/{expected_exe_name} in "
                f"{wheel_path.name}, found {found!r}"
            )
        entry = matches[0]
        if stat.S_ISLNK(entry.external_attr >> 16):
            raise ValueError(
                f"refusing symlink entry {entry.filename!r} inside wheel {wheel_path.name}"
            )
        if expected_version is not None:
            expected_entry = f"agy_companion-{expected_version}.data/scripts/{expected_exe_name}"
            if entry.filename != expected_entry:
                raise ValueError(
                    f"unexpected wheel binary path {entry.filename!r} in {wheel_path.name} "
                    f"(expected {expected_entry!r})"
                )
        raw_bytes = zf.read(entry)

    if not raw_bytes:
        raise ValueError(f"extracted binary {entry.filename!r} from {wheel_path.name} is empty")

    dest_path.parent.mkdir(parents=True, exist_ok=True)
    dest_path.write_bytes(raw_bytes)
    if os.name != "nt":
        dest_path.chmod(0o755)
    return dest_path


def smoke_test_binary(binary_path: pathlib.Path, expected_version: str) -> str:
    """Run `<binary> --version` and assert it reports `agy-companion <expected_version>`."""
    proc = subprocess.run(
        [str(binary_path), "--version"],
        capture_output=True,
        text=True,
        encoding="utf-8",
        errors="replace",
        check=False,
    )
    if proc.returncode != 0:
        raise RuntimeError(
            f"{binary_path} --version failed (exit {proc.returncode}): {proc.stderr.strip()}"
        )
    actual = proc.stdout.strip()
    expected = f"{BINARY_NAME} {expected_version}"
    if actual != expected:
        raise RuntimeError(f"unexpected --version output: got {actual!r}, expected {expected!r}")
    return actual


def create_release_archive(
    binary_path: pathlib.Path,
    archive_path: pathlib.Path,
    exe_name: str,
    archive_ext: str,
) -> pathlib.Path:
    """Create a deterministic single-entry archive matching plugin/scripts/run-companion.sh."""
    raw_bytes = binary_path.read_bytes()
    if not raw_bytes:
        raise ValueError(f"binary is empty: {binary_path}")

    archive_path.parent.mkdir(parents=True, exist_ok=True)
    if archive_ext == "tar.gz":
        with (
            archive_path.open("wb") as raw_fh,
            gzip.GzipFile(filename="", mode="wb", fileobj=raw_fh, mtime=0) as gz,
            tarfile.open(fileobj=gz, mode="w", format=tarfile.PAX_FORMAT) as tf,
        ):
            info = tarfile.TarInfo(name=exe_name)
            info.size = len(raw_bytes)
            info.mode = 0o755
            info.mtime = 0
            info.uid = 0
            info.gid = 0
            info.uname = ""
            info.gname = ""
            tf.addfile(info, io.BytesIO(raw_bytes))
    elif archive_ext == "zip":
        with zipfile.ZipFile(archive_path, "w", compression=zipfile.ZIP_DEFLATED) as zf:
            zi = zipfile.ZipInfo(filename=exe_name, date_time=(1980, 1, 1, 0, 0, 0))
            zi.compress_type = zipfile.ZIP_DEFLATED
            zi.external_attr = (stat.S_IFREG | 0o755) << 16
            zf.writestr(zi, raw_bytes)
    else:
        raise ValueError(f"unsupported archive extension: {archive_ext!r}")
    return archive_path


def package_wheel(
    *,
    wheels_dir: pathlib.Path,
    target: str,
    version: str,
    bin_out_dir: pathlib.Path,
    release_out_dir: pathlib.Path,
    skip_smoke_test: bool = False,
) -> tuple[pathlib.Path, pathlib.Path, pathlib.Path]:
    _validate_semver(version)
    if target not in SUPPORTED_TARGETS:
        raise ValueError(
            f"unsupported target {target!r} (expected one of {sorted(SUPPORTED_TARGETS)})"
        )
    exe_name, archive_ext = SUPPORTED_TARGETS[target]

    wheels = sorted(wheels_dir.glob("*.whl"))
    if len(wheels) != 1:
        raise ValueError(
            f"expected exactly 1 .whl file in {wheels_dir}, found {[w.name for w in wheels]}"
        )
    wheel_path = wheels[0]

    binary_path = extract_binary_from_wheel(
        wheel_path,
        bin_out_dir / target / exe_name,
        exe_name,
        expected_version=version,
    )
    if not skip_smoke_test:
        smoke_test_binary(binary_path, version)

    archive_name = f"{BINARY_NAME}-v{version}-{target}.{archive_ext}"
    archive_path = create_release_archive(
        binary_path,
        release_out_dir / archive_name,
        exe_name,
        archive_ext,
    )
    digest = sha256_file(archive_path)
    sha_path = release_out_dir / f"{archive_name}.sha256"
    sha_path.write_text(f"{digest}  {archive_name}\n", encoding="utf-8")
    return binary_path, archive_path, sha_path


def assemble_checksums(
    *,
    release_dir: pathlib.Path,
    version: str,
    out_file: pathlib.Path,
) -> pathlib.Path:
    """Merge per-target `.sha256` files into `checksums.txt` and verify every archive."""
    _validate_semver(version)
    expected_archives = {
        f"{BINARY_NAME}-v{version}-{target}.{ext}"
        for target, (_exe, ext) in SUPPORTED_TARGETS.items()
    }

    entries: dict[str, str] = {}
    for sha_file in sorted(release_dir.glob("*.sha256")):
        if sha_file.is_symlink():
            raise ValueError(f"refusing symlink checksum file: {sha_file.name}")
        lines = [
            line.strip()
            for line in sha_file.read_text(encoding="utf-8").splitlines()
            if line.strip()
        ]
        if len(lines) != 1:
            raise ValueError(f"{sha_file.name}: expected exactly 1 checksum line, got {len(lines)}")
        match = SHA256_LINE_RE.match(lines[0])
        if not match:
            raise ValueError(f"{sha_file.name}: invalid checksum line format: {lines[0]!r}")
        digest, fname = match.group(1), match.group(2)
        expected_sha_name = f"{fname}.sha256"
        if sha_file.name != expected_sha_name:
            raise ValueError(
                f"checksum filename {sha_file.name!r} does not match referenced archive {fname!r}"
            )
        if fname in entries:
            raise ValueError(f"duplicate checksum entry for {fname!r}")
        archive_path = release_dir / fname
        if archive_path.is_symlink():
            raise ValueError(f"refusing symlink release archive: {fname}")
        if not archive_path.is_file():
            raise ValueError(f"checksum references missing archive: {fname}")
        actual = sha256_file(archive_path)
        if actual != digest:
            raise ValueError(
                f"SHA-256 mismatch for {fname}: file has {actual}, {sha_file.name} has {digest}"
            )
        entries[fname] = digest

    missing = sorted(expected_archives - set(entries.keys()))
    extra = sorted(set(entries.keys()) - expected_archives)
    if missing:
        raise ValueError(f"missing expected release archive(s): {missing}")
    if extra:
        raise ValueError(f"unexpected extra release archive(s): {extra}")

    disk_archives = [
        p
        for pattern in ("*.tar.gz", "*.zip")
        for p in sorted(release_dir.glob(pattern))
    ]
    for p in disk_archives:
        if p.is_symlink():
            raise ValueError(f"refusing symlink release archive: {p.name}")
    stray = sorted({p.name for p in disk_archives} - expected_archives)
    if stray:
        raise ValueError(f"unexpected stray release archive(s) in {release_dir}: {stray}")

    out_file.parent.mkdir(parents=True, exist_ok=True)
    rendered = "".join(f"{entries[name]}  {name}\n" for name in sorted(entries))
    out_file.write_text(rendered, encoding="utf-8")
    return out_file


def parse_args(argv: list[str] | None = None) -> argparse.Namespace:
    parser = argparse.ArgumentParser(description="Release packaging helpers for agy-companion.")
    sub = parser.add_subparsers(dest="command", required=True)

    p_pkg = sub.add_parser("package-wheel", help="Extract binary from wheel and build release archive.")
    p_pkg.add_argument("--wheels-dir", type=pathlib.Path, required=True)
    p_pkg.add_argument("--target", choices=sorted(SUPPORTED_TARGETS), required=True)
    p_pkg.add_argument("--version", required=True)
    p_pkg.add_argument("--bin-out-dir", type=pathlib.Path, required=True)
    p_pkg.add_argument("--release-out-dir", type=pathlib.Path, required=True)
    p_pkg.add_argument("--skip-smoke-test", action="store_true")

    p_cs = sub.add_parser("checksums", help="Assemble and verify checksums.txt across all 5 targets.")
    p_cs.add_argument("--release-dir", type=pathlib.Path, required=True)
    p_cs.add_argument("--version", required=True)
    p_cs.add_argument("--out-file", type=pathlib.Path, required=True)

    return parser.parse_args(argv)


def main(argv: list[str] | None = None) -> int:
    args = parse_args(argv)
    if args.command == "package-wheel":
        binary_path, archive_path, sha_path = package_wheel(
            wheels_dir=args.wheels_dir,
            target=args.target,
            version=args.version,
            bin_out_dir=args.bin_out_dir,
            release_out_dir=args.release_out_dir,
            skip_smoke_test=args.skip_smoke_test,
        )
        print(
            f"Packaged {args.target}: binary={binary_path} archive={archive_path} sha256={sha_path}"
        )
        return 0
    if args.command == "checksums":
        out_file = assemble_checksums(
            release_dir=args.release_dir,
            version=args.version,
            out_file=args.out_file,
        )
        print(f"Verified and wrote {out_file} ({len(SUPPORTED_TARGETS)} archives)")
        return 0
    return 1


if __name__ == "__main__":
    raise SystemExit(main())
