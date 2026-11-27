"""Slim pure-Python harness safety guardrails (`python3 -B e2e/run_e2e.py --self-test`).

Covers the essential harness contracts without requiring a compiled binary:
  1. `IsolatedEnv`: hostile env scrubbing (zero in-process `os.environ` mutation),
     ephemeral loopback port outside `RESERVED_PORT_RANGE`, `0600` config write, teardown.
  2. `PidfileReaper`: LIFO `ExitStack` teardown and 5-layer unsafe-target refusal.
"""

from __future__ import annotations

import contextlib
import json
import os
import stat
import subprocess
import sys
import time
from pathlib import Path

from harness import (
    E2E_DIR,
    E2ETestCase,
    IsolatedEnv,
    RESERVED_PORT_RANGE,
    reap_env_daemons,
    reap_pidfile_daemon,
    register_pidfile_cleanup,
    run_cli,
)


class TestHarnessGuardrails(E2ETestCase):
    SUITE = "self"
    FEATURE = "SELF"

    def test_self_01_isolated_env_scrubbing_ephemeral_port_and_cleanup(self) -> None:
        """IsolatedEnv scrubs hostile env vars without mutating os.environ, avoids reserved ports, and cleans up."""
        iso = self.make_env()
        cfg = iso.make_config_dict()
        port = cfg["proxy"]["port"]
        self.assertIsInstance(port, int)
        self.assertGreater(port, 0)
        self.assertNotIn(port, RESERVED_PORT_RANGE)
        self.assertNotIn("api_key", cfg["providers"]["gemini"])
        self.assertFalse(iso.auth_dir.exists())
        auth_file = iso.write_gemini_auth("test-self-auth-key-01")
        self.assertEqual(auth_file, iso.gemini_auth_path)
        self.assertTrue(auth_file.is_file())
        if os.name != "nt":
            self.assertEqual(stat.S_IMODE(os.lstat(iso.auth_dir).st_mode), 0o700)
            self.assertEqual(stat.S_IMODE(os.lstat(auth_file).st_mode), 0o600)

        hostile_vars = {
            "GEMINI_API_KEY": "hostile-test-key",
            "GOOGLE_API_KEY": "hostile-google-key",
            "GOOGLE_GEMINI_BASE_URL": "http://127.0.0.1:1",
            "CLOUD_CODE_URL": "http://127.0.0.1:1",
            "AGY_COMPANION_CONFIG": "/nonexistent/hostile.json",
            "AGY_COMPANION_UI_HTML": "/nonexistent/hostile.html",
            "HTTP_PROXY": "http://127.0.0.1:1",
            "HTTPS_PROXY": "http://127.0.0.1:1",
            "ALL_PROXY": "http://127.0.0.1:1",
            "NO_PROXY": "example.com",
            "http_proxy": "http://127.0.0.1:1",
            "https_proxy": "http://127.0.0.1:1",
            "all_proxy": "http://127.0.0.1:1",
            "no_proxy": "example.com",
            "RUST_LOG": "trace",
        }
        child_env = iso.build_env(extra=hostile_vars)
        child_script = (
            "import json, os, stat, sys\n"
            f"sys.path.insert(0, {str(E2E_DIR)!r})\n"
            "from harness import IsolatedEnv, RESERVED_PORT_RANGE\n"
            f"hostile_keys = {sorted(hostile_vars.keys())!r}\n"
            "with IsolatedEnv() as inner:\n"
            "    root = inner.root\n"
            "    built = inner.build_env()\n"
            "    cfg = inner.make_config_dict()\n"
            "    assert cfg['proxy']['port'] not in RESERVED_PORT_RANGE\n"
            "    for k in hostile_keys:\n"
            "        if k == 'AGY_COMPANION_CONFIG':\n"
            "            assert built[k] == str(inner.config_path)\n"
            "        else:\n"
            "            assert k not in built, f'leaked {k}'\n"
            "    for k in ('HOME', 'XDG_CONFIG_HOME', 'XDG_DATA_HOME', 'XDG_CACHE_HOME', 'TMPDIR', 'TEMP', 'TMP', 'APPDATA', 'LOCALAPPDATA', 'USERPROFILE'):\n"
            "        assert built[k].startswith(str(root)), f'{k} outside root: {built[k]}'\n"
            "    assert built.get('PYTHONDONTWRITEBYTECODE') == '1'\n"
            "    assert os.environ.get('GEMINI_API_KEY') == 'hostile-test-key'\n"
            "    written = inner.write_config(cfg, mode=0o600)\n"
            "    if os.name != 'nt':\n"
            "        assert stat.S_IMODE(os.lstat(written).st_mode) == 0o600\n"
            "assert not root.exists(), f'root not cleaned: {root}'\n"
            "print(json.dumps({'ok': True}))\n"
        )
        res = run_cli([sys.executable, "-B", "-c", child_script], env=child_env, timeout=10.0)
        self.assertEqual(res.returncode, 0, f"stdout={res.stdout}\nstderr={res.stderr}")
        self.assertIs(res.json_stdout().get("ok"), True)

    def test_self_02_pidfile_reaper_lifo_cleanup_and_unsafe_refusal(self) -> None:
        """PidfileReaper terminates matching detached sleeper in LIFO order and refuses unsafe targets."""
        if os.name == "nt":
            self.skipTest("POSIX pidfile reaper test")

        token1 = f"agy-self-reap-lifo-{os.getpid()}-{time.monotonic_ns()}"
        sleeper1 = subprocess.Popen(
            [sys.executable, "-B", "-c", "import time; time.sleep(30)", token1],
            start_new_session=True,
        )
        try:
            with contextlib.ExitStack() as stack:
                iso = stack.enter_context(IsolatedEnv())
                register_pidfile_cleanup(
                    stack,
                    iso.data_dir / "proxy.pid",
                    expected_cmdline_substr=token1,
                    timeout=2.0,
                )
                (iso.data_dir / "proxy.pid").write_text(
                    json.dumps({"pid": sleeper1.pid}), encoding="utf-8"
                )
            self.assertIsNotNone(sleeper1.wait(timeout=2.0))
        finally:
            if sleeper1.poll() is None:
                sleeper1.kill()
                sleeper1.wait(timeout=2.0)

        token2 = f"agy-self-reap-guard-{os.getpid()}-{time.monotonic_ns()}"
        sleeper2 = subprocess.Popen(
            [sys.executable, "-B", "-c", "import time; time.sleep(30)", token2],
            start_new_session=True,
        )
        try:
            iso = self.make_env()
            pid_file = iso.data_dir / "proxy.pid"
            pid_file.write_text(json.dumps({"pid": sleeper2.pid}), encoding="utf-8")

            self.assertFalse(reap_pidfile_daemon(pid_file, expected_cmdline_substr="wrong-token"))
            self.assertFalse(reap_pidfile_daemon(pid_file))

            sym = iso.data_dir / "proxy_sym.pid"
            os.symlink(pid_file, sym)
            self.assertFalse(reap_pidfile_daemon(sym, expected_cmdline_substr=token2))

            bad_file = iso.data_dir / "bad.pid"
            for bad_raw in (
                json.dumps({"pid": 0}),
                json.dumps({"pid": 1}),
                json.dumps({"pid": os.getpid()}),
                json.dumps({"pid": os.getppid()}),
                json.dumps({"pid": True}),
                json.dumps({"pid": 2**31}),
                "{not-json",
            ):
                bad_file.write_text(bad_raw, encoding="utf-8")
                self.assertFalse(reap_pidfile_daemon(bad_file, expected_cmdline_substr=token2))

            self.assertIsNone(sleeper2.poll(), "Sleeper must survive all unsafe refusal checks")
            self.assertTrue(reap_pidfile_daemon(pid_file, expected_cmdline_substr=token2, timeout=2.0))
            self.assertIsNotNone(sleeper2.wait(timeout=2.0))
            self.assertFalse(reap_pidfile_daemon(pid_file, expected_cmdline_substr=token2))
        finally:
            if sleeper2.poll() is None:
                sleeper2.kill()
                sleeper2.wait(timeout=2.0)

    def test_self_03_isolated_env_reaps_detached_daemon_without_pidfile_on_close(self) -> None:
        """IsolatedEnv.close() reaps detached env-scoped daemons writing files without ENOTEMPTY."""
        if os.name == "nt":
            self.skipTest("POSIX env-daemon reaper test")

        unrelated_token = f"agy-self-unrelated-{os.getpid()}-{time.monotonic_ns()}"
        unrelated = subprocess.Popen(
            [sys.executable, "-B", "-c", "import time; time.sleep(30)", "agy-companion", "serve", unrelated_token],
            start_new_session=True,
        )
        writer: subprocess.Popen[bytes] | None = None
        try:
            iso = IsolatedEnv()
            root = iso.root
            log_path = iso.data_dir / "proxy.log"
            log_path.write_text("started\n", encoding="utf-8")
            writer_code = (
                "import pathlib, sys, time\n"
                "d = pathlib.Path(sys.argv[1])\n"
                "i = 0\n"
                "while True:\n"
                "    try:\n"
                "        (d / f'churn-{i % 4}.tmp').write_text('x', encoding='utf-8')\n"
                "    except OSError:\n"
                "        pass\n"
                "    i += 1\n"
                "    time.sleep(0.005)\n"
            )
            # Simulate a detached `agy-companion serve --config <root>/skew/config.json`
            # process actively creating files in `iso.data_dir` without a `proxy.pid`.
            writer = subprocess.Popen(
                [
                    sys.executable,
                    "-B",
                    "-c",
                    writer_code,
                    str(iso.data_dir),
                    "agy-companion",
                    "serve",
                    "--config",
                    str(root / "skew" / "config.json"),
                ],
                start_new_session=True,
            )
            time.sleep(0.05)
            self.assertIsNone(writer.poll())

            iso.close()
            self.assertIsNotNone(writer.wait(timeout=2.0))
            self.assertFalse(root.exists(), f"IsolatedEnv root should be cleanly removed: {root}")
            self.assertIsNone(unrelated.poll(), "Unrelated process must not be signaled")
            self.assertEqual(reap_env_daemons(root, expected_cmdline_substr=unrelated_token), 0)
        finally:
            for proc in (writer, unrelated):
                if proc is not None and proc.poll() is None:
                    proc.kill()
                    proc.wait(timeout=2.0)
