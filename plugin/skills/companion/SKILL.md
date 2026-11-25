---
name: companion
description: Open Antigravity Companion settings, manage Dock/Launcher auto-start and install/uninstall, configure customModelsConfig for the local Gemini proxy, inspect cache/usage telemetry, and diagnose setup issues with agy-companion doctor.
---

# Antigravity Companion (`agy-companion`)

Use this skill when the user wants to install or uninstall Antigravity Companion, configure Bring Your Own Model (BYOM) Gemini routing or Dock/Launcher auto-start in Google Antigravity, open the Antigravity Companion Settings panel, generate `customModelsConfig` for `agy` CLI, or troubleshoot the local proxy sidecar.

## 1. Open the Settings Panel (MCP App UI)

Invoke the bundled MCP tool **`open_settings`** (on server `agy-companion_hub`, serving `ui://companion/settings.html` in the `aux_pane` and `global` entrypoints).

- **Do not ask the user to paste plaintext API keys into chat** when the Settings UI is available. Direct them to enter the key in the write-only API key field inside the Settings panel (saved atomically — Unix mode `0600` / parent dir `0700`, Windows user-profile ACLs with bounded `3`-attempt rename retry) or export `GEMINI_API_KEY` in their environment.
- **Day-0 behavior:** Until a real Gemini API key is saved, `agy-companion` leaves Antigravity's native `fetchAvailableModels` catalog completely untouched (no `(Proxy)` models appear, and `"agy-companion-local"` is quarantined so it never leaks upstream). After saving a key, the UI auto-verifies via `test_connection` and displays a live 5-minute model-refresh countdown (or quit Antigravity with `Cmd+Q` and reopen to load `(Proxy)` models immediately).
- Available MCP tools on `agy-companion_hub` (5 tools):
  - `open_settings` — launches the Settings & Proxy Telemetry panel (`aux_pane` + `global`) and returns a redacted status summary.
  - `get_settings` *(app-only)* — returns redacted config (`api_key_configured`, masked `api_key_hint`), `launcher_integration` status, `cloudcode` active-upstream status, proxy `/health`, version skew status, and `/api/stats` telemetry. Never returns plaintext keys.
  - `save_settings` *(app-only)* — validates `api_key`, `clear_api_key`, `default_model`, `base_url`, `listen_port`, and `listen_host` (loopback only), then persists `config.json` atomically (Unix `0600` / Windows ACLs). Leaving `api_key` blank preserves the existing key unless `clear_api_key: true` is passed.
  - `test_connection` *(app-only)* — verifies upstream Gemini API reachability and key validity (`GET /v1beta/models?pageSize=1` via `x-goog-api-key`).
  - `set_launcher_integration` *(app-only, `{ enabled: bool }`, `deny_unknown_fields`)* — toggles Dock/Launcher login-shell auto-start on macOS/Linux without ever following symlinked RC files (`manual_required` on Nix/Stow).

## 2. Install, Connect Antigravity App (Dock Auto-Start / Hybrid Mode) & `agy` CLI

- **One-Command Install / Upgrade / Uninstall (macOS, Linux & Windows):**
  - Install or upgrade: `npx -y agy-companion@latest install` (alternatives: `npm i -g agy-companion && agy-companion install` or `uvx --refresh agy-companion install`).
  - Uninstall: `npx -y agy-companion@latest uninstall [--purge]` (or `agy-companion uninstall [--purge]`). Default preserves `config.json` and `usage/`; `--purge` removes all companion data and never touches `~/.gemini/config/config.json`.
- **Antigravity App (Dock / Launcher Auto-Start — Recommended):**
  - On macOS/Linux, `install` (or `agy-companion integration enable`) writes `0600` zero-`eval` `env.sh` + a 1-line hook in `~/.zlogin` (Zsh), `~/.profile` / existing login profile (Bash), or `fish/conf.d/agy-companion.fish` (Fish) so opening Antigravity from Dock/Spotlight/Launcher automatically keeps all original Google AI Pro / PayGo / Enterprise models alongside `(Proxy)` Gemini 4 / 3 models while isolating MCP servers (`JETSKI_MCP_ENV_RESTORE_*`) and integrated terminals. Quit completely (`Cmd+Q` on macOS) and reopen after enabling.
  - On Windows, `install` emits direct-`.exe` plugin manifests (zero `bash` dependency) and creates a reversible Start Menu shortcut **`Antigravity (Companion).lnk`** (no `setx` or global environment mutations).
  - Manual one-shot launch: `agy-companion launch` (supports `--print-env`, `--app <PATH>`, `--port <PORT>`).
- **Antigravity CLI (`agy`):** Run `agy-companion models --json` (optionally `--port <PORT>` or `--config <CONFIG>`) to generate the ready-to-paste JSON snippet for `~/.gemini/antigravity-cli/settings.json`:

```json
{
  "modelProvider": "gemini",
  "customModelsConfig": {
    "customModels": [
      {
        "modelName": "gemini-api://http://127.0.0.1:24141/models/gemini-4-hydrogen",
        "label": "Antigravity Companion (gemini-4-hydrogen)"
      },
      {
        "modelName": "gemini-api://http://127.0.0.1:24141/models/gemini-4-argon",
        "label": "Antigravity Companion (gemini-4-argon)"
      },
      {
        "modelName": "gemini-api://http://127.0.0.1:24141/models/gemini-4-neon",
        "label": "Antigravity Companion (gemini-4-neon)"
      },
      {
        "modelName": "gemini-api://http://127.0.0.1:24141/models/gemini-4-carbon",
        "label": "Antigravity Companion (gemini-4-carbon)"
      },
      {
        "modelName": "gemini-api://http://127.0.0.1:24141/models/gemini-3.8-flash",
        "label": "Antigravity Companion (gemini-3.8-flash)"
      }
    ]
  }
}
```

- When `modelProvider` is `"gemini"`, `agy` CLI requires `GEMINI_API_KEY` to be non-empty in the shell environment. If an API key is already saved in `agy-companion`'s config file, the proxy authenticates upstream with the saved key.
- Alternatively, route CLI, IDE extensions, or SDK requests via:
  ```bash
  export GOOGLE_GEMINI_BASE_URL="http://127.0.0.1:24141"
  ```

## 3. CLI Subcommands & Endpoints

| Command | Purpose |
| --- | --- |
| `agy-companion install [--plugins-dir <DIR>] [--no-launcher-hook] [--no-start-proxy] [--force] [--json]` | Install embedded 7-file plugin, stable binary, Dock/Launcher hook (or Windows `.lnk`), and local proxy |
| `agy-companion uninstall [--plugins-dir <DIR>] [--purge] [--dry-run] [--json]` | Strip shell RCs, delete `env.sh` and plugin before stopping verified proxy, remove binary (preserve config/usage unless `--purge`) |
| `agy-companion integration enable\|disable\|status [--shell <zsh\|bash\|fish>] [--rc-file <PATH>] [--binary <PATH>] [--follow-rc-symlink] [--json]` | Manage macOS/Linux login-shell Dock/Launcher integration |
| `agy-companion hook-env --ensure-running --print-port [--port <PORT>] [--config <CONFIG>] [--plugins-dir <DIR>]` | Fail-open login-shell hook helper: ensure Managed On-Demand Daemon is healthy and print validated `u16` port |
| `agy-companion serve [--port <PORT>] [--config <CONFIG>] [--adopt-running] [--idle-shutdown-secs <SECS>]` | Run the loopback proxy (`127.0.0.1:24141` by default) with config mtime hot-reload, sidecar adoption (`500ms` poll), and optional idle shutdown |
| `agy-companion launch [--port <PORT>] [--config <CONFIG>] [--app <PATH>] [--print-env]` | Ensure proxy is running and launch Antigravity App in Hybrid Mode |
| `agy-companion ui-mcp [--data-dir <DATA_DIR>] [--config <CONFIG>]` | Run the stdio MCP App server (stdout strictly JSON-RPC; logs to stderr) |
| `agy-companion doctor [--config <CONFIG>] [--port <PORT>] [--json]` | Check config permissions (Unix `0600`; skipped on Windows), config validity, proxy `/health`, version/protocol skew, and env vars |
| `agy-companion models [--json] [--port <PORT>] [--config <CONFIG>]` | Emit `customModelsConfig` JSON for `~/.gemini/antigravity-cli/settings.json` |
| `agy-companion --version` | Print the binary version (`agy-companion 0.1.0`) |

Local proxy HTTP endpoints (`127.0.0.1:24141`):
- `GET /health` — `{"status":"ok","version":"0.1.0","protocol_version":1}`
- `GET /api/stats` — session request/token totals, `cache_hit_ratio`, `last_catalog_refresh_at_ms`, `last_catalog_included_proxy_models`, `cloudcode_mode`, and `cloudcode_resolved_upstream`
- `POST /v1beta/models/{model}:generateContent` & `:streamGenerateContent?alt=sse` — byte-for-byte streaming pass-through

## 4. Config, Usage & Troubleshooting

- **Config file:** `--config <path>` → `AGY_COMPANION_CONFIG` → `$XDG_CONFIG_HOME/agy-companion/config.json` *(when set, non-empty, and absolute)* → platform default:
  - **macOS & Linux:** `~/.config/agy-companion/config.json` *(intentional XDG default on macOS — no `~/Library/Application Support/` drift)*
  - **Windows:** `%APPDATA%\agy-companion\config.json` *(fallback: `%USERPROFILE%\.config\agy-companion\config.json`)*
  - Saved atomically (Unix `0600` / parent `0700`; Windows ACLs with bounded `3`-attempt retry). Symlinks, `..` traversal, and NUL bytes are rejected fail-closed.
- **Data, usage & daemon files:** `$XDG_DATA_HOME/agy-companion/` *(platform default `~/.local/share/agy-companion/` on macOS/Linux; `%LOCALAPPDATA%\agy-companion\` on Windows)* stores `usage/usage.db` (SQLite WAL, `0600`, metadata only), `env.sh` (`0600`), `proxy.pid` (`0600`), rotating `5 MiB` `proxy.log` (`0600`), and `bin/agy-companion[.exe]`.
- **Sidecar logs (Antigravity App):** `~/.gemini/antigravity/sidecar_data/agy-companion/proxy/logs/`.
- **Diagnose issues:**
  1. Run `agy-companion doctor` (exits non-zero on config permission errors, unreachable proxy, or version/protocol skew) and `agy-companion integration status`.
  2. If config permissions fail on macOS/Linux, run `chmod 600 ~/.config/agy-companion/config.json`.
  3. On symlinked or read-only dotfiles (Nix/Stow/Home Manager), `integration` returns `manual_required` with a 1-line static snippet; CLI users can opt in explicitly via `agy-companion integration enable --follow-rc-symlink`.
  4. If a non-Gemini-3/4 tool call inside an agent subshell returns `400`, run `unset GOOGLE_GEMINI_BASE_URL` in that command to reach the Gemini API directly.
  5. After changing `listen_port` or upgrading, quit Antigravity completely (`Cmd+Q` on macOS) and reopen, or re-run `npx -y agy-companion@latest install`. To remove cleanly, run `agy-companion uninstall` (add `--purge` to delete config and usage database).
