use std::path::{Path, PathBuf};
use std::sync::Arc;

use companion_core::integration::{IntegrationOptions, IntegrationStatus};
use companion_core::protocol::PROTOCOL_VERSION;
use companion_core::{
    AuthStore, CloudCodeStatus, CodexCredentials, Config, CoreError, FileAuthStore,
    GeminiCredentials, HostPlatform, ProxyHealthSummary, StatsSnapshot,
    curated_codex_model_presets, curated_model_presets,
};
use companion_providers::{
    CodexAuthManager, DEFAULT_PROBE_TIMEOUT, LoopbackProxyClient, evaluate_health,
};

use crate::types::{AccountPlanStatus, SettingsSnapshot, UiError, UiServerOptions};
use crate::ui_resource::{EMBEDDED_UI_HTML, load_ui_html_override, resolve_ui_html_override_path};

pub const DEFAULT_PROBE_TIMEOUT_SECS: u64 = DEFAULT_PROBE_TIMEOUT.as_secs();

#[derive(Clone)]
pub struct CompanionUiServer {
    pub(crate) config_path: PathBuf,
    pub(crate) auth_store: Arc<dyn AuthStore>,
    pub(crate) codex_auth: Arc<CodexAuthManager>,
    pub(crate) proxy_client: LoopbackProxyClient,
    pub(crate) config_write_lock: Arc<tokio::sync::Mutex<()>>,
    pub(crate) integration_write_lock: Arc<tokio::sync::Mutex<()>>,
    pub(crate) options: UiServerOptions,
}

impl CompanionUiServer {
    pub fn new(config_path: PathBuf) -> Result<Self, UiError> {
        Self::with_options(config_path, UiServerOptions::default())
    }

    pub fn with_options(config_path: PathBuf, options: UiServerOptions) -> Result<Self, UiError> {
        let auth_store: Arc<dyn AuthStore> = match options.auth_store_override.clone() {
            Some(store) => store,
            None => Arc::new(
                FileAuthStore::for_config_path(&config_path)
                    .map_err(|err| UiError::Initialize(err.to_string()))?,
            ),
        };
        let codex_auth: Arc<CodexAuthManager> = match options.codex_auth_override.clone() {
            Some(manager) => manager,
            None => Arc::new(
                CodexAuthManager::new(Arc::clone(&auth_store))
                    .map_err(|err| UiError::Initialize(err.to_string()))?,
            ),
        };
        let proxy_client = LoopbackProxyClient::new(options.probe_timeout, options.probe_timeout)
            .map_err(|err| UiError::HttpClient(err.to_string()))?;

        Ok(Self {
            config_path,
            auth_store,
            codex_auth,
            proxy_client,
            config_write_lock: Arc::new(tokio::sync::Mutex::new(())),
            integration_write_lock: Arc::new(tokio::sync::Mutex::new(())),
            options,
        })
    }

    pub fn config_path(&self) -> &Path {
        &self.config_path
    }

    pub async fn load_ui_html(&self) -> Result<String, String> {
        let override_path = resolve_ui_html_override_path(&self.options.ui_html_source);
        match override_path {
            Some(path) => load_ui_html_override(path).await,
            None => Ok(EMBEDDED_UI_HTML.to_string()),
        }
    }

    pub(crate) async fn load_config_blocking(&self) -> Result<Config, String> {
        let config_path = self.config_path.clone();
        tokio::task::spawn_blocking(move || Config::load_or_default(&config_path))
            .await
            .map_err(|err| format!("config load task failed: {err}"))?
            .map_err(|err| err.to_string())
    }

    pub(crate) async fn load_config_and_auth_blocking(
        &self,
    ) -> Result<(Config, Option<GeminiCredentials>, Option<CodexCredentials>), String> {
        let config_path = self.config_path.clone();
        let auth_store = Arc::clone(&self.auth_store);
        tokio::task::spawn_blocking(move || {
            let cfg = Config::load_or_default(&config_path)?;
            let gemini_creds = auth_store.load_gemini()?;
            let codex_creds = auth_store.load_codex()?;
            Ok::<_, CoreError>((cfg, gemini_creds, codex_creds))
        })
        .await
        .map_err(|err| format!("config/auth load task failed: {err}"))?
        .map_err(|err| err.to_string())
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

    pub(crate) async fn build_snapshot_with_session(
        &self,
        cfg: &Config,
        gemini_creds: Option<&GeminiCredentials>,
        codex_creds: Option<&CodexCredentials>,
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

        let ((proxy, version_skew, stats), mut launcher_integration, codex_login_pending) = tokio::join!(
            probe_fut,
            integration_fut,
            self.codex_auth.is_login_pending()
        );

        let cloudcode = CloudCodeStatus::from_config_and_stats(
            &cfg.cloudcode.upstream,
            stats.as_ref().map(|s| &s.cloudcode),
        );
        let account_plan = AccountPlanStatus::from_config_and_stats(
            &cfg.cloudcode.upstream,
            stats.as_ref().map(|s| &s.cloudcode),
        );
        let proxy_rss_bytes = stats.as_ref().and_then(|s| s.proxy_rss_bytes);
        let last_catalog_refresh_at_ms = stats
            .as_ref()
            .and_then(|s| s.cloudcode.last_catalog_refresh_at_ms);

        // Sidecar launcher scripts scrub CLOUD_CODE_URL to prevent self-looping, so a recorded
        // fetchAvailableModels refresh on our local proxy is authoritative proof of live Hybrid Mode.
        if last_catalog_refresh_at_ms.is_some() {
            launcher_integration.mark_hybrid_active();
        }

        SettingsSnapshot {
            config: cfg.redacted_with_auth(gemini_creds, codex_creds),
            binary_version,
            proxy,
            version_skew,
            stats,
            launcher_integration,
            cloudcode,
            supported_models: curated_model_presets(),
            supported_codex_models: curated_codex_model_presets(),
            codex_login_pending,
            platform: HostPlatform::current(),
            account_plan,
            proxy_rss_bytes,
            ls_telemetry_active: false,
            last_catalog_refresh_at_ms,
        }
    }
}
