"""Pure-Python harness self-tests (`SELF-01` .. `SELF-26`).

Runs via `python3 e2e/run_e2e.py --self-test` with zero external dependencies
and without requiring the compiled `agy-companion` binary.
"""

from __future__ import annotations

import concurrent.futures
import contextlib
import errno
import hashlib
import http.client
import json
import os
import socket
import sqlite3
import stat
import subprocess
import sys
import threading
import time
import types
import unittest
from pathlib import Path
from typing import Any
from unittest import mock as unittest_mock

from harness import (
    CompanionProcess,
    CompanionStartError,
    E2ETestCase,
    IsolatedEnv,
    MockConnectProxy,
    STRIPPED_ENV_VARS,
    USAGE_DB_FILENAME,
    find_free_port,
    http_raw_request,
    http_stream_request,
    open_usage_db_readonly,
    poll_usage_events,
    read_usage_raw_bytes,
    reap_pidfile_daemon,
    register_pidfile_cleanup,
    resolve_binary,
    run_cli,
    usage_files,
    usage_storage_files,
)
from mcp_client import (
    McpEofError,
    McpProtocolViolationError,
    McpStdioClient,
    McpTimeoutError,
)
from mock_gemini import (
    ConnectionResetAction,
    EchoChecksumAction,
    JsonAction,
    MockGeminiServer,
    SseStreamAction,
    build_sse_events,
    split_across_utf8_char,
)


_FAKE_MCP_SERVER_SCRIPT = r"""
import hashlib, json, sys, time

mode = sys.argv[1] if len(sys.argv) > 1 else "normal"

if mode == "pollute":
    sys.stdout.write("STRAY LOG LINE NOT JSONRPC\n")
    sys.stdout.flush()

for line in sys.stdin:
    line = line.strip()
    if not line:
        continue
    msg = json.loads(line)
    method = msg.get("method")
    req_id = msg.get("id")
    if req_id is None:
        continue

    if mode == "crash" and method == "tools/list":
        sys.stderr.write("fatal crash simulated\n")
        sys.stderr.flush()
        sys.exit(42)

    if mode == "hang" and method == "tools/call":
        time.sleep(30.0)

    if method == "initialize":
        sys.stderr.write("fake-mcp stderr: initialized\n")
        sys.stderr.flush()
        resp = {
            "jsonrpc": "2.0",
            "id": req_id,
            "result": {
                "protocolVersion": "2025-03-26",
                "capabilities": {"tools": {}, "resources": {}},
                "serverInfo": {"name": "fake-mcp", "version": "0.1.0"},
            },
        }
    elif method == "resources/list":
        resp = {
            "jsonrpc": "2.0",
            "id": req_id,
            "result": {
                "resources": [
                    {
                        "uri": "ui://companion/settings.html",
                        "name": "Settings",
                        "mimeType": "text/html;profile=mcp-app",
                    }
                ]
            },
        }
    elif method == "resources/read":
        resp = {
            "jsonrpc": "2.0",
            "id": req_id,
            "result": {
                "contents": [
                    {
                        "uri": msg["params"]["uri"],
                        "mimeType": "text/html;profile=mcp-app",
                        "text": "<!doctype html><html><body>fake</body></html>",
                    }
                ]
            },
        }
    elif method == "tools/list":
        resp = {
            "jsonrpc": "2.0",
            "id": req_id,
            "result": {
                "tools": [
                    {
                        "name": "save_settings",
                        "description": "Save settings",
                        "inputSchema": {
                            "type": "object",
                            "properties": {
                                "api_key": {"type": "string"},
                                "gemini_base_url": {"type": "string"},
                                "gemini_default_model": {"type": "string"},
                            },
                        },
                    }
                ]
            },
        }
    elif method == "tools/call":
        resp = {
            "jsonrpc": "2.0",
            "id": req_id,
            "result": {
                "content": [{"type": "text", "text": "ok"}],
                "structuredContent": {"echo_args": msg["params"].get("arguments", {})},
            },
        }
    elif method == "echo":
        params = msg.get("params") or {}
        blob = str(params.get("blob", ""))
        resp = {
            "jsonrpc": "2.0",
            "id": req_id,
            "result": {
                "tag": params.get("tag"),
                "len": len(blob),
                "sha256": hashlib.sha256(blob.encode("utf-8")).hexdigest(),
            },
        }
    else:
        resp = {"jsonrpc": "2.0", "id": req_id, "error": {"code": -32601, "message": "not found"}}

    sys.stdout.write(json.dumps(resp) + "\n")
    sys.stdout.flush()
"""


_TINY_HEALTH_SERVER_SCRIPT = r"""
import http.server, json, sys

port = int(sys.argv[1])
sys.stderr.write(f"tiny-health listening on 127.0.0.1:{port}\n")
sys.stderr.flush()

class Handler(http.server.BaseHTTPRequestHandler):
    def log_message(self, fmt, *args):
        pass
    def do_GET(self):
        body = json.dumps({"status": "ok", "version": "0.1.0", "protocol_version": 1}).encode("utf-8")
        self.send_response(200)
        self.send_header("Content-Type", "application/json")
        self.send_header("Content-Length", str(len(body)))
        self.end_headers()
        self.wfile.write(body)

http.server.HTTPServer(("127.0.0.1", port), Handler).serve_forever()
"""


class TestHarnessSelfTests(E2ETestCase):
    TIER = 0
    FEATURE = "SELF"

    def test_self_01_mock_gemini_capture_wire_details(self) -> None:
        """SELF-01: MockGeminiServer binds 127.0.0.1:0 and captures exact method, path, query, headers, body & SHA-256."""
        mock = self.start_mock_gemini()
        self.assertEqual(mock.host, "127.0.0.1")
        self.assertGreater(mock.port, 0)
        body = b'{"hello":  "world", "n": 42}'
        resp = http_raw_request(
            mock.host,
            mock.port,
            "POST",
            "/v1beta/models/gemini-4-hydrogen:generateContent?alt=sse&foo=bar",
            body=body,
            headers={"Content-Type": "application/json", "X-Goog-Api-Key": "TEST_KEY_01"},
        )
        self.assertEqual(resp.status, 200)
        req = mock.last_request()
        self.assertIsNotNone(req)
        assert req is not None
        self.assertEqual(req.method, "POST")
        self.assertEqual(req.raw_target, "/v1beta/models/gemini-4-hydrogen:generateContent?alt=sse&foo=bar")
        self.assertEqual(req.path, "/v1beta/models/gemini-4-hydrogen:generateContent")
        self.assertEqual(req.query_params, {"alt": ["sse"], "foo": ["bar"]})
        self.assertEqual(req.header("x-goog-api-key"), "TEST_KEY_01")
        self.assertEqual(req.body, body)
        self.assertEqual(req.body_sha256, hashlib.sha256(body).hexdigest())

    def test_self_02_mock_gemini_unary_generate_content_usage(self) -> None:
        """SELF-02: MockGeminiServer returns :generateContent JSON with custom usageMetadata."""
        usage = {
            "promptTokenCount": 777,
            "candidatesTokenCount": 88,
            "cachedContentTokenCount": 555,
            "thoughtsTokenCount": 33,
        }
        mock = self.start_mock_gemini(default_usage=usage)
        resp = http_raw_request(
            mock.host,
            mock.port,
            "POST",
            "/v1beta/models/gemini-4-hydrogen:generateContent",
            body=b"{}",
        )
        self.assertEqual(resp.status, 200)
        data = resp.json()
        self.assertEqual(data["usageMetadata"], usage)

    def test_self_03_mock_gemini_chunked_sse_split_utf8_crlf_and_incremental(self) -> None:
        """SELF-03: Chunked SSE with split 4-byte UTF-8, 3-byte CJK, CRLF/LF, and per-chunk delay streams incrementally."""
        mock = self.start_mock_gemini()
        payload = build_sse_events(
            [
                {"candidates": [{"content": {"parts": [{"text": "rocket 🚀 中文"}]}}]},
                {
                    "candidates": [{"content": {"parts": [{"text": "end"}]}}],
                    "usageMetadata": {"promptTokenCount": 120, "candidatesTokenCount": 30},
                },
            ],
            line_ending=b"\r\n",
        )
        # Split across both the 4-byte emoji and the 3-byte CJK char.
        part_a, part_b = split_across_utf8_char(payload, "🚀")
        part_b1, part_b2 = split_across_utf8_char(part_b, "中")
        mock.enqueue(SseStreamAction(chunks=[part_a, part_b1, part_b2], chunk_delay_s=0.12))

        chunks_seen: list[tuple[bytes, float]] = []
        with http_stream_request(
            mock.host,
            mock.port,
            "POST",
            "/v1beta/models/gemini-4-hydrogen:streamGenerateContent?alt=sse",
            body=b"{}",
        ) as stream:
            self.assertEqual(stream.status, 200)
            self.assertIn("text/event-stream", (stream.header("content-type") or "").lower())
            for chunk, ts in stream.iter_chunks():
                chunks_seen.append((chunk, ts))

        self.assertGreaterEqual(len(chunks_seen), 2)
        reconstructed = b"".join(c for c, _ in chunks_seen)
        self.assertEqual(reconstructed, payload)
        self.assertIn("🚀 中文", reconstructed.decode("utf-8"))
        # Incremental delivery: last chunk arrives noticeably later than first chunk.
        self.assertGreater(chunks_seen[-1][1] - chunks_seen[0][1], 0.10)

    def test_self_04_mock_gemini_models_health_check_auth(self) -> None:
        """SELF-04: GET /v1beta/models?pageSize=1 returns 200 on expected key and 400 API_KEY_INVALID otherwise."""
        mock = self.start_mock_gemini(expected_api_key="GOOD_KEY")
        ok_resp = http_raw_request(
            mock.host,
            mock.port,
            "GET",
            "/v1beta/models?pageSize=1",
            headers={"x-goog-api-key": "GOOD_KEY"},
        )
        self.assertEqual(ok_resp.status, 200)
        bad_resp = http_raw_request(
            mock.host,
            mock.port,
            "GET",
            "/v1beta/models?pageSize=1",
            headers={"x-goog-api-key": "WRONG_KEY"},
        )
        self.assertEqual(bad_resp.status, 400)
        self.assertIn("API_KEY_INVALID", bad_resp.text)

    def test_self_05_mock_gemini_fault_queue_and_tcp_rst(self) -> None:
        """SELF-05: FIFO fault queue executes 429 -> 500 -> ConnectionResetAction (TCP RST) -> recovery 200."""
        mock = self.start_mock_gemini()
        mock.enqueue(JsonAction(status=429, payload={"error": "rate_limit"}))
        mock.enqueue(JsonAction(status=500, payload={"error": "internal"}))
        mock.enqueue(ConnectionResetAction())

        r1 = http_raw_request(mock.host, mock.port, "POST", "/v1beta/models/m:generateContent", body=b"{}")
        r2 = http_raw_request(mock.host, mock.port, "POST", "/v1beta/models/m:generateContent", body=b"{}")
        self.assertEqual(r1.status, 429)
        self.assertEqual(r2.status, 500)

        with self.assertRaises((OSError, http.client.HTTPException)):
            http_raw_request(mock.host, mock.port, "POST", "/v1beta/models/m:generateContent", body=b"{}")

        r4 = http_raw_request(mock.host, mock.port, "POST", "/v1beta/models/m:generateContent", body=b"{}")
        self.assertEqual(r4.status, 200)

    def test_self_06_mock_gemini_concurrency_16_requests(self) -> None:
        """SELF-06: 16 concurrent requests yield 16 distinct monotonic sequence numbers with zero errors."""
        mock = self.start_mock_gemini()

        def call(i: int) -> int:
            r = http_raw_request(
                mock.host,
                mock.port,
                "POST",
                f"/v1beta/models/m-{i}:generateContent",
                body=f'{{"i":{i}}}'.encode("utf-8"),
            )
            return r.status

        with concurrent.futures.ThreadPoolExecutor(max_workers=16) as pool:
            statuses = list(pool.map(call, range(16)))
        self.assertEqual(statuses, [200] * 16)
        reqs = mock.wait_for_requests(16, timeout=5.0)
        self.assertEqual(len(reqs), 16)
        seqs = [r.seq for r in reqs]
        self.assertEqual(sorted(seqs), list(range(1, 17)))

    def test_self_07_mock_gemini_large_2mib_and_chunked_request_body(self) -> None:
        """SELF-07: Large 2 MiB body and HTTP/1.1 chunked request body both match EchoChecksumAction SHA-256."""
        mock = self.start_mock_gemini()
        mock.enqueue(EchoChecksumAction())
        big_body = b"Z" * (2 * 1024 * 1024)
        r_big = http_raw_request(
            mock.host,
            mock.port,
            "POST",
            "/v1beta/models/m:generateContent",
            body=big_body,
        )
        self.assertEqual(r_big.status, 200)
        echo_big = r_big.json()["echo"]
        self.assertEqual(echo_big["length"], len(big_body))
        self.assertEqual(echo_big["sha256"], hashlib.sha256(big_body).hexdigest())

        # Also test Transfer-Encoding: chunked request body decoding.
        mock.enqueue(EchoChecksumAction())
        part1, part2 = b"chunked-part-one-", b"chunked-part-two"
        wire_chunked = (
            f"{len(part1):x}\r\n".encode("ascii")
            + part1
            + b"\r\n"
            + f"{len(part2):x}\r\n".encode("ascii")
            + part2
            + b"\r\n0\r\n\r\n"
        )
        r_chunked = http_raw_request(
            mock.host,
            mock.port,
            "POST",
            "/v1beta/models/m:generateContent",
            body=wire_chunked,
            headers={"Transfer-Encoding": "chunked"},
        )
        self.assertEqual(r_chunked.status, 200)
        expected = part1 + part2
        self.assertEqual(r_chunked.json()["echo"]["sha256"], hashlib.sha256(expected).hexdigest())

    def test_self_08_mcp_client_handshake_resources_tools_and_smart_save(self) -> None:
        """SELF-08: McpStdioClient completes full handshake, resources, tools, save_settings_smart, and stdout audit."""
        cmd = [sys.executable, "-u", "-c", _FAKE_MCP_SERVER_SCRIPT, "normal"]
        client = self.stack.enter_context(McpStdioClient(cmd))
        init = client.initialize()
        self.assertEqual(init["serverInfo"]["version"], "0.1.0")
        resources = client.list_resources()
        self.assertEqual(resources["resources"][0]["uri"], "ui://companion/settings.html")
        read_res = client.read_resource("ui://companion/settings.html")
        self.assertEqual(read_res["contents"][0]["mimeType"], "text/html;profile=mcp-app")
        tools = client.list_tools()
        self.assertEqual(tools["tools"][0]["name"], "save_settings")
        # save_settings_smart should map base_url -> gemini_base_url & default_model -> gemini_default_model!
        call_res = client.save_settings_smart(base_url="http://127.0.0.1:9999", default_model="gemini-4-hydrogen")
        echo_args = call_res["structuredContent"]["echo_args"]
        self.assertEqual(echo_args["gemini_base_url"], "http://127.0.0.1:9999")
        self.assertEqual(echo_args["gemini_default_model"], "gemini-4-hydrogen")
        client.assert_clean_stdout()
        self.assertIn("fake-mcp stderr: initialized", client.stderr_text)

    def test_self_09_mcp_client_detects_stray_stdout_pollution(self) -> None:
        """SELF-09: McpStdioClient.assert_clean_stdout() raises on stray non-JSON line on stdout."""
        cmd = [sys.executable, "-u", "-c", _FAKE_MCP_SERVER_SCRIPT, "pollute"]
        client = self.stack.enter_context(McpStdioClient(cmd))
        client.initialize()
        with self.assertRaises(McpProtocolViolationError):
            client.assert_clean_stdout()

    def test_self_10_mcp_client_timeout_and_process_termination(self) -> None:
        """SELF-10: McpStdioClient raises McpTimeoutError on hung call and kills child PID on close()."""
        cmd = [sys.executable, "-u", "-c", _FAKE_MCP_SERVER_SCRIPT, "hang"]
        client = McpStdioClient(cmd).start()
        pid = client.pid
        assert pid is not None
        try:
            client.initialize()
            with self.assertRaises(McpTimeoutError):
                client.call_tool("save_settings", {}, timeout=0.3)
        finally:
            client.close()
        with self.assertRaises(ProcessLookupError):
            os.kill(pid, 0)

    def test_self_11_mcp_client_fast_eof_detection_on_crash(self) -> None:
        """SELF-11: McpStdioClient raises McpEofError in < 1.0s when fake MCP server crashes."""
        cmd = [sys.executable, "-u", "-c", _FAKE_MCP_SERVER_SCRIPT, "crash"]
        client = self.stack.enter_context(McpStdioClient(cmd))
        client.initialize()
        t0 = time.monotonic()
        with self.assertRaises(McpEofError):
            client.list_tools(timeout=10.0)
        elapsed = time.monotonic() - t0
        self.assertLess(elapsed, 1.0, f"EOF detection was too slow: {elapsed:.2f}s")

    def test_self_12_isolated_env_strips_secrets_sets_0600_and_cleans_up(self) -> None:
        """SELF-12: IsolatedEnv strips ambient secrets/proxy vars, writes 0600 config, and removes tempdir."""
        saved_env = {k: os.environ.get(k) for k in ("GEMINI_API_KEY", "GOOGLE_GEMINI_BASE_URL", "HTTP_PROXY")}
        os.environ["GEMINI_API_KEY"] = "AMBIENT_LEAK"
        os.environ["GOOGLE_GEMINI_BASE_URL"] = "http://ambient-proxy"
        os.environ["HTTP_PROXY"] = "http://corp-proxy:8080"
        root_path: Path | None = None
        try:
            with IsolatedEnv() as env:
                root_path = env.root
                built = env.build_env()
                for var in STRIPPED_ENV_VARS:
                    if var == "AGY_COMPANION_CONFIG":
                        self.assertEqual(built[var], str(env.config_path))
                    else:
                        self.assertNotIn(var, built, f"Failed to strip {var}")
                cfg = env.write_config(env.make_config_dict(), mode=0o600, mtime_offset_secs=-5.0)
                self.assertEqual(stat.S_IMODE(os.stat(cfg).st_mode), 0o600)
                old_mtime = os.stat(cfg).st_mtime
                new_mtime = IsolatedEnv.bump_mtime(cfg, delta=2.0)
                self.assertGreater(new_mtime, old_mtime)
        finally:
            for k, old in saved_env.items():
                if old is None:
                    os.environ.pop(k, None)
                else:
                    os.environ[k] = old

        assert root_path is not None
        self.assertFalse(root_path.exists(), f"IsolatedEnv leaked temp directory: {root_path}")

    def test_self_13_companion_process_ready_teardown_and_fast_crash(self) -> None:
        """SELF-13: CompanionProcess waits for /health, captures stderr, kills PID on stop, and fails fast on crash."""
        port = find_free_port()
        proc = CompanionProcess(
            port=port,
            custom_cmd=[sys.executable, "-u", "-c", _TINY_HEALTH_SERVER_SCRIPT, "{port}"],
            ready_timeout=15.0,
        )
        proc.start()
        pid = proc.pid
        assert pid is not None
        try:
            resp = http_raw_request("127.0.0.1", proc.port, "GET", "/health")
            self.assertEqual(resp.status, 200)
            self.assertIn("tiny-health listening", proc.stderr_text)
        finally:
            proc.stop()
        with self.assertRaises(ProcessLookupError):
            os.kill(pid, 0)

        # Fast crash detection (< 1.0s).
        t0 = time.monotonic()
        bad = CompanionProcess(
            port=find_free_port(),
            custom_cmd=[sys.executable, "-c", "import sys; sys.stderr.write('boom\\n'); sys.exit(1)"],
            ready_timeout=10.0,
        )
        with self.assertRaises(CompanionStartError) as ctx:
            bad.start()
        self.assertLess(time.monotonic() - t0, 1.0)
        self.assertIn("boom", str(ctx.exception))

    def test_self_14_http_raw_request_preserves_dotdot_and_arbitrary_verbs(self) -> None:
        """SELF-14: http_raw_request sends unnormalized '..' paths and arbitrary HTTP verbs (PUT/DELETE/PATCH/OPTIONS)."""
        mock = self.start_mock_gemini()
        raw_path = "/v1beta/models/../../secret?key=abc&alt=sse"
        r = http_raw_request(mock.host, mock.port, "POST", raw_path, body=b"{}")
        self.assertEqual(r.status, 200)
        self.assertEqual(mock.last_request().raw_target, raw_path)  # type: ignore[union-attr]

        for verb in ("PUT", "DELETE", "PATCH", "OPTIONS"):
            rv = http_raw_request(mock.host, mock.port, verb, "/health")
            self.assertEqual(rv.status, 200)
            self.assertEqual(mock.last_request().method, verb)  # type: ignore[union-attr]

    def test_self_15_resolve_binary_precedence(self) -> None:
        """SELF-15: resolve_binary follows exact precedence: --binary > AGY_COMPANION_E2E_BIN > cargo debug default."""
        fake_root = Path("/tmp/agy-fake-root")
        p1 = resolve_binary(
            cli_binary="/tmp/cli-bin",
            repo_root=fake_root,
            env={"AGY_COMPANION_E2E_BIN": "/tmp/env-bin"},
        )
        self.assertEqual(p1, Path("/tmp/cli-bin").resolve())

        p2 = resolve_binary(
            cli_binary=None,
            repo_root=fake_root,
            env={"AGY_COMPANION_E2E_BIN": "/tmp/env-bin"},
        )
        self.assertEqual(p2, Path("/tmp/env-bin").resolve())

        p3 = resolve_binary(cli_binary=None, repo_root=fake_root, env={})
        self.assertEqual(p3, (fake_root / "target" / "debug" / "agy-companion").resolve())

    def test_self_16_suite_inventory_thresholds_and_list_cli(self) -> None:
        """SELF-16: Verifies zero duplicate test names, tier/feature minimums, and `run_e2e.py --list` exit 0."""
        import run_e2e

        tests = run_e2e.discover_tests([1, 2, 3, 4], include_self=False)
        by_tier, by_feature = run_e2e.build_counts(tests)

        self.assertGreaterEqual(by_tier.get("T1", 0), 72, f"Tier 1 below target: {by_tier}")
        self.assertGreaterEqual(by_tier.get("T2", 0), 80, f"Tier 2 below target: {by_tier}")
        self.assertGreaterEqual(by_tier.get("T3", 0), 16, f"Tier 3 below target: {by_tier}")
        self.assertGreaterEqual(by_tier.get("T4", 0), 7, f"Tier 4 below target: {by_tier}")
        self.assertEqual(len(tests), len({t.name for t in tests}), "Duplicate test names detected")

        for feat_idx in range(1, 13):
            feat = f"F{feat_idx}"
            t1_count = by_feature.get("T1", {}).get(feat, 0)
            t2_count = by_feature.get("T2", {}).get(feat, 0)
            self.assertGreaterEqual(t1_count, 6, f"{feat} Tier 1 count < 6: {t1_count}")
            self.assertGreaterEqual(t2_count, 6, f"{feat} Tier 2 count < 6: {t2_count}")

        # Additive F14, F15 & F16 coverage check.
        self.assertGreaterEqual(by_feature.get("T1", {}).get("F14", 0), 2)
        self.assertGreaterEqual(by_feature.get("T1", {}).get("F15", 0), 1)
        self.assertGreaterEqual(by_feature.get("T1", {}).get("F16", 0), 4)
        self.assertGreaterEqual(by_feature.get("T2", {}).get("F14", 0), 1)
        self.assertGreaterEqual(by_feature.get("T2", {}).get("F15", 0), 1)
        self.assertGreaterEqual(by_feature.get("T2", {}).get("F16", 0), 2)

        # Execute runner CLI with --list and --list --json without requiring any binary.
        runner = Path(__file__).resolve().parent / "run_e2e.py"
        res_list = run_cli([sys.executable, str(runner), "--list"])
        self.assertEqual(res_list.returncode, 0, res_list.stderr)
        lines = [ln for ln in res_list.stdout.splitlines() if ln.strip()]
        self.assertEqual(len(lines), len(tests))

        res_json = run_cli([sys.executable, str(runner), "--list", "--json"])
        self.assertEqual(res_json.returncode, 0, res_json.stderr)
        payload = res_json.json_stdout()
        self.assertEqual(payload["total"], len(tests))

    def test_self_17_http_raw_request_handles_close_delimited_early_413(self) -> None:
        """SELF-17: http_raw_request returns 413 cleanly on close-delimited early-413 socket reset across 5 trials."""
        trials = 5
        srv = socket.socket(socket.AF_INET, socket.SOCK_STREAM)
        srv.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
        srv.bind(("127.0.0.1", 0))
        port = srv.getsockname()[1]
        srv.listen(8)
        srv.settimeout(10.0)
        self.stack.callback(srv.close)

        def serve_early_413() -> None:
            for _ in range(trials):
                try:
                    conn, _ = srv.accept()
                except OSError:
                    break
                try:
                    conn.settimeout(5.0)
                    buf = b""
                    while b"\r\n\r\n" not in buf:
                        chunk = conn.recv(4096)
                        if not chunk:
                            break
                        buf += chunk
                    # Read ~4 KiB of request body, then reply 413 WITHOUT Content-Length and close immediately.
                    sep = buf.find(b"\r\n\r\n")
                    body_read = len(buf) - (sep + 4) if sep >= 0 else 0
                    while body_read < 4096:
                        chunk = conn.recv(4096 - body_read)
                        if not chunk:
                            break
                        body_read += len(chunk)
                    conn.sendall(
                        b"HTTP/1.1 413 Payload Too Large\r\n"
                        b"Connection: close\r\n"
                        b"\r\n"
                        b"Payload Too Large"
                    )
                except OSError:
                    pass
                finally:
                    try:
                        conn.close()
                    except OSError:
                        pass

        thread = threading.Thread(target=serve_early_413, daemon=True)
        thread.start()
        self.stack.callback(thread.join, 5.0)

        payload = b"X" * (10 * 1024 * 1024)
        for trial in range(trials):
            with self.subTest(trial=trial):
                resp = http_raw_request(
                    "127.0.0.1",
                    port,
                    "POST",
                    "/v1beta/models/gemini-4-hydrogen:generateContent",
                    body=payload,
                    headers={"Content-Type": "application/json"},
                    timeout=15.0,
                )
                self.assertEqual(resp.status, 413, f"Trial {trial}: expected 413, got {resp.status}")

    def test_self_18_mcp_client_concurrent_large_writes_preserve_framing(self) -> None:
        """SELF-18: 4 concurrent 128 KiB MCP requests over bufsize=0 stdin preserve intact JSON-RPC framing."""
        cmd = [sys.executable, "-u", "-c", _FAKE_MCP_SERVER_SCRIPT, "normal"]
        client = self.stack.enter_context(McpStdioClient(cmd))
        client.initialize()

        workers = 4
        blob_size = 128 * 1024
        barrier = threading.Barrier(workers)

        def call_worker(idx: int) -> tuple[str, str, dict[str, object]]:
            tag = f"t{idx}"
            blob = chr(ord("a") + idx) * blob_size
            expected_sha = hashlib.sha256(blob.encode("utf-8")).hexdigest()
            barrier.wait(timeout=5.0)
            res = client.request_ok("echo", {"tag": tag, "blob": blob}, timeout=15.0)
            return tag, expected_sha, res

        with concurrent.futures.ThreadPoolExecutor(max_workers=workers) as pool:
            futs = [pool.submit(call_worker, i) for i in range(workers)]
            for fut in futs:
                tag, expected_sha, res = fut.result()
                self.assertEqual(res.get("tag"), tag)
                self.assertEqual(res.get("len"), blob_size)
                self.assertEqual(res.get("sha256"), expected_sha)

        client.assert_clean_stdout()

    def test_self_19_json_result_records_subtest_failures_and_cleans_start_times(self) -> None:
        """SELF-19: _RecordingTestResult records failing/erroring subTest entries and cleans _start_times."""
        import run_e2e

        class _SyntheticSubTests(unittest.TestCase):
            def test_three_failing_subtests(self) -> None:
                for idx in range(3):
                    with self.subTest(idx=idx):
                        self.assertEqual(idx, -1, f"subtest-fail-{idx}")

            def test_pass_and_fail_subtest(self) -> None:
                with self.subTest(part="ok"):
                    self.assertEqual(1, 1)
                with self.subTest(part="bad"):
                    self.assertEqual(1, 2, "subtest-bad")

            def test_all_passing_subtests(self) -> None:
                for idx in range(2):
                    with self.subTest(idx=idx):
                        self.assertEqual(idx, idx)

            def test_fail_and_error_subtests(self) -> None:
                with self.subTest(kind="fail"):
                    self.assertTrue(False, "boom-fail")
                with self.subTest(kind="err"):
                    raise RuntimeError("boom-error")

            def test_subtest_fail_then_outer_fail(self) -> None:
                with self.subTest(part="inner"):
                    self.assertTrue(False, "inner-fail")
                self.fail("outer-fail")

        def _run_case(method_name: str, *, failfast: bool = False) -> Any:
            suite = unittest.TestSuite([_SyntheticSubTests(method_name)])
            res = run_e2e._RecordingTestResult(failfast=failfast)
            suite.run(res)
            self.assertEqual(res._start_times, {}, f"_start_times leaked for {method_name}: {res._start_times}")
            return res

        # Case A: 1 test, 3 failing subTests.
        res_a = _run_case("test_three_failing_subtests")
        self.assertFalse(res_a.wasSuccessful())
        self.assertEqual(res_a.testsRun, 1)
        self.assertEqual(sum(1 for r in res_a.records if r["status"] == "pass"), 0)
        self.assertEqual(len(res_a.failures), 3)
        self.assertEqual(len(res_a.records), 3)
        for idx, rec in enumerate(res_a.records):
            self.assertEqual(rec["status"], "fail")
            self.assertIn(f"idx={idx}", rec["test"])
            self.assertIn(f"subtest-fail-{idx}", rec["detail"])
            self.assertGreaterEqual(rec["duration_s"], 0.0)

        # Case C: 1 passing subTest + 1 failing subTest.
        res_c = _run_case("test_pass_and_fail_subtest")
        self.assertFalse(res_c.wasSuccessful())
        self.assertEqual(sum(1 for r in res_c.records if r["status"] == "pass"), 0)
        self.assertEqual(len(res_c.failures), 1)
        self.assertEqual(len(res_c.records), 1)
        self.assertEqual(res_c.records[0]["status"], "fail")
        self.assertIn("part='bad'", res_c.records[0]["test"])

        # Case D: 2 passing subTests.
        res_d = _run_case("test_all_passing_subtests")
        self.assertTrue(res_d.wasSuccessful())
        self.assertEqual(sum(1 for r in res_d.records if r["status"] == "pass"), 1)
        self.assertEqual(len(res_d.failures), 0)
        self.assertEqual(len(res_d.records), 1)
        self.assertEqual(res_d.records[0]["status"], "pass")

        # Case E: 1 failing subTest + 1 erroring subTest.
        res_e = _run_case("test_fail_and_error_subtests")
        self.assertFalse(res_e.wasSuccessful())
        self.assertEqual(len(res_e.failures), 1)
        self.assertEqual(len(res_e.errors), 1)
        self.assertEqual([r["status"] for r in res_e.records], ["fail", "error"])
        self.assertIn("boom-error", res_e.records[1]["detail"])

        # Case F: 1 failing subTest + outer self.fail.
        res_f = _run_case("test_subtest_fail_then_outer_fail")
        self.assertFalse(res_f.wasSuccessful())
        self.assertEqual(len(res_f.failures), 2)
        self.assertEqual([r["status"] for r in res_f.records], ["fail", "fail"])
        self.assertIn("inner-fail", res_f.records[0]["detail"])
        self.assertIn("outer-fail", res_f.records[1]["detail"])

        # Fail-fast (-x) stops on first failing subTest and still records it.
        res_ff = _run_case("test_three_failing_subtests", failfast=True)
        self.assertFalse(res_ff.wasSuccessful())
        self.assertEqual(len(res_ff.failures), 1)
        self.assertEqual(len(res_ff.records), 1)
        self.assertEqual(res_ff.records[0]["status"], "fail")
        self.assertIn("idx=0", res_ff.records[0]["test"])

    def test_self_20_http_raw_request_retries_transient_enobufs_and_eagain(self) -> None:
        """SELF-20: http_raw_request retries transient ENOBUFS/EAGAIN/EINTR and partial socket writes."""
        mock_srv = self.start_mock_gemini()
        mock_srv.enqueue(EchoChecksumAction())
        payload = bytes((i % 251 for i in range(256 * 1024)))
        expected_sha = hashlib.sha256(payload).hexdigest()

        orig_connect = http.client.HTTPConnection.connect
        faults = [
            OSError(errno.ENOBUFS, "No buffer space available"),
            OSError(errno.EAGAIN, "Resource temporarily unavailable"),
            OSError(errno.EINTR, "Interrupted system call"),
        ]
        state = {"send_calls": 0, "fault_idx": 0}

        class _FlakySocketProxy:
            def __init__(self, real_sock: socket.socket) -> None:
                self._real = real_sock

            def send(self, data: Any, *args: Any, **kwargs: Any) -> int:
                state["send_calls"] += 1
                # Inject transient errors on early body writes (after headers are sent via sendall).
                if state["send_calls"] in (2, 4, 6) and state["fault_idx"] < len(faults):
                    exc = faults[state["fault_idx"]]
                    state["fault_idx"] += 1
                    raise exc
                # Exercise partial-write offset advancement on every 3rd call.
                mv = memoryview(data)
                if state["send_calls"] == 3 and len(mv) > 4096:
                    return self._real.send(mv[:4096], *args, **kwargs)
                return self._real.send(data, *args, **kwargs)

            def sendall(self, data: Any, *args: Any, **kwargs: Any) -> None:
                # Headers use sendall(); body writes should use send(), but if legacy conn.send()
                # calls sendall() for body chunks, inject ENOBUFS there too so the test fails on unfixed code.
                mv = memoryview(data)
                if len(mv) >= 32768 and state["fault_idx"] < len(faults):
                    exc = faults[state["fault_idx"]]
                    state["fault_idx"] += 1
                    raise exc
                self._real.sendall(data, *args, **kwargs)

            def __getattr__(self, name: str) -> Any:
                return getattr(self._real, name)

        def _wrapped_connect(conn: http.client.HTTPConnection) -> None:
            orig_connect(conn)
            assert conn.sock is not None
            conn.sock = _FlakySocketProxy(conn.sock)  # type: ignore[assignment]

        with unittest_mock.patch.object(http.client.HTTPConnection, "connect", _wrapped_connect):
            resp = http_raw_request(
                mock_srv.host,
                mock_srv.port,
                "POST",
                "/v1beta/models/gemini-4-hydrogen:generateContent",
                body=payload,
                headers={"Content-Type": "application/json"},
                timeout=5.0,
                chunk_size=32768,
            )

        self.assertEqual(resp.status, 200)
        echo = resp.json()["echo"]
        self.assertEqual(echo["length"], len(payload))
        self.assertEqual(echo["sha256"], expected_sha)
        self.assertEqual(state["fault_idx"], len(faults), "Expected all 3 transient socket faults to be injected")

    def test_self_21_dev_install_heredoc_scanner_edge_cases(self) -> None:
        """SELF-21: dev-install.sh config.json mutation scanner handles all 10 heredoc/here-string/quote cases."""
        from test_tier2_boundaries import scan_shell_script_for_config_json_mutations

        cases: list[tuple[str, list[str], bool]] = [
            # 1. Here-string <<<EOF must NOT enter heredoc skip mode -> catches mutation on next line.
            (
                "herestring_unquoted",
                [
                    "read -r x <<<EOF",
                    'cp /tmp/evil "$HOME/.gemini/config/config.json"',
                ],
                True,
            ),
            # 2. Here-string <<< 'EOF' must NOT enter heredoc skip mode -> catches mutation.
            (
                "herestring_quoted",
                [
                    "read -r x <<< 'EOF'",
                    'cp /tmp/evil "$HOME/.gemini/config/config.json"',
                ],
                True,
            ),
            # 3. Quoted <<EOF inside echo must NOT enter heredoc skip mode -> catches mutation.
            (
                "quoted_heredoc_in_echo",
                [
                    'echo "use cat <<EOF for help"',
                    'cp /tmp/evil "$HOME/.gemini/config/config.json"',
                ],
                True,
            ),
            # 4. Backslash-escaped <<\EOF IS a real heredoc -> prose inside body is safe (no false positive).
            (
                "escaped_heredoc_body_safe",
                [
                    r"cat <<\EOF",
                    "cp /tmp/example ~/.gemini/config/config.json",
                    "EOF",
                ],
                False,
            ),
            # 5. Tab-stripped <<-\EOF IS a real heredoc -> tab-indented EOF terminates it, mutation after caught.
            (
                "escaped_tab_heredoc_then_mutation",
                [
                    r"cat <<-\EOF",
                    "\tdo not mutate config.json",
                    "\tEOF",
                    'rm -f "$HOME/.gemini/config/config.json"',
                ],
                True,
            ),
            # 6. Partially backslash-escaped <<E\OF IS a real heredoc terminated by EOF.
            (
                "partial_escaped_heredoc_safe",
                [
                    r"cat <<E\OF",
                    "echo '{}' > ~/.gemini/config/config.json",
                    "EOF",
                ],
                False,
            ),
            # 7. Single-quoted <<'EOF' (used in scripts/dev-install.sh:16) IS a real heredoc.
            (
                "single_quoted_heredoc_safe",
                [
                    "cat <<'EOF'",
                    "Never write > ~/.gemini/config/config.json",
                    "EOF",
                ],
                False,
            ),
            # 8. Double-quoted <<"EOF" IS a real heredoc.
            (
                "double_quoted_heredoc_safe",
                [
                    'cat <<"EOF"',
                    "cp /tmp/a ~/.gemini/config/config.json",
                    "EOF",
                ],
                False,
            ),
            # 9. Tab-stripped <<-EOF IS a real heredoc terminated by tab-indented EOF.
            (
                "tab_stripped_heredoc_safe",
                [
                    "cat <<-EOF",
                    "\techo '{}' > ~/.gemini/config/config.json",
                    "\tEOF",
                ],
                False,
            ),
            # 10. Heredoc header redirecting to config.json MUST be caught on the header line.
            (
                "heredoc_header_redirect_caught",
                [
                    "cat <<EOF > ~/.gemini/config/config.json",
                    '{"bad": true}',
                    "EOF",
                ],
                True,
            ),
        ]

        for name, script_lines, expect_offender in cases:
            with self.subTest(case=name):
                offenders = scan_shell_script_for_config_json_mutations(script_lines)
                if expect_offender:
                    self.assertTrue(
                        len(offenders) > 0,
                        f"Case {name}: expected mutation offender to be detected, got []",
                    )
                else:
                    self.assertEqual(
                        offenders,
                        [],
                        f"Case {name}: unexpected false-positive offenders: {offenders!r}",
                    )

    def test_self_22_isolated_env_strips_rollout_shell_and_hook_vars(self) -> None:
        """SELF-22: IsolatedEnv strips all rollout shell/hook/SSH/API vars and supports explicit overrides."""
        rollout_vars = (
            "CLOUD_CODE_URL",
            "AGY_COMPANION_HOOK_ACTIVE",
            "AGY_COMPANION_ALLOW_REMOTE_SSH",
            "JETSKI_MCP_ENV_RESTORE_PREFIX",
            "JETSKI_MCP_ENV_RESTORE_VARS",
            "ANTIGRAVITY_RESOLVING_ENVIRONMENT",
            "SSH_CONNECTION",
            "SSH_CLIENT",
            "ZDOTDIR",
            "SHELL",
            "GOOGLE_API_KEY",
        )
        for var in rollout_vars:
            self.assertIn(var, STRIPPED_ENV_VARS, f"{var} missing from STRIPPED_ENV_VARS")

        env = self.make_env()
        polluted = {var: f"polluted-{var}" for var in rollout_vars}
        with unittest_mock.patch.dict(os.environ, polluted, clear=False):
            clean = env.build_env()
            for var in rollout_vars:
                self.assertNotIn(var, clean, f"{var} leaked into clean build_env()")

            overridden = env.build_env(
                extra={"SHELL": "/bin/zsh", "ZDOTDIR": str(env.home_dir)}
            )
            self.assertEqual(overridden["SHELL"], "/bin/zsh")
            self.assertEqual(overridden["ZDOTDIR"], str(env.home_dir))
            self.assertNotIn("SSH_CONNECTION", overridden)
            self.assertNotIn("GOOGLE_API_KEY", overridden)

        cfg_default = env.make_config_dict()
        self.assertNotIn("cloudcode_upstream_url", cfg_default)
        cfg_cc = env.make_config_dict(cloudcode_upstream_url="http://127.0.0.1:49999")
        self.assertEqual(cfg_cc["cloudcode_upstream_url"], "http://127.0.0.1:49999")

    def test_self_23_pidfile_reaper_kills_verified_daemon_and_refuses_unsafe_targets(self) -> None:
        """SELF-23: PID-file reaper terminates verified detached daemon in LIFO teardown and refuses unsafe targets."""
        if os.name == "nt":
            self.skipTest("POSIX process inspection self-test")

        token = f"agy-self23-sentinel-{os.getpid()}-{time.time_ns()}"
        sleeper_code = "import signal, time; signal.signal(signal.SIGTERM, lambda *_: exit(0)); time.sleep(60)"

        # 1. Verify LIFO teardown: register_pidfile_cleanup registered AFTER IsolatedEnv reaps before tempdir removal.
        proc1 = subprocess.Popen(
            [sys.executable, "-c", sleeper_code, token],
            start_new_session=True,
            stdout=subprocess.DEVNULL,
            stderr=subprocess.DEVNULL,
        )
        try:
            with contextlib.ExitStack() as lifo_stack:
                iso = lifo_stack.enter_context(IsolatedEnv(prefix="agy-self23-"))
                pid_file = iso.data_dir / "proxy.pid"
                register_pidfile_cleanup(
                    lifo_stack,
                    pid_file,
                    expected_cmdline_substr=token,
                    timeout=3.0,
                )
                pid_file.write_text(
                    json.dumps({"pid": proc1.pid, "port": 4141, "version": "0.1.0"}),
                    encoding="utf-8",
                )
            # Exiting lifo_stack must have terminated proc1 cleanly.
            rc1 = proc1.wait(timeout=3.0)
            self.assertIsNotNone(rc1)
        finally:
            if proc1.poll() is None:
                proc1.kill()
                proc1.wait(timeout=2.0)

        # 2. Verify safe refusal on unsafe/invalid/mismatched PID files while live sleeper stays untouched.
        env = self.make_env()
        proc2 = subprocess.Popen(
            [sys.executable, "-c", sleeper_code, f"{token}-keepalive"],
            start_new_session=True,
            stdout=subprocess.DEVNULL,
            stderr=subprocess.DEVNULL,
        )
        try:
            real_pid_file = env.data_dir / "real.pid"
            real_pid_file.write_text(json.dumps({"pid": proc2.pid}), encoding="utf-8")

            # Mismatched explicit token
            self.assertFalse(
                reap_pidfile_daemon(real_pid_file, expected_cmdline_substr="nonexistent-token-xyz")
            )
            self.assertIsNone(proc2.poll(), "Sleeper must survive mismatched cmdline token")

            # Default companion check refuses non-companion Python process
            self.assertFalse(reap_pidfile_daemon(real_pid_file))
            self.assertIsNone(proc2.poll(), "Sleeper must survive default agy-companion check")

            # Symlinked proxy.pid refused even when token matches
            sym_pid = env.data_dir / "symlink.pid"
            os.symlink(real_pid_file, sym_pid)
            self.assertFalse(
                reap_pidfile_daemon(sym_pid, expected_cmdline_substr=f"{token}-keepalive")
            )
            self.assertIsNone(proc2.poll(), "Sleeper must survive symlinked proxy.pid")

            # Empty and whitespace expected_cmdline_substr must fail closed (never wildcard-match).
            for bad_substr in ("", "   ", "\t\n"):
                with self.subTest(bad_substr=repr(bad_substr)):
                    self.assertFalse(
                        reap_pidfile_daemon(real_pid_file, expected_cmdline_substr=bad_substr)
                    )
                    self.assertIsNone(proc2.poll(), "Sleeper must survive empty/whitespace substr")

            # Unsafe PID values and malformed files (including C pid_t overflow values)
            unsafe_cases: list[tuple[str, str]] = [
                ("pid_zero", json.dumps({"pid": 0})),
                ("pid_one", json.dumps({"pid": 1})),
                ("pid_neg", json.dumps({"pid": -42})),
                ("pid_self", json.dumps({"pid": os.getpid()})),
                ("pid_parent", json.dumps({"pid": os.getppid()})),
                ("pid_bool", json.dumps({"pid": True})),
                ("pid_str", json.dumps({"pid": str(proc2.pid)})),
                ("pid_overflow_i32", json.dumps({"pid": 2**31})),
                ("pid_overflow_i64", json.dumps({"pid": 2**63})),
                ("corrupt_json", "{not-json"),
                ("non_dict_json", "[1, 2, 3]"),
            ]
            for label, content in unsafe_cases:
                with self.subTest(unsafe_case=label):
                    bad_path = env.data_dir / f"{label}.pid"
                    bad_path.write_text(content, encoding="utf-8")
                    self.assertFalse(
                        reap_pidfile_daemon(bad_path, expected_cmdline_substr=f"{token}-keepalive")
                    )

            # Missing file
            self.assertFalse(
                reap_pidfile_daemon(env.data_dir / "does_not_exist.pid", expected_cmdline_substr=token)
            )

            # Matching token reaps proc2 cleanly, then dead PID returns False
            self.assertTrue(
                reap_pidfile_daemon(real_pid_file, expected_cmdline_substr=f"{token}-keepalive")
            )
            proc2.wait(timeout=3.0)
            self.assertFalse(
                reap_pidfile_daemon(real_pid_file, expected_cmdline_substr=f"{token}-keepalive")
            )
        finally:
            if proc2.poll() is None:
                proc2.kill()
                proc2.wait(timeout=2.0)

        # 3. Verify relative pid_path anchoring survives os.chdir() and nested ExitStacks stay tracked.
        import harness as harness_mod

        orig_cwd = os.getcwd()
        sub_a = env.root / "rel_a"
        sub_b = env.root / "rel_b"
        sub_a.mkdir(parents=True, exist_ok=True)
        sub_b.mkdir(parents=True, exist_ok=True)
        rel_token = f"{token}-relchdir"
        proc3 = subprocess.Popen(
            [sys.executable, "-c", sleeper_code, rel_token],
            start_new_session=True,
            stdout=subprocess.DEVNULL,
            stderr=subprocess.DEVNULL,
        )
        try:
            with contextlib.ExitStack() as outer_stack:
                os.chdir(sub_a)
                anchored = register_pidfile_cleanup(
                    outer_stack,
                    Path("proxy.pid"),
                    expected_cmdline_substr=rel_token,
                    timeout=3.0,
                )
                self.assertTrue(anchored.is_absolute())
                with contextlib.ExitStack() as inner_stack:
                    register_pidfile_cleanup(
                        inner_stack,
                        Path("proxy.pid"),
                        expected_cmdline_substr="unrelated-inner-token",
                        timeout=0.2,
                    )
                    (sub_a / "proxy.pid").write_text(
                        json.dumps({"pid": proc3.pid}), encoding="utf-8"
                    )
                    os.chdir(sub_b)
                # Inner stack exit must not untrack outer stack's registration in _WATCHED_PIDFILES.
                with harness_mod._WATCHED_PIDFILES_LOCK:
                    remaining_paths = [entry[0] for entry in harness_mod._WATCHED_PIDFILES.values()]
                self.assertIn(anchored, remaining_paths)
                self.assertIsNone(proc3.poll(), "Inner mismatched token must not kill proc3")
            # Exiting outer_stack from sub_b must still reap proc3 via anchored path.
            rc3 = proc3.wait(timeout=3.0)
            self.assertIsNotNone(rc3)
        finally:
            os.chdir(orig_cwd)
            if proc3.poll() is None:
                proc3.kill()
                proc3.wait(timeout=2.0)

    def test_self_24_multi_module_tier_discovery_and_duplicate_rejection(self) -> None:
        """SELF-24: run_e2e supports tuple and str TIER_MODULES, rejects cross-module duplicates, and filters by module."""
        import run_e2e

        mod_a_name = "_self24_fake_mod_a"
        mod_b_name = "_self24_fake_mod_b"
        mod_dup_name = "_self24_fake_mod_dup"

        mod_a = types.ModuleType(mod_a_name)
        mod_b = types.ModuleType(mod_b_name)
        mod_dup = types.ModuleType(mod_dup_name)

        class _FakeCaseA(E2ETestCase):
            TIER = 1
            FEATURE = "FAKE_A"

            def test_fake_alpha(self) -> None:
                """Alpha synthetic test."""

        class _FakeCaseB(E2ETestCase):
            TIER = 1
            FEATURE = "FAKE_B"

            def test_fake_beta(self) -> None:
                """Beta synthetic test."""

        class _FakeCaseDup(E2ETestCase):
            TIER = 1
            FEATURE = "FAKE_DUP"

            def test_fake_alpha(self) -> None:
                """Duplicate alpha synthetic test."""

        _FakeCaseA.__module__ = mod_a_name
        _FakeCaseB.__module__ = mod_b_name
        _FakeCaseDup.__module__ = mod_dup_name
        setattr(mod_a, "_FakeCaseA", _FakeCaseA)
        setattr(mod_b, "_FakeCaseB", _FakeCaseB)
        setattr(mod_dup, "_FakeCaseDup", _FakeCaseDup)

        fake_mods = {mod_a_name: mod_a, mod_b_name: mod_b, mod_dup_name: mod_dup}
        with unittest_mock.patch.dict(sys.modules, fake_mods, clear=False):
            # 1. Tuple multi-module discovery
            with unittest_mock.patch.dict(
                run_e2e.TIER_MODULES, {1: (mod_a_name, mod_b_name)}, clear=True
            ):
                discovered = run_e2e.discover_tests([1], include_self=False)
                self.assertEqual([t.name for t in discovered], ["test_fake_alpha", "test_fake_beta"])
                # filter_tests matches module_name
                only_b = run_e2e.filter_tests(discovered, mod_b_name)
                self.assertEqual([t.name for t in only_b], ["test_fake_beta"])

            # 2. Legacy single-str entry compatibility
            with unittest_mock.patch.dict(
                run_e2e.TIER_MODULES, {1: mod_a_name}, clear=True  # type: ignore[dict-item]
            ):
                discovered_single = run_e2e.discover_tests([1], include_self=False)
                self.assertEqual([t.name for t in discovered_single], ["test_fake_alpha"])

            # 3. Cross-module duplicate rejection
            with unittest_mock.patch.dict(
                run_e2e.TIER_MODULES, {1: (mod_a_name, mod_dup_name)}, clear=True
            ):
                with self.assertRaises(ValueError):
                    run_e2e.discover_tests([1], include_self=False)

    def test_self_25_mock_connect_proxy_captures_targets_and_returns_502(self) -> None:
        """SELF-25: MockConnectProxy records CONNECT targets/headers, returns 502, and cleans up via ExitStack."""
        with contextlib.ExitStack() as stack:
            proxy = stack.enter_context(MockConnectProxy())
            self.assertTrue(proxy.proxy_url.startswith("http://127.0.0.1:"))
            proxy_port = proxy.port

            for target in (
                "daily-cloudcode-pa.googleapis.com:443",
                "cloudcode-pa.googleapis.com:443",
            ):
                with socket.create_connection(("127.0.0.1", proxy_port), timeout=3.0) as sock:
                    req = (
                        f"CONNECT {target} HTTP/1.1\r\n"
                        f"Host: {target}\r\n"
                        "User-Agent: antigravity/2.34.0\r\n"
                        "\r\n"
                    ).encode("ascii")
                    sock.sendall(req)
                    raw_resp = sock.recv(4096)
                self.assertTrue(
                    raw_resp.startswith(b"HTTP/1.1 502 Bad Gateway\r\n"),
                    f"Unexpected CONNECT response: {raw_resp!r}",
                )

            targets = proxy.wait_for_connects(2, timeout=3.0)
            self.assertEqual(
                targets,
                [
                    "daily-cloudcode-pa.googleapis.com:443",
                    "cloudcode-pa.googleapis.com:443",
                ],
            )
            self.assertEqual(len(proxy.captured_connects), 2)
            self.assertEqual(
                proxy.captured_connects[0]["headers"].get("user-agent"),
                "antigravity/2.34.0",
            )
            proxy.clear_targets()
            self.assertEqual(proxy.connect_targets, [])

            # 20-parallel-socket burst must capture 100% of CONNECT requests with zero drops.
            burst_n = 20
            barrier = threading.Barrier(burst_n)

            def _send_burst_connect(idx: int) -> bytes:
                barrier.wait(timeout=5.0)
                target = f"burst-{idx}.cloudcode-pa.googleapis.com:443"
                with socket.create_connection(("127.0.0.1", proxy_port), timeout=5.0) as sock:
                    sock.sendall(f"CONNECT {target} HTTP/1.1\r\nHost: {target}\r\n\r\n".encode("ascii"))
                    return sock.recv(4096)

            with concurrent.futures.ThreadPoolExecutor(max_workers=burst_n) as pool:
                futs = [pool.submit(_send_burst_connect, i) for i in range(burst_n)]
                for fut in futs:
                    raw_resp = fut.result(timeout=10.0)
                    self.assertTrue(
                        raw_resp.startswith(b"HTTP/1.1 502 Bad Gateway\r\n"),
                        f"Unexpected burst CONNECT response: {raw_resp!r}",
                    )
            burst_targets = proxy.wait_for_connects(burst_n, timeout=3.0)
            self.assertEqual(len(burst_targets), burst_n)
            self.assertEqual(len(set(burst_targets)), burst_n)
            proxy.clear_targets()

            # Closing the proxy must promptly wake a blocked wait_for_connects() caller.
            t0 = time.monotonic()
            closer = threading.Timer(0.1, proxy.close)
            closer.start()
            try:
                woke = proxy.wait_for_connects(1, timeout=3.0)
            finally:
                closer.cancel()
                closer.join(timeout=2.0)
            elapsed = time.monotonic() - t0
            self.assertEqual(woke, [])
            self.assertLess(elapsed, 1.5, f"close() did not wake wait_for_connects promptly: {elapsed:.2f}s")

        # After ExitStack teardown, proxy socket must refuse new connections.
        with self.assertRaises(OSError):
            with socket.create_connection(("127.0.0.1", proxy_port), timeout=0.5):
                pass

    def test_self_26_sqlite_usage_harness_helpers(self) -> None:
        """SELF-26: SQLite usage harness helpers handle empty dirs, missing tables, WAL reads, raw bytes, and symlinks."""
        env = self.make_env()

        # 1. Missing and empty directories return empty without creating usage.db.
        missing_dir = env.root / "no_such_usage"
        self.assertEqual(usage_files(missing_dir), [])
        self.assertEqual(usage_storage_files(missing_dir), [])
        self.assertEqual(read_usage_raw_bytes(missing_dir), b"")
        files_m, events_m = poll_usage_events(missing_dir, min_count=1, timeout=0.1, interval=0.02)
        self.assertEqual((files_m, events_m), ([], []))
        self.assertFalse(missing_dir.exists())

        empty_dir = env.root / "empty_usage"
        empty_dir.mkdir()
        self.assertEqual(env.read_usage_events(empty_dir), [])
        self.assertFalse((empty_dir / USAGE_DB_FILENAME).exists())

        # 2. Empty/schema-less DB file does not crash poll_usage_events.
        init_dir = env.root / "initializing_usage"
        init_dir.mkdir()
        (init_dir / USAGE_DB_FILENAME).write_bytes(b"")
        files_i, events_i = poll_usage_events(init_dir, min_count=1, timeout=0.1, interval=0.02)
        self.assertEqual(files_i, [init_dir / USAGE_DB_FILENAME])
        self.assertEqual(events_i, [])

        # 3. WAL-mode usage.db with open writer connection exposes rows in id ASC order + raw WAL bytes.
        wal_dir = env.root / "wal_usage"
        wal_dir.mkdir()
        db_path = wal_dir / USAGE_DB_FILENAME
        with contextlib.closing(sqlite3.connect(str(db_path))) as writer:
            writer.execute("PRAGMA journal_mode = WAL")
            writer.execute(
                """
                CREATE TABLE usage_events (
                    id INTEGER PRIMARY KEY AUTOINCREMENT,
                    timestamp_utc TEXT NOT NULL,
                    request_id TEXT NOT NULL UNIQUE,
                    provider TEXT NOT NULL,
                    model TEXT NOT NULL,
                    input_tokens INTEGER NOT NULL,
                    output_tokens INTEGER NOT NULL,
                    cached_tokens INTEGER NOT NULL,
                    reasoning_tokens INTEGER NOT NULL,
                    latency_ms INTEGER NOT NULL,
                    source TEXT NOT NULL
                )
                """
            )
            writer.execute("PRAGMA user_version = 1")
            writer.execute(
                "INSERT INTO usage_events (timestamp_utc, request_id, provider, model, "
                "input_tokens, output_tokens, cached_tokens, reasoning_tokens, latency_ms, source) "
                "VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
                ("2026-11-25T00:00:00Z", "req-wal-1", "gemini", "gemini-4-hydrogen", 100, 20, 50, 5, 42, "proxy"),
            )
            writer.execute(
                "INSERT INTO usage_events (timestamp_utc, request_id, provider, model, "
                "input_tokens, output_tokens, cached_tokens, reasoning_tokens, latency_ms, source) "
                "VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
                ("2026-11-25T00:00:00Z", "req-wal-2", "gemini", "gemini-4-hydrogen", 200, 30, 80, 10, 64, "proxy"),
            )
            writer.commit()

            self.assertEqual(usage_files(wal_dir), [db_path])
            storage = usage_storage_files(wal_dir)
            self.assertIn(db_path, storage)
            self.assertIn(wal_dir / "usage.db-wal", storage)
            raw = read_usage_raw_bytes(wal_dir)
            self.assertIn(b"req-wal-1", raw)
            self.assertIn(b"req-wal-2", raw)

            files_w, events_w = poll_usage_events(wal_dir, min_count=2, timeout=1.0)
            self.assertEqual(files_w, [db_path])
            self.assertEqual([e["request_id"] for e in events_w], ["req-wal-1", "req-wal-2"])
            self.assertEqual(
                set(events_w[0].keys()),
                {
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
                },
            )
            self.assertEqual(env.read_usage_events(wal_dir), events_w)
            with contextlib.closing(open_usage_db_readonly(db_path)) as ro_conn:
                self.assertEqual(ro_conn.execute("PRAGMA user_version").fetchone()[0], 1)

        # 4. Symlinked usage.db is excluded by usage_files and usage_storage_files.
        if os.name != "nt":
            sym_dir = env.root / "sym_usage"
            sym_dir.mkdir()
            os.symlink(db_path, sym_dir / USAGE_DB_FILENAME)
            self.assertEqual(usage_files(sym_dir), [])
            self.assertEqual(usage_storage_files(sym_dir), [])
