use std::fmt;
use std::fs::File;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use companion_core::config::{
    ApiKeyPatch as CoreApiKeyPatch, CloudCodeConfigPatch, ConfigPatch, GeminiConfigPatch,
    ProvidersConfigPatch, ProxyConfigPatch,
};
use companion_core::integration::{IntegrationOptions, IntegrationState, IntegrationStatus};
use companion_core::protocol::HealthResponse;
use companion_core::{
    CloudCodeStatus, Config, GeminiConfig, HostPlatform, ModelPreset, ProxyHealthSummary,
    RedactedConfig, StatsSnapshot, api_key_hint, curated_model_presets, validate_path,
};
use companion_providers::{GeminiProvider, ProviderAdapter};
use rmcp::handler::server::router::tool::ToolRouter;
use rmcp::handler::server::wrapper::Parameters;
use rmcp::model::{
    CallToolResult, ContentBlock, ErrorData, Icon, Implementation, ListResourcesResult, MetaObject,
    PaginatedRequestParams, ReadResourceRequestParams, ReadResourceResponse, ReadResourceResult,
    Resource, ResourceContents, ServerCapabilities, ServerConfig,
};
use rmcp::service::RequestContext;
use rmcp::transport::io::stdio;
use rmcp::{RoleServer, ServerHandler, ServiceExt, tool, tool_handler, tool_router};
use schemars::JsonSchema;
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};
use thiserror::Error;

pub const UI_RESOURCE_URI: &str = "ui://companion/settings.html";
pub const UI_MIME_TYPE: &str = "text/html;profile=mcp-app";
pub const UI_HTML_ENV_VAR: &str = "AGY_COMPANION_UI_HTML";
pub const MAX_UI_HTML_BYTES: u64 = 10 * 1024 * 1024;
pub const DEFAULT_PROBE_TIMEOUT_SECS: u64 = 2;
pub const EMBEDDED_UI_HTML: &str = include_str!("../../../ui/dist/index.html");
pub const EMBEDDED_LOGO_SVG: &[u8] = include_bytes!("../../../plugin/assets/logo.svg");

const MAX_PROBE_BODY_BYTES: usize = 64 * 1024;
const MAX_TEST_CONNECTION_TIMEOUT_SECS: u64 = 15;

#[derive(Debug, Error)]
pub enum UiMcpError {
    #[error("failed to initialize HTTP probe client: {0}")]
    HttpClient(String),
    #[error("MCP server initialization failed: {0}")]
    Initialize(String),
    #[error("MCP server task failed: {0}")]
    Join(#[from] tokio::task::JoinError),
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

#[derive(Clone)]
enum UiHtmlSource {
    ProcessEnv,
    #[cfg(test)]
    Override(std::sync::Arc<std::sync::Mutex<Option<PathBuf>>>),
}

#[derive(Clone)]
pub struct CompanionUiServer {
    config_path: PathBuf,
    data_dir: Option<PathBuf>,
    proxy_client: reqwest::Client,
    config_write_lock: Arc<tokio::sync::Mutex<()>>,
    integration_write_lock: Arc<tokio::sync::Mutex<()>>,
    tool_router: ToolRouter<Self>,
    ui_html_source: UiHtmlSource,
    #[cfg(test)]
    env_api_key_fallback: bool,
    #[cfg(test)]
    test_connection_timeout_override: Option<u64>,
    #[cfg(test)]
    integration_options_override: Option<IntegrationOptions>,
}

impl CompanionUiServer {
    pub fn new(config_path: PathBuf, data_dir: Option<PathBuf>) -> Result<Self, UiMcpError> {
        let proxy_client = reqwest::Client::builder()
            .no_proxy()
            .redirect(reqwest::redirect::Policy::none())
            .timeout(Duration::from_secs(DEFAULT_PROBE_TIMEOUT_SECS))
            .connect_timeout(Duration::from_secs(DEFAULT_PROBE_TIMEOUT_SECS))
            .user_agent(concat!("agy-companion/", env!("CARGO_PKG_VERSION")))
            .build()
            .map_err(|err| UiMcpError::HttpClient(err.to_string()))?;

        Ok(Self {
            config_path,
            data_dir,
            proxy_client,
            config_write_lock: Arc::new(tokio::sync::Mutex::new(())),
            integration_write_lock: Arc::new(tokio::sync::Mutex::new(())),
            tool_router: Self::tool_router(),
            ui_html_source: UiHtmlSource::ProcessEnv,
            #[cfg(test)]
            env_api_key_fallback: true,
            #[cfg(test)]
            test_connection_timeout_override: None,
            #[cfg(test)]
            integration_options_override: None,
        })
    }

    pub fn config_path(&self) -> &Path {
        &self.config_path
    }

    pub fn data_dir(&self) -> Option<&Path> {
        self.data_dir.as_deref()
    }

    pub fn list_tool_definitions(&self) -> Vec<rmcp::model::Tool> {
        self.tool_router.list_all()
    }

    pub fn list_ui_resources(&self) -> ListResourcesResult {
        let resource = Resource::new(UI_RESOURCE_URI, "Antigravity Companion Settings")
            .with_title("Antigravity Companion Settings")
            .with_description(
                "Embedded MCP App UI for configuring Gemini API access and inspecting local proxy telemetry.",
            )
            .with_mime_type(UI_MIME_TYPE)
            .with_icons(companion_icons())
            .with_meta(ui_resource_meta());
        ListResourcesResult::with_all_items(vec![resource])
    }

    pub async fn read_ui_resource(&self, uri: &str) -> Result<ReadResourceResult, ErrorData> {
        if uri != UI_RESOURCE_URI {
            return Err(ErrorData::resource_not_found(
                format!("unknown resource: {uri}"),
                None,
            ));
        }

        let override_path = self.resolve_ui_html_override_path();
        let html = match override_path {
            Some(path) => load_ui_html_override(path).await?,
            None => EMBEDDED_UI_HTML.to_string(),
        };

        let contents = ResourceContents::text(html, UI_RESOURCE_URI)
            .with_mime_type(UI_MIME_TYPE)
            .with_meta(ui_resource_meta());
        Ok(ReadResourceResult::new(vec![contents]))
    }

    fn resolve_ui_html_override_path(&self) -> Option<PathBuf> {
        match &self.ui_html_source {
            UiHtmlSource::ProcessEnv => {
                let raw = std::env::var_os(UI_HTML_ENV_VAR)?;
                if raw.is_empty() {
                    return None;
                }
                if let Some(text) = raw.to_str() {
                    let trimmed = text.trim();
                    if trimmed.is_empty() {
                        return None;
                    }
                    return Some(PathBuf::from(trimmed));
                }
                Some(PathBuf::from(raw))
            }
            #[cfg(test)]
            UiHtmlSource::Override(slot) => slot.lock().ok().and_then(|guard| guard.clone()),
        }
    }

    async fn read_bounded_json<T: DeserializeOwned>(&self, url: &str) -> Result<T, String> {
        let mut resp = self
            .proxy_client
            .get(url)
            .send()
            .await
            .map_err(sanitize_probe_error)?;

        if !resp.status().is_success() {
            return Err(format!("proxy returned HTTP {}", resp.status().as_u16()));
        }

        let mut buf = Vec::new();
        while let Some(chunk) = resp.chunk().await.map_err(sanitize_probe_error)? {
            if buf.len().saturating_add(chunk.len()) > MAX_PROBE_BODY_BYTES {
                return Err(format!(
                    "proxy response exceeded {MAX_PROBE_BODY_BYTES} byte limit"
                ));
            }
            buf.extend_from_slice(&chunk);
        }

        serde_json::from_slice(&buf).map_err(|_| "invalid JSON payload from proxy".to_string())
    }

    async fn probe_proxy(
        &self,
        listen_port: u16,
        binary_version: &str,
    ) -> (ProxyHealthSummary, bool, Option<StatsSnapshot>) {
        let health_url = format!("http://127.0.0.1:{listen_port}/health");
        match self.read_bounded_json::<HealthResponse>(&health_url).await {
            Ok(raw) => {
                let version_skew = raw.version != binary_version;
                let stats_url = format!("http://127.0.0.1:{listen_port}/api/stats");
                let stats = self
                    .read_bounded_json::<StatsSnapshot>(&stats_url)
                    .await
                    .ok();
                let proxy = ProxyHealthSummary::from_health(&raw);
                (proxy, version_skew, stats)
            }
            Err(err) => (ProxyHealthSummary::unreachable(err), false, None),
        }
    }

    fn integration_options(&self) -> IntegrationOptions {
        #[cfg(test)]
        if let Some(ref override_opts) = self.integration_options_override {
            return override_opts.clone();
        }
        // MUST keep data_dir = None (never self.data_dir / --data-dir=${PLUGIN_DATA}) so
        // env.sh always resolves to default_data_dir() (${XDG_DATA_HOME:-~/.local/share}/agy-companion/env.sh).
        IntegrationOptions {
            shell: None,
            rc_file: None,
            binary_path: None,
            data_dir: None,
            follow_rc_symlink: false,
        }
    }

    async fn build_snapshot(&self, cfg: &Config) -> SettingsSnapshot {
        self.build_snapshot_with_integration(cfg, None).await
    }

    async fn build_snapshot_with_integration(
        &self,
        cfg: &Config,
        integration_override: Option<IntegrationStatus>,
    ) -> SettingsSnapshot {
        let binary_version = env!("CARGO_PKG_VERSION").to_string();
        let (proxy, version_skew, stats) = self
            .probe_proxy(cfg.proxy.port.get(), &binary_version)
            .await;
        let cloudcode = CloudCodeStatus::from_config_and_stats(
            &cfg.cloudcode.upstream,
            stats.as_ref().map(|s| &s.cloudcode),
        );

        let mut launcher_integration = match integration_override {
            Some(status) => status,
            None => {
                let opts = self.integration_options();
                match tokio::task::spawn_blocking(move || {
                    companion_core::integration::status(&opts)
                })
                .await
                {
                    Ok(Ok(status)) => status,
                    Ok(Err(err)) => IntegrationStatus::error_fallback(
                        companion_core::integration::detect_hybrid_active(),
                        err.to_string(),
                    ),
                    Err(err) => IntegrationStatus::error_fallback(
                        companion_core::integration::detect_hybrid_active(),
                        format!("launcher integration check failed: {err}"),
                    ),
                }
            }
        };

        // Antigravity's mcp.restoreEnv() strips CLOUD_CODE_URL from MCP children, so a recorded
        // fetchAvailableModels refresh on our local proxy is authoritative proof of live Hybrid Mode.
        if stats
            .as_ref()
            .and_then(|s| s.cloudcode.last_catalog_refresh_at_ms)
            .is_some()
        {
            launcher_integration.mark_hybrid_active();
        }

        SettingsSnapshot {
            config: cfg.redacted(),
            binary_version,
            proxy,
            version_skew,
            stats,
            launcher_integration,
            cloudcode,
            supported_models: curated_model_presets(),
            platform: HostPlatform::current(),
        }
    }

    #[cfg(test)]
    fn with_ui_html_override(mut self, path: Option<PathBuf>) -> Self {
        self.ui_html_source =
            UiHtmlSource::Override(std::sync::Arc::new(std::sync::Mutex::new(path)));
        self
    }

    #[cfg(test)]
    fn with_env_api_key_fallback(mut self, enabled: bool) -> Self {
        self.env_api_key_fallback = enabled;
        self
    }

    #[cfg(test)]
    fn with_test_connection_timeout_secs(mut self, secs: u64) -> Self {
        self.test_connection_timeout_override = Some(secs);
        self
    }

    #[cfg(test)]
    fn with_integration_options(mut self, opts: IntegrationOptions) -> Self {
        self.integration_options_override = Some(opts);
        self
    }
}

fn meta_from_value(value: Value) -> MetaObject {
    match value {
        Value::Object(map) => MetaObject::from(map),
        _ => MetaObject::from(Map::new()),
    }
}

fn ui_resource_meta() -> MetaObject {
    meta_from_value(json!({
        "ui": {
            "prefersBorder": false
        }
    }))
}

fn encode_base64(input: &[u8]) -> String {
    const TABLE: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(input.len().div_ceil(3) * 4);
    let mut chunks = input.chunks_exact(3);
    for chunk in &mut chunks {
        let n = ((chunk[0] as u32) << 16) | ((chunk[1] as u32) << 8) | (chunk[2] as u32);
        out.push(TABLE[((n >> 18) & 0x3f) as usize] as char);
        out.push(TABLE[((n >> 12) & 0x3f) as usize] as char);
        out.push(TABLE[((n >> 6) & 0x3f) as usize] as char);
        out.push(TABLE[(n & 0x3f) as usize] as char);
    }
    match chunks.remainder() {
        [a] => {
            let n = (*a as u32) << 16;
            out.push(TABLE[((n >> 18) & 0x3f) as usize] as char);
            out.push(TABLE[((n >> 12) & 0x3f) as usize] as char);
            out.push('=');
            out.push('=');
        }
        [a, b] => {
            let n = ((*a as u32) << 16) | ((*b as u32) << 8);
            out.push(TABLE[((n >> 18) & 0x3f) as usize] as char);
            out.push(TABLE[((n >> 12) & 0x3f) as usize] as char);
            out.push(TABLE[((n >> 6) & 0x3f) as usize] as char);
            out.push('=');
        }
        [] => {}
        _ => unreachable!(),
    }
    out
}

fn companion_icons() -> Vec<Icon> {
    let encoded = encode_base64(EMBEDDED_LOGO_SVG);
    vec![
        Icon::new(format!("data:image/svg+xml;base64,{encoded}"))
            .with_mime_type("image/svg+xml")
            .with_sizes(vec!["any".to_string()]),
    ]
}

fn open_settings_meta() -> MetaObject {
    meta_from_value(json!({
        "ui": {
            "resourceUri": UI_RESOURCE_URI
        },
        "com.google.antigravity/ui": {
            "entrypoints": [
                { "type": "aux_pane", "title": "Antigravity Companion" },
                { "type": "global", "title": "Antigravity Companion" }
            ]
        }
    }))
}

fn app_only_meta() -> MetaObject {
    meta_from_value(json!({
        "ui": {
            "visibility": ["app"]
        }
    }))
}

fn sanitize_probe_error(err: reqwest::Error) -> String {
    if err.is_timeout() {
        "loopback proxy probe timed out".to_string()
    } else if err.is_connect() {
        "failed to connect to loopback proxy".to_string()
    } else {
        "loopback proxy probe failed".to_string()
    }
}

async fn load_ui_html_override(path: PathBuf) -> Result<String, ErrorData> {
    tokio::task::spawn_blocking(move || {
        validate_path(&path).map_err(|err| {
            ErrorData::internal_error(format!("invalid {UI_HTML_ENV_VAR} path: {err}"), None)
        })?;

        // Stat before File::open so FIFOs, sockets, devices, or directories fail fast without blocking open(2).
        let pre_meta = std::fs::metadata(&path).map_err(|err| {
            ErrorData::internal_error(
                format!("failed to stat {UI_HTML_ENV_VAR} file: {err}"),
                None,
            )
        })?;
        if !pre_meta.is_file() {
            return Err(ErrorData::internal_error(
                format!("{UI_HTML_ENV_VAR} path must point to a regular file"),
                None,
            ));
        }
        if pre_meta.len() > MAX_UI_HTML_BYTES {
            return Err(ErrorData::internal_error(
                format!("{UI_HTML_ENV_VAR} exceeds maximum size of {MAX_UI_HTML_BYTES} bytes"),
                None,
            ));
        }

        let file = File::open(&path).map_err(|err| {
            ErrorData::internal_error(
                format!("failed to open {UI_HTML_ENV_VAR} file: {err}"),
                None,
            )
        })?;

        let metadata = file.metadata().map_err(|err| {
            ErrorData::internal_error(
                format!("failed to stat {UI_HTML_ENV_VAR} file: {err}"),
                None,
            )
        })?;
        if !metadata.is_file() {
            return Err(ErrorData::internal_error(
                format!("{UI_HTML_ENV_VAR} path must point to a regular file"),
                None,
            ));
        }

        let mut buf = Vec::new();
        file.take(MAX_UI_HTML_BYTES + 1)
            .read_to_end(&mut buf)
            .map_err(|err| {
                ErrorData::internal_error(
                    format!("failed to read {UI_HTML_ENV_VAR} file: {err}"),
                    None,
                )
            })?;

        if (buf.len() as u64) > MAX_UI_HTML_BYTES {
            return Err(ErrorData::internal_error(
                format!("{UI_HTML_ENV_VAR} exceeds maximum size of {MAX_UI_HTML_BYTES} bytes"),
                None,
            ));
        }

        let html = String::from_utf8(buf).map_err(|_| {
            ErrorData::internal_error(format!("{UI_HTML_ENV_VAR} must be valid UTF-8"), None)
        })?;

        if html.trim().is_empty() {
            return Err(ErrorData::internal_error(
                format!("{UI_HTML_ENV_VAR} must not be empty"),
                None,
            ));
        }

        Ok(html)
    })
    .await
    .map_err(|err| ErrorData::internal_error(format!("UI HTML load task failed: {err}"), None))?
}

fn tool_error(message: impl Into<String>) -> CallToolResult {
    let message = message.into();
    let mut result = CallToolResult::error(vec![ContentBlock::text(message.clone())]);
    result.structured_content = Some(json!({
        "ok": false,
        "error": message,
    }));
    result
}

fn snapshot_tool_result(
    summary: String,
    snapshot: &SettingsSnapshot,
) -> Result<CallToolResult, ErrorData> {
    let structured = serde_json::to_value(snapshot).map_err(|err| {
        ErrorData::internal_error(
            format!("failed to serialize settings snapshot: {err}"),
            None,
        )
    })?;
    let mut result = CallToolResult::success(vec![ContentBlock::text(summary)]);
    result.structured_content = Some(structured);
    Ok(result)
}

fn format_snapshot_summary(snapshot: &SettingsSnapshot) -> String {
    let proxy_state = if snapshot.proxy.reachable {
        match snapshot.proxy.version.as_deref() {
            Some(ver) if snapshot.version_skew => format!("reachable (v{ver}, version skew)"),
            Some(ver) => format!("reachable (v{ver})"),
            None => "reachable".to_string(),
        }
    } else {
        "unreachable".to_string()
    };

    let gemini = &snapshot.config.providers.gemini;
    let key_state = match gemini.api_key_hint.as_deref() {
        Some(hint) => format!("configured ({hint})"),
        None if gemini.api_key_configured => "configured".to_string(),
        None => "not configured".to_string(),
    };

    format!(
        "Antigravity Companion v{} — proxy at {}:{} is {}, model {}, API key {}.",
        snapshot.binary_version,
        snapshot.config.proxy.host,
        snapshot.config.proxy.port,
        proxy_state,
        gemini.default_model,
        key_state
    )
}

fn test_connection_result(payload: &TestConnectionPayload) -> Result<CallToolResult, ErrorData> {
    let structured = serde_json::to_value(payload).map_err(|err| {
        ErrorData::internal_error(
            format!("failed to serialize test_connection result: {err}"),
            None,
        )
    })?;
    let mut result = CallToolResult::success(vec![ContentBlock::text(payload.message.clone())]);
    result.structured_content = Some(structured);
    Ok(result)
}

#[tool_router]
impl CompanionUiServer {
    #[tool(
        name = "open_settings",
        title = "Open Antigravity Companion Settings",
        description = "Launch the Antigravity Companion settings and proxy telemetry panel.",
        icons = companion_icons(),
        meta = open_settings_meta()
    )]
    pub async fn open_settings(&self) -> Result<CallToolResult, ErrorData> {
        let cfg = match Config::load_or_default(&self.config_path) {
            Ok(cfg) => cfg,
            Err(err) => return Ok(tool_error(err.to_string())),
        };
        let snapshot = self.build_snapshot(&cfg).await;
        snapshot_tool_result(format_snapshot_summary(&snapshot), &snapshot)
    }

    #[tool(
        name = "get_settings",
        title = "Get Antigravity Companion Settings",
        description = "Read redacted Antigravity Companion configuration, loopback proxy health, and live session telemetry.",
        meta = app_only_meta()
    )]
    pub async fn get_settings(&self) -> Result<CallToolResult, ErrorData> {
        let cfg = match Config::load_or_default(&self.config_path) {
            Ok(cfg) => cfg,
            Err(err) => return Ok(tool_error(err.to_string())),
        };
        let snapshot = self.build_snapshot(&cfg).await;
        snapshot_tool_result(format_snapshot_summary(&snapshot), &snapshot)
    }

    #[tool(
        name = "save_settings",
        title = "Save Antigravity Companion Settings",
        description = "Validate and atomically persist Antigravity Companion settings (0600 permissions) without returning plaintext secrets.",
        meta = app_only_meta()
    )]
    pub async fn save_settings(
        &self,
        Parameters(args): Parameters<SaveSettingsArgs>,
    ) -> Result<CallToolResult, ErrorData> {
        let patch: ConfigPatch = args.into();
        let write_guard = self.config_write_lock.lock().await;
        let config_path = self.config_path.clone();
        let save_outcome = tokio::task::spawn_blocking(move || -> Result<Config, String> {
            let mut cfg = Config::load_or_default(&config_path).map_err(|err| err.to_string())?;
            cfg.apply_patch(patch).map_err(|err| err.to_string())?;
            cfg.save_secure(&config_path)
                .map_err(|err| err.to_string())?;
            Ok(cfg)
        })
        .await
        .map_err(|err| {
            ErrorData::internal_error(format!("save_settings task failed: {err}"), None)
        })?;
        drop(write_guard);

        let cfg = match save_outcome {
            Ok(cfg) => cfg,
            Err(msg) => return Ok(tool_error(msg)),
        };

        let snapshot = self.build_snapshot(&cfg).await;
        snapshot_tool_result(
            format!("Settings saved. {}", format_snapshot_summary(&snapshot)),
            &snapshot,
        )
    }

    #[tool(
        name = "test_connection",
        title = "Test Gemini Upstream Connection",
        description = "Verify connectivity and API key authentication against the configured Gemini upstream endpoint.",
        meta = app_only_meta()
    )]
    pub async fn test_connection(&self) -> Result<CallToolResult, ErrorData> {
        let cfg = match Config::load_or_default(&self.config_path) {
            Ok(cfg) => cfg,
            Err(err) => {
                return test_connection_result(&TestConnectionPayload {
                    ok: false,
                    message: err.to_string(),
                });
            }
        };

        let timeout_secs = cfg
            .providers
            .gemini
            .timeout_secs
            .clamp(1, MAX_TEST_CONNECTION_TIMEOUT_SECS);
        #[cfg(test)]
        let timeout_secs = self
            .test_connection_timeout_override
            .unwrap_or(timeout_secs);

        let test_cfg = GeminiConfig {
            timeout_secs,
            ..cfg.providers.gemini.clone()
        };
        let provider = match GeminiProvider::from_config(&test_cfg) {
            Ok(provider) => {
                #[cfg(test)]
                let provider = provider.with_env_fallback(self.env_api_key_fallback);
                provider
            }
            Err(err) => {
                return test_connection_result(&TestConnectionPayload {
                    ok: false,
                    message: err.to_string(),
                });
            }
        };

        let payload = match provider.health_check().await {
            Ok(()) => TestConnectionPayload {
                ok: true,
                message: format!(
                    "Connected to Gemini upstream ({}) successfully.",
                    cfg.providers.gemini.base_url
                ),
            },
            Err(err) => TestConnectionPayload {
                ok: false,
                message: err.to_string(),
            },
        };

        test_connection_result(&payload)
    }

    #[tool(
        name = "set_launcher_integration",
        title = "Configure Dock / Launcher Auto-Start",
        description = "Enable or disable the guarded login-shell hook for Antigravity Dock/Launcher Hybrid Mode auto-start (never follows RC symlinks).",
        meta = app_only_meta()
    )]
    pub async fn set_launcher_integration(
        &self,
        Parameters(args): Parameters<SetLauncherIntegrationArgs>,
    ) -> Result<CallToolResult, ErrorData> {
        let write_guard = self.integration_write_lock.lock().await;
        let mut opts = self.integration_options();
        // Hard invariant: MCP tool NEVER follows RC symlinks.
        opts.follow_rc_symlink = false;
        let enabled = args.enabled;
        let outcome = tokio::task::spawn_blocking(move || {
            if enabled {
                companion_core::integration::enable(&opts)
            } else {
                companion_core::integration::disable(&opts)
            }
        })
        .await
        .map_err(|err| {
            ErrorData::internal_error(format!("set_launcher_integration task failed: {err}"), None)
        })?;
        // Release write lock before config reads & loopback HTTP probes.
        drop(write_guard);

        let status = match outcome {
            Ok(status) => status,
            Err(err) => return Ok(tool_error(err.to_string())),
        };

        let cfg = match Config::load_or_default(&self.config_path) {
            Ok(cfg) => cfg,
            Err(err) => return Ok(tool_error(err.to_string())),
        };

        let snapshot = self
            .build_snapshot_with_integration(&cfg, Some(status))
            .await;
        let prefix = match snapshot.launcher_integration.state() {
            IntegrationState::Unsupported => {
                "Dock / Launcher hook is not supported on this platform."
            }
            IntegrationState::ManualRequired => {
                if enabled {
                    if snapshot.launcher_integration.env_script_path.is_some() {
                        "Dock / Launcher env.sh prepared; manual 1-line RC step required."
                    } else {
                        "Dock / Launcher integration requires a manual shell config step."
                    }
                } else if snapshot.launcher_integration.enabled {
                    "Dock / Launcher hook could not be removed automatically; remove the managed shell config lines manually."
                } else {
                    "Dock / Launcher hook neutralized; remove the managed RC lines manually."
                }
            }
            IntegrationState::Active | IntegrationState::Enabled | IntegrationState::Disabled => {
                if enabled {
                    "Dock / Launcher auto-start enabled."
                } else {
                    "Dock / Launcher auto-start disabled."
                }
            }
        };
        snapshot_tool_result(
            format!("{prefix} {}", format_snapshot_summary(&snapshot)),
            &snapshot,
        )
    }
}

#[tool_handler(router = self.tool_router)]
impl ServerHandler for CompanionUiServer {
    fn get_info(&self) -> ServerConfig {
        ServerConfig::new(
            ServerCapabilities::builder()
                .enable_tools()
                .enable_resources()
                .build(),
        )
        .with_server_info(
            Implementation::new("agy-companion", env!("CARGO_PKG_VERSION"))
                .with_title("Antigravity Companion")
                .with_icons(companion_icons()),
        )
        .with_instructions(
            "Antigravity Companion settings and proxy telemetry MCP App. Call open_settings to launch the settings panel.",
        )
    }

    async fn list_resources(
        &self,
        _request: Option<PaginatedRequestParams>,
        _context: RequestContext<RoleServer>,
    ) -> Result<ListResourcesResult, ErrorData> {
        Ok(self.list_ui_resources())
    }

    async fn read_resource(
        &self,
        request: ReadResourceRequestParams,
        _context: RequestContext<RoleServer>,
    ) -> Result<ReadResourceResponse, ErrorData> {
        let result = self.read_ui_resource(&request.uri).await?;
        Ok(result.into())
    }
}

pub async fn serve_stdio(
    config_path: PathBuf,
    data_dir: Option<PathBuf>,
) -> Result<(), UiMcpError> {
    let server = CompanionUiServer::new(config_path, data_dir)?;
    let running = server
        .serve(stdio())
        .await
        .map_err(|err| UiMcpError::Initialize(err.to_string()))?;
    let _quit = running.waiting().await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    use std::fs;
    use std::num::NonZeroU16;
    #[cfg(unix)]
    use std::os::unix::fs::PermissionsExt;
    use std::process::Command;

    use axum::Router;
    use axum::http::StatusCode;
    use axum::routing::get;
    use companion_core::{ApiKey, CloudCodeUpstream, Config, LoopbackHost};
    use rmcp::model::ErrorCode;
    use serde_json::{Value, json};
    use tempfile::TempDir;
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
    use tokio::net::TcpListener;

    fn config_with_port(port: u16) -> Config {
        let mut cfg = Config::default();
        cfg.proxy.port = NonZeroU16::new(port).expect("non-zero port");
        cfg
    }

    fn write_raw_secure(path: &Path, content: &str) {
        fs::write(path, content).expect("write raw config");
        #[cfg(unix)]
        fs::set_permissions(path, fs::Permissions::from_mode(0o600)).expect("chmod 0600");
    }

    async fn free_closed_port() -> u16 {
        let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
        let port = listener.local_addr().expect("addr").port();
        drop(listener);
        port
    }

    async fn spawn_mock_http(router: Router) -> (u16, tokio::task::JoinHandle<()>) {
        let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
        let port = listener.local_addr().expect("addr").port();
        let handle = tokio::spawn(async move {
            let _ = axum::serve(listener, router).await;
        });
        (port, handle)
    }

    fn decode_base64(input: &str) -> Vec<u8> {
        let bytes = input.as_bytes();
        assert_eq!(bytes.len() % 4, 0, "base64 length must be a multiple of 4");
        let val = |b: u8| -> u32 {
            match b {
                b'A'..=b'Z' => (b - b'A') as u32,
                b'a'..=b'z' => (b - b'a' + 26) as u32,
                b'0'..=b'9' => (b - b'0' + 52) as u32,
                b'+' => 62,
                b'/' => 63,
                other => panic!("unexpected base64 byte: {other}"),
            }
        };
        let mut out = Vec::with_capacity(bytes.len() / 4 * 3);
        for chunk in bytes.chunks_exact(4) {
            let a = val(chunk[0]);
            let b = val(chunk[1]);
            let pad_c = chunk[2] == b'=';
            let pad_d = chunk[3] == b'=';
            let c = if pad_c { 0 } else { val(chunk[2]) };
            let d = if pad_d { 0 } else { val(chunk[3]) };
            let n = (a << 18) | (b << 12) | (c << 6) | d;
            out.push(((n >> 16) & 0xff) as u8);
            if !pad_c {
                out.push(((n >> 8) & 0xff) as u8);
            }
            if !pad_d {
                out.push((n & 0xff) as u8);
            }
        }
        out
    }

    #[test]
    fn base64_encoder_matches_rfc4648_and_companion_icons_roundtrips_logo_svg() {
        let vectors = [
            ("", ""),
            ("f", "Zg=="),
            ("fo", "Zm8="),
            ("foo", "Zm9v"),
            ("foob", "Zm9vYg=="),
            ("fooba", "Zm9vYmE="),
            ("foobar", "Zm9vYmFy"),
        ];
        for (plain, expected) in vectors {
            assert_eq!(encode_base64(plain.as_bytes()), expected);
            assert_eq!(decode_base64(expected), plain.as_bytes());
        }

        let icons = companion_icons();
        assert_eq!(icons.len(), 1);
        let icon = &icons[0];
        assert_eq!(icon.mime_type.as_deref(), Some("image/svg+xml"));
        assert_eq!(icon.sizes.as_deref(), Some(&["any".to_string()][..]));
        let b64 = icon
            .src
            .strip_prefix("data:image/svg+xml;base64,")
            .expect("icon src must be SVG base64 data URI");
        let decoded = decode_base64(b64);
        assert_eq!(decoded, EMBEDDED_LOGO_SVG);

        let disk_logo =
            PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../plugin/assets/logo.svg");
        let disk_bytes = fs::read(&disk_logo).expect("read plugin/assets/logo.svg");
        assert_eq!(decoded, disk_bytes);
    }

    #[test]
    fn open_settings_tool_declares_aux_pane_and_global_entrypoints() {
        let tmp = TempDir::new().expect("tempdir");
        let server = CompanionUiServer::new(tmp.path().join("config.json"), None).expect("server");
        let tools = server.list_tool_definitions();

        let open = tools
            .iter()
            .find(|t| t.name == "open_settings")
            .expect("open_settings tool");
        assert_eq!(
            open.title.as_deref(),
            Some("Open Antigravity Companion Settings")
        );
        let open_icons = open.icons.as_ref().expect("open_settings icons");
        assert_eq!(open_icons.len(), 1);
        assert!(open_icons[0].src.starts_with("data:image/svg+xml;base64,"));
        assert_eq!(open_icons[0].mime_type.as_deref(), Some("image/svg+xml"));
        assert_eq!(
            open_icons[0].sizes.as_deref(),
            Some(&["any".to_string()][..])
        );
        let meta = open.meta.as_ref().expect("open_settings _meta");
        assert_eq!(
            meta.0.get("ui").and_then(|v| v.get("resourceUri")),
            Some(&json!(UI_RESOURCE_URI))
        );
        let entrypoints = meta
            .0
            .get("com.google.antigravity/ui")
            .and_then(|v| v.get("entrypoints"))
            .and_then(Value::as_array)
            .expect("entrypoints array");
        assert_eq!(entrypoints.len(), 2);
        assert!(
            entrypoints
                .iter()
                .any(|e| e.get("type") == Some(&json!("aux_pane")))
        );
        assert!(
            entrypoints
                .iter()
                .any(|e| e.get("type") == Some(&json!("global")))
        );
        for ep in entrypoints {
            assert_eq!(
                ep.get("title"),
                Some(&json!("Antigravity Companion")),
                "entrypoint title must be Antigravity Companion: {ep:?}"
            );
        }

        let expected_titles = [
            ("get_settings", "Get Antigravity Companion Settings"),
            ("save_settings", "Save Antigravity Companion Settings"),
            ("test_connection", "Test Gemini Upstream Connection"),
            (
                "set_launcher_integration",
                "Configure Dock / Launcher Auto-Start",
            ),
        ];
        for (app_only, expected_title) in expected_titles {
            let tool = tools
                .iter()
                .find(|t| t.name == app_only)
                .unwrap_or_else(|| panic!("missing tool {app_only}"));
            assert_eq!(tool.title.as_deref(), Some(expected_title));
            let tool_meta = tool
                .meta
                .as_ref()
                .unwrap_or_else(|| panic!("missing _meta for {app_only}"));
            assert_eq!(
                tool_meta.0.get("ui").and_then(|v| v.get("visibility")),
                Some(&json!(["app"]))
            );
            if app_only == "save_settings" {
                assert_eq!(
                    tool.input_schema.get("additionalProperties"),
                    Some(&json!(false)),
                    "save_settings schema must deny unknown fields: {:?}",
                    tool.input_schema
                );
                let props = tool
                    .input_schema
                    .get("properties")
                    .and_then(Value::as_object)
                    .expect("save_settings properties object");
                assert!(props.contains_key("proxy"));
                assert!(props.contains_key("cloudcode"));
                assert!(props.contains_key("providers"));
                assert!(!props.contains_key("listen_port"));
                assert!(!props.contains_key("api_key"));
                assert!(!props.contains_key("clear_api_key"));
            }
            if app_only == "set_launcher_integration" {
                assert_eq!(
                    tool.input_schema.get("additionalProperties"),
                    Some(&json!(false)),
                    "set_launcher_integration schema must deny unknown fields: {:?}",
                    tool.input_schema
                );
                assert_eq!(
                    tool.input_schema
                        .get("properties")
                        .and_then(|p| p.get("enabled"))
                        .and_then(|e| e.get("type")),
                    Some(&json!("boolean"))
                );
            }
        }
    }

    #[tokio::test]
    async fn ui_resource_serves_mcp_app_mime_and_non_empty_html() {
        let tmp = TempDir::new().expect("tempdir");
        let server = CompanionUiServer::new(tmp.path().join("config.json"), None)
            .expect("server")
            .with_ui_html_override(None);

        let listed = server.list_ui_resources();
        assert_eq!(listed.resources.len(), 1);
        let res = &listed.resources[0];
        assert_eq!(res.uri, UI_RESOURCE_URI);
        assert_eq!(res.name, "Antigravity Companion Settings");
        assert_eq!(res.title.as_deref(), Some("Antigravity Companion Settings"));
        assert_eq!(res.mime_type.as_deref(), Some(UI_MIME_TYPE));
        let res_icons = res.icons.as_ref().expect("resource icons");
        assert_eq!(res_icons.len(), 1);
        assert!(res_icons[0].src.starts_with("data:image/svg+xml;base64,"));
        assert_eq!(res_icons[0].mime_type.as_deref(), Some("image/svg+xml"));
        assert_eq!(
            res_icons[0].sizes.as_deref(),
            Some(&["any".to_string()][..])
        );
        assert_eq!(
            res.meta
                .as_ref()
                .and_then(|m| m.0.get("ui"))
                .and_then(|u| u.get("prefersBorder")),
            Some(&json!(false))
        );

        let read = server
            .read_ui_resource(UI_RESOURCE_URI)
            .await
            .expect("read_ui_resource");
        assert_eq!(read.contents.len(), 1);
        match &read.contents[0] {
            ResourceContents::TextResourceContents {
                uri,
                mime_type,
                text,
                meta,
            } => {
                assert_eq!(uri, UI_RESOURCE_URI);
                assert_eq!(mime_type.as_deref(), Some(UI_MIME_TYPE));
                assert!(!text.trim().is_empty());
                assert!(text.len() < 512_000, "embedded UI HTML must be < 500 KiB");
                assert!(text.contains("<!doctype html>") || text.contains("<!DOCTYPE html>"));
                assert!(text.contains("<title>Antigravity Companion Settings</title>"));
                assert!(
                    !text.contains("AGY Companion"),
                    "embedded UI HTML must not contain stale AGY Companion display strings"
                );
                assert_eq!(
                    meta.as_ref()
                        .and_then(|m| m.0.get("ui"))
                        .and_then(|u| u.get("prefersBorder")),
                    Some(&json!(false))
                );
            }
            other => panic!("expected text resource contents, got {other:?}"),
        }

        // Verify ui/src has zero forbidden DOM/JS sinks
        let ui_src = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../ui/src");
        let forbidden = [
            "innerHTML",
            "outerHTML",
            "insertAdjacentHTML",
            "document.write",
            "dangerouslySetInnerHTML",
            "localStorage",
            "sessionStorage",
            "document.cookie",
            "new Function",
            "XMLHttpRequest",
            "eval(",
            "alert(",
            "confirm(",
            "prompt(",
            "fetch(",
        ];
        for entry in fs::read_dir(&ui_src).expect("read ui/src") {
            let path = entry.expect("entry").path();
            if path.is_file() {
                let content = fs::read_to_string(&path).expect("read src file");
                for token in forbidden {
                    assert!(
                        !content.contains(token),
                        "forbidden token `{token}` found in {}",
                        path.display()
                    );
                }
            }
        }
    }

    #[tokio::test]
    async fn get_settings_never_returns_plaintext_api_key() {
        let tmp = TempDir::new().expect("tempdir");
        let cfg_path = tmp.path().join("config.json");
        let secret = "AIzaSySecret1234567890";

        let mut cfg = config_with_port(free_closed_port().await);
        cfg.providers.gemini.api_key = Some(ApiKey::parse_strict(secret).expect("valid api key"));
        cfg.save_secure(&cfg_path).expect("save config");

        let server = CompanionUiServer::new(cfg_path, None).expect("server");

        for result in [
            server.get_settings().await.expect("get_settings"),
            server.open_settings().await.expect("open_settings"),
        ] {
            assert_eq!(result.is_error, Some(false));
            let serialized = serde_json::to_string(&result).expect("serialize result");
            assert!(!serialized.contains(secret));
            assert!(!serialized.contains("SySecret1234"));
            assert!(serialized.contains("AIza...7890"));
            assert!(serialized.contains("Antigravity Companion v"));
            assert!(!serialized.contains("AGY Companion"));

            let snapshot: SettingsSnapshot = serde_json::from_value(
                result
                    .structured_content
                    .clone()
                    .expect("structured_content"),
            )
            .expect("parse SettingsSnapshot");
            assert!(snapshot.config.providers.gemini.api_key_configured);
            assert_eq!(
                snapshot.config.providers.gemini.api_key_hint.as_deref(),
                Some("AIza...7890")
            );
            assert_eq!(snapshot.supported_models, curated_model_presets());
            assert_eq!(snapshot.platform, HostPlatform::current());
        }
    }

    #[tokio::test]
    async fn save_settings_persists_eagerly_and_validates_inputs() {
        let tmp = TempDir::new().expect("tempdir");
        let cfg_path = tmp.path().join("config.json");
        let closed_port = free_closed_port().await;
        let server = CompanionUiServer::new(cfg_path.clone(), None).expect("server");

        // Reject remote http:// URL
        let bad = server
            .save_settings(Parameters(SaveSettingsArgs {
                providers: Some(ProvidersSettingsPatch {
                    gemini: Some(GeminiSettingsPatch {
                        base_url: Some("http://example.com".to_string()),
                        ..Default::default()
                    }),
                }),
                ..Default::default()
            }))
            .await
            .expect("save_settings error result");
        assert_eq!(bad.is_error, Some(true));
        assert!(!cfg_path.exists());

        // Persist valid settings atomically at 0600
        let secret = "AIzaSyValidSaveTestKey9876";
        let ok = server
            .save_settings(Parameters(SaveSettingsArgs {
                proxy: Some(ProxySettingsPatch {
                    host: Some("127.0.0.1".to_string()),
                    port: Some(closed_port),
                }),
                cloudcode: Some(CloudCodeSettingsPatch {
                    upstream: Some("https://cloudcode-pa.googleapis.com".to_string()),
                }),
                providers: Some(ProvidersSettingsPatch {
                    gemini: Some(GeminiSettingsPatch {
                        api_key: Some(ApiKeyPatch::Set(secret.to_string())),
                        base_url: Some("https://generativelanguage.googleapis.com".to_string()),
                        default_model: Some("gemini-3.8-flash".to_string()),
                    }),
                }),
            }))
            .await
            .expect("save_settings ok");
        assert_eq!(ok.is_error, Some(false));

        let serialized = serde_json::to_string(&ok).expect("serialize");
        assert!(!serialized.contains(secret));
        assert!(serialized.contains("AIza...9876"));

        let saved = Config::load_or_default(&cfg_path).expect("reload config");
        assert_eq!(
            saved.providers.gemini.api_key.as_ref().map(ApiKey::expose),
            Some(secret)
        );
        assert_eq!(saved.providers.gemini.default_model, "gemini-3.8-flash");
        assert_eq!(saved.proxy.port.get(), closed_port);
        assert_eq!(
            saved.cloudcode.upstream,
            CloudCodeUpstream::Fixed("https://cloudcode-pa.googleapis.com".to_string())
        );

        #[cfg(unix)]
        {
            let mode = fs::metadata(&cfg_path)
                .expect("metadata")
                .permissions()
                .mode()
                & 0o777;
            assert_eq!(mode, 0o600);
        }
    }

    #[tokio::test]
    async fn save_settings_preserves_existing_key_when_omitted_rejects_blank_set_and_clears_on_variant()
     {
        let tmp = TempDir::new().expect("tempdir");
        let cfg_path = tmp.path().join("config.json");
        let closed_port = free_closed_port().await;
        let initial_key = "AIzaSyKeepThisSecretKey4321";

        let mut cfg = config_with_port(closed_port);
        cfg.providers.gemini.api_key = Some(ApiKey::parse_strict(initial_key).expect("valid key"));
        cfg.save_secure(&cfg_path).expect("save initial");

        let server = CompanionUiServer::new(cfg_path.clone(), None).expect("server");

        // Omitting providers, providers.gemini, or providers.gemini.api_key preserves existing key
        for args in [
            SaveSettingsArgs::default(),
            SaveSettingsArgs {
                providers: Some(ProvidersSettingsPatch { gemini: None }),
                ..Default::default()
            },
            SaveSettingsArgs {
                providers: Some(ProvidersSettingsPatch {
                    gemini: Some(GeminiSettingsPatch {
                        api_key: None,
                        default_model: Some("gemini-3.8-flash".to_string()),
                        ..Default::default()
                    }),
                }),
                ..Default::default()
            },
        ] {
            let res = server.save_settings(Parameters(args)).await.expect("save");
            assert_eq!(res.is_error, Some(false));
            let reloaded = Config::load_or_default(&cfg_path).expect("reload");
            assert_eq!(
                reloaded
                    .providers
                    .gemini
                    .api_key
                    .as_ref()
                    .map(ApiKey::expose),
                Some(initial_key)
            );
        }

        // ApiKeyPatch::Set("") and ApiKeyPatch::Set("   ") are strictly rejected without altering initial_key
        for blank in ["", "   "] {
            let rejected = server
                .save_settings(Parameters(SaveSettingsArgs {
                    providers: Some(ProvidersSettingsPatch {
                        gemini: Some(GeminiSettingsPatch {
                            api_key: Some(ApiKeyPatch::Set(blank.to_string())),
                            ..Default::default()
                        }),
                    }),
                    ..Default::default()
                }))
                .await
                .expect("blank set");
            assert_eq!(rejected.is_error, Some(true));
            let reloaded = Config::load_or_default(&cfg_path).expect("reload");
            assert_eq!(
                reloaded
                    .providers
                    .gemini
                    .api_key
                    .as_ref()
                    .map(ApiKey::expose),
                Some(initial_key)
            );
        }

        // Explicit ApiKeyPatch::Clear clears saved key
        let cleared = server
            .save_settings(Parameters(SaveSettingsArgs {
                providers: Some(ProvidersSettingsPatch {
                    gemini: Some(GeminiSettingsPatch {
                        api_key: Some(ApiKeyPatch::Clear),
                        ..Default::default()
                    }),
                }),
                ..Default::default()
            }))
            .await
            .expect("clear");
        assert_eq!(cleared.is_error, Some(false));
        let reloaded = Config::load_or_default(&cfg_path).expect("reload");
        assert_eq!(reloaded.providers.gemini.api_key, None);
    }

    #[tokio::test]
    async fn save_settings_rejects_invalid_inputs_without_echoing_secret() {
        let tmp = TempDir::new().expect("tempdir");
        let cfg_path = tmp.path().join("config.json");
        let server = CompanionUiServer::new(cfg_path.clone(), None).expect("server");

        // Verify SaveSettingsArgs Debug redacts api_key via ApiKeyPatch
        let debug_args = SaveSettingsArgs {
            providers: Some(ProvidersSettingsPatch {
                gemini: Some(GeminiSettingsPatch {
                    api_key: Some(ApiKeyPatch::Set("AIzaSyDebugSecretShouldHide".to_string())),
                    ..Default::default()
                }),
            }),
            ..Default::default()
        };
        let dbg = format!("{debug_args:?}");
        assert!(!dbg.contains("AIzaSyDebugSecretShouldHide"));
        assert!(dbg.contains("AIza...Hide"));

        // Invalid API keys: non-ASCII, control chars, embedded space, whitespace+CRLF
        for bad_key in [
            "AIza\u{00A0}SecretNonAsciiKey",
            "AIzaSyControl\r\nSecretKey",
            "AIzaSy Embedded SpaceKey",
            "  \r\n  ",
        ] {
            let res = server
                .save_settings(Parameters(SaveSettingsArgs {
                    providers: Some(ProvidersSettingsPatch {
                        gemini: Some(GeminiSettingsPatch {
                            api_key: Some(ApiKeyPatch::Set(bad_key.to_string())),
                            ..Default::default()
                        }),
                    }),
                    ..Default::default()
                }))
                .await
                .expect("invalid key result");
            assert_eq!(
                res.is_error,
                Some(true),
                "expected rejection for {bad_key:?}"
            );
            let serialized = serde_json::to_string(&res).expect("serialize");
            assert!(!serialized.contains("Secret"));
            assert!(!serialized.contains("Embedded"));
        }

        // Invalid host, port, model (incl. bare gemini-5), remote http URL, credentialed URL
        let invalid_cases = [
            SaveSettingsArgs {
                proxy: Some(ProxySettingsPatch {
                    host: Some("0.0.0.0".to_string()),
                    ..Default::default()
                }),
                ..Default::default()
            },
            SaveSettingsArgs {
                proxy: Some(ProxySettingsPatch {
                    port: Some(0),
                    ..Default::default()
                }),
                ..Default::default()
            },
            SaveSettingsArgs {
                providers: Some(ProvidersSettingsPatch {
                    gemini: Some(GeminiSettingsPatch {
                        default_model: Some("../evil".to_string()),
                        ..Default::default()
                    }),
                }),
                ..Default::default()
            },
            SaveSettingsArgs {
                providers: Some(ProvidersSettingsPatch {
                    gemini: Some(GeminiSettingsPatch {
                        default_model: Some("gemini-2.5-pro".to_string()),
                        ..Default::default()
                    }),
                }),
                ..Default::default()
            },
            SaveSettingsArgs {
                providers: Some(ProvidersSettingsPatch {
                    gemini: Some(GeminiSettingsPatch {
                        default_model: Some("gemini-2.5-flash".to_string()),
                        ..Default::default()
                    }),
                }),
                ..Default::default()
            },
            SaveSettingsArgs {
                providers: Some(ProvidersSettingsPatch {
                    gemini: Some(GeminiSettingsPatch {
                        default_model: Some("gemini-5".to_string()),
                        ..Default::default()
                    }),
                }),
                ..Default::default()
            },
            SaveSettingsArgs {
                providers: Some(ProvidersSettingsPatch {
                    gemini: Some(GeminiSettingsPatch {
                        base_url: Some("http://evil.com".to_string()),
                        ..Default::default()
                    }),
                }),
                ..Default::default()
            },
            SaveSettingsArgs {
                providers: Some(ProvidersSettingsPatch {
                    gemini: Some(GeminiSettingsPatch {
                        base_url: Some(
                            "https://user:pass@generativelanguage.googleapis.com".to_string(),
                        ),
                        ..Default::default()
                    }),
                }),
                ..Default::default()
            },
            SaveSettingsArgs {
                cloudcode: Some(CloudCodeSettingsPatch {
                    upstream: Some("http://daily-cloudcode-pa.googleapis.com".to_string()),
                }),
                ..Default::default()
            },
        ];

        for args in invalid_cases {
            let res = server
                .save_settings(Parameters(args))
                .await
                .expect("invalid settings result");
            assert_eq!(res.is_error, Some(true));
        }

        assert!(!cfg_path.exists());
    }

    #[tokio::test]
    async fn get_settings_detects_proxy_health_version_skew_and_stats() {
        let tmp = TempDir::new().expect("tempdir");
        let cfg_path = tmp.path().join("config.json");

        // Case 1: Reachable with matching version
        let matching_router = Router::new()
            .route(
                "/health",
                get(|| async {
                    axum::Json(json!({
                        "status": "ok",
                        "version": env!("CARGO_PKG_VERSION"),
                        "protocol_version": 1
                    }))
                }),
            )
            .route(
                "/api/stats",
                get(|| async {
                    axum::Json(json!({
                        "usage": {
                            "requests": 7,
                            "prompt_tokens": 1000,
                            "cached_tokens": 600,
                            "output_tokens": 250,
                            "reasoning_tokens": 50,
                            "cache_hit_ratio": 0.6
                        },
                        "cloudcode": {}
                    }))
                }),
            );
        let (port_match, handle_match) = spawn_mock_http(matching_router).await;

        let mut cfg = config_with_port(port_match);
        cfg.save_secure(&cfg_path).expect("save");

        let server = CompanionUiServer::new(cfg_path.clone(), None).expect("server");
        let res = server.get_settings().await.expect("get_settings");
        assert_eq!(res.is_error, Some(false));
        let snap: SettingsSnapshot =
            serde_json::from_value(res.structured_content.expect("structured")).expect("parse");
        assert!(snap.proxy.reachable);
        assert_eq!(snap.proxy.status.as_deref(), Some("ok"));
        assert_eq!(
            snap.proxy.version.as_deref(),
            Some(env!("CARGO_PKG_VERSION"))
        );
        assert_eq!(snap.proxy.protocol_version, Some(1));
        assert_eq!(snap.proxy.error, None);
        assert!(!snap.version_skew);
        let stats = snap.stats.expect("stats");
        assert_eq!(stats.usage.requests, 7);
        assert_eq!(stats.usage.cached_tokens, 600);
        handle_match.abort();

        // Case 2: Reachable with mismatched version (skew = true)
        let skew_router = Router::new().route(
            "/health",
            get(|| async {
                axum::Json(json!({
                    "status": "ok",
                    "version": "9.9.9",
                    "protocol_version": 1
                }))
            }),
        );
        let (port_skew, handle_skew) = spawn_mock_http(skew_router).await;
        cfg.proxy.port = NonZeroU16::new(port_skew).unwrap();
        cfg.save_secure(&cfg_path).expect("save skew");

        let res_skew = server.get_settings().await.expect("get_settings skew");
        let snap_skew: SettingsSnapshot =
            serde_json::from_value(res_skew.structured_content.expect("structured"))
                .expect("parse");
        assert!(snap_skew.proxy.reachable);
        assert!(snap_skew.version_skew);
        assert_eq!(snap_skew.proxy.version.as_deref(), Some("9.9.9"));
        assert!(snap_skew.stats.is_none());
        handle_skew.abort();

        // Case 3: Oversized /health body (> 64 KiB) rejected cleanly without OOM
        let huge_router = Router::new().route(
            "/health",
            get(|| async { "x".repeat(MAX_PROBE_BODY_BYTES + 1024) }),
        );
        let (port_huge, handle_huge) = spawn_mock_http(huge_router).await;
        cfg.proxy.port = NonZeroU16::new(port_huge).unwrap();
        cfg.save_secure(&cfg_path).expect("save huge");
        let res_huge = server.get_settings().await.expect("get_settings huge");
        let snap_huge: SettingsSnapshot =
            serde_json::from_value(res_huge.structured_content.expect("structured"))
                .expect("parse");
        assert!(!snap_huge.proxy.reachable);
        assert!(
            snap_huge
                .proxy
                .error
                .as_deref()
                .is_some_and(|e| e.contains("limit"))
        );
        handle_huge.abort();

        // Case 4: Unreachable closed port
        cfg.proxy.port = NonZeroU16::new(free_closed_port().await).unwrap();
        cfg.save_secure(&cfg_path).expect("save closed");
        let res_down = server.get_settings().await.expect("get_settings down");
        let snap_down: SettingsSnapshot =
            serde_json::from_value(res_down.structured_content.expect("structured"))
                .expect("parse");
        assert!(!snap_down.proxy.reachable);
        assert!(!snap_down.version_skew);
        assert!(snap_down.proxy.error.is_some());
        assert!(snap_down.stats.is_none());
    }

    #[tokio::test]
    async fn proxy_probe_ignores_hostile_http_proxy_env() {
        if std::env::var_os("AGY_COMPANION_HOSTILE_PROXY_CHILD").is_some() {
            let router = Router::new()
                .route(
                    "/health",
                    get(|| async {
                        axum::Json(json!({
                            "status": "ok",
                            "version": env!("CARGO_PKG_VERSION"),
                            "protocol_version": 1
                        }))
                    }),
                )
                .route(
                    "/api/stats",
                    get(|| async {
                        axum::Json(json!({
                            "usage": {
                                "requests": 2,
                                "prompt_tokens": 100,
                                "cached_tokens": 50,
                                "output_tokens": 20,
                                "reasoning_tokens": 5,
                                "cache_hit_ratio": 0.5
                            }
                        }))
                    }),
                );
            let (port, handle) = spawn_mock_http(router).await;
            let tmp = TempDir::new().expect("tempdir");
            let cfg_path = tmp.path().join("config.json");
            let cfg = config_with_port(port);
            cfg.save_secure(&cfg_path).expect("save");

            let server = CompanionUiServer::new(cfg_path, None).expect("server");
            let res = server.get_settings().await.expect("get_settings");
            let snap: SettingsSnapshot =
                serde_json::from_value(res.structured_content.expect("structured")).expect("parse");
            assert!(snap.proxy.reachable);
            assert_eq!(snap.stats.expect("stats").usage.requests, 2);
            handle.abort();
            return;
        }

        let exe = std::env::current_exe().expect("current_exe");
        let output = Command::new(exe)
            .arg("--exact")
            .arg("server::tests::proxy_probe_ignores_hostile_http_proxy_env")
            .arg("--nocapture")
            .env("AGY_COMPANION_HOSTILE_PROXY_CHILD", "1")
            .env("HTTP_PROXY", "http://127.0.0.1:1")
            .env("HTTPS_PROXY", "http://127.0.0.1:1")
            .env("ALL_PROXY", "http://127.0.0.1:1")
            .output()
            .expect("spawn child test");
        assert!(
            output.status.success(),
            "hostile proxy child failed:\nstdout: {}\nstderr: {}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
    }

    #[tokio::test]
    async fn test_connection_handles_ok_invalid_key_missing_key_timeout_without_leaking_key() {
        let tmp = TempDir::new().expect("tempdir");
        let cfg_path = tmp.path().join("config.json");
        let valid_secret = "AIzaSyConnectionOkKey123456";
        let bad_secret = "AIzaSyBadKeyMustNotLeak9999";

        // 1. Upstream 200 OK
        let ok_router = Router::new().route(
            "/v1beta/models",
            get(|| async { (StatusCode::OK, axum::Json(json!({ "models": [] }))) }),
        );
        let (ok_port, ok_handle) = spawn_mock_http(ok_router).await;
        let mut cfg = Config::default();
        cfg.providers.gemini.base_url = format!("http://127.0.0.1:{ok_port}");
        cfg.providers.gemini.api_key = Some(ApiKey::parse_strict(valid_secret).unwrap());
        cfg.save_secure(&cfg_path).expect("save ok");

        let server = CompanionUiServer::new(cfg_path.clone(), None)
            .expect("server")
            .with_env_api_key_fallback(false);
        let res_ok = server.test_connection().await.expect("test ok");
        assert_eq!(res_ok.is_error, Some(false));
        let serialized_ok = serde_json::to_string(&res_ok).expect("serialize ok");
        assert!(!serialized_ok.contains(valid_secret));
        let payload_ok: TestConnectionPayload =
            serde_json::from_value(res_ok.structured_content.expect("structured")).expect("parse");
        assert!(payload_ok.ok);
        ok_handle.abort();

        // 2. Upstream 400 API_KEY_INVALID echoing secret in body -> must NOT leak key
        let bad_secret_owned = bad_secret.to_string();
        let bad_router = Router::new().route(
            "/v1beta/models",
            get(move || {
                let leak = bad_secret_owned.clone();
                async move {
                    (
                        StatusCode::BAD_REQUEST,
                        axum::Json(json!({
                            "error": {
                                "status": "INVALID_ARGUMENT",
                                "message": format!("API key not valid: {leak}"),
                                "details": [{ "reason": "API_KEY_INVALID" }]
                            }
                        })),
                    )
                }
            }),
        );
        let (bad_port, bad_handle) = spawn_mock_http(bad_router).await;
        cfg.providers.gemini.base_url = format!("http://127.0.0.1:{bad_port}");
        cfg.providers.gemini.api_key = Some(ApiKey::parse_strict(bad_secret).unwrap());
        cfg.save_secure(&cfg_path).expect("save bad");

        let res_bad = server.test_connection().await.expect("test bad");
        assert_eq!(res_bad.is_error, Some(false));
        let serialized_bad = serde_json::to_string(&res_bad).expect("serialize bad");
        assert!(!serialized_bad.contains(bad_secret));
        let payload_bad: TestConnectionPayload =
            serde_json::from_value(res_bad.structured_content.expect("structured")).expect("parse");
        assert!(!payload_bad.ok);
        assert!(
            payload_bad.message.contains("API_KEY_INVALID") || payload_bad.message.contains("400")
        );
        bad_handle.abort();

        // 3. Missing API key (deterministic with env fallback disabled)
        cfg.providers.gemini.api_key = None;
        cfg.save_secure(&cfg_path).expect("save missing");
        let res_missing = server.test_connection().await.expect("test missing");
        assert_eq!(res_missing.is_error, Some(false));
        let payload_missing: TestConnectionPayload =
            serde_json::from_value(res_missing.structured_content.expect("structured"))
                .expect("parse");
        assert!(!payload_missing.ok);

        // 4. Slow upstream timeout
        let slow_router = Router::new().route(
            "/v1beta/models",
            get(|| async {
                tokio::time::sleep(Duration::from_secs(3)).await;
                (StatusCode::OK, axum::Json(json!({ "models": [] })))
            }),
        );
        let (slow_port, slow_handle) = spawn_mock_http(slow_router).await;
        cfg.providers.gemini.base_url = format!("http://127.0.0.1:{slow_port}");
        cfg.providers.gemini.api_key = Some(ApiKey::parse_strict(valid_secret).unwrap());
        cfg.save_secure(&cfg_path).expect("save slow");

        let fast_timeout_server = CompanionUiServer::new(cfg_path, None)
            .expect("server")
            .with_env_api_key_fallback(false)
            .with_test_connection_timeout_secs(1);
        let res_timeout = fast_timeout_server
            .test_connection()
            .await
            .expect("test timeout");
        assert_eq!(res_timeout.is_error, Some(false));
        let serialized_timeout = serde_json::to_string(&res_timeout).expect("serialize");
        assert!(!serialized_timeout.contains(valid_secret));
        let payload_timeout: TestConnectionPayload =
            serde_json::from_value(res_timeout.structured_content.expect("structured"))
                .expect("parse");
        assert!(!payload_timeout.ok);
        slow_handle.abort();
    }

    #[tokio::test]
    async fn ui_resource_dynamic_reload_oversized_rejection_and_unknown_uri() {
        let tmp = TempDir::new().expect("tempdir");
        let html_path = tmp.path().join("override.html");
        fs::write(&html_path, "<!doctype html><title>v1</title>").expect("write v1");

        let server = CompanionUiServer::new(tmp.path().join("config.json"), None)
            .expect("server")
            .with_ui_html_override(Some(html_path.clone()));

        // Read v1
        let r1 = server
            .read_ui_resource(UI_RESOURCE_URI)
            .await
            .expect("read v1");
        let text1 = match &r1.contents[0] {
            ResourceContents::TextResourceContents { text, .. } => text.clone(),
            _ => panic!("expected text"),
        };
        assert!(text1.contains("v1"));

        // Overwrite file -> read v2 dynamically on same server instance
        fs::write(&html_path, "<!doctype html><title>v2-reloaded</title>").expect("write v2");
        let r2 = server
            .read_ui_resource(UI_RESOURCE_URI)
            .await
            .expect("read v2");
        let text2 = match &r2.contents[0] {
            ResourceContents::TextResourceContents { text, .. } => text.clone(),
            _ => panic!("expected text"),
        };
        assert!(text2.contains("v2-reloaded"));

        // Oversized (> 10 MiB) rejected cleanly
        let oversized = vec![b'a'; (MAX_UI_HTML_BYTES + 1) as usize];
        fs::write(&html_path, oversized).expect("write oversized");
        let err_big = server
            .read_ui_resource(UI_RESOURCE_URI)
            .await
            .expect_err("oversized must fail");
        assert_eq!(err_big.code, ErrorCode::INTERNAL_ERROR);

        // Empty rejected
        fs::write(&html_path, "   \n  ").expect("write empty");
        let err_empty = server
            .read_ui_resource(UI_RESOURCE_URI)
            .await
            .expect_err("empty must fail");
        assert_eq!(err_empty.code, ErrorCode::INTERNAL_ERROR);

        // Invalid UTF-8 rejected
        fs::write(&html_path, [0xff, 0xfe, 0xfd]).expect("write invalid utf8");
        let err_utf8 = server
            .read_ui_resource(UI_RESOURCE_URI)
            .await
            .expect_err("non-utf8 must fail");
        assert_eq!(err_utf8.code, ErrorCode::INTERNAL_ERROR);

        // Directory path rejected
        let dir_server = CompanionUiServer::new(tmp.path().join("config.json"), None)
            .expect("server")
            .with_ui_html_override(Some(tmp.path().to_path_buf()));
        let err_dir = dir_server
            .read_ui_resource(UI_RESOURCE_URI)
            .await
            .expect_err("directory must fail");
        assert_eq!(err_dir.code, ErrorCode::INTERNAL_ERROR);

        // Unknown URI -> RESOURCE_NOT_FOUND
        let err_uri = server
            .read_ui_resource("ui://companion/unknown.html")
            .await
            .expect_err("unknown uri must fail");
        assert_eq!(err_uri.code, ErrorCode::RESOURCE_NOT_FOUND);
    }

    #[tokio::test]
    async fn ui_resource_reads_process_env_var_dynamically() {
        if std::env::var_os("AGY_COMPANION_UI_HTML_CHILD").is_some() {
            let tmp = TempDir::new().expect("tempdir");
            let server =
                CompanionUiServer::new(tmp.path().join("config.json"), None).expect("server");
            let read = server
                .read_ui_resource(UI_RESOURCE_URI)
                .await
                .expect("read env override");
            let text = match &read.contents[0] {
                ResourceContents::TextResourceContents { text, .. } => text.as_str(),
                other => panic!("expected text, got {other:?}"),
            };
            assert!(text.contains("env-override-live-marker"));
            return;
        }

        let tmp = TempDir::new().expect("tempdir");
        let html_path = tmp.path().join("live.html");
        fs::write(
            &html_path,
            "<!doctype html><title>env-override-live-marker</title>",
        )
        .expect("write live html");

        let exe = std::env::current_exe().expect("current_exe");
        let output = Command::new(exe)
            .arg("--exact")
            .arg("server::tests::ui_resource_reads_process_env_var_dynamically")
            .arg("--nocapture")
            .env("AGY_COMPANION_UI_HTML_CHILD", "1")
            .env(UI_HTML_ENV_VAR, &html_path)
            .output()
            .expect("spawn child");
        assert!(
            output.status.success(),
            "env override child failed:\nstdout: {}\nstderr: {}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn mcp_json_rpc_full_wire_protocol_over_duplex() {
        let tmp = TempDir::new().expect("tempdir");
        let cfg_path = tmp.path().join("config.json");
        let closed_port = free_closed_port().await;

        let upstream_router = Router::new().route(
            "/v1beta/models",
            get(|| async { (StatusCode::OK, axum::Json(json!({ "models": [] }))) }),
        );
        let (upstream_port, upstream_handle) = spawn_mock_http(upstream_router).await;

        let server = CompanionUiServer::new(cfg_path, None)
            .expect("server")
            .with_ui_html_override(None)
            .with_env_api_key_fallback(false);

        let (client_io, server_io) = tokio::io::duplex(1024 * 1024);
        let server_task = tokio::spawn(async move {
            let running = server.serve(server_io).await.expect("serve duplex");
            let _ = running.waiting().await;
        });

        let (read_half, mut write_half) = tokio::io::split(client_io);
        let mut reader = BufReader::new(read_half);

        async fn send_msg(w: &mut tokio::io::WriteHalf<tokio::io::DuplexStream>, msg: &Value) {
            let mut line = serde_json::to_string(msg).expect("json");
            line.push('\n');
            w.write_all(line.as_bytes()).await.expect("write");
            w.flush().await.expect("flush");
        }

        async fn recv_msg(
            r: &mut BufReader<tokio::io::ReadHalf<tokio::io::DuplexStream>>,
        ) -> Value {
            let mut line = String::new();
            let n = tokio::time::timeout(Duration::from_secs(15), r.read_line(&mut line))
                .await
                .expect("timed out waiting for MCP server response")
                .expect("read");
            assert!(n > 0, "unexpected EOF from MCP server");
            serde_json::from_str(&line).expect("parse json-rpc response")
        }

        // 1. initialize
        send_msg(
            &mut write_half,
            &json!({
                "jsonrpc": "2.0",
                "id": 1,
                "method": "initialize",
                "params": {
                    "protocolVersion": "2025-11-25",
                    "capabilities": {},
                    "clientInfo": { "name": "test-client", "version": "0.1.0" }
                }
            }),
        )
        .await;
        let init_resp = recv_msg(&mut reader).await;
        assert_eq!(init_resp["id"], 1);
        assert_eq!(init_resp["result"]["serverInfo"]["name"], "agy-companion");
        assert_eq!(
            init_resp["result"]["serverInfo"]["title"],
            "Antigravity Companion"
        );
        let init_icons = init_resp["result"]["serverInfo"]["icons"]
            .as_array()
            .expect("serverInfo icons array");
        assert_eq!(init_icons.len(), 1);
        assert!(
            init_icons[0]["src"]
                .as_str()
                .is_some_and(|s| s.starts_with("data:image/svg+xml;base64,"))
        );
        assert_eq!(init_icons[0]["mimeType"], "image/svg+xml");
        assert_eq!(init_icons[0]["sizes"], json!(["any"]));

        // 2. notifications/initialized
        send_msg(
            &mut write_half,
            &json!({
                "jsonrpc": "2.0",
                "method": "notifications/initialized"
            }),
        )
        .await;

        // 3. tools/list
        send_msg(
            &mut write_half,
            &json!({ "jsonrpc": "2.0", "id": 2, "method": "tools/list", "params": {} }),
        )
        .await;
        let tools_resp = recv_msg(&mut reader).await;
        let tools = tools_resp["result"]["tools"]
            .as_array()
            .expect("tools array");
        assert_eq!(tools.len(), 5);
        assert!(
            tools
                .iter()
                .any(|t| t.get("name") == Some(&json!("set_launcher_integration")))
        );

        // 3b. Verify set_launcher_integration rejects unknown fields at the JSON-RPC boundary
        send_msg(
            &mut write_half,
            &json!({
                "jsonrpc": "2.0",
                "id": 21,
                "method": "tools/call",
                "params": {
                    "name": "set_launcher_integration",
                    "arguments": {
                        "enabled": true,
                        "rc_file": "/etc/passwd"
                    }
                }
            }),
        )
        .await;
        let bad_integration_resp = recv_msg(&mut reader).await;
        let rejected = bad_integration_resp.get("error").is_some()
            || bad_integration_resp["result"]["isError"] == json!(true);
        assert!(
            rejected,
            "set_launcher_integration with unknown field must be rejected before handler: {bad_integration_resp:?}"
        );
        let err_blob = serde_json::to_string(&bad_integration_resp).expect("serialize");
        assert!(
            err_blob.contains("unknown field") && err_blob.contains("rc_file"),
            "expected unknown field error message, got: {err_blob}"
        );

        // 4. resources/list
        send_msg(
            &mut write_half,
            &json!({ "jsonrpc": "2.0", "id": 3, "method": "resources/list", "params": {} }),
        )
        .await;
        let res_list = recv_msg(&mut reader).await;
        assert_eq!(
            res_list["result"]["resources"][0]["uri"],
            json!(UI_RESOURCE_URI)
        );
        assert_eq!(
            res_list["result"]["resources"][0]["name"],
            json!("Antigravity Companion Settings")
        );
        assert_eq!(
            res_list["result"]["resources"][0]["title"],
            json!("Antigravity Companion Settings")
        );
        assert!(
            res_list["result"]["resources"][0]["icons"][0]["src"]
                .as_str()
                .is_some_and(|s| s.starts_with("data:image/svg+xml;base64,"))
        );

        // 5. resources/read (valid + unknown URI)
        send_msg(
            &mut write_half,
            &json!({
                "jsonrpc": "2.0",
                "id": 4,
                "method": "resources/read",
                "params": { "uri": UI_RESOURCE_URI }
            }),
        )
        .await;
        let res_read = recv_msg(&mut reader).await;
        assert_eq!(
            res_read["result"]["contents"][0]["mimeType"],
            json!(UI_MIME_TYPE)
        );

        send_msg(
            &mut write_half,
            &json!({
                "jsonrpc": "2.0",
                "id": 404,
                "method": "resources/read",
                "params": { "uri": "ui://companion/does-not-exist.html" }
            }),
        )
        .await;
        let res_not_found = recv_msg(&mut reader).await;
        assert_eq!(res_not_found["error"]["code"], json!(-32002));

        // 6a. Verify save_settings rejects legacy flat / unknown fields at the JSON-RPC boundary
        send_msg(
            &mut write_half,
            &json!({
                "jsonrpc": "2.0",
                "id": 50,
                "method": "tools/call",
                "params": {
                    "name": "save_settings",
                    "arguments": {
                        "listen_port": closed_port
                    }
                }
            }),
        )
        .await;
        let bad_save_resp = recv_msg(&mut reader).await;
        let save_rejected = bad_save_resp.get("error").is_some()
            || bad_save_resp["result"]["isError"] == json!(true);
        assert!(
            save_rejected,
            "save_settings with legacy flat listen_port must be rejected: {bad_save_resp:?}"
        );
        let save_err_blob = serde_json::to_string(&bad_save_resp).expect("serialize");
        assert!(
            save_err_blob.contains("unknown field") && save_err_blob.contains("listen_port"),
            "expected unknown field error for listen_port, got: {save_err_blob}"
        );

        // 6. tools/call save_settings first (sets closed_port so open_settings never probes ambient host port)
        let wire_secret = "AIzaSyWireProtocolSecret123";
        send_msg(
            &mut write_half,
            &json!({
                "jsonrpc": "2.0",
                "id": 5,
                "method": "tools/call",
                "params": {
                    "name": "save_settings",
                    "arguments": {
                        "proxy": { "port": closed_port },
                        "providers": {
                            "gemini": {
                                "api_key": { "set": wire_secret },
                                "base_url": format!("http://127.0.0.1:{upstream_port}"),
                                "default_model": "gemini-3.8-flash"
                            }
                        }
                    }
                }
            }),
        )
        .await;
        let save_resp = recv_msg(&mut reader).await;
        assert_eq!(save_resp["result"]["isError"], json!(false));
        assert!(
            !serde_json::to_string(&save_resp)
                .unwrap()
                .contains(wire_secret)
        );

        // 7. tools/call open_settings with {}
        send_msg(
            &mut write_half,
            &json!({
                "jsonrpc": "2.0",
                "id": 6,
                "method": "tools/call",
                "params": { "name": "open_settings", "arguments": {} }
            }),
        )
        .await;
        let open_resp = recv_msg(&mut reader).await;
        assert_eq!(open_resp["result"]["isError"], json!(false));
        assert!(
            !serde_json::to_string(&open_resp)
                .unwrap()
                .contains(wire_secret)
        );

        // 8. tools/call get_settings
        send_msg(
            &mut write_half,
            &json!({
                "jsonrpc": "2.0",
                "id": 7,
                "method": "tools/call",
                "params": { "name": "get_settings", "arguments": {} }
            }),
        )
        .await;
        let get_resp = recv_msg(&mut reader).await;
        assert_eq!(get_resp["result"]["isError"], json!(false));
        assert_eq!(
            get_resp["result"]["structuredContent"]["config"]["providers"]["gemini"]["api_key_hint"],
            json!("AIza...t123")
        );
        assert_eq!(
            get_resp["result"]["structuredContent"]["supported_models"]
                .as_array()
                .map(Vec::len),
            Some(5)
        );
        assert_eq!(
            get_resp["result"]["structuredContent"]["platform"],
            json!(HostPlatform::current().as_str())
        );
        assert!(
            !serde_json::to_string(&get_resp)
                .unwrap()
                .contains(wire_secret)
        );

        // 9. tools/call test_connection
        send_msg(
            &mut write_half,
            &json!({
                "jsonrpc": "2.0",
                "id": 8,
                "method": "tools/call",
                "params": { "name": "test_connection", "arguments": {} }
            }),
        )
        .await;
        let test_resp = recv_msg(&mut reader).await;
        assert_eq!(test_resp["result"]["isError"], json!(false));
        assert_eq!(test_resp["result"]["structuredContent"]["ok"], json!(true));
        assert!(
            !serde_json::to_string(&test_resp)
                .unwrap()
                .contains(wire_secret)
        );

        drop(write_half);
        drop(reader);
        server_task.await.expect("server shutdown");
        upstream_handle.abort();
    }

    #[cfg(unix)]
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn ui_mcp_rejects_post_startup_fifo_and_oversized_config_swap() {
        let tmp = TempDir::new().expect("tempdir");
        let cfg_path = tmp.path().join("config.json");
        let closed_port = free_closed_port().await;

        // Start server with nonexistent config (valid first-run UX).
        let server = CompanionUiServer::new(cfg_path.clone(), None)
            .expect("server")
            .with_ui_html_override(None)
            .with_env_api_key_fallback(false);

        // 1. Post-startup swap to oversized (2 MiB) config file -> immediate tool error.
        let mut oversized = br#"{"schema_version":1}"#.to_vec();
        oversized.resize(2 * 1024 * 1024, b' ');
        fs::write(&cfg_path, oversized).expect("write oversized config");

        let get_big = tokio::time::timeout(Duration::from_secs(2), server.get_settings())
            .await
            .expect("get_settings timed out on oversized config")
            .expect("get_settings result");
        assert_eq!(get_big.is_error, Some(true));

        let save_big = tokio::time::timeout(
            Duration::from_secs(2),
            server.save_settings(Parameters(SaveSettingsArgs {
                proxy: Some(ProxySettingsPatch {
                    port: Some(closed_port),
                    ..Default::default()
                }),
                ..Default::default()
            })),
        )
        .await
        .expect("save_settings timed out on oversized config")
        .expect("save_settings result");
        assert_eq!(save_big.is_error, Some(true));

        // 2. Post-startup swap to FIFO -> immediate tool error without hanging.
        fs::remove_file(&cfg_path).expect("remove oversized");
        let status = Command::new("mkfifo")
            .arg(&cfg_path)
            .status()
            .expect("run mkfifo");
        assert!(status.success(), "mkfifo failed");

        let get_fifo = tokio::time::timeout(Duration::from_secs(2), server.get_settings())
            .await
            .expect("get_settings hung on FIFO config")
            .expect("get_settings result");
        assert_eq!(get_fifo.is_error, Some(true));

        let save_fifo = tokio::time::timeout(
            Duration::from_secs(2),
            server.save_settings(Parameters(SaveSettingsArgs {
                proxy: Some(ProxySettingsPatch {
                    port: Some(closed_port),
                    ..Default::default()
                }),
                ..Default::default()
            })),
        )
        .await
        .expect("save_settings hung on FIFO config")
        .expect("save_settings result");
        assert_eq!(save_fifo.is_error, Some(true));
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn concurrent_save_settings_calls_preserve_disjoint_field_updates_and_clear_api_key() {
        let closed_port = free_closed_port().await;

        // Part A: 10 trials of 3 concurrent disjoint save_settings updates -> 0 lost updates.
        for trial in 0..10u16 {
            let tmp = TempDir::new().expect("tempdir");
            let cfg_path = tmp.path().join("config.json");
            let initial = config_with_port(closed_port);
            initial.save_secure(&cfg_path).expect("seed config");

            let server = CompanionUiServer::new(cfg_path.clone(), None)
                .expect("server")
                .with_ui_html_override(None)
                .with_env_api_key_fallback(false);

            let key_value = format!("AIzaSyConcurrentKeyTrial{trial:02}");
            let model_value = format!("gemini-3.8-flash-t{trial}");
            let new_port = free_closed_port().await;

            let s1 = server.clone();
            let s2 = server.clone();
            let s3 = server.clone();
            let key_clone = key_value.clone();
            let model_clone = model_value.clone();

            let (r1, r2, r3) = tokio::join!(
                tokio::spawn(async move {
                    s1.save_settings(Parameters(SaveSettingsArgs {
                        providers: Some(ProvidersSettingsPatch {
                            gemini: Some(GeminiSettingsPatch {
                                api_key: Some(ApiKeyPatch::Set(key_clone)),
                                ..Default::default()
                            }),
                        }),
                        ..Default::default()
                    }))
                    .await
                    .expect("save key")
                }),
                tokio::spawn(async move {
                    s2.save_settings(Parameters(SaveSettingsArgs {
                        providers: Some(ProvidersSettingsPatch {
                            gemini: Some(GeminiSettingsPatch {
                                default_model: Some(model_clone),
                                ..Default::default()
                            }),
                        }),
                        ..Default::default()
                    }))
                    .await
                    .expect("save model")
                }),
                tokio::spawn(async move {
                    s3.save_settings(Parameters(SaveSettingsArgs {
                        proxy: Some(ProxySettingsPatch {
                            port: Some(new_port),
                            ..Default::default()
                        }),
                        ..Default::default()
                    }))
                    .await
                    .expect("save port")
                }),
            );

            assert_ne!(r1.expect("join 1").is_error, Some(true));
            assert_ne!(r2.expect("join 2").is_error, Some(true));
            assert_ne!(r3.expect("join 3").is_error, Some(true));

            let final_cfg = Config::load_or_default(&cfg_path).expect("load final config");
            assert_eq!(
                final_cfg
                    .providers
                    .gemini
                    .api_key
                    .as_ref()
                    .map(ApiKey::expose),
                Some(key_value.as_str()),
                "trial {trial}: lost api_key update"
            );
            assert_eq!(
                final_cfg.providers.gemini.default_model, model_value,
                "trial {trial}: lost default_model update"
            );
            assert_eq!(
                final_cfg.proxy.port.get(),
                new_port,
                "trial {trial}: lost proxy.port update"
            );
        }

        // Part B: 10 trials of concurrent ApiKeyPatch::Clear vs non-key model update -> zero key resurrection.
        for trial in 0..10u16 {
            let tmp = TempDir::new().expect("tempdir");
            let cfg_path = tmp.path().join("config.json");
            let mut initial = config_with_port(closed_port);
            initial.providers.gemini.api_key = Some(
                ApiKey::parse_strict(&format!("AIzaSyMustStayCleared{trial:02}"))
                    .expect("valid key"),
            );
            initial.save_secure(&cfg_path).expect("seed config");

            let server = CompanionUiServer::new(cfg_path.clone(), None)
                .expect("server")
                .with_ui_html_override(None)
                .with_env_api_key_fallback(false);

            let model_value = format!("gemini-4-hydrogen-cleared-{trial}");
            let s1 = server.clone();
            let s2 = server.clone();
            let model_clone = model_value.clone();

            let (r1, r2) = tokio::join!(
                tokio::spawn(async move {
                    s1.save_settings(Parameters(SaveSettingsArgs {
                        providers: Some(ProvidersSettingsPatch {
                            gemini: Some(GeminiSettingsPatch {
                                api_key: Some(ApiKeyPatch::Clear),
                                ..Default::default()
                            }),
                        }),
                        ..Default::default()
                    }))
                    .await
                    .expect("clear key")
                }),
                tokio::spawn(async move {
                    s2.save_settings(Parameters(SaveSettingsArgs {
                        providers: Some(ProvidersSettingsPatch {
                            gemini: Some(GeminiSettingsPatch {
                                default_model: Some(model_clone),
                                ..Default::default()
                            }),
                        }),
                        ..Default::default()
                    }))
                    .await
                    .expect("save model")
                }),
            );

            assert_ne!(r1.expect("join clear").is_error, Some(true));
            assert_ne!(r2.expect("join model").is_error, Some(true));

            let final_cfg = Config::load_or_default(&cfg_path).expect("load final config");
            assert_eq!(
                final_cfg.providers.gemini.api_key, None,
                "trial {trial}: cleared API key was resurrected by concurrent non-key save"
            );
            assert_eq!(
                final_cfg.providers.gemini.default_model, model_value,
                "trial {trial}: lost default_model update alongside ApiKeyPatch::Clear"
            );
        }
    }

    #[cfg(unix)]
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn ui_resource_override_rejects_fifo_without_blocking() {
        let tmp = TempDir::new().expect("tempdir");
        let fifo_path = tmp.path().join("ui.fifo");
        let status = Command::new("mkfifo")
            .arg(&fifo_path)
            .status()
            .expect("run mkfifo");
        assert!(status.success(), "mkfifo failed");

        let server = CompanionUiServer::new(tmp.path().join("config.json"), None)
            .expect("server")
            .with_ui_html_override(Some(fifo_path));

        let err = tokio::time::timeout(
            Duration::from_secs(2),
            server.read_ui_resource(UI_RESOURCE_URI),
        )
        .await
        .expect("read_ui_resource hung on FIFO override")
        .expect_err("FIFO override must fail");
        assert_eq!(err.code, ErrorCode::INTERNAL_ERROR);
    }

    #[tokio::test]
    async fn stats_snapshot_deserializes_minimal_usage_only_and_full_structured_json() {
        let minimal: StatsSnapshot = serde_json::from_value(json!({
            "usage": {
                "requests": 3,
                "prompt_tokens": 100,
                "cached_tokens": 50,
                "output_tokens": 20,
                "reasoning_tokens": 5,
                "cache_hit_ratio": 0.5
            }
        }))
        .expect("minimal usage-only stats should deserialize");
        assert_eq!(minimal.usage.requests, 3);
        assert_eq!(minimal.cloudcode.last_catalog_refresh_at_ms, None);
        assert_eq!(minimal.cloudcode.last_catalog_included_proxy_models, None);
        assert_eq!(minimal.cloudcode.mode, "");
        assert_eq!(minimal.cloudcode.resolved_upstream, None);

        let extended_router = Router::new()
            .route(
                "/health",
                get(|| async {
                    axum::Json(json!({
                        "status": "ok",
                        "version": env!("CARGO_PKG_VERSION"),
                        "protocol_version": 1
                    }))
                }),
            )
            .route(
                "/api/stats",
                get(|| async {
                    axum::Json(json!({
                        "usage": {
                            "requests": 9,
                            "prompt_tokens": 2000,
                            "cached_tokens": 1200,
                            "output_tokens": 400,
                            "reasoning_tokens": 80,
                            "cache_hit_ratio": 0.6
                        },
                        "cloudcode": {
                            "mode": "auto",
                            "resolved_upstream": "https://daily-cloudcode-pa.googleapis.com",
                            "last_catalog_refresh_at_ms": 1735689600123u64,
                            "last_catalog_included_proxy_models": true
                        }
                    }))
                }),
            );
        let (port, handle) = spawn_mock_http(extended_router).await;
        let tmp = TempDir::new().expect("tempdir");
        let cfg_path = tmp.path().join("config.json");
        let cfg = config_with_port(port);
        cfg.save_secure(&cfg_path).expect("save");

        let server = CompanionUiServer::new(cfg_path, None).expect("server");
        let res = server.get_settings().await.expect("get_settings");
        assert_eq!(res.is_error, Some(false));
        let snap: SettingsSnapshot =
            serde_json::from_value(res.structured_content.expect("structured")).expect("parse");
        let stats = snap.stats.expect("extended stats");
        assert_eq!(stats.usage.requests, 9);
        assert_eq!(
            stats.cloudcode.last_catalog_refresh_at_ms,
            Some(1735689600123)
        );
        assert_eq!(
            stats.cloudcode.last_catalog_included_proxy_models,
            Some(true)
        );
        assert_eq!(stats.cloudcode.mode, "auto");
        assert_eq!(
            stats.cloudcode.resolved_upstream.as_deref(),
            Some("https://daily-cloudcode-pa.googleapis.com")
        );
        handle.abort();
    }

    #[tokio::test]
    async fn save_settings_rejects_placeholder_api_key_and_heals_legacy_disk_placeholder() {
        let tmp = TempDir::new().expect("tempdir");
        let cfg_path = tmp.path().join("config.json");
        let closed_port = free_closed_port().await;

        let server = CompanionUiServer::new(cfg_path.clone(), None)
            .expect("server")
            .with_env_api_key_fallback(false);

        for raw in [
            companion_core::PROXY_PLACEHOLDER_API_KEY.to_string(),
            format!("  {}  ", companion_core::PROXY_PLACEHOLDER_API_KEY),
        ] {
            let res = server
                .save_settings(Parameters(SaveSettingsArgs {
                    providers: Some(ProvidersSettingsPatch {
                        gemini: Some(GeminiSettingsPatch {
                            api_key: Some(ApiKeyPatch::Set(raw)),
                            ..Default::default()
                        }),
                    }),
                    ..Default::default()
                }))
                .await
                .expect("save_settings error result");
            assert_eq!(res.is_error, Some(true));
            let text = serde_json::to_string(&res).expect("serialize");
            assert!(
                text.contains("placeholder"),
                "expected placeholder rejection message, got: {text}"
            );
        }
        assert!(
            !cfg_path.exists(),
            "rejected placeholder saves must not create config.json"
        );

        // Seed legacy config.json on disk containing the placeholder sentinel via raw JSON.
        write_raw_secure(
            &cfg_path,
            &json!({
                "schema_version": 1,
                "proxy": { "host": "127.0.0.1", "port": closed_port },
                "providers": {
                    "gemini": {
                        "api_key": companion_core::PROXY_PLACEHOLDER_API_KEY
                    }
                }
            })
            .to_string(),
        );

        let get_res = server.get_settings().await.expect("get_settings");
        assert_eq!(get_res.is_error, Some(false));
        let get_snap: SettingsSnapshot =
            serde_json::from_value(get_res.structured_content.expect("structured")).expect("parse");
        assert!(!get_snap.config.providers.gemini.api_key_configured);
        assert_eq!(get_snap.config.providers.gemini.api_key_hint, None);

        // Saving an unrelated field must scrub the legacy placeholder on disk.
        let heal_res = server
            .save_settings(Parameters(SaveSettingsArgs {
                providers: Some(ProvidersSettingsPatch {
                    gemini: Some(GeminiSettingsPatch {
                        default_model: Some("gemini-4-neon".to_string()),
                        ..Default::default()
                    }),
                }),
                ..Default::default()
            }))
            .await
            .expect("heal save");
        assert_eq!(heal_res.is_error, Some(false));

        let reloaded = Config::load_or_default(&cfg_path).expect("reload");
        assert_eq!(reloaded.providers.gemini.default_model, "gemini-4-neon");
        assert_eq!(
            reloaded.providers.gemini.api_key, None,
            "legacy on-disk placeholder must be scrubbed on save"
        );
        let raw_disk = fs::read_to_string(&cfg_path).expect("read healed config");
        assert!(
            !raw_disk.contains(companion_core::PROXY_PLACEHOLDER_API_KEY),
            "placeholder sentinel must be removed from disk file after save"
        );
    }

    #[tokio::test]
    async fn test_connection_never_sends_placeholder_api_key_from_config_or_env() {
        use std::sync::atomic::{AtomicUsize, Ordering};

        if std::env::var_os("AGY_COMPANION_PLACEHOLDER_ENV_CHILD").is_some() {
            let hits = Arc::new(AtomicUsize::new(0));
            let hits_clone = Arc::clone(&hits);
            let router = Router::new().route(
                "/v1beta/models",
                get(move || {
                    let hits = Arc::clone(&hits_clone);
                    async move {
                        hits.fetch_add(1, Ordering::SeqCst);
                        (StatusCode::OK, axum::Json(json!({ "models": [] })))
                    }
                }),
            );
            let (port, handle) = spawn_mock_http(router).await;
            let tmp = TempDir::new().expect("tempdir");
            let cfg_path = tmp.path().join("config.json");
            let mut cfg = Config::default();
            cfg.providers.gemini.base_url = format!("http://127.0.0.1:{port}");
            cfg.providers.gemini.api_key = None;
            cfg.save_secure(&cfg_path).expect("save");

            let server = CompanionUiServer::new(cfg_path, None).expect("server");
            let res = server.test_connection().await.expect("test_connection");
            assert_eq!(res.is_error, Some(false));
            let payload: TestConnectionPayload =
                serde_json::from_value(res.structured_content.expect("structured")).expect("parse");
            assert!(!payload.ok, "placeholder env key must not pass connection");
            assert_eq!(
                hits.load(Ordering::SeqCst),
                0,
                "placeholder env key must never hit upstream"
            );
            handle.abort();
            return;
        }

        // Case 1: Legacy placeholder stored in config.json (env fallback disabled) -> 0 upstream hits.
        let hits = Arc::new(AtomicUsize::new(0));
        let hits_clone = Arc::clone(&hits);
        let router = Router::new().route(
            "/v1beta/models",
            get(move || {
                let hits = Arc::clone(&hits_clone);
                async move {
                    hits.fetch_add(1, Ordering::SeqCst);
                    (StatusCode::OK, axum::Json(json!({ "models": [] })))
                }
            }),
        );
        let (port, handle) = spawn_mock_http(router).await;
        let tmp = TempDir::new().expect("tempdir");
        let cfg_path = tmp.path().join("config.json");
        write_raw_secure(
            &cfg_path,
            &json!({
                "schema_version": 1,
                "providers": {
                    "gemini": {
                        "base_url": format!("http://127.0.0.1:{port}"),
                        "api_key": companion_core::PROXY_PLACEHOLDER_API_KEY
                    }
                }
            })
            .to_string(),
        );

        let server = CompanionUiServer::new(cfg_path, None)
            .expect("server")
            .with_env_api_key_fallback(false);
        let res = server.test_connection().await.expect("test_connection");
        assert_eq!(res.is_error, Some(false));
        let payload: TestConnectionPayload =
            serde_json::from_value(res.structured_content.expect("structured")).expect("parse");
        assert!(!payload.ok);
        assert_eq!(
            hits.load(Ordering::SeqCst),
            0,
            "placeholder config key must never hit upstream"
        );
        handle.abort();

        // Case 2: Child-process re-exec (zero unsafe) with GEMINI_API_KEY=agy-companion-local.
        let exe = std::env::current_exe().expect("current_exe");
        let output = Command::new(exe)
            .arg("--exact")
            .arg(
                "server::tests::test_connection_never_sends_placeholder_api_key_from_config_or_env",
            )
            .arg("--nocapture")
            .env("AGY_COMPANION_PLACEHOLDER_ENV_CHILD", "1")
            .env("GEMINI_API_KEY", companion_core::PROXY_PLACEHOLDER_API_KEY)
            .output()
            .expect("spawn child test");
        assert!(
            output.status.success(),
            "placeholder env child failed:\nstdout: {}\nstderr: {}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
    }

    #[tokio::test]
    async fn set_launcher_integration_enables_disables_and_never_follows_rc_symlink() {
        let tmp = TempDir::new().expect("tempdir");
        let cfg_path = tmp.path().join("config.json");
        let cfg = config_with_port(free_closed_port().await);
        cfg.save_secure(&cfg_path).expect("save config");

        let rc_path = tmp.path().join(".zlogin");
        let data_dir = tmp.path().join("data");
        let fake_bin = tmp.path().join("agy-companion");
        fs::write(&fake_bin, "#!/bin/sh\n").expect("write bin");

        let opts = IntegrationOptions {
            shell: Some(companion_core::ShellKind::Zsh),
            rc_file: Some(rc_path.clone()),
            binary_path: Some(fake_bin.clone()),
            data_dir: Some(data_dir.clone()),
            follow_rc_symlink: false,
        };
        let server = CompanionUiServer::new(cfg_path.clone(), None)
            .expect("server")
            .with_integration_options(opts);

        // Enable on regular RC file
        let enabled_res = server
            .set_launcher_integration(Parameters(SetLauncherIntegrationArgs { enabled: true }))
            .await
            .expect("enable");
        assert_eq!(enabled_res.is_error, Some(false));
        let snap: SettingsSnapshot =
            serde_json::from_value(enabled_res.structured_content.expect("sc")).expect("parse");
        if cfg!(windows) {
            assert_eq!(
                snap.launcher_integration.state(),
                IntegrationState::Unsupported
            );
            return;
        }
        assert!(snap.launcher_integration.enabled);
        assert!(matches!(
            snap.launcher_integration.state(),
            IntegrationState::Enabled | IntegrationState::Active
        ));
        assert!(rc_path.is_file());
        assert!(data_dir.join("env.sh").is_file());

        // Disable removes empty .zlogin and env.sh
        let disabled_res = server
            .set_launcher_integration(Parameters(SetLauncherIntegrationArgs { enabled: false }))
            .await
            .expect("disable");
        assert_eq!(disabled_res.is_error, Some(false));
        let snap_off: SettingsSnapshot =
            serde_json::from_value(disabled_res.structured_content.expect("sc")).expect("parse");
        assert!(!snap_off.launcher_integration.enabled);
        assert_eq!(
            snap_off.launcher_integration.state(),
            IntegrationState::Disabled
        );
        assert!(!rc_path.exists());
        assert!(!data_dir.join("env.sh").exists());

        // Symlinked RC file: NEVER followed even if override tried follow_rc_symlink = true
        #[cfg(unix)]
        {
            use std::os::unix::fs::symlink;
            let managed_target = tmp.path().join("managed-dotfiles-zlogin");
            fs::write(&managed_target, "# nix managed\n").expect("write target");
            symlink(&managed_target, &rc_path).expect("symlink");

            let server_force = CompanionUiServer::new(cfg_path, None)
                .expect("server")
                .with_integration_options(IntegrationOptions {
                    shell: Some(companion_core::ShellKind::Zsh),
                    rc_file: Some(rc_path.clone()),
                    binary_path: Some(fake_bin),
                    data_dir: Some(data_dir.clone()),
                    follow_rc_symlink: true, // MUST be overridden to false inside tool!
                });
            let sym_res = server_force
                .set_launcher_integration(Parameters(SetLauncherIntegrationArgs { enabled: true }))
                .await
                .expect("enable symlink");
            assert_eq!(sym_res.is_error, Some(false));
            let sym_snap: SettingsSnapshot =
                serde_json::from_value(sym_res.structured_content.expect("sc")).expect("parse");
            assert!(!sym_snap.launcher_integration.enabled);
            assert_eq!(
                sym_snap.launcher_integration.state(),
                IntegrationState::ManualRequired
            );
            assert!(sym_snap.launcher_integration.manual_snippet.is_some());
            assert_eq!(
                sym_snap.launcher_integration.symlink_target.as_deref(),
                Some(managed_target.as_path())
            );
            assert_eq!(
                fs::read_to_string(&managed_target).expect("read target"),
                "# nix managed\n"
            );
            assert!(data_dir.join("env.sh").is_file());
        }
    }

    #[tokio::test]
    async fn settings_snapshot_populates_cloudcode_status_and_marks_hybrid_active_from_catalog_refresh()
     {
        let router = Router::new()
            .route(
                "/health",
                get(|| async {
                    axum::Json(json!({
                        "status": "ok",
                        "version": env!("CARGO_PKG_VERSION"),
                        "protocol_version": 1
                    }))
                }),
            )
            .route(
                "/api/stats",
                get(|| async {
                    axum::Json(json!({
                        "usage": {
                            "requests": 3,
                            "prompt_tokens": 100,
                            "cached_tokens": 50,
                            "output_tokens": 25,
                            "reasoning_tokens": 0,
                            "cache_hit_ratio": 0.5
                        },
                        "cloudcode": {
                            "mode": "auto",
                            "resolved_upstream": "https://daily-cloudcode-pa.googleapis.com",
                            "last_catalog_refresh_at_ms": 1735689600123_u64,
                            "last_catalog_included_proxy_models": true
                        }
                    }))
                }),
            );
        let (port, handle) = spawn_mock_http(router).await;

        let tmp = TempDir::new().expect("tempdir");
        let cfg_path = tmp.path().join("config.json");
        let cfg = config_with_port(port);
        cfg.save_secure(&cfg_path).expect("save config");

        let rc_path = tmp.path().join(".zlogin");
        let data_dir = tmp.path().join("data");
        let fake_bin = tmp.path().join("agy-companion");
        fs::write(&fake_bin, "#!/bin/sh\n").expect("write bin");
        let opts = IntegrationOptions {
            shell: Some(companion_core::ShellKind::Zsh),
            rc_file: Some(rc_path),
            binary_path: Some(fake_bin),
            data_dir: Some(data_dir),
            follow_rc_symlink: false,
        };
        if !cfg!(windows) {
            companion_core::integration::enable(&opts).expect("enable integration");
        }

        let server = CompanionUiServer::new(cfg_path, None)
            .expect("server")
            .with_integration_options(opts);
        let res = server.get_settings().await.expect("get_settings");
        assert_eq!(res.is_error, Some(false));
        let snap: SettingsSnapshot =
            serde_json::from_value(res.structured_content.expect("sc")).expect("parse");

        assert_eq!(
            snap.cloudcode,
            CloudCodeStatus {
                mode: "auto".to_string(),
                configured_upstream: "auto".to_string(),
                resolved_upstream: Some("https://daily-cloudcode-pa.googleapis.com".to_string()),
            }
        );
        assert_ne!(snap.cloudcode.resolved_upstream.as_deref(), Some("auto"));
        assert!(snap.launcher_integration.hybrid_active);
        if !cfg!(windows) {
            assert_eq!(snap.launcher_integration.state(), IntegrationState::Active);
        }
        assert_eq!(snap.supported_models, curated_model_presets());
        assert_eq!(snap.platform, HostPlatform::current());
        handle.abort();
    }

    #[tokio::test]
    async fn concurrent_set_launcher_integration_calls_are_serialized_without_corruption() {
        let tmp = TempDir::new().expect("tempdir");
        let cfg_path = tmp.path().join("config.json");
        let cfg = config_with_port(free_closed_port().await);
        cfg.save_secure(&cfg_path).expect("save config");

        let rc_path = tmp.path().join(".zlogin");
        fs::write(&rc_path, "# user line\n").expect("seed rc");
        let data_dir = tmp.path().join("data");
        let fake_bin = tmp.path().join("agy-companion");
        fs::write(&fake_bin, "#!/bin/sh\n").expect("write bin");

        let server = Arc::new(
            CompanionUiServer::new(cfg_path, None)
                .expect("server")
                .with_integration_options(IntegrationOptions {
                    shell: Some(companion_core::ShellKind::Zsh),
                    rc_file: Some(rc_path.clone()),
                    binary_path: Some(fake_bin),
                    data_dir: Some(data_dir),
                    follow_rc_symlink: false,
                }),
        );

        let mut tasks = Vec::new();
        for idx in 0..8 {
            let srv = Arc::clone(&server);
            let enable_flag = idx % 2 == 0;
            tasks.push(tokio::spawn(async move {
                srv.set_launcher_integration(Parameters(SetLauncherIntegrationArgs {
                    enabled: enable_flag,
                }))
                .await
            }));
        }

        for task in tasks {
            let res = task.await.expect("join").expect("tool call");
            assert_eq!(res.is_error, Some(false));
        }

        // Final RC file must remain valid UTF-8 with user line preserved and zero corrupted markers.
        let final_content = fs::read_to_string(&rc_path).expect("read rc");
        assert!(final_content.contains("# user line"));
        let final_status = server.get_settings().await.expect("get_settings");
        assert_eq!(final_status.is_error, Some(false));
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn set_launcher_integration_disable_on_symlink_and_fish_manual_required_copy_is_accurate()
    {
        use std::os::unix::fs::symlink;

        let tmp = TempDir::new().expect("tempdir");
        let cfg_path = tmp.path().join("config.json");
        let cfg = config_with_port(free_closed_port().await);
        cfg.save_secure(&cfg_path).expect("save config");

        let data_dir = tmp.path().join("data");
        let fake_bin = tmp.path().join("agy-companion");
        fs::write(&fake_bin, "#!/bin/sh\n").expect("write bin");

        // 1. Zsh symlink already containing valid markers: enable -> disable neutralizes env.sh
        let managed_zlogin = tmp.path().join("nix-zlogin");
        let snippet = companion_core::integration::render_rc_snippet(
            companion_core::ShellKind::Zsh,
            &data_dir.join("env.sh"),
        )
        .expect("snippet");
        fs::write(&managed_zlogin, format!("# nix\n{snippet}\n")).expect("write target");
        let rc_path = tmp.path().join(".zlogin");
        symlink(&managed_zlogin, &rc_path).expect("symlink");

        let server = CompanionUiServer::new(cfg_path.clone(), None)
            .expect("server")
            .with_integration_options(IntegrationOptions {
                shell: Some(companion_core::ShellKind::Zsh),
                rc_file: Some(rc_path),
                binary_path: Some(fake_bin.clone()),
                data_dir: Some(data_dir.clone()),
                follow_rc_symlink: false,
            });

        let en_res = server
            .set_launcher_integration(Parameters(SetLauncherIntegrationArgs { enabled: true }))
            .await
            .expect("enable");
        assert_eq!(en_res.is_error, Some(false));
        assert!(data_dir.join("env.sh").is_file());

        let dis_res = server
            .set_launcher_integration(Parameters(SetLauncherIntegrationArgs { enabled: false }))
            .await
            .expect("disable");
        assert_eq!(dis_res.is_error, Some(false));
        assert!(!data_dir.join("env.sh").exists());
        let dis_snap: SettingsSnapshot =
            serde_json::from_value(dis_res.structured_content.expect("sc")).expect("parse");
        assert!(!dis_snap.launcher_integration.enabled);
        assert_eq!(
            dis_snap.launcher_integration.state(),
            IntegrationState::ManualRequired
        );
        assert!(dis_snap.launcher_integration.manual_snippet.is_none());
        let dis_text = dis_res
            .content
            .first()
            .and_then(|c| c.as_text())
            .map(|t| t.text.as_str())
            .unwrap_or("");
        assert!(
            !dis_text.contains("env.sh prepared"),
            "disable summary must not claim env.sh prepared: {dis_text}"
        );
        assert!(
            dis_text.contains("neutralized") && dis_text.contains("manually"),
            "disable summary should mention neutralized and manual removal: {dis_text}"
        );

        // 2. Fish symlink without markers: enable must not claim env.sh prepared
        let managed_fish = tmp.path().join("nix-fish");
        fs::write(&managed_fish, "# nix fish\n").expect("write fish target");
        let fish_rc = tmp.path().join("agy-companion.fish");
        symlink(&managed_fish, &fish_rc).expect("fish symlink");

        let fish_server = CompanionUiServer::new(cfg_path, None)
            .expect("fish server")
            .with_integration_options(IntegrationOptions {
                shell: Some(companion_core::ShellKind::Fish),
                rc_file: Some(fish_rc),
                binary_path: Some(fake_bin.clone()),
                data_dir: Some(data_dir),
                follow_rc_symlink: false,
            });
        let fish_res = fish_server
            .set_launcher_integration(Parameters(SetLauncherIntegrationArgs { enabled: true }))
            .await
            .expect("fish enable");
        assert_eq!(fish_res.is_error, Some(false));
        let fish_text = fish_res
            .content
            .first()
            .and_then(|c| c.as_text())
            .map(|t| t.text.as_str())
            .unwrap_or("");
        assert!(
            !fish_text.contains("env.sh prepared"),
            "Fish enable summary must not mention env.sh prepared: {fish_text}"
        );
        assert!(
            fish_text.contains("manual shell config step"),
            "Fish enable summary should mention manual shell config step: {fish_text}"
        );

        // 3. Fish symlink with valid markers: disable leaves hook enabled & must not claim neutralized
        let fish_snippet = companion_core::integration::render_rc_snippet(
            companion_core::ShellKind::Fish,
            &fake_bin,
        )
        .expect("fish snippet");
        fs::write(&managed_fish, format!("# nix fish\n{fish_snippet}\n"))
            .expect("write fish target with markers");
        let fish_dis_res = fish_server
            .set_launcher_integration(Parameters(SetLauncherIntegrationArgs { enabled: false }))
            .await
            .expect("fish disable");
        assert_eq!(fish_dis_res.is_error, Some(false));
        let fish_dis_snap: SettingsSnapshot =
            serde_json::from_value(fish_dis_res.structured_content.expect("sc")).expect("parse");
        assert!(
            fish_dis_snap.launcher_integration.enabled,
            "Fish symlink disable leaves hook active because Fish has no env.sh"
        );
        assert_eq!(
            fish_dis_snap.launcher_integration.state(),
            IntegrationState::ManualRequired
        );
        let fish_dis_text = fish_dis_res
            .content
            .first()
            .and_then(|c| c.as_text())
            .map(|t| t.text.as_str())
            .unwrap_or("");
        assert!(
            !fish_dis_text.contains("neutralized"),
            "Fish symlink disable summary must not claim hook neutralized: {fish_dis_text}"
        );
        assert!(
            fish_dis_text.contains("could not be removed automatically"),
            "Fish symlink disable summary should explain automatic removal could not occur: {fish_dis_text}"
        );
    }

    #[test]
    fn save_settings_args_denies_unknown_fields_at_every_nesting_level_and_api_key_patch_variants()
    {
        // Valid payloads
        assert_eq!(
            serde_json::from_value::<SaveSettingsArgs>(json!({})).expect("empty patch"),
            SaveSettingsArgs::default()
        );
        let clear_patch: SaveSettingsArgs = serde_json::from_value(json!({
            "providers": { "gemini": { "api_key": "clear" } }
        }))
        .expect("clear patch");
        assert_eq!(
            clear_patch
                .providers
                .as_ref()
                .and_then(|p| p.gemini.as_ref())
                .and_then(|g| g.api_key.as_ref()),
            Some(&ApiKeyPatch::Clear)
        );
        let set_patch: SaveSettingsArgs = serde_json::from_value(json!({
            "providers": { "gemini": { "api_key": { "set": "AIzaSyValidPatchKey123" } } }
        }))
        .expect("set patch");
        assert_eq!(
            set_patch
                .providers
                .as_ref()
                .and_then(|p| p.gemini.as_ref())
                .and_then(|g| g.api_key.as_ref()),
            Some(&ApiKeyPatch::Set("AIzaSyValidPatchKey123".to_string()))
        );

        // Unknown fields at every nesting level, legacy flat keys, and malformed api_key variants
        let rejected_payloads = [
            json!({ "unknown_top": 1 }),
            json!({ "listen_port": 24141 }),
            json!({ "api_key": "AIzaSyFlatKey123456" }),
            json!({ "clear_api_key": true }),
            json!({ "proxy": { "port": 24141, "extra": true } }),
            json!({ "cloudcode": { "upstream": "auto", "extra": 1 } }),
            json!({ "providers": { "openai": {} } }),
            json!({ "providers": { "gemini": { "timeout_secs": 900 } } }),
            json!({ "providers": { "gemini": { "unknown": "x" } } }),
            json!({ "providers": { "gemini": { "api_key": { "set": "k", "extra": 1 } } } }),
            json!({ "providers": { "gemini": { "api_key": { "clear": true } } } }),
            json!({ "providers": { "gemini": { "api_key": "other" } } }),
            json!({ "providers": { "gemini": { "api_key": {} } } }),
        ];

        for payload in rejected_payloads {
            assert!(
                serde_json::from_value::<SaveSettingsArgs>(payload.clone()).is_err(),
                "expected rejection for payload: {payload}"
            );
        }
    }

    #[tokio::test]
    async fn settings_snapshot_json_exposes_nested_config_structured_stats_supported_models_and_platform()
     {
        let router = Router::new()
            .route(
                "/health",
                get(|| async {
                    axum::Json(json!({
                        "status": "ok",
                        "version": env!("CARGO_PKG_VERSION"),
                        "protocol_version": 1
                    }))
                }),
            )
            .route(
                "/api/stats",
                get(|| async {
                    // Simulate stale stats claiming mode="fixed" with resolved_upstream=null
                    // while config is CloudCodeUpstream::Auto -> resolved_upstream MUST NEVER be "auto".
                    axum::Json(json!({
                        "usage": {
                            "requests": 1,
                            "prompt_tokens": 10,
                            "cached_tokens": 5,
                            "output_tokens": 2,
                            "reasoning_tokens": 0,
                            "cache_hit_ratio": 0.5
                        },
                        "cloudcode": {
                            "mode": "fixed",
                            "resolved_upstream": null,
                            "last_catalog_refresh_at_ms": null,
                            "last_catalog_included_proxy_models": null
                        }
                    }))
                }),
            );
        let (port, handle) = spawn_mock_http(router).await;

        let tmp = TempDir::new().expect("tempdir");
        let cfg_path = tmp.path().join("config.json");
        let cfg = config_with_port(port);
        cfg.save_secure(&cfg_path).expect("save config");

        let server = CompanionUiServer::new(cfg_path, None)
            .expect("server")
            .with_integration_options(IntegrationOptions {
                shell: Some(companion_core::ShellKind::Zsh),
                rc_file: Some(tmp.path().join(".zlogin")),
                binary_path: Some(tmp.path().join("agy-companion")),
                data_dir: Some(tmp.path().join("data")),
                follow_rc_symlink: false,
            });
        let res = server.get_settings().await.expect("get_settings");
        assert_eq!(res.is_error, Some(false));
        let sc = res.structured_content.expect("structured_content");

        // Top-level keys
        let obj = sc.as_object().expect("snapshot object");
        for key in [
            "config",
            "binary_version",
            "proxy",
            "version_skew",
            "stats",
            "launcher_integration",
            "cloudcode",
            "supported_models",
            "platform",
        ] {
            assert!(obj.contains_key(key), "missing snapshot key {key}");
        }

        // supported_models has 5 curated presets & platform is lowercase
        let models = obj["supported_models"].as_array().expect("models array");
        assert_eq!(models.len(), 5);
        assert_eq!(obj["platform"], json!(HostPlatform::current().as_str()));

        // launcher_integration exposes canonical `state` without removed `supported`/`manual_required`
        let li = obj["launcher_integration"]
            .as_object()
            .expect("launcher_integration object");
        assert!(li.contains_key("state"));
        assert!(!li.contains_key("supported"));
        assert!(!li.contains_key("manual_required"));

        // cloudcode.resolved_upstream must NEVER be "auto"
        assert_eq!(obj["cloudcode"]["configured_upstream"], json!("auto"));
        assert!(obj["cloudcode"]["resolved_upstream"].is_null());

        handle.abort();
    }

    #[tokio::test]
    async fn save_settings_accepts_gemini_5_and_3_9_models_and_trims_proxy_host() {
        let tmp = TempDir::new().expect("tempdir");
        let cfg_path = tmp.path().join("config.json");
        let closed_port = free_closed_port().await;
        let server = CompanionUiServer::new(cfg_path.clone(), None).expect("server");

        for model in ["gemini-5-pro", "gemini-3.9-pro"] {
            let ok = server
                .save_settings(Parameters(SaveSettingsArgs {
                    proxy: Some(ProxySettingsPatch {
                        host: Some(" localhost ".to_string()),
                        port: Some(closed_port),
                    }),
                    providers: Some(ProvidersSettingsPatch {
                        gemini: Some(GeminiSettingsPatch {
                            default_model: Some(model.to_string()),
                            ..Default::default()
                        }),
                    }),
                    ..Default::default()
                }))
                .await
                .expect("save model");
            assert_eq!(ok.is_error, Some(false), "model={model} must succeed");
            let saved = Config::load_or_default(&cfg_path).expect("reload");
            assert_eq!(saved.providers.gemini.default_model, model);
            assert_eq!(saved.proxy.host, LoopbackHost::Localhost);
            assert_eq!(saved.proxy.port.get(), closed_port);
        }

        let bare = server
            .save_settings(Parameters(SaveSettingsArgs {
                providers: Some(ProvidersSettingsPatch {
                    gemini: Some(GeminiSettingsPatch {
                        default_model: Some("gemini-5".to_string()),
                        ..Default::default()
                    }),
                }),
                ..Default::default()
            }))
            .await
            .expect("bare gemini-5");
        assert_eq!(bare.is_error, Some(true), "bare gemini-5 must be rejected");
    }
}
