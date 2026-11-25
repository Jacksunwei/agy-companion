"""Tier 4: Multi-Step Real-World End-to-End Application Scenarios (7 tests).

Exercises complete user lifecycles across F1–F12 and all Security Invariants S1–S8.
"""

from __future__ import annotations

import concurrent.futures
import importlib.util
import json
import os
import re
import shutil
import stat
import tempfile
import tomllib
from pathlib import Path
from typing import Any

from harness import (
    E2ETestCase,
    IsolatedEnv,
    extract_models_list,
    find_free_port,
    find_key_recursive,
    http_raw_request,
    poll_stats,
    poll_usage_events,
    read_usage_raw_bytes,
    run_cli,
    usage_storage_files,
)
from mcp_client import McpStdioClient
from mock_gemini import JsonAction, MockGeminiServer, SseStreamAction, build_sse_events


class TestTier4Scenarios(E2ETestCase):
    TIER = 4
    FEATURE = "SCENARIO"

    def test_t4_01_full_onboarding_and_cached_conversation_lifecycle(self) -> None:
        """Brand-new user onboarding -> 3-turn cached SSE chat -> stats, MCP, doctor, and 0600 SQLite audit."""
        binary = self.require_binary()
        env = self.make_env()
        api_key = "AIzaSyOnboardingUser1234"
        mock = self.start_mock_gemini(expected_api_key=api_key)
        port = find_free_port()
        cfg = env.root / "onboard" / "config.json"

        # 1. Spawn ui-mcp, open_settings, save_settings, test_connection.
        mcp = self.start_mcp(env, config_path=cfg)
        open_res = mcp.call_tool("open_settings", {})
        self.assertFalse(open_res.get("isError", False))

        save_kwargs: dict[str, Any] = {
            "base_url": mock.base_url,
            "api_key": api_key,
            "listen_port": port,
            "default_model": "gemini-4-hydrogen",
        }
        if "usage_dir" in mcp.get_save_settings_schema():
            save_kwargs["usage_dir"] = str(env.usage_dir)
        mcp.save_settings_smart(**save_kwargs)
        # Ensure usage_dir is persisted even if save_settings schema does not expose usage_dir.
        saved = json.loads(cfg.read_text(encoding="utf-8"))
        if saved.get("usage_dir") != str(env.usage_dir):
            saved["usage_dir"] = str(env.usage_dir)
            env.write_config(saved, mode=0o600, path=cfg)

        self.assertEqual(stat.S_IMODE(os.stat(cfg).st_mode), 0o600)
        conn_res = mcp.call_tool("test_connection", {})
        found_ok, ok_val = find_key_recursive(conn_res.get("structuredContent", {}), "ok")
        self.assertTrue(found_ok and ok_val is True, f"test_connection failed: {conn_res!r}")

        # 2. Launch serve and simulate 3-turn SSE conversation with growing implicit cache.
        proxy = self.start_proxy(env, config_path=cfg, port=port)
        turns = [
            (1000, 0, 120, 30, "TURN1_PROMPT_SECRET", "TURN1_RESP_SECRET"),
            (1200, 800, 150, 40, "TURN2_PROMPT_SECRET", "TURN2_RESP_SECRET"),
            (2000, 1600, 200, 50, "TURN3_PROMPT_SECRET", "TURN3_RESP_SECRET"),
        ]
        for prompt_tok, cached_tok, out_tok, think_tok, _, resp_txt in turns:
            c1 = build_sse_events([{"candidates": [{"content": {"parts": [{"text": resp_txt}]}}]}])
            c2 = build_sse_events(
                [
                    {
                        "candidates": [{"content": {"parts": [{"text": " done"}]}}],
                        "usageMetadata": {
                            "promptTokenCount": prompt_tok,
                            "cachedContentTokenCount": cached_tok,
                            "candidatesTokenCount": out_tok,
                            "thoughtsTokenCount": think_tok,
                        },
                    }
                ]
            )
            mock.enqueue(SseStreamAction(chunks=[c1, c2]))

        for _, _, _, _, prompt_txt, resp_txt in turns:
            r = http_raw_request(
                "127.0.0.1",
                proxy.port,
                "POST",
                "/v1beta/models/gemini-4-hydrogen:streamGenerateContent?alt=sse",
                body=json.dumps({"contents": [{"parts": [{"text": prompt_txt}]}]}).encode("utf-8"),
                headers={"Content-Type": "application/json"},
            )
            self.assertEqual(r.status, 200)
            self.assertIn(resp_txt, r.text)

        # 3. Verify /api/stats totals and exact cache_hit_ratio = 2400 / 4200 (after SSE EOF flush, Guard A13).
        poll_usage_events(env.usage_dir, min_count=3)
        stats = poll_stats(proxy.port, min_requests=3)
        usage = stats["usage"]
        self.assertEqual(usage["requests"], 3)
        self.assertEqual(usage["prompt_tokens"], 4200)
        self.assertEqual(usage["cached_tokens"], 2400)
        self.assertEqual(usage["output_tokens"], 470)
        self.assertEqual(usage["reasoning_tokens"], 120)
        self.assertAlmostEqual(usage["cache_hit_ratio"], 2400 / 4200, places=6)

        # 4. Verify ui-mcp get_settings reports masked hint & live stats.
        get_res = mcp.call_tool("get_settings", {})
        mcp.assert_clean_stdout()
        raw_get = json.dumps(get_res)
        self.assertNotIn(api_key, raw_get)
        found_h, hint = find_key_recursive(get_res.get("structuredContent", {}), "api_key_hint")
        self.assertTrue(found_h and hint == "AIza...1234")
        found_reqs, reqs = find_key_recursive(get_res.get("structuredContent", {}), "requests")
        self.assertTrue(found_reqs and reqs == 3)

        # 5. Verify doctor exits 0.
        doc_env = env.build_env(
            extra={
                "GEMINI_API_KEY": api_key,
                "GOOGLE_GEMINI_BASE_URL": f"http://127.0.0.1:{proxy.port}",
            }
        )
        r_doc = run_cli([str(binary), "doctor", "--config", str(cfg), "--port", str(proxy.port)], env=doc_env)
        self.assertEqual(r_doc.returncode, 0, f"{r_doc.stdout}\n{r_doc.stderr}")

        # 6. Audit SQLite DB & sidecar perms (0600) and zero secret/body leaks.
        files, events = poll_usage_events(env.usage_dir, min_count=3)
        self.assertEqual(len(files), 1)
        self.assertEqual(len(events), 3)
        for sf in usage_storage_files(env.usage_dir):
            self.assertEqual(stat.S_IMODE(os.stat(sf).st_mode), 0o600)
        raw_sqlite = read_usage_raw_bytes(env.usage_dir)
        self.assertGreater(len(raw_sqlite), 0)
        self.assertNotIn(api_key.encode("utf-8"), raw_sqlite)
        for _, _, _, _, prompt_txt, resp_txt in turns:
            self.assertNotIn(prompt_txt.encode("utf-8"), raw_sqlite)
            self.assertNotIn(resp_txt.encode("utf-8"), raw_sqlite)

    def test_t4_02_live_api_key_rotation_and_revocation_without_restart(self) -> None:
        """Zero-downtime key rotation, preservation across unrelated saves, and clean revocation."""
        env = self.make_env()
        mock = self.start_mock_gemini()
        key1 = "AIzaSyRotateOldKey1111"
        key2 = "AIzaSyRotateNewKey2222"
        port = find_free_port()
        cfg = env.write_config(
            env.make_config_dict(listen_port=port, base_url=mock.base_url, api_key=key1),
            mtime_offset_secs=-5.0,
        )
        proxy = self.start_proxy(env, config_path=cfg, port=port)
        mcp = self.start_mcp(env, config_path=cfg)

        # 1. Traffic uses KEY_1.
        r1 = http_raw_request(
            "127.0.0.1",
            proxy.port,
            "POST",
            "/v1beta/models/gemini-4-hydrogen:generateContent",
            body=b'{"contents":[]}',
            headers={"Content-Type": "application/json"},
        )
        self.assertEqual(r1.status, 200)
        self.assertEqual(mock.requests[0].header("x-goog-api-key"), key1)

        # 2. Rotate key to KEY_2 via save_settings without restarting serve.
        mcp.save_settings_smart(api_key=key2)
        IsolatedEnv.bump_mtime(cfg, delta=2.0)
        self.assertEqual(stat.S_IMODE(os.stat(cfg).st_mode), 0o600)

        r2 = http_raw_request(
            "127.0.0.1",
            proxy.port,
            "POST",
            "/v1beta/models/gemini-4-hydrogen:generateContent",
            body=b'{"contents":[]}',
            headers={"Content-Type": "application/json"},
        )
        self.assertEqual(r2.status, 200)
        self.assertEqual(mock.requests[1].header("x-goog-api-key"), key2)
        g2 = mcp.call_tool("get_settings", {})
        found_h, hint = find_key_recursive(g2.get("structuredContent", {}), "api_key_hint")
        self.assertTrue(found_h and hint == "AIza...2222")

        # 3. Unrelated save preserves KEY_2.
        mcp.save_settings_smart(default_model="gemini-3.8-flash")
        IsolatedEnv.bump_mtime(cfg, delta=2.0)
        r3 = http_raw_request(
            "127.0.0.1",
            proxy.port,
            "POST",
            "/v1beta/models/gemini-4-hydrogen:generateContent",
            body=b'{"contents":[]}',
            headers={"Content-Type": "application/json"},
        )
        self.assertEqual(r3.status, 200)
        self.assertEqual(mock.requests[2].header("x-goog-api-key"), key2)

        # 4. Revoke key via clear_api_key=True.
        mcp.save_settings_smart(clear_api_key=True)
        IsolatedEnv.bump_mtime(cfg, delta=2.0)
        self.assertEqual(stat.S_IMODE(os.stat(cfg).st_mode), 0o600)
        g4 = mcp.call_tool("get_settings", {})
        mcp.assert_clean_stdout()
        found_c, configured = find_key_recursive(g4.get("structuredContent", {}), "api_key_configured")
        self.assertTrue(found_c and configured is False)

        r4 = http_raw_request(
            "127.0.0.1",
            proxy.port,
            "POST",
            "/v1beta/models/gemini-4-hydrogen:generateContent",
            body=b'{"contents":[]}',
            headers={"Content-Type": "application/json"},
        )
        self.assertGreaterEqual(r4.status, 400)
        self.assertNotIn(key1, r4.text)
        self.assertNotIn(key2, r4.text)
        self.assertNotIn(key1, mcp.stdout_text + mcp.stderr_text + proxy.stderr_text)
        self.assertNotIn(key2, mcp.stdout_text + mcp.stderr_text + proxy.stderr_text)

    def test_t4_03_upstream_outage_rate_limit_and_automatic_recovery(self) -> None:
        """Proxy survives 429, 500, and complete upstream outage, then recovers immediately when upstream returns."""
        env = self.make_env()
        mock_port = find_free_port()
        mock1 = MockGeminiServer(port=mock_port)
        mock1.start()
        self.stack.callback(mock1.stop)

        cfg = env.write_config(
            env.make_config_dict(base_url=mock1.base_url, api_key="AIzaSyOutageRecov1234")
        )
        proxy = self.start_proxy(env, config_path=cfg)

        # 1. Healthy baseline.
        r_ok1 = http_raw_request(
            "127.0.0.1",
            proxy.port,
            "POST",
            "/v1beta/models/gemini-4-hydrogen:generateContent",
            body=b'{"contents":[]}',
            headers={"Content-Type": "application/json"},
        )
        self.assertEqual(r_ok1.status, 200)

        # 2. Rate-limit 429 and upstream 500.
        mock1.enqueue(JsonAction(status=429, payload={"error": {"code": 429, "status": "RESOURCE_EXHAUSTED"}}))
        mock1.enqueue(JsonAction(status=500, payload={"error": {"code": 500, "status": "INTERNAL"}}))
        r_429 = http_raw_request(
            "127.0.0.1",
            proxy.port,
            "POST",
            "/v1beta/models/gemini-4-hydrogen:generateContent",
            body=b'{"contents":[]}',
            headers={"Content-Type": "application/json"},
        )
        self.assertEqual(r_429.status, 429)
        r_500 = http_raw_request(
            "127.0.0.1",
            proxy.port,
            "POST",
            "/v1beta/models/gemini-4-hydrogen:generateContent",
            body=b'{"contents":[]}',
            headers={"Content-Type": "application/json"},
        )
        self.assertEqual(r_500.status, 500)

        # 3. Stop mock entirely (TCP connection refused) while /health stays 200.
        mock1.stop()
        self.assertEqual(http_raw_request("127.0.0.1", proxy.port, "GET", "/health").status, 200)
        r_down = http_raw_request(
            "127.0.0.1",
            proxy.port,
            "POST",
            "/v1beta/models/gemini-4-hydrogen:generateContent",
            body=b'{"contents":[]}',
            headers={"Content-Type": "application/json"},
        )
        self.assertIn(r_down.status, (502, 503, 504))
        self.assertEqual(http_raw_request("127.0.0.1", proxy.port, "GET", "/health").status, 200)

        # 4. Restart mock on the exact same port -> immediate recovery.
        mock2 = MockGeminiServer(
            port=mock_port,
            default_usage={"promptTokenCount": 300, "candidatesTokenCount": 70, "cachedContentTokenCount": 150, "thoughtsTokenCount": 10},
        )
        mock2.start()
        self.stack.callback(mock2.stop)

        r_rec = http_raw_request(
            "127.0.0.1",
            proxy.port,
            "POST",
            "/v1beta/models/gemini-4-hydrogen:generateContent",
            body=b'{"contents":[]}',
            headers={"Content-Type": "application/json"},
        )
        self.assertEqual(r_rec.status, 200)
        stats = http_raw_request("127.0.0.1", proxy.port, "GET", "/api/stats").json()
        self.assertGreaterEqual(stats["usage"]["requests"], 2)
        _, events = poll_usage_events(env.usage_dir, min_count=2)
        self.assertGreaterEqual(len(events), 2)

    def test_t4_04_concurrent_mixed_streaming_and_unary_workload(self) -> None:
        """8 concurrent workers (4 slow SSE + 4 fast unary, 16 total) preserve byte-for-byte isolation and exact totals."""
        env = self.make_env()
        mock = self.start_mock_gemini()

        def dynamic_handler(req: Any) -> Any:
            body_obj = json.loads(req.body.decode("utf-8"))
            tag = body_obj["tag"]
            if ":streamGenerateContent" in req.path:
                chunks = [
                    build_sse_events([{"candidates": [{"content": {"parts": [{"text": f"{tag}-chunk-{i}"}]}}]}])
                    for i in range(4)
                ]
                chunks.append(
                    build_sse_events(
                        [
                            {
                                "candidates": [{"content": {"parts": [{"text": f"{tag}-final"}]}}],
                                "usageMetadata": {
                                    "promptTokenCount": 200,
                                    "cachedContentTokenCount": 100,
                                    "candidatesTokenCount": 50,
                                    "thoughtsTokenCount": 10,
                                },
                            }
                        ]
                    )
                )
                return SseStreamAction(chunks=chunks, chunk_delay_s=0.02)
            return JsonAction(
                status=200,
                payload={
                    "tag": tag,
                    "candidates": [{"content": {"parts": [{"text": f"unary-{tag}"}]}}],
                    "usageMetadata": {
                        "promptTokenCount": 100,
                        "cachedContentTokenCount": 50,
                        "candidatesTokenCount": 25,
                        "thoughtsTokenCount": 5,
                    },
                },
            )

        mock.set_handler(dynamic_handler)
        cfg = env.write_config(env.make_config_dict(base_url=mock.base_url, api_key="AIzaSyConcurrentMix12"))
        proxy = self.start_proxy(env, config_path=cfg)

        def run_sse_worker(worker_id: int) -> None:
            for turn in range(2):
                tag = f"sse-w{worker_id}-t{turn}"
                r = http_raw_request(
                    "127.0.0.1",
                    proxy.port,
                    "POST",
                    "/v1beta/models/gemini-4-hydrogen:streamGenerateContent?alt=sse",
                    body=json.dumps({"tag": tag, "contents": []}).encode("utf-8"),
                    headers={"Content-Type": "application/json"},
                )
                assert r.status == 200, f"SSE {tag} failed: {r.status}"
                for i in range(4):
                    assert f"{tag}-chunk-{i}" in r.text, f"Missing chunk {i} for {tag}"
                assert f"{tag}-final" in r.text, f"Missing final for {tag}"

        def run_unary_worker(worker_id: int) -> None:
            for turn in range(2):
                tag = f"unary-w{worker_id}-t{turn}"
                r = http_raw_request(
                    "127.0.0.1",
                    proxy.port,
                    "POST",
                    "/v1beta/models/gemini-4-hydrogen:generateContent",
                    body=json.dumps({"tag": tag, "contents": []}).encode("utf-8"),
                    headers={"Content-Type": "application/json"},
                )
                assert r.status == 200, f"Unary {tag} failed: {r.status}"
                assert r.json()["tag"] == tag

        with concurrent.futures.ThreadPoolExecutor(max_workers=8) as pool:
            futs = [pool.submit(run_sse_worker, i) for i in range(4)] + [
                pool.submit(run_unary_worker, i) for i in range(4)
            ]
            for fut in futs:
                fut.result()

        # 8 SSE requests * (200, 100, 50, 10) + 8 unary requests * (100, 50, 25, 5)
        expected_prompt = 8 * 200 + 8 * 100
        expected_cached = 8 * 100 + 8 * 50
        expected_out = 8 * 50 + 8 * 25
        expected_think = 8 * 10 + 8 * 5

        files, events = poll_usage_events(env.usage_dir, min_count=16, timeout=4.0)
        self.assertEqual(len(events), 16)
        self.assertEqual(len(files), 1)

        stats = poll_stats(proxy.port, min_requests=16, timeout=4.0)
        usage = stats["usage"]
        self.assertEqual(usage["requests"], 16)
        self.assertEqual(usage["prompt_tokens"], expected_prompt)
        self.assertEqual(usage["cached_tokens"], expected_cached)
        self.assertEqual(usage["output_tokens"], expected_out)
        self.assertEqual(usage["reasoning_tokens"], expected_think)

    def test_t4_05_end_to_end_security_and_privacy_audit_s1_to_s8(self) -> None:
        """Full-stack S1–S8 security & canary privacy audit across HTTP, MCP, CLI, SQLite, UI, plugin, and npm."""
        binary = self.require_binary()
        env = self.make_env()
        canary_key = "AIzaSyCANARY_PRIVACY_987654"
        canary_prompt = "CANARY_SECRET_PROMPT_ALPHA"
        canary_resp = "CANARY_SECRET_RESP_OMEGA"

        # S1 + S5: reject 0.0.0.0 and remote http:// on serve startup.
        bad_host_cfg = env.write_config(
            env.make_config_dict(listen_host="0.0.0.0", listen_port=find_free_port()),
            path=env.root / "bad_host.json",
        )
        self.assertNotEqual(run_cli([str(binary), "serve", "--config", str(bad_host_cfg)], env=env.build_env()).returncode, 0)
        bad_url_cfg = env.write_config(
            env.make_config_dict(base_url="http://example.com", listen_port=find_free_port()),
            path=env.root / "bad_url.json",
        )
        self.assertNotEqual(run_cli([str(binary), "serve", "--config", str(bad_url_cfg)], env=env.build_env()).returncode, 0)

        # S2 + S3 + S6 + S7: Live proxy + MCP with canary secrets.
        mock = self.start_mock_gemini(expected_api_key=canary_key)
        mock.enqueue(
            JsonAction(
                status=200,
                payload={
                    "candidates": [{"content": {"parts": [{"text": canary_resp}]}}],
                    "usageMetadata": {"promptTokenCount": 500, "cachedContentTokenCount": 250, "candidatesTokenCount": 80, "thoughtsTokenCount": 20},
                },
            ),
            method="POST",
            path_contains=":generateContent",
        )
        port = find_free_port()
        cfg = env.root / "audit_cfg.json"
        mcp = self.start_mcp(env, config_path=cfg)
        open_res = mcp.call_tool("open_settings", {})
        save_res = mcp.save_settings_smart(base_url=mock.base_url, api_key=canary_key, listen_port=port)
        saved = json.loads(cfg.read_text(encoding="utf-8"))
        saved["usage_dir"] = str(env.usage_dir)
        env.write_config(saved, mode=0o600, path=cfg)
        self.assertEqual(stat.S_IMODE(os.stat(cfg).st_mode), 0o600)

        conn_res = mcp.call_tool("test_connection", {})
        found_ok, ok_val = find_key_recursive(conn_res.get("structuredContent", {}), "ok")
        self.assertTrue(found_ok and ok_val is True, f"test_connection failed in T4-05: {conn_res!r}")
        proxy = self.start_proxy(env, config_path=cfg, port=port)

        # S6: path traversal blocked with 0 upstream hits.
        before_count = len(mock.requests)
        r_trav = http_raw_request("127.0.0.1", proxy.port, "POST", "/v1beta/models/../../etc/passwd:generateContent", body=b"{}")
        self.assertIn(r_trav.status, (400, 404))
        self.assertEqual(len(mock.requests), before_count)

        # S7: DELETE -> 405, security headers on /health & /api/stats.
        self.assertEqual(http_raw_request("127.0.0.1", proxy.port, "DELETE", "/health").status, 405)
        h_resp = http_raw_request("127.0.0.1", proxy.port, "GET", "/health")
        self.assertEqual((h_resp.header("x-content-type-options") or "").lower(), "nosniff")
        self.assertIn("no-store", (h_resp.header("cache-control") or "").lower())

        # Proxy unary + SSE traffic with canary markers.
        r_unary = http_raw_request(
            "127.0.0.1",
            proxy.port,
            "POST",
            "/v1beta/models/gemini-4-hydrogen:generateContent?key=LEAK_ATTEMPT",
            body=json.dumps({"contents": [{"parts": [{"text": canary_prompt}]}]}).encode("utf-8"),
            headers={"Content-Type": "application/json"},
        )
        self.assertEqual(r_unary.status, 200)
        self.assertIn(canary_resp, r_unary.text, "Expected canary response payload to transit proxy")
        s_resp = http_raw_request("127.0.0.1", proxy.port, "GET", "/api/stats")
        self.assertEqual((s_resp.header("x-content-type-options") or "").lower(), "nosniff")
        self.assertIn("no-store", (s_resp.header("cache-control") or "").lower())

        get_res = mcp.call_tool("get_settings", {})
        mcp.assert_clean_stdout()

        doc_env = env.build_env(
            extra={"GEMINI_API_KEY": canary_key, "GOOGLE_GEMINI_BASE_URL": f"http://127.0.0.1:{proxy.port}"}
        )
        r_doc = run_cli([str(binary), "doctor", "--config", str(cfg), "--port", str(proxy.port)], env=doc_env)
        self.assertEqual(r_doc.returncode, 0, f"{r_doc.stdout}\n{r_doc.stderr}")

        files, events = poll_usage_events(env.usage_dir, min_count=1)
        self.assertEqual(len(events), 1)
        self.assertEqual(len(files), 1)
        for sf in usage_storage_files(env.usage_dir):
            self.assertEqual(stat.S_IMODE(os.stat(sf).st_mode), 0o600)
        raw_sqlite = read_usage_raw_bytes(env.usage_dir)
        self.assertGreater(len(raw_sqlite), 0)

        # S2: Zero canary leaks across ALL observable surfaces.
        surfaces = {
            "health": h_resp.text,
            "stats": s_resp.text,
            "open_settings": json.dumps(open_res),
            "save_settings": json.dumps(save_res),
            "get_settings": json.dumps(get_res),
            "test_connection": json.dumps(conn_res),
            "doctor_out": r_doc.stdout + r_doc.stderr,
            "proxy_stderr": proxy.stderr_text,
            "mcp_stderr": mcp.stderr_text,
        }
        for name, text in surfaces.items():
            self.assertNotIn(canary_key, text, f"S2 VIOLATION: canary_key leaked in {name}")
        self.assertNotIn(canary_key.encode("utf-8"), raw_sqlite, "S2 VIOLATION: canary_key leaked in sqlite")
        for req in mock.requests:
            self.assertNotIn(canary_key, req.raw_target)
            self.assertNotIn("LEAK_ATTEMPT", req.raw_target)
        for name in ("proxy_stderr", "mcp_stderr", "stats"):
            self.assertNotIn(canary_prompt, surfaces[name], f"S2 VIOLATION: prompt leaked in {name}")
            self.assertNotIn(canary_resp, surfaces[name], f"S2 VIOLATION: response leaked in {name}")
        self.assertNotIn(canary_prompt.encode("utf-8"), raw_sqlite, "S2 VIOLATION: prompt leaked in sqlite")
        self.assertNotIn(canary_resp.encode("utf-8"), raw_sqlite, "S2 VIOLATION: response leaked in sqlite")

        # S4: UI XSS scan.
        ui_src = self.repo_root / "ui" / "src"
        xss_re = re.compile(r"innerHTML|outerHTML|document\.write|dangerouslySetInnerHTML|\beval\(|localStorage|\balert\(|\bconfirm\(")
        for p in ui_src.rglob("*"):
            if p.is_file():
                self.assertIsNone(xss_re.search(p.read_text(encoding="utf-8")), f"S4 violation in {p}")

        # S6: npm execFileSync + run-companion.sh SHA-256.
        npm_shim = (self.repo_root / "packages" / "npm" / "agy-companion" / "bin" / "agy-companion.js").read_text(encoding="utf-8")
        self.assertIn("execFileSync", npm_shim)
        self.assertNotIn("execSync(", npm_shim)
        launcher = (self.repo_root / "plugin" / "scripts" / "run-companion.sh").read_text(encoding="utf-8")
        self.assertIn("sha256sum", launcher)
        self.assertIn("shasum -a 256", launcher)

        # S8: Strict plugin validate.
        r_plug = run_cli(["agy", "plugin", "validate", "--strict", str(self.repo_root / "plugin")])
        self.assertEqual(r_plug.returncode, 0, f"{r_plug.stdout}\n{r_plug.stderr}")

    def test_t4_06_distribution_plugin_and_packaging_readiness_workflow(self) -> None:
        """Full release-readiness audit across plugin, UI bundle, npm, Python, 4-way sync, and agent_check.sh."""
        binary = self.require_binary()
        env = self.make_env()

        # 1. Strict plugin validation.
        r_plug = run_cli(["agy", "plugin", "validate", "--strict", "--json", str(self.repo_root / "plugin")])
        self.assertEqual(r_plug.returncode, 0, f"{r_plug.stdout}\n{r_plug.stderr}")

        # 2. run-companion.sh --version and models --json.
        script = self.repo_root / "plugin" / "scripts" / "run-companion.sh"
        proc_env = env.build_env(extra={"AGY_COMPANION_BIN": str(binary), "PATH": "/usr/bin:/bin"})
        r_ver = run_cli(["bash", str(script), "--version"], env=proc_env)
        self.assertEqual(r_ver.returncode, 0, r_ver.stderr)
        bin_ver = r_ver.stdout.strip().split()[-1]
        r_mod = run_cli(["bash", str(script), "models", "--json"], env=proc_env)
        self.assertEqual(r_mod.returncode, 0, r_mod.stderr)
        self.assertGreater(len(extract_models_list(r_mod.json_stdout())), 0)

        # 3. ui/dist/index.html size & zero external CDN links.
        dist = self.repo_root / "ui" / "dist" / "index.html"
        self.assertTrue(0 < os.path.getsize(dist) < 500 * 1024)
        html = dist.read_text(encoding="utf-8")
        self.assertIsNone(re.search(r'<script[^>]+src=["\']https?://', html, re.IGNORECASE))

        # 4. npm syntax checks + temp-dir platform package generation.
        gen_mjs = self.repo_root / "packages" / "npm" / "generate-platform-pkgs.mjs"
        bin_js = self.repo_root / "packages" / "npm" / "agy-companion" / "bin" / "agy-companion.js"
        for fpath in (bin_js, gen_mjs):
            self.assertEqual(run_cli(["node", "--check", str(fpath)]).returncode, 0)
        with tempfile.TemporaryDirectory(prefix="agy-release-npm-") as tmpdir:
            base_dir = Path(tmpdir)
            work_dir = base_dir / "work"
            out_dir = base_dir / "pkgs"
            work_dir.mkdir(parents=True, exist_ok=True)
            out_dir.mkdir(parents=True, exist_ok=True)
            r_gen = run_cli(["node", str(gen_mjs), "--out-dir", str(out_dir)], cwd=work_dir)
            manifests = list(out_dir.rglob("package.json"))
            if r_gen.returncode != 0 or not manifests:
                for d in (work_dir, out_dir):
                    for child in list(d.iterdir()):
                        if child.is_dir():
                            shutil.rmtree(child)
                        else:
                            child.unlink()
                r_gen = run_cli(["node", str(gen_mjs), str(out_dir)], cwd=work_dir)
                manifests = list(out_dir.rglob("package.json"))
            self.assertEqual(r_gen.returncode, 0, f"{r_gen.stdout}\n{r_gen.stderr}")
            self.assertGreaterEqual(len(manifests), 5)

        # 5. Python maturin pyproject + find_binary().
        py_data = tomllib.loads((self.repo_root / "packages" / "python" / "pyproject.toml").read_text(encoding="utf-8"))
        self.assertEqual(py_data["tool"]["maturin"]["bindings"], "bin")
        init_py = self.repo_root / "packages" / "python" / "python" / "agy_companion" / "__init__.py"
        spec = importlib.util.spec_from_file_location("_e2e_t4_py_shim", init_py)
        assert spec is not None and spec.loader is not None
        mod = importlib.util.module_from_spec(spec)
        spec.loader.exec_module(mod)
        self.assertTrue(callable(getattr(mod, "find_binary", None)))

        # 6. 4-way version sync vs live binary.
        cargo_data = tomllib.loads((self.repo_root / "Cargo.toml").read_text(encoding="utf-8"))
        v_cargo = cargo_data.get("workspace", {}).get("package", {}).get("version") or cargo_data["package"]["version"]
        v_plugin = json.loads((self.repo_root / "plugin" / "plugin.json").read_text(encoding="utf-8"))["version"]
        v_npm = json.loads(
            (self.repo_root / "packages" / "npm" / "agy-companion" / "package.json").read_text(encoding="utf-8")
        )["version"]
        v_py = py_data["project"]["version"]
        self.assertEqual({bin_ver, v_cargo, v_plugin, v_npm, v_py}, {"0.1.0"})

        # 7. Full scripts/agent_check.sh.
        r_gate = run_cli([str(self.repo_root / "scripts" / "agent_check.sh")], cwd=self.repo_root, timeout=180.0)
        self.assertEqual(r_gate.returncode, 0, f"{r_gate.stdout}\n{r_gate.stderr}")

    def test_t4_07_version_skew_and_degraded_environment_diagnostics(self) -> None:
        """Degraded 0644+stopped -> remediated 0600+skew(0.0.9) -> remediated live 0.1.0 transitions cleanly."""
        binary = self.require_binary()
        env = self.make_env()
        port = find_free_port()
        cfg = env.write_config(
            env.make_config_dict(listen_port=port, api_key="AIzaSyDiagLifecycle12"),
            mode=0o644,
        )
        doc_env = env.build_env(
            extra={
                "GEMINI_API_KEY": "AIzaSyDiagLifecycle12",
                "GOOGLE_GEMINI_BASE_URL": f"http://127.0.0.1:{port}",
            }
        )

        # Stage 1: Degraded (0644 perms + proxy stopped).
        r1 = run_cli([str(binary), "doctor", "--config", str(cfg), "--port", str(port)], env=doc_env)
        self.assertNotEqual(r1.returncode, 0)

        # Stage 2: Fix perms to 0600, but run stale MockHealthServer ("0.0.9").
        os.chmod(cfg, 0o600)
        mock_health = self.start_mock_health(version="0.0.9", protocol_version=1, port=port)
        r2 = run_cli([str(binary), "doctor", "--config", str(cfg), "--port", str(port)], env=doc_env)
        self.assertNotEqual(r2.returncode, 0)

        mcp = self.start_mcp(env, config_path=cfg)
        g_skew = mcp.call_tool("get_settings", {})
        self.assertIn("0.0.9", json.dumps(g_skew))

        # Stage 3: Replace stale mock with real serve matching 0.1.0.
        mock_health.stop()
        proxy = self.start_proxy(env, config_path=cfg, port=port)
        r3 = run_cli([str(binary), "doctor", "--config", str(cfg), "--port", str(proxy.port)], env=doc_env)
        self.assertEqual(r3.returncode, 0, f"doctor should pass after remediation:\n{r3.stdout}\n{r3.stderr}")

        g_ok = mcp.call_tool("get_settings", {})
        mcp.assert_clean_stdout()
        self.assertIn("0.1.0", json.dumps(g_ok))
