"""Pure-stdlib mock Antigravity Language Server for opaque-box E2E tests.

Speaks the exact wire protocol used by the local Antigravity LS over loopback HTTP/1.1:
- Dual CSRF header validation (`x-antigravity-csrf-token` and `x-codeium-csrf-token`)
- Unary JSON RPCs:
  - `GetCascadeModelConfigData`
  - `GetCascadeTrajectorySteps` (`stepOffset`, `trajectoryVerbosity`, `disableRehydration`)
  - `GetAllCascadeTrajectories` (`excludeSubtrajectories`)
- Connect HTTP/1.1 chunked streaming RPC (`application/connect+json`, 5-byte envelope):
  - `JetboxSubscribeToSummaries` (multi-frame initial snapshot + live pushed deltas)
"""

from __future__ import annotations

import dataclasses
import http.server
import json
import struct
import threading
import time
from typing import Any


RPC_PREFIX = "/exa.language_server_pb.LanguageServerService/"


def encode_connect_frame(flags: int, payload: bytes) -> bytes:
    """Encodes one Connect 5-byte envelope `[flags: u8][len: u32_be][payload]`."""
    return struct.pack(">BI", flags & 0xFF, len(payload)) + payload


@dataclasses.dataclass(frozen=True)
class CapturedLsRpc:
    method: str
    headers: dict[str, str]
    body: dict[str, Any]
    monotonic_ts: float


class _ThreadingLsServer(http.server.ThreadingHTTPServer):
    allow_reuse_address = True
    daemon_threads = True
    request_queue_size = 64


class MockLsServer:
    """Ephemeral loopback mock Antigravity Language Server (`127.0.0.1:0`)."""

    def __init__(
        self,
        *,
        csrf_token: str = "e2e-mock-ls-csrf-token-0123456789",
        model_configs: list[dict[str, Any]] | None = None,
        initial_snapshot_frames: list[dict[str, Any]] | None = None,
        initial_frame_delay_s: float = 0.015,
        summaries_unimplemented: bool = False,
        host: str = "127.0.0.1",
        port: int = 0,
    ) -> None:
        self.host = host
        self.csrf_token = csrf_token
        self.initial_frame_delay_s = float(initial_frame_delay_s)
        self.summaries_unimplemented = bool(summaries_unimplemented)

        self._lock = threading.Lock()
        self._cond = threading.Condition(self._lock)
        self._closed = False
        self._model_configs: list[dict[str, Any]] = list(model_configs or [])
        self._initial_snapshot_frames: list[dict[str, Any]] = list(initial_snapshot_frames or [])
        self._steps_by_cascade: dict[str, list[dict[str, Any]]] = {}
        self._generator_metadata_by_cascade: dict[str, list[dict[str, Any]]] = {}
        self._summaries_by_cascade: dict[str, dict[str, Any]] = {}
        for frame in self._initial_snapshot_frames:
            updates = frame.get("updates")
            if isinstance(updates, dict):
                for cid, summary in updates.items():
                    self._summaries_by_cascade[cid] = dict(summary)

        self._live_deltas: list[dict[str, Any]] = []
        self._captured_rpcs: list[CapturedLsRpc] = []
        self._stream_connections = 0

        parent = self

        class _Handler(http.server.BaseHTTPRequestHandler):
            protocol_version = "HTTP/1.1"

            def log_message(self, format: str, *args: Any) -> None:  # noqa: A002
                return

            def _read_body(self) -> bytes:
                length = int(self.headers.get("Content-Length", "0") or "0")
                if length <= 0:
                    return b""
                return self.rfile.read(length)

            def _lower_headers(self) -> dict[str, str]:
                return {k.lower(): v for k, v in self.headers.items()}

            def _check_csrf(self, hdrs: dict[str, str]) -> bool:
                primary = hdrs.get("x-antigravity-csrf-token")
                legacy = hdrs.get("x-codeium-csrf-token")
                return primary == parent.csrf_token and legacy == parent.csrf_token

            def _send_json(self, status: int, payload: Any) -> None:
                raw = json.dumps(payload).encode("utf-8")
                self.send_response(status)
                self.send_header("Content-Type", "application/json")
                self.send_header("Content-Length", str(len(raw)))
                self.send_header("Connection", "close")
                self.end_headers()
                self.wfile.write(raw)
                self.wfile.flush()

            def _write_chunk(self, data: bytes) -> bool:
                if not data:
                    return True
                try:
                    self.wfile.write(f"{len(data):x}\r\n".encode("ascii"))
                    self.wfile.write(data)
                    self.wfile.write(b"\r\n")
                    self.wfile.flush()
                    return True
                except OSError:
                    return False

            def do_POST(self) -> None:  # noqa: N802
                hdrs = self._lower_headers()
                raw_body = self._read_body()
                if not self.path.startswith(RPC_PREFIX):
                    self._send_json(404, {"code": "unimplemented"})
                    return
                method = self.path[len(RPC_PREFIX) :]
                if not self._check_csrf(hdrs):
                    self._send_json(401, {"code": "unauthenticated"})
                    return

                if method == "JetboxSubscribeToSummaries":
                    self._handle_subscribe_summaries(hdrs)
                    return

                try:
                    parsed_body = json.loads(raw_body.decode("utf-8")) if raw_body else {}
                except json.JSONDecodeError:
                    self._send_json(400, {"code": "invalid_argument"})
                    return
                if not isinstance(parsed_body, dict):
                    parsed_body = {}

                with parent._cond:
                    parent._captured_rpcs.append(
                        CapturedLsRpc(
                            method=method,
                            headers=hdrs,
                            body=parsed_body,
                            monotonic_ts=time.monotonic(),
                        )
                    )
                    parent._cond.notify_all()

                if method == "GetCascadeModelConfigData":
                    with parent._lock:
                        configs = list(parent._model_configs)
                    self._send_json(200, {"clientModelConfigs": configs})
                    return

                if method == "GetCascadeTrajectorySteps":
                    cid = str(parsed_body.get("cascadeId", ""))
                    offset = int(parsed_body.get("stepOffset", 0) or 0)
                    with parent._lock:
                        all_steps = parent._steps_by_cascade.get(cid)
                        if all_steps is None:
                            self._send_json(404, {"code": "not_found"})
                            return
                        sliced = list(all_steps[offset:])
                    self._send_json(200, {"steps": sliced})
                    return

                if method == "GetCascadeTrajectoryGeneratorMetadata":
                    cid = str(parsed_body.get("cascadeId", ""))
                    offset = int(parsed_body.get("generatorMetadataOffset", 0) or 0)
                    with parent._lock:
                        all_meta = parent._generator_metadata_by_cascade.get(cid)
                        if all_meta is None:
                            self._send_json(404, {"code": "not_found"})
                            return
                        sliced = list(all_meta[offset:])
                    self._send_json(200, {"generatorMetadata": sliced})
                    return

                if method == "GetAllCascadeTrajectories":
                    with parent._lock:
                        summaries = {k: dict(v) for k, v in parent._summaries_by_cascade.items()}
                    self._send_json(200, {"trajectorySummaries": summaries})
                    return

                self._send_json(404, {"code": "unimplemented"})

            def _handle_subscribe_summaries(self, hdrs: dict[str, str]) -> None:
                with parent._cond:
                    parent._captured_rpcs.append(
                        CapturedLsRpc(
                            method="JetboxSubscribeToSummaries",
                            headers=hdrs,
                            body={},
                            monotonic_ts=time.monotonic(),
                        )
                    )
                    parent._stream_connections += 1
                    unimpl = parent.summaries_unimplemented
                    initial_frames = list(parent._initial_snapshot_frames)
                    next_delta_idx = 0
                    parent._cond.notify_all()

                if unimpl:
                    self._send_json(501, {"code": "unimplemented"})
                    return

                self.send_response(200)
                self.send_header("Content-Type", "application/connect+json")
                self.send_header("Transfer-Encoding", "chunked")
                self.send_header("Connection", "close")
                self.end_headers()

                for idx, frame in enumerate(initial_frames):
                    if idx > 0 and parent.initial_frame_delay_s > 0:
                        time.sleep(parent.initial_frame_delay_s)
                    payload = json.dumps(frame).encode("utf-8")
                    if not self._write_chunk(encode_connect_frame(0x00, payload)):
                        return

                while True:
                    with parent._cond:
                        while not parent._closed and next_delta_idx >= len(parent._live_deltas):
                            parent._cond.wait(timeout=0.05)
                        if parent._closed:
                            break
                        pending = parent._live_deltas[next_delta_idx:]
                        next_delta_idx = len(parent._live_deltas)

                    for delta in pending:
                        payload = json.dumps(delta).encode("utf-8")
                        if not self._write_chunk(encode_connect_frame(0x00, payload)):
                            return

                # Send clean EndStream frame + zero chunk on server close.
                self._write_chunk(encode_connect_frame(0x02, b"{}"))
                try:
                    self.wfile.write(b"0\r\n\r\n")
                    self.wfile.flush()
                except OSError:
                    pass

        self._server = _ThreadingLsServer((host, port), _Handler)
        self.port = int(self._server.server_address[1])
        self._thread = threading.Thread(
            target=self._server.serve_forever,
            kwargs={"poll_interval": 0.01},
            daemon=True,
        )
        self._thread.start()

    @property
    def address(self) -> str:
        return f"localhost:{self.port}"

    def set_steps(self, cascade_id: str, steps: list[dict[str, Any]]) -> None:
        with self._lock:
            self._steps_by_cascade[cascade_id] = list(steps)

    def set_generator_metadata(self, cascade_id: str, metadata: list[dict[str, Any]]) -> None:
        with self._lock:
            self._generator_metadata_by_cascade[cascade_id] = list(metadata)

    def push_summary_delta(
        self,
        updates: dict[str, dict[str, Any]],
        *,
        deletes: list[str] | None = None,
    ) -> None:
        delta: dict[str, Any] = {"updates": updates}
        if deletes:
            delta["deletes"] = list(deletes)
        with self._cond:
            for cid, summary in updates.items():
                self._summaries_by_cascade[cid] = dict(summary)
            for cid in deletes or ():
                self._summaries_by_cascade.pop(cid, None)
            self._live_deltas.append(delta)
            self._cond.notify_all()

    def captured_rpcs(self, method: str | None = None) -> list[CapturedLsRpc]:
        with self._lock:
            if method is None:
                return list(self._captured_rpcs)
            return [r for r in self._captured_rpcs if r.method == method]

    def wait_for_stream_connected(self, timeout: float = 5.0) -> bool:
        deadline = time.monotonic() + timeout
        with self._cond:
            while not self._closed and self._stream_connections < 1:
                remaining = deadline - time.monotonic()
                if remaining <= 0:
                    return False
                self._cond.wait(timeout=remaining)
            return self._stream_connections >= 1

    def wait_for_rpc(
        self,
        method: str,
        *,
        min_count: int = 1,
        timeout: float = 5.0,
    ) -> list[CapturedLsRpc]:
        deadline = time.monotonic() + timeout
        with self._cond:
            while not self._closed:
                matches = [r for r in self._captured_rpcs if r.method == method]
                if len(matches) >= min_count:
                    return matches
                remaining = deadline - time.monotonic()
                if remaining <= 0:
                    return matches
                self._cond.wait(timeout=remaining)
            return [r for r in self._captured_rpcs if r.method == method]

    def close(self) -> None:
        with self._cond:
            self._closed = True
            self._cond.notify_all()
        try:
            self._server.shutdown()
        except OSError:
            pass
        try:
            self._server.server_close()
        except OSError:
            pass
        if self._thread.is_alive():
            self._thread.join(timeout=2.0)

    def __enter__(self) -> MockLsServer:
        return self

    def __exit__(self, exc_type: Any, exc: Any, tb: Any) -> None:
        self.close()
