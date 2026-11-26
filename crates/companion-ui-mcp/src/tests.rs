use std::fs;
use std::num::NonZeroU16;
#[cfg(unix)]
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use axum::Router;
use axum::http::StatusCode;
use axum::routing::get;
use companion_core::integration::{IntegrationOptions, IntegrationState};
use companion_core::{ApiKey, CloudCodeUpstream, Config, LoopbackHost, curated_model_presets};
use companion_providers::MAX_PROBE_BODY_BYTES;
use rmcp::ServiceExt;
use rmcp::handler::server::wrapper::Parameters;
use rmcp::model::{ErrorCode, ResourceContents};
use serde_json::{Value, json};
use tempfile::TempDir;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::TcpListener;

use crate::ui_resource::{companion_icons, encode_base64};
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
        auto_enable_launcher: false,
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
    let (chunks, _) = bytes.as_chunks::<4>();
    for chunk in chunks {
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

    let disk_logo = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../plugin/assets/logo.svg");
    let disk_bytes = fs::read(&disk_logo).expect("read plugin/assets/logo.svg");
    assert_eq!(decoded, disk_bytes);
}

#[test]
fn open_settings_tool_declares_aux_pane_and_global_entrypoints() {
    let tmp = TempDir::new().expect("tempdir");
    let server = isolated_ui_server(&tmp, tmp.path().join("config.json"));
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
        ("get_usage", "Get Historical Usage Summary"),
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
            assert!(props.contains_key("account_plan"));
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
        if app_only == "get_usage" {
            assert_eq!(
                tool.input_schema.get("additionalProperties"),
                Some(&json!(false)),
                "get_usage schema must deny unknown fields: {:?}",
                tool.input_schema
            );
            let props = tool
                .input_schema
                .get("properties")
                .and_then(Value::as_object)
                .expect("get_usage properties object");
            assert!(props.contains_key("range"));
            let schema_str = serde_json::to_string(&tool.input_schema).expect("schema json");
            assert!(schema_str.contains("\"1d\""));
            assert!(schema_str.contains("\"7d\""));
            assert!(schema_str.contains("\"30d\""));
        }
    }
}

#[tokio::test]
async fn ui_resource_serves_mcp_app_mime_and_non_empty_html() {
    let tmp = TempDir::new().expect("tempdir");
    let server = isolated_ui_server(&tmp, tmp.path().join("config.json"));

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
            let text_lower = text.to_lowercase();
            let forbidden_ui_copy = [
                "cloudcode",
                "jetski",
                "-cloudcode-pa.googleapis.com",
                "restart required",
                "agy companion",
                "coming soon",
                "refreshbtn",
                "auto \u{2192} workspace",
            ];
            for token in forbidden_ui_copy {
                assert!(
                    !text_lower.contains(token),
                    "forbidden UX v2 token `{token}` found in embedded UI HTML"
                );
            }
            assert_eq!(
                meta.as_ref()
                    .and_then(|m| m.0.get("ui"))
                    .and_then(|u| u.get("prefersBorder")),
                Some(&json!(false))
            );
        }
        other => panic!("expected text resource contents, got {other:?}"),
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

    let server = isolated_ui_server(&tmp, cfg_path);

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
    let server = isolated_ui_server(&tmp, cfg_path.clone());

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
                    ..Default::default()
                }),
            }),
            ..Default::default()
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

    let server = isolated_ui_server(&tmp, cfg_path.clone());

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
    let server = isolated_ui_server(&tmp, cfg_path.clone());

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

    let server = isolated_ui_server(&tmp, cfg_path.clone());
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
        serde_json::from_value(res_skew.structured_content.expect("structured")).expect("parse");
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
        serde_json::from_value(res_huge.structured_content.expect("structured")).expect("parse");
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
        serde_json::from_value(res_down.structured_content.expect("structured")).expect("parse");
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

        let server = isolated_ui_server(&tmp, cfg_path);
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
        .arg("tests::proxy_probe_ignores_hostile_http_proxy_env")
        .arg("--nocapture")
        .env("AGY_COMPANION_HOSTILE_PROXY_CHILD", "1")
        .env("HTTP_PROXY", "http://127.0.0.1:1")
        .env("HTTPS_PROXY", "http://127.0.0.1:1")
        .env("ALL_PROXY", "http://127.0.0.1:1")
        .output()
        .expect("spawn child test");
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        output.status.success(),
        "hostile proxy child failed:\nstdout: {stdout}\nstderr: {stderr}"
    );
    assert!(
        stdout.contains("1 passed"),
        "expected child test to execute 1 test, stdout: {stdout}"
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

    let server = isolated_ui_server(&tmp, cfg_path.clone());
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
    assert!(payload_bad.message.contains("API_KEY_INVALID") || payload_bad.message.contains("400"));
    bad_handle.abort();

    // 3. Missing API key (deterministic with env fallback disabled)
    cfg.providers.gemini.api_key = None;
    cfg.save_secure(&cfg_path).expect("save missing");
    let res_missing = server.test_connection().await.expect("test missing");
    assert_eq!(res_missing.is_error, Some(false));
    let payload_missing: TestConnectionPayload =
        serde_json::from_value(res_missing.structured_content.expect("structured")).expect("parse");
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

    let fast_timeout_server = isolated_ui_server_with(&tmp, cfg_path, |o| {
        o.test_connection_timeout_override = Some(Duration::from_millis(150));
    });
    let res_timeout = fast_timeout_server
        .test_connection()
        .await
        .expect("test timeout");
    assert_eq!(res_timeout.is_error, Some(false));
    let serialized_timeout = serde_json::to_string(&res_timeout).expect("serialize");
    assert!(!serialized_timeout.contains(valid_secret));
    let payload_timeout: TestConnectionPayload =
        serde_json::from_value(res_timeout.structured_content.expect("structured")).expect("parse");
    assert!(!payload_timeout.ok);
    slow_handle.abort();
}

#[tokio::test]
async fn ui_resource_dynamic_reload_oversized_rejection_and_unknown_uri() {
    let tmp = TempDir::new().expect("tempdir");
    let html_path = tmp.path().join("override.html");
    fs::write(&html_path, "<!doctype html><title>v1</title>").expect("write v1");

    let server = isolated_ui_server_with(&tmp, tmp.path().join("config.json"), |o| {
        o.ui_html_source = UiHtmlSource::Fixed(Some(html_path.clone()));
    });

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
    let dir_server = isolated_ui_server_with(&tmp, tmp.path().join("config.json"), |o| {
        o.ui_html_source = UiHtmlSource::Fixed(Some(tmp.path().to_path_buf()));
    });
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
        let server = CompanionUiServer::new(tmp.path().join("config.json")).expect("server");
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
        .arg("tests::ui_resource_reads_process_env_var_dynamically")
        .arg("--nocapture")
        .env("AGY_COMPANION_UI_HTML_CHILD", "1")
        .env(UI_HTML_ENV_VAR, &html_path)
        .output()
        .expect("spawn child");
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        output.status.success(),
        "env override child failed:\nstdout: {stdout}\nstderr: {stderr}"
    );
    assert!(
        stdout.contains("1 passed"),
        "expected child test to execute 1 test, stdout: {stdout}"
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

    let server = isolated_ui_server(&tmp, cfg_path);

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

    async fn recv_msg(r: &mut BufReader<tokio::io::ReadHalf<tokio::io::DuplexStream>>) -> Value {
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
    assert_eq!(tools.len(), 6);
    assert!(
        tools
            .iter()
            .any(|t| t.get("name") == Some(&json!("set_launcher_integration")))
    );
    assert!(
        tools
            .iter()
            .any(|t| t.get("name") == Some(&json!("get_usage")))
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
    let save_rejected =
        bad_save_resp.get("error").is_some() || bad_save_resp["result"]["isError"] == json!(true);
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
    let server = isolated_ui_server(&tmp, cfg_path.clone());

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

        let server = isolated_ui_server(&tmp, cfg_path.clone());

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
            ApiKey::parse_strict(&format!("AIzaSyMustStayCleared{trial:02}")).expect("valid key"),
        );
        initial.save_secure(&cfg_path).expect("seed config");

        let server = isolated_ui_server(&tmp, cfg_path.clone());

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

    let server = isolated_ui_server_with(&tmp, tmp.path().join("config.json"), |o| {
        o.ui_html_source = UiHtmlSource::Fixed(Some(fifo_path));
    });

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

    let server = isolated_ui_server(&tmp, cfg_path);
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

    let server = isolated_ui_server(&tmp, cfg_path.clone());

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

    // Legacy placeholder on disk must also fail test_connection cleanly without hitting upstream.
    let conn_res = server.test_connection().await.expect("test_connection");
    assert_eq!(conn_res.is_error, Some(false));
    let conn_payload: TestConnectionPayload =
        serde_json::from_value(conn_res.structured_content.expect("structured")).expect("parse");
    assert!(!conn_payload.ok);

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
    let server = isolated_ui_server_with(&tmp, cfg_path.clone(), |o| {
        o.integration_options_override = Some(opts);
    });

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

        let server_force = isolated_ui_server_with(&tmp, cfg_path, |o| {
            o.integration_options_override = Some(IntegrationOptions {
                shell: Some(companion_core::ShellKind::Zsh),
                rc_file: Some(rc_path.clone()),
                binary_path: Some(fake_bin),
                data_dir: Some(data_dir.clone()),
                follow_rc_symlink: true, // MUST be overridden to false inside tool!
            });
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

    let server = isolated_ui_server_with(&tmp, cfg_path, |o| {
        o.integration_options_override = Some(opts);
    });
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

    let server = Arc::new(isolated_ui_server_with(&tmp, cfg_path, |o| {
        o.integration_options_override = Some(IntegrationOptions {
            shell: Some(companion_core::ShellKind::Zsh),
            rc_file: Some(rc_path.clone()),
            binary_path: Some(fake_bin),
            data_dir: Some(data_dir),
            follow_rc_symlink: false,
        });
    }));

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
async fn set_launcher_integration_disable_on_symlink_and_fish_manual_required_copy_is_accurate() {
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

    let server = isolated_ui_server_with(&tmp, cfg_path.clone(), |o| {
        o.integration_options_override = Some(IntegrationOptions {
            shell: Some(companion_core::ShellKind::Zsh),
            rc_file: Some(rc_path),
            binary_path: Some(fake_bin.clone()),
            data_dir: Some(data_dir.clone()),
            follow_rc_symlink: false,
        });
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

    let fish_server = isolated_ui_server_with(&tmp, cfg_path, |o| {
        o.integration_options_override = Some(IntegrationOptions {
            shell: Some(companion_core::ShellKind::Fish),
            rc_file: Some(fish_rc),
            binary_path: Some(fake_bin.clone()),
            data_dir: Some(data_dir),
            follow_rc_symlink: false,
        });
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
    let fish_snippet =
        companion_core::integration::render_rc_snippet(companion_core::ShellKind::Fish, &fake_bin)
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
fn save_settings_args_denies_unknown_fields_at_every_nesting_level_and_api_key_patch_variants() {
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

    // Valid account_plan & enabled_models variants
    for (raw_plan, expected_plan) in [
        ("auto", AccountPlan::Auto),
        ("personal", AccountPlan::Personal),
        ("organization", AccountPlan::Organization),
    ] {
        let parsed: SaveSettingsArgs = serde_json::from_value(json!({
            "account_plan": raw_plan,
            "providers": { "gemini": { "enabled_models": ["gemini-4-carbon"] } }
        }))
        .expect("valid account_plan + enabled_models");
        assert_eq!(parsed.account_plan, Some(expected_plan));
        assert_eq!(
            parsed
                .providers
                .as_ref()
                .and_then(|p| p.gemini.as_ref())
                .and_then(|g| g.enabled_models.as_ref()),
            Some(&vec!["gemini-4-carbon".to_string()])
        );
    }
    let paused_patch: SaveSettingsArgs = serde_json::from_value(json!({
        "providers": { "gemini": { "enabled_models": [] } }
    }))
    .expect("empty enabled_models pause patch");
    assert_eq!(
        paused_patch
            .providers
            .as_ref()
            .and_then(|p| p.gemini.as_ref())
            .and_then(|g| g.enabled_models.as_ref()),
        Some(&Vec::new())
    );

    // Pure ConfigPatch::try_from mutual exclusion & account_plan -> cloudcode.upstream mapping
    let conflict_err = companion_core::config::ConfigPatch::try_from(SaveSettingsArgs {
        account_plan: Some(AccountPlan::Personal),
        cloudcode: Some(CloudCodeSettingsPatch {
            upstream: Some(companion_core::CLOUDCODE_PROD_UPSTREAM_URL.to_string()),
        }),
        ..Default::default()
    })
    .expect_err("account_plan + cloudcode.upstream must fail");
    assert!(
        conflict_err.contains("account_plan") && conflict_err.contains("cloudcode.upstream"),
        "unexpected conflict error: {conflict_err}"
    );
    let org_patch = companion_core::config::ConfigPatch::try_from(SaveSettingsArgs {
        account_plan: Some(AccountPlan::Organization),
        cloudcode: Some(CloudCodeSettingsPatch { upstream: None }),
        ..Default::default()
    })
    .expect("empty cloudcode alongside account_plan must succeed");
    assert_eq!(
        org_patch.cloudcode.and_then(|c| c.upstream).as_deref(),
        Some(companion_core::CLOUDCODE_PROD_UPSTREAM_URL)
    );

    // Unknown fields at every nesting level, legacy flat keys, and malformed api_key/account_plan variants
    let rejected_payloads = [
        json!({ "unknown_top": 1 }),
        json!({ "listen_port": companion_core::DEFAULT_LISTEN_PORT }),
        json!({ "api_key": "AIzaSyFlatKey123456" }),
        json!({ "clear_api_key": true }),
        json!({ "account_plan": "custom" }),
        json!({ "account_plan": "PERSONAL" }),
        json!({ "account_plan": "auto", "extra": 1 }),
        json!({ "proxy": { "port": companion_core::DEFAULT_LISTEN_PORT, "extra": true } }),
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

    let server = isolated_ui_server(&tmp, cfg_path);
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
        "account_plan",
        "proxy_rss_bytes",
    ] {
        assert!(obj.contains_key(key), "missing snapshot key {key}");
    }
    assert_eq!(
        obj["account_plan"],
        json!({ "mode": "auto", "resolved": null })
    );
    assert_eq!(obj["proxy_rss_bytes"], json!(null));

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
    let server = isolated_ui_server(&tmp, cfg_path.clone());

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

#[tokio::test]
async fn test_connection_returns_is_error_true_on_local_config_load_failure() {
    let tmp = TempDir::new().expect("tempdir");
    let corrupt_cfg = tmp.path().join("corrupt.json");
    fs::write(&corrupt_cfg, "{ not valid json").expect("write corrupt config");

    let server = isolated_ui_server(&tmp, corrupt_cfg);
    let res = server.test_connection().await.expect("call tool");
    assert_eq!(
        res.is_error,
        Some(true),
        "corrupt local config must return isError: true"
    );
    let sc = res.structured_content.expect("structured_content");
    assert_eq!(sc["ok"], json!(false));
    assert!(sc["error"].as_str().is_some_and(|s| !s.is_empty()));

    #[cfg(unix)]
    {
        let real_cfg = tmp.path().join("real.json");
        Config::default()
            .save_secure(&real_cfg)
            .expect("save real config");
        let sym_cfg = tmp.path().join("symlink.json");
        std::os::unix::fs::symlink(&real_cfg, &sym_cfg).expect("symlink config");
        let sym_server = isolated_ui_server(&tmp, sym_cfg);
        let sym_res = sym_server.test_connection().await.expect("call tool");
        assert_eq!(
            sym_res.is_error,
            Some(true),
            "symlinked local config must return isError: true"
        );
    }
}

#[tokio::test]
async fn set_launcher_integration_does_not_mutate_dotfiles_when_config_is_corrupt_or_symlink() {
    let tmp = TempDir::new().expect("tempdir");
    let corrupt_cfg = tmp.path().join("corrupt.json");
    fs::write(&corrupt_cfg, "{ broken").expect("write corrupt config");

    let rc_path = tmp.path().join(".zlogin");
    fs::write(&rc_path, "# pristine\n").expect("write pristine rc");
    let data_dir = tmp.path().join("data");
    let fake_bin = tmp.path().join("agy-companion");
    fs::write(&fake_bin, "#!/bin/sh\n").expect("write fake bin");

    let server = isolated_ui_server_with(&tmp, corrupt_cfg, |o| {
        o.integration_options_override = Some(IntegrationOptions {
            shell: Some(companion_core::ShellKind::Zsh),
            rc_file: Some(rc_path.clone()),
            binary_path: Some(fake_bin.clone()),
            data_dir: Some(data_dir.clone()),
            follow_rc_symlink: false,
        });
    });
    let res = server
        .set_launcher_integration(Parameters(SetLauncherIntegrationArgs { enabled: true }))
        .await
        .expect("call set_launcher_integration");
    assert_eq!(res.is_error, Some(true));
    assert!(
        !data_dir.join("env.sh").exists(),
        "env.sh must NOT be created when config.json fails to load"
    );
    assert_eq!(
        fs::read_to_string(&rc_path).expect("read rc"),
        "# pristine\n",
        "RC file must NOT be mutated when config.json fails to load"
    );

    #[cfg(unix)]
    {
        let real_cfg = tmp.path().join("real.json");
        Config::default()
            .save_secure(&real_cfg)
            .expect("save real config");
        let sym_cfg = tmp.path().join("symlink.json");
        std::os::unix::fs::symlink(&real_cfg, &sym_cfg).expect("symlink config");

        let sym_server = isolated_ui_server_with(&tmp, sym_cfg, |o| {
            o.integration_options_override = Some(IntegrationOptions {
                shell: Some(companion_core::ShellKind::Zsh),
                rc_file: Some(rc_path.clone()),
                binary_path: Some(fake_bin),
                data_dir: Some(data_dir.clone()),
                follow_rc_symlink: false,
            });
        });
        let sym_res = sym_server
            .set_launcher_integration(Parameters(SetLauncherIntegrationArgs { enabled: true }))
            .await
            .expect("call set_launcher_integration on symlink");
        assert_eq!(sym_res.is_error, Some(true));
        assert!(!data_dir.join("env.sh").exists());
        assert_eq!(
            fs::read_to_string(&rc_path).expect("read rc"),
            "# pristine\n"
        );
    }
}

#[tokio::test]
async fn probe_proxy_handles_non_ok_status_without_stats_and_detects_protocol_skew() {
    let tmp = TempDir::new().expect("tempdir");

    // 1. Non-"ok" status -> skips /api/stats completely and reports error
    let stats_hits = Arc::new(AtomicUsize::new(0));
    let stats_hits_clone = Arc::clone(&stats_hits);
    let degraded_router = Router::new()
        .route(
            "/health",
            get(|| async {
                axum::Json(json!({
                    "status": "degraded",
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
                    hits.fetch_add(1, Ordering::SeqCst);
                    axum::Json(json!({}))
                }
            }),
        );
    let (degraded_port, degraded_handle) = spawn_mock_http(degraded_router).await;
    let degraded_cfg = tmp.path().join("degraded.json");
    config_with_port(degraded_port)
        .save_secure(&degraded_cfg)
        .expect("save degraded cfg");

    let server = isolated_ui_server(&tmp, degraded_cfg);
    let res = server.get_settings().await.expect("get_settings");
    let snap: SettingsSnapshot =
        serde_json::from_value(res.structured_content.expect("structured")).expect("parse");
    assert!(snap.proxy.reachable);
    assert_eq!(snap.proxy.status.as_deref(), Some("degraded"));
    assert!(
        snap.proxy
            .error
            .as_deref()
            .is_some_and(|e| e.contains("degraded") && e.contains("non-ok")),
        "expected non-ok degraded error, got {:?}",
        snap.proxy.error
    );
    assert!(snap.stats.is_none());
    assert_eq!(
        stats_hits.load(Ordering::SeqCst),
        0,
        "/api/stats must not be probed when /health status is non-ok"
    );
    degraded_handle.abort();

    // 2. Protocol skew -> reachable, version_skew == true, proxy.error mentions protocol skew
    let skew_router = Router::new().route(
        "/health",
        get(|| async {
            axum::Json(json!({
                "status": "ok",
                "version": env!("CARGO_PKG_VERSION"),
                "protocol_version": 99
            }))
        }),
    );
    let (skew_port, skew_handle) = spawn_mock_http(skew_router).await;
    let skew_cfg = tmp.path().join("skew.json");
    config_with_port(skew_port)
        .save_secure(&skew_cfg)
        .expect("save skew cfg");

    let skew_server = isolated_ui_server(&tmp, skew_cfg);
    let skew_res = skew_server.get_settings().await.expect("get_settings");
    let skew_snap: SettingsSnapshot =
        serde_json::from_value(skew_res.structured_content.expect("structured")).expect("parse");
    assert!(skew_snap.proxy.reachable);
    assert!(skew_snap.version_skew);
    assert_eq!(skew_snap.proxy.protocol_version, Some(99));
    assert!(
        skew_snap
            .proxy
            .error
            .as_deref()
            .is_some_and(|e| e.contains("protocol skew")),
        "expected protocol skew error, got {:?}",
        skew_snap.proxy.error
    );
    skew_handle.abort();
}

#[tokio::test]
async fn ui_html_override_handles_missing_and_symlink() {
    let tmp = TempDir::new().expect("tempdir");

    // Missing file path must return INTERNAL_ERROR (not silently fall back or succeed)
    let missing_server = isolated_ui_server_with(&tmp, tmp.path().join("config.json"), |o| {
        o.ui_html_source = UiHtmlSource::Fixed(Some(tmp.path().join("does-not-exist.html")));
    });
    let missing_err = missing_server
        .read_ui_resource(UI_RESOURCE_URI)
        .await
        .expect_err("missing override file must error");
    assert_eq!(missing_err.code, ErrorCode::INTERNAL_ERROR);

    #[cfg(unix)]
    {
        // Symlink pointing to regular HTML file succeeds (SymlinkPolicy::Follow)
        let real_html = tmp.path().join("real.html");
        fs::write(&real_html, "<!doctype html><title>symlink-ok</title>").expect("write html");
        let sym_html = tmp.path().join("link.html");
        std::os::unix::fs::symlink(&real_html, &sym_html).expect("symlink html");

        let sym_server = isolated_ui_server_with(&tmp, tmp.path().join("config.json"), |o| {
            o.ui_html_source = UiHtmlSource::Fixed(Some(sym_html));
        });
        let read = sym_server
            .read_ui_resource(UI_RESOURCE_URI)
            .await
            .expect("symlink to regular HTML must succeed");
        let text = match &read.contents[0] {
            ResourceContents::TextResourceContents { text, .. } => text.as_str(),
            other => panic!("expected text, got {other:?}"),
        };
        assert!(text.contains("symlink-ok"));

        // Symlink pointing to FIFO fails fast without blocking
        let fifo_path = tmp.path().join("ui.fifo");
        let mkfifo_status = Command::new("mkfifo")
            .arg(&fifo_path)
            .status()
            .expect("mkfifo");
        assert!(mkfifo_status.success());
        let sym_fifo = tmp.path().join("link.fifo");
        std::os::unix::fs::symlink(&fifo_path, &sym_fifo).expect("symlink fifo");

        let fifo_server = isolated_ui_server_with(&tmp, tmp.path().join("config.json"), |o| {
            o.ui_html_source = UiHtmlSource::Fixed(Some(sym_fifo));
        });
        let fifo_err = tokio::time::timeout(
            Duration::from_secs(2),
            fifo_server.read_ui_resource(UI_RESOURCE_URI),
        )
        .await
        .expect("symlinked FIFO must not block")
        .expect_err("symlinked FIFO must fail");
        assert_eq!(fifo_err.code, ErrorCode::INTERNAL_ERROR);
    }
}

#[test]
fn account_plan_status_mapping_matrix_never_leaks_raw_urls() {
    use companion_core::{
        CLOUDCODE_DAILY_UPSTREAM_URL, CLOUDCODE_PROD_UPSTREAM_URL, CloudCodeStatsSnapshot,
        CloudCodeUpstream,
    };

    let auto = CloudCodeUpstream::Auto;
    let daily = CloudCodeUpstream::Fixed(CLOUDCODE_DAILY_UPSTREAM_URL.to_string());
    let prod = CloudCodeUpstream::Fixed(CLOUDCODE_PROD_UPSTREAM_URL.to_string());
    let custom = CloudCodeUpstream::Fixed("http://127.0.0.1:9999".to_string());

    let stats_with = |url: &str| CloudCodeStatsSnapshot {
        mode: "auto".to_string(),
        resolved_upstream: Some(url.to_string()),
        ..Default::default()
    };

    let cases = [
        (
            AccountPlanStatus::from_config_and_stats(&auto, None),
            AccountPlanMode::Auto,
            None,
        ),
        (
            AccountPlanStatus::from_config_and_stats(
                &auto,
                Some(&stats_with("https://daily-cloudcode-pa.googleapis.com/")),
            ),
            AccountPlanMode::Auto,
            Some(ResolvedAccountPlan::Personal),
        ),
        (
            AccountPlanStatus::from_config_and_stats(
                &auto,
                Some(&stats_with("HTTPS://CLOUDCODE-PA.GOOGLEAPIS.COM")),
            ),
            AccountPlanMode::Auto,
            Some(ResolvedAccountPlan::Organization),
        ),
        (
            AccountPlanStatus::from_config_and_stats(
                &auto,
                Some(&stats_with("http://127.0.0.1:9999")),
            ),
            AccountPlanMode::Auto,
            None,
        ),
        (
            AccountPlanStatus::from_config_and_stats(&daily, None),
            AccountPlanMode::Personal,
            Some(ResolvedAccountPlan::Personal),
        ),
        (
            AccountPlanStatus::from_config_and_stats(
                &daily,
                Some(&stats_with(CLOUDCODE_PROD_UPSTREAM_URL)),
            ),
            AccountPlanMode::Personal,
            Some(ResolvedAccountPlan::Personal),
        ),
        (
            AccountPlanStatus::from_config_and_stats(&prod, None),
            AccountPlanMode::Organization,
            Some(ResolvedAccountPlan::Organization),
        ),
        (
            AccountPlanStatus::from_config_and_stats(&custom, None),
            AccountPlanMode::Custom,
            None,
        ),
    ];

    for (status, expected_mode, expected_resolved) in cases {
        assert_eq!(status.mode, expected_mode);
        assert_eq!(status.resolved, expected_resolved);
        let json_str = serde_json::to_string(&status).expect("serialize account_plan");
        assert!(
            !json_str.contains("cloudcode-pa.googleapis.com") && !json_str.contains("127.0.0.1"),
            "account_plan must never leak raw URLs: {json_str}"
        );
    }
}

#[tokio::test]
async fn account_plan_auto_ignores_stale_fixed_mode_proxy_stats() {
    use companion_core::{
        CLOUDCODE_DAILY_UPSTREAM_URL, CLOUDCODE_PROD_UPSTREAM_URL, CloudCodeStatsSnapshot,
        CloudCodeUpstream,
    };

    let auto = CloudCodeUpstream::Auto;

    // Unit-level: stale mode="fixed" (or empty) stats must NOT leak into Auto.resolved.
    let stale_fixed_daily = CloudCodeStatsSnapshot {
        mode: "fixed".to_string(),
        resolved_upstream: Some(CLOUDCODE_DAILY_UPSTREAM_URL.to_string()),
        ..Default::default()
    };
    assert_eq!(
        AccountPlanStatus::from_config_and_stats(&auto, Some(&stale_fixed_daily)),
        AccountPlanStatus {
            mode: AccountPlanMode::Auto,
            resolved: None,
        }
    );

    let stale_fixed_prod = CloudCodeStatsSnapshot {
        mode: "fixed".to_string(),
        resolved_upstream: Some(CLOUDCODE_PROD_UPSTREAM_URL.to_string()),
        ..Default::default()
    };
    assert_eq!(
        AccountPlanStatus::from_config_and_stats(&auto, Some(&stale_fixed_prod)),
        AccountPlanStatus {
            mode: AccountPlanMode::Auto,
            resolved: None,
        }
    );

    let active_auto_prod = CloudCodeStatsSnapshot {
        mode: " AUTO ".to_string(),
        resolved_upstream: Some(CLOUDCODE_PROD_UPSTREAM_URL.to_string()),
        ..Default::default()
    };
    assert_eq!(
        AccountPlanStatus::from_config_and_stats(&auto, Some(&active_auto_prod)),
        AccountPlanStatus {
            mode: AccountPlanMode::Auto,
            resolved: Some(ResolvedAccountPlan::Organization),
        }
    );

    // End-to-end: save_settings({ account_plan: "auto" }) against a proxy still reporting
    // pre-hot-reload mode="fixed" must return account_plan == { mode: "auto", resolved: null }.
    let mock_router = Router::new()
        .route(
            "/health",
            get(|| async {
                axum::Json(json!({
                    "status": "ok",
                    "version": env!("CARGO_PKG_VERSION"),
                    "protocol_version": 1,
                }))
            }),
        )
        .route(
            "/api/stats",
            get(|| async {
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
                        "mode": "fixed",
                        "resolved_upstream": CLOUDCODE_DAILY_UPSTREAM_URL,
                        "last_catalog_refresh_at_ms": null,
                        "last_catalog_included_proxy_models": null
                    }
                }))
            }),
        );
    let (port, handle) = spawn_mock_http(mock_router).await;

    let tmp = TempDir::new().expect("tempdir");
    let cfg_path = tmp.path().join("config.json");
    let mut cfg = config_with_port(port);
    cfg.cloudcode.upstream = CloudCodeUpstream::Fixed(CLOUDCODE_DAILY_UPSTREAM_URL.to_string());
    cfg.save_secure(&cfg_path).expect("save cfg");

    let server = isolated_ui_server(&tmp, cfg_path);
    let res = server
        .save_settings(Parameters(SaveSettingsArgs {
            account_plan: Some(AccountPlan::Auto),
            ..Default::default()
        }))
        .await
        .expect("save_settings");
    assert_eq!(res.is_error, Some(false));
    let sc = res.structured_content.expect("sc");
    assert_eq!(
        sc["account_plan"],
        json!({ "mode": "auto", "resolved": null }),
        "stale fixed-mode proxy stats must not leak resolved plan when switched to auto"
    );

    handle.abort();
}

#[tokio::test]
async fn save_settings_handles_account_plan_enabled_models_and_rejects_conflict() {
    let tmp = TempDir::new().expect("tempdir");
    let cfg_path = tmp.path().join("config.json");
    let closed_port = free_closed_port().await;
    let server = isolated_ui_server(&tmp, cfg_path.clone());

    // 1. Conflict: simultaneous account_plan + cloudcode.upstream must fail closed without touching disk
    let conflict = server
        .save_settings(Parameters(SaveSettingsArgs {
            account_plan: Some(AccountPlan::Personal),
            cloudcode: Some(CloudCodeSettingsPatch {
                upstream: Some("https://cloudcode-pa.googleapis.com".to_string()),
            }),
            ..Default::default()
        }))
        .await
        .expect("conflict call");
    assert_eq!(conflict.is_error, Some(true));
    assert!(!cfg_path.exists(), "conflict must not write config.json");

    // 2. Empty cloudcode: {} alongside account_plan is NOT a conflict; account_plan wins
    let personal = server
        .save_settings(Parameters(SaveSettingsArgs {
            proxy: Some(ProxySettingsPatch {
                port: Some(closed_port),
                ..Default::default()
            }),
            account_plan: Some(AccountPlan::Personal),
            cloudcode: Some(CloudCodeSettingsPatch { upstream: None }),
            providers: Some(ProvidersSettingsPatch {
                gemini: Some(GeminiSettingsPatch {
                    enabled_models: Some(vec![
                        "gemini-4-argon".to_string(),
                        "gemini-4-argon".to_string(),
                        "gemini-3.8-flash".to_string(),
                    ]),
                    ..Default::default()
                }),
            }),
        }))
        .await
        .expect("personal save");
    assert_eq!(personal.is_error, Some(false));
    let sc = personal.structured_content.expect("sc");
    assert_eq!(
        sc["account_plan"],
        json!({ "mode": "personal", "resolved": "personal" })
    );
    assert_eq!(
        sc["config"]["providers"]["gemini"]["enabled_models"],
        json!(["gemini-4-argon", "gemini-3.8-flash"])
    );

    // 3. Empty enabled_models [] + account_plan: "organization" persists cleanly
    let org = server
        .save_settings(Parameters(SaveSettingsArgs {
            account_plan: Some(AccountPlan::Organization),
            providers: Some(ProvidersSettingsPatch {
                gemini: Some(GeminiSettingsPatch {
                    enabled_models: Some(Vec::new()),
                    ..Default::default()
                }),
            }),
            ..Default::default()
        }))
        .await
        .expect("org save");
    assert_eq!(org.is_error, Some(false));
    let sc2 = org.structured_content.expect("sc2");
    assert_eq!(
        sc2["account_plan"],
        json!({ "mode": "organization", "resolved": "organization" })
    );
    assert_eq!(
        sc2["config"]["providers"]["gemini"]["enabled_models"],
        json!([])
    );

    // 4. Invalid model in enabled_models is rejected fail-closed
    let bad_model = server
        .save_settings(Parameters(SaveSettingsArgs {
            providers: Some(ProvidersSettingsPatch {
                gemini: Some(GeminiSettingsPatch {
                    enabled_models: Some(vec!["gemini-2.0-flash".to_string()]),
                    ..Default::default()
                }),
            }),
            ..Default::default()
        }))
        .await
        .expect("bad model call");
    assert_eq!(bad_model.is_error, Some(true));
}

#[tokio::test]
async fn snapshot_propagates_proxy_rss_bytes_from_live_stats() {
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let port = listener.local_addr().expect("addr").port();
    let app = Router::new()
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
                        "output_tokens": 25,
                        "reasoning_tokens": 0,
                        "cache_hit_ratio": 0.5
                    },
                    "cloudcode": {
                        "mode": "auto",
                        "resolved_upstream": "https://cloudcode-pa.googleapis.com"
                    },
                    "proxy_rss_bytes": 19_922_944,
                    "recent_calls": [],
                    "by_model": []
                }))
            }),
        );
    let handle = tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });

    let tmp = TempDir::new().expect("tempdir");
    let cfg_path = tmp.path().join("config.json");
    let mut cfg = Config::default();
    cfg.proxy.port = std::num::NonZeroU16::new(port).expect("nonzero port");
    cfg.save_secure(&cfg_path).expect("save cfg");

    let server = isolated_ui_server(&tmp, cfg_path);

    let res = server.get_settings().await.expect("get_settings");
    assert_eq!(res.is_error, Some(false));
    let sc = res.structured_content.expect("sc");
    assert_eq!(sc["proxy_rss_bytes"], json!(19_922_944));
    assert_eq!(
        sc["account_plan"],
        json!({ "mode": "auto", "resolved": "organization" })
    );

    handle.abort();
}

#[tokio::test]
async fn get_usage_missing_db_returns_empty_and_rejects_bad_args_at_json_boundary() {
    let tmp = TempDir::new().expect("tempdir");
    let cfg_path = tmp.path().join("config.json");
    let usage_dir = tmp.path().join("missing-usage-dir");
    let cfg = Config {
        usage_dir: Some(usage_dir.clone()),
        ..Default::default()
    };
    cfg.save_secure(&cfg_path).expect("save cfg");

    let server = isolated_ui_server(&tmp, cfg_path);
    let res = server
        .get_usage(Parameters(GetUsageArgs {
            range: UsageRangeArg::Days7,
        }))
        .await
        .expect("get_usage");
    assert_eq!(res.is_error, Some(false));
    let sc = res.structured_content.expect("sc");
    assert_eq!(sc["range"], json!("7d"));
    assert_eq!(sc["totals"]["requests"], json!(0));
    assert_eq!(sc["buckets"], json!([]));
    assert!(
        !usage_dir.exists(),
        "get_usage must not create missing usage_dir"
    );

    // Strict serde boundary checks for GetUsageArgs
    assert!(serde_json::from_str::<GetUsageArgs>("{}").is_err());
    assert!(serde_json::from_str::<GetUsageArgs>(r#"{"range":"99d"}"#).is_err());
    assert!(serde_json::from_str::<GetUsageArgs>(r#"{"range":"7d","extra":1}"#).is_err());
}

#[cfg(feature = "sqlite")]
#[tokio::test]
async fn get_usage_queries_seeded_sqlite_and_rejects_symlink_db() {
    #[cfg(unix)]
    use companion_core::USAGE_DB_FILENAME;
    use companion_core::{GeminiUsageMetadata, SqliteUsageSink, UsageEvent};

    let tmp = TempDir::new().expect("tempdir");
    let cfg_path = tmp.path().join("config.json");
    let usage_dir = tmp.path().join("usage");
    let cfg = Config {
        usage_dir: Some(usage_dir.clone()),
        ..Default::default()
    };
    cfg.save_secure(&cfg_path).expect("save cfg");

    let sink = SqliteUsageSink::new(&usage_dir).expect("sink");
    let u1 = GeminiUsageMetadata {
        prompt_token_count: 1000,
        candidates_token_count: 200,
        cached_content_token_count: 600,
        thoughts_token_count: 50,
    };
    let u2 = GeminiUsageMetadata {
        prompt_token_count: 500,
        candidates_token_count: 100,
        cached_content_token_count: 0,
        thoughts_token_count: 0,
    };
    sink.record_checked(&UsageEvent::from_gemini("gemini-4-carbon", &u1, 200))
        .expect("record gemini");
    sink.record_checked(&UsageEvent::from_antigravity_builtin(
        "gemini-3.8-pro",
        &u2,
        100,
    ))
    .expect("record builtin");

    let server = isolated_ui_server(&tmp, cfg_path);
    let res = server
        .get_usage(Parameters(GetUsageArgs {
            range: UsageRangeArg::Today,
        }))
        .await
        .expect("get_usage");
    assert_eq!(res.is_error, Some(false));
    let sc = res.structured_content.expect("sc");
    assert_eq!(sc["range"], json!("1d"));
    assert_eq!(sc["totals"]["requests"], json!(2));
    assert_eq!(sc["totals"]["input_tokens"], json!(1500));
    assert_eq!(sc["by_model"].as_array().expect("by_model").len(), 2);
    assert_eq!(sc["by_provider"].as_array().expect("by_provider").len(), 2);

    #[cfg(unix)]
    {
        let bad_dir = tmp.path().join("symlink-usage");
        fs::create_dir_all(&bad_dir).expect("mkdir");
        let victim = tmp.path().join("victim.db");
        fs::write(&victim, b"").expect("write victim");
        std::os::unix::fs::symlink(&victim, bad_dir.join(USAGE_DB_FILENAME)).expect("symlink");

        let bad_cfg_path = tmp.path().join("bad_config.json");
        let bad_cfg = Config {
            usage_dir: Some(bad_dir),
            ..Default::default()
        };
        bad_cfg.save_secure(&bad_cfg_path).expect("save bad cfg");

        let bad_server = isolated_ui_server(&tmp, bad_cfg_path);
        let bad_res = bad_server
            .get_usage(Parameters(GetUsageArgs {
                range: UsageRangeArg::Days7,
            }))
            .await
            .expect("get_usage error result");
        assert_eq!(bad_res.is_error, Some(true));
    }
}

#[cfg(unix)]
#[tokio::test]
async fn set_launcher_integration_persists_launcher_auto_start_and_open_settings_respects_flag() {
    let tmp = TempDir::new().expect("tempdir");
    let cfg_path = tmp.path().join("config.json");
    // Pin an ephemeral closed port (launcher_auto_start remains None) so none of steps 1-5 probe host 24141.
    config_with_port(free_closed_port().await)
        .save_secure(&cfg_path)
        .expect("save cfg");
    let rc_path = tmp.path().join(".zlogin");
    let data_dir = tmp.path().join("data");
    let opts = IntegrationOptions {
        shell: Some(companion_core::ShellKind::Zsh),
        rc_file: Some(rc_path.clone()),
        binary_path: Some(PathBuf::from("/usr/local/bin/agy-companion")),
        data_dir: Some(data_dir.clone()),
        follow_rc_symlink: false,
    };

    // 1. Default auto_enable_launcher == false leaves config & RC untouched on open_settings
    let default_server = isolated_ui_server_with(&tmp, cfg_path.clone(), |o| {
        o.integration_options_override = Some(opts.clone());
        o.auto_enable_launcher = false;
    });
    let open_default = default_server.open_settings().await.expect("open default");
    assert_eq!(open_default.is_error, Some(false));
    assert!(!rc_path.exists());
    assert_eq!(
        open_default.structured_content.expect("sc")["config"]["launcher_auto_start"],
        json!(null)
    );

    // 2. auto_enable_launcher == true enables hook on first open_settings and persists Some(true)
    let auto_server = isolated_ui_server_with(&tmp, cfg_path.clone(), |o| {
        o.integration_options_override = Some(opts.clone());
        o.auto_enable_launcher = true;
    });
    let open_auto = auto_server.open_settings().await.expect("open auto");
    assert_eq!(open_auto.is_error, Some(false));
    let open_sc = open_auto.structured_content.expect("open sc");
    assert_eq!(open_sc["config"]["launcher_auto_start"], json!(true));
    assert!(rc_path.exists(), "first-run auto-enable should write RC");
    assert_eq!(open_sc["launcher_integration"]["enabled"], json!(true));
    assert_eq!(
        open_sc["launcher_integration"]["hybrid_active"],
        json!(false)
    );

    // 3. set_launcher_integration(false) disables hook and persists Some(false) immediately in snapshot
    let disabled = auto_server
        .set_launcher_integration(Parameters(SetLauncherIntegrationArgs { enabled: false }))
        .await
        .expect("disable");
    assert_eq!(disabled.is_error, Some(false));
    let dis_sc = disabled.structured_content.expect("dis sc");
    assert_eq!(dis_sc["config"]["launcher_auto_start"], json!(false));
    assert_eq!(dis_sc["launcher_integration"]["enabled"], json!(false));

    // 4. Subsequent open_settings with auto_enable_launcher == true respects explicit opt-out Some(false)
    let reopen = auto_server.open_settings().await.expect("reopen");
    assert_eq!(reopen.is_error, Some(false));
    let reopen_sc = reopen.structured_content.expect("reopen sc");
    assert_eq!(reopen_sc["config"]["launcher_auto_start"], json!(false));
    assert_eq!(reopen_sc["launcher_integration"]["enabled"], json!(false));

    // 5. Concurrent save_settings + set_launcher_integration(true) loses neither update
    let (save_res, enable_res) = tokio::join!(
        auto_server.save_settings(Parameters(SaveSettingsArgs {
            account_plan: Some(AccountPlan::Organization),
            ..Default::default()
        })),
        auto_server
            .set_launcher_integration(Parameters(SetLauncherIntegrationArgs { enabled: true })),
    );
    assert_eq!(save_res.expect("save").is_error, Some(false));
    assert_eq!(enable_res.expect("enable").is_error, Some(false));
    let final_cfg = Config::load_or_default(&cfg_path).expect("reload final cfg");
    assert_eq!(final_cfg.launcher_auto_start, Some(true));
    assert_eq!(
        final_cfg.cloudcode.upstream.as_str(),
        companion_core::CLOUDCODE_PROD_UPSTREAM_URL
    );

    // 6. Injected hybrid_active_override == Some(true) persists Some(true) without touching RC
    let cfg_path2 = tmp.path().join("config_hybrid.json");
    config_with_port(free_closed_port().await)
        .save_secure(&cfg_path2)
        .expect("save hybrid cfg");
    let rc_path2 = tmp.path().join(".zlogin_hybrid");
    let data_dir2 = tmp.path().join("data_hybrid");
    let hybrid_server = isolated_ui_server_with(&tmp, cfg_path2.clone(), |o| {
        o.integration_options_override = Some(IntegrationOptions {
            shell: Some(companion_core::ShellKind::Zsh),
            rc_file: Some(rc_path2.clone()),
            binary_path: Some(PathBuf::from("/usr/local/bin/agy-companion")),
            data_dir: Some(data_dir2),
            follow_rc_symlink: false,
        });
        o.hybrid_active_override = Some(true);
        o.auto_enable_launcher = true;
    });
    let open_hybrid = hybrid_server.open_settings().await.expect("open hybrid");
    assert_eq!(open_hybrid.is_error, Some(false));
    let hybrid_sc = open_hybrid.structured_content.expect("hybrid sc");
    assert_eq!(hybrid_sc["config"]["launcher_auto_start"], json!(true));
    assert_eq!(
        hybrid_sc["launcher_integration"]["hybrid_active"],
        json!(true)
    );
    assert!(
        !rc_path2.exists(),
        "already-active hybrid mode must not rewrite RC file on first run"
    );
}

#[test]
fn golden_wire_fixtures_match_rust_serialization_verbatim() {
    use companion_core::{
        CloudCodeConfig, CloudCodeStatsSnapshot, CloudCodeUpstream, Config,
        DEFAULT_LISTEN_PORT_NONZERO, IntegrationStatus, ModelSessionStats, ProxyConfig,
        RecentCallSnapshot, RedactedConfig, RedactedGeminiConfig, RedactedProvidersConfig,
        ShellKind, UsageBreakdownRow, UsageBucket, UsageRange, UsageStatsSnapshot, UsageSummary,
        default_enabled_gemini_models,
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
            },
            launcher_auto_start: Some(true),
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
                    requests: 1,
                    input_tokens: 3000,
                    cached_tokens: 2500,
                    output_tokens: 600,
                    reasoning_tokens: 120,
                    total_latency_ms: 480,
                },
                ModelSessionStats {
                    provider: "antigravity".to_string(),
                    source: "builtin".to_string(),
                    model: "gemini-3.8-pro".to_string(),
                    requests: 1,
                    input_tokens: 1000,
                    cached_tokens: 0,
                    output_tokens: 200,
                    reasoning_tokens: 40,
                    total_latency_ms: 320,
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
        platform: HostPlatform::Macos,
        account_plan: AccountPlanStatus {
            mode: AccountPlanMode::Auto,
            resolved: Some(ResolvedAccountPlan::Organization),
        },
        proxy_rss_bytes: Some(25_165_824),
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
        config: Config::default().redacted(),
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
        platform: HostPlatform::Macos,
        account_plan: AccountPlanStatus::from_config_and_stats(&CloudCodeUpstream::Auto, None),
        proxy_rss_bytes: None,
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
        message:
            "Connected to Gemini upstream (https://generativelanguage.googleapis.com) successfully."
                .to_string(),
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
}
