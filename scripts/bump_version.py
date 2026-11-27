#!/usr/bin/env python3
"""Bump the workspace version across all synced manifests and Cargo.lock.

Updates:
  - Cargo.toml ([workspace.package].version)
  - Cargo.lock (workspace companion-* packages)
  - plugin/plugin.json (version)
  - packages/npm/agy-companion/package.json (version + @agy-companion/* optionalDependencies)
  - packages/python/pyproject.toml ([project].version)

Usage:
  python3 -B scripts/bump_version.py                     # daily dev bump (X.Y.Z -> X.Y.(Z+1)-dev.0; X.Y.Z-dev.N -> X.Y.Z-dev.(N+1))
  python3 -B scripts/bump_version.py --release           # promote X.Y.Z-dev.N -> X.Y.Z (fails if already final)
  python3 -B scripts/bump_version.py --part patch        # bump explicit final patch version
  python3 -B scripts/bump_version.py --part minor        # bump explicit final minor version
  python3 -B scripts/bump_version.py --part major        # bump explicit final major version
  python3 -B scripts/bump_version.py --set 0.1.1-dev.0   # set explicit SemVer (X.Y.Z or X.Y.Z-dev.N)
"""

from __future__ import annotations

import argparse
import json
import pathlib
import re
import sys

sys.dont_write_bytecode = True

SCRIPTS_DIR = pathlib.Path(__file__).resolve().parent
if str(SCRIPTS_DIR) not in sys.path:
    sys.path.insert(0, str(SCRIPTS_DIR))

import versioning  # noqa: E402

WORKSPACE_CRATES = frozenset(
    {
        "companion-cli",
        "companion-core",
        "companion-providers",
        "companion-proxy",
        "companion-ui",
    }
)


def compute_next_version(
    current: str,
    *,
    part: str | None = None,
    release: bool = False,
    explicit: str | None = None,
) -> str:
    selected_modes = sum(bool(flag) for flag in (part is not None, release, explicit is not None))
    if selected_modes > 1:
        raise ValueError("options 'part', 'release', and 'explicit' are mutually exclusive")

    cur = versioning.parse_version(current)

    if explicit is not None:
        return str(versioning.parse_version(explicit))

    if release:
        if cur.is_final:
            raise ValueError(
                f"cannot promote {current!r} with --release: version is already a final release"
            )
        return f"{cur.major}.{cur.minor}.{cur.patch}"

    if part is not None:
        if part == "major":
            return f"{cur.major + 1}.0.0"
        if part == "minor":
            return f"{cur.major}.{cur.minor + 1}.0"
        if part == "patch":
            return f"{cur.major}.{cur.minor}.{cur.patch + 1}"
        raise ValueError(f"unsupported bump part {part!r} (expected 'patch', 'minor', or 'major')")

    if cur.is_dev:
        assert cur.dev is not None
        return f"{cur.major}.{cur.minor}.{cur.patch}-dev.{cur.dev + 1}"
    return f"{cur.major}.{cur.minor}.{cur.patch + 1}-dev.0"


def _read_toml_section_version(text: str, section: str) -> str:
    in_section = False
    sec_re = re.compile(r"^\s*\[" + re.escape(section) + r"\]\s*(?:#.*)?$")
    any_sec_re = re.compile(r"^\s*\[")
    ver_re = re.compile(r'^\s*version\s*=\s*"([^"]+)"\s*(?:#.*)?$')
    for line in text.splitlines():
        if sec_re.match(line):
            in_section = True
            continue
        if any_sec_re.match(line):
            in_section = False
            continue
        if in_section:
            m = ver_re.match(line)
            if m:
                return m.group(1)
    raise ValueError(f"could not locate [{section}].version")


def _replace_toml_section_version(text: str, section: str, new_version: str) -> str:
    in_section = False
    replaced = False
    sec_re = re.compile(r"^\s*\[" + re.escape(section) + r"\]\s*(?:#.*)?$")
    any_sec_re = re.compile(r"^\s*\[")
    ver_re = re.compile(r'^(\s*version\s*=\s*)"([^"]+)"(.*)$')
    out_lines: list[str] = []
    for line in text.splitlines(keepends=True):
        stripped = line.rstrip("\r\n")
        eol = line[len(stripped) :]
        if sec_re.match(stripped):
            in_section = True
            out_lines.append(line)
            continue
        if any_sec_re.match(stripped):
            in_section = False
            out_lines.append(line)
            continue
        if in_section and not replaced:
            m = ver_re.match(stripped)
            if m:
                out_lines.append(f'{m.group(1)}"{new_version}"{m.group(3)}{eol}')
                replaced = True
                continue
        out_lines.append(line)
    if not replaced:
        raise ValueError(f"could not replace [{section}].version")
    return "".join(out_lines)


def _update_cargo_lock(text: str, new_version: str) -> tuple[str, int]:
    pkg_header_re = re.compile(r"^\s*\[\[package\]\]\s*$")
    name_re = re.compile(r'^\s*name\s*=\s*"([^"]+)"\s*$')
    ver_re = re.compile(r'^(\s*version\s*=\s*)"([^"]+)"(.*)$')

    out_lines: list[str] = []
    in_workspace_pkg = False
    updated_count = 0
    for line in text.splitlines(keepends=True):
        stripped = line.rstrip("\r\n")
        eol = line[len(stripped) :]
        if pkg_header_re.match(stripped):
            in_workspace_pkg = False
            out_lines.append(line)
            continue
        m_name = name_re.match(stripped)
        if m_name:
            in_workspace_pkg = m_name.group(1) in WORKSPACE_CRATES
            out_lines.append(line)
            continue
        if in_workspace_pkg:
            m_ver = ver_re.match(stripped)
            if m_ver:
                out_lines.append(f'{m_ver.group(1)}"{new_version}"{m_ver.group(3)}{eol}')
                updated_count += 1
                in_workspace_pkg = False
                continue
        out_lines.append(line)
    return "".join(out_lines), updated_count


def bump_repo_version(
    repo_root: pathlib.Path,
    *,
    part: str | None = None,
    release: bool = False,
    explicit: str | None = None,
) -> tuple[str, str, list[str]]:
    cargo_toml = repo_root / "Cargo.toml"
    cargo_lock = repo_root / "Cargo.lock"
    plugin_json = repo_root / "plugin" / "plugin.json"
    npm_pkg_json = repo_root / "packages" / "npm" / "agy-companion" / "package.json"
    pyproject_toml = repo_root / "packages" / "python" / "pyproject.toml"

    cargo_text = cargo_toml.read_text(encoding="utf-8")
    current_ver = _read_toml_section_version(cargo_text, "workspace.package")
    new_ver = compute_next_version(current_ver, part=part, release=release, explicit=explicit)

    updated_files: list[str] = []

    # 1. Cargo.toml
    cargo_toml.write_text(
        _replace_toml_section_version(cargo_text, "workspace.package", new_ver),
        encoding="utf-8",
    )
    updated_files.append("Cargo.toml")

    # 2. Cargo.lock (if present)
    if cargo_lock.is_file():
        lock_text = cargo_lock.read_text(encoding="utf-8")
        new_lock_text, lock_count = _update_cargo_lock(lock_text, new_ver)
        if lock_count > 0:
            cargo_lock.write_text(new_lock_text, encoding="utf-8")
            updated_files.append("Cargo.lock")

    # 3. plugin/plugin.json
    plugin_data = json.loads(plugin_json.read_text(encoding="utf-8"))
    plugin_data["version"] = new_ver
    plugin_json.write_text(json.dumps(plugin_data, indent=2) + "\n", encoding="utf-8")
    updated_files.append("plugin/plugin.json")

    # 4. packages/npm/agy-companion/package.json
    npm_data = json.loads(npm_pkg_json.read_text(encoding="utf-8"))
    npm_data["version"] = new_ver
    opt_deps = npm_data.get("optionalDependencies")
    if isinstance(opt_deps, dict):
        for dep_name in list(opt_deps.keys()):
            if dep_name.startswith("@agy-companion/"):
                opt_deps[dep_name] = new_ver
    npm_pkg_json.write_text(json.dumps(npm_data, indent=2) + "\n", encoding="utf-8")
    updated_files.append("packages/npm/agy-companion/package.json")

    # 5. packages/python/pyproject.toml
    py_text = pyproject_toml.read_text(encoding="utf-8")
    pyproject_toml.write_text(
        _replace_toml_section_version(py_text, "project", new_ver),
        encoding="utf-8",
    )
    updated_files.append("packages/python/pyproject.toml")

    return current_ver, new_ver, updated_files


def parse_args(argv: list[str] | None = None) -> argparse.Namespace:
    parser = argparse.ArgumentParser(
        description="Bump the workspace version across all 4 synced manifests and Cargo.lock."
    )
    group = parser.add_mutually_exclusive_group()
    group.add_argument(
        "--release",
        action="store_true",
        default=False,
        help="Promote current X.Y.Z-dev.N prerelease to final X.Y.Z release.",
    )
    group.add_argument(
        "--part",
        choices=("patch", "minor", "major"),
        default=None,
        help="Bump an explicit final SemVer component.",
    )
    group.add_argument(
        "--set",
        dest="explicit",
        metavar="VERSION",
        help="Set an explicit MAJOR.MINOR.PATCH or MAJOR.MINOR.PATCH-dev.N version.",
    )
    parser.add_argument(
        "--repo-root",
        type=pathlib.Path,
        default=pathlib.Path(__file__).resolve().parent.parent,
        help="Repository root directory (default: parent of scripts/).",
    )
    return parser.parse_args(argv)


def main(argv: list[str] | None = None) -> int:
    args = parse_args(argv)
    try:
        old_ver, new_ver, updated = bump_repo_version(
            args.repo_root.resolve(),
            part=args.part,
            release=args.release,
            explicit=args.explicit,
        )
    except ValueError as exc:
        print(f"bump_version.py: {exc}", file=sys.stderr)
        return 1
    print(f"Bumped version: {old_ver} -> {new_ver} ({', '.join(updated)})")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
