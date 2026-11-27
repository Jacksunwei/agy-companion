# User Version Upgrades & Lifecycle (TODO)

> **Status:** Deferred post-Milestone 1 (day-1 seams included in M1).

## Problem Statement

Users have two coupled install surfaces that can drift out of sync:
1. The native `agy-companion` binary (installed via npm, PyPI, curl/Homebrew, or cached by the plugin launcher).
2. The Antigravity plugin bundle (`~/.gemini/config/plugins/agy-companion/`).

## Day-1 Seams (Implemented in Milestone 1)

1. **Single version source of truth:** Workspace `Cargo.toml` version drives `plugin/plugin.json`, `packages/npm/agy-companion/package.json`, and `packages/python/pyproject.toml`, verified by `scripts/agent_check.sh`.
2. **Versioned config:** Config file carries `schema_version = 1` from day 1 for safe forward migrations.
3. **Skew detection:** `GET /health` returns `{ "version": "...", "protocol_version": 1 }`. `agy-companion doctor` and the Aux Panel Settings UI compare running proxy version against plugin manifest version and surface a clear warning on mismatch.

## Deferred Scope (Design Later)

1. `agy-companion upgrade` command (detects install channel: npm, PyPI/uv, standalone binary, or plugin cache; upgrades cleanly with SHA-256 verification).
2. Lockstep plugin + binary refresh workflow (including restarting the sidecar cleanly after upgrade).
3. **Windows sidecar self-convergence after bootstrap upgrade:** macOS/Linux sidecars converge in-place via POSIX `execve` (`CommandExt::exec` in `crates/companion-cli/src/serve.rs`), keeping the same PID and re-binding `ANTIGRAVITY_SIDECAR_WEB_PORT` with zero error toast. Windows lacks `execve` (`CreateProcess` always creates a new PID), so the Windows sidecar currently stays alive on `vOld` under `#[cfg(not(unix))]` until Antigravity reloads/restarts. Follow-up: verify whether Antigravity's `SidecarManager` (`"restart_policy": "on-failure"`) silently respawns without showing a user-facing error toast when a Windows sidecar exits non-zero after confirming `/health` converged to `vNext`, and enable automatic Windows sidecar convergence if clean.
4. Non-intrusive update check (rate-limited, opt-out via env var / config).
5. Automated `schema_version` config migrations (`v1 -> v2`, etc.) with pre-migration backup.
