# `companion-ui`

UI service crate powering the **Antigravity Companion** Sidecar Web UI (`sidecar_ui` in `companion-cli`).

| Module | Responsibility |
| --- | --- |
| `src/types.rs` | Wire DTOs (`SettingsSnapshot` incl. `supported_codex_models`, `codex_login_pending`, and optional `update: Option<UpdateStatusSnapshot>`, `UpdateLifecycleState`, `UpdateStatusSnapshot`, `UpdateStatusProvider`, `SaveSettingsArgs` with `deny_unknown_fields` and Gemini/Codex config+auth patch splitting, `TestConnectionArgs`/`TestConnectionProvider`/`TestConnectionPayload`, `StartCodexLoginPayload`, `CompleteCodexLoginArgs`, `GetUsageArgs`, `AccountPlanStatus`, `UiServerOptions` with `auth_store_override`, `codex_auth_override`, & `update_status_provider`, `UiHtmlSource`) |
| `src/ui_resource.rs` | Embedded `ui/dist/index.html` (`include_str!`) and fail-closed `AGY_COMPANION_UI_HTML` override loader (`fs_safe::read_regular_file_utf8_bounded`, `10 MiB` cap, FIFO/TOCTOU guard) |
| `src/server.rs` | `CompanionUiServer` parallel loopback proxy health/session-stats + launcher integration + `CodexAuthManager` pending-login snapshot builder wired to `AuthStore` (`FileAuthStore::for_config_path` or `auth_store_override`) |
| `src/tools.rs` | Async service operations (`get_settings_snapshot`, `save_settings_snapshot` with upfront validation & `AuthStore` rollback, provider-aware `test_connection_payload`, `start_codex_login`, `cancel_codex_login_snapshot`, `complete_codex_login_snapshot`, `disconnect_codex_snapshot`, `set_launcher_integration_snapshot`, `get_usage_summary`) with ordered write mutexes |
