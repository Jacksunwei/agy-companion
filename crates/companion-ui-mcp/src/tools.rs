use std::time::Duration;

use companion_core::config::ConfigPatch;
use companion_core::integration::IntegrationState;
use companion_core::{Config, CoreError, UsageRange, UsageSummary};
use companion_providers::{GeminiProvider, ProviderAdapter};
use rmcp::handler::server::wrapper::Parameters;
use rmcp::model::{CallToolResult, ContentBlock, ErrorData};
use rmcp::{tool, tool_router};
use serde_json::json;

use crate::server::CompanionUiServer;
use crate::types::{
    GetUsageArgs, SaveSettingsArgs, SetLauncherIntegrationArgs, SettingsSnapshot,
    TestConnectionPayload,
};
use crate::ui_resource::{app_only_meta, companion_icons, open_settings_meta};

const MAX_TEST_CONNECTION_TIMEOUT_SECS: u64 = 15;

pub(crate) fn tool_error(message: impl Into<String>) -> CallToolResult {
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

fn usage_summary_tool_result(summary: &UsageSummary) -> Result<CallToolResult, ErrorData> {
    let structured = serde_json::to_value(summary).map_err(|err| {
        ErrorData::internal_error(format!("failed to serialize usage summary: {err}"), None)
    })?;
    let text = format!(
        "Usage ({}): {} calls, {} input tokens, {} output tokens.",
        summary.range.as_str(),
        summary.totals.requests,
        summary.totals.input_tokens,
        summary.totals.output_tokens,
    );
    let mut result = CallToolResult::success(vec![ContentBlock::text(text)]);
    result.structured_content = Some(structured);
    Ok(result)
}

impl CompanionUiServer {
    pub async fn get_settings_snapshot(
        &self,
        conversation_id: Option<&str>,
    ) -> Result<SettingsSnapshot, String> {
        let cfg = self
            .load_config_blocking()
            .await
            .map_err(|err| err.message.to_string())?
            .map_err(|err| err.to_string())?;
        Ok(self
            .build_snapshot_with_session(&cfg, None, conversation_id)
            .await)
    }

    pub async fn save_settings_snapshot(
        &self,
        args: SaveSettingsArgs,
        conversation_id: Option<&str>,
    ) -> Result<SettingsSnapshot, String> {
        let patch: ConfigPatch = args.try_into()?;
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
        .map_err(|err| format!("save_settings task failed: {err}"))?;
        drop(write_guard);

        let cfg = save_outcome?;
        Ok(self
            .build_snapshot_with_session(&cfg, None, conversation_id)
            .await)
    }

    pub async fn test_connection_payload(&self) -> Result<TestConnectionPayload, String> {
        let cfg = self
            .load_config_blocking()
            .await
            .map_err(|err| err.message.to_string())?
            .map_err(|err| err.to_string())?;

        let timeout = self
            .options
            .test_connection_timeout_override
            .unwrap_or_else(|| {
                Duration::from_secs(
                    cfg.providers
                        .gemini
                        .timeout_secs
                        .clamp(1, MAX_TEST_CONNECTION_TIMEOUT_SECS),
                )
            });

        let provider =
            match GeminiProvider::from_config_with_timeout(&cfg.providers.gemini, timeout) {
                Ok(provider) => provider.with_env_fallback(self.options.env_api_key_fallback),
                Err(err) => {
                    return Ok(TestConnectionPayload {
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
        Ok(payload)
    }

    pub async fn set_launcher_integration_snapshot(
        &self,
        enabled: bool,
        conversation_id: Option<&str>,
    ) -> Result<SettingsSnapshot, String> {
        // Strict lock order: integration_write_lock -> config_write_lock (dropped before HTTP probes).
        let integration_guard = self.integration_write_lock.lock().await;
        let config_guard = self.config_write_lock.lock().await;
        let config_path = self.config_path.clone();
        let mut opts = self.integration_options();
        // Hard invariant: UI/MCP NEVER follows RC symlinks.
        opts.follow_rc_symlink = false;
        let hybrid_override = self.options.hybrid_active_override;
        let outcome = tokio::task::spawn_blocking(move || -> Result<(Config, _), CoreError> {
            let _ = Config::load_or_default(&config_path)?;
            let status = if enabled {
                companion_core::integration::enable_and_persist_with_hybrid_active(
                    &config_path,
                    &opts,
                    hybrid_override,
                )?
            } else {
                companion_core::integration::disable_and_persist_with_hybrid_active(
                    &config_path,
                    &opts,
                    hybrid_override,
                )?
            };
            let updated_cfg = Config::load_or_default(&config_path)?;
            Ok((updated_cfg, status))
        })
        .await
        .map_err(|err| format!("set_launcher_integration task failed: {err}"))?;
        drop(config_guard);
        drop(integration_guard);

        let (cfg, status) = outcome.map_err(|err| err.to_string())?;
        Ok(self
            .build_snapshot_with_session(&cfg, Some(status), conversation_id)
            .await)
    }

    pub async fn get_usage_summary(&self, range: UsageRange) -> Result<UsageSummary, String> {
        let cfg = self
            .load_config_blocking()
            .await
            .map_err(|err| err.message.to_string())?
            .map_err(|err| err.to_string())?;
        let outcome = tokio::task::spawn_blocking(move || -> Result<UsageSummary, CoreError> {
            let usage_dir = cfg.effective_usage_dir()?;
            #[cfg(feature = "sqlite")]
            {
                companion_core::usage::query_usage_summary(&usage_dir, range)
            }
            #[cfg(not(feature = "sqlite"))]
            {
                let _ = usage_dir;
                Ok(UsageSummary::empty(range))
            }
        })
        .await
        .map_err(|err| format!("get_usage task failed: {err}"))?;

        outcome.map_err(|err| err.to_string())
    }
}

#[tool_router(vis = "pub(crate)")]
impl CompanionUiServer {
    #[tool(
        name = "open_settings",
        title = "Open Antigravity Companion Settings",
        description = "Launch the Antigravity Companion settings and proxy telemetry panel.",
        icons = companion_icons(),
        meta = open_settings_meta()
    )]
    pub async fn open_settings(&self) -> Result<CallToolResult, ErrorData> {
        if self.options.auto_enable_launcher {
            // Strict lock order: integration_write_lock -> config_write_lock (dropped before HTTP probes).
            let integration_guard = self.integration_write_lock.lock().await;
            let config_guard = self.config_write_lock.lock().await;
            let config_path = self.config_path.clone();
            let mut opts = self.integration_options();
            opts.follow_rc_symlink = false;
            let hybrid_override = self.options.hybrid_active_override;
            let auto_outcome = tokio::task::spawn_blocking(move || {
                companion_core::integration::ensure_first_run_launcher_enabled_with_hybrid_active(
                    &config_path,
                    &opts,
                    hybrid_override,
                )
            })
            .await
            .map_err(|err| {
                ErrorData::internal_error(
                    format!("open_settings auto-enable task failed: {err}"),
                    None,
                )
            })?;
            drop(config_guard);
            drop(integration_guard);

            if let Err(err) = auto_outcome {
                tracing::warn!(
                    error = %err,
                    "open_settings first-run launcher auto-enable failed; continuing snapshot load"
                );
            }
        }

        let cfg = match self.load_config_blocking().await? {
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
        let snapshot = match self.get_settings_snapshot(None).await {
            Ok(snapshot) => snapshot,
            Err(msg) => return Ok(tool_error(msg)),
        };
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
        let snapshot = match self.save_settings_snapshot(args, None).await {
            Ok(snapshot) => snapshot,
            Err(msg) => return Ok(tool_error(msg)),
        };
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
        let payload = match self.test_connection_payload().await {
            Ok(payload) => payload,
            Err(msg) => return Ok(tool_error(msg)),
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
        let enabled = args.enabled;
        let snapshot = match self.set_launcher_integration_snapshot(enabled, None).await {
            Ok(snapshot) => snapshot,
            Err(msg) => return Ok(tool_error(msg)),
        };
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

    #[tool(
        name = "get_usage",
        title = "Get Historical Usage Summary",
        description = "Query local historical usage and token aggregates for 1d, 7d, or 30d.",
        meta = app_only_meta()
    )]
    pub async fn get_usage(
        &self,
        Parameters(args): Parameters<GetUsageArgs>,
    ) -> Result<CallToolResult, ErrorData> {
        let summary = match self.get_usage_summary(args.range.into()).await {
            Ok(summary) => summary,
            Err(msg) => return Ok(tool_error(msg)),
        };
        usage_summary_tool_result(&summary)
    }
}
