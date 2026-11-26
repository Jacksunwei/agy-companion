"""Core opaque-box binary CUJ suite (`--suite cuj`, 12 cohesive tests).

Exercises the compiled `agy-companion` binary end-to-end across:
- Gemini unary + chunked SSE byte-for-byte pass-through, usage tee, faults & concurrency
- Model discovery (`GET /v1beta/models`) & Gemini 3.8+/4+/5+ routing
- `ui-mcp` stdio 6-tool lifecycle, hot-reload, `get_usage`, launcher toggle, HTML override & key clear
- CLI subcommands (`--version`, `models`, `doctor`/skew, `integration`, `hook-env`, `launch`)
- Daemon `serve --adopt-running`, `SIGTERM`, and active-SSE idle guard
- CloudCode Day-0 -> Day-1 catalog merge & Enterprise 403 per-account LRU
- `install` / upgrade / default `uninstall` / `--purge` / `--dry-run` lifecycle
"""

from __future__ import annotations

import concurrent.futures
import hashlib
import json
import os
import re
import shlex
import shutil
import signal
import stat
import sys
import time
from pathlib import Path
from typing import Any

from harness import (
    CompanionProcess,
    E2ETestCase,
    IsolatedEnv,
    extract_models_list,
    find_free_port,
    http_raw_request,
    http_stream_request,
    open_usage_db_readonly,
    poll_stats,
    poll_usage_events,
    run_cli,
    usage_storage_files,
)
from mock_gemini import (
    CapturedRequest,
    ConnectionResetAction,
    EchoChecksumAction,
    JsonAction,
    RawBytesAction,
    SseStreamAction,
    build_sse_events,
    split_across_utf8_char,
)


_PLACEHOLDER_KEY = "agy-companion-local"
_PLACEHOLDER_SLOT_RE = re.compile(r"^MODEL_PLACEHOLDER_M(\d+)$")


def _sample_ccpa_catalog() -> dict[str, Any]:
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
        f"exec {shlex.quote(sys.executable)} -B -c "
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
        time.sleep(0.02)
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
        time.sleep(0.02)
    return None


class TestCujGeminiProxy(E2ETestCase):
    SUITE = "cuj"
    FEATURE = "PROXY"

    def test_cuj_gemini_unary_and_sse_passthrough_and_usage_tee(self) -> None:
        """Unary + split-UTF-8 CRLF SSE forward byte-for-byte with ?key= scrubbed, accurate /api/stats & SQLite WAL usage."""
        env = self.make_env()
        mock = self.start_mock_gemini()
        api_key = "AIzaSyCujUnaryKey1234"
        cfg = env.write_config(
            env.make_config_dict(
                base_url=mock.base_url,
                api_key=api_key,
                default_model="gemini-4-hydrogen",
            )
        )
        proxy = self.start_proxy(env, config_path=cfg)

        # (a) Unary request with non-canonical whitespace & ?key=LEAKED.
        unary_body = (
            b'{\n  "contents": [  { "role": "user", "parts": [ { "text": "preserve exact bytes" } ] } ]\n}\n'
        )
        unary_sha = hashlib.sha256(unary_body).hexdigest()
        unary_usage = {
            "promptTokenCount": 120,
            "candidatesTokenCount": 30,
            "cachedContentTokenCount": 60,
            "thoughtsTokenCount": 12,
        }
        mock.enqueue(EchoChecksumAction(usage=unary_usage))
        resp = http_raw_request(
            "127.0.0.1",
            proxy.port,
            "POST",
            "/v1beta/models/gemini-4-hydrogen:generateContent?key=LEAKED_QUERY_KEY",
            headers={"Content-Type": "application/json"},
            body=unary_body,
        )
        self.assertEqual(resp.status, 200, resp.text)
        self.assertIn("application/json", (resp.header("content-type") or "").lower())
        unary_json = resp.json()
        self.assertEqual(unary_json["echo"]["sha256"], unary_sha)
        self.assertEqual(unary_json["echo"]["length"], len(unary_body))

        up_unary = mock.requests[-1]
        self.assertEqual(up_unary.body_sha256, unary_sha)
        self.assertEqual(up_unary.header("x-goog-api-key"), api_key)
        self.assertNotIn("key", up_unary.query_params)
        self.assertNotIn("LEAKED_QUERY_KEY", up_unary.raw_target)

        # (b) Streaming SSE with split 4-byte UTF-8 emoji + CRLF + 2 cumulative usageMetadata frames.
        sse_payload = build_sse_events(
            [
                {
                    "candidates": [
                        {"content": {"role": "model", "parts": [{"text": "rocket \U0001f680 part1"}]}}
                    ],
                    "usageMetadata": {
                        "promptTokenCount": 200,
                        "candidatesTokenCount": 10,
                        "cachedContentTokenCount": 100,
                        "thoughtsTokenCount": 4,
                    },
                },
                {
                    "candidates": [
                        {"content": {"role": "model", "parts": [{"text": " part2"}]}}
                    ],
                    "usageMetadata": {
                        "promptTokenCount": 200,
                        "candidatesTokenCount": 45,
                        "cachedContentTokenCount": 100,
                        "thoughtsTokenCount": 8,
                    },
                },
            ],
            line_ending=b"\r\n",
            include_keepalive=True,
        )
        split_chunks = split_across_utf8_char(sse_payload, "\U0001f680")
        mock.enqueue(SseStreamAction(chunks=split_chunks, chunk_delay_s=0.05))

        received_chunks: list[tuple[bytes, float]] = []
        with http_stream_request(
            "127.0.0.1",
            proxy.port,
            "POST",
            "/v1beta/models/gemini-3.8-flash:streamGenerateContent?alt=sse&key=LEAKED_SSE",
            headers={"Content-Type": "application/json"},
            body=b'{"contents":[{"parts":[{"text":"stream"}]}]}',
        ) as stream:
            self.assertEqual(stream.status, 200)
            self.assertIn("text/event-stream", (stream.header("content-type") or "").lower())
            for chunk, ts in stream.iter_chunks():
                received_chunks.append((chunk, ts))

        full_stream = b"".join(c for c, _ in received_chunks)
        self.assertEqual(full_stream, sse_payload)
        self.assertGreaterEqual(len(received_chunks), 2)

        up_sse = mock.requests[-1]
        self.assertEqual(up_sse.header("x-goog-api-key"), api_key)
        self.assertEqual(up_sse.query_params.get("alt"), ["sse"])
        self.assertNotIn("key", up_sse.query_params)

        # (c) /api/stats sums unary + FINAL cumulative SSE frame (not intermediate frame).
        stats = poll_stats(proxy.port, min_requests=2, timeout=3.0)
        usage = stats.get("usage", {})
        self.assertEqual(usage.get("requests"), 2, stats)
        self.assertEqual(usage.get("prompt_tokens"), 320, stats)
        self.assertEqual(usage.get("cached_tokens"), 160, stats)
        self.assertEqual(usage.get("output_tokens"), 75, stats)
        self.assertEqual(usage.get("reasoning_tokens"), 20, stats)
        self.assertAlmostEqual(usage.get("cache_hit_ratio", -1.0), 0.5, places=4)
        if os.name != "nt":
            self.assertIsInstance(stats.get("proxy_rss_bytes"), int)
            self.assertGreater(stats["proxy_rss_bytes"], 0)
        self.assertEqual(len(stats.get("recent_calls") or []), 2)
        self.assertGreaterEqual(len(stats.get("by_model") or []), 2)

        # (d) SQLite WAL usage.db integrity, 0600 permissions, canonical columns, zero JSONL.
        files, events = poll_usage_events(env.usage_dir, min_count=2, timeout=3.0)
        self.assertEqual(len(files), 1)
        self.assertEqual(len(events), 2)
        self.assertEqual(list(env.usage_dir.glob("*.jsonl")), [])
        self.assertEqual(len({e["request_id"] for e in events}), 2)
        expected_cols = {
            "timestamp_utc",
            "request_id",
            "provider",
            "model",
            "input_tokens",
            "output_tokens",
            "cached_tokens",
            "reasoning_tokens",
            "latency_ms",
            "source",
        }
        for ev in events:
            self.assertEqual(set(ev.keys()), expected_cols)
        with open_usage_db_readonly(files[0]) as conn:
            user_ver = conn.execute("PRAGMA user_version").fetchone()[0]
            jmode = str(conn.execute("PRAGMA journal_mode").fetchone()[0]).lower()
        self.assertEqual(user_ver, 1)
        self.assertEqual(jmode, "wal")
        if os.name != "nt":
            for sf in usage_storage_files(env.usage_dir):
                self.assertEqual(stat.S_IMODE(os.lstat(sf).st_mode), 0o600, f"Bad mode on {sf}")

    def test_cuj_gemini_upstream_fault_recovery_and_concurrency(self) -> None:
        """Proxy propagates 400/429/500 verbatim, returns 502 on TCP RST, recovers, and isolates 4 concurrent calls."""
        env = self.make_env()
        mock = self.start_mock_gemini()
        api_key = "AIzaSyCujFaultKey5678"
        cfg = env.write_config(env.make_config_dict(base_url=mock.base_url, api_key=api_key))
        proxy = self.start_proxy(env, config_path=cfg)

        mock.enqueue(
            JsonAction(status=400, payload={"error": {"code": 400, "status": "API_KEY_INVALID"}})
        )
        mock.enqueue(JsonAction(status=429, payload={"error": {"code": 429, "message": "rate limit"}}))
        mock.enqueue(JsonAction(status=500, payload={"error": {"code": 500, "message": "boom"}}))
        mock.enqueue(ConnectionResetAction())

        for expected_status in (400, 429, 500):
            r = http_raw_request(
                "127.0.0.1",
                proxy.port,
                "POST",
                "/v1beta/models/gemini-4-hydrogen:generateContent",
                headers={"Content-Type": "application/json"},
                body=b'{"contents":[]}',
            )
            self.assertEqual(r.status, expected_status, r.text)
            self.assertNotIn(api_key, r.text)

        r_rst = http_raw_request(
            "127.0.0.1",
            proxy.port,
            "POST",
            "/v1beta/models/gemini-4-hydrogen:generateContent",
            headers={"Content-Type": "application/json"},
            body=b'{"contents":[]}',
        )
        self.assertEqual(r_rst.status, 502, r_rst.text)
        self.assertNotIn(api_key, r_rst.text)

        # Immediate recovery to 200.
        r_ok = http_raw_request(
            "127.0.0.1",
            proxy.port,
            "POST",
            "/v1beta/models/gemini-4-hydrogen:generateContent",
            headers={"Content-Type": "application/json"},
            body=b'{"contents":[{"parts":[{"text":"recovered"}]}]}',
        )
        self.assertEqual(r_ok.status, 200, r_ok.text)

        # 4 concurrent mixed unary + SSE requests.
        def _handler(req: CapturedRequest) -> JsonAction | SseStreamAction:
            if req.path.endswith(":streamGenerateContent"):
                body_str = req.body.decode("utf-8")
                sse = build_sse_events(
                    [
                        {
                            "candidates": [{"content": {"parts": [{"text": body_str}]}}],
                            "usageMetadata": {
                                "promptTokenCount": 10,
                                "candidatesTokenCount": 5,
                                "cachedContentTokenCount": 0,
                                "thoughtsTokenCount": 0,
                            },
                        }
                    ]
                )
                return SseStreamAction(chunks=[sse])
            return EchoChecksumAction()

        mock.clear_requests()
        mock.set_handler(_handler)

        def _worker(idx: int) -> tuple[int, str]:
            is_sse = idx % 2 == 1
            marker = f"concurrent-req-{idx}"
            body = json.dumps({"marker": marker}).encode("utf-8")
            path = (
                f"/v1beta/models/gemini-4-hydrogen:streamGenerateContent?alt=sse&key=LEAK_{idx}"
                if is_sse
                else f"/v1beta/models/gemini-4-hydrogen:generateContent?key=LEAK_{idx}"
            )
            resp = http_raw_request(
                "127.0.0.1",
                proxy.port,
                "POST",
                path,
                headers={"Content-Type": "application/json"},
                body=body,
            )
            if is_sse:
                return resp.status, (marker if marker in resp.text else resp.text)
            expected_sha = hashlib.sha256(body).hexdigest()
            return resp.status, (marker if resp.json()["echo"]["sha256"] == expected_sha else resp.text)

        with concurrent.futures.ThreadPoolExecutor(max_workers=4) as pool:
            results = list(pool.map(_worker, range(4)))
        for idx, (status, tag) in enumerate(results):
            self.assertEqual(status, 200, f"worker {idx}: {tag}")
            self.assertEqual(tag, f"concurrent-req-{idx}")
        self.assertEqual(len(mock.requests), 4)
        for req in mock.requests:
            self.assertNotIn("key", req.query_params)
            self.assertEqual(req.header("x-goog-api-key"), api_key)

        _, events = poll_usage_events(env.usage_dir, min_count=5, timeout=3.0)
        self.assertGreaterEqual(len(events), 5)

    def test_cuj_gemini_models_discovery_and_5_x_routing(self) -> None:
        """GET /v1beta/models filters Gemini 3.8+/4+/5+ with fallback; 5.x/3.9 route 200; legacy models fail 400."""
        env = self.make_env()
        mock = self.start_mock_gemini()
        cfg = env.write_config(
            env.make_config_dict(
                base_url=mock.base_url,
                api_key="AIzaSyCujModels1234",
                default_model="gemini-3.8-flash",
            )
        )
        proxy = self.start_proxy(env, config_path=cfg)

        mock.enqueue(
            JsonAction(
                status=200,
                payload={
                    "models": [
                        {"name": "models/gemini-1.5-pro"},
                        {"name": "models/gemini-4-hydrogen"},
                        {"name": "models/gemini-3.8-flash"},
                        {"name": "models/gemini-5-pro"},
                    ]
                },
            )
        )
        r_list = http_raw_request("127.0.0.1", proxy.port, "GET", "/v1beta/models")
        self.assertEqual(r_list.status, 200, r_list.text)
        names = [m["name"] for m in r_list.json().get("models", [])]
        self.assertTrue(names, r_list.text)
        self.assertEqual(names[0], "models/gemini-3.8-flash")
        self.assertIn("models/gemini-5-pro", names)
        self.assertNotIn("models/gemini-1.5-pro", names)

        # Upstream 500 falls back cleanly to curated defaults.
        mock.enqueue(JsonAction(status=500, payload={"error": "upstream down"}))
        r_fb = http_raw_request("127.0.0.1", proxy.port, "GET", "/v1beta/models")
        self.assertEqual(r_fb.status, 200, r_fb.text)
        self.assertTrue(r_fb.json().get("models"))

        # Supported 5.x and 3.9 paths forward verbatim.
        for model_id in ("gemini-5-pro", "gemini-3.9-pro"):
            r_gen = http_raw_request(
                "127.0.0.1",
                proxy.port,
                "POST",
                f"/v1beta/models/{model_id}:generateContent",
                headers={"Content-Type": "application/json"},
                body=b'{"contents":[]}',
            )
            self.assertEqual(r_gen.status, 200, r_gen.text)
            self.assertEqual(mock.requests[-1].path, f"/v1beta/models/{model_id}:generateContent")

        # Unsupported legacy / incomplete models fail 400 with zero upstream hits.
        mock.clear_requests()
        for bad_model in ("gemini-2.5-pro", "gemini-1.5-pro", "gemini-5"):
            r_bad = http_raw_request(
                "127.0.0.1",
                proxy.port,
                "POST",
                f"/v1beta/models/{bad_model}:generateContent",
                headers={"Content-Type": "application/json"},
                body=b'{"contents":[]}',
            )
            self.assertEqual(r_bad.status, 400, f"{bad_model}: {r_bad.text}")
            self.assertEqual(r_bad.header("x-content-type-options"), "nosniff")
            self.assertIn("no-store", r_bad.header("cache-control") or "")
            self.assertIn("Gemini 3.8+ and Gemini 4+", r_bad.text)
            self.assertIn("unset GOOGLE_GEMINI_BASE_URL", r_bad.text)
        self.assertEqual(len(mock.requests), 0)


class TestCujUiMcpLifecycle(E2ETestCase):
    SUITE = "cuj"
    FEATURE = "UI_MCP"

    def test_cuj_ui_mcp_handshake_6_tools_hot_reload_and_usage(self) -> None:
        """ui-mcp serves 6 tools + UI resource, hot-reloads key into live proxy, tests connection, and queries get_usage."""
        env = self.make_env()
        mock = self.start_mock_gemini()
        key_v1 = "AIzaSyMcpKeyV1_1111"
        key_v2 = "AIzaSyMcpKeyV2_2222"
        port = find_free_port()
        cfg = env.write_config(
            env.make_config_dict(
                listen_port=port,
                base_url=mock.base_url,
                api_key=key_v1,
                default_model="gemini-4-hydrogen",
            )
        )
        proxy = self.start_proxy(env, config_path=cfg, port=port)
        mcp = self.start_mcp(
            env,
            config_path=cfg,
            extra_env={"RUST_LOG": "debug", "SHELL": "/bin/zsh", "ZDOTDIR": str(env.home_dir)},
            auto_initialize=False,
        )

        # (a) Initialize handshake.
        init = mcp.initialize(timeout=10.0)
        mcp.assert_clean_stdout()
        self.assertIn("tools", init.get("capabilities", {}))
        self.assertIn("resources", init.get("capabilities", {}))
        self.assertEqual(init["serverInfo"]["version"], "0.0.2")

        # (b) Resources list & read.
        res_list = mcp.list_resources()
        mcp.assert_clean_stdout()
        uris = [r["uri"] for r in res_list.get("resources", [])]
        self.assertIn("ui://companion/settings.html", uris)
        res_read = mcp.read_resource("ui://companion/settings.html")
        mcp.assert_clean_stdout()
        item = res_read["contents"][0]
        self.assertEqual(item.get("mimeType"), "text/html;profile=mcp-app")
        html_text = item.get("text", "")
        self.assertGreater(len(html_text), 0)
        self.assertLess(len(html_text.encode("utf-8")), 512_000)

        # (c) Exact 6 tools with _meta.ui contracts.
        tools = {t["name"]: t for t in mcp.list_tools().get("tools", [])}
        mcp.assert_clean_stdout()
        expected_tools = {
            "open_settings",
            "get_settings",
            "save_settings",
            "test_connection",
            "set_launcher_integration",
            "get_usage",
        }
        self.assertEqual(set(tools.keys()), expected_tools)
        open_meta = tools["open_settings"].get("_meta") or {}
        self.assertEqual((open_meta.get("ui") or {}).get("resourceUri"), "ui://companion/settings.html")
        self.assertTrue((open_meta.get("com.google.antigravity/ui") or {}).get("entrypoints"))
        for app_tool in expected_tools - {"open_settings"}:
            self.assertEqual(((tools[app_tool].get("_meta") or {}).get("ui") or {}).get("visibility"), ["app"])

        # (d) open_settings & get_settings dual content + structuredContent.
        for tool_name in ("open_settings", "get_settings"):
            res = mcp.call_tool(tool_name, {})
            mcp.assert_clean_stdout()
            self.assertTrue(res.get("content"))
            sc = res["structuredContent"]
            self.assertIs(sc["proxy"]["reachable"], True)
            if os.name != "nt":
                self.assertIsInstance(sc.get("proxy_rss_bytes"), int)
                self.assertGreater(sc["proxy_rss_bytes"], 0)
            self.assertIn("account_plan", sc)
            self.assertTrue(sc.get("supported_models"))
            gem = sc["config"]["providers"]["gemini"]
            self.assertIs(gem["api_key_configured"], True)
            self.assertEqual(gem["api_key_hint"], "AIza...1111")
            self.assertNotIn(key_v1, json.dumps(res))

        # (e) save_settings updates key_v1 -> key_v2 atomically at 0600, live proxy hot-reloads.
        save_res = mcp.save_settings_smart(api_key=key_v2, default_model="gemini-3.8-flash")
        mcp.assert_clean_stdout()
        self.assertEqual(save_res["structuredContent"]["config"]["providers"]["gemini"]["api_key_hint"], "AIza...2222")
        if os.name != "nt":
            self.assertEqual(stat.S_IMODE(os.lstat(cfg).st_mode), 0o600)
        self.assertEqual(list(cfg.parent.glob("*.tmp")), [])

        env.bump_mtime(cfg, delta=3.0)
        r_gen = http_raw_request(
            "127.0.0.1",
            proxy.port,
            "POST",
            "/v1beta/models/gemini-4-hydrogen:generateContent",
            headers={"Content-Type": "application/json"},
            body=b'{"contents":[]}',
        )
        self.assertEqual(r_gen.status, 200, r_gen.text)
        self.assertEqual(mock.requests[-1].header("x-goog-api-key"), key_v2)

        # (f) test_connection hits GET /v1beta/models?pageSize=1 with key_v2.
        mock.expected_api_key = key_v2
        tc_res = mcp.call_tool("test_connection", {})
        mcp.assert_clean_stdout()
        self.assertIs(tc_res["structuredContent"]["ok"], True, tc_res)
        self.assertEqual(mock.requests[-1].method, "GET")
        self.assertEqual(mock.requests[-1].path, "/v1beta/models")
        self.assertEqual(mock.requests[-1].query_params.get("pageSize"), ["1"])
        self.assertEqual(mock.requests[-1].header("x-goog-api-key"), key_v2)

        # (g) get_usage("1d") reflects SQLite usage.db row from proxy call above.
        poll_usage_events(env.usage_dir, min_count=1, timeout=3.0)
        usage_res = mcp.call_tool("get_usage", {"range": "1d"})
        mcp.assert_clean_stdout()
        us = usage_res["structuredContent"]
        self.assertEqual(us.get("range"), "1d")
        self.assertGreaterEqual(us.get("totals", {}).get("requests", 0), 1)

    def test_cuj_ui_mcp_launcher_integration_and_html_override(self) -> None:
        """AGY_COMPANION_UI_HTML reloads dynamically without restart; set_launcher_integration toggles env.sh & .zlogin."""
        if os.name == "nt":
            self.skipTest("POSIX launcher integration test")
        binary = self.require_binary()
        env = self.make_env()
        cfg = env.write_config(env.make_config_dict())
        html_file = env.root / "override.html"
        html_file.write_text("<html><body>V1-OVERRIDE</body></html>", encoding="utf-8")

        extra_env = {
            "AGY_COMPANION_UI_HTML": str(html_file),
            "SHELL": "/bin/zsh",
            "ZDOTDIR": str(env.home_dir),
        }
        mcp = self.start_mcp(env, config_path=cfg, extra_env=extra_env)

        r1 = mcp.read_resource("ui://companion/settings.html")
        mcp.assert_clean_stdout()
        self.assertIn("V1-OVERRIDE", r1["contents"][0]["text"])

        html_file.write_text("<html><body>V2-RELOADED</body></html>", encoding="utf-8")
        r2 = mcp.read_resource("ui://companion/settings.html")
        mcp.assert_clean_stdout()
        self.assertIn("V2-RELOADED", r2["contents"][0]["text"])

        en = mcp.call_tool("set_launcher_integration", {"enabled": True})
        mcp.assert_clean_stdout()
        self.assertIs(en["structuredContent"]["launcher_integration"]["enabled"], True)
        env_sh = env.data_dir / "env.sh"
        zlogin = env.home_dir / ".zlogin"
        self.assertTrue(env_sh.is_file())
        self.assertEqual(stat.S_IMODE(os.lstat(env_sh).st_mode), 0o600)
        self.assertTrue(zlogin.is_file())

        cli_st = run_cli(
            [str(binary), "integration", "status", "--shell", "zsh", "--json"],
            env=env.build_env(extra=extra_env),
        )
        self.assertEqual(cli_st.returncode, 0, cli_st.stderr)
        self.assertIs(cli_st.json_stdout().get("enabled"), True)

        dis = mcp.call_tool("set_launcher_integration", {"enabled": False})
        mcp.assert_clean_stdout()
        self.assertIs(dis["structuredContent"]["launcher_integration"]["enabled"], False)
        self.assertFalse(env_sh.exists())
        self.assertFalse(zlogin.exists())

    def test_cuj_ui_mcp_key_preservation_and_clear(self) -> None:
        """Omitting api_key preserves saved key; explicit clear removes key from disk and get_settings."""
        env = self.make_env()
        cfg = env.write_config(env.make_config_dict())
        mcp = self.start_mcp(env, config_path=cfg)

        key_v1 = "AIzaSyPreserveMe1234"
        mcp.save_settings_smart(api_key=key_v1)
        mcp.save_settings_smart(default_model="gemini-3.8-flash")
        mcp.assert_clean_stdout()

        on_disk = json.loads(cfg.read_text(encoding="utf-8"))
        self.assertEqual(on_disk["providers"]["gemini"]["api_key"], key_v1)
        snap1 = mcp.call_tool("get_settings", {})["structuredContent"]
        self.assertIs(snap1["config"]["providers"]["gemini"]["api_key_configured"], True)
        self.assertEqual(snap1["config"]["providers"]["gemini"]["default_model"], "gemini-3.8-flash")

        mcp.save_settings_smart(clear_api_key=True)
        mcp.assert_clean_stdout()
        on_disk2 = json.loads(cfg.read_text(encoding="utf-8"))
        self.assertIsNone(on_disk2["providers"]["gemini"].get("api_key"))
        snap2 = mcp.call_tool("get_settings", {})["structuredContent"]
        self.assertIs(snap2["config"]["providers"]["gemini"]["api_key_configured"], False)
        self.assertIsNone(snap2["config"]["providers"]["gemini"].get("api_key_hint"))


class TestCujCliSubcommands(E2ETestCase):
    SUITE = "cuj"
    FEATURE = "CLI"

    def test_cuj_cli_version_models_doctor_and_version_skew(self) -> None:
        """--version, --help (9 subcommands), models --json routing, doctor healthy/skew/unbound/0644 checks."""
        binary = self.require_binary()
        env = self.make_env()
        mock = self.start_mock_gemini()
        base_env = env.build_env()

        # (a) --version, --help, unknown subcommand.
        r_ver = run_cli([str(binary), "--version"], env=base_env)
        self.assertEqual(r_ver.returncode, 0, r_ver.stderr)
        self.assertEqual(r_ver.stdout.strip(), "agy-companion 0.0.2")

        r_help = run_cli([str(binary), "--help"], env=base_env)
        self.assertEqual(r_help.returncode, 0, r_help.stderr)
        for sub in ("serve", "ui-mcp", "doctor", "models", "launch", "hook-env", "integration", "install", "uninstall"):
            self.assertIn(sub, r_help.stdout)

        r_unk = run_cli([str(binary), "no-such-subcommand"], env=base_env)
        self.assertNotEqual(r_unk.returncode, 0)

        # (b) models --json --port <p> routes through live proxy.
        port = find_free_port()
        cfg = env.write_config(
            env.make_config_dict(
                listen_port=port,
                base_url=mock.base_url,
                api_key="AIzaSyCliDoctor1234",
            ),
            mode=0o600,
        )
        proxy = self.start_proxy(env, config_path=cfg, port=port)

        r_models = run_cli([str(binary), "models", "--json", "--port", str(port)], env=base_env)
        self.assertEqual(r_models.returncode, 0, r_models.stderr)
        models_list = extract_models_list(r_models.json_stdout())
        self.assertTrue(models_list)
        first = models_list[0]
        self.assertIn(f"http://127.0.0.1:{port}", json.dumps(first))
        model_id = first.get("model") or first.get("id") or "gemini-4-hydrogen"
        r_route = http_raw_request(
            "127.0.0.1",
            proxy.port,
            "POST",
            f"/v1beta/models/{model_id}:generateContent",
            headers={"Content-Type": "application/json"},
            body=b'{"contents":[]}',
        )
        self.assertEqual(r_route.status, 200, r_route.text)

        # (c) doctor --json healthy against live proxy + 0600 config + matching GOOGLE_GEMINI_BASE_URL.
        doc_env = env.build_env(extra={"GOOGLE_GEMINI_BASE_URL": f"http://127.0.0.1:{port}"})
        r_doc = run_cli([str(binary), "doctor", "--json", "--config", str(cfg)], env=doc_env)
        self.assertEqual(r_doc.returncode, 0, f"{r_doc.stdout}\n{r_doc.stderr}")
        d_ok = r_doc.json_stdout()
        self.assertIs(d_ok["ok"], True, d_ok)
        self.assertIs(d_ok["version_skew"], False, d_ok)

        # (d) Version skew against MockHealthServer(version="9.9.9") on both doctor and ui-mcp.
        skew_srv = self.start_mock_health(version="9.9.9")
        skew_cfg = env.write_config(
            env.make_config_dict(listen_port=skew_srv.port, api_key="AIzaSyCliSkew123456"),
            path=env.root / "skew.json",
            mode=0o600,
        )
        r_skew = run_cli(
            [str(binary), "doctor", "--json", "--config", str(skew_cfg)],
            env=env.build_env(extra={"GOOGLE_GEMINI_BASE_URL": f"http://127.0.0.1:{skew_srv.port}"}),
        )
        self.assertNotEqual(r_skew.returncode, 0)
        self.assertIs(r_skew.json_stdout()["version_skew"], True)

        mcp = self.start_mcp(env, config_path=skew_cfg)
        snap = mcp.call_tool("get_settings", {})["structuredContent"]
        mcp.assert_clean_stdout()
        self.assertIs(snap["version_skew"], True)

        # (e) Unbound ephemeral port + 0644 config fails doctor.
        dead_port = find_free_port()
        bad_cfg = env.write_config(
            env.make_config_dict(listen_port=dead_port, api_key="AIzaSyCliDead123456"),
            path=env.root / "dead.json",
            mode=0o644,
        )
        r_dead = run_cli([str(binary), "doctor", "--json", "--config", str(bad_cfg)], env=base_env)
        self.assertNotEqual(r_dead.returncode, 0)
        self.assertIs(r_dead.json_stdout()["ok"], False)

    def test_cuj_cli_integration_hook_env_and_launch_lifecycle(self) -> None:
        """integration enable/disable + real sh/zsh env.sh sourcing, hook-env daemon start/warm hit, and launch env restore."""
        if os.name == "nt":
            self.skipTest("POSIX integration/hook-env/launch test")
        binary = self.require_binary()
        env = self.make_env()
        self.register_pidfile_cleanup(env=env)
        port = find_free_port()
        cfg = env.write_config(env.make_config_dict(listen_port=port))

        # (a) integration enable preserves existing .zlogin lines; sh -n & live sh/zsh sourcing activate then scrub.
        zlogin = env.home_dir / ".zlogin"
        original_rc = "# user existing line\nexport KEEP_USER_VAR=yes\n"
        zlogin.write_text(original_rc, encoding="utf-8")
        shell_env = env.build_env(extra={"SHELL": "/bin/zsh", "ZDOTDIR": str(env.home_dir)})

        r_en = run_cli([str(binary), "integration", "enable", "--shell", "zsh", "--json"], env=shell_env)
        self.assertEqual(r_en.returncode, 0, r_en.stderr)
        env_sh = env.data_dir / "env.sh"
        self.assertTrue(env_sh.is_file())
        self.assertEqual(stat.S_IMODE(os.lstat(env_sh).st_mode), 0o600)
        env_sh_text = env_sh.read_text(encoding="utf-8")
        self.assertIsNone(re.search(r"(?m)^[^#]*\beval\b", env_sh_text))
        self.assertIn("export KEEP_USER_VAR=yes", zlogin.read_text(encoding="utf-8"))

        r_syn = run_cli(["sh", "-n", str(env_sh)], env=shell_env)
        self.assertEqual(r_syn.returncode, 0, r_syn.stderr)

        # (b) hook-env with empty plugins dir fails open; with plugin.json cold-starts then warm-hits.
        empty_plugins = env.root / "empty_plugins"
        empty_plugins.mkdir(parents=True, exist_ok=True)
        r_open = run_cli(
            [str(binary), "hook-env", "--ensure-running", "--print-port", "--plugins-dir", str(empty_plugins)],
            env=env.build_env(extra={"AGY_COMPANION_CONFIG": str(cfg)}),
        )
        self.assertEqual(r_open.returncode, 0, r_open.stderr)
        self.assertEqual(r_open.stdout.strip(), "")

        _install_dummy_plugin_json(env)
        r_cold = run_cli(
            [str(binary), "hook-env", "--ensure-running", "--print-port"],
            env=env.build_env(extra={"AGY_COMPANION_CONFIG": str(cfg)}),
        )
        self.assertEqual(r_cold.returncode, 0, r_cold.stderr)
        self.assertEqual(r_cold.stdout.strip(), str(port))
        self.assertIsNotNone(_wait_for_health(port, timeout=4.0))
        pid_data_1 = json.loads((env.data_dir / "proxy.pid").read_text(encoding="utf-8"))

        r_warm = run_cli(
            [str(binary), "hook-env", "--ensure-running", "--print-port"],
            env=env.build_env(extra={"AGY_COMPANION_CONFIG": str(cfg)}),
        )
        self.assertEqual(r_warm.returncode, 0, r_warm.stderr)
        self.assertEqual(r_warm.stdout.strip(), str(port))
        pid_data_2 = json.loads((env.data_dir / "proxy.pid").read_text(encoding="utf-8"))
        self.assertEqual(pid_data_1["pid"], pid_data_2["pid"])

        # Source env.sh under real sh (and zsh if present): activates on ANTIGRAVITY_RESOLVING_ENVIRONMENT=1, scrubs after.
        shells = [["sh", "-c"]]
        if shutil.which("zsh"):
            shells.append(["zsh", "-f", "-c"])
        probe_script = (
            '. "$1"\n'
            'printf "ACT:%s|%s|%s|%s|%s|%s\\n" '
            '"${CLOUD_CODE_URL:-}" "${GOOGLE_GEMINI_BASE_URL:-}" "${GEMINI_API_KEY:-}" '
            '"${AGY_COMPANION_HOOK_ACTIVE:-}" "${JETSKI_MCP_ENV_RESTORE_PREFIX:-}" "${JETSKI_MCP_ENV_RESTORE_VARS:-}"\n'
            'unset ANTIGRAVITY_RESOLVING_ENVIRONMENT\n'
            '. "$1"\n'
            'printf "SCRUB:%s|%s|%s|%s\\n" '
            '"${CLOUD_CODE_URL-<UNSET>}" "${GOOGLE_GEMINI_BASE_URL-<UNSET>}" '
            '"${GEMINI_API_KEY-<UNSET>}" "${AGY_COMPANION_HOOK_ACTIVE-<UNSET>}"\n'
        )
        for sh_prefix in shells:
            act = run_cli(
                [*sh_prefix, probe_script, "_", str(env_sh)],
                env=env.build_env(
                    extra={
                        "ZDOTDIR": str(env.home_dir),
                        "ANTIGRAVITY_RESOLVING_ENVIRONMENT": "1",
                        "AGY_COMPANION_CONFIG": str(cfg),
                    }
                ),
            )
            self.assertEqual(act.returncode, 0, act.stderr)
            lines = act.stdout.splitlines()
            self.assertGreaterEqual(len(lines), 2, act.stdout)
            fields = lines[0].removeprefix("ACT:").split("|")
            self.assertEqual(fields[0], f"http://127.0.0.1:{port}")
            self.assertEqual(fields[1], f"http://127.0.0.1:{port}")
            self.assertEqual(fields[2], _PLACEHOLDER_KEY)
            self.assertEqual(fields[3], "1")
            self.assertTrue(fields[4])
            self.assertIn("CLOUD_CODE_URL", fields[5])
            self.assertEqual(lines[1], "SCRUB:<UNSET>|<UNSET>|<UNSET>|<UNSET>")

        # Disable restores user .zlogin cleanly.
        r_dis = run_cli([str(binary), "integration", "disable", "--shell", "zsh", "--json"], env=shell_env)
        self.assertEqual(r_dis.returncode, 0, r_dis.stderr)
        self.assertEqual(zlogin.read_text(encoding="utf-8"), original_rc)

        # (c) launch --print-env & launch --app reuses warm daemon & preserves pre-existing real keys.
        r_pe = run_cli(
            [str(binary), "launch", "--port", str(port), "--config", str(cfg), "--app", "/opt/antigravity", "--print-env"],
            env=env.build_env(),
        )
        self.assertEqual(r_pe.returncode, 0, r_pe.stderr)
        self.assertIn(f'CLOUD_CODE_URL="http://127.0.0.1:{port}"', r_pe.stdout)
        self.assertIn(f'GOOGLE_GEMINI_BASE_URL="http://127.0.0.1:{port}"', r_pe.stdout)
        self.assertIn("JETSKI_MCP_ENV_RESTORE_PREFIX=", r_pe.stdout)
        self.assertIn("JETSKI_MCP_ENV_RESTORE_VARS=", r_pe.stdout)

        dump_path = env.root / "launch_env.json"
        fake_app = env.root / "fake_app.sh"
        _write_env_dumper_script(fake_app, dump_path)
        r_launch = run_cli(
            [str(binary), "launch", "--port", str(port), "--config", str(cfg), "--app", str(fake_app)],
            env=env.build_env(
                extra={
                    "GEMINI_API_KEY": "real-user-gemini-key",
                    "GOOGLE_API_KEY": "real-user-google-key",
                }
            ),
        )
        self.assertEqual(r_launch.returncode, 0, f"{r_launch.stdout}\n{r_launch.stderr}")
        app_env = _wait_for_json_file(dump_path, timeout=4.0)
        self.assertEqual(app_env.get("CLOUD_CODE_URL"), f"http://127.0.0.1:{port}")
        self.assertEqual(app_env.get("GOOGLE_GEMINI_BASE_URL"), f"http://127.0.0.1:{port}")
        self.assertEqual(app_env.get("GEMINI_API_KEY"), "real-user-gemini-key")
        self.assertEqual(app_env.get("GOOGLE_API_KEY"), "real-user-google-key")
        self.assertTrue(app_env.get("JETSKI_MCP_ENV_RESTORE_PREFIX"))
        self.assertIn("CLOUD_CODE_URL", app_env.get("JETSKI_MCP_ENV_RESTORE_VARS", ""))


class TestCujDaemonAndCloudCode(E2ETestCase):
    SUITE = "cuj"
    FEATURE = "DAEMON_CLOUDCODE"

    def test_cuj_daemon_adopt_running_sigterm_and_idle_sse_guard(self) -> None:
        """Active SSE guards --idle-shutdown-secs; SIGTERM triggers clean exit & --adopt-running takeover."""
        if os.name == "nt":
            self.skipTest("POSIX SIGTERM / adopt-running test")
        binary = self.require_binary()
        env = self.make_env()
        mock = self.start_mock_gemini()
        port = find_free_port()
        cfg = env.write_config(
            env.make_config_dict(listen_port=port, base_url=mock.base_url, api_key="AIzaSyDaemonKey1234")
        )

        primary = self.start_proxy(
            env,
            config_path=cfg,
            port=port,
            extra_args=("--idle-shutdown-secs", "2"),
        )
        pid_file = env.data_dir / "proxy.pid"
        log_file = env.data_dir / "proxy.log"
        self.assertTrue(pid_file.is_file())
        self.assertEqual(stat.S_IMODE(os.lstat(pid_file).st_mode), 0o600)
        self.assertEqual(json.loads(pid_file.read_text(encoding="utf-8"))["pid"], primary.pid)
        if log_file.exists():
            self.assertEqual(stat.S_IMODE(os.lstat(log_file).st_mode), 0o600)

        # Park secondary --adopt-running on the same ephemeral port.
        secondary = CompanionProcess(
            binary=binary,
            env=env.build_env(),
            port=port,
            config_path=cfg,
            extra_args=("--adopt-running",),
        )
        self.stack.callback(secondary.stop)
        secondary.start(wait_for_ready=False)

        # Stream an SSE response spanning > 2.0s (idle timeout) to verify active-SSE guard.
        sse_chunks = [
            build_sse_events([{"candidates": [{"content": {"parts": [{"text": f"c{i}"}]}}]}])
            for i in range(4)
        ]
        mock.enqueue(SseStreamAction(chunks=sse_chunks, chunk_delay_s=0.75))
        with http_stream_request(
            "127.0.0.1",
            port,
            "POST",
            "/v1beta/models/gemini-4-hydrogen:streamGenerateContent?alt=sse",
            headers={"Content-Type": "application/json"},
            body=b'{"contents":[]}',
            timeout=10.0,
        ) as stream:
            self.assertEqual(stream.status, 200)
            body = b"".join(chunk for chunk, _ in stream.iter_chunks())
        self.assertEqual(body, b"".join(sse_chunks))
        self.assertIsNone(primary.returncode, "Primary must stay alive through active SSE")

        # SIGTERM primary -> clean exit 0; secondary adopts port promptly.
        assert primary.proc is not None
        primary.proc.send_signal(signal.SIGTERM)
        rc1 = primary.proc.wait(timeout=3.0)
        self.assertEqual(rc1, 0, f"Primary SIGTERM rc={rc1}\n{primary.stderr_text}")

        secondary.wait_ready(timeout=4.0)
        deadline = time.monotonic() + 2.5
        adopted_pid = None
        while time.monotonic() < deadline:
            if pid_file.is_file():
                try:
                    adopted_pid = json.loads(pid_file.read_text(encoding="utf-8")).get("pid")
                    if adopted_pid == secondary.pid:
                        break
                except (OSError, json.JSONDecodeError):
                    pass
            time.sleep(0.02)
        self.assertEqual(adopted_pid, secondary.pid)

        assert secondary.proc is not None
        secondary.proc.send_signal(signal.SIGTERM)
        rc2 = secondary.proc.wait(timeout=3.0)
        self.assertEqual(rc2, 0, f"Secondary SIGTERM rc={rc2}\n{secondary.stderr_text}")
        self.assertFalse(pid_file.exists(), "proxy.pid must be cleaned up on graceful SIGTERM")

    def test_cuj_cloudcode_day0_to_day1_catalog_and_403_lru(self) -> None:
        """CloudCode preserves Day-0 catalog, caches Enterprise 403 x-goog-user-project strip per-account, and merges Day-1."""
        env = self.make_env()
        mock_ccpa = self.start_mock_gemini()
        raw_upstream = json.dumps(_sample_ccpa_catalog(), separators=(",", ":")).encode("utf-8")

        cfg = env.write_config(
            env.make_config_dict(
                api_key=None,
                cloudcode_upstream_url=mock_ccpa.base_url,
            )
        )
        proxy = self.start_proxy(env, config_path=cfg)

        # (a) Day-0 untouched catalog & upstream Content-Encoding header preserved.
        mock_ccpa.enqueue(
            RawBytesAction(
                status=200,
                content_type="application/json; charset=utf-8",
                body=raw_upstream,
                headers={"Content-Encoding": "identity"},
            )
        )
        r_day0 = http_raw_request(
            "127.0.0.1",
            proxy.port,
            "POST",
            "/v1internal:fetchAvailableModels",
            headers={"Authorization": "Bearer acct-day0", "Content-Type": "application/json"},
            body=b"{}",
        )
        self.assertEqual(r_day0.status, 200, r_day0.text)
        self.assertEqual(r_day0.body, raw_upstream)
        self.assertEqual(r_day0.header("content-encoding"), "identity")
        self.assertNotIn("companion/", r_day0.text)
        st0 = http_raw_request("127.0.0.1", proxy.port, "GET", "/api/stats").json()
        self.assertIs(st0["cloudcode"]["last_catalog_included_proxy_models"], False)
        if os.name != "nt":
            self.assertGreater(st0.get("proxy_rss_bytes") or 0, 0)

        # (b) Enterprise 403 on Account A strips x-goog-user-project once & caches in LRU; Account B keeps header.
        mock_ccpa.clear_requests()

        def _ccpa_handler(req: CapturedRequest) -> JsonAction | RawBytesAction:
            if req.path == "/v1internal:streamGenerateContent":
                return JsonAction(status=403, payload={"error": {"code": 403, "message": "forbidden"}})
            if req.header("x-goog-user-project") is not None and req.header("authorization") == "Bearer acct-A":
                return JsonAction(status=403, payload={"error": {"code": 403, "message": "enterprise project"}})
            return RawBytesAction(
                status=200,
                content_type="application/json; charset=utf-8",
                body=raw_upstream,
                headers={"Content-Encoding": "identity"},
            )

        mock_ccpa.set_handler(_ccpa_handler)
        r_a1 = http_raw_request(
            "127.0.0.1",
            proxy.port,
            "POST",
            "/v1internal:fetchAvailableModels",
            headers={
                "Authorization": "Bearer acct-A",
                "x-goog-user-project": "ent-proj-1",
                "Content-Type": "application/json",
            },
            body=b"{}",
        )
        self.assertEqual(r_a1.status, 200, r_a1.text)
        self.assertEqual(len(mock_ccpa.requests), 2)
        self.assertEqual(mock_ccpa.requests[0].header("x-goog-user-project"), "ent-proj-1")
        self.assertIsNone(mock_ccpa.requests[1].header("x-goog-user-project"))

        mock_ccpa.clear_requests()
        r_b = http_raw_request(
            "127.0.0.1",
            proxy.port,
            "POST",
            "/v1internal:fetchAvailableModels",
            headers={
                "Authorization": "Bearer acct-B",
                "x-goog-user-project": "ent-proj-2",
                "Content-Type": "application/json",
            },
            body=b"{}",
        )
        self.assertEqual(r_b.status, 200, r_b.text)
        self.assertEqual(len(mock_ccpa.requests), 1)
        self.assertEqual(mock_ccpa.requests[0].header("x-goog-user-project"), "ent-proj-2")

        # (c) Hot-reload adds real Gemini key -> Day-1 merged catalog + Account A LRU preserved + stream 403 never retries.
        env.write_config(
            env.make_config_dict(
                api_key="AIzaSyCloudCodeDay1Key",
                cloudcode_upstream_url=mock_ccpa.base_url,
            ),
            path=cfg,
        )
        env.bump_mtime(cfg, delta=3.0)

        mock_ccpa.clear_requests()
        r_day1 = http_raw_request(
            "127.0.0.1",
            proxy.port,
            "POST",
            "/v1internal:fetchAvailableModels",
            headers={
                "Authorization": "Bearer acct-A",
                "x-goog-user-project": "ent-proj-1",
                "Content-Type": "application/json",
            },
            body=b"{}",
        )
        self.assertEqual(r_day1.status, 200, r_day1.text)
        # Account A LRU entry preserved across hot-reload: 1 upstream hit without x-goog-user-project!
        self.assertEqual(len(mock_ccpa.requests), 1)
        self.assertIsNone(mock_ccpa.requests[0].header("x-goog-user-project"))
        self.assertIsNone(r_day1.header("content-encoding"))

        models = r_day1.json()["models"]
        self.assertIn("companion/gemini-4-neon-xhigh", models)
        self.assertEqual(models["companion/gemini-4-neon-xhigh"]["thinkingLevel"], 7)
        companion_slots = []
        for mid, entry in models.items():
            if mid.startswith("companion/"):
                m = _PLACEHOLDER_SLOT_RE.match(entry["model"])
                self.assertIsNotNone(m, f"Bad slot for {mid}: {entry}")
                assert m is not None
                slot_num = int(m.group(1))
                self.assertGreaterEqual(slot_num, 334)
                self.assertLessEqual(slot_num, 365)
                self.assertNotEqual(slot_num, 334, "Must avoid upstream-occupied M334")
                companion_slots.append(slot_num)
        self.assertTrue(companion_slots)

        st1 = http_raw_request("127.0.0.1", proxy.port, "GET", "/api/stats").json()
        self.assertIs(st1["cloudcode"]["last_catalog_included_proxy_models"], True)

        # Non-read-only RPC /v1internal:streamGenerateContent must NEVER retry on 403.
        mock_ccpa.clear_requests()
        r_stream403 = http_raw_request(
            "127.0.0.1",
            proxy.port,
            "POST",
            "/v1internal:streamGenerateContent",
            headers={
                "Authorization": "Bearer acct-B",
                "x-goog-user-project": "ent-proj-2",
                "Content-Type": "application/json",
            },
            body=b"{}",
        )
        self.assertEqual(r_stream403.status, 403)
        self.assertEqual(len(mock_ccpa.requests), 1)


class TestCujInstallUninstallLifecycle(E2ETestCase):
    SUITE = "cuj"
    FEATURE = "INSTALL"

    def test_cuj_install_upgrade_and_default_uninstall_preserving_data(self) -> None:
        """install deploys binary/7-file plugin/env.sh/RC/proxy; upgrade is idempotent; default uninstall keeps config & usage."""
        binary = self.require_binary()
        env = self.make_env()
        self.register_pidfile_cleanup(env=env)
        port = find_free_port()
        cfg = env.write_config(env.make_config_dict(listen_port=port, api_key="AIzaSyInstallKeep1234"))
        usage_canary = env.usage_dir / "usage.db"
        usage_canary.write_bytes(b"SQLite format 3\x00")
        gemini_cfg = env.home_dir / ".gemini" / "config" / "config.json"
        gemini_cfg.parent.mkdir(parents=True, exist_ok=True)
        gemini_cfg.write_text('{"keep":"user"}\n', encoding="utf-8")

        plugins_dir = env.root / "plugins"
        proc_env = env.build_env(extra={"SHELL": "/bin/zsh", "ZDOTDIR": str(env.home_dir)})

        # (a) Fresh install.
        inst = run_cli([str(binary), "install", "--plugins-dir", str(plugins_dir), "--json"], env=proc_env)
        self.assertEqual(inst.returncode, 0, f"{inst.stdout}\n{inst.stderr}")
        self.assertIs(inst.json_stdout().get("ok"), True)

        stable_bin = env.data_dir / "bin" / ("agy-companion.exe" if os.name == "nt" else "agy-companion")
        self.assertTrue(stable_bin.is_file() and os.access(stable_bin, os.X_OK))
        self.assertTrue((env.data_dir / "bin" / ".version").is_file())
        installed = plugins_dir / "agy-companion"
        installed_files = [p for p in installed.rglob("*") if p.is_file() and not p.is_symlink()]
        self.assertEqual(len(installed_files), 7, f"Expected 7 regular plugin files, got {installed_files}")
        for rel in ("plugin.json", "mcp_config.json", "sidecars/proxy/sidecar.json"):
            self.assertIsInstance(json.loads((installed / rel).read_text(encoding="utf-8")), dict)
        if os.name != "nt":
            self.assertEqual(stat.S_IMODE(os.lstat(env.data_dir / "env.sh").st_mode), 0o600)
            self.assertTrue((env.home_dir / ".zlogin").is_file())
        leftovers = [p.name for p in plugins_dir.iterdir() if ".stage." in p.name or ".backup" in p.name]
        self.assertEqual(leftovers, [])
        self.assertIsNotNone(_wait_for_health(port, timeout=4.0))

        # (b) Idempotent upgrade.
        upg = run_cli(
            [str(binary), "install", "--plugins-dir", str(plugins_dir), "--no-launcher-hook", "--no-start-proxy", "--json"],
            env=proc_env,
        )
        self.assertEqual(upg.returncode, 0, f"{upg.stdout}\n{upg.stderr}")
        self.assertIs(upg.json_stdout().get("ok"), True)
        self.assertEqual(
            [p.name for p in plugins_dir.iterdir() if ".stage." in p.name or ".backup" in p.name],
            [],
        )

        # (c) Default uninstall removes artifacts & stops proxy while preserving config.json, usage.db & Antigravity config.
        un = run_cli([str(binary), "uninstall", "--plugins-dir", str(plugins_dir), "--json"], env=proc_env)
        self.assertEqual(un.returncode, 0, f"{un.stdout}\n{un.stderr}")
        self.assertFalse(installed.exists())
        self.assertFalse((env.data_dir / "env.sh").exists())
        self.assertFalse((env.home_dir / ".zlogin").exists())
        self.assertFalse(stable_bin.exists())
        self.assertFalse((env.data_dir / "bin" / ".version").exists())
        self.assertFalse((env.data_dir / "proxy.pid").exists())

        deadline = time.monotonic() + 2.0
        while time.monotonic() < deadline:
            try:
                http_raw_request("127.0.0.1", port, "GET", "/health", timeout=0.3)
            except OSError:
                break
            time.sleep(0.02)
        else:
            self.fail(f"Proxy on port {port} still responded to /health after uninstall")

        self.assertTrue(cfg.is_file())
        self.assertTrue(usage_canary.is_file())
        self.assertEqual(gemini_cfg.read_text(encoding="utf-8"), '{"keep":"user"}\n')

    def test_cuj_uninstall_purge_and_dry_run_safety(self) -> None:
        """uninstall --dry-run --purge deletes nothing; --purge removes config & usage while preserving Antigravity config."""
        binary = self.require_binary()
        env = self.make_env()
        self.register_pidfile_cleanup(env=env)
        port = find_free_port()
        cfg = env.write_config(env.make_config_dict(listen_port=port, api_key="AIzaSyPurgeKey1234"))
        usage_db = env.usage_dir / "usage.db"
        usage_wal = env.usage_dir / "usage.db-wal"
        usage_db.write_bytes(b"SQLite format 3\x00")
        usage_wal.write_bytes(b"wal-canary")
        gemini_cfg = env.home_dir / ".gemini" / "config" / "config.json"
        gemini_cfg.parent.mkdir(parents=True, exist_ok=True)
        gemini_cfg.write_text('{"keep":"antigravity"}\n', encoding="utf-8")

        plugins_dir = env.root / "plugins"
        proc_env = env.build_env(extra={"SHELL": "/bin/zsh", "ZDOTDIR": str(env.home_dir)})
        inst = run_cli(
            [str(binary), "install", "--plugins-dir", str(plugins_dir), "--no-start-proxy", "--json"],
            env=proc_env,
        )
        self.assertEqual(inst.returncode, 0, inst.stderr)

        dry = run_cli(
            [str(binary), "uninstall", "--plugins-dir", str(plugins_dir), "--dry-run", "--purge", "--json"],
            env=proc_env,
        )
        self.assertEqual(dry.returncode, 0, dry.stderr)
        dry_json = dry.json_stdout()
        self.assertIs(dry_json["dry_run"], True)
        self.assertIs(dry_json["purge"], True)
        self.assertTrue(cfg.is_file())
        self.assertTrue(usage_db.is_file())
        self.assertTrue((plugins_dir / "agy-companion").is_dir())

        purge = run_cli(
            [str(binary), "uninstall", "--plugins-dir", str(plugins_dir), "--purge", "--json"],
            env=proc_env,
        )
        self.assertEqual(purge.returncode, 0, purge.stderr)
        self.assertIs(purge.json_stdout()["purge"], True)
        self.assertFalse(cfg.exists())
        self.assertFalse(usage_db.exists())
        self.assertFalse(usage_wal.exists())
        self.assertFalse((plugins_dir / "agy-companion").exists())
        self.assertEqual(gemini_cfg.read_text(encoding="utf-8"), '{"keep":"antigravity"}\n')
