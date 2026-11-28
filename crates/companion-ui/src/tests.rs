use std::fs;
use std::num::NonZeroU16;
#[cfg(unix)]
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
#[cfg(unix)]
use std::time::Duration;

use axum::Router;
use axum::http::StatusCode;
use axum::routing::{get, post};
use companion_core::integration::{IntegrationOptions, IntegrationState};
use companion_core::{
    AUTH_DIR_NAME, ApiKey, AuthStore, Config, FileAuthStore, GeminiCredentials, LoopbackHost,
    MemoryAuthStore, curated_codex_model_presets, curated_model_presets,
};
use serde_json::{Value, json};
use tempfile::TempDir;
use tokio::net::TcpListener;

use crate::*;

fn isolated_integration_options(tmp: &TempDir) -> IntegrationOptions {
    IntegrationOptions {
        shell: Some(companion_core::ShellKind::Zsh),
        rc_file: Some(tmp.path().join(".zlogin")),
        binary_path: Some(tmp.path().join("agy-companion")),
        data_dir: Some(tmp.path().join("data")),
        follow_rc_symlink: false,
    }
}

fn isolated_ui_server_options(tmp: &TempDir) -> UiServerOptions {
    UiServerOptions {
        ui_html_source: UiHtmlSource::Fixed(None),
        env_api_key_fallback: false,
        test_connection_timeout_override: None,
        integration_options_override: Some(isolated_integration_options(tmp)),
        hybrid_active_override: Some(false),
        probe_timeout: companion_providers::DEFAULT_PROBE_TIMEOUT,
        auth_store_override: None,
        codex_auth_override: None,
        update_status_provider: None,
    }
}

fn isolated_ui_server_with(
    tmp: &TempDir,
    config_path: PathBuf,
    customize: impl FnOnce(&mut UiServerOptions),
) -> CompanionUiServer {
    let mut opts = isolated_ui_server_options(tmp);
    customize(&mut opts);
    CompanionUiServer::with_options(config_path, opts).expect("isolated server")
}

fn isolated_ui_server(tmp: &TempDir, config_path: PathBuf) -> CompanionUiServer {
    isolated_ui_server_with(tmp, config_path, |_| {})
}

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
    loop {
        let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
        let port = listener.local_addr().expect("addr").port();
        drop(listener);
        if !(24140..=24160).contains(&port) {
            return port;
        }
    }
}

async fn spawn_mock_http(router: Router) -> (u16, tokio::task::JoinHandle<()>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let port = listener.local_addr().expect("addr").port();
    let handle = tokio::spawn(async move {
        let _ = axum::serve(listener, router).await;
    });
    (port, handle)
}

#[tokio::test]
async fn get_settings_never_returns_plaintext_api_key() {
    let tmp = TempDir::new().expect("tmp");
    let cfg_path = tmp.path().join("config.json");
    let secret = "test-super-secret-gemini-key-9876";
    let cfg = config_with_port(free_closed_port().await);
    cfg.save_secure(&cfg_path).expect("save");
    let auth_store = FileAuthStore::for_config_path(&cfg_path).expect("auth store");
    auth_store
        .save_gemini(&GeminiCredentials::new(
            ApiKey::parse_strict(secret).expect("key"),
        ))
        .expect("save gemini auth");

    let server = isolated_ui_server(&tmp, cfg_path);
    let snap = server.get_settings_snapshot(None).await.expect("snapshot");
    let raw = serde_json::to_string(&snap).expect("serialize");
    assert!(!raw.contains(secret), "plaintext API key leaked: {raw}");
    assert!(snap.config.providers.gemini.api_key_configured);
    assert_eq!(
        snap.config.providers.gemini.api_key_hint.as_deref(),
        Some("test...9876")
    );
}

#[tokio::test]
async fn save_settings_persists_preserves_clears_and_rejects_invalid_or_placeholder_keys() {
    let tmp = TempDir::new().expect("tmp");
    let cfg_path = tmp.path().join("config.json");
    let closed_port = free_closed_port().await;
    config_with_port(closed_port)
        .save_secure(&cfg_path)
        .expect("save initial");
    let auth_store = FileAuthStore::for_config_path(&cfg_path).expect("auth store");

    let server = isolated_ui_server(&tmp, cfg_path.clone());

    // 1. Set API key + model + account_plan.
    let saved = server
        .save_settings_snapshot(
            SaveSettingsArgs {
                account_plan: Some(AccountPlan::Personal),
                providers: Some(ProvidersSettingsPatch {
                    gemini: Some(GeminiSettingsPatch {
                        api_key: Some(ApiKeyPatch::Set("test-valid-key-abcd".to_string())),
                        default_model: Some("gemini-4-neon".to_string()),
                        enabled_models: Some(vec!["gemini-4-neon".to_string()]),
                        base_url: None,
                    }),
                    ..Default::default()
                }),
                ..Default::default()
            },
            None,
        )
        .await
        .expect("save valid");
    assert!(saved.config.providers.gemini.api_key_configured);
    assert_eq!(
        saved.config.providers.gemini.api_key_hint.as_deref(),
        Some("test...abcd")
    );
    assert_eq!(saved.config.providers.gemini.default_model, "gemini-4-neon");
    assert_eq!(saved.account_plan.mode, AccountPlanMode::Personal);

    let raw_cfg = fs::read_to_string(&cfg_path).expect("read config.json");
    assert!(!raw_cfg.contains("test-valid-key-abcd"));
    assert!(!raw_cfg.contains("api_key"));
    assert_eq!(
        auth_store
            .load_gemini()
            .expect("load auth")
            .expect("saved creds")
            .api_key
            .expose(),
        "test-valid-key-abcd"
    );
    #[cfg(unix)]
    {
        assert_eq!(
            fs::metadata(&cfg_path)
                .expect("cfg metadata")
                .permissions()
                .mode()
                & 0o777,
            0o600
        );
        assert_eq!(
            fs::metadata(auth_store.auth_dir().unwrap())
                .expect("auth dir metadata")
                .permissions()
                .mode()
                & 0o777,
            0o700
        );
        assert_eq!(
            fs::metadata(auth_store.gemini_path())
                .expect("auth file metadata")
                .permissions()
                .mode()
                & 0o777,
            0o600
        );
    }

    // 2. Omitting api_key preserves existing key.
    let preserved = server
        .save_settings_snapshot(
            SaveSettingsArgs {
                providers: Some(ProvidersSettingsPatch {
                    gemini: Some(GeminiSettingsPatch {
                        default_model: Some("gemini-5-pro".to_string()),
                        ..Default::default()
                    }),
                    ..Default::default()
                }),
                ..Default::default()
            },
            None,
        )
        .await
        .expect("preserve key");
    assert!(preserved.config.providers.gemini.api_key_configured);
    assert_eq!(
        preserved.config.providers.gemini.default_model,
        "gemini-5-pro"
    );
    assert_eq!(
        auth_store
            .load_gemini()
            .expect("reload auth after preserve")
            .expect("preserved creds")
            .api_key
            .expose(),
        "test-valid-key-abcd"
    );

    // 3. Rejects placeholder key, blank key, conflicting account_plan + cloudcode.upstream, and non-loopback host.
    let placeholder_err = server
        .save_settings_snapshot(
            SaveSettingsArgs {
                providers: Some(ProvidersSettingsPatch {
                    gemini: Some(GeminiSettingsPatch {
                        api_key: Some(ApiKeyPatch::Set("agy-companion-local".to_string())),
                        ..Default::default()
                    }),
                    ..Default::default()
                }),
                ..Default::default()
            },
            None,
        )
        .await
        .expect_err("placeholder must fail");
    assert!(placeholder_err.to_lowercase().contains("placeholder"));

    for blank in ["", "   "] {
        assert!(
            server
                .save_settings_snapshot(
                    SaveSettingsArgs {
                        providers: Some(ProvidersSettingsPatch {
                            gemini: Some(GeminiSettingsPatch {
                                api_key: Some(ApiKeyPatch::Set(blank.to_string())),
                                ..Default::default()
                            }),
                            ..Default::default()
                        }),
                        ..Default::default()
                    },
                    None,
                )
                .await
                .is_err(),
            "blank key {blank:?} must fail"
        );
        assert_eq!(
            auth_store
                .load_gemini()
                .expect("reload auth after blank rejection")
                .expect("key preserved after blank rejection")
                .api_key
                .expose(),
            "test-valid-key-abcd"
        );
    }

    let conflict_err = server
        .save_settings_snapshot(
            SaveSettingsArgs {
                account_plan: Some(AccountPlan::Auto),
                cloudcode: Some(CloudCodeSettingsPatch {
                    upstream: Some("auto".to_string()),
                }),
                ..Default::default()
            },
            None,
        )
        .await
        .expect_err("conflict must fail");
    assert!(conflict_err.contains("cannot specify both"));

    let host_err = server
        .save_settings_snapshot(
            SaveSettingsArgs {
                proxy: Some(ProxySettingsPatch {
                    host: Some("0.0.0.0".to_string()),
                    port: None,
                }),
                ..Default::default()
            },
            None,
        )
        .await
        .expect_err("0.0.0.0 must fail");
    assert!(!host_err.is_empty());

    // 4. Clear key explicitly.
    let cleared = server
        .save_settings_snapshot(
            SaveSettingsArgs {
                providers: Some(ProvidersSettingsPatch {
                    gemini: Some(GeminiSettingsPatch {
                        api_key: Some(ApiKeyPatch::Clear),
                        ..Default::default()
                    }),
                    ..Default::default()
                }),
                ..Default::default()
            },
            None,
        )
        .await
        .expect("clear key");
    assert!(!cleared.config.providers.gemini.api_key_configured);
    assert!(cleared.config.providers.gemini.api_key_hint.is_none());
    assert!(
        auth_store
            .load_gemini()
            .expect("reload auth after clear")
            .is_none()
    );
    assert!(!auth_store.gemini_path().exists());
}

#[test]
fn save_settings_args_denies_unknown_fields_at_every_nesting_level() {
    for bad in [
        r#"{"unknown_root": 1}"#,
        r#"{"proxy": {"unknown_proxy": 1}}"#,
        r#"{"cloudcode": {"unknown_cc": 1}}"#,
        r#"{"providers": {"unknown_provider": {}}}"#,
        r#"{"providers": {"gemini": {"unknown_gemini": 1}}}"#,
        r#"{"providers": {"codex": {"unknown_codex": 1}}}"#,
    ] {
        assert!(
            serde_json::from_str::<SaveSettingsArgs>(bad).is_err(),
            "must reject unknown field: {bad}"
        );
    }
    assert!(serde_json::from_str::<GetUsageArgs>(r#"{"range":"7d","extra":1}"#).is_err());
    assert!(
        serde_json::from_str::<TestConnectionArgs>(r#"{"provider":"gemini","extra":1}"#).is_err()
    );
    assert!(serde_json::from_str::<TestConnectionArgs>(r#"{"provider":"unknown"}"#).is_err());
    assert_eq!(
        serde_json::from_str::<TestConnectionArgs>(r#"{}"#).expect("default provider"),
        TestConnectionArgs {
            provider: TestConnectionProvider::Gemini
        }
    );
    assert_eq!(
        serde_json::from_str::<TestConnectionArgs>(r#"{"provider":"codex"}"#).expect("codex"),
        TestConnectionArgs {
            provider: TestConnectionProvider::Codex
        }
    );
    assert!(
        serde_json::from_str::<CompleteCodexLoginArgs>(r#"{"callback_input":"code","extra":1}"#)
            .is_err()
    );
}

#[tokio::test]
async fn concurrent_save_settings_calls_preserve_disjoint_updates() {
    let tmp = TempDir::new().expect("tmp");
    let cfg_path = tmp.path().join("config.json");
    config_with_port(free_closed_port().await)
        .save_secure(&cfg_path)
        .expect("save");
    let server = Arc::new(isolated_ui_server(&tmp, cfg_path.clone()));

    let s1 = Arc::clone(&server);
    let s2 = Arc::clone(&server);
    let (r1, r2) = tokio::join!(
        s1.save_settings_snapshot(
            SaveSettingsArgs {
                providers: Some(ProvidersSettingsPatch {
                    gemini: Some(GeminiSettingsPatch {
                        api_key: Some(ApiKeyPatch::Set("test-concurrent-key-1111".to_string())),
                        ..Default::default()
                    }),
                    ..Default::default()
                }),
                ..Default::default()
            },
            None,
        ),
        s2.save_settings_snapshot(
            SaveSettingsArgs {
                providers: Some(ProvidersSettingsPatch {
                    gemini: Some(GeminiSettingsPatch {
                        default_model: Some("gemini-4-carbon".to_string()),
                        ..Default::default()
                    }),
                    ..Default::default()
                }),
                ..Default::default()
            },
            None,
        ),
    );
    r1.expect("r1");
    r2.expect("r2");

    let final_cfg = Config::load_or_default(&cfg_path).expect("reload");
    let auth_store = FileAuthStore::for_config_path(&cfg_path).expect("auth store");
    assert_eq!(
        auth_store
            .load_gemini()
            .expect("reload auth")
            .map(|c| c.api_key.expose().to_string())
            .as_deref(),
        Some("test-concurrent-key-1111")
    );
    assert_eq!(final_cfg.providers.gemini.default_model, "gemini-4-carbon");
}

#[tokio::test]
async fn get_settings_detects_proxy_health_version_skew_stats_and_rss() {
    let tmp = TempDir::new().expect("tmp");
    let cfg_path = tmp.path().join("config.json");
    let router = Router::new()
        .route(
            "/health",
            get(|| async {
                axum::Json(json!({
                    "status": "ok",
                    "version": "9.9.9",
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
                        "prompt_tokens": 3000,
                        "cached_tokens": 1500,
                        "output_tokens": 400,
                        "reasoning_tokens": 50,
                        "cache_hit_ratio": 0.5
                    },
                    "cloudcode": {
                        "mode": "auto",
                        "resolved_upstream": "https://cloudcode-pa.googleapis.com",
                        "last_catalog_refresh_at_ms": 1700000000000_u64,
                        "last_catalog_included_proxy_models": true
                    },
                    "proxy_rss_bytes": 26214400_u64,
                    "recent_calls": [],
                    "by_model": []
                }))
            }),
        );
    let (port, handle) = spawn_mock_http(router).await;
    config_with_port(port).save_secure(&cfg_path).expect("save");

    let server = isolated_ui_server(&tmp, cfg_path);
    let snap = server.get_settings_snapshot(None).await.expect("snapshot");
    handle.abort();

    assert!(snap.proxy.reachable);
    assert!(snap.version_skew);
    assert_eq!(snap.proxy_rss_bytes, Some(26_214_400));
    assert_eq!(snap.last_catalog_refresh_at_ms, Some(1_700_000_000_000));
    assert_eq!(snap.account_plan.mode, AccountPlanMode::Auto);
    assert_eq!(
        snap.account_plan.resolved,
        Some(ResolvedAccountPlan::Organization)
    );
    assert!(snap.launcher_integration.hybrid_active);
}

#[tokio::test]
async fn test_connection_handles_ok_invalid_key_missing_key_and_corrupt_config() {
    let tmp = TempDir::new().expect("tmp");
    let cfg_path = tmp.path().join("config.json");
    let secret = "test-conn-secret-4321";

    let hits = Arc::new(AtomicUsize::new(0));
    let hits_clone = Arc::clone(&hits);
    let router = Router::new().route(
        "/v1beta/models",
        get(move |headers: axum::http::HeaderMap| {
            let hits = Arc::clone(&hits_clone);
            async move {
                hits.fetch_add(1, Ordering::SeqCst);
                let provided = headers
                    .get("x-goog-api-key")
                    .and_then(|v| v.to_str().ok())
                    .unwrap_or("");
                if provided == secret {
                    (StatusCode::OK, axum::Json(json!({ "models": [] })))
                } else {
                    (
                        StatusCode::UNAUTHORIZED,
                        axum::Json(json!({ "error": { "message": "invalid key" } })),
                    )
                }
            }
        }),
    );
    let (port, handle) = spawn_mock_http(router).await;

    let mut cfg = config_with_port(free_closed_port().await);
    cfg.providers.gemini.base_url = format!("http://127.0.0.1:{port}");
    cfg.save_secure(&cfg_path).expect("save");
    let auth_store = FileAuthStore::for_config_path(&cfg_path).expect("auth store");
    auth_store
        .save_gemini(&GeminiCredentials::new(
            ApiKey::parse_strict(secret).expect("key"),
        ))
        .expect("save gemini auth");

    let server = isolated_ui_server(&tmp, cfg_path.clone());
    let ok = server
        .test_connection_payload(TestConnectionProvider::Gemini)
        .await
        .expect("ok payload");
    assert!(ok.ok);
    assert_eq!(ok.message, "Connected \u{00b7} Google AI Studio");
    assert!(!ok.message.contains("http://") && !ok.message.contains("https://"));
    assert!(!ok.message.contains(secret));
    assert_eq!(hits.load(Ordering::SeqCst), 1);

    // Missing key returns ok=false without contacting upstream.
    auth_store.clear_gemini().expect("clear key");
    let missing = server
        .test_connection_payload(TestConnectionProvider::Gemini)
        .await
        .expect("missing key payload");
    assert!(!missing.ok);

    // Corrupt config returns Err(String).
    write_raw_secure(&cfg_path, "{corrupt json");
    assert!(
        server
            .test_connection_payload(TestConnectionProvider::Gemini)
            .await
            .is_err()
    );

    handle.abort();
}

#[tokio::test]
async fn ui_html_override_handles_embedded_dynamic_missing_oversized_and_fifo() {
    let tmp = TempDir::new().expect("tmp");
    let cfg_path = tmp.path().join("config.json");

    // 1. Default embedded HTML is non-empty and matches EMBEDDED_UI_HTML.
    let embedded_server = isolated_ui_server(&tmp, cfg_path.clone());
    let html = embedded_server.load_ui_html().await.expect("embedded");
    assert_eq!(html, EMBEDDED_UI_HTML);
    assert!(html.contains("Antigravity Companion"));

    // 2. Dynamic file reload works without recreating server.
    let override_file = tmp.path().join("override.html");
    fs::write(&override_file, "<!doctype html><html>v1</html>").expect("write v1");
    let dyn_server = isolated_ui_server_with(&tmp, cfg_path.clone(), |o| {
        o.ui_html_source = UiHtmlSource::Fixed(Some(override_file.clone()));
    });
    assert!(dyn_server.load_ui_html().await.expect("v1").contains("v1"));
    fs::write(&override_file, "<!doctype html><html>v2</html>").expect("write v2");
    assert!(dyn_server.load_ui_html().await.expect("v2").contains("v2"));

    // 3. Missing file fails cleanly.
    let missing_server = isolated_ui_server_with(&tmp, cfg_path.clone(), |o| {
        o.ui_html_source = UiHtmlSource::Fixed(Some(tmp.path().join("does-not-exist.html")));
    });
    assert!(missing_server.load_ui_html().await.is_err());

    // 4. Oversized file (> MAX_UI_HTML_BYTES) fails cleanly.
    let big_file = tmp.path().join("too-big.html");
    fs::File::create(&big_file)
        .expect("create big")
        .set_len(MAX_UI_HTML_BYTES + 1)
        .expect("set_len");
    let big_server = isolated_ui_server_with(&tmp, cfg_path.clone(), |o| {
        o.ui_html_source = UiHtmlSource::Fixed(Some(big_file));
    });
    assert!(big_server.load_ui_html().await.is_err());

    // 5. Unix FIFO fails closed without hanging.
    #[cfg(unix)]
    {
        let fifo = tmp.path().join("override.fifo");
        let status = std::process::Command::new("mkfifo")
            .arg(&fifo)
            .status()
            .expect("mkfifo");
        assert!(status.success());
        let fifo_server = isolated_ui_server_with(&tmp, cfg_path, |o| {
            o.ui_html_source = UiHtmlSource::Fixed(Some(fifo));
        });
        let res = tokio::time::timeout(Duration::from_secs(3), fifo_server.load_ui_html())
            .await
            .expect("FIFO load_ui_html must not hang");
        assert!(res.is_err());
    }
}

#[tokio::test]
async fn ui_resource_reads_process_env_var_dynamically() {
    let tmp = TempDir::new().expect("tmp");
    let cfg_path = tmp.path().join("config.json");
    let server = CompanionUiServer::new(cfg_path).expect("default server");
    // Exercise default ProcessEnv resolution without mutating ambient env.
    if std::env::var_os(UI_HTML_ENV_VAR).is_none() {
        let html = server.load_ui_html().await.expect("embedded fallback");
        assert_eq!(html, EMBEDDED_UI_HTML);
    }
}

#[tokio::test]
async fn set_launcher_integration_never_follows_rc_symlink_and_fails_closed_on_corrupt_config() {
    let tmp = TempDir::new().expect("tmp");
    let cfg_path = tmp.path().join("config.json");
    config_with_port(free_closed_port().await)
        .save_secure(&cfg_path)
        .expect("save");

    let server = isolated_ui_server(&tmp, cfg_path.clone());

    #[cfg(unix)]
    {
        use std::os::unix::fs::symlink;
        let real_rc = tmp.path().join("dotfiles-zlogin");
        fs::write(&real_rc, "# managed by stow\n").expect("write real rc");
        symlink(&real_rc, tmp.path().join(".zlogin")).expect("symlink rc");

        let snap = server
            .set_launcher_integration_snapshot(true, None)
            .await
            .expect("manual_required snapshot");
        assert_eq!(
            snap.launcher_integration.state(),
            IntegrationState::ManualRequired
        );
        assert_eq!(
            fs::read_to_string(&real_rc).expect("read real rc"),
            "# managed by stow\n"
        );
    }

    // Corrupt config must fail closed before mutating dotfiles on ALL platforms.
    write_raw_secure(&cfg_path, "{invalid");
    assert!(
        server
            .set_launcher_integration_snapshot(true, None)
            .await
            .is_err()
    );
}

#[tokio::test]
async fn get_usage_queries_empty_and_seeded_sqlite() {
    let tmp = TempDir::new().expect("tmp");
    let cfg_path = tmp.path().join("config.json");
    let usage_dir = tmp.path().join("usage");
    let mut cfg = config_with_port(free_closed_port().await);
    cfg.usage_dir = Some(usage_dir.clone());
    cfg.save_secure(&cfg_path).expect("save");

    let server = isolated_ui_server(&tmp, cfg_path);
    let empty = server
        .get_usage_summary(companion_core::UsageRange::Days7)
        .await
        .expect("empty usage");
    assert_eq!(empty.totals.requests, 0);

    #[cfg(feature = "sqlite")]
    {
        use companion_core::usage::{GeminiUsageMetadata, SqliteUsageSink, UsageEvent, UsageSink};
        let sink = SqliteUsageSink::new(&usage_dir).expect("open sink");
        let event = UsageEvent::from_gemini(
            "gemini-4-neon",
            &GeminiUsageMetadata {
                prompt_token_count: 1000,
                candidates_token_count: 120,
                cached_content_token_count: 500,
                thoughts_token_count: 20,
            },
            250,
        );
        sink.record(event);
        drop(sink);

        let seeded = server
            .get_usage_summary(companion_core::UsageRange::Days7)
            .await
            .expect("seeded usage");
        assert_eq!(seeded.totals.requests, 1);
        assert_eq!(seeded.totals.input_tokens, 1000);
    }
}

#[test]
fn golden_wire_fixtures_match_rust_serialization_verbatim() {
    use companion_core::{
        CloudCodeConfig, CloudCodeStatsSnapshot, CloudCodeUpstream, Config,
        DEFAULT_LISTEN_PORT_NONZERO, IntegrationStatus, ModelSessionStats, ProxyConfig,
        RecentCallSnapshot, RedactedCodexConfig, RedactedConfig, RedactedGeminiConfig,
        RedactedProvidersConfig, ShellKind, UsageBreakdownRow, UsageBucket, UsageRange,
        UsageStatsSnapshot, UsageSummary, default_enabled_gemini_models,
    };

    // 1. settings_snapshot_active.json
    const ACTIVE_FIXTURE: &str =
        include_str!("../../../ui/tests/fixtures/settings_snapshot_active.json");
    let active_snapshot = SettingsSnapshot {
        config: RedactedConfig {
            schema_version: 1,
            proxy: ProxyConfig {
                host: LoopbackHost::Ipv4Loopback,
                port: DEFAULT_LISTEN_PORT_NONZERO,
            },
            cloudcode: CloudCodeConfig {
                upstream: CloudCodeUpstream::Auto,
            },
            providers: RedactedProvidersConfig {
                gemini: RedactedGeminiConfig {
                    base_url: "https://generativelanguage.googleapis.com".to_string(),
                    default_model: "gemini-4-hydrogen".to_string(),
                    enabled_models: default_enabled_gemini_models(),
                    timeout_secs: 900,
                    api_key_configured: true,
                    api_key_hint: Some("AIza...1234".to_string()),
                },
                codex: RedactedCodexConfig::default(),
            },
            launcher_auto_start: Some(true),
            check_for_updates: None,
        },
        binary_version: "0.1.0".to_string(),
        proxy: ProxyHealthSummary {
            reachable: true,
            status: Some("ok".to_string()),
            version: Some("0.1.0".to_string()),
            protocol_version: Some(1),
            error: None,
        },
        version_skew: false,
        stats: Some(StatsSnapshot {
            usage: UsageStatsSnapshot {
                requests: 2,
                prompt_tokens: 4000,
                cached_tokens: 2500,
                output_tokens: 800,
                reasoning_tokens: 160,
                cache_hit_ratio: 0.625,
            },
            cloudcode: CloudCodeStatsSnapshot {
                mode: "auto".to_string(),
                resolved_upstream: Some("https://cloudcode-pa.googleapis.com".to_string()),
                last_catalog_refresh_at_ms: Some(1_747_299_990_000),
                last_catalog_included_proxy_models: Some(true),
            },
            proxy_rss_bytes: Some(25_165_824),
            max_context_tokens: Some(512_000),
            recent_calls: vec![
                RecentCallSnapshot {
                    timestamp_ms: 1_747_300_000_000,
                    provider: "antigravity".to_string(),
                    source: "builtin".to_string(),
                    model: "gemini-3.8-pro".to_string(),
                    input_tokens: 1000,
                    cached_tokens: 0,
                    output_tokens: 200,
                    reasoning_tokens: 40,
                    latency_ms: 320,
                    cold_start: true,
                    cache_break: false,
                },
                RecentCallSnapshot {
                    timestamp_ms: 1_747_300_060_000,
                    provider: "gemini".to_string(),
                    source: "proxy".to_string(),
                    model: "gemini-4-carbon".to_string(),
                    input_tokens: 3000,
                    cached_tokens: 2500,
                    output_tokens: 600,
                    reasoning_tokens: 120,
                    latency_ms: 480,
                    cold_start: false,
                    cache_break: false,
                },
            ],
            by_model: vec![
                ModelSessionStats {
                    provider: "gemini".to_string(),
                    source: "proxy".to_string(),
                    model: "gemini-4-carbon".to_string(),
                    model_display_name: None,
                    requests: 1,
                    input_tokens: 3000,
                    cached_tokens: 2500,
                    output_tokens: 600,
                    reasoning_tokens: 120,
                    total_latency_ms: 480,
                    subagent: None,
                },
                ModelSessionStats {
                    provider: "antigravity".to_string(),
                    source: "builtin".to_string(),
                    model: "gemini-3.8-pro".to_string(),
                    model_display_name: None,
                    requests: 1,
                    input_tokens: 1000,
                    cached_tokens: 0,
                    output_tokens: 200,
                    reasoning_tokens: 40,
                    total_latency_ms: 320,
                    subagent: None,
                },
            ],
        }),
        launcher_integration: IntegrationStatus {
            state: IntegrationState::Active,
            enabled: true,
            hybrid_active: true,
            shell: Some(ShellKind::Zsh),
            rc_path: Some(PathBuf::from("/Users/dev/.zlogin")),
            env_script_path: Some(PathBuf::from(
                "/Users/dev/.local/share/agy-companion/env.sh",
            )),
            binary_path: Some(PathBuf::from(
                "/Users/dev/.local/share/agy-companion/bin/agy-companion",
            )),
            symlink_target: None,
            manual_snippet: None,
            message: None,
        },
        cloudcode: CloudCodeStatus {
            mode: "auto".to_string(),
            configured_upstream: "auto".to_string(),
            resolved_upstream: Some("https://cloudcode-pa.googleapis.com".to_string()),
        },
        supported_models: curated_model_presets(),
        supported_codex_models: curated_codex_model_presets(),
        codex_login_pending: false,
        platform: HostPlatform::Macos,
        account_plan: AccountPlanStatus {
            mode: AccountPlanMode::Auto,
            resolved: Some(ResolvedAccountPlan::Organization),
        },
        proxy_rss_bytes: Some(25_165_824),
        ls_telemetry_active: false,
        last_catalog_refresh_at_ms: Some(1_747_299_990_000),
        update: None,
    };
    let expected_active: Value =
        serde_json::from_str(ACTIVE_FIXTURE).expect("parse active fixture");
    assert_eq!(
        serde_json::to_value(&active_snapshot).expect("serialize active snapshot"),
        expected_active
    );
    assert_eq!(
        serde_json::from_str::<SettingsSnapshot>(ACTIVE_FIXTURE).expect("roundtrip active"),
        active_snapshot
    );

    // 2. settings_snapshot_day0_offline.json
    const DAY0_FIXTURE: &str =
        include_str!("../../../ui/tests/fixtures/settings_snapshot_day0_offline.json");
    let day0_snapshot = SettingsSnapshot {
        config: Config::default().redacted_with_auth(None, None),
        binary_version: "0.1.0".to_string(),
        proxy: ProxyHealthSummary::unreachable("connection refused"),
        version_skew: false,
        stats: None,
        launcher_integration: IntegrationStatus {
            state: IntegrationState::ManualRequired,
            enabled: false,
            hybrid_active: false,
            shell: Some(ShellKind::Zsh),
            rc_path: Some(PathBuf::from("/Users/dev/.zlogin")),
            env_script_path: Some(PathBuf::from(
                "/Users/dev/.local/share/agy-companion/env.sh",
            )),
            binary_path: Some(PathBuf::from(
                "/Users/dev/.local/share/agy-companion/bin/agy-companion",
            )),
            symlink_target: Some(PathBuf::from("/Users/dev/dotfiles/.zlogin")),
            manual_snippet: Some(
                "# >>> agy-companion (Antigravity Dock/Launcher integration) >>>\n[ -r \"${XDG_DATA_HOME:-$HOME/.local/share}/agy-companion/env.sh\" ] && . \"${XDG_DATA_HOME:-$HOME/.local/share}/agy-companion/env.sh\"\n# <<< agy-companion <<<"
                    .to_string(),
            ),
            message: Some(
                "RC file is a symlink (likely managed by Nix, Stow, or Chezmoi); add the snippet manually."
                    .to_string(),
            ),
        },
        cloudcode: CloudCodeStatus::from_config_and_stats(&CloudCodeUpstream::Auto, None),
        supported_models: curated_model_presets(),
        supported_codex_models: curated_codex_model_presets(),
        codex_login_pending: false,
        platform: HostPlatform::Macos,
        account_plan: AccountPlanStatus::from_config_and_stats(&CloudCodeUpstream::Auto, None),
        proxy_rss_bytes: None,
        ls_telemetry_active: false,
        last_catalog_refresh_at_ms: None,
        update: None,
    };
    let expected_day0: Value = serde_json::from_str(DAY0_FIXTURE).expect("parse day0 fixture");
    assert_eq!(
        serde_json::to_value(&day0_snapshot).expect("serialize day0 snapshot"),
        expected_day0
    );
    assert_eq!(
        serde_json::from_str::<SettingsSnapshot>(DAY0_FIXTURE).expect("roundtrip day0"),
        day0_snapshot
    );

    // 3. usage_summary_7d.json
    const USAGE_FIXTURE: &str = include_str!("../../../ui/tests/fixtures/usage_summary_7d.json");
    let zero_bucket = |label: &str| UsageBucket {
        label: label.to_string(),
        ..Default::default()
    };
    let usage_summary = UsageSummary {
        range: UsageRange::Days7,
        since_utc: "2026-05-09T00:00:00Z".to_string(),
        totals: UsageBucket {
            label: String::new(),
            requests: 3,
            input_tokens: 3500,
            cached_tokens: 2000,
            output_tokens: 850,
            reasoning_tokens: 170,
        },
        buckets: vec![
            zero_bucket("2026-05-09"),
            zero_bucket("2026-05-10"),
            zero_bucket("2026-05-11"),
            UsageBucket {
                label: "2026-05-12".to_string(),
                requests: 1,
                input_tokens: 500,
                cached_tokens: 100,
                output_tokens: 150,
                reasoning_tokens: 20,
            },
            zero_bucket("2026-05-13"),
            zero_bucket("2026-05-14"),
            UsageBucket {
                label: "2026-05-15".to_string(),
                requests: 2,
                input_tokens: 3000,
                cached_tokens: 1900,
                output_tokens: 700,
                reasoning_tokens: 150,
            },
        ],
        by_model: vec![
            UsageBreakdownRow {
                key: "gemini-4-carbon".to_string(),
                provider: "gemini".to_string(),
                source: Some("proxy".to_string()),
                model_display_name: None,
                model_count: None,
                requests: 1,
                input_tokens: 2000,
                cached_tokens: 1500,
                output_tokens: 500,
                reasoning_tokens: 100,
                avg_latency_ms: 500,
            },
            UsageBreakdownRow {
                key: "gemini-3.8-pro".to_string(),
                provider: "antigravity".to_string(),
                source: Some("builtin".to_string()),
                model_display_name: None,
                model_count: None,
                requests: 1,
                input_tokens: 1000,
                cached_tokens: 400,
                output_tokens: 200,
                reasoning_tokens: 50,
                avg_latency_ms: 300,
            },
            UsageBreakdownRow {
                key: "gemini-4-neon".to_string(),
                provider: "gemini".to_string(),
                source: Some("proxy".to_string()),
                model_display_name: None,
                model_count: None,
                requests: 1,
                input_tokens: 500,
                cached_tokens: 100,
                output_tokens: 150,
                reasoning_tokens: 20,
                avg_latency_ms: 100,
            },
        ],
        by_provider: vec![
            UsageBreakdownRow {
                key: "gemini".to_string(),
                provider: "gemini".to_string(),
                source: None,
                model_display_name: None,
                model_count: Some(2),
                requests: 2,
                input_tokens: 2500,
                cached_tokens: 1600,
                output_tokens: 650,
                reasoning_tokens: 120,
                avg_latency_ms: 300,
            },
            UsageBreakdownRow {
                key: "antigravity".to_string(),
                provider: "antigravity".to_string(),
                source: None,
                model_display_name: None,
                model_count: Some(1),
                requests: 1,
                input_tokens: 1000,
                cached_tokens: 400,
                output_tokens: 200,
                reasoning_tokens: 50,
                avg_latency_ms: 300,
            },
        ],
    };
    let expected_usage: Value = serde_json::from_str(USAGE_FIXTURE).expect("parse usage fixture");
    assert_eq!(
        serde_json::to_value(&usage_summary).expect("serialize usage summary"),
        expected_usage
    );
    assert_eq!(
        serde_json::from_str::<UsageSummary>(USAGE_FIXTURE).expect("roundtrip usage"),
        usage_summary
    );

    // 4. test_connection_ok.json
    const TEST_CONN_FIXTURE: &str =
        include_str!("../../../ui/tests/fixtures/test_connection_ok.json");
    let test_conn = TestConnectionPayload {
        ok: true,
        message: "Connected \u{00b7} Google AI Studio".to_string(),
    };
    let expected_conn: Value =
        serde_json::from_str(TEST_CONN_FIXTURE).expect("parse test_connection fixture");
    assert_eq!(
        serde_json::to_value(&test_conn).expect("serialize test_connection"),
        expected_conn
    );
    assert_eq!(
        serde_json::from_str::<TestConnectionPayload>(TEST_CONN_FIXTURE).expect("roundtrip conn"),
        test_conn
    );

    // 5. start_codex_login.json
    const START_CODEX_FIXTURE: &str =
        include_str!("../../../ui/tests/fixtures/start_codex_login.json");
    let start_codex = StartCodexLoginPayload {
        authorize_url: "https://auth.openai.com/oauth/authorize?response_type=code&client_id=app_EMoamEEZ73f0CkXaXp7hrann".to_string(),
        callback_listening: true,
        manual_fallback: false,
    };
    let expected_start_codex: Value =
        serde_json::from_str(START_CODEX_FIXTURE).expect("parse start_codex_login fixture");
    assert_eq!(
        serde_json::to_value(&start_codex).expect("serialize start_codex_login"),
        expected_start_codex
    );
    assert_eq!(
        serde_json::from_str::<StartCodexLoginPayload>(START_CODEX_FIXTURE)
            .expect("roundtrip start_codex_login"),
        start_codex
    );
}

#[tokio::test]
async fn save_settings_atomic_rejection_and_key_only_write_without_creating_config_json() {
    let tmp = TempDir::new().expect("tempdir");
    let cfg_path = tmp.path().join("config.json");
    let auth_store = FileAuthStore::for_config_path(&cfg_path).expect("auth store");
    let server = isolated_ui_server(&tmp, cfg_path.clone());

    // 1. Valid api_key + invalid base_url -> writes NEITHER config.json NOR auth/gemini.json
    assert!(
        server
            .save_settings_snapshot(
                SaveSettingsArgs {
                    providers: Some(ProvidersSettingsPatch {
                        gemini: Some(GeminiSettingsPatch {
                            api_key: Some(ApiKeyPatch::Set("AIzaSyShouldNotWrite01".to_string())),
                            base_url: Some("http://evil.example.com".to_string()),
                            ..Default::default()
                        }),
                        ..Default::default()
                    }),
                    ..Default::default()
                },
                None,
            )
            .await
            .is_err()
    );
    assert!(
        !cfg_path.exists(),
        "config.json must not be created on error"
    );
    assert!(
        !auth_store.gemini_path().exists(),
        "auth/gemini.json must not be created on config patch error"
    );

    // 2. Invalid api_key (placeholder) + valid default_model -> writes NEITHER
    assert!(
        server
            .save_settings_snapshot(
                SaveSettingsArgs {
                    providers: Some(ProvidersSettingsPatch {
                        gemini: Some(GeminiSettingsPatch {
                            api_key: Some(ApiKeyPatch::Set(
                                companion_core::PROXY_PLACEHOLDER_API_KEY.to_string(),
                            )),
                            default_model: Some("gemini-4-neon".to_string()),
                            ..Default::default()
                        }),
                        ..Default::default()
                    }),
                    ..Default::default()
                },
                None,
            )
            .await
            .is_err()
    );
    assert!(!cfg_path.exists());
    assert!(!auth_store.gemini_path().exists());

    // 3. Key-only save when config.json absent -> creates auth/gemini.json (0700/0600) WITHOUT creating config.json
    let key_only_snap = server
        .save_settings_snapshot(
            SaveSettingsArgs {
                providers: Some(ProvidersSettingsPatch {
                    gemini: Some(GeminiSettingsPatch {
                        api_key: Some(ApiKeyPatch::Set("AIzaSyKeyOnlyNoCfg0001".to_string())),
                        ..Default::default()
                    }),
                    ..Default::default()
                }),
                ..Default::default()
            },
            None,
        )
        .await
        .expect("key-only save");
    assert!(key_only_snap.config.providers.gemini.api_key_configured);
    assert!(
        !cfg_path.exists(),
        "key-only save must not needlessly create config.json"
    );
    assert!(auth_store.gemini_path().is_file());

    #[cfg(unix)]
    {
        assert_eq!(
            fs::metadata(auth_store.auth_dir().unwrap())
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o700
        );
        assert_eq!(
            fs::metadata(auth_store.gemini_path())
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o600
        );
    }

    let snap = server
        .get_settings_snapshot(None)
        .await
        .expect("get_settings");
    assert!(snap.config.providers.gemini.api_key_configured);
    assert_eq!(
        snap.config.providers.gemini.api_key_hint.as_deref(),
        Some("AIza...0001")
    );
}

#[cfg(unix)]
#[tokio::test]
async fn ui_service_fails_closed_on_symlinked_auth_dir_and_gemini_file() {
    use std::os::unix::fs::symlink;

    use companion_core::GEMINI_AUTH_FILENAME;

    let tmp = TempDir::new().expect("tempdir");
    let victim_dir = tmp.path().join("victim_dir");
    fs::create_dir_all(&victim_dir).unwrap();
    let victim_file = victim_dir.join(GEMINI_AUTH_FILENAME);
    let victim_sentinel = r#"{"schema_version":1,"api_key":"AIzaSyVictimStayIntact"}"#;
    fs::write(&victim_file, victim_sentinel).unwrap();

    for case in ["symlink_auth_dir", "symlink_gemini_file"] {
        let case_root = tmp.path().join(case);
        fs::create_dir_all(&case_root).unwrap();
        let cfg_path = case_root.join("config.json");
        let auth_dir = case_root.join(AUTH_DIR_NAME);
        if case == "symlink_auth_dir" {
            symlink(&victim_dir, &auth_dir).unwrap();
        } else {
            fs::create_dir_all(&auth_dir).unwrap();
            symlink(&victim_file, auth_dir.join(GEMINI_AUTH_FILENAME)).unwrap();
        }

        let server = isolated_ui_server(&tmp, cfg_path.clone());
        assert!(
            server.get_settings_snapshot(None).await.is_err(),
            "{case}: get_settings_snapshot must fail closed"
        );
        assert!(
            server
                .test_connection_payload(TestConnectionProvider::Gemini)
                .await
                .is_err(),
            "{case}: test_connection_payload must fail closed"
        );
        assert!(
            server
                .save_settings_snapshot(
                    SaveSettingsArgs {
                        providers: Some(ProvidersSettingsPatch {
                            gemini: Some(GeminiSettingsPatch {
                                api_key: Some(ApiKeyPatch::Set(
                                    "AIzaSyShouldNotWrite02".to_string(),
                                )),
                                default_model: Some("gemini-4-neon".to_string()),
                                ..Default::default()
                            }),
                            ..Default::default()
                        }),
                        ..Default::default()
                    },
                    None,
                )
                .await
                .is_err(),
            "{case}: save_settings_snapshot must fail closed"
        );
        assert!(
            server
                .set_launcher_integration_snapshot(true, None)
                .await
                .is_err(),
            "{case}: set_launcher_integration_snapshot must fail closed"
        );
        assert!(
            !cfg_path.exists(),
            "{case}: failed save_settings must not create config.json"
        );
        assert_eq!(
            fs::read_to_string(&victim_file).unwrap(),
            victim_sentinel,
            "{case}: symlink victim must remain untouched"
        );
    }
}

#[tokio::test]
async fn ui_server_options_auth_store_override_uses_memory_store_without_disk_auth_dir() {
    let tmp = TempDir::new().expect("tempdir");
    let cfg_path = tmp.path().join("config.json");
    let mem_store = Arc::new(MemoryAuthStore::with_gemini_key(
        ApiKey::parse_strict("AIzaSyMemOverrideKey01").unwrap(),
    ));
    let mem_store_dyn: Arc<dyn AuthStore> = mem_store.clone();
    let server = isolated_ui_server_with(&tmp, cfg_path.clone(), |o| {
        o.auth_store_override = Some(mem_store_dyn);
    });

    let snap = server
        .get_settings_snapshot(None)
        .await
        .expect("get_settings");
    assert!(snap.config.providers.gemini.api_key_configured);
    assert_eq!(
        snap.config.providers.gemini.api_key_hint.as_deref(),
        Some("AIza...ey01")
    );
    assert!(!tmp.path().join(AUTH_DIR_NAME).exists());

    // Saving a rotated key updates MemoryAuthStore with zero disk auth/ creation
    let saved = server
        .save_settings_snapshot(
            SaveSettingsArgs {
                providers: Some(ProvidersSettingsPatch {
                    gemini: Some(GeminiSettingsPatch {
                        api_key: Some(ApiKeyPatch::Set("AIzaSyMemOverrideKey02".to_string())),
                        ..Default::default()
                    }),
                    ..Default::default()
                }),
                ..Default::default()
            },
            None,
        )
        .await
        .expect("save rotated");
    assert!(saved.config.providers.gemini.api_key_configured);
    assert_eq!(
        mem_store.load_gemini().unwrap().unwrap().api_key.expose(),
        "AIzaSyMemOverrideKey02"
    );
    assert!(!tmp.path().join(AUTH_DIR_NAME).exists());
}

#[cfg(unix)]
#[tokio::test]
async fn save_settings_rolls_back_gemini_auth_when_config_save_fails_on_read_only_dir() {
    struct RestoreDirMode(PathBuf);
    impl Drop for RestoreDirMode {
        fn drop(&mut self) {
            let _ = fs::set_permissions(&self.0, fs::Permissions::from_mode(0o700));
        }
    }

    let tmp = TempDir::new().expect("tempdir");
    let cfg_dir = tmp.path().join("ro_cfg_dir");
    fs::create_dir_all(&cfg_dir).expect("mkdir cfg_dir");
    let cfg_path = cfg_dir.join("config.json");
    Config::default()
        .save_secure(&cfg_path)
        .expect("seed config.json");

    let auth_store = FileAuthStore::for_config_path(&cfg_path).expect("auth store");
    let original_key = "AIzaSyOrigBeforeFail01";
    auth_store
        .save_gemini(&GeminiCredentials::new(
            ApiKey::parse_strict(original_key).unwrap(),
        ))
        .expect("seed gemini auth");

    // Make <config_dir> read-only (0500) while keeping <config_dir>/auth writable (0700).
    let _restore_guard = RestoreDirMode(cfg_dir.clone());
    fs::set_permissions(&cfg_dir, fs::Permissions::from_mode(0o500)).expect("chmod cfg_dir 0500");

    let server = isolated_ui_server_with(&tmp, cfg_path.clone(), |_| {});

    // 1. Combined Set(new_key) + default_model must fail and roll back auth/gemini.json to original_key.
    assert!(
        server
            .save_settings_snapshot(
                SaveSettingsArgs {
                    providers: Some(ProvidersSettingsPatch {
                        gemini: Some(GeminiSettingsPatch {
                            api_key: Some(ApiKeyPatch::Set("AIzaSyMutatedKey9999".to_string())),
                            default_model: Some("gemini-3.8-flash".to_string()),
                            ..Default::default()
                        }),
                        ..Default::default()
                    }),
                    ..Default::default()
                },
                None,
            )
            .await
            .is_err()
    );
    assert_eq!(
        auth_store
            .load_gemini()
            .expect("reload auth after set rollback")
            .expect("original creds restored")
            .api_key
            .expose(),
        original_key
    );

    // 2. Combined Clear + default_model must also fail and restore original_key.
    assert!(
        server
            .save_settings_snapshot(
                SaveSettingsArgs {
                    providers: Some(ProvidersSettingsPatch {
                        gemini: Some(GeminiSettingsPatch {
                            api_key: Some(ApiKeyPatch::Clear),
                            default_model: Some("gemini-3.8-flash".to_string()),
                            ..Default::default()
                        }),
                        ..Default::default()
                    }),
                    ..Default::default()
                },
                None,
            )
            .await
            .is_err()
    );
    assert_eq!(
        auth_store
            .load_gemini()
            .expect("reload auth after clear rollback")
            .expect("original creds restored after clear")
            .api_key
            .expose(),
        original_key
    );

    // 3. When no prior key existed, combined Set + default_model failure must roll back by clearing.
    auth_store.clear_gemini().expect("clear prior key");
    assert!(auth_store.load_gemini().expect("verify cleared").is_none());
    assert!(
        server
            .save_settings_snapshot(
                SaveSettingsArgs {
                    providers: Some(ProvidersSettingsPatch {
                        gemini: Some(GeminiSettingsPatch {
                            api_key: Some(ApiKeyPatch::Set("AIzaSyDay0Mutated9999".to_string())),
                            default_model: Some("gemini-3.8-flash".to_string()),
                            ..Default::default()
                        }),
                        ..Default::default()
                    }),
                    ..Default::default()
                },
                None,
            )
            .await
            .is_err()
    );
    assert!(
        auth_store
            .load_gemini()
            .expect("reload auth after day0 rollback")
            .is_none()
    );
    assert!(!auth_store.gemini_path().exists());

    // Explicitly restore 0700 before TempDir drops (RestoreDirMode also handles panics).
    drop(_restore_guard);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn get_settings_reflects_redacted_codex_auth_and_fails_closed_on_corrupt_or_symlink() {
    use companion_core::{
        AUTH_SCHEMA_VERSION, AuthStore, CodexAuthMode, CodexCredentials, FileAuthStore,
        SecretString,
    };

    let tmp = TempDir::new().expect("temp dir");
    let cfg_path = tmp.path().join("config.json");
    let auth_store = FileAuthStore::for_config_path(&cfg_path).expect("auth store");
    let server = isolated_ui_server(&tmp, cfg_path.clone());

    // 1. Authenticated auth/codex.json appears redacted in get_settings_snapshot with zero token leakage.
    let creds = CodexCredentials {
        schema_version: AUTH_SCHEMA_VERSION,
        auth_mode: CodexAuthMode::ChatGptOAuth,
        access_token: SecretString::new("eyJhbGciOiJSUzI1NiJ9.ui_codex_access_secret.sig"),
        refresh_token: SecretString::new("rt_ui_codex_refresh_secret"),
        id_token: Some(SecretString::new(
            "eyJhbGciOiJSUzI1NiJ9.ui_codex_id_secret.sig",
        )),
        account_id: "acct_ui_99887766wxyz".to_string(),
        expires_at_ms: 1_950_000_000_000,
        plan_type: Some("pro".to_string()),
        email_hint: Some("j***@example.com".to_string()),
        last_refresh_utc: "2026-11-26T22:00:00Z".to_string(),
    };
    auth_store.save_codex(&creds).expect("save codex creds");

    let snap = server
        .get_settings_snapshot(None)
        .await
        .expect("get_settings_snapshot");
    let raw = serde_json::to_string(&snap).expect("serialize snapshot");
    for secret in [
        "ui_codex_access_secret",
        "rt_ui_codex_refresh_secret",
        "ui_codex_id_secret",
        "acct_ui_99887766wxyz",
    ] {
        assert!(!raw.contains(secret), "get_settings leaked '{secret}'");
    }
    assert!(snap.config.providers.codex.authenticated);
    assert_eq!(
        snap.config.providers.codex.account_hint.as_deref(),
        Some("j***@example.com")
    );
    assert_eq!(
        snap.config.providers.codex.plan_type.as_deref(),
        Some("pro")
    );
    assert_eq!(
        snap.config.providers.codex.expires_at_ms,
        Some(1_950_000_000_000)
    );

    // 2. Corrupt auth/codex.json fails closed in get_settings, save_settings, test_connection,
    // and set_launcher_integration without mutating disk.
    fs::write(auth_store.codex_path(), "{corrupt_codex_json").expect("write corrupt");
    assert!(server.get_settings_snapshot(None).await.is_err());
    assert!(
        server
            .test_connection_payload(TestConnectionProvider::Gemini)
            .await
            .is_err()
    );
    assert!(
        server
            .test_connection_payload(TestConnectionProvider::Codex)
            .await
            .is_err()
    );
    assert!(
        server
            .save_settings_snapshot(
                SaveSettingsArgs {
                    providers: Some(ProvidersSettingsPatch {
                        gemini: Some(GeminiSettingsPatch {
                            default_model: Some("gemini-4-neon".to_string()),
                            ..Default::default()
                        }),
                        ..Default::default()
                    }),
                    ..Default::default()
                },
                None,
            )
            .await
            .is_err()
    );
    assert!(
        server
            .set_launcher_integration_snapshot(true, None)
            .await
            .is_err()
    );
    assert!(
        !cfg_path.exists(),
        "save_settings must not mutate config when auth/codex.json is corrupt"
    );

    #[cfg(unix)]
    {
        use std::os::unix::fs::symlink;

        fs::remove_file(auth_store.codex_path()).expect("remove corrupt");
        let victim = tmp.path().join("victim-codex.json");
        fs::write(&victim, "victim-codex-sentinel").expect("write victim");
        symlink(&victim, auth_store.codex_path()).expect("symlink codex");

        assert!(server.get_settings_snapshot(None).await.is_err());
        assert!(
            server
                .test_connection_payload(TestConnectionProvider::Gemini)
                .await
                .is_err()
        );
        assert!(
            server
                .test_connection_payload(TestConnectionProvider::Codex)
                .await
                .is_err()
        );
        assert!(
            server
                .set_launcher_integration_snapshot(true, None)
                .await
                .is_err()
        );
        assert_eq!(
            fs::read_to_string(&victim).unwrap(),
            "victim-codex-sentinel"
        );
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn save_settings_codex_patch_persists_and_rejects_invalid_atomically() {
    let tmp = TempDir::new().expect("tempdir");
    let cfg_path = tmp.path().join("config.json");
    let auth_store = FileAuthStore::for_config_path(&cfg_path).expect("auth store");
    let server = isolated_ui_server(&tmp, cfg_path.clone());

    let initial = server
        .get_settings_snapshot(None)
        .await
        .expect("initial snapshot");
    assert_eq!(initial.supported_codex_models.len(), 4);
    assert_eq!(initial.supported_codex_models[0].id, "gpt-6.2-astra");
    assert!(!initial.codex_login_pending);

    // 1. Atomic rejection: valid Gemini API key + invalid Codex base_url writes neither disk file.
    assert!(
        server
            .save_settings_snapshot(
                SaveSettingsArgs {
                    providers: Some(ProvidersSettingsPatch {
                        gemini: Some(GeminiSettingsPatch {
                            api_key: Some(ApiKeyPatch::Set("test-codex-atomic-key".to_string())),
                            ..Default::default()
                        }),
                        codex: Some(CodexSettingsPatch {
                            base_url: Some("http://evil.example.com".to_string()),
                            ..Default::default()
                        }),
                    }),
                    ..Default::default()
                },
                None,
            )
            .await
            .is_err()
    );
    assert!(!cfg_path.exists());
    assert!(!auth_store.gemini_path().exists());

    // 2. Atomic rejection: invalid Codex model ID writes nothing.
    assert!(
        server
            .save_settings_snapshot(
                SaveSettingsArgs {
                    providers: Some(ProvidersSettingsPatch {
                        codex: Some(CodexSettingsPatch {
                            enabled_models: Some(vec!["gemini-4-carbon".to_string()]),
                            ..Default::default()
                        }),
                        ..Default::default()
                    }),
                    ..Default::default()
                },
                None,
            )
            .await
            .is_err()
    );
    assert!(!cfg_path.exists());

    // 3. Valid Codex patch persists cleanly (including empty enabled_models pause) without touching Gemini.
    let saved = server
        .save_settings_snapshot(
            SaveSettingsArgs {
                providers: Some(ProvidersSettingsPatch {
                    codex: Some(CodexSettingsPatch {
                        default_model: Some("gpt-6.2-astra".to_string()),
                        enabled_models: Some(vec![
                            "gpt-6.2-astra".to_string(),
                            "gpt-6.1-terra".to_string(),
                        ]),
                        base_url: None,
                    }),
                    ..Default::default()
                }),
                ..Default::default()
            },
            None,
        )
        .await
        .expect("save codex settings");
    assert_eq!(saved.config.providers.codex.default_model, "gpt-6.2-astra");
    assert_eq!(
        saved.config.providers.codex.enabled_models,
        vec!["gpt-6.2-astra", "gpt-6.1-terra"]
    );

    let paused = server
        .save_settings_snapshot(
            SaveSettingsArgs {
                providers: Some(ProvidersSettingsPatch {
                    codex: Some(CodexSettingsPatch {
                        enabled_models: Some(vec![]),
                        ..Default::default()
                    }),
                    ..Default::default()
                }),
                ..Default::default()
            },
            None,
        )
        .await
        .expect("pause codex models");
    assert!(paused.config.providers.codex.enabled_models.is_empty());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn codex_oauth_lifecycle_and_test_connection_via_ui_server() {
    use companion_providers::{CodexAuthManager, CodexAuthOptions};

    let tmp = TempDir::new().expect("tempdir");
    let cfg_path = tmp.path().join("config.json");

    // Helper: minimal unsigned JWT encoding account_id, plan_type, email, and future exp.
    fn make_test_jwt(payload_json: &str) -> String {
        fn b64url(input: &[u8]) -> String {
            const TABLE: &[u8; 64] =
                b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";
            let mut out = String::new();
            for chunk in input.chunks(3) {
                let b0 = chunk[0] as u32;
                let b1 = chunk.get(1).copied().unwrap_or(0) as u32;
                let b2 = chunk.get(2).copied().unwrap_or(0) as u32;
                let triple = (b0 << 16) | (b1 << 8) | b2;
                out.push(TABLE[((triple >> 18) & 0x3f) as usize] as char);
                out.push(TABLE[((triple >> 12) & 0x3f) as usize] as char);
                if chunk.len() > 1 {
                    out.push(TABLE[((triple >> 6) & 0x3f) as usize] as char);
                }
                if chunk.len() > 2 {
                    out.push(TABLE[(triple & 0x3f) as usize] as char);
                }
            }
            out
        }
        format!(
            "{}.{}.sig",
            b64url(br#"{"alg":"RS256","typ":"JWT"}"#),
            b64url(payload_json.as_bytes())
        )
    }

    let access_jwt = make_test_jwt(
        r#"{"exp":4102444800,"https://api.openai.com/auth":{"chatgpt_account_id":"acct_ui_test_12345","chatgpt_plan_type":"pro"},"https://api.openai.com/profile":{"email":"jack@example.com"}}"#,
    );
    let access_jwt_for_oauth = access_jwt.clone();
    let access_jwt_for_codex = access_jwt.clone();
    let refresh_secret = "rt_ui_secret_should_never_leak";
    let codex_hits = Arc::new(AtomicUsize::new(0));
    let codex_hits_clone = Arc::clone(&codex_hits);

    let mock_router = Router::new()
        .route(
            "/oauth/token",
            post(move || {
                let access = access_jwt_for_oauth.clone();
                async move {
                    axum::Json(json!({
                        "access_token": access,
                        "refresh_token": refresh_secret,
                        "expires_in": 3600
                    }))
                }
            }),
        )
        .route(
            "/responses",
            post(move |headers: axum::http::HeaderMap| {
                let expected_auth = format!("Bearer {access_jwt_for_codex}");
                let hits = Arc::clone(&codex_hits_clone);
                async move {
                    hits.fetch_add(1, Ordering::SeqCst);
                    let auth = headers
                        .get("authorization")
                        .and_then(|v| v.to_str().ok())
                        .unwrap_or("");
                    let acct = headers
                        .get("chatgpt-account-id")
                        .and_then(|v| v.to_str().ok())
                        .unwrap_or("");
                    if auth == expected_auth && acct == "acct_ui_test_12345" {
                        let sse = "data: {\"type\":\"response.output_text.delta\",\"delta\":\"ok\"}\n\ndata: {\"type\":\"response.completed\",\"response\":{\"status\":\"completed\",\"usage\":{\"input_tokens\":10,\"output_tokens\":2}}}\n\n";
                        (
                            StatusCode::OK,
                            [("content-type", "text/event-stream")],
                            sse.to_string(),
                        )
                    } else {
                        (
                            StatusCode::UNAUTHORIZED,
                            [("content-type", "application/json")],
                            r#"{"error":{"message":"unauthorized"}}"#.to_string(),
                        )
                    }
                }
            }),
        );
    let (mock_port, mock_handle) = spawn_mock_http(mock_router).await;
    tokio::task::yield_now().await;

    let mut cfg = config_with_port(free_closed_port().await);
    cfg.providers.codex.base_url = format!("http://127.0.0.1:{mock_port}");
    cfg.save_secure(&cfg_path).expect("save config");

    let auth_store: Arc<dyn AuthStore> =
        Arc::new(FileAuthStore::for_config_path(&cfg_path).expect("file auth store"));
    let codex_auth = Arc::new(
        CodexAuthManager::with_options(
            Arc::clone(&auth_store),
            CodexAuthOptions {
                oauth_issuer: format!("http://127.0.0.1:{mock_port}"),
                callback_bind_port: 0,
                ..CodexAuthOptions::default()
            },
        )
        .expect("codex auth manager"),
    );

    let server = isolated_ui_server_with(&tmp, cfg_path.clone(), |o| {
        o.auth_store_override = Some(Arc::clone(&auth_store));
        o.codex_auth_override = Some(Arc::clone(&codex_auth));
    });

    // 1. Unauthenticated Codex test_connection fails cleanly without contacting upstream.
    let unauth = server
        .test_connection_payload(TestConnectionProvider::Codex)
        .await
        .expect("unauth codex test");
    assert!(!unauth.ok);
    assert_eq!(codex_hits.load(Ordering::SeqCst), 0);

    // 2. start_codex_login -> pending == true -> cancel_codex_login_snapshot -> pending == false.
    let started1 = server.start_codex_login().await.expect("start login 1");
    assert!(started1.authorize_url.contains("/oauth/authorize?"));
    assert!(started1.callback_listening);
    assert!(!started1.manual_fallback);
    assert!(
        server
            .get_settings_snapshot(None)
            .await
            .expect("pending snap")
            .codex_login_pending
    );

    let canceled = server
        .cancel_codex_login_snapshot(None)
        .await
        .expect("cancel login");
    assert!(!canceled.codex_login_pending);
    assert!(!canceled.config.providers.codex.authenticated);

    // 3. start_codex_login + complete_codex_login_snapshot via manual callback URL.
    let started2 = server.start_codex_login().await.expect("start login 2");
    let state_param = started2
        .authorize_url
        .split('&')
        .find_map(|kv| kv.strip_prefix("state="))
        .expect("state query param")
        .to_string();
    let completed = server
        .complete_codex_login_snapshot(
            &format!("http://localhost:1455/auth/callback?code=authcode-123&state={state_param}"),
            None,
        )
        .await
        .expect("complete codex login");
    assert!(!completed.codex_login_pending);
    assert!(completed.config.providers.codex.authenticated);
    assert_eq!(
        completed.config.providers.codex.plan_type.as_deref(),
        Some("pro")
    );
    assert_eq!(
        completed.config.providers.codex.account_hint.as_deref(),
        Some("jack@example.com")
    );
    let completed_raw = serde_json::to_string(&completed).expect("serialize completed");
    assert!(!completed_raw.contains(&access_jwt));
    assert!(!completed_raw.contains(refresh_secret));

    // 4. Authenticated Codex test_connection succeeds against mock Responses server.
    let ok = server
        .test_connection_payload(TestConnectionProvider::Codex)
        .await
        .expect("authed codex test");
    assert!(ok.ok, "expected ok, got: {}", ok.message);
    assert_eq!(ok.message, "Connected \u{00b7} OpenAI Codex");
    assert!(!ok.message.contains("http://") && !ok.message.contains("https://"));
    assert!(!ok.message.contains(&access_jwt));
    assert!(!ok.message.contains(refresh_secret));
    assert_eq!(codex_hits.load(Ordering::SeqCst), 1);

    // 5. disconnect_codex_snapshot clears auth/codex.json and resets authenticated.
    let disconnected = server
        .disconnect_codex_snapshot(None)
        .await
        .expect("disconnect codex");
    assert!(!disconnected.config.providers.codex.authenticated);
    assert!(!disconnected.codex_login_pending);
    assert!(auth_store.load_codex().expect("load codex").is_none());

    mock_handle.abort();
}

#[tokio::test]
async fn get_settings_latches_hybrid_active_in_production_and_respects_override() {
    let tmp = TempDir::new().expect("tmp");
    let cfg_path = tmp.path().join("config.json");
    let stats_hits = Arc::new(AtomicUsize::new(0));
    let stats_hits_clone = Arc::clone(&stats_hits);

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
            get(move || {
                let hits = Arc::clone(&stats_hits_clone);
                async move {
                    let n = hits.fetch_add(1, Ordering::SeqCst);
                    // Calls 0 and 2 return Some(ts); calls 1 and 3 return None (simulating proxy restart).
                    let refresh_ts = if n.is_multiple_of(2) {
                        json!(1700000000000_u64)
                    } else {
                        Value::Null
                    };
                    axum::Json(json!({
                        "usage": {
                            "requests": 0,
                            "prompt_tokens": 0,
                            "cached_tokens": 0,
                            "output_tokens": 0,
                            "reasoning_tokens": 0,
                            "cache_hit_ratio": 0.0
                        },
                        "cloudcode": {
                            "mode": "auto",
                            "resolved_upstream": "https://cloudcode-pa.googleapis.com",
                            "last_catalog_refresh_at_ms": refresh_ts,
                            "last_catalog_included_proxy_models": false
                        },
                        "proxy_rss_bytes": 1048576_u64,
                        "recent_calls": [],
                        "by_model": []
                    }))
                }
            }),
        );

    let (port, handle) = spawn_mock_http(router).await;
    let cfg = config_with_port(port);
    cfg.save_secure(&cfg_path).expect("save");

    // Pass an explicit Enabled + hybrid_active=false IntegrationStatus to
    // build_snapshot_with_session so the test is 100% hermetic even when cargo
    // runs inside a Hybrid-Mode terminal with CLOUD_CODE_URL / HOOK_ACTIVE set.
    let make_non_hybrid_enabled = || {
        companion_core::integration::IntegrationStatus::enabled(
            companion_core::integration::IntegrationPaths {
                shell: companion_core::ShellKind::Zsh,
                rc_path: tmp.path().join(".zlogin"),
                env_script_path: Some(tmp.path().join("data/env.sh")),
                binary_path: tmp.path().join("agy-companion"),
            },
            None,
            false,
        )
    };

    // 1. Production mode (`hybrid_active_override: None`): latch survives proxy restart
    //    when `last_catalog_refresh_at_ms` drops back to `None`.
    let prod_server = isolated_ui_server_with(&tmp, cfg_path.clone(), |opts| {
        opts.hybrid_active_override = None;
    });
    let snap1 = prod_server
        .build_snapshot_with_session(&cfg, None, None, Some(make_non_hybrid_enabled()), None)
        .await;
    assert_eq!(snap1.last_catalog_refresh_at_ms, Some(1_700_000_000_000));
    assert!(snap1.launcher_integration.hybrid_active);
    assert_eq!(snap1.launcher_integration.state, IntegrationState::Active);

    let snap2 = prod_server
        .build_snapshot_with_session(&cfg, None, None, Some(make_non_hybrid_enabled()), None)
        .await;
    assert_eq!(snap2.last_catalog_refresh_at_ms, None);
    assert!(
        snap2.launcher_integration.hybrid_active,
        "latched_hybrid_active must persist after last_catalog_refresh_at_ms drops to None"
    );
    assert_eq!(snap2.launcher_integration.state, IntegrationState::Active);

    // 2. Test-override mode (`hybrid_active_override: Some(false)`): latch is bypassed
    //    so deterministic tests stay unaffected across snapshots.
    let override_server = isolated_ui_server_with(&tmp, cfg_path, |opts| {
        opts.hybrid_active_override = Some(false);
    });
    let ov1 = override_server
        .build_snapshot_with_session(&cfg, None, None, Some(make_non_hybrid_enabled()), None)
        .await;
    assert_eq!(ov1.last_catalog_refresh_at_ms, Some(1_700_000_000_000));
    assert!(ov1.launcher_integration.hybrid_active);
    assert_eq!(ov1.launcher_integration.state, IntegrationState::Active);

    let ov2 = override_server
        .build_snapshot_with_session(&cfg, None, None, Some(make_non_hybrid_enabled()), None)
        .await;
    assert_eq!(ov2.last_catalog_refresh_at_ms, None);
    assert!(
        !ov2.launcher_integration.hybrid_active,
        "hybrid_active_override must bypass latch reads/writes for test determinism"
    );
    assert_eq!(ov2.launcher_integration.state, IntegrationState::Enabled);

    handle.abort();
}

#[tokio::test]
async fn settings_snapshot_omits_update_when_none_and_includes_when_provider_set() {
    let tmp = TempDir::new().expect("tmp");
    let cfg_path = tmp.path().join("config.json");
    let cfg = config_with_port(free_closed_port().await);
    cfg.save_secure(&cfg_path).expect("save");

    // Default (no provider): `update` is None and omitted from serialized JSON.
    let default_server = isolated_ui_server(&tmp, cfg_path.clone());
    let default_snap = default_server
        .get_settings_snapshot(None)
        .await
        .expect("default snapshot");
    assert_eq!(default_snap.update, None);
    let default_json: Value = serde_json::to_value(&default_snap).expect("to value");
    assert!(
        default_json.get("update").is_none(),
        "update key must be omitted when None to preserve golden fixture compatibility"
    );

    // With provider: `update` is populated with exact wire schema.
    let expected_update = UpdateStatusSnapshot {
        state: UpdateLifecycleState::Available,
        channel: companion_core::InstallChannel::Npx,
        current_version: "0.1.3-dev.2".to_string(),
        latest_version: Some("0.1.3".to_string()),
        can_one_click_upgrade: true,
        fallback_command: companion_core::InstallChannel::Npx
            .fallback_command()
            .to_string(),
        checked_at_ms: Some(1_760_000_000_000),
        error: None,
    };
    let expected_clone = expected_update.clone();
    let provider_server = isolated_ui_server_with(&tmp, cfg_path, move |opts| {
        opts.update_status_provider = Some(Arc::new(move |_| Some(expected_clone.clone())));
    });
    let snap = provider_server
        .get_settings_snapshot(None)
        .await
        .expect("snapshot with update");
    assert_eq!(snap.update.as_ref(), Some(&expected_update));

    let raw = serde_json::to_value(&snap).expect("serialize");
    let upd = raw.get("update").expect("update field");
    assert_eq!(upd["state"], "available");
    assert_eq!(upd["channel"], "npx");
    assert_eq!(upd["current_version"], "0.1.3-dev.2");
    assert_eq!(upd["latest_version"], "0.1.3");
    assert_eq!(upd["can_one_click_upgrade"], true);
    assert_eq!(
        upd["fallback_command"],
        "npx -y agy-companion@latest install"
    );
    assert_eq!(upd["checked_at_ms"], 1_760_000_000_000u64);
    assert!(upd.get("error").is_none());
}
