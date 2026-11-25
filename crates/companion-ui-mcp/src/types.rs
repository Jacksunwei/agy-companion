use std::fmt;
use std::path::PathBuf;
use std::time::Duration;

use companion_core::config::{
    ApiKeyPatch as CoreApiKeyPatch, CloudCodeConfigPatch, ConfigPatch, GeminiConfigPatch,
    ProvidersConfigPatch, ProxyConfigPatch,
};
use companion_core::integration::{IntegrationOptions, IntegrationStatus};
use companion_core::{
    CloudCodeStatus, HostPlatform, ModelPreset, ProxyHealthSummary, RedactedConfig, StatsSnapshot,
    api_key_hint, curated_model_presets,
};
use companion_providers::DEFAULT_PROBE_TIMEOUT;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use thiserror::Error;

#[derive(Debug, Error)]
pub enum UiMcpError {
    #[error("failed to initialize HTTP probe client: {0}")]
    HttpClient(String),
    #[error("MCP server initialization failed: {0}")]
    Initialize(String),
    #[error("MCP server task failed: {0}")]
    Join(#[from] tokio::task::JoinError),
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub enum UiHtmlSource {
    #[default]
    ProcessEnv,
    Fixed(Option<PathBuf>),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UiServerOptions {
    pub ui_html_source: UiHtmlSource,
    pub env_api_key_fallback: bool,
    pub test_connection_timeout_override: Option<u64>,
    pub integration_options_override: Option<IntegrationOptions>,
    pub probe_timeout: Duration,
}

impl Default for UiServerOptions {
    fn default() -> Self {
        Self {
            ui_html_source: UiHtmlSource::ProcessEnv,
            env_api_key_fallback: true,
            test_connection_timeout_override: None,
            integration_options_override: None,
            probe_timeout: DEFAULT_PROBE_TIMEOUT,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SettingsSnapshot {
    pub config: RedactedConfig,
    pub binary_version: String,
    pub proxy: ProxyHealthSummary,
    pub version_skew: bool,
    pub stats: Option<StatsSnapshot>,
    pub launcher_integration: IntegrationStatus,
    pub cloudcode: CloudCodeStatus,
    #[serde(default = "curated_model_presets")]
    pub supported_models: Vec<ModelPreset>,
    #[serde(default)]
    pub platform: HostPlatform,
}

#[derive(Debug, Clone, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct SetLauncherIntegrationArgs {
    pub enabled: bool,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize, JsonSchema)]
#[serde(default, deny_unknown_fields)]
pub struct ProxySettingsPatch {
    pub host: Option<String>,
    pub port: Option<u16>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize, JsonSchema)]
#[serde(default, deny_unknown_fields)]
pub struct CloudCodeSettingsPatch {
    pub upstream: Option<String>,
}

#[derive(Clone, PartialEq, Eq, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum ApiKeyPatch {
    Clear,
    Set(String),
}

impl fmt::Debug for ApiKeyPatch {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Clear => f.write_str("ApiKeyPatch::Clear"),
            Self::Set(raw) => f
                .debug_tuple("ApiKeyPatch::Set")
                .field(&api_key_hint(Some(raw)))
                .finish(),
        }
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize, JsonSchema)]
#[serde(default, deny_unknown_fields)]
pub struct GeminiSettingsPatch {
    pub api_key: Option<ApiKeyPatch>,
    pub base_url: Option<String>,
    pub default_model: Option<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize, JsonSchema)]
#[serde(default, deny_unknown_fields)]
pub struct ProvidersSettingsPatch {
    pub gemini: Option<GeminiSettingsPatch>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize, JsonSchema)]
#[serde(default, deny_unknown_fields)]
pub struct SaveSettingsArgs {
    pub proxy: Option<ProxySettingsPatch>,
    pub cloudcode: Option<CloudCodeSettingsPatch>,
    pub providers: Option<ProvidersSettingsPatch>,
}

impl From<ApiKeyPatch> for CoreApiKeyPatch {
    fn from(patch: ApiKeyPatch) -> Self {
        match patch {
            ApiKeyPatch::Clear => Self::Clear,
            ApiKeyPatch::Set(raw) => Self::Set(raw),
        }
    }
}

impl From<ProxySettingsPatch> for ProxyConfigPatch {
    fn from(patch: ProxySettingsPatch) -> Self {
        Self {
            host: patch.host,
            port: patch.port,
        }
    }
}

impl From<CloudCodeSettingsPatch> for CloudCodeConfigPatch {
    fn from(patch: CloudCodeSettingsPatch) -> Self {
        Self {
            upstream: patch.upstream,
        }
    }
}

impl From<GeminiSettingsPatch> for GeminiConfigPatch {
    fn from(patch: GeminiSettingsPatch) -> Self {
        Self {
            api_key: patch.api_key.map(Into::into),
            base_url: patch.base_url,
            default_model: patch.default_model,
            timeout_secs: None,
        }
    }
}

impl From<ProvidersSettingsPatch> for ProvidersConfigPatch {
    fn from(patch: ProvidersSettingsPatch) -> Self {
        Self {
            gemini: patch.gemini.map(Into::into),
        }
    }
}

impl From<SaveSettingsArgs> for ConfigPatch {
    fn from(args: SaveSettingsArgs) -> Self {
        Self {
            proxy: args.proxy.map(Into::into),
            cloudcode: args.cloudcode.map(Into::into),
            providers: args.providers.map(Into::into),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TestConnectionPayload {
    pub ok: bool,
    pub message: String,
}
