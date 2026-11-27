#!/usr/bin/env python3
"""Unified cross-platform verification runner for agy-companion.

Profiles:
  --profile repo : OS-independent hygiene, secrets, commits, shell syntax,
                   4-way version/packaging sync, rustfmt, UI S5/bundle gate,
                   7 static hermeticity & S3/S4 guards, and plugin manifest/SVG checks.
  --profile os   : OS-dependent runtime checks (cargo clippy, cargo test,
                   UI behavioral tests, SVG validator unit tests, and optional
                   parallel opaque-box E2E via --with-e2e).
  --profile all  : Both repo and os checks (default; invoked by scripts/agent_check.sh).
"""

from __future__ import annotations

import argparse
import ast
import io
import json
import os
import pathlib
import py_compile
import re
import shutil
import subprocess
import sys
import tempfile
import time
import tokenize

sys.dont_write_bytecode = True

MAX_UI_BUNDLE_BYTES = 512_000

FORBIDDEN_TRACKED_RE = re.compile(
    r"(^|/)\.DS_Store$"
    r"|(^|/)\.env($|\.)"
    r"|(^|/)\.antigravitycli/"
    r"|(^|/)\.remember/"
    r"|^target/"
    r"|(^|/)node_modules/"
    r"|(^|/)__pycache__/"
    r"|\.py[cod]$"
    r"|\.(pem|key)$"
)
ENV_EXAMPLE_RE = re.compile(r"(^|/)\.env\.example$")

GOOGLE_API_KEY_PATTERN = r"AIza[0-9A-Za-z_-]{35}"

AI_ATTRIBUTION_RE = re.compile(
    r"co-authored-by:.*((claude|gemini|copilot|openai|chatgpt|anthropic|antigravity|cursor)"
    r"|(^|[^A-Za-z0-9])(ai|bot)([^A-Za-z0-9]|$))"
    r"|generated with\s+((claude|gemini|copilot)|ai([^A-Za-z0-9]|$))",
    re.IGNORECASE,
)

UNSAFE_JS_RE = re.compile(
    r"""(^|[^A-Za-z0-9_$])(exec|execSync)\s*\(|shell\s*:\s*(true|['"])"""
)

UI_S5_SINK_RE = re.compile(
    r"innerHTML|outerHTML|insertAdjacentHTML|document\.write|dangerouslySetInnerHTML"
    r"|localStorage|sessionStorage|document\.cookie|new\s+Function|XMLHttpRequest"
    r"|(^|[^A-Za-z0-9_$])(eval|alert|confirm|prompt)\s*\("
)

UI_FETCH_RE = re.compile(r"(^|[^A-Za-z0-9_$])fetch\s*\(")
UI_ALLOWED_SIDECAR_FETCH_RE = re.compile(r"""fetch\(\s*[`'"]/_api/""")
UI_BUNDLE_EXT_FETCH_RE = re.compile(
    r"""(^|[^A-Za-z0-9_$])fetch\s*\(\s*[`'"]\s*(https?:|//)""",
    re.IGNORECASE,
)

UI_EXTERNAL_TAG_RE = re.compile(
    r"<script[^>]*\ssrc\s*=|<link[^>]*\shref\s*=|<img[^>]*\ssrc\s*=|<iframe|@import",
    re.IGNORECASE,
)

_RUST_TOKEN_RE = re.compile(
    r'b?r(#*)"[\s\S]*?"\1'
    r"|b?'(?:\\(?:x[0-9A-Fa-f]{2}|u\{[0-9A-Fa-f_]+\}|.)|[^'\\])'"
    r'|b?"(?:\\[\s\S]|[^"\\])*"'
    r"|//[^\n]*"
    r"|/\*[\s\S]*?\*/"
)


class CheckRunner:
    def __init__(self, repo_root: pathlib.Path, verify_ui_build: bool, with_e2e: bool) -> None:
        self.repo_root = repo_root.resolve()
        self.verify_ui_build = verify_ui_build
        self.with_e2e = with_e2e
        self.failures = 0

    def pass_(self, msg: str) -> None:
        print(f"PASS {msg}", flush=True)

    def fail(self, msg: str, details: list[str] | None = None) -> None:
        print(f"FAIL {msg}", flush=True)
        if details:
            for line in details:
                print(f"  - {line}", flush=True)
        self.failures += 1

    def tool(self, name: str) -> str | None:
        return shutil.which(name)

    def _cmd_env(self, c_locale: bool = False) -> dict[str, str]:
        env = os.environ.copy()
        if c_locale and os.name != "nt":
            env["LC_ALL"] = "C"
        return env

    def run_capture(
        self,
        cmd: list[str],
        *,
        cwd: pathlib.Path | None = None,
        c_locale: bool = False,
    ) -> subprocess.CompletedProcess[str]:
        return subprocess.run(
            cmd,
            cwd=str(cwd or self.repo_root),
            env=self._cmd_env(c_locale=c_locale),
            capture_output=True,
            text=True,
            encoding="utf-8",
            errors="replace",
            check=False,
        )

    def run_stream(
        self,
        cmd: list[str],
        *,
        cwd: pathlib.Path | None = None,
    ) -> int:
        proc = subprocess.run(
            cmd,
            cwd=str(cwd or self.repo_root),
            env=self._cmd_env(),
            check=False,
        )
        return proc.returncode

    def is_executable(self, path: pathlib.Path) -> bool:
        if os.name != "nt":
            return os.access(path, os.X_OK)
        git = self.tool("git")
        if not git:
            return True
        rel = path.relative_to(self.repo_root).as_posix()
        proc = self.run_capture([git, "ls-files", "--stage", "--", rel])
        if proc.returncode != 0 or not proc.stdout.strip():
            return True
        mode = proc.stdout.strip().split()[0]
        return mode == "100755"

    def resolve_commit_range(self, git: str) -> str:
        rev_range = "HEAD"
        verify = self.run_capture([git, "rev-parse", "--verify", "origin/main"])
        if verify.returncode == 0:
            mb = self.run_capture([git, "merge-base", "origin/main", "HEAD"])
            head = self.run_capture([git, "rev-parse", "HEAD"])
            base = mb.stdout.strip()
            head_sha = head.stdout.strip()
            if mb.returncode == 0 and head.returncode == 0 and base and base != head_sha:
                rev_range = f"{base}..HEAD"
        return rev_range

    # ------------------------------------------------------------------
    # Repo checks (OS-independent)
    # ------------------------------------------------------------------

    def check_governance_files(self, tag: str) -> None:
        required = (
            "LICENSE",
            "AGENTS.md",
            ".agents/rules/workflow.md",
            ".github/PULL_REQUEST_TEMPLATE.md",
        )
        missing = [f for f in required if not (self.repo_root / f).is_file()]
        if missing:
            self.fail(f"{tag} Governance files: missing {' '.join(missing)}")
        elif (self.repo_root / "GEMINI.md").is_file():
            self.fail(
                f"{tag} Governance files: redundant GEMINI.md present "
                "(use AGENTS.md + .agents/rules/)"
            )
        else:
            self.pass_(
                f"{tag} Governance files: all {len(required)} required files present "
                "(Antigravity-native layout)"
            )

    def check_tracked_hygiene_and_secrets(self, tag: str) -> None:
        git = self.tool("git")
        if not git:
            self.fail(f"{tag} Tracked file hygiene & secret scan: git not found on PATH")
            return

        failed = False

        # 2a. Forbidden tracked filenames (incl. __pycache__/ and *.py[cod])
        ls_proc = self.run_capture([git, "ls-files"], c_locale=True)
        if ls_proc.returncode != 0:
            self.fail(f"{tag} Tracked file hygiene & secret scan: git ls-files failed")
            return

        forbidden_hits = [
            line
            for line in ls_proc.stdout.splitlines()
            if line and FORBIDDEN_TRACKED_RE.search(line) and not ENV_EXAMPLE_RE.search(line)
        ]
        if forbidden_hits:
            self.fail(
                f"{tag} Tracked file hygiene & secret scan: forbidden files tracked in git",
                forbidden_hits,
            )
            failed = True

        # 2b. Real Google API key secret scan (39-char AIza...) — NEVER echo matched key values!
        wt_proc = self.run_capture(
            [git, "grep", "-I", "-l", "-E", "--", GOOGLE_API_KEY_PATTERN],
            c_locale=True,
        )
        if wt_proc.returncode > 1:
            self.fail(
                f"{tag} Tracked file hygiene & secret scan: "
                f"git grep failed (exit {wt_proc.returncode})"
            )
            failed = True
        elif wt_proc.returncode == 0 and wt_proc.stdout.strip():
            self.fail(
                f"{tag} Tracked file hygiene & secret scan: "
                "potential Google API key detected in tracked file(s)",
                [line for line in wt_proc.stdout.splitlines() if line],
            )
            failed = True

        idx_proc = self.run_capture(
            [git, "grep", "--cached", "-I", "-l", "-E", "--", GOOGLE_API_KEY_PATTERN],
            c_locale=True,
        )
        if idx_proc.returncode > 1:
            self.fail(
                f"{tag} Tracked file hygiene & secret scan: "
                f"git grep --cached failed (exit {idx_proc.returncode})"
            )
            failed = True
        elif idx_proc.returncode == 0 and idx_proc.stdout.strip():
            self.fail(
                f"{tag} Tracked file hygiene & secret scan: "
                "potential Google API key detected in staged file(s)",
                [line for line in idx_proc.stdout.splitlines() if line],
            )
            failed = True

        has_head = self.run_capture([git, "rev-parse", "--verify", "HEAD"]).returncode == 0
        if has_head:
            rev_range = self.resolve_commit_range(git)

            diff_proc = self.run_capture(
                [git, "log", rev_range, "-m", f"-G{GOOGLE_API_KEY_PATTERN}", "--format=%h"],
                c_locale=True,
            )
            if diff_proc.returncode != 0:
                self.fail(
                    f"{tag} Tracked file hygiene & secret scan: "
                    f"git log -G failed (exit {diff_proc.returncode})"
                )
                failed = True
            elif diff_proc.stdout.strip():
                commits = list(dict.fromkeys(line for line in diff_proc.stdout.splitlines() if line))
                self.fail(
                    f"{tag} Tracked file hygiene & secret scan: "
                    "potential Google API key detected in commit diff(s)",
                    [f"commit {sha}" for sha in commits],
                )
                failed = True

            msg_proc = self.run_capture(
                [
                    git,
                    "log",
                    rev_range,
                    "--extended-regexp",
                    f"--grep={GOOGLE_API_KEY_PATTERN}",
                    "--format=%h",
                ],
                c_locale=True,
            )
            if msg_proc.returncode != 0:
                self.fail(
                    f"{tag} Tracked file hygiene & secret scan: "
                    f"git log --grep failed (exit {msg_proc.returncode})"
                )
                failed = True
            elif msg_proc.stdout.strip():
                commits = list(dict.fromkeys(line for line in msg_proc.stdout.splitlines() if line))
                self.fail(
                    f"{tag} Tracked file hygiene & secret scan: "
                    "potential Google API key detected in commit message(s)",
                    [f"commit {sha}" for sha in commits],
                )
                failed = True

        if not failed:
            self.pass_(
                f"{tag} Tracked file hygiene & secret scan: "
                "0 forbidden files and 0 Google API key matches (tracked, staged, commits)"
            )

    def check_zero_ai_attribution(self, tag: str) -> None:
        git = self.tool("git")
        if not git:
            self.fail(f"{tag} Commit hygiene: git not found on PATH")
            return

        if self.run_capture([git, "rev-parse", "--verify", "HEAD"]).returncode != 0:
            self.pass_(f"{tag} Commit hygiene: skipped (no commits yet)")
            return

        rev_range = self.resolve_commit_range(git)
        log_proc = self.run_capture([git, "log", rev_range, "--format=%h %B"], c_locale=True)
        if log_proc.returncode != 0:
            self.fail(f"{tag} Commit hygiene: git log failed for range {rev_range}")
            return

        hits = [line for line in log_proc.stdout.splitlines() if AI_ATTRIBUTION_RE.search(line)]
        if hits:
            self.fail(
                f"{tag} Commit hygiene: AI attribution detected in commit messages",
                hits,
            )
        else:
            self.pass_(f"{tag} Commit hygiene: 0 AI attribution lines found")

    def check_shell_scripts(self, tag: str) -> None:
        failed = False
        required_scripts = (
            "scripts/agent_check.sh",
            "scripts/dev-install.sh",
            "plugin/scripts/run-companion.sh",
        )
        for req in required_scripts:
            if not (self.repo_root / req).is_file():
                self.fail(f"{tag} Shell syntax: missing required script {req}")
                failed = True

        search_dirs = [self.repo_root / "scripts"]
        plugin_scripts = self.repo_root / "plugin" / "scripts"
        if plugin_scripts.is_dir():
            search_dirs.append(plugin_scripts)

        scripts: list[pathlib.Path] = []
        for directory in search_dirs:
            if directory.is_dir():
                scripts.extend(sorted(p for p in directory.rglob("*.sh") if p.is_file()))

        if not scripts:
            self.fail(f"{tag} Shell syntax: no .sh files found under scripts/ or plugin/scripts/")
            failed = True

        bash_bin = self.tool("bash")
        sys_bash = pathlib.Path("/bin/bash")
        check_sys_bash = os.name != "nt" and sys_bash.is_file() and os.access(sys_bash, os.X_OK)

        if os.name != "nt" and not bash_bin:
            self.fail(f"{tag} Shell syntax: bash not found on PATH")
            failed = True

        non_exec: list[str] = []
        bad_syntax: list[str] = []
        for script in scripts:
            rel = script.relative_to(self.repo_root).as_posix()
            if not self.is_executable(script):
                non_exec.append(rel)
            if bash_bin:
                if self.run_capture([bash_bin, "-n", "--", str(script)]).returncode != 0:
                    bad_syntax.append(rel)
                    continue
            if check_sys_bash:
                if self.run_capture([str(sys_bash), "-n", "--", str(script)]).returncode != 0:
                    bad_syntax.append(rel)

        if non_exec:
            self.fail(f"{tag} Shell syntax: missing executable bit (-x) on {' '.join(non_exec)}")
            failed = True
        if bad_syntax:
            self.fail(f"{tag} Shell syntax: failed bash -n on {' '.join(bad_syntax)}")
            failed = True

        dev_install = self.repo_root / "scripts" / "dev-install.sh"
        if dev_install.is_file():
            content = dev_install.read_text(encoding="utf-8")
            if re.search(r"config\.json", content, re.IGNORECASE):
                self.fail(f"{tag} Shell syntax: scripts/dev-install.sh must not reference config.json")
                failed = True

        if not failed:
            self.pass_(
                f"{tag} Shell syntax: all {len(scripts)} scripts/*.sh and plugin/scripts/*.sh "
                "valid & executable (dev-install.sh zero config.json refs)"
            )

    def check_version_sync_and_packaging(self, tag: str) -> None:
        failed = False
        node_bin = self.tool("node")
        if not node_bin:
            self.fail(f"{tag} Version sync & packaging: node not found on PATH")
            return

        # 5a. 4-way TOML/JSON version sync + npm/PyPI manifest assertions + py_compile
        cargo_ver, manifest_errors = _validate_manifests_and_py_compile(self.repo_root)
        if manifest_errors:
            self.fail(
                f"{tag} Version sync & packaging: 4-way manifest/Python validation failed",
                manifest_errors,
            )
            failed = True

        # 5b. JS shim existence, executable bit, node --check, and execFileSync allow-list hygiene
        npm_shim = self.repo_root / "packages" / "npm" / "agy-companion" / "bin" / "agy-companion.js"
        npm_gen = self.repo_root / "packages" / "npm" / "generate-platform-pkgs.mjs"

        if not npm_shim.is_file():
            self.fail(
                f"{tag} Version sync & packaging: "
                "missing packages/npm/agy-companion/bin/agy-companion.js"
            )
            failed = True
        else:
            if not self.is_executable(npm_shim):
                self.fail(
                    f"{tag} Version sync & packaging: "
                    "packages/npm/agy-companion/bin/agy-companion.js is not executable (expected 0755)"
                )
                failed = True
            if self.run_capture([node_bin, "--check", str(npm_shim)]).returncode != 0:
                self.fail(
                    f"{tag} Version sync & packaging: node --check failed on bin/agy-companion.js"
                )
                failed = True
            shim_text = npm_shim.read_text(encoding="utf-8")
            if "execFileSync" not in shim_text:
                self.fail(
                    f"{tag} Version sync & packaging: bin/agy-companion.js must use execFileSync"
                )
                failed = True
            unsafe_hits = [
                f"{idx}:{line.strip()}"
                for idx, line in enumerate(shim_text.splitlines(), start=1)
                if UNSAFE_JS_RE.search(line)
            ]
            if unsafe_hits:
                self.fail(
                    f"{tag} Version sync & packaging: "
                    "unsafe exec/shell pattern detected in bin/agy-companion.js",
                    unsafe_hits,
                )
                failed = True

        if not npm_gen.is_file():
            self.fail(
                f"{tag} Version sync & packaging: missing packages/npm/generate-platform-pkgs.mjs"
            )
            failed = True
        else:
            if not self.is_executable(npm_gen):
                self.fail(
                    f"{tag} Version sync & packaging: "
                    "packages/npm/generate-platform-pkgs.mjs is not executable (expected 0755)"
                )
                failed = True
            if self.run_capture([node_bin, "--check", str(npm_gen)]).returncode != 0:
                self.fail(
                    f"{tag} Version sync & packaging: "
                    "node --check failed on generate-platform-pkgs.mjs"
                )
                failed = True
            elif cargo_ver:
                with tempfile.TemporaryDirectory() as tmp_out:
                    gen_proc = self.run_capture(
                        [node_bin, str(npm_gen), "--out-dir", tmp_out]
                    )
                    if gen_proc.returncode != 0:
                        self.fail(
                            f"{tag} Version sync & packaging: "
                            "generate-platform-pkgs.mjs dry-run exited non-zero",
                            [gen_proc.stderr.strip()] if gen_proc.stderr.strip() else None,
                        )
                        failed = True
                    else:
                        gen_errors = _validate_generated_platform_pkgs(
                            pathlib.Path(tmp_out), cargo_ver
                        )
                        if gen_errors:
                            self.fail(
                                f"{tag} Version sync & packaging: "
                                "generate-platform-pkgs.mjs dry-run validation failed",
                                gen_errors,
                            )
                            failed = True

        if cargo_ver:
            bump_error = self._check_pr_version_bump(cargo_ver)
            if bump_error:
                self.fail(
                    f"{tag} Version sync & packaging: PR version bump check failed",
                    [bump_error],
                )
                failed = True

        # 5c. Release packaging helper script existence & executable bit
        release_helper = self.repo_root / "scripts" / "package_release_asset.py"
        if not release_helper.is_file():
            self.fail(
                f"{tag} Version sync & packaging: missing scripts/package_release_asset.py"
            )
            failed = True
        elif not self.is_executable(release_helper):
            self.fail(
                f"{tag} Version sync & packaging: "
                "scripts/package_release_asset.py is not executable (expected 0755)"
            )
            failed = True

        # 5d. Registry-safe README.md links/images (no relative paths that break on PyPI/npmjs.com)
        readme_errors = _validate_readme_absolute_urls(self.repo_root / "README.md")
        if readme_errors:
            self.fail(
                f"{tag} Version sync & packaging: README.md contains relative links/images "
                "(must use absolute https:// URLs or # anchors for PyPI/npmjs.com rendering)",
                readme_errors,
            )
            failed = True

        if not failed:
            self.pass_(
                f"{tag} Version sync & packaging: 4-way version {cargo_ver}, PR version bump, publishConfig, "
                "5 platform manifests, README absolute URLs, node --check, execFileSync hygiene & py_compile passed"
            )

    def _check_pr_version_bump(self, current_ver: str) -> str | None:
        git = self.tool("git")
        if not git:
            return None

        base_ref = "origin/main"
        gh_base = os.environ.get("GITHUB_BASE_REF", "").strip()
        if gh_base:
            candidate = f"origin/{gh_base}"
            if self.run_capture([git, "rev-parse", "--verify", candidate]).returncode == 0:
                base_ref = candidate
            elif self.run_capture([git, "rev-parse", "--verify", gh_base]).returncode == 0:
                base_ref = gh_base

        if self.run_capture([git, "rev-parse", "--verify", base_ref]).returncode != 0:
            return None

        is_pr_event = os.environ.get("GITHUB_EVENT_NAME", "").strip() == "pull_request"
        if not is_pr_event:
            ahead_proc = self.run_capture([git, "rev-list", "--count", f"{base_ref}..HEAD"])
            ahead_count = int(ahead_proc.stdout.strip()) if ahead_proc.returncode == 0 and ahead_proc.stdout.strip().isdigit() else 0
            branch_proc = self.run_capture([git, "rev-parse", "--abbrev-ref", "HEAD"])
            branch = branch_proc.stdout.strip() if branch_proc.returncode == 0 else ""
            status_proc = self.run_capture([git, "status", "--porcelain"])
            is_dirty = status_proc.returncode == 0 and bool(status_proc.stdout.strip())

            # On clean main (or untouched branch with no commits ahead of origin/main), skip.
            if ahead_count == 0 and (not is_dirty or branch == "main"):
                return None

        base_cargo = self.run_capture([git, "show", f"{base_ref}:Cargo.toml"])
        if base_cargo.returncode != 0:
            return None

        try:
            base_ver = _parse_toml_value_text(base_cargo.stdout, "workspace.package", "version", f"{base_ref}:Cargo.toml")
        except Exception as exc:
            return f"failed to read base version from {base_ref}:Cargo.toml: {exc}"

        cur_tuple = _parse_semver_tuple(current_ver)
        base_tuple = _parse_semver_tuple(base_ver)
        if cur_tuple is None:
            return f"current workspace version {current_ver!r} is not valid MAJOR.MINOR.PATCH SemVer"
        if base_tuple is None:
            return f"base version {base_ver!r} on {base_ref} is not valid MAJOR.MINOR.PATCH SemVer"

        if cur_tuple <= base_tuple:
            return (
                f"PR version bump required: workspace version {current_ver} is not greater than "
                f"{base_ref} ({base_ver}); run 'python3 -B scripts/bump_version.py' to bump the patch version"
            )
        return None

    def check_fmt_ui_static_and_hermeticity(self, tag: str) -> None:
        failed = False

        # Rust formatting (OS-independent)
        if (self.repo_root / "Cargo.toml").is_file():
            cargo_bin = self.tool("cargo")
            if not cargo_bin:
                self.fail(f"{tag} Static & UI: Cargo.toml present but cargo not found on PATH")
                failed = True
            elif self.run_stream([cargo_bin, "fmt", "--all", "--", "--check"]) != 0:
                self.fail(f"{tag} Static & UI: cargo fmt --all -- --check failed")
                failed = True

        # Optional deterministic UI build check (CI repo-checks)
        if self.verify_ui_build:
            npm_bin = self.tool("npm")
            git_bin = self.tool("git")
            ui_dir = str(self.repo_root / "ui")
            if not npm_bin:
                self.fail(f"{tag} Static & UI: npm not found on PATH for --verify-ui-build")
                failed = True
            elif not git_bin:
                self.fail(f"{tag} Static & UI: git not found on PATH for --verify-ui-build")
                failed = True
            else:
                for step_cmd in (
                    [npm_bin, "--prefix", ui_dir, "ci"],
                    [npm_bin, "--prefix", ui_dir, "run", "typecheck"],
                    [npm_bin, "--prefix", ui_dir, "run", "build"],
                    [git_bin, "diff", "--exit-code", "--", "ui/dist/index.html"],
                ):
                    if self.run_stream(step_cmd) != 0:
                        self.fail(
                            f"{tag} Static & UI: --verify-ui-build step failed: "
                            f"{' '.join(step_cmd)}"
                        )
                        failed = True
                        break

        # UI static S5 sink & single-file bundle size gate
        ui_src = self.repo_root / "ui" / "src"
        ui_bundle = self.repo_root / "ui" / "dist" / "index.html"
        bundle_bytes = 0

        if not ui_src.is_dir():
            self.fail(f"{tag} Static & UI: missing ui/src directory")
            failed = True
        else:
            src_hits = _scan_dir_regex(self.repo_root, ui_src, UI_S5_SINK_RE)
            if src_hits:
                self.fail(
                    f"{tag} Static & UI: forbidden DOM/JS/network sink(s) detected in ui/src/",
                    src_hits,
                )
                failed = True

            # S5 same-origin sidecar fetch guard: fetch() is only permitted inside
            # ui/src/transport.ts and must strictly target relative /_api/ paths.
            bad_fetch_hits: list[str] = []
            for path in sorted(ui_src.rglob("*")):
                if not path.is_file():
                    continue
                rel = path.relative_to(self.repo_root).as_posix()
                text = path.read_text(encoding="utf-8", errors="replace")
                for line_no, line in enumerate(text.splitlines(), start=1):
                    if UI_FETCH_RE.search(line):
                        if path.name != "transport.ts":
                            bad_fetch_hits.append(
                                f"{rel}:{line_no}: fetch() is only allowed inside ui/src/transport.ts"
                            )
                        elif not UI_ALLOWED_SIDECAR_FETCH_RE.search(line):
                            bad_fetch_hits.append(
                                f"{rel}:{line_no}: fetch() in ui/src/transport.ts must target relative /_api/ paths only"
                            )
            if bad_fetch_hits:
                self.fail(
                    f"{tag} Static & UI: S5 fetch() policy violation in ui/src/",
                    bad_fetch_hits,
                )
                failed = True

            if not ui_bundle.is_file() or ui_bundle.stat().st_size == 0:
                self.fail(f"{tag} Static & UI: ui/dist/index.html missing or empty")
                failed = True
            else:
                bundle_bytes = ui_bundle.stat().st_size
                if bundle_bytes <= 0 or bundle_bytes >= MAX_UI_BUNDLE_BYTES:
                    self.fail(
                        f"{tag} Static & UI: ui/dist/index.html size {bundle_bytes} bytes "
                        f"must be > 0 and < {MAX_UI_BUNDLE_BYTES} bytes"
                    )
                    failed = True

                ext_hits = _scan_file_regex(self.repo_root, ui_bundle, UI_EXTERNAL_TAG_RE)
                if ext_hits:
                    self.fail(
                        f"{tag} Static & UI: external resource tag(s) detected in ui/dist/index.html",
                        ext_hits,
                    )
                    failed = True

                bundle_sink_hits = _scan_file_regex(self.repo_root, ui_bundle, UI_S5_SINK_RE)
                if bundle_sink_hits:
                    self.fail(
                        f"{tag} Static & UI: forbidden sink(s) detected in ui/dist/index.html",
                        bundle_sink_hits,
                    )
                    failed = True

                bundle_ext_fetch_hits = _scan_file_regex(
                    self.repo_root, ui_bundle, UI_BUNDLE_EXT_FETCH_RE
                )
                if bundle_ext_fetch_hits:
                    self.fail(
                        f"{tag} Static & UI: external URL fetch() detected in ui/dist/index.html",
                        bundle_ext_fetch_hits,
                    )
                    failed = True

        # 7 Automated Static Hermeticity & S3/S4 Security Guards (Gate 6c)
        guard_errors = _run_static_hermeticity_guards(self.repo_root)
        if guard_errors:
            self.fail(
                f"{tag} Static & UI: static hermeticity/security guard(s) failed",
                guard_errors,
            )
            failed = True

        if not failed:
            build_note = " (+ deterministic UI rebuild)" if self.verify_ui_build else ""
            self.pass_(
                f"{tag} Static & UI: cargo fmt, 7 static hermeticity/security guards, 0 forbidden sinks/external tags, "
                f"ui/dist/index.html is {bundle_bytes} bytes (< {MAX_UI_BUNDLE_BYTES}){build_note}"
            )

    def check_plugin_bundle(self, tag: str) -> None:
        logo_validator = self.repo_root / "scripts" / "check_plugin_logo.py"
        if not logo_validator.is_file():
            self.fail(f"{tag} Plugin bundle: missing scripts/check_plugin_logo.py")
            return

        val_proc = self.run_capture(
            [sys.executable, "-B", str(logo_validator), str(self.repo_root)]
        )
        if val_proc.returncode != 0:
            details = [
                line
                for line in (val_proc.stderr + "\n" + val_proc.stdout).splitlines()
                if line.strip()
            ]
            self.fail(
                f"{tag} Plugin bundle: JSON manifest and Expat SVG logo validation failed",
                details,
            )
            return

        agy_bin = self.tool("agy")
        if not agy_bin:
            self.pass_(
                f"{tag} Plugin bundle: all 2 plugin JSON manifests and logo valid; "
                "skipped agy plugin validate --strict ./plugin (agy CLI not found on PATH)"
            )
            return

        if self.run_stream([agy_bin, "plugin", "validate", "--strict", str(self.repo_root / "plugin")]) == 0:
            self.pass_(
                f"{tag} Plugin bundle: all 2 plugin JSON manifests, logo, "
                "and agy plugin validate --strict ./plugin passed"
            )
        else:
            self.fail(f"{tag} Plugin bundle: agy plugin validate --strict ./plugin failed")

    # ------------------------------------------------------------------
    # OS-dependent runtime checks
    # ------------------------------------------------------------------

    def check_rust_clippy(self, tag: str) -> None:
        cargo_bin = self.tool("cargo")
        if not cargo_bin:
            self.fail(f"{tag} Rust clippy: cargo not found on PATH")
            return
        if self.run_stream([cargo_bin, "clippy", "--workspace", "--all-targets", "--", "-D", "warnings"]) == 0:
            self.pass_(f"{tag} Rust clippy: --workspace --all-targets -D warnings passed")
        else:
            self.fail(f"{tag} Rust clippy: cargo clippy --workspace --all-targets -- -D warnings failed")

    def check_rust_tests(self, tag: str) -> None:
        cargo_bin = self.tool("cargo")
        if not cargo_bin:
            self.fail(f"{tag} Rust tests: cargo not found on PATH")
            return
        if self.run_stream([cargo_bin, "test", "--workspace"]) == 0:
            self.pass_(f"{tag} Rust tests: cargo test --workspace passed")
        else:
            self.fail(f"{tag} Rust tests: cargo test --workspace failed")

    def check_ui_tests(self, tag: str) -> None:
        npm_bin = self.tool("npm")
        if not npm_bin:
            self.fail(f"{tag} UI tests: npm not found on PATH")
            return
        if self.run_stream([npm_bin, "--prefix", str(self.repo_root / "ui"), "test"]) == 0:
            self.pass_(f"{tag} UI tests: npm --prefix ui test passed")
        else:
            self.fail(f"{tag} UI tests: npm --prefix ui test failed")

    def check_svg_validator_unit_tests(self, tag: str) -> None:
        test_scripts = (
            self.repo_root / "scripts" / "test_check_plugin_logo.py",
            self.repo_root / "scripts" / "test_version_bump.py",
            self.repo_root / "scripts" / "test_release_packaging.py",
        )
        failed = False
        for test_script in test_scripts:
            rel = test_script.relative_to(self.repo_root).as_posix()
            if not test_script.is_file():
                self.fail(f"{tag} Script unit tests: missing {rel}")
                failed = True
                continue
            proc = self.run_capture([sys.executable, "-B", str(test_script)])
            if proc.returncode != 0:
                details = [
                    line for line in (proc.stdout + "\n" + proc.stderr).splitlines() if line.strip()
                ]
                self.fail(f"{tag} Script unit tests: {rel} failed", details)
                failed = True
        if not failed:
            names = ", ".join(p.relative_to(self.repo_root).as_posix() for p in test_scripts)
            self.pass_(f"{tag} Script unit tests: {names} passed")

    def check_e2e_suite(self, tag: str) -> None:
        cargo_bin = self.tool("cargo")
        if not cargo_bin:
            self.fail(f"{tag} Opaque-box E2E: cargo not found on PATH")
            return
        if self.run_stream([cargo_bin, "build", "-p", "companion-cli"]) != 0:
            self.fail(f"{tag} Opaque-box E2E: cargo build -p companion-cli failed")
            return
        bin_name = "agy-companion.exe" if os.name == "nt" else "agy-companion"
        binary_path = self.repo_root / "target" / "debug" / bin_name
        e2e_runner = self.repo_root / "e2e" / "run_e2e.py"
        cmd = [
            sys.executable,
            "-B",
            str(e2e_runner),
            "--suite",
            "all",
            "--workers",
            "4",
            "--binary",
            str(binary_path),
            "--repo-root",
            str(self.repo_root),
            "-v",
        ]
        if self.run_stream(cmd) == 0:
            self.pass_(f"{tag} Opaque-box E2E: --suite all --workers 4 passed")
        else:
            self.fail(f"{tag} Opaque-box E2E: e2e/run_e2e.py --suite all failed")

    # ------------------------------------------------------------------
    # Orchestration
    # ------------------------------------------------------------------

    def run(self, profile: str) -> int:
        started = time.monotonic()
        if profile in ("repo", "all"):
            self.check_governance_files("[repo 1/7]")
            self.check_tracked_hygiene_and_secrets("[repo 2/7]")
            self.check_zero_ai_attribution("[repo 3/7]")
            self.check_shell_scripts("[repo 4/7]")
            self.check_version_sync_and_packaging("[repo 5/7]")
            self.check_fmt_ui_static_and_hermeticity("[repo 6/7]")
            self.check_plugin_bundle("[repo 7/7]")

        if profile in ("os", "all"):
            os_total = 5 if self.with_e2e else 4
            self.check_rust_clippy(f"[os 1/{os_total}]")
            self.check_rust_tests(f"[os 2/{os_total}]")
            self.check_ui_tests(f"[os 3/{os_total}]")
            self.check_svg_validator_unit_tests(f"[os 4/{os_total}]")
            if self.with_e2e:
                self.check_e2e_suite(f"[os 5/{os_total}]")

        elapsed = time.monotonic() - started
        if self.failures > 0:
            print(
                f"\ncheck.py failed ({self.failures} failure(s), profile={profile}, {elapsed:.2f}s)",
                flush=True,
            )
            return 1
        print(
            f"\nAll check.py gates passed (profile={profile}, {elapsed:.2f}s).",
            flush=True,
        )
        return 0


# ----------------------------------------------------------------------
# Pure helper functions
# ----------------------------------------------------------------------


SEMVER_RE = re.compile(r"^(\d+)\.(\d+)\.(\d+)$")


def _parse_semver_tuple(version: str) -> tuple[int, int, int] | None:
    m = SEMVER_RE.match(version.strip())
    if not m:
        return None
    return int(m.group(1)), int(m.group(2)), int(m.group(3))


def _parse_toml_value(path: pathlib.Path, section: str, key: str) -> str:
    return _parse_toml_value_text(path.read_text(encoding="utf-8"), section, key, str(path))


def _parse_toml_value_text(text: str, section: str, key: str, label: str) -> str:
    try:
        import tomllib  # type: ignore[import-not-found]

        data = tomllib.loads(text)
        node: object = data
        for part in section.split("."):
            if not isinstance(node, dict) or part not in node:
                raise KeyError(f"missing [{section}]")
            node = node[part]
        if not isinstance(node, dict) or key not in node:
            raise KeyError(f"missing [{section}].{key}")
        val = node[key]
        if not isinstance(val, str) or not val:
            raise ValueError(f"[{section}].{key} must be a non-empty string")
        return val
    except ImportError:
        in_sec = False
        sec_re = re.compile(r"^\s*\[" + re.escape(section) + r"\]\s*(?:#.*)?$")
        any_sec_re = re.compile(r"^\s*\[")
        key_re = re.compile(r"^\s*" + re.escape(key) + r'\s*=\s*"([^"]+)"\s*(?:#.*)?$')
        for line in text.splitlines():
            if sec_re.match(line):
                in_sec = True
                continue
            if any_sec_re.match(line):
                in_sec = False
                continue
            if in_sec:
                m = key_re.match(line)
                if m:
                    return m.group(1)
        raise ValueError(f"could not locate [{section}].{key} in {label}")


def _validate_manifests_and_py_compile(root: pathlib.Path) -> tuple[str | None, list[str]]:
    errors: list[str] = []
    cargo_toml = root / "Cargo.toml"
    plugin_json = root / "plugin" / "plugin.json"
    npm_pkg_json = root / "packages" / "npm" / "agy-companion" / "package.json"
    pyproject_toml = root / "packages" / "python" / "pyproject.toml"
    py_src_root = root / "packages" / "python" / "python"

    for required in (cargo_toml, plugin_json, npm_pkg_json, pyproject_toml):
        if not required.is_file():
            errors.append(f"missing required manifest: {required.relative_to(root).as_posix()}")

    if errors:
        return None, errors

    try:
        cargo_ver = _parse_toml_value(cargo_toml, "workspace.package", "version")
        plugin_data = json.loads(plugin_json.read_text(encoding="utf-8"))
        plugin_ver = plugin_data.get("version")
        npm_data = json.loads(npm_pkg_json.read_text(encoding="utf-8"))
        npm_ver = npm_data.get("version")
        py_ver = _parse_toml_value(pyproject_toml, "project", "version")
        py_requires_python = _parse_toml_value(pyproject_toml, "project", "requires-python")
        py_readme = _parse_toml_value(pyproject_toml, "project", "readme")
        maturin_bindings = _parse_toml_value(pyproject_toml, "tool.maturin", "bindings")
        maturin_manifest = _parse_toml_value(pyproject_toml, "tool.maturin", "manifest-path")
        maturin_module = _parse_toml_value(pyproject_toml, "tool.maturin", "module-name")
    except Exception as exc:
        return None, [str(exc)]

    if not (cargo_ver == plugin_ver == npm_ver == py_ver):
        errors.append(
            f"4-way version mismatch: Cargo.toml={cargo_ver!r}, "
            f"plugin/plugin.json={plugin_ver!r}, "
            f"packages/npm/agy-companion/package.json={npm_ver!r}, "
            f"packages/python/pyproject.toml={py_ver!r}"
        )

    expected_publish = {"registry": "https://registry.npmjs.org/", "access": "public"}
    if npm_data.get("publishConfig") != expected_publish:
        errors.append(
            f"packages/npm/agy-companion/package.json publishConfig must be {expected_publish!r}, "
            f"got {npm_data.get('publishConfig')!r}"
        )

    expected_bin = {
        "agy-companion": "bin/agy-companion.js",
        "agyc": "bin/agy-companion.js",
    }
    if npm_data.get("bin") != expected_bin:
        errors.append(
            f"packages/npm/agy-companion/package.json bin must be {expected_bin!r}, "
            f"got {npm_data.get('bin')!r}"
        )

    expected_targets = {
        "@agy-companion/darwin-arm64",
        "@agy-companion/darwin-x64",
        "@agy-companion/linux-x64",
        "@agy-companion/linux-arm64",
        "@agy-companion/win32-x64",
    }
    opt_deps = npm_data.get("optionalDependencies")
    if not isinstance(opt_deps, dict):
        errors.append("packages/npm/agy-companion/package.json missing optionalDependencies object")
    else:
        missing_targets = sorted(expected_targets - set(opt_deps.keys()))
        if missing_targets:
            errors.append(f"optionalDependencies missing targets: {missing_targets}")
        for dep_name, dep_ver in sorted(opt_deps.items()):
            if dep_name.startswith("@agy-companion/") and dep_ver != cargo_ver:
                errors.append(f"optionalDependencies[{dep_name!r}]={dep_ver!r} != {cargo_ver!r}")

    if py_requires_python != ">=3.11":
        errors.append(
            f"[project].requires-python must be '>=3.11', got {py_requires_python!r}"
        )

    if py_readme != "../../README.md":
        errors.append(
            f"[project].readme must be '../../README.md', got {py_readme!r}"
        )
    else:
        resolved_readme = (pyproject_toml.parent / py_readme).resolve()
        if not resolved_readme.is_file():
            errors.append(f"[project].readme target does not exist: {py_readme}")

    if maturin_bindings != "bin":
        errors.append(f"[tool.maturin].bindings must be 'bin', got {maturin_bindings!r}")

    if maturin_module != "agy_companion":
        errors.append(
            f"[tool.maturin].module-name must be 'agy_companion', got {maturin_module!r}"
        )

    resolved_manifest = (pyproject_toml.parent / maturin_manifest).resolve()
    if not resolved_manifest.is_file():
        errors.append(f"[tool.maturin].manifest-path does not exist: {maturin_manifest}")

    py_init = py_src_root / "agy_companion" / "__init__.py"
    if not py_init.is_file():
        errors.append("missing packages/python/python/agy_companion/__init__.py")
    else:
        py_files = sorted(py_src_root.rglob("*.py"))
        e2e_runner = root / "e2e" / "run_e2e.py"
        if e2e_runner.is_file():
            py_files.extend(sorted((root / "e2e").glob("*.py")))
        scripts_dir = root / "scripts"
        if scripts_dir.is_dir():
            py_files.extend(sorted(scripts_dir.glob("*.py")))
        with tempfile.TemporaryDirectory() as td:
            for idx, py_file in enumerate(py_files):
                try:
                    py_compile.compile(
                        str(py_file),
                        cfile=os.path.join(td, f"mod_{idx}.pyc"),
                        doraise=True,
                    )
                except py_compile.PyCompileError as exc:
                    errors.append(f"py_compile failed for {py_file.relative_to(root).as_posix()}: {exc}")

    return cargo_ver, errors


def _validate_generated_platform_pkgs(out_dir: pathlib.Path, expected_ver: str) -> list[str]:
    expected_publish = {"registry": "https://registry.npmjs.org/", "access": "public"}
    targets = {
        "darwin-arm64": (["darwin"], ["arm64"]),
        "darwin-x64": (["darwin"], ["x64"]),
        "linux-x64": (["linux"], ["x64"]),
        "linux-arm64": (["linux"], ["arm64"]),
        "win32-x64": (["win32"], ["x64"]),
    }
    errors: list[str] = []
    for target, (exp_os, exp_cpu) in targets.items():
        manifest_path = out_dir / target / "package.json"
        if not manifest_path.is_file():
            errors.append(f"missing generated manifest: {target}/package.json")
            continue
        data = json.loads(manifest_path.read_text(encoding="utf-8"))
        if data.get("name") != f"@agy-companion/{target}":
            errors.append(f"{target}: unexpected name {data.get('name')!r}")
        if data.get("version") != expected_ver:
            errors.append(f"{target}: version {data.get('version')!r} != {expected_ver!r}")
        if data.get("publishConfig") != expected_publish:
            errors.append(f"{target}: unexpected publishConfig {data.get('publishConfig')!r}")
        if data.get("os") != exp_os or data.get("cpu") != exp_cpu:
            errors.append(f"{target}: unexpected os/cpu {data.get('os')!r}/{data.get('cpu')!r}")
        if data.get("files") != ["bin/"]:
            errors.append(f"{target}: unexpected files {data.get('files')!r}")
        if data.get("preferUnplugged") is not True:
            errors.append(f"{target}: preferUnplugged must be true")
        if "exports" in data:
            errors.append(f"{target}: must omit 'exports' field")
        repo_dir = (
            data.get("repository", {}).get("directory")
            if isinstance(data.get("repository"), dict)
            else None
        )
        if repo_dir != "packages/npm":
            errors.append(f"{target}: repository.directory must be 'packages/npm', got {repo_dir!r}")
        readme_path = out_dir / target / "README.md"
        if not readme_path.is_file() or not readme_path.read_text(encoding="utf-8").strip():
            errors.append(f"{target}: missing or empty generated README.md")
    return errors


_README_HTML_ATTR_RE = re.compile(r"""\b(?:src|href)\s*=\s*(?:"([^"]+)"|'([^']+)')""", re.IGNORECASE)
_README_MD_LINK_RE = re.compile(r"""!?\[[^\]]*\]\(\s*<?([^)\s>]+)>?(?:\s+["'][^"']*["'])?\s*\)""")


def _validate_readme_absolute_urls(readme_path: pathlib.Path) -> list[str]:
    if not readme_path.is_file():
        return [f"missing {readme_path.name}"]

    errors: list[str] = []
    in_fence = False
    for idx, line in enumerate(readme_path.read_text(encoding="utf-8").splitlines(), start=1):
        stripped = line.lstrip()
        if stripped.startswith("```"):
            in_fence = not in_fence
            continue
        if in_fence:
            continue

        for match in _README_HTML_ATTR_RE.finditer(line):
            url = (match.group(1) or match.group(2) or "").strip()
            if url and not url.startswith(("https://", "http://", "#", "mailto:")):
                errors.append(f"README.md:{idx}: relative HTML src/href {url!r}")

        for match in _README_MD_LINK_RE.finditer(line):
            url = match.group(1).strip()
            if url and not url.startswith(("https://", "http://", "#", "mailto:")):
                errors.append(f"README.md:{idx}: relative Markdown link {url!r}")

    return errors


def _snippet(line: str, match: re.Match[str], radius: int = 80) -> str:
    start = max(0, match.start() - radius)
    end = min(len(line), match.end() + radius)
    text = line[start:end].strip()
    if start > 0:
        text = "..." + text
    if end < len(line):
        text = text + "..."
    return text


def _scan_file_regex(
    root: pathlib.Path, path: pathlib.Path, pattern: re.Pattern[str]
) -> list[str]:
    hits: list[str] = []
    rel = path.relative_to(root).as_posix()
    text = path.read_text(encoding="utf-8", errors="replace")
    for idx, line in enumerate(text.splitlines(), start=1):
        m = pattern.search(line)
        if m:
            hits.append(f"{rel}:{idx}: {_snippet(line, m)}")
    return hits


def _scan_dir_regex(
    root: pathlib.Path, directory: pathlib.Path, pattern: re.Pattern[str]
) -> list[str]:
    hits: list[str] = []
    for path in sorted(directory.rglob("*")):
        if not path.is_file():
            continue
        hits.extend(_scan_file_regex(root, path, pattern))
    return hits


def _strip_rust_comments(text: str) -> str:
    def _repl(match: re.Match[str]) -> str:
        tok = match.group(0)
        if tok.startswith("//"):
            return ""
        if tok.startswith("/*"):
            return "\n" * tok.count("\n")
        return tok

    return _RUST_TOKEN_RE.sub(_repl, text)


def _strip_py_comments(text: str) -> str:
    lines = text.splitlines(keepends=True)
    try:
        tokens = list(tokenize.generate_tokens(io.StringIO(text).readline))
    except tokenize.TokenError:
        return text
    comment_tokens = [tok for tok in tokens if tok.type == tokenize.COMMENT]
    for tok in reversed(comment_tokens):
        row = tok.start[0] - 1
        lines[row] = lines[row][: tok.start[1]] + lines[row][tok.end[1] :]
    return "".join(lines)


def _extract_repo_root_segments(node: ast.AST) -> list[str] | None:
    if isinstance(node, ast.Name) and node.id == "repo_root":
        return []
    if isinstance(node, ast.Attribute) and node.attr == "repo_root":
        return []
    if isinstance(node, ast.BinOp) and isinstance(node.op, ast.Div):
        base = _extract_repo_root_segments(node.left)
        if base is not None:
            if isinstance(node.right, ast.Constant) and isinstance(node.right.value, str):
                return base + [p for p in node.right.value.split("/") if p]
            return base
    if isinstance(node, ast.Call):
        if isinstance(node.func, ast.Attribute) and node.func.attr == "joinpath":
            base = _extract_repo_root_segments(node.func.value)
            if base is not None:
                segs = list(base)
                for arg in node.args:
                    if isinstance(arg, ast.Constant) and isinstance(arg.value, str):
                        segs.extend(p for p in arg.value.split("/") if p)
                return segs
        if (
            (isinstance(node.func, ast.Attribute) and node.func.attr == "join")
            or (isinstance(node.func, ast.Name) and node.func.id == "Path")
        ) and node.args:
            base = _extract_repo_root_segments(node.args[0])
            if base is not None:
                segs = list(base)
                for arg in node.args[1:]:
                    if isinstance(arg, ast.Constant) and isinstance(arg.value, str):
                        segs.extend(p for p in arg.value.split("/") if p)
                return segs
    return None


def _run_static_hermeticity_guards(root: pathlib.Path) -> list[str]:
    errors: list[str] = []

    # Guard 1: Zero raw Command::new(BIN) / tokio::process::Command::new(BIN) or
    # CARGO_BIN_EXE_agy-companion in crates/companion-cli/tests/**/*.rs outside tests/common/mod.rs.
    cli_tests_dir = root / "crates" / "companion-cli" / "tests"
    common_mod = cli_tests_dir / "common" / "mod.rs"
    raw_bin_re = re.compile(
        r"\b(?:tokio::process::|std::process::)?Command::new\s*\(\s*(?:[\w:]+::)?BIN\b"
        r"|\bCARGO_BIN_EXE_agy-companion\b"
    )
    if not cli_tests_dir.is_dir():
        errors.append("Guard 1: missing crates/companion-cli/tests directory")
    else:
        for rs_file in sorted(cli_tests_dir.rglob("*.rs")):
            if rs_file == common_mod:
                continue
            cleaned = _strip_rust_comments(rs_file.read_text(encoding="utf-8"))
            for match in raw_bin_re.finditer(cleaned):
                line_no = cleaned.count("\n", 0, match.start()) + 1
                rel = rs_file.relative_to(root).as_posix()
                errors.append(
                    f"Guard 1 ({rel}:{line_no}): "
                    "raw Command::new(BIN) or CARGO_BIN_EXE_agy-companion outside tests/common/mod.rs"
                )

    # Guard 2: Zero bare CompanionUiServer::new( or unisolated CompanionUiServer::with_options(
    # in crates/companion-ui/src/tests.rs outside isolated_ui_server_with and
    # ui_resource_reads_process_env_var_dynamically.
    ui_tests = root / "crates" / "companion-ui" / "src" / "tests.rs"
    if not ui_tests.is_file():
        errors.append("Guard 2: missing crates/companion-ui/src/tests.rs")
    else:
        cleaned = _strip_rust_comments(ui_tests.read_text(encoding="utf-8"))
        fn_re = re.compile(
            r"(?m)^[ \t]*(?:pub(?:\([^)]*\))?\s+)?(?:async\s+)?fn\s+([A-Za-z_][A-Za-z0-9_]*)\b"
        )
        ctor_re = re.compile(r"\bCompanionUiServer\s*::\s*(new|with_options)\s*\(")
        fn_matches = list(fn_re.finditer(cleaned))
        fn_names = {m.group(1) for m in fn_matches}
        if "isolated_ui_server_with" not in fn_names:
            errors.append(
                "Guard 2: isolated_ui_server_with helper not found in companion-ui/src/tests.rs"
            )

        slices: list[tuple[str, int, str]] = []
        if fn_matches:
            slices.append(("<module>", 0, cleaned[: fn_matches[0].start()]))
            for idx, m in enumerate(fn_matches):
                end = fn_matches[idx + 1].start() if idx + 1 < len(fn_matches) else len(cleaned)
                slices.append((m.group(1), m.start(), cleaned[m.start() : end]))
        else:
            slices.append(("<module>", 0, cleaned))

        for fn_name, offset, body in slices:
            for match in ctor_re.finditer(body):
                ctor = match.group(1)
                allowed = (
                    (ctor == "with_options" and fn_name == "isolated_ui_server_with")
                    or (ctor == "new" and fn_name == "ui_resource_reads_process_env_var_dynamically")
                )
                if not allowed:
                    line_no = cleaned.count("\n", 0, offset + match.start()) + 1
                    errors.append(
                        f"Guard 2 (crates/companion-ui/src/tests.rs:{line_no} in {fn_name}): "
                        f"disallowed CompanionUiServer::{ctor}("
                    )

    # Guard 3: Zero direct 24141 binds or live port literals in crates/ tests or e2e/.
    rust_port_patterns = (
        re.compile(r"\bbind\s*\([^;{}]{0,200}24141\b"),
        re.compile(r"(?<![:\w])(?:listen_)?port\s*=\s*24141\b"),
        re.compile(r"\b(?:NonZeroU16::new|ProxyPort::new)\s*\(\s*24141\s*\)"),
    )
    crates_dir = root / "crates"
    for rs_file in sorted(crates_dir.rglob("*.rs")):
        rel = rs_file.relative_to(root)
        raw_text = rs_file.read_text(encoding="utf-8")
        is_test_file = "tests" in rel.parts or rs_file.name == "tests.rs"
        if is_test_file:
            test_offset = 0
            test_text = _strip_rust_comments(raw_text)
        else:
            cfg_idx = raw_text.find("#[cfg(test)]")
            if cfg_idx == -1:
                continue
            test_offset = cfg_idx
            test_text = _strip_rust_comments(raw_text[cfg_idx:])
        for pat in rust_port_patterns:
            for match in pat.finditer(test_text):
                line_no = (
                    raw_text.count("\n", 0, test_offset)
                    + test_text.count("\n", 0, match.start())
                    + 1
                )
                errors.append(
                    f"Guard 3 ({rel.as_posix()}:{line_no}): "
                    "direct 24141 bind/port literal in Rust test"
                )

    py_port_patterns = (
        re.compile(r"\bbind\s*\([^;\n]{0,200}24141\b"),
        re.compile(r"\b(?:listen_)?port\s*=\s*24141\b"),
        re.compile(r"""["'][^"']*:24141\b"""),
    )
    e2e_dir = root / "e2e"
    if e2e_dir.is_dir():
        for py_file in sorted(e2e_dir.rglob("*.py")):
            if "__pycache__" in py_file.parts:
                continue
            rel = py_file.relative_to(root).as_posix()
            cleaned = _strip_py_comments(py_file.read_text(encoding="utf-8"))
            for pat in py_port_patterns:
                for match in pat.finditer(cleaned):
                    line_no = cleaned.count("\n", 0, match.start()) + 1
                    errors.append(
                        f"Guard 3 ({rel}:{line_no}): direct 24141 bind/port literal in e2e"
                    )

    # Guard 4 & Guard 5: e2e opaque-box hygiene & explicit non-None env= on run_cli().
    forbidden_repo_substr = re.compile(
        r"(?:^|/)(?:crates|docs|ui/src)(?:/|$)|(?:^|/)(?:README\.md|AGENTS\.md)$"
    )
    forbidden_repo_segments = frozenset({"crates", "docs", "README.md", "AGENTS.md"})
    forbidden_gate_refs = ("agent_check.sh", "scripts/check.py")

    if e2e_dir.is_dir():
        for py_file in sorted(e2e_dir.rglob("*.py")):
            if "__pycache__" in py_file.parts:
                continue
            rel = py_file.relative_to(root).as_posix()
            src = py_file.read_text(encoding="utf-8")
            tree = ast.parse(src, filename=rel)
            cleaned = _strip_py_comments(src)
            for gate_ref in forbidden_gate_refs:
                if gate_ref in cleaned:
                    errors.append(f"Guard 4 ({rel}): forbidden {gate_ref} reference in e2e")
            for node in ast.walk(tree):
                if isinstance(node, ast.Constant) and isinstance(node.value, str):
                    for gate_ref in forbidden_gate_refs:
                        if gate_ref in node.value:
                            errors.append(
                                f"Guard 4 ({rel}:{node.lineno}): forbidden {gate_ref} string in e2e"
                            )

            if not py_file.name.startswith("test_"):
                continue

            for node in ast.walk(tree):
                if isinstance(node, ast.Constant) and isinstance(node.value, str):
                    if forbidden_repo_substr.search(node.value):
                        errors.append(
                            f"Guard 4 ({rel}:{node.lineno}): white-box repo path string {node.value!r}"
                        )
                segs = _extract_repo_root_segments(node)
                if segs:
                    if segs[0] in forbidden_repo_segments or segs[:2] == ["ui", "src"]:
                        errors.append(
                            f"Guard 4 ({rel}:{getattr(node, 'lineno', 0)}): "
                            f"white-box repo_root read {'/'.join(segs)!r}"
                        )
                if isinstance(node, ast.Call):
                    is_run_cli = (
                        (isinstance(node.func, ast.Name) and node.func.id == "run_cli")
                        or (isinstance(node.func, ast.Attribute) and node.func.attr == "run_cli")
                    )
                    if is_run_cli:
                        env_kw = next((kw for kw in node.keywords if kw.arg == "env"), None)
                        if env_kw is None:
                            errors.append(
                                f"Guard 5 ({rel}:{node.lineno}): run_cli() missing explicit env= keyword"
                            )
                        elif isinstance(env_kw.value, ast.Constant) and env_kw.value.value is None:
                            errors.append(
                                f"Guard 5 ({rel}:{node.lineno}): run_cli() passed env=None"
                            )

    # Guard 6 (S4): reqwest must keep default-features = false and enable zero compression features
    # (gzip, brotli, deflate, zstd) across root Cargo.toml and all crates/*/Cargo.toml.
    forbidden_reqwest_features = frozenset({"gzip", "brotli", "deflate", "zstd"})
    cargo_manifests = [root / "Cargo.toml", *sorted(crates_dir.glob("*/Cargo.toml"))]
    saw_workspace_reqwest = False
    for manifest in cargo_manifests:
        if not manifest.is_file():
            continue
        rel = manifest.relative_to(root).as_posix()
        text = manifest.read_text(encoding="utf-8")
        try:
            import tomllib  # type: ignore[import-not-found]

            data = tomllib.loads(text)
        except ImportError:
            # Minimal fallback for Python <3.11 without tomllib.
            for line_no, line in enumerate(text.splitlines(), start=1):
                stripped = line.split("#", 1)[0]
                if not re.match(r"^\s*reqwest\s*=", stripped):
                    continue
                if manifest == root / "Cargo.toml":
                    saw_workspace_reqwest = True
                for feat in sorted(forbidden_reqwest_features):
                    if re.search(rf"""["']{feat}["']""", stripped, re.IGNORECASE):
                        errors.append(
                            f"Guard 6 ({rel}:{line_no}): forbidden reqwest compression feature {feat!r}"
                        )
                if "workspace" not in stripped and not re.search(
                    r"default[-_]features\s*=\s*false\b", stripped
                ):
                    errors.append(
                        f"Guard 6 ({rel}:{line_no}): reqwest must set default-features = false"
                    )
            continue
        except Exception as exc:
            errors.append(f"Guard 6 ({rel}): failed to parse TOML: {exc}")
            continue

        dep_tables: list[tuple[str, object]] = []
        ws_node = data.get("workspace")
        if isinstance(ws_node, dict) and isinstance(ws_node.get("dependencies"), dict):
            dep_tables.append(("workspace.dependencies", ws_node["dependencies"]))
        for sec in ("dependencies", "dev-dependencies", "build-dependencies"):
            if isinstance(data.get(sec), dict):
                dep_tables.append((sec, data[sec]))
        target_node = data.get("target")
        if isinstance(target_node, dict):
            for tgt_name, tgt_val in target_node.items():
                if not isinstance(tgt_val, dict):
                    continue
                for sec in ("dependencies", "dev-dependencies", "build-dependencies"):
                    if isinstance(tgt_val.get(sec), dict):
                        dep_tables.append((f"target.{tgt_name}.{sec}", tgt_val[sec]))

        for sec_name, deps in dep_tables:
            assert isinstance(deps, dict)
            for dep_key, spec in deps.items():
                is_reqwest = dep_key == "reqwest" or (
                    isinstance(spec, dict) and spec.get("package") == "reqwest"
                )
                if not is_reqwest:
                    continue
                if manifest == root / "Cargo.toml" and sec_name == "workspace.dependencies":
                    saw_workspace_reqwest = True
                if isinstance(spec, str):
                    errors.append(
                        f"Guard 6 ({rel} [{sec_name}].{dep_key}): "
                        "reqwest must use table syntax with default-features = false"
                    )
                    continue
                if not isinstance(spec, dict):
                    errors.append(
                        f"Guard 6 ({rel} [{sec_name}].{dep_key}): unsupported reqwest dependency entry"
                    )
                    continue

                features = spec.get("features") or []
                if isinstance(features, list):
                    for feat in features:
                        if isinstance(feat, str) and feat.strip().lower() in forbidden_reqwest_features:
                            errors.append(
                                f"Guard 6 ({rel} [{sec_name}].{dep_key}): "
                                f"forbidden reqwest compression feature {feat!r} (S4)"
                            )

                df = spec.get("default-features", spec.get("default_features"))
                if spec.get("workspace") is True:
                    if df is True:
                        errors.append(
                            f"Guard 6 ({rel} [{sec_name}].{dep_key}): "
                            "reqwest workspace dependency must not set default-features = true (S4)"
                        )
                elif df is not False:
                    errors.append(
                        f"Guard 6 ({rel} [{sec_name}].{dep_key}): "
                        "reqwest must explicitly set default-features = false (S4)"
                    )

    if not saw_workspace_reqwest:
        errors.append(
            "Guard 6 (Cargo.toml): expected reqwest in [workspace.dependencies] with default-features = false"
        )

    # Guard 7 (S3): Zero danger_accept_invalid_certs anywhere in crates/**/*.rs (comment-stripped).
    danger_certs_re = re.compile(r"\bdanger_accept_invalid_certs\b")
    for rs_file in sorted(crates_dir.rglob("*.rs")):
        rel = rs_file.relative_to(root).as_posix()
        cleaned = _strip_rust_comments(rs_file.read_text(encoding="utf-8"))
        for match in danger_certs_re.finditer(cleaned):
            line_no = cleaned.count("\n", 0, match.start()) + 1
            errors.append(
                f"Guard 7 ({rel}:{line_no}): forbidden danger_accept_invalid_certs in Rust workspace (S3)"
            )

    return errors


def parse_args(argv: list[str] | None = None) -> argparse.Namespace:
    parser = argparse.ArgumentParser(
        description="Unified cross-platform verification runner for agy-companion."
    )
    parser.add_argument(
        "--profile",
        choices=("repo", "os", "all"),
        default="all",
        help="Verification profile to execute (default: all).",
    )
    parser.add_argument(
        "--verify-ui-build",
        action="store_true",
        help="Rebuild ui/dist/index.html via npm ci/typecheck/build and assert zero git diff.",
    )
    parser.add_argument(
        "--with-e2e",
        action="store_true",
        help="Build companion-cli and run the parallel opaque-box E2E suite (--suite all --workers 4).",
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
    runner = CheckRunner(
        repo_root=args.repo_root,
        verify_ui_build=args.verify_ui_build,
        with_e2e=args.with_e2e,
    )
    return runner.run(args.profile)


if __name__ == "__main__":
    raise SystemExit(main())
