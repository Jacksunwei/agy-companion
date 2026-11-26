"""UX v2 cross-process opaque-box CUJs (`--suite cuj`).

Exercises real multi-process UX v2 contracts across `serve`, `ui-mcp`,
`install`, `integration`, `doctor`, `models`, and SQLite `usage.db`:
1. Day-0 built-in `/v1internal:*` passive `usageMetadata` tee + S4 gzip skip,
   `proxy_rss_bytes` on `/api/stats` & `open_settings`, and `get_usage` rollups.
2. `enabled_models` zero-debounce hot-reload filtering & `[]` pause mode.
3. First-run `--auto-enable-launcher` cross-process race on `integration.lock`.
4. Launcher opt-out parity (`install --no-launcher-hook`, `integration disable`)
   and symlinked login RC fail-closed (`manual_required`).
5. Semantic `account_plan` persistence across proxy restarts + CLI continuity.
"""

from __future__ import annotations

import concurrent.futures
import gzip
import json
import os
import stat
import sys
import threading
from pathlib import Path
from typing import Any

from harness import (
    CompanionProcess,
    E2ETestCase,
    IsolatedEnv,
    extract_models_list,
    find_free_port,
    http_raw_request,
    poll_stats,
    poll_usage_events,
    run_cli,
)
from mcp_client import McpStdioClient
from mock_gemini import (
    CapturedRequest,
    JsonAction,
    RawBytesAction,
    SseStreamAction,
    build_sse_events,
)


_HOOK_BEGIN_PREFIX = "# >>> agy-companion"
_HOOK_END_PREFIX = "# <<< agy-companion"


def _zsh_env(env: IsolatedEnv) -> dict[str, str]:
    return {"SHELL": "/bin/zsh", "ZDOTDIR": str(env.home_dir)}


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
                "groups": [
                    {
                        "displayName": "Google AI Pro",
                        "modelIds": ["gemini-4-carbon-high"],
                    }
                ],
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


def _companion_model_keys(catalog: dict[str, Any]) -> list[str]:
    models = catalog.get("models")
    if not isinstance(models, dict):
        return []
    return sorted(k for k in models if isinstance(k, str) and k.startswith("companion/"))


def _count_rc_markers(rc_path: Path) -> tuple[int, int]:
    if not rc_path.exists():
        return (0, 0)
    lines = rc_path.read_text(encoding="utf-8").splitlines()
    begins = sum(1 for line in lines if line.startswith(_HOOK_BEGIN_PREFIX))
    ends = sum(1 for line in lines if line.startswith(_HOOK_END_PREFIX))
    return (begins, ends)


class TestCujUxV2(E2ETestCase):
    SUITE = "cuj"
    FEATURE = "UX_V2"

    def _fetch_catalog(self, port: int, token: str) -> dict[str, Any]:
        resp = http_raw_request(
            "127.0.0.1",
            port,
            "POST",
            "/v1internal:fetchAvailableModels",
            headers={
                "Content-Type": "application/json",
                "Authorization": f"Bearer {token}",
            },
            body=b"{}",
        )
        self.assertEqual(
            resp.status,
            200,
            f"fetchAvailableModels failed: status={resp.status} body={resp.body!r}",
        )
        data = resp.json()
        self.assertIsInstance(data, dict)
        return data

    def test_cuj_ux_day0_builtin_passive_usage_tee_and_s4_gzip_skip(self) -> None:
        """Day-0 built-in `/v1internal:*` passive tee skips S4 gzip, records SQLite + `/api/stats` (`proxy_rss_bytes`), and feeds MCP `get_usage` + `open_settings`."""
        env = self.make_env()
        mock = self.start_mock_gemini()

        gz_payload = gzip.compress(
            build_sse_events(
                [
                    {
                        "response": {
                            "candidates": [
                                {"content": {"parts": [{"text": "gz-skip"}]}}
                            ],
                            "usageMetadata": {
                                "promptTokenCount": 77_777,
                                "candidatesTokenCount": 7_777,
                            },
                        }
                    }
                ]
            )
        )
        stream_sse = build_sse_events(
            [
                {
                    "response": {
                        "candidates": [
                            {"content": {"parts": [{"text": "stream-builtin"}]}}
                        ],
                        "usageMetadata": {
                            "promptTokenCount": 12_000,
                            "cachedContentTokenCount": 9_000,
                            "candidatesTokenCount": 420,
                            "thoughtsTokenCount": 80,
                        },
                    }
                }
            ]
        )
        unary_payload = {
            "response": {
                "candidates": [{"content": {"parts": [{"text": "unary-builtin"}]}}],
                "usageMetadata": {
                    "promptTokenCount": 8_000,
                    "cachedContentTokenCount": 6_000,
                    "candidatesTokenCount": 310,
                    "thoughtsTokenCount": 50,
                },
            }
        }

        def handler(req: CapturedRequest) -> Any:
            if ":fetchAvailableModels" in req.path:
                return JsonAction(200, _sample_catalog())
            if "/v1internal:streamGenerateContent" in req.path:
                if "gzip" in (req.header("accept-encoding") or "").lower():
                    return RawBytesAction(
                        status=200,
                        content_type="text/event-stream",
                        body=gz_payload,
                        headers={"Content-Encoding": "gzip"},
                    )
                return SseStreamAction([stream_sse], status=200)
            if "/v1internal:generateContent" in req.path:
                return JsonAction(200, unary_payload)
            return JsonAction(404, {"error": f"unexpected path {req.path}"})

        mock.set_handler(handler)

        port = find_free_port()
        cfg = env.write_config(
            env.make_config_dict(
                listen_port=port,
                api_key=None,
                cloudcode_upstream_url=mock.base_url,
            )
        )
        proxy = self.start_proxy(env, config_path=cfg, port=port)

        # Day-0 catalog fetch leaves upstream models intact and populates last_catalog_refresh_at_ms.
        cat = self._fetch_catalog(proxy.port, "ux1-cat")
        self.assertEqual(_companion_model_keys(cat), [])
        self.assertIn("gemini-4-carbon-high", cat["models"])

        # Send S4 compressed stream FIRST so completion of the 2 uncompressed calls proves gzip wasn't teed.
        gz_resp = http_raw_request(
            "127.0.0.1",
            proxy.port,
            "POST",
            "/v1internal:streamGenerateContent?alt=sse",
            headers={
                "Content-Type": "application/json",
                "Authorization": "Bearer ux1-gz",
                "Accept-Encoding": "gzip",
            },
            body=json.dumps({"model": "gemini-3.8-pro"}).encode("utf-8"),
        )
        self.assertEqual(gz_resp.status, 200)
        self.assertEqual(gz_resp.header("content-encoding"), "gzip")
        self.assertEqual(gz_resp.body, gz_payload)

        # Uncompressed SSE + unary built-in calls.
        sse_resp = http_raw_request(
            "127.0.0.1",
            proxy.port,
            "POST",
            "/v1internal:streamGenerateContent?alt=sse",
            headers={
                "Content-Type": "application/json",
                "Authorization": "Bearer ux1-sse",
            },
            body=json.dumps({"model": "gemini-3.8-pro"}).encode("utf-8"),
        )
        self.assertEqual(sse_resp.status, 200)
        self.assertIn(b"stream-builtin", sse_resp.body)

        unary_resp = http_raw_request(
            "127.0.0.1",
            proxy.port,
            "POST",
            "/v1internal:generateContent",
            headers={
                "Content-Type": "application/json",
                "Authorization": "Bearer ux1-unary",
            },
            body=json.dumps({"model": "gemini-4-carbon-high"}).encode("utf-8"),
        )
        self.assertEqual(unary_resp.status, 200)
        self.assertIn(b"unary-builtin", unary_resp.body)

        # SQLite usage.db persistence: exact 2 uncompressed built-in events, 0600 on Unix.
        _files, events = poll_usage_events(
            env.usage_dir, min_count=2, timeout=5.0, interval=0.02
        )
        self.assertEqual(len(events), 2)
        self.assertTrue(all(e["provider"] == "antigravity" for e in events))
        self.assertTrue(all(e["source"] == "builtin" for e in events))
        self.assertEqual(sum(int(e["input_tokens"]) for e in events), 20_000)
        self.assertEqual(sum(int(e["cached_tokens"]) for e in events), 15_000)
        self.assertEqual(sum(int(e["output_tokens"]) for e in events), 730)
        self.assertEqual(sum(int(e["reasoning_tokens"]) for e in events), 130)
        self.assertEqual(
            {e["model"] for e in events},
            {"gemini-3.8-pro", "gemini-4-carbon-high"},
        )
        if os.name != "nt":
            db_mode = stat.S_IMODE(os.lstat(env.usage_dir / "usage.db").st_mode)
            self.assertEqual(db_mode, 0o600)

        # GET /api/stats reflects built-in usage + proxy_rss_bytes on POSIX.
        stats = poll_stats(proxy.port, min_requests=2, timeout=5.0, interval=0.02)
        self.assertEqual(stats["usage"]["requests"], 2)
        self.assertEqual(stats["usage"]["prompt_tokens"], 20_000)
        self.assertEqual(stats["usage"]["cached_tokens"], 15_000)
        self.assertEqual(len(stats["recent_calls"]), 2)
        self.assertEqual(len(stats["by_model"]), 2)
        self.assertTrue(
            all(
                c["provider"] == "antigravity" and c["source"] == "builtin"
                for c in stats["recent_calls"]
            )
        )
        if sys.platform in ("darwin", "linux"):
            rss = stats.get("proxy_rss_bytes")
            self.assertIsInstance(rss, int)
            self.assertGreater(rss, 0)

        # MCP open_settings + get_usage(1d/7d/30d).
        mcp = self.start_mcp(env, config_path=cfg)
        snap = mcp.call_tool("open_settings", {})["structuredContent"]
        self.assertIs(snap["config"]["providers"]["gemini"]["api_key_configured"], False)
        self.assertIs(snap["launcher_integration"]["hybrid_active"], True)
        if sys.platform in ("darwin", "linux"):
            snap_rss = snap.get("proxy_rss_bytes")
            self.assertIsInstance(snap_rss, int)
            self.assertGreater(snap_rss, 0)

        for rng in ("1d", "7d", "30d"):
            usage = mcp.call_tool("get_usage", {"range": rng})["structuredContent"]
            self.assertEqual(usage["range"], rng)
            self.assertEqual(usage["totals"]["requests"], 2)
            self.assertEqual(usage["totals"]["input_tokens"], 20_000)
            self.assertEqual(
                {row["provider"] for row in usage["by_provider"]},
                {"antigravity"},
            )
            self.assertEqual(len(usage["by_model"]), 2)
        mcp.assert_clean_stdout()

    def test_cuj_ux_enabled_models_hot_reload_and_empty_pause(self) -> None:
        """`save_settings(enabled_models=...)` hot-reloads proxy catalog slots (`11 -> 3 -> 17 -> 0`) with zero restart."""
        env = self.make_env()
        mock = self.start_mock_gemini()
        mock.set_handler(lambda _req: JsonAction(200, _sample_catalog()))

        port = find_free_port()
        cfg = env.write_config(
            env.make_config_dict(
                listen_port=port,
                api_key="test-uxv2-gemini-key-02",
                base_url=mock.base_url,
                cloudcode_upstream_url=mock.base_url,
            )
        )
        proxy = self.start_proxy(env, config_path=cfg, port=port)
        mcp = self.start_mcp(env, config_path=cfg)

        # Default 3 models (carbon=3, neon=4, hydrogen=4) -> 11 companion slots.
        default_keys = _companion_model_keys(self._fetch_catalog(proxy.port, "ux2-def"))
        self.assertEqual(len(default_keys), 11, f"unexpected default keys: {default_keys}")

        # Single model ["gemini-4-argon"] -> 3 slots (default_model hydrogen NOT injected).
        r1 = mcp.save_settings_smart(enabled_models=["gemini-4-argon"], raw=False)
        self.assertEqual(
            r1["structuredContent"]["config"]["providers"]["gemini"]["enabled_models"],
            ["gemini-4-argon"],
        )
        env.bump_mtime(cfg, delta=2.0)
        one_keys = _companion_model_keys(self._fetch_catalog(proxy.port, "ux2-one"))
        self.assertEqual(len(one_keys), 3, f"unexpected single-model keys: {one_keys}")
        self.assertTrue(all("gemini-4-argon-" in k for k in one_keys))

        # All 5 curated models -> 17 slots (3+4+4+3+3).
        all_five = [
            "gemini-4-carbon",
            "gemini-4-neon",
            "gemini-4-hydrogen",
            "gemini-4-argon",
            "gemini-3.8-flash",
        ]
        mcp.save_settings_smart(enabled_models=all_five, raw=False)
        env.bump_mtime(cfg, delta=2.0)
        five_keys = _companion_model_keys(self._fetch_catalog(proxy.port, "ux2-five"))
        self.assertEqual(len(five_keys), 17, f"unexpected 5-model keys: {five_keys}")

        # Empty pause [] -> 0 companion slots, upstream models untouched, stats flag false.
        mcp.save_settings_smart(enabled_models=[], raw=False)
        env.bump_mtime(cfg, delta=2.0)
        cat_empty = self._fetch_catalog(proxy.port, "ux2-empty")
        self.assertEqual(_companion_model_keys(cat_empty), [])
        self.assertIn("gemini-4-carbon-high", cat_empty["models"])
        self.assertIn("existing-m334", cat_empty["models"])

        stats = http_raw_request("127.0.0.1", proxy.port, "GET", "/api/stats").json()
        self.assertIs(stats["cloudcode"]["last_catalog_included_proxy_models"], False)
        mcp.assert_clean_stdout()

    def test_cuj_ux_first_run_auto_enable_and_concurrent_integration_lock(self) -> None:
        """Concurrent `serve --auto-enable-launcher` and `ui-mcp --auto-enable-launcher` serialize cleanly on `integration.lock`."""
        if os.name == "nt":
            self.skipTest("Unix RC hook auto-enable requires POSIX")

        binary = self.require_binary()
        env = self.make_env()
        shell_extra = _zsh_env(env)
        zlogin = env.home_dir / ".zlogin"
        zlogin.write_text("# user zlogin header\n", encoding="utf-8")

        port = find_free_port()
        cfg = env.write_config(
            env.make_config_dict(
                listen_port=port,
                api_key=None,
                launcher_auto_start=None,
            )
        )

        # Register RAII teardown strictly on the main thread before concurrent work.
        proc_env = env.build_env(extra=shell_extra)
        ready_timeout = min(10.0, self.cfg.default_timeout)
        proxy = CompanionProcess(
            binary=binary,
            env=proc_env,
            port=port,
            config_path=cfg,
            extra_args=("--auto-enable-launcher",),
            ready_timeout=ready_timeout,
        )
        self.stack.callback(proxy.stop)

        mcp = McpStdioClient(
            [
                str(binary),
                "ui-mcp",
                "--config",
                str(cfg),
                "--data-dir",
                str(env.data_dir),
                "--auto-enable-launcher",
            ],
            env=proc_env,
        )
        self.stack.callback(mcp.close)
        mcp.start()
        # `initialize` does not trigger auto-enable; `open_settings` does.
        mcp.initialize(timeout=ready_timeout)

        barrier = threading.Barrier(2)

        def run_proxy_start() -> CompanionProcess:
            barrier.wait(timeout=5.0)
            return proxy.start()

        def run_mcp_open() -> dict[str, Any]:
            barrier.wait(timeout=5.0)
            return mcp.call_tool("open_settings", {})

        with concurrent.futures.ThreadPoolExecutor(max_workers=2) as pool:
            fut_proxy = pool.submit(run_proxy_start)
            fut_mcp = pool.submit(run_mcp_open)
            fut_proxy.result(timeout=15.0)
            open_res = fut_mcp.result(timeout=15.0)

        self.assertEqual(
            http_raw_request("127.0.0.1", proxy.port, "GET", "/health").status,
            200,
        )
        self.assertFalse(McpStdioClient.is_tool_error(open_res))

        lock_path = env.data_dir / "integration.lock"
        self.assertTrue(lock_path.is_file())
        self.assertFalse(lock_path.is_symlink())
        self.assertEqual(stat.S_IMODE(os.lstat(lock_path).st_mode), 0o600)

        cfg_data = json.loads(cfg.read_text(encoding="utf-8"))
        self.assertIs(cfg_data.get("launcher_auto_start"), True)
        self.assertEqual(stat.S_IMODE(os.lstat(cfg).st_mode), 0o600)

        self.assertEqual(_count_rc_markers(zlogin), (1, 1))
        self.assertIn("# user zlogin header", zlogin.read_text(encoding="utf-8"))

        env_sh = env.data_dir / "env.sh"
        self.assertTrue(env_sh.is_file())
        self.assertEqual(stat.S_IMODE(os.lstat(env_sh).st_mode), 0o600)

        snap = mcp.call_tool("get_settings", {})["structuredContent"]
        self.assertIs(snap["launcher_integration"]["enabled"], True)
        self.assertIs(snap["proxy"]["reachable"], True)
        mcp.assert_clean_stdout()

    def test_cuj_ux_launcher_opt_out_parity_and_symlink_rc_fail_closed(self) -> None:
        """`install --no-launcher-hook` & `integration disable` opt out of auto-enable; symlinked RC fails closed to `manual_required`."""
        if os.name == "nt":
            self.skipTest("Unix launcher RC opt-out & symlink guard require POSIX")

        binary = self.require_binary()

        # Part (a): Opt-out parity across install, serve, ui-mcp, and integration CLI.
        env = self.make_env()
        self.register_pidfile_cleanup(env=env)
        shell_extra = _zsh_env(env)
        zlogin = env.home_dir / ".zlogin"
        zlogin.write_text("# pre-install\n", encoding="utf-8")

        port = find_free_port()
        # Pre-write config so `install` sees an existing config and skips fresh-install port probing.
        cfg = env.write_config(env.make_config_dict(listen_port=port, api_key=None))
        plugins_dir = env.home_dir / ".gemini" / "config" / "plugins"

        res = run_cli(
            [
                str(binary),
                "install",
                "--config",
                str(cfg),
                "--plugins-dir",
                str(plugins_dir),
                "--no-launcher-hook",
                "--no-start-proxy",
                "--json",
            ],
            env=env.build_env(extra=shell_extra),
        )
        self.assertEqual(res.returncode, 0, f"install failed: {res.stderr}")
        install_json = res.json_stdout()
        self.assertIs(install_json["ok"], True)
        self.assertIsNone(install_json["launcher_integration"])
        self.assertIs(json.loads(cfg.read_text(encoding="utf-8"))["launcher_auto_start"], False)
        self.assertEqual(_count_rc_markers(zlogin), (0, 0))
        self.assertFalse((env.data_dir / "env.sh").exists())

        p1 = self.start_proxy(
            env,
            config_path=cfg,
            port=port,
            extra_args=("--auto-enable-launcher",),
            extra_env=shell_extra,
        )
        mcp1 = self.start_mcp(
            env,
            config_path=cfg,
            extra_args=("--auto-enable-launcher",),
            extra_env=shell_extra,
        )
        open1 = mcp1.call_tool("open_settings", {})["structuredContent"]
        self.assertIs(open1["launcher_integration"]["enabled"], False)
        self.assertEqual(open1["launcher_integration"]["state"], "disabled")
        self.assertIs(json.loads(cfg.read_text(encoding="utf-8"))["launcher_auto_start"], False)
        self.assertEqual(_count_rc_markers(zlogin), (0, 0))
        self.assertFalse((env.data_dir / "env.sh").exists())

        # Explicit MCP opt-in re-enables the hook.
        on1 = mcp1.call_tool("set_launcher_integration", {"enabled": True})["structuredContent"]
        self.assertIs(on1["launcher_integration"]["enabled"], True)
        self.assertIs(json.loads(cfg.read_text(encoding="utf-8"))["launcher_auto_start"], True)
        self.assertEqual(_count_rc_markers(zlogin), (1, 1))
        self.assertTrue((env.data_dir / "env.sh").is_file())
        mcp1.assert_clean_stdout()
        mcp1.close()
        p1.stop()

        # CLI `integration disable` opts back out and blocks subsequent `--auto-enable-launcher`.
        dis = run_cli(
            [str(binary), "integration", "disable", "--config", str(cfg), "--json"],
            env=env.build_env(extra=shell_extra),
        )
        self.assertEqual(dis.returncode, 0, f"integration disable failed: {dis.stderr}")
        self.assertIs(dis.json_stdout()["enabled"], False)
        self.assertIs(json.loads(cfg.read_text(encoding="utf-8"))["launcher_auto_start"], False)
        self.assertEqual(_count_rc_markers(zlogin), (0, 0))
        self.assertFalse((env.data_dir / "env.sh").exists())

        mcp2 = self.start_mcp(
            env,
            config_path=cfg,
            extra_args=("--auto-enable-launcher",),
            extra_env=shell_extra,
        )
        open2 = mcp2.call_tool("open_settings", {})["structuredContent"]
        self.assertIs(open2["launcher_integration"]["enabled"], False)
        self.assertEqual(_count_rc_markers(zlogin), (0, 0))
        mcp2.assert_clean_stdout()
        mcp2.close()

        # CLI `integration enable` restores the hook cleanly.
        en = run_cli(
            [str(binary), "integration", "enable", "--config", str(cfg), "--json"],
            env=env.build_env(extra=shell_extra),
        )
        self.assertEqual(en.returncode, 0, f"integration enable failed: {en.stderr}")
        self.assertIs(en.json_stdout()["enabled"], True)
        self.assertIs(json.loads(cfg.read_text(encoding="utf-8"))["launcher_auto_start"], True)
        self.assertEqual(_count_rc_markers(zlogin), (1, 1))

        # Part (b): Symlinked `$HOME/.zlogin` fails closed to `manual_required` without modifying target.
        env_sym = self.make_env()
        sym_shell = _zsh_env(env_sym)
        store_dir = env_sym.root / "nix-store"
        store_dir.mkdir(parents=True, exist_ok=True)
        store_zlogin = store_dir / "zlogin"
        sentinel = "# managed by stow\nexport NIX_PROFILE=1\n"
        store_zlogin.write_text(sentinel, encoding="utf-8")
        os.chmod(store_zlogin, 0o444)
        self.stack.callback(
            lambda: os.chmod(store_zlogin, 0o644) if store_zlogin.exists() else None
        )

        sym_rc = env_sym.home_dir / ".zlogin"
        os.symlink(store_zlogin, sym_rc)

        sym_port = find_free_port()
        cfg_sym = env_sym.write_config(
            env_sym.make_config_dict(
                listen_port=sym_port,
                api_key=None,
                launcher_auto_start=None,
            )
        )
        self.start_proxy(
            env_sym,
            config_path=cfg_sym,
            port=sym_port,
            extra_args=("--auto-enable-launcher",),
            extra_env=sym_shell,
        )
        self.assertTrue(sym_rc.is_symlink())
        self.assertEqual(store_zlogin.read_text(encoding="utf-8"), sentinel)
        self.assertIs(
            json.loads(cfg_sym.read_text(encoding="utf-8"))["launcher_auto_start"],
            True,
        )

        mcp_sym = self.start_mcp(
            env_sym,
            config_path=cfg_sym,
            extra_args=("--auto-enable-launcher",),
            extra_env=sym_shell,
        )
        li = mcp_sym.call_tool("open_settings", {})["structuredContent"]["launcher_integration"]
        self.assertEqual(li["state"], "manual_required")
        self.assertIs(li["enabled"], False)
        self.assertIsInstance(li.get("manual_snippet"), str)
        self.assertIn("env.sh", li["manual_snippet"])
        self.assertEqual(store_zlogin.read_text(encoding="utf-8"), sentinel)
        mcp_sym.assert_clean_stdout()

    def test_cuj_ux_semantic_account_plan_persistence_and_cli_continuity(self) -> None:
        """`account_plan` persists across proxy restarts without leaking raw CloudCode URLs, and `doctor`/`models` remain green."""
        binary = self.require_binary()
        env = self.make_env()
        port = find_free_port()
        cfg = env.write_config(
            env.make_config_dict(
                listen_port=port,
                api_key="test-uxv2-gemini-key-05",
            )
        )

        # Save `personal` + `enabled_models`, and verify mutual exclusion with `cloudcode.upstream`.
        p1 = self.start_proxy(env, config_path=cfg, port=port)
        mcp1 = self.start_mcp(env, config_path=cfg)
        mcp1.save_settings_smart(
            account_plan="personal",
            enabled_models=["gemini-4-carbon", "gemini-4-neon"],
            raw=False,
        )
        conflict = mcp1.save_settings_smart(
            account_plan="personal",
            cloudcode_upstream="auto",
            raw=True,
        )
        self.assertTrue(
            McpStdioClient.is_tool_error(conflict),
            f"expected mutual-exclusion error, got: {conflict}",
        )
        mcp1.assert_clean_stdout()
        mcp1.close()
        p1.stop()

        # Restart and confirm `personal` persisted, then switch to `organization`.
        p2 = self.start_proxy(env, config_path=cfg, port=port)
        mcp2 = self.start_mcp(env, config_path=cfg)
        snap_p = mcp2.call_tool("get_settings", {})["structuredContent"]
        self.assertEqual(
            snap_p["account_plan"],
            {"mode": "personal", "resolved": "personal"},
        )
        self.assertEqual(
            snap_p["config"]["providers"]["gemini"]["enabled_models"],
            ["gemini-4-carbon", "gemini-4-neon"],
        )
        plan_p_json = json.dumps(snap_p["account_plan"])
        self.assertNotIn("cloudcode-pa.googleapis.com", plan_p_json)
        self.assertNotIn("https://", plan_p_json)

        mcp2.save_settings_smart(account_plan="organization", raw=False)
        mcp2.assert_clean_stdout()
        mcp2.close()
        p2.stop()

        # Restart and confirm `organization`, then switch back to `auto`.
        p3 = self.start_proxy(env, config_path=cfg, port=port)
        mcp3 = self.start_mcp(env, config_path=cfg)
        snap_o = mcp3.call_tool("get_settings", {})["structuredContent"]
        self.assertEqual(
            snap_o["account_plan"],
            {"mode": "organization", "resolved": "organization"},
        )
        plan_o_json = json.dumps(snap_o["account_plan"])
        self.assertNotIn("cloudcode-pa.googleapis.com", plan_o_json)
        self.assertNotIn("https://", plan_o_json)

        snap_a = mcp3.save_settings_smart(account_plan="auto", raw=False)["structuredContent"]
        self.assertEqual(snap_a["account_plan"]["mode"], "auto")
        self.assertIsNone(snap_a["account_plan"]["resolved"])
        mcp3.assert_clean_stdout()

        # CLI continuity: doctor & models with explicit hermetic env.
        doc = run_cli(
            [str(binary), "doctor", "--config", str(cfg), "--port", str(p3.port), "--json"],
            env=env.build_env(),
        )
        self.assertEqual(doc.returncode, 0, f"doctor failed: {doc.stderr}")
        doc_json = doc.json_stdout()
        self.assertIs(doc_json["ok"], True)
        self.assertIs(doc_json["proxy"]["reachable"], True)
        self.assertIs(doc_json["version_skew"], False)

        mods = run_cli(
            [str(binary), "models", "--config", str(cfg), "--port", str(p3.port), "--json"],
            env=env.build_env(),
        )
        self.assertEqual(mods.returncode, 0, f"models failed: {mods.stderr}")
        models_list = extract_models_list(mods.json_stdout())
        self.assertGreaterEqual(len(models_list), 1)
        self.assertIn(f"127.0.0.1:{p3.port}", mods.stdout)
