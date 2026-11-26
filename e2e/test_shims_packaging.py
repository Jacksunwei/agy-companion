"""Runtime packaging & launcher shims suite (`--suite shims`, 7 cohesive tests).

Exercises ONLY executable runtime shims (zero static repo source/doc grepping):
- `plugin/scripts/run-companion.sh`: dev symlink discovery, env scrubbing, XDG cache precedence,
  bad override rejection, SHA-256 & tarball safety, and physical `cd -P` symlink loop guard
- `scripts/dev-install.sh`: dry-run, link, idempotent uninstall, zero Antigravity settings mutation,
  real-directory/foreign-symlink/physical-`cd -P` guards
- `packages/npm/agy-companion/bin/agy-companion.js`: `NODE_PATH` stub dispatch, exit-code propagation,
  missing-package and unsupported-platform errors
- `packages/python/python/agy_companion/__init__.py`: `find_binary()` in isolated child process
"""

from __future__ import annotations

import hashlib
import io
import json
import os
import shlex
import shutil
import sys
import tarfile
from pathlib import Path
from typing import Any

from harness import E2ETestCase, IsolatedEnv, run_cli


_PLACEHOLDER_KEY = "agy-companion-local"


def _build_safe_bin_dir(env: IsolatedEnv, name: str = "safe_bin") -> Path:
    safe_bin = env.root / name
    safe_bin.mkdir(parents=True, exist_ok=True)
    tools = (
        "awk",
        "basename",
        "bash",
        "sh",
        "cat",
        "chmod",
        "cp",
        "cut",
        "dirname",
        "env",
        "grep",
        "head",
        "id",
        "ln",
        "ls",
        "mkdir",
        "mktemp",
        "mv",
        "printf",
        "pwd",
        "readlink",
        "realpath",
        "rm",
        "sed",
        "shasum",
        "sha256sum",
        "sleep",
        "tar",
        "tr",
        "uname",
        "wc",
    )
    for tool in tools:
        for base in ("/usr/bin", "/bin", "/usr/local/bin"):
            cand = Path(base) / tool
            if cand.is_file() and os.access(cand, os.X_OK):
                dst = safe_bin / tool
                if not dst.exists():
                    os.symlink(cand, dst)
                break
    return safe_bin


def _copy_isolated_run_companion(repo_root: Path, dest_plugin_dir: Path) -> Path:
    src_script = repo_root / "plugin" / "scripts" / "run-companion.sh"
    src_pj = repo_root / "plugin" / "plugin.json"
    scripts_dir = dest_plugin_dir / "scripts"
    scripts_dir.mkdir(parents=True, exist_ok=True)
    dst_script = scripts_dir / "run-companion.sh"
    shutil.copy2(src_script, dst_script)
    os.chmod(dst_script, 0o755)
    shutil.copy2(src_pj, dest_plugin_dir / "plugin.json")
    return dst_script


class TestShimRunCompanion(E2ETestCase):
    SUITE = "shims"
    FEATURE = "SHIM_LAUNCHER"

    def test_shim_run_companion_symlink_dev_discovery_and_env_scrub(self) -> None:
        """run-companion.sh discovers dev binary via symlinked plugin and scrubs proxy/placeholder env vars."""
        if os.name == "nt":
            self.skipTest("POSIX bash launcher test")
        binary = self.require_binary()
        env = self.make_env()

        # (a) Fake checkout with symlinked plugin dir & target/debug/agy-companion -> real binary.
        checkout = env.root / "fake_checkout"
        _copy_isolated_run_companion(self.repo_root, checkout / "plugin")
        dbg_bin = checkout / "target" / "debug" / "agy-companion"
        dbg_bin.parent.mkdir(parents=True, exist_ok=True)
        os.symlink(binary, dbg_bin)
        plugins_dir = env.root / "plugins"
        plugins_dir.mkdir()
        linked_plugin = plugins_dir / "agy-companion"
        os.symlink(checkout / "plugin", linked_plugin)

        safe_bin = _build_safe_bin_dir(env)
        r_dev = run_cli(
            ["/bin/bash", str(linked_plugin / "scripts" / "run-companion.sh"), "--version"],
            env=env.build_env(extra={"PATH": str(safe_bin)}),
            timeout=10.0,
        )
        self.assertEqual(r_dev.returncode, 0, f"{r_dev.stdout}\n{r_dev.stderr}")
        self.assertEqual(r_dev.stdout.strip(), "agy-companion 0.1.0")

        # (b) Env scrubbing: placeholder GEMINI_API_KEY & proxy vars scrubbed; GOOGLE_API_KEY & real GEMINI_API_KEY kept.
        dumper = env.root / "env_dumper.sh"
        dumper.write_text(
            f"#!/bin/sh\nexec {shlex.quote(sys.executable)} -B -c "
            f"'import json, os; print(json.dumps(dict(os.environ)))'\n",
            encoding="utf-8",
        )
        os.chmod(dumper, 0o755)
        launcher = self.repo_root / "plugin" / "scripts" / "run-companion.sh"

        r_ph = run_cli(
            ["/bin/bash", str(launcher)],
            env=env.build_env(
                extra={
                    "AGY_COMPANION_BIN": str(dumper),
                    "CLOUD_CODE_URL": "http://127.0.0.1:9999",
                    "GOOGLE_GEMINI_BASE_URL": "http://127.0.0.1:9999",
                    "GEMINI_API_KEY": _PLACEHOLDER_KEY,
                    "GOOGLE_API_KEY": "real-google-key",
                    "AGY_COMPANION_HOOK_ACTIVE": "1",
                    "JETSKI_MCP_ENV_RESTORE_PREFIX": "__TEST__",
                    "JETSKI_MCP_ENV_RESTORE_VARS": "CLOUD_CODE_URL",
                }
            ),
            timeout=10.0,
        )
        self.assertEqual(r_ph.returncode, 0, r_ph.stderr)
        seen_ph = r_ph.json_stdout()
        for scrubbed in (
            "CLOUD_CODE_URL",
            "GOOGLE_GEMINI_BASE_URL",
            "GEMINI_API_KEY",
            "AGY_COMPANION_HOOK_ACTIVE",
            "JETSKI_MCP_ENV_RESTORE_PREFIX",
            "JETSKI_MCP_ENV_RESTORE_VARS",
        ):
            self.assertNotIn(scrubbed, seen_ph, f"{scrubbed} should have been scrubbed")
        self.assertEqual(seen_ph.get("GOOGLE_API_KEY"), "real-google-key")

        r_real = run_cli(
            ["/bin/bash", str(launcher)],
            env=env.build_env(
                extra={
                    "AGY_COMPANION_BIN": str(dumper),
                    "CLOUD_CODE_URL": "http://127.0.0.1:9999",
                    "GOOGLE_GEMINI_BASE_URL": "http://127.0.0.1:9999",
                    "GEMINI_API_KEY": "real-non-placeholder-key",
                }
            ),
            timeout=10.0,
        )
        self.assertEqual(r_real.returncode, 0, r_real.stderr)
        seen_real = r_real.json_stdout()
        self.assertEqual(seen_real.get("GEMINI_API_KEY"), "real-non-placeholder-key")
        self.assertNotIn("CLOUD_CODE_URL", seen_real)
        self.assertNotIn("GOOGLE_GEMINI_BASE_URL", seen_real)

    def test_shim_run_companion_xdg_cache_precedence_and_bad_override(self) -> None:
        """run-companion.sh prefers version-matched XDG cache over PATH, skips bad .version, and rejects missing override."""
        if os.name == "nt":
            self.skipTest("POSIX bash launcher test")
        env = self.make_env()
        iso_script = _copy_isolated_run_companion(self.repo_root, env.root / "isolated_plugin")
        version = json.loads((env.root / "isolated_plugin" / "plugin.json").read_text(encoding="utf-8"))["version"]

        safe_bin = _build_safe_bin_dir(env)
        path_bin = safe_bin / "agy-companion"
        path_bin.write_text("#!/bin/sh\necho FROM_PATH\n", encoding="utf-8")
        os.chmod(path_bin, 0o755)

        cache_dir = env.data_dir / "bin"
        cache_dir.mkdir(parents=True, exist_ok=True)
        cache_bin = cache_dir / "agy-companion"
        cache_bin.write_text("#!/bin/sh\necho FROM_XDG_CACHE\n", encoding="utf-8")
        os.chmod(cache_bin, 0o755)
        ver_file = cache_dir / ".version"

        # (a) Version-matched XDG cache beats $PATH.
        ver_file.write_text(f"{version}\n", encoding="utf-8")
        r_hit = run_cli(
            ["/bin/bash", str(iso_script)],
            env=env.build_env(extra={"PATH": str(safe_bin)}),
            timeout=5.0,
        )
        self.assertEqual(r_hit.returncode, 0, r_hit.stderr)
        self.assertEqual(r_hit.stdout.strip(), "FROM_XDG_CACHE")

        # (b) Mismatched, missing, symlinked, and FIFO .version all skip cache cleanly -> PATH.
        ver_target = env.root / "ver_target.txt"
        ver_target.write_text(f"{version}\n", encoding="utf-8")
        for case in ("mismatched", "missing", "symlink", "fifo"):
            if ver_file.exists() or ver_file.is_symlink():
                ver_file.unlink()
            if case == "mismatched":
                ver_file.write_text("9.9.9\n", encoding="utf-8")
            elif case == "symlink":
                os.symlink(ver_target, ver_file)
            elif case == "fifo":
                os.mkfifo(ver_file)
            r_fb = run_cli(
                ["/bin/bash", str(iso_script)],
                env=env.build_env(extra={"PATH": str(safe_bin)}),
                timeout=5.0,
            )
            self.assertEqual(r_fb.returncode, 0, f"{case}: {r_fb.stderr}")
            self.assertEqual(r_fb.stdout.strip(), "FROM_PATH", f"{case} should fall back to PATH")

        # (c) Nonexistent AGY_COMPANION_BIN fails cleanly with no curl/wget.
        no_bin_path = _build_safe_bin_dir(env, name="safe_bin_no_companion")
        r_bad = run_cli(
            ["/bin/bash", str(iso_script), "--version"],
            env=env.build_env(
                extra={
                    "AGY_COMPANION_BIN": "/nonexistent/path/agy-companion",
                    "ANTIGRAVITY_EXECUTABLE_DATA_DIR": str(env.root / "no_cache"),
                    "PATH": str(no_bin_path),
                }
            ),
            timeout=5.0,
        )
        self.assertNotEqual(r_bad.returncode, 0)

    def test_shim_run_companion_sha256_tarball_and_physical_cd_security(self) -> None:
        """run-companion.sh enforces SHA-256 verification, tarball traversal/symlink guards, and physical cd -P."""
        if os.name == "nt":
            self.skipTest("POSIX bash launcher test")
        script = self.repo_root / "plugin" / "scripts" / "run-companion.sh"
        env = self.make_env()
        work = env.root / "launcher_sec"
        work.mkdir(parents=True, exist_ok=True)
        proc_env = env.build_env()

        def _make_tar_gz(dest: Path, entries: list[tuple[str, bytes | None, str | None]]) -> Path:
            dest.parent.mkdir(parents=True, exist_ok=True)
            with tarfile.open(dest, "w:gz") as tf:
                for name, data, linkname in entries:
                    ti = tarfile.TarInfo(name=name)
                    if linkname is not None:
                        ti.type = tarfile.SYMTYPE
                        ti.linkname = linkname
                        tf.addfile(ti)
                    else:
                        payload = data if data is not None else b""
                        ti.size = len(payload)
                        ti.mode = 0o755
                        tf.addfile(ti, io.BytesIO(payload))
            return dest

        def _run_sourced(snippet: str, *, cwd: Path = work) -> Any:
            return run_cli(
                ["bash", "-c", f'source "$1"; {snippet}', "_", str(script)],
                env=proc_env,
                cwd=cwd,
                stdin_bytes=b"",
                timeout=10.0,
            )

        archive_name = "agy-companion-v0.1.0-darwin-arm64.tar.gz"
        valid_payload = b"#!/bin/sh\necho ok\n"
        valid_tar = _make_tar_gz(work / archive_name, [("agy-companion", valid_payload, None)])
        valid_sha = hashlib.sha256(valid_tar.read_bytes()).hexdigest()
        checksums = work / "checksums.txt"
        checksums.write_text(f"{valid_sha}  {archive_name}\n", encoding="utf-8")

        eq_dir = work / "dir=1"
        eq_dir.mkdir()
        (eq_dir / "checksums.txt").write_text(f"{valid_sha}  {archive_name}\n", encoding="utf-8")
        (work / "-checksums.txt").write_text(f"{valid_sha}  {archive_name}\n", encoding="utf-8")

        bad_cs = work / "bad_checksums.txt"
        bad_cs.write_text(("0" * 64) + f"  {archive_name}\n", encoding="utf-8")
        dup_cs = work / "dup_checksums.txt"
        dup_cs.write_text(f"{valid_sha}  {archive_name}\n{valid_sha}  {archive_name}\n", encoding="utf-8")

        _make_tar_gz(work / "dir:colon" / "plain.tar.gz", [("agy-companion", valid_payload, None)])
        malicious_tars: list[tuple[str, Path]] = []
        for name, entries in (
            ("dotdot", [("../evil", b"x", None), ("agy-companion", valid_payload, None)]),
            ("abs", [("/tmp/evil", b"x", None), ("agy-companion", valid_payload, None)]),
            ("symlink", [("agy-companion", None, "/bin/sh")]),
            ("dup", [("agy-companion", valid_payload, None), ("./agy-companion", valid_payload, None)]),
            ("empty", [("agy-companion", b"", None)]),
        ):
            malicious_tars.append((name, _make_tar_gz(work / f"bad_{name}.tar.gz", entries)))

        probe = env.root / "resolve_probe"
        real_script = probe / "real_repo" / "plugin" / "scripts" / "run-companion.sh"
        real_sidecars = probe / "real_repo" / "plugin" / "sidecars"
        decoy_script = probe / "other" / "scripts" / "run-companion.sh"
        real_script.parent.mkdir(parents=True, exist_ok=True)
        real_sidecars.mkdir(parents=True, exist_ok=True)
        decoy_script.parent.mkdir(parents=True, exist_ok=True)
        real_script.write_text("#!/bin/sh\n", encoding="utf-8")
        decoy_script.write_text("#!/bin/sh\n", encoding="utf-8")
        os.symlink(real_sidecars, probe / "other" / "link_sidecars")
        tricky = f"{probe}/other/link_sidecars/../scripts/run-companion.sh"

        # 42-hop chain triggers resolve_script_path's >40-hop symlink-loop guard without macOS ELOOP unlink stalls.
        loop_dir = probe / "loop"
        loop_dir.mkdir()
        chain_target = real_script
        for hop in range(41, -1, -1):
            hop_link = loop_dir / f"hop_{hop}"
            os.symlink(chain_target, hop_link)
            chain_target = hop_link
        loop_entry = chain_target

        # Run all sourced-function security assertions inside ONE bash process (subshells preserve set -e).
        mal_checks = "\n".join(
            f'if ( extract_verified_binary {shlex.quote(str(tpath))} {shlex.quote(str(work / f"out_{mname}"))} >/dev/null 2>&1 ); then echo "FAIL:mal_{mname}" >&2; exit 1; fi'
            for mname, tpath in malicious_tars
        )
        batch = "\n".join(
            [
                f'verify_sha256 {shlex.quote(str(valid_tar))} {shlex.quote(str(checksums))} {shlex.quote(archive_name)}',
                f'EXTRACTED="$(extract_verified_binary {shlex.quote(str(valid_tar))} {shlex.quote(str(work / "out_valid"))})"',
                'printf "EXTRACTED:%s\\n" "$EXTRACTED"',
                f'EQ_SHA="$(lookup_expected_sha256 "dir=1/checksums.txt" {shlex.quote(archive_name)})"',
                'printf "EQ_SHA:%s\\n" "$EQ_SHA"',
                f'DASH_SHA="$(lookup_expected_sha256 "-checksums.txt" {shlex.quote(archive_name)})"',
                'printf "DASH_SHA:%s\\n" "$DASH_SHA"',
                f'if ( verify_sha256 {shlex.quote(str(valid_tar))} {shlex.quote(str(bad_cs))} {shlex.quote(archive_name)} >/dev/null 2>&1 ); then echo "FAIL:bad_sha" >&2; exit 1; fi',
                f'if ( lookup_expected_sha256 {shlex.quote(str(dup_cs))} {shlex.quote(archive_name)} >/dev/null 2>&1 ); then echo "FAIL:dup_sha" >&2; exit 1; fi',
                f'extract_verified_binary "dir:colon/plain.tar.gz" {shlex.quote(str(work / "out_colon"))} >/dev/null',
                mal_checks,
                f'PHYS="$(resolve_script_path {shlex.quote(tricky)})"',
                'printf "PHYS:%s\\n" "$PHYS"',
                f'LOOP_RC=0; LOOP_ERR="$( ( resolve_script_path {shlex.quote(str(loop_entry))} ) 2>&1 )" || LOOP_RC=$?',
                'if [ "$LOOP_RC" -eq 0 ]; then echo "FAIL:loop" >&2; exit 1; fi',
                'printf "LOOP_ERR:%s\\n" "$LOOP_ERR"',
            ]
        )
        r_all = _run_sourced(batch)
        self.assertEqual(r_all.returncode, 0, f"stdout={r_all.stdout}\nstderr={r_all.stderr}")
        kv = dict(line.split(":", 1) for line in r_all.stdout.splitlines() if ":" in line)
        self.assertEqual(Path(kv["EXTRACTED"]).read_bytes(), valid_payload)
        self.assertEqual(kv["EQ_SHA"], valid_sha)
        self.assertEqual(kv["DASH_SHA"], valid_sha)
        self.assertEqual(Path(kv["PHYS"]).resolve(), real_script.resolve())
        self.assertIn("symlink loop", kv.get("LOOP_ERR", "").lower())


class TestShimDevInstall(E2ETestCase):
    SUITE = "shims"
    FEATURE = "SHIM_DEV_INSTALL"

    def test_shim_dev_install_dry_run_link_and_idempotent_uninstall(self) -> None:
        """dev-install.sh links/uninstalls idempotently, respects --dry-run, and never mutates Antigravity config.json."""
        if os.name == "nt":
            self.skipTest("POSIX dev-install.sh test")
        script = self.repo_root / "scripts" / "dev-install.sh"
        env = self.make_env()
        proc_env = env.build_env()
        gemini_cfg = env.home_dir / ".gemini" / "config" / "config.json"
        gemini_cfg.parent.mkdir(parents=True, exist_ok=True)
        canary = '{"custom":"do-not-touch"}\n'
        gemini_cfg.write_text(canary, encoding="utf-8")

        pdir = env.root / "dev_plugins"
        link = pdir / "agy-companion"

        r_dry = run_cli([str(script), "--no-build", "--dry-run", "--plugins-dir", str(pdir)], env=proc_env)
        self.assertEqual(r_dry.returncode, 0, r_dry.stderr)
        self.assertFalse(link.exists() or link.is_symlink())

        r_inst = run_cli([str(script), "--no-build", "--plugins-dir", str(pdir)], env=proc_env)
        self.assertEqual(r_inst.returncode, 0, r_inst.stderr)
        self.assertTrue(link.is_symlink())
        self.assertEqual(link.resolve(), (self.repo_root / "plugin").resolve())

        r_udry = run_cli([str(script), "--uninstall", "--dry-run", "--plugins-dir", str(pdir)], env=proc_env)
        self.assertEqual(r_udry.returncode, 0, r_udry.stderr)
        self.assertTrue(link.is_symlink())

        r_un1 = run_cli([str(script), "--uninstall", "--plugins-dir", str(pdir)], env=proc_env)
        self.assertEqual(r_un1.returncode, 0, r_un1.stderr)
        self.assertFalse(link.exists() or link.is_symlink())

        r_un2 = run_cli([str(script), "--uninstall", "--plugins-dir", str(pdir)], env=proc_env)
        self.assertEqual(r_un2.returncode, 0, r_un2.stderr)
        self.assertIn("Nothing to uninstall", r_un2.stdout + r_un2.stderr)
        self.assertEqual(gemini_cfg.read_text(encoding="utf-8"), canary)

    def test_shim_dev_install_physical_symlink_and_overwrite_guards(self) -> None:
        """dev-install.sh refuses overwriting/removing real directories, foreign symlinks, and logical-cd decoys."""
        if os.name == "nt":
            self.skipTest("POSIX dev-install.sh test")
        script = self.repo_root / "scripts" / "dev-install.sh"
        env = self.make_env()
        proc_env = env.build_env()

        # Real non-symlink directory preserved on both install & uninstall.
        pdir_real = env.root / "real_dir_case"
        real_plugin = pdir_real / "agy-companion"
        real_plugin.mkdir(parents=True, exist_ok=True)
        marker = real_plugin / "keep.txt"
        marker.write_text("keep\n", encoding="utf-8")
        self.assertNotEqual(
            run_cli([str(script), "--no-build", "--plugins-dir", str(pdir_real)], env=proc_env).returncode, 0
        )
        self.assertNotEqual(
            run_cli([str(script), "--uninstall", "--plugins-dir", str(pdir_real)], env=proc_env).returncode, 0
        )
        self.assertTrue(marker.is_file())

        # Foreign symlink refused on uninstall.
        pdir_foreign = env.root / "foreign_case"
        foreign_target = env.root / "foreign_plugin"
        pdir_foreign.mkdir()
        foreign_target.mkdir()
        foreign_link = pdir_foreign / "agy-companion"
        os.symlink(foreign_target, foreign_link)
        self.assertNotEqual(
            run_cli([str(script), "--uninstall", "--plugins-dir", str(pdir_foreign)], env=proc_env).returncode, 0
        )
        self.assertTrue(foreign_link.is_symlink())

        # CF-1 Probe 1A (False Allow): logical-cd decoy must be refused, preserving victim symlink.
        probe = env.root / "cf1_probe_1a"
        parent = probe / "parent"
        victim_sub = probe / "victim_dir" / "sub"
        victim_plugin = probe / "victim_dir" / "plugin"
        parent.mkdir(parents=True)
        victim_sub.mkdir(parents=True)
        victim_plugin.mkdir(parents=True)
        os.symlink(self.repo_root / "plugin", parent / "plugin")
        os.symlink(victim_sub, parent / "link_dir")
        victim_link = victim_sub / "agy-companion"
        os.symlink("../plugin", victim_link)
        r_1a = run_cli([str(script), "--uninstall", "--plugins-dir", str(parent / "link_dir")], env=proc_env)
        self.assertNotEqual(r_1a.returncode, 0, r_1a.stdout)
        self.assertTrue(victim_link.is_symlink())

        # CF-1 Probe 1B (False Deny): valid relative symlink physically resolving to repo plugin must be removed.
        good_sub = env.root / "cf1_probe_1b" / "good_dir" / "sub"
        good_sub.mkdir(parents=True)
        os.symlink(self.repo_root / "scripts", good_sub / "link_to_repo_scripts")
        good_link = good_sub / "agy-companion"
        os.symlink("./link_to_repo_scripts/../plugin", good_link)
        r_1b = run_cli([str(script), "--uninstall", "--plugins-dir", str(good_sub)], env=proc_env)
        self.assertEqual(r_1b.returncode, 0, r_1b.stderr)
        self.assertFalse(good_link.exists() or good_link.is_symlink())


class TestShimNpmPackaging(E2ETestCase):
    SUITE = "shims"
    FEATURE = "SHIM_NPM"

    def test_shim_npm_bin_dispatch_exit_propagation_and_errors(self) -> None:
        """npm bin/agy-companion.js dispatches via NODE_PATH stub, propagates exit 42, and reports missing/unsupported errors."""
        node = shutil.which("node")
        if not node:
            self.skipTest("node not found in PATH")
        shim = self.repo_root / "packages" / "npm" / "agy-companion" / "bin" / "agy-companion.js"
        env = self.make_env()

        # Discover Node's exact platform & arch for the stub package name.
        r_info = run_cli(
            [node, "-e", "console.log(JSON.stringify({p:process.platform,a:process.arch}))"],
            env=env.build_env(),
        )
        self.assertEqual(r_info.returncode, 0, r_info.stderr)
        info = r_info.json_stdout()
        pkg_name = f"{info['p']}-{info['a']}"
        bin_name = "agy-companion.exe" if info["p"] == "win32" else "agy-companion"

        # (a) Valid NODE_PATH stub forwards argv (exit 0) and propagates exit code 42 + stderr.
        if os.name != "nt":
            node_path = env.root / "node_modules"
            stub_bin = node_path / "@agy-companion" / pkg_name / "bin" / bin_name
            stub_bin.parent.mkdir(parents=True, exist_ok=True)
            stub_bin.write_text(
                '#!/bin/sh\nif [ "$1" = "--fail" ]; then echo "stub-err" >&2; exit 42; fi\necho "ARGV:$*"\n',
                encoding="utf-8",
            )
            os.chmod(stub_bin, 0o755)
            stub_env = env.build_env(extra={"NODE_PATH": str(node_path)})

            r_ok = run_cli([node, str(shim), "--hello", "world"], env=stub_env, cwd=env.root)
            self.assertEqual(r_ok.returncode, 0, r_ok.stderr)
            self.assertEqual(r_ok.stdout.strip(), "ARGV:--hello world")

            r_fail = run_cli([node, str(shim), "--fail"], env=stub_env, cwd=env.root)
            self.assertEqual(r_fail.returncode, 42)
            self.assertIn("stub-err", r_fail.stderr)

        # (b) Empty NODE_PATH -> exit 1 with missing package error.
        r_miss = run_cli(
            [node, str(shim), "--version"],
            env=env.build_env(extra={"NODE_PATH": ""}),
            cwd=env.root,
        )
        self.assertEqual(r_miss.returncode, 1)
        self.assertIn("Unable to locate prebuilt binary package", r_miss.stderr)

        # (c) Preload overriding platform/arch -> exit 1 with unsupported platform error & zero JS syntax/runtime errors.
        preload = env.root / "fake_platform.cjs"
        preload.write_text(
            "Object.defineProperty(process, 'platform', { value: 'freebsd' });\n"
            "Object.defineProperty(process, 'arch', { value: 'mips' });\n",
            encoding="utf-8",
        )
        r_unsup = run_cli(
            [node, "-r", str(preload), str(shim), "--version"],
            env=env.build_env(extra={"NODE_PATH": ""}),
            cwd=env.root,
        )
        self.assertEqual(r_unsup.returncode, 1)
        self.assertIn("Unsupported platform/architecture", r_unsup.stderr)
        for js_err in ("SyntaxError", "ReferenceError", "TypeError"):
            self.assertNotIn(js_err, r_unsup.stderr)


class TestShimPythonPackaging(E2ETestCase):
    SUITE = "shims"
    FEATURE = "SHIM_PYTHON"

    def test_shim_python_find_binary_override_and_traversal_rejection(self) -> None:
        """Python agy_companion.find_binary() resolves valid override and rejects relative, non-exec, .., and NUL paths."""
        env = self.make_env()
        pkg_root = self.repo_root / "packages" / "python" / "python"

        valid_bin = env.root / "bin_valid" / "agy-companion"
        valid_bin.parent.mkdir(parents=True)
        valid_bin.write_text("#!/bin/sh\nexit 0\n", encoding="utf-8")
        os.chmod(valid_bin, 0o755)

        non_exec = env.root / "bin_non_exec" / "agy-companion"
        non_exec.parent.mkdir(parents=True)
        non_exec.write_text("not executable\n", encoding="utf-8")
        os.chmod(non_exec, 0o644)

        trav_path = str(valid_bin.parent / ".." / "bin_valid" / "agy-companion")
        child_script = (
            "import json, os, sys\n"
            "from pathlib import Path\n"
            f"sys.path.insert(0, {str(pkg_root)!r})\n"
            "import agy_companion as mod\n"
            "mod._candidate_scripts_dirs = lambda: []\n"
            "os.environ['PATH'] = ''\n"
            "results = {}\n"
            f"os.environ['AGY_COMPANION_BIN'] = {str(valid_bin)!r}\n"
            "r1 = mod.find_binary()\n"
            f"results['valid'] = (r1 == Path({str(valid_bin)!r}).resolve())\n"
            "os.environ['AGY_COMPANION_BIN'] = './agy-companion'\n"
            "results['relative'] = (mod.find_binary() is None)\n"
            f"os.environ['AGY_COMPANION_BIN'] = {str(non_exec)!r}\n"
            "results['non_exec'] = (mod.find_binary() is None) if os.name != 'nt' else True\n"
            f"os.environ['AGY_COMPANION_BIN'] = {trav_path!r}\n"
            "results['traversal'] = (mod.find_binary() is None)\n"
            "class _NulEnv(dict):\n"
            "    def get(self, k, d=None):\n"
            f"        return {str(valid_bin)!r} + '\\x00evil' if k == 'AGY_COMPANION_BIN' else super().get(k, d)\n"
            "orig_env = mod.os.environ\n"
            "try:\n"
            "    mod.os.environ = _NulEnv()\n"
            "    results['nul'] = (mod.find_binary() is None)\n"
            "finally:\n"
            "    mod.os.environ = orig_env\n"
            "print(json.dumps(results))\n"
        )
        r = run_cli(
            [sys.executable, "-B", "-c", child_script],
            env=env.build_env(),
            cwd=env.root,
            timeout=10.0,
        )
        self.assertEqual(r.returncode, 0, f"{r.stdout}\n{r.stderr}")
        res = r.json_stdout()
        for key in ("valid", "relative", "non_exec", "traversal", "nul"):
            self.assertIs(res.get(key), True, f"find_binary case {key!r} failed: {res}")
