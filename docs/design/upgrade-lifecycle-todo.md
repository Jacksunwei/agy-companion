# User Version Upgrades & Lifecycle

> **Status:** Opt-in background update checks, header version & one-click upgrade pills, detached `npx`/`uvx` upgrades, and Unix `execve` + panel auto-reload are implemented. Remaining follow-ups are tracked below.

## Problem Statement

Users have two coupled install surfaces that can drift out of sync:
1. The native `agy-companion` binary (installed via npm, PyPI, curl/Homebrew, or cached by the plugin launcher).
2. The Antigravity plugin bundle (`~/.gemini/config/plugins/agy-companion/`).

## Implemented

1. **Single version source of truth:** Workspace `Cargo.toml` version drives `plugin/plugin.json`, `packages/npm/agy-companion/package.json`, and `packages/python/pyproject.toml`, verified by `scripts/agent_check.sh`.
2. **Versioned config:** Config file carries `schema_version = 1` from day 1 for safe forward migrations.
3. **Skew detection:** `GET /health` returns `{ "version": "...", "protocol_version": 1 }`. `agy-companion doctor` and the Aux Panel Settings UI compare running proxy version against plugin manifest version and surface a clear warning on mismatch (suppressed during active in-panel upgrades).
4. **Opt-in update checks & one-click upgrade:** Install channel (`npx`/`uvx`) persisted in `update-state.json`, rate-limited background + manual registry checks (`AGY_COMPANION_NO_UPDATE_CHECK=1` / `check_for_updates = false` opt-out), detached `npx`/`uvx` upgrade with `upgrade.lock`, header version & 1-click upgrade pills in the Aux Panel, and automatic panel reload after Unix sidecar `execve`.

## Remaining Follow-Ups

1. **Windows sidecar self-convergence after bootstrap upgrade:** macOS/Linux sidecars converge in-place via POSIX `execve` (`CommandExt::exec` in `crates/companion-cli/src/serve.rs`), keeping the same PID and re-binding `ANTIGRAVITY_SIDECAR_WEB_PORT` with zero error toast. Windows lacks `execve` (`CreateProcess` always creates a new PID), so the Windows sidecar currently stays alive on `vOld` under `#[cfg(not(unix))]` (showing a green `✓ Updated to {version}` header pill) until Antigravity reloads/restarts. Follow-up: verify whether Antigravity's `SidecarManager` (`"restart_policy": "on-failure"`) silently respawns without showing a user-facing error toast when a Windows sidecar exits non-zero after confirming `/health` converged to `vNext`, and enable automatic Windows sidecar convergence if clean.
2. **Automated `schema_version` config migrations** (`v1 -> v2`, etc.) with pre-migration backup.
