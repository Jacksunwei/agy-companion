"""Pure-Python harness self-tests (`SELF-01` .. `SELF-19`).

Runs via `python3 e2e/run_e2e.py --self-test` with zero external dependencies
and without requiring the compiled `agy-companion` binary.
"""

from __future__ import annotations

import concurrent.futures
import hashlib
import http.client
import json
import os
import socket
import stat
import sys
import threading
import time
import unittest
from pathlib import Path
from typing import Any

from harness import (
    CompanionProcess,
    CompanionStartError,
    E2ETestCase,
    IsolatedEnv,
    STRIPPED_ENV_VARS,
    find_free_port,
    http_raw_request,
    http_stream_request,
    resolve_binary,
    run_cli,
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
            "/v1beta/models/gemini-2.5-pro:generateContent?alt=sse&foo=bar",
            body=body,
            headers={"Content-Type": "application/json", "X-Goog-Api-Key": "TEST_KEY_01"},
        )
        self.assertEqual(resp.status, 200)
        req = mock.last_request()
        self.assertIsNotNone(req)
        assert req is not None
        self.assertEqual(req.method, "POST")
        self.assertEqual(req.raw_target, "/v1beta/models/gemini-2.5-pro:generateContent?alt=sse&foo=bar")
        self.assertEqual(req.path, "/v1beta/models/gemini-2.5-pro:generateContent")
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
            "/v1beta/models/gemini-2.5-pro:generateContent",
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
            "/v1beta/models/gemini-2.5-pro:streamGenerateContent?alt=sse",
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
        call_res = client.save_settings_smart(base_url="http://127.0.0.1:9999", default_model="gemini-2.5-pro")
        echo_args = call_res["structuredContent"]["echo_args"]
        self.assertEqual(echo_args["gemini_base_url"], "http://127.0.0.1:9999")
        self.assertEqual(echo_args["gemini_default_model"], "gemini-2.5-pro")
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
        self.assertGreaterEqual(by_tier.get("T3", 0), 14, f"Tier 3 below target: {by_tier}")
        self.assertGreaterEqual(by_tier.get("T4", 0), 7, f"Tier 4 below target: {by_tier}")
        self.assertEqual(len(tests), len({t.name for t in tests}), "Duplicate test names detected")

        for feat_idx in range(1, 13):
            feat = f"F{feat_idx}"
            t1_count = by_feature.get("T1", {}).get(feat, 0)
            t2_count = by_feature.get("T2", {}).get(feat, 0)
            self.assertGreaterEqual(t1_count, 6, f"{feat} Tier 1 count < 6: {t1_count}")
            self.assertGreaterEqual(t2_count, 6, f"{feat} Tier 2 count < 6: {t2_count}")

        # Additive F14 & F15 coverage check.
        self.assertGreaterEqual(by_feature.get("T1", {}).get("F14", 0), 2)
        self.assertGreaterEqual(by_feature.get("T1", {}).get("F15", 0), 1)
        self.assertGreaterEqual(by_feature.get("T2", {}).get("F14", 0), 1)
        self.assertGreaterEqual(by_feature.get("T2", {}).get("F15", 0), 1)

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
                    "/v1beta/models/gemini-2.5-pro:generateContent",
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
