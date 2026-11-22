"""Strict newline-delimited JSON-RPC 2.0 stdio MCP client for opaque-box E2E tests.

Features:
- Non-blocking daemon stdout & stderr reader threads (zero pipe deadlocks).
- Strict stdout protocol purity auditing (every stdout line must be valid JSON-RPC 2.0).
- Fast EOF detection (wakes all pending waiters immediately on server crash/exit).
- Dynamic `save_settings` schema introspection per Ambiguity Guard A3.
- Process-group SIGTERM -> SIGKILL teardown + atexit PID guard.
"""

from __future__ import annotations

import atexit
import json
import os
import signal
import subprocess
import threading
import time
from typing import Any


_ACTIVE_MCP_PIDS: set[int] = set()
_ACTIVE_MCP_LOCK = threading.Lock()


def _cleanup_active_mcp_pids() -> None:
    with _ACTIVE_MCP_LOCK:
        pids = list(_ACTIVE_MCP_PIDS)
    for pid in pids:
        try:
            os.killpg(pid, signal.SIGKILL)
        except OSError:
            try:
                os.kill(pid, signal.SIGKILL)
            except OSError:
                pass


atexit.register(_cleanup_active_mcp_pids)


_UNSET = object()


class McpError(RuntimeError):
    """Base error for MCP stdio client failures."""


class McpRpcError(McpError):
    """Raised when the MCP server returns a JSON-RPC error object."""

    def __init__(self, error_obj: Any, method: str = "") -> None:
        self.error = error_obj
        self.method = method
        super().__init__(f"MCP JSON-RPC error on {method!r}: {error_obj!r}")


class McpTimeoutError(McpError):
    """Raised when an MCP JSON-RPC request exceeds its deadline."""


class McpEofError(McpError):
    """Raised immediately when the MCP server closes stdout / exits unexpectedly."""


class McpProtocolViolationError(McpError):
    """Raised when non-JSON-RPC bytes pollute MCP stdout."""


class _Waiter:
    def __init__(self) -> None:
        self.event = threading.Event()
        self.response: dict[str, Any] | None = None
        self.eof: bool = False


class McpStdioClient:
    """Newline-delimited JSON-RPC 2.0 client over stdio."""

    def __init__(
        self,
        cmd: list[str],
        *,
        env: dict[str, str] | None = None,
        cwd: str | None = None,
    ) -> None:
        self.cmd = list(cmd)
        self.env = env
        self.cwd = cwd
        self._proc: subprocess.Popen[bytes] | None = None
        self._lock = threading.Lock()
        self._write_lock = threading.Lock()
        self._next_id = 0
        self._waiters: dict[int, _Waiter] = {}
        self._eof = False
        self._closed = False
        self.raw_stdout_lines: list[bytes] = []
        self.stdout_protocol_violations: list[str] = []
        self.notifications: list[dict[str, Any]] = []
        self.stderr_lines: list[str] = []
        self._stdout_thread: threading.Thread | None = None
        self._stderr_thread: threading.Thread | None = None
        self._save_settings_props_cache: dict[str, Any] | None = None

    @property
    def pid(self) -> int | None:
        return self._proc.pid if self._proc is not None else None

    @property
    def returncode(self) -> int | None:
        return self._proc.poll() if self._proc is not None else None

    @property
    def stderr_text(self) -> str:
        with self._lock:
            return "".join(self.stderr_lines)

    @property
    def stdout_text(self) -> str:
        with self._lock:
            return b"".join(self.raw_stdout_lines).decode("utf-8", errors="replace")

    def start(self) -> McpStdioClient:
        if self._proc is not None:
            return self
        proc = subprocess.Popen(
            self.cmd,
            stdin=subprocess.PIPE,
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
            env=self.env,
            cwd=self.cwd,
            bufsize=0,
            start_new_session=True,
        )
        self._proc = proc
        with _ACTIVE_MCP_LOCK:
            _ACTIVE_MCP_PIDS.add(proc.pid)
        self._stdout_thread = threading.Thread(
            target=self._read_stdout_loop,
            args=(proc,),
            name=f"mcp-stdout-{proc.pid}",
            daemon=True,
        )
        self._stderr_thread = threading.Thread(
            target=self._read_stderr_loop,
            args=(proc,),
            name=f"mcp-stderr-{proc.pid}",
            daemon=True,
        )
        self._stdout_thread.start()
        self._stderr_thread.start()
        return self

    def __enter__(self) -> McpStdioClient:
        return self.start()

    def __exit__(self, exc_type: Any, exc: Any, tb: Any) -> None:
        self.close()

    def _read_stdout_loop(self, proc: subprocess.Popen[bytes]) -> None:
        assert proc.stdout is not None
        try:
            while True:
                raw_line = proc.stdout.readline()
                if not raw_line:
                    break
                with self._lock:
                    self.raw_stdout_lines.append(raw_line)
                stripped = raw_line.rstrip(b"\r\n")
                if not stripped:
                    with self._lock:
                        self.stdout_protocol_violations.append(
                            f"Blank line on MCP stdout: {raw_line!r}"
                        )
                    continue
                try:
                    text = stripped.decode("utf-8")
                    msg = json.loads(text)
                except (UnicodeDecodeError, json.JSONDecodeError) as exc:
                    with self._lock:
                        self.stdout_protocol_violations.append(
                            f"Non-JSON line on MCP stdout ({exc}): {raw_line!r}"
                        )
                    continue
                if not isinstance(msg, dict) or msg.get("jsonrpc") != "2.0":
                    with self._lock:
                        self.stdout_protocol_violations.append(
                            f"Invalid JSON-RPC 2.0 envelope on MCP stdout: {msg!r}"
                        )
                    continue

                if "id" in msg and ("result" in msg or "error" in msg):
                    req_id = msg["id"]
                    with self._lock:
                        waiter = self._waiters.pop(req_id, None)
                    if waiter is not None:
                        waiter.response = msg
                        waiter.event.set()
                else:
                    with self._lock:
                        self.notifications.append(msg)
        except (OSError, ValueError):
            pass
        finally:
            with self._lock:
                self._eof = True
                pending = list(self._waiters.values())
                self._waiters.clear()
            for waiter in pending:
                waiter.eof = True
                waiter.event.set()

    def _read_stderr_loop(self, proc: subprocess.Popen[bytes]) -> None:
        assert proc.stderr is not None
        try:
            while True:
                line = proc.stderr.readline()
                if not line:
                    break
                decoded = line.decode("utf-8", errors="replace")
                with self._lock:
                    self.stderr_lines.append(decoded)
        except (OSError, ValueError):
            pass

    def assert_clean_stdout(self) -> None:
        with self._lock:
            violations = list(self.stdout_protocol_violations)
        if violations:
            joined = "\n  - ".join(violations)
            raise McpProtocolViolationError(
                f"MCP stdout contained {len(violations)} protocol violation(s):\n  - {joined}"
            )

    def _write_line(self, payload_bytes: bytes) -> None:
        proc = self._proc
        if proc is None or proc.stdin is None:
            raise McpEofError("MCP process is not started or stdin is closed")
        data = payload_bytes + b"\n"
        view = memoryview(data)
        total = len(data)
        with self._write_lock:
            try:
                written = 0
                while written < total:
                    n = proc.stdin.write(view[written:])
                    if not n:
                        rc = proc.poll()
                        raise McpEofError(
                            f"Short zero-byte write to MCP stdin (exit={rc})\nstderr:\n{self.stderr_text}"
                        )
                    written += n
                proc.stdin.flush()
            except McpEofError:
                raise
            except (BrokenPipeError, OSError, ValueError) as exc:
                rc = proc.poll()
                raise McpEofError(
                    f"Failed writing to MCP stdin (exit={rc}): {exc}\nstderr:\n{self.stderr_text}"
                ) from exc

    def send_raw(self, raw_bytes: bytes) -> None:
        self._write_line(raw_bytes)

    def notify(self, method: str, params: dict[str, Any] | None = None) -> None:
        msg: dict[str, Any] = {"jsonrpc": "2.0", "method": method}
        if params is not None:
            msg["params"] = params
        self._write_line(json.dumps(msg, separators=(",", ":")).encode("utf-8"))

    def request(
        self,
        method: str,
        params: dict[str, Any] | None = None,
        timeout: float = 10.0,
    ) -> dict[str, Any]:
        with self._lock:
            if self._eof:
                rc = self._proc.poll() if self._proc else None
                raise McpEofError(
                    f"MCP stdout already at EOF before {method!r} (exit={rc})\n"
                    f"stderr:\n{''.join(self.stderr_lines)}"
                )
            self._next_id += 1
            req_id = self._next_id
            waiter = _Waiter()
            self._waiters[req_id] = waiter

        msg: dict[str, Any] = {"jsonrpc": "2.0", "id": req_id, "method": method}
        if params is not None:
            msg["params"] = params
        try:
            self._write_line(json.dumps(msg, separators=(",", ":")).encode("utf-8"))
        except Exception:
            with self._lock:
                self._waiters.pop(req_id, None)
            raise

        if not waiter.event.wait(timeout=timeout):
            with self._lock:
                self._waiters.pop(req_id, None)
            raise McpTimeoutError(
                f"MCP request {method!r} (id={req_id}) timed out after {timeout:.1f}s\n"
                f"stderr:\n{self.stderr_text}"
            )
        if waiter.eof or waiter.response is None:
            rc = self._proc.poll() if self._proc else None
            raise McpEofError(
                f"MCP server closed stdout while waiting for {method!r} (exit={rc})\n"
                f"stderr:\n{self.stderr_text}"
            )
        return waiter.response

    def request_ok(
        self,
        method: str,
        params: dict[str, Any] | None = None,
        timeout: float = 10.0,
    ) -> dict[str, Any]:
        resp = self.request(method, params=params, timeout=timeout)
        if "error" in resp:
            raise McpRpcError(resp["error"], method=method)
        result = resp.get("result")
        if not isinstance(result, dict):
            raise McpError(f"Expected dict result for {method!r}, got {resp!r}")
        return result

    def initialize(
        self,
        protocol_version: str = "2025-03-26",
        timeout: float = 10.0,
    ) -> dict[str, Any]:
        result = self.request_ok(
            "initialize",
            params={
                "protocolVersion": protocol_version,
                "capabilities": {},
                "clientInfo": {"name": "agy-e2e-client", "version": "0.1.0"},
            },
            timeout=timeout,
        )
        self.notify("notifications/initialized")
        return result

    def list_resources(self, timeout: float = 10.0) -> dict[str, Any]:
        return self.request_ok("resources/list", timeout=timeout)

    def read_resource(self, uri: str, timeout: float = 10.0) -> dict[str, Any]:
        return self.request_ok("resources/read", params={"uri": uri}, timeout=timeout)

    def read_resource_raw(self, uri: str, timeout: float = 10.0) -> dict[str, Any]:
        return self.request("resources/read", params={"uri": uri}, timeout=timeout)

    def list_tools(self, timeout: float = 10.0) -> dict[str, Any]:
        return self.request_ok("tools/list", timeout=timeout)

    def call_tool(
        self,
        name: str,
        arguments: dict[str, Any] | None = None,
        timeout: float = 15.0,
    ) -> dict[str, Any]:
        params: dict[str, Any] = {"name": name, "arguments": arguments if arguments is not None else {}}
        return self.request_ok("tools/call", params=params, timeout=timeout)

    def call_tool_raw(
        self,
        name: str,
        arguments: dict[str, Any] | None = None,
        timeout: float = 15.0,
    ) -> dict[str, Any]:
        params: dict[str, Any] = {"name": name, "arguments": arguments if arguments is not None else {}}
        return self.request("tools/call", params=params, timeout=timeout)

    def get_save_settings_schema(self, timeout: float = 10.0) -> dict[str, Any]:
        """Introspect `tools/list` for `save_settings.inputSchema.properties` (Guard A3)."""
        if self._save_settings_props_cache is not None:
            return dict(self._save_settings_props_cache)
        tools_res = self.list_tools(timeout=timeout)
        tools = tools_res.get("tools", [])
        props: dict[str, Any] = {}
        for tool in tools:
            if isinstance(tool, dict) and tool.get("name") == "save_settings":
                schema = tool.get("inputSchema") or {}
                if isinstance(schema, dict):
                    raw_props = schema.get("properties")
                    if isinstance(raw_props, dict):
                        props = dict(raw_props)
                break
        self._save_settings_props_cache = props
        return dict(props)

    def build_save_settings_args(
        self,
        *,
        api_key: Any = _UNSET,
        default_model: Any = _UNSET,
        base_url: Any = _UNSET,
        listen_port: Any = _UNSET,
        listen_host: Any = _UNSET,
        clear_api_key: Any = _UNSET,
        usage_dir: Any = _UNSET,
        extra: dict[str, Any] | None = None,
        timeout: float = 10.0,
    ) -> dict[str, Any]:
        """Map logical setting names to exact advertised `save_settings` schema keys (Guard A3)."""
        props = self.get_save_settings_schema(timeout=timeout)

        def pick(candidates: tuple[str, ...]) -> str:
            for key in candidates:
                if key in props:
                    return key
            return candidates[0]

        args: dict[str, Any] = {}
        if api_key is not _UNSET:
            args[pick(("api_key", "gemini_api_key"))] = api_key
        if default_model is not _UNSET:
            args[pick(("default_model", "gemini_default_model"))] = default_model
        if base_url is not _UNSET:
            args[pick(("base_url", "gemini_base_url"))] = base_url
        if listen_port is not _UNSET:
            args[pick(("listen_port", "port"))] = listen_port
        if listen_host is not _UNSET:
            args[pick(("listen_host", "host"))] = listen_host
        if clear_api_key is not _UNSET:
            args[pick(("clear_api_key",))] = clear_api_key
        if usage_dir is not _UNSET:
            args[pick(("usage_dir",))] = usage_dir
        if extra:
            args.update(extra)
        return args

    def save_settings_smart(
        self,
        *,
        api_key: Any = _UNSET,
        default_model: Any = _UNSET,
        base_url: Any = _UNSET,
        listen_port: Any = _UNSET,
        listen_host: Any = _UNSET,
        clear_api_key: Any = _UNSET,
        usage_dir: Any = _UNSET,
        extra: dict[str, Any] | None = None,
        raw: bool = False,
        timeout: float = 15.0,
    ) -> dict[str, Any]:
        """Call `save_settings` using schema-introspected parameter names (Guard A3)."""
        args = self.build_save_settings_args(
            api_key=api_key,
            default_model=default_model,
            base_url=base_url,
            listen_port=listen_port,
            listen_host=listen_host,
            clear_api_key=clear_api_key,
            usage_dir=usage_dir,
            extra=extra,
            timeout=timeout,
        )
        if raw:
            return self.call_tool_raw("save_settings", args, timeout=timeout)
        return self.call_tool("save_settings", args, timeout=timeout)

    @staticmethod
    def is_tool_error(envelope_or_result: dict[str, Any]) -> bool:
        """Accept either JSON-RPC `error` or `result.isError == True` as valid rejection."""
        if "error" in envelope_or_result:
            return True
        res = envelope_or_result.get("result", envelope_or_result)
        if isinstance(res, dict) and res.get("isError") is True:
            return True
        return False

    def close(self) -> int | None:
        if self._closed:
            return self.returncode
        self._closed = True
        proc = self._proc
        if proc is None:
            return None

        # 1. Signal graceful EOF on stdin.
        if proc.stdin is not None:
            try:
                proc.stdin.close()
            except OSError:
                pass

        # 2. Wait briefly for clean exit.
        try:
            proc.wait(timeout=2.0)
        except subprocess.TimeoutExpired:
            # 3. SIGTERM entire process group.
            try:
                os.killpg(proc.pid, signal.SIGTERM)
            except (ProcessLookupError, PermissionError, OSError):
                try:
                    proc.terminate()
                except OSError:
                    pass
            try:
                proc.wait(timeout=2.0)
            except subprocess.TimeoutExpired:
                # 4. SIGKILL entire process group.
                try:
                    os.killpg(proc.pid, signal.SIGKILL)
                except (ProcessLookupError, PermissionError, OSError):
                    try:
                        proc.kill()
                    except OSError:
                        pass
                try:
                    proc.wait(timeout=2.0)
                except subprocess.TimeoutExpired:
                    pass

        # 5. Join reader threads and close pipe FDs.
        for thread in (self._stdout_thread, self._stderr_thread):
            if thread is not None and thread.is_alive():
                thread.join(timeout=2.0)
        for pipe in (proc.stdout, proc.stderr):
            if pipe is not None:
                try:
                    pipe.close()
                except OSError:
                    pass

        with _ACTIVE_MCP_LOCK:
            _ACTIVE_MCP_PIDS.discard(proc.pid)
        return proc.returncode
