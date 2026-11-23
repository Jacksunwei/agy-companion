#![forbid(unsafe_code)]

pub mod server;

pub use server::{
    CompanionUiServer, DEFAULT_PROBE_TIMEOUT_SECS, EMBEDDED_UI_HTML, MAX_UI_HTML_BYTES,
    ProxyHealthPayload, ProxyStatsPayload, SaveSettingsArgs, SettingsSnapshot,
    TestConnectionPayload, UI_HTML_ENV_VAR, UI_MIME_TYPE, UI_RESOURCE_URI, UiMcpError, serve_stdio,
};
