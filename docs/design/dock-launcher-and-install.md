# Native Dock/Launcher Integration, CloudCode Transparency & One-Command Install

> **Status:** Implemented (PR 1–PR 3 rollout)
> **Scope:** PR 1 (`fix/cloudcode-pro-transparency-and-proxy-hardening`), PR 2 (`feat/dock-launcher-integration`), and PR 3 (`feat/one-command-install-uninstall`).

This design documents how `agy-companion` integrates seamlessly with the stock Google Antigravity desktop Dock/Spotlight/Launcher icon across macOS, Linux, and Windows while keeping personal Google AI Pro, PayGo, and Enterprise CloudCode routing transparent and reversible.

---

## 1. Problem & Timing Constraints

When Antigravity App starts from macOS Dock/Spotlight or a Linux desktop launcher, Electron resolves the user's login-shell environment (`$SHELL -ilc ...` with `ANTIGRAVITY_RESOLVING_ENVIRONMENT=1`, bounded at 10s), then spawns `language_server` before the plugin sidecar manager starts:

- `language_server` reads `CLOUD_CODE_URL` at **`T+72ms`** and calls `POST /v1internal:fetchAvailableModels` at **`T+839ms`**.
- `SidecarManager` starts the plugin's `proxy` sidecar *after* authentication completes at **`T+~2,200ms`** (~1.5s too late for initial model catalog discovery and unable to inject env vars into `language_server`).

**Non-invasive contract:** `agy-companion` **never** patches `/Applications/Antigravity.app` (preserves code signing) and **never** mutates `~/.gemini/config/config.json`.

### End-to-End Boot, Runtime & Env-Isolation Flow

```text
  [macOS / Linux: User clicks stock Antigravity Dock / Spotlight / Launcher icon]
         |
         v
  Electron main process runs: $SHELL -ilc ... (ANTIGRAVITY_RESOLVING_ENVIRONMENT=1)
         |
         v  Zsh (~/.zlogin) / Bash (existing login profile or ~/.profile) / Fish (conf.d/agy-companion.fish)
  Sources owned 0600 ${XDG_DATA_HOME:-$HOME/.local/share}/agy-companion/env.sh
         |-- If SSH_CONNECTION or SSH_CLIENT is set (and AGY_COMPANION_ALLOW_REMOTE_SSH != 1)
         |      -> no-op (Remote SSH safe)
         |-- If ANTIGRAVITY_RESOLVING_ENVIRONMENT=1 and AGY_COMPANION_HOOK_ACTIVE != 1:
         |      -> Runs: agy-companion hook-env --ensure-running --print-port (ZERO eval!)
         |            |-- Verifies ~/.gemini/config/plugins/agy-companion/plugin.json exists (else fail-open)
         |            |-- Probes http://127.0.0.1:<port>/health (250ms timeout, 64 KiB body cap)
         |            |-- If unreachable: spawns detached Managed On-Demand Daemon
         |            |     agy-companion serve --idle-shutdown-secs 1800 [--watch-pid <ANTIGRAVITY_PID>]
         |            |     (walks ancestor PIDs under ANTIGRAVITY_RESOLVING_ENVIRONMENT=1 to tie lifetime
         |            |      to Antigravity.app, inherits live HTTPS_PROXY / XDG_*, writes 0600 proxy.pid & proxy.log)
         |            `-- Prints validated u16 port ONLY (or empty stdout, exiting 0 -> fail-open to stock Google)
         |      -> Exports CLOUD_CODE_URL, GOOGLE_GEMINI_BASE_URL, conditional GEMINI_API_KEY
         |         (NEVER overwrites or copies a real GEMINI_API_KEY or GOOGLE_API_KEY),
         |         sets AGY_COMPANION_HOOK_ACTIVE=1, and merges JETSKI_MCP_ENV_RESTORE_PREFIX / _VARS.
         v
  language_server boots (T+0ms)
         |-- T+72ms:  Overrides CloudCodeServerURL -> http://127.0.0.1:<port>
         |-- T+839ms: POST /v1internal:fetchAvailableModels -> local proxy (ALREADY WARM)
         |              |-- Day-0 (no real API key saved): returns upstream JSON UNTOUCHED
         |              `-- Key saved: preserves ALL native Google models + appends (AI Studio) models
         |-- T+2.2s:  SidecarManager runs `serve --adopt-running` (500ms health poll; owns Aux Panel UI; NEVER binds :24141)
         |              |-- Adopts warm bootstrap daemon (toggling/restarting plugin never breaks :24141)
         |              |-- Watches `<plugin_dir>/plugin.json` (`AGY_COMPANION_PLUGIN_DIR` -> `AGY_PLUGINS_DIR`
         |              |     -> default plugins dir) in standby
         |              |-- Converges toward `desired_version = plugin.json.version ?? running_version`:
         |              |     * Direct skew (`health != running == desired`): spawns `current_exe()` `--replace-stale`
         |              |     * Plugin advance/rollback (`health != desired != running`): passively polls local
         |              |       candidate binaries (`AGY_COMPANION_BIN` -> dev `target/{debug,release}` -> startup
         |              |       `current_exe()` -> cache `bin/` -> `$PATH`) gated by `(len, mtime)` + `--version`
         |              |       and spawns the matching binary with `--replace-stale` (+ `--watch-pid`, sidecar
         |              |       UI/LS/plugin env scrubbed) for ~10-20ms zero-drop SSE handoff; old sidecar stays
         |              |       alive and goes idle once `/health.version == desired_version`
         |              `-- Recovers :24141 in <1s on any mid-session crash by spawning a detached `bootstrap`
         |                  daemon (sidecar UI/LS/plugin env scrubbed; killing the sidecar leaves :24141 alive)
         |-- MCP servers (gemini-api, ui-mcp, 3P): restored via JETSKI_MCP_ENV_RESTORE_* + run-companion.sh scrub
         `-- Integrated terminals ($SHELL -l): env.sh value-guarded ${VAR+x} 3-state scrub restores clean env
```

---

## 2. PR 1 — Google AI Pro / PayGo / Enterprise CloudCode Transparency & Daemon Primitives

### 2.1 Accurate, OAuth-Refresh-Safe CloudCode `auto` Routing

Consumer Google AI Pro accounts populate a non-empty managed `cloudaicompanionProject` field, so treating any `"project"` key as PayGo/Business misrouted personal accounts to `prod`. PR 1 deletes `request_hints_business_project` and enforces a strict precedence order in `crates/companion-proxy/src/cloudcode/` (`routing.rs` / `metadata.rs`):

1. **Explicit fixed `cloudcode.upstream`** (any value other than `"auto"`): honors the configured upstream directly.
2. **Enterprise entitlement hint:** `fetchAvailableModels` request body containing `entitlement.userTier` routes to `https://cloudcode-pa.googleapis.com` (`prod`) with `strip_user_project = true`.
3. **`User-Agent` `auth_method=` hint:** `auth_method=gcp` or `auth_method=adc` routes to `prod`; `auth_method=consumer` routes to `https://daily-cloudcode-pa.googleapis.com` (`daily`).
4. **`loadCodeAssist` tier inspection:** inspects both `paidTier.usesGcpTos` and `currentTier.usesGcpTos` plus `gcp-*` tier prefixes, with `prod` -> `daily` fallback on non-2xx/transport failure.
5. **Unresolved `fetchAvailableModels`:** defaults to `daily` first with metadata-only `daily` <-> `prod` fallback when upstream returns non-2xx or empty `models`.

**Per-Account LRU & Hot-Reload Preservation (zero new crates):**
- An 8-entry bounded LRU keyed by 128-bit SipHash-1-3 (`std::collections::hash_map::RandomState` x2; raw Bearer tokens are never stored on the heap) stores `AccountRoute { use_prod, strip_user_project }`.
- On hourly OAuth token refresh (cache miss on `forward_raw`), routing checks `User-Agent` `auth_method=` and falls back to `last_resolved_prod`, preventing PayGo/Enterprise sessions from bouncing back to `daily`.
- `HotReloadProvider` preserves the `Arc<CloudCodeClient>` (and routing cache) across config hot-reloads whenever `cloudcode.upstream` and `timeout_secs` are unchanged.

**Hard Idempotency Invariant:**
- `daily` <-> `prod` fallback and the 1-time Enterprise `403` retry without `x-goog-user-project` run **ONLY** on read-only metadata RPCs (`fetchAvailableModels`, `loadCodeAssist`, `listExperiments`) — **NEVER** on generation or streaming RPCs (`streamGenerateContent`, etc.).

**Transport, Timeout & Model Slot Fidelity:**
- Conditional `.no_proxy()` on `CloudCodeClient` (only for loopback upstream URLs) preserves corporate `HTTPS_PROXY`/`HTTP_PROXY`/`ALL_PROXY`.
- `.http1_only()` + `.read_timeout(>=600s)` for long streaming turns, plus `tokio::time::timeout(60s)` on buffered metadata calls.
- Forwards `x-goog-request-reason` and `content-encoding`; preserves upstream `Content-Encoding` verbatim on raw fallback and Day-0 pass-through (S4).
- Deterministic placeholder enum slot assignment across `MODEL_PLACEHOLDER_M334..=M365` (independent of `default_model` ordering) and `XHigh` (`thinkingLevel: 7`) support for `gemini-4-neon`.

### 2.2 Day-0 UX, Sentinel Quarantine & Managed On-Demand Daemon

- `PROXY_PLACEHOLDER_API_KEY` (`"agy-companion-local"`) lives in `companion-core` (`is_placeholder_api_key`) and is quarantined across `companion-providers`, `companion-proxy`, `companion-ui-mcp`, and `doctor` so it never leaks upstream or false-passes diagnostics.
- On Day-0 (`!has_configured_api_key`), `POST /v1internal:fetchAvailableModels` returns upstream JSON and headers untouched (hiding `(AI Studio)` models until a real API key is saved).
- `GET /api/stats` exposes structured `StatsSnapshot` JSON (`{ "usage": { ... }, "cloudcode": { "mode", "resolved_upstream", "last_catalog_refresh_at_ms", "last_catalog_included_proxy_models" } }`).
- Unsupported non-Gemini-3.8+/4+ `400` responses include the actionable hint `unset GOOGLE_GEMINI_BASE_URL to reach Gemini API directly`.
- Managed On-Demand Daemon primitives: `--watch-pid <PID>` (zombie- and PID-reuse-safe parent tracking that exits cleanly within ~1–2s after `Antigravity.app` quits once `in_flight_requests == 0`), `--idle-shutdown-secs 1800` fallback guarded by active in-flight requests and open SSE streams (passive `/health` and `/api/stats` probes do not reset idle time), `--replace-stale` smooth version-skew handoff (pre-initialized `ProxyState`, carrying over `last_catalog_refresh_at_ms` / `last_catalog_included_proxy_models` from the outgoing daemon's `/api/stats` so Hybrid Mode detection never resets across handoffs, 4-layer OS-verified non-blocking `SIGTERM` without `SIGKILL`, `10ms` bind tight-poll while the old daemon drains active SSE streams, and idempotent exit `0` when a matching version already owns the port), `64 KiB`-capped `/health` probe with optional `role` (`"bootstrap"` | `"standalone"` — port `24141` is exclusively owned by a detached `bootstrap` daemon or manual `standalone` `agy-companion serve`, never a sidecar), `500ms` `--adopt-running` standby polling with detached-bootstrap crash recovery and plugin-manifest self-upgrade watching (`plugin/scripts/run-companion.sh` exports `AGY_COMPANION_PLUGIN_DIR`; `serve --adopt-running` never binds the proxy port in-process, recovers an unreachable `listen_port` in `<1s` by spawning a detached `bootstrap` daemon with sidecar UI/LS/plugin env scrubbed so killing or restarting the sidecar leaves `:24141` alive, fail-closed reads `plugin.json`, passively polls local candidate binaries gated by `(len, mtime)` + exact `--version` match whenever `/health` differs from `plugin.json`, spawns the matching binary with `--replace-stale` + `--watch-pid` while scrubbing `ANTIGRAVITY_SIDECAR_WEB_PORT`, `ANTIGRAVITY_SIDECAR_UI_TOKEN`, `ANTIGRAVITY_LS_ADDRESS`, `ANTIGRAVITY_CSRF_TOKEN`, and `AGY_COMPANION_PLUGIN_DIR`, and once `/health` converges self-`exec`s the upgraded binary in-place on Unix — preserving PID and re-binding the Sidecar Web UI with zero version skew — while staying alive without downgrading on Windows), atomic `0600` structured `proxy.pid` (PID-matched `ProxyPidGuard::drop` race safety), and `0600` rotating `5 MiB` `proxy.log`.

| Criterion | OS User Service (`launchd` / `systemd --user`) | Managed On-Demand Daemon + `SidecarManager` (Chosen) |
| --- | --- | --- |
| Live `HTTPS_PROXY` & `~/.zshrc` `XDG_*` | Stripped by OS service manager | Inherited automatically from `$SHELL -ilc` |
| macOS Login Items alert | Triggers "Background Items Added" popup | Zero OS popups |
| Idle footprint after `Cmd+Q` | Runs 24/7 | Exits cleanly within ~1–2s via `--watch-pid` (with `--idle-shutdown-secs 1800` fallback) |
| Version bump / rebuild handoff | Manual restart drops active streams | ~10–20ms zero-drop SSE handoff via sidecar `--adopt-running` -> detached `--replace-stale` |
| Mid-session crash recovery | Fast, but fights `SIGTERM` / port changes | `<1s` detached `bootstrap` respawn via `SidecarManager` `serve --adopt-running` (`500ms` poll; sidecar never binds `:24141`) |
| Cross-platform portability | Divergent (`launchd` / `systemd` / Windows Task) | One unified lifecycle across macOS, Linux, WSL, containers, Windows |

---

## 3. PR 2 — Dock / Launcher Integration (`env.sh`, Shell RC Hooks & MCP App UI)

- **Zero-`eval` `0700`/`0600` owned `env.sh`:** Written to `${XDG_DATA_HOME:-$HOME/.local/share}/agy-companion/env.sh`. Invokes `agy-companion hook-env --ensure-running --print-port` and validates the numeric port via POSIX `case` (zero `eval`, zero `| source`).
- **Remote SSH guard:** Skips injection whenever `SSH_CONNECTION` or `SSH_CLIENT` is non-empty unless `AGY_COMPANION_ALLOW_REMOTE_SSH=1` is explicitly exported.
- **Collision-safe native MCP restoration:** Reuses any valid pre-existing `JETSKI_MCP_ENV_RESTORE_PREFIX` (defaulting to `AGY_COMPANION_ORIG_`) and appends overridden variables to `JETSKI_MCP_ENV_RESTORE_VARS`. Never touches `GOOGLE_API_KEY` and never copies or overwrites an existing real `GEMINI_API_KEY`.
- **Value-guarded 3-state integrated-terminal scrub:** When `AGY_COMPANION_HOOK_ACTIVE=1` is inherited by integrated login terminals (`$SHELL -l`), `env.sh` restores original `CLOUD_CODE_URL`/`GOOGLE_GEMINI_BASE_URL` via POSIX `${VAR+x}` 3-state checks, unsets `GEMINI_API_KEY` only when equal to `"agy-companion-local"`, and clears all companion hook variables.
- **Shell RC placement & dotfile safety:**
  - Zsh: `${ZDOTDIR:-$HOME}/.zlogin` (runs after `.zshrc`, survives `.zshrc` `ANTIGRAVITY_RESOLVING_ENVIRONMENT` early returns, leaves Stow/Nix `.zshrc` untouched, and is deleted cleanly on disable/uninstall when empty).
  - Bash: first existing of `~/.bash_profile`, `~/.bash_login`, `~/.profile`, falling back to `~/.profile`.
  - Fish: `${XDG_CONFIG_HOME:-$HOME/.config}/fish/conf.d/agy-companion.fish` (zero `| source`).
  - Symlinked or read-only RC files (Nix, Home Manager, Stow, Chezmoi) are never silently mutated; status reports `state: "manual_required"` with a 1-line static snippet (CLI opts in explicitly via `--follow-rc-symlink`).
  - Marker parser matches `# >>> agy-companion` and `# <<< agy-companion` prefixes and fails closed with exact 1-based line numbers on unbalanced or corrupted markers.
- **CLI & MCP App UI:** Adds `hook-env --ensure-running --print-port`, `integration enable|disable|status [--shell] [--rc-file] [--binary] [--follow-rc-symlink] [--json]`, updated `launch` (`JETSKI_MCP_ENV_RESTORE_*` + `--idle-shutdown-secs 1800`), app-only `set_launcher_integration({ enabled: bool })` (`#[serde(deny_unknown_fields)]`, async mutex serialized, `follow_rc_symlink: false`), and an updated single-file UI (`< 512,000` bytes, strictly S5 zero-`innerHTML`) with a **Dock / Launcher Auto-Start** card, CloudCode active-upstream badge, post-save auto-verification + live 5-minute model-refresh countdown banner, collapsed Advanced/CLI `<details>`, and platform-aware launch command snippet.

---

## 4. PR 3 — One-Command Install, Upgrade, Clean Uninstall & Windows Parity

### 4.1 v1 Distribution Policy (`npx` / `npm` / `uvx` — No Web Bootstrap Scripts)

Across macOS, Linux, and Windows, v1 leads exclusively with package-managed native binary shims delegating to the single Rust `install` / `uninstall` engine:

- **Hero one-command install:** `npx -y agy-companion@latest install`
- **Global npm alternative:** `npm i -g agy-companion && agy-companion install`
- **Python / `uvx` alternative:** `uvx --refresh agy-companion install`
- **Upgrade:** Re-run `npx -y agy-companion@latest install` (or `npm i -g agy-companion@latest && agy-companion install`).
- **Uninstall:** `npx -y agy-companion@latest uninstall [--purge]` (or `agy-companion uninstall [--purge]`).

Standalone web bootstrap scripts are intentionally omitted in v1 to avoid duplicate downloader maintenance, pipe-to-shell trust friction, and platform script drift. Signed macOS `.pkg`/`.dmg`, Windows `.msi`, Homebrew, and `winget` remain deferred to Phase 2.

### 4.2 Embedded 7-File Plugin Bundle & Race-Free `install`

- All 7 files in `plugin/` (`README.md`, `assets/logo.svg`, `mcp_config.json`, `sidecars/proxy/sidecar.json`, `skills/companion/SKILL.md`, `scripts/run-companion.sh`, and `plugin.json` **last**) are embedded into `crates/companion-cli/src/install.rs` via `include_str!` and enforced by a byte-for-byte Rust unit test against `./plugin/`.
- `agy-companion install [--plugins-dir <PATH>] [--no-launcher-hook] [--no-start-proxy] [--force] [--json]`:
  1. Validates `--plugins-dir` (S6 fail-closed) and enforces a **dev-symlink guard** refusing to overwrite symlinked plugin checkouts unless `--force` is passed (which unlinks only the symlink, never its target).
  2. On fresh install (when `config.json` does not exist), probes `127.0.0.1:24141..=24150` (even with `--no-start-proxy`) and persists an auto-selected free port to `0600` `config.json` if `24141` is occupied by a non-companion process.
  3. Stages the stable binary (`<data_dir>/bin/agy-companion[.exe]`), `.version`, and 7-file plugin directory (`.agy-companion.stage.<uuid>`, writing `plugin.json` last) with RAII cleanup guards.
  4. Executes a tight atomic swap for the binary + `.version`, creates an optional `~/.local/bin/agy-companion` symlink (never overwriting existing regular files or foreign symlinks), and performs a 2-step same-filesystem plugin swap (`.backup.<uuid>` with rollback on rename failure and zero leftover staging/backup dirs).
  5. Starts the detached daemon from the permanent `stable_bin` path (not an ephemeral `npx` cache path), or replaces a version-skewed running proxy using **4-layer zero-`unsafe` PID verification** (PID sanity -> optional `/health` -> OS `argv[0]`/`serve` identity check via `/proc`, `/bin/ps`, or `Get-CimInstance` -> graceful stop + re-verified escalation).
  6. Configures the login-shell hook on Unix (or reversible Start Menu `Antigravity (Companion).lnk` on Windows) and prints `$PATH`-aware uninstall guidance plus explicit `Cmd+Q` / full-quit instructions.

### 4.3 Race-Free `uninstall` Ordering

`agy-companion uninstall [--plugins-dir <PATH>] [--purge] [--dry-run] [--json]` executes in strict race-free order:

1. Strip marker blocks across all known shell RC files (`.zlogin`, `.zprofile`, `.zshrc`, `.bash_profile`, `.bash_login`, `.profile`, `fish/conf.d/agy-companion.fish`), deleting empty `.zlogin` and `agy-companion.fish` while preserving user lines and reporting symlinked/read-only files in `rc_manual_remaining`.
2. Delete `<data_dir>/env.sh` (instant global neutralization even on read-only Nix dotfiles).
3. **Delete `<plugins_dir>/agy-companion` BEFORE stopping the proxy** to eliminate Antigravity `SidecarManager`'s `"restart_policy": "on-failure"` respawn race. Unlinks symlinks without deleting targets, and refuses to delete foreign directories (missing or non-companion `plugin.json`) with a warning and exit `0`.
4. Stop any verified companion proxy via 4-layer zero-`unsafe` PID verification (never signaling unrelated PIDs) and remove `proxy.pid` + Windows `.lnk`.
5. Verify `~/.local/bin/agy-companion` symlink ownership **before** removing `stable_bin` (so macOS `/var` -> `/private/var` canonicalization succeeds), then delete `stable_bin`, `.version`, and empty `bin/`.
6. Preserve `config.json` and `usage/` by default; `--purge` deletes `config.json`, `usage/`, `proxy.log`, `proxy.log.1`, `sidecar_data/agy-companion/`, and prunes empty parent directories. `~/.gemini/config/config.json` and `/Applications/Antigravity.app` are **never** touched.

### 4.4 Hardened `plugin/scripts/run-companion.sh` & Windows Parity

- `plugin/scripts/run-companion.sh` (Bash 3.2 `/bin/bash -n` & S8 clean): scrubs injected proxy env vars via `exec_clean` before every `exec` (preserving real `GEMINI_API_KEY` and `GOOGLE_API_KEY`), honors `XDG_DATA_HOME` (absolute, no `..`), and checks version-matched cache directories **before** unversioned `$PATH` (falling back to `$PATH` when `.version` mismatches or cache is missing).
- **Windows parity:** `launch.rs` auto-discovers `%LOCALAPPDATA%\Programs\Antigravity\Antigravity.exe` and `%ProgramFiles%\Antigravity\Antigravity.exe`; `install.rs` synthesizes direct-`.exe` `mcp_config.json` and `sidecars/proxy/sidecar.json` on `#[cfg(windows)]` (zero `bash` dependency) and manages a reversible Start Menu shortcut `Antigravity (Companion).lnk` (`CREATE_NO_WINDOW`, `WindowStyle = 7`). Registry environment mutations and `setx`-style global overrides are strictly prohibited.

---

## 5. Verification & Residual Caveats

- **Verification gates:** `./scripts/agent_check.sh` (7/7 gates: governance, hygiene/secret scan, zero AI attribution, Bash 3.2 & modern bash checks, 4-way version parity & packaging shims + `26/26` E2E self-tests, Rust `fmt`/`clippy -D warnings`/`test` + S5 UI scanning, plugin manifest/SVG/`agy plugin validate --strict ./plugin`) plus the full 4-tier isolated-`$HOME` opaque-box E2E suite (`333` tests across Tiers 1–4, run via `python3 -B`).
- **Residual non-login `run_command` subshells:** Non-login agent tool subshells inherit `GOOGLE_GEMINI_BASE_URL`; keeping `/v1beta/models/*` strictly Gemini-3.8+/4+-only with the explicit `400` `unset GOOGLE_GEMINI_BASE_URL to reach Gemini API directly` hint avoids widening S7 or silently billing the companion key for unrelated scripts.
- **Windows host smoke test:** Cross-platform Rust unit tests validate Windows `.exe` discovery, direct-`.exe` manifests, and PowerShell shortcut quoting on macOS/Linux; end-to-end Start Menu execution should be smoke-tested on a native Windows host when cutting the first release.

## In-Panel Upgrade Checks & One-Click Upgrade

- **Channel tracking:** `install --channel npx|uvx` (auto-propagated by the npm/Python shims via `AGY_COMPANION_INSTALL_CHANNEL`) persists the active install channel in `0600` `update-state.json` under the runtime data directory. `agy-companion doctor` surfaces offline update status without making network calls.
- **Rate-limited background checks:** The sidecar checks the matching public registry (`registry.npmjs.org` for `npx`, `pypi.org` for `uvx`) after a startup delay with a 12-hour TTL, and skips background checks when `check_for_updates = false`, `AGY_COMPANION_NO_UPDATE_CHECK=1`, or `CI=1`.
- **Header pills (Aux Panel UI):**
  - Clicking the header version pill opens a compact popover showing `v{current} · {channel}`, current update status, and a manual **Check for updates** button (`POST /_api/check_update`).
  - When a newer version is available, a contextual **`↑ Upgrade to {version}`** pill appears beside the version pill. Clicking it triggers a detached `npx`/`uvx` upgrade immediately via `POST /_api/start_upgrade` (or shows a copyable fallback command when `npx`/`uvx` is not on `PATH`).
  - While upgrading, the UI polls `POST /_api/get_update_status` every 2 seconds, holds `Upgrading…` across brief Unix `execve` reconnect blips, and suppresses transient footer `Version skew` warnings. On macOS/Linux, once the sidecar self-`execve`s onto the new version, the panel reloads automatically; on Windows, the pill transitions to green **`✓ Updated to {version}`**.
