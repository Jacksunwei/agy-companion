#!/usr/bin/env python3
"""Bump the workspace version across all synced manifests and Cargo.lock.

Updates:
  - Cargo.toml ([workspace.package].version)
  - Cargo.lock (workspace companion-* packages)
  - plugin/plugin.json (version)
  - packages/npm/agy-companion/package.json (version + @agy-companion/* optionalDependencies)
  - packages/python/pyproject.toml ([project].version)

Usage:
  python3 -B scripts/bump_version.py                 # bump patch (default)
  python3 -B scripts/bump_version.py --part minor    # bump minor
  python3 -B scripts/bump_version.py --part major    # bump major
  python3 -B scripts/bump_version.py --set 0.1.0     # set explicit SemVer
"""

from __future__ import annotations

import argparse
import json
import pathlib
import re
import sys

sys.dont_write_bytecode = True

SEMVER_RE = re.compile(r"^(\d+)\.(\d+)\.(\d+)$")

WORKSPACE_CRATES = frozenset(
    {
        "companion-cli",
        "companion-core",
        "companion-providers",
        "companion-proxy",
        "companion-ui",
    }
)


def parse_semver(version: str) -> tuple[int, int, int]:
    match = SEMVER_RE.match(version.strip())
    if not match:
        raise ValueError(f"invalid SemVer version {version!r} (expected MAJOR.MINOR.PATCH)")
    return int(match.group(1)), int(match.group(2)), int(match.group(3))


def compute_next_version(current: str, *, part: str | None = None, explicit: str | None = None) -> str:
    major, minor, patch = parse_semver(current)
    if explicit is not None:
        parse_semver(explicit)
        return explicit.strip()
    if part == "major":
        return f"{major + 1}.0.0"
    if part == "minor":
        return f"{major}.{minor + 1}.0"
    return f"{major}.{minor}.{patch + 1}"


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
    part: str = "patch",
    explicit: str | None = None,
) -> tuple[str, str, list[str]]:
    cargo_toml = repo_root / "Cargo.toml"
    cargo_lock = repo_root / "Cargo.lock"
    plugin_json = repo_root / "plugin" / "plugin.json"
    npm_pkg_json = repo_root / "packages" / "npm" / "agy-companion" / "package.json"
    pyproject_toml = repo_root / "packages" / "python" / "pyproject.toml"

    cargo_text = cargo_toml.read_text(encoding="utf-8")
    current_ver = _read_toml_section_version(cargo_text, "workspace.package")
    new_ver = compute_next_version(current_ver, part=part, explicit=explicit)

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
        "--part",
        choices=("patch", "minor", "major"),
        default="patch",
        help="SemVer component to bump (default: patch).",
    )
    group.add_argument(
        "--set",
        dest="explicit",
        metavar="X.Y.Z",
        help="Set an explicit MAJOR.MINOR.PATCH version.",
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
    old_ver, new_ver, updated = bump_repo_version(
        args.repo_root.resolve(),
        part=args.part,
        explicit=args.explicit,
    )
    print(f"Bumped version: {old_ver} -> {new_ver} ({', '.join(updated)})")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
