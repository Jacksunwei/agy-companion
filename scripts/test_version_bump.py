#!/usr/bin/env python3
"""Unit tests for scripts/bump_version.py and check.py PR version-bump enforcement."""

from __future__ import annotations

import json
import os
import pathlib
import subprocess
import sys
import tempfile
import unittest

sys.dont_write_bytecode = True

SCRIPTS_DIR = pathlib.Path(__file__).resolve().parent
if str(SCRIPTS_DIR) not in sys.path:
    sys.path.insert(0, str(SCRIPTS_DIR))

import bump_version  # noqa: E402
import check  # noqa: E402


def _write_minimal_repo(root: pathlib.Path, version: str = "0.0.3") -> None:
    (root / "Cargo.toml").write_text(
        f'[workspace]\nmembers = []\n\n[workspace.package]\nversion = "{version}"\n',
        encoding="utf-8",
    )
    (root / "Cargo.lock").write_text(
        "# Auto-generated\n"
        "version = 4\n\n"
        '[[package]]\nname = "companion-cli"\nversion = "' + version + '"\n\n'
        '[[package]]\nname = "companion-core"\nversion = "' + version + '"\n\n'
        '[[package]]\nname = "serde"\nversion = "1.0.219"\n',
        encoding="utf-8",
    )
    (root / "plugin").mkdir(parents=True, exist_ok=True)
    (root / "plugin" / "plugin.json").write_text(
        json.dumps({"name": "agy-companion", "version": version}, indent=2) + "\n",
        encoding="utf-8",
    )
    npm_dir = root / "packages" / "npm" / "agy-companion"
    npm_dir.mkdir(parents=True, exist_ok=True)
    (npm_dir / "package.json").write_text(
        json.dumps(
            {
                "name": "agy-companion",
                "version": version,
                "optionalDependencies": {
                    "@agy-companion/darwin-arm64": version,
                    "@agy-companion/darwin-x64": version,
                    "@agy-companion/linux-x64": version,
                    "@agy-companion/linux-arm64": version,
                    "@agy-companion/win32-x64": version,
                },
            },
            indent=2,
        )
        + "\n",
        encoding="utf-8",
    )
    py_dir = root / "packages" / "python"
    py_dir.mkdir(parents=True, exist_ok=True)
    (py_dir / "pyproject.toml").write_text(
        f'[project]\nname = "agy-companion"\nversion = "{version}"\n',
        encoding="utf-8",
    )


def _git(cwd: pathlib.Path, *args: str) -> str:
    proc = subprocess.run(
        ["git", *args],
        cwd=str(cwd),
        capture_output=True,
        text=True,
        check=True,
    )
    return proc.stdout.strip()


def _init_git_repo_with_origin_main(root: pathlib.Path, version: str = "0.0.3") -> None:
    _write_minimal_repo(root, version=version)
    _git(root, "init", "-b", "main")
    _git(root, "config", "user.email", "test@example.com")
    _git(root, "config", "user.name", "Test")
    _git(root, "add", ".")
    _git(root, "commit", "-m", "chore: initial commit")
    head_sha = _git(root, "rev-parse", "HEAD")
    _git(root, "update-ref", "refs/remotes/origin/main", head_sha)


class TestBumpVersion(unittest.TestCase):
    def test_patch_minor_major_and_explicit_bump(self) -> None:
        with tempfile.TemporaryDirectory() as td:
            root = pathlib.Path(td)
            _write_minimal_repo(root, version="0.0.3")

            old, new, updated = bump_version.bump_repo_version(root, part="patch")
            self.assertEqual((old, new), ("0.0.3", "0.0.4"))
            self.assertEqual(
                updated,
                [
                    "Cargo.toml",
                    "Cargo.lock",
                    "plugin/plugin.json",
                    "packages/npm/agy-companion/package.json",
                    "packages/python/pyproject.toml",
                ],
            )
            self.assertIn('version = "0.0.4"', (root / "Cargo.toml").read_text(encoding="utf-8"))
            lock_text = (root / "Cargo.lock").read_text(encoding="utf-8")
            self.assertEqual(lock_text.count('version = "0.0.4"'), 2)
            self.assertIn('version = "1.0.219"', lock_text)
            self.assertEqual(
                json.loads((root / "plugin" / "plugin.json").read_text(encoding="utf-8"))["version"],
                "0.0.4",
            )
            npm_data = json.loads(
                (root / "packages" / "npm" / "agy-companion" / "package.json").read_text(encoding="utf-8")
            )
            self.assertEqual(npm_data["version"], "0.0.4")
            for dep_ver in npm_data["optionalDependencies"].values():
                self.assertEqual(dep_ver, "0.0.4")
            self.assertIn(
                'version = "0.0.4"',
                (root / "packages" / "python" / "pyproject.toml").read_text(encoding="utf-8"),
            )

            _, minor_ver, _ = bump_version.bump_repo_version(root, part="minor")
            self.assertEqual(minor_ver, "0.1.0")

            _, major_ver, _ = bump_version.bump_repo_version(root, part="major")
            self.assertEqual(major_ver, "1.0.0")

            _, explicit_ver, _ = bump_version.bump_repo_version(root, explicit="2.3.4")
            self.assertEqual(explicit_ver, "2.3.4")

    def test_invalid_semver_rejected(self) -> None:
        with self.assertRaises(ValueError):
            bump_version.parse_semver("0.0.3-alpha")
        with self.assertRaises(ValueError):
            bump_version.parse_semver("0.0")


class TestCheckPrVersionBump(unittest.TestCase):
    def _clean_ci_env(self) -> dict[str, str | None]:
        saved: dict[str, str | None] = {}
        for key in ("GITHUB_EVENT_NAME", "GITHUB_BASE_REF"):
            saved[key] = os.environ.pop(key, None)
        return saved

    def _restore_ci_env(self, saved: dict[str, str | None]) -> None:
        for key, val in saved.items():
            if val is None:
                os.environ.pop(key, None)
            else:
                os.environ[key] = val

    def test_skips_on_clean_main(self) -> None:
        saved = self._clean_ci_env()
        try:
            with tempfile.TemporaryDirectory() as td:
                root = pathlib.Path(td)
                _init_git_repo_with_origin_main(root, version="0.0.3")
                runner = check.CheckRunner(root, verify_ui_build=False, with_e2e=False)
                err = runner._check_pr_version_bump("0.0.3")
                self.assertIsNone(err)
        finally:
            self._restore_ci_env(saved)

    def test_fails_on_feature_branch_with_commit_without_bump(self) -> None:
        saved = self._clean_ci_env()
        try:
            with tempfile.TemporaryDirectory() as td:
                root = pathlib.Path(td)
                _init_git_repo_with_origin_main(root, version="0.0.3")
                _git(root, "checkout", "-b", "feat/no-bump")
                (root / "notes.txt").write_text("hello\n", encoding="utf-8")
                _git(root, "add", "notes.txt")
                _git(root, "commit", "-m", "feat: add notes")

                runner = check.CheckRunner(root, verify_ui_build=False, with_e2e=False)
                err = runner._check_pr_version_bump("0.0.3")
                self.assertIsNotNone(err)
                assert err is not None
                self.assertIn("PR version bump required", err)
        finally:
            self._restore_ci_env(saved)

    def test_fails_on_dirty_feature_branch_without_bump(self) -> None:
        saved = self._clean_ci_env()
        try:
            with tempfile.TemporaryDirectory() as td:
                root = pathlib.Path(td)
                _init_git_repo_with_origin_main(root, version="0.0.3")
                _git(root, "checkout", "-b", "fix/uncommitted")
                (root / "notes.txt").write_text("wip\n", encoding="utf-8")

                runner = check.CheckRunner(root, verify_ui_build=False, with_e2e=False)
                err = runner._check_pr_version_bump("0.0.3")
                self.assertIsNotNone(err)
                assert err is not None
                self.assertIn("PR version bump required", err)
        finally:
            self._restore_ci_env(saved)

    def test_passes_on_feature_branch_when_bumped(self) -> None:
        saved = self._clean_ci_env()
        try:
            with tempfile.TemporaryDirectory() as td:
                root = pathlib.Path(td)
                _init_git_repo_with_origin_main(root, version="0.0.3")
                _git(root, "checkout", "-b", "feat/with-bump")
                bump_version.bump_repo_version(root, part="patch")
                _git(root, "add", ".")
                _git(root, "commit", "-m", "feat: bump patch version")

                runner = check.CheckRunner(root, verify_ui_build=False, with_e2e=False)
                self.assertIsNone(runner._check_pr_version_bump("0.0.4"))
                self.assertIsNone(runner._check_pr_version_bump("0.1.0"))
        finally:
            self._restore_ci_env(saved)


if __name__ == "__main__":
    unittest.main()
