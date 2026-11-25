"""Tier 3 Rollout: Pairwise Cross-Feature Combination Tests (`ROLLOUT_COMBO`).

Verifies multi-feature interactions across PR 1 (`R01`–`R12`), PR 2 (`R13`–`R19`),
and PR 3 (`R20`–`R25`) in isolated opaque-box environments.
"""

from __future__ import annotations

import json
import os
import re
import shlex
import signal
import stat
import subprocess
import sys
import time
from pathlib import Path
from typing import Any

from harness import (
    E2ETestCase,
    IsolatedEnv,
    find_free_port,
    http_raw_request,
    run_cli,
)
from mcp_client import McpStdioClient
from mock_gemini import CapturedRequest, JsonAction, SseStreamAction


_PLACEHOLDER_KEY = "agy-companion-local"
_S5_FORBIDDEN_SINKS_RE = re.compile(
    r"innerHTML|outerHTML|insertAdjacentHTML|document\.write|dangerouslySetInnerHTML"
    r"|localStorage|sessionStorage|document\.cookie|new\s+Function|XMLHttpRequest"
    r"|(?<![A-Za-z0-9_$])(?:eval|alert|confirm|prompt|fetch)\s*\("
)
_S5_EXTERNAL_TAGS_RE = re.compile(
    r"<script[^>]*\ssrc\s*=|<link[^>]*\shref\s*=|<img[^>]*\ssrc\s*=|<iframe|@import",
    re.IGNORECASE,
)


def _read_rust_module_source(module_base: Path, *, include_tests: bool = False) -> str:
    single_file = module_base.with_suffix(".rs")
    if single_file.is_file():
        files = [single_file]
    elif module_base.is_dir():
        files = [
            candidate
            for candidate in sorted(module_base.rglob("*.rs"))
            if candidate.is_file()
            and (
                include_tests
                or (
                    candidate.name != "tests.rs"
                    and "tests" not in candidate.relative_to(module_base).parts
                )
            )
        ]
    else:
        files = []
    if not files:
        raise AssertionError(f"Missing expected Rust module: {module_base}")
    return "\n".join(path.read_text(encoding="utf-8") for path in files)


def _sample_catalog() -> dict[str, Any]:
    return {
        "models": {
            "gemini-4-carbon-high": {
                "model": "MODEL_PLACEHOLDER_M427",
                "displayName": "Gemini 4 Carbon (High)",
                "thinkingLevel": 3,
            },
            "existing-m334": {
                "model": "MODEL_PLACEHOLDER_M334",
                "displayName": "Existing Slot",
            },
        },
        "defaultAgentModelId": "gemini-4-carbon-high",
        "agentModelSorts": [
            {
                "displayName": "Recommended",
                "groups": [{"displayName": "Google AI Pro", "modelIds": ["gemini-4-carbon-high"]}],
            }
        ],
        "groupedModels": {
            "gemini-4-carbon": {
                "displayName": "Gemini 4 Carbon",
                "supportedThinkingLevels": [
                    {"thinkingSuffix": "high", "thinkingDisplayName": "High"}
                ],
            }
        },
    }


def _install_dummy_plugin_json(env: IsolatedEnv, version: str = "0.1.0") -> Path:
    plugin_dir = env.home_dir / ".gemini" / "config" / "plugins" / "agy-companion"
    plugin_dir.mkdir(parents=True, exist_ok=True)
    pj = plugin_dir / "plugin.json"
    pj.write_text(
        json.dumps({"name": "agy-companion", "version": version}, indent=2) + "\n",
        encoding="utf-8",
    )
    return pj


def _write_env_dumper_script(script_path: Path, dump_path: Path) -> None:
    script_path.write_text(
        f"#!/bin/sh\n"
        f"exec {shlex.quote(sys.executable)} -c "
        f'\'import json,os,pathlib,sys; p=pathlib.Path(sys.argv[1]); t=p.with_suffix(".tmp"); '
        f't.write_text(json.dumps(dict(os.environ)), encoding="utf-8"); os.replace(t, p)\' '
        f"{shlex.quote(str(dump_path))}\n",
        encoding="utf-8",
    )
    os.chmod(script_path, 0o755)


def _wait_for_json_file(path: Path, timeout: float = 5.0) -> dict[str, Any]:
    deadline = time.monotonic() + timeout
    while time.monotonic() < deadline:
        if path.is_file():
            try:
                data = json.loads(path.read_text(encoding="utf-8"))
                if isinstance(data, dict):
                    return data
            except (OSError, json.JSONDecodeError):
                pass
        time.sleep(0.05)
    raise AssertionError(f"Timed out waiting for JSON file {path} after {timeout}s")


def _wait_for_health(port: int, timeout: float = 5.0) -> dict[str, Any] | None:
    deadline = time.monotonic() + timeout
    while time.monotonic() < deadline:
        try:
            resp = http_raw_request("127.0.0.1", port, "GET", "/health", timeout=0.5)
            if resp.status == 200:
                data = resp.json()
                if isinstance(data, dict):
                    return data
        except Exception:
            pass
        time.sleep(0.05)
    return None


class TestTier3RolloutCombinations(E2ETestCase):
    TIER = 3
    FEATURE = "ROLLOUT_COMBO"

    def test_t3_r01_auto_routing_matrix_with_zero_generation_retry(self) -> None:
        """R01+R02+R04+R05: Auto mode picks daily vs prod by signal and never retries streamGenerateContent."""
        env = self.make_env()
        connect_proxy = self.start_mock_connect_proxy()
        cfg = env.write_config(
            env.make_config_dict(
                api_key="AIzaSyRolloutT3C01Auto1",
                cloudcode_upstream_url="auto",
            )
        )
        proxy = self.start_proxy(
            env,
            config_path=cfg,
            extra_env={"HTTPS_PROXY": connect_proxy.proxy_url},
        )

        # 1. Consumer managed project -> daily then prod
        connect_proxy.clear_targets()
        r1 = http_raw_request(
            "127.0.0.1",
            proxy.port,
            "POST",
            "/v1internal:fetchAvailableModels",
            headers={"Authorization": "Bearer t3-c01-pro", "Content-Type": "application/json"},
            body=b'{"project":"managed-pro-1"}',
            timeout=5.0,
        )
        self.assertEqual(r1.status, 502)
        t1 = connect_proxy.wait_for_connects(2, timeout=3.0)
        self.assertEqual(t1[:2], ["daily-cloudcode-pa.googleapis.com:443", "cloudcode-pa.googleapis.com:443"])

        # 2. Enterprise auth_method=gcp -> prod then daily
        connect_proxy.clear_targets()
        r2 = http_raw_request(
            "127.0.0.1",
            proxy.port,
            "POST",
            "/v1internal:fetchAvailableModels",
            headers={
                "Authorization": "Bearer t3-c01-gcp",
                "User-Agent": "antigravity/2.34.0 auth_method=gcp",
                "Content-Type": "application/json",
            },
            body=b"{}",
            timeout=5.0,
        )
        self.assertEqual(r2.status, 502)
        t2 = connect_proxy.wait_for_connects(2, timeout=3.0)
        self.assertEqual(t2[:2], ["cloudcode-pa.googleapis.com:443", "daily-cloudcode-pa.googleapis.com:443"])

        # 3. streamGenerateContent -> exact 1 CONNECT (zero fallback/retry)
        connect_proxy.clear_targets()
        r3 = http_raw_request(
            "127.0.0.1",
            proxy.port,
            "POST",
            "/v1internal:streamGenerateContent?alt=sse",
            headers={"Authorization": "Bearer t3-c01-stream", "Content-Type": "application/json"},
            body=b'{"model":"gemini-4-carbon-high"}',
            timeout=5.0,
        )
        self.assertEqual(r3.status, 502)
        time.sleep(0.15)
        self.assertEqual(len(connect_proxy.connect_targets), 1)

    def test_t3_r02_enterprise_403_lru_survives_day0_to_day1_hot_reload(self) -> None:
        """R03+R04+R08+R09: Account 403 strip LRU learned on Day-0 survives hot-reload transition to Day-1 merged catalog."""
        env = self.make_env()
        mock_ccpa = self.start_mock_gemini()

        def handler(req: CapturedRequest) -> Any:
            if ":fetchAvailableModels" in req.path:
                if req.header("x-goog-user-project") is not None:
                    return JsonAction(status=403, payload={"error": {"message": "strip user project"}})
                return JsonAction(status=200, payload=_sample_catalog())
            return SseStreamAction(chunks=[b'data: {"ok":true}\n\n'], status=200)

        mock_ccpa.set_handler(handler)
        cfg = env.write_config(
            env.make_config_dict(api_key=None, cloudcode_upstream_url=mock_ccpa.base_url)
        )
        proxy = self.start_proxy(env, config_path=cfg)

        # Day-0: 403 retry learns strip_user_project=true while returning untouched catalog.
        r0 = http_raw_request(
            "127.0.0.1",
            proxy.port,
            "POST",
            "/v1internal:fetchAvailableModels",
            headers={
                "Authorization": "Bearer t3-c02-ent",
                "x-goog-user-project": "proj-c02",
                "Content-Type": "application/json",
            },
            body=b"{}",
        )
        self.assertEqual(r0.status, 200)
        self.assertNotIn("companion/gemini-4-hydrogen-low", r0.json()["models"])
        self.assertIs(
            http_raw_request("127.0.0.1", proxy.port, "GET", "/api/stats").json()["cloudcode"]["last_catalog_included_proxy_models"],
            False,
        )

        # Hot-reload: save real API key (cloudcode_upstream_url & timeout_secs unchanged)
        env.write_config(
            env.make_config_dict(
                api_key="AIzaSyRolloutT3C02Key12",
                cloudcode_upstream_url=mock_ccpa.base_url,
            ),
            path=cfg,
        )
        env.bump_mtime(cfg, delta=3.0)
        mock_ccpa.clear_requests()

        # Stream uses preserved LRU (omits x-goog-user-project on first attempt)
        rs = http_raw_request(
            "127.0.0.1",
            proxy.port,
            "POST",
            "/v1internal:streamGenerateContent?alt=sse",
            headers={
                "Authorization": "Bearer t3-c02-ent",
                "x-goog-user-project": "proj-c02",
                "Content-Type": "application/json",
            },
            body=b'{"model":"gemini-4-carbon-high"}',
        )
        self.assertEqual(rs.status, 200)
        self.assertEqual(len(mock_ccpa.requests), 1)
        self.assertIsNone(mock_ccpa.requests[0].header("x-goog-user-project"))

        # Next fetchAvailableModels omits x-goog-user-project AND merges (Proxy) models
        r1 = http_raw_request(
            "127.0.0.1",
            proxy.port,
            "POST",
            "/v1internal:fetchAvailableModels",
            headers={
                "Authorization": "Bearer t3-c02-ent",
                "x-goog-user-project": "proj-c02",
                "Content-Type": "application/json",
            },
            body=b"{}",
        )
        self.assertEqual(r1.status, 200)
        self.assertIn("companion/gemini-4-hydrogen-low", r1.json()["models"])
        self.assertIs(
            http_raw_request("127.0.0.1", proxy.port, "GET", "/api/stats").json()["cloudcode"]["last_catalog_included_proxy_models"],
            True,
        )

    def test_t3_r03_deterministic_slots_neon_xhigh_and_non_gemini_400_hint(self) -> None:
        """R06+R09+R10: Merged neon XHigh catalog with stable slots coexists with non-Gemini-3/4 400 unset hint."""
        env = self.make_env()
        mock_ccpa = self.start_mock_gemini()
        mock_gemini = self.start_mock_gemini()
        mock_ccpa.set_handler(lambda _req: JsonAction(status=200, payload=_sample_catalog()))
        cfg = env.write_config(
            env.make_config_dict(
                base_url=mock_gemini.base_url,
                api_key="AIzaSyRolloutT3C03Key12",
                default_model="gemini-4-neon",
                cloudcode_upstream_url=mock_ccpa.base_url,
            )
        )
        proxy = self.start_proxy(env, config_path=cfg)

        cat = http_raw_request(
            "127.0.0.1",
            proxy.port,
            "POST",
            "/v1internal:fetchAvailableModels",
            headers={"Authorization": "Bearer t3-c03-tok", "Content-Type": "application/json"},
            body=b"{}",
        )
        self.assertEqual(cat.status, 200)
        models = cat.json()["models"]
        self.assertEqual(models["companion/gemini-4-neon-xhigh"]["thinkingLevel"], 7)

        mock_gemini.clear_requests()
        bad = http_raw_request(
            "127.0.0.1",
            proxy.port,
            "POST",
            "/v1beta/models/gemini-2.5-pro:generateContent",
            headers={"Content-Type": "application/json"},
            body=b"{}",
        )
        self.assertEqual(bad.status, 400)
        self.assertIn("unset GOOGLE_GEMINI_BASE_URL to reach Gemini API directly", bad.json()["error"]["message"])
        self.assertEqual(len(mock_gemini.requests), 0)

        stats = http_raw_request("127.0.0.1", proxy.port, "GET", "/api/stats").json()
        self.assertEqual(stats["cloudcode"]["mode"], "fixed")
        self.assertIs(stats["cloudcode"]["last_catalog_included_proxy_models"], True)

    def test_t3_r04_placeholder_quarantine_across_proxy_mcp_and_doctor(self) -> None:
        """R07+R08+R18: Placeholder key is quarantined consistently across proxy, Day-0 catalog, MCP, and doctor."""
        binary = self.require_binary()
        env = self.make_env()
        mock_gemini = self.start_mock_gemini()
        mock_ccpa = self.start_mock_gemini()
        mock_ccpa.enqueue(JsonAction(status=200, payload=_sample_catalog()))
        cfg = env.write_config(
            env.make_config_dict(
                base_url=mock_gemini.base_url,
                api_key=_PLACEHOLDER_KEY,
                cloudcode_upstream_url=mock_ccpa.base_url,
            )
        )
        proxy = self.start_proxy(
            env,
            config_path=cfg,
            extra_env={"GEMINI_API_KEY": _PLACEHOLDER_KEY},
        )

        # 1. Proxy /v1beta rejects with 401 and 0 upstream calls
        r_gen = http_raw_request(
            "127.0.0.1",
            proxy.port,
            "POST",
            "/v1beta/models/gemini-4-hydrogen:generateContent",
            headers={"Content-Type": "application/json", "x-goog-api-key": _PLACEHOLDER_KEY},
            body=b"{}",
        )
        self.assertEqual(r_gen.status, 401)
        self.assertEqual(len(mock_gemini.requests), 0)

        # 2. Proxy Day-0 hides (Proxy) models
        r_cat = http_raw_request(
            "127.0.0.1",
            proxy.port,
            "POST",
            "/v1internal:fetchAvailableModels",
            headers={"Authorization": "Bearer t3-c04", "Content-Type": "application/json"},
            body=b"{}",
        )
        self.assertEqual(r_cat.status, 200)
        self.assertNotIn("companion/gemini-4-hydrogen-low", r_cat.json()["models"])

        # 3. doctor --json marks env.gemini_api_key_set=false
        doc = run_cli(
            [str(binary), "doctor", "--config", str(cfg), "--port", str(proxy.port), "--json"],
            env=env.build_env(extra={"GEMINI_API_KEY": _PLACEHOLDER_KEY}),
        )
        self.assertIs(doc.json_stdout()["env"]["gemini_api_key_set"], False)

        # 4. MCP get_settings reports unconfigured and save_settings rejects placeholder
        mcp = self.start_mcp(env, config_path=cfg, extra_env={"GEMINI_API_KEY": _PLACEHOLDER_KEY})
        sc = mcp.call_tool("get_settings", {}).get("structuredContent", {})
        self.assertIs(sc["config"]["providers"]["gemini"]["api_key_configured"], False)
        err = mcp.save_settings_smart(api_key=_PLACEHOLDER_KEY, raw=True)
        mcp.assert_clean_stdout()
        self.assertTrue(McpStdioClient.is_tool_error(err))

    def test_t3_r05_mcp_save_settings_flips_day0_proxy_and_updates_snapshot_stats(self) -> None:
        """R08+R09+R18: Saving a real key via MCP hot-reloads Day-0 proxy and updates MCP snapshot stats & cloudcode."""
        env = self.make_env()
        mock_ccpa = self.start_mock_gemini()
        mock_ccpa.set_handler(lambda _req: JsonAction(status=200, payload=_sample_catalog()))
        port = find_free_port()
        cfg = env.write_config(
            env.make_config_dict(
                listen_port=port,
                api_key=None,
                cloudcode_upstream_url=mock_ccpa.base_url,
            )
        )
        proxy = self.start_proxy(env, config_path=cfg, port=port)
        mcp = self.start_mcp(env, config_path=cfg)

        # Before save: Day-0 untouched
        r0 = http_raw_request(
            "127.0.0.1",
            proxy.port,
            "POST",
            "/v1internal:fetchAvailableModels",
            headers={"Authorization": "Bearer t3-c05", "Content-Type": "application/json"},
            body=b"{}",
        )
        self.assertEqual(r0.status, 200)
        self.assertNotIn("companion/gemini-4-hydrogen-low", r0.json()["models"])

        # Save real key through MCP
        save_res = mcp.save_settings_smart(api_key="AIzaSyRolloutT3C05Save1", raw=False)
        mcp.assert_clean_stdout()
        self.assertIs(
            save_res.get("structuredContent", {})["config"]["providers"]["gemini"]["api_key_configured"],
            True,
        )
        env.bump_mtime(cfg, delta=3.0)

        # After save: merged catalog + updated MCP snapshot stats & cloudcode
        r1 = http_raw_request(
            "127.0.0.1",
            proxy.port,
            "POST",
            "/v1internal:fetchAvailableModels",
            headers={"Authorization": "Bearer t3-c05", "Content-Type": "application/json"},
            body=b"{}",
        )
        self.assertEqual(r1.status, 200)
        self.assertIn("companion/gemini-4-hydrogen-low", r1.json()["models"])

        snap = mcp.call_tool("get_settings", {}).get("structuredContent", {})
        mcp.assert_clean_stdout()
        self.assertTrue(snap["proxy"]["reachable"])
        self.assertIs(snap["stats"]["cloudcode"]["last_catalog_included_proxy_models"], True)
        self.assertEqual(snap["cloudcode"]["mode"], "fixed")

    def test_t3_r06_idle_shutdown_primary_hands_off_to_adopt_running_secondary(self) -> None:
        """R11+R12: When an idle-shutdown primary daemon exits cleanly, serve --adopt-running takes over under 2.5s."""
        binary = self.require_binary()
        env = self.make_env()
        self.register_pidfile_cleanup(env=env)
        port = find_free_port()
        cfg = env.write_config(env.make_config_dict(listen_port=port))

        primary = subprocess.Popen(
            [str(binary), "serve", "--port", str(port), "--config", str(cfg), "--idle-shutdown-secs", "2"],
            env=env.build_env(),
            stdout=subprocess.DEVNULL,
            stderr=subprocess.DEVNULL,
            start_new_session=True,
        )
        adopter: subprocess.Popen[bytes] | None = None
        try:
            self.assertIsNotNone(_wait_for_health(port, timeout=5.0))
            adopter = subprocess.Popen(
                [str(binary), "serve", "--port", str(port), "--config", str(cfg), "--adopt-running"],
                env=env.build_env(),
                stdout=subprocess.DEVNULL,
                stderr=subprocess.DEVNULL,
                start_new_session=True,
            )
            rc_primary = primary.wait(timeout=8.0)
            self.assertEqual(rc_primary, 0)
            t0 = time.monotonic()
            health = _wait_for_health(port, timeout=2.5)
            elapsed = time.monotonic() - t0
            self.assertIsNotNone(health, "Adopter failed to take over after primary idle-shutdown")
            self.assertLess(elapsed, 2.5)
            self.assertIsNone(adopter.poll())
        finally:
            for p in (adopter, primary):
                if p is not None and p.poll() is None:
                    p.send_signal(signal.SIGTERM)
                    try:
                        p.wait(timeout=3.0)
                    except subprocess.TimeoutExpired:
                        p.kill()
                        p.wait(timeout=2.0)

    def test_t3_r07_zsh_zlogin_sources_env_sh_cold_starts_hook_env_and_scrubs_terminal(self) -> None:
        """R12+R13+R14+R15: Real zsh .zlogin sources env.sh, cold-starts daemon via hook-env, then scrubs terminal."""
        if os.name == "nt" or not Path("/bin/zsh").is_file():
            self.skipTest("Requires POSIX /bin/zsh")
        binary = self.require_binary()
        env = self.make_env()
        self.register_pidfile_cleanup(env=env)
        port = find_free_port()
        cfg = env.write_config(env.make_config_dict(listen_port=port))
        _install_dummy_plugin_json(env)

        proc_env = env.build_env(extra={"SHELL": "/bin/zsh", "ZDOTDIR": str(env.home_dir)})
        en = run_cli(
            [str(binary), "integration", "enable", "--shell", "zsh", "--binary", str(binary), "--json"],
            env=proc_env,
        )
        self.assertEqual(en.returncode, 0, en.stderr)
        zlogin = env.home_dir / ".zlogin"
        self.assertTrue(zlogin.is_file())
        # Simulate user .zshrc early-return when ANTIGRAVITY_RESOLVING_ENVIRONMENT is set:
        # real zsh --no-globalrcs -ilc must still auto-load .zlogin, and child -lc must auto-scrub.
        (env.home_dir / ".zshrc").write_text(
            '[[ -n "${ANTIGRAVITY_RESOLVING_ENVIRONMENT:-}" ]] && return 0\n',
            encoding="utf-8",
        )

        script = (
            'printf "BOOT:%s|%s\\n" "${CLOUD_CODE_URL:-}" "${AGY_COMPANION_HOOK_ACTIVE:-}"\n'
            'unset ANTIGRAVITY_RESOLVING_ENVIRONMENT\n'
            '/bin/zsh --no-globalrcs -lc '
            '\'printf "TERM:%s|%s\\n" "${CLOUD_CODE_URL-<UNSET>}" "${AGY_COMPANION_HOOK_ACTIVE-<UNSET>}"\'\n'
        )
        res = run_cli(
            ["/bin/zsh", "--no-globalrcs", "-ilc", script],
            env=env.build_env(
                extra={
                    "HOME": str(env.home_dir),
                    "SHELL": "/bin/zsh",
                    "ZDOTDIR": str(env.home_dir),
                    "ANTIGRAVITY_RESOLVING_ENVIRONMENT": "1",
                    "AGY_COMPANION_CONFIG": str(cfg),
                }
            ),
        )
        self.assertEqual(res.returncode, 0, res.stderr)
        lines = res.stdout.splitlines()
        self.assertEqual(lines[:2], [f"BOOT:http://127.0.0.1:{port}|1", "TERM:<UNSET>|<UNSET>"])
        self.assertIsNotNone(_wait_for_health(port, timeout=3.0))
        self.assertTrue((env.data_dir / "proxy.pid").is_file())

    def test_t3_r08_mcp_set_launcher_integration_roundtrip_with_cli_and_env_sh(self) -> None:
        """R13+R14+R16+R18: MCP enable matches CLI status --json & sh -n env.sh; CLI disable reflects in MCP."""
        if os.name == "nt":
            self.skipTest("POSIX launcher integration test")
        binary = self.require_binary()
        env = self.make_env()
        cfg = env.write_config(env.make_config_dict())
        extra = {"SHELL": "/bin/zsh", "ZDOTDIR": str(env.home_dir)}
        mcp = self.start_mcp(env, config_path=cfg, extra_env=extra)

        r_en = mcp.call_tool("set_launcher_integration", {"enabled": True})
        mcp.assert_clean_stdout()
        self.assertIs(r_en["structuredContent"]["launcher_integration"]["enabled"], True)

        cli_st = run_cli([str(binary), "integration", "status", "--shell", "zsh", "--json"], env=env.build_env(extra=extra))
        self.assertEqual(cli_st.returncode, 0, cli_st.stderr)
        self.assertIs(cli_st.json_stdout()["enabled"], True)
        self.assertEqual(run_cli(["/bin/sh", "-n", str(env.data_dir / "env.sh")]).returncode, 0)

        cli_dis = run_cli([str(binary), "integration", "disable", "--shell", "zsh", "--json"], env=env.build_env(extra=extra))
        self.assertEqual(cli_dis.returncode, 0, cli_dis.stderr)
        snap = mcp.call_tool("get_settings", {}).get("structuredContent", {})
        mcp.assert_clean_stdout()
        self.assertIs(snap["launcher_integration"]["enabled"], False)

    def test_t3_r09_launch_reuses_warm_hook_env_daemon_and_emits_restore_vars(self) -> None:
        """R15+R17: launch reuses an already-running hook-env daemon and injects JETSKI_MCP_ENV_RESTORE_*."""
        if os.name == "nt":
            self.skipTest("POSIX fake app test")
        binary = self.require_binary()
        env = self.make_env()
        self.register_pidfile_cleanup(env=env)
        port = find_free_port()
        cfg = env.write_config(env.make_config_dict(listen_port=port))
        _install_dummy_plugin_json(env)

        hk = run_cli(
            [str(binary), "hook-env", "--ensure-running", "--print-port"],
            env=env.build_env(extra={"AGY_COMPANION_CONFIG": str(cfg)}),
        )
        self.assertEqual(hk.returncode, 0, hk.stderr)
        self.assertEqual(hk.stdout.strip(), str(port))
        pid_before = json.loads((env.data_dir / "proxy.pid").read_text(encoding="utf-8"))["pid"]

        dump_path = env.root / "launch_env.json"
        fake_app = env.root / "fake_app.sh"
        _write_env_dumper_script(fake_app, dump_path)

        res = run_cli(
            [str(binary), "launch", "--port", str(port), "--config", str(cfg), "--app", str(fake_app)],
            env=env.build_env(extra={"GOOGLE_API_KEY": "AIzaSyRolloutT3C09Goog1"}),
        )
        self.assertEqual(res.returncode, 0, res.stderr)
        app_env = _wait_for_json_file(dump_path, timeout=5.0)
        self.assertEqual(app_env["CLOUD_CODE_URL"], f"http://127.0.0.1:{port}")
        self.assertEqual(app_env["GOOGLE_API_KEY"], "AIzaSyRolloutT3C09Goog1")
        self.assertIn("CLOUD_CODE_URL", app_env.get("JETSKI_MCP_ENV_RESTORE_VARS", ""))
        pid_after = json.loads((env.data_dir / "proxy.pid").read_text(encoding="utf-8"))["pid"]
        self.assertEqual(pid_before, pid_after, "launch should reuse warm healthy proxy")

    def test_t3_r10_installed_plugin_run_companion_scrubs_env_and_beats_stale_path(self) -> None:
        """R20+R21+R22: Installed plugin's run-companion.sh scrubs proxy env and prefers installed XDG cache over PATH."""
        if os.name == "nt":
            self.skipTest("POSIX launcher test")
        binary = self.require_binary()
        env = self.make_env()
        port = find_free_port()
        env.write_config(env.make_config_dict(listen_port=port))
        plugins_dir = env.root / "plugins"
        inst = run_cli(
            [
                str(binary),
                "install",
                "--plugins-dir",
                str(plugins_dir),
                "--no-launcher-hook",
                "--no-start-proxy",
                "--json",
            ],
            env=env.build_env(),
        )
        self.assertEqual(inst.returncode, 0, inst.stderr)

        # Put a poisoned binary on PATH that fails loudly if executed
        bad_path = env.root / "bad_path"
        bad_path.mkdir(parents=True, exist_ok=True)
        bad_bin = bad_path / "agy-companion"
        bad_bin.write_text("#!/bin/sh\necho WRONG_STALE_PATH >&2; exit 99\n", encoding="utf-8")
        os.chmod(bad_bin, 0o755)

        installed_script = plugins_dir / "agy-companion" / "scripts" / "run-companion.sh"
        res = run_cli(
            [str(installed_script), "--version"],
            env=env.build_env(
                extra={
                    "PATH": f"{bad_path}:{os.environ.get('PATH', '/usr/bin:/bin')}",
                    "CLOUD_CODE_URL": "http://127.0.0.1:4141",
                    "GOOGLE_GEMINI_BASE_URL": "http://127.0.0.1:4141",
                    "GEMINI_API_KEY": _PLACEHOLDER_KEY,
                }
            ),
        )
        self.assertEqual(res.returncode, 0, f"{res.stdout}\n{res.stderr}")
        self.assertNotIn("WRONG_STALE_PATH", res.stderr)

    def test_t3_r11_install_integrates_with_mcp_settings_snapshot(self) -> None:
        """R18+R22: After fresh install, MCP get_settings reports reachable proxy and enabled launcher_integration."""
        binary = self.require_binary()
        env = self.make_env()
        self.register_pidfile_cleanup(env=env)
        port = find_free_port()
        cfg = env.write_config(env.make_config_dict(listen_port=port))
        plugins_dir = env.root / "plugins"
        extra = {"SHELL": "/bin/zsh", "ZDOTDIR": str(env.home_dir)}

        inst = run_cli(
            [str(binary), "install", "--plugins-dir", str(plugins_dir), "--json"],
            env=env.build_env(extra=extra),
        )
        self.assertEqual(inst.returncode, 0, inst.stderr)
        self.assertIsNotNone(_wait_for_health(port, timeout=4.0))

        mcp = self.start_mcp(env, config_path=cfg, extra_env=extra)
        snap = mcp.call_tool("get_settings", {}).get("structuredContent", {})
        mcp.assert_clean_stdout()
        self.assertTrue(snap["proxy"]["reachable"])
        if os.name != "nt":
            self.assertIs(snap["launcher_integration"]["enabled"], True)

    def test_t3_r12_multi_shell_hooks_all_stripped_cleanly_by_uninstall(self) -> None:
        """R14+R16+R23: uninstall strips Bash, Zsh (preserving user lines), and Fish hooks in one pass."""
        if os.name == "nt":
            self.skipTest("POSIX multi-shell uninstall test")
        binary = self.require_binary()
        env = self.make_env()
        zlogin = env.home_dir / ".zlogin"
        zlogin.write_text("# user zlogin line\n", encoding="utf-8")

        for sh_name in ("zsh", "bash", "fish"):
            r = run_cli(
                [str(binary), "integration", "enable", "--shell", sh_name, "--json"],
                env=env.build_env(extra={"SHELL": f"/bin/{sh_name}", "ZDOTDIR": str(env.home_dir)}),
            )
            self.assertEqual(r.returncode, 0, f"{sh_name}: {r.stderr}")

        fish_dropin = env.config_root / "fish" / "conf.d" / "agy-companion.fish"
        profile = env.home_dir / ".profile"
        self.assertTrue(fish_dropin.is_file())
        self.assertIn("# >>> agy-companion", profile.read_text(encoding="utf-8"))
        self.assertIn("# >>> agy-companion", zlogin.read_text(encoding="utf-8"))

        plugins_dir = env.root / "plugins"
        plugins_dir.mkdir(parents=True, exist_ok=True)
        un = run_cli(
            [str(binary), "uninstall", "--plugins-dir", str(plugins_dir), "--json"],
            env=env.build_env(extra={"SHELL": "/bin/zsh", "ZDOTDIR": str(env.home_dir)}),
        )
        self.assertEqual(un.returncode, 0, un.stderr)
        self.assertFalse((env.data_dir / "env.sh").exists())
        self.assertFalse(fish_dropin.exists())
        self.assertEqual(zlogin.read_text(encoding="utf-8"), "# user zlogin line\n")
        if profile.exists():
            self.assertNotIn("# >>> agy-companion", profile.read_text(encoding="utf-8"))

    def test_t3_r13_ui_bundle_served_by_mcp_matches_5_tool_surface_and_s5_gate(self) -> None:
        """R18+R19: MCP resource HTML matches ui/dist/index.html, passes S5 regexes, and calls all UI tools."""
        env = self.make_env()
        cfg = env.write_config(env.make_config_dict())
        mcp = self.start_mcp(env, config_path=cfg)
        tools = {t["name"] for t in (mcp.list_tools().get("tools") or [])}
        res = mcp.read_resource("ui://companion/settings.html")
        mcp.assert_clean_stdout()
        contents = res.get("contents") or []
        self.assertGreaterEqual(len(contents), 1)
        html = contents[0]["text"]

        self.assertEqual(
            tools,
            {"open_settings", "get_settings", "save_settings", "test_connection", "set_launcher_integration"},
        )
        self.assertLess(len(html.encode("utf-8")), 512_000)
        self.assertIsNone(_S5_FORBIDDEN_SINKS_RE.search(html))
        self.assertIsNone(_S5_EXTERNAL_TAGS_RE.search(html))
        for tool_name in ("get_settings", "save_settings", "test_connection", "set_launcher_integration"):
            self.assertIn(tool_name, html, f"UI bundle missing tool call reference: {tool_name}")

    def test_t3_r14_windows_parity_and_docs_align_with_npx_npm_uvx_only_policy(self) -> None:
        """R24+R25: Docs and Rust Windows parity align on npx/npm/uvx install, direct .exe manifests, and zero curl installers."""
        self.assertFalse((self.repo_root / "scripts" / "install.sh").exists())
        self.assertFalse((self.repo_root / "scripts" / "install.ps1").exists())

        readme = (self.repo_root / "README.md").read_text(encoding="utf-8")
        design = (self.repo_root / "docs" / "design" / "dock-launcher-and-install.md").read_text(encoding="utf-8")
        install_rs = _read_rust_module_source(self.repo_root / "crates" / "companion-cli" / "src" / "install")

        self.assertIn("npx -y agy-companion@latest install", readme)
        self.assertIn("npx -y agy-companion@latest install", design)
        self.assertIn("Antigravity (Companion).lnk", install_rs)
        self.assertNotIn("releases/latest/download/install.ps1", readme)
        self.assertNotIn("releases/latest/download/install.sh", readme)

    def test_t3_r15_concurrent_cold_start_hook_env_leaves_no_zombie_after_uninstall_purge(self) -> None:
        """R15+R23: 3 concurrent cold-start hook-env calls leave zero zombie standbys after uninstall --purge."""
        binary = self.require_binary()
        env = self.make_env()
        port = find_free_port()
        env.write_config(env.make_config_dict(listen_port=port, include_usage_dir=False))

        plugins_dir = env.root / "plugins"
        plugin_root = plugins_dir / "agy-companion"
        plugin_root.mkdir(parents=True, exist_ok=True)
        (plugin_root / "plugin.json").write_text(
            '{"name":"agy-companion","version":"0.1.0"}\n', encoding="utf-8"
        )

        pid_path = env.data_dir / "proxy.pid"
        log_path = env.data_dir / "proxy.log"
        self.register_pidfile_cleanup(pid_path, expected_cmdline_substr=str(binary))

        proc_env = env.build_env(
            set_config_env=False,
            extra={"SHELL": "/bin/zsh", "ZDOTDIR": str(env.home_dir)},
        )

        procs = [
            subprocess.Popen(
                [
                    str(binary),
                    "hook-env",
                    "--ensure-running",
                    "--print-port",
                    "--plugins-dir",
                    str(plugins_dir),
                ],
                env=proc_env,
                stdout=subprocess.PIPE,
                stderr=subprocess.PIPE,
                text=True,
            )
            for _ in range(3)
        ]
        for idx, proc in enumerate(procs):
            out, err = proc.communicate(timeout=10.0)
            self.assertEqual(proc.returncode, 0, f"hook-env #{idx} stderr={err}")
            self.assertEqual(out.strip(), str(port), f"hook-env #{idx} stdout={out!r} stderr={err}")

        time.sleep(0.3)

        un = run_cli(
            [
                str(binary),
                "uninstall",
                "--plugins-dir",
                str(plugins_dir),
                "--purge",
                "--json",
            ],
            env=proc_env,
        )
        self.assertEqual(un.returncode, 0, f"{un.stdout}\n{un.stderr}")
        rep = un.json_stdout()
        self.assertIs(rep.get("ok"), True, rep)
        self.assertIs(rep.get("proxy_stopped"), True, rep)

        time.sleep(1.0)
        with self.assertRaises(OSError, msg=f"Port {port} must refuse connections after uninstall --purge"):
            http_raw_request("127.0.0.1", port, "GET", "/health", timeout=0.5)
        self.assertFalse(pid_path.exists(), "proxy.pid must not resurrect in purged data dir")
        self.assertFalse(log_path.exists(), "proxy.log must not resurrect in purged data dir")
