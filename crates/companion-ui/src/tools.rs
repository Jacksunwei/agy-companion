use std::sync::Arc;
use std::time::Duration;

use companion_core::{ApiKey, Config, CoreError, GeminiCredentials, UsageRange, UsageSummary};
use companion_providers::{GeminiProvider, ProviderAdapter};

use crate::server::CompanionUiServer;
use crate::types::{ApiKeyPatch, SaveSettingsArgs, SettingsSnapshot, TestConnectionPayload};

const MAX_TEST_CONNECTION_TIMEOUT_SECS: u64 = 15;

impl CompanionUiServer {
    pub async fn get_settings_snapshot(
        &self,
        conversation_id: Option<&str>,
    ) -> Result<SettingsSnapshot, String> {
        let (cfg, gemini_creds) = self.load_config_and_auth_blocking().await?;
        Ok(self
            .build_snapshot_with_session(&cfg, gemini_creds.as_ref(), None, conversation_id)
            .await)
    }

    pub async fn save_settings_snapshot(
        &self,
        args: SaveSettingsArgs,
        conversation_id: Option<&str>,
    ) -> Result<SettingsSnapshot, String> {
        enum ValidatedKeyOp {
            Clear,
            Set(GeminiCredentials),
        }

        let (config_patch, api_key_patch, has_config_changes) =
            args.into_config_and_auth_patches()?;
        let key_op = match api_key_patch {
            Some(ApiKeyPatch::Clear) => Some(ValidatedKeyOp::Clear),
            Some(ApiKeyPatch::Set(raw)) => Some(ValidatedKeyOp::Set(GeminiCredentials::new(
                ApiKey::parse_strict(&raw).map_err(|err| err.to_string())?,
            ))),
            None => None,
        };

        let write_guard = self.config_write_lock.lock().await;
        let config_path = self.config_path.clone();
        let auth_store = Arc::clone(&self.auth_store);
        let save_outcome = tokio::task::spawn_blocking(
            move || -> Result<(Config, Option<GeminiCredentials>), String> {
                let mut cfg =
                    Config::load_or_default(&config_path).map_err(|err| err.to_string())?;
                cfg.apply_patch(config_patch)
                    .map_err(|err| err.to_string())?;

                let prev_creds: Option<Option<GeminiCredentials>> =
                    if has_config_changes && key_op.is_some() {
                        auth_store.load_gemini().ok()
                    } else {
                        None
                    };

                let gemini_creds = match key_op {
                    Some(ValidatedKeyOp::Set(creds)) => {
                        auth_store
                            .save_gemini(&creds)
                            .map_err(|err| err.to_string())?;
                        Some(creds)
                    }
                    Some(ValidatedKeyOp::Clear) => {
                        auth_store.clear_gemini().map_err(|err| err.to_string())?;
                        None
                    }
                    None => auth_store.load_gemini().map_err(|err| err.to_string())?,
                };

                if has_config_changes && let Err(err) = cfg.save_secure(&config_path) {
                    if let Some(prev) = prev_creds {
                        match prev {
                            Some(creds) => {
                                let _ = auth_store.save_gemini(&creds);
                            }
                            None => {
                                let _ = auth_store.clear_gemini();
                            }
                        }
                    }
                    return Err(err.to_string());
                }
                Ok((cfg, gemini_creds))
            },
        )
        .await
        .map_err(|err| format!("save_settings task failed: {err}"))?;
        drop(write_guard);

        let (cfg, gemini_creds) = save_outcome?;
        Ok(self
            .build_snapshot_with_session(&cfg, gemini_creds.as_ref(), None, conversation_id)
            .await)
    }

    pub async fn test_connection_payload(&self) -> Result<TestConnectionPayload, String> {
        let (cfg, gemini_creds) = self.load_config_and_auth_blocking().await?;

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

        let provider = match GeminiProvider::from_config_with_timeout(
            &cfg.providers.gemini,
            gemini_creds.as_ref().map(|c| &c.api_key),
            timeout,
        ) {
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
        let auth_store = Arc::clone(&self.auth_store);
        let mut opts = self.integration_options();
        // Hard invariant: Sidecar UI NEVER follows RC symlinks.
        opts.follow_rc_symlink = false;
        let hybrid_override = self.options.hybrid_active_override;
        let outcome = tokio::task::spawn_blocking(
            move || -> Result<(Config, Option<GeminiCredentials>, _), CoreError> {
                // Fail closed on corrupt/symlinked config on ALL platforms (including Windows where
                // enable_and_persist / disable_and_persist return Unsupported before reading config).
                let _ = Config::load_or_default(&config_path)?;
                let gemini_creds = auth_store.load_gemini()?;
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
                Ok((updated_cfg, gemini_creds, status))
            },
        )
        .await
        .map_err(|err| format!("set_launcher_integration task failed: {err}"))?;
        drop(config_guard);
        drop(integration_guard);

        let (cfg, gemini_creds, status) = outcome.map_err(|err| err.to_string())?;
        Ok(self
            .build_snapshot_with_session(&cfg, gemini_creds.as_ref(), Some(status), conversation_id)
            .await)
    }

    pub async fn get_usage_summary(&self, range: UsageRange) -> Result<UsageSummary, String> {
        let cfg = self.load_config_blocking().await?;
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
