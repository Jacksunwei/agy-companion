"""Opaque-box S1-S7 security & error boundary suite (`--suite boundary`, 10 cohesive tests).

Exercises compiled binary boundaries without inspecting source code:
- S1: Loopback-only binding (`0.0.0.0` / `::` / port `0` rejected, no `--host` flag, `localhost` allowed)
- S2: Secret redaction, zero plaintext leaks across all outputs/logs/SQLite, `0600` tightening,
      placeholder key quarantine, and Sidecar Web UI `deny_unknown_fields` / token / error boundaries
- S3: Remote `http://`, embedded credentials, path traversal, and upstream HTTP `302` redirect blocking
- S6: Post-M2 eager fail-closed symlink/FIFO/oversized config & runtime files, hot-reload symlink swap,
      XDG precedence/traversal, and symlink RC / dev-plugin guards
- S7: HTTP method allowlist (`405`), `404`, security headers, malformed/conflicting/oversized
      `Content-Length` upfront rejection (`400`/`413`), and `3 MiB` payload pass-through
"""

from __future__ import annotations

import hashlib
import json
import os
import signal
import socket
import stat
from pathlib import Path
from typing import Any

from harness import (
    E2ETestCase,
    SidecarUiClient,
    find_free_port,
    http_raw_request,
    poll_usage_events,
    read_usage_raw_bytes,
    reap_env_daemons,
    reap_port_daemons,
    run_cli,
)
from mock_gemini import EchoChecksumAction, JsonAction, RawBytesAction


_PLACEHOLDER_KEY = "agy-companion-local"


def _send_raw_http_bytes(host: str, port: int, payload: bytes, timeout: float = 5.0) -> tuple[int, dict[str, str], bytes]:
    """Send raw HTTP/1.1 bytes over TCP (for duplicate/malformed Content-Length tests)."""
    with socket.create_connection((host, port), timeout=timeout) as sock:
        sock.settimeout(timeout)
        try:
            sock.sendall(payload)
        except (BrokenPipeError, ConnectionResetError):
            pass
        chunks: list[bytes] = []
        while True:
            try:
                part = sock.recv(65536)
            except (ConnectionResetError, TimeoutError):
                break
            if not part:
                break
            chunks.append(part)
    raw = b"".join(chunks)
    head, _, body = raw.partition(b"\r\n\r\n")
    lines = head.decode("iso-8859-1", errors="replace").split("\r\n")
    status = int(lines[0].split()[1]) if lines and len(lines[0].split()) >= 2 else 0
    headers: dict[str, str] = {}
    for line in lines[1:]:
        if ":" in line:
            k, v = line.split(":", 1)
            headers[k.strip().lower()] = v.strip()
    return status, headers, body


class TestBoundaryS1Loopback(E2ETestCase):
    SUITE = "boundary"
    FEATURE = "S1"

    def test_s1_loopback_only_binding_and_no_host_flag(self) -> None:
        """S1: Rejects 0.0.0.0, ::, port 0, and --host flag across CLI & Sidecar Web UI; accepts localhost."""
        binary = self.require_binary()
        env = self.make_env()
        proc_env = env.build_env()

        # (a) serve rejects 0.0.0.0, ::, and port 0 cleanly without panic.
        for label, bad_host, bad_port in (
            ("0.0.0.0", "0.0.0.0", find_free_port()),
            ("ipv6_any", "::", find_free_port()),
            ("port_0", "127.0.0.1", 0),
        ):
            cfg = env.write_config(
                env.make_config_dict(listen_host=bad_host, listen_port=bad_port),
                path=env.root / f"bad_{label}.json",
            )
            r = run_cli([str(binary), "serve", "--config", str(cfg)], env=proc_env, timeout=5.0)
            self.assertNotEqual(r.returncode, 0, f"{label} should fail: stdout={r.stdout}")
            self.assertNotIn("panicked at", r.stderr)

        # (b) CLI exposes no --host flag.
        r_flag = run_cli([str(binary), "serve", "--host", "0.0.0.0"], env=proc_env, timeout=5.0)
        self.assertNotEqual(r_flag.returncode, 0)

        # (c) Sidecar Web UI save_settings rejects 0.0.0.0 & port 0 without corrupting valid config.
        good_port = find_free_port()
        good_cfg = env.write_config(env.make_config_dict(listen_port=good_port))
        before = good_cfg.read_text(encoding="utf-8")
        ui = self.start_sidecar_ui(env, config_path=good_cfg)
        for bad_kwargs in ({"listen_host": "0.0.0.0"}, {"listen_port": 0}):
            res = ui.save_settings_smart(**bad_kwargs, raw=True)
            self.assertTrue(SidecarUiClient.is_tool_error(res), f"Expected rejection for {bad_kwargs}: {res}")
        self.assertEqual(good_cfg.read_text(encoding="utf-8"), before)

        # (d) serve accepts proxy.host = "localhost".
        lh_port = find_free_port()
        lh_cfg = env.write_config(
            env.make_config_dict(listen_host="localhost", listen_port=lh_port),
            path=env.root / "localhost.json",
        )
        proxy = self.start_proxy(env, config_path=lh_cfg, port=lh_port, pass_port_flag=False)
        h = http_raw_request("127.0.0.1", proxy.port, "GET", "/health")
        self.assertEqual(h.status, 200)


class TestBoundaryS2SecretsAndSchema(E2ETestCase):
    SUITE = "boundary"
    FEATURE = "S2"

    def test_s2_redaction_zero_plaintext_leaks_and_0600_tightening(self) -> None:
        """S2: Tightens 0644->0600, masks API key, and never leaks secrets or prompt/response bodies anywhere."""
        env = self.make_env()
        mock = self.start_mock_gemini()
        sentinel_key = "AIzaSySentinelAudit9999"
        bearer_canary = "oauth-secret-canary-7788"
        prompt_canary = "TOP_SECRET_PROMPT_CANARY_42"
        response_canary = "TOP_SECRET_RESPONSE_CANARY_99"

        port = find_free_port()
        cfg = env.write_config(
            env.make_config_dict(
                listen_port=port,
                base_url=mock.base_url,
                cloudcode_upstream_url=mock.base_url,
            ),
            mode=0o644,
        )
        env.write_gemini_auth(sentinel_key, config_path=cfg)
        proxy = self.start_proxy(env, config_path=cfg, port=port, extra_env={"RUST_LOG": "debug"})
        ui = self.start_sidecar_ui(env, config_path=cfg, extra_env={"RUST_LOG": "debug"})

        save_res = ui.save_settings_smart(default_model="gemini-3.8-flash")
        if os.name != "nt":
            self.assertEqual(stat.S_IMODE(os.lstat(cfg).st_mode), 0o600)
        cfg_text = cfg.read_text(encoding="utf-8")
        self.assertNotIn(sentinel_key, cfg_text)
        self.assertNotIn("api_key", cfg_text)

        get_res = ui.call_tool("get_settings", {})
        for res in (save_res, get_res):
            self.assertEqual(res["structuredContent"]["config"]["providers"]["gemini"]["api_key_hint"], "AIza...9999")
            self.assertNotIn(sentinel_key, json.dumps(res))

        # test_connection on upstream 400 API_KEY_INVALID returns ok=False without leaking key.
        mock.enqueue(
            JsonAction(
                status=400,
                payload={"error": {"code": 400, "message": "API_KEY_INVALID", "status": "INVALID_ARGUMENT"}},
            )
        )
        tc_res = ui.call_tool("test_connection", {})
        self.assertIs(tc_res["structuredContent"]["ok"], False)
        self.assertNotIn(sentinel_key, json.dumps(tc_res))

        # Send Gemini call with prompt/response sentinels + CloudCode call with Bearer canary.
        mock.enqueue(
            JsonAction(
                status=200,
                payload={
                    "candidates": [{"content": {"role": "model", "parts": [{"text": response_canary}]}}],
                    "usageMetadata": {
                        "promptTokenCount": 50,
                        "candidatesTokenCount": 20,
                        "cachedContentTokenCount": 10,
                        "thoughtsTokenCount": 5,
                    },
                },
            )
        )
        r_gem = http_raw_request(
            "127.0.0.1",
            proxy.port,
            "POST",
            "/v1beta/models/gemini-4-hydrogen:generateContent",
            headers={"Content-Type": "application/json"},
            body=json.dumps({"contents": [{"parts": [{"text": prompt_canary}]}]}).encode("utf-8"),
        )
        self.assertEqual(r_gem.status, 200)

        r_cc = http_raw_request(
            "127.0.0.1",
            proxy.port,
            "POST",
            "/v1internal:loadCodeAssist",
            headers={"Authorization": f"Bearer {bearer_canary}", "Content-Type": "application/json"},
            body=b"{}",
        )
        self.assertEqual(r_cc.status, 200)
        poll_usage_events(env.usage_dir, min_count=1, timeout=3.0)

        health_text = http_raw_request("127.0.0.1", proxy.port, "GET", "/health").text
        stats_text = http_raw_request("127.0.0.1", proxy.port, "GET", "/api/stats").text
        usage_raw = read_usage_raw_bytes(env.usage_dir)

        text_surfaces = {
            "sidecar_ui_stderr": ui.stderr_text,
            "proxy_stderr": proxy.stderr_text,
            "health": health_text,
            "stats": stats_text,
        }
        for secret in (sentinel_key, bearer_canary, prompt_canary, response_canary):
            for surface_name, content in text_surfaces.items():
                self.assertNotIn(secret, content, f"Leaked {secret!r} into {surface_name}")
            self.assertNotIn(secret.encode("utf-8"), usage_raw, f"Leaked {secret!r} into usage.db raw bytes")

    def test_s2_placeholder_api_key_quarantine_across_cli_proxy_and_sidecar_ui(self) -> None:
        """S2: Quarantines PROXY_PLACEHOLDER_API_KEY across save_settings, doctor, Gemini 401, and Day-0 CloudCode."""
        binary = self.require_binary()
        env = self.make_env()
        mock = self.start_mock_gemini()
        real_key = "AIzaSyRealKeyStay1234"
        cfg = env.write_config(
            env.make_config_dict(base_url=mock.base_url, cloudcode_upstream_url=mock.base_url)
        )
        auth_file = env.write_gemini_auth(real_key, config_path=cfg)

        # (a) Sidecar Web UI rejects exact & whitespace-padded placeholder key, preserving real key in auth/gemini.json.
        ui = self.start_sidecar_ui(env, config_path=cfg)
        for bad_key in (_PLACEHOLDER_KEY, f"  {_PLACEHOLDER_KEY}  "):
            res = ui.save_settings_smart(api_key=bad_key, raw=True)
            self.assertTrue(SidecarUiClient.is_tool_error(res), f"Should reject placeholder {bad_key!r}: {res}")
        self.assertEqual(json.loads(auth_file.read_text(encoding="utf-8"))["api_key"], real_key)
        self.assertNotIn("api_key", json.loads(cfg.read_text(encoding="utf-8"))["providers"]["gemini"])

        # (b) doctor --json ignores GEMINI_API_KEY=agy-companion-local.
        r_doc = run_cli(
            [str(binary), "doctor", "--json", "--config", str(cfg)],
            env=env.build_env(extra={"GEMINI_API_KEY": _PLACEHOLDER_KEY}),
        )
        self.assertIs(r_doc.json_stdout()["env"]["gemini_api_key_set"], False)

        # (c) Live proxy with placeholder key in auth/gemini.json returns 401 on Gemini (0 upstream hits) and Day-0 on CloudCode.
        ph_cfg = env.write_config(
            env.make_config_dict(
                base_url=mock.base_url,
                cloudcode_upstream_url=mock.base_url,
            ),
            path=env.root / "ph" / "config.json",
        )
        env.write_gemini_auth(_PLACEHOLDER_KEY, config_path=ph_cfg)
        proxy = self.start_proxy(env, config_path=ph_cfg)
        mock.clear_requests()

        r_gem = http_raw_request(
            "127.0.0.1",
            proxy.port,
            "POST",
            "/v1beta/models/gemini-4-hydrogen:generateContent",
            headers={"Content-Type": "application/json", "x-goog-api-key": _PLACEHOLDER_KEY},
            body=b'{"contents":[]}',
        )
        self.assertEqual(r_gem.status, 401, r_gem.text)
        self.assertEqual(len(mock.requests), 0)

        day0_raw = b'{"models":{"gemini-4-carbon-high":{"model":"MODEL_PLACEHOLDER_M427"}}}'
        mock.enqueue(RawBytesAction(status=200, content_type="application/json", body=day0_raw))
        r_cc = http_raw_request(
            "127.0.0.1",
            proxy.port,
            "POST",
            "/v1internal:fetchAvailableModels",
            headers={"Authorization": "Bearer ph-day0", "Content-Type": "application/json"},
            body=b"{}",
        )
        self.assertEqual(r_cc.status, 200)
        self.assertEqual(r_cc.body, day0_raw)
        self.assertNotIn("companion/", r_cc.text)

    def test_s2_sidecar_ui_deny_unknown_fields_and_error_boundaries(self) -> None:
        """S2: Sidecar Web UI enforces X-Sidecar-Token (401), JSON Content-Type (415), deny_unknown_fields, and mutual exclusion."""
        env = self.make_env()
        cfg = env.write_config(env.make_config_dict())
        env.write_gemini_auth("AIzaSySchemaKeep1234", config_path=cfg)
        before = cfg.read_text(encoding="utf-8")
        ui = self.start_sidecar_ui(env, config_path=cfg)

        # (a) Missing/wrong X-Sidecar-Token -> 401; non-JSON Content-Type -> 415.
        self.assertEqual(ui.call_api("get_settings", {}, token="").status, 401)
        self.assertEqual(ui.call_api("get_settings", {}, token="wrong-token").status, 401)
        self.assertEqual(ui.call_api("get_settings", {}, content_type="text/plain").status, 415)

        # (b) save_settings unknown fields at every level + mutually exclusive account_plan & cloudcode.upstream.
        bad_save_payloads: list[dict[str, Any]] = [
            {"bogus": 1},
            {"proxy": {"extra_proxy_field": 1}},
            {"cloudcode": {"extra_cc_field": 1}},
            {"providers": {"extra_provider": {}}},
            {"providers": {"gemini": {"extra_gemini_field": True}}},
            {"account_plan": "personal", "cloudcode": {"upstream": "auto"}},
        ]
        for payload in bad_save_payloads:
            res = ui.call_tool_raw("save_settings", payload)
            self.assertTrue(SidecarUiClient.is_tool_error(res), f"Expected error for save_settings({payload}): {res}")
        self.assertEqual(cfg.read_text(encoding="utf-8"), before)

        # (c) set_launcher_integration unknown fields.
        for payload in ({"enabled": True, "rc_file": "/tmp/x"}, {"enabled": True, "follow_rc_symlink": True}):
            res = ui.call_tool_raw("set_launcher_integration", payload)
            self.assertTrue(SidecarUiClient.is_tool_error(res), f"Expected error for {payload}: {res}")

        # (d) get_usage invalid range & unknown field.
        for payload in ({"range": "999d"}, {"range": "7d", "extra": 1}):
            res = ui.call_tool_raw("get_usage", payload)
            self.assertTrue(SidecarUiClient.is_tool_error(res), f"Expected error for get_usage({payload}): {res}")

        # (e) Unknown endpoint returns 404 while server stays healthy.
        bad_ep = ui.call_api("does_not_exist", {})
        self.assertEqual(bad_ep.status, 404)
        ok_after = ui.call_tool("get_settings", {})
        self.assertIn("structuredContent", ok_after)


class TestBoundaryS3UpstreamSsrfAndRedirects(E2ETestCase):
    SUITE = "boundary"
    FEATURE = "S3"

    def test_s3_rejects_remote_http_credentials_and_path_traversal(self) -> None:
        """S3: Rejects remote http://, file://, ftp://, embedded credentials, and raw path traversal with 0 upstream hits."""
        binary = self.require_binary()
        env = self.make_env()
        proc_env = env.build_env()

        # (a) serve rejects unsafe base_url schemes & embedded credentials.
        for idx, bad_url in enumerate(
            (
                "http://example.com",
                "file:///etc/passwd",
                "ftp://example.com",
                "https://user:pass@generativelanguage.googleapis.com",
            )
        ):
            cfg = env.write_config(
                env.make_config_dict(base_url=bad_url),
                path=env.root / f"bad_url_{idx}.json",
            )
            r = run_cli([str(binary), "serve", "--config", str(cfg)], env=proc_env, timeout=5.0)
            self.assertNotEqual(r.returncode, 0, f"Expected serve to reject {bad_url}")

        # (b) Sidecar Web UI save_settings rejects remote http:// without corrupting disk config.
        mock = self.start_mock_gemini()
        good_cfg = env.write_config(env.make_config_dict(base_url=mock.base_url))
        env.write_gemini_auth("AIzaSySsrfTest1234", config_path=good_cfg)
        before = good_cfg.read_text(encoding="utf-8")
        ui = self.start_sidecar_ui(env, config_path=good_cfg)
        bad_res = ui.save_settings_smart(base_url="http://example.com", raw=True)
        self.assertTrue(SidecarUiClient.is_tool_error(bad_res))
        self.assertEqual(good_cfg.read_text(encoding="utf-8"), before)
        ui.close()

        # (c) Live proxy rejects path traversal & NUL encodings with 400/404 and 0 upstream calls.
        proxy = self.start_proxy(env, config_path=good_cfg)
        mock.clear_requests()
        for bad_path in (
            "/v1beta/models/../../etc/passwd:generateContent",
            "/v1beta/models/%2e%2e/secret:generateContent",
            "/v1beta/models/gemini-4-hydrogen%00evil:generateContent",
            "/v1internal:../../etc/passwd",
        ):
            r = http_raw_request(
                "127.0.0.1",
                proxy.port,
                "POST",
                bad_path,
                headers={"Content-Type": "application/json"},
                body=b"{}",
            )
            self.assertIn(r.status, (400, 404), f"{bad_path} -> HTTP {r.status}: {r.text}")
        self.assertEqual(len(mock.requests), 0)

    def test_s3_blocks_upstream_http_redirects_no_key_leak(self) -> None:
        """S3: Blocks upstream 302 redirects (Policy::none()) on both proxy and test_connection with 0 sink hits."""
        env = self.make_env()
        primary = self.start_mock_gemini()
        sink = self.start_mock_gemini()

        primary.set_handler(
            lambda _req: JsonAction(
                status=302,
                payload={"redirect": True},
                headers={"Location": f"{sink.base_url}/steal"},
            )
        )
        cfg = env.write_config(
            env.make_config_dict(base_url=primary.base_url)
        )
        env.write_gemini_auth("AIzaSyNoRedirect1234", config_path=cfg)
        proxy = self.start_proxy(env, config_path=cfg)

        r_gen = http_raw_request(
            "127.0.0.1",
            proxy.port,
            "POST",
            "/v1beta/models/gemini-4-hydrogen:generateContent",
            headers={"Content-Type": "application/json"},
            body=b'{"contents":[]}',
        )
        self.assertEqual(r_gen.status, 302, r_gen.text)
        self.assertEqual(len(sink.requests), 0, "Proxy must never follow upstream 302 redirect")

        ui = self.start_sidecar_ui(env, config_path=cfg)
        tc = ui.call_tool("test_connection", {})
        self.assertIs(tc["structuredContent"]["ok"], False)
        self.assertIn("302", tc["structuredContent"].get("message", ""))
        self.assertEqual(len(sink.requests), 0, "test_connection must never follow upstream 302 redirect")


class TestBoundaryS6FileSafetyAndXdg(E2ETestCase):
    SUITE = "boundary"
    FEATURE = "S6"

    def test_s6_symlink_fifo_and_oversized_config_and_runtime_files_fail_closed(self) -> None:
        """S6: Malformed/oversized/symlink config and eager symlink/FIFO runtime targets fail closed; hot-reload retains active key."""
        binary = self.require_binary()
        env = self.make_env()
        proc_env = env.build_env()

        # (a) Malformed JSON, schema_version=99, and > 1 MiB oversized config fail closed.
        bad_json = env.write_config("{not-valid-json", path=env.root / "bad_json.json")
        bad_ver = env.write_config(env.make_config_dict(schema_version=99), path=env.root / "bad_ver.json")
        big_cfg = env.root / "oversized.json"
        big_cfg.write_bytes(b" " * (1024 * 1024 + 64))
        for bad_path in (bad_json, bad_ver, big_cfg):
            r_s = run_cli([str(binary), "serve", "--config", str(bad_path)], env=proc_env, timeout=5.0)
            self.assertNotEqual(r_s.returncode, 0, f"serve should fail on {bad_path.name}")
            self.assertNotIn("panicked at", r_s.stderr)
            r_d = run_cli([str(binary), "doctor", "--json", "--config", str(bad_path)], env=proc_env, timeout=5.0)
            self.assertNotEqual(r_d.returncode, 0, f"doctor should fail on {bad_path.name}")

        if os.name == "nt":
            return

        # (b) Symlink config fails closed on serve, doctor, and post-startup Sidecar Web UI TOCTOU swap.
        victim = env.root / "victim.txt"
        victim.write_text("DO_NOT_TOUCH", encoding="utf-8")
        os.chmod(victim, 0o644)
        sym_cfg = env.root / "sym_config.json"
        os.symlink(victim, sym_cfg)

        self.assertNotEqual(run_cli([str(binary), "serve", "--config", str(sym_cfg)], env=proc_env, timeout=5.0).returncode, 0)
        self.assertNotEqual(run_cli([str(binary), "doctor", "--json", "--config", str(sym_cfg)], env=proc_env, timeout=5.0).returncode, 0)

        toctou_cfg = env.write_config(env.make_config_dict(), path=env.root / "toctou" / "config.json")
        env.write_gemini_auth("AIzaSyToctouKey1234", config_path=toctou_cfg)
        ui = self.start_sidecar_ui(env, config_path=toctou_cfg)
        os.unlink(toctou_cfg)
        os.symlink(victim, toctou_cfg)
        self.assertTrue(SidecarUiClient.is_tool_error(ui.call_tool_raw("get_settings", {})))
        self.assertTrue(SidecarUiClient.is_tool_error(ui.save_settings_smart(default_model="gemini-3.8-flash", raw=True)))
        self.assertEqual(victim.read_text(encoding="utf-8"), "DO_NOT_TOUCH")
        self.assertEqual(stat.S_IMODE(os.lstat(victim).st_mode), 0o644)

        # (c) Eager serve prechecks reject symlink auth/, auth/gemini.json, usage_dir, symlink/FIFO usage.db, symlink proxy.pid & proxy.log immediately.
        for case in (
            "sym_auth_dir",
            "sym_auth_gemini",
            "sym_usage_dir",
            "sym_usage_db",
            "fifo_usage_db",
            "sym_pid",
            "sym_log",
        ):
            case_env = self.make_env()
            case_victim = case_env.root / "victim_target"
            case_victim.write_text("DO_NOT_TOUCH", encoding="utf-8")
            os.chmod(case_victim, 0o644)
            case_cfg = case_env.write_config(case_env.make_config_dict())

            if case == "sym_auth_dir":
                real_auth_target = case_env.root / "real_auth_target"
                real_auth_target.mkdir()
                os.symlink(real_auth_target, case_env.auth_dir)
            elif case == "sym_auth_gemini":
                case_env.auth_dir.mkdir(parents=True, exist_ok=True)
                os.symlink(case_victim, case_env.gemini_auth_path)
            elif case == "sym_usage_dir":
                case_env.usage_dir.rmdir()
                real_dir = case_env.root / "real_usage_target"
                real_dir.mkdir()
                os.symlink(real_dir, case_env.usage_dir)
            elif case == "sym_usage_db":
                os.symlink(case_victim, case_env.usage_dir / "usage.db")
            elif case == "fifo_usage_db":
                os.mkfifo(case_env.usage_dir / "usage.db")
            elif case == "sym_pid":
                os.symlink(case_victim, case_env.data_dir / "proxy.pid")
            elif case == "sym_log":
                os.symlink(case_victim, case_env.data_dir / "proxy.log")

            r_pre = run_cli(
                [str(binary), "serve", "--config", str(case_cfg), "--port", str(find_free_port())],
                env=case_env.build_env(),
                timeout=5.0,
            )
            self.assertNotEqual(r_pre.returncode, 0, f"serve should fail closed eagerly for {case}")
            self.assertLess(r_pre.duration_s, 3.0, f"serve hung on {case}")
            self.assertEqual(case_victim.read_text(encoding="utf-8"), "DO_NOT_TOUCH")
            self.assertEqual(stat.S_IMODE(os.lstat(case_victim).st_mode), 0o644)

        # (d) Hot-reload symlink swap on BOTH config.json AND auth/gemini.json retains active key_v1 without mutating victims.
        mock = self.start_mock_gemini()
        key_v1 = "AIzaSyHotKeepV1_1111"
        key_v2 = "AIzaSyHotSwapV2_2222"
        hr_env = self.make_env()
        hr_cfg = hr_env.write_config(hr_env.make_config_dict(base_url=mock.base_url))
        hr_auth = hr_env.write_gemini_auth(key_v1, config_path=hr_cfg)
        hr_victim = hr_env.write_config(
            hr_env.make_config_dict(base_url=mock.base_url),
            path=hr_env.root / "hr_victim.json",
            mode=0o644,
        )
        hr_auth_victim = hr_env.root / "hr_auth_victim.json"
        hr_auth_victim_text = json.dumps({"schema_version": 1, "api_key": key_v2})
        hr_auth_victim.write_text(hr_auth_victim_text, encoding="utf-8")
        os.chmod(hr_auth_victim, 0o644)
        proxy = self.start_proxy(hr_env, config_path=hr_cfg)

        os.unlink(hr_cfg)
        os.symlink(hr_victim, hr_cfg)
        hr_env.bump_mtime(hr_victim, delta=3.0)
        os.unlink(hr_auth)
        os.symlink(hr_auth_victim, hr_auth)
        hr_env.bump_mtime(hr_auth_victim, delta=3.0)

        r_hr = http_raw_request(
            "127.0.0.1",
            proxy.port,
            "POST",
            "/v1beta/models/gemini-4-hydrogen:generateContent",
            headers={"Content-Type": "application/json"},
            body=b'{"contents":[]}',
        )
        self.assertEqual(r_hr.status, 200, r_hr.text)
        self.assertEqual(mock.requests[-1].header("x-goog-api-key"), key_v1)
        self.assertEqual(hr_auth_victim.read_text(encoding="utf-8"), hr_auth_victim_text)

    def test_s6_xdg_precedence_traversal_and_symlink_rc_plugin_guards(self) -> None:
        """S6: Config precedence, relative/empty XDG fallback, .. traversal rejection, symlink RC & dev-plugin guards."""
        binary = self.require_binary()
        env = self.make_env()

        def _cleanup_detached_daemons() -> None:
            reap_env_daemons(env.root, expected_binary=binary)
            if os.name == "nt":
                return
            for pid_path in (
                env.data_dir / "proxy.pid",
                env.home_dir / ".local" / "share" / "agy-companion" / "proxy.pid",
            ):
                if pid_path.is_file():
                    try:
                        pid = json.loads(pid_path.read_text(encoding="utf-8")).get("pid")
                        if isinstance(pid, int) and pid > 1:
                            os.kill(pid, signal.SIGTERM)
                    except (OSError, ValueError):
                        pass

        self.stack.callback(_cleanup_detached_daemons)

        # (a) Precedence: --config > AGY_COMPANION_CONFIG > XDG_CONFIG_HOME > $HOME/.config & relative/empty fallback.
        home_cfg = env.home_dir / ".config" / "agy-companion" / "config.json"
        xdg_cfg = env.config_root / "agy-companion" / "config.json"
        env_cfg = env.root / "env_cfg.json"
        cli_cfg = env.root / "cli_cfg.json"
        env.write_config(env.make_config_dict(default_model="gemini-4-home"), path=home_cfg)
        env.write_config(env.make_config_dict(default_model="gemini-4-xdg"), path=xdg_cfg)
        env.write_config(env.make_config_dict(default_model="gemini-4-env"), path=env_cfg)
        env.write_config(env.make_config_dict(default_model="gemini-4-cli"), path=cli_cfg)

        ui_cli = self.start_sidecar_ui(env, config_path=cli_cfg, extra_env={"AGY_COMPANION_CONFIG": str(env_cfg)})
        self.assertEqual(
            ui_cli.call_tool("get_settings", {})["structuredContent"]["config"]["providers"]["gemini"]["default_model"],
            "gemini-4-cli",
        )
        ui_cli.close()
        ui_env = self.start_sidecar_ui(
            env, pass_config_flag=False, set_config_env=False, extra_env={"AGY_COMPANION_CONFIG": str(env_cfg)}
        )
        self.assertEqual(
            ui_env.call_tool("get_settings", {})["structuredContent"]["config"]["providers"]["gemini"]["default_model"],
            "gemini-4-env",
        )
        ui_env.close()
        ui_xdg = self.start_sidecar_ui(env, pass_config_flag=False, set_config_env=False)
        self.assertEqual(
            ui_xdg.call_tool("get_settings", {})["structuredContent"]["config"]["providers"]["gemini"]["default_model"],
            "gemini-4-xdg",
        )
        ui_xdg.close()
        rel_port = find_free_port()
        self.stack.callback(reap_port_daemons, rel_port, expected_binary=binary)
        ui_rel = self.start_sidecar_ui(
            env,
            pass_config_flag=False,
            set_config_env=False,
            extra_args=("--port", str(rel_port)),
            extra_env={"XDG_CONFIG_HOME": "relative/xdg", "XDG_DATA_HOME": ""},
        )
        rel_model = ui_rel.call_tool("get_settings", {})["structuredContent"]["config"]["providers"]["gemini"][
            "default_model"
        ]
        ui_rel.close()
        if os.name == "nt":
            # Windows falls back to SHGetKnownFolderPath(RoamingAppData) rather than $HOME/.config;
            # never mutate the host user profile, just verify relative XDG_CONFIG_HOME was ignored.
            self.assertNotEqual(rel_model, "gemini-4-xdg")
        else:
            self.assertEqual(rel_model, "gemini-4-home")

        # (b) .. traversal in XDG_CONFIG_HOME, XDG_DATA_HOME, usage_dir, --plugins-dir, and --rc-file fails closed.
        trav_cfg = env.write_config(env.make_config_dict(usage_dir="/tmp/../evil"), path=env.root / "trav.json")
        self.assertNotEqual(
            run_cli([str(binary), "serve", "--config", str(trav_cfg)], env=env.build_env(), timeout=5.0).returncode,
            0,
        )
        self.assertNotEqual(
            run_cli(
                [str(binary), "doctor", "--json"],
                env=env.build_env(extra={"XDG_CONFIG_HOME": "/tmp/../evil", "AGY_COMPANION_CONFIG": None}, set_config_env=False),
                timeout=5.0,
            ).returncode,
            0,
        )
        no_usage_cfg_dict = env.make_config_dict()
        no_usage_cfg_dict.pop("usage_dir", None)
        no_usage_cfg = env.write_config(no_usage_cfg_dict, path=env.root / "no_usage_dir.json")
        self.assertNotEqual(
            run_cli(
                [str(binary), "serve", "--config", str(no_usage_cfg)],
                env=env.build_env(extra={"XDG_DATA_HOME": "/tmp/../evil"}),
                timeout=5.0,
            ).returncode,
            0,
        )
        self.assertNotEqual(
            run_cli(
                [str(binary), "install", "--plugins-dir", "/tmp/../evil", "--no-start-proxy", "--json"],
                env=env.build_env(),
                timeout=5.0,
            ).returncode,
            0,
        )

        if os.name == "nt":
            return

        self.assertNotEqual(
            run_cli(
                [str(binary), "integration", "enable", "--shell", "zsh", "--rc-file", "/tmp/../evil", "--json"],
                env=env.build_env(),
                timeout=5.0,
            ).returncode,
            0,
        )

        # (c) Symlinked .zlogin -> manual_required on both CLI integration & Sidecar Web UI set_launcher_integration, victim untouched.
        rc_victim = env.root / "rc_victim.txt"
        rc_victim.write_text("# victim rc\n", encoding="utf-8")
        os.symlink(rc_victim, env.home_dir / ".zlogin")
        sh_env = env.build_env(extra={"SHELL": "/bin/zsh", "ZDOTDIR": str(env.home_dir)})

        r_int = run_cli([str(binary), "integration", "enable", "--shell", "zsh", "--json"], env=sh_env)
        self.assertEqual(r_int.returncode, 0, r_int.stderr)
        self.assertEqual(r_int.json_stdout()["state"], "manual_required")

        ui_rc = self.start_sidecar_ui(env, config_path=xdg_cfg, extra_env={"SHELL": "/bin/zsh", "ZDOTDIR": str(env.home_dir)})
        r_ui_rc = ui_rc.call_tool("set_launcher_integration", {"enabled": True})
        self.assertEqual(r_ui_rc["structuredContent"]["launcher_integration"]["state"], "manual_required")
        self.assertEqual(rc_victim.read_text(encoding="utf-8"), "# victim rc\n")

        # (d) install refuses symlinked dev plugin without --force & never overwrites unrelated ~/.local/bin/agy-companion.
        plugins_dir = env.root / "plugins_dev"
        plugins_dir.mkdir()
        dev_target = env.root / "dev_plugin"
        dev_target.mkdir()
        os.symlink(dev_target, plugins_dir / "agy-companion")
        r_dev = run_cli(
            [str(binary), "install", "--plugins-dir", str(plugins_dir), "--no-launcher-hook", "--no-start-proxy", "--json"],
            env=env.build_env(),
        )
        self.assertNotEqual(r_dev.returncode, 0)
        self.assertTrue((plugins_dir / "agy-companion").is_symlink())

        local_bin = env.home_dir / ".local" / "bin" / "agy-companion"
        local_bin.parent.mkdir(parents=True, exist_ok=True)
        local_bin.write_text("#!/bin/sh\necho user-tool\n", encoding="utf-8")
        os.chmod(local_bin, 0o755)
        fresh_plugins = env.root / "plugins_fresh"
        r_fresh = run_cli(
            [str(binary), "install", "--plugins-dir", str(fresh_plugins), "--no-launcher-hook", "--no-start-proxy", "--json"],
            env=env.build_env(),
        )
        self.assertEqual(r_fresh.returncode, 0, r_fresh.stderr)
        self.assertEqual(local_bin.read_text(encoding="utf-8"), "#!/bin/sh\necho user-tool\n")


class TestBoundaryS7HttpSurface(E2ETestCase):
    SUITE = "boundary"
    FEATURE = "S7"

    def test_s7_method_allowlist_404_and_security_headers(self) -> None:
        """S7: Enforces GET/POST allowlist (405), 404, nosniff + no-store headers, and resilient invalid-UTF-8/empty SSE."""
        env = self.make_env()
        mock = self.start_mock_gemini()
        cfg = env.write_config(
            env.make_config_dict(
                base_url=mock.base_url,
                cloudcode_upstream_url=mock.base_url,
            )
        )
        env.write_gemini_auth("AIzaSyS7Surface1234", config_path=cfg)
        proxy = self.start_proxy(env, config_path=cfg)

        # (a) PUT/DELETE/PATCH -> 405 JSON with nosniff + no-store across all routes.
        for method in ("PUT", "DELETE", "PATCH"):
            for path in (
                "/health",
                "/api/stats",
                "/v1beta/models/gemini-4-hydrogen:generateContent",
                "/v1internal:fetchAvailableModels",
            ):
                r = http_raw_request("127.0.0.1", proxy.port, method, path)
                self.assertEqual(r.status, 405, f"{method} {path} -> {r.status}")
                self.assertEqual(r.header("x-content-type-options"), "nosniff")
                self.assertIn("no-store", r.header("cache-control") or "")

        # (b) POST /health & /api/stats -> 405; unknown -> 404; GET /health & /api/stats set headers.
        for path in ("/health", "/api/stats"):
            r_post = http_raw_request("127.0.0.1", proxy.port, "POST", path)
            self.assertEqual(r_post.status, 405)
            r_get = http_raw_request("127.0.0.1", proxy.port, "GET", path)
            self.assertEqual(r_get.status, 200)
            self.assertEqual(r_get.header("x-content-type-options"), "nosniff")
            self.assertIn("no-store", r_get.header("cache-control") or "")

        r_404 = http_raw_request("127.0.0.1", proxy.port, "GET", "/nonexistent")
        self.assertEqual(r_404.status, 404)
        self.assertEqual(r_404.header("x-content-type-options"), "nosniff")
        self.assertIn("no-store", r_404.header("cache-control") or "")

        # (c) Upstream invalid UTF-8 bytes & empty SSE complete without crashing; cache_hit_ratio == 0.0.
        mock.enqueue(RawBytesAction(status=200, content_type="application/octet-stream", body=b"\xff\xfe\x80\x81"))
        r_bin = http_raw_request(
            "127.0.0.1",
            proxy.port,
            "POST",
            "/v1beta/models/gemini-4-hydrogen:generateContent",
            headers={"Content-Type": "application/json"},
            body=b"{}",
        )
        self.assertEqual(r_bin.status, 200)
        self.assertEqual(r_bin.body, b"\xff\xfe\x80\x81")

        mock.enqueue(RawBytesAction(status=200, content_type="text/event-stream", body=b"", chunked=True))
        r_empty = http_raw_request(
            "127.0.0.1",
            proxy.port,
            "POST",
            "/v1beta/models/gemini-4-hydrogen:streamGenerateContent?alt=sse",
            headers={"Content-Type": "application/json"},
            body=b"{}",
        )
        self.assertEqual(r_empty.status, 200)
        self.assertEqual(r_empty.body, b"")

        st = http_raw_request("127.0.0.1", proxy.port, "GET", "/api/stats").json()
        self.assertEqual(st["usage"]["cache_hit_ratio"], 0.0)

    def test_s7_content_length_validation_and_64mib_body_limit(self) -> None:
        """S7: Rejects malformed/conflicting Content-Length (400) & >64 MiB/u64-overflow (413) upfront; passes 3 MiB body."""
        env = self.make_env()
        mock = self.start_mock_gemini()
        cfg = env.write_config(env.make_config_dict(base_url=mock.base_url))
        env.write_gemini_auth("AIzaSyS7Body1234", config_path=cfg)
        proxy = self.start_proxy(env, config_path=cfg)
        mock.clear_requests()

        # (a) Malformed Content-Length: abc & conflicting duplicate Content-Length -> 400 upfront (0 upstream hits).
        for label, raw_req in (
            (
                "malformed",
                b"POST /v1beta/models/gemini-4-hydrogen:generateContent HTTP/1.1\r\n"
                b"Host: 127.0.0.1\r\nContent-Type: application/json\r\n"
                b"Content-Length: abc\r\nConnection: close\r\n\r\n",
            ),
            (
                "conflicting",
                b"POST /v1beta/models/gemini-4-hydrogen:generateContent HTTP/1.1\r\n"
                b"Host: 127.0.0.1\r\nContent-Type: application/json\r\n"
                b"Content-Length: 5\r\nContent-Length: 10\r\nConnection: close\r\n\r\n",
            ),
        ):
            status, _headers, body = _send_raw_http_bytes("127.0.0.1", proxy.port, raw_req)
            self.assertEqual(status, 400, f"{label}: {status} {body!r}")

        # (b) >64 MiB (67108865 with 64 KiB sent) -> 413 JSON upfront with security headers;
        # u64-overflow -> 400 (HTTP/1.1 parser) or 413 (middleware), both upfront with 0 upstream hits.
        over_req = (
            b"POST /v1beta/models/gemini-4-hydrogen:generateContent HTTP/1.1\r\n"
            b"Host: 127.0.0.1\r\nContent-Type: application/json\r\n"
            b"Content-Length: 67108865\r\nConnection: close\r\n\r\n"
        ) + (b"x" * 65536)
        status_over, headers_over, body_over = _send_raw_http_bytes("127.0.0.1", proxy.port, over_req)
        self.assertEqual(status_over, 413, f"over_64mib: {status_over} {body_over!r}")
        self.assertEqual(headers_over.get("x-content-type-options"), "nosniff")
        self.assertIn("no-store", headers_over.get("cache-control", ""))

        ovf_req = (
            b"POST /v1beta/models/gemini-4-hydrogen:generateContent HTTP/1.1\r\n"
            b"Host: 127.0.0.1\r\nContent-Type: application/json\r\n"
            b"Content-Length: 99999999999999999999999\r\nConnection: close\r\n\r\n"
        )
        status_ovf, _headers_ovf, body_ovf = _send_raw_http_bytes("127.0.0.1", proxy.port, ovf_req)
        self.assertIn(status_ovf, (400, 413), f"u64_overflow: {status_ovf} {body_ovf!r}")
        self.assertEqual(len(mock.requests), 0)

        # (c) Valid 3 MiB JSON body (above Axum's 2 MiB default) forwards intact with SHA-256 match.
        big_body = b'{"pad":"' + (b"a" * (3 * 1024 * 1024)) + b'"}'
        big_sha = hashlib.sha256(big_body).hexdigest()
        mock.enqueue(EchoChecksumAction())
        r_3m = http_raw_request(
            "127.0.0.1",
            proxy.port,
            "POST",
            "/v1beta/models/gemini-4-hydrogen:generateContent",
            headers={"Content-Type": "application/json"},
            body=big_body,
            timeout=15.0,
        )
        self.assertEqual(r_3m.status, 200, r_3m.text[:200])
        self.assertEqual(r_3m.json()["echo"]["sha256"], big_sha)
