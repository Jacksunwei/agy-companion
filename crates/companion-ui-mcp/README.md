# `companion-ui-mcp`

Stdio MCP App server (`rmcp`) that exposes the **Antigravity Companion** settings UI (`aux_pane` and `global` entrypoints) and configuration tools to Antigravity.

## Modules

| File | Responsibility |
| --- | --- |
| `src/lib.rs` | Crate re-exports and `#![forbid(unsafe_code)]` enforcement |
| `src/types.rs` | `UiMcpError`, `UiServerOptions` / `UiHtmlSource`, `SettingsSnapshot` (nested `RedactedConfig`, structured `StatsSnapshot`, canonical `IntegrationStatus.state`, `supported_models: Vec<ModelPreset>`, `platform: HostPlatform`), strict nested `SaveSettingsArgs` w/ `ApiKeyPatch` (`"clear" \| { "set": "..." }`) & `#[serde(default, deny_unknown_fields)]`, `SetLauncherIntegrationArgs`, and `TestConnectionPayload` |
| `src/ui_resource.rs` | Embedded single-file UI resource (`ui/dist/index.html`), `OnceLock`-cached `plugin/assets/logo.svg` base64 data URI, MCP App metadata helpers, and `AGY_COMPANION_UI_HTML` dev override via `companion_core::fs_safe::read_regular_file_utf8_bounded` (`SymlinkPolicy::Follow` + post-open `fstat` / Unix `(dev, ino)` TOCTOU verification) |
| `src/tools.rs` | `#[tool_router(vis = "pub(crate)")]` handlers for `open_settings`, `get_settings`, `save_settings`, `test_connection` (`isError: true` on local config load failures; `{ ok: false, message }` for valid-config upstream/key errors), and `set_launcher_integration` (validates local config before acquiring write lock or mutating dotfiles) |
| `src/server.rs` | `CompanionUiServer` (`::new` / `::with_options`), pooled `LoopbackProxyClient` `/health` + `/api/stats` probing with `evaluate_health` (version + protocol skew), `spawn_blocking` config loading, `tokio::join!` parallel snapshot assembly, `ServerHandler`, and `serve_stdio` |
| `src/tests.rs` | Unit and duplex MCP JSON-RPC protocol tests |
