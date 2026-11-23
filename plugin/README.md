# Antigravity Companion

Bring your own models (BYOM) and track token usage and implicit cache hit rates in Google Antigravity (`agy`) with a local Rust Gemini pass-through proxy sidecar and an embedded MCP App Settings UI.

## Features

- **Loopback-Only Rust Proxy Sidecar (`agy-companion serve`):** Listens exclusively on `127.0.0.1` (default port `4141`), forwards `/v1beta/models/*:generateContent` and `:streamGenerateContent` byte-for-byte with a 15-minute streaming timeout to preserve Gemini implicit context caching, authenticates upstream via `x-goog-api-key` (never `?key=` query strings), and hot-reloads config changes on file modification.
- **Embedded MCP App Settings UI (`agy-companion ui-mcp`):** Native Antigravity panel (`ui://companion/settings.html`) accessible from the side panel (`aux_pane`) and left sidebar **Plugins** view (`global`). Includes a live Proxy Status Bar, write-only Gemini Settings form with **Test Connection**, copyable `customModelsConfig` snippet, and live Usage & Cache Telemetry — with zero external network fetches and redacted API key handling.
- **Usage & Cache Observability:** Tees `usageMetadata` from unary JSON and SSE responses into `GET /api/stats` and append-only `0600` monthly JSONL logs (`YYYY-MM.jsonl`) containing only token counts and latency metadata (zero prompt/response bodies or secrets).
- **Zero-Prerequisite Launcher:** `scripts/run-companion.sh` resolves a local `agy-companion` binary or downloads a SHA-256-verified release binary on first run — no Node, Python, or Rust runtime required.

## Installation & Quick Setup

### 1. Install the Plugin

```bash
agy plugin install ./plugin
agy plugin list
```

*(You can also install from the GitHub repository URL or the Antigravity Marketplace once published.)*

### 2. Configure Your Gemini API Key

Open the Settings panel in Antigravity App via the side panel `+` menu (**AGY Companion**), the left sidebar **Plugins** view, or by asking the agent:

> "Open the Antigravity Companion settings panel"

In the panel:
1. Paste your Gemini API key (stored atomically at `0600` permissions — never returned in plaintext), or export `GEMINI_API_KEY` in your shell environment.
2. Adjust the default model (default: `gemini-2.5-pro`), upstream base URL (default: `https://generativelanguage.googleapis.com`), or loopback port (default: `4141`) if needed.
3. Click **Save Settings**, then **Test Connection**.

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
        "modelName": "gemini-api://http://127.0.0.1:4141/models/gemini-2.5-pro",
        "label": "AGY Companion (gemini-2.5-pro)"
      },
      {
        "modelName": "gemini-api://http://127.0.0.1:4141/models/gemini-2.5-flash",
        "label": "AGY Companion (gemini-2.5-flash)"
      }
    ]
  }
}
```

> **Note:** When `modelProvider` is `"gemini"`, `agy` CLI requires `GEMINI_API_KEY` to be non-empty in your shell environment at startup. If you already saved an API key in `~/.config/agy-companion/config.json`, the proxy uses your saved config key for upstream requests.

Alternatively, route `agy` CLI, IDE extensions, or SDKs through the proxy via environment variables:

```bash
export GOOGLE_GEMINI_BASE_URL="http://127.0.0.1:4141"
```

## Configuration & Data Paths

| Purpose | Precedence / Path | Notes |
| --- | --- | --- |
| **Config file** | `--config <path>` → `AGY_COMPANION_CONFIG` → `~/.config/agy-companion/config.json` | JSON (`schema_version: 1`), Unix mode `0600` (parent dir `0700`); symlinks rejected |
| **Usage JSONL logs** | `config.usage_dir` → `~/.local/share/agy-companion/usage/YYYY-MM.jsonl` | Append-only, Unix mode `0600`; metadata only (zero prompt text or keys) |
| **Sidecar logs** | `~/.gemini/antigravity/sidecar_data/agy-companion/proxy/logs/` | Managed by Antigravity App for the `proxy` sidecar |
| **Cached binary** | `${ANTIGRAVITY_EXECUTABLE_DATA_DIR:-~/.local/share/agy-companion}/bin/agy-companion` | Used by `scripts/run-companion.sh` when not on `$PATH` |

## CLI Reference

| Command | Description |
| --- | --- |
| `agy-companion serve [--port <PORT>] [--config <CONFIG>]` | Start the loopback proxy (`127.0.0.1:<port>`) |
| `agy-companion ui-mcp [--data-dir <DATA_DIR>] [--config <CONFIG>]` | Start the stdio MCP App server (stdout strictly JSON-RPC; logs to stderr) |
| `agy-companion doctor [--config <CONFIG>] [--port <PORT>] [--json]` | Verify config permissions, config schema, proxy `/health`, version match, and env vars |
| `agy-companion models [--json] [--port <PORT>] [--config <CONFIG>]` | Print `customModelsConfig` JSON for `~/.gemini/antigravity-cli/settings.json` |
| `agy-companion --version` | Print the installed binary version |

## Local Development Overrides

When developing from a local checkout:

- **`AGY_COMPANION_BIN`**: Point directly to a local executable binary (highest precedence in `scripts/run-companion.sh`; fails fast with an error if set to a non-executable path).
- **Linked dev checkout auto-detection**: If `./plugin` is symlinked into `~/.gemini/config/plugins/agy-companion`, `scripts/run-companion.sh` follows symlinks and automatically runs `<repo>/target/debug/agy-companion` or `<repo>/target/release/agy-companion` (preferring the newer build) without downloading from GitHub Releases.
- **`AGY_COMPANION_UI_HTML`**: Point `agy-companion ui-mcp` at a freshly built `ui/dist/index.html` file. The server re-reads the file on every `resources/read`, allowing UI rebuilds (`npm --prefix ui run build`) without recompiling Rust.

## Troubleshooting

1. **Run diagnostics:**
   ```bash
   agy-companion doctor
   ```
   Use `agy-companion doctor --json` for machine-readable output. The command exits non-zero if config permissions are insecure, config is invalid, the proxy is unreachable, or binary-vs-proxy version/protocol skew is detected.
2. **Fix config file permissions:**
   ```bash
   chmod 600 ~/.config/agy-companion/config.json
   ```
   Ensure the config path is a regular file (symbolic links are rejected).
3. **Probe the proxy directly:**
   ```bash
   curl -s http://127.0.0.1:4141/health
   curl -s http://127.0.0.1:4141/api/stats
   ```
4. **Inspect sidecar logs:** Check `~/.gemini/antigravity/sidecar_data/agy-companion/proxy/logs/` (or run `agy-companion serve` in a terminal to observe live stderr logs).
5. **Port changes & version skew:** Changes to `api_key`, `base_url`, and `default_model` hot-reload automatically via config mtime. Changing `listen_port` or upgrading the binary requires restarting the proxy sidecar (or Antigravity App).
6. **Reload the MCP App UI:** In Antigravity App, restart the `agy-companion_companion` server under **Settings > MCP Servers > Restart**.

## Security & Privacy

- Proxy binds strictly to loopback (`127.0.0.1` or `localhost`) and rejects non-loopback hosts.
- Remote upstream URLs require `https://` (`http://` is restricted to `127.0.0.1` and `localhost`).
- API keys are stored with mode `0600`, sent via `x-goog-api-key` (never `?key=`), never returned in plaintext by MCP tools (`get_settings` returns masked hints only), and never written to logs or usage JSONL files.
- Release binaries downloaded by `scripts/run-companion.sh` are verified against SHA-256 checksums before execution.
