#![forbid(unsafe_code)]

pub mod server;
mod tools;
pub mod types;
mod ui_resource;

#[cfg(test)]
mod tests;

pub use companion_core::protocol::{
    CloudCodeStatus, HostPlatform, ProxyHealthSummary, StatsSnapshot,
};
pub use server::{CompanionUiServer, DEFAULT_PROBE_TIMEOUT_SECS, serve_stdio};
pub use types::{
    ApiKeyPatch, CloudCodeSettingsPatch, GeminiSettingsPatch, ProvidersSettingsPatch,
    ProxySettingsPatch, SaveSettingsArgs, SetLauncherIntegrationArgs, SettingsSnapshot,
    TestConnectionPayload, UiHtmlSource, UiMcpError, UiServerOptions,
};
pub use ui_resource::{
    EMBEDDED_LOGO_SVG, EMBEDDED_UI_HTML, MAX_UI_HTML_BYTES, UI_HTML_ENV_VAR, UI_MIME_TYPE,
    UI_RESOURCE_URI,
};
