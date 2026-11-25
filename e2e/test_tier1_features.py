"""Tier 1: Core Feature Coverage (82 isolated happy-path opaque-box E2E tests).

Covers all 12 core features (6–7 tests per feature F1–F12 = 74 tests) plus 8
additive feature tests for F14, F15 & F16 (XDG paths).
"""

from __future__ import annotations

import datetime
import importlib.util
import json
import os
import re
import shutil
import signal
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
    http_stream_request,
    poll_stats,
    poll_usage_events,
    run_cli,
)
from mock_gemini import JsonAction, SseStreamAction, build_sse_events


def _extract_cargo_workspace_version(repo_root: Path) -> str:
    cargo_toml = repo_root / "Cargo.toml"
    data = tomllib.loads(cargo_toml.read_text(encoding="utf-8"))
    ver = data.get("workspace", {}).get("package", {}).get("version")
    if not ver:
        ver = data.get("package", {}).get("version")
    assert isinstance(ver, str) and ver.strip(), f"Missing version in {cargo_toml}"
    return ver.strip()


def _text_blocks(tool_result: dict[str, Any]) -> str:
    blocks = tool_result.get("content") or []
    return "\n".join(
        str(b.get("text", ""))
        for b in blocks
        if isinstance(b, dict) and b.get("type") == "text"
    )


def _find_any_key(obj: Any, candidates: tuple[str, ...]) -> tuple[bool, Any]:
    for key in candidates:
        found, val = find_key_recursive(obj, key)
        if found:
            return True, val
    return False, None


def _strip_ts_comments(source: str) -> str:
    """Strip // and /* */ comments while preserving string/template literals."""
    pattern = re.compile(
        r'("(?:\\.|[^"\\])*"|\'(?:\\.|[^\'\\])*\'|`(?:\\.|[^`\\])*`)'
        r"|//[^\n]*|/\*[\s\S]*?\*/"
    )
    return pattern.sub(lambda m: m.group(1) if m.group(1) is not None else " ", source)


class TestTier1F1Config(E2ETestCase):
    TIER = 1
    FEATURE = "F1"

    def test_t1_f1_default_config_schema_version_and_values(self) -> None:
        """Missing config file yields schema_version=1 defaults via get_settings."""
        env = self.make_env()
        nonexistent_cfg = env.root / "does_not_exist" / "cfg.json"
        mcp = self.start_mcp(env, config_path=nonexistent_cfg)
        res = mcp.call_tool("get_settings", {})
        mcp.assert_clean_stdout()
        sc = res.get("structuredContent", {})
        self.assertIsInstance(sc, dict)

        found_sv, sv = find_key_recursive(sc, "schema_version")
        self.assertTrue(found_sv, f"Missing schema_version in {sc!r}")
        self.assertEqual(sv, 1)

        found_host, host = find_key_recursive(sc, "listen_host")
        self.assertTrue(found_host, f"Missing listen_host in {sc!r}")
        self.assertEqual(host, "127.0.0.1")

        found_port, port = find_key_recursive(sc, "listen_port")
        self.assertTrue(found_port, f"Missing listen_port in {sc!r}")
        self.assertEqual(port, 4141)

        found_url, base_url = _find_any_key(sc, ("gemini_base_url", "base_url"))
        self.assertTrue(found_url, f"Missing gemini_base_url in {sc!r}")
        self.assertTrue(
            str(base_url).startswith("https://generativelanguage.googleapis.com"),
            f"Unexpected default base_url: {base_url!r}",
        )

        found_timeout, timeout_secs = _find_any_key(sc, ("gemini_timeout_secs", "timeout_secs"))
        self.assertTrue(found_timeout, f"Missing timeout_secs in {sc!r}")
        self.assertEqual(timeout_secs, 900)

        found_cfg, configured = find_key_recursive(sc, "api_key_configured")
        self.assertTrue(found_cfg, f"Missing api_key_configured in {sc!r}")
        self.assertIs(configured, False)

        found_hint, hint = find_key_recursive(sc, "api_key_hint")
        if found_hint:
            self.assertIsNone(hint)

    def test_t1_f1_save_creates_config_with_mode_0600(self) -> None:
        """save_settings creates missing parent dirs and persists schema_version=1 at 0600."""
        env = self.make_env()
        nested_cfg = env.root / "nested" / "dir" / "config.json"
        mcp = self.start_mcp(env, config_path=nested_cfg)
        mcp.save_settings_smart(
            api_key="AIzaSyModeCheck12345678",
            default_model="gemini-4-hydrogen",
            base_url="https://generativelanguage.googleapis.com",
        )
        mcp.assert_clean_stdout()
        self.assertTrue(nested_cfg.is_file(), f"Config file was not created at {nested_cfg}")
        mode = stat.S_IMODE(os.stat(nested_cfg).st_mode)
        self.assertEqual(mode, 0o600, f"Expected 0600, got {oct(mode)}")

        saved = json.loads(nested_cfg.read_text(encoding="utf-8"))
        self.assertEqual(saved.get("schema_version"), 1)
        self.assertEqual(saved.get("gemini", {}).get("default_model"), "gemini-4-hydrogen")
        self.assertEqual(saved.get("gemini", {}).get("api_key"), "AIzaSyModeCheck12345678")

    def test_t1_f1_atomic_save_leaves_no_temp_files(self) -> None:
        """Sequential atomic save_settings writes leave only config.json at 0600."""
        env = self.make_env()
        cfg_dir = env.root / "cfg_dir"
        cfg_path = cfg_dir / "config.json"
        mcp = self.start_mcp(env, config_path=cfg_path)
        mcp.save_settings_smart(default_model="gemini-3.8-flash")
        mcp.save_settings_smart(default_model="gemini-4-hydrogen")
        mcp.assert_clean_stdout()

        entries = sorted(p.name for p in cfg_dir.iterdir())
        self.assertEqual(entries, ["config.json"], f"Unexpected temp files left behind: {entries}")
        parsed = json.loads(cfg_path.read_text(encoding="utf-8"))
        self.assertEqual(parsed.get("gemini", {}).get("default_model"), "gemini-4-hydrogen")
        self.assertEqual(stat.S_IMODE(os.stat(cfg_path).st_mode), 0o600)

    def test_t1_f1_env_var_config_path_override(self) -> None:
        """AGY_COMPANION_CONFIG env var selects config path when --config is omitted."""
        env = self.make_env()
        custom_cfg = env.root / "env_cfg.json"
        env.write_config(
            env.make_config_dict(default_model="gemini-3.8-flash"),
            mode=0o600,
            path=custom_cfg,
        )
        mcp = self.start_mcp(
            env,
            pass_config_flag=False,
            set_config_env=False,
            extra_env={"AGY_COMPANION_CONFIG": str(custom_cfg)},
        )
        res = mcp.call_tool("get_settings", {})
        mcp.assert_clean_stdout()
        found, model = _find_any_key(res.get("structuredContent", {}), ("gemini_default_model", "default_model"))
        self.assertTrue(found, f"Missing default_model in {res!r}")
        self.assertEqual(model, "gemini-3.8-flash")

    def test_t1_f1_accepts_localhost_listen_host_and_https_upstream(self) -> None:
        """serve accepts listen_host='localhost' and remote https:// upstream."""
        env = self.make_env()
        port = find_free_port()
        cfg_path = env.write_config(
            env.make_config_dict(
                listen_host="localhost",
                listen_port=port,
                base_url="https://generativelanguage.googleapis.com",
            ),
            mode=0o600,
        )
        proxy = self.start_proxy(env, config_path=cfg_path, port=port, pass_port_flag=False)
        resp = http_raw_request("127.0.0.1", proxy.port, "GET", "/health")
        self.assertEqual(resp.status, 200)
        self.assertEqual(resp.json().get("status"), "ok")

    def test_t1_f1_usage_dir_config_honored(self) -> None:
        """Configured usage_dir receives monthly JSONL events from proxy traffic."""
        env = self.make_env()
        mock = self.start_mock_gemini()
        custom_usage = env.root / "custom_usage"
        cfg_path = env.write_config(
            env.make_config_dict(
                base_url=mock.base_url,
                api_key="AIzaSyUsageDirTest1234",
                usage_dir=custom_usage,
            )
        )
        proxy = self.start_proxy(env, config_path=cfg_path)
        resp = http_raw_request(
            "127.0.0.1",
            proxy.port,
            "POST",
            "/v1beta/models/gemini-4-hydrogen:generateContent",
            body=b'{"contents":[{"parts":[{"text":"hi"}]}]}',
            headers={"Content-Type": "application/json"},
        )
        self.assertEqual(resp.status, 200)
        files, events = poll_usage_events(custom_usage, min_count=1)
        self.assertEqual(len(files), 1, f"Expected 1 monthly JSONL file in {custom_usage}, got {files}")
        self.assertEqual(len(events), 1)
        self.assertRegex(files[0].name, r"^\d{4}-\d{2}\.jsonl$")


class TestTier1F2Redaction(E2ETestCase):
    TIER = 1
    FEATURE = "F2"

    def test_t1_f2_standard_gemini_key_masked_prefix_suffix(self) -> None:
        """Standard 21-char Gemini key masks to AIza...abcd across save_settings and get_settings."""
        env = self.make_env()
        mcp = self.start_mcp(env)
        secret = "AIzaSyD1234567890abcd"
        save_res = mcp.save_settings_smart(api_key=secret)
        get_res = mcp.call_tool("get_settings", {})
        mcp.assert_clean_stdout()

        for label, payload in (("save_settings", save_res), ("get_settings", get_res)):
            raw = json.dumps(payload)
            self.assertNotIn(secret, raw, f"Plaintext key leaked in {label}")
            self.assertNotIn("SyD1234567890", raw, f"Middle key chars leaked in {label}")
            sc = payload.get("structuredContent", {})
            found_c, configured = find_key_recursive(sc, "api_key_configured")
            found_h, hint = find_key_recursive(sc, "api_key_hint")
            self.assertTrue(found_c and configured is True, f"{label}: {sc!r}")
            self.assertTrue(found_h and hint == "AIza...abcd", f"{label}: {sc!r}")

    def test_t1_f2_unconfigured_key_reports_false_and_null_hint(self) -> None:
        """Fresh config without API key reports api_key_configured=false and api_key_hint=None."""
        env = self.make_env()
        mcp = self.start_mcp(env)
        res = mcp.call_tool("get_settings", {})
        mcp.assert_clean_stdout()
        sc = res.get("structuredContent", {})
        found_c, configured = find_key_recursive(sc, "api_key_configured")
        self.assertTrue(found_c)
        self.assertIs(configured, False)
        found_h, hint = find_key_recursive(sc, "api_key_hint")
        if found_h:
            self.assertIsNone(hint)

    def test_t1_f2_redacted_config_omits_plaintext_field(self) -> None:
        """get_settings structuredContent omits plaintext api_key field entirely."""
        env = self.make_env()
        secret = "AIzaSySecretSentinel998877"
        env.write_config(env.make_config_dict(api_key=secret))
        mcp = self.start_mcp(env)
        res = mcp.call_tool("get_settings", {})
        mcp.assert_clean_stdout()
        raw = json.dumps(res)
        self.assertNotIn(secret, raw)
        sc = res.get("structuredContent", {})
        found_plain, plain_val = find_key_recursive(sc, "api_key")
        if found_plain:
            self.assertNotEqual(plain_val, secret)
        found_h, hint = find_key_recursive(sc, "api_key_hint")
        self.assertTrue(found_h)
        self.assertEqual(hint, "AIza...8877")

    def test_t1_f2_open_settings_redacts_api_key(self) -> None:
        """open_settings redacts stored API key across both content and structuredContent."""
        env = self.make_env()
        secret = "AIzaSyOpenPanelSecret1234"
        env.write_config(env.make_config_dict(api_key=secret))
        mcp = self.start_mcp(env)
        res = mcp.call_tool("open_settings", {})
        mcp.assert_clean_stdout()
        raw = json.dumps(res)
        self.assertNotIn(secret, raw)
        self.assertNotIn("SyOpenPanelSecret", raw)
        sc = res.get("structuredContent", {})
        found_h, hint = find_key_recursive(sc, "api_key_hint")
        if found_h:
            self.assertEqual(hint, "AIza...1234")
        else:
            found_c, configured = find_key_recursive(sc, "api_key_configured")
            self.assertTrue(found_c and configured is True, f"Expected configured state in {res!r}")

    def test_t1_f2_save_settings_returns_masked_not_plaintext(self) -> None:
        """save_settings response surfaces masked hint and zero plaintext key."""
        env = self.make_env()
        secret = "AIzaSyReturnMaskCheck5678"
        mcp = self.start_mcp(env)
        res = mcp.save_settings_smart(api_key=secret)
        mcp.assert_clean_stdout()
        raw = json.dumps(res)
        self.assertNotIn(secret, raw)
        self.assertIn("AIza...5678", raw)

    def test_t1_f2_long_api_key_masks_to_exact_4_dots_4(self) -> None:
        """64-char API key masks to exact 11-char prefix4...suffix4 representation."""
        env = self.make_env()
        long_key = "ABCD" + ("x" * 56) + "WXYZ"
        env.write_config(env.make_config_dict(api_key=long_key))
        mcp = self.start_mcp(env)
        res = mcp.call_tool("get_settings", {})
        mcp.assert_clean_stdout()
        raw = json.dumps(res)
        self.assertNotIn(long_key, raw)
        sc = res.get("structuredContent", {})
        found_c, configured = find_key_recursive(sc, "api_key_configured")
        found_h, hint = find_key_recursive(sc, "api_key_hint")
        self.assertTrue(found_c and configured is True)
        self.assertTrue(found_h and hint == "ABCD...WXYZ")


class TestTier1F3UsageSeam(E2ETestCase):
    TIER = 1
    FEATURE = "F3"

    def test_t1_f3_parses_all_four_token_fields_unary(self) -> None:
        """Unary :generateContent usageMetadata maps all four token counts to JSONL."""
        env = self.make_env()
        mock = self.start_mock_gemini(
            default_usage={
                "promptTokenCount": 1000,
                "candidatesTokenCount": 250,
                "cachedContentTokenCount": 800,
                "thoughtsTokenCount": 120,
                "totalTokenCount": 1370,
            }
        )
        cfg = env.write_config(env.make_config_dict(base_url=mock.base_url, api_key="AIzaSyUnaryUsage1234"))
        proxy = self.start_proxy(env, config_path=cfg)
        resp = http_raw_request(
            "127.0.0.1",
            proxy.port,
            "POST",
            "/v1beta/models/gemini-4-hydrogen:generateContent",
            body=b'{"contents":[{"parts":[{"text":"count tokens"}]}]}',
            headers={"Content-Type": "application/json"},
        )
        self.assertEqual(resp.status, 200)
        _, events = poll_usage_events(env.usage_dir, min_count=1)
        self.assertEqual(len(events), 1)
        ev = events[0]
        self.assertEqual(ev["input_tokens"], 1000)
        self.assertEqual(ev["output_tokens"], 250)
        self.assertEqual(ev["cached_tokens"], 800)
        self.assertEqual(ev["reasoning_tokens"], 120)

    def test_t1_f3_parses_all_four_token_fields_sse(self) -> None:
        """Streaming :streamGenerateContent?alt=sse usageMetadata maps all four fields to JSONL."""
        env = self.make_env()
        mock = self.start_mock_gemini(
            default_usage={
                "promptTokenCount": 500,
                "candidatesTokenCount": 100,
                "cachedContentTokenCount": 300,
                "thoughtsTokenCount": 50,
            }
        )
        cfg = env.write_config(env.make_config_dict(base_url=mock.base_url, api_key="AIzaSySseUsage123456"))
        proxy = self.start_proxy(env, config_path=cfg)
        resp = http_raw_request(
            "127.0.0.1",
            proxy.port,
            "POST",
            "/v1beta/models/gemini-4-hydrogen:streamGenerateContent?alt=sse",
            body=b'{"contents":[{"parts":[{"text":"stream"}]}]}',
            headers={"Content-Type": "application/json"},
        )
        self.assertEqual(resp.status, 200)
        _, events = poll_usage_events(env.usage_dir, min_count=1)
        self.assertEqual(len(events), 1)
        ev = events[0]
        self.assertEqual(ev["input_tokens"], 500)
        self.assertEqual(ev["output_tokens"], 100)
        self.assertEqual(ev["cached_tokens"], 300)
        self.assertEqual(ev["reasoning_tokens"], 50)

    def test_t1_f3_usage_event_contains_all_canonical_fields(self) -> None:
        """JSONL UsageEvent contains all 10 canonical fields with valid types."""
        env = self.make_env()
        mock = self.start_mock_gemini()
        cfg = env.write_config(env.make_config_dict(base_url=mock.base_url, api_key="AIzaSyCanonicalFields1"))
        proxy = self.start_proxy(env, config_path=cfg)
        resp = http_raw_request(
            "127.0.0.1",
            proxy.port,
            "POST",
            "/v1beta/models/gemini-4-hydrogen:generateContent",
            body=b'{"contents":[]}',
            headers={"Content-Type": "application/json"},
        )
        self.assertEqual(resp.status, 200)
        _, events = poll_usage_events(env.usage_dir, min_count=1)
        self.assertEqual(len(events), 1)
        ev = events[0]
        required = {
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
        self.assertTrue(required.issubset(ev.keys()), f"Missing keys: {required - set(ev.keys())}")
        self.assertRegex(str(ev["timestamp_utc"]), r"^\d{4}-\d{2}-\d{2}T\d{2}:\d{2}:\d{2}")
        self.assertTrue(isinstance(ev["request_id"], str) and len(ev["request_id"]) > 0)
        self.assertEqual(ev["provider"], "gemini")
        self.assertIn("gemini-4-hydrogen", str(ev["model"]))
        self.assertIsInstance(ev["latency_ms"], int)
        self.assertGreaterEqual(ev["latency_ms"], 0)
        self.assertEqual(ev["source"], "proxy")

    def test_t1_f3_jsonl_written_to_monthly_filename(self) -> None:
        """Usage file name matches UTC YYYY-MM.jsonl matching event timestamp_utc."""
        env = self.make_env()
        mock = self.start_mock_gemini()
        cfg = env.write_config(env.make_config_dict(base_url=mock.base_url, api_key="AIzaSyMonthlyFile1234"))
        proxy = self.start_proxy(env, config_path=cfg)
        resp = http_raw_request(
            "127.0.0.1",
            proxy.port,
            "POST",
            "/v1beta/models/gemini-4-hydrogen:generateContent",
            body=b'{"contents":[]}',
            headers={"Content-Type": "application/json"},
        )
        self.assertEqual(resp.status, 200)
        files, events = poll_usage_events(env.usage_dir, min_count=1)
        self.assertEqual(len(files), 1)
        expected_ym = str(events[0]["timestamp_utc"])[:7]
        current_ym = datetime.datetime.now(datetime.timezone.utc).strftime("%Y-%m")
        self.assertEqual(files[0].name, f"{expected_ym}.jsonl")
        self.assertEqual(expected_ym, current_ym)

    def test_t1_f3_jsonl_created_with_mode_0600(self) -> None:
        """Newly created monthly JSONL usage log enforces strict 0600 permissions (S3)."""
        env = self.make_env()
        mock = self.start_mock_gemini()
        cfg = env.write_config(env.make_config_dict(base_url=mock.base_url, api_key="AIzaSyJsonlPerm06001"))
        proxy = self.start_proxy(env, config_path=cfg)
        resp = http_raw_request(
            "127.0.0.1",
            proxy.port,
            "POST",
            "/v1beta/models/gemini-4-hydrogen:generateContent",
            body=b'{"contents":[]}',
            headers={"Content-Type": "application/json"},
        )
        self.assertEqual(resp.status, 200)
        files, _ = poll_usage_events(env.usage_dir, min_count=1)
        self.assertEqual(len(files), 1)
        mode = stat.S_IMODE(os.stat(files[0]).st_mode)
        self.assertEqual(mode, 0o600, f"Expected 0600 for {files[0]}, got {oct(mode)}")

    def test_t1_f3_distinct_request_ids_per_request(self) -> None:
        """Sequential requests emit distinct non-empty request_id values."""
        env = self.make_env()
        mock = self.start_mock_gemini()
        cfg = env.write_config(env.make_config_dict(base_url=mock.base_url, api_key="AIzaSyDistinctReqIds1"))
        proxy = self.start_proxy(env, config_path=cfg)
        for _ in range(2):
            resp = http_raw_request(
                "127.0.0.1",
                proxy.port,
                "POST",
                "/v1beta/models/gemini-4-hydrogen:generateContent",
                body=b'{"contents":[]}',
                headers={"Content-Type": "application/json"},
            )
            self.assertEqual(resp.status, 200)
        _, events = poll_usage_events(env.usage_dir, min_count=2)
        self.assertEqual(len(events), 2)
        id1, id2 = events[0]["request_id"], events[1]["request_id"]
        self.assertTrue(id1 and id2)
        self.assertNotEqual(id1, id2)


class TestTier1F4Provider(E2ETestCase):
    TIER = 1
    FEATURE = "F4"

    def test_t1_f4_forwards_exact_request_body_bytes_untouched(self) -> None:
        """Non-canonical JSON body bytes reach upstream byte-for-byte untouched."""
        env = self.make_env()
        mock = self.start_mock_gemini()
        cfg = env.write_config(env.make_config_dict(base_url=mock.base_url, api_key="AIzaSyExactBytes1234"))
        proxy = self.start_proxy(env, config_path=cfg)
        raw_payload = (
            b'{\n'
            b'  "systemInstruction": { "parts": [ { "text": "keep   spaces" } ] },\n'
            b'  "tools": [ { "functionDeclarations": [ { "name": "ping" } ] } ],\n'
            b'  "contents": [ { "parts": [ { "text": "hello  world" } ] } ],\n'
            b'  "cachedContent": "cachedContents/abc"\n'
            b'}'
        )
        resp = http_raw_request(
            "127.0.0.1",
            proxy.port,
            "POST",
            "/v1beta/models/gemini-4-hydrogen:generateContent",
            body=raw_payload,
            headers={"Content-Type": "application/json"},
        )
        self.assertEqual(resp.status, 200)
        req = mock.last_request()
        self.assertIsNotNone(req)
        assert req is not None
        self.assertEqual(req.body, raw_payload)

    def test_t1_f4_sends_x_goog_api_key_header_not_query_param(self) -> None:
        """Proxy authenticates upstream via x-goog-api-key header, never ?key= query param."""
        env = self.make_env()
        mock = self.start_mock_gemini()
        api_key = "AIzaSyHeaderAuthTest1234"
        cfg = env.write_config(env.make_config_dict(base_url=mock.base_url, api_key=api_key))
        proxy = self.start_proxy(env, config_path=cfg)
        resp = http_raw_request(
            "127.0.0.1",
            proxy.port,
            "POST",
            "/v1beta/models/gemini-4-hydrogen:generateContent",
            body=b'{"contents":[]}',
            headers={"Content-Type": "application/json"},
        )
        self.assertEqual(resp.status, 200)
        req = mock.last_request()
        assert req is not None
        self.assertEqual(req.header("x-goog-api-key"), api_key)
        self.assertNotIn("key=", req.raw_target)
        self.assertNotIn(api_key, req.raw_target)

    def test_t1_f4_streams_sse_chunks_byte_for_byte(self) -> None:
        """Multi-chunk SSE response streams incrementally unbuffered and arrives byte-for-byte identical."""
        env = self.make_env()
        mock = self.start_mock_gemini()
        c1 = build_sse_events([{"candidates": [{"content": {"parts": [{"text": "one "}]}}]}])
        c2 = build_sse_events([{"candidates": [{"content": {"parts": [{"text": "two "}]}}]}])
        c3 = build_sse_events(
            [
                {
                    "candidates": [{"content": {"parts": [{"text": "three"}]}}],
                    "usageMetadata": {"promptTokenCount": 10, "candidatesTokenCount": 5},
                }
            ]
        )
        mock.enqueue(SseStreamAction(chunks=[c1, c2, c3], chunk_delay_s=0.15))
        cfg = env.write_config(env.make_config_dict(base_url=mock.base_url, api_key="AIzaSySseExactBytes1"))
        proxy = self.start_proxy(env, config_path=cfg)
        received_chunks: list[bytes] = []
        timestamps: list[float] = []
        with http_stream_request(
            "127.0.0.1",
            proxy.port,
            "POST",
            "/v1beta/models/gemini-4-hydrogen:streamGenerateContent?alt=sse",
            body=b'{"contents":[]}',
            headers={"Content-Type": "application/json"},
        ) as stream:
            self.assertEqual(stream.status, 200)
            self.assertIn("text/event-stream", (stream.header("content-type") or "").lower())
            for chunk, ts in stream.iter_chunks():
                received_chunks.append(chunk)
                timestamps.append(ts)
        self.assertGreaterEqual(
            len(received_chunks),
            2,
            f"Expected incremental unbuffered SSE chunks, got {len(received_chunks)} chunk(s)",
        )
        self.assertGreaterEqual(
            timestamps[-1] - timestamps[0],
            0.12,
            f"Expected unbuffered chunk arrival spread >= 0.12s, got {timestamps[-1] - timestamps[0]:.4f}s",
        )
        self.assertEqual(b"".join(received_chunks), c1 + c2 + c3)

    def test_t1_f4_passthrough_caller_api_key_when_config_key_unset(self) -> None:
        """Caller x-goog-api-key header passes through to upstream when config api_key and GEMINI_API_KEY are unset (A5)."""
        env = self.make_env()
        caller_key = "AIzaSyCallerHeaderKey12"
        mock = self.start_mock_gemini(expected_api_key=caller_key)
        cfg = env.write_config(env.make_config_dict(base_url=mock.base_url, api_key=None))
        proxy = self.start_proxy(env, config_path=cfg)
        resp = http_raw_request(
            "127.0.0.1",
            proxy.port,
            "POST",
            "/v1beta/models/gemini-4-hydrogen:generateContent",
            body=b'{"contents":[]}',
            headers={"Content-Type": "application/json", "x-goog-api-key": caller_key},
        )
        self.assertEqual(resp.status, 200)
        req = mock.last_request()
        self.assertIsNotNone(req)
        assert req is not None
        self.assertEqual(req.header("x-goog-api-key"), caller_key)
        self.assertNotIn("key=", req.raw_target)
        self.assertNotIn(caller_key, req.raw_target)
        self.assertNotIn(caller_key, proxy.stderr_text)

    def test_t1_f4_preserves_alt_sse_query_param(self) -> None:
        """Query parameter ?alt=sse is preserved verbatim to upstream."""
        env = self.make_env()
        mock = self.start_mock_gemini()
        cfg = env.write_config(env.make_config_dict(base_url=mock.base_url, api_key="AIzaSyAltSseParam1234"))
        proxy = self.start_proxy(env, config_path=cfg)
        target = "/v1beta/models/gemini-4-hydrogen:streamGenerateContent?alt=sse"
        resp = http_raw_request(
            "127.0.0.1",
            proxy.port,
            "POST",
            target,
            body=b'{"contents":[]}',
            headers={"Content-Type": "application/json"},
        )
        self.assertEqual(resp.status, 200)
        req = mock.last_request()
        assert req is not None
        self.assertEqual(req.raw_target, target)

    def test_t1_f4_propagates_response_content_type_and_status(self) -> None:
        """Upstream status, Content-Type, and exact JSON response bytes propagate verbatim."""
        env = self.make_env()
        mock = self.start_mock_gemini()
        payload = {"candidates": [{"content": {"parts": [{"text": "exact-json-test"}]}}]}
        mock.enqueue(JsonAction(status=200, payload=payload, content_type="application/json; charset=UTF-8"))
        cfg = env.write_config(env.make_config_dict(base_url=mock.base_url, api_key="AIzaSyRespContentType1"))
        proxy = self.start_proxy(env, config_path=cfg)
        resp = http_raw_request(
            "127.0.0.1",
            proxy.port,
            "POST",
            "/v1beta/models/gemini-4-hydrogen:generateContent",
            body=b'{"contents":[]}',
            headers={"Content-Type": "application/json"},
        )
        self.assertEqual(resp.status, 200)
        self.assertIn("application/json", (resp.header("content-type") or "").lower())
        self.assertEqual(resp.json(), payload)

    def test_t1_f4_health_check_hits_models_page_size_1(self) -> None:
        """test_connection invokes GET /v1beta/models?pageSize=1 with x-goog-api-key."""
        env = self.make_env()
        api_key = "AIzaSyHealthCheckKey12"
        mock = self.start_mock_gemini(expected_api_key=api_key)
        cfg = env.write_config(env.make_config_dict(base_url=mock.base_url, api_key=api_key))
        mcp = self.start_mcp(env, config_path=cfg)
        res = mcp.call_tool("test_connection", {})
        mcp.assert_clean_stdout()
        req = mock.last_request()
        self.assertIsNotNone(req)
        assert req is not None
        self.assertEqual(req.method, "GET")
        self.assertEqual(req.path, "/v1beta/models")
        self.assertEqual(req.query_params.get("pageSize"), ["1"])
        self.assertEqual(req.header("x-goog-api-key"), api_key)

        sc = res.get("structuredContent", {})
        found_ok, ok_val = find_key_recursive(sc, "ok")
        self.assertTrue(found_ok and ok_val is True, f"Expected ok=True in {res!r}")


class TestTier1F5Proxy(E2ETestCase):
    TIER = 1
    FEATURE = "F5"

    def test_t1_f5_binds_strictly_to_loopback_port(self) -> None:
        """serve binds 127.0.0.1:<port> cleanly and responds on /health."""
        env = self.make_env()
        cfg = env.write_config(env.make_config_dict())
        port = find_free_port()
        proxy = self.start_proxy(env, config_path=cfg, port=port)
        resp = http_raw_request("127.0.0.1", proxy.port, "GET", "/health")
        self.assertEqual(resp.status, 200)
        self.assertNotIn("0.0.0.0", proxy.stderr_text)

    def test_t1_f5_proxies_unary_generate_content(self) -> None:
        """POST /v1beta/models/gemini-3.8-flash:generateContent forwards body and updates stats."""
        env = self.make_env()
        mock = self.start_mock_gemini()
        cfg = env.write_config(env.make_config_dict(base_url=mock.base_url, api_key="AIzaSyUnaryProxy12345"))
        proxy = self.start_proxy(env, config_path=cfg)
        resp = http_raw_request(
            "127.0.0.1",
            proxy.port,
            "POST",
            "/v1beta/models/gemini-3.8-flash:generateContent",
            body=b'{"contents":[{"parts":[{"text":"ping"}]}]}',
            headers={"Content-Type": "application/json"},
        )
        self.assertEqual(resp.status, 200)
        self.assertIn("mock unary response", resp.text)
        stats = http_raw_request("127.0.0.1", proxy.port, "GET", "/api/stats").json()
        self.assertEqual(stats["requests"], 1)

    def test_t1_f5_proxies_stream_generate_content_with_sse_tee(self) -> None:
        """POST :streamGenerateContent?alt=sse passes SSE bytes untouched and tees usage."""
        env = self.make_env()
        mock = self.start_mock_gemini(
            default_usage={
                "promptTokenCount": 400,
                "candidatesTokenCount": 80,
                "cachedContentTokenCount": 200,
                "thoughtsTokenCount": 20,
            }
        )
        cfg = env.write_config(env.make_config_dict(base_url=mock.base_url, api_key="AIzaSyStreamProxy1234"))
        proxy = self.start_proxy(env, config_path=cfg)
        resp = http_raw_request(
            "127.0.0.1",
            proxy.port,
            "POST",
            "/v1beta/models/gemini-4-hydrogen:streamGenerateContent?alt=sse",
            body=b'{"contents":[]}',
            headers={"Content-Type": "application/json"},
        )
        self.assertEqual(resp.status, 200)
        self.assertIn(b"data: ", resp.body)
        _, events = poll_usage_events(env.usage_dir, min_count=1)
        self.assertEqual(len(events), 1)
        stats = poll_stats(proxy.port, min_requests=1)
        self.assertEqual(stats["requests"], 1)
        self.assertEqual(stats["prompt_tokens"], 400)
        self.assertEqual(stats["cached_tokens"], 200)
        self.assertEqual(stats["output_tokens"], 80)
        self.assertEqual(stats["reasoning_tokens"], 20)

    def test_t1_f5_routes_arbitrary_model_name_in_wildcard(self) -> None:
        """Wildcard route forwards arbitrary model identifiers verbatim."""
        env = self.make_env()
        mock = self.start_mock_gemini()
        cfg = env.write_config(env.make_config_dict(base_url=mock.base_url, api_key="AIzaSyWildcardModel12"))
        proxy = self.start_proxy(env, config_path=cfg)
        path = "/v1beta/models/gemini-3.9-pro-preview-05-06:generateContent"
        resp = http_raw_request(
            "127.0.0.1",
            proxy.port,
            "POST",
            path,
            body=b'{"contents":[]}',
            headers={"Content-Type": "application/json"},
        )
        self.assertEqual(resp.status, 200)
        req = mock.last_request()
        assert req is not None
        self.assertEqual(req.path, path)

    def test_t1_f5_config_mtime_hot_reload_applies_new_key(self) -> None:
        """Modifying config file mtime hot-reloads api_key without restarting serve."""
        env = self.make_env()
        mock = self.start_mock_gemini()
        key_v1 = "AIzaSyKeyVersionOne1111"
        key_v2 = "AIzaSyKeyVersionTwo2222"
        cfg = env.write_config(
            env.make_config_dict(base_url=mock.base_url, api_key=key_v1),
            mtime_offset_secs=-5.0,
        )
        proxy = self.start_proxy(env, config_path=cfg)

        r1 = http_raw_request(
            "127.0.0.1",
            proxy.port,
            "POST",
            "/v1beta/models/gemini-4-hydrogen:generateContent",
            body=b'{"contents":[]}',
            headers={"Content-Type": "application/json"},
        )
        self.assertEqual(r1.status, 200)
        self.assertEqual(mock.requests[0].header("x-goog-api-key"), key_v1)

        env.write_config(env.make_config_dict(base_url=mock.base_url, api_key=key_v2), path=cfg)
        IsolatedEnv.bump_mtime(cfg, delta=2.0)

        r2 = http_raw_request(
            "127.0.0.1",
            proxy.port,
            "POST",
            "/v1beta/models/gemini-4-hydrogen:generateContent",
            body=b'{"contents":[]}',
            headers={"Content-Type": "application/json"},
        )
        self.assertEqual(r2.status, 200)
        self.assertEqual(mock.requests[1].header("x-goog-api-key"), key_v2)

    def test_t1_f5_clean_shutdown_on_sigterm(self) -> None:
        """serve shuts down promptly on SIGTERM without requiring SIGKILL."""
        env = self.make_env()
        cfg = env.write_config(env.make_config_dict())
        proxy = self.start_proxy(env, config_path=cfg)
        assert proxy.proc is not None
        os.kill(proxy.proc.pid, signal.SIGTERM)
        rc = proxy.proc.wait(timeout=5.0)
        self.assertIsNotNone(rc)
        # Clean exit (0) or signal-terminated by SIGTERM (-15 / 143) are acceptable without hang.
        self.assertIn(rc, (0, -signal.SIGTERM, 128 + signal.SIGTERM))


class TestTier1F6HealthAndStats(E2ETestCase):
    TIER = 1
    FEATURE = "F6"

    def test_t1_f6_health_returns_ok_version_and_protocol_1(self) -> None:
        """GET /health returns exact {status:'ok', version:'0.1.0', protocol_version:1}."""
        env = self.make_env()
        cfg = env.write_config(env.make_config_dict())
        proxy = self.start_proxy(env, config_path=cfg)
        resp = http_raw_request("127.0.0.1", proxy.port, "GET", "/health")
        self.assertEqual(resp.status, 200)
        body = resp.json()
        self.assertEqual(body.get("status"), "ok")
        self.assertEqual(body.get("version"), "0.1.0")
        self.assertEqual(body.get("protocol_version"), 1)

    def test_t1_f6_health_sets_security_headers(self) -> None:
        """GET /health sets X-Content-Type-Options: nosniff and Cache-Control: no-store (S7)."""
        env = self.make_env()
        cfg = env.write_config(env.make_config_dict())
        proxy = self.start_proxy(env, config_path=cfg)
        resp = http_raw_request("127.0.0.1", proxy.port, "GET", "/health")
        self.assertEqual(resp.status, 200)
        self.assertEqual((resp.header("x-content-type-options") or "").lower(), "nosniff")
        self.assertIn("no-store", (resp.header("cache-control") or "").lower())

    def test_t1_f6_stats_initial_zero_state(self) -> None:
        """Fresh proxy GET /api/stats returns all zero counters and cache_hit_ratio 0.0."""
        env = self.make_env()
        cfg = env.write_config(env.make_config_dict())
        proxy = self.start_proxy(env, config_path=cfg)
        resp = http_raw_request("127.0.0.1", proxy.port, "GET", "/api/stats")
        self.assertEqual(resp.status, 200)
        stats = resp.json()
        self.assertEqual(stats["requests"], 0)
        self.assertEqual(stats["prompt_tokens"], 0)
        self.assertEqual(stats["cached_tokens"], 0)
        self.assertEqual(stats["output_tokens"], 0)
        self.assertEqual(stats["reasoning_tokens"], 0)
        self.assertEqual(stats["cache_hit_ratio"], 0.0)

    def test_t1_f6_stats_sets_security_headers(self) -> None:
        """GET /api/stats sets X-Content-Type-Options: nosniff and Cache-Control: no-store (S7)."""
        env = self.make_env()
        cfg = env.write_config(env.make_config_dict())
        proxy = self.start_proxy(env, config_path=cfg)
        resp = http_raw_request("127.0.0.1", proxy.port, "GET", "/api/stats")
        self.assertEqual(resp.status, 200)
        self.assertEqual((resp.header("x-content-type-options") or "").lower(), "nosniff")
        self.assertIn("no-store", (resp.header("cache-control") or "").lower())

    def test_t1_f6_stats_computes_exact_cache_hit_ratio(self) -> None:
        """GET /api/stats computes cache_hit_ratio = cached / prompt (0.8 for 800/1000)."""
        env = self.make_env()
        mock = self.start_mock_gemini(
            default_usage={
                "promptTokenCount": 1000,
                "cachedContentTokenCount": 800,
                "candidatesTokenCount": 150,
                "thoughtsTokenCount": 40,
            }
        )
        cfg = env.write_config(env.make_config_dict(base_url=mock.base_url, api_key="AIzaSyStatsRatio12345"))
        proxy = self.start_proxy(env, config_path=cfg)
        resp = http_raw_request(
            "127.0.0.1",
            proxy.port,
            "POST",
            "/v1beta/models/gemini-4-hydrogen:generateContent",
            body=b'{"contents":[]}',
            headers={"Content-Type": "application/json"},
        )
        self.assertEqual(resp.status, 200)
        stats = http_raw_request("127.0.0.1", proxy.port, "GET", "/api/stats").json()
        self.assertEqual(stats["requests"], 1)
        self.assertEqual(stats["prompt_tokens"], 1000)
        self.assertEqual(stats["cached_tokens"], 800)
        self.assertEqual(stats["output_tokens"], 150)
        self.assertEqual(stats["reasoning_tokens"], 40)
        self.assertAlmostEqual(stats["cache_hit_ratio"], 0.8, places=6)

    def test_t1_f6_stats_accumulates_across_multiple_requests(self) -> None:
        """GET /api/stats sums counters accurately across sequential requests."""
        env = self.make_env()
        mock = self.start_mock_gemini()
        mock.enqueue(
            JsonAction(
                status=200,
                payload={
                    "candidates": [{"content": {"parts": [{"text": "r1"}]}}],
                    "usageMetadata": {
                        "promptTokenCount": 1000,
                        "cachedContentTokenCount": 500,
                        "candidatesTokenCount": 100,
                        "thoughtsTokenCount": 20,
                    },
                },
            )
        )
        mock.enqueue(
            JsonAction(
                status=200,
                payload={
                    "candidates": [{"content": {"parts": [{"text": "r2"}]}}],
                    "usageMetadata": {
                        "promptTokenCount": 1000,
                        "cachedContentTokenCount": 1000,
                        "candidatesTokenCount": 200,
                        "thoughtsTokenCount": 30,
                    },
                },
            )
        )
        cfg = env.write_config(env.make_config_dict(base_url=mock.base_url, api_key="AIzaSyStatsAccum12345"))
        proxy = self.start_proxy(env, config_path=cfg)
        for _ in range(2):
            r = http_raw_request(
                "127.0.0.1",
                proxy.port,
                "POST",
                "/v1beta/models/gemini-4-hydrogen:generateContent",
                body=b'{"contents":[]}',
                headers={"Content-Type": "application/json"},
            )
            self.assertEqual(r.status, 200)
        stats = http_raw_request("127.0.0.1", proxy.port, "GET", "/api/stats").json()
        self.assertEqual(stats["requests"], 2)
        self.assertEqual(stats["prompt_tokens"], 2000)
        self.assertEqual(stats["cached_tokens"], 1500)
        self.assertEqual(stats["output_tokens"], 300)
        self.assertEqual(stats["reasoning_tokens"], 50)
        self.assertAlmostEqual(stats["cache_hit_ratio"], 0.75, places=6)


class TestTier1F7UiBundle(E2ETestCase):
    TIER = 1
    FEATURE = "F7"

    def test_t1_f7_single_file_dist_html_exists_and_valid(self) -> None:
        """Committed ui/dist/index.html exists, is valid UTF-8 HTML, and < 500 KB."""
        dist = self.repo_root / "ui" / "dist" / "index.html"
        self.assertTrue(dist.is_file(), f"Missing built UI bundle: {dist}")
        raw = dist.read_bytes()
        self.assertGreater(len(raw), 0)
        self.assertLess(len(raw), 500 * 1024)
        text = raw.decode("utf-8")
        self.assertTrue("<!doctype html>" in text.lower() or "<html" in text.lower())
        self.assertIn("Antigravity Companion", text)
        self.assertNotIn("AGY Companion", text)

    def test_t1_f7_ui_package_uses_expected_mcp_app_deps(self) -> None:
        """ui/package.json declares @modelcontextprotocol/ext-apps, vite-plugin-singlefile, vite, build script."""
        pkg_path = self.repo_root / "ui" / "package.json"
        self.assertTrue(pkg_path.is_file(), f"Missing {pkg_path}")
        pkg = json.loads(pkg_path.read_text(encoding="utf-8"))
        all_deps = {}
        all_deps.update(pkg.get("dependencies") or {})
        all_deps.update(pkg.get("devDependencies") or {})
        self.assertIn("@modelcontextprotocol/ext-apps", all_deps)
        self.assertIn("vite-plugin-singlefile", all_deps)
        self.assertIn("vite", all_deps)
        self.assertIn("build", pkg.get("scripts") or {})

    def test_t1_f7_ui_npmrc_points_to_public_registry(self) -> None:
        """ui/.npmrc sets public registry=https://registry.npmjs.org/."""
        npmrc = self.repo_root / "ui" / ".npmrc"
        self.assertTrue(npmrc.is_file(), f"Missing {npmrc}")
        content = npmrc.read_text(encoding="utf-8")
        self.assertIn("registry=https://registry.npmjs.org/", content)

    def test_t1_f7_ui_src_registers_handlers_before_connect(self) -> None:
        """ui/src registers ontoolinput, ontoolresult, onhostcontextchanged before app.connect()."""
        src_dir = self.repo_root / "ui" / "src"
        self.assertTrue(src_dir.is_dir(), f"Missing {src_dir}")
        ts_files = sorted(
            p
            for p in list(src_dir.rglob("*.ts")) + list(src_dir.rglob("*.tsx"))
            if p.is_file()
        )
        self.assertGreater(len(ts_files), 0, f"No .ts/.tsx files found in {src_dir}")
        stripped_by_file = {p: _strip_ts_comments(p.read_text(encoding="utf-8")) for p in ts_files}
        connect_files = [(p, txt) for p, txt in stripped_by_file.items() if ".connect(" in txt]
        self.assertGreater(len(connect_files), 0, "Expected .connect() call in ui/src/*.ts(x)")

        required_handlers = ("ontoolinput", "ontoolresult", "onhostcontextchanged")
        all_stripped = "\n".join(stripped_by_file.values())
        for handler in required_handlers:
            self.assertIn(handler, all_stripped, f"Missing handler {handler} in ui/src/*.ts(x)")

        # Prefer checking within the file calling .connect(); if split across helper modules,
        # verify every handler present in the connect file precedes .connect().
        single_file_satisfied = False
        for _, text in connect_files:
            connect_idx = text.find(".connect(")
            if all(0 <= text.find(h) < connect_idx for h in required_handlers):
                single_file_satisfied = True
                break
        if not single_file_satisfied:
            for path, text in connect_files:
                connect_idx = text.find(".connect(")
                for handler in required_handlers:
                    h_idx = text.find(handler)
                    if h_idx >= 0:
                        self.assertLess(
                            h_idx,
                            connect_idx,
                            f"{handler} must be registered before .connect() in {path.name}",
                        )

    def test_t1_f7_ui_src_uses_safe_dom_and_host_css_tokens(self) -> None:
        """ui/src uses safe DOM APIs, Antigravity CSS variables, and MCP server tool calls."""
        src_dir = self.repo_root / "ui" / "src"
        self.assertTrue(src_dir.is_dir(), f"Missing {src_dir}")
        all_src = "\n".join(
            p.read_text(encoding="utf-8")
            for p in sorted(src_dir.rglob("*"))
            if p.is_file()
        )
        self.assertTrue(
            "createElement" in all_src or "textContent" in all_src,
            "Expected safe DOM APIs (createElement / textContent) in ui/src",
        )
        self.assertTrue(
            "--color-background-" in all_src or "--color-text-" in all_src,
            "Expected Antigravity host CSS variables in ui/src",
        )
        self.assertIn("callServerTool", all_src)
        for tool in ("get_settings", "save_settings", "test_connection"):
            self.assertIn(tool, all_src, f"Expected tool call {tool!r} in ui/src")

    def test_t1_f7_ui_bundle_inlines_script_and_style(self) -> None:
        """ui/dist/index.html is self-contained with inline script/style and no relative ./assets refs."""
        dist = self.repo_root / "ui" / "dist" / "index.html"
        self.assertTrue(dist.is_file(), f"Missing {dist}")
        html = dist.read_text(encoding="utf-8")
        self.assertIn("<script", html.lower())
        self.assertIn("<style", html.lower())
        self.assertIsNone(re.search(r'<script[^>]+src=["\']\./assets/', html, re.IGNORECASE))
        self.assertIsNone(re.search(r'<link[^>]+href=["\']\./assets/', html, re.IGNORECASE))

    def test_t1_f7_ui_src_guards_stale_refresh_and_disables_refresh_during_writes(self) -> None:
        """ui/src/main.ts tracks monotonic snapshotEpoch, guards stale get_settings, and disables refresh during writes."""
        main_ts = self.repo_root / "ui" / "src" / "main.ts"
        self.assertTrue(main_ts.is_file(), f"Missing {main_ts}")
        src = _strip_ts_comments(main_ts.read_text(encoding="utf-8"))
        self.assertIn("snapshotEpoch", src, "Expected monotonic snapshotEpoch in ui/src/main.ts")
        self.assertRegex(
            src,
            r"refs\.refreshBtn\.disabled\s*=\s*!canCall\s*\|\|\s*state\.refreshing\s*\|\|\s*anyWrite",
            "refreshBtn must be disabled while save_settings or clear_api_key is in flight",
        )
        self.assertRegex(
            src,
            r"state\.snapshotEpoch\s*!==\s*epoch",
            "refreshSettings must drop stale get_settings responses when snapshotEpoch advanced",
        )
        self.assertRegex(
            src,
            r"refs\.endpointTag\.textContent\s*=\s*`\$\{snapshot\.config\.listen_host\}:\$\{snapshot\.config\.listen_port\}`",
            "endpointTag should render snapshot.config.listen_host dynamically instead of hardcoded 127.0.0.1",
        )
        self.assertRegex(
            src,
            r"if\s*\(\s*outcome\.data\.proxy\.reachable\s*\)\s*\{\s*state\.portChangedSinceSave\s*=\s*false;",
            "refreshSettings should reset portChangedSinceSave once proxy is reachable again",
        )


class TestTier1F8McpServer(E2ETestCase):
    TIER = 1
    FEATURE = "F8"

    def test_t1_f8_initialize_advertises_tools_and_resources(self) -> None:
        """initialize handshake advertises tools & resources capabilities and serverInfo.version 0.1.0."""
        env = self.make_env()
        mcp = self.start_mcp(env, auto_initialize=False)
        init_res = mcp.initialize(protocol_version="2025-03-26")
        mcp.assert_clean_stdout()
        self.assertTrue(isinstance(init_res.get("protocolVersion"), str) and init_res["protocolVersion"])
        caps = init_res.get("capabilities") or {}
        self.assertIsInstance(caps.get("tools"), dict)
        self.assertIsInstance(caps.get("resources"), dict)
        server_info = init_res.get("serverInfo") or {}
        self.assertEqual(server_info.get("name"), "agy-companion")
        self.assertEqual(server_info.get("title"), "Antigravity Companion")
        self.assertEqual(server_info.get("version"), "0.1.0")
        icons = server_info.get("icons") or []
        self.assertTrue(
            isinstance(icons, list)
            and len(icons) >= 1
            and str(icons[0].get("src", "")).startswith("data:image/svg+xml;base64,"),
            f"Expected SVG data-URI icon in serverInfo: {server_info!r}",
        )

    def test_t1_f8_resources_list_exposes_settings_html_mcp_app_mime(self) -> None:
        """resources/list exposes ui://companion/settings.html with text/html;profile=mcp-app."""
        env = self.make_env()
        mcp = self.start_mcp(env)
        res = mcp.list_resources()
        mcp.assert_clean_stdout()
        resources = res.get("resources") or []
        matches = [r for r in resources if isinstance(r, dict) and r.get("uri") == "ui://companion/settings.html"]
        self.assertEqual(len(matches), 1, f"Expected settings resource in {resources!r}")
        self.assertEqual(matches[0].get("mimeType"), "text/html;profile=mcp-app")

    def test_t1_f8_resources_read_returns_embedded_html(self) -> None:
        """resources/read serves embedded HTML with MCP App MIME type and size < 10 MiB."""
        env = self.make_env()
        mcp = self.start_mcp(env)
        res = mcp.read_resource("ui://companion/settings.html")
        mcp.assert_clean_stdout()
        contents = res.get("contents") or []
        self.assertGreaterEqual(len(contents), 1)
        entry = contents[0]
        self.assertEqual(entry.get("uri"), "ui://companion/settings.html")
        self.assertEqual(entry.get("mimeType"), "text/html;profile=mcp-app")
        text = entry.get("text", "")
        self.assertTrue(isinstance(text, str) and 0 < len(text.encode("utf-8")) < 10 * 1024 * 1024)
        self.assertIn("<html", text.lower())

    def test_t1_f8_tools_list_exposes_four_tools_with_correct_meta(self) -> None:
        """tools/list exposes exact 5 tools with required MCP App & Antigravity _meta."""
        env = self.make_env()
        mcp = self.start_mcp(env, auto_initialize=False)
        init_res = mcp.initialize(protocol_version="2025-03-26")
        server_info = init_res.get("serverInfo") or {}
        self.assertEqual(server_info.get("name"), "agy-companion")
        self.assertEqual(server_info.get("title"), "Antigravity Companion")

        res = mcp.list_tools()
        mcp.assert_clean_stdout()
        tools = {t["name"]: t for t in (res.get("tools") or []) if isinstance(t, dict) and "name" in t}
        self.assertEqual(
            set(tools.keys()),
            {
                "open_settings",
                "get_settings",
                "save_settings",
                "test_connection",
                "set_launcher_integration",
            },
        )

        open_meta = tools["open_settings"].get("_meta") or {}
        self.assertEqual((open_meta.get("ui") or {}).get("resourceUri"), "ui://companion/settings.html")
        agy_ui = open_meta.get("com.google.antigravity/ui") or {}
        entrypoints = [ep for ep in (agy_ui.get("entrypoints") or []) if isinstance(ep, dict)]
        ep_types = {ep.get("type") for ep in entrypoints}
        self.assertTrue({"aux_pane", "global"}.issubset(ep_types), f"Missing entrypoints in {agy_ui!r}")
        for ep in entrypoints:
            self.assertEqual(
                ep.get("title"),
                "Antigravity Companion",
                f"Expected entrypoint title 'Antigravity Companion' in {ep!r}",
            )

        for app_tool in (
            "get_settings",
            "save_settings",
            "test_connection",
            "set_launcher_integration",
        ):
            meta = tools[app_tool].get("_meta") or {}
            self.assertEqual((meta.get("ui") or {}).get("visibility"), ["app"], f"Bad _meta for {app_tool}: {meta!r}")

    def test_t1_f8_open_settings_accepts_empty_args_and_returns_dual_content(self) -> None:
        """open_settings({}) succeeds returning both plain-text content and structuredContent."""
        env = self.make_env()
        mcp = self.start_mcp(env)
        res = mcp.call_tool("open_settings", {})
        mcp.assert_clean_stdout()
        self.assertFalse(res.get("isError", False))
        self.assertTrue(_text_blocks(res).strip(), f"Expected non-empty content text in {res!r}")
        sc = res.get("structuredContent")
        self.assertIsInstance(sc, dict)
        self.assertGreater(len(sc), 0)

    def test_t1_f8_save_and_get_settings_roundtrip(self) -> None:
        """save_settings followed by get_settings persists and returns updated redacted config."""
        env = self.make_env()
        mcp = self.start_mcp(env)
        mcp.save_settings_smart(
            default_model="gemini-3.8-flash",
            base_url="https://generativelanguage.googleapis.com",
            api_key="AIzaSyRoundTripTest1234",
        )
        res = mcp.call_tool("get_settings", {})
        mcp.assert_clean_stdout()
        self.assertNotIn("AIzaSyRoundTripTest1234", json.dumps(res))
        sc = res.get("structuredContent", {})
        found_m, model = _find_any_key(sc, ("gemini_default_model", "default_model"))
        found_c, configured = find_key_recursive(sc, "api_key_configured")
        found_h, hint = find_key_recursive(sc, "api_key_hint")
        self.assertTrue(found_m and model == "gemini-3.8-flash", f"Bad model in {sc!r}")
        self.assertTrue(found_c and configured is True, f"Bad configured in {sc!r}")
        self.assertTrue(found_h and hint == "AIza...1234", f"Bad hint in {sc!r}")


class TestTier1F9CliBinary(E2ETestCase):
    TIER = 1
    FEATURE = "F9"

    def test_t1_f9_version_flag_prints_expected_semver(self) -> None:
        """agy-companion --version prints exact 'agy-companion 0.1.0'."""
        binary = self.require_binary()
        env = self.make_env()
        res = run_cli([str(binary), "--version"], env=env.build_env())
        self.assertEqual(res.returncode, 0, res.stderr)
        self.assertEqual(res.stdout.strip(), "agy-companion 0.1.0")

    def test_t1_f9_help_lists_all_four_subcommands(self) -> None:
        """agy-companion --help advertises serve, ui-mcp, doctor, and models."""
        binary = self.require_binary()
        env = self.make_env()
        res = run_cli([str(binary), "--help"], env=env.build_env())
        self.assertEqual(res.returncode, 0, res.stderr)
        for subcmd in ("serve", "ui-mcp", "doctor", "models"):
            self.assertIn(subcmd, res.stdout)

    def test_t1_f9_models_json_emits_valid_custom_models_config(self) -> None:
        """agy-companion models --json emits valid JSON pointing to 127.0.0.1:4141."""
        binary = self.require_binary()
        env = self.make_env()
        res = run_cli([str(binary), "models", "--json"], env=env.build_env())
        self.assertEqual(res.returncode, 0, res.stderr)
        data = res.json_stdout()
        models = extract_models_list(data)
        self.assertGreater(len(models), 0, f"Expected customModels entries in {data!r}")
        self.assertTrue(
            any(
                "gemini-api://http://127.0.0.1:4141/models/" in str(m.get("modelName", ""))
                for m in models
            ),
            f"Expected gemini-api://http://127.0.0.1:4141/models/ in {models!r}",
        )
        for m in models:
            label = str(m.get("label", ""))
            self.assertTrue(
                label.startswith("Antigravity Companion ("),
                f"Expected label starting with 'Antigravity Companion (', got {label!r}",
            )
            self.assertNotIn("AGY Companion", label)

    def test_t1_f9_models_human_output_succeeds(self) -> None:
        """agy-companion models prints human-readable output referencing 127.0.0.1:4141 and gemini-api://."""
        binary = self.require_binary()
        env = self.make_env()
        res = run_cli([str(binary), "models"], env=env.build_env())
        self.assertEqual(res.returncode, 0, res.stderr)
        self.assertIn("127.0.0.1:4141", res.stdout)
        self.assertIn("gemini-api://", res.stdout)

    def test_t1_f9_serve_accepts_port_and_config_flags(self) -> None:
        """agy-companion serve --port <p> --config <c> serves /health 200 OK."""
        env = self.make_env()
        cfg = env.write_config(env.make_config_dict())
        port = find_free_port()
        proxy = self.start_proxy(env, config_path=cfg, port=port)
        resp = http_raw_request("127.0.0.1", proxy.port, "GET", "/health")
        self.assertEqual(resp.status, 200)
        self.assertEqual(resp.json().get("version"), "0.1.0")

    def test_t1_f9_doctor_succeeds_when_healthy(self) -> None:
        """agy-companion doctor exits 0 with 0600 config, live proxy, and env vars set (Guard A8)."""
        binary = self.require_binary()
        env = self.make_env()
        port = find_free_port()
        cfg = env.write_config(
            env.make_config_dict(listen_port=port, api_key="AIzaSyDoctorHealthy123"),
            mode=0o600,
        )
        proxy = self.start_proxy(env, config_path=cfg, port=port)
        doc_env = env.build_env(
            extra={
                "GEMINI_API_KEY": "AIzaSyDoctorHealthy123",
                "GOOGLE_GEMINI_BASE_URL": f"http://127.0.0.1:{proxy.port}",
            }
        )
        res = run_cli(
            [str(binary), "doctor", "--config", str(cfg), "--port", str(proxy.port)],
            env=doc_env,
        )
        self.assertEqual(res.returncode, 0, f"doctor failed:\nstdout:\n{res.stdout}\nstderr:\n{res.stderr}")
        self.assertNotIn("AIzaSyDoctorHealthy123", res.stdout)
        self.assertNotIn("AIzaSyDoctorHealthy123", res.stderr)


class TestTier1F10PluginBundle(E2ETestCase):
    TIER = 1
    FEATURE = "F10"

    def test_t1_f10_agy_plugin_validate_strict_passes(self) -> None:
        """agy plugin validate --strict --json ./plugin exits 0 with zero findings (0 errors, warnings, notes)."""
        plugin_dir = self.repo_root / "plugin"
        self.assertTrue(plugin_dir.is_dir(), f"Missing plugin dir: {plugin_dir}")
        res = run_cli(["agy", "plugin", "validate", "--strict", "--json", str(plugin_dir)])
        self.assertEqual(res.returncode, 0, f"agy plugin validate failed:\n{res.stdout}\n{res.stderr}")
        payload = res.json_stdout()
        self.assertIsInstance(payload, dict)
        findings = payload.get("findings")
        self.assertEqual(findings, [], f"Expected zero plugin validation findings, got: {findings!r}")

    def test_t1_f10_plugin_json_metadata_and_seo_fields(self) -> None:
        """plugin/plugin.json matches v1 schema, metadata, bundled SVG logo, 1..3 suggestedPrompts, SEO keywords."""
        pj_path = self.repo_root / "plugin" / "plugin.json"
        self.assertTrue(pj_path.is_file(), f"Missing {pj_path}")
        pj = json.loads(pj_path.read_text(encoding="utf-8"))
        self.assertEqual(pj.get("$schema"), "https://antigravity.google/schemas/v1/plugin.json")
        self.assertEqual(pj.get("name"), "agy-companion")
        self.assertEqual(pj.get("displayName"), "Antigravity Companion")
        self.assertEqual(pj.get("logo"), "assets/logo.svg")
        logo_path = self.repo_root / "plugin" / "assets" / "logo.svg"
        self.assertFalse(logo_path.is_symlink(), f"Logo must not be a symlink: {logo_path}")
        self.assertTrue(logo_path.is_file(), f"Missing logo asset: {logo_path}")
        logo_text = logo_path.read_text(encoding="utf-8")
        self.assertGreater(len(logo_text.strip()), 0)
        self.assertIn("<svg", logo_text.lower())
        self.assertIsNone(re.search(r"<\s*script\b", logo_text, re.IGNORECASE))
        self.assertEqual(pj.get("version"), "0.1.0")
        self.assertEqual(pj.get("license"), "Apache-2.0")
        prompts = pj.get("suggestedPrompts") or []
        self.assertTrue(1 <= len(prompts) <= 3, f"Expected 1..3 suggestedPrompts, got {prompts!r}")
        keywords = {str(k).lower() for k in (pj.get("keywords") or [])}
        for kw in ("antigravity", "agy", "byom", "gemini"):
            self.assertIn(kw, keywords)

    def test_t1_f10_mcp_config_launches_ui_mcp_via_bash_args(self) -> None:
        """plugin/mcp_config.json uses command='bash' with PLUGIN_ROOT script arg and ui-mcp."""
        mc_path = self.repo_root / "plugin" / "mcp_config.json"
        self.assertTrue(mc_path.is_file(), f"Missing {mc_path}")
        mc = json.loads(mc_path.read_text(encoding="utf-8"))
        servers = mc.get("mcpServers") or {}
        self.assertIn("hub", servers)
        hub = servers["hub"]
        self.assertEqual(hub.get("command"), "bash")
        args = hub.get("args") or []
        self.assertIn("${PLUGIN_ROOT}/scripts/run-companion.sh", args)
        self.assertIn("ui-mcp", args)
        self.assertTrue(
            any("${PLUGIN_DATA}" in str(a) for a in args),
            f"Expected --data-dir=${{PLUGIN_DATA}} in {args!r}",
        )

    def test_t1_f10_sidecar_json_launches_serve_on_failure(self) -> None:
        """plugin/sidecars/proxy/sidecar.json invokes run-companion.sh serve with on-failure policy (Guard A15)."""
        sc_path = self.repo_root / "plugin" / "sidecars" / "proxy" / "sidecar.json"
        self.assertTrue(sc_path.is_file(), f"Missing {sc_path}")
        sc = json.loads(sc_path.read_text(encoding="utf-8"))
        self.assertEqual(sc.get("restart_policy"), "on-failure")
        self.assertEqual(sc.get("display_name"), "Antigravity Companion Proxy")
        combined = [str(sc.get("command", ""))] + [str(a) for a in (sc.get("args") or [])]
        self.assertTrue(
            any("../../scripts/run-companion.sh" in tok for tok in combined),
            f"Expected ../../scripts/run-companion.sh in {combined!r}",
        )
        self.assertIn("serve", combined)

    def test_t1_f10_skill_md_frontmatter_and_readme_present(self) -> None:
        """plugin/skills/companion/SKILL.md and plugin/README.md exist with valid frontmatter."""
        skill_path = self.repo_root / "plugin" / "skills" / "companion" / "SKILL.md"
        readme_path = self.repo_root / "plugin" / "README.md"
        self.assertTrue(skill_path.is_file(), f"Missing {skill_path}")
        self.assertTrue(readme_path.is_file(), f"Missing {readme_path}")
        self.assertGreater(len(readme_path.read_text(encoding="utf-8").strip()), 0)

        text = skill_path.read_text(encoding="utf-8")
        self.assertTrue(text.startswith("---\n") or text.startswith("---\r\n"))
        parts = text.split("---", 2)
        self.assertGreaterEqual(len(parts), 3)
        frontmatter = parts[1]
        self.assertRegex(frontmatter, r"(?m)^name:\s*companion\s*$")
        self.assertRegex(frontmatter, r"(?m)^description:\s*\S+")
        self.assertTrue("doctor" in text.lower() or "settings" in text.lower())

    def test_t1_f10_run_companion_sh_invokes_binary_version(self) -> None:
        """plugin/scripts/run-companion.sh execs AGY_COMPANION_BIN --version cleanly."""
        binary = self.require_binary()
        script = self.repo_root / "plugin" / "scripts" / "run-companion.sh"
        self.assertTrue(script.is_file(), f"Missing {script}")
        env = self.make_env()
        proc_env = env.build_env(
            extra={
                "AGY_COMPANION_BIN": str(binary),
                "PATH": "/usr/bin:/bin",
            }
        )
        res = run_cli(["bash", str(script), "--version"], env=proc_env)
        self.assertEqual(res.returncode, 0, res.stderr)
        self.assertEqual(res.stdout.strip(), "agy-companion 0.1.0")


class TestTier1F11PackagingShims(E2ETestCase):
    TIER = 1
    FEATURE = "F11"

    def test_t1_f11_npm_package_json_bin_and_optional_deps(self) -> None:
        """packages/npm/agy-companion/package.json defines agy-companion/agyc bins, public publishConfig, and 5 optionalDeps."""
        pkg_path = self.repo_root / "packages" / "npm" / "agy-companion" / "package.json"
        self.assertTrue(pkg_path.is_file(), f"Missing {pkg_path}")
        pkg = json.loads(pkg_path.read_text(encoding="utf-8"))
        self.assertEqual(pkg.get("name"), "agy-companion")
        self.assertEqual(pkg.get("version"), "0.1.0")
        self.assertEqual(
            pkg.get("publishConfig"),
            {"registry": "https://registry.npmjs.org/", "access": "public"},
        )
        bins = pkg.get("bin") or {}
        self.assertEqual(bins.get("agy-companion"), "bin/agy-companion.js")
        self.assertEqual(bins.get("agyc"), "bin/agy-companion.js")
        opt_deps = pkg.get("optionalDependencies") or {}
        expected = {
            "@agy-companion/darwin-arm64": "0.1.0",
            "@agy-companion/darwin-x64": "0.1.0",
            "@agy-companion/linux-x64": "0.1.0",
            "@agy-companion/linux-arm64": "0.1.0",
            "@agy-companion/win32-x64": "0.1.0",
        }
        for name, ver in expected.items():
            self.assertEqual(opt_deps.get(name), ver, f"Mismatch for {name} in {opt_deps!r}")

    def test_t1_f11_npm_js_files_pass_node_syntax_check(self) -> None:
        """node --check succeeds on bin/agy-companion.js and generate-platform-pkgs.mjs."""
        bin_js = self.repo_root / "packages" / "npm" / "agy-companion" / "bin" / "agy-companion.js"
        gen_mjs = self.repo_root / "packages" / "npm" / "generate-platform-pkgs.mjs"
        for fpath in (bin_js, gen_mjs):
            self.assertTrue(fpath.is_file(), f"Missing {fpath}")
            res = run_cli(["node", "--check", str(fpath)])
            self.assertEqual(res.returncode, 0, f"node --check failed for {fpath}: {res.stderr}")
            self.assertEqual(res.stderr.strip(), "")

    def test_t1_f11_generate_platform_pkgs_outputs_5_valid_manifests(self) -> None:
        """generate-platform-pkgs.mjs generates 5 valid platform package.json files into a temp dir."""
        gen_mjs = self.repo_root / "packages" / "npm" / "generate-platform-pkgs.mjs"
        self.assertTrue(gen_mjs.is_file(), f"Missing {gen_mjs}")
        with tempfile.TemporaryDirectory(prefix="agy-npm-gen-") as tmpdir:
            base_dir = Path(tmpdir)
            work_dir = base_dir / "work"
            out_dir = base_dir / "pkgs"
            work_dir.mkdir(parents=True, exist_ok=True)
            out_dir.mkdir(parents=True, exist_ok=True)
            res = run_cli(["node", str(gen_mjs), "--out-dir", str(out_dir)], cwd=work_dir)
            manifests = sorted(out_dir.rglob("package.json"))
            if res.returncode != 0 or not manifests:
                for d in (work_dir, out_dir):
                    for child in list(d.iterdir()):
                        if child.is_dir():
                            shutil.rmtree(child)
                        else:
                            child.unlink()
                res = run_cli(["node", str(gen_mjs), str(out_dir)], cwd=work_dir)
                manifests = sorted(out_dir.rglob("package.json"))
            self.assertEqual(res.returncode, 0, f"Generator failed: {res.stdout}\n{res.stderr}")
            self.assertGreaterEqual(len(manifests), 5, f"Expected 5 manifests in {out_dir}, got {manifests}")
            found_names: set[str] = set()
            for mf in manifests:
                data = json.loads(mf.read_text(encoding="utf-8"))
                name = data.get("name", "")
                if str(name).startswith("@agy-companion/"):
                    found_names.add(str(name))
                    self.assertEqual(data.get("version"), "0.1.0")
                    self.assertEqual(
                        data.get("publishConfig"),
                        {"registry": "https://registry.npmjs.org/", "access": "public"},
                        f"{name} missing expected public publishConfig: {data.get('publishConfig')!r}",
                    )
                    self.assertTrue(isinstance(data.get("os"), list) and len(data["os"]) >= 1)
                    self.assertTrue(isinstance(data.get("cpu"), list) and len(data["cpu"]) >= 1)
            self.assertEqual(
                found_names,
                {
                    "@agy-companion/darwin-arm64",
                    "@agy-companion/darwin-x64",
                    "@agy-companion/linux-x64",
                    "@agy-companion/linux-arm64",
                    "@agy-companion/win32-x64",
                },
            )

    def test_t1_f11_python_pyproject_maturin_bin_config(self) -> None:
        """packages/python/pyproject.toml configures maturin bindings='bin' and version 0.1.0."""
        py_toml = self.repo_root / "packages" / "python" / "pyproject.toml"
        self.assertTrue(py_toml.is_file(), f"Missing {py_toml}")
        data = tomllib.loads(py_toml.read_text(encoding="utf-8"))
        self.assertEqual(data.get("build-system", {}).get("build-backend"), "maturin")
        self.assertEqual(data.get("project", {}).get("name"), "agy-companion")
        self.assertEqual(data.get("project", {}).get("version"), "0.1.0")
        maturin = data.get("tool", {}).get("maturin", {})
        self.assertEqual(maturin.get("bindings"), "bin")
        self.assertIn("crates/companion-cli/Cargo.toml", str(maturin.get("manifest-path", "")))

    def test_t1_f11_python_shim_exports_find_binary(self) -> None:
        """packages/python/python/agy_companion/__init__.py imports cleanly and exports find_binary."""
        init_py = self.repo_root / "packages" / "python" / "python" / "agy_companion" / "__init__.py"
        self.assertTrue(init_py.is_file(), f"Missing {init_py}")
        spec = importlib.util.spec_from_file_location("_e2e_agy_companion_shim", init_py)
        self.assertIsNotNone(spec)
        assert spec is not None and spec.loader is not None
        module = importlib.util.module_from_spec(spec)
        spec.loader.exec_module(module)
        self.assertTrue(callable(getattr(module, "find_binary", None)))

    def test_t1_f11_four_way_version_sync_matches_cargo_toml(self) -> None:
        """Cargo.toml, plugin.json, npm package.json, and pyproject.toml all report 0.1.0."""
        v_cargo = _extract_cargo_workspace_version(self.repo_root)
        v_plugin = json.loads((self.repo_root / "plugin" / "plugin.json").read_text(encoding="utf-8"))["version"]
        v_npm = json.loads(
            (self.repo_root / "packages" / "npm" / "agy-companion" / "package.json").read_text(encoding="utf-8")
        )["version"]
        v_py = tomllib.loads(
            (self.repo_root / "packages" / "python" / "pyproject.toml").read_text(encoding="utf-8")
        )["project"]["version"]
        self.assertEqual(v_cargo, "0.1.0")
        self.assertEqual({v_cargo, v_plugin, v_npm, v_py}, {"0.1.0"})


class TestTier1F12AgentCheckAndCi(E2ETestCase):
    TIER = 1
    FEATURE = "F12"

    def test_t1_f12_agent_check_script_passes_on_repo(self) -> None:
        """scripts/agent_check.sh exits 0 with PASS output and zero FAIL lines."""
        script = self.repo_root / "scripts" / "agent_check.sh"
        self.assertTrue(script.is_file(), f"Missing {script}")
        res = run_cli([str(script)], cwd=self.repo_root, timeout=180.0)
        self.assertEqual(res.returncode, 0, f"agent_check.sh failed:\n{res.stdout}\n{res.stderr}")
        self.assertIn("PASS", res.stdout)
        fail_lines = [ln for ln in res.stdout.splitlines() if ln.strip().startswith("FAIL")]
        self.assertEqual(fail_lines, [], f"Unexpected FAIL lines: {fail_lines!r}")

    def test_t1_f12_agent_check_reports_all_7_gates(self) -> None:
        """scripts/agent_check.sh runs and passes all 7 verification gates."""
        script = self.repo_root / "scripts" / "agent_check.sh"
        res = run_cli([str(script)], cwd=self.repo_root, timeout=180.0)
        self.assertEqual(res.returncode, 0, f"agent_check.sh failed:\n{res.stdout}\n{res.stderr}")
        pass_lines = [ln for ln in res.stdout.splitlines() if "PASS" in ln]
        self.assertGreaterEqual(len(pass_lines), 7, f"Expected >= 7 PASS gates, got:\n{res.stdout}")

    def test_t1_f12_ci_yml_invokes_agent_check(self) -> None:
        """.github/workflows/ci.yml triggers on push/pull_request to main and invokes ./scripts/agent_check.sh."""
        ci_yml = self.repo_root / ".github" / "workflows" / "ci.yml"
        self.assertTrue(ci_yml.is_file(), f"Missing {ci_yml}")
        text = ci_yml.read_text(encoding="utf-8")
        self.assertIn("./scripts/agent_check.sh", text)
        self.assertIn("pull_request", text)
        self.assertIn("push", text)
        self.assertIn("main", text)

    def test_t1_f12_governance_files_present_and_native(self) -> None:
        """Required Antigravity-native governance files exist and GEMINI.md is absent."""
        for rel in (
            "AGENTS.md",
            "LICENSE",
            ".agents/rules/workflow.md",
            ".github/PULL_REQUEST_TEMPLATE.md",
        ):
            self.assertTrue((self.repo_root / rel).is_file(), f"Missing governance file: {rel}")
        self.assertFalse((self.repo_root / "GEMINI.md").exists(), "Forbidden GEMINI.md exists at repo root")

    def test_t1_f12_gitignore_excludes_target_and_whitelists_ui_bundle(self) -> None:
        """.gitignore ignores target/ and explicitly un-ignores !ui/dist/index.html."""
        gi = self.repo_root / ".gitignore"
        self.assertTrue(gi.is_file(), f"Missing {gi}")
        lines = [ln.strip() for ln in gi.read_text(encoding="utf-8").splitlines()]
        self.assertIn("target/", lines)
        self.assertIn("!ui/dist/index.html", lines)

    def test_t1_f12_git_tracked_files_have_zero_secrets_or_junk(self) -> None:
        """git ls-files contains zero forbidden secrets or runtime junk files."""
        res = run_cli(["git", "-C", str(self.repo_root), "ls-files"])
        self.assertEqual(res.returncode, 0, res.stderr)
        forbidden = re.compile(
            r"(^|/)\.DS_Store$|(^|/)\.env($|\.)|(^|/)\.antigravitycli/|(^|/)\.remember/|\.(pem|key)$"
        )
        offenders = [
            f
            for f in res.stdout.splitlines()
            if f and not f.endswith(".env.example") and forbidden.search(f)
        ]
        self.assertEqual(offenders, [], f"Forbidden tracked files: {offenders!r}")


class TestTier1F14AgentDocs(E2ETestCase):
    TIER = 1
    FEATURE = "F14"

    def test_t1_f14_agents_md_documents_m1_layout_and_gates(self) -> None:
        """AGENTS.md documents M1 workspace layout and verification gates while GEMINI.md is absent."""
        agents_md = self.repo_root / "AGENTS.md"
        self.assertTrue(agents_md.is_file(), f"Missing {agents_md}")
        self.assertFalse((self.repo_root / "GEMINI.md").exists(), "Forbidden GEMINI.md exists at repo root")
        text = agents_md.read_text(encoding="utf-8")
        for path_token in ("crates/", "ui/", "plugin/", "packages/"):
            self.assertIn(path_token, text, f"AGENTS.md missing workspace layout entry {path_token!r}")
        for gate_token in ("scripts/agent_check.sh", "cargo test", "agy plugin validate"):
            self.assertIn(gate_token, text, f"AGENTS.md missing verification gate {gate_token!r}")

    def test_t1_f14_local_dev_skill_frontmatter_and_reload_loop(self) -> None:
        """.agents/skills/local-dev/SKILL.md has valid frontmatter and documents Antigravity App reload loop."""
        skill_md = self.repo_root / ".agents" / "skills" / "local-dev" / "SKILL.md"
        self.assertTrue(skill_md.is_file(), f"Missing {skill_md}")
        text = skill_md.read_text(encoding="utf-8")
        self.assertGreater(len(text.strip()), 0)
        self.assertTrue(text.startswith("---\n") or text.startswith("---\r\n"))
        parts = text.split("---", 2)
        self.assertGreaterEqual(len(parts), 3)
        frontmatter = parts[1]
        self.assertRegex(frontmatter, r"(?m)^name:\s*local-dev\s*$")
        self.assertRegex(frontmatter, r"(?m)^description:\s*\S+")
        lower = text.lower()
        self.assertTrue(
            "refreshmcpservers" in lower or ("mcp" in lower and "restart" in lower),
            "local-dev SKILL.md should document RefreshMcpServers or MCP restart loop",
        )
        self.assertIn("sidecar", lower)
        self.assertIn("agy", lower)


class TestTier1F15LocalDevWorkflow(E2ETestCase):
    TIER = 1
    FEATURE = "F15"

    def test_t1_f15_dev_install_script_exists_executable_and_valid_syntax(self) -> None:
        """scripts/dev-install.sh exists, has executable bit set, and passes bash -n."""
        script = self.repo_root / "scripts" / "dev-install.sh"
        self.assertTrue(script.is_file(), f"Missing {script}")
        mode = stat.S_IMODE(os.stat(script).st_mode)
        self.assertTrue(mode & 0o111, f"scripts/dev-install.sh is not executable: {oct(mode)}")
        res = run_cli(["bash", "-n", str(script)])
        self.assertEqual(res.returncode, 0, f"bash -n failed for dev-install.sh: {res.stderr}")

    def test_t1_f15_dev_install_dry_run_install_and_uninstall_lifecycle(self) -> None:
        """scripts/dev-install.sh handles --dry-run, --no-build link creation, and idempotent --uninstall."""
        script = self.repo_root / "scripts" / "dev-install.sh"
        self.assertTrue(script.is_file(), f"Missing {script}")
        env = self.make_env()
        plugins_dir = env.root / "dev_plugins"
        link_path = plugins_dir / "agy-companion"
        expected_target = (self.repo_root / "plugin").resolve()
        proc_env = env.build_env()

        # 1. --dry-run --no-build reports planned actions without creating directories or symlinks.
        r_dry = run_cli(
            [str(script), "--no-build", "--dry-run", "--plugins-dir", str(plugins_dir)],
            env=proc_env,
        )
        self.assertEqual(r_dry.returncode, 0, f"dry-run failed:\n{r_dry.stdout}\n{r_dry.stderr}")
        self.assertIn("[dry-run]", r_dry.stdout)
        self.assertFalse(link_path.exists() or link_path.is_symlink())
        self.assertFalse(plugins_dir.exists())

        # 2. --no-build creates plugins dir and symlink pointing to ./plugin.
        r_inst = run_cli(
            [str(script), "--no-build", "--plugins-dir", str(plugins_dir)],
            env=proc_env,
        )
        self.assertEqual(r_inst.returncode, 0, f"install failed:\n{r_inst.stdout}\n{r_inst.stderr}")
        self.assertTrue(link_path.is_symlink(), f"Expected symlink at {link_path}")
        self.assertEqual(link_path.resolve(), expected_target)

        # 3. --uninstall --dry-run preserves the symlink.
        r_udry = run_cli(
            [str(script), "--uninstall", "--dry-run", "--plugins-dir", str(plugins_dir)],
            env=proc_env,
        )
        self.assertEqual(r_udry.returncode, 0, f"uninstall dry-run failed:\n{r_udry.stdout}\n{r_udry.stderr}")
        self.assertIn("[dry-run] rm --", r_udry.stdout)
        self.assertTrue(link_path.is_symlink())

        # 4. --uninstall removes the symlink cleanly.
        r_uninst = run_cli(
            [str(script), "--uninstall", "--plugins-dir", str(plugins_dir)],
            env=proc_env,
        )
        self.assertEqual(r_uninst.returncode, 0, f"uninstall failed:\n{r_uninst.stdout}\n{r_uninst.stderr}")
        self.assertFalse(link_path.exists() or link_path.is_symlink())

        # 5. Repeated --uninstall is idempotent and exits 0.
        r_idem = run_cli(
            [str(script), "--uninstall", "--plugins-dir", str(plugins_dir)],
            env=proc_env,
        )
        self.assertEqual(r_idem.returncode, 0, f"idempotent uninstall failed:\n{r_idem.stdout}\n{r_idem.stderr}")
        self.assertIn("Nothing to uninstall", r_idem.stdout)


class TestTier1F16XdgPaths(E2ETestCase):
    TIER = 1
    FEATURE = "F16"

    def test_t1_f16_xdg_config_home_honored_across_cli_and_mcp(self) -> None:
        """XDG_CONFIG_HOME is honored by models, serve, and ui-mcp when --config and AGY_COMPANION_CONFIG are unset."""
        binary = self.require_binary()
        env = self.make_env()
        mock = self.start_mock_gemini(expected_api_key="AIzaSyXdgConfigTest12")
        custom_xdg_cfg = env.root / "custom_xdg_cfg"
        xdg_cfg_file = custom_xdg_cfg / "agy-companion" / "config.json"
        port = find_free_port()
        env.write_config(
            env.make_config_dict(
                listen_port=port,
                base_url=mock.base_url,
                api_key="AIzaSyXdgConfigTest12",
                default_model="gemini-3.8-flash",
            ),
            path=xdg_cfg_file,
        )

        xdg_env = env.build_env(
            extra={"XDG_CONFIG_HOME": str(custom_xdg_cfg)},
            set_config_env=False,
        )

        # CLI `models --json` loads listen_port & default_model from XDG_CONFIG_HOME.
        r_models = run_cli([str(binary), "models", "--json"], env=xdg_env)
        self.assertEqual(r_models.returncode, 0, r_models.stderr)
        models = extract_models_list(r_models.json_stdout())
        self.assertTrue(
            any(f"127.0.0.1:{port}/models/gemini-3.8-flash" in str(m.get("modelName", "")) for m in models),
            f"Expected XDG port {port} and model in models output: {models!r}",
        )

        # `serve` without --config or --port binds XDG config port and forwards to mock.
        proxy = self.start_proxy(
            env,
            port=port,
            extra_env={"XDG_CONFIG_HOME": str(custom_xdg_cfg)},
            pass_port_flag=False,
            pass_config_flag=False,
            set_config_env=False,
        )
        resp = http_raw_request(
            "127.0.0.1",
            proxy.port,
            "POST",
            "/v1beta/models/gemini-3.8-flash:generateContent",
            body=b'{"contents":[]}',
            headers={"Content-Type": "application/json"},
        )
        self.assertEqual(resp.status, 200, resp.text)
        req = mock.last_request()
        self.assertIsNotNone(req)
        assert req is not None
        self.assertEqual(req.header("x-goog-api-key"), "AIzaSyXdgConfigTest12")

        # `ui-mcp` without --config loads the same XDG config (redacted).
        mcp = self.start_mcp(
            env,
            extra_env={"XDG_CONFIG_HOME": str(custom_xdg_cfg)},
            pass_config_flag=False,
            set_config_env=False,
        )
        get_res = mcp.call_tool("get_settings", {})
        mcp.assert_clean_stdout()
        sc = get_res.get("structuredContent", {})
        found_p, listen_port = find_key_recursive(sc, "listen_port")
        found_m, model = _find_any_key(sc, ("gemini_default_model", "default_model"))
        found_h, hint = find_key_recursive(sc, "api_key_hint")
        self.assertTrue(found_p and listen_port == port, f"Bad listen_port in {sc!r}")
        self.assertTrue(found_m and model == "gemini-3.8-flash", f"Bad model in {sc!r}")
        self.assertTrue(found_h and hint == "AIza...st12", f"Bad hint in {sc!r}")

    def test_t1_f16_xdg_data_home_honored_when_usage_dir_omitted(self) -> None:
        """XDG_DATA_HOME/agy-companion/usage receives JSONL usage logs when config.usage_dir is omitted."""
        env = self.make_env()
        mock = self.start_mock_gemini(
            default_usage={
                "promptTokenCount": 420,
                "cachedContentTokenCount": 120,
                "candidatesTokenCount": 60,
                "thoughtsTokenCount": 10,
            }
        )
        custom_xdg_data = env.root / "custom_xdg_data"
        expected_usage_dir = custom_xdg_data / "agy-companion" / "usage"
        cfg = env.write_config(
            env.make_config_dict(
                base_url=mock.base_url,
                api_key="AIzaSyXdgDataHome1234",
                include_usage_dir=False,
            )
        )
        self.assertNotIn("usage_dir", json.loads(cfg.read_text(encoding="utf-8")))

        proxy = self.start_proxy(
            env,
            config_path=cfg,
            extra_env={"XDG_DATA_HOME": str(custom_xdg_data)},
        )
        resp = http_raw_request(
            "127.0.0.1",
            proxy.port,
            "POST",
            "/v1beta/models/gemini-4-hydrogen:generateContent",
            body=b'{"contents":[]}',
            headers={"Content-Type": "application/json"},
        )
        self.assertEqual(resp.status, 200, resp.text)

        files, events = poll_usage_events(expected_usage_dir, min_count=1, timeout=3.0)
        self.assertGreaterEqual(len(files), 1, f"Expected JSONL under {expected_usage_dir}")
        self.assertEqual(len(events), 1)
        self.assertEqual(events[0]["input_tokens"], 420)
        self.assertEqual(events[0]["cached_tokens"], 120)
        self.assertEqual(stat.S_IMODE(os.stat(files[0]).st_mode), 0o600)
        # Default IsolatedEnv usage_dir and $HOME/.local/share must remain untouched.
        self.assertEqual(env.usage_files(), [])
        self.assertEqual(env.usage_files(env.home_dir / ".local" / "share" / "agy-companion" / "usage"), [])

    def test_t1_f16_config_precedence_cli_env_xdg_home(self) -> None:
        """Config path precedence strictly follows: --config > AGY_COMPANION_CONFIG > XDG_CONFIG_HOME > $HOME/.config."""
        env = self.make_env()
        cli_cfg = env.root / "prec_cli" / "config.json"
        env_cfg = env.root / "prec_env" / "config.json"
        xdg_root = env.root / "prec_xdg"
        xdg_cfg = xdg_root / "agy-companion" / "config.json"
        home_cfg = env.home_dir / ".config" / "agy-companion" / "config.json"

        env.write_config(env.make_config_dict(default_model="gemini-4-cli-win"), path=cli_cfg)
        env.write_config(env.make_config_dict(default_model="gemini-4-env-win"), path=env_cfg)
        env.write_config(env.make_config_dict(default_model="gemini-4-xdg-win"), path=xdg_cfg)
        env.write_config(env.make_config_dict(default_model="gemini-4-home-win"), path=home_cfg)

        def _query_model(
            *,
            pass_config_flag: bool,
            config_path: Path | None,
            extra_env: dict[str, str | None],
        ) -> str:
            mcp = self.start_mcp(
                env,
                config_path=config_path,
                extra_env=extra_env,
                pass_config_flag=pass_config_flag,
                set_config_env=False,
            )
            sc = mcp.call_tool("get_settings", {}).get("structuredContent", {})
            mcp.assert_clean_stdout()
            found, model = _find_any_key(sc, ("gemini_default_model", "default_model"))
            self.assertTrue(found, f"Missing default_model in {sc!r}")
            return str(model)

        # 1. --config beats AGY_COMPANION_CONFIG, XDG_CONFIG_HOME, and $HOME/.config
        self.assertEqual(
            _query_model(
                pass_config_flag=True,
                config_path=cli_cfg,
                extra_env={"AGY_COMPANION_CONFIG": str(env_cfg), "XDG_CONFIG_HOME": str(xdg_root)},
            ),
            "gemini-4-cli-win",
        )
        # 2. AGY_COMPANION_CONFIG beats XDG_CONFIG_HOME and $HOME/.config
        self.assertEqual(
            _query_model(
                pass_config_flag=False,
                config_path=None,
                extra_env={"AGY_COMPANION_CONFIG": str(env_cfg), "XDG_CONFIG_HOME": str(xdg_root)},
            ),
            "gemini-4-env-win",
        )
        # 3. XDG_CONFIG_HOME beats $HOME/.config
        self.assertEqual(
            _query_model(
                pass_config_flag=False,
                config_path=None,
                extra_env={"AGY_COMPANION_CONFIG": None, "XDG_CONFIG_HOME": str(xdg_root)},
            ),
            "gemini-4-xdg-win",
        )
        # 4. Unset XDG_CONFIG_HOME falls back to $HOME/.config
        self.assertEqual(
            _query_model(
                pass_config_flag=False,
                config_path=None,
                extra_env={"AGY_COMPANION_CONFIG": None, "XDG_CONFIG_HOME": None},
            ),
            "gemini-4-home-win",
        )

    def test_t1_f16_usage_dir_precedence_config_xdg_home(self) -> None:
        """Usage dir precedence strictly follows: config.usage_dir > XDG_DATA_HOME > $HOME/.local/share."""
        env = self.make_env()
        mock = self.start_mock_gemini()
        explicit_usage = env.root / "explicit_usage"
        xdg_data = env.root / "prec_xdg_data"
        xdg_usage = xdg_data / "agy-companion" / "usage"
        home_usage = env.home_dir / ".local" / "share" / "agy-companion" / "usage"

        # Tier 1: config.usage_dir beats XDG_DATA_HOME and $HOME/.local/share.
        cfg1 = env.write_config(
            env.make_config_dict(
                base_url=mock.base_url,
                api_key="AIzaSyUsagePrecOne123",
                usage_dir=explicit_usage,
            ),
            path=env.root / "cfg1.json",
        )
        p1 = self.start_proxy(env, config_path=cfg1, extra_env={"XDG_DATA_HOME": str(xdg_data)})
        self.assertEqual(
            http_raw_request(
                "127.0.0.1", p1.port, "POST", "/v1beta/models/gemini-4-hydrogen:generateContent", body=b"{}"
            ).status,
            200,
        )
        _, ev1 = poll_usage_events(explicit_usage, min_count=1, timeout=3.0)
        self.assertEqual(len(ev1), 1)
        self.assertEqual(env.usage_files(xdg_usage), [])
        self.assertEqual(env.usage_files(home_usage), [])
        p1.stop()

        # Tier 2: omitted config.usage_dir uses XDG_DATA_HOME/agy-companion/usage.
        cfg2 = env.write_config(
            env.make_config_dict(
                base_url=mock.base_url,
                api_key="AIzaSyUsagePrecTwo123",
                include_usage_dir=False,
            ),
            path=env.root / "cfg2.json",
        )
        p2 = self.start_proxy(env, config_path=cfg2, extra_env={"XDG_DATA_HOME": str(xdg_data)})
        self.assertEqual(
            http_raw_request(
                "127.0.0.1", p2.port, "POST", "/v1beta/models/gemini-4-hydrogen:generateContent", body=b"{}"
            ).status,
            200,
        )
        _, ev2 = poll_usage_events(xdg_usage, min_count=1, timeout=3.0)
        self.assertEqual(len(ev2), 1)
        self.assertEqual(env.usage_files(home_usage), [])
        p2.stop()

        # Tier 3: omitted config.usage_dir and unset XDG_DATA_HOME falls back to $HOME/.local/share.
        p3 = self.start_proxy(env, config_path=cfg2, extra_env={"XDG_DATA_HOME": None})
        self.assertEqual(
            http_raw_request(
                "127.0.0.1", p3.port, "POST", "/v1beta/models/gemini-4-hydrogen:generateContent", body=b"{}"
            ).status,
            200,
        )
        _, ev3 = poll_usage_events(home_usage, min_count=1, timeout=3.0)
        self.assertEqual(len(ev3), 1)
        p3.stop()

    def test_t1_gateway_v1beta_models_discovery_live_and_fallback(self) -> None:
        """GET /v1beta/models serves filtered live Gemini 3/4 models (default first) and falls back cleanly."""
        env = self.make_env()
        mock = self.start_mock_gemini()
        mock.enqueue(
            JsonAction(
                status=200,
                payload={
                    "models": [
                        {
                            "name": "models/gemini-2.5-pro",
                            "displayName": "Gemini 2.5 Pro",
                            "supportedGenerationMethods": ["generateContent"],
                        },
                        {
                            "name": "models/gemini-4-carbon",
                            "displayName": "Gemini 4 Carbon",
                            "inputTokenLimit": 1048576,
                            "outputTokenLimit": 65536,
                            "supportedGenerationMethods": ["generateContent"],
                        },
                        {
                            "name": "models/gemini-3.8-flash",
                            "displayName": "Gemini 3.8 Flash",
                            "inputTokenLimit": 1048576,
                            "outputTokenLimit": 65536,
                            "supportedGenerationMethods": ["generateContent"],
                            "thinking": True,
                        },
                        {
                            "name": "models/gemini-3.8-flash-tts",
                            "displayName": "Gemini 3.8 Flash TTS",
                            "supportedGenerationMethods": ["generateContent"],
                        },
                    ]
                },
            )
        )
        cfg = env.write_config(
            env.make_config_dict(
                base_url=mock.base_url,
                api_key="AIzaSyGatewayList123",
                default_model="gemini-3.8-flash",
            )
        )
        proxy = self.start_proxy(env, config_path=cfg)

        # 1. Live filtered catalog from upstream with default_model first and security headers.
        resp = http_raw_request("127.0.0.1", proxy.port, "GET", "/v1beta/models?pageSize=1000")
        self.assertEqual(resp.status, 200, resp.text)
        self.assertEqual(resp.header("x-content-type-options"), "nosniff")
        self.assertIn("no-store", resp.header("cache-control"))
        names = [m["name"] for m in json.loads(resp.text)["models"]]
        self.assertEqual(names, ["models/gemini-3.8-flash", "models/gemini-4-carbon"])

        # 2. Fallback to curated Gemini 4/3 catalog when upstream returns 503.
        mock.enqueue(JsonAction(status=503, payload={"error": {"message": "overloaded"}}))
        fb_resp = http_raw_request("127.0.0.1", proxy.port, "GET", "/v1beta/models?pageSize=1000")
        self.assertEqual(fb_resp.status, 200, fb_resp.text)
        fb_names = [m["name"] for m in json.loads(fb_resp.text)["models"]]
        self.assertEqual(fb_names[0], "models/gemini-3.8-flash")
        self.assertIn("models/gemini-4-hydrogen", fb_names)

        # 3. POST /v1beta/models remains rejected with 405.
        post_resp = http_raw_request("127.0.0.1", proxy.port, "POST", "/v1beta/models", body=b"{}")
        self.assertEqual(post_resp.status, 405, post_resp.text)
        proxy.stop()

    def test_t1_cloudcode_hybrid_fetch_available_models_and_v1internal_passthrough(self) -> None:
        """CloudCode /v1internal:fetchAvailableModels merges proxy models with deterministic slots + XHigh and streams other /v1internal:* RPCs."""
        env = self.make_env()
        mock_ccpa = self.start_mock_gemini()
        mock_gemini = self.start_mock_gemini()
        ccpa_catalog = {
            "models": {
                "gemini-4-carbon-high": {
                    "model": "MODEL_PLACEHOLDER_M427",
                    "displayName": "Gemini 4 Carbon (High)",
                    "thinkingLevel": 3,
                },
                "existing-m334": {
                    "model": "MODEL_PLACEHOLDER_M334",
                    "displayName": "Existing Custom",
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
        gemini_models_payload = {
            "models": [
                {"name": "models/gemini-4-hydrogen"},
                {"name": "models/gemini-4-argon"},
                {"name": "models/gemini-4-neon"},
                {"name": "models/gemini-4-carbon"},
            ]
        }
        mock_gemini.enqueue(JsonAction(status=200, payload=gemini_models_payload))
        mock_gemini.enqueue(JsonAction(status=200, payload=gemini_models_payload))
        mock_ccpa.enqueue(JsonAction(status=200, payload=ccpa_catalog))
        mock_ccpa.enqueue(JsonAction(status=200, payload=ccpa_catalog))
        mock_ccpa.enqueue(
            SseStreamAction(
                chunks=[b'data: {"response":{"text":"hello-ccpa"}}\n\n'],
                status=200,
            )
        )

        cfg_dict = env.make_config_dict(
            base_url=mock_gemini.base_url,
            api_key="AIzaSyCloudCodeHybrid12",
            default_model="gemini-4-hydrogen",
        )
        cfg_dict["cloudcode_upstream_url"] = mock_ccpa.base_url
        cfg = env.write_config(cfg_dict)
        proxy = self.start_proxy(env, config_path=cfg)

        # 1. fetchAvailableModels preserves original models + groupedModels and injects [Proxy] models.
        fetch_resp = http_raw_request(
            "127.0.0.1",
            proxy.port,
            "POST",
            "/v1internal:fetchAvailableModels",
            headers={
                "Authorization": "Bearer google-ai-pro-test-token",
                "Content-Type": "application/json",
            },
            body=b'{"project":"personal"}',
        )
        self.assertEqual(fetch_resp.status, 200, fetch_resp.text)
        self.assertEqual(fetch_resp.header("x-content-type-options"), "nosniff")
        self.assertIn("no-store", fetch_resp.header("cache-control"))
        merged = json.loads(fetch_resp.text)

        self.assertEqual(merged["defaultAgentModelId"], "gemini-4-carbon-high")
        self.assertEqual(
            merged["models"]["gemini-4-carbon-high"]["displayName"], "Gemini 4 Carbon (High)"
        )
        self.assertEqual(
            merged["groupedModels"]["gemini-4-carbon"]["displayName"], "Gemini 4 Carbon"
        )
        # Injected proxy model avoids M334 collision and carries gemini-api:// URL + thinking effort labels.
        h_low = merged["models"]["companion/gemini-4-hydrogen-low"]
        h_xhigh = merged["models"]["companion/gemini-4-hydrogen-xhigh"]
        n_xhigh = merged["models"]["companion/gemini-4-neon-xhigh"]
        self.assertEqual(h_low["model"], "MODEL_PLACEHOLDER_M335")
        self.assertEqual(h_low["modelUrl"], "gemini-api://gemini-4-hydrogen")
        self.assertEqual(h_low["displayName"], "Gemini 4 Hydrogen (Proxy) (Low)")
        self.assertEqual(h_xhigh["thinkingLevel"], 7)
        self.assertEqual(h_xhigh["displayName"], "Gemini 4 Hydrogen (Proxy) (XHigh)")
        self.assertEqual(n_xhigh["thinkingLevel"], 7)
        self.assertEqual(n_xhigh["displayName"], "Gemini 4 Neon (Proxy) (XHigh)")
        self.assertEqual(
            merged["groupedModels"]["companion/gemini-4-hydrogen"]["displayName"],
            "Gemini 4 Hydrogen (Proxy)",
        )
        suffixes = [
            lvl["thinkingSuffix"]
            for lvl in merged["groupedModels"]["companion/gemini-4-hydrogen"][
                "supportedThinkingLevels"
            ]
        ]
        self.assertEqual(suffixes, ["low", "medium", "high", "xhigh"])
        neon_suffixes = [
            lvl["thinkingSuffix"]
            for lvl in merged["groupedModels"]["companion/gemini-4-neon"][
                "supportedThinkingLevels"
            ]
        ]
        self.assertEqual(neon_suffixes, ["low", "medium", "high", "xhigh"])
        # Appended directly into Recommended groups[0] right after original models.
        group0_ids = merged["agentModelSorts"][0]["groups"][0]["modelIds"]
        self.assertEqual(group0_ids[0], "gemini-4-carbon-high")
        self.assertIn("companion/gemini-4-hydrogen-low", group0_ids)
        self.assertIn("companion/gemini-4-hydrogen-xhigh", group0_ids)
        self.assertIn("companion/gemini-4-neon-xhigh", group0_ids)

        # 1b. Changing default_model hot-reloads without shifting deterministic placeholder enum slots.
        slots_before = {
            k: v["model"] for k, v in merged["models"].items() if k.startswith("companion/")
        }
        cfg_dict["gemini"]["default_model"] = "gemini-4-neon"
        env.write_config(cfg_dict, path=cfg)
        env.bump_mtime(cfg)
        fetch_resp2 = http_raw_request(
            "127.0.0.1",
            proxy.port,
            "POST",
            "/v1internal:fetchAvailableModels",
            headers={
                "Authorization": "Bearer google-ai-pro-test-token",
                "Content-Type": "application/json",
            },
            body=b'{"project":"personal"}',
        )
        self.assertEqual(fetch_resp2.status, 200, fetch_resp2.text)
        merged2 = json.loads(fetch_resp2.text)
        slots_after = {
            k: v["model"] for k, v in merged2["models"].items() if k.startswith("companion/")
        }
        self.assertEqual(slots_before, slots_after)
        self.assertEqual(
            merged2["models"]["companion/gemini-4-hydrogen-low"]["model"],
            "MODEL_PLACEHOLDER_M335",
        )

        # 2. Other /v1internal:* RPCs (including alt=sse streaming) pass through transparently.
        stream_resp = http_raw_request(
            "127.0.0.1",
            proxy.port,
            "POST",
            "/v1internal:streamGenerateContent?alt=sse",
            headers={
                "Authorization": "Bearer google-ai-pro-test-token",
                "Content-Type": "application/json",
            },
            body=b'{"model":"gemini-4-carbon-high"}',
        )
        self.assertEqual(stream_resp.status, 200, stream_resp.text)
        self.assertEqual(stream_resp.body, b'data: {"response":{"text":"hello-ccpa"}}\n\n')
        self.assertEqual(len(mock_ccpa.requests), 3)
        self.assertEqual(
            mock_ccpa.requests[0].headers.get("authorization"),
            "Bearer google-ai-pro-test-token",
        )
        self.assertEqual(
            mock_ccpa.requests[2].headers.get("authorization"),
            "Bearer google-ai-pro-test-token",
        )

        # 3. S3/S7 rejections on /v1internal routes.
        self.assertEqual(
            http_raw_request("127.0.0.1", proxy.port, "PUT", "/v1internal:loadCodeAssist").status,
            405,
        )
        self.assertEqual(
            http_raw_request(
                "127.0.0.1", proxy.port, "POST", "/v1internal/foo/../secret", body=b"{}"
            ).status,
            400,
        )
        proxy.stop()

    def test_t1_launch_print_env_and_serve_adopt_running(self) -> None:
        """launch --print-env emits Hybrid Mode env vars and serve --adopt-running parks cleanly on an occupied companion port."""
        import subprocess
        import time

        binary = self.require_binary()
        env = self.make_env()
        cfg = env.write_config(env.make_config_dict())

        # 1. launch --print-env prints exact Hybrid Mode command.
        res = run_cli(
            [
                str(binary),
                "launch",
                "--port",
                "4321",
                "--config",
                str(cfg),
                "--app",
                "/opt/antigravity",
                "--print-env",
            ],
            env=env.build_env(),
        )
        self.assertEqual(res.returncode, 0, res.stderr)
        self.assertIn('CLOUD_CODE_URL="http://127.0.0.1:4321"', res.stdout)
        self.assertIn('GOOGLE_GEMINI_BASE_URL="http://127.0.0.1:4321"', res.stdout)
        self.assertIn('GEMINI_API_KEY="agy-companion-local"', res.stdout)
        self.assertIn("/opt/antigravity", res.stdout)

        # 2. serve --adopt-running stays alive cleanly when a healthy companion proxy already owns the port.
        proxy = self.start_proxy(env, config_path=cfg)
        adopter = subprocess.Popen(
            [str(binary), "serve", "--port", str(proxy.port), "--config", str(cfg), "--adopt-running"],
            env=env.build_env(),
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
        )
        try:
            time.sleep(0.5)
            self.assertIsNone(adopter.poll(), "adopter should stay alive monitoring existing proxy")
            adopter.send_signal(signal.SIGTERM)
            rc = adopter.wait(timeout=5)
            self.assertEqual(rc, 0)
        finally:
            if adopter.poll() is None:
                adopter.kill()
                adopter.wait(timeout=3)
            proxy.stop()

    def test_t1_pr1_day0_untouched_fetch_available_models_and_extended_stats(self) -> None:
        """Day-0 (no key or placeholder key) returns upstream fetchAvailableModels untouched and exposes all 10 /api/stats fields."""
        upstream_catalog = {
            "models": {
                "gemini-4-carbon-high": {
                    "model": "MODEL_PLACEHOLDER_M427",
                    "displayName": "Gemini 4 Carbon (High)",
                }
            },
            "defaultAgentModelId": "gemini-4-carbon-high",
        }
        for label, api_key in (("none", None), ("placeholder", "agy-companion-local")):
            with self.subTest(api_key_case=label):
                env = self.make_env()
                mock_ccpa = self.start_mock_gemini()
                mock_ccpa.enqueue(JsonAction(status=200, payload=upstream_catalog))

                cfg_dict = env.make_config_dict(api_key=api_key)
                cfg_dict["cloudcode_upstream_url"] = mock_ccpa.base_url
                cfg = env.write_config(cfg_dict)
                proxy = self.start_proxy(env, config_path=cfg)

                fetch_resp = http_raw_request(
                    "127.0.0.1",
                    proxy.port,
                    "POST",
                    "/v1internal:fetchAvailableModels",
                    headers={
                        "Authorization": "Bearer day0-pro-token",
                        "Content-Type": "application/json",
                    },
                    body=b'{"project":"personal"}',
                )
                self.assertEqual(fetch_resp.status, 200, fetch_resp.text)
                body = json.loads(fetch_resp.text)
                self.assertEqual(body, upstream_catalog)
                self.assertFalse(
                    any(k.startswith("companion/") for k in body.get("models", {})),
                    f"Day-0 case '{label}' must not inject proxy models: {body!r}",
                )

                stats_resp = http_raw_request("127.0.0.1", proxy.port, "GET", "/api/stats")
                self.assertEqual(stats_resp.status, 200, stats_resp.text)
                stats = json.loads(stats_resp.text)
                expected_keys = {
                    "requests",
                    "prompt_tokens",
                    "cached_tokens",
                    "output_tokens",
                    "reasoning_tokens",
                    "cache_hit_ratio",
                    "last_catalog_refresh_at_ms",
                    "last_catalog_included_proxy_models",
                    "cloudcode_mode",
                    "cloudcode_resolved_upstream",
                }
                self.assertTrue(
                    expected_keys.issubset(stats.keys()),
                    f"Missing /api/stats fields: {expected_keys - set(stats.keys())}",
                )
                self.assertIsInstance(stats["last_catalog_refresh_at_ms"], int)
                self.assertGreater(stats["last_catalog_refresh_at_ms"], 0)
                self.assertIs(stats["last_catalog_included_proxy_models"], False)
                self.assertEqual(stats["cloudcode_mode"], "fixed")
                self.assertEqual(stats["cloudcode_resolved_upstream"], mock_ccpa.base_url)
                proxy.stop()

    def test_t1_pr1_placeholder_sentinel_quarantine_in_doctor_and_ui_mcp(self) -> None:
        """Placeholder sentinel 'agy-companion-local' never false-passes doctor and is rejected by ui-mcp save_settings."""
        binary = self.require_binary()
        env = self.make_env()
        cfg = env.write_config(env.make_config_dict(api_key=None))
        proxy = self.start_proxy(env, config_path=cfg)

        # 1. doctor with GEMINI_API_KEY="agy-companion-local" and no config key -> WARN (not PASS).
        doc_env = env.build_env(extra={"GEMINI_API_KEY": "agy-companion-local"})
        res_json = run_cli(
            [str(binary), "doctor", "--config", str(cfg), "--port", str(proxy.port), "--json"],
            env=doc_env,
        )
        self.assertEqual(res_json.returncode, 0, res_json.stderr)
        parsed = json.loads(res_json.stdout)
        self.assertIs(parsed["ok"], True)
        self.assertIs(parsed["env"]["gemini_api_key_set"], False)
        key_checks = [c for c in parsed["checks"] if c["name"] == "env_gemini_api_key"]
        self.assertEqual(len(key_checks), 1)
        self.assertEqual(key_checks[0]["status"], "warn")
        self.assertNotEqual(key_checks[0]["status"], "pass")

        res_human = run_cli(
            [str(binary), "doctor", "--config", str(cfg), "--port", str(proxy.port)],
            env=doc_env,
        )
        self.assertEqual(res_human.returncode, 0, res_human.stderr)
        self.assertIn("[WARN] env_gemini_api_key:", res_human.stdout)
        self.assertNotIn("[PASS] env_gemini_api_key:", res_human.stdout)
        proxy.stop()

        # 2. ui-mcp save_settings rejects "agy-companion-local".
        mcp = self.start_mcp(env, config_path=cfg)
        save_res = mcp.call_tool("save_settings", {"api_key": "agy-companion-local"})
        mcp.assert_clean_stdout()
        self.assertIs(save_res.get("isError"), True, save_res)
        self.assertIn("placeholder", _text_blocks(save_res).lower())
        saved_cfg = json.loads(cfg.read_text(encoding="utf-8"))
        self.assertIsNone(saved_cfg["gemini"]["api_key"])

    def test_t1_pr1_unsupported_model_400_includes_unset_base_url_hint(self) -> None:
        """Non-Gemini-3/4 400 error explains both model support and unsetting GOOGLE_GEMINI_BASE_URL."""
        env = self.make_env()
        mock_gemini = self.start_mock_gemini()
        cfg = env.write_config(
            env.make_config_dict(base_url=mock_gemini.base_url, api_key="AIzaSyUnsupported400Key")
        )
        proxy = self.start_proxy(env, config_path=cfg)

        resp = http_raw_request(
            "127.0.0.1",
            proxy.port,
            "POST",
            "/v1beta/models/gemini-2.5-pro:generateContent",
            headers={"Content-Type": "application/json"},
            body=b'{"contents":[{"parts":[{"text":"hi"}]}]}',
        )
        self.assertEqual(resp.status, 400, resp.text)
        self.assertIn("Gemini 3.8+ and Gemini 4+", resp.text)
        self.assertIn("unset GOOGLE_GEMINI_BASE_URL to reach Gemini API directly", resp.text)
        self.assertEqual(len(mock_gemini.requests), 0)
        proxy.stop()

    def test_t1_pr1_serve_idle_shutdown_writes_0600_pid_and_log_and_cleans_up(self) -> None:
        """serve --idle-shutdown-secs 1 writes 0600 proxy.pid & proxy.log in env.data_dir, exits 0 cleanly, and cleans up proxy.pid."""
        import subprocess
        import time

        binary = self.require_binary()
        env = self.make_env()
        port = find_free_port()
        cfg = env.write_config(env.make_config_dict(listen_port=port))

        proc = subprocess.Popen(
            [
                str(binary),
                "serve",
                "--port",
                str(port),
                "--config",
                str(cfg),
                "--idle-shutdown-secs",
                "1",
            ],
            env=env.build_env(),
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
        )
        try:
            deadline = time.monotonic() + 10.0
            while True:
                try:
                    resp = http_raw_request("127.0.0.1", port, "GET", "/health", timeout=1.0)
                    if resp.status == 200:
                        break
                except OSError:
                    pass
                self.assertLess(time.monotonic(), deadline, "idle-shutdown proxy never became healthy")
                time.sleep(0.05)

            pid_path = env.data_dir / "proxy.pid"
            log_path = env.data_dir / "proxy.log"
            self.assertTrue(pid_path.is_file(), f"Missing {pid_path}")
            self.assertTrue(log_path.is_file(), f"Missing {log_path}")

            self.assertEqual(stat.S_IMODE(os.stat(pid_path).st_mode), 0o600)
            self.assertEqual(stat.S_IMODE(os.stat(log_path).st_mode), 0o600)

            pid_info = json.loads(pid_path.read_text(encoding="utf-8"))
            self.assertEqual(pid_info["pid"], proc.pid)
            self.assertEqual(pid_info["port"], port)
            self.assertTrue(pid_info["version"])
            self.assertGreater(pid_info["started_at_ms"], 0)
            self.assertTrue(pid_info["binary_path"])

            log_text = log_path.read_text(encoding="utf-8")
            self.assertIn("agy-companion proxy listening", log_text)

            rc = proc.wait(timeout=6)
            self.assertEqual(rc, 0, f"Expected clean exit 0 on idle shutdown, got {rc}")
            self.assertFalse(pid_path.exists(), "proxy.pid must be removed after clean idle shutdown")
        finally:
            if proc.poll() is None:
                proc.kill()
                proc.wait(timeout=3)
