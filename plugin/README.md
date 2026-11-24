# Antigravity Companion

Bring your own models (BYOM) and track token usage and implicit cache hit rates in Google Antigravity (`agy`) with a local Rust Gemini pass-through proxy sidecar, native Dock/Launcher auto-start, and an embedded MCP App Settings UI.

## Features

- **One-Command Install, Upgrade & Clean Uninstall:** Install the embedded 7-file plugin, stable native binary, Dock/Launcher integration, and background loopback proxy in a single command (`npx -y agy-companion@latest install`), upgrade by re-running the same command, and remove cleanly with `npx -y agy-companion@latest uninstall [--purge]`.
- **Native Dock / Spotlight / Launcher Auto-Start:** On macOS and Linux, an owned `0600` zero-`eval` `env.sh` + 1-line login-shell hook (`~/.zlogin`, `~/.profile` / existing Bash login profile, or `fish/conf.d/agy-companion.fish`) ensures the Managed On-Demand Daemon is warm before Antigravity's `language_server` boots — while isolating MCP servers (`JETSKI_MCP_ENV_RESTORE_*`) and integrated terminals (`${VAR+x}` 3-state scrub). On Windows, `install` writes direct-`.exe` plugin manifests (zero `bash` dependency) and creates a reversible Start Menu shortcut (`Antigravity (Companion).lnk`).
- **Google AI Pro, PayGo & Enterprise CloudCode Transparency:** `cloudcode_upstream_url` defaults to `"auto"`, using an 8-entry per-account 128-bit SipHash-1-3 LRU preserved across config hot-reloads to keep Personal Google AI Pro (`daily-cloudcode-pa.googleapis.com`), PayGo, and Enterprise (`cloudcode-pa.googleapis.com` with metadata-only `403` `x-goog-user-project` stripping) working seamlessly. On Day-0 (before an API key is saved), the native Google model picker is returned completely untouched.
- **Loopback-Only Rust Proxy Sidecar (`agy-companion serve`):** Listens exclusively on `127.0.0.1` (default port `4141`, with fresh-install `4141..=4150` fallback), forwards `/v1beta/models/*:generateContent` and `:streamGenerateContent` byte-for-byte to preserve Gemini implicit context caching, authenticates upstream via `x-goog-api-key` (never `?key=`), and supports `--adopt-running` (`500ms` takeover) and `--idle-shutdown-secs`.
- **Embedded MCP App Settings UI (`agy-companion ui-mcp`):** Native Antigravity panel (`ui://companion/settings.html`) accessible from the side panel (`aux_pane`) and left sidebar **Plugins** view (`global`). Includes a live Proxy Status Bar, CloudCode active-upstream badge, **Dock / Launcher Auto-Start** toggle (`set_launcher_integration`), write-only Gemini Settings form with post-save auto-verify and 5-minute model-refresh countdown, copyable `customModelsConfig` snippet, and live Usage & Cache Telemetry — with zero external network fetches and redacted API key handling.
- **Usage & Cache Observability:** Tees `usageMetadata` from unary JSON and SSE responses into `GET /api/stats` and append-only monthly JSONL logs (`YYYY-MM.jsonl`, Unix mode `0600`) containing only token counts and latency metadata (zero prompt/response bodies or secrets).

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
- **Uninstall (preserves `config.json` & `usage/`):** `npx -y agy-companion@latest uninstall` (or `agy-companion uninstall`).
- **Full purge (also removes `config.json`, `usage/`, `proxy.log`, and `sidecar_data/agy-companion`):** `npx -y agy-companion@latest uninstall --purge` (or `agy-companion uninstall --purge`). Never touches `~/.gemini/config/config.json`.
- **Manual / Local Plugin Install:** You can also install the plugin bundle directly with `agy plugin install ./plugin && agy plugin list` (or `./scripts/dev-install.sh` from a source checkout).

After installing, **quit Antigravity completely** (`Cmd+Q` on macOS — closing the window is not enough) and reopen it from Dock, Spotlight, or your desktop launcher (on Windows, launch **Antigravity (Companion)** from the Start Menu).

### 2. Configure Your Gemini API Key

Open the Settings panel in Antigravity App via the side panel `+` menu (**Antigravity Companion**), the left sidebar **Plugins** view, or by asking the agent:

> "Open the Antigravity Companion settings panel"

In the panel:
1. Paste your Gemini API key (stored atomically — Unix mode `0600` / parent dir `0700`, Windows user-profile ACLs with bounded `3`-attempt rename retry — never returned in plaintext), or export `GEMINI_API_KEY` in your shell environment.
2. Adjust the default model (Gemini 3 & 4 only; default: `gemini-4-hydrogen`), upstream base URL (default: `https://generativelanguage.googleapis.com`), or loopback port (default: `4141`) if needed.
3. Click **Save Settings**. The UI automatically verifies the key via `test_connection` and displays a live 5-minute model-refresh countdown (or quit with `Cmd+Q` and reopen Antigravity to load `(Proxy)` models immediately).
4. Use the **Dock / Launcher Auto-Start** card to toggle login-shell integration on macOS/Linux (or run `agy-companion launch` directly at any time to launch Antigravity App in Hybrid Mode).

> **Day-0 Behavior:** Until you save a real Gemini API key, `agy-companion` returns Antigravity's native `fetchAvailableModels` catalog completely untouched — zero `(Proxy)` models appear and the local `"agy-companion-local"` placeholder is quarantined so it never leaks upstream.

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
        "modelName": "gemini-api://http://127.0.0.1:4141/models/gemini-4-hydrogen",
        "label": "Antigravity Companion (gemini-4-hydrogen)"
      },
      {
        "modelName": "gemini-api://http://127.0.0.1:4141/models/gemini-4-argon",
        "label": "Antigravity Companion (gemini-4-argon)"
      },
      {
        "modelName": "gemini-api://http://127.0.0.1:4141/models/gemini-4-neon",
        "label": "Antigravity Companion (gemini-4-neon)"
      },
      {
        "modelName": "gemini-api://http://127.0.0.1:4141/models/gemini-4-carbon",
        "label": "Antigravity Companion (gemini-4-carbon)"
      },
      {
        "modelName": "gemini-api://http://127.0.0.1:4141/models/gemini-3.8-flash",
        "label": "Antigravity Companion (gemini-3.8-flash)"
      }
    ]
  }
}
```

> **Note:** When `modelProvider` is `"gemini"`, `agy` CLI requires `GEMINI_API_KEY` to be non-empty in your shell environment at startup. If you already saved an API key in your `agy-companion` config file, the proxy uses your saved config key for upstream requests.

Alternatively, route `agy` CLI, IDE extensions, or SDKs through the proxy via environment variables:

```bash
export GOOGLE_GEMINI_BASE_URL="http://127.0.0.1:4141"
```

## Configuration & Data Paths

| Purpose | Precedence / Path | Notes |
| --- | --- | --- |
| **Config file** | `--config <path>` → `AGY_COMPANION_CONFIG` → `$XDG_CONFIG_HOME/agy-companion/config.json` → Platform default | JSON (`schema_version: 1`); atomic temp+rename (Unix `0600`, parent `0700`; Windows bounded `3`-attempt `25ms` → `50ms` retry on transient `PermissionDenied`); symlinks, NUL bytes, and `..` traversal rejected |
| **Usage JSONL logs** | `config.usage_dir` → `$XDG_DATA_HOME/agy-companion/usage` → Platform default (writes `YYYY-MM.jsonl`) | Append-only, Unix mode `0600`; metadata only (zero prompt text or keys); NUL bytes and `..` traversal rejected |
| **Launcher hook & daemon files** | `$XDG_DATA_HOME/agy-companion/{env.sh,proxy.pid,proxy.log}` (default `~/.local/share/agy-companion/`) | `0600` zero-`eval` `env.sh`, structured `proxy.pid`, and rotating `5 MiB` `proxy.log` |
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
| `agy-companion install [--plugins-dir <DIR>] [--no-launcher-hook] [--no-start-proxy] [--force] [--json]` | Install embedded 7-file plugin, stable binary, Dock/Launcher hook (or Windows `.lnk`), and local proxy |
| `agy-companion uninstall [--plugins-dir <DIR>] [--purge] [--dry-run] [--json]` | Strip shell hooks, remove plugin & binary, stop verified proxy, preserve config/usage unless `--purge` |
| `agy-companion integration enable\|disable\|status [--shell <zsh\|bash\|fish>] [--rc-file <PATH>] [--binary <PATH>] [--follow-rc-symlink] [--json]` | Manage macOS/Linux Dock & Launcher login-shell integration |
| `agy-companion hook-env --ensure-running --print-port [--port <PORT>] [--config <CONFIG>] [--plugins-dir <DIR>]` | Fail-open login-shell helper ensuring daemon health and printing only the validated `u16` port |
| `agy-companion serve [--port <PORT>] [--config <CONFIG>] [--adopt-running] [--idle-shutdown-secs <SECS>]` | Start the loopback proxy (`127.0.0.1:<port>`) with optional sidecar adoption and idle auto-exit |
| `agy-companion launch [--port <PORT>] [--config <CONFIG>] [--app <PATH>] [--print-env]` | Start proxy if needed and launch Antigravity App in Hybrid Mode |
| `agy-companion ui-mcp [--data-dir <DATA_DIR>] [--config <CONFIG>]` | Start the stdio MCP App server (stdout strictly JSON-RPC; logs to stderr) |
| `agy-companion doctor [--config <CONFIG>] [--port <PORT>] [--json]` | Verify config permissions, config schema, proxy `/health`, version match, and env vars |
| `agy-companion models [--json] [--port <PORT>] [--config <CONFIG>]` | Print `customModelsConfig` JSON for `~/.gemini/antigravity-cli/settings.json` |
| `agy-companion --version` | Print the installed binary version |

## Local Development Overrides

When developing from a local checkout:

- **`AGY_COMPANION_BIN`**: Point directly to a local executable binary (highest precedence in `scripts/run-companion.sh`; fails fast with an error if set to a non-executable path).
- **Linked dev checkout auto-detection**: If `./plugin` is symlinked into `~/.gemini/config/plugins/agy-companion`, `scripts/run-companion.sh` follows symlinks and automatically runs `<repo>/target/debug/agy-companion` or `<repo>/target/release/agy-companion` (preferring the newer build) without downloading from GitHub Releases. `scripts/run-companion.sh` scrubs inherited proxy env vars before `exec`ing `ui-mcp` or `serve`.
- **`AGY_COMPANION_UI_HTML`**: Point `agy-companion ui-mcp` at a freshly built `ui/dist/index.html` file. The server re-reads the file on every `resources/read`, allowing UI rebuilds (`npm --prefix ui run build`) without recompiling Rust.

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
   If your shell RC file is symlinked or read-only (Nix, Home Manager, Stow), `integration` reports `manual_required` and prints a single static line to add (or opt in explicitly with `agy-companion integration enable --follow-rc-symlink`).
3. **Fix config file permissions (macOS/Linux):**
   ```bash
   chmod 600 ~/.config/agy-companion/config.json
   ```
   *(Substitute `$XDG_CONFIG_HOME/agy-companion/config.json` or `$AGY_COMPANION_CONFIG` if overridden. On Windows, Unix mode checks are skipped and user-profile ACLs apply.)*
4. **Non-Gemini-3/4 `400` in an agent subshell:** Run `unset GOOGLE_GEMINI_BASE_URL` in that command to reach the Gemini API directly without passing through the Gemini-3/4-only proxy.
5. **Probe the proxy directly:**
   ```bash
   curl -s http://127.0.0.1:4141/health
   curl -s http://127.0.0.1:4141/api/stats
   ```
6. **Inspect logs:** Check `~/.local/share/agy-companion/proxy.log` (Managed On-Demand Daemon) and `~/.gemini/antigravity/sidecar_data/agy-companion/proxy/logs/` (Antigravity sidecar).
7. **Port changes & version skew:** Changes to `api_key`, `base_url`, and `default_model` hot-reload automatically via config mtime. Changing `listen_port` or upgrading requires restarting the proxy sidecar (or quitting Antigravity with `Cmd+Q` and reopening).
8. **Reload the MCP App UI:** In Antigravity App, restart the `agy-companion_hub` server under **Settings > MCP Servers > Restart**.

## Security & Privacy

- Proxy binds strictly to loopback (`127.0.0.1` or `localhost`) and rejects non-loopback hosts.
- Remote upstream URLs require `https://` (`http://` is restricted to `127.0.0.1` and `localhost`).
- API keys are stored atomically (Unix mode `0600`, parent dir `0700`; Windows user-profile ACLs with bounded `3`-attempt `25ms` → `50ms` retry on transient `PermissionDenied`), sent via `x-goog-api-key` (never `?key=`), never returned in plaintext by MCP tools (`get_settings` returns masked hints only), and never written to logs or usage JSONL files.
- Release binaries downloaded by `scripts/run-companion.sh` are verified against SHA-256 checksums before execution.
