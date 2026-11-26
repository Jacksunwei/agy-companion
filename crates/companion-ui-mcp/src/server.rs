use std::path::{Path, PathBuf};
use std::sync::Arc;

use companion_core::integration::{IntegrationOptions, IntegrationStatus};
use companion_core::protocol::PROTOCOL_VERSION;
use companion_core::{
    CloudCodeStatus, Config, CoreError, HostPlatform, ProxyHealthSummary, StatsSnapshot,
    curated_model_presets,
};
use companion_providers::{DEFAULT_PROBE_TIMEOUT, LoopbackProxyClient, evaluate_health};
use rmcp::handler::server::router::tool::ToolRouter;
use rmcp::model::{
    ErrorData, Implementation, ListResourcesResult, PaginatedRequestParams,
    ReadResourceRequestParams, ReadResourceResponse, ReadResourceResult, Resource,
    ResourceContents, ServerCapabilities, ServerConfig,
};
use rmcp::service::RequestContext;
use rmcp::transport::io::stdio;
use rmcp::{RoleServer, ServerHandler, ServiceExt, tool_handler};

use crate::types::{AccountPlanStatus, SettingsSnapshot, UiMcpError, UiServerOptions};
use crate::ui_resource::{
    EMBEDDED_UI_HTML, UI_MIME_TYPE, UI_RESOURCE_URI, companion_icons, load_ui_html_override,
    resolve_ui_html_override_path, ui_resource_meta,
};

pub const DEFAULT_PROBE_TIMEOUT_SECS: u64 = DEFAULT_PROBE_TIMEOUT.as_secs();

#[derive(Clone)]
pub struct CompanionUiServer {
    pub(crate) config_path: PathBuf,
    pub(crate) proxy_client: LoopbackProxyClient,
    pub(crate) config_write_lock: Arc<tokio::sync::Mutex<()>>,
    pub(crate) integration_write_lock: Arc<tokio::sync::Mutex<()>>,
    tool_router: ToolRouter<Self>,
    pub(crate) options: UiServerOptions,
}

impl CompanionUiServer {
    pub fn new(config_path: PathBuf) -> Result<Self, UiMcpError> {
        Self::with_options(config_path, UiServerOptions::default())
    }

    pub fn with_options(
        config_path: PathBuf,
        options: UiServerOptions,
    ) -> Result<Self, UiMcpError> {
        let proxy_client = LoopbackProxyClient::new(options.probe_timeout, options.probe_timeout)
            .map_err(|err| UiMcpError::HttpClient(err.to_string()))?;

        Ok(Self {
            config_path,
            proxy_client,
            config_write_lock: Arc::new(tokio::sync::Mutex::new(())),
            integration_write_lock: Arc::new(tokio::sync::Mutex::new(())),
            tool_router: Self::tool_router(),
            options,
        })
    }

    pub fn config_path(&self) -> &Path {
        &self.config_path
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

    pub async fn load_ui_html(&self) -> Result<String, ErrorData> {
        let override_path = resolve_ui_html_override_path(&self.options.ui_html_source);
        match override_path {
            Some(path) => load_ui_html_override(path).await,
            None => Ok(EMBEDDED_UI_HTML.to_string()),
        }
    }

    pub async fn read_ui_resource(&self, uri: &str) -> Result<ReadResourceResult, ErrorData> {
        if uri != UI_RESOURCE_URI {
            return Err(ErrorData::resource_not_found(
                format!("unknown resource: {uri}"),
                None,
            ));
        }

        let html = self.load_ui_html().await?;

        let contents = ResourceContents::text(html, UI_RESOURCE_URI)
            .with_mime_type(UI_MIME_TYPE)
            .with_meta(ui_resource_meta());
        Ok(ReadResourceResult::new(vec![contents]))
    }

    pub(crate) async fn load_config_blocking(
        &self,
    ) -> Result<Result<Config, CoreError>, ErrorData> {
        let config_path = self.config_path.clone();
        tokio::task::spawn_blocking(move || Config::load_or_default(&config_path))
            .await
            .map_err(|err| {
                ErrorData::internal_error(format!("config load task failed: {err}"), None)
            })
    }

    async fn probe_proxy(
        &self,
        listen_port: u16,
        binary_version: &str,
        conversation_id: Option<&str>,
    ) -> (ProxyHealthSummary, bool, Option<StatsSnapshot>) {
        match self.proxy_client.probe_health(listen_port, None).await {
            Ok(raw) => {
                let eval = evaluate_health(&raw, binary_version, PROTOCOL_VERSION);
                let mut proxy = ProxyHealthSummary::from_health(&raw);
                if !eval.status_ok {
                    proxy.error = Some(eval.health_message);
                } else if eval.protocol_skew {
                    proxy.error = Some(eval.version_message);
                }
                let stats = if eval.status_ok {
                    self.proxy_client
                        .probe_stats_for_session(listen_port, conversation_id, None)
                        .await
                        .ok()
                } else {
                    None
                };
                (proxy, eval.version_skew || eval.protocol_skew, stats)
            }
            Err(err) => (
                ProxyHealthSummary::unreachable(err.to_string()),
                false,
                None,
            ),
        }
    }

    pub(crate) fn integration_options(&self) -> IntegrationOptions {
        if let Some(ref override_opts) = self.options.integration_options_override {
            return override_opts.clone();
        }
        // Keep data_dir = None so env.sh always resolves to default_data_dir()
        // (${XDG_DATA_HOME:-~/.local/share}/agy-companion/env.sh).
        IntegrationOptions {
            shell: None,
            rc_file: None,
            binary_path: None,
            data_dir: None,
            follow_rc_symlink: false,
        }
    }

    pub(crate) fn resolved_hybrid_active(&self) -> bool {
        self.options
            .hybrid_active_override
            .unwrap_or_else(companion_core::integration::detect_hybrid_active)
    }

    pub(crate) async fn build_snapshot(&self, cfg: &Config) -> SettingsSnapshot {
        self.build_snapshot_with_session(cfg, None, None).await
    }

    pub(crate) async fn build_snapshot_with_session(
        &self,
        cfg: &Config,
        integration_override: Option<IntegrationStatus>,
        conversation_id: Option<&str>,
    ) -> SettingsSnapshot {
        let binary_version = env!("CARGO_PKG_VERSION").to_string();
        let probe_fut = self.probe_proxy(cfg.proxy.port.get(), &binary_version, conversation_id);
        let integration_fut = async {
            match integration_override {
                Some(status) => status,
                None => {
                    let opts = self.integration_options();
                    let hybrid_override = self.options.hybrid_active_override;
                    match tokio::task::spawn_blocking(move || {
                        companion_core::integration::status_with_hybrid_active(
                            &opts,
                            hybrid_override,
                        )
                    })
                    .await
                    {
                        Ok(Ok(status)) => status,
                        Ok(Err(err)) => IntegrationStatus::error_fallback(
                            self.resolved_hybrid_active(),
                            err.to_string(),
                        ),
                        Err(err) => IntegrationStatus::error_fallback(
                            self.resolved_hybrid_active(),
                            format!("launcher integration check failed: {err}"),
                        ),
                    }
                }
            }
        };

        let ((proxy, version_skew, stats), mut launcher_integration) =
            tokio::join!(probe_fut, integration_fut);

        let cloudcode = CloudCodeStatus::from_config_and_stats(
            &cfg.cloudcode.upstream,
            stats.as_ref().map(|s| &s.cloudcode),
        );
        let account_plan = AccountPlanStatus::from_config_and_stats(
            &cfg.cloudcode.upstream,
            stats.as_ref().map(|s| &s.cloudcode),
        );
        let proxy_rss_bytes = stats.as_ref().and_then(|s| s.proxy_rss_bytes);

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
            account_plan,
            proxy_rss_bytes,
        }
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

pub async fn serve_stdio(config_path: PathBuf) -> Result<(), UiMcpError> {
    serve_stdio_with_options(config_path, UiServerOptions::default()).await
}

pub async fn serve_stdio_with_options(
    config_path: PathBuf,
    options: UiServerOptions,
) -> Result<(), UiMcpError> {
    let server = CompanionUiServer::with_options(config_path, options)?;
    let running = server
        .serve(stdio())
        .await
        .map_err(|err| UiMcpError::Initialize(err.to_string()))?;
    let _quit = running.waiting().await?;
    Ok(())
}
