"""Tier 4 Rollout: Real-World End-to-End Application Lifecycle Scenarios (`ROLLOUT_SCENARIO`).

Six comprehensive isolated-`$HOME` multi-step workflows exercising PR 1 (`R01`–`R12`),
PR 2 (`R13`–`R19`), and PR 3 (`R20`–`R25`) end-to-end.
"""

from __future__ import annotations

import json
import os
import re
import shutil
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
    http_stream_request,
    reap_pidfile_daemon,
    run_cli,
)
from mcp_client import McpStdioClient
from mock_gemini import CapturedRequest, JsonAction, SseStreamAction


_PLACEHOLDER_KEY = "agy-companion-local"
_PLACEHOLDER_SLOT_RE = re.compile(r"^MODEL_PLACEHOLDER_M(\d+)$")
_S5_FORBIDDEN_SINKS_RE = re.compile(
    r"innerHTML|outerHTML|insertAdjacentHTML|document\.write|dangerouslySetInnerHTML"
    r"|localStorage|sessionStorage|document\.cookie|new\s+Function|XMLHttpRequest"
    r"|(?<![A-Za-z0-9_$])(?:eval|alert|confirm|prompt|fetch)\s*\("
)
_S5_EXTERNAL_TAGS_RE = re.compile(
    r"<script[^>]*\ssrc\s*=|<link[^>]*\shref\s*=|<img[^>]*\ssrc\s*=|<iframe|@import",
    re.IGNORECASE,
)


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
                "displayName": "Existing Upstream Slot",
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


class TestTier4RolloutScenarios(E2ETestCase):
    TIER = 4
    FEATURE = "ROLLOUT_SCENARIO"

    def test_t4_r01_day0_to_day1_to_uninstall_and_purge_full_lifecycle(self) -> None:
        """Scenario 1 (R07,R08,R09,R13,R14,R15,R18,R22,R23): Full install -> Day-0 -> save_settings -> Day-1 -> Dock/terminal -> uninstall -> purge."""
        binary = self.require_binary()
        env = self.make_env()
        self.register_pidfile_cleanup(env=env)
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
        usage_canary = env.usage_dir / "usage.db"
        usage_canary.write_bytes(b"SQLite format 3\x00")
        usage_wal_canary = env.usage_dir / "usage.db-wal"
        usage_wal_canary.write_bytes(b"wal-canary")
        legacy_jsonl_canary = env.usage_dir / "2026-11.jsonl"
        legacy_jsonl_canary.write_text('{"ts":"2026-11-24T00:00:00Z"}\n', encoding="utf-8")
        gemini_cfg = env.home_dir / ".gemini" / "config" / "config.json"
        gemini_cfg.parent.mkdir(parents=True, exist_ok=True)
        gemini_cfg.write_text('{"antigravity":"untouched"}\n', encoding="utf-8")

        plugins_dir = env.home_dir / ".gemini" / "config" / "plugins"
        shell_extra = {"SHELL": "/bin/zsh", "ZDOTDIR": str(env.home_dir)}

        # 1. Fresh install
        inst = run_cli(
            [str(binary), "install", "--plugins-dir", str(plugins_dir), "--json"],
            env=env.build_env(extra=shell_extra),
        )
        self.assertEqual(inst.returncode, 0, f"{inst.stdout}\n{inst.stderr}")
        self.assertIs(inst.json_stdout().get("ok"), True)
        self.assertIsNotNone(_wait_for_health(port, timeout=4.0))

        # 2. Day-0 catalog is untouched (zero Proxy models)
        day0_resp = http_raw_request(
            "127.0.0.1",
            port,
            "POST",
            "/v1internal:fetchAvailableModels",
            headers={"Authorization": "Bearer s1-day0", "Content-Type": "application/json"},
            body=b"{}",
        )
        self.assertEqual(day0_resp.status, 200, day0_resp.text)
        self.assertNotIn("companion/gemini-4-hydrogen-low", day0_resp.json()["models"])
        self.assertIs(
            http_raw_request("127.0.0.1", port, "GET", "/api/stats").json()["last_catalog_included_proxy_models"],
            False,
        )

        # 3. MCP save_settings saves a real API key -> Day-1 merged catalog
        mcp = self.start_mcp(env, config_path=cfg, extra_env=shell_extra)
        snap0 = mcp.call_tool("get_settings", {}).get("structuredContent", {})
        self.assertIs(snap0["config"]["api_key_configured"], False)
        if os.name != "nt":
            self.assertIs(snap0["launcher_integration"]["enabled"], True)

        mcp.save_settings_smart(api_key="AIzaSyScenario1DayOne1", raw=False)
        mcp.assert_clean_stdout()
        env.bump_mtime(cfg, delta=3.0)

        day1_resp = http_raw_request(
            "127.0.0.1",
            port,
            "POST",
            "/v1internal:fetchAvailableModels",
            headers={"Authorization": "Bearer s1-day1", "Content-Type": "application/json"},
            body=b"{}",
        )
        self.assertEqual(day1_resp.status, 200, day1_resp.text)
        self.assertIn("companion/gemini-4-hydrogen-low", day1_resp.json()["models"])
        self.assertIs(
            http_raw_request("127.0.0.1", port, "GET", "/api/stats").json()["last_catalog_included_proxy_models"],
            True,
        )
        mcp.close()

        # 4. Dock boot + integrated terminal scrub via real shell (.zlogin auto-load on zsh)
        if os.name != "nt":
            if Path("/bin/zsh").is_file():
                (env.home_dir / ".zshrc").write_text(
                    '[[ -n "${ANTIGRAVITY_RESOLVING_ENVIRONMENT:-}" ]] && return 0\n',
                    encoding="utf-8",
                )
                script = (
                    'printf "DOCK:%s|%s\\n" "${CLOUD_CODE_URL:-}" "${AGY_COMPANION_HOOK_ACTIVE:-}"\n'
                    'unset ANTIGRAVITY_RESOLVING_ENVIRONMENT\n'
                    '/bin/zsh --no-globalrcs -lc '
                    '\'printf "TERM:%s|%s\\n" "${CLOUD_CODE_URL-<UNSET>}" "${AGY_COMPANION_HOOK_ACTIVE-<UNSET>}"\'\n'
                )
                sh_res = run_cli(
                    ["/bin/zsh", "--no-globalrcs", "-ilc", script],
                    env=env.build_env(
                        extra={
                            **shell_extra,
                            "HOME": str(env.home_dir),
                            "ZDOTDIR": str(env.home_dir),
                            "ANTIGRAVITY_RESOLVING_ENVIRONMENT": "1",
                            "AGY_COMPANION_CONFIG": str(cfg),
                        }
                    ),
                )
            else:
                env_sh = env.data_dir / "env.sh"
                script = (
                    '. "$1"\n'
                    'printf "DOCK:%s|%s\\n" "${CLOUD_CODE_URL:-}" "${AGY_COMPANION_HOOK_ACTIVE:-}"\n'
                    'unset ANTIGRAVITY_RESOLVING_ENVIRONMENT\n'
                    '. "$1"\n'
                    'printf "TERM:%s|%s\\n" "${CLOUD_CODE_URL-<UNSET>}" "${AGY_COMPANION_HOOK_ACTIVE-<UNSET>}"\n'
                )
                sh_res = run_cli(
                    ["/bin/sh", "-c", script, "_", str(env_sh)],
                    env=env.build_env(
                        extra={
                            **shell_extra,
                            "ANTIGRAVITY_RESOLVING_ENVIRONMENT": "1",
                            "AGY_COMPANION_CONFIG": str(cfg),
                        }
                    ),
                )
            self.assertEqual(sh_res.returncode, 0, sh_res.stderr)
            self.assertEqual(
                sh_res.stdout.splitlines()[:2],
                [f"DOCK:http://127.0.0.1:{port}|1", "TERM:<UNSET>|<UNSET>"],
            )

        # 5. Uninstall preserves config & usage, then uninstall --purge wipes them while keeping ~/.gemini/config/config.json
        un1 = run_cli(
            [str(binary), "uninstall", "--plugins-dir", str(plugins_dir), "--json"],
            env=env.build_env(extra=shell_extra),
        )
        self.assertEqual(un1.returncode, 0, un1.stderr)
        self.assertFalse((plugins_dir / "agy-companion").exists())
        self.assertFalse((env.data_dir / "env.sh").exists())
        self.assertTrue(cfg.is_file(), "Default uninstall must preserve config.json")
        self.assertTrue(usage_canary.is_file(), "Default uninstall must preserve usage/usage.db")
        self.assertTrue(usage_wal_canary.is_file(), "Default uninstall must preserve usage/usage.db-wal")
        self.assertTrue(legacy_jsonl_canary.is_file(), "Default uninstall must preserve legacy usage files")

        un2 = run_cli(
            [str(binary), "uninstall", "--plugins-dir", str(plugins_dir), "--purge", "--json"],
            env=env.build_env(extra=shell_extra),
        )
        self.assertEqual(un2.returncode, 0, un2.stderr)
        self.assertFalse(cfg.exists(), "--purge must remove config.json")
        self.assertFalse(usage_canary.exists(), "--purge must remove usage/usage.db")
        self.assertFalse(usage_wal_canary.exists(), "--purge must remove usage/usage.db-wal")
        self.assertFalse(legacy_jsonl_canary.exists(), "--purge must remove legacy usage files")
        self.assertEqual(gemini_cfg.read_text(encoding="utf-8"), '{"antigravity":"untouched"}\n')

    def test_t4_r02_cloudcode_pro_enterprise_403_strip_hot_reload_slots_and_400_hint(self) -> None:
        """Scenario 2 (R01-R06,R09,R10): CloudCode auto routing, Enterprise 403 strip, hot-reload LRU, stable slots, and 400 hint."""
        # Phase A: Real "auto" mode routing via MockConnectProxy
        env_auto = self.make_env()
        connect_proxy = self.start_mock_connect_proxy()
        cfg_auto = env_auto.write_config(
            env_auto.make_config_dict(api_key="AIzaSyScenario2Auto123", cloudcode_upstream_url="auto")
        )
        p_auto = self.start_proxy(
            env_auto,
            config_path=cfg_auto,
            extra_env={"HTTPS_PROXY": connect_proxy.proxy_url},
        )
        # Consumer managed project -> daily first, prod fallback
        http_raw_request(
            "127.0.0.1",
            p_auto.port,
            "POST",
            "/v1internal:fetchAvailableModels",
            headers={"Authorization": "Bearer s2-pro", "Content-Type": "application/json"},
            body=b'{"project":"managed-ai-pro-42"}',
            timeout=5.0,
        )
        self.assertEqual(
            connect_proxy.wait_for_connects(2, timeout=3.0)[:2],
            ["daily-cloudcode-pa.googleapis.com:443", "cloudcode-pa.googleapis.com:443"],
        )
        # Enterprise auth_method=gcp -> prod first, daily fallback
        connect_proxy.clear_targets()
        http_raw_request(
            "127.0.0.1",
            p_auto.port,
            "POST",
            "/v1internal:fetchAvailableModels",
            headers={
                "Authorization": "Bearer s2-ent",
                "User-Agent": "antigravity/2.34.0 auth_method=gcp",
                "Content-Type": "application/json",
            },
            body=b"{}",
            timeout=5.0,
        )
        self.assertEqual(
            connect_proxy.wait_for_connects(2, timeout=3.0)[:2],
            ["cloudcode-pa.googleapis.com:443", "daily-cloudcode-pa.googleapis.com:443"],
        )
        # Streaming generation -> zero retry (1 CONNECT)
        connect_proxy.clear_targets()
        http_raw_request(
            "127.0.0.1",
            p_auto.port,
            "POST",
            "/v1internal:streamGenerateContent?alt=sse",
            headers={"Authorization": "Bearer s2-gen", "Content-Type": "application/json"},
            body=b'{"model":"gemini-4-carbon-high"}',
            timeout=5.0,
        )
        time.sleep(0.15)
        self.assertEqual(len(connect_proxy.connect_targets), 1)
        p_auto.stop()

        # Phase B: Fixed loopback mock for 403 strip, LRU hot-reload, stable slots, neon XHigh, headers, and 400 hint
        env = self.make_env()
        mock_ccpa = self.start_mock_gemini()
        mock_gemini = self.start_mock_gemini()

        def ccpa_handler(req: CapturedRequest) -> Any:
            if ":fetchAvailableModels" in req.path:
                if req.header("x-goog-user-project") is not None:
                    return JsonAction(status=403, payload={"error": {"message": "strip required"}})
                return JsonAction(status=200, payload=_sample_catalog())
            return SseStreamAction(chunks=[b'data: {"ok":true}\n\n'], status=200)

        mock_ccpa.set_handler(ccpa_handler)
        cfg = env.write_config(
            env.make_config_dict(
                base_url=mock_gemini.base_url,
                api_key="AIzaSyScenario2Fixed12",
                default_model="gemini-4-hydrogen",
                cloudcode_upstream_url=mock_ccpa.base_url,
            )
        )
        proxy = self.start_proxy(env, config_path=cfg)

        # Enterprise Token A triggers 403 metadata strip and captures deterministic slots
        r_meta1 = http_raw_request(
            "127.0.0.1",
            proxy.port,
            "POST",
            "/v1internal:fetchAvailableModels",
            headers={
                "Authorization": "Bearer s2-enterprise-a",
                "x-goog-user-project": "ent-proj-a",
                "x-goog-request-reason": "scenario-2-reason",
                "Content-Type": "application/json",
            },
            body=b"{}",
        )
        self.assertEqual(r_meta1.status, 200)
        self.assertEqual(mock_ccpa.requests[0].header("x-goog-request-reason"), "scenario-2-reason")
        slots1 = {
            mid: d["model"]
            for mid, d in r_meta1.json()["models"].items()
            if mid.startswith("companion/")
        }

        # Hot-reload default_model -> gemini-4-neon (cloudcode_upstream_url & timeout_secs unchanged)
        env.write_config(
            env.make_config_dict(
                base_url=mock_gemini.base_url,
                api_key="AIzaSyScenario2Fixed12",
                default_model="gemini-4-neon",
                cloudcode_upstream_url=mock_ccpa.base_url,
            ),
            path=cfg,
        )
        env.bump_mtime(cfg, delta=3.0)
        mock_ccpa.clear_requests()

        # Token A still strips x-goog-user-project after hot-reload; Token B preserves it
        http_raw_request(
            "127.0.0.1",
            proxy.port,
            "POST",
            "/v1internal:streamGenerateContent?alt=sse",
            headers={
                "Authorization": "Bearer s2-enterprise-a",
                "x-goog-user-project": "ent-proj-a",
                "Content-Type": "application/json",
            },
            body=b'{"model":"gemini-4-carbon-high"}',
        )
        http_raw_request(
            "127.0.0.1",
            proxy.port,
            "POST",
            "/v1internal:streamGenerateContent?alt=sse",
            headers={
                "Authorization": "Bearer s2-pro-b",
                "x-goog-user-project": "pro-proj-b",
                "Content-Type": "application/json",
            },
            body=b'{"model":"gemini-4-carbon-high"}',
        )
        self.assertIsNone(mock_ccpa.requests[0].header("x-goog-user-project"))
        self.assertEqual(mock_ccpa.requests[1].header("x-goog-user-project"), "pro-proj-b")

        # Re-fetch catalog: deterministic slots unchanged + gemini-4-neon XHigh level 7 present
        r_meta2 = http_raw_request(
            "127.0.0.1",
            proxy.port,
            "POST",
            "/v1internal:fetchAvailableModels",
            headers={"Authorization": "Bearer s2-enterprise-a", "Content-Type": "application/json"},
            body=b"{}",
        )
        self.assertEqual(r_meta2.status, 200)
        models2 = r_meta2.json()["models"]
        self.assertEqual(models2["companion/gemini-4-neon-xhigh"]["thinkingLevel"], 7)
        for mid, slot in slots1.items():
            if mid in models2:
                self.assertEqual(models2[mid]["model"], slot)
                self.assertIsNotNone(_PLACEHOLDER_SLOT_RE.match(slot))

        # Non-Gemini-3/4 returns 400 with explicit unset hint
        mock_gemini.clear_requests()
        r_400 = http_raw_request(
            "127.0.0.1",
            proxy.port,
            "POST",
            "/v1beta/models/gemini-2.5-pro:generateContent",
            headers={"Content-Type": "application/json"},
            body=b"{}",
        )
        self.assertEqual(r_400.status, 400)
        self.assertIn("unset GOOGLE_GEMINI_BASE_URL to reach Gemini API directly", r_400.json()["error"]["message"])
        self.assertEqual(len(mock_gemini.requests), 0)

    def test_t4_r03_managed_on_demand_daemon_idle_sse_guard_and_adopt_running_lifecycle(self) -> None:
        """Scenario 3 (R11,R12,R15,R17): hook-env cold/warm -> active SSE outliving idle timeout -> clean exit -> adopt takeover."""
        binary = self.require_binary()
        env = self.make_env()
        self.register_pidfile_cleanup(env=env)
        mock_gemini = self.start_mock_gemini()
        chunks = [
            b'data: {"candidates":[{"content":{"parts":[{"text":"c1"}]}}]}\n\n',
            b'data: {"candidates":[{"content":{"parts":[{"text":"c2"}]}}]}\n\n',
            b'data: {"candidates":[{"content":{"parts":[{"text":"c3"}]}}],"usageMetadata":{"promptTokenCount":5,"candidatesTokenCount":3,"totalTokenCount":8}}\n\n',
        ]
        mock_gemini.enqueue(SseStreamAction(chunks=chunks, chunk_delay_s=1.2))
        port = find_free_port()
        cfg = env.write_config(
            env.make_config_dict(
                listen_port=port,
                base_url=mock_gemini.base_url,
                api_key="AIzaSyScenario3Daemon1",
            )
        )
        _install_dummy_plugin_json(env)

        # 1. Cold start via hook-env + warm hit + launch --print-env
        hk1 = run_cli(
            [str(binary), "hook-env", "--ensure-running", "--print-port"],
            env=env.build_env(extra={"AGY_COMPANION_CONFIG": str(cfg)}),
        )
        self.assertEqual(hk1.returncode, 0, hk1.stderr)
        self.assertEqual(hk1.stdout.strip(), str(port))
        hk2 = run_cli(
            [str(binary), "hook-env", "--ensure-running", "--print-port"],
            env=env.build_env(extra={"AGY_COMPANION_CONFIG": str(cfg)}),
        )
        self.assertEqual(hk2.returncode, 0)
        self.assertEqual(hk2.stdout.strip(), str(port))
        self.assertLess(hk2.duration_s, 1.5)

        pe = run_cli(
            [str(binary), "launch", "--port", str(port), "--config", str(cfg), "--app", "/opt/antigravity", "--print-env"],
            env=env.build_env(),
        )
        self.assertEqual(pe.returncode, 0, pe.stderr)
        self.assertIn("JETSKI_MCP_ENV_RESTORE_PREFIX=", pe.stdout)

        # Stop the hook-env daemon cleanly to test short --idle-shutdown-secs 2 + SSE guard + adopt takeover
        reap_pidfile_daemon(env.data_dir / "proxy.pid", expected_binary=binary, timeout=3.0)

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

            # Slow SSE stream (~3.6s) outlives --idle-shutdown-secs 2 and completes intact
            received = bytearray()
            with http_stream_request(
                "127.0.0.1",
                port,
                "POST",
                "/v1beta/models/gemini-4-hydrogen:streamGenerateContent?alt=sse",
                headers={"Content-Type": "application/json"},
                body=b'{"contents":[{"parts":[{"text":"long"}]}]}',
                timeout=10.0,
            ) as stream:
                self.assertEqual(stream.status, 200)
                for chunk, _ts in stream.iter_chunks():
                    received.extend(chunk)
            self.assertEqual(bytes(received), b"".join(chunks))

            # After stream finishes, primary idle-exits 0 and adopter takes over in < 2.5s
            self.assertEqual(primary.wait(timeout=8.0), 0)
            t0 = time.monotonic()
            self.assertIsNotNone(_wait_for_health(port, timeout=2.5))
            self.assertLess(time.monotonic() - t0, 2.5)
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

    def test_t4_r04_multi_shell_and_nix_stow_dotfile_manager_workflow(self) -> None:
        """Scenario 4 (R13,R14,R16,R18,R23): Symlinked .zlogin -> MCP & CLI manual_required -> --follow-rc-symlink -> Bash/Fish -> uninstall strip."""
        if os.name == "nt":
            self.skipTest("POSIX multi-shell/symlink workflow")
        binary = self.require_binary()
        env = self.make_env()
        cfg = env.write_config(env.make_config_dict())

        # Stow/Nix-style symlinked .zlogin
        dotfiles_zlogin = env.root / "dotfiles_repo" / "zlogin"
        dotfiles_zlogin.parent.mkdir(parents=True, exist_ok=True)
        dotfiles_zlogin.write_text("# managed by stow\n", encoding="utf-8")
        zlogin_link = env.home_dir / ".zlogin"
        os.symlink(dotfiles_zlogin, zlogin_link)
        z_extra = {"SHELL": "/bin/zsh", "ZDOTDIR": str(env.home_dir)}

        # 1. MCP set_launcher_integration respects symlink -> manual_required, never follows
        mcp = self.start_mcp(env, config_path=cfg, extra_env=z_extra)
        r_mcp = mcp.call_tool("set_launcher_integration", {"enabled": True})
        mcp.assert_clean_stdout()
        li = r_mcp["structuredContent"]["launcher_integration"]
        self.assertIs(li["manual_required"], True)
        self.assertTrue(li.get("manual_snippet"))
        self.assertEqual(dotfiles_zlogin.read_text(encoding="utf-8"), "# managed by stow\n")
        self.assertTrue((env.data_dir / "env.sh").is_file())
        mcp.close()

        # 2. CLI default also reports manual_required; explicit --follow-rc-symlink updates target
        r_cli_def = run_cli([str(binary), "integration", "enable", "--shell", "zsh", "--json"], env=env.build_env(extra=z_extra))
        self.assertEqual(r_cli_def.returncode, 0, r_cli_def.stderr)
        self.assertIs(r_cli_def.json_stdout()["manual_required"], True)

        r_cli_opt = run_cli(
            [str(binary), "integration", "enable", "--shell", "zsh", "--follow-rc-symlink", "--json"],
            env=env.build_env(extra=z_extra),
        )
        self.assertEqual(r_cli_opt.returncode, 0, r_cli_opt.stderr)
        self.assertIs(r_cli_opt.json_stdout()["enabled"], True)
        self.assertIn("# >>> agy-companion", dotfiles_zlogin.read_text(encoding="utf-8"))

        # 3. Also enable Bash (.profile with user line) and Fish (conf.d)
        profile = env.home_dir / ".profile"
        profile.write_text("export BASH_USER_LINE=yes\n", encoding="utf-8")
        self.assertEqual(
            run_cli(
                [str(binary), "integration", "enable", "--shell", "bash", "--json"],
                env=env.build_env(extra={"SHELL": "/bin/bash"}),
            ).returncode,
            0,
        )
        self.assertEqual(
            run_cli(
                [str(binary), "integration", "enable", "--shell", "fish", "--json"],
                env=env.build_env(extra={"SHELL": "/usr/bin/fish"}),
            ).returncode,
            0,
        )
        fish_dropin = env.config_root / "fish" / "conf.d" / "agy-companion.fish"
        self.assertTrue(fish_dropin.is_file())

        # 4. Uninstall strips Bash & Fish (preserving user lines) and removes env.sh
        plugins_dir = env.root / "plugins"
        plugins_dir.mkdir(parents=True, exist_ok=True)
        un = run_cli(
            [str(binary), "uninstall", "--plugins-dir", str(plugins_dir), "--json"],
            env=env.build_env(extra=z_extra),
        )
        self.assertEqual(un.returncode, 0, un.stderr)
        self.assertFalse((env.data_dir / "env.sh").exists())
        self.assertFalse(fish_dropin.exists())
        self.assertEqual(profile.read_text(encoding="utf-8"), "export BASH_USER_LINE=yes\n")

    def test_t4_r05_live_upgrade_plugin_swap_and_run_companion_cache_precedence(self) -> None:
        """Scenario 5 (R20,R21,R22,R23): Fresh install -> parity & strict validate -> live upgrade -> cache beats PATH -> uninstall."""
        binary = self.require_binary()
        env = self.make_env()
        self.register_pidfile_cleanup(env=env)
        port = find_free_port()
        env.write_config(env.make_config_dict(listen_port=port))
        plugins_dir = env.root / "plugins"
        proc_env = env.build_env(extra={"SHELL": "/bin/zsh", "ZDOTDIR": str(env.home_dir)})

        # 1. Fresh install + Unix 7-file parity + strict agy plugin validate
        r1 = run_cli([str(binary), "install", "--plugins-dir", str(plugins_dir), "--json"], env=proc_env)
        self.assertEqual(r1.returncode, 0, f"{r1.stdout}\n{r1.stderr}")
        installed = plugins_dir / "agy-companion"
        for rel in (
            "plugin.json",
            "README.md",
            "assets/logo.svg",
            "mcp_config.json",
            "sidecars/proxy/sidecar.json",
            "skills/companion/SKILL.md",
            "scripts/run-companion.sh",
        ):
            self.assertTrue((installed / rel).is_file(), f"Missing {rel}")
            if os.name != "nt":
                self.assertEqual((installed / rel).read_bytes(), (self.repo_root / "plugin" / rel).read_bytes())
        if shutil.which("agy"):
            val = run_cli(["agy", "plugin", "validate", "--strict", "--json", str(installed)])
            self.assertEqual(val.returncode, 0, val.stderr)
            self.assertEqual(val.json_stdout().get("findings"), [])

        # 2. Live upgrade (re-run install while proxy is already running) -> zero leftover stage/backup dirs
        r2 = run_cli([str(binary), "install", "--plugins-dir", str(plugins_dir), "--json"], env=proc_env)
        self.assertEqual(r2.returncode, 0, f"{r2.stdout}\n{r2.stderr}")
        leftovers = [p.name for p in plugins_dir.iterdir() if ".stage." in p.name or ".backup" in p.name]
        self.assertEqual(leftovers, [])
        self.assertIsNotNone(_wait_for_health(port, timeout=4.0))

        # 3. Installed run-companion.sh beats poisoned PATH and scrubs injected env vars
        if os.name != "nt":
            poison_path = env.root / "poison_path"
            poison_path.mkdir(parents=True, exist_ok=True)
            poison_bin = poison_path / "agy-companion"
            poison_bin.write_text("#!/bin/sh\necho POISONED_PATH_RAN >&2; exit 88\n", encoding="utf-8")
            os.chmod(poison_bin, 0o755)
            rc_res = run_cli(
                [str(installed / "scripts" / "run-companion.sh"), "--version"],
                env=env.build_env(
                    extra={
                        "PATH": f"{poison_path}:{os.environ.get('PATH', '/usr/bin:/bin')}",
                        "CLOUD_CODE_URL": f"http://127.0.0.1:{port}",
                        "GOOGLE_GEMINI_BASE_URL": f"http://127.0.0.1:{port}",
                        "GEMINI_API_KEY": _PLACEHOLDER_KEY,
                    }
                ),
            )
            self.assertEqual(rc_res.returncode, 0, rc_res.stderr)
            self.assertNotIn("POISONED_PATH_RAN", rc_res.stderr)

        # 4. Clean uninstall
        un = run_cli([str(binary), "uninstall", "--plugins-dir", str(plugins_dir), "--json"], env=proc_env)
        self.assertEqual(un.returncode, 0, un.stderr)
        self.assertFalse(installed.exists())

    def test_t4_r06_end_to_end_security_and_zero_leak_hardening_lifecycle(self) -> None:
        """Scenario 6 (R07,R13,R18,R19,R23,R24,R25): Placeholder quarantine, SSH & injection guards, MCP deny_unknown_fields, PID/foreign-dir safety, S5 & docs hygiene."""
        binary = self.require_binary()
        env = self.make_env()
        mock_gemini = self.start_mock_gemini()
        cfg = env.write_config(
            env.make_config_dict(
                base_url=mock_gemini.base_url,
                api_key=_PLACEHOLDER_KEY,
            )
        )
        proxy = self.start_proxy(env, config_path=cfg, extra_env={"GEMINI_API_KEY": _PLACEHOLDER_KEY})

        # 1. Placeholder quarantine across proxy, doctor, and MCP
        self.assertEqual(
            http_raw_request(
                "127.0.0.1",
                proxy.port,
                "POST",
                "/v1beta/models/gemini-4-hydrogen:generateContent",
                headers={"Content-Type": "application/json", "x-goog-api-key": _PLACEHOLDER_KEY},
                body=b"{}",
            ).status,
            401,
        )
        self.assertEqual(len(mock_gemini.requests), 0)
        doc = run_cli(
            [str(binary), "doctor", "--config", str(cfg), "--port", str(proxy.port), "--json"],
            env=env.build_env(extra={"GEMINI_API_KEY": _PLACEHOLDER_KEY}),
        )
        self.assertIs(doc.json_stdout()["env"]["gemini_api_key_set"], False)
        proxy.stop()

        # 2. MCP deny_unknown_fields + placeholder save rejection
        mcp = self.start_mcp(
            env,
            config_path=cfg,
            extra_env={"SHELL": "/bin/zsh", "ZDOTDIR": str(env.home_dir)},
        )
        self.assertTrue(
            McpStdioClient.is_tool_error(
                mcp.call_tool_raw("set_launcher_integration", {"enabled": True, "rc_file": "/etc/passwd"})
            )
        )
        self.assertTrue(McpStdioClient.is_tool_error(mcp.save_settings_smart(api_key=_PLACEHOLDER_KEY, raw=True)))
        res = mcp.read_resource("ui://companion/settings.html")
        mcp.assert_clean_stdout()
        contents = res.get("contents") or []
        self.assertGreaterEqual(len(contents), 1)
        html = contents[0]["text"]
        mcp.close()

        # 3. UI S5 & size gate
        self.assertLess(len(html.encode("utf-8")), 512_000)
        self.assertIsNone(_S5_FORBIDDEN_SINKS_RE.search(html))
        self.assertIsNone(_S5_EXTERNAL_TAGS_RE.search(html))

        # 4. SSH guard + hostile port injection guard + foreign plugin dir + unrelated PID safety
        if os.name != "nt":
            pwn = env.root / "pwn_s6"
            hostile_bin = env.root / "hostile_s6.sh"
            hostile_bin.write_text(f'#!/bin/sh\nprintf "4141; touch {pwn}\\n"\n', encoding="utf-8")
            os.chmod(hostile_bin, 0o755)
            self.assertEqual(
                run_cli(
                    [str(binary), "integration", "enable", "--shell", "zsh", "--binary", str(hostile_bin), "--json"],
                    env=env.build_env(extra={"SHELL": "/bin/zsh", "ZDOTDIR": str(env.home_dir)}),
                ).returncode,
                0,
            )
            env_sh = env.data_dir / "env.sh"
            r_hostile = run_cli(
                ["/bin/sh", "-c", '. "$1"; printf "%s" "${CLOUD_CODE_URL:-}"', "_", str(env_sh)],
                env=env.build_env(extra={"ANTIGRAVITY_RESOLVING_ENVIRONMENT": "1"}),
            )
            self.assertEqual(r_hostile.stdout, "")
            self.assertFalse(pwn.exists())

            # Foreign plugin dir + unrelated sleeper PID survive uninstall
            foreign_plugins = env.root / "foreign_plugins"
            fdir = foreign_plugins / "agy-companion"
            fdir.mkdir(parents=True, exist_ok=True)
            (fdir / "plugin.json").write_text('{"name":"foreign-app","version":"1.0.0"}\n', encoding="utf-8")
            sleeper = subprocess.Popen(
                [sys.executable, "-c", "import time; time.sleep(60)"],
                start_new_session=True,
                stdout=subprocess.DEVNULL,
                stderr=subprocess.DEVNULL,
            )
            try:
                (env.data_dir / "proxy.pid").write_text(
                    json.dumps({"pid": sleeper.pid, "port": find_free_port(), "version": "0.1.0"}),
                    encoding="utf-8",
                )
                un = run_cli(
                    [str(binary), "uninstall", "--plugins-dir", str(foreign_plugins), "--json"],
                    env=env.build_env(extra={"SHELL": "/bin/zsh", "ZDOTDIR": str(env.home_dir)}),
                )
                self.assertEqual(un.returncode, 0, un.stderr)
                self.assertTrue((fdir / "plugin.json").is_file(), "Foreign plugin dir was deleted!")
                self.assertIsNone(sleeper.poll(), "Unrelated process was killed by uninstall!")
            finally:
                if sleeper.poll() is None:
                    sleeper.kill()
                    sleeper.wait(timeout=2.0)

        # 5. Docs & Windows zero-setx / zero-curl-installer policy
        self.assertFalse((self.repo_root / "scripts" / "install.sh").exists())
        self.assertFalse((self.repo_root / "scripts" / "install.ps1").exists())
        readme = (self.repo_root / "README.md").read_text(encoding="utf-8")
        self.assertIn("npx -y agy-companion@latest install", readme)
        self.assertNotIn("releases/latest/download/install.sh", readme)
        self.assertNotIn("releases/latest/download/install.ps1", readme)
