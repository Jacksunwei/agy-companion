---
name: companion
description: Open Antigravity Companion settings, configure customModelsConfig for the local Gemini proxy, inspect cache/usage telemetry, and diagnose setup issues with agy-companion doctor.
---

# Antigravity Companion (`agy-companion`)

Use this skill when the user wants to configure Bring Your Own Model (BYOM) Gemini routing in Google Antigravity, open the Antigravity Companion Settings panel, generate `customModelsConfig` for `agy` CLI, or troubleshoot the local proxy sidecar.

## 1. Open the Settings Panel (MCP App UI)

Invoke the bundled MCP tool **`open_settings`** (on server `agy-companion_companion`, serving `ui://companion/settings.html` in the `aux_pane` and `global` entrypoints).

- **Do not ask the user to paste plaintext API keys into chat** when the Settings UI is available. Direct them to enter the key in the write-only API key field inside the Settings panel (saved atomically — Unix mode `0600` / parent dir `0700`, Windows user-profile ACLs with bounded `3`-attempt rename retry) or export `GEMINI_API_KEY` in their environment.
- Available MCP tools on `agy-companion_companion`:
  - `open_settings` — launches the Settings & Proxy Telemetry panel (`aux_pane` + `global`) and returns a redacted status summary.
  - `get_settings` *(app-only)* — returns redacted config (`api_key_configured`, masked `api_key_hint`), proxy `/health`, version skew status, and `/api/stats` telemetry. Never returns plaintext keys.
  - `save_settings` *(app-only)* — validates `api_key`, `clear_api_key`, `default_model`, `base_url`, `listen_port`, and `listen_host` (loopback only), then persists `config.json` atomically (Unix mode `0600` / Windows user-profile ACLs). Leaving `api_key` blank preserves the existing key unless `clear_api_key: true` is passed.
  - `test_connection` *(app-only)* — verifies upstream Gemini API reachability and key validity (`GET /v1beta/models?pageSize=1` via `x-goog-api-key`).

## 2. Configure Antigravity CLI (`customModelsConfig`)

Run `agy-companion models --json` (optionally `--port <PORT>` or `--config <CONFIG>`) to generate the ready-to-paste JSON snippet for `~/.gemini/antigravity-cli/settings.json`:

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

- When `modelProvider` is `"gemini"`, `agy` CLI requires `GEMINI_API_KEY` to be non-empty in the shell environment. If an API key is already saved in the `agy-companion` config file (e.g., `~/.config/agy-companion/config.json` on macOS/Linux or `%APPDATA%\agy-companion\config.json` on Windows), the proxy authenticates upstream with the saved key.
- Alternatively, route CLI, IDE extensions, or SDK requests via environment variables:
  ```bash
  export GOOGLE_GEMINI_BASE_URL="http://127.0.0.1:4141"
  ```

## 3. CLI Subcommands & Endpoints

| Command | Purpose |
| --- | --- |
| `agy-companion serve [--port <PORT>] [--config <CONFIG>]` | Run the loopback proxy (`127.0.0.1:4141` by default) with config mtime hot-reload |
| `agy-companion ui-mcp [--data-dir <DATA_DIR>] [--config <CONFIG>]` | Run the stdio MCP App server (stdout strictly JSON-RPC; logs to stderr) |
| `agy-companion doctor [--config <CONFIG>] [--port <PORT>] [--json]` | Check config permissions (Unix `0600`; skipped on Windows), config validity, proxy `/health`, version/protocol skew, and env vars |
| `agy-companion models [--json] [--port <PORT>] [--config <CONFIG>]` | Emit `customModelsConfig` JSON for `~/.gemini/antigravity-cli/settings.json` |
| `agy-companion --version` | Print the binary version (`agy-companion 0.1.0`) |

Local proxy HTTP endpoints (`127.0.0.1:4141`):
- `GET /health` — `{"status":"ok","version":"0.1.0","protocol_version":1}`
- `GET /api/stats` — session request count, token totals, and `cache_hit_ratio`
- `POST /v1beta/models/{model}:generateContent` & `:streamGenerateContent?alt=sse` — byte-for-byte streaming pass-through

## 4. Config, Usage & Troubleshooting

- **Config file:** `--config <path>` → `AGY_COMPANION_CONFIG` → `$XDG_CONFIG_HOME/agy-companion/config.json` *(when set, non-empty, and absolute)* → platform default:
  - **macOS & Linux:** `~/.config/agy-companion/config.json` *(intentional XDG default on macOS — no `~/Library/Application Support/` drift)*
  - **Windows:** `%APPDATA%\agy-companion\config.json` *(fallback: `%USERPROFILE%\.config\agy-companion\config.json`)*
  - Saved atomically (Unix mode `0600` / parent dir `0700`; Windows user-profile ACLs with bounded `3`-attempt `25ms` → `50ms` retry on transient rename `PermissionDenied`). Symlinks, `..` traversal, and NUL bytes are rejected fail-closed; unset, empty/whitespace, or benign relative `XDG_CONFIG_HOME` values fall back to the platform default.
- **Usage JSONL logs:** `config.usage_dir` → `$XDG_DATA_HOME/agy-companion/usage` *(when set, non-empty, and absolute)* → platform default (appends `YYYY-MM.jsonl`):
  - **macOS & Linux:** `~/.local/share/agy-companion/usage`
  - **Windows:** `%LOCALAPPDATA%\agy-companion\usage` *(fallback: `%USERPROFILE%\.local\share\agy-companion\usage`)*
  - Unix mode `0600`; token & latency metadata only (zero prompt/response bodies or secrets). `..` traversal and NUL bytes are rejected fail-closed; unset, empty/whitespace, or benign relative `XDG_DATA_HOME` values fall back to the platform default.
- **Sidecar logs (Antigravity App):** `~/.gemini/antigravity/sidecar_data/agy-companion/proxy/logs/`.
- **Diagnose issues:**
  1. Run `agy-companion doctor` (exits non-zero on config permission errors, unreachable proxy, or version/protocol skew).
  2. If config permissions fail on macOS/Linux, run `chmod 600 ~/.config/agy-companion/config.json` (substitute `$XDG_CONFIG_HOME/agy-companion/config.json` or `$AGY_COMPANION_CONFIG` if overridden; on Windows, Unix mode checks are skipped and user-profile ACLs apply).
  3. After changing `listen_port`, restart the proxy sidecar (API key, base URL, and default model hot-reload automatically).
  4. On version skew, restart the proxy sidecar or Antigravity App so `/health` matches `agy-companion --version`.
