---
name: companion
description: Open Antigravity Companion (Monitor, Usage, and Settings), manage local proxy auto-start and install/uninstall, configure Google AI Studio enabled models and customModelsConfig, inspect cache/usage telemetry, and diagnose setup issues with agy-companion doctor.
---

# Antigravity Companion (`agy-companion`)

Use this skill when the user wants to install or uninstall Antigravity Companion, configure Bring Your Own Model (BYOM) Gemini routing or local proxy auto-start in Google Antigravity, open the Antigravity Companion panel (**Monitor**, **Usage**, and **Settings**), generate `customModelsConfig` for `agy` CLI, inspect live or multi-day usage/cache telemetry, or troubleshoot the local proxy sidecar.

## 1. Open Antigravity Companion (3-Tab MCP App UI)

Invoke the bundled MCP tool **`open_settings`** (on server `agy-companion_hub`, serving `ui://companion/settings.html` in the `aux_pane` and `global` entrypoints).

- **3-tab single-column layout (`max-width: 560px`):**
  - **Monitor** — live active-session context/cache bar, call health dot strip, 3 metric tiles (`Session cache`, `Input`, `Output`), approx list price, and per-model breakdown. Passively tracks **built-in Antigravity models even without a Google AI Studio API key**, alongside your own API keys (idle states never ask for an API key or show `Open Settings`).
  - **Usage** — historical SQLite analytics (`Today` | `7 days` | `30 days`, `$` | `Tokens` toggle, stacked daily bar chart, and `Model` | `Provider` breakdown via `get_usage`).
  - **Settings** — **Model providers > Google AI Studio** card (**`Get API key ↗`** to `https://aistudio.google.com/api-keys`, write-only API key with **Save**, on-demand **Test**, inline 2-click **Clear**, and multi-select **`Enabled models`** chips) plus collapsed **Advanced** containing only **Antigravity plan** (`Auto` / `Personal` / `Organization`) and Unix-only right-aligned **Disable proxy** (2-click confirmation; when disabled, shows a persistent top **Local proxy is disabled** banner with one-click **Re-enable proxy**).
  - **Pinned footer** — left **`Live`** pill with a strictly health-only **Proxy health** popover (`Endpoint`, `Antigravity plan`, `Proxy / CLI`, `Memory`, and `Issue` when unhealthy), quiet right-side `XX MB RAM` readout when reachable, and visibility-aware automatic background sync (no manual Refresh button).
- **Landing tab & Day-0 behavior:** Opens on **Monitor** when Antigravity is connected (`hybrid_active`) or an API key is configured, and on **Settings** on Day-0 before connection and key setup. Until a real Google AI Studio API key is saved, Antigravity's native model picker stays completely untouched (zero `(Proxy)` models injected, and `"agy-companion-local"` is quarantined so it never leaks upstream) while **Monitor** and **Usage** still track built-in Antigravity models passively.
- **Never ask for plaintext API keys in chat** when the panel is available. Direct the user to paste the key in **Settings > Google AI Studio** (saved atomically — Unix `0600` / parent `0700`, Windows ACLs with bounded `3`-attempt rename retry) or export `GEMINI_API_KEY`.
- **Default enabled models:** **`4 Carbon`** (`gemini-4-carbon`), **`4 Neon`** (`gemini-4-neon`), and **`4 Hydrogen`** (`gemini-4-hydrogen`) are ON by default; **`4 Argon`** (`gemini-4-argon`) and **`3.8 Flash`** (`gemini-3.8-flash`) are optional OFF. Toggling chips persists and hot-reloads immediately; unchecking all pauses injection non-destructively with a one-click **`Restore defaults`** button.
- Available MCP tools on `agy-companion_hub` (**6 tools**):
  - `open_settings` — opens the 3-tab panel (`aux_pane` + `global`), runs the locked first-run launcher auto-enable fallback when `ui-mcp --auto-enable-launcher` is active, and returns a redacted status summary.
  - `get_settings` *(app-only)* — returns nested redacted config (`proxy.{host, port}`, `launcher_auto_start`, `providers.gemini.{base_url, default_model, enabled_models, timeout_secs, api_key_configured, api_key_hint}`), `account_plan` (`mode`, `resolved`), `launcher_integration` status (`state` enum), `supported_models` curated presets, `platform`, proxy `/health`, version skew status, `proxy_rss_bytes`, and live `/api/stats` telemetry. Never returns plaintext keys.
  - `save_settings` *(app-only, `deny_unknown_fields`)* — validates nested patches `{ proxy?: { host?, port? }, account_plan?: "auto" | "personal" | "organization", providers?: { gemini?: { api_key?: { set: "..." } | "clear", base_url?, default_model?, enabled_models?: [...] } } }`, enforcing loopback `proxy.host`, port `1..=65535`, HTTPS/loopback URLs, and Gemini 3.8+/4+ model IDs, then persists `config.json` atomically (`0600`). Omitting `api_key` preserves the existing key; passing `"clear"` removes it.
  - `test_connection` *(app-only)* — verifies upstream Gemini API reachability and key validity (`GET /v1beta/models?pageSize=1` via `x-goog-api-key`).
  - `set_launcher_integration` *(app-only, `{ enabled: bool }`, `deny_unknown_fields`)* — enables or disables the macOS/Linux login-shell hook under `<data_dir>/integration.lock` (`0600`), persists `launcher_auto_start: true | false`, and never follows symlinked RC files (`state: "manual_required"` on Nix/Stow).
  - `get_usage` *(app-only, `{ range: "1d" | "7d" | "30d" }`, `deny_unknown_fields`)* — queries local read-only SQLite `usage.db` aggregates by day, model, and provider (covering both built-in Antigravity models and BYOM Gemini requests).

## 2. Install, First-Run Local Proxy Hook & `agy` CLI

- **One-Command Install / Upgrade / Uninstall (macOS, Linux & Windows):**
  - Install or upgrade: `npx -y agy-companion@latest install` (alternatives: `npm i -g agy-companion && agy-companion install` or `uvx --refresh agy-companion install`).
  - Uninstall: `npx -y agy-companion@latest uninstall [--purge]` (or `agy-companion uninstall [--purge]`). Default preserves `config.json` and `usage/`; `--purge` removes all companion data and never touches `~/.gemini/config/config.json`.
- **Antigravity App (First-Run Auto-Enable & Hybrid Mode):**
  - On macOS/Linux, the plugin sidecar (`serve --auto-enable-launcher`) and MCP fallback (`ui-mcp --auto-enable-launcher`) automatically enable the local proxy hook once on first run (`launcher_auto_start` tri-state in `config.json`, serialized via `<data_dir>/integration.lock` `0600`), writing `0600` zero-`eval` `env.sh` + a 1-line hook in `~/.zlogin` (Zsh), `~/.profile` / existing login profile (Bash), or `fish/conf.d/agy-companion.fish` (Fish) while isolating MCP servers and integrated terminals. Explicit opt-outs (**Disable proxy**, `install --no-launcher-hook`, or `integration disable`) persist `launcher_auto_start: false` and are never silently re-enabled.
  - After enabling or re-enabling, reopen Antigravity completely (**macOS:** Quit `⌘Q` and reopen; **Linux:** quit and reopen; **Windows:** close and launch **`Antigravity (Companion)`** from the Start Menu).
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
        "modelName": "gemini-api://http://127.0.0.1:24141/models/gemini-4-carbon",
        "label": "Antigravity Companion (gemini-4-carbon)"
      },
      {
        "modelName": "gemini-api://http://127.0.0.1:24141/models/gemini-4-neon",
        "label": "Antigravity Companion (gemini-4-neon)"
      },
      {
        "modelName": "gemini-api://http://127.0.0.1:24141/models/gemini-4-argon",
        "label": "Antigravity Companion (gemini-4-argon)"
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

## 3. CLI Subcommands & Local Proxy Endpoints

| Command | Purpose |
| --- | --- |
| `agy-companion install [--config <CONFIG>] [--plugins-dir <DIR>] [--no-launcher-hook] [--no-start-proxy] [--force] [--json]` | Install embedded 7-file plugin, stable binary, Dock/Launcher hook (or Windows `.lnk`), and local proxy; persists `launcher_auto_start` |
| `agy-companion uninstall [--config <CONFIG>] [--plugins-dir <DIR>] [--purge] [--dry-run] [--json]` | Strip shell RCs, delete `env.sh` and plugin before stopping verified proxy, remove binary (preserve config/usage unless `--purge`) |
| `agy-companion integration enable\|disable\|status [--shell <zsh\|bash\|fish>] [--rc-file <PATH>] [--binary <PATH>] [--follow-rc-symlink] [--config <CONFIG>] [--json]` | Manage macOS/Linux login-shell Dock/Launcher integration and persist `launcher_auto_start` under `integration.lock` |
| `agy-companion hook-env --ensure-running --print-port [--port <PORT>] [--config <CONFIG>] [--plugins-dir <DIR>]` | Fail-open login-shell hook helper: ensure Managed On-Demand Daemon is healthy and print validated `u16` port |
| `agy-companion serve [--port <PORT>] [--config <CONFIG>] [--adopt-running] [--auto-enable-launcher] [--idle-shutdown-secs <SECS>]` | Run the loopback proxy (`127.0.0.1:24141` by default) with config mtime hot-reload, sidecar adoption (`500ms` poll), optional first-run auto-enable, and idle shutdown |
| `agy-companion launch [--port <PORT>] [--config <CONFIG>] [--app <PATH>] [--print-env]` | Ensure proxy is running and launch Antigravity App in Hybrid Mode |
| `agy-companion ui-mcp [--data-dir <DATA_DIR>] [--config <CONFIG>] [--auto-enable-launcher]` | Run the stdio MCP App server (stdout strictly JSON-RPC; logs to stderr) with optional locked first-run launcher fallback |
| `agy-companion doctor [--config <CONFIG>] [--port <PORT>] [--json]` | Check config permissions (Unix `0600`; skipped on Windows), config validity, proxy `/health`, version/protocol skew, and env vars |
| `agy-companion models [--json] [--port <PORT>] [--config <CONFIG>]` | Emit `customModelsConfig` JSON for `~/.gemini/antigravity-cli/settings.json` |
| `agy-companion --version` | Print the binary version (`agy-companion 0.0.2`) |

Local proxy HTTP endpoints (`127.0.0.1:24141`):
- `GET /health` — `{"status":"ok","version":"0.0.2","protocol_version":1}`
- `GET /api/stats` — live session totals (`usage`: requests, prompt/cached/output/reasoning tokens, cache hit ratio), `proxy_rss_bytes`, bounded `recent_calls` ring buffer (max 32), `by_model` session rollups (max 32), and routing/catalog-refresh status.
- `POST /v1beta/models/{model}:generateContent` & `:streamGenerateContent?alt=sse` — byte-for-byte streaming pass-through for BYOM Gemini requests; built-in Antigravity pass-through calls are also passively teed for uncompressed `usageMetadata` into session stats and SQLite `usage.db`.

## 4. Config, Usage & Troubleshooting

- **Config file:** `--config <path>` → `AGY_COMPANION_CONFIG` → `$XDG_CONFIG_HOME/agy-companion/config.json` *(when set, non-empty, and absolute)* → platform default:
  - **macOS & Linux:** `~/.config/agy-companion/config.json` *(intentional XDG default on macOS — no `~/Library/Application Support/` drift)*
  - **Windows:** `%APPDATA%\agy-companion\config.json` *(fallback: `%USERPROFILE%\.config\agy-companion\config.json`)*
  - Saved atomically (Unix `0600` / parent `0700`; Windows ACLs with bounded `3`-attempt retry). Symlinks, `..` traversal, and NUL bytes are rejected fail-closed.
- **Data, usage & daemon files:** `$XDG_DATA_HOME/agy-companion/` *(platform default `~/.local/share/agy-companion/` on macOS/Linux; `%LOCALAPPDATA%\agy-companion\` on Windows)* stores `usage/usage.db` (SQLite WAL, `0600`, metadata only for built-in + BYOM models), `env.sh` (`0600`), `integration.lock` (`0600`), `proxy.pid` (`0600`), rotating `5 MiB` `proxy.log` (`0600`), and `bin/agy-companion[.exe]`.
- **Sidecar logs (Antigravity App):** `~/.gemini/antigravity/sidecar_data/agy-companion/proxy/logs/`.
- **Diagnose issues:**
  1. Run `agy-companion doctor` (exits non-zero on config permission errors, unreachable proxy, or version/protocol skew) and `agy-companion integration status`.
  2. If config permissions fail on macOS/Linux, run `chmod 600 ~/.config/agy-companion/config.json`.
  3. On symlinked or read-only dotfiles (Nix/Stow/Home Manager), `integration` returns `state: "manual_required"` with a 1-line static snippet; CLI users can opt in explicitly via `agy-companion integration enable --follow-rc-symlink`.
  4. If a non-Gemini-3.8+/4+ tool call inside an agent subshell returns `400`, run `unset GOOGLE_GEMINI_BASE_URL` in that command to reach the Gemini API directly.
  5. Changes to `api_key`, `base_url`, `default_model`, and `enabled_models` hot-reload automatically. After changing `proxy.port` or upgrading, quit and reopen Antigravity completely (**macOS:** Quit `⌘Q` and reopen; **Linux:** quit and reopen; **Windows:** close and launch **Antigravity (Companion)** from the Start Menu), or re-run `npx -y agy-companion@latest install`. To remove cleanly, run `agy-companion uninstall` (add `--purge` to delete config and usage database).
