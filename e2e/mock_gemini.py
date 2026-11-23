"""In-process ephemeral HTTP/1.1 mock servers for opaque-box E2E tests.

Provides:
- MockGeminiServer: 127.0.0.1:<ephemeral|explicit> ThreadingHTTPServer with
  exact request capture (method, wire path, query, headers, raw bytes, SHA-256),
  chunked request decoding, HTTP/1.1 chunked SSE streaming (split multi-byte
  UTF-8, LF/CRLF, multiple usageMetadata, per-chunk delays), FIFO fault queues,
  SO_LINGER TCP RST fault injection, and EchoChecksumAction.
- MockHealthServer: lightweight configurable GET /health & GET /api/stats server
  for binary-vs-proxy version skew diagnostics.
"""

from __future__ import annotations

import dataclasses
import hashlib
import http.server
import json
import socket
import struct
import threading
import time
from typing import Any, Callable
from urllib.parse import parse_qs, urlsplit


DEFAULT_USAGE_METADATA: dict[str, int] = {
    "promptTokenCount": 100,
    "candidatesTokenCount": 25,
    "cachedContentTokenCount": 50,
    "thoughtsTokenCount": 10,
    "totalTokenCount": 135,
}


@dataclasses.dataclass(frozen=True)
class CapturedRequest:
    """Exact wire-level capture of an HTTP request received by MockGeminiServer."""

    seq: int
    monotonic_ts: float
    method: str
    raw_target: str
    path: str
    raw_query: str
    query_params: dict[str, list[str]]
    headers: dict[str, str]
    raw_headers: list[tuple[str, str]]
    body: bytes

    def header(self, name: str) -> str | None:
        target = name.lower()
        for k, v in self.raw_headers:
            if k.lower() == target:
                return v
        return None

    def headers_all(self, name: str) -> list[str]:
        target = name.lower()
        return [v for k, v in self.raw_headers if k.lower() == target]

    @property
    def body_sha256(self) -> str:
        return hashlib.sha256(self.body).hexdigest()


class MockAction:
    """Base class for scripted MockGeminiServer responses."""


@dataclasses.dataclass(frozen=True)
class JsonAction(MockAction):
    """Returns a standard JSON response with optional status, headers, and delay."""

    status: int = 200
    payload: Any = dataclasses.field(default_factory=dict)
    headers: dict[str, str] | None = None
    delay_s: float = 0.0
    content_type: str = "application/json; charset=utf-8"


@dataclasses.dataclass(frozen=True)
class SseStreamAction(MockAction):
    """Streams explicit raw byte chunks via HTTP/1.1 Transfer-Encoding: chunked."""

    chunks: list[bytes]
    status: int = 200
    content_type: str = "text/event-stream"
    chunk_delay_s: float = 0.0
    initial_delay_s: float = 0.0
    extra_headers: dict[str, str] | None = None


@dataclasses.dataclass(frozen=True)
class RawBytesAction(MockAction):
    """Returns exact raw bytes with optional HTTP/1.1 chunked transfer encoding."""

    status: int = 200
    content_type: str = "application/octet-stream"
    body: bytes = b""
    chunked: bool = False
    headers: dict[str, str] | None = None
    delay_s: float = 0.0


@dataclasses.dataclass(frozen=True)
class ConnectionResetAction(MockAction):
    """Forces an immediate TCP RST via SO_LINGER (1, 0), optionally after partial output."""

    send_headers_first: bool = False
    partial_body: bytes = b""


@dataclasses.dataclass(frozen=True)
class EchoChecksumAction(MockAction):
    """Returns JSON echoing the SHA-256 digest and byte length of the captured body."""

    usage: dict[str, int] | None = None
    status: int = 200


@dataclasses.dataclass
class _QueuedEntry:
    action: MockAction
    path_contains: str | None = None
    method: str | None = None


def build_sse_events(
    events: list[dict[str, Any]],
    line_ending: bytes = b"\n",
    include_keepalive: bool = False,
) -> bytes:
    """Encode a list of JSON payloads as SSE `data: <json><eol><eol>` frames."""
    parts: list[bytes] = []
    if include_keepalive:
        parts.append(b": keepalive" + line_ending + line_ending)
    for event in events:
        encoded = json.dumps(event, ensure_ascii=False, separators=(",", ":")).encode("utf-8")
        parts.append(b"data: " + encoded + line_ending + line_ending)
    return b"".join(parts)


def split_bytes_at(data: bytes, offsets: list[int]) -> list[bytes]:
    """Slice `data` at sorted byte offsets into non-empty chunks."""
    clean = sorted({o for o in offsets if 0 < o < len(data)})
    chunks: list[bytes] = []
    prev = 0
    for idx in clean:
        chunks.append(data[prev:idx])
        prev = idx
    if prev < len(data) or not chunks:
        chunks.append(data[prev:])
    return chunks


def split_across_utf8_char(data: bytes, char: str) -> list[bytes]:
    """Split `data` strictly in the middle of a multi-byte UTF-8 sequence for `char`."""
    needle = char.encode("utf-8")
    if len(needle) < 2:
        raise ValueError(f"Expected multi-byte UTF-8 char, got {char!r} ({len(needle)} bytes)")
    pos = data.find(needle)
    if pos == -1:
        raise ValueError(f"Character {char!r} not found in SSE payload")
    mid = pos + (len(needle) // 2)
    return [data[:mid], data[mid:]]


def _read_request_body(handler: http.server.BaseHTTPRequestHandler) -> bytes:
    """Read complete HTTP/1.1 body supporting both Content-Length and chunked transfer."""
    transfer_encoding = (handler.headers.get("Transfer-Encoding") or "").lower()
    if "chunked" in transfer_encoding:
        buf = bytearray()
        while True:
            size_line = handler.rfile.readline()
            if not size_line:
                break
            size_token = size_line.split(b";", 1)[0].strip()
            if not size_token:
                continue
            chunk_size = int(size_token, 16)
            if chunk_size == 0:
                # Consume optional trailer headers until terminating CRLF.
                while True:
                    trailer = handler.rfile.readline()
                    if not trailer or trailer in (b"\r\n", b"\n"):
                        break
                break
            remaining = chunk_size
            while remaining > 0:
                piece = handler.rfile.read(remaining)
                if not piece:
                    break
                buf.extend(piece)
                remaining -= len(piece)
            # Consume trailing CRLF after chunk payload.
            handler.rfile.readline()
        return bytes(buf)

    length_header = handler.headers.get("Content-Length")
    if not length_header:
        return b""
    content_length = int(length_header.strip())
    if content_length <= 0:
        return b""
    buf = bytearray()
    remaining = content_length
    while remaining > 0:
        piece = handler.rfile.read(min(remaining, 65536))
        if not piece:
            break
        buf.extend(piece)
        remaining -= len(piece)
    return bytes(buf)


class _ReusableThreadingHTTPServer(http.server.ThreadingHTTPServer):
    allow_reuse_address = True
    daemon_threads = True
    request_queue_size = 64

    def __init__(
        self,
        server_address: tuple[str, int],
        handler_cls: type[http.server.BaseHTTPRequestHandler],
        owner: Any,
    ) -> None:
        self.owner = owner
        super().__init__(server_address, handler_cls)


class _MockGeminiHandler(http.server.BaseHTTPRequestHandler):
    protocol_version = "HTTP/1.1"

    def log_message(self, format: str, *args: Any) -> None:  # noqa: A002
        owner: MockGeminiServer = self.server.owner  # type: ignore[attr-defined]
        owner._record_log(format % args)

    def _handle_any(self) -> None:
        self.close_connection = True
        owner: MockGeminiServer = self.server.owner  # type: ignore[attr-defined]
        body = _read_request_body(self)
        split = urlsplit(self.path)
        raw_headers = [(k, v) for k, v in self.headers.items()]
        headers_dict = dict(raw_headers)
        captured = owner._record_request(
            method=self.command,
            raw_target=self.path,
            path=split.path,
            raw_query=split.query,
            query_params=parse_qs(split.query, keep_blank_values=True),
            headers=headers_dict,
            raw_headers=raw_headers,
            body=body,
        )
        action = owner._resolve_action(captured)
        self._execute_action(owner, captured, action)

    def _execute_action(
        self,
        owner: MockGeminiServer,
        captured: CapturedRequest,
        action: MockAction,
    ) -> None:
        if isinstance(action, ConnectionResetAction):
            try:
                if action.send_headers_first:
                    self.send_response(200)
                    self.send_header("Content-Type", "text/event-stream")
                    self.send_header("Connection", "close")
                    self.end_headers()
                    if action.partial_body:
                        self.wfile.write(action.partial_body)
                        self.wfile.flush()
                self.connection.setsockopt(
                    socket.SOL_SOCKET,
                    socket.SO_LINGER,
                    struct.pack("ii", 1, 0),
                )
            except OSError:
                pass
            finally:
                try:
                    self.connection.close()
                except OSError:
                    pass
            return

        if isinstance(action, EchoChecksumAction):
            usage = action.usage if action.usage is not None else dict(owner.default_usage)
            payload = {
                "echo": {
                    "sha256": captured.body_sha256,
                    "length": len(captured.body),
                },
                "candidates": [
                    {
                        "content": {
                            "role": "model",
                            "parts": [{"text": f"echo:{captured.body_sha256}"}],
                        }
                    }
                ],
                "usageMetadata": usage,
            }
            self._send_json(action.status, payload, None, "application/json; charset=utf-8")
            return

        if isinstance(action, JsonAction):
            if action.delay_s > 0:
                if owner._stop_event.wait(action.delay_s):
                    return
            self._send_json(action.status, action.payload, action.headers, action.content_type)
            return

        if isinstance(action, SseStreamAction):
            if action.initial_delay_s > 0:
                if owner._stop_event.wait(action.initial_delay_s):
                    return
            self.send_response(action.status)
            self.send_header("Content-Type", action.content_type)
            self.send_header("Transfer-Encoding", "chunked")
            self.send_header("Cache-Control", "no-cache")
            self.send_header("Connection", "close")
            if action.extra_headers:
                for k, v in action.extra_headers.items():
                    self.send_header(k, v)
            self.end_headers()
            try:
                for idx, chunk in enumerate(action.chunks):
                    if idx > 0 and action.chunk_delay_s > 0:
                        if owner._stop_event.wait(action.chunk_delay_s):
                            break
                    if not chunk:
                        continue
                    frame = f"{len(chunk):x}\r\n".encode("ascii") + chunk + b"\r\n"
                    self.wfile.write(frame)
                    self.wfile.flush()
                self.wfile.write(b"0\r\n\r\n")
                self.wfile.flush()
            except (BrokenPipeError, ConnectionResetError, OSError):
                pass
            return

        if isinstance(action, RawBytesAction):
            if action.delay_s > 0:
                if owner._stop_event.wait(action.delay_s):
                    return
            self.send_response(action.status)
            self.send_header("Content-Type", action.content_type)
            if action.headers:
                for k, v in action.headers.items():
                    self.send_header(k, v)
            self.send_header("Connection", "close")
            if action.chunked:
                self.send_header("Transfer-Encoding", "chunked")
                self.end_headers()
                try:
                    if action.body:
                        frame = f"{len(action.body):x}\r\n".encode("ascii") + action.body + b"\r\n"
                        self.wfile.write(frame)
                    self.wfile.write(b"0\r\n\r\n")
                    self.wfile.flush()
                except (BrokenPipeError, ConnectionResetError, OSError):
                    pass
            else:
                self.send_header("Content-Length", str(len(action.body)))
                self.end_headers()
                try:
                    if action.body:
                        self.wfile.write(action.body)
                    self.wfile.flush()
                except (BrokenPipeError, ConnectionResetError, OSError):
                    pass
            return

        self._send_json(500, {"error": {"message": f"Unknown mock action: {type(action)!r}"}}, None, "application/json")

    def _send_json(
        self,
        status: int,
        payload: Any,
        headers: dict[str, str] | None,
        content_type: str,
    ) -> None:
        raw = json.dumps(payload, ensure_ascii=False).encode("utf-8")
        self.send_response(status)
        self.send_header("Content-Type", content_type)
        self.send_header("Content-Length", str(len(raw)))
        self.send_header("Connection", "close")
        if headers:
            for k, v in headers.items():
                self.send_header(k, v)
        self.end_headers()
        try:
            self.wfile.write(raw)
            self.wfile.flush()
        except (BrokenPipeError, ConnectionResetError, OSError):
            pass

    do_GET = _handle_any
    do_POST = _handle_any
    do_PUT = _handle_any
    do_DELETE = _handle_any
    do_PATCH = _handle_any
    do_OPTIONS = _handle_any
    do_HEAD = _handle_any


class MockGeminiServer:
    """Ephemeral or fixed-port HTTP/1.1 mock Gemini API server."""

    def __init__(
        self,
        host: str = "127.0.0.1",
        port: int = 0,
        expected_api_key: str | None = None,
        default_usage: dict[str, int] | None = None,
    ) -> None:
        self._bind_host = host
        self._requested_port = port
        self.expected_api_key = expected_api_key
        self.default_usage: dict[str, int] = (
            dict(default_usage) if default_usage is not None else dict(DEFAULT_USAGE_METADATA)
        )
        self._lock = threading.Lock()
        self._req_cond = threading.Condition(self._lock)
        self._stop_event = threading.Event()
        self._requests: list[CapturedRequest] = []
        self._queue: list[_QueuedEntry] = []
        self._handler_fn: Callable[[CapturedRequest], MockAction | None] | None = None
        self._logs: list[str] = []
        self._seq = 0
        self._httpd: _ReusableThreadingHTTPServer | None = None
        self._thread: threading.Thread | None = None
        self._actual_port: int = port

    @property
    def host(self) -> str:
        return self._bind_host

    @property
    def port(self) -> int:
        return self._actual_port

    @property
    def base_url(self) -> str:
        return f"http://{self._bind_host}:{self._actual_port}"

    @property
    def base_url_with_v1beta(self) -> str:
        return f"http://{self._bind_host}:{self._actual_port}/v1beta"

    @property
    def requests(self) -> list[CapturedRequest]:
        with self._lock:
            return list(self._requests)

    @property
    def logs(self) -> list[str]:
        with self._lock:
            return list(self._logs)

    def start(self) -> MockGeminiServer:
        if self._httpd is not None:
            return self
        self._stop_event.clear()
        httpd = _ReusableThreadingHTTPServer(
            (self._bind_host, self._requested_port),
            _MockGeminiHandler,
            owner=self,
        )
        self._actual_port = int(httpd.server_address[1])
        self._httpd = httpd
        thread = threading.Thread(
            target=httpd.serve_forever,
            kwargs={"poll_interval": 0.05},
            name=f"mock-gemini-{self._actual_port}",
            daemon=True,
        )
        thread.start()
        self._thread = thread
        return self

    def stop(self) -> None:
        self._stop_event.set()
        httpd = self._httpd
        thread = self._thread
        self._httpd = None
        self._thread = None
        if httpd is not None:
            try:
                httpd.shutdown()
            except OSError:
                pass
            try:
                httpd.server_close()
            except OSError:
                pass
        if thread is not None and thread.is_alive():
            thread.join(timeout=3.0)

    def __enter__(self) -> MockGeminiServer:
        return self.start()

    def __exit__(self, exc_type: Any, exc: Any, tb: Any) -> None:
        self.stop()

    def enqueue(
        self,
        action: MockAction,
        *,
        path_contains: str | None = None,
        method: str | None = None,
    ) -> None:
        with self._lock:
            self._queue.append(
                _QueuedEntry(
                    action=action,
                    path_contains=path_contains,
                    method=method.upper() if method else None,
                )
            )

    def set_handler(self, fn: Callable[[CapturedRequest], MockAction | None] | None) -> None:
        with self._lock:
            self._handler_fn = fn

    def clear_requests(self) -> None:
        with self._lock:
            self._requests.clear()

    def clear_queue(self) -> None:
        with self._lock:
            self._queue.clear()

    def last_request(self) -> CapturedRequest | None:
        with self._lock:
            return self._requests[-1] if self._requests else None

    def wait_for_requests(self, count: int, timeout: float = 5.0) -> list[CapturedRequest]:
        deadline = time.monotonic() + timeout
        with self._req_cond:
            while len(self._requests) < count:
                remaining = deadline - time.monotonic()
                if remaining <= 0:
                    break
                self._req_cond.wait(timeout=remaining)
            return list(self._requests)

    def _record_log(self, line: str) -> None:
        with self._lock:
            self._logs.append(line)
            if len(self._logs) > 200:
                del self._logs[: len(self._logs) - 200]

    def _record_request(
        self,
        *,
        method: str,
        raw_target: str,
        path: str,
        raw_query: str,
        query_params: dict[str, list[str]],
        headers: dict[str, str],
        raw_headers: list[tuple[str, str]],
        body: bytes,
    ) -> CapturedRequest:
        with self._req_cond:
            self._seq += 1
            captured = CapturedRequest(
                seq=self._seq,
                monotonic_ts=time.monotonic(),
                method=method,
                raw_target=raw_target,
                path=path,
                raw_query=raw_query,
                query_params=query_params,
                headers=headers,
                raw_headers=raw_headers,
                body=body,
            )
            self._requests.append(captured)
            self._req_cond.notify_all()
            return captured

    def _resolve_action(self, captured: CapturedRequest) -> MockAction:
        handler_fn: Callable[[CapturedRequest], MockAction | None] | None = None
        with self._lock:
            for idx, entry in enumerate(self._queue):
                if entry.method and entry.method != captured.method.upper():
                    continue
                if entry.path_contains and entry.path_contains not in captured.raw_target:
                    continue
                return self._queue.pop(idx).action
            handler_fn = self._handler_fn

        if handler_fn is not None:
            custom = handler_fn(captured)
            if custom is not None:
                return custom

        return self._default_action(captured)

    def _default_action(self, captured: CapturedRequest) -> MockAction:
        # Health check: GET /v1beta/models?pageSize=1
        if captured.method == "GET" and captured.path.rstrip("/") == "/v1beta/models":
            req_key = captured.header("x-goog-api-key")
            if self.expected_api_key is not None and req_key != self.expected_api_key:
                return JsonAction(
                    status=400,
                    payload={
                        "error": {
                            "code": 400,
                            "message": "API key not valid. Please pass a valid API key.",
                            "status": "INVALID_ARGUMENT",
                            "details": [
                                {
                                    "@type": "type.googleapis.com/google.rpc.ErrorInfo",
                                    "reason": "API_KEY_INVALID",
                                }
                            ],
                        }
                    },
                )
            return JsonAction(
                status=200,
                payload={"models": [{"name": "models/gemini-2.5-pro"}]},
            )

        if captured.method == "POST" and captured.path.endswith(":generateContent"):
            return JsonAction(
                status=200,
                payload={
                    "candidates": [
                        {
                            "content": {
                                "role": "model",
                                "parts": [{"text": "mock unary response"}],
                            }
                        }
                    ],
                    "usageMetadata": dict(self.default_usage),
                },
            )

        if captured.method == "POST" and captured.path.endswith(":streamGenerateContent"):
            chunk1 = build_sse_events(
                [
                    {
                        "candidates": [
                            {"content": {"role": "model", "parts": [{"text": "Hello "}]}}
                        ]
                    }
                ]
            )
            chunk2 = build_sse_events(
                [
                    {
                        "candidates": [
                            {"content": {"role": "model", "parts": [{"text": "world"}]}}
                        ],
                        "usageMetadata": dict(self.default_usage),
                    }
                ]
            )
            return SseStreamAction(chunks=[chunk1, chunk2])

        return JsonAction(
            status=200,
            payload={"ok": True, "path": captured.raw_target, "method": captured.method},
        )


class _MockHealthHandler(http.server.BaseHTTPRequestHandler):
    protocol_version = "HTTP/1.1"

    def log_message(self, format: str, *args: Any) -> None:  # noqa: A002
        pass

    def do_GET(self) -> None:  # noqa: N802
        self.close_connection = True
        owner: MockHealthServer = self.server.owner  # type: ignore[attr-defined]
        path = urlsplit(self.path).path
        owner._record_path(path)
        if path == "/health":
            payload = owner.health_payload
            status = owner.health_status
        elif path == "/api/stats":
            payload = owner.stats_payload
            status = 200
        else:
            payload = {"error": "not found"}
            status = 404
        raw = json.dumps(payload).encode("utf-8")
        self.send_response(status)
        self.send_header("Content-Type", "application/json")
        self.send_header("X-Content-Type-Options", "nosniff")
        self.send_header("Cache-Control", "no-store")
        self.send_header("Content-Length", str(len(raw)))
        self.send_header("Connection", "close")
        self.end_headers()
        try:
            self.wfile.write(raw)
            self.wfile.flush()
        except OSError:
            pass


class MockHealthServer:
    """Lightweight loopback server returning configurable /health & /api/stats payloads."""

    def __init__(
        self,
        *,
        version: str = "0.1.0",
        protocol_version: int = 1,
        status: str = "ok",
        health_status: int = 200,
        host: str = "127.0.0.1",
        port: int = 0,
        stats_payload: dict[str, Any] | None = None,
    ) -> None:
        self._host = host
        self._requested_port = port
        self._actual_port = port
        self.health_status = health_status
        self.health_payload: dict[str, Any] = {
            "status": status,
            "version": version,
            "protocol_version": protocol_version,
        }
        self.stats_payload: dict[str, Any] = stats_payload or {
            "requests": 0,
            "prompt_tokens": 0,
            "cached_tokens": 0,
            "output_tokens": 0,
            "reasoning_tokens": 0,
            "cache_hit_ratio": 0.0,
        }
        self._lock = threading.Lock()
        self._paths: list[str] = []
        self._httpd: _ReusableThreadingHTTPServer | None = None
        self._thread: threading.Thread | None = None

    @property
    def host(self) -> str:
        return self._host

    @property
    def port(self) -> int:
        return self._actual_port

    @property
    def base_url(self) -> str:
        return f"http://{self._host}:{self._actual_port}"

    @property
    def requested_paths(self) -> list[str]:
        with self._lock:
            return list(self._paths)

    def _record_path(self, path: str) -> None:
        with self._lock:
            self._paths.append(path)

    def start(self) -> MockHealthServer:
        if self._httpd is not None:
            return self
        httpd = _ReusableThreadingHTTPServer(
            (self._host, self._requested_port),
            _MockHealthHandler,
            owner=self,
        )
        self._actual_port = int(httpd.server_address[1])
        self._httpd = httpd
        thread = threading.Thread(
            target=httpd.serve_forever,
            kwargs={"poll_interval": 0.05},
            name=f"mock-health-{self._actual_port}",
            daemon=True,
        )
        thread.start()
        self._thread = thread
        return self

    def stop(self) -> None:
        httpd = self._httpd
        thread = self._thread
        self._httpd = None
        self._thread = None
        if httpd is not None:
            try:
                httpd.shutdown()
            except OSError:
                pass
            try:
                httpd.server_close()
            except OSError:
                pass
        if thread is not None and thread.is_alive():
            thread.join(timeout=3.0)

    def __enter__(self) -> MockHealthServer:
        return self.start()

    def __exit__(self, exc_type: Any, exc: Any, tb: Any) -> None:
        self.stop()
