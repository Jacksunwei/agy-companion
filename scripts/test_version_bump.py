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
import versioning  # noqa: E402


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


class TestVersioning(unittest.TestCase):
    def test_parse_valid_final_and_dev_versions(self) -> None:
        final = versioning.parse_version("0.1.0")
        self.assertEqual((final.major, final.minor, final.patch, final.dev), (0, 1, 0, None))
        self.assertFalse(final.is_dev)
        self.assertTrue(final.is_final)
        self.assertEqual(str(final), "0.1.0")
        self.assertEqual(final.to_pep440(), "0.1.0")
        self.assertFalse(versioning.is_dev_version("0.1.0"))
        self.assertTrue(versioning.is_final_version("0.1.0"))
        self.assertEqual(versioning.to_pep440("0.1.0"), "0.1.0")

        dev = versioning.parse_version("0.2.0-dev.0")
        self.assertEqual((dev.major, dev.minor, dev.patch, dev.dev), (0, 2, 0, 0))
        self.assertTrue(dev.is_dev)
        self.assertFalse(dev.is_final)
        self.assertEqual(str(dev), "0.2.0-dev.0")
        self.assertEqual(dev.to_pep440(), "0.2.0.dev0")
        self.assertTrue(versioning.is_dev_version("0.2.0-dev.0"))
        self.assertFalse(versioning.is_final_version("0.2.0-dev.0"))
        self.assertEqual(versioning.to_pep440("0.2.0-dev.0"), "0.2.0.dev0")
        self.assertEqual(versioning.to_pep440("1.10.3-dev.12"), "1.10.3.dev12")

    def test_precedence_chain_strictly_increasing(self) -> None:
        ordered = [
            "0.0.12",
            "0.1.0-dev.0",
            "0.1.0-dev.1",
            "0.1.0-dev.2",
            "0.1.0-dev.10",
            "0.1.0",
            "0.2.0-dev.0",
            "0.2.0-dev.1",
            "0.2.0",
            "0.2.1-dev.0",
            "0.2.1",
            "1.0.0-dev.0",
            "1.0.0",
        ]
        parsed = [versioning.parse_version(v) for v in ordered]
        keys = [versioning.version_sort_key(v) for v in ordered]
        for i in range(len(ordered) - 1):
            self.assertLess(parsed[i], parsed[i + 1], f"expected {ordered[i]} < {ordered[i + 1]}")
            self.assertLessEqual(parsed[i], parsed[i + 1])
            self.assertGreater(parsed[i + 1], parsed[i])
            self.assertGreaterEqual(parsed[i + 1], parsed[i])
            self.assertLess(keys[i], keys[i + 1])

        self.assertEqual(versioning.parse_version("0.2.0-dev.0"), versioning.parse_version("0.2.0-dev.0"))
        self.assertLessEqual(versioning.parse_version("0.2.0-dev.0"), versioning.parse_version("0.2.0-dev.0"))
        self.assertGreaterEqual(versioning.parse_version("0.2.0-dev.0"), versioning.parse_version("0.2.0-dev.0"))

    def test_invalid_versions_rejected(self) -> None:
        invalid = [
            "",
            "0.0",
            "v0.2.0",
            "0.0.3-alpha",
            "0.2.0-rc.1",
            "0.2.0-dev",
            "0.2.0-dev.",
            "0.2.0-dev.-1",
            "0.2.0-dev.01",
            "01.2.3",
            "0.02.3",
            "0.2.03",
            "0.2.0\n",
            " 0.2.0",
            "0.2.0 ",
            "0.2.0.dev0",
            "../0.2.0",
            "1\u0660.0.0",
            "0.1\u0661.0",
            "0.0.1\u0662",
            "0.2.0-dev.1\u0661",
            "0.2.0-dev.1\u0966",
            "1\uff10.0.0",
            "0.2.0-dev.1\uff10",
        ]
        for bad in invalid:
            with self.subTest(version=bad):
                with self.assertRaisesRegex(ValueError, "invalid SemVer"):
                    versioning.parse_version(bad)
        with self.assertRaisesRegex(ValueError, "invalid SemVer"):
            versioning.parse_version(None)  # type: ignore[arg-type]


class TestBumpVersion(unittest.TestCase):
    def test_default_daily_dev_bump_and_release_promotion(self) -> None:
        with tempfile.TemporaryDirectory() as td:
            root = pathlib.Path(td)
            _write_minimal_repo(root, version="0.0.12")

            # Final 0.0.12 -> 0.0.13-dev.0 -> 0.0.13-dev.1 -> --release -> 0.0.13
            old, new, updated = bump_version.bump_repo_version(root)
            self.assertEqual((old, new), ("0.0.12", "0.0.13-dev.0"))
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
            self.assertIn('version = "0.0.13-dev.0"', (root / "Cargo.toml").read_text(encoding="utf-8"))
            lock_text = (root / "Cargo.lock").read_text(encoding="utf-8")
            self.assertEqual(lock_text.count('version = "0.0.13-dev.0"'), 2)
            self.assertIn('version = "1.0.219"', lock_text)
            self.assertEqual(
                json.loads((root / "plugin" / "plugin.json").read_text(encoding="utf-8"))["version"],
                "0.0.13-dev.0",
            )
            npm_data = json.loads(
                (root / "packages" / "npm" / "agy-companion" / "package.json").read_text(encoding="utf-8")
            )
            self.assertEqual(npm_data["version"], "0.0.13-dev.0")
            for dep_ver in npm_data["optionalDependencies"].values():
                self.assertEqual(dep_ver, "0.0.13-dev.0")
            self.assertIn(
                'version = "0.0.13-dev.0"',
                (root / "packages" / "python" / "pyproject.toml").read_text(encoding="utf-8"),
            )

            self.assertEqual(bump_version.bump_repo_version(root)[:2], ("0.0.13-dev.0", "0.0.13-dev.1"))
            self.assertEqual(
                bump_version.bump_repo_version(root, release=True)[:2],
                ("0.0.13-dev.1", "0.0.13"),
            )
            # Final minor 0.1.0 -> 0.1.1-dev.0 -> 0.1.1-dev.1 -> --part minor -> 0.2.0 -> 0.2.1-dev.0
            self.assertEqual(bump_version.bump_repo_version(root, part="minor")[:2], ("0.0.13", "0.1.0"))
            self.assertEqual(bump_version.bump_repo_version(root)[:2], ("0.1.0", "0.1.1-dev.0"))
            self.assertEqual(bump_version.bump_repo_version(root)[:2], ("0.1.1-dev.0", "0.1.1-dev.1"))
            self.assertEqual(bump_version.bump_repo_version(root, part="minor")[:2], ("0.1.1-dev.1", "0.2.0"))
            self.assertEqual(bump_version.bump_repo_version(root)[:2], ("0.2.0", "0.2.1-dev.0"))

    def test_release_fails_closed_when_already_final(self) -> None:
        with tempfile.TemporaryDirectory() as td:
            root = pathlib.Path(td)
            _write_minimal_repo(root, version="0.1.0")
            with self.assertRaisesRegex(ValueError, "already a final release"):
                bump_version.bump_repo_version(root, release=True)

            rc = bump_version.main(["--release", "--repo-root", str(root)])
            self.assertEqual(rc, 1)

    def test_patch_minor_major_and_explicit_bump(self) -> None:
        with tempfile.TemporaryDirectory() as td:
            root = pathlib.Path(td)
            _write_minimal_repo(root, version="0.0.3")

            old, new, updated = bump_version.bump_repo_version(root, part="patch")
            self.assertEqual((old, new), ("0.0.3", "0.0.4"))
            self.assertEqual(len(updated), 5)

            _, minor_ver, _ = bump_version.bump_repo_version(root, part="minor")
            self.assertEqual(minor_ver, "0.1.0")

            _, major_ver, _ = bump_version.bump_repo_version(root, part="major")
            self.assertEqual(major_ver, "1.0.0")

            _, explicit_ver, _ = bump_version.bump_repo_version(root, explicit="2.3.4")
            self.assertEqual(explicit_ver, "2.3.4")

            _, explicit_dev, _ = bump_version.bump_repo_version(root, explicit="2.4.0-dev.0")
            self.assertEqual(explicit_dev, "2.4.0-dev.0")

            # Explicit --part from a -dev.N version
            self.assertEqual(bump_version.compute_next_version("0.2.0-dev.3", part="patch"), "0.2.1")
            self.assertEqual(bump_version.compute_next_version("0.2.0-dev.3", part="minor"), "0.3.0")
            self.assertEqual(bump_version.compute_next_version("0.2.0-dev.3", part="major"), "1.0.0")

    def test_mutual_exclusion_and_invalid_inputs_rejected(self) -> None:
        with self.assertRaisesRegex(ValueError, "mutually exclusive"):
            bump_version.compute_next_version("0.2.0-dev.0", part="minor", release=True)
        with self.assertRaisesRegex(ValueError, "mutually exclusive"):
            bump_version.compute_next_version("0.2.0-dev.0", release=True, explicit="0.2.0")
        with self.assertRaisesRegex(ValueError, "mutually exclusive"):
            bump_version.compute_next_version("0.2.0-dev.0", part="patch", explicit="0.2.1")
        with self.assertRaisesRegex(ValueError, "invalid SemVer"):
            bump_version.compute_next_version("0.1.0", explicit="0.2.0-rc.1")
        for bad_explicit in (" 0.2.0", "0.2.0\n", 123):
            with self.subTest(explicit=bad_explicit):
                with self.assertRaisesRegex(ValueError, "invalid SemVer"):
                    bump_version.compute_next_version("0.1.0", explicit=bad_explicit)  # type: ignore[arg-type]
        with self.assertRaisesRegex(ValueError, "invalid SemVer"):
            bump_version.compute_next_version("0.0.3-alpha")

    def test_cli_main_modes(self) -> None:
        with tempfile.TemporaryDirectory() as td:
            root = pathlib.Path(td)
            _write_minimal_repo(root, version="0.1.0")

            self.assertEqual(bump_version.main(["--repo-root", str(root)]), 0)
            self.assertIn('version = "0.1.1-dev.0"', (root / "Cargo.toml").read_text(encoding="utf-8"))

            self.assertEqual(bump_version.main(["--repo-root", str(root)]), 0)
            self.assertIn('version = "0.1.1-dev.1"', (root / "Cargo.toml").read_text(encoding="utf-8"))

            self.assertEqual(bump_version.main(["--release", "--repo-root", str(root)]), 0)
            self.assertIn('version = "0.1.1"', (root / "Cargo.toml").read_text(encoding="utf-8"))

            self.assertEqual(bump_version.main(["--set", "0.2.1-dev.0", "--repo-root", str(root)]), 0)
            self.assertIn('version = "0.2.1-dev.0"', (root / "Cargo.toml").read_text(encoding="utf-8"))

            self.assertEqual(bump_version.main(["--set", "bad-ver", "--repo-root", str(root)]), 1)


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
                _init_git_repo_with_origin_main(root, version="0.1.0")
                _git(root, "checkout", "-b", "feat/with-bump")
                bump_version.bump_repo_version(root)
                _git(root, "add", ".")
                _git(root, "commit", "-m", "feat: start 0.1.1-dev.0 cycle")

                runner = check.CheckRunner(root, verify_ui_build=False, with_e2e=False)
                self.assertIsNone(runner._check_pr_version_bump("0.1.1-dev.0"))
                self.assertIsNone(runner._check_pr_version_bump("0.1.1-dev.1"))
                self.assertIsNone(runner._check_pr_version_bump("0.1.1"))
                self.assertIsNone(runner._check_pr_version_bump("0.2.0"))
        finally:
            self._restore_ci_env(saved)

    def test_exempts_github_meta_only_prs_and_rejects_mixed_or_downgrade(self) -> None:
        saved = self._clean_ci_env()
        try:
            with tempfile.TemporaryDirectory() as td:
                root = pathlib.Path(td)
                _init_git_repo_with_origin_main(root, version="0.2.0-dev.3")
                _git(root, "checkout", "-b", "dependabot/github-actions")

                # Committed .github/-only change passes without version bump
                wf_dir = root / ".github" / "workflows"
                wf_dir.mkdir(parents=True, exist_ok=True)
                (wf_dir / "ci.yml").write_text("name: CI\n", encoding="utf-8")
                _git(root, "add", ".github/workflows/ci.yml")
                _git(root, "commit", "-m", "ci: bump github actions")

                runner = check.CheckRunner(root, verify_ui_build=False, with_e2e=False)
                self.assertIsNone(runner._check_pr_version_bump("0.2.0-dev.3"))

                # Combined .github/ + .agents/ change also passes without version bump
                rules_dir = root / ".agents" / "rules"
                rules_dir.mkdir(parents=True, exist_ok=True)
                (rules_dir / "workflow.md").write_text("# Workflow\n", encoding="utf-8")
                _git(root, "add", ".agents/rules/workflow.md")
                _git(root, "commit", "-m", "docs(agents): update workflow rule")
                self.assertIsNone(runner._check_pr_version_bump("0.2.0-dev.3"))

                # Downgrade is still rejected even for exempt PRs
                self.assertIsNotNone(runner._check_pr_version_bump("0.2.0-dev.2"))

                # Uncommitted file outside .github/ and .agents/ fails
                (root / "notes.txt").write_text("oops\n", encoding="utf-8")
                err = runner._check_pr_version_bump("0.2.0-dev.3")
                self.assertIsNotNone(err)
                assert err is not None
                self.assertIn("PR version bump required", err)

                # Committed mixed change (.github/ + .agents/ + non-exempt) fails
                _git(root, "add", "notes.txt")
                _git(root, "commit", "-m", "feat: also touch notes")
                err = runner._check_pr_version_bump("0.2.0-dev.3")
                self.assertIsNotNone(err)
                assert err is not None
                self.assertIn("PR version bump required", err)
        finally:
            self._restore_ci_env(saved)

    def test_exempts_agents_only_prs_and_rejects_mixed_or_downgrade(self) -> None:
        saved = self._clean_ci_env()
        try:
            with tempfile.TemporaryDirectory() as td:
                root = pathlib.Path(td)
                _init_git_repo_with_origin_main(root, version="0.2.0-dev.3")
                _git(root, "checkout", "-b", "docs/agents-rule")

                rules_dir = root / ".agents" / "rules"
                rules_dir.mkdir(parents=True, exist_ok=True)
                (rules_dir / "workflow.md").write_text("# Workflow\n", encoding="utf-8")
                _git(root, "add", ".agents/rules/workflow.md")
                _git(root, "commit", "-m", "docs(agents): update workflow rule")

                runner = check.CheckRunner(root, verify_ui_build=False, with_e2e=False)
                self.assertIsNone(runner._check_pr_version_bump("0.2.0-dev.3"))
                self.assertIsNotNone(runner._check_pr_version_bump("0.2.0-dev.2"))

                (root / "README.md").write_text("changed\n", encoding="utf-8")
                err = runner._check_pr_version_bump("0.2.0-dev.3")
                self.assertIsNotNone(err)
                assert err is not None
                self.assertIn("PR version bump required", err)
        finally:
            self._restore_ci_env(saved)

    def test_dev_prerelease_transitions_and_downgrade_rejections(self) -> None:
        saved = self._clean_ci_env()
        try:
            cases = [
                ("0.0.12", "0.1.0-dev.0", True),
                ("0.1.0-dev.0", "0.1.0-dev.1", True),
                ("0.1.0-dev.1", "0.1.0", True),
                ("0.1.0", "0.2.0-dev.0", True),
                ("0.2.0-dev.0", "0.2.0-dev.1", True),
                ("0.2.0-dev.1", "0.2.0", True),
                ("0.2.0-dev.1", "0.2.0-dev.1", False),
                ("0.2.0-dev.1", "0.2.0-dev.0", False),
                ("0.2.0", "0.2.0-dev.0", False),
                ("0.2.0", "0.2.0-rc.1", False),
            ]
            for base_ver, head_ver, expect_pass in cases:
                with self.subTest(base=base_ver, head=head_ver):
                    with tempfile.TemporaryDirectory() as td:
                        root = pathlib.Path(td)
                        _init_git_repo_with_origin_main(root, version=base_ver)
                        _git(root, "checkout", "-b", "feat/check-transition")
                        (root / "notes.txt").write_text("change\n", encoding="utf-8")
                        _git(root, "add", "notes.txt")
                        _git(root, "commit", "-m", "feat: test transition")

                        runner = check.CheckRunner(root, verify_ui_build=False, with_e2e=False)
                        err = runner._check_pr_version_bump(head_ver)
                        if expect_pass:
                            self.assertIsNone(err)
                        else:
                            self.assertIsNotNone(err)
        finally:
            self._restore_ci_env(saved)

    def test_dev_track_retarget_preserves_dev_counter_above_latest_release_tag(self) -> None:
        saved = self._clean_ci_env()
        try:
            cases = [
                # (base_ver, head_ver, release_tags, expect_pass)
                ("0.2.0-dev.5", "0.1.1-dev.5", ["v0.1.0"], True),
                ("0.2.0-dev.5", "0.1.1-dev.6", ["v0.1.0"], True),
                ("0.2.0-dev.5", "0.1.1-dev.5", [], True),
                ("0.2.0-dev.5", "0.1.0-dev.5", ["v0.1.0"], False),
                ("0.2.0-dev.5", "0.0.13-dev.5", ["v0.1.0"], False),
                ("0.2.0-dev.5", "0.1.1-dev.0", ["v0.1.0"], False),
                ("0.2.0-dev.5", "0.1.1-dev.4", ["v0.1.0"], False),
                ("0.2.0-dev.5", "0.2.0-dev.5", ["v0.1.0"], False),
                ("0.2.0", "0.1.1-dev.5", ["v0.1.0"], False),
                ("0.2.0-dev.5", "0.1.1", ["v0.1.0"], False),
            ]
            for base_ver, head_ver, tags, expect_pass in cases:
                with self.subTest(base=base_ver, head=head_ver, tags=tags):
                    with tempfile.TemporaryDirectory() as td:
                        root = pathlib.Path(td)
                        _init_git_repo_with_origin_main(root, version=base_ver)
                        for tag in tags:
                            _git(root, "tag", tag)
                        _git(root, "checkout", "-b", "chore/retarget-dev-track")
                        (root / "notes.txt").write_text("retarget\n", encoding="utf-8")
                        _git(root, "add", "notes.txt")
                        _git(root, "commit", "-m", "chore: retarget dev version")

                        runner = check.CheckRunner(root, verify_ui_build=False, with_e2e=False)
                        err = runner._check_pr_version_bump(head_ver)
                        if expect_pass:
                            self.assertIsNone(err, f"expected pass, got {err!r}")
                        else:
                            self.assertIsNotNone(err, f"expected rejection for {base_ver} -> {head_ver}")
        finally:
            self._restore_ci_env(saved)

    def test_validate_manifests_rejects_invalid_semver_on_clean_main(self) -> None:
        with tempfile.TemporaryDirectory() as td:
            root = pathlib.Path(td)
            _write_minimal_repo(root, version="0.2.0-rc.1")
            (root / "README.md").write_text("# readme\n", encoding="utf-8")
            py_dir = root / "packages" / "python"
            (py_dir / "pyproject.toml").write_text(
                '[project]\nname = "agy-companion"\nversion = "0.2.0-rc.1"\n'
                'requires-python = ">=3.11"\nreadme = "../../README.md"\n\n'
                '[tool.maturin]\nbindings = "bin"\nmanifest-path = "../../Cargo.toml"\n'
                'module-name = "agy_companion"\n',
                encoding="utf-8",
            )
            py_pkg = py_dir / "python" / "agy_companion"
            py_pkg.mkdir(parents=True, exist_ok=True)
            (py_pkg / "__init__.py").write_text("", encoding="utf-8")
            npm_pkg = root / "packages" / "npm" / "agy-companion" / "package.json"
            npm_data = json.loads(npm_pkg.read_text(encoding="utf-8"))
            npm_data["publishConfig"] = {"registry": "https://registry.npmjs.org/", "access": "public"}
            npm_data["bin"] = {
                "agy-companion": "bin/agy-companion.js",
                "agyc": "bin/agy-companion.js",
            }
            npm_pkg.write_text(json.dumps(npm_data, indent=2) + "\n", encoding="utf-8")

            _, errors = check._validate_manifests_and_py_compile(root)
            self.assertTrue(
                any("invalid SemVer" in e for e in errors),
                f"expected invalid SemVer error, got {errors!r}",
            )


if __name__ == "__main__":
    unittest.main()
