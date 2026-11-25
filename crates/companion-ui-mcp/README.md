# `companion-ui-mcp`

Stdio MCP App server (`rmcp`) that exposes the **Antigravity Companion** settings UI (`aux_pane` and `global` entrypoints) and configuration tools to Antigravity.

## Modules

| File | Responsibility |
| --- | --- |
| `src/lib.rs` | Crate re-exports and `#![forbid(unsafe_code)]` enforcement |
| `src/server.rs` | MCP server implementation (`open_settings`, `get_settings`, strict nested `save_settings` w/ `ApiKeyPatch` `"clear" \| { "set": "..." }` & `#[serde(deny_unknown_fields)]`, `test_connection`, `set_launcher_integration`), `SettingsSnapshot` (nested `RedactedConfig`, structured `StatsSnapshot`, canonical `IntegrationStatus.state`, `supported_models: Vec<ModelPreset>`, `platform: HostPlatform`), embedded single-file UI resource (`ui/dist/index.html` + `AGY_COMPANION_UI_HTML` dev override), serialized config/launcher writes, and live proxy `/health` + `/api/stats` probing |
