# Antigravity Companion

Bring your own Gemini models (BYOM) and monitor live session context usage, cache hit health, and multi-day token/cost trends in Google Antigravity (`agy`) — for both built-in Antigravity models and your own Google AI Studio API keys — via a loopback-only Rust proxy sidecar and a single-column 3-tab Sidecar Web UI (**Monitor** | **Usage** | **Settings**).

## Features

- **One-Command Install, Upgrade & Clean Uninstall:** Install the embedded 6-file plugin, stable native binary, Dock/Launcher integration, and background loopback proxy in a single command (`npx -y agy-companion@latest install`), upgrade by re-running the same command, and remove cleanly with `npx -y agy-companion@latest uninstall [--purge]`.
- **First-Run Local Proxy Auto-Enable & Native Dock / Launcher Integration:** When started by Antigravity (`serve --auto-enable-launcher` in the sidecar manifest), the plugin automatically enables the local proxy hook once on macOS and Linux — writing an owned `0600` zero-`eval` `env.sh` + 1-line login-shell hook (`~/.zlogin`, `~/.profile` / existing Bash login profile, or `fish/conf.d/agy-companion.fish`) serialized via `<data_dir>/integration.lock` (`0600`). It keeps the local proxy ready before Antigravity's backend starts while isolating MCP servers and integrated terminals (`${VAR+x}` 3-state scrub), never follows symlinked RC files (`follow_rc_symlink: false`), and permanently respects explicit opt-outs (**Disable proxy**, `install --no-launcher-hook`, or `integration disable`) persisted via `launcher_auto_start` in `config.json`. On Windows, `install` writes direct-`.exe` plugin manifests (zero `bash` dependency) and creates a reversible Start Menu shortcut (`Antigravity (Companion).lnk`).
- **Automatic Antigravity Plan Detection:** **Antigravity plan** defaults to **Auto**, automatically distinguishing **Personal** (Google AI Pro or personal Google accounts) from **Organization** (managed organization, Workspace, or PayGo plans) across accounts with an optional manual override under **Settings > Advanced**. On Day-0 (before a Google AI Studio API key is saved), Antigravity's native model picker is returned untouched while **Monitor** and **Usage** still track built-in Antigravity models passively.
- **Loopback-Only Rust Proxy Sidecar (`agy-companion serve`):** Listens exclusively on `127.0.0.1` (default port `24141`, with fresh-install `24141..=24150` fallback), forwards `/v1beta/models/*:generateContent` and `:streamGenerateContent` byte-for-byte to preserve Gemini implicit context caching, authenticates upstream via `x-goog-api-key` (never `?key=`), and supports `--adopt-running` (`500ms` takeover), `--auto-enable-launcher`, and `--idle-shutdown-secs`.
- **Single-Column 3-Tab Sidecar Web UI (`SIDECAR_UI_ENTRYPOINT_AUX_PANE`):** Native centered (`max-width: 560px`) Antigravity auxiliary panel served by the sidecar over loopback (`127.0.0.1:$ANTIGRAVITY_SIDECAR_WEB_PORT`) with strict CSP, constant-time `X-Sidecar-Token` authentication, redacted API key handling, and visibility-aware automatic background sync (no manual Refresh button):
  - **Monitor** — live active-session card with latest prompt context/cache bar, call health dot strip, 3 metric tiles (`Session cache`, `Input`, `Output`), estimated session list price, and per-model breakdown. Works for built-in Antigravity models even without a Google AI Studio API key (idle states never ask for an API key or show an `Open Settings` button).
  - **Usage** — multi-day historical analytics (`Today` | `7 days` | `30 days`, `$` | `Tokens` toggle, hero total + cache savings, stacked daily bar chart, and `Model` | `Provider` breakdown) backed by local read-only SQLite `usage.db` via `get_usage`.
  - **Settings** — **Model providers > Google AI Studio** card (**`Get API key ↗`**, write-only API key with **Save**, on-demand **Test**, inline 2-click **Clear**, and multi-select **`Enabled models`** chips) plus a collapsed **Advanced** section containing only **Antigravity plan** (`Auto` / `Personal` / `Organization`) and Unix-only right-aligned **Disable proxy** (2-click confirmation; when disabled, surfaces a persistent top **Local proxy is disabled** banner with one-click **Re-enable proxy**).
  - **Pinned Footer** — left **`Live`** pill opening a strictly health-only **Proxy health** popover (`Endpoint`, `Antigravity plan`, `Proxy / CLI`, `Memory`, and `Issue` when unhealthy) and a quiet right-side `XX MB RAM` readout when the proxy is reachable.
- **Passive Built-In & BYOM Usage Observability:** Passively tees uncompressed `usageMetadata` from both built-in Antigravity model pass-through calls (`provider = "antigravity"`, `source = "builtin"`, skipping compressed responses to preserve byte-for-byte streaming) and BYOM Gemini calls (`provider = "gemini"`, `source = "proxy"`) into `GET /api/stats` (including bounded `recent_calls`, `by_model`, and `proxy_rss_bytes`) and a WAL-mode SQLite database (`usage.db`, Unix mode `0600`) containing only token counts and latency metadata (zero prompt/response bodies or secrets).

## Installation & Quick Setup

### 1. One-Command Install (macOS, Linux & Windows)

```bash
# Recommended one-command install (Node >= 18)
npx -y agy-companion@latest install

# Or install globally via npm
npm i -g agy-companion && agy-companion install

# Or via Python / uvx
uvx --refresh agy-companion install
```

- **Upgrade:** Re-run `npx -y agy-companion@latest install` (or `npm i -g agy-companion@latest && agy-companion install`).
- **Uninstall (preserves `config.json`, `auth/`, & `usage/`):** `npx -y agy-companion@latest uninstall` (or `agy-companion uninstall`).
- **Full purge (also removes `config.json`, `auth/`, `usage/`, `proxy.log`, and `sidecar_data/agy-companion`):** `npx -y agy-companion@latest uninstall --purge` (or `agy-companion uninstall --purge`). Never touches `~/.gemini/config/config.json`.
- **Manual / Local Plugin Install:** You can also install the plugin bundle directly with `agy plugin install ./plugin && agy plugin list` (or `./scripts/dev-install.sh` from a source checkout).

After installing or re-enabling the local proxy hook, reopen Antigravity completely so it picks up the local proxy:
- **macOS:** Quit (`⌘Q`) and reopen Antigravity (closing the window is not enough).
- **Linux:** Quit and reopen Antigravity from your desktop launcher.
- **Windows:** Close Antigravity and launch **Antigravity (Companion)** from the Start Menu.

### 2. Configure Your Google AI Studio API Key & Enabled Models

Open the panel in Antigravity App via the side panel `+` menu (**Antigravity Companion**), the left sidebar **Plugins** view, or by asking the agent:

> "Open the Antigravity Companion settings panel"

The panel lands on **Monitor** automatically once Antigravity is connected or an API key is configured, and on **Settings** on Day-0 before connection and key setup. In **Settings**:

1. Click **`Get API key ↗`** (`https://aistudio.google.com/api-keys`), paste your Google AI Studio API key into the write-only field (stored atomically — Unix mode `0600` / parent dir `0700`, Windows user-profile ACLs with bounded `3`-attempt rename retry — never returned in plaintext), or export `GEMINI_API_KEY` in your shell environment.
2. Click **Save**, then click **Test** to verify upstream connectivity and authentication. Use **Clear** (inline 2-click confirmation) to remove a saved key.
3. Toggle **`Enabled models`** chips to control which Gemini models appear in Antigravity's native model picker (persisted and hot-reloaded immediately):
   - **Enabled by default:** **`4 Carbon`** (`gemini-4-carbon`), **`4 Neon`** (`gemini-4-neon`), **`4 Hydrogen`** (`gemini-4-hydrogen`).
   - **Optional (off by default):** **`4 Argon`** (`gemini-4-argon`), **`3.8 Flash`** (`gemini-3.8-flash`).
   - Unchecking all models pauses injection non-destructively and shows a one-click **`Restore defaults`** button.
4. Expand **Advanced** only if you need to override **Antigravity plan** (`Auto` / `Personal` / `Organization`) or disable/re-enable the **Local proxy** hook on macOS/Linux (or run `agy-companion launch` directly at any time to launch Antigravity App in Hybrid Mode).

> **Day-0 Behavior:** Until you save a real Google AI Studio API key, `agy-companion` leaves Antigravity's native model picker completely untouched — zero `(Proxy)` models appear and the local `"agy-companion-local"` placeholder is quarantined so it never leaks upstream — while **Monitor** and **Usage** still track your built-in Antigravity models passively.

### 3. Connect Antigravity CLI (`agy`)

Generate a ready-to-paste `customModelsConfig` snippet from the CLI:

```bash
agy-companion models --json
```

Paste the output into `~/.gemini/antigravity-cli/settings.json`:

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

> **Note:** When `modelProvider` is `"gemini"`, `agy` CLI requires `GEMINI_API_KEY` to be non-empty in your shell environment at startup. If you already saved an API key in your `agy-companion` config file, the proxy uses your saved config key for upstream requests.

Alternatively, route `agy` CLI, IDE extensions, or SDKs through the proxy via environment variables:

```bash
export GOOGLE_GEMINI_BASE_URL="http://127.0.0.1:24141"
```

## Configuration & Data Paths

| Purpose | Precedence / Path | Notes |
| --- | --- | --- |
| **Config & auth files** | `--config <path>` → `AGY_COMPANION_CONFIG` → `$XDG_CONFIG_HOME/agy-companion/config.json` → Platform default (with sibling `0700`/`0600` `auth/gemini.json`) | Non-secret `config.json` (`schema_version: 1`, `providers.gemini.enabled_models`, tri-state `launcher_auto_start`) + sibling `auth/gemini.json` (`AuthStore`); atomic temp+rename (Unix `0600`, parent/`auth/` `0700`; Windows bounded `3`-attempt `25ms` → `50ms` retry on transient `PermissionDenied`); symlinks, NUL bytes, and `..` traversal rejected |
| **Usage database** | `config.usage_dir` → `$XDG_DATA_HOME/agy-companion/usage` → Platform default (stores `usage.db`) | WAL-mode SQLite, Unix mode `0600` (`usage.db`, `-wal`, `-shm`); metadata only for built-in + BYOM requests (zero prompt text or keys); symlinks, FIFOs, NUL bytes, and `..` traversal rejected |
| **Launcher hook & daemon files** | `$XDG_DATA_HOME/agy-companion/{env.sh,integration.lock,proxy.pid,proxy.log}` (default `~/.local/share/agy-companion/`) | `0600` zero-`eval` `env.sh`, `0600` cross-process `integration.lock`, structured `proxy.pid`, and rotating `5 MiB` `proxy.log` |
| **Sidecar logs** | `~/.gemini/antigravity/sidecar_data/agy-companion/proxy/logs/` | Managed by Antigravity App for the `proxy` sidecar |
| **Cached / installed binary** | `${ANTIGRAVITY_EXECUTABLE_DATA_DIR:-${XDG_DATA_HOME:-~/.local/share}/agy-companion}/bin/agy-companion` | Version-matched cache checked **before** unversioned `$PATH` in `scripts/run-companion.sh` |

**Platform Defaults:**

| Platform | Default Config File | Default Data / Usage Directory |
| --- | --- | --- |
| **macOS & Linux** | `~/.config/agy-companion/config.json` *(no `~/Library/Application Support/` drift on macOS)* | `~/.local/share/agy-companion` (`usage/` inside) |
| **Windows** | `%APPDATA%\agy-companion\config.json` *(fallback: `%USERPROFILE%\.config\agy-companion\config.json`)* | `%LOCALAPPDATA%\agy-companion` (`usage/` inside; fallback: `%USERPROFILE%\.local\share\agy-companion`) |

> **XDG Environment Variables:** Per the XDG Base Directory Specification, `XDG_CONFIG_HOME` and `XDG_DATA_HOME` are applied only when set, non-empty, and absolute. Unset, empty/whitespace, or benign relative values are ignored and fall back to the platform default; any value containing `..` traversal or NUL bytes is rejected fail-closed.

## CLI Reference

| Command | Description |
| --- | --- |
| `agy-companion install [--config <CONFIG>] [--plugins-dir <DIR>] [--no-launcher-hook] [--no-start-proxy] [--force] [--json]` | Install embedded 6-file plugin, stable binary, Dock/Launcher hook (or Windows `.lnk`), and local proxy; persists `launcher_auto_start` (`false` with `--no-launcher-hook`) |
| `agy-companion uninstall [--config <CONFIG>] [--plugins-dir <DIR>] [--purge] [--dry-run] [--json]` | Strip shell hooks, remove plugin & binary, stop verified proxy, preserve config/usage unless `--purge` |
| `agy-companion integration enable\|disable\|status [--shell <zsh\|bash\|fish>] [--rc-file <PATH>] [--binary <PATH>] [--follow-rc-symlink] [--config <CONFIG>] [--json]` | Manage macOS/Linux Dock & Launcher login-shell integration and persist `launcher_auto_start` under `integration.lock` |
| `agy-companion hook-env --ensure-running --print-port [--port <PORT>] [--config <CONFIG>] [--plugins-dir <DIR>]` | Fail-open login-shell helper ensuring daemon health and printing only the validated `u16` port |
| `agy-companion serve [--port <PORT>] [--config <CONFIG>] [--adopt-running] [--auto-enable-launcher] [--idle-shutdown-secs <SECS>]` | Start the loopback proxy (`127.0.0.1:<port>`) and Sidecar Web UI with optional sidecar adoption, first-run launcher auto-enable, and idle auto-exit |
| `agy-companion launch [--port <PORT>] [--config <CONFIG>] [--app <PATH>] [--print-env]` | Start proxy if needed and launch Antigravity App in Hybrid Mode |
| `agy-companion doctor [--config <CONFIG>] [--port <PORT>] [--json]` | Verify config permissions, config schema, proxy `/health`, version match, and env vars |
| `agy-companion models [--json] [--port <PORT>] [--config <CONFIG>]` | Print `customModelsConfig` JSON for `~/.gemini/antigravity-cli/settings.json` |
| `agy-companion --version` | Print the installed binary version |

## Local Development Overrides

When developing from a local checkout:

- **`AGY_COMPANION_BIN`**: Point directly to a local executable binary (highest precedence in `scripts/run-companion.sh`; fails fast with an error if set to a non-executable path).
- **Linked dev checkout auto-detection**: If `./plugin` is symlinked into `~/.gemini/config/plugins/agy-companion`, `scripts/run-companion.sh` follows symlinks and automatically runs `<repo>/target/debug/agy-companion` or `<repo>/target/release/agy-companion` (preferring the newer build) without downloading from GitHub Releases. `scripts/run-companion.sh` scrubs inherited proxy env vars before `exec`ing `serve`.
- **`AGY_COMPANION_UI_HTML`**: Point the Sidecar Web UI (`agy-companion serve`) at a freshly built `ui/dist/index.html` file. The server re-reads the file on every `GET /`, allowing UI rebuilds (`npm --prefix ui run build`) without recompiling Rust.

## Troubleshooting

1. **Run diagnostics:**
   ```bash
   agy-companion doctor
   ```
   Use `agy-companion doctor --json` for machine-readable output. Exits non-zero if config permissions are insecure, config is invalid, the proxy is unreachable, or binary-vs-proxy version/protocol skew is detected.
2. **Check Dock / Launcher integration status:**
   ```bash
   agy-companion integration status
   ```
   If your shell RC file is symlinked or read-only (Nix, Home Manager, Stow), `integration` reports `state: "manual_required"` and prints a single static line to add (or opt in explicitly with `agy-companion integration enable --follow-rc-symlink`).
3. **Fix config & auth permissions (macOS/Linux):**
   ```bash
   chmod 600 ~/.config/agy-companion/config.json
   chmod 700 ~/.config/agy-companion/auth && chmod 600 ~/.config/agy-companion/auth/gemini.json
   ```
   *(Substitute `$XDG_CONFIG_HOME/agy-companion/config.json` or `$AGY_COMPANION_CONFIG` if overridden. On Windows, Unix mode checks are skipped and user-profile ACLs apply.)*
4. **Non-Gemini-3.8+/4+ `400` in an agent subshell:** Run `unset GOOGLE_GEMINI_BASE_URL` in that command to reach the Gemini API directly without passing through the Gemini-3.8+/4+-only proxy.
5. **Probe the proxy directly:**
   ```bash
   curl -s http://127.0.0.1:24141/health
   curl -s http://127.0.0.1:24141/api/stats
   ```
6. **Inspect logs:** Check `~/.local/share/agy-companion/proxy.log` (Managed On-Demand Daemon) and `~/.gemini/antigravity/sidecar_data/agy-companion/proxy/logs/` (Antigravity sidecar).
7. **Port changes & upgrades:** Changes to `api_key` (in `auth/gemini.json`), `base_url`, `default_model`, and `enabled_models` hot-reload automatically via `config.json` and `auth/gemini.json` mtime. After changing `proxy.port` or upgrading, quit and reopen Antigravity completely (**macOS:** Quit `⌘Q` and reopen; **Linux:** quit and reopen; **Windows:** close and reopen from the **Antigravity (Companion)** Start Menu shortcut).

## Security & Privacy

- Proxy binds strictly to loopback (`127.0.0.1` or `localhost`) and rejects non-loopback hosts.
- Remote upstream URLs require `https://` (`http://` is restricted to `127.0.0.1` and `localhost`).
- API keys are stored atomically (Unix mode `0600`, parent dir `0700`; Windows user-profile ACLs with bounded `3`-attempt `25ms` → `50ms` retry on transient `PermissionDenied`), sent via `x-goog-api-key` (never `?key=`), never returned in plaintext by the Sidecar Web UI (`/_api/get_settings` returns masked hints only), and never written to logs or the SQLite usage database (`usage.db`).
- Release binaries downloaded by `scripts/run-companion.sh` are verified against SHA-256 checksums before execution.
