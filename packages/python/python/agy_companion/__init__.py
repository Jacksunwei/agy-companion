"""Python helper for locating the native ``agy-companion`` binary."""

from __future__ import annotations

import os
import shutil
import sys
import sysconfig
from pathlib import Path

BINARY_NAME = "agy-companion"
__all__ = ["BINARY_NAME", "find_binary"]


def _executable_name(platform: str | None = None) -> str:
    """Return the platform-appropriate binary filename."""
    target_platform = sys.platform if platform is None else platform
    return f"{BINARY_NAME}.exe" if target_platform == "win32" else BINARY_NAME


def _is_executable_file(path: Path) -> bool:
    """Return True if ``path`` points to an existing executable regular file."""
    try:
        return path.is_file() and os.access(path, os.X_OK)
    except (OSError, ValueError):
        return False


def _resolve_executable(path: Path) -> Path | None:
    """Resolve ``path`` to an absolute executable regular file, or return None."""
    try:
        resolved = path.resolve()
    except (OSError, ValueError, RuntimeError):
        return None
    if resolved.is_absolute() and _is_executable_file(resolved):
        return resolved
    return None


def _candidate_scripts_dirs() -> list[Path]:
    """Return deduplicated candidate script directories for installed wheels."""
    dirs: list[Path] = []

    def _add(raw: str | Path | None) -> None:
        if not raw:
            return
        candidate = Path(raw)
        if candidate not in dirs:
            dirs.append(candidate)

    _add(sysconfig.get_path("scripts"))

    user_scheme: str | None = None
    if hasattr(sysconfig, "get_preferred_scheme"):
        try:
            user_scheme = sysconfig.get_preferred_scheme("user")
        except (KeyError, ValueError):
            user_scheme = None
    else:
        # Python 3.9 fallback (get_preferred_scheme added in Python 3.10).
        user_scheme = "nt_user" if os.name == "nt" else "posix_user"

    if user_scheme:
        try:
            _add(sysconfig.get_path("scripts", scheme=user_scheme))
        except (KeyError, ValueError):
            pass

    if sys.executable:
        try:
            _add(Path(sys.executable).resolve().parent)
        except OSError:
            pass

    return dirs


def find_binary() -> Path | None:
    """Locate the native ``agy-companion`` executable without spawning a shell.

    Resolution order:
    1. ``AGY_COMPANION_BIN`` environment variable (if set, non-empty, an
       absolute path, and an executable regular file; relative values are
       ignored and fall through).
    2. Bundled binary adjacent to this package directory.
    3. Installed wheel scripts directories (``sysconfig.get_path("scripts")``,
       user-scheme scripts, and the active Python executable directory).
    4. ``PATH`` lookup via ``shutil.which("agy-companion")``.

    Every non-``None`` return value is guaranteed to be an absolute ``Path``
    pointing to an existing executable regular file. Returns ``None`` if no
    executable binary is found.
    """
    env_override = os.environ.get("AGY_COMPANION_BIN")
    if env_override is not None:
        stripped = env_override.strip()
        if stripped and "\x00" not in env_override:
            candidate = Path(stripped)
            if candidate.is_absolute() and _is_executable_file(candidate):
                return candidate

    exe_name = _executable_name()

    bundled = Path(__file__).resolve().parent / exe_name
    if bundled.is_absolute() and _is_executable_file(bundled):
        return bundled

    for scripts_dir in _candidate_scripts_dirs():
        resolved = _resolve_executable(scripts_dir / exe_name)
        if resolved is not None:
            return resolved

    on_path = shutil.which(BINARY_NAME)
    if on_path:
        resolved = _resolve_executable(Path(on_path))
        if resolved is not None:
            return resolved

    return None
