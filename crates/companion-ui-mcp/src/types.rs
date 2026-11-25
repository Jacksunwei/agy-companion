use std::fmt;
use std::path::PathBuf;
use std::time::Duration;

use companion_core::config::{
    ApiKeyPatch as CoreApiKeyPatch, CloudCodeConfigPatch, ConfigPatch, GeminiConfigPatch,
    ProvidersConfigPatch, ProxyConfigPatch,
};
use companion_core::integration::{IntegrationOptions, IntegrationStatus};
use companion_core::{
    CLOUDCODE_DAILY_UPSTREAM_URL, CLOUDCODE_PROD_UPSTREAM_URL, CloudCodeStatsSnapshot,
    CloudCodeStatus, CloudCodeUpstream, DEFAULT_CLOUDCODE_UPSTREAM_URL, HostPlatform, ModelPreset,
    ProxyHealthSummary, RedactedConfig, StatsSnapshot, UsageRange, api_key_hint,
    curated_model_presets,
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
    pub auto_enable_launcher: bool,
}

impl Default for UiServerOptions {
    fn default() -> Self {
        Self {
            ui_html_source: UiHtmlSource::ProcessEnv,
            env_api_key_fallback: true,
            test_connection_timeout_override: None,
            integration_options_override: None,
            probe_timeout: DEFAULT_PROBE_TIMEOUT,
            auto_enable_launcher: false,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "lowercase")]
pub enum AccountPlan {
    Auto,
    Personal,
    Organization,
}

impl AccountPlan {
    pub const fn to_upstream_str(self) -> &'static str {
        match self {
            Self::Auto => DEFAULT_CLOUDCODE_UPSTREAM_URL,
            Self::Personal => CLOUDCODE_DAILY_UPSTREAM_URL,
            Self::Organization => CLOUDCODE_PROD_UPSTREAM_URL,
        }
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum AccountPlanMode {
    #[default]
    Auto,
    Personal,
    Organization,
    Custom,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ResolvedAccountPlan {
    Personal,
    Organization,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct AccountPlanStatus {
    pub mode: AccountPlanMode,
    pub resolved: Option<ResolvedAccountPlan>,
}

fn map_upstream_url_to_resolved_plan(raw: &str) -> Option<ResolvedAccountPlan> {
    let normalized = raw.trim().trim_end_matches('/');
    if normalized.eq_ignore_ascii_case(CLOUDCODE_DAILY_UPSTREAM_URL) {
        Some(ResolvedAccountPlan::Personal)
    } else if normalized.eq_ignore_ascii_case(CLOUDCODE_PROD_UPSTREAM_URL) {
        Some(ResolvedAccountPlan::Organization)
    } else {
        None
    }
}

impl AccountPlanStatus {
    pub fn from_config_and_stats(
        configured: &CloudCodeUpstream,
        stats: Option<&CloudCodeStatsSnapshot>,
    ) -> Self {
        match configured {
            CloudCodeUpstream::Auto => {
                let resolved = stats
                    .filter(|s| s.mode.trim().eq_ignore_ascii_case("auto"))
                    .and_then(|s| s.resolved_upstream.as_deref())
                    .and_then(map_upstream_url_to_resolved_plan);
                Self {
                    mode: AccountPlanMode::Auto,
                    resolved,
                }
            }
            CloudCodeUpstream::Fixed(url) => match map_upstream_url_to_resolved_plan(url) {
                Some(ResolvedAccountPlan::Personal) => Self {
                    mode: AccountPlanMode::Personal,
                    resolved: Some(ResolvedAccountPlan::Personal),
                },
                Some(ResolvedAccountPlan::Organization) => Self {
                    mode: AccountPlanMode::Organization,
                    resolved: Some(ResolvedAccountPlan::Organization),
                },
                None => Self {
                    mode: AccountPlanMode::Custom,
                    resolved: None,
                },
            },
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
    #[serde(default)]
    pub account_plan: AccountPlanStatus,
    #[serde(default)]
    pub proxy_rss_bytes: Option<u64>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub enum UsageRangeArg {
    #[serde(rename = "1d")]
    Today,
    #[serde(rename = "7d")]
    Days7,
    #[serde(rename = "30d")]
    Days30,
}

impl From<UsageRangeArg> for UsageRange {
    fn from(arg: UsageRangeArg) -> Self {
        match arg {
            UsageRangeArg::Today => Self::Today,
            UsageRangeArg::Days7 => Self::Days7,
            UsageRangeArg::Days30 => Self::Days30,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct GetUsageArgs {
    pub range: UsageRangeArg,
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
    pub enabled_models: Option<Vec<String>>,
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
    pub account_plan: Option<AccountPlan>,
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
            enabled_models: patch.enabled_models,
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

impl TryFrom<SaveSettingsArgs> for ConfigPatch {
    type Error = String;

    fn try_from(args: SaveSettingsArgs) -> Result<Self, Self::Error> {
        let has_legacy_upstream = args
            .cloudcode
            .as_ref()
            .and_then(|c| c.upstream.as_ref())
            .is_some();
        if args.account_plan.is_some() && has_legacy_upstream {
            return Err("cannot specify both account_plan and cloudcode.upstream".to_string());
        }

        let cloudcode = match args.account_plan {
            Some(plan) => Some(CloudCodeConfigPatch {
                upstream: Some(plan.to_upstream_str().to_string()),
            }),
            None => args.cloudcode.map(Into::into),
        };

        Ok(Self {
            proxy: args.proxy.map(Into::into),
            cloudcode,
            providers: args.providers.map(Into::into),
            launcher_auto_start: None,
        })
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TestConnectionPayload {
    pub ok: bool,
    pub message: String,
}
