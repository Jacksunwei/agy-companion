"""Tier 2: Boundary, Corner-Case, Negative & Security Tests (83 tests).

Covers all 12 core features (6–9 tests per feature F1–F12 = 81 tests) plus 2
additive boundary tests for F14 & F15, enforcing Security Invariants S1–S8 and
Ambiguity Guards A1–A15.
"""

from __future__ import annotations

import json
import math
import os
import re
import shutil
import stat
import tomllib
from pathlib import Path
from typing import Any

from harness import (
    E2ETestCase,
    extract_models_list,
    find_free_port,
    find_key_recursive,
    http_raw_request,
    poll_stats,
    poll_usage_events,
    run_cli,
)
from mcp_client import McpStdioClient
from mock_gemini import (
    EchoChecksumAction,
    JsonAction,
    RawBytesAction,
    SseStreamAction,
    build_sse_events,
    split_across_utf8_char,
)


def _extract_cargo_version(repo_root: Path) -> str:
    data = tomllib.loads((repo_root / "Cargo.toml").read_text(encoding="utf-8"))
    ver = data.get("workspace", {}).get("package", {}).get("version") or data.get("package", {}).get("version")
    assert isinstance(ver, str) and ver.strip()
    return ver.strip()


class TestTier2F1ConfigBoundaries(E2ETestCase):
    TIER = 2
    FEATURE = "F1"

    def test_t2_f1_rejects_bind_0_0_0_0(self) -> None:
        """serve refuses non-loopback listen_host '0.0.0.0' with non-zero exit (S1)."""
        binary = self.require_binary()
        env = self.make_env()
        cfg = env.write_config(env.make_config_dict(listen_host="0.0.0.0", listen_port=find_free_port()))
        res = run_cli([str(binary), "serve", "--config", str(cfg)], env=env.build_env(), timeout=10.0)
        self.assertNotEqual(res.returncode, 0, f"Expected failure on 0.0.0.0, stdout={res.stdout!r}")
        self.assertTrue(res.stderr.strip(), "Expected diagnostic error on stderr")

    def test_t2_f1_rejects_bind_ipv6_wildcard(self) -> None:
        """serve refuses IPv6 wildcard listen_host '::' with non-zero exit (S1)."""
        binary = self.require_binary()
        env = self.make_env()
        cfg = env.write_config(env.make_config_dict(listen_host="::", listen_port=find_free_port()))
        res = run_cli([str(binary), "serve", "--config", str(cfg)], env=env.build_env(), timeout=10.0)
        self.assertNotEqual(res.returncode, 0, "Expected failure on listen_host '::'")

    def test_t2_f1_rejects_remote_http_upstream(self) -> None:
        """serve and doctor reject remote http://, file://, and ftp:// upstream URLs (S5)."""
        binary = self.require_binary()
        for bad_url in ("http://example.com", "file:///etc/passwd", "ftp://example.com"):
            with self.subTest(base_url=bad_url):
                env = self.make_env()
                cfg = env.write_config(env.make_config_dict(base_url=bad_url, listen_port=find_free_port()))
                res_serve = run_cli([str(binary), "serve", "--config", str(cfg)], env=env.build_env(), timeout=10.0)
                self.assertNotEqual(res_serve.returncode, 0, f"serve accepted forbidden upstream {bad_url}")
                self.assertNotIn("panicked at", res_serve.stderr.lower())
                res_doc = run_cli([str(binary), "doctor", "--config", str(cfg)], env=env.build_env(), timeout=10.0)
                self.assertNotEqual(res_doc.returncode, 0, f"doctor accepted forbidden upstream {bad_url}")

    def test_t2_f1_rejects_dotdot_traversal_in_usage_dir(self) -> None:
        """Config validation rejects '..' path traversal components in usage_dir (S6)."""
        binary = self.require_binary()
        for bad_dir in ("/tmp/../etc", "../escape"):
            with self.subTest(usage_dir=bad_dir):
                env = self.make_env()
                cfg = env.write_config(
                    env.make_config_dict(usage_dir=bad_dir, listen_port=find_free_port())
                )
                res_serve = run_cli([str(binary), "serve", "--config", str(cfg)], env=env.build_env(), timeout=10.0)
                self.assertNotEqual(res_serve.returncode, 0, f"serve accepted traversal usage_dir={bad_dir!r}")
                self.assertNotIn("panicked at", res_serve.stderr.lower())
        # If save_settings exposes usage_dir, it must also reject traversal cleanly.
        env_mcp = self.make_env()
        mcp = self.start_mcp(env_mcp)
        if "usage_dir" in mcp.get_save_settings_schema():
            resp = mcp.save_settings_smart(usage_dir="/tmp/../etc", raw=True)
            mcp.assert_clean_stdout()
            self.assertTrue(
                McpStdioClient.is_tool_error(resp),
                f"save_settings accepted traversal usage_dir: {resp!r}",
            )

    def test_t2_f1_allows_loopback_http_upstreams_127_and_localhost(self) -> None:
        """Loopback http://127.0.0.1:<p> and http://localhost:<p> are accepted (S5 exception)."""
        for host_label in ("127.0.0.1", "localhost"):
            with self.subTest(upstream_host=host_label):
                env = self.make_env()
                mock = self.start_mock_gemini()
                base_url = f"http://{host_label}:{mock.port}"
                cfg = env.write_config(env.make_config_dict(base_url=base_url, api_key="AIzaSyLoopbackOk1234"))
                proxy = self.start_proxy(env, config_path=cfg)
                resp = http_raw_request(
                    "127.0.0.1",
                    proxy.port,
                    "POST",
                    "/v1beta/models/gemini-2.5-pro:generateContent",
                    body=b'{"contents":[]}',
                    headers={"Content-Type": "application/json"},
                )
                self.assertEqual(resp.status, 200)
                self.assertEqual(len(mock.requests), 1)

    def test_t2_f1_rejects_port_zero(self) -> None:
        """serve rejects listen_port == 0 in config file."""
        binary = self.require_binary()
        env = self.make_env()
        cfg = env.write_config(env.make_config_dict(listen_port=0))
        res = run_cli([str(binary), "serve", "--config", str(cfg)], env=env.build_env(), timeout=10.0)
        self.assertNotEqual(res.returncode, 0, "serve accepted listen_port == 0")

    def test_t2_f1_rejects_credentials_in_upstream_url(self) -> None:
        """serve rejects embedded credentials user:pass@ in gemini.base_url (S5)."""
        binary = self.require_binary()
        env = self.make_env()
        cfg = env.write_config(
            env.make_config_dict(
                base_url="https://user:pass@generativelanguage.googleapis.com",
                listen_port=find_free_port(),
            )
        )
        res = run_cli([str(binary), "serve", "--config", str(cfg)], env=env.build_env(), timeout=10.0)
        self.assertNotEqual(res.returncode, 0, "serve accepted credentials in base_url")
        self.assertNotIn("user:pass", res.stderr)

    def test_t2_f1_rejects_invalid_json_and_bad_schema_version(self) -> None:
        """Malformed JSON and unsupported schema_version=99 exit non-zero cleanly without panic."""
        binary = self.require_binary()
        cases = [
            ("malformed_json", "{not valid json"),
            ("bad_schema_version", json.dumps(self.make_env().make_config_dict(schema_version=99))),
        ]
        for label, raw in cases:
            with self.subTest(case=label):
                env = self.make_env()
                cfg = env.write_config(raw, mode=0o600)
                res = run_cli([str(binary), "serve", "--config", str(cfg)], env=env.build_env(), timeout=10.0)
                self.assertNotEqual(res.returncode, 0, f"serve should reject {label}")
                self.assertNotIn("panicked at", res.stderr.lower())
                self.assertTrue(res.stderr.strip(), f"Expected error message on stderr for {label}")

    def test_t2_f1_rewrites_0644_to_0600_and_protects_symlink_target(self) -> None:
        """save_settings tightens 0644->0600; symlink config fails closed at startup and post-startup (S2/S6)."""
        with self.subTest(case="tighten_0644_to_0600"):
            env = self.make_env()
            cfg = env.write_config(env.make_config_dict(), mode=0o644)
            self.assertEqual(stat.S_IMODE(os.stat(cfg).st_mode), 0o644)
            mcp = self.start_mcp(env, config_path=cfg)
            mcp.save_settings_smart(default_model="gemini-2.5-flash")
            mcp.assert_clean_stdout()
            self.assertEqual(stat.S_IMODE(os.stat(cfg).st_mode), 0o600)

        with self.subTest(case="protect_symlink_target_startup"):
            binary = self.require_binary()
            env = self.make_env()
            victim = env.root / "victim.txt"
            sentinel_json = json.dumps(env.make_config_dict(default_model="VICTIM_SENTINEL"), indent=2) + "\n"
            victim.write_text(sentinel_json, encoding="utf-8")
            sym_cfg = env.root / "sym_config.json"
            os.symlink(victim, sym_cfg)
            res = run_cli(
                [str(binary), "ui-mcp", "--config", str(sym_cfg), "--data-dir", str(env.data_dir)],
                env=env.build_env(),
                stdin_bytes=b"",
                timeout=10.0,
            )
            self.assertNotEqual(res.returncode, 0, "ui-mcp must fail closed on symlink --config at startup (S6)")
            self.assertEqual(res.stdout, "", f"ui-mcp emitted unexpected stdout on symlink rejection: {res.stdout!r}")
            self.assertNotIn("panicked at", res.stderr.lower())
            stderr_lower = res.stderr.lower()
            self.assertTrue(
                "symlink" in stderr_lower or "symbolic link" in stderr_lower,
                f"Expected symlink rejection diagnostic on stderr, got: {res.stderr!r}",
            )
            self.assertEqual(victim.read_text(encoding="utf-8"), sentinel_json)
            self.assertTrue(sym_cfg.is_symlink(), "Symlink config must not be mutated on startup rejection")

        with self.subTest(case="protect_symlink_target_post_startup"):
            env = self.make_env()
            victim = env.root / "victim.txt"
            sentinel_json = (
                json.dumps(
                    env.make_config_dict(
                        default_model="VICTIM_SENTINEL",
                        api_key="AIzaSyVictimSentinel0001",
                    ),
                    indent=2,
                )
                + "\n"
            )
            victim.write_text(sentinel_json, encoding="utf-8")
            sym_cfg = env.root / "sym_config.json"
            # Start cleanly while config does not exist yet (valid first-run UX), then swap in a symlink (TOCTOU).
            mcp = self.start_mcp(env, config_path=sym_cfg)
            os.symlink(victim, sym_cfg)

            get_resp = mcp.call_tool_raw("get_settings", {})
            mcp.assert_clean_stdout()
            self.assertTrue(
                McpStdioClient.is_tool_error(get_resp),
                f"get_settings must reject post-startup symlink config (S6): {get_resp!r}",
            )
            self.assertNotIn("VICTIM_SENTINEL", json.dumps(get_resp))
            self.assertNotIn("AIzaSyVictimSentinel0001", json.dumps(get_resp))
            self.assertNotIn("VICTIM_SENTINEL", mcp.stderr_text)
            self.assertNotIn("AIzaSyVictimSentinel0001", mcp.stderr_text)
            self.assertNotIn("panicked at", mcp.stderr_text.lower())

            save_resp = mcp.save_settings_smart(default_model="gemini-2.5-flash", raw=True)
            mcp.assert_clean_stdout()
            self.assertTrue(
                McpStdioClient.is_tool_error(save_resp),
                f"save_settings must reject post-startup symlink config (S6): {save_resp!r}",
            )
            self.assertNotIn("VICTIM_SENTINEL", json.dumps(save_resp))
            self.assertNotIn("AIzaSyVictimSentinel0001", json.dumps(save_resp))
            self.assertNotIn("VICTIM_SENTINEL", mcp.stderr_text)
            self.assertNotIn("AIzaSyVictimSentinel0001", mcp.stderr_text)
            self.assertNotIn("panicked at", mcp.stderr_text.lower())
            self.assertEqual(victim.read_text(encoding="utf-8"), sentinel_json)
            self.assertTrue(sym_cfg.is_symlink(), "Symlink must remain untouched when save_settings fails closed")


class TestTier2F2RedactionBoundaries(E2ETestCase):
    TIER = 2
    FEATURE = "F2"

    def test_t2_f2_key_length_12_exact_boundary(self) -> None:
        """Exact 12-char API key masks to first4...last4 ('1234...9012')."""
        env = self.make_env()
        secret = "123456789012"
        mcp = self.start_mcp(env)
        mcp.save_settings_smart(api_key=secret)
        res = mcp.call_tool("get_settings", {})
        mcp.assert_clean_stdout()
        raw = json.dumps(res)
        self.assertNotIn(secret, raw)
        sc = res.get("structuredContent", {})
        found_c, configured = find_key_recursive(sc, "api_key_configured")
        found_h, hint = find_key_recursive(sc, "api_key_hint")
        self.assertTrue(found_c and configured is True)
        self.assertTrue(found_h and hint == "1234...9012")

    def test_t2_f2_key_length_11_masks_to_four_stars(self) -> None:
        """11-char API key (below 12 boundary) masks strictly to '****'."""
        env = self.make_env()
        secret = "12345678901"
        mcp = self.start_mcp(env)
        mcp.save_settings_smart(api_key=secret)
        res = mcp.call_tool("get_settings", {})
        mcp.assert_clean_stdout()
        raw = json.dumps(res)
        self.assertNotIn(secret, raw)
        sc = res.get("structuredContent", {})
        found_c, configured = find_key_recursive(sc, "api_key_configured")
        found_h, hint = find_key_recursive(sc, "api_key_hint")
        self.assertTrue(found_c and configured is True)
        self.assertTrue(found_h and hint == "****")

    def test_t2_f2_key_length_1_masks_to_four_stars(self) -> None:
        """1-char API key masks to '****' without exposing the character."""
        env = self.make_env()
        mcp = self.start_mcp(env)
        mcp.save_settings_smart(api_key="X")
        res = mcp.call_tool("get_settings", {})
        mcp.assert_clean_stdout()
        sc = res.get("structuredContent", {})
        found_c, configured = find_key_recursive(sc, "api_key_configured")
        found_h, hint = find_key_recursive(sc, "api_key_hint")
        self.assertTrue(found_c and configured is True)
        self.assertTrue(found_h and hint == "****")

    def test_t2_f2_empty_string_key_treated_as_unconfigured(self) -> None:
        """Config with api_key='' reports api_key_configured=false and hint=None."""
        env = self.make_env()
        env.write_config(env.make_config_dict(api_key=""))
        mcp = self.start_mcp(env)
        res = mcp.call_tool("get_settings", {})
        mcp.assert_clean_stdout()
        sc = res.get("structuredContent", {})
        found_c, configured = find_key_recursive(sc, "api_key_configured")
        self.assertTrue(found_c and configured is False)
        found_h, hint = find_key_recursive(sc, "api_key_hint")
        if found_h:
            self.assertIsNone(hint)

    def test_t2_f2_whitespace_only_key_treated_as_unconfigured(self) -> None:
        """Space-only api_key reports unconfigured/hint=None; control-char keys reject fail-closed without leaking."""
        with self.subTest(case="spaces_only"):
            env = self.make_env()
            env.write_config(env.make_config_dict(api_key="     "))
            mcp = self.start_mcp(env)
            res = mcp.call_tool("get_settings", {})
            mcp.assert_clean_stdout()
            self.assertFalse(McpStdioClient.is_tool_error(res), f"Expected success for space-only api_key, got {res!r}")
            sc = res.get("structuredContent", {})
            found_c, configured = find_key_recursive(sc, "api_key_configured")
            self.assertTrue(found_c and configured is False, f"Expected api_key_configured=False in {sc!r}")
            found_h, hint = find_key_recursive(sc, "api_key_hint")
            if found_h:
                self.assertIsNone(hint)

        with self.subTest(case="control_char_whitespace_rejected"):
            raw_ws = "   \t\n  "
            env = self.make_env()
            env.write_config(env.make_config_dict(api_key=raw_ws))
            mcp = self.start_mcp(env)
            raw_resp = mcp.call_tool_raw("get_settings", {})
            mcp.assert_clean_stdout()
            self.assertTrue(
                McpStdioClient.is_tool_error(raw_resp),
                f"Expected fail-closed rejection for control-char whitespace api_key, got {raw_resp!r}",
            )
            serialized = json.dumps(raw_resp)
            self.assertIn("control", serialized.lower(), f"Expected control-char diagnostic, got {serialized}")
            self.assertNotIn(raw_ws, serialized)
            self.assertNotIn(raw_ws, mcp.stderr_text)
            self.assertNotIn("panicked at", mcp.stderr_text.lower())

        with self.subTest(case="control_char_secret_rejected_no_leak"):
            injected_prefix = "AIzaSyCtrlReject99"
            injected_hdr = "X-Injected"
            bad_secret = f"{injected_prefix}\r\n{injected_hdr}: 1"
            env = self.make_env()
            env.write_config(env.make_config_dict(api_key=bad_secret))
            mcp = self.start_mcp(env)
            raw_resp = mcp.call_tool_raw("get_settings", {})
            mcp.assert_clean_stdout()
            self.assertTrue(
                McpStdioClient.is_tool_error(raw_resp),
                f"Expected fail-closed rejection for CRLF-injected api_key, got {raw_resp!r}",
            )
            serialized = json.dumps(raw_resp)
            self.assertIn("control", serialized.lower(), f"Expected control-char diagnostic, got {serialized}")
            self.assertNotIn(injected_prefix, serialized)
            self.assertNotIn(injected_hdr, serialized)
            self.assertNotIn(injected_prefix, mcp.stderr_text)
            self.assertNotIn(injected_hdr, mcp.stderr_text)
            self.assertNotIn("panicked at", mcp.stderr_text.lower())

    def test_t2_f2_unicode_and_special_chars_in_key(self) -> None:
        """Special ASCII keys mask safely; multi-byte Unicode keys either mask safely or reject cleanly without panic/leak (A10)."""
        with self.subTest(case="special_ascii"):
            key = "AIza!@#$_-+=?/&%90ab"
            env = self.make_env()
            mcp = self.start_mcp(env)
            mcp.save_settings_smart(api_key=key)
            res = mcp.call_tool("get_settings", {})
            mcp.assert_clean_stdout()
            raw = json.dumps(res, ensure_ascii=False)
            self.assertNotIn(key, raw)
            self.assertNotIn(key, mcp.stderr_text)
            sc = res.get("structuredContent", {})
            found_c, configured = find_key_recursive(sc, "api_key_configured")
            found_h, hint = find_key_recursive(sc, "api_key_hint")
            self.assertTrue(found_c and configured is True)
            self.assertTrue(found_h and isinstance(hint, str) and len(hint) > 0 and hint != key)

        with self.subTest(case="multibyte_unicode"):
            key = "αβγδεζηθικλμνξοπ"
            env = self.make_env()
            mcp = self.start_mcp(env)
            save_resp = mcp.save_settings_smart(api_key=key, raw=True)
            mcp.assert_clean_stdout()
            self.assertNotIn(key, json.dumps(save_resp, ensure_ascii=False))
            self.assertNotIn(key, mcp.stderr_text)
            # Server must remain alive and responsive whether it accepted or cleanly rejected non-ASCII.
            res = mcp.call_tool("get_settings", {})
            mcp.assert_clean_stdout()
            raw = json.dumps(res, ensure_ascii=False)
            self.assertNotIn(key, raw)
            self.assertNotIn(key, mcp.stderr_text)
            if not McpStdioClient.is_tool_error(save_resp):
                sc = res.get("structuredContent", {})
                found_c, configured = find_key_recursive(sc, "api_key_configured")
                found_h, hint = find_key_recursive(sc, "api_key_hint")
                self.assertTrue(found_c and configured is True)
                self.assertTrue(found_h and isinstance(hint, str) and len(hint) > 0 and hint != key)

    def test_t2_f2_zero_plaintext_leak_in_mcp_and_proxy_stderr(self) -> None:
        """Unique sentinel key appears 0 times in MCP stdout/stderr and proxy stderr (S2)."""
        env = self.make_env()
        sentinel = "AIzaSyZeroLeakAuditSentinel9999"
        mock = self.start_mock_gemini(expected_api_key=sentinel)
        port = find_free_port()
        cfg = env.write_config(
            env.make_config_dict(listen_port=port, base_url=mock.base_url, api_key=sentinel)
        )
        proxy = self.start_proxy(env, config_path=cfg, port=port)
        mcp = self.start_mcp(env, config_path=cfg, extra_env={"RUST_LOG": "debug"})

        mcp.save_settings_smart(api_key=sentinel, base_url=mock.base_url, listen_port=port)
        mcp.call_tool("get_settings", {})
        mcp.call_tool("open_settings", {})
        mcp.call_tool("test_connection", {})
        mcp.assert_clean_stdout()

        resp = http_raw_request(
            "127.0.0.1",
            proxy.port,
            "POST",
            "/v1beta/models/gemini-2.5-pro:generateContent",
            body=b'{"contents":[]}',
            headers={"Content-Type": "application/json"},
        )
        self.assertEqual(resp.status, 200)

        self.assertNotIn(sentinel, mcp.stdout_text)
        self.assertNotIn(sentinel, mcp.stderr_text)
        self.assertNotIn(sentinel, proxy.stderr_text)
        self.assertNotIn(sentinel, proxy.stdout_text)


class TestTier2F3UsageBoundaries(E2ETestCase):
    TIER = 2
    FEATURE = "F3"

    def test_t2_f3_zero_tokens_usage_metadata(self) -> None:
        """All-zero usageMetadata records zero counts cleanly without crashing."""
        env = self.make_env()
        mock = self.start_mock_gemini(
            default_usage={
                "promptTokenCount": 0,
                "candidatesTokenCount": 0,
                "cachedContentTokenCount": 0,
                "thoughtsTokenCount": 0,
            }
        )
        cfg = env.write_config(env.make_config_dict(base_url=mock.base_url, api_key="AIzaSyZeroTokens1234"))
        proxy = self.start_proxy(env, config_path=cfg)
        resp = http_raw_request(
            "127.0.0.1",
            proxy.port,
            "POST",
            "/v1beta/models/gemini-2.5-pro:generateContent",
            body=b'{"contents":[]}',
            headers={"Content-Type": "application/json"},
        )
        self.assertEqual(resp.status, 200)
        _, events = poll_usage_events(env.usage_dir, min_count=1)
        self.assertEqual(len(events), 1)
        self.assertEqual(events[0]["input_tokens"], 0)
        self.assertEqual(events[0]["output_tokens"], 0)
        self.assertEqual(events[0]["cached_tokens"], 0)
        self.assertEqual(events[0]["reasoning_tokens"], 0)

    def test_t2_f3_missing_optional_cached_and_thoughts_fields(self) -> None:
        """usageMetadata omitting cachedContentTokenCount & thoughtsTokenCount defaults them to 0."""
        env = self.make_env()
        mock = self.start_mock_gemini(
            default_usage={
                "promptTokenCount": 42,
                "candidatesTokenCount": 18,
            }
        )
        cfg = env.write_config(env.make_config_dict(base_url=mock.base_url, api_key="AIzaSyPartialUsage123"))
        proxy = self.start_proxy(env, config_path=cfg)
        resp = http_raw_request(
            "127.0.0.1",
            proxy.port,
            "POST",
            "/v1beta/models/gemini-2.5-pro:generateContent",
            body=b'{"contents":[]}',
            headers={"Content-Type": "application/json"},
        )
        self.assertEqual(resp.status, 200)
        _, events = poll_usage_events(env.usage_dir, min_count=1)
        self.assertEqual(len(events), 1)
        self.assertEqual(events[0]["input_tokens"], 42)
        self.assertEqual(events[0]["output_tokens"], 18)
        self.assertEqual(events[0]["cached_tokens"], 0)
        self.assertEqual(events[0]["reasoning_tokens"], 0)

    def test_t2_f3_malformed_json_response_does_not_crash_sink(self) -> None:
        """Malformed JSON upstream response passes bytes through and leaves proxy + sink healthy."""
        env = self.make_env()
        mock = self.start_mock_gemini()
        bad_bytes = b"{truncated-json:::"
        mock.enqueue(RawBytesAction(status=200, content_type="application/json", body=bad_bytes))
        cfg = env.write_config(env.make_config_dict(base_url=mock.base_url, api_key="AIzaSyMalformedResp12"))
        proxy = self.start_proxy(env, config_path=cfg)

        r1 = http_raw_request(
            "127.0.0.1",
            proxy.port,
            "POST",
            "/v1beta/models/gemini-2.5-pro:generateContent",
            body=b'{"contents":[]}',
            headers={"Content-Type": "application/json"},
        )
        self.assertEqual(r1.status, 200)
        self.assertEqual(r1.body, bad_bytes)
        self.assertEqual(http_raw_request("127.0.0.1", proxy.port, "GET", "/health").status, 200)

        r2 = http_raw_request(
            "127.0.0.1",
            proxy.port,
            "POST",
            "/v1beta/models/gemini-2.5-pro:generateContent",
            body=b'{"contents":[]}',
            headers={"Content-Type": "application/json"},
        )
        self.assertEqual(r2.status, 200)
        files, events = poll_usage_events(env.usage_dir, min_count=1)
        self.assertGreaterEqual(len(events), 1)
        for fpath in files:
            for line in fpath.read_text(encoding="utf-8").splitlines():
                if line.strip():
                    json.loads(line)

    def test_t2_f3_multiple_events_append_without_overwriting(self) -> None:
        """5 sequential requests append 5 distinct ordered JSONL lines without truncation."""
        env = self.make_env()
        mock = self.start_mock_gemini()
        expected_prompts = [10, 20, 30, 40, 50]
        for p in expected_prompts:
            mock.enqueue(
                JsonAction(
                    status=200,
                    payload={
                        "candidates": [{"content": {"parts": [{"text": f"p={p}"}]}}],
                        "usageMetadata": {"promptTokenCount": p, "candidatesTokenCount": 5},
                    },
                )
            )
        cfg = env.write_config(env.make_config_dict(base_url=mock.base_url, api_key="AIzaSyAppendOrder1234"))
        proxy = self.start_proxy(env, config_path=cfg)
        for _ in expected_prompts:
            r = http_raw_request(
                "127.0.0.1",
                proxy.port,
                "POST",
                "/v1beta/models/gemini-2.5-pro:generateContent",
                body=b'{"contents":[]}',
                headers={"Content-Type": "application/json"},
            )
            self.assertEqual(r.status, 200)
        files, events = poll_usage_events(env.usage_dir, min_count=5)
        self.assertEqual(len(files), 1)
        self.assertEqual([e["input_tokens"] for e in events], expected_prompts)

    def test_t2_f3_jsonl_file_permissions_remain_0600_after_appends(self) -> None:
        """Monthly JSONL file retains mode 0600 across multiple appends (S3)."""
        env = self.make_env()
        mock = self.start_mock_gemini()
        cfg = env.write_config(env.make_config_dict(base_url=mock.base_url, api_key="AIzaSyPermAfterAppend1"))
        proxy = self.start_proxy(env, config_path=cfg)
        for _ in range(3):
            r = http_raw_request(
                "127.0.0.1",
                proxy.port,
                "POST",
                "/v1beta/models/gemini-2.5-pro:generateContent",
                body=b'{"contents":[]}',
                headers={"Content-Type": "application/json"},
            )
            self.assertEqual(r.status, 200)
        files, events = poll_usage_events(env.usage_dir, min_count=3)
        self.assertEqual(len(events), 3)
        self.assertEqual(len(files), 1)
        self.assertEqual(stat.S_IMODE(os.stat(files[0]).st_mode), 0o600)

    def test_t2_f3_zero_prompt_response_or_key_in_jsonl(self) -> None:
        """JSONL file contains zero API keys, prompt text, or response text (S2)."""
        env = self.make_env()
        secret_key = "AIzaSyJsonlNoLeakKey7777"
        prompt_marker = "SECRET_PROMPT_MARKER_9911"
        resp_marker = "SECRET_RESPONSE_MARKER_8822"
        mock = self.start_mock_gemini()
        mock.enqueue(
            JsonAction(
                status=200,
                payload={
                    "candidates": [{"content": {"parts": [{"text": resp_marker}]}}],
                    "usageMetadata": {"promptTokenCount": 50, "candidatesTokenCount": 12},
                },
            )
        )
        cfg = env.write_config(env.make_config_dict(base_url=mock.base_url, api_key=secret_key))
        proxy = self.start_proxy(env, config_path=cfg)
        body = json.dumps({"contents": [{"parts": [{"text": prompt_marker}]}]}).encode("utf-8")
        r = http_raw_request(
            "127.0.0.1",
            proxy.port,
            "POST",
            "/v1beta/models/gemini-2.5-pro:generateContent",
            body=body,
            headers={"Content-Type": "application/json"},
        )
        self.assertEqual(r.status, 200)
        files, events = poll_usage_events(env.usage_dir, min_count=1)
        self.assertEqual(len(events), 1)
        raw_jsonl = files[0].read_text(encoding="utf-8")
        self.assertNotIn(secret_key, raw_jsonl)
        self.assertNotIn(prompt_marker, raw_jsonl)
        self.assertNotIn(resp_marker, raw_jsonl)


class TestTier2F4ProviderBoundaries(E2ETestCase):
    TIER = 2
    FEATURE = "F4"

    def test_t2_f4_base_url_trailing_slash_normalized(self) -> None:
        """Trailing '/' on gemini.base_url produces clean /v1beta/models/... path."""
        env = self.make_env()
        mock = self.start_mock_gemini()
        cfg = env.write_config(env.make_config_dict(base_url=f"{mock.base_url}/", api_key="AIzaSyNormSlash12345"))
        proxy = self.start_proxy(env, config_path=cfg)
        r = http_raw_request(
            "127.0.0.1",
            proxy.port,
            "POST",
            "/v1beta/models/gemini-2.5-pro:generateContent",
            body=b'{"contents":[]}',
            headers={"Content-Type": "application/json"},
        )
        self.assertEqual(r.status, 200)
        req = mock.last_request()
        assert req is not None
        self.assertEqual(req.path, "/v1beta/models/gemini-2.5-pro:generateContent")

    def test_t2_f4_base_url_trailing_v1beta_normalized(self) -> None:
        """Trailing '/v1beta' on gemini.base_url does NOT duplicate /v1beta/v1beta."""
        env = self.make_env()
        mock = self.start_mock_gemini()
        cfg = env.write_config(
            env.make_config_dict(base_url=mock.base_url_with_v1beta, api_key="AIzaSyNormV1Beta1234")
        )
        proxy = self.start_proxy(env, config_path=cfg)
        r = http_raw_request(
            "127.0.0.1",
            proxy.port,
            "POST",
            "/v1beta/models/gemini-2.5-pro:generateContent",
            body=b'{"contents":[]}',
            headers={"Content-Type": "application/json"},
        )
        self.assertEqual(r.status, 200)
        req = mock.last_request()
        assert req is not None
        self.assertEqual(req.path, "/v1beta/models/gemini-2.5-pro:generateContent")

    def test_t2_f4_base_url_trailing_v1beta_slash_normalized(self) -> None:
        """Trailing '/v1beta/' on gemini.base_url normalizes to single /v1beta/models/..."""
        env = self.make_env()
        mock = self.start_mock_gemini()
        cfg = env.write_config(
            env.make_config_dict(base_url=f"{mock.base_url}/v1beta/", api_key="AIzaSyNormV1BetaSl12")
        )
        proxy = self.start_proxy(env, config_path=cfg)
        r = http_raw_request(
            "127.0.0.1",
            proxy.port,
            "POST",
            "/v1beta/models/gemini-2.5-pro:generateContent",
            body=b'{"contents":[]}',
            headers={"Content-Type": "application/json"},
        )
        self.assertEqual(r.status, 200)
        req = mock.last_request()
        assert req is not None
        self.assertEqual(req.path, "/v1beta/models/gemini-2.5-pro:generateContent")

    def test_t2_f4_strips_query_key_while_preserving_alt_sse(self) -> None:
        """Inbound ?key=LEAKED_QUERY_SECRET&alt=sse strips key= while preserving alt=sse (S2)."""
        env = self.make_env()
        mock = self.start_mock_gemini()
        cfg_key = "AIzaSyConfigHeaderKey1"
        cfg = env.write_config(env.make_config_dict(base_url=mock.base_url, api_key=cfg_key))
        proxy = self.start_proxy(env, config_path=cfg)
        r = http_raw_request(
            "127.0.0.1",
            proxy.port,
            "POST",
            "/v1beta/models/gemini-2.5-pro:streamGenerateContent?key=LEAKED_QUERY_SECRET&alt=sse",
            body=b'{"contents":[]}',
            headers={"Content-Type": "application/json"},
        )
        self.assertEqual(r.status, 200)
        req = mock.last_request()
        assert req is not None
        self.assertEqual(req.query_params.get("alt"), ["sse"])
        self.assertNotIn("key", req.query_params)
        self.assertNotIn("LEAKED_QUERY_SECRET", req.raw_target)
        self.assertEqual(req.header("x-goog-api-key"), cfg_key)

    def test_t2_f4_rejects_dotdot_path_traversal(self) -> None:
        """Raw POST /v1beta/models/../../etc/passwd:generateContent returns 400/404 with 0 upstream calls (A2, S6)."""
        env = self.make_env()
        mock = self.start_mock_gemini()
        cfg = env.write_config(env.make_config_dict(base_url=mock.base_url, api_key="AIzaSyDotDotTest1234"))
        proxy = self.start_proxy(env, config_path=cfg)
        r = http_raw_request(
            "127.0.0.1",
            proxy.port,
            "POST",
            "/v1beta/models/../../etc/passwd:generateContent",
            body=b'{"contents":[]}',
            headers={"Content-Type": "application/json"},
        )
        self.assertIn(r.status, (400, 404))
        self.assertEqual(len(mock.requests), 0, f"Upstream received forbidden traversal: {mock.requests!r}")

    def test_t2_f4_rejects_encoded_and_null_byte_traversal(self) -> None:
        """Percent-encoded %2e%2e and %00 null-byte paths are rejected with 0 upstream calls (A2, S6)."""
        env = self.make_env()
        mock = self.start_mock_gemini()
        cfg = env.write_config(env.make_config_dict(base_url=mock.base_url, api_key="AIzaSyEncTraversal12"))
        proxy = self.start_proxy(env, config_path=cfg)
        for bad_path in (
            "/v1beta/models/%2e%2e/secret:generateContent",
            "/v1beta/models/foo%00bar:generateContent",
        ):
            with self.subTest(path=bad_path):
                r = http_raw_request(
                    "127.0.0.1",
                    proxy.port,
                    "POST",
                    bad_path,
                    body=b'{"contents":[]}',
                    headers={"Content-Type": "application/json"},
                )
                self.assertIn(r.status, (400, 404))
        self.assertEqual(len(mock.requests), 0)

    def test_t2_f4_propagates_upstream_400_429_500_without_key_leak(self) -> None:
        """Upstream 400 API_KEY_INVALID, 429, and 500 propagate verbatim without leaking API key (S2)."""
        env = self.make_env()
        sentinel = "AIzaSyUpstreamErrSecret4444"
        mock = self.start_mock_gemini()
        cases = [
            (
                400,
                {
                    "error": {
                        "code": 400,
                        "status": "INVALID_ARGUMENT",
                        "details": [{"reason": "API_KEY_INVALID"}],
                    }
                },
            ),
            (429, {"error": {"code": 429, "status": "RESOURCE_EXHAUSTED"}}),
            (500, {"error": {"code": 500, "status": "INTERNAL"}}),
        ]
        for code, payload in cases:
            mock.enqueue(JsonAction(status=code, payload=payload))
        cfg = env.write_config(env.make_config_dict(base_url=mock.base_url, api_key=sentinel))
        proxy = self.start_proxy(env, config_path=cfg)

        for code, payload in cases:
            with self.subTest(upstream_status=code):
                r = http_raw_request(
                    "127.0.0.1",
                    proxy.port,
                    "POST",
                    "/v1beta/models/gemini-2.5-pro:generateContent",
                    body=b'{"contents":[]}',
                    headers={"Content-Type": "application/json"},
                )
                self.assertEqual(r.status, code)
                self.assertEqual(r.json(), payload)
                self.assertNotIn(sentinel, r.text)

        self.assertEqual(http_raw_request("127.0.0.1", proxy.port, "GET", "/health").status, 200)
        self.assertNotIn(sentinel, proxy.stderr_text)

    def test_t2_f4_missing_api_key_returns_clean_error(self) -> None:
        """When config key, env GEMINI_API_KEY, and caller header are all absent, proxy errors cleanly without calling upstream."""
        env = self.make_env()
        mock = self.start_mock_gemini()
        cfg = env.write_config(env.make_config_dict(base_url=mock.base_url, api_key=None))
        proxy = self.start_proxy(env, config_path=cfg)
        r = http_raw_request(
            "127.0.0.1",
            proxy.port,
            "POST",
            "/v1beta/models/gemini-2.5-pro:generateContent",
            body=b'{"contents":[]}',
            headers={"Content-Type": "application/json"},
        )
        self.assertGreaterEqual(r.status, 400)
        self.assertLess(r.status, 600)
        self.assertEqual(len(mock.requests), 0)
        self.assertEqual(http_raw_request("127.0.0.1", proxy.port, "GET", "/health").status, 200)


class TestTier2F5ProxyBoundaries(E2ETestCase):
    TIER = 2
    FEATURE = "F5"

    def test_t2_f5_disallowed_methods_return_405(self) -> None:
        """PUT, DELETE, and PATCH on exposed routes return HTTP 405 Method Not Allowed (A12, S7)."""
        env = self.make_env()
        mock = self.start_mock_gemini()
        cfg = env.write_config(env.make_config_dict(base_url=mock.base_url, api_key="AIzaSyMethod405Test12"))
        proxy = self.start_proxy(env, config_path=cfg)
        for method, path in (
            ("PUT", "/health"),
            ("DELETE", "/health"),
            ("PATCH", "/api/stats"),
            ("DELETE", "/v1beta/models/gemini-2.5-pro:generateContent"),
        ):
            with self.subTest(method=method, path=path):
                r = http_raw_request("127.0.0.1", proxy.port, method, path)
                self.assertEqual(r.status, 405, f"Expected 405 for {method} {path}, got {r.status}")

    def test_t2_f5_accepts_request_body_over_2mib_under_64mib(self) -> None:
        """3 MiB JSON request body (exceeding Axum's 2 MiB default) forwards intact with HTTP 200."""
        env = self.make_env()
        mock = self.start_mock_gemini()
        mock.enqueue(EchoChecksumAction())
        cfg = env.write_config(env.make_config_dict(base_url=mock.base_url, api_key="AIzaSyThreeMibBody123"))
        proxy = self.start_proxy(env, config_path=cfg)

        filler = "A" * (3 * 1024 * 1024)
        payload = json.dumps({"contents": [{"parts": [{"text": filler}]}]}).encode("utf-8")
        r = http_raw_request(
            "127.0.0.1",
            proxy.port,
            "POST",
            "/v1beta/models/gemini-2.5-pro:generateContent",
            body=payload,
            headers={"Content-Type": "application/json"},
            timeout=30.0,
        )
        self.assertEqual(r.status, 200)
        req = mock.last_request()
        assert req is not None
        self.assertEqual(len(req.body), len(payload))
        self.assertEqual(req.body, payload)

    def test_t2_f5_rejects_body_above_64mib_with_413(self) -> None:
        """Payload exceeding 64 MiB returns 413 Payload Too Large (handling early socket close) with 0 upstream calls."""
        env = self.make_env()
        mock = self.start_mock_gemini()
        cfg = env.write_config(env.make_config_dict(base_url=mock.base_url, api_key="AIzaSyBodyLimit413Key"))
        proxy = self.start_proxy(env, config_path=cfg)

        oversized = b"x" * (64 * 1024 * 1024 + 1024)
        r = http_raw_request(
            "127.0.0.1",
            proxy.port,
            "POST",
            "/v1beta/models/gemini-2.5-pro:generateContent",
            body=oversized,
            headers={"Content-Type": "application/json"},
            timeout=30.0,
        )
        self.assertEqual(r.status, 413, f"Expected 413 Payload Too Large, got {r.status}")
        self.assertEqual(len(mock.requests), 0)
        self.assertEqual(http_raw_request("127.0.0.1", proxy.port, "GET", "/health").status, 200)

    def test_t2_f5_split_multibyte_utf8_sse_chunk(self) -> None:
        """4-byte UTF-8 emoji split mid-codepoint across TCP chunks forwards intact and parses usageMetadata."""
        env = self.make_env()
        mock = self.start_mock_gemini()
        full_sse = build_sse_events(
            [
                {
                    "candidates": [{"content": {"parts": [{"text": "launch 🚀 rocket"}]}}],
                    "usageMetadata": {
                        "promptTokenCount": 321,
                        "candidatesTokenCount": 65,
                        "cachedContentTokenCount": 100,
                        "thoughtsTokenCount": 11,
                    },
                }
            ]
        )
        chunks = split_across_utf8_char(full_sse, "🚀")
        mock.enqueue(SseStreamAction(chunks=chunks, chunk_delay_s=0.02))
        cfg = env.write_config(env.make_config_dict(base_url=mock.base_url, api_key="AIzaSySplitUtf8Sse123"))
        proxy = self.start_proxy(env, config_path=cfg)

        r = http_raw_request(
            "127.0.0.1",
            proxy.port,
            "POST",
            "/v1beta/models/gemini-2.5-pro:streamGenerateContent?alt=sse",
            body=b'{"contents":[]}',
            headers={"Content-Type": "application/json"},
        )
        self.assertEqual(r.status, 200)
        self.assertEqual(r.body, full_sse)
        self.assertIn("🚀", r.body.decode("utf-8"))

        _, events = poll_usage_events(env.usage_dir, min_count=1)
        self.assertEqual(len(events), 1)
        self.assertEqual(events[0]["input_tokens"], 321)
        stats = poll_stats(proxy.port, min_requests=1)
        self.assertEqual(stats["prompt_tokens"], 321)
        self.assertEqual(stats["output_tokens"], 65)
        self.assertEqual(stats["cached_tokens"], 100)
        self.assertEqual(stats["reasoning_tokens"], 11)

    def test_t2_f5_crlf_sse_framing_extracted(self) -> None:
        """CRLF-framed SSE lines preserve exact bytes and accurately extract usageMetadata."""
        env = self.make_env()
        mock = self.start_mock_gemini()
        crlf_sse = build_sse_events(
            [
                {
                    "candidates": [{"content": {"parts": [{"text": "crlf-ok"}]}}],
                    "usageMetadata": {
                        "promptTokenCount": 250,
                        "candidatesTokenCount": 75,
                        "cachedContentTokenCount": 50,
                        "thoughtsTokenCount": 15,
                    },
                }
            ],
            line_ending=b"\r\n",
        )
        mock.enqueue(SseStreamAction(chunks=[crlf_sse[:24], crlf_sse[24:]]))
        cfg = env.write_config(env.make_config_dict(base_url=mock.base_url, api_key="AIzaSyCrlfSseTest1234"))
        proxy = self.start_proxy(env, config_path=cfg)

        r = http_raw_request(
            "127.0.0.1",
            proxy.port,
            "POST",
            "/v1beta/models/gemini-2.5-pro:streamGenerateContent?alt=sse",
            body=b'{"contents":[]}',
            headers={"Content-Type": "application/json"},
        )
        self.assertEqual(r.status, 200)
        self.assertEqual(r.body, crlf_sse)
        _, events = poll_usage_events(env.usage_dir, min_count=1)
        self.assertEqual(len(events), 1)
        self.assertEqual(events[0]["input_tokens"], 250)
        stats = poll_stats(proxy.port, min_requests=1)
        self.assertEqual(stats["prompt_tokens"], 250)
        self.assertEqual(stats["output_tokens"], 75)

    def test_t2_f5_multiple_usage_metadata_frames_records_last_not_sum(self) -> None:
        """Cumulative SSE usageMetadata chunks record ONLY the final chunk, never summing."""
        env = self.make_env()
        mock = self.start_mock_gemini()
        c1 = build_sse_events(
            [
                {
                    "candidates": [{"content": {"parts": [{"text": "a"}]}}],
                    "usageMetadata": {"promptTokenCount": 100, "candidatesTokenCount": 10, "cachedContentTokenCount": 50},
                }
            ]
        )
        c2 = build_sse_events(
            [
                {
                    "candidates": [{"content": {"parts": [{"text": "b"}]}}],
                    "usageMetadata": {"promptTokenCount": 100, "candidatesTokenCount": 40, "cachedContentTokenCount": 50},
                }
            ]
        )
        c3 = build_sse_events(
            [
                {
                    "candidates": [{"content": {"parts": [{"text": "c"}]}}],
                    "usageMetadata": {"promptTokenCount": 100, "candidatesTokenCount": 75, "cachedContentTokenCount": 50},
                }
            ]
        )
        mock.enqueue(SseStreamAction(chunks=[c1, c2, c3]))
        cfg = env.write_config(env.make_config_dict(base_url=mock.base_url, api_key="AIzaSyLastUsageWins12"))
        proxy = self.start_proxy(env, config_path=cfg)

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
        self.assertEqual(events[0]["input_tokens"], 100)
        self.assertEqual(events[0]["output_tokens"], 75)
        self.assertEqual(events[0]["cached_tokens"], 50)
        stats = poll_stats(proxy.port, min_requests=1)
        self.assertEqual(stats["requests"], 1)
        self.assertEqual(stats["prompt_tokens"], 100)
        self.assertEqual(stats["output_tokens"], 75)
        self.assertEqual(stats["cached_tokens"], 50)

    def test_t2_f5_binary_invalid_utf8_response_passthrough_safe(self) -> None:
        """Invalid UTF-8 binary response passes through untouched without crashing proxy."""
        env = self.make_env()
        mock = self.start_mock_gemini()
        raw_bin = b"\xff\xfe\x00\x80\x81BINARY"
        mock.enqueue(RawBytesAction(status=200, content_type="application/octet-stream", body=raw_bin))
        cfg = env.write_config(env.make_config_dict(base_url=mock.base_url, api_key="AIzaSyBinaryResp12345"))
        proxy = self.start_proxy(env, config_path=cfg)

        r = http_raw_request(
            "127.0.0.1",
            proxy.port,
            "POST",
            "/v1beta/models/gemini-2.5-pro:generateContent",
            body=b'{"contents":[]}',
            headers={"Content-Type": "application/json"},
        )
        self.assertEqual(r.status, 200)
        self.assertEqual(r.body, raw_bin)
        self.assertEqual(http_raw_request("127.0.0.1", proxy.port, "GET", "/health").status, 200)

    def test_t2_f5_empty_sse_stream_completes_cleanly(self) -> None:
        """Empty text/event-stream completes with HTTP 200 and finite 0.0 cache_hit_ratio."""
        env = self.make_env()
        mock = self.start_mock_gemini()
        mock.enqueue(SseStreamAction(chunks=[]))
        cfg = env.write_config(env.make_config_dict(base_url=mock.base_url, api_key="AIzaSyEmptySseStream1"))
        proxy = self.start_proxy(env, config_path=cfg)

        r = http_raw_request(
            "127.0.0.1",
            proxy.port,
            "POST",
            "/v1beta/models/gemini-2.5-pro:streamGenerateContent?alt=sse",
            body=b'{"contents":[]}',
            headers={"Content-Type": "application/json"},
        )
        self.assertEqual(r.status, 200)
        self.assertEqual(r.body, b"")
        self.assertEqual(http_raw_request("127.0.0.1", proxy.port, "GET", "/health").status, 200)
        stats = http_raw_request("127.0.0.1", proxy.port, "GET", "/api/stats").json()
        self.assertTrue(math.isfinite(stats["cache_hit_ratio"]))
        self.assertEqual(stats["cache_hit_ratio"], 0.0)


class TestTier2F6HealthAndStatsBoundaries(E2ETestCase):
    TIER = 2
    FEATURE = "F6"

    def test_t2_f6_cache_hit_ratio_zero_when_prompt_tokens_zero(self) -> None:
        """cache_hit_ratio is strictly finite 0.0 (never NaN/null) when prompt_tokens == 0."""
        env = self.make_env()
        mock = self.start_mock_gemini(
            default_usage={"promptTokenCount": 0, "cachedContentTokenCount": 0, "candidatesTokenCount": 0}
        )
        cfg = env.write_config(env.make_config_dict(base_url=mock.base_url, api_key="AIzaSyZeroRatioTest12"))
        proxy = self.start_proxy(env, config_path=cfg)

        s0 = http_raw_request("127.0.0.1", proxy.port, "GET", "/api/stats").json()
        self.assertEqual(s0["cache_hit_ratio"], 0.0)

        r = http_raw_request(
            "127.0.0.1",
            proxy.port,
            "POST",
            "/v1beta/models/gemini-2.5-pro:generateContent",
            body=b'{"contents":[]}',
            headers={"Content-Type": "application/json"},
        )
        self.assertEqual(r.status, 200)
        s1 = http_raw_request("127.0.0.1", proxy.port, "GET", "/api/stats").json()
        self.assertIsInstance(s1["cache_hit_ratio"], (int, float))
        self.assertTrue(math.isfinite(s1["cache_hit_ratio"]))
        self.assertEqual(s1["cache_hit_ratio"], 0.0)

    def test_t2_f6_cache_hit_ratio_one_when_fully_cached(self) -> None:
        """cache_hit_ratio equals 1.0 when cachedContentTokenCount == promptTokenCount."""
        env = self.make_env()
        mock = self.start_mock_gemini(
            default_usage={"promptTokenCount": 500, "cachedContentTokenCount": 500, "candidatesTokenCount": 25}
        )
        cfg = env.write_config(env.make_config_dict(base_url=mock.base_url, api_key="AIzaSyFullCache123456"))
        proxy = self.start_proxy(env, config_path=cfg)
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
        self.assertAlmostEqual(stats["cache_hit_ratio"], 1.0, places=6)

    def test_t2_f6_security_headers_present_on_both_health_and_stats(self) -> None:
        """Both /health and /api/stats set nosniff and no-store security headers (S7)."""
        env = self.make_env()
        cfg = env.write_config(env.make_config_dict())
        proxy = self.start_proxy(env, config_path=cfg)
        for route in ("/health", "/api/stats"):
            with self.subTest(route=route):
                r = http_raw_request("127.0.0.1", proxy.port, "GET", route)
                self.assertEqual(r.status, 200)
                self.assertEqual((r.header("x-content-type-options") or "").lower(), "nosniff")
                self.assertIn("no-store", (r.header("cache-control") or "").lower())

    def test_t2_f6_health_protocol_version_is_integer_1(self) -> None:
        """GET /health returns protocol_version as strict JSON integer 1 (not string or bool)."""
        env = self.make_env()
        cfg = env.write_config(env.make_config_dict())
        proxy = self.start_proxy(env, config_path=cfg)
        body = http_raw_request("127.0.0.1", proxy.port, "GET", "/health").json()
        self.assertIs(type(body.get("protocol_version")), int)
        self.assertEqual(body["protocol_version"], 1)

    def test_t2_f6_nonexistent_routes_return_404(self) -> None:
        """Unknown routes return 404 Not Found and POST /health returns 405 Method Not Allowed (S7)."""
        env = self.make_env()
        cfg = env.write_config(env.make_config_dict())
        proxy = self.start_proxy(env, config_path=cfg)
        self.assertEqual(http_raw_request("127.0.0.1", proxy.port, "GET", "/nonexistent").status, 404)
        self.assertEqual(http_raw_request("127.0.0.1", proxy.port, "GET", "/v1beta/unknown").status, 404)
        self.assertEqual(http_raw_request("127.0.0.1", proxy.port, "POST", "/health").status, 405)

    def test_t2_f6_stats_does_not_leak_secrets_or_prompt_bodies(self) -> None:
        """Neither /api/stats nor /health leaks API keys, prompt text, or response text (S2)."""
        env = self.make_env()
        secret = "AIzaSyStatsNoLeak9999"
        prompt_marker = "STATS_SECRET_PROMPT_XYZ"
        resp_marker = "STATS_SECRET_RESP_ABC"
        mock = self.start_mock_gemini()
        mock.enqueue(
            JsonAction(
                status=200,
                payload={
                    "candidates": [{"content": {"parts": [{"text": resp_marker}]}}],
                    "usageMetadata": {"promptTokenCount": 50, "candidatesTokenCount": 10},
                },
            )
        )
        cfg = env.write_config(env.make_config_dict(base_url=mock.base_url, api_key=secret))
        proxy = self.start_proxy(env, config_path=cfg)
        http_raw_request(
            "127.0.0.1",
            proxy.port,
            "POST",
            "/v1beta/models/gemini-2.5-pro:generateContent",
            body=json.dumps({"contents": [{"parts": [{"text": prompt_marker}]}]}).encode("utf-8"),
            headers={"Content-Type": "application/json"},
        )
        for route in ("/api/stats", "/health"):
            raw = http_raw_request("127.0.0.1", proxy.port, "GET", route).text
            self.assertNotIn(secret, raw)
            self.assertNotIn(prompt_marker, raw)
            self.assertNotIn(resp_marker, raw)


class TestTier2F7UiBoundaries(E2ETestCase):
    TIER = 2
    FEATURE = "F7"

    def test_t2_f7_dist_html_size_under_500kb(self) -> None:
        """ui/dist/index.html is strictly > 0 and < 500 KiB."""
        dist = self.repo_root / "ui" / "dist" / "index.html"
        self.assertTrue(dist.is_file(), f"Missing {dist}")
        size = os.path.getsize(dist)
        self.assertGreater(size, 0)
        self.assertLess(size, 500 * 1024, f"UI bundle too large: {size} bytes")

    def test_t2_f7_zero_forbidden_xss_dom_sinks_in_ui_src(self) -> None:
        """ui/src/ has strictly 0 hits for forbidden XSS/DOM execution sinks (S4)."""
        src_dir = self.repo_root / "ui" / "src"
        self.assertTrue(src_dir.is_dir(), f"Missing {src_dir}")
        pattern = re.compile(r"innerHTML|outerHTML|document\.write|dangerouslySetInnerHTML|\beval\(")
        hits: list[str] = []
        for path in sorted(src_dir.rglob("*")):
            if not path.is_file():
                continue
            for lineno, line in enumerate(path.read_text(encoding="utf-8").splitlines(), 1):
                if pattern.search(line):
                    hits.append(f"{path.relative_to(self.repo_root)}:{lineno}: {line.strip()}")
        self.assertEqual(hits, [], f"Forbidden XSS/DOM sinks found:\n" + "\n".join(hits))

    def test_t2_f7_zero_storage_and_blocking_dialogs_in_ui_src(self) -> None:
        """ui/src/ has strictly 0 hits for localStorage/sessionStorage/alert/confirm/prompt (S4)."""
        src_dir = self.repo_root / "ui" / "src"
        self.assertTrue(src_dir.is_dir(), f"Missing {src_dir}")
        pattern = re.compile(r"localStorage|sessionStorage|\balert\(|\bconfirm\(|\bprompt\(")
        hits: list[str] = []
        for path in sorted(src_dir.rglob("*")):
            if not path.is_file():
                continue
            for lineno, line in enumerate(path.read_text(encoding="utf-8").splitlines(), 1):
                if pattern.search(line):
                    hits.append(f"{path.relative_to(self.repo_root)}:{lineno}: {line.strip()}")
        self.assertEqual(hits, [], f"Forbidden storage/dialog APIs found:\n" + "\n".join(hits))

    def test_t2_f7_no_external_cdn_scripts_or_stylesheets_in_bundle(self) -> None:
        """ui/dist/index.html has zero external http(s) script or stylesheet links (S4)."""
        dist = self.repo_root / "ui" / "dist" / "index.html"
        self.assertTrue(dist.is_file(), f"Missing {dist}")
        html = dist.read_text(encoding="utf-8")
        self.assertIsNone(re.search(r'<script[^>]+src=["\']https?://', html, re.IGNORECASE))
        self.assertIsNone(re.search(r'<link[^>]+href=["\']https?://', html, re.IGNORECASE))

    def test_t2_f7_npmrc_overrides_airlock_registry(self) -> None:
        """ui/.npmrc pins public https://registry.npmjs.org/ with zero internal/auth tokens."""
        npmrc = self.repo_root / "ui" / ".npmrc"
        self.assertTrue(npmrc.is_file(), f"Missing {npmrc}")
        lines = [ln.strip() for ln in npmrc.read_text(encoding="utf-8").splitlines() if ln.strip() and not ln.strip().startswith("#")]
        self.assertIn("registry=https://registry.npmjs.org/", lines)
        for ln in lines:
            self.assertNotIn("_authToken", ln)
            self.assertNotIn("corp.google.com", ln.lower())

    def test_t2_f7_gitignore_explicitly_unignores_dist_index_html(self) -> None:
        """.gitignore contains !ui/dist/index.html AND ui/dist/index.html is git-tracked."""
        gi = (self.repo_root / ".gitignore").read_text(encoding="utf-8").splitlines()
        self.assertIn("!ui/dist/index.html", [ln.strip() for ln in gi])
        res = run_cli(["git", "-C", str(self.repo_root), "ls-files", "ui/dist/index.html"])
        self.assertEqual(res.returncode, 0, res.stderr)
        self.assertEqual(res.stdout.strip(), "ui/dist/index.html")


class TestTier2F8McpBoundaries(E2ETestCase):
    TIER = 2
    FEATURE = "F8"

    def test_t2_f8_unknown_resource_uri_returns_error(self) -> None:
        """resources/read on unknown URI returns JSON-RPC error while keeping server alive."""
        env = self.make_env()
        mcp = self.start_mcp(env)
        resp = mcp.read_resource_raw("ui://companion/does-not-exist.html")
        self.assertIn("error", resp, f"Expected JSON-RPC error, got {resp!r}")
        # Server must remain responsive after error.
        res_list = mcp.list_resources()
        self.assertIn("resources", res_list)
        mcp.assert_clean_stdout()

    def test_t2_f8_save_settings_rejects_non_loopback_host(self) -> None:
        """Attempting listen_host='0.0.0.0' via save_settings never persists '0.0.0.0' (S1)."""
        env = self.make_env()
        cfg = env.write_config(env.make_config_dict())
        mcp = self.start_mcp(env, config_path=cfg)
        props = mcp.get_save_settings_schema()
        resp = mcp.save_settings_smart(listen_host="0.0.0.0", raw=True)
        mcp.assert_clean_stdout()
        if "listen_host" in props or "host" in props:
            self.assertTrue(
                McpStdioClient.is_tool_error(resp),
                f"Expected rejection when listen_host='0.0.0.0' is in schema, got {resp!r}",
            )
        disk = json.loads(cfg.read_text(encoding="utf-8"))
        self.assertNotEqual(disk.get("listen_host"), "0.0.0.0")
        self.assertIn(disk.get("listen_host"), ("127.0.0.1", "localhost"))

    def test_t2_f8_save_settings_rejects_remote_http_and_port_zero(self) -> None:
        """save_settings rejects remote http:// base_url and listen_port==0 without corrupting disk config (S5)."""
        env = self.make_env()
        cfg = env.write_config(env.make_config_dict(base_url="https://generativelanguage.googleapis.com", listen_port=4141))
        mcp = self.start_mcp(env, config_path=cfg)

        with self.subTest(case="remote_http_base_url"):
            r1 = mcp.save_settings_smart(base_url="http://example.com", raw=True)
            self.assertTrue(McpStdioClient.is_tool_error(r1), f"Expected error for http://example.com, got {r1!r}")
            disk = json.loads(cfg.read_text(encoding="utf-8"))
            self.assertEqual(disk["gemini"]["base_url"], "https://generativelanguage.googleapis.com")

        with self.subTest(case="port_zero"):
            r2 = mcp.save_settings_smart(listen_port=0, raw=True)
            self.assertTrue(McpStdioClient.is_tool_error(r2), f"Expected error for listen_port=0, got {r2!r}")
            disk = json.loads(cfg.read_text(encoding="utf-8"))
            self.assertEqual(disk["listen_port"], 4141)

        mcp.assert_clean_stdout()

    def test_t2_f8_save_settings_omitting_api_key_preserves_existing_key(self) -> None:
        """save_settings with omitted or empty api_key preserves previously stored key."""
        env = self.make_env()
        secret = "AIzaSyPreserveMe12345678"
        cfg = env.write_config(env.make_config_dict(api_key=secret))
        mcp = self.start_mcp(env, config_path=cfg)

        mcp.save_settings_smart(default_model="gemini-2.5-flash")
        mcp.save_settings_smart(default_model="gemini-2.5-pro", api_key="")
        res = mcp.call_tool("get_settings", {})
        mcp.assert_clean_stdout()

        sc = res.get("structuredContent", {})
        found_c, configured = find_key_recursive(sc, "api_key_configured")
        found_h, hint = find_key_recursive(sc, "api_key_hint")
        self.assertTrue(found_c and configured is True)
        self.assertTrue(found_h and hint == "AIza...5678")
        disk = json.loads(cfg.read_text(encoding="utf-8"))
        self.assertEqual(disk["gemini"]["api_key"], secret)

    def test_t2_f8_save_settings_clear_api_key_true_removes_key(self) -> None:
        """save_settings(clear_api_key=True) clears api_key in memory and on disk."""
        env = self.make_env()
        cfg = env.write_config(env.make_config_dict(api_key="AIzaSyClearMe12345678"))
        mcp = self.start_mcp(env, config_path=cfg)

        mcp.save_settings_smart(clear_api_key=True)
        res = mcp.call_tool("get_settings", {})
        mcp.assert_clean_stdout()
        sc = res.get("structuredContent", {})
        found_c, configured = find_key_recursive(sc, "api_key_configured")
        self.assertTrue(found_c and configured is False)
        found_h, hint = find_key_recursive(sc, "api_key_hint")
        if found_h:
            self.assertIsNone(hint)
        disk = json.loads(cfg.read_text(encoding="utf-8"))
        self.assertIsNone(disk.get("gemini", {}).get("api_key"))

    def test_t2_f8_ui_mcp_stdout_strictly_json_rpc_only(self) -> None:
        """Full MCP session with RUST_LOG=debug emits strictly JSON-RPC 2.0 lines on stdout."""
        env = self.make_env()
        mock = self.start_mock_gemini(expected_api_key="AIzaSyStdoutPurity123")
        cfg = env.write_config(env.make_config_dict(base_url=mock.base_url, api_key="AIzaSyStdoutPurity123"))
        mcp = self.start_mcp(env, config_path=cfg, extra_env={"RUST_LOG": "debug"})
        mcp.list_resources()
        mcp.read_resource("ui://companion/settings.html")
        mcp.list_tools()
        mcp.call_tool("open_settings", {})
        mcp.call_tool("get_settings", {})
        mcp.save_settings_smart(default_model="gemini-2.5-flash")
        mcp.call_tool("test_connection", {})
        mcp.assert_clean_stdout()
        self.assertGreater(len(mcp.raw_stdout_lines), 0)

    def test_t2_f8_test_connection_invalid_key_returns_ok_false_no_leak(self) -> None:
        """test_connection reports failure on upstream 400 API_KEY_INVALID without leaking key (S2)."""
        env = self.make_env()
        bad_key = "AIzaSyBadConnKey8888"
        mock = self.start_mock_gemini(expected_api_key="AIzaSyDifferentExpected")
        cfg = env.write_config(env.make_config_dict(base_url=mock.base_url, api_key=bad_key))
        mcp = self.start_mcp(env, config_path=cfg)
        resp = mcp.call_tool_raw("test_connection", {})
        mcp.assert_clean_stdout()

        raw = json.dumps(resp)
        self.assertNotIn(bad_key, raw)
        self.assertNotIn(bad_key, mcp.stderr_text)
        if not McpStdioClient.is_tool_error(resp):
            sc = resp.get("result", {}).get("structuredContent", {})
            found_ok, ok_val = find_key_recursive(sc, "ok")
            self.assertTrue(found_ok and ok_val is False, f"Expected ok=False on 400 API_KEY_INVALID, got {resp!r}")


class TestTier2F9CliBoundaries(E2ETestCase):
    TIER = 2
    FEATURE = "F9"

    def test_t2_f9_unknown_subcommand_exits_nonzero(self) -> None:
        """Unknown subcommand exits non-zero with usage error on stderr."""
        binary = self.require_binary()
        env = self.make_env()
        res = run_cli([str(binary), "nonexistent-cmd"], env=env.build_env())
        self.assertNotEqual(res.returncode, 0)
        self.assertTrue(res.stderr.strip())

    def test_t2_f9_unknown_flag_exits_nonzero(self) -> None:
        """Unknown flag on serve exits non-zero with clap error on stderr."""
        binary = self.require_binary()
        env = self.make_env()
        res = run_cli([str(binary), "serve", "--no-such-flag"], env=env.build_env())
        self.assertNotEqual(res.returncode, 0)
        self.assertTrue(res.stderr.strip())

    def test_t2_f9_doctor_detects_insecure_0644_config_perms(self) -> None:
        """doctor flags world/group-readable 0644 config file permissions (S3)."""
        binary = self.require_binary()
        env = self.make_env()
        port = find_free_port()
        cfg = env.write_config(env.make_config_dict(listen_port=port, api_key="AIzaSyDocPerm0644123"), mode=0o644)
        proxy = self.start_proxy(env, config_path=cfg, port=port)
        doc_env = env.build_env(
            extra={
                "GEMINI_API_KEY": "AIzaSyDocPerm0644123",
                "GOOGLE_GEMINI_BASE_URL": f"http://127.0.0.1:{proxy.port}",
            }
        )
        res = run_cli([str(binary), "doctor", "--config", str(cfg), "--port", str(proxy.port)], env=doc_env)
        combined = (res.stdout + "\n" + res.stderr).lower()
        self.assertNotEqual(res.returncode, 0, f"doctor should fail on 0644 config:\n{combined}")
        self.assertTrue(
            any(tok in combined for tok in ("0600", "0644", "600", "644", "perm", "mode", "readable")),
            f"Expected permission diagnostic in doctor output:\n{combined}",
        )

    def test_t2_f9_doctor_detects_proxy_down(self) -> None:
        """doctor exits non-zero when proxy port is unbound/unreachable."""
        binary = self.require_binary()
        env = self.make_env()
        unused_port = find_free_port()
        cfg = env.write_config(env.make_config_dict(listen_port=unused_port), mode=0o600)
        res = run_cli(
            [str(binary), "doctor", "--config", str(cfg), "--port", str(unused_port)],
            env=env.build_env(),
        )
        self.assertNotEqual(res.returncode, 0, f"doctor should fail when proxy is down:\n{res.stdout}")

    def test_t2_f9_doctor_detects_binary_vs_proxy_version_skew(self) -> None:
        """doctor exits non-zero and flags version skew when /health returns '9.9.9'."""
        binary = self.require_binary()
        env = self.make_env()
        mock_health = self.start_mock_health(version="9.9.9", protocol_version=1)
        cfg = env.write_config(env.make_config_dict(listen_port=mock_health.port), mode=0o600)
        doc_env = env.build_env(
            extra={
                "GEMINI_API_KEY": "AIzaSySkewCheck12345",
                "GOOGLE_GEMINI_BASE_URL": mock_health.base_url,
            }
        )
        res = run_cli(
            [str(binary), "doctor", "--config", str(cfg), "--port", str(mock_health.port)],
            env=doc_env,
        )
        combined = (res.stdout + "\n" + res.stderr).lower()
        self.assertNotEqual(res.returncode, 0, f"doctor should fail on version skew:\n{combined}")
        self.assertTrue(
            "9.9.9" in combined or "skew" in combined or "mismatch" in combined or "version" in combined,
            f"Expected version skew mention in doctor output:\n{combined}",
        )

    def test_t2_f9_models_custom_port_reflected_in_json(self) -> None:
        """agy-companion models --json --port 15151 emits URLs bound to port 15151."""
        binary = self.require_binary()
        env = self.make_env()
        res = run_cli([str(binary), "models", "--json", "--port", "15151"], env=env.build_env())
        self.assertEqual(res.returncode, 0, res.stderr)
        models = extract_models_list(res.json_stdout())
        self.assertGreater(len(models), 0)
        self.assertTrue(
            all("127.0.0.1:15151" in str(m.get("modelName", "")) for m in models),
            f"Expected port 15151 in all customModels: {models!r}",
        )
        self.assertFalse(any("127.0.0.1:4141" in str(m.get("modelName", "")) for m in models))


class TestTier2F10PluginBoundaries(E2ETestCase):
    TIER = 2
    FEATURE = "F10"

    def test_t2_f10_mcp_config_command_has_no_plugin_root_var(self) -> None:
        """plugin/mcp_config.json never puts ${PLUGIN_ROOT} or ${PLUGIN_DATA} in 'command' (S8)."""
        mc = json.loads((self.repo_root / "plugin" / "mcp_config.json").read_text(encoding="utf-8"))
        servers = mc.get("mcpServers") or {}
        self.assertGreater(len(servers), 0)
        for name, spec in servers.items():
            cmd = str(spec.get("command", ""))
            self.assertNotIn("${PLUGIN_ROOT}", cmd, f"Server {name} puts PLUGIN_ROOT in command")
            self.assertNotIn("${PLUGIN_DATA}", cmd, f"Server {name} puts PLUGIN_DATA in command")

    def test_t2_f10_run_companion_sh_executable_valid_and_has_sha256(self) -> None:
        """run-companion.sh is executable, passes bash -n, and verifies SHA-256 via sha256sum & shasum (S6)."""
        script = self.repo_root / "plugin" / "scripts" / "run-companion.sh"
        self.assertTrue(script.is_file(), f"Missing {script}")
        mode = stat.S_IMODE(os.stat(script).st_mode)
        self.assertTrue(mode & 0o111, f"Script lacks executable bits: {oct(mode)}")
        res = run_cli(["bash", "-n", str(script)])
        self.assertEqual(res.returncode, 0, res.stderr)
        text = script.read_text(encoding="utf-8")
        self.assertIn("sha256sum", text)
        self.assertIn("shasum -a 256", text)

    def test_t2_f10_skill_md_size_limit_and_matching_name(self) -> None:
        """plugin/skills/companion/SKILL.md is <= 24000 bytes and frontmatter name matches parent dir."""
        skill = self.repo_root / "plugin" / "skills" / "companion" / "SKILL.md"
        self.assertTrue(skill.is_file(), f"Missing {skill}")
        raw = skill.read_bytes()
        self.assertLessEqual(len(raw), 24000, f"SKILL.md exceeds 24000 bytes: {len(raw)}")
        text = raw.decode("utf-8")
        parts = text.split("---", 2)
        self.assertGreaterEqual(len(parts), 3)
        match = re.search(r"(?m)^name:\s*([a-z0-9-]+)\s*$", parts[1])
        self.assertIsNotNone(match, f"Invalid or missing skill name in frontmatter:\n{parts[1]}")
        assert match is not None
        self.assertEqual(match.group(1), skill.parent.name)

    def test_t2_f10_sidecar_restart_policy_and_allowed_keys(self) -> None:
        """sidecar.json enforces restart_policy='on-failure' and strictly uses allowed keys."""
        sc = json.loads(
            (self.repo_root / "plugin" / "sidecars" / "proxy" / "sidecar.json").read_text(encoding="utf-8")
        )
        self.assertEqual(sc.get("restart_policy"), "on-failure")
        allowed = {
            "command",
            "args",
            "env",
            "restart_policy",
            "display_name",
            "description",
            "has_web_ui",
            "agent_permissions",
            "ui_config",
        }
        unknown = set(sc.keys()) - allowed
        self.assertEqual(unknown, set(), f"Unexpected sidecar.json keys: {unknown!r}")

    def test_t2_f10_plugin_json_strict_schema_constraints(self) -> None:
        """plugin/plugin.json satisfies strict v1 schema key and field constraints."""
        pj = json.loads((self.repo_root / "plugin" / "plugin.json").read_text(encoding="utf-8"))
        self.assertRegex(str(pj.get("name", "")), r"^[a-zA-Z0-9_-]+$")
        prompts = pj.get("suggestedPrompts") or []
        self.assertLessEqual(len(prompts), 3)
        self.assertTrue(isinstance(pj.get("version"), str) and bool(pj["version"].strip()))
        allowed = {
            "$schema",
            "name",
            "displayName",
            "display_name",
            "description",
            "version",
            "disabled",
            "logo",
            "suggestedPrompts",
            "suggested_prompts",
            "skills",
            "author",
            "homepage",
            "repository",
            "license",
            "keywords",
            "extensions",
        }
        unknown = set(pj.keys()) - allowed
        self.assertEqual(unknown, set(), f"Unknown keys in plugin.json: {unknown!r}")

    def test_t2_f10_run_companion_sh_rejects_bad_override_binary_cleanly(self) -> None:
        """run-companion.sh in an isolated dir with nonexistent AGY_COMPANION_BIN and no curl/wget fails cleanly."""
        src_script = self.repo_root / "plugin" / "scripts" / "run-companion.sh"
        self.assertTrue(src_script.is_file(), f"Missing {src_script}")
        env = self.make_env()

        # Copy script (and plugin.json) into an isolated plugin dir with NO sibling target/ directory
        # so F15 dev-checkout auto-detection cannot discover <repo_root>/target/debug/agy-companion.
        iso_plugin = env.root / "isolated_plugin"
        iso_scripts = iso_plugin / "scripts"
        iso_scripts.mkdir(parents=True, exist_ok=True)
        iso_script = iso_scripts / "run-companion.sh"
        shutil.copy2(src_script, iso_script)
        os.chmod(iso_script, 0o755)
        src_plugin_json = self.repo_root / "plugin" / "plugin.json"
        if src_plugin_json.is_file():
            shutil.copy2(src_plugin_json, iso_plugin / "plugin.json")

        # Build a controlled PATH with standard POSIX utilities ONLY (explicitly excluding agy-companion, curl, wget).
        safe_bin = env.root / "safe_bin"
        safe_bin.mkdir(parents=True, exist_ok=True)
        posix_tools = (
            "sh",
            "bash",
            "dirname",
            "basename",
            "readlink",
            "realpath",
            "uname",
            "cat",
            "sed",
            "grep",
            "awk",
            "tr",
            "mkdir",
            "rm",
            "mktemp",
            "env",
            "head",
            "cut",
            "chmod",
            "mv",
            "cp",
            "shasum",
            "sha256sum",
        )
        for tool in posix_tools:
            found = shutil.which(tool, path="/usr/bin:/bin")
            if found:
                os.symlink(found, safe_bin / tool)

        proc_env = env.build_env(
            extra={
                "AGY_COMPANION_BIN": "/nonexistent/path/agy-companion",
                "ANTIGRAVITY_EXECUTABLE_DATA_DIR": str(env.root / "no_cache"),
                "PATH": str(safe_bin),
            }
        )
        res = run_cli(["/bin/bash", str(iso_script), "--version"], env=proc_env, timeout=10.0)
        self.assertNotEqual(res.returncode, 0, f"Expected non-zero exit with bad override, stdout={res.stdout!r}")


class TestTier2F11PackagingBoundaries(E2ETestCase):
    TIER = 2
    FEATURE = "F11"

    def test_t2_f11_npm_shim_uses_execfilesync_no_shell(self) -> None:
        """bin/agy-companion.js uses execFileSync/spawnSync without shell execution (S6)."""
        shim = self.repo_root / "packages" / "npm" / "agy-companion" / "bin" / "agy-companion.js"
        self.assertTrue(shim.is_file(), f"Missing {shim}")
        text = shim.read_text(encoding="utf-8")
        self.assertTrue("execFileSync" in text or "spawnSync" in text)
        self.assertIsNone(re.search(r"\bexecSync\s*\(", text))
        self.assertIsNone(re.search(r"(?<![A-Za-z0-9_])exec\s*\(", text))
        self.assertIsNone(re.search(r"shell\s*:\s*true", text))

    def test_t2_f11_npm_shim_has_hardcoded_platform_arch_allowlist(self) -> None:
        """bin/agy-companion.js hardcodes all 5 allowed (platform, arch) platform packages (S6)."""
        shim = self.repo_root / "packages" / "npm" / "agy-companion" / "bin" / "agy-companion.js"
        self.assertTrue(shim.is_file(), f"Missing {shim}")
        text = shim.read_text(encoding="utf-8")
        for target in (
            "@agy-companion/darwin-arm64",
            "@agy-companion/darwin-x64",
            "@agy-companion/linux-x64",
            "@agy-companion/linux-arm64",
            "@agy-companion/win32-x64",
        ):
            self.assertIn(target, text, f"Missing hardcoded allow-list entry {target}")

    def test_t2_f11_npm_shim_rejects_unsupported_platform_cleanly(self) -> None:
        """Running bin/agy-companion.js as a CLI on unsupported platform 'freebsd' exits non-zero cleanly."""
        shim = self.repo_root / "packages" / "npm" / "agy-companion" / "bin" / "agy-companion.js"
        self.assertTrue(shim.is_file(), f"Missing {shim}")
        env = self.make_env()
        preload = env.tmp_dir / "override_platform.cjs"
        preload.write_text(
            "Object.defineProperty(process, 'platform', { value: 'freebsd', configurable: true });\n"
            "Object.defineProperty(process, 'arch', { value: 'mips', configurable: true });\n",
            encoding="utf-8",
        )
        res = run_cli(
            ["node", "-r", str(preload), str(shim)],
            env=env.build_env(),
            cwd=self.repo_root,
        )
        self.assertNotEqual(res.returncode, 0, "Expected non-zero exit on unsupported platform")
        self.assertTrue(res.stderr.strip(), "Expected diagnostic error on stderr")
        combined = (res.stdout + "\n" + res.stderr).lower()
        for js_err in ("syntaxerror", "referenceerror", "typeerror"):
            self.assertNotIn(js_err, combined, f"Unexpected JS runtime error ({js_err}):\n{combined}")
        self.assertTrue(
            "freebsd" in combined or "unsupported" in combined or "platform" in combined,
            f"Expected clear unsupported platform message, got:\n{combined}",
        )

    def test_t2_f11_all_npm_js_mjs_pass_node_check(self) -> None:
        """Every *.js and *.mjs file under packages/npm/ passes node --check."""
        npm_dir = self.repo_root / "packages" / "npm"
        self.assertTrue(npm_dir.is_dir(), f"Missing {npm_dir}")
        scripts = sorted(
            p
            for p in list(npm_dir.rglob("*.js")) + list(npm_dir.rglob("*.mjs"))
            if "node_modules" not in p.parts
        )
        self.assertGreaterEqual(len(scripts), 2)
        for path in scripts:
            with self.subTest(script=str(path.relative_to(self.repo_root))):
                res = run_cli(["node", "--check", str(path)])
                self.assertEqual(res.returncode, 0, res.stderr)

    def test_t2_f11_four_way_version_sync_detects_all_four_files(self) -> None:
        """All 4 versioned manifests exist, parse as X.Y.Z semver, and match identically."""
        v_cargo = _extract_cargo_version(self.repo_root)
        v_plugin = json.loads((self.repo_root / "plugin" / "plugin.json").read_text(encoding="utf-8"))["version"]
        v_npm = json.loads(
            (self.repo_root / "packages" / "npm" / "agy-companion" / "package.json").read_text(encoding="utf-8")
        )["version"]
        v_py = tomllib.loads(
            (self.repo_root / "packages" / "python" / "pyproject.toml").read_text(encoding="utf-8")
        )["project"]["version"]
        semver = re.compile(r"^\d+\.\d+\.\d+$")
        for label, ver in (("cargo", v_cargo), ("plugin", v_plugin), ("npm", v_npm), ("python", v_py)):
            self.assertRegex(str(ver), semver, f"Non-semver version for {label}: {ver!r}")
        self.assertEqual(len({v_cargo, v_plugin, v_npm, v_py}), 1)

    def test_t2_f11_python_pyproject_bindings_bin_and_keywords(self) -> None:
        """packages/python/pyproject.toml specifies bindings='bin' and required SEO keywords."""
        data = tomllib.loads((self.repo_root / "packages" / "python" / "pyproject.toml").read_text(encoding="utf-8"))
        self.assertEqual(data.get("tool", {}).get("maturin", {}).get("bindings"), "bin")
        kws = {str(k).lower() for k in (data.get("project", {}).get("keywords") or [])}
        for required in ("antigravity", "agy", "byom", "gemini"):
            self.assertIn(required, kws)


class TestTier2F12AgentCheckBoundaries(E2ETestCase):
    TIER = 2
    FEATURE = "F12"

    def test_t2_f12_agent_check_executable_and_bash_n_valid(self) -> None:
        """scripts/agent_check.sh has executable bit set and passes bash -n."""
        script = self.repo_root / "scripts" / "agent_check.sh"
        self.assertTrue(script.is_file(), f"Missing {script}")
        self.assertTrue(stat.S_IMODE(os.stat(script).st_mode) & 0o111)
        res = run_cli(["bash", "-n", str(script)])
        self.assertEqual(res.returncode, 0, res.stderr)

    def test_t2_f12_all_repo_shell_scripts_pass_bash_n(self) -> None:
        """All *.sh scripts in scripts/ and plugin/scripts/ pass bash -n."""
        dirs = [self.repo_root / "scripts", self.repo_root / "plugin" / "scripts"]
        scripts: list[Path] = []
        for d in dirs:
            if d.is_dir():
                scripts.extend(sorted(d.glob("*.sh")))
        self.assertGreaterEqual(len(scripts), 2)
        for s in scripts:
            with self.subTest(script=str(s.relative_to(self.repo_root))):
                res = run_cli(["bash", "-n", str(s)])
                self.assertEqual(res.returncode, 0, res.stderr)

    def test_t2_f12_ci_yml_kept_in_sync_with_agent_check(self) -> None:
        """.github/workflows/ci.yml uses fetch-depth: 0 and invokes ./scripts/agent_check.sh."""
        text = (self.repo_root / ".github" / "workflows" / "ci.yml").read_text(encoding="utf-8")
        self.assertIn("./scripts/agent_check.sh", text)
        self.assertIn("fetch-depth: 0", text)

    def test_t2_f12_no_gemini_md_at_repo_root(self) -> None:
        """GEMINI.md does not exist at repository root."""
        self.assertFalse((self.repo_root / "GEMINI.md").exists())

    def test_t2_f12_no_forbidden_runtime_junk_tracked(self) -> None:
        """git ls-files tracks zero runtime junk, build dirs, or secret files (S8)."""
        res = run_cli(["git", "-C", str(self.repo_root), "ls-files"])
        self.assertEqual(res.returncode, 0, res.stderr)
        forbidden = re.compile(
            r"(^|/)(\.antigravitycli|\.remember|target|node_modules)/|"
            r"(^|/)\.DS_Store$|(^|/)\.env($|\.)|\.(pem|key)$"
        )
        offenders = [
            f
            for f in res.stdout.splitlines()
            if f and not f.endswith(".env.example") and forbidden.search(f)
        ]
        self.assertEqual(offenders, [], f"Forbidden tracked files: {offenders!r}")

    def test_t2_f12_agent_check_exits_zero_clean_output(self) -> None:
        """scripts/agent_check.sh exits 0 with zero lines starting with FAIL."""
        script = self.repo_root / "scripts" / "agent_check.sh"
        res = run_cli([str(script)], cwd=self.repo_root, timeout=180.0)
        self.assertEqual(res.returncode, 0, f"{res.stdout}\n{res.stderr}")
        fails = [ln for ln in res.stdout.splitlines() if ln.strip().startswith("FAIL")]
        self.assertEqual(fails, [])


class TestTier2F14AgentDocsBoundaries(E2ETestCase):
    TIER = 2
    FEATURE = "F14"

    def test_t2_f14_local_dev_skill_size_and_no_gemini_md(self) -> None:
        """.agents/skills/local-dev/SKILL.md stays <= 24000 bytes, matches dir name, and GEMINI.md is absent."""
        skill = self.repo_root / ".agents" / "skills" / "local-dev" / "SKILL.md"
        self.assertTrue(skill.is_file(), f"Missing {skill}")
        raw = skill.read_bytes()
        self.assertLessEqual(len(raw), 24000, f"local-dev SKILL.md too large: {len(raw)} bytes")
        parts = raw.decode("utf-8").split("---", 2)
        self.assertGreaterEqual(len(parts), 3)
        match = re.search(r"(?m)^name:\s*([a-z0-9-]+)\s*$", parts[1])
        self.assertIsNotNone(match)
        assert match is not None
        self.assertEqual(match.group(1), skill.parent.name)
        self.assertFalse((self.repo_root / "GEMINI.md").exists())


class TestTier2F15DevInstallBoundaries(E2ETestCase):
    TIER = 2
    FEATURE = "F15"

    def test_t2_f15_dev_install_script_never_mutates_gemini_config_json(self) -> None:
        """scripts/dev-install.sh must NEVER write or mutate ~/.gemini/config/config.json."""
        script = self.repo_root / "scripts" / "dev-install.sh"
        self.assertTrue(script.is_file(), f"Missing {script}")
        lines = script.read_text(encoding="utf-8").splitlines()
        mutation_patterns = (
            re.compile(r"(?:>>?|\btee\b|\bsed\s+-i|\bjq\b|\bmv\b|\bcp\b|\brm\b|\bpython3?\b).*config\.json"),
            re.compile(r"config\.json.*(?:>>?|\btee\b)"),
        )
        heredoc_re = re.compile(r"<<(-)?\s*['\"]?([A-Za-z_][A-Za-z0-9_]*)['\"]?")
        quoted_re = re.compile(r'"(?:\\.|[^"\\])*"|\$\'(?:\\.|[^\'\\])*\'|\'[^\']*\'')
        fd_dup_re = re.compile(r">&(?:[0-9]+|-)(?=$|[\s;|&><)])")
        offenders: list[str] = []
        heredoc_delim: str | None = None
        heredoc_strip_tabs = False

        def _strip_inline_comment(text: str) -> str:
            in_single = False
            in_ansi_single = False
            in_double = False
            i = 0
            n = len(text)
            while i < n:
                ch = text[i]
                if in_single:
                    if ch == "'":
                        in_single = False
                elif in_ansi_single:
                    if ch == "\\" and i + 1 < n:
                        i += 1
                    elif ch == "'":
                        in_ansi_single = False
                elif in_double:
                    if ch == "\\" and i + 1 < n:
                        i += 1
                    elif ch == '"':
                        in_double = False
                else:
                    if ch == "\\" and i + 1 < n:
                        i += 1
                    elif ch == "$" and i + 1 < n and text[i + 1] == "'":
                        in_ansi_single = True
                        i += 1
                    elif ch == "'":
                        in_single = True
                    elif ch == '"':
                        in_double = True
                    elif ch == "#" and (i == 0 or text[i - 1] in (" ", "\t", ";", "&", "|", "(")):
                        return text[:i].rstrip()
                i += 1
            return text

        def _split_shell_commands(text: str) -> list[str]:
            segments: list[str] = []
            in_single = False
            in_ansi_single = False
            in_double = False
            start = 0
            i = 0
            n = len(text)
            while i < n:
                ch = text[i]
                if in_single:
                    if ch == "'":
                        in_single = False
                elif in_ansi_single:
                    if ch == "\\" and i + 1 < n:
                        i += 1
                    elif ch == "'":
                        in_ansi_single = False
                elif in_double:
                    if ch == "\\" and i + 1 < n:
                        i += 1
                    elif ch == '"':
                        in_double = False
                else:
                    if ch == "\\" and i + 1 < n:
                        i += 1
                    elif ch == "$" and i + 1 < n and text[i + 1] == "'":
                        in_ansi_single = True
                        i += 1
                    elif ch == "'":
                        in_single = True
                    elif ch == '"':
                        in_double = True
                    elif ch == ";":
                        segments.append(text[start:i])
                        start = i + 1
                    elif ch == "&":
                        is_redir_amp = (i > start and text[i - 1] in (">", "<")) or (
                            i + 1 < n and text[i + 1] == ">"
                        )
                        if not is_redir_amp:
                            segments.append(text[start:i])
                            if i + 1 < n and text[i + 1] == "&":
                                i += 1
                            start = i + 1
                    elif ch == "|" and i + 1 < n and text[i + 1] == "|":
                        segments.append(text[start:i])
                        i += 1
                        start = i + 1
                i += 1
            segments.append(text[start:])
            return [seg.strip() for seg in segments if seg.strip()]

        def _first_unquoted_shell_op(text: str) -> int | None:
            in_single = False
            in_ansi_single = False
            in_double = False
            i = 0
            n = len(text)
            while i < n:
                ch = text[i]
                if in_single:
                    if ch == "'":
                        in_single = False
                elif in_ansi_single:
                    if ch == "\\" and i + 1 < n:
                        i += 1
                    elif ch == "'":
                        in_ansi_single = False
                elif in_double:
                    if ch == "\\" and i + 1 < n:
                        i += 1
                    elif ch == '"':
                        in_double = False
                else:
                    if ch == "\\" and i + 1 < n:
                        i += 1
                    elif ch == "$" and i + 1 < n and text[i + 1] == "'":
                        in_ansi_single = True
                        i += 1
                    elif ch == "'":
                        in_single = True
                    elif ch == '"':
                        in_double = True
                    elif ch == ">":
                        fd_match = fd_dup_re.match(text, i)
                        if fd_match:
                            i = fd_match.end() - 1
                        else:
                            return i
                    elif ch == "|":
                        return i
                i += 1
            return None

        for lineno, line in enumerate(lines, 1):
            if heredoc_delim is not None:
                candidate = line.lstrip("\t") if heredoc_strip_tabs else line
                if candidate.strip() == heredoc_delim:
                    heredoc_delim = None
                    heredoc_strip_tabs = False
                continue

            stripped = line.strip()
            if not stripped or stripped.startswith("#"):
                continue

            code = _strip_inline_comment(stripped)
            if not code:
                continue

            # Check for heredoc start; scan the command header line itself (e.g., `cat <<EOF > config.json`)
            # before entering heredoc body skip mode.
            hd_match = heredoc_re.search(code)
            if hd_match:
                heredoc_strip_tabs = hd_match.group(1) == "-"
                heredoc_delim = hd_match.group(2)

            # Split command lists (`;`, `&&`, `||`, `&`), then on echo/printf segments strip quoted
            # literals only BEFORE the first unquoted file redirect or pipe (`>` excluding fd-dups
            # like `>&2`/`2>&1`, or `|`). Safe prose like `echo >&2 "Edit ~/.gemini/config/config.json"`
            # passes while quoted targets (`echo '{}' > "$HOME/.gemini/config/config.json"`) and
            # chained mutations (`echo "msg" && cp /tmp/cfg "$HOME/.gemini/config/config.json"`) fail.
            matched_line = False
            for seg in _split_shell_commands(code):
                if re.match(r"^(echo|printf)\b", seg):
                    op_idx = _first_unquoted_shell_op(seg)
                    if op_idx is not None:
                        scan_target = quoted_re.sub(" ", seg[:op_idx]) + seg[op_idx:]
                    else:
                        scan_target = quoted_re.sub(" ", seg)
                else:
                    scan_target = seg

                for pat in mutation_patterns:
                    if pat.search(scan_target):
                        offenders.append(f"line {lineno}: {stripped}")
                        matched_line = True
                        break
                if matched_line:
                    break
        self.assertEqual(
            offenders,
            [],
            "scripts/dev-install.sh must not mutate config.json:\n" + "\n".join(offenders),
        )
