"""Core opaque-box E2E harness primitives (pure Python 3 stdlib).

Provides:
- TestRuntimeConfig & global runtime config accessors
- E2ETestCase(unittest.TestCase) base class with deterministic ExitStack cleanup
- IsolatedEnv (sanitized env, 0600 atomic config writes, mtime offset/bump helpers)
- CompanionProcess (`agy-companion serve` + `custom_cmd` for self-tests, process-group cleanup)
- run_cli (one-shot CLI execution with captured stdout/stderr/duration)
- http_raw_request (unnormalized raw paths + 413 early socket close resilience)
- http_stream_request (unbuffered read1() incremental SSE arrival measurement)
- poll_usage_events (Guard A13 async SQLite polling)
- usage_storage_files / read_usage_raw_bytes / open_usage_db_readonly
- find_key_recursive / extract_models_list / resolve_binary / find_free_port
"""

from __future__ import annotations

import atexit
import contextlib
import csv
import dataclasses
import errno
import http.client
import io
import json
import os
import shutil
import signal
import socket
import socketserver
import sqlite3
import stat
import subprocess
import tempfile
import threading
import time
import unittest
from pathlib import Path
from typing import Any, Iterator

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
    "SystemRoot",
    "SYSTEMROOT",
    "SystemDrive",
    "SYSTEMDRIVE",
    "COMSPEC",
    "ComSpec",
    "PATHEXT",
    "WINDIR",
    "ProgramData",
    "PSModulePath",
    "OS",
    "NUMBER_OF_PROCESSORS",
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
    "AGY_COMPANION_CODEX_OAUTH_ISSUER",
    "AGY_COMPANION_CODEX_CALLBACK_PORT",
    "AGY_COMPANION_BIN",
    "AGY_COMPANION_E2E_BIN",
    "AGY_COMPANION_HOOK_ACTIVE",
    "AGY_COMPANION_ALLOW_REMOTE_SSH",
    "AGY_COMPANION_NO_UPDATE_CHECK",
    "AGY_COMPANION_INSTALL_CHANNEL",
    "JETSKI_MCP_ENV_RESTORE_PREFIX",
    "JETSKI_MCP_ENV_RESTORE_VARS",
    "ANTIGRAVITY_EXECUTABLE_DATA_DIR",
    "ANTIGRAVITY_RESOLVING_ENVIRONMENT",
    "ANTIGRAVITY_LS_ADDRESS",
    "ANTIGRAVITY_CSRF_TOKEN",
    "ANTIGRAVITY_SIDECAR_WEB_PORT",
    "ANTIGRAVITY_SIDECAR_UI_TOKEN",
    "SSH_CONNECTION",
    "SSH_CLIENT",
    "ZDOTDIR",
    "SHELL",
    "RUST_LOG",
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
        except (OSError, AttributeError):
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


def _windows_process_image_name(pid: int) -> str | None:
    """Return the Windows image name (e.g. `agy-companion.exe`) for `pid`, or None if gone."""
    try:
        completed = subprocess.run(
            ["tasklist", "/FI", f"PID eq {pid}", "/FO", "CSV", "/NH"],
            stdout=subprocess.PIPE,
            stderr=subprocess.DEVNULL,
            timeout=5.0,
            check=False,
        )
    except (OSError, subprocess.SubprocessError):
        return None
    if completed.returncode != 0:
        return None
    text = completed.stdout.decode("utf-8", errors="replace")
    for row in csv.reader(io.StringIO(text)):
        if len(row) >= 2 and row[1].strip() == str(pid):
            image = row[0].strip()
            return image or None
    return None


def _windows_matches_expected_daemon(
    image_name: str,
    *,
    expected_binary: Path | None,
    expected_cmdline_substr: str | None,
) -> bool:
    """Verify Windows image name matches the expected companion binary."""
    img = image_name.strip().lower()
    if not img:
        return False
    if expected_cmdline_substr is not None:
        tok = expected_cmdline_substr.strip().lower()
        return bool(tok) and tok in img
    if expected_binary is not None:
        exp = expected_binary.name.lower()
        exp_exe = exp if exp.endswith(".exe") else f"{exp}.exe"
        return img in (exp, exp_exe)
    return "agy-companion" in img


def _windows_terminate_pid(pid: int) -> bool:
    """Terminate a verified Windows PID via `taskkill /F /PID`, falling back to `os.kill`."""
    try:
        completed = subprocess.run(
            ["taskkill", "/F", "/PID", str(pid)],
            stdout=subprocess.DEVNULL,
            stderr=subprocess.DEVNULL,
            timeout=5.0,
            check=False,
        )
        if completed.returncode == 0:
            return True
    except (OSError, subprocess.SubprocessError):
        pass
    try:
        os.kill(pid, signal.SIGTERM)
        return True
    except (OSError, ValueError):
        return False


def _list_windows_companion_processes() -> list[tuple[int, str]]:
    """Return `(pid, cmdline)` pairs for running Windows `agy-companion*` processes."""
    if os.name != "nt":
        return []
    ps_cmd = (
        "Get-CimInstance Win32_Process -Filter \"Name LIKE 'agy-companion%'\" | "
        "ForEach-Object { \"$($_.ProcessId)`t$($_.CommandLine)\" }"
    )
    try:
        completed = subprocess.run(
            ["powershell", "-NoProfile", "-NonInteractive", "-Command", ps_cmd],
            stdout=subprocess.PIPE,
            stderr=subprocess.DEVNULL,
            timeout=5.0,
            check=False,
        )
    except (OSError, subprocess.SubprocessError):
        return []
    if completed.returncode != 0:
        return []
    results: list[tuple[int, str]] = []
    for line in completed.stdout.decode("utf-8", errors="replace").splitlines():
        if "\t" not in line:
            continue
        pid_str, cmdline = line.split("\t", 1)
        pid_str = pid_str.strip()
        cmdline = cmdline.strip()
        if pid_str.isdigit() and cmdline:
            results.append((int(pid_str), cmdline))
    return results


def reap_pidfile_daemon(
    pid_path: Path,
    *,
    expected_binary: str | Path | None = None,
    expected_cmdline_substr: str | None = None,
    timeout: float = 3.0,
) -> bool:
    """Safely terminate a detached daemon referenced by a JSON `proxy.pid` file.

    Enforces 5 safety layers before signalling:
      1. `pid_path` must open with `O_NOFOLLOW | O_NONBLOCK` (symlink-checked on Windows) and `fstat` as a regular file (<= 64 KiB).
      2. JSON `"pid"` must be a strict int in `2..=0x7FFFFFFF` and neither current PID nor parent PID.
      3. Process must be alive and accessible.
      4. Live OS identity (`/proc/<pid>/cmdline` or `/bin/ps` on POSIX; `tasklist` image name on Windows) must match expected daemon.
      5. Safe PGID-or-PID `SIGTERM` -> poll -> re-verify cmdline -> `SIGKILL` (POSIX) or `taskkill /F /PID` + stale `proxy.pid` cleanup (Windows).
    """
    path = Path(pid_path)
    if os.name == "nt" and path.is_symlink():
        return False
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

    exp_bin = Path(expected_binary) if expected_binary is not None else None
    if os.name == "nt":
        image_name = _windows_process_image_name(pid)
        if not image_name:
            with contextlib.suppress(OSError):
                path.unlink()
            return False
        if not _windows_matches_expected_daemon(
            image_name,
            expected_binary=exp_bin,
            expected_cmdline_substr=expected_cmdline_substr,
        ):
            return False
        _windows_terminate_pid(pid)
        deadline = time.monotonic() + max(0.05, timeout)
        while time.monotonic() < deadline:
            if _windows_process_image_name(pid) is None:
                with contextlib.suppress(OSError):
                    path.unlink()
                return True
            time.sleep(0.05)
        gone = _windows_process_image_name(pid) is None
        if gone:
            with contextlib.suppress(OSError):
                path.unlink()
        return gone

    try:
        os.kill(pid, 0)
    except (ProcessLookupError, PermissionError, OSError, OverflowError, ValueError):
        return False

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
        time.sleep(0.015)

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
            time.sleep(0.015)
    return not _is_pid_alive(pid)


def _list_posix_processes() -> list[tuple[int, str]]:
    """Return `(pid, cmdline)` pairs for visible POSIX processes without raising."""
    if os.name == "nt":
        return []
    proc_root = Path("/proc")
    if proc_root.is_dir():
        results: list[tuple[int, str]] = []
        try:
            entries = list(proc_root.iterdir())
        except OSError:
            entries = []
        for entry in entries:
            if not entry.name.isdigit():
                continue
            pid = int(entry.name)
            try:
                raw = (entry / "cmdline").read_bytes()
            except OSError:
                continue
            text = raw.replace(b"\x00", b" ").decode("utf-8", errors="replace").strip()
            if text:
                results.append((pid, text))
        if results:
            return results
    try:
        completed = subprocess.run(
            ["/bin/ps", "-axo", "pid=,command="],
            stdout=subprocess.PIPE,
            stderr=subprocess.DEVNULL,
            timeout=2.0,
            check=False,
        )
    except (OSError, subprocess.SubprocessError):
        return []
    if completed.returncode != 0:
        return []
    results = []
    for line in completed.stdout.decode("utf-8", errors="replace").splitlines():
        stripped = line.strip()
        if not stripped:
            continue
        parts = stripped.split(None, 1)
        if len(parts) != 2 or not parts[0].isdigit():
            continue
        results.append((int(parts[0]), parts[1].strip()))
    return results


def reap_env_daemons(
    env_root: Path,
    *,
    expected_binary: str | Path | None = None,
    expected_cmdline_substr: str | None = None,
    timeout: float = 3.0,
) -> int:
    """Safely terminate detached daemons whose command line or `proxy.pid` belongs to `env_root`.

    Catches detached replacement/bootstrap daemons (e.g., `serve --adopt-running` spawning
    `serve --replace-stale --config <env_root>/...` or detached `serve --config <env_root>/...`)
    even if `proxy.pid` was overwritten or not yet written.
    """
    raw_root = str(env_root)
    real_root = os.path.realpath(raw_root)
    root_tokens = tuple(dict.fromkeys(tok for tok in (raw_root, real_root) if len(tok) >= 8))
    if not root_tokens:
        return 0

    self_pid = os.getpid()
    parent_pid = os.getppid()
    exp_bin = Path(expected_binary) if expected_binary is not None else None
    if os.name == "nt":
        reaped = 0
        try:
            pid_files = list(Path(env_root).rglob("proxy.pid"))
        except OSError:
            pid_files = []
        for pid_path in pid_files:
            if not pid_path.is_symlink() and reap_pidfile_daemon(
                pid_path,
                expected_binary=exp_bin,
                expected_cmdline_substr=expected_cmdline_substr,
                timeout=timeout,
            ):
                reaped += 1
        lower_tokens = tuple(tok.lower() for tok in root_tokens)
        matched_pids: list[int] = []
        for pid, cmdline in _list_windows_companion_processes():
            if pid <= 1 or pid > 0x7FFFFFFF or pid == self_pid or pid == parent_pid:
                continue
            cmd_lower = cmdline.lower()
            if not any(tok in cmd_lower for tok in lower_tokens):
                continue
            if not _matches_expected_daemon(
                cmdline,
                expected_binary=exp_bin,
                expected_cmdline_substr=expected_cmdline_substr,
            ):
                continue
            matched_pids.append(pid)
        for pid in matched_pids:
            if _windows_terminate_pid(pid):
                reaped += 1
        return reaped

    matched_pids: list[int] = []
    for pid, cmdline in _list_posix_processes():
        if pid <= 1 or pid > 0x7FFFFFFF or pid == self_pid or pid == parent_pid:
            continue
        if not any(tok in cmdline for tok in root_tokens):
            continue
        if not _matches_expected_daemon(
            cmdline,
            expected_binary=exp_bin,
            expected_cmdline_substr=expected_cmdline_substr,
        ):
            continue
        matched_pids.append(pid)

    if not matched_pids:
        return 0

    for pid in matched_pids:
        _signal_pid_safely(pid, signal.SIGTERM)

    deadline = time.monotonic() + max(0.05, timeout)
    remaining = list(matched_pids)
    while remaining and time.monotonic() < deadline:
        remaining = [pid for pid in remaining if _is_pid_alive(pid)]
        if remaining:
            time.sleep(0.015)

    for pid in remaining:
        recheck_cmdline = _read_process_cmdline(pid)
        if (
            recheck_cmdline
            and any(tok in recheck_cmdline for tok in root_tokens)
            and _matches_expected_daemon(
                recheck_cmdline,
                expected_binary=exp_bin,
                expected_cmdline_substr=expected_cmdline_substr,
            )
        ):
            _signal_pid_safely(pid, signal.SIGKILL)

    if remaining:
        kill_deadline = time.monotonic() + 1.0
        while remaining and time.monotonic() < kill_deadline:
            remaining = [pid for pid in remaining if _is_pid_alive(pid)]
            if remaining:
                time.sleep(0.015)

    return sum(1 for pid in matched_pids if not _is_pid_alive(pid))


def reap_port_daemons(
    port: int,
    *,
    expected_binary: str | Path | None = None,
    timeout: float = 3.0,
) -> int:
    """Safely terminate detached `agy-companion serve` daemons started with `--port <port>`."""
    port_str = str(int(port))
    self_pid = os.getpid()
    parent_pid = os.getppid()
    exp_bin = Path(expected_binary) if expected_binary is not None else None

    if os.name == "nt":
        reaped = 0
        for pid, cmdline in _list_windows_companion_processes():
            if pid <= 1 or pid > 0x7FFFFFFF or pid == self_pid or pid == parent_pid:
                continue
            cmd_lower = cmdline.lower()
            if "--port" not in cmd_lower or port_str not in cmd_lower:
                continue
            if not _matches_expected_daemon(
                cmdline,
                expected_binary=exp_bin,
                expected_cmdline_substr=None,
            ):
                continue
            if _windows_terminate_pid(pid):
                reaped += 1
        return reaped

    matched_pids: list[int] = []
    for pid, cmdline in _list_posix_processes():
        if pid <= 1 or pid > 0x7FFFFFFF or pid == self_pid or pid == parent_pid:
            continue
        if "--port" not in cmdline or port_str not in cmdline:
            continue
        if not _matches_expected_daemon(
            cmdline,
            expected_binary=exp_bin,
            expected_cmdline_substr=None,
        ):
            continue
        matched_pids.append(pid)

    if not matched_pids:
        return 0
    for pid in matched_pids:
        _signal_pid_safely(pid, signal.SIGTERM)
    deadline = time.monotonic() + max(0.05, timeout)
    remaining = list(matched_pids)
    while remaining and time.monotonic() < deadline:
        remaining = [pid for pid in remaining if _is_pid_alive(pid)]
        if remaining:
            time.sleep(0.015)
    for pid in remaining:
        recheck_cmdline = _read_process_cmdline(pid)
        if (
            recheck_cmdline
            and "--port" in recheck_cmdline
            and port_str in recheck_cmdline
            and _matches_expected_daemon(
                recheck_cmdline,
                expected_binary=exp_bin,
                expected_cmdline_substr=None,
            )
        ):
            _signal_pid_safely(pid, signal.SIGKILL)
    return sum(1 for pid in matched_pids if not _is_pid_alive(pid))


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


RESERVED_PORT_RANGE = range(24140, 24161)


def find_free_port(host: str = "127.0.0.1") -> int:
    """Allocate an available ephemeral TCP port on loopback, excluding RESERVED_PORT_RANGE."""
    held: list[socket.socket] = []
    try:
        for _ in range(64):
            sock = socket.socket(socket.AF_INET, socket.SOCK_STREAM)
            sock.bind((host, 0))
            port = int(sock.getsockname()[1])
            if port not in RESERVED_PORT_RANGE:
                sock.close()
                return port
            held.append(sock)
        raise RuntimeError(
            f"Unable to allocate ephemeral port outside {RESERVED_PORT_RANGE.start}..{RESERVED_PORT_RANGE.stop - 1}"
        )
    finally:
        for sock in held:
            try:
                sock.close()
            except OSError:
                pass


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
        self.auth_dir = self.config_dir / "auth"
        self.gemini_auth_path = self.auth_dir / "gemini.json"
        self.data_root = self.root / "data"
        self.data_dir = self.data_root / "agy-companion"
        self.usage_dir = self.data_dir / "usage"
        self.cache_root = self.root / "cache"
        self.tmp_dir = self.root / "tmp"
        self._spawned_sidecar = False
        for d in (self.home_dir, self.config_dir, self.usage_dir, self.cache_root, self.tmp_dir):
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
        env["XDG_CACHE_HOME"] = str(self.cache_root)
        env["TMPDIR"] = str(self.tmp_dir)
        env["TEMP"] = str(self.tmp_dir)
        env["TMP"] = str(self.tmp_dir)
        env["APPDATA"] = str(self.config_root)
        env["LOCALAPPDATA"] = str(self.data_root)
        env["USERPROFILE"] = str(self.home_dir)
        env["PYTHONDONTWRITEBYTECODE"] = "1"
        env["AGY_COMPANION_NO_UPDATE_CHECK"] = "1"
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
        listen_port: int | None = None,
        base_url: str = "https://generativelanguage.googleapis.com",
        default_model: str = "gemini-4-hydrogen",
        enabled_models: list[str] | None = None,
        timeout_secs: int = 900,
        usage_dir: str | Path | None = None,
        include_usage_dir: bool = True,
        schema_version: int = 1,
        cloudcode_upstream_url: str | None = None,
        launcher_auto_start: bool | None = None,
        codex_base_url: str | None = None,
        codex_enabled_models: list[str] | None = None,
    ) -> dict[str, Any]:
        """Build a nested schema_version=1 non-secret config dict pointing at isolated usage_dir by default."""
        gemini_cfg: dict[str, Any] = {
            "base_url": base_url,
            "default_model": default_model,
            "timeout_secs": timeout_secs,
        }
        if enabled_models is not None:
            gemini_cfg["enabled_models"] = list(enabled_models)
        providers_cfg: dict[str, Any] = {"gemini": gemini_cfg}
        if codex_base_url is not None or codex_enabled_models is not None:
            codex_cfg: dict[str, Any] = {}
            if codex_base_url is not None:
                codex_cfg["base_url"] = codex_base_url
            if codex_enabled_models is not None:
                codex_cfg["enabled_models"] = list(codex_enabled_models)
            providers_cfg["codex"] = codex_cfg
        resolved_port = listen_port if listen_port is not None else find_free_port()
        cfg: dict[str, Any] = {
            "schema_version": schema_version,
            "proxy": {
                "host": listen_host,
                "port": resolved_port,
            },
            "cloudcode": {
                "upstream": cloudcode_upstream_url if cloudcode_upstream_url is not None else "auto",
            },
            "providers": providers_cfg,
        }
        if launcher_auto_start is not None:
            cfg["launcher_auto_start"] = launcher_auto_start
        if usage_dir is not None:
            cfg["usage_dir"] = str(usage_dir)
        elif include_usage_dir:
            cfg["usage_dir"] = str(self.usage_dir)
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
    def auth_dir_for_config(config_path: Path) -> Path:
        """Derive the sibling auth/ directory for a given config.json path."""
        return config_path.parent / "auth"

    def gemini_auth_path_for(self, config_path: Path | None = None) -> Path:
        """Return sibling auth/gemini.json path for config_path (or default self.config_path)."""
        target_cfg = config_path if config_path is not None else self.config_path
        return self.auth_dir_for_config(target_cfg) / "gemini.json"

    def codex_auth_path_for(self, config_path: Path | None = None) -> Path:
        """Return sibling auth/codex.json path for config_path (or default self.config_path)."""
        target_cfg = config_path if config_path is not None else self.config_path
        return self.auth_dir_for_config(target_cfg) / "codex.json"

    def write_gemini_auth(
        self,
        api_key: str,
        *,
        config_path: Path | None = None,
        mode: int = 0o600,
        dir_mode: int = 0o700,
        schema_version: int = 1,
        mtime_offset_secs: float | None = None,
    ) -> Path:
        """Atomically write sibling auth/gemini.json with 0700 dir / 0600 file permissions on POSIX."""
        target = self.gemini_auth_path_for(config_path)
        target.parent.mkdir(parents=True, exist_ok=True)
        if os.name != "nt":
            os.chmod(target.parent, dir_mode)
        raw_text = json.dumps({"schema_version": schema_version, "api_key": api_key}, indent=2) + "\n"
        fd, tmp_name = tempfile.mkstemp(prefix=".auth-", suffix=".tmp", dir=str(target.parent))
        try:
            with os.fdopen(fd, "w", encoding="utf-8") as handle:
                handle.write(raw_text)
            if os.name != "nt":
                os.chmod(tmp_name, mode)
            os.replace(tmp_name, target)
            if os.name != "nt":
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
        return usage_files(target_dir)

    def read_usage_events(self, usage_dir: Path | None = None) -> list[dict[str, Any]]:
        target_dir = usage_dir if usage_dir is not None else self.usage_dir
        events: list[dict[str, Any]] = []
        for db_path in usage_files(target_dir):
            events.extend(_read_usage_events_from_db(db_path))
        return events

    def close(self) -> None:
        if os.name == "nt":
            pid_file = self.data_dir / "proxy.pid"
            has_pid = pid_file.exists() and not pid_file.is_symlink()
            has_log = (self.data_dir / "proxy.log").exists()
            if has_pid:
                reap_pidfile_daemon(pid_file, timeout=2.0)
            if (
                self._spawned_sidecar
                or has_pid
                or has_log
                or any(self.root.rglob("proxy.pid"))
                or any(self.root.rglob("proxy.log"))
            ):
                reap_env_daemons(self.root, timeout=2.0)
            for attempt in range(10):
                try:
                    self._tempdir.cleanup()
                    return
                except PermissionError:
                    if attempt == 9:
                        reap_env_daemons(self.root, timeout=1.0)
                        shutil.rmtree(self._tempdir.name, ignore_errors=True)
                        return
                    reap_env_daemons(self.root, timeout=1.0)
                    time.sleep(0.05)
            return

        pid_file = self.data_dir / "proxy.pid"
        log_file = self.data_dir / "proxy.log"
        has_pid = pid_file.exists() or pid_file.is_symlink()
        has_log = log_file.exists() or log_file.is_symlink()
        if has_pid:
            reap_pidfile_daemon(pid_file, timeout=2.0)
        if self._spawned_sidecar or has_pid or has_log:
            reap_env_daemons(self.root, timeout=2.0)

        for attempt in range(10):
            try:
                self._tempdir.cleanup()
                return
            except OSError as exc:
                if exc.errno not in (errno.ENOTEMPTY, errno.EBUSY) or attempt == 9:
                    raise
                reap_pidfile_daemon(pid_file, timeout=1.0)
                reap_env_daemons(self.root, timeout=1.0)
                time.sleep(0.05)

    def __enter__(self) -> IsolatedEnv:
        return self

    def __exit__(self, exc_type: Any, exc: Any, tb: Any) -> None:
        self.close()


USAGE_DB_FILENAME = "usage.db"
USAGE_SQLITE_FILENAMES: tuple[str, ...] = (
    "usage.db",
    "usage.db-wal",
    "usage.db-shm",
    "usage.db-journal",
)
_USAGE_EVENT_COLUMNS: tuple[str, ...] = (
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
)


def usage_files(usage_dir: Path) -> list[Path]:
    """Return `[<usage_dir>/usage.db]` when present as a regular non-symlink file."""
    if not usage_dir.exists():
        return []
    db_path = usage_dir / USAGE_DB_FILENAME
    if db_path.is_file() and not db_path.is_symlink():
        return [db_path]
    return []


def usage_storage_files(usage_dir: Path) -> list[Path]:
    """Return all existing regular SQLite files (`usage.db`, `-wal`, `-shm`, `-journal`)."""
    if not usage_dir.exists():
        return []
    found: list[Path] = []
    for name in USAGE_SQLITE_FILENAMES:
        candidate = usage_dir / name
        if candidate.is_file() and not candidate.is_symlink():
            found.append(candidate)
    return found


def read_usage_raw_bytes(usage_dir: Path) -> bytes:
    """Concatenate raw bytes across `usage.db` and any existing SQLite sidecars."""
    chunks: list[bytes] = []
    for path in usage_storage_files(usage_dir):
        try:
            chunks.append(path.read_bytes())
        except FileNotFoundError:
            continue
    return b"".join(chunks)


def open_usage_db_readonly(db_path: Path, *, timeout: float = 2.0) -> sqlite3.Connection:
    """Open a SQLite usage database strictly read-only via RFC 3986 file URI."""
    uri = f"{db_path.resolve().as_uri()}?mode=ro"
    return sqlite3.connect(uri, uri=True, timeout=timeout)


def _read_usage_events_from_db(db_path: Path) -> list[dict[str, Any]]:
    with contextlib.closing(open_usage_db_readonly(db_path)) as conn:
        cur = conn.execute(
            "SELECT timestamp_utc, request_id, provider, model, "
            "input_tokens, output_tokens, cached_tokens, reasoning_tokens, "
            "latency_ms, source "
            "FROM usage_events ORDER BY id ASC"
        )
        rows = cur.fetchall()
    return [dict(zip(_USAGE_EVENT_COLUMNS, row)) for row in rows]


def poll_usage_events(
    usage_dir: Path,
    *,
    min_count: int = 1,
    timeout: float = 2.0,
    interval: float = 0.02,
) -> tuple[list[Path], list[dict[str, Any]]]:
    """Poll `<usage_dir>/usage.db` until at least `min_count` valid usage rows exist (Guard A13)."""
    deadline = time.monotonic() + timeout
    last_files: list[Path] = []
    last_events: list[dict[str, Any]] = []
    while True:
        files = usage_files(usage_dir)
        events: list[dict[str, Any]] = []
        parse_ok = True
        for db_path in files:
            try:
                events.extend(_read_usage_events_from_db(db_path))
            except (sqlite3.OperationalError, sqlite3.DatabaseError, OSError):
                parse_ok = False
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
    interval: float = 0.02,
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
                    usage = data.get("usage")
                    reqs = usage.get("requests") if isinstance(usage, dict) else None
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
    env: dict[str, str],
    cwd: str | Path | None = None,
    timeout: float = 15.0,
    stdin_bytes: bytes | None = None,
) -> CliResult:
    """Run a CLI command to completion with timeout and captured stdout/stderr."""
    if env is None:
        raise ValueError("run_cli requires an explicit isolated env= dict (ISO-10)")
    start = time.monotonic()
    cwd_str = str(cwd) if cwd is not None else None
    if os.name == "nt":
        # On Windows, Rust's Command::spawn() passes bInheritHandles=TRUE, so a
        # detached grandchild daemon (e.g., spawned by `install` or `hook-env`)
        # inherits stdout/stderr pipe handles and blocks subprocess.run()'s
        # communicate() forever. Capturing to TemporaryFile lets proc.wait()
        # return as soon as the direct child exits without blocking on pipe EOF.
        in_file = tempfile.TemporaryFile() if stdin_bytes is not None else None
        out_file = tempfile.TemporaryFile()
        err_file = tempfile.TemporaryFile()
        try:
            if in_file is not None and stdin_bytes is not None:
                in_file.write(stdin_bytes)
                in_file.seek(0)
            proc = subprocess.Popen(
                cmd,
                stdin=in_file if in_file is not None else subprocess.DEVNULL,
                stdout=out_file,
                stderr=err_file,
                env=env,
                cwd=cwd_str,
            )
            try:
                rc = proc.wait(timeout=timeout)
            except subprocess.TimeoutExpired:
                proc.kill()
                proc.wait(timeout=5.0)
                raise
            out_file.seek(0)
            err_file.seek(0)
            out_bytes = out_file.read()
            err_bytes = err_file.read()
        finally:
            if in_file is not None:
                in_file.close()
            out_file.close()
            err_file.close()
        duration = time.monotonic() - start
        return CliResult(
            returncode=rc,
            stdout=out_bytes.decode("utf-8", errors="replace"),
            stderr=err_bytes.decode("utf-8", errors="replace"),
            duration_s=duration,
            cmd=list(cmd),
        )

    completed = subprocess.run(
        cmd,
        input=stdin_bytes,
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
        env=env,
        cwd=cwd_str,
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
        self._stdout_file: Any | None = None
        self._stderr_file: Any | None = None
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
            if self._stdout_file is not None:
                try:
                    self._stdout_file.flush()
                    self._stdout_file.seek(0)
                    return self._stdout_file.read().decode("utf-8", errors="replace")
                except (OSError, ValueError):
                    pass
            return "".join(self._stdout_lines)

    @property
    def stderr_text(self) -> str:
        with self._lock:
            if self._stderr_file is not None:
                try:
                    self._stderr_file.flush()
                    self._stderr_file.seek(0)
                    return self._stderr_file.read().decode("utf-8", errors="replace")
                except (OSError, ValueError):
                    pass
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
        if os.name == "nt":
            # Avoid Windows CRT pipe-close deadlocks when detached grandchild daemons
            # inherit stdout/stderr handles (bInheritHandles=TRUE).
            self._stdout_file = tempfile.TemporaryFile()
            self._stderr_file = tempfile.TemporaryFile()
            proc = subprocess.Popen(
                cmd,
                stdout=self._stdout_file,
                stderr=self._stderr_file,
                env=self._env,
                cwd=self._cwd,
            )
            self._proc = proc
            with _ACTIVE_COMPANION_LOCK:
                _ACTIVE_COMPANION_PIDS.add(proc.pid)
            return

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
            except BaseException:
                self.stop()
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
                time.sleep(0.02)
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
            time.sleep(0.015)

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
            except (ProcessLookupError, PermissionError, OSError, AttributeError):
                try:
                    proc.terminate()
                except OSError:
                    pass
            try:
                proc.wait(timeout=3.0)
            except subprocess.TimeoutExpired:
                try:
                    os.killpg(proc.pid, signal.SIGKILL)
                except (ProcessLookupError, PermissionError, OSError, AttributeError):
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
        with self._lock:
            if self._stdout_file is not None:
                with contextlib.suppress(OSError, ValueError):
                    self._stdout_file.flush()
                    self._stdout_file.seek(0)
                    self._stdout_lines = [self._stdout_file.read().decode("utf-8", errors="replace")]
                with contextlib.suppress(OSError):
                    self._stdout_file.close()
                self._stdout_file = None
            if self._stderr_file is not None:
                with contextlib.suppress(OSError, ValueError):
                    self._stderr_file.flush()
                    self._stderr_file.seek(0)
                    self._stderr_lines = [self._stderr_file.read().decode("utf-8", errors="replace")]
                with contextlib.suppress(OSError):
                    self._stderr_file.close()
                self._stderr_file = None

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
            kwargs={"poll_interval": 0.01},
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
    """Base TestCase for all E2E suites with deterministic ExitStack resource teardown."""

    SUITE: str = ""
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

    def start_sidecar_ui(
        self,
        env_obj: IsolatedEnv,
        *,
        config_path: Path | None = None,
        extra_env: dict[str, str | None] | None = None,
        extra_args: tuple[str, ...] | list[str] = (),
        pass_config_flag: bool = True,
        set_config_env: bool = True,
        token: str = "e2e-sidecar-token",
    ) -> SidecarUiClient:
        """Start `agy-companion serve --adopt-running` with the Sidecar Web UI enabled."""
        binary = self.require_binary()
        env_obj._spawned_sidecar = True
        sidecar_port = find_free_port()
        cmd = [str(binary), "serve", "--adopt-running"]
        if pass_config_flag:
            cfg = config_path if config_path is not None else env_obj.config_path
            cmd.extend(["--config", str(cfg)])
        cmd.extend(extra_args)
        merged_extra: dict[str, str | None] = {
            "ANTIGRAVITY_SIDECAR_WEB_PORT": str(sidecar_port),
            "ANTIGRAVITY_SIDECAR_UI_TOKEN": token,
        }
        if extra_env:
            merged_extra.update(extra_env)
        proc_env = env_obj.build_env(extra=merged_extra, set_config_env=set_config_env)
        client = SidecarUiClient(
            cmd,
            env=proc_env,
            sidecar_port=sidecar_port,
            token=token,
            ready_timeout=min(10.0, self.cfg.default_timeout),
        )
        return self.stack.enter_context(client)


class SidecarUiClient:
    """Client managing a real `agy-companion serve` Sidecar Web UI HTTP process."""

    def __init__(
        self,
        cmd: list[str],
        *,
        env: dict[str, str],
        sidecar_port: int,
        token: str,
        ready_timeout: float = 10.0,
    ) -> None:
        self.cmd = list(cmd)
        self.env = dict(env)
        self.sidecar_port = int(sidecar_port)
        self.token = token
        self.ready_timeout = float(ready_timeout)
        self._proc: subprocess.Popen[bytes] | None = None
        self._stderr_chunks: list[bytes] = []
        self._stderr_file: Any | None = None
        self._stderr_thread: threading.Thread | None = None

    @staticmethod
    def is_tool_error(res: dict[str, Any]) -> bool:
        return bool(res.get("isError"))

    @property
    def pid(self) -> int | None:
        return self._proc.pid if self._proc is not None else None

    @property
    def stderr_text(self) -> str:
        if self._stderr_file is not None:
            with contextlib.suppress(OSError, ValueError):
                self._stderr_file.flush()
                self._stderr_file.seek(0)
                return self._stderr_file.read().decode("utf-8", errors="replace")
        return b"".join(self._stderr_chunks).decode("utf-8", errors="replace")

    def start(self) -> SidecarUiClient:
        if os.name == "nt":
            # Avoid Windows CRT pipe-close deadlocks when detached bootstrap daemons
            # inherit stderr handles (bInheritHandles=TRUE).
            self._stderr_file = tempfile.TemporaryFile()
            kwargs: dict[str, Any] = {
                "stdin": subprocess.DEVNULL,
                "stdout": subprocess.DEVNULL,
                "stderr": self._stderr_file,
                "env": self.env,
            }
            self._proc = subprocess.Popen(self.cmd, **kwargs)
        else:
            kwargs = {
                "stdin": subprocess.DEVNULL,
                "stdout": subprocess.DEVNULL,
                "stderr": subprocess.PIPE,
                "env": self.env,
                "start_new_session": True,
            }
            self._proc = subprocess.Popen(self.cmd, **kwargs)
            stderr_pipe = self._proc.stderr
            assert stderr_pipe is not None

            def _drain_stderr() -> None:
                if stderr_pipe is None:
                    return
                try:
                    while True:
                        chunk = stderr_pipe.read(4096)
                        if not chunk:
                            break
                        self._stderr_chunks.append(chunk)
                except (OSError, ValueError):
                    pass

            self._stderr_thread = threading.Thread(target=_drain_stderr, daemon=True)
            self._stderr_thread.start()

        deadline = time.monotonic() + self.ready_timeout
        last_err: str = "not ready"
        try:
            while time.monotonic() < deadline:
                if self._proc.poll() is not None:
                    rc = self._proc.returncode
                    self.close()
                    raise RuntimeError(
                        f"Sidecar UI process exited early ({rc}): {self.stderr_text}"
                    )
                try:
                    resp = http_raw_request("127.0.0.1", self.sidecar_port, "GET", "/", timeout=1.0)
                    if resp.status in (200, 500):
                        return self
                    last_err = f"HTTP {resp.status}"
                except OSError as exc:
                    last_err = str(exc)
                time.sleep(0.02)
            raise TimeoutError(
                f"Sidecar UI on 127.0.0.1:{self.sidecar_port} not ready within {self.ready_timeout}s: {last_err}"
            )
        except BaseException:
            self.close()
            raise

    def get_html(self, *, timeout: float = 5.0) -> HttpResponse:
        return http_raw_request("127.0.0.1", self.sidecar_port, "GET", "/", timeout=timeout)

    def call_api(
        self,
        name: str,
        arguments: dict[str, Any] | None = None,
        *,
        token: str | None = None,
        content_type: str = "application/json",
        timeout: float = 10.0,
    ) -> HttpResponse:
        eff_token = self.token if token is None else token
        headers: dict[str, str] = {"Content-Type": content_type}
        if eff_token:
            headers["X-Sidecar-Token"] = eff_token
        body = json.dumps(arguments or {}).encode("utf-8")
        return http_raw_request(
            "127.0.0.1",
            self.sidecar_port,
            "POST",
            f"/_api/{name}",
            headers=headers,
            body=body,
            timeout=timeout,
        )

    def call_tool_raw(
        self,
        name: str,
        arguments: dict[str, Any] | None = None,
        *,
        timeout: float = 10.0,
    ) -> dict[str, Any]:
        resp = self.call_api(name, arguments, timeout=timeout)
        try:
            payload = resp.json()
        except Exception:
            payload = {"ok": False, "error": resp.text}
        return {
            "isError": resp.status >= 400,
            "status": resp.status,
            "structuredContent": payload,
            "content": [{"type": "text", "text": json.dumps(payload)}],
        }

    def call_tool(
        self,
        name: str,
        arguments: dict[str, Any] | None = None,
        *,
        timeout: float = 10.0,
    ) -> dict[str, Any]:
        res = self.call_tool_raw(name, arguments, timeout=timeout)
        if res["isError"]:
            raise RuntimeError(f"Sidecar API /_api/{name} failed (HTTP {res['status']}): {res['structuredContent']}")
        return res

    def save_settings_smart(
        self,
        *,
        listen_host: str | None = None,
        listen_port: int | None = None,
        base_url: str | None = None,
        default_model: str | None = None,
        api_key: str | None = None,
        clear_api_key: bool = False,
        cloudcode_upstream_url: str | None = None,
        cloudcode_upstream: str | None = None,
        enabled_models: list[str] | None = None,
        account_plan: str | None = None,
        raw: bool = False,
        timeout: float = 10.0,
    ) -> dict[str, Any]:
        payload: dict[str, Any] = {}
        proxy: dict[str, Any] = {}
        if listen_host is not None:
            proxy["host"] = listen_host
        if listen_port is not None:
            proxy["port"] = listen_port
        if proxy:
            payload["proxy"] = proxy
        upstream = cloudcode_upstream if cloudcode_upstream is not None else cloudcode_upstream_url
        if upstream is not None:
            payload["cloudcode"] = {"upstream": upstream}
        if account_plan is not None:
            payload["account_plan"] = account_plan
        gemini: dict[str, Any] = {}
        if clear_api_key:
            gemini["api_key"] = "clear"
        elif api_key is not None:
            gemini["api_key"] = {"set": api_key}
        if base_url is not None:
            gemini["base_url"] = base_url
        if default_model is not None:
            gemini["default_model"] = default_model
        if enabled_models is not None:
            gemini["enabled_models"] = list(enabled_models)
        if gemini:
            payload["providers"] = {"gemini": gemini}
        if raw:
            return self.call_tool_raw("save_settings", payload, timeout=timeout)
        return self.call_tool("save_settings", payload, timeout=timeout)

    def close(self) -> None:
        proc = self._proc
        self._proc = None
        if proc is not None and proc.poll() is None:
            if os.name != "nt":
                with contextlib.suppress(OSError):
                    os.killpg(os.getpgid(proc.pid), signal.SIGTERM)
            with contextlib.suppress(OSError):
                proc.terminate()
            try:
                proc.wait(timeout=3.0)
            except subprocess.TimeoutExpired:
                if os.name != "nt":
                    with contextlib.suppress(OSError):
                        os.killpg(os.getpgid(proc.pid), signal.SIGKILL)
                with contextlib.suppress(OSError):
                    proc.kill()
                with contextlib.suppress(subprocess.TimeoutExpired):
                    proc.wait(timeout=2.0)
        if self._stderr_thread is not None:
            self._stderr_thread.join(timeout=1.0)
            self._stderr_thread = None
        if proc is not None and proc.stderr is not None:
            with contextlib.suppress(OSError):
                proc.stderr.close()
        if self._stderr_file is not None:
            with contextlib.suppress(OSError, ValueError):
                self._stderr_file.flush()
                self._stderr_file.seek(0)
                self._stderr_chunks = [self._stderr_file.read()]
            with contextlib.suppress(OSError):
                self._stderr_file.close()
            self._stderr_file = None

    def __enter__(self) -> SidecarUiClient:
        return self.start()

    def __exit__(self, exc_type: Any, exc: Any, tb: Any) -> None:
        self.close()
