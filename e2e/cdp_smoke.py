"""Lean pure-stdlib Headless Chrome DevTools Protocol (CDP) UI smoke harness.

Provides:
- Chrome discovery (`find_chrome_binary` with authoritative `AGY_E2E_CHROME_BIN`
  override, `require_chrome_or_skip`)
- WCAG 2.1 contrast math (`parse_css_rgb`, `relative_luminance`, `wcag_contrast_ratio`)
- Minimal RFC 6455 masked WebSocket + `CdpClient`
- Hermetic `ChromeBrowser` (isolated `--user-data-dir`, proxy-free env, process-group
  + `atexit` teardown)
- Loopback `MockMcpAppHostServer` (`127.0.0.1:0` avoiding `harness.RESERVED_PORT_RANGE`)
  serving `/host.html` + `/app.html` (`ui/dist/index.html`)
- `UiCdpSession` context manager
"""

from __future__ import annotations

import atexit
import base64
import contextlib
import errno
import hashlib
import http.server
import json
import os
import re
import shutil
import signal
import socket
import socketserver
import struct
import subprocess
import sys
import tempfile
import threading
import time
import unittest
import urllib.parse
import urllib.request
from pathlib import Path
from typing import Any

from harness import RESERVED_PORT_RANGE


WS_MAGIC_GUID = "258EAFA5-E914-47DA-95CA-C5AB0DC85B11"
WS_OP_CONT = 0x0
WS_OP_TEXT = 0x1
WS_OP_BINARY = 0x2
WS_OP_CLOSE = 0x8
WS_OP_PING = 0x9
WS_OP_PONG = 0xA

_DEVTOOLS_WS_RE = re.compile(r"DevTools listening on (ws://127\.0\.0\.1:(\d+)/\S+)")
_MACOS_CHROME_CANDIDATES: tuple[str, ...] = (
    "/Applications/Google Chrome.app/Contents/MacOS/Google Chrome",
    "/Applications/Google Chrome Canary.app/Contents/MacOS/Google Chrome Canary",
    "/Applications/Chromium.app/Contents/MacOS/Chromium",
)
_PATH_CHROME_CANDIDATES: tuple[str, ...] = (
    "google-chrome",
    "google-chrome-stable",
    "chromium",
    "chromium-browser",
)
_KEY_DEFAULTS: dict[str, tuple[str, int]] = {
    "Escape": ("Escape", 27),
    "Tab": ("Tab", 9),
    "Enter": ("Enter", 13),
    " ": ("Space", 32),
}
_STRIPPED_CHROME_ENV: tuple[str, ...] = (
    "HTTP_PROXY",
    "HTTPS_PROXY",
    "ALL_PROXY",
    "http_proxy",
    "https_proxy",
    "all_proxy",
    "AGY_COMPANION_CONFIG",
    "AGY_COMPANION_UI_HTML",
    "GEMINI_API_KEY",
    "GOOGLE_API_KEY",
    "GOOGLE_GEMINI_BASE_URL",
)


class CdpError(RuntimeError):
    """Raised on Chrome DevTools Protocol transport or invocation failure."""


# ---------------------------------------------------------------------------
# Chrome discovery & clean SkipTest
# ---------------------------------------------------------------------------


def _is_executable_file(path: str) -> bool:
    return bool(path) and os.path.isfile(path) and os.access(path, os.X_OK)


def find_chrome_binary() -> str | None:
    """Locate a headless Chrome/Chromium executable.

    `AGY_E2E_CHROME_BIN` is an authoritative override: when present in the
    environment, return it ONLY if non-empty and executable, otherwise return
    `None` without falling through to system Chrome.
    """
    if "AGY_E2E_CHROME_BIN" in os.environ:
        raw = os.environ["AGY_E2E_CHROME_BIN"].strip()
        return raw if raw and _is_executable_file(raw) else None
    chrome_bin = os.environ.get("CHROME_BIN", "").strip()
    if chrome_bin and _is_executable_file(chrome_bin):
        return chrome_bin
    if sys.platform == "darwin":
        for candidate in _MACOS_CHROME_CANDIDATES:
            if _is_executable_file(candidate):
                return candidate
    for name in _PATH_CHROME_CANDIDATES:
        found = shutil.which(name)
        if found and _is_executable_file(found):
            return found
    return None


def require_chrome_or_skip(test_case: unittest.TestCase) -> str:
    """Return Chrome path, fail if `AGY_E2E_REQUIRE_CHROME` is truthy, else skip."""
    binary = find_chrome_binary()
    if binary:
        return binary
    require = os.environ.get("AGY_E2E_REQUIRE_CHROME", "").strip().lower()
    msg = "Headless Chrome not available (install Chrome/Chromium or set AGY_E2E_CHROME_BIN)"
    if require in ("1", "true", "yes"):
        test_case.fail(f"{msg} [AGY_E2E_REQUIRE_CHROME={require!r}]")
    test_case.skipTest(msg)
    raise AssertionError("unreachable")


# ---------------------------------------------------------------------------
# WCAG 2.1 contrast math
# ---------------------------------------------------------------------------


def parse_css_rgb(value: str) -> tuple[int, int, int]:
    """Parse `#rgb`, `#rrggbb`, `#rrggbbaa`, `rgb(...)`, or `rgba(...)` to `(r, g, b)`."""
    s = value.strip().lower()
    if s.startswith("#"):
        h = s[1:]
        if len(h) == 3:
            return (int(h[0] * 2, 16), int(h[1] * 2, 16), int(h[2] * 2, 16))
        if len(h) in (6, 8):
            return (int(h[0:2], 16), int(h[2:4], 16), int(h[4:6], 16))
        raise ValueError(f"Unsupported hex color: {value!r}")
    m = re.match(r"^rgba?\(\s*([0-9.]+)\s*[,\s]\s*([0-9.]+)\s*[,\s]\s*([0-9.]+)", s)
    if m:
        return (
            max(0, min(255, int(round(float(m.group(1)))))),
            max(0, min(255, int(round(float(m.group(2)))))),
            max(0, min(255, int(round(float(m.group(3)))))),
        )
    raise ValueError(f"Unsupported CSS color: {value!r}")


def relative_luminance(rgb: tuple[int, int, int]) -> float:
    def _lin(ch: int) -> float:
        c = ch / 255.0
        return c / 12.92 if c <= 0.03928 else ((c + 0.055) / 1.055) ** 2.4

    return 0.2126 * _lin(rgb[0]) + 0.7152 * _lin(rgb[1]) + 0.0722 * _lin(rgb[2])


def wcag_contrast_ratio(fg: tuple[int, int, int], bg: tuple[int, int, int]) -> float:
    l1 = relative_luminance(fg)
    l2 = relative_luminance(bg)
    hi, lo = (l1, l2) if l1 >= l2 else (l2, l1)
    return (hi + 0.05) / (lo + 0.05)


# ---------------------------------------------------------------------------
# Pure-stdlib RFC 6455 WebSocket & CDP client
# ---------------------------------------------------------------------------


def _apply_ws_mask(payload: bytes, mask_key: bytes) -> bytes:
    n = len(payload)
    if n == 0:
        return b""
    if n < 256:
        return bytes(b ^ mask_key[i & 3] for i, b in enumerate(payload))
    repeated = (mask_key * ((n + 3) // 4))[:n]
    return (int.from_bytes(payload, "big") ^ int.from_bytes(repeated, "big")).to_bytes(n, "big")


def _encode_ws_client_frame(opcode: int, payload: bytes, *, fin: bool = True) -> bytes:
    key = os.urandom(4)
    b0 = (0x80 if fin else 0x00) | (opcode & 0x0F)
    length = len(payload)
    if length < 126:
        header = struct.pack("!BB", b0, 0x80 | length)
    elif length <= 0xFFFF:
        header = struct.pack("!BBH", b0, 0x80 | 126, length)
    else:
        header = struct.pack("!BBQ", b0, 0x80 | 127, length)
    return header + key + _apply_ws_mask(payload, key)


class CdpClient:
    """Minimal pure-stdlib RFC 6455 WebSocket client for Chrome DevTools Protocol."""

    def __init__(self, ws_url: str, *, timeout: float = 15.0) -> None:
        self.ws_url = ws_url
        self.timeout = float(timeout)
        self._next_id = 1
        self._closed = False
        self._buf = bytearray()
        self._sock: socket.socket | None = None
        self.console_errors: list[dict[str, Any]] = []
        self.exceptions: list[dict[str, Any]] = []

        parsed = urllib.parse.urlparse(ws_url)
        if parsed.scheme != "ws":
            raise CdpError(f"Only ws:// URLs are supported, got {ws_url!r}")
        host = parsed.hostname or "127.0.0.1"
        port = parsed.port or 80
        path = parsed.path or "/"
        if parsed.query:
            path = f"{path}?{parsed.query}"

        sock = socket.create_connection((host, port), timeout=self.timeout)
        sock.settimeout(self.timeout)
        self._sock = sock
        try:
            self._handshake(host, port, path)
        except Exception:
            self.close()
            raise

    def _handshake(self, host: str, port: int, path: str) -> None:
        ws_key = base64.b64encode(os.urandom(16)).decode("ascii")
        expected = base64.b64encode(
            hashlib.sha1((ws_key + WS_MAGIC_GUID).encode("ascii")).digest()
        ).decode("ascii")
        req = (
            f"GET {path} HTTP/1.1\r\n"
            f"Host: {host}:{port}\r\n"
            f"Upgrade: websocket\r\n"
            f"Connection: Upgrade\r\n"
            f"Sec-WebSocket-Key: {ws_key}\r\n"
            f"Sec-WebSocket-Version: 13\r\n\r\n"
        ).encode("ascii")
        self._send_all(req)

        deadline = time.monotonic() + self.timeout
        while b"\r\n\r\n" not in self._buf:
            chunk = self._recv_some(deadline)
            if not chunk:
                raise CdpError("EOF during WebSocket handshake")
            self._buf.extend(chunk)
            if len(self._buf) > 65536:
                raise CdpError("WebSocket handshake header exceeded 64 KiB")

        sep = self._buf.index(b"\r\n\r\n")
        head = bytes(self._buf[:sep]).decode("iso-8859-1").split("\r\n")
        del self._buf[: sep + 4]  # Preserve trailing bytes from first WS frame.
        if not head or " 101 " not in head[0]:
            raise CdpError(f"WebSocket handshake failed: {head[0] if head else ''!r}")
        hdrs = {
            k.strip().lower(): v.strip()
            for line in head[1:]
            if ":" in line
            for k, v in [line.split(":", 1)]
        }
        if hdrs.get("sec-websocket-accept", "") != expected:
            raise CdpError("Invalid Sec-WebSocket-Accept header")

    def _send_all(self, data: bytes) -> None:
        if self._closed or self._sock is None:
            raise CdpError("CDP socket is closed")
        view = memoryview(data)
        sent = 0
        while sent < len(data):
            try:
                n = self._sock.send(view[sent:])
                if n <= 0:
                    raise CdpError("Socket send returned 0 bytes")
                sent += n
            except OSError as exc:
                if exc.errno == errno.EINTR:
                    continue
                raise CdpError(f"WebSocket send failed: {exc}") from exc

    def _recv_some(self, deadline: float) -> bytes:
        if self._closed or self._sock is None:
            raise CdpError("CDP socket is closed")
        while True:
            remaining = deadline - time.monotonic()
            if remaining <= 0:
                raise TimeoutError("Timed out reading from CDP WebSocket")
            self._sock.settimeout(remaining)
            try:
                return self._sock.recv(65536)
            except OSError as exc:
                if exc.errno == errno.EINTR:
                    continue
                raise

    def _read_exact(self, n: int, deadline: float) -> bytes:
        while len(self._buf) < n:
            chunk = self._recv_some(deadline)
            if not chunk:
                raise CdpError("Unexpected EOF on CDP WebSocket")
            self._buf.extend(chunk)
        out = bytes(self._buf[:n])
        del self._buf[:n]
        return out

    def _recv_frame(self, deadline: float) -> tuple[bool, int, bytes]:
        b0, b1 = struct.unpack("!BB", self._read_exact(2, deadline))
        fin = bool(b0 & 0x80)
        opcode = b0 & 0x0F
        masked = bool(b1 & 0x80)
        length = b1 & 0x7F
        if length == 126:
            length = struct.unpack("!H", self._read_exact(2, deadline))[0]
        elif length == 127:
            length = struct.unpack("!Q", self._read_exact(8, deadline))[0]
        mask_key = self._read_exact(4, deadline) if masked else None
        payload = self._read_exact(length, deadline) if length else b""
        if mask_key is not None:
            payload = _apply_ws_mask(payload, mask_key)
        return fin, opcode, payload

    def _recv_message(self, deadline: float) -> tuple[int, bytes]:
        data_opcode: int | None = None
        chunks: list[bytes] = []
        while True:
            fin, opcode, payload = self._recv_frame(deadline)
            if opcode == WS_OP_PING:
                self._send_all(_encode_ws_client_frame(WS_OP_PONG, payload))
                continue
            if opcode == WS_OP_PONG:
                continue
            if opcode == WS_OP_CLOSE:
                with contextlib.suppress(Exception):
                    self._send_all(_encode_ws_client_frame(WS_OP_CLOSE, payload[:2]))
                self._closed = True
                raise CdpError("Server closed WebSocket connection")
            if opcode == WS_OP_CONT:
                if data_opcode is None:
                    raise CdpError("Unexpected continuation frame")
                chunks.append(payload)
                if fin:
                    return data_opcode, b"".join(chunks)
            elif opcode in (WS_OP_TEXT, WS_OP_BINARY):
                if fin:
                    return opcode, payload
                data_opcode = opcode
                chunks = [payload]
            else:
                raise CdpError(f"Unsupported WebSocket opcode: 0x{opcode:x}")

    def _record_event(self, msg: dict[str, Any]) -> None:
        method = msg.get("method")
        params = msg.get("params") or {}
        if method == "Runtime.exceptionThrown":
            self.exceptions.append(msg)
        elif method == "Runtime.consoleAPICalled" and params.get("type") == "error":
            self.console_errors.append(msg)
        elif method == "Log.entryAdded" and (params.get("entry") or {}).get("level") == "error":
            self.console_errors.append(msg)

    def pump_events(self, duration: float = 0.05) -> None:
        if self._closed or self._sock is None:
            return
        deadline = time.monotonic() + max(0.005, float(duration))
        while time.monotonic() < deadline:
            try:
                opcode, payload = self._recv_message(deadline)
            except (TimeoutError, socket.timeout, CdpError):
                break
            if opcode == WS_OP_TEXT:
                with contextlib.suppress(ValueError):
                    msg = json.loads(payload.decode("utf-8"))
                    if isinstance(msg, dict) and "method" in msg and "id" not in msg:
                        self._record_event(msg)

    def call(
        self,
        method: str,
        params: dict[str, Any] | None = None,
        *,
        timeout: float | None = None,
    ) -> dict[str, Any]:
        call_id = self._next_id
        self._next_id += 1
        req: dict[str, Any] = {"id": call_id, "method": method}
        if params is not None:
            req["params"] = params
        self._send_all(_encode_ws_client_frame(WS_OP_TEXT, json.dumps(req).encode("utf-8")))

        eff = self.timeout if timeout is None else float(timeout)
        deadline = time.monotonic() + eff
        while True:
            try:
                opcode, payload = self._recv_message(deadline)
            except (TimeoutError, socket.timeout) as exc:
                raise CdpError(f"CDP call {method!r} timed out after {eff}s") from exc
            if opcode != WS_OP_TEXT:
                continue
            msg = json.loads(payload.decode("utf-8"))
            if not isinstance(msg, dict):
                continue
            if "id" not in msg and "method" in msg:
                self._record_event(msg)
                continue
            if msg.get("id") == call_id:
                if "error" in msg:
                    raise CdpError(f"CDP {method!r} failed: {msg['error']}")
                res = msg.get("result")
                return res if isinstance(res, dict) else {}

    def evaluate(
        self,
        expr: str,
        *,
        await_promise: bool = True,
        timeout: float | None = None,
    ) -> Any:
        res = self.call(
            "Runtime.evaluate",
            {"expression": expr, "awaitPromise": await_promise, "returnByValue": True},
            timeout=timeout,
        )
        if "exceptionDetails" in res:
            details = res["exceptionDetails"]
            exc_obj = details.get("exception") or {}
            raise CdpError(
                f"JS exception: {exc_obj.get('description') or details.get('text') or details}"
            )
        remote = res.get("result") or {}
        return None if remote.get("type") == "undefined" else remote.get("value")

    def set_viewport(self, width: int, height: int) -> None:
        self.call(
            "Emulation.setDeviceMetricsOverride",
            {
                "width": int(width),
                "height": int(height),
                "deviceScaleFactor": 1.0,
                "mobile": False,
            },
        )

    def press_key(self, key: str) -> None:
        code, kc = _KEY_DEFAULTS.get(key, (key, 0))
        base = {
            "key": key,
            "code": code,
            "windowsVirtualKeyCode": kc,
            "nativeVirtualKeyCode": kc,
        }
        self.call("Input.dispatchKeyEvent", {"type": "rawKeyDown", **base})
        self.call("Input.dispatchKeyEvent", {"type": "keyUp", **base})

    def close(self) -> None:
        if self._closed and self._sock is None:
            return
        sock = self._sock
        if sock is not None and not self._closed:
            with contextlib.suppress(Exception):
                self._send_all(_encode_ws_client_frame(WS_OP_CLOSE, b""))
        self._closed = True
        self._sock = None
        if sock is not None:
            with contextlib.suppress(Exception):
                sock.close()


# ---------------------------------------------------------------------------
# Hermetic Headless Chrome process manager
# ---------------------------------------------------------------------------


def _terminate_chrome_proc(proc: subprocess.Popen[bytes] | None, pgid: int | None) -> None:
    if proc is None:
        return
    if os.name != "nt":
        if proc.poll() is None:
            if pgid is not None:
                with contextlib.suppress(OSError, AttributeError):
                    os.killpg(pgid, signal.SIGTERM)
            with contextlib.suppress(OSError):
                proc.terminate()
            try:
                proc.wait(timeout=1.0)
            except subprocess.TimeoutExpired:
                if pgid is not None:
                    with contextlib.suppress(OSError, AttributeError):
                        os.killpg(pgid, signal.SIGKILL)
                with contextlib.suppress(OSError):
                    proc.kill()
                with contextlib.suppress(subprocess.TimeoutExpired):
                    proc.wait(timeout=2.0)
        if pgid is not None:
            # Ensure no straggler renderer/helper remains in the Chrome process group.
            with contextlib.suppress(OSError, AttributeError):
                os.killpg(pgid, signal.SIGKILL)
    else:
        if proc.poll() is None:
            with contextlib.suppress(OSError):
                proc.terminate()
            try:
                proc.wait(timeout=1.5)
            except subprocess.TimeoutExpired:
                with contextlib.suppress(OSError):
                    proc.kill()
                with contextlib.suppress(subprocess.TimeoutExpired):
                    proc.wait(timeout=2.0)


class ChromeBrowser:
    """Isolated Headless Chrome process with deterministic process-group teardown."""

    def __init__(
        self,
        chrome_bin: str,
        user_data_dir: Path,
        *,
        startup_timeout: float = 15.0,
    ) -> None:
        self.chrome_bin = chrome_bin
        self.user_data_dir = Path(user_data_dir)
        self.startup_timeout = float(startup_timeout)
        self._proc: subprocess.Popen[bytes] | None = None
        self._pgid: int | None = None
        self.cdp_port: int | None = None
        self._stderr_log: Path = self.user_data_dir / "chrome-stderr.log"

    def _build_env(self) -> dict[str, str]:
        env = dict(os.environ)
        for key in _STRIPPED_CHROME_ENV:
            env.pop(key, None)
        env["NO_PROXY"] = "127.0.0.1,localhost"
        env["no_proxy"] = "127.0.0.1,localhost"
        if sys.platform == "darwin":
            # macOS Chrome network service requires the real account HOME (all profile writes
            # stay confined to --user-data-dir, and poisoned shell HOME will not stall/mutate).
            import pwd

            with contextlib.suppress(KeyError, OSError):
                env["HOME"] = pwd.getpwuid(os.getuid()).pw_dir
        elif os.name != "nt":
            env["HOME"] = str(self.user_data_dir)
        env["XDG_CONFIG_HOME"] = str(self.user_data_dir / "xdg-config")
        env["XDG_DATA_HOME"] = str(self.user_data_dir / "xdg-data")
        env["XDG_CACHE_HOME"] = str(self.user_data_dir / "xdg-cache")
        return env

    def _atexit_cleanup(self) -> None:
        _terminate_chrome_proc(self._proc, self._pgid)

    def start(self) -> ChromeBrowser:
        self.user_data_dir.mkdir(parents=True, exist_ok=True)
        cmd = [
            self.chrome_bin,
            "--headless=new",
            "--remote-debugging-port=0",
            f"--user-data-dir={self.user_data_dir}",
            "--no-proxy-server",
            "--no-first-run",
            "--no-default-browser-check",
            "--disable-gpu",
            "--disable-extensions",
            "--disable-background-networking",
            "--disable-background-timer-throttling",
            "--disable-renderer-backgrounding",
            "--disable-crash-reporter",
            "--disable-breakpad",
            "--mute-audio",
            "--disable-sync",
        ]
        if sys.platform.startswith("linux"):
            cmd.append("--disable-dev-shm-usage")
        cmd.append("about:blank")

        # Redirect stderr to a regular file inside --user-data-dir instead of
        # subprocess.PIPE so detached Chrome helpers cannot deadlock BufferedReader.close().
        with open(self._stderr_log, "wb") as stderr_file:
            popen_kwargs: dict[str, Any] = {
                "stdin": subprocess.DEVNULL,
                "stdout": subprocess.DEVNULL,
                "stderr": stderr_file,
                "env": self._build_env(),
            }
            if os.name != "nt":
                popen_kwargs["start_new_session"] = True
            self._proc = subprocess.Popen(cmd, **popen_kwargs)
        self._pgid = self._proc.pid if os.name != "nt" else None
        atexit.register(self._atexit_cleanup)

        deadline = time.monotonic() + self.startup_timeout
        active_port_file = self.user_data_dir / "DevToolsActivePort"
        while time.monotonic() < deadline:
            if active_port_file.is_file():
                with contextlib.suppress(OSError, ValueError):
                    lines = active_port_file.read_text(encoding="utf-8").splitlines()
                    if lines and lines[0].strip().isdigit():
                        self.cdp_port = int(lines[0].strip())
                        return self
            if self._stderr_log.is_file():
                with contextlib.suppress(OSError):
                    text = self._stderr_log.read_text(encoding="utf-8", errors="replace")
                    m = _DEVTOOLS_WS_RE.search(text)
                    if m:
                        self.cdp_port = int(m.group(2))
                        return self
            if self._proc.poll() is not None:
                break
            time.sleep(0.02)

        tail = ""
        with contextlib.suppress(OSError):
            tail = "\n".join(
                self._stderr_log.read_text(encoding="utf-8", errors="replace").splitlines()[-20:]
            )
        self.stop()
        raise CdpError(f"Headless Chrome failed to start CDP within {self.startup_timeout}s:\n{tail}")

    def open_page_client(self, *, timeout: float = 15.0) -> CdpClient:
        if self.cdp_port is None:
            raise CdpError("ChromeBrowser is not started")
        opener = urllib.request.build_opener(urllib.request.ProxyHandler({}))
        deadline = time.monotonic() + timeout
        while time.monotonic() < deadline:
            with contextlib.suppress(Exception):
                with opener.open(f"http://127.0.0.1:{self.cdp_port}/json/list", timeout=2.0) as resp:
                    targets = json.loads(resp.read().decode("utf-8"))
                for t in targets:
                    if t.get("type") == "page" and t.get("webSocketDebuggerUrl"):
                        return CdpClient(t["webSocketDebuggerUrl"], timeout=timeout)
                req = urllib.request.Request(
                    f"http://127.0.0.1:{self.cdp_port}/json/new?about:blank", method="PUT"
                )
                with opener.open(req, timeout=2.0) as resp:
                    created = json.loads(resp.read().decode("utf-8"))
                if created.get("webSocketDebuggerUrl"):
                    return CdpClient(created["webSocketDebuggerUrl"], timeout=timeout)
            time.sleep(0.05)
        raise CdpError(f"No CDP page target became available on 127.0.0.1:{self.cdp_port}")

    def stop(self) -> None:
        with contextlib.suppress(Exception):
            atexit.unregister(self._atexit_cleanup)
        _terminate_chrome_proc(self._proc, self._pgid)
        self._proc = None
        self._pgid = None


# ---------------------------------------------------------------------------
# Minimal Loopback Mock MCP App Host & UiCdpSession
# ---------------------------------------------------------------------------

_MOCK_HOST_HTML = r"""<!doctype html>
<html lang="en">
<head>
  <meta charset="UTF-8" />
  <link rel="icon" href="data:," />
  <title>Antigravity Companion Smoke Host</title>
  <style>
    html, body { margin: 0; padding: 0; width: 100%; height: 100%; overflow: hidden; background: #1b1c1f; }
    #app-frame { border: 0; width: 100%; height: 100%; display: block; }
  </style>
</head>
<body>
  <iframe id="app-frame" src="/app.html"></iframe>
  <script>
    (function () {
      const boot = (window.__MOCK_BOOT_CONFIG && typeof window.__MOCK_BOOT_CONFIG === "object")
        ? window.__MOCK_BOOT_CONFIG : {};
      const frame = document.getElementById("app-frame");
      const clone = (v) => (v === undefined ? undefined : JSON.parse(JSON.stringify(v)));
      let theme = boot.theme === "light" ? "light" : "dark";
      let snapshot = boot.initialSnapshot !== undefined ? clone(boot.initialSnapshot) : null;
      const usageByRange = boot.initialUsage ? clone(boot.initialUsage) : {};
      let initialized = false;

      const post = (msg) => frame && frame.contentWindow && frame.contentWindow.postMessage(msg, "*");
      const toolResult = (structured) => ({
        content: [{ type: "text", text: JSON.stringify(structured) }],
        structuredContent: structured,
        isError: false,
      });
      const emptyUsage = (range) => ({
        range: range || "7d",
        since_utc: "2026-05-09T00:00:00Z",
        totals: { label: "", requests: 0, input_tokens: 0, cached_tokens: 0, output_tokens: 0, reasoning_tokens: 0 },
        buckets: [], by_model: [], by_provider: [],
      });

      window.addEventListener("message", (event) => {
        if (!frame || event.source !== frame.contentWindow) return;
        const msg = event.data;
        if (!msg || typeof msg !== "object" || msg.jsonrpc !== "2.0") return;
        if (msg.method === "ui/initialize") {
          post({
            jsonrpc: "2.0", id: msg.id,
            result: {
              protocolVersion: "2026-01-26",
              hostInfo: { name: "Antigravity Smoke Host", version: "0.1.0" },
              hostCapabilities: { serverTools: {}, openLinks: {} },
              hostContext: { theme, displayMode: "inline", platform: "desktop" },
            },
          });
          return;
        }
        if (msg.method === "ui/notifications/initialized") {
          initialized = true;
          if (snapshot !== null) {
            post({ jsonrpc: "2.0", method: "ui/notifications/tool-input", params: { arguments: {} } });
            post({ jsonrpc: "2.0", method: "ui/notifications/tool-result", params: toolResult(clone(snapshot)) });
          }
          return;
        }
        if (msg.method === "ui/open-link") {
          post({ jsonrpc: "2.0", id: msg.id, result: {} });
          return;
        }
        if (msg.method === "tools/call") {
          const name = msg.params && msg.params.name;
          const args = (msg.params && msg.params.arguments) || {};
          if (name === "open_settings" || name === "get_settings") {
            post({ jsonrpc: "2.0", id: msg.id, result: toolResult(clone(snapshot || {})) });
          } else if (name === "get_usage") {
            const r = args.range || "7d";
            post({ jsonrpc: "2.0", id: msg.id, result: toolResult(clone(usageByRange[r] || emptyUsage(r))) });
          } else {
            post({ jsonrpc: "2.0", id: msg.id, error: { code: -32601, message: "Unknown tool: " + name } });
          }
        }
      });

      const AsyncFunction = Object.getPrototypeOf(async function () {}).constructor;
      window.__mockHost = {
        isInitialized: () => initialized,
        doc: () => (frame ? frame.contentDocument : null),
        evalApp: (source) => {
          const win = frame.contentWindow;
          const doc = frame.contentDocument;
          const gcs = win.getComputedStyle.bind(win);
          let fn;
          try {
            fn = new AsyncFunction("document", "window", "getComputedStyle", "return (\n" + source + "\n);");
          } catch (_err) {
            fn = new AsyncFunction("document", "window", "getComputedStyle", source);
          }
          return fn(doc, win, gcs);
        },
        setTheme: (nextTheme) => {
          theme = nextTheme === "light" ? "light" : "dark";
          post({ jsonrpc: "2.0", method: "ui/notifications/host-context-changed", params: { theme } });
        },
        setSnapshot: (nextSnap) => {
          snapshot = clone(nextSnap);
          post({ jsonrpc: "2.0", method: "ui/notifications/tool-result", params: toolResult(clone(snapshot)) });
        },
      };
    })();
  </script>
</body>
</html>
"""


class _ThreadingHttpServer(socketserver.ThreadingMixIn, http.server.HTTPServer):
    daemon_threads = True
    allow_reuse_address = True

    def server_bind(self) -> None:
        # Skip HTTPServer's socket.getfqdn() reverse-DNS lookup on 127.0.0.1,
        # which intermittently stalls 5-30s on macOS mDNSResponder / corp DNS.
        socketserver.TCPServer.server_bind(self)
        host, port = self.server_address[:2]
        self.server_name = host
        self.server_port = port


class MockMcpAppHostServer:
    """Loopback HTTP server serving `/host.html` and compiled `ui/dist/index.html`."""

    def __init__(self, app_html_bytes: bytes) -> None:
        self._app_html = app_html_bytes
        self._host_html = _MOCK_HOST_HTML.encode("utf-8")
        self._server: _ThreadingHttpServer | None = None
        self._thread: threading.Thread | None = None
        self.port: int = 0

    @property
    def host_url(self) -> str:
        return f"http://127.0.0.1:{self.port}/host.html"

    def start(self) -> MockMcpAppHostServer:
        host_bytes = self._host_html
        app_bytes = self._app_html

        class Handler(http.server.BaseHTTPRequestHandler):
            def log_message(self, format: str, *args: Any) -> None:  # noqa: A002
                return

            def do_GET(self) -> None:  # noqa: N802
                path = urllib.parse.urlsplit(self.path).path
                if path in ("/", "/host.html"):
                    body = host_bytes
                elif path == "/app.html":
                    body = app_bytes
                elif path == "/favicon.ico":
                    self.send_response(204)
                    self.end_headers()
                    return
                else:
                    self.send_response(404)
                    self.end_headers()
                    return
                self.send_response(200)
                self.send_header("Content-Type", "text/html; charset=utf-8")
                self.send_header("Content-Length", str(len(body)))
                self.send_header("Connection", "close")
                self.end_headers()
                self.wfile.write(body)

        skipped: list[_ThreadingHttpServer] = []
        try:
            for _ in range(32):
                srv = _ThreadingHttpServer(("127.0.0.1", 0), Handler)
                if srv.server_address[1] not in RESERVED_PORT_RANGE:
                    self._server = srv
                    self.port = int(srv.server_address[1])
                    break
                skipped.append(srv)
            if self._server is None:
                raise CdpError("Unable to bind MockMcpAppHostServer outside reserved port range")
        finally:
            for s in skipped:
                with contextlib.suppress(Exception):
                    s.server_close()

        self._thread = threading.Thread(
            target=self._server.serve_forever,
            kwargs={"poll_interval": 0.05},
            daemon=True,
        )
        self._thread.start()
        return self

    def stop(self) -> None:
        if self._server is not None:
            with contextlib.suppress(Exception):
                self._server.shutdown()
            with contextlib.suppress(Exception):
                self._server.server_close()
            self._server = None
        if self._thread is not None:
            self._thread.join(timeout=2.0)
            self._thread = None


class UiCdpSession:
    """Hermetic context manager binding MockMcpAppHostServer + ChromeBrowser + CdpClient."""

    def __init__(
        self,
        app_html_bytes: bytes,
        *,
        chrome_bin: str | None = None,
        boot_config: dict[str, Any] | None = None,
        viewport: tuple[int, int] = (460, 760),
        user_data_dir: Path | None = None,
        timeout: float = 15.0,
    ) -> None:
        self._app_html = app_html_bytes
        self._chrome_bin = chrome_bin or find_chrome_binary()
        if not self._chrome_bin:
            raise CdpError("No Chrome binary available for UiCdpSession")
        self._boot_config = boot_config or {}
        self._viewport = viewport
        self._owns_profile = user_data_dir is None
        self._user_data_dir = (
            Path(tempfile.mkdtemp(prefix="agy-cdp-smoke-"))
            if user_data_dir is None
            else Path(user_data_dir)
        )
        self._timeout = float(timeout)
        self._server: MockMcpAppHostServer | None = None
        self._browser: ChromeBrowser | None = None
        self._cdp: CdpClient | None = None

    def __enter__(self) -> UiCdpSession:
        try:
            self._server = MockMcpAppHostServer(self._app_html).start()
            self._browser = ChromeBrowser(
                self._chrome_bin, self._user_data_dir, startup_timeout=self._timeout
            ).start()
            self._cdp = self._browser.open_page_client(timeout=self._timeout)
            self._cdp.call("Page.enable")
            self._cdp.call("Runtime.enable")
            self._cdp.call("Log.enable")
            self._cdp.set_viewport(self._viewport[0], self._viewport[1])
            boot_js = f"window.__MOCK_BOOT_CONFIG = {json.dumps(self._boot_config)};"
            self._cdp.call("Page.addScriptToEvaluateOnNewDocument", {"source": boot_js})
            self._cdp.call("Page.navigate", {"url": self._server.host_url})
            self.wait_for(
                "Boolean(window.__mockHost && window.__mockHost.isInitialized()"
                " && window.__mockHost.doc() && window.__mockHost.doc().querySelector('footer.status-bar'))",
                timeout=self._timeout,
                in_app=False,
            )
            self._flush_frames()
            return self
        except Exception:
            self.close()
            raise

    def __exit__(self, exc_type: Any, exc: Any, tb: Any) -> None:
        self.close()

    def _flush_frames(self) -> None:
        assert self._cdp is not None
        self._cdp.evaluate(
            "new Promise((resolve) => {"
            " const ch = new MessageChannel();"
            " ch.port1.onmessage = () => {"
            "  const d = window.__mockHost && window.__mockHost.doc();"
            "  if (d && d.body) void d.body.offsetHeight;"
            "  resolve(true);"
            " };"
            " ch.port2.postMessage(null);"
            "})"
        )

    def eval_app(self, js_expr: str) -> Any:
        assert self._cdp is not None
        return self._cdp.evaluate(f"window.__mockHost.evalApp({json.dumps(js_expr)})")

    def wait_for(self, js_expr: str, *, timeout: float = 10.0, in_app: bool = True) -> Any:
        assert self._cdp is not None
        deadline = time.monotonic() + timeout
        last: Any = None
        while time.monotonic() < deadline:
            last = self.eval_app(js_expr) if in_app else self._cdp.evaluate(js_expr)
            if last:
                return last
            time.sleep(0.02)
        raise CdpError(f"wait_for timed out after {timeout}s (last={last!r}): {js_expr}")

    def set_viewport(self, width: int, height: int) -> None:
        assert self._cdp is not None
        self._cdp.set_viewport(width, height)
        self._flush_frames()

    def set_theme(self, theme: str) -> None:
        assert self._cdp is not None
        self._cdp.evaluate(f"window.__mockHost.setTheme({json.dumps(theme)})")
        self.wait_for(
            f"document.documentElement.dataset.theme === {json.dumps(theme)}",
            timeout=5.0,
        )
        self._flush_frames()

    def set_snapshot(self, snapshot: dict[str, Any]) -> None:
        assert self._cdp is not None
        self._cdp.evaluate(f"window.__mockHost.setSnapshot({json.dumps(snapshot)})")
        self._flush_frames()

    def press_key(self, key: str) -> None:
        assert self._cdp is not None
        self._cdp.press_key(key)
        self._flush_frames()

    def assert_no_console_errors(self) -> None:
        assert self._cdp is not None
        self._cdp.pump_events(0.04)
        if self._cdp.console_errors or self._cdp.exceptions:
            raise AssertionError(
                f"Browser console errors/exceptions detected: "
                f"console={self._cdp.console_errors!r} exceptions={self._cdp.exceptions!r}"
            )

    def close(self) -> None:
        if self._cdp is not None:
            with contextlib.suppress(Exception):
                self._cdp.call("Browser.close", timeout=1.0)
            with contextlib.suppress(Exception):
                self._cdp.close()
            self._cdp = None
        if self._browser is not None:
            with contextlib.suppress(Exception):
                self._browser.stop()
            self._browser = None
        if self._server is not None:
            with contextlib.suppress(Exception):
                self._server.stop()
            self._server = None
        if self._owns_profile:
            shutil.rmtree(self._user_data_dir, ignore_errors=True)
