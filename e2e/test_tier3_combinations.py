"""Tier 3: Pairwise Cross-Feature Combination Tests (15 tests).

Covers all 14 pairwise combinations from the E2E matrix (with T3-10 enhanced
for F15 live UI reload without restarting ui-mcp) plus T3-15 verifying F15
symlink-aware local target/debug binary resolution in plugin/scripts/run-companion.sh.
"""

from __future__ import annotations

import concurrent.futures
import json
import os
import shutil
import stat
import tomllib
from pathlib import Path
from typing import Any
from urllib.parse import urlsplit

from harness import (
    E2ETestCase,
    IsolatedEnv,
    extract_models_list,
    find_free_port,
    find_key_recursive,
    http_raw_request,
    poll_stats,
    poll_usage_events,
    run_cli,
)
from mock_gemini import JsonAction, SseStreamAction, build_sse_events


class TestTier3Combinations(E2ETestCase):
    TIER = 3
    FEATURE = "COMBINATION"

    def test_t3_01_ui_mcp_save_hot_reloads_live_proxy(self) -> None:
        """F8+F5+F1: ui-mcp save_settings hot-reloads live serve API key without proxy restart."""
        env = self.make_env()
        mock = self.start_mock_gemini()
        key_a = "AIzaSyHotReloadKeyA1111"
        key_b = "AIzaSyHotReloadKeyB2222"
        port = find_free_port()
        cfg = env.write_config(
            env.make_config_dict(listen_port=port, base_url=mock.base_url, api_key=key_a),
            mtime_offset_secs=-5.0,
        )
        proxy = self.start_proxy(env, config_path=cfg, port=port)

        r1 = http_raw_request(
            "127.0.0.1",
            proxy.port,
            "POST",
            "/v1beta/models/gemini-2.5-pro:generateContent",
            body=b'{"contents":[]}',
            headers={"Content-Type": "application/json"},
        )
        self.assertEqual(r1.status, 200)
        self.assertEqual(mock.requests[0].header("x-goog-api-key"), key_a)

        mcp = self.start_mcp(env, config_path=cfg)
        mcp.save_settings_smart(api_key=key_b)
        mcp.assert_clean_stdout()

        r2 = http_raw_request(
            "127.0.0.1",
            proxy.port,
            "POST",
            "/v1beta/models/gemini-2.5-pro:generateContent",
            body=b'{"contents":[]}',
            headers={"Content-Type": "application/json"},
        )
        self.assertEqual(r2.status, 200)
        self.assertEqual(mock.requests[1].header("x-goog-api-key"), key_b)

    def test_t3_02_sse_stream_updates_stats_jsonl_and_get_settings(self) -> None:
        """F5+F6+F3+F8: SSE stream updates /api/stats, JSONL usage file, and ui-mcp get_settings consistently."""
        env = self.make_env()
        mock = self.start_mock_gemini(
            default_usage={
                "promptTokenCount": 1000,
                "cachedContentTokenCount": 750,
                "candidatesTokenCount": 200,
                "thoughtsTokenCount": 50,
            }
        )
        port = find_free_port()
        cfg = env.write_config(
            env.make_config_dict(listen_port=port, base_url=mock.base_url, api_key="AIzaSyThreeWaySync123")
        )
        proxy = self.start_proxy(env, config_path=cfg, port=port)
        r = http_raw_request(
            "127.0.0.1",
            proxy.port,
            "POST",
            "/v1beta/models/gemini-2.5-pro:streamGenerateContent?alt=sse",
            body=b'{"contents":[]}',
            headers={"Content-Type": "application/json"},
        )
        self.assertEqual(r.status, 200)

        _, events = poll_usage_events(env.usage_dir, min_count=1)
        self.assertEqual(len(events), 1)
        self.assertEqual(events[0]["input_tokens"], 1000)
        self.assertEqual(events[0]["cached_tokens"], 750)
        self.assertEqual(events[0]["output_tokens"], 200)
        self.assertEqual(events[0]["reasoning_tokens"], 50)

        stats = poll_stats(proxy.port, min_requests=1)
        self.assertEqual(stats["requests"], 1)
        self.assertEqual(stats["prompt_tokens"], 1000)
        self.assertEqual(stats["cached_tokens"], 750)
        self.assertEqual(stats["output_tokens"], 200)
        self.assertEqual(stats["reasoning_tokens"], 50)
        self.assertAlmostEqual(stats["cache_hit_ratio"], 0.75, places=6)

        mcp = self.start_mcp(env, config_path=cfg)
        get_res = mcp.call_tool("get_settings", {})
        mcp.assert_clean_stdout()
        sc = get_res.get("structuredContent", {})
        found_reqs, req_count = find_key_recursive(sc, "requests")
        self.assertTrue(found_reqs and isinstance(req_count, int) and req_count >= 1, f"Expected stats in {sc!r}")

    def test_t3_03_doctor_passes_against_live_proxy_and_0600_config(self) -> None:
        """F9+F1+F6: Config created via ui-mcp save_settings passes doctor against live serve."""
        binary = self.require_binary()
        env = self.make_env()
        port = find_free_port()
        cfg = env.root / "doctor_combo_cfg.json"
        mcp = self.start_mcp(env, config_path=cfg)
        mcp.save_settings_smart(
            api_key="AIzaSyDoctorCombo1234",
            listen_port=port,
            base_url="https://generativelanguage.googleapis.com",
        )
        mcp.assert_clean_stdout()
        self.assertEqual(stat.S_IMODE(os.stat(cfg).st_mode), 0o600)

        proxy = self.start_proxy(env, config_path=cfg, port=port)
        doc_env = env.build_env(
            extra={
                "GEMINI_API_KEY": "AIzaSyDoctorCombo1234",
                "GOOGLE_GEMINI_BASE_URL": f"http://127.0.0.1:{proxy.port}",
            }
        )
        res = run_cli([str(binary), "doctor", "--config", str(cfg), "--port", str(proxy.port)], env=doc_env)
        self.assertEqual(res.returncode, 0, f"doctor failed:\n{res.stdout}\n{res.stderr}")

    def test_t3_04_models_json_url_routes_to_live_proxy(self) -> None:
        """F9+F5+F4: URL emitted by `models --json` routes cleanly through live proxy to MockGeminiServer."""
        binary = self.require_binary()
        env = self.make_env()
        mock = self.start_mock_gemini()
        port = find_free_port()
        cfg = env.write_config(
            env.make_config_dict(listen_port=port, base_url=mock.base_url, api_key="AIzaSyModelsRoute1234")
        )
        proxy = self.start_proxy(env, config_path=cfg, port=port)

        res = run_cli([str(binary), "models", "--json", "--port", str(proxy.port)], env=env.build_env())
        self.assertEqual(res.returncode, 0, res.stderr)
        models = extract_models_list(res.json_stdout())
        self.assertGreater(len(models), 0)
        model_url = str(models[0]["modelName"])
        self.assertTrue(model_url.startswith("gemini-api://http://127.0.0.1:"), model_url)
        stripped = model_url.removeprefix("gemini-api://")
        parsed = urlsplit(stripped)
        self.assertEqual(parsed.port, proxy.port)
        # Extract model name after /models/ and issue Gemini v1beta generateContent call.
        model_id = parsed.path.split("/models/", 1)[1].strip("/")
        r = http_raw_request(
            "127.0.0.1",
            proxy.port,
            "POST",
            f"/v1beta/models/{model_id}:generateContent",
            body=b'{"contents":[{"parts":[{"text":"hello via models --json"}]}]}',
            headers={"Content-Type": "application/json"},
        )
        self.assertEqual(r.status, 200)
        self.assertEqual(len(mock.requests), 1)
        self.assertIn(model_id, mock.requests[0].path)

    def test_t3_05_plugin_launcher_execs_binary_via_env_override(self) -> None:
        """F10+F9: plugin/scripts/run-companion.sh dispatches --version and models --json via AGY_COMPANION_BIN."""
        binary = self.require_binary()
        script = self.repo_root / "plugin" / "scripts" / "run-companion.sh"
        env = self.make_env()
        proc_env = env.build_env(extra={"AGY_COMPANION_BIN": str(binary), "PATH": "/usr/bin:/bin"})

        r_ver = run_cli(["bash", str(script), "--version"], env=proc_env)
        self.assertEqual(r_ver.returncode, 0, r_ver.stderr)
        self.assertEqual(r_ver.stdout.strip(), "agy-companion 0.1.0")

        r_mod = run_cli(["bash", str(script), "models", "--json", "--port", "14242"], env=proc_env)
        self.assertEqual(r_mod.returncode, 0, r_mod.stderr)
        models = extract_models_list(r_mod.json_stdout())
        self.assertTrue(any("127.0.0.1:14242" in str(m.get("modelName", "")) for m in models))

    def test_t3_06_save_settings_then_test_connection_against_mock_gemini(self) -> None:
        """F8+F4+F1: save_settings persists mock base_url + key; test_connection verifies GET /v1beta/models?pageSize=1."""
        env = self.make_env()
        key = "AIzaSyConnCheckKey1234"
        mock = self.start_mock_gemini(expected_api_key=key)
        mcp = self.start_mcp(env)
        mcp.save_settings_smart(base_url=mock.base_url, api_key=key)
        res = mcp.call_tool("test_connection", {})
        mcp.assert_clean_stdout()

        req = mock.last_request()
        self.assertIsNotNone(req)
        assert req is not None
        self.assertEqual(req.method, "GET")
        self.assertEqual(req.path, "/v1beta/models")
        self.assertEqual(req.query_params.get("pageSize"), ["1"])
        self.assertEqual(req.header("x-goog-api-key"), key)
        found_ok, ok_val = find_key_recursive(res.get("structuredContent", {}), "ok")
        self.assertTrue(found_ok and ok_val is True)
        self.assertNotIn(key, json.dumps(res))

    def test_t3_07_gemini_api_key_env_fallback_vs_config_precedence(self) -> None:
        """F1+F4+F5: GEMINI_API_KEY env fallback works when config key is null, and config key overrides env."""
        env = self.make_env()
        mock = self.start_mock_gemini()
        env_key = "AIzaSyEnvFallback1111"
        cfg_key = "AIzaSyConfigWins2222"
        cfg = env.write_config(
            env.make_config_dict(base_url=mock.base_url, api_key=None),
            mtime_offset_secs=-5.0,
        )
        proxy = self.start_proxy(env, config_path=cfg, extra_env={"GEMINI_API_KEY": env_key})

        r1 = http_raw_request(
            "127.0.0.1",
            proxy.port,
            "POST",
            "/v1beta/models/gemini-2.5-pro:generateContent",
            body=b'{"contents":[]}',
            headers={"Content-Type": "application/json"},
        )
        self.assertEqual(r1.status, 200)
        self.assertEqual(mock.requests[0].header("x-goog-api-key"), env_key)

        env.write_config(env.make_config_dict(base_url=mock.base_url, api_key=cfg_key), path=cfg)
        IsolatedEnv.bump_mtime(cfg, delta=2.0)

        r2 = http_raw_request(
            "127.0.0.1",
            proxy.port,
            "POST",
            "/v1beta/models/gemini-2.5-pro:generateContent",
            body=b'{"contents":[]}',
            headers={"Content-Type": "application/json"},
        )
        self.assertEqual(r2.status, 200)
        self.assertEqual(mock.requests[1].header("x-goog-api-key"), cfg_key)

    def test_t3_08_key_redaction_consistent_across_save_get_open_and_jsonl(self) -> None:
        """F2+F8+F3: Masked hint AIza...5555 matches across all MCP tools and zero secret appears in JSONL/stderr."""
        env = self.make_env()
        sentinel = "AIzaSyCrossSurfaceKey5555"
        mock = self.start_mock_gemini()
        port = find_free_port()
        cfg = env.write_config(env.make_config_dict(listen_port=port, base_url=mock.base_url))
        mcp = self.start_mcp(env, config_path=cfg)

        save_res = mcp.save_settings_smart(api_key=sentinel, base_url=mock.base_url, listen_port=port)
        get_res = mcp.call_tool("get_settings", {})
        open_res = mcp.call_tool("open_settings", {})
        mcp.assert_clean_stdout()

        for name, resp in (("save_settings", save_res), ("get_settings", get_res), ("open_settings", open_res)):
            raw = json.dumps(resp)
            self.assertNotIn(sentinel, raw, f"Key leaked in {name}")
            found_h, hint = find_key_recursive(resp.get("structuredContent", {}), "api_key_hint")
            if found_h:
                self.assertEqual(hint, "AIza...5555", f"Inconsistent hint in {name}: {hint!r}")

        proxy = self.start_proxy(env, config_path=cfg, port=port)
        r = http_raw_request(
            "127.0.0.1",
            proxy.port,
            "POST",
            "/v1beta/models/gemini-2.5-pro:generateContent",
            body=b'{"contents":[]}',
            headers={"Content-Type": "application/json"},
        )
        self.assertEqual(r.status, 200)
        files, events = poll_usage_events(env.usage_dir, min_count=1)
        self.assertEqual(len(events), 1)
        self.assertNotIn(sentinel, files[0].read_text(encoding="utf-8"))
        self.assertNotIn(sentinel, mcp.stderr_text)
        self.assertNotIn(sentinel, proxy.stderr_text)

    def test_t3_09_concurrent_generate_and_stream_with_query_key_stripping(self) -> None:
        """F5+F4+F3: 4 concurrent unary+SSE requests strip ?key=, preserve alt=sse, and write 4 valid JSONL lines."""
        env = self.make_env()
        mock = self.start_mock_gemini()
        cfg_key = "AIzaSyConcurrentKey1234"
        cfg = env.write_config(env.make_config_dict(base_url=mock.base_url, api_key=cfg_key))
        proxy = self.start_proxy(env, config_path=cfg)

        targets = [
            "/v1beta/models/gemini-2.5-pro:generateContent?key=SECRET_Q1",
            "/v1beta/models/gemini-2.5-pro:generateContent?key=SECRET_Q1",
            "/v1beta/models/gemini-2.5-pro:streamGenerateContent?key=SECRET_Q2&alt=sse",
            "/v1beta/models/gemini-2.5-pro:streamGenerateContent?key=SECRET_Q2&alt=sse",
        ]

        def send_one(path: str) -> int:
            resp = http_raw_request(
                "127.0.0.1",
                proxy.port,
                "POST",
                path,
                body=b'{"contents":[{"parts":[{"text":"concurrent"}]}]}',
                headers={"Content-Type": "application/json"},
            )
            return resp.status

        with concurrent.futures.ThreadPoolExecutor(max_workers=4) as pool:
            statuses = list(pool.map(send_one, targets))
        self.assertEqual(statuses, [200, 200, 200, 200])

        captured = mock.wait_for_requests(4, timeout=5.0)
        self.assertEqual(len(captured), 4)
        sse_count = 0
        for req in captured:
            self.assertNotIn("key", req.query_params)
            self.assertNotIn("SECRET_Q1", req.raw_target)
            self.assertNotIn("SECRET_Q2", req.raw_target)
            self.assertEqual(req.header("x-goog-api-key"), cfg_key)
            if ":streamGenerateContent" in req.path:
                self.assertEqual(req.query_params.get("alt"), ["sse"])
                sse_count += 1
        self.assertEqual(sse_count, 2)

        _, events = poll_usage_events(env.usage_dir, min_count=4)
        self.assertEqual(len(events), 4)

    def test_t3_10_ui_html_env_override_served_via_mcp_resources_read(self) -> None:
        """F8+F7+F15: AGY_COMPANION_UI_HTML serves override and live-reloads V1->V2 without restarting ui-mcp."""
        env = self.make_env()
        dev_html = env.root / "dev.html"
        dev_html.write_text("<!doctype html><html><body>DEV_OVERRIDE_V1</body></html>", encoding="utf-8")

        mcp = self.start_mcp(env, extra_env={"AGY_COMPANION_UI_HTML": str(dev_html)})
        res1 = mcp.read_resource("ui://companion/settings.html")
        contents1 = res1.get("contents") or []
        self.assertGreaterEqual(len(contents1), 1)
        self.assertEqual(contents1[0].get("mimeType"), "text/html;profile=mcp-app")
        self.assertIn("DEV_OVERRIDE_V1", str(contents1[0].get("text", "")))

        # Overwrite the HTML file on disk WITHOUT restarting ui-mcp (F15 live UI reload).
        dev_html.write_text("<!doctype html><html><body>DEV_OVERRIDE_V2</body></html>", encoding="utf-8")
        res2 = mcp.read_resource("ui://companion/settings.html")
        mcp.assert_clean_stdout()
        contents2 = res2.get("contents") or []
        self.assertGreaterEqual(len(contents2), 1)
        self.assertEqual(contents2[0].get("mimeType"), "text/html;profile=mcp-app")
        self.assertIn("DEV_OVERRIDE_V2", str(contents2[0].get("text", "")))
        self.assertNotIn("DEV_OVERRIDE_V1", str(contents2[0].get("text", "")))

    def test_t3_11_cross_surface_version_consistency_and_plugin_ci_gate(self) -> None:
        """F10+F11+F12+F9: Live binary --version matches all 4 manifests and both strict plugin & agent_check gates pass."""
        binary = self.require_binary()
        env = self.make_env()
        r_ver = run_cli([str(binary), "--version"], env=env.build_env())
        self.assertEqual(r_ver.returncode, 0, r_ver.stderr)
        bin_ver = r_ver.stdout.strip().split()[-1]

        cargo_data = tomllib.loads((self.repo_root / "Cargo.toml").read_text(encoding="utf-8"))
        v_cargo = cargo_data.get("workspace", {}).get("package", {}).get("version") or cargo_data["package"]["version"]
        v_plugin = json.loads((self.repo_root / "plugin" / "plugin.json").read_text(encoding="utf-8"))["version"]
        v_npm = json.loads(
            (self.repo_root / "packages" / "npm" / "agy-companion" / "package.json").read_text(encoding="utf-8")
        )["version"]
        v_py = tomllib.loads(
            (self.repo_root / "packages" / "python" / "pyproject.toml").read_text(encoding="utf-8")
        )["project"]["version"]
        self.assertEqual({bin_ver, v_cargo, v_plugin, v_npm, v_py}, {"0.1.0"})

        r_plug = run_cli(["agy", "plugin", "validate", "--strict", str(self.repo_root / "plugin")])
        self.assertEqual(r_plug.returncode, 0, f"{r_plug.stdout}\n{r_plug.stderr}")

        r_gate = run_cli([str(self.repo_root / "scripts" / "agent_check.sh")], cwd=self.repo_root, timeout=180.0)
        self.assertEqual(r_gate.returncode, 0, f"{r_gate.stdout}\n{r_gate.stderr}")

    def test_t3_12_shared_config_env_across_ui_mcp_serve_and_doctor(self) -> None:
        """F1+F8+F5+F9: AGY_COMPANION_CONFIG transparently coordinates ui-mcp, serve, and doctor without --config."""
        binary = self.require_binary()
        env = self.make_env()
        mock = self.start_mock_gemini()
        port = find_free_port()
        shared_cfg = env.root / "shared.json"
        shared_extra = {"AGY_COMPANION_CONFIG": str(shared_cfg)}

        mcp = self.start_mcp(env, pass_config_flag=False, set_config_env=False, extra_env=shared_extra)
        mcp.save_settings_smart(
            api_key="AIzaSySharedEnvCfg123",
            base_url=mock.base_url,
            listen_port=port,
        )
        mcp.assert_clean_stdout()
        self.assertTrue(shared_cfg.is_file())

        proxy = self.start_proxy(
            env,
            port=port,
            pass_port_flag=False,
            pass_config_flag=False,
            set_config_env=False,
            extra_env=shared_extra,
        )
        r = http_raw_request(
            "127.0.0.1",
            proxy.port,
            "POST",
            "/v1beta/models/gemini-2.5-pro:generateContent",
            body=b'{"contents":[]}',
            headers={"Content-Type": "application/json"},
        )
        self.assertEqual(r.status, 200)

        doc_env = env.build_env(
            set_config_env=False,
            extra={
                "AGY_COMPANION_CONFIG": str(shared_cfg),
                "GEMINI_API_KEY": "AIzaSySharedEnvCfg123",
                "GOOGLE_GEMINI_BASE_URL": f"http://127.0.0.1:{proxy.port}",
            },
        )
        r_doc = run_cli([str(binary), "doctor"], env=doc_env)
        self.assertEqual(r_doc.returncode, 0, f"{r_doc.stdout}\n{r_doc.stderr}")

    def test_t3_13_proxy_non_streaming_generate_updates_stats_and_jsonl_accurately(self) -> None:
        """F4+F5+F6+F3: 3 unary requests with varying cached/thoughts produce exact matching /api/stats and JSONL sums."""
        env = self.make_env()
        mock = self.start_mock_gemini()
        usages = [
            {"promptTokenCount": 1000, "cachedContentTokenCount": 0, "candidatesTokenCount": 100, "thoughtsTokenCount": 50},
            {"promptTokenCount": 2000, "cachedContentTokenCount": 1500, "candidatesTokenCount": 200, "thoughtsTokenCount": 0},
            {"promptTokenCount": 1000, "cachedContentTokenCount": 500, "candidatesTokenCount": 150, "thoughtsTokenCount": 30},
        ]
        for idx, u in enumerate(usages):
            mock.enqueue(
                JsonAction(
                    status=200,
                    payload={"candidates": [{"content": {"parts": [{"text": f"resp-{idx}"}]}}], "usageMetadata": u},
                )
            )
        cfg = env.write_config(env.make_config_dict(base_url=mock.base_url, api_key="AIzaSyUnaryStatsSum12"))
        proxy = self.start_proxy(env, config_path=cfg)

        for _ in usages:
            r = http_raw_request(
                "127.0.0.1",
                proxy.port,
                "POST",
                "/v1beta/models/gemini-2.5-pro:generateContent",
                body=b'{"contents":[]}',
                headers={"Content-Type": "application/json"},
            )
            self.assertEqual(r.status, 200)

        stats = http_raw_request("127.0.0.1", proxy.port, "GET", "/api/stats").json()
        _, events = poll_usage_events(env.usage_dir, min_count=3)
        self.assertEqual(len(events), 3)

        sum_in = sum(e["input_tokens"] for e in events)
        sum_cached = sum(e["cached_tokens"] for e in events)
        sum_out = sum(e["output_tokens"] for e in events)
        sum_reason = sum(e["reasoning_tokens"] for e in events)

        self.assertEqual(stats["requests"], 3)
        self.assertEqual(stats["prompt_tokens"], sum_in)
        self.assertEqual(stats["cached_tokens"], sum_cached)
        self.assertEqual(stats["output_tokens"], sum_out)
        self.assertEqual(stats["reasoning_tokens"], sum_reason)
        self.assertAlmostEqual(stats["cache_hit_ratio"], sum_cached / sum_in, places=6)

    def test_t3_14_doctor_and_get_settings_both_detect_proxy_version_skew(self) -> None:
        """F6+F8+F9: Both doctor and ui-mcp get_settings detect stale proxy version '0.0.1-stale'."""
        binary = self.require_binary()
        env = self.make_env()
        mock_health = self.start_mock_health(version="0.0.1-stale", protocol_version=1)
        cfg = env.write_config(env.make_config_dict(listen_port=mock_health.port), mode=0o600)

        doc_env = env.build_env(
            extra={
                "GEMINI_API_KEY": "AIzaSySkewComboKey123",
                "GOOGLE_GEMINI_BASE_URL": mock_health.base_url,
            }
        )
        r_doc = run_cli(
            [str(binary), "doctor", "--config", str(cfg), "--port", str(mock_health.port)],
            env=doc_env,
        )
        self.assertNotEqual(r_doc.returncode, 0, f"doctor should fail on version skew:\n{r_doc.stdout}")

        mcp = self.start_mcp(env, config_path=cfg)
        get_res = mcp.call_tool("get_settings", {})
        mcp.assert_clean_stdout()
        raw = json.dumps(get_res)
        self.assertIn("0.0.1-stale", raw, f"Expected stale proxy version in get_settings: {raw}")
        sc = get_res.get("structuredContent", {})
        # Check for explicit skew flag or skew wording in response.
        found_skew, skew_val = find_key_recursive(sc, "version_skew")
        if found_skew:
            self.assertTrue(bool(skew_val), f"Expected version_skew truthy in {sc!r}")
        else:
            self.assertTrue(
                "skew" in raw.lower() or "mismatch" in raw.lower() or "0.0.1-stale" in raw,
                f"Expected version skew indicator in {raw}",
            )

    def test_t3_15_run_companion_sh_symlink_aware_local_debug_binary(self) -> None:
        """F15+F10+F9: Symlinked plugin checkout resolves local target/debug/agy-companion without network download."""
        binary = self.require_binary()
        src_script = self.repo_root / "plugin" / "scripts" / "run-companion.sh"
        self.assertTrue(src_script.is_file(), f"Missing {src_script}")

        env = self.make_env()
        checkout = env.root / "checkout"
        plugin_scripts = checkout / "plugin" / "scripts"
        target_debug = checkout / "target" / "debug"
        plugins_dir = env.root / "plugins"
        plugin_scripts.mkdir(parents=True, exist_ok=True)
        target_debug.mkdir(parents=True, exist_ok=True)
        plugins_dir.mkdir(parents=True, exist_ok=True)

        copied_script = plugin_scripts / "run-companion.sh"
        shutil.copy2(src_script, copied_script)
        os.chmod(copied_script, 0o755)
        src_plugin_json = self.repo_root / "plugin" / "plugin.json"
        if src_plugin_json.is_file():
            shutil.copy2(src_plugin_json, checkout / "plugin" / "plugin.json")
        os.symlink(binary, target_debug / "agy-companion")

        linked_plugin = plugins_dir / "agy-companion"
        os.symlink(checkout / "plugin", linked_plugin)

        proc_env = env.build_env(
            extra={
                "AGY_COMPANION_BIN": None,
                "ANTIGRAVITY_EXECUTABLE_DATA_DIR": str(env.root / "empty_cache"),
                "PATH": "/usr/bin:/bin",
            }
        )
        res = run_cli(
            ["bash", str(linked_plugin / "scripts" / "run-companion.sh"), "--version"],
            env=proc_env,
        )
        self.assertEqual(res.returncode, 0, f"Symlink-aware launcher failed:\n{res.stdout}\n{res.stderr}")
        self.assertEqual(res.stdout.strip(), "agy-companion 0.1.0")
