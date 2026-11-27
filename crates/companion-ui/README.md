# `companion-ui`

UI service crate powering the **Antigravity Companion** Sidecar Web UI (`sidecar_ui` in `companion-cli`).

| Module | Responsibility |
| --- | --- |
| `src/types.rs` | Wire DTOs (`SettingsSnapshot`, `SaveSettingsArgs` with `deny_unknown_fields` and config/auth patch splitting, `TestConnectionPayload`, `GetUsageArgs`, `AccountPlanStatus`, `UiServerOptions` with `auth_store_override`, `UiHtmlSource`) |
| `src/ui_resource.rs` | Embedded `ui/dist/index.html` (`include_str!`) and fail-closed `AGY_COMPANION_UI_HTML` override loader (`fs_safe::read_regular_file_utf8_bounded`, `10 MiB` cap, FIFO/TOCTOU guard) |
| `src/server.rs` | `CompanionUiServer` parallel loopback proxy health/session-stats + launcher integration snapshot builder wired to `AuthStore` (`FileAuthStore::for_config_path` or `auth_store_override`) |
| `src/tools.rs` | Async service operations (`get_settings_snapshot`, `save_settings_snapshot` with upfront validation & `AuthStore` rollback, `test_connection_payload`, `set_launcher_integration_snapshot`, `get_usage_summary`) with ordered write mutexes |
