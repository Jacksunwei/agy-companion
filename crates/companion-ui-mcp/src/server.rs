use std::fmt;
use std::fs::File;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use companion_core::{
    Config, RedactedConfig, api_key_hint, validate_listen_host, validate_path,
    validate_upstream_url,
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

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProxyHealthPayload {
    pub reachable: bool,
    pub status: Option<String>,
    pub version: Option<String>,
    pub protocol_version: Option<u32>,
    pub error: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ProxyStatsPayload {
    pub requests: u64,
    pub prompt_tokens: u64,
    pub cached_tokens: u64,
    pub output_tokens: u64,
    pub reasoning_tokens: u64,
    pub cache_hit_ratio: f64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SettingsSnapshot {
    pub config: RedactedConfig,
    pub binary_version: String,
    pub proxy: ProxyHealthPayload,
    pub version_skew: bool,
    pub stats: Option<ProxyStatsPayload>,
}

#[derive(Clone, Default, Deserialize, JsonSchema)]
#[serde(default)]
pub struct SaveSettingsArgs {
    pub api_key: Option<String>,
    pub clear_api_key: Option<bool>,
    pub default_model: Option<String>,
    pub base_url: Option<String>,
    pub cloudcode_upstream_url: Option<String>,
    pub listen_port: Option<u16>,
    pub listen_host: Option<String>,
}

impl fmt::Debug for SaveSettingsArgs {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("SaveSettingsArgs")
            .field("api_key", &api_key_hint(self.api_key.as_deref()))
            .field("clear_api_key", &self.clear_api_key)
            .field("default_model", &self.default_model)
            .field("base_url", &self.base_url)
            .field("cloudcode_upstream_url", &self.cloudcode_upstream_url)
            .field("listen_port", &self.listen_port)
            .field("listen_host", &self.listen_host)
            .finish()
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TestConnectionPayload {
    pub ok: bool,
    pub message: String,
}

#[derive(Debug, Deserialize)]
struct RawProxyHealth {
    status: String,
    version: String,
    protocol_version: u32,
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
    tool_router: ToolRouter<Self>,
    ui_html_source: UiHtmlSource,
    #[cfg(test)]
    env_api_key_fallback: bool,
    #[cfg(test)]
    test_connection_timeout_override: Option<u64>,
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
            tool_router: Self::tool_router(),
            ui_html_source: UiHtmlSource::ProcessEnv,
            #[cfg(test)]
            env_api_key_fallback: true,
            #[cfg(test)]
            test_connection_timeout_override: None,
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
    ) -> (ProxyHealthPayload, bool, Option<ProxyStatsPayload>) {
        let health_url = format!("http://127.0.0.1:{listen_port}/health");
        match self.read_bounded_json::<RawProxyHealth>(&health_url).await {
            Ok(raw) => {
                let version_skew = raw.version != binary_version;
                let stats_url = format!("http://127.0.0.1:{listen_port}/api/stats");
                let stats = self
                    .read_bounded_json::<ProxyStatsPayload>(&stats_url)
                    .await
                    .ok();
                let proxy = ProxyHealthPayload {
                    reachable: true,
                    status: Some(raw.status),
                    version: Some(raw.version),
                    protocol_version: Some(raw.protocol_version),
                    error: None,
                };
                (proxy, version_skew, stats)
            }
            Err(err) => {
                let proxy = ProxyHealthPayload {
                    reachable: false,
                    status: None,
                    version: None,
                    protocol_version: None,
                    error: Some(err),
                };
                (proxy, false, None)
            }
        }
    }

    async fn build_snapshot(&self, cfg: &Config) -> SettingsSnapshot {
        let binary_version = env!("CARGO_PKG_VERSION").to_string();
        let (proxy, version_skew, stats) = self.probe_proxy(cfg.listen_port, &binary_version).await;
        SettingsSnapshot {
            config: cfg.redacted(),
            binary_version,
            proxy,
            version_skew,
            stats,
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

fn validate_api_key_input(raw: &str) -> Result<Option<String>, &'static str> {
    if !raw.bytes().all(|b| (0x20..=0x7E).contains(&b)) {
        return Err("api_key contains invalid header characters");
    }

    let trimmed = raw.trim_matches(' ');
    if trimmed.is_empty() {
        return Ok(None);
    }

    if !trimmed.bytes().all(|b| (0x21..=0x7E).contains(&b)) {
        return Err("api_key contains invalid header characters");
    }

    Ok(Some(trimmed.to_string()))
}

fn validate_trimmed_ascii_field(raw: &str, field_name: &str) -> Result<String, String> {
    if !raw.bytes().all(|b| (0x20..=0x7E).contains(&b)) {
        return Err(format!("{field_name} contains invalid characters"));
    }
    Ok(raw.trim_matches(' ').to_string())
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

    let key_state = match snapshot.config.api_key_hint.as_deref() {
        Some(hint) => format!("configured ({hint})"),
        None if snapshot.config.api_key_configured => "configured".to_string(),
        None => "not configured".to_string(),
    };

    format!(
        "Antigravity Companion v{} — proxy at {}:{} is {}, model {}, API key {}.",
        snapshot.binary_version,
        snapshot.config.listen_host,
        snapshot.config.listen_port,
        proxy_state,
        snapshot.config.gemini_default_model,
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
        let write_guard = self.config_write_lock.lock().await;
        let config_path = self.config_path.clone();
        let save_outcome = tokio::task::spawn_blocking(move || -> Result<Config, String> {
            let mut cfg = Config::load_or_default(&config_path).map_err(|err| err.to_string())?;

            // Validate api_key strictly (ASCII-before-trim) before inspecting clear_api_key.
            let validated_api_key = match args.api_key.as_deref() {
                Some(raw) => validate_api_key_input(raw).map_err(str::to_string)?,
                None => None,
            };

            if args.clear_api_key == Some(true) {
                if validated_api_key.is_some() {
                    return Err("cannot set both api_key and clear_api_key: true".to_string());
                }
                cfg.gemini.api_key = None;
            } else if let Some(new_key) = validated_api_key {
                cfg.gemini.api_key = Some(new_key);
            }

            if let Some(raw_model) = args.default_model.as_deref() {
                let trimmed = validate_trimmed_ascii_field(raw_model, "default_model")?;
                cfg.gemini.default_model = trimmed;
            }

            if let Some(raw_url) = args.base_url.as_deref() {
                let trimmed = validate_trimmed_ascii_field(raw_url, "base_url")?;
                validate_upstream_url(&trimmed).map_err(|err| err.to_string())?;
                cfg.gemini.base_url = trimmed;
            }

            if let Some(raw_cc_url) = args.cloudcode_upstream_url.as_deref() {
                let trimmed = validate_trimmed_ascii_field(raw_cc_url, "cloudcode_upstream_url")?;
                companion_core::validate_cloudcode_upstream_url(&trimmed)
                    .map_err(|err| err.to_string())?;
                cfg.cloudcode_upstream_url = trimmed;
            }

            if let Some(raw_host) = args.listen_host.as_deref() {
                validate_listen_host(raw_host).map_err(|err| err.to_string())?;
                cfg.listen_host = raw_host.to_string();
            }

            if let Some(port) = args.listen_port {
                if port == 0 {
                    return Err("listen_port must be between 1 and 65535".to_string());
                }
                cfg.listen_port = port;
            }

            cfg.validate().map_err(|err| err.to_string())?;
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
            .gemini
            .timeout_secs
            .clamp(1, MAX_TEST_CONNECTION_TIMEOUT_SECS);
        #[cfg(test)]
        let timeout_secs = self
            .test_connection_timeout_override
            .unwrap_or(timeout_secs);

        let provider = match GeminiProvider::new(
            &cfg.gemini.base_url,
            cfg.gemini.api_key.clone(),
            timeout_secs,
        ) {
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
                    cfg.gemini.base_url
                ),
            },
            Err(err) => TestConnectionPayload {
                ok: false,
                message: err.to_string(),
            },
        };

        test_connection_result(&payload)
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
    #[cfg(unix)]
    use std::os::unix::fs::PermissionsExt;
    use std::process::Command;

    use axum::Router;
    use axum::http::StatusCode;
    use axum::routing::get;
    use companion_core::Config;
    use rmcp::model::ErrorCode;
    use serde_json::{Value, json};
    use tempfile::TempDir;
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
    use tokio::net::TcpListener;

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

        let mut cfg = Config {
            listen_port: free_closed_port().await,
            ..Default::default()
        };
        cfg.gemini.api_key = Some(secret.to_string());
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
            assert!(snapshot.config.api_key_configured);
            assert_eq!(snapshot.config.api_key_hint.as_deref(), Some("AIza...7890"));
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
                base_url: Some("http://example.com".to_string()),
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
                api_key: Some(secret.to_string()),
                default_model: Some("gemini-3.8-flash".to_string()),
                base_url: Some("https://generativelanguage.googleapis.com".to_string()),
                cloudcode_upstream_url: Some("https://cloudcode-pa.googleapis.com".to_string()),
                listen_port: Some(closed_port),
                listen_host: Some("127.0.0.1".to_string()),
                clear_api_key: None,
            }))
            .await
            .expect("save_settings ok");
        assert_eq!(ok.is_error, Some(false));

        let serialized = serde_json::to_string(&ok).expect("serialize");
        assert!(!serialized.contains(secret));
        assert!(serialized.contains("AIza...9876"));

        let saved = Config::load_or_default(&cfg_path).expect("reload config");
        assert_eq!(saved.gemini.api_key.as_deref(), Some(secret));
        assert_eq!(saved.gemini.default_model, "gemini-3.8-flash");
        assert_eq!(saved.listen_port, closed_port);

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
    async fn save_settings_preserves_existing_key_when_omitted_or_blank_and_clears_on_flag() {
        let tmp = TempDir::new().expect("tempdir");
        let cfg_path = tmp.path().join("config.json");
        let closed_port = free_closed_port().await;
        let initial_key = "AIzaSyKeepThisSecretKey4321";

        let mut cfg = Config {
            listen_port: closed_port,
            ..Default::default()
        };
        cfg.gemini.api_key = Some(initial_key.to_string());
        cfg.save_secure(&cfg_path).expect("save initial");

        let server = CompanionUiServer::new(cfg_path.clone(), None).expect("server");

        // Omitted, empty, and ASCII-spaces-only api_key all preserve existing key
        for api_key_arg in [None, Some(String::new()), Some("   ".to_string())] {
            let res = server
                .save_settings(Parameters(SaveSettingsArgs {
                    api_key: api_key_arg,
                    default_model: Some("gemini-3.8-flash".to_string()),
                    ..Default::default()
                }))
                .await
                .expect("save");
            assert_eq!(res.is_error, Some(false));
            let reloaded = Config::load_or_default(&cfg_path).expect("reload");
            assert_eq!(reloaded.gemini.api_key.as_deref(), Some(initial_key));
        }

        // Conflicting clear_api_key: true + non-empty api_key must be rejected without echoing secret
        let conflicting_secret = "AIzaSyConflictingKeyValue999";
        let conflict = server
            .save_settings(Parameters(SaveSettingsArgs {
                api_key: Some(conflicting_secret.to_string()),
                clear_api_key: Some(true),
                ..Default::default()
            }))
            .await
            .expect("conflict");
        assert_eq!(conflict.is_error, Some(true));
        let conflict_json = serde_json::to_string(&conflict).expect("serialize");
        assert!(!conflict_json.contains(conflicting_secret));
        let reloaded = Config::load_or_default(&cfg_path).expect("reload");
        assert_eq!(reloaded.gemini.api_key.as_deref(), Some(initial_key));

        // Explicit clear_api_key: true clears saved key
        let cleared = server
            .save_settings(Parameters(SaveSettingsArgs {
                clear_api_key: Some(true),
                ..Default::default()
            }))
            .await
            .expect("clear");
        assert_eq!(cleared.is_error, Some(false));
        let reloaded = Config::load_or_default(&cfg_path).expect("reload");
        assert_eq!(reloaded.gemini.api_key, None);
    }

    #[tokio::test]
    async fn save_settings_rejects_invalid_inputs_without_echoing_secret() {
        let tmp = TempDir::new().expect("tempdir");
        let cfg_path = tmp.path().join("config.json");
        let server = CompanionUiServer::new(cfg_path.clone(), None).expect("server");

        // Verify SaveSettingsArgs Debug redacts api_key
        let debug_args = SaveSettingsArgs {
            api_key: Some("AIzaSyDebugSecretShouldHide".to_string()),
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
                    api_key: Some(bad_key.to_string()),
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

        // Invalid host, port, model, remote http URL, credentialed URL
        let invalid_cases = [
            SaveSettingsArgs {
                listen_host: Some("0.0.0.0".to_string()),
                ..Default::default()
            },
            SaveSettingsArgs {
                listen_port: Some(0),
                ..Default::default()
            },
            SaveSettingsArgs {
                default_model: Some("../evil".to_string()),
                ..Default::default()
            },
            SaveSettingsArgs {
                default_model: Some("gemini-2.5-pro".to_string()),
                ..Default::default()
            },
            SaveSettingsArgs {
                default_model: Some("gemini-2.5-flash".to_string()),
                ..Default::default()
            },
            SaveSettingsArgs {
                base_url: Some("http://evil.com".to_string()),
                ..Default::default()
            },
            SaveSettingsArgs {
                base_url: Some("https://user:pass@generativelanguage.googleapis.com".to_string()),
                ..Default::default()
            },
            SaveSettingsArgs {
                cloudcode_upstream_url: Some(
                    "http://daily-cloudcode-pa.googleapis.com".to_string(),
                ),
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
                        "requests": 7,
                        "prompt_tokens": 1000,
                        "cached_tokens": 600,
                        "output_tokens": 250,
                        "reasoning_tokens": 50,
                        "cache_hit_ratio": 0.6
                    }))
                }),
            );
        let (port_match, handle_match) = spawn_mock_http(matching_router).await;

        let mut cfg = Config {
            listen_port: port_match,
            ..Default::default()
        };
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
        assert_eq!(stats.requests, 7);
        assert_eq!(stats.cached_tokens, 600);
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
        cfg.listen_port = port_skew;
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
        cfg.listen_port = port_huge;
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
        cfg.listen_port = free_closed_port().await;
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
                            "requests": 2,
                            "prompt_tokens": 100,
                            "cached_tokens": 50,
                            "output_tokens": 20,
                            "reasoning_tokens": 5,
                            "cache_hit_ratio": 0.5
                        }))
                    }),
                );
            let (port, handle) = spawn_mock_http(router).await;
            let tmp = TempDir::new().expect("tempdir");
            let cfg_path = tmp.path().join("config.json");
            let cfg = Config {
                listen_port: port,
                ..Default::default()
            };
            cfg.save_secure(&cfg_path).expect("save");

            let server = CompanionUiServer::new(cfg_path, None).expect("server");
            let res = server.get_settings().await.expect("get_settings");
            let snap: SettingsSnapshot =
                serde_json::from_value(res.structured_content.expect("structured")).expect("parse");
            assert!(snap.proxy.reachable);
            assert_eq!(snap.stats.expect("stats").requests, 2);
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
        cfg.gemini.base_url = format!("http://127.0.0.1:{ok_port}");
        cfg.gemini.api_key = Some(valid_secret.to_string());
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
        cfg.gemini.base_url = format!("http://127.0.0.1:{bad_port}");
        cfg.gemini.api_key = Some(bad_secret.to_string());
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
        cfg.gemini.api_key = None;
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
        cfg.gemini.base_url = format!("http://127.0.0.1:{slow_port}");
        cfg.gemini.api_key = Some(valid_secret.to_string());
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
        assert_eq!(tools.len(), 4);

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

        // 6. tools/call save_settings first (sets closed_port so open_settings never probes ambient host port 4141)
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
                        "api_key": wire_secret,
                        "base_url": format!("http://127.0.0.1:{upstream_port}"),
                        "listen_port": closed_port,
                        "default_model": "gemini-3.8-flash"
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
            get_resp["result"]["structuredContent"]["config"]["api_key_hint"],
            json!("AIza...t123")
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
                listen_port: Some(closed_port),
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
                listen_port: Some(closed_port),
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
            let initial = Config {
                listen_port: closed_port,
                ..Default::default()
            };
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
                        api_key: Some(key_clone),
                        ..Default::default()
                    }))
                    .await
                    .expect("save key")
                }),
                tokio::spawn(async move {
                    s2.save_settings(Parameters(SaveSettingsArgs {
                        default_model: Some(model_clone),
                        ..Default::default()
                    }))
                    .await
                    .expect("save model")
                }),
                tokio::spawn(async move {
                    s3.save_settings(Parameters(SaveSettingsArgs {
                        listen_port: Some(new_port),
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
                final_cfg.gemini.api_key.as_deref(),
                Some(key_value.as_str()),
                "trial {trial}: lost api_key update"
            );
            assert_eq!(
                final_cfg.gemini.default_model, model_value,
                "trial {trial}: lost default_model update"
            );
            assert_eq!(
                final_cfg.listen_port, new_port,
                "trial {trial}: lost listen_port update"
            );
        }

        // Part B: 10 trials of concurrent clear_api_key vs non-key model update -> zero key resurrection.
        for trial in 0..10u16 {
            let tmp = TempDir::new().expect("tempdir");
            let cfg_path = tmp.path().join("config.json");
            let initial = Config {
                listen_port: closed_port,
                gemini: companion_core::GeminiConfig {
                    api_key: Some(format!("AIzaSyMustStayCleared{trial:02}")),
                    ..Default::default()
                },
                ..Default::default()
            };
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
                        clear_api_key: Some(true),
                        ..Default::default()
                    }))
                    .await
                    .expect("clear key")
                }),
                tokio::spawn(async move {
                    s2.save_settings(Parameters(SaveSettingsArgs {
                        default_model: Some(model_clone),
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
                final_cfg.gemini.api_key, None,
                "trial {trial}: cleared API key was resurrected by concurrent non-key save"
            );
            assert_eq!(
                final_cfg.gemini.default_model, model_value,
                "trial {trial}: lost default_model update alongside clear_api_key"
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
}
