#![forbid(unsafe_code)]

pub mod server;

pub use companion_core::protocol::{
    CloudCodeStatus, HostPlatform, ProxyHealthSummary, StatsSnapshot,
};
pub use server::{
    ApiKeyPatch, CloudCodeSettingsPatch, CompanionUiServer, DEFAULT_PROBE_TIMEOUT_SECS,
    EMBEDDED_UI_HTML, GeminiSettingsPatch, MAX_UI_HTML_BYTES, ProvidersSettingsPatch,
    ProxySettingsPatch, SaveSettingsArgs, SetLauncherIntegrationArgs, SettingsSnapshot,
    TestConnectionPayload, UI_HTML_ENV_VAR, UI_MIME_TYPE, UI_RESOURCE_URI, UiMcpError, serve_stdio,
};
