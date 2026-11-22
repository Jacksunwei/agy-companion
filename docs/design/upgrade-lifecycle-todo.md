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
3. Non-intrusive update check (rate-limited, opt-out via env var / config).
4. Automated `schema_version` config migrations (`v1 -> v2`, etc.) with pre-migration backup.
