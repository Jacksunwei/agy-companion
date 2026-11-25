"""Core opaque-box E2E harness primitives (pure Python 3 stdlib).

Provides:
- TestRuntimeConfig & global runtime config accessors
- E2ETestCase(unittest.TestCase) base class with deterministic ExitStack cleanup
- IsolatedEnv (sanitized env, 0600 atomic config writes, mtime offset/bump helpers)
- CompanionProcess (`agy-companion serve` + `custom_cmd` for self-tests, process-group cleanup)
- run_cli (one-shot CLI execution with captured stdout/stderr/duration)
- http_raw_request (unnormalized raw paths + 413 early socket close resilience)
- http_stream_request (unbuffered read1() incremental SSE arrival measurement)
- poll_usage_events (Guard A13 async JSONL polling)
- find_key_recursive / extract_models_list / resolve_binary / find_free_port
"""

from __future__ import annotations

import atexit
import contextlib
import dataclasses
import errno
import http.client
import json
import os
import signal
import socket
import socketserver
import stat
import subprocess
import tempfile
import threading
import time
import unittest
from pathlib import Path
from typing import Any, Iterator

from mcp_client import McpStdioClient
from mock_gemini import MockGeminiServer, MockHealthServer


E2E_DIR = Path(__file__).resolve().parent
DEFAULT_REPO_ROOT = E2E_DIR.parent

SAFE_HOST_ENV_VARS: tuple[str, ...] = (
    "PATH",
    "LANG",
    "LC_ALL",
    "USER",
    "LOGNAME",
    "TERM",
    "SSL_CERT_FILE",
    "SSL_CERT_DIR",
)

STRIPPED_ENV_VARS: tuple[str, ...] = (
    "GEMINI_API_KEY",
    "GOOGLE_API_KEY",
    "GOOGLE_GEMINI_BASE_URL",
    "CLOUD_CODE_URL",
    "HTTP_PROXY",
    "HTTPS_PROXY",
    "ALL_PROXY",
    "NO_PROXY",
    "http_proxy",
    "https_proxy",
    "all_proxy",
    "no_proxy",
    "AGY_COMPANION_CONFIG",
    "AGY_COMPANION_UI_HTML",
    "AGY_COMPANION_BIN",
    "AGY_COMPANION_HOOK_ACTIVE",
    "AGY_COMPANION_ALLOW_REMOTE_SSH",
    "JETSKI_MCP_ENV_RESTORE_PREFIX",
    "JETSKI_MCP_ENV_RESTORE_VARS",
    "ANTIGRAVITY_EXECUTABLE_DATA_DIR",
    "ANTIGRAVITY_RESOLVING_ENVIRONMENT",
    "SSH_CONNECTION",
    "SSH_CLIENT",
    "ZDOTDIR",
    "SHELL",
)

_ACTIVE_COMPANION_PIDS: set[int] = set()
_ACTIVE_COMPANION_LOCK = threading.Lock()
_WATCHED_PIDFILES: dict[int, tuple[Path, Path | None, str | None]] = {}
_WATCHED_PIDFILES_NEXT_ID = 0
_WATCHED_PIDFILES_LOCK = threading.Lock()


def _cleanup_active_companion_pids() -> None:
    with _ACTIVE_COMPANION_LOCK:
        pids = list(_ACTIVE_COMPANION_PIDS)
    for pid in pids:
        try:
            os.killpg(pid, signal.SIGKILL)
        except OSError:
            try:
                os.kill(pid, signal.SIGKILL)
            except (OSError, ValueError):
                pass


def _read_process_cmdline(pid: int) -> str | None:
    """Read live OS process command line for `pid` (POSIX only; None on Windows/failure)."""
    if os.name == "nt":
        return None
    proc_cmdline = Path(f"/proc/{pid}/cmdline")
    if proc_cmdline.exists():
        try:
            raw = proc_cmdline.read_bytes()
        except OSError:
            return None
        text = raw.replace(b"\x00", b" ").decode("utf-8", errors="replace").strip()
        return text or None
    try:
        completed = subprocess.run(
            ["/bin/ps", "-p", str(pid), "-o", "command="],
            stdout=subprocess.PIPE,
            stderr=subprocess.DEVNULL,
            timeout=2.0,
            check=False,
        )
    except (OSError, subprocess.SubprocessError):
        return None
    if completed.returncode != 0:
        return None
    text = completed.stdout.decode("utf-8", errors="replace").strip()
    return text or None


def _matches_expected_daemon(
    cmdline: str,
    *,
    expected_binary: Path | None,
    expected_cmdline_substr: str | None,
) -> bool:
    """Verify OS command line matches either an explicit test token or `agy-companion serve`."""
    if expected_cmdline_substr is not None:
        if not expected_cmdline_substr.strip():
            return False
        return expected_cmdline_substr in cmdline
    if "agy-companion" not in cmdline or "serve" not in cmdline:
        return False
    if expected_binary is not None:
        return str(expected_binary) in cmdline or expected_binary.name in cmdline
    return True


def _signal_pid_safely(pid: int, sig: int) -> None:
    """Signal PGID only when `pid` is its own process-group leader and not our own PGID."""
    use_pgid = False
    if hasattr(os, "getpgid") and hasattr(os, "getpgrp") and hasattr(os, "killpg"):
        try:
            pgid = os.getpgid(pid)
            if pgid == pid and pgid != os.getpgrp():
                use_pgid = True
        except (OSError, OverflowError, ValueError):
            use_pgid = False
    try:
        if use_pgid:
            os.killpg(pid, sig)
        else:
            os.kill(pid, sig)
    except (ProcessLookupError, PermissionError, OSError, OverflowError, ValueError):
        pass


def _is_pid_alive(pid: int) -> bool:
    """Check whether `pid` is still alive, reaping it cleanly if it is our direct child."""
    if hasattr(os, "waitpid") and hasattr(os, "WNOHANG"):
        try:
            wpid, _ = os.waitpid(pid, os.WNOHANG)
            if wpid == pid:
                return False
        except ChildProcessError:
            pass
        except (OSError, OverflowError, ValueError):
            pass
    try:
        os.kill(pid, 0)
        return True
    except ProcessLookupError:
        return False
    except PermissionError:
        return True
    except (OSError, OverflowError, ValueError):
        return False


def reap_pidfile_daemon(
    pid_path: Path,
    *,
    expected_binary: str | Path | None = None,
    expected_cmdline_substr: str | None = None,
    timeout: float = 3.0,
) -> bool:
    """Safely terminate a detached daemon referenced by a JSON `proxy.pid` file.

    Enforces 5 safety layers before signalling:
      1. `pid_path` must open with `O_NOFOLLOW | O_NONBLOCK` and `fstat` as a regular file (<= 64 KiB).
      2. JSON `"pid"` must be a strict int in `2..=0x7FFFFFFF` and neither current PID nor parent PID.
      3. Process must be alive and accessible via `os.kill(pid, 0)`.
      4. Live OS cmdline (`/proc/<pid>/cmdline` or `/bin/ps`) must match expected daemon.
      5. Safe PGID-or-PID `SIGTERM` -> poll -> re-verify cmdline -> `SIGKILL`.
    """
    if os.name == "nt":
        return False
    path = Path(pid_path)
    open_flags = os.O_RDONLY | getattr(os, "O_NOFOLLOW", 0) | getattr(os, "O_NONBLOCK", 0)
    try:
        fd = os.open(path, open_flags)
    except OSError:
        return False
    try:
        st = os.fstat(fd)
        if not stat.S_ISREG(st.st_mode) or st.st_size > 65536:
            return False
        chunks: list[bytes] = []
        total = 0
        while True:
            chunk = os.read(fd, 8192)
            if not chunk:
                break
            total += len(chunk)
            if total > 65536:
                return False
            chunks.append(chunk)
        raw_bytes = b"".join(chunks)
    except OSError:
        return False
    finally:
        try:
            os.close(fd)
        except OSError:
            pass

    try:
        payload = json.loads(raw_bytes.decode("utf-8"))
    except (UnicodeDecodeError, json.JSONDecodeError):
        return False
    if not isinstance(payload, dict):
        return False

    pid = payload.get("pid")
    if not isinstance(pid, int) or isinstance(pid, bool):
        return False
    if pid <= 1 or pid > 0x7FFFFFFF or pid == os.getpid() or pid == os.getppid():
        return False

    try:
        os.kill(pid, 0)
    except (ProcessLookupError, PermissionError, OSError, OverflowError, ValueError):
        return False

    exp_bin = Path(expected_binary) if expected_binary is not None else None
    cmdline = _read_process_cmdline(pid)
    if not cmdline or not _matches_expected_daemon(
        cmdline,
        expected_binary=exp_bin,
        expected_cmdline_substr=expected_cmdline_substr,
    ):
        return False

    _signal_pid_safely(pid, signal.SIGTERM)
    deadline = time.monotonic() + max(0.05, timeout)
    while time.monotonic() < deadline:
        if not _is_pid_alive(pid):
            return True
        time.sleep(0.05)

    # Re-verify identity before SIGKILL escalation to prevent killing a recycled PID.
    recheck_cmdline = _read_process_cmdline(pid)
    if recheck_cmdline and _matches_expected_daemon(
        recheck_cmdline,
        expected_binary=exp_bin,
        expected_cmdline_substr=expected_cmdline_substr,
    ):
        _signal_pid_safely(pid, signal.SIGKILL)
        kill_deadline = time.monotonic() + 1.0
        while time.monotonic() < kill_deadline:
            if not _is_pid_alive(pid):
                break
            time.sleep(0.05)
    return not _is_pid_alive(pid)


def _cleanup_watched_pidfiles() -> None:
    with _WATCHED_PIDFILES_LOCK:
        entries = list(_WATCHED_PIDFILES.values())
        _WATCHED_PIDFILES.clear()
    for pid_path, exp_bin, exp_substr in entries:
        try:
            reap_pidfile_daemon(
                pid_path,
                expected_binary=exp_bin,
                expected_cmdline_substr=exp_substr,
                timeout=1.5,
            )
        except Exception:
            pass


def register_pidfile_cleanup(
    stack: contextlib.ExitStack,
    pid_path: Path,
    *,
    expected_binary: str | Path | None = None,
    expected_cmdline_substr: str | None = None,
    timeout: float = 3.0,
) -> Path:
    """Register a detached-daemon `proxy.pid` reaper on `stack` and `atexit`."""
    global _WATCHED_PIDFILES_NEXT_ID
    # Anchor relative paths without dereferencing symlinks so Layer 1 symlink rejection stays intact.
    resolved = Path(os.path.abspath(pid_path))
    exp_bin = Path(expected_binary) if expected_binary is not None else None
    with _WATCHED_PIDFILES_LOCK:
        _WATCHED_PIDFILES_NEXT_ID += 1
        reg_id = _WATCHED_PIDFILES_NEXT_ID
        _WATCHED_PIDFILES[reg_id] = (resolved, exp_bin, expected_cmdline_substr)

    def _teardown() -> None:
        try:
            reap_pidfile_daemon(
                resolved,
                expected_binary=exp_bin,
                expected_cmdline_substr=expected_cmdline_substr,
                timeout=timeout,
            )
        finally:
            with _WATCHED_PIDFILES_LOCK:
                _WATCHED_PIDFILES.pop(reg_id, None)

    stack.callback(_teardown)
    return resolved


atexit.register(_cleanup_active_companion_pids)
atexit.register(_cleanup_watched_pidfiles)


@dataclasses.dataclass
class TestRuntimeConfig:
    """Runtime configuration shared by E2ETestCase instances during a test run."""

    binary_path: Path | None = None
    repo_root: Path = DEFAULT_REPO_ROOT
    default_timeout: float = 30.0
    verbose: bool = False


_RUNTIME_CONFIG = TestRuntimeConfig()


def get_runtime_config() -> TestRuntimeConfig:
    return _RUNTIME_CONFIG


def set_runtime_config(cfg: TestRuntimeConfig) -> None:
    global _RUNTIME_CONFIG
    _RUNTIME_CONFIG = cfg


def resolve_binary(
    cli_binary: str | Path | None = None,
    repo_root: str | Path | None = None,
    env: dict[str, str] | None = None,
) -> Path:
    """Resolve `agy-companion` binary path.

    Precedence:
      1. `--binary` CLI argument
      2. `AGY_COMPANION_E2E_BIN` environment variable
      3. `<repo-root>/target/debug/agy-companion`
    """
    if cli_binary is not None and str(cli_binary).strip():
        return Path(cli_binary).expanduser().resolve()
    source_env = os.environ if env is None else env
    env_bin = source_env.get("AGY_COMPANION_E2E_BIN", "").strip()
    if env_bin:
        return Path(env_bin).expanduser().resolve()
    root = Path(repo_root).expanduser().resolve() if repo_root is not None else DEFAULT_REPO_ROOT
    return root / "target" / "debug" / "agy-companion"


def find_free_port(host: str = "127.0.0.1") -> int:
    """Allocate an available ephemeral TCP port on loopback."""
    with socket.socket(socket.AF_INET, socket.SOCK_STREAM) as sock:
        sock.bind((host, 0))
        return int(sock.getsockname()[1])


def find_key_recursive(obj: Any, target_key: str) -> tuple[bool, Any]:
    """Recursively locate `target_key` inside nested dicts/lists (Guard A4)."""
    if isinstance(obj, dict):
        if target_key in obj:
            return True, obj[target_key]
        for value in obj.values():
            found, res = find_key_recursive(value, target_key)
            if found:
                return True, res
    elif isinstance(obj, list):
        for item in obj:
            found, res = find_key_recursive(item, target_key)
            if found:
                return True, res
    return False, None


def find_all_values_recursive(obj: Any, target_key: str) -> list[Any]:
    """Collect all values for `target_key` across nested dicts/lists."""
    matches: list[Any] = []
    if isinstance(obj, dict):
        if target_key in obj:
            matches.append(obj[target_key])
        for value in obj.values():
            matches.extend(find_all_values_recursive(value, target_key))
    elif isinstance(obj, list):
        for item in obj:
            matches.extend(find_all_values_recursive(item, target_key))
    return matches


def extract_models_list(data: Any) -> list[dict[str, Any]]:
    """Extract `customModels` array from `agy-companion models --json` output (Guard A6)."""
    if not isinstance(data, dict):
        return []
    if isinstance(data.get("customModelsConfig"), dict):
        models = data["customModelsConfig"].get("customModels")
        if isinstance(models, list):
            return [m for m in models if isinstance(m, dict)]
    if isinstance(data.get("customModels"), list):
        return [m for m in data["customModels"] if isinstance(m, dict)]
    return []


class IsolatedEnv:
    """Isolated temporary workspace + sanitized environment for a single E2E test."""

    def __init__(self, prefix: str = "agy-e2e-") -> None:
        self._tempdir = tempfile.TemporaryDirectory(prefix=prefix)
        self.root = Path(self._tempdir.name).resolve()
        self.home_dir = self.root / "home"
        self.config_root = self.root / "config"
        self.config_dir = self.config_root / "agy-companion"
        self.config_path = self.config_dir / "config.json"
        self.data_root = self.root / "data"
        self.data_dir = self.data_root / "agy-companion"
        self.usage_dir = self.data_dir / "usage"
        self.tmp_dir = self.root / "tmp"
        for d in (self.home_dir, self.config_dir, self.usage_dir, self.tmp_dir):
            d.mkdir(parents=True, exist_ok=True)

    def build_env(
        self,
        extra: dict[str, str | None] | None = None,
        *,
        set_config_env: bool = True,
    ) -> dict[str, str]:
        """Construct a clean environment stripped of ambient secrets and proxy vars."""
        env: dict[str, str] = {}
        for key in SAFE_HOST_ENV_VARS:
            val = os.environ.get(key)
            if val is not None:
                env[key] = val
        # Guarantee all stripped vars are absent even if someone added one above.
        for key in STRIPPED_ENV_VARS:
            env.pop(key, None)

        env["HOME"] = str(self.home_dir)
        env["XDG_CONFIG_HOME"] = str(self.config_root)
        env["XDG_DATA_HOME"] = str(self.data_root)
        env["TMPDIR"] = str(self.tmp_dir)
        if set_config_env:
            env["AGY_COMPANION_CONFIG"] = str(self.config_path)

        if extra:
            for key, val in extra.items():
                if val is None:
                    env.pop(key, None)
                else:
                    env[key] = val
        return env

    def make_config_dict(
        self,
        *,
        listen_host: str = "127.0.0.1",
        listen_port: int = 24141,
        base_url: str = "https://generativelanguage.googleapis.com",
        api_key: str | None = None,
        default_model: str = "gemini-4-hydrogen",
        timeout_secs: int = 900,
        usage_dir: str | Path | None = None,
        include_usage_dir: bool = True,
        schema_version: int = 1,
        cloudcode_upstream_url: str | None = None,
    ) -> dict[str, Any]:
        """Build a schema_version=1 config dict pointing at isolated usage_dir by default."""
        cfg: dict[str, Any] = {
            "schema_version": schema_version,
            "listen_host": listen_host,
            "listen_port": listen_port,
            "gemini": {
                "base_url": base_url,
                "api_key": api_key,
                "default_model": default_model,
                "timeout_secs": timeout_secs,
            },
        }
        if usage_dir is not None:
            cfg["usage_dir"] = str(usage_dir)
        elif include_usage_dir:
            cfg["usage_dir"] = str(self.usage_dir)
        if cloudcode_upstream_url is not None:
            cfg["cloudcode_upstream_url"] = cloudcode_upstream_url
        return cfg

    def write_config(
        self,
        data: dict[str, Any] | str,
        *,
        mode: int = 0o600,
        path: Path | None = None,
        mtime_offset_secs: float | None = None,
    ) -> Path:
        """Atomically write config JSON (or raw text) with explicit Unix permissions & optional mtime offset."""
        target = path if path is not None else self.config_path
        target.parent.mkdir(parents=True, exist_ok=True)
        raw_text = data if isinstance(data, str) else (json.dumps(data, indent=2) + "\n")
        fd, tmp_name = tempfile.mkstemp(prefix=".cfg-", suffix=".tmp", dir=str(target.parent))
        try:
            with os.fdopen(fd, "w", encoding="utf-8") as handle:
                handle.write(raw_text)
            os.chmod(tmp_name, mode)
            os.replace(tmp_name, target)
            os.chmod(target, mode)
        except Exception:
            try:
                os.unlink(tmp_name)
            except (OSError, ValueError):
                pass
            raise
        if mtime_offset_secs is not None:
            ts = time.time() + mtime_offset_secs
            os.utime(target, (ts, ts))
        return target

    @staticmethod
    def bump_mtime(path: Path, delta: float = 2.0) -> float:
        """Strictly advance a file's mtime for deterministic hot-reload tests (Guard A14)."""
        st = os.stat(path)
        new_ts = max(st.st_mtime, time.time()) + delta
        os.utime(path, (new_ts, new_ts))
        return new_ts

    def usage_files(self, usage_dir: Path | None = None) -> list[Path]:
        target_dir = usage_dir if usage_dir is not None else self.usage_dir
        if not target_dir.exists():
            return []
        return sorted(p for p in target_dir.glob("*.jsonl") if p.is_file())

    def read_usage_events(self, usage_dir: Path | None = None) -> list[dict[str, Any]]:
        events: list[dict[str, Any]] = []
        for fpath in self.usage_files(usage_dir=usage_dir):
            text = fpath.read_text(encoding="utf-8")
            for line in text.splitlines():
                stripped = line.strip()
                if stripped:
                    events.append(json.loads(stripped))
        return events

    def close(self) -> None:
        self._tempdir.cleanup()

    def __enter__(self) -> IsolatedEnv:
        return self

    def __exit__(self, exc_type: Any, exc: Any, tb: Any) -> None:
        self.close()


def poll_usage_events(
    usage_dir: Path,
    *,
    min_count: int = 1,
    timeout: float = 2.0,
    interval: float = 0.05,
) -> tuple[list[Path], list[dict[str, Any]]]:
    """Poll `<usage_dir>/*.jsonl` until at least `min_count` valid JSONL events exist (Guard A13)."""
    deadline = time.monotonic() + timeout
    last_files: list[Path] = []
    last_events: list[dict[str, Any]] = []
    while True:
        files = sorted(p for p in usage_dir.glob("*.jsonl") if p.is_file()) if usage_dir.exists() else []
        events: list[dict[str, Any]] = []
        parse_ok = True
        for fpath in files:
            try:
                content = fpath.read_text(encoding="utf-8")
            except OSError:
                parse_ok = False
                break
            for line in content.splitlines():
                stripped = line.strip()
                if not stripped:
                    continue
                try:
                    events.append(json.loads(stripped))
                except json.JSONDecodeError:
                    parse_ok = False
                    break
            if not parse_ok:
                break
        last_files = files
        last_events = events
        if parse_ok and len(events) >= min_count:
            return files, events
        if time.monotonic() >= deadline:
            return last_files, last_events
        time.sleep(interval)


def poll_stats(
    port: int,
    *,
    host: str = "127.0.0.1",
    min_requests: int = 1,
    timeout: float = 2.0,
    interval: float = 0.05,
) -> dict[str, Any]:
    """Poll `GET /api/stats` until `requests >= min_requests` (Guard A13 SSE EOF race guard)."""
    deadline = time.monotonic() + timeout
    last_stats: dict[str, Any] = {}
    while True:
        try:
            resp = http_raw_request(host, port, "GET", "/api/stats", timeout=1.0)
            if resp.status == 200:
                data = resp.json()
                if isinstance(data, dict):
                    last_stats = data
                    reqs = data.get("requests")
                    if isinstance(reqs, int) and not isinstance(reqs, bool) and reqs >= min_requests:
                        return data
        except (OSError, ValueError, json.JSONDecodeError, http.client.HTTPException):
            pass
        if time.monotonic() >= deadline:
            return last_stats
        time.sleep(interval)


@dataclasses.dataclass(frozen=True)
class CliResult:
    """Result of a one-shot CLI subprocess execution."""

    returncode: int
    stdout: str
    stderr: str
    duration_s: float
    cmd: list[str]

    def json_stdout(self) -> Any:
        return json.loads(self.stdout)


def run_cli(
    cmd: list[str],
    *,
    env: dict[str, str] | None = None,
    cwd: str | Path | None = None,
    timeout: float = 15.0,
    stdin_bytes: bytes | None = None,
) -> CliResult:
    """Run a CLI command to completion with timeout and captured stdout/stderr."""
    start = time.monotonic()
    completed = subprocess.run(
        cmd,
        input=stdin_bytes,
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
        env=env,
        cwd=str(cwd) if cwd is not None else None,
        timeout=timeout,
        check=False,
    )
    duration = time.monotonic() - start
    return CliResult(
        returncode=completed.returncode,
        stdout=completed.stdout.decode("utf-8", errors="replace"),
        stderr=completed.stderr.decode("utf-8", errors="replace"),
        duration_s=duration,
        cmd=list(cmd),
    )


@dataclasses.dataclass(frozen=True)
class HttpResponse:
    """Raw HTTP response returned by `http_raw_request`."""

    status: int
    reason: str
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
    def text(self) -> str:
        return self.body.decode("utf-8", errors="replace")

    def json(self) -> Any:
        return json.loads(self.body.decode("utf-8"))


_TRANSIENT_SEND_ERRNOS: frozenset[int] = frozenset(
    getattr(errno, name)
    for name in ("ENOBUFS", "EAGAIN", "EWOULDBLOCK", "EINTR")
    if hasattr(errno, name)
)
_PEER_CLOSE_SEND_ERRNOS: frozenset[int] = frozenset(
    getattr(errno, name)
    for name in ("EPIPE", "ECONNRESET", "ECONNABORTED", "ESHUTDOWN")
    if hasattr(errno, name)
)


def http_raw_request(
    host: str,
    port: int,
    method: str,
    path: str,
    *,
    body: bytes = b"",
    headers: dict[str, str] | None = None,
    timeout: float = 15.0,
    chunk_size: int = 65536,
) -> HttpResponse:
    """Issue a raw HTTP/1.1 request without normalizing `..` paths.

    Gracefully handles early socket close on oversized bodies (e.g. HTTP 413
    Payload Too Large) by catching peer-close errors during body transmission
    while retrying transient ENOBUFS/EAGAIN/EWOULDBLOCK/EINTR socket errors
    with bounded backoff and exact partial-write byte offset tracking.
    """
    conn = http.client.HTTPConnection(host, port, timeout=timeout)
    try:
        conn.connect()
        conn.putrequest(method, path, skip_host=False, skip_accept_encoding=True)
        req_headers = dict(headers) if headers else {}
        lower_keys = {k.lower() for k in req_headers}
        if body and "content-length" not in lower_keys and "transfer-encoding" not in lower_keys:
            req_headers["Content-Length"] = str(len(body))
        elif method in ("POST", "PUT", "PATCH") and "content-length" not in lower_keys and "transfer-encoding" not in lower_keys:
            req_headers["Content-Length"] = "0"
        if "connection" not in lower_keys:
            req_headers["Connection"] = "close"
        for k, v in req_headers.items():
            conn.putheader(k, v)
        conn.endheaders()

        send_aborted = False
        if body:
            sock = conn.sock
            if sock is None:
                raise OSError("HTTP connection socket closed before body transmission")
            view = memoryview(body)
            offset = 0
            total = len(body)
            deadline = time.monotonic() + timeout
            backoff = 0.005
            while offset < total:
                end = min(offset + chunk_size, total)
                try:
                    sent = sock.send(view[offset:end])
                    if sent == 0:
                        send_aborted = True
                        break
                    offset += sent
                    backoff = 0.005
                except TimeoutError:
                    raise
                except (BrokenPipeError, ConnectionResetError):
                    # Server may respond early (e.g. 413 Payload Too Large) and close write side.
                    send_aborted = True
                    break
                except OSError as exc:
                    if exc.errno in _TRANSIENT_SEND_ERRNOS:
                        remaining = deadline - time.monotonic()
                        if remaining <= 0:
                            raise TimeoutError(
                                f"timed out retrying transient send errno={exc.errno} "
                                f"after {offset}/{total} bytes"
                            ) from exc
                        time.sleep(min(backoff, remaining))
                        backoff = min(backoff * 2.0, 0.05)
                        continue
                    if exc.errno in _PEER_CLOSE_SEND_ERRNOS:
                        send_aborted = True
                        break
                    raise

        resp = conn.getresponse()
        if send_aborted:
            chunks: list[bytes] = []
            try:
                while True:
                    part = resp.read1(65536)
                    if not part:
                        break
                    chunks.append(part)
            except http.client.IncompleteRead as exc:
                if exc.partial:
                    chunks.append(exc.partial)
            except (ConnectionResetError, BrokenPipeError):
                pass
            resp_body = b"".join(chunks)
        else:
            resp_body = resp.read()
        raw_hdrs = list(resp.getheaders())
        return HttpResponse(
            status=resp.status,
            reason=resp.reason,
            headers=dict(raw_hdrs),
            raw_headers=raw_hdrs,
            body=resp_body,
        )
    finally:
        try:
            conn.close()
        except OSError:
            pass


# Convenient alias matching explorer_1 naming
http_request = http_raw_request


class StreamResponse:
    """Streaming HTTP response exposing unbuffered read1() chunk arrival timestamps."""

    def __init__(self, resp: http.client.HTTPResponse) -> None:
        self._resp = resp
        self.status = resp.status
        self.reason = resp.reason
        self.raw_headers = list(resp.getheaders())
        self.headers = dict(self.raw_headers)

    def header(self, name: str) -> str | None:
        target = name.lower()
        for k, v in self.raw_headers:
            if k.lower() == target:
                return v
        return None

    def iter_chunks(self, size: int = 4096) -> Iterator[tuple[bytes, float]]:
        while True:
            chunk = self._resp.read1(size)
            if not chunk:
                break
            yield chunk, time.monotonic()


@contextlib.contextmanager
def http_stream_request(
    host: str,
    port: int,
    method: str,
    path: str,
    *,
    body: bytes = b"",
    headers: dict[str, str] | None = None,
    timeout: float = 15.0,
) -> Iterator[StreamResponse]:
    """Open an HTTP/1.1 streaming connection and yield unbuffered `(chunk, monotonic_ts)` pairs."""
    conn = http.client.HTTPConnection(host, port, timeout=timeout)
    try:
        conn.connect()
        conn.putrequest(method, path, skip_host=False, skip_accept_encoding=True)
        req_headers = dict(headers) if headers else {}
        lower_keys = {k.lower() for k in req_headers}
        if body and "content-length" not in lower_keys and "transfer-encoding" not in lower_keys:
            req_headers["Content-Length"] = str(len(body))
        elif method in ("POST", "PUT", "PATCH") and "content-length" not in lower_keys and "transfer-encoding" not in lower_keys:
            req_headers["Content-Length"] = "0"
        if "connection" not in lower_keys:
            req_headers["Connection"] = "close"
        for k, v in req_headers.items():
            conn.putheader(k, v)
        conn.endheaders()
        if body:
            conn.send(body)
        resp = conn.getresponse()
        yield StreamResponse(resp)
    finally:
        try:
            conn.close()
        except OSError:
            pass


class CompanionStartError(RuntimeError):
    """Raised when `CompanionProcess` fails to start or crashes before readiness."""


class CompanionProcess:
    """Managed subprocess for `agy-companion serve` (or a `custom_cmd` for self-tests)."""

    def __init__(
        self,
        binary: str | Path | None = None,
        *,
        env: dict[str, str] | None = None,
        port: int | None = None,
        config_path: str | Path | None = None,
        extra_args: tuple[str, ...] | list[str] = (),
        ready_timeout: float = 10.0,
        ready_path: str = "/health",
        custom_cmd: list[str] | None = None,
        cwd: str | Path | None = None,
        pass_port_flag: bool = True,
        pass_config_flag: bool = True,
    ) -> None:
        self._binary = Path(binary) if binary is not None else None
        self._env = env
        self._explicit_port = port
        self.port: int = port if port is not None else find_free_port()
        self._config_path = Path(config_path) if config_path is not None else None
        self._extra_args = list(extra_args)
        self._ready_timeout = ready_timeout
        self._ready_path = ready_path
        self._custom_cmd = list(custom_cmd) if custom_cmd is not None else None
        self._cwd = str(cwd) if cwd is not None else None
        self._pass_port_flag = pass_port_flag
        self._pass_config_flag = pass_config_flag

        self._proc: subprocess.Popen[bytes] | None = None
        self._lock = threading.Lock()
        self._stdout_lines: list[str] = []
        self._stderr_lines: list[str] = []
        self._stdout_thread: threading.Thread | None = None
        self._stderr_thread: threading.Thread | None = None
        self._stopped = False

    @property
    def host(self) -> str:
        return "127.0.0.1"

    @property
    def base_url(self) -> str:
        return f"http://127.0.0.1:{self.port}"

    @property
    def pid(self) -> int | None:
        return self._proc.pid if self._proc is not None else None

    @property
    def proc(self) -> subprocess.Popen[bytes] | None:
        return self._proc

    @property
    def returncode(self) -> int | None:
        return self._proc.poll() if self._proc is not None else None

    @property
    def stdout_text(self) -> str:
        with self._lock:
            return "".join(self._stdout_lines)

    @property
    def stderr_text(self) -> str:
        with self._lock:
            return "".join(self._stderr_lines)

    def _build_cmd(self) -> list[str]:
        if self._custom_cmd is not None:
            return [arg.replace("{port}", str(self.port)) for arg in self._custom_cmd]
        if self._binary is None:
            raise CompanionStartError("CompanionProcess requires either `binary` or `custom_cmd`")
        cmd = [str(self._binary), "serve"]
        if self._pass_port_flag:
            cmd.extend(["--port", str(self.port)])
        if self._pass_config_flag and self._config_path is not None:
            cmd.extend(["--config", str(self._config_path)])
        cmd.extend(self._extra_args)
        return cmd

    def _spawn(self) -> None:
        cmd = self._build_cmd()
        self._stdout_lines = []
        self._stderr_lines = []
        self._stopped = False
        proc = subprocess.Popen(
            cmd,
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
            env=self._env,
            cwd=self._cwd,
            bufsize=0,
            start_new_session=True,
        )
        self._proc = proc
        with _ACTIVE_COMPANION_LOCK:
            _ACTIVE_COMPANION_PIDS.add(proc.pid)

        def drain(stream: Any, target: list[str]) -> None:
            try:
                while True:
                    line = stream.readline()
                    if not line:
                        break
                    decoded = line.decode("utf-8", errors="replace")
                    with self._lock:
                        target.append(decoded)
            except (OSError, ValueError):
                pass

        self._stdout_thread = threading.Thread(
            target=drain,
            args=(proc.stdout, self._stdout_lines),
            name=f"companion-out-{proc.pid}",
            daemon=True,
        )
        self._stderr_thread = threading.Thread(
            target=drain,
            args=(proc.stderr, self._stderr_lines),
            name=f"companion-err-{proc.pid}",
            daemon=True,
        )
        self._stdout_thread.start()
        self._stderr_thread.start()

    def start(self, wait_for_ready: bool = True) -> CompanionProcess:
        if self._proc is not None:
            return self
        attempts = (
            2
            if self._explicit_port is None and self._custom_cmd is None and self._pass_port_flag
            else 1
        )
        last_exc: Exception | None = None
        for attempt in range(attempts):
            if attempt > 0:
                self.port = find_free_port()
            self._spawn()
            if not wait_for_ready:
                return self
            try:
                self.wait_ready(self._ready_timeout)
                return self
            except CompanionStartError as exc:
                last_exc = exc
                err_lower = str(exc).lower()
                self.stop()
                if attempt + 1 < attempts and ("address already in use" in err_lower or "eaddrinuse" in err_lower):
                    continue
                raise
        assert last_exc is not None
        raise last_exc

    def wait_ready(self, timeout: float = 10.0) -> None:
        deadline = time.monotonic() + timeout
        last_err = ""
        while time.monotonic() < deadline:
            proc = self._proc
            if proc is None:
                raise CompanionStartError("Companion process was not started")
            rc = proc.poll()
            if rc is not None:
                # Let reader threads flush remaining output briefly.
                time.sleep(0.05)
                raise CompanionStartError(
                    f"Companion process exited prematurely with code {rc}\n"
                    f"stdout:\n{self.stdout_text}\nstderr:\n{self.stderr_text}"
                )
            try:
                resp = http_raw_request("127.0.0.1", self.port, "GET", self._ready_path, timeout=0.5)
                if resp.status == 200:
                    return
                last_err = f"HTTP {resp.status}: {resp.text[:200]}"
            except OSError as exc:
                last_err = str(exc)
            time.sleep(0.05)

        raise CompanionStartError(
            f"Companion process did not become ready on 127.0.0.1:{self.port}{self._ready_path} "
            f"within {timeout:.1f}s (last error: {last_err})\n"
            f"stdout:\n{self.stdout_text}\nstderr:\n{self.stderr_text}"
        )

    def stop(self) -> int | None:
        if self._stopped:
            return self.returncode
        self._stopped = True
        proc = self._proc
        if proc is None:
            return None

        if proc.poll() is None:
            try:
                os.killpg(proc.pid, signal.SIGTERM)
            except (ProcessLookupError, PermissionError, OSError):
                try:
                    proc.terminate()
                except OSError:
                    pass
            try:
                proc.wait(timeout=3.0)
            except subprocess.TimeoutExpired:
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

        for thread in (self._stdout_thread, self._stderr_thread):
            if thread is not None and thread.is_alive():
                thread.join(timeout=2.0)
        for pipe in (proc.stdout, proc.stderr):
            if pipe is not None:
                try:
                    pipe.close()
                except OSError:
                    pass

        with _ACTIVE_COMPANION_LOCK:
            _ACTIVE_COMPANION_PIDS.discard(proc.pid)
        return proc.returncode

    def __enter__(self) -> CompanionProcess:
        return self.start()

    def __exit__(self, exc_type: Any, exc: Any, tb: Any) -> None:
        self.stop()


class _ConnectProxyTcpServer(socketserver.ThreadingTCPServer):
    allow_reuse_address = True
    daemon_threads = True
    request_queue_size = 128


class MockConnectProxy:
    """Loopback HTTP CONNECT proxy that records requested targets and returns 502 Bad Gateway."""

    def __init__(self, host: str = "127.0.0.1", port: int = 0) -> None:
        self.host = host
        self._cond = threading.Condition()
        self._close_lock = threading.Lock()
        self._connect_targets: list[str] = []
        self._captured_connects: list[dict[str, Any]] = []
        self._closed = False
        self._server_closed = False

        parent = self

        class _Handler(socketserver.BaseRequestHandler):
            def handle(self) -> None:
                sock = self.request
                sock.settimeout(3.0)
                buf = bytearray()
                try:
                    while b"\r\n\r\n" not in buf and len(buf) < 65536:
                        chunk = sock.recv(4096)
                        if not chunk:
                            break
                        buf.extend(chunk)
                except OSError:
                    return

                head_bytes = bytes(buf).split(b"\r\n\r\n", 1)[0]
                lines = head_bytes.decode("iso-8859-1", errors="replace").split("\r\n")
                if lines and lines[0].strip():
                    parts = lines[0].strip().split()
                    if len(parts) >= 2 and parts[0].upper() == "CONNECT":
                        target = parts[1]
                        version = parts[2] if len(parts) >= 3 else "HTTP/1.1"
                        raw_headers: list[tuple[str, str]] = []
                        headers: dict[str, str] = {}
                        for hline in lines[1:]:
                            if ":" in hline:
                                k, v = hline.split(":", 1)
                                key = k.strip()
                                val = v.strip()
                                raw_headers.append((key, val))
                                headers[key.lower()] = val
                        with parent._cond:
                            parent._connect_targets.append(target)
                            parent._captured_connects.append(
                                {
                                    "method": parts[0].upper(),
                                    "target": target,
                                    "version": version,
                                    "headers": headers,
                                    "raw_headers": raw_headers,
                                }
                            )
                            parent._cond.notify_all()
                try:
                    sock.sendall(
                        b"HTTP/1.1 502 Bad Gateway\r\n"
                        b"Content-Length: 0\r\n"
                        b"Connection: close\r\n"
                        b"\r\n"
                    )
                except OSError:
                    pass

        self._server = _ConnectProxyTcpServer((host, port), _Handler)
        self.port = int(self._server.server_address[1])
        self._thread = threading.Thread(
            target=self._server.serve_forever,
            kwargs={"poll_interval": 0.05},
            daemon=True,
        )
        self._thread.start()

    @property
    def proxy_url(self) -> str:
        return f"http://{self.host}:{self.port}"

    @property
    def connect_targets(self) -> list[str]:
        with self._cond:
            return list(self._connect_targets)

    @property
    def captured_connects(self) -> list[dict[str, Any]]:
        with self._cond:
            return list(self._captured_connects)

    def clear_targets(self) -> None:
        with self._cond:
            self._connect_targets.clear()
            self._captured_connects.clear()

    def wait_for_connects(self, count: int, timeout: float = 5.0) -> list[str]:
        deadline = time.monotonic() + timeout
        with self._cond:
            while not self._closed and len(self._connect_targets) < count:
                remaining = deadline - time.monotonic()
                if remaining <= 0:
                    break
                self._cond.wait(timeout=remaining)
            return list(self._connect_targets)

    def close(self) -> None:
        with self._cond:
            self._closed = True
            self._cond.notify_all()
        with self._close_lock:
            if self._server_closed:
                return
            self._server_closed = True
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

    def __enter__(self) -> MockConnectProxy:
        return self

    def __exit__(self, exc_type: Any, exc: Any, tb: Any) -> None:
        self.close()


class E2ETestCase(unittest.TestCase):
    """Base TestCase for all E2E tiers with deterministic ExitStack resource teardown."""

    TIER: int = 0
    FEATURE: str = "GENERIC"

    def setUp(self) -> None:
        super().setUp()
        self.stack = contextlib.ExitStack()
        self.addCleanup(self.stack.close)
        self.cfg = get_runtime_config()

    @property
    def repo_root(self) -> Path:
        return self.cfg.repo_root

    def require_binary(self) -> Path:
        binary = self.cfg.binary_path
        if binary is None:
            binary = resolve_binary(repo_root=self.cfg.repo_root)
        self.assertTrue(
            binary.is_file() and os.access(binary, os.X_OK),
            f"agy-companion binary is missing or not executable: {binary}",
        )
        return binary

    def make_env(self) -> IsolatedEnv:
        return self.stack.enter_context(IsolatedEnv())

    def register_pidfile_cleanup(
        self,
        pid_path: Path | None = None,
        *,
        env: IsolatedEnv | None = None,
        expected_binary: str | Path | None = None,
        expected_cmdline_substr: str | None = None,
        timeout: float = 3.0,
    ) -> Path:
        """Register detached-daemon `proxy.pid` cleanup on `self.stack` (call AFTER `make_env()`)."""
        if pid_path is None:
            if env is None:
                raise ValueError("register_pidfile_cleanup requires either pid_path or env")
            pid_path = env.data_dir / "proxy.pid"
        if expected_binary is None and expected_cmdline_substr is None:
            expected_binary = self.cfg.binary_path
        return register_pidfile_cleanup(
            self.stack,
            pid_path,
            expected_binary=expected_binary,
            expected_cmdline_substr=expected_cmdline_substr,
            timeout=timeout,
        )

    def start_mock_connect_proxy(
        self,
        *,
        host: str = "127.0.0.1",
        port: int = 0,
    ) -> MockConnectProxy:
        return self.stack.enter_context(MockConnectProxy(host=host, port=port))

    def start_mock_gemini(
        self,
        *,
        host: str = "127.0.0.1",
        port: int = 0,
        expected_api_key: str | None = None,
        default_usage: dict[str, int] | None = None,
    ) -> MockGeminiServer:
        return self.stack.enter_context(
            MockGeminiServer(
                host=host,
                port=port,
                expected_api_key=expected_api_key,
                default_usage=default_usage,
            )
        )

    def start_mock_health(
        self,
        *,
        version: str = "0.1.0",
        protocol_version: int = 1,
        status: str = "ok",
        health_status: int = 200,
        port: int = 0,
        stats_payload: dict[str, Any] | None = None,
    ) -> MockHealthServer:
        return self.stack.enter_context(
            MockHealthServer(
                version=version,
                protocol_version=protocol_version,
                status=status,
                health_status=health_status,
                port=port,
                stats_payload=stats_payload,
            )
        )

    def start_proxy(
        self,
        env_obj: IsolatedEnv,
        *,
        config_path: Path | None = None,
        port: int | None = None,
        extra_env: dict[str, str | None] | None = None,
        extra_args: tuple[str, ...] = (),
        pass_port_flag: bool = True,
        pass_config_flag: bool = True,
        set_config_env: bool = True,
    ) -> CompanionProcess:
        binary = self.require_binary()
        proc_env = env_obj.build_env(extra=extra_env, set_config_env=set_config_env)
        cfg_path = config_path if config_path is not None else env_obj.config_path
        proc = CompanionProcess(
            binary=binary,
            env=proc_env,
            port=port,
            config_path=cfg_path,
            extra_args=extra_args,
            ready_timeout=min(10.0, self.cfg.default_timeout),
            pass_port_flag=pass_port_flag,
            pass_config_flag=pass_config_flag,
        )
        return self.stack.enter_context(proc)

    def start_mcp(
        self,
        env_obj: IsolatedEnv,
        *,
        config_path: Path | None = None,
        data_dir: Path | None = None,
        extra_env: dict[str, str | None] | None = None,
        pass_config_flag: bool = True,
        pass_data_dir_flag: bool = True,
        set_config_env: bool = True,
        auto_initialize: bool = True,
    ) -> McpStdioClient:
        binary = self.require_binary()
        cmd = [str(binary), "ui-mcp"]
        if pass_config_flag:
            cfg = config_path if config_path is not None else env_obj.config_path
            cmd.extend(["--config", str(cfg)])
        if pass_data_dir_flag:
            ddir = data_dir if data_dir is not None else env_obj.data_dir
            cmd.extend(["--data-dir", str(ddir)])
        proc_env = env_obj.build_env(extra=extra_env, set_config_env=set_config_env)
        client = self.stack.enter_context(McpStdioClient(cmd, env=proc_env))
        if auto_initialize:
            client.initialize(timeout=min(10.0, self.cfg.default_timeout))
        return client
