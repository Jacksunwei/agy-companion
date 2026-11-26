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
pub use server::{CompanionUiServer, DEFAULT_PROBE_TIMEOUT_SECS};
pub use types::{
    AccountPlan, AccountPlanMode, AccountPlanStatus, ApiKeyPatch, CloudCodeSettingsPatch,
    GeminiSettingsPatch, GetUsageArgs, ProvidersSettingsPatch, ProxySettingsPatch,
    ResolvedAccountPlan, SaveSettingsArgs, SettingsSnapshot, TestConnectionPayload, UiError,
    UiHtmlSource, UiServerOptions, UsageRangeArg,
};
pub use ui_resource::{EMBEDDED_UI_HTML, MAX_UI_HTML_BYTES, UI_HTML_ENV_VAR};
