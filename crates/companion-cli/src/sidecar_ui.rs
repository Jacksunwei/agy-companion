use std::future::Future;
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::path::PathBuf;
use std::sync::Arc;

use axum::Router;
use axum::body::Body;
use axum::extract::{DefaultBodyLimit, State};
use axum::response::Response;
use axum::routing::{get, post};
use companion_core::UsageRange;
use companion_ui::{
    CompanionUiServer, GetUsageArgs, SaveSettingsArgs, SettingsSnapshot, UiServerOptions,
};
use http::{HeaderMap, HeaderValue, StatusCode, header};
use serde::Deserialize;
use tokio::net::TcpListener;

use crate::CliError;
use crate::ls_collector::LsCollectorHandle;

pub const SIDECAR_WEB_PORT_ENV: &str = "ANTIGRAVITY_SIDECAR_WEB_PORT";
pub const SIDECAR_UI_TOKEN_ENV: &str = "ANTIGRAVITY_SIDECAR_UI_TOKEN";
const MAX_SIDECAR_API_BODY_BYTES: usize = 64 * 1024;
const SIDECAR_TOKEN_HEADER: &str = "x-sidecar-token";
const SIDECAR_UI_CSP: &str = "default-src 'none'; script-src 'unsafe-inline'; style-src 'unsafe-inline'; img-src data:; connect-src 'self'; font-src 'none'; frame-src 'none'; object-src 'none'; base-uri 'none'; form-action 'none'";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SidecarUiConfig {
    pub port: u16,
    pub token: Option<String>,
}

impl SidecarUiConfig {
    pub fn from_env() -> Option<Self> {
        let raw_port = std::env::var(SIDECAR_WEB_PORT_ENV).ok()?;
        let port: u16 = raw_port.trim().parse().ok().filter(|&p| p > 0)?;
        let token = std::env::var(SIDECAR_UI_TOKEN_ENV)
            .ok()
            .map(|t| t.trim().to_string())
            .filter(|t| !t.is_empty());
        Some(Self { port, token })
    }
}

#[derive(Clone)]
struct SidecarUiState {
    ui: Arc<CompanionUiServer>,
    token: Option<Arc<String>>,
    ls_collector: Option<LsCollectorHandle>,
}

async fn apply_ls_telemetry_overlay(
    state: &SidecarUiState,
    mut snapshot: SettingsSnapshot,
    conversation_id: Option<&str>,
) -> SettingsSnapshot {
    let Some(ref collector) = state.ls_collector else {
        snapshot.ls_telemetry_active = false;
        return snapshot;
    };
    if let Some(mut ls_stats) = collector.session_stats(conversation_id).await {
        if let Some(proxy_stats) = snapshot.stats.as_ref() {
            ls_stats.cloudcode = proxy_stats.cloudcode.clone();
            ls_stats.proxy_rss_bytes = proxy_stats.proxy_rss_bytes;
        }
        snapshot.stats = Some(ls_stats);
        snapshot.ls_telemetry_active = true;
    } else {
        snapshot.ls_telemetry_active = false;
    }
    snapshot
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct GetSettingsHttpArgs {
    #[serde(default)]
    conversation_id: Option<String>,
}

fn parse_save_settings_http_args(
    body: &[u8],
) -> Result<(Option<String>, SaveSettingsArgs), String> {
    let mut map: serde_json::Map<String, serde_json::Value> =
        serde_json::from_slice(body).map_err(|err| format!("invalid request: {err}"))?;
    let conversation_id = match map.remove("conversation_id") {
        None | Some(serde_json::Value::Null) => None,
        Some(serde_json::Value::String(s)) => Some(s),
        Some(_) => return Err("invalid request: conversation_id must be a string".to_string()),
    };
    let settings: SaveSettingsArgs = serde_json::from_value(serde_json::Value::Object(map))
        .map_err(|err| format!("invalid request: {err}"))?;
    Ok((conversation_id, settings))
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct SetLauncherHttpArgs {
    enabled: bool,
    #[serde(default)]
    conversation_id: Option<String>,
}

fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    let mut diff = 0u8;
    for (&x, &y) in a.iter().zip(b.iter()) {
        diff |= x ^ y;
    }
    diff == 0
}

fn verify_token(state: &SidecarUiState, headers: &HeaderMap) -> bool {
    let Some(expected) = state.token.as_deref() else {
        return true;
    };
    let Some(provided) = headers
        .get(SIDECAR_TOKEN_HEADER)
        .and_then(|v| v.to_str().ok())
    else {
        return false;
    };
    constant_time_eq(provided.as_bytes(), expected.as_bytes())
}

fn json_response(status: StatusCode, body: serde_json::Value) -> Response {
    let mut resp = Response::new(Body::from(body.to_string()));
    *resp.status_mut() = status;
    let headers = resp.headers_mut();
    headers.insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("application/json"),
    );
    headers.insert(
        header::X_CONTENT_TYPE_OPTIONS,
        HeaderValue::from_static("nosniff"),
    );
    headers.insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    resp
}

fn api_error(status: StatusCode, message: impl Into<String>) -> Response {
    json_response(
        status,
        serde_json::json!({
            "ok": false,
            "error": message.into(),
        }),
    )
}

fn api_ok<T: serde::Serialize>(payload: &T) -> Response {
    match serde_json::to_value(payload) {
        Ok(value) => json_response(StatusCode::OK, value),
        Err(err) => api_error(
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("failed to serialize response: {err}"),
        ),
    }
}

async fn index_handler(State(state): State<SidecarUiState>) -> Response {
    match state.ui.load_ui_html().await {
        Ok(html) => {
            let mut resp = Response::new(Body::from(html));
            *resp.status_mut() = StatusCode::OK;
            let headers = resp.headers_mut();
            headers.insert(
                header::CONTENT_TYPE,
                HeaderValue::from_static("text/html; charset=utf-8"),
            );
            headers.insert(
                header::X_CONTENT_TYPE_OPTIONS,
                HeaderValue::from_static("nosniff"),
            );
            headers.insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
            headers.insert(
                header::CONTENT_SECURITY_POLICY,
                HeaderValue::from_static(SIDECAR_UI_CSP),
            );
            resp
        }
        Err(err) => api_error(StatusCode::INTERNAL_SERVER_ERROR, err),
    }
}

fn reject_invalid_api_request(state: &SidecarUiState, headers: &HeaderMap) -> Option<Response> {
    if !verify_token(state, headers) {
        return Some(api_error(StatusCode::UNAUTHORIZED, "unauthorized"));
    }
    let is_json = headers
        .get(header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .and_then(|ct| ct.split(';').next())
        .is_some_and(|mime| mime.trim().eq_ignore_ascii_case("application/json"));
    if !is_json {
        return Some(api_error(
            StatusCode::UNSUPPORTED_MEDIA_TYPE,
            "content-type must be application/json",
        ));
    }
    None
}

async fn get_settings_handler(
    State(state): State<SidecarUiState>,
    headers: HeaderMap,
    body: axum::body::Bytes,
) -> Response {
    if let Some(resp) = reject_invalid_api_request(&state, &headers) {
        return resp;
    }
    let args: GetSettingsHttpArgs = if body.is_empty() {
        GetSettingsHttpArgs::default()
    } else {
        match serde_json::from_slice(&body) {
            Ok(parsed) => parsed,
            Err(err) => {
                return api_error(StatusCode::BAD_REQUEST, format!("invalid request: {err}"));
            }
        }
    };
    match state
        .ui
        .get_settings_snapshot(args.conversation_id.as_deref())
        .await
    {
        Ok(snapshot) => {
            let overlaid =
                apply_ls_telemetry_overlay(&state, snapshot, args.conversation_id.as_deref()).await;
            api_ok(&overlaid)
        }
        Err(msg) => api_error(StatusCode::BAD_REQUEST, msg),
    }
}

async fn get_usage_handler(
    State(state): State<SidecarUiState>,
    headers: HeaderMap,
    body: axum::body::Bytes,
) -> Response {
    if let Some(resp) = reject_invalid_api_request(&state, &headers) {
        return resp;
    }
    let args: GetUsageArgs = match serde_json::from_slice(&body) {
        Ok(parsed) => parsed,
        Err(err) => return api_error(StatusCode::BAD_REQUEST, format!("invalid request: {err}")),
    };
    let range: UsageRange = args.range.into();
    match state.ui.get_usage_summary(range).await {
        Ok(summary) => api_ok(&summary),
        Err(msg) => api_error(StatusCode::BAD_REQUEST, msg),
    }
}

async fn save_settings_handler(
    State(state): State<SidecarUiState>,
    headers: HeaderMap,
    body: axum::body::Bytes,
) -> Response {
    if let Some(resp) = reject_invalid_api_request(&state, &headers) {
        return resp;
    }
    let (conversation_id, settings) = match parse_save_settings_http_args(&body) {
        Ok(parsed) => parsed,
        Err(msg) => return api_error(StatusCode::BAD_REQUEST, msg),
    };
    match state
        .ui
        .save_settings_snapshot(settings, conversation_id.as_deref())
        .await
    {
        Ok(snapshot) => {
            let overlaid =
                apply_ls_telemetry_overlay(&state, snapshot, conversation_id.as_deref()).await;
            api_ok(&overlaid)
        }
        Err(msg) => api_error(StatusCode::BAD_REQUEST, msg),
    }
}

async fn test_connection_handler(
    State(state): State<SidecarUiState>,
    headers: HeaderMap,
) -> Response {
    if let Some(resp) = reject_invalid_api_request(&state, &headers) {
        return resp;
    }
    match state.ui.test_connection_payload().await {
        Ok(payload) => api_ok(&payload),
        Err(msg) => api_error(StatusCode::BAD_REQUEST, msg),
    }
}

async fn set_launcher_integration_handler(
    State(state): State<SidecarUiState>,
    headers: HeaderMap,
    body: axum::body::Bytes,
) -> Response {
    if let Some(resp) = reject_invalid_api_request(&state, &headers) {
        return resp;
    }
    let args: SetLauncherHttpArgs = match serde_json::from_slice(&body) {
        Ok(parsed) => parsed,
        Err(err) => return api_error(StatusCode::BAD_REQUEST, format!("invalid request: {err}")),
    };
    match state
        .ui
        .set_launcher_integration_snapshot(args.enabled, args.conversation_id.as_deref())
        .await
    {
        Ok(snapshot) => {
            let overlaid =
                apply_ls_telemetry_overlay(&state, snapshot, args.conversation_id.as_deref()).await;
            api_ok(&overlaid)
        }
        Err(msg) => api_error(StatusCode::BAD_REQUEST, msg),
    }
}

pub fn build_sidecar_ui_router(ui: Arc<CompanionUiServer>, token: Option<String>) -> Router {
    build_sidecar_ui_router_with_ls(ui, token, None)
}

pub fn build_sidecar_ui_router_with_ls(
    ui: Arc<CompanionUiServer>,
    token: Option<String>,
    ls_collector: Option<LsCollectorHandle>,
) -> Router {
    let state = SidecarUiState {
        ui,
        token: token.map(Arc::new),
        ls_collector,
    };
    Router::new()
        .route("/", get(index_handler))
        .route("/_api/get_settings", post(get_settings_handler))
        .route("/_api/get_usage", post(get_usage_handler))
        .route("/_api/save_settings", post(save_settings_handler))
        .route("/_api/test_connection", post(test_connection_handler))
        .route(
            "/_api/set_launcher_integration",
            post(set_launcher_integration_handler),
        )
        .layer(DefaultBodyLimit::max(MAX_SIDECAR_API_BODY_BYTES))
        .with_state(state)
}

pub async fn bind_and_spawn_sidecar_ui<F>(
    config_path: PathBuf,
    sidecar_cfg: SidecarUiConfig,
    shutdown: F,
) -> Result<(SocketAddr, tokio::task::JoinHandle<()>), CliError>
where
    F: Future<Output = ()> + Send + 'static,
{
    bind_and_spawn_sidecar_ui_with_ls(config_path, sidecar_cfg, None, shutdown).await
}

pub async fn bind_and_spawn_sidecar_ui_with_ls<F>(
    config_path: PathBuf,
    sidecar_cfg: SidecarUiConfig,
    ls_collector: Option<LsCollectorHandle>,
    shutdown: F,
) -> Result<(SocketAddr, tokio::task::JoinHandle<()>), CliError>
where
    F: Future<Output = ()> + Send + 'static,
{
    let ui = Arc::new(CompanionUiServer::with_options(
        config_path,
        UiServerOptions::default(),
    )?);
    let addr = SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), sidecar_cfg.port);
    let listener = TcpListener::bind(addr).await?;
    let bound_addr = listener.local_addr()?;
    if bound_addr.ip() != IpAddr::V4(Ipv4Addr::LOCALHOST) {
        return Err(CliError::Io(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "sidecar UI listener must bind to 127.0.0.1",
        )));
    }
    let router = build_sidecar_ui_router_with_ls(ui, sidecar_cfg.token, ls_collector);
    tracing::info!(
        addr = %bound_addr,
        "agy-companion sidecar Web UI listening"
    );
    let handle = tokio::spawn(async move {
        if let Err(err) = axum::serve(listener, router)
            .with_graceful_shutdown(shutdown)
            .await
        {
            tracing::warn!(error = %err, "sidecar Web UI server exited with error");
        }
    });
    Ok((bound_addr, handle))
}

#[cfg(test)]
mod tests {
    use super::*;
    use companion_core::integration::IntegrationOptions;
    use companion_ui::UiHtmlSource;
    use tempfile::TempDir;

    fn isolated_ui_server(tmp: &TempDir) -> Arc<CompanionUiServer> {
        let cfg_path = tmp.path().join("config.json");
        let opts = UiServerOptions {
            ui_html_source: UiHtmlSource::Fixed(None),
            env_api_key_fallback: false,
            integration_options_override: Some(IntegrationOptions {
                shell: None,
                rc_file: Some(tmp.path().join(".zlogin")),
                binary_path: Some(tmp.path().join("agy-companion")),
                data_dir: Some(tmp.path().join("data")),
                follow_rc_symlink: false,
            }),
            hybrid_active_override: Some(true),
            ..UiServerOptions::default()
        };
        Arc::new(CompanionUiServer::with_options(cfg_path, opts).expect("ui server"))
    }

    #[tokio::test]
    async fn sidecar_ui_serves_html_and_enforces_token_on_api() {
        let tmp = TempDir::new().expect("tmp");
        let ui = isolated_ui_server(&tmp);
        let router = build_sidecar_ui_router(ui, Some("secret-token-123".to_string()));
        let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
        let addr = listener.local_addr().expect("addr");
        let server = tokio::spawn(async move {
            let _ = axum::serve(listener, router).await;
        });

        let client = reqwest::Client::builder()
            .no_proxy()
            .build()
            .expect("client");

        // GET / is readable without token (Antigravity readiness probe) and sets strict CSP.
        let root_resp = client
            .get(format!("http://{addr}/"))
            .send()
            .await
            .expect("get /");
        assert_eq!(root_resp.status(), StatusCode::OK);
        assert!(
            root_resp
                .headers()
                .get(header::CONTENT_SECURITY_POLICY)
                .is_some()
        );
        let html = root_resp.text().await.expect("html");
        assert!(html.contains("Antigravity Companion"));

        // POST /_api/get_settings without token -> 401.
        let unauth = client
            .post(format!("http://{addr}/_api/get_settings"))
            .header(header::CONTENT_TYPE, "application/json")
            .body("{}")
            .send()
            .await
            .expect("unauth");
        assert_eq!(unauth.status(), StatusCode::UNAUTHORIZED);

        // POST /_api/get_settings with valid token but non-JSON Content-Type -> 415.
        let wrong_ct = client
            .post(format!("http://{addr}/_api/get_settings"))
            .header(SIDECAR_TOKEN_HEADER, "secret-token-123")
            .header(header::CONTENT_TYPE, "text/plain")
            .body("{}")
            .send()
            .await
            .expect("wrong_ct");
        assert_eq!(wrong_ct.status(), StatusCode::UNSUPPORTED_MEDIA_TYPE);

        // POST /_api/get_settings with valid token + JSON Content-Type -> 200 redacted SettingsSnapshot.
        let authed = client
            .post(format!("http://{addr}/_api/get_settings"))
            .header(SIDECAR_TOKEN_HEADER, "secret-token-123")
            .header(header::CONTENT_TYPE, "application/json")
            .body(r#"{"conversation_id":"conv-test-123456"}"#)
            .send()
            .await
            .expect("authed");
        assert_eq!(authed.status(), StatusCode::OK);
        let json: serde_json::Value = authed.json().await.expect("json");
        assert!(json.get("config").is_some());
        assert!(json.get("proxy").is_some());
        assert!(json.get("account_plan").is_some());

        // POST /_api/save_settings persists nested config and API key (in AuthStore) without leaking plaintext.
        let save_ok = client
            .post(format!("http://{addr}/_api/save_settings"))
            .header(SIDECAR_TOKEN_HEADER, "secret-token-123")
            .header(header::CONTENT_TYPE, "application/json")
            .body(r#"{"conversation_id":"conv-test-123456","providers":{"gemini":{"api_key":{"set":"AIzaSySidecarSaveTest1234"},"default_model":"gemini-4-neon"}}}"#)
            .send()
            .await
            .expect("save_ok");
        assert_eq!(save_ok.status(), StatusCode::OK);
        let saved_raw = save_ok.text().await.expect("saved text");
        assert!(
            !saved_raw.contains("AIzaSySidecarSaveTest1234"),
            "save_settings response must never leak plaintext API key"
        );
        let saved: serde_json::Value = serde_json::from_str(&saved_raw).expect("saved json");
        assert_eq!(
            saved["config"]["providers"]["gemini"]["default_model"].as_str(),
            Some("gemini-4-neon")
        );
        assert_eq!(
            saved["config"]["providers"]["gemini"]["api_key_configured"].as_bool(),
            Some(true)
        );
        assert_eq!(
            saved["config"]["providers"]["gemini"]["api_key_hint"].as_str(),
            Some("AIza...1234")
        );
        let cfg_path = tmp.path().join("config.json");
        let cfg_disk = std::fs::read_to_string(&cfg_path).expect("read config.json");
        assert!(
            !cfg_disk.contains("AIzaSySidecarSaveTest1234") && !cfg_disk.contains("api_key"),
            "config.json must not store plaintext API key"
        );
        let auth_store =
            companion_core::FileAuthStore::for_config_path(&cfg_path).expect("auth store");
        let stored_key = companion_core::AuthStore::load_gemini(&auth_store)
            .expect("load gemini auth")
            .expect("persisted creds");
        assert_eq!(stored_key.api_key.expose(), "AIzaSySidecarSaveTest1234");

        let save_unknown = client
            .post(format!("http://{addr}/_api/save_settings"))
            .header(SIDECAR_TOKEN_HEADER, "secret-token-123")
            .header(header::CONTENT_TYPE, "application/json")
            .body(r#"{"unexpected_field":true}"#)
            .send()
            .await
            .expect("save_unknown");
        assert_eq!(save_unknown.status(), StatusCode::BAD_REQUEST);

        server.abort();
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn sidecar_ui_overlays_ls_stats_preserving_proxy_cloudcode_and_rss_and_fails_open() {
        use companion_core::{
            CloudCodeStatsSnapshot, Config, CsrfToken, HealthResponse, LsEndpoint, SqliteUsageSink,
            StatsSnapshot, UsageStatsSnapshot,
        };
        use companion_providers::LsClient;
        use std::num::NonZeroU16;

        // 1. Mock proxy serving /health and /api/stats with cloudcode + proxy_rss_bytes.
        let mock_proxy_router = Router::new()
            .route(
                "/health",
                get(|| async { axum::Json(HealthResponse::ok(env!("CARGO_PKG_VERSION"))) }),
            )
            .route(
                "/api/stats",
                get(|| async {
                    axum::Json(StatsSnapshot {
                        usage: UsageStatsSnapshot {
                            requests: 99,
                            prompt_tokens: 999,
                            ..Default::default()
                        },
                        cloudcode: CloudCodeStatsSnapshot {
                            mode: "auto".to_string(),
                            resolved_upstream: Some(
                                "https://cloudcode-pa.googleapis.com".to_string(),
                            ),
                            last_catalog_refresh_at_ms: Some(1_700_000_000_000),
                            last_catalog_included_proxy_models: Some(true),
                        },
                        proxy_rss_bytes: Some(16_777_216),
                        recent_calls: Vec::new(),
                        by_model: Vec::new(),
                    })
                }),
            );
        let proxy_listener = TcpListener::bind("127.0.0.1:0").await.expect("bind proxy");
        let proxy_port = proxy_listener.local_addr().expect("proxy addr").port();
        let proxy_srv = tokio::spawn(async move {
            let _ = axum::serve(proxy_listener, mock_proxy_router).await;
        });

        // 2. Mock LS serving model catalog + 1 Vertex Claude step for conv-overlay-01.
        let mock_ls_router = Router::new()
            .route(
                "/exa.language_server_pb.LanguageServerService/GetCascadeModelConfigData",
                post(|| async {
                    axum::Json(serde_json::json!({
                        "clientModelConfigs": [{
                            "label": "Claude Opus 5.5 (High)",
                            "modelOrAlias": { "model": "MODEL_PLACEHOLDER_M501" },
                            "provider": "MODEL_PROVIDER_ANTHROPIC",
                            "modelId": "claude-opus-5-5-high"
                        }]
                    }))
                }),
            )
            .route(
                "/exa.language_server_pb.LanguageServerService/GetCascadeTrajectorySteps",
                post(|| async {
                    axum::Json(serde_json::json!({
                        "steps": [{
                            "type": "CORTEX_STEP_TYPE_PLANNER_RESPONSE",
                            "status": "CORTEX_STEP_STATUS_DONE",
                            "metadata": {
                                "createdAt": "2026-11-26T10:00:00Z",
                                "finishedGeneratingAt": "2026-11-26T10:00:01Z",
                                "generatorModel": "MODEL_PLACEHOLDER_M501",
                                "modelUsage": {
                                    "inputTokens": "500",
                                    "cacheReadTokens": "1500",
                                    "outputTokens": "120",
                                    "responseOutputTokens": "100",
                                    "thinkingOutputTokens": "20",
                                    "apiProvider": "API_PROVIDER_ANTHROPIC_VERTEX",
                                    "responseHeader": { "sessionID": "sess-1" }
                                }
                            }
                        }]
                    }))
                }),
            )
            .route(
                "/exa.language_server_pb.LanguageServerService/JetboxSubscribeToSummaries",
                post(|| async {
                    let mut resp = Response::new(Body::from(vec![0u8, 0, 0, 0, 2, b'{', b'}']));
                    *resp.status_mut() = StatusCode::OK;
                    resp.headers_mut().insert(
                        header::CONTENT_TYPE,
                        HeaderValue::from_static("application/connect+json"),
                    );
                    resp
                }),
            );
        let ls_listener = TcpListener::bind("127.0.0.1:0").await.expect("bind ls");
        let ls_port = ls_listener.local_addr().expect("ls addr").port();
        let ls_srv = tokio::spawn(async move {
            let _ = axum::serve(ls_listener, mock_ls_router).await;
        });
        tokio::task::yield_now().await;

        let tmp = TempDir::new().expect("tmp");
        let cfg_path = tmp.path().join("config.json");
        let mut cfg = Config::default();
        cfg.proxy.port = NonZeroU16::new(proxy_port).expect("non-zero");
        cfg.save_secure(&cfg_path).expect("save cfg");

        let ui = isolated_ui_server(&tmp);
        let sink = Arc::new(SqliteUsageSink::new(tmp.path().join("usage")).expect("sink"));
        let ls_client = LsClient::new(
            LsEndpoint::parse(&format!("127.0.0.1:{ls_port}")).expect("ep"),
            CsrfToken::parse("csrf-sidecar-overlay-test-1234").expect("csrf"),
        )
        .expect("ls client");
        let (shutdown_tx, shutdown_rx) = tokio::sync::watch::channel(false);
        let handle = crate::ls_collector::spawn_ls_collector(
            ls_client,
            sink,
            crate::ls_collector::LsCollectorConfig::default(),
            shutdown_rx,
        );

        let router =
            build_sidecar_ui_router_with_ls(ui, Some("tok-overlay".to_string()), Some(handle));
        let ui_listener = TcpListener::bind("127.0.0.1:0").await.expect("bind ui");
        let ui_addr = ui_listener.local_addr().expect("ui addr");
        let ui_srv = tokio::spawn(async move {
            let _ = axum::serve(ui_listener, router).await;
        });

        let client = reqwest::Client::builder().no_proxy().build().expect("http");
        let resp = client
            .post(format!("http://{ui_addr}/_api/get_settings"))
            .header(SIDECAR_TOKEN_HEADER, "tok-overlay")
            .header(header::CONTENT_TYPE, "application/json")
            .body(r#"{"conversation_id":"conv-overlay-01"}"#)
            .send()
            .await
            .expect("get_settings");
        assert_eq!(resp.status(), StatusCode::OK);
        let snap: SettingsSnapshot = resp.json().await.expect("snapshot");
        assert!(snap.ls_telemetry_active);
        let stats = snap.stats.expect("overlaid stats");
        assert_eq!(stats.usage.requests, 1);
        assert_eq!(stats.usage.prompt_tokens, 2000);
        assert_eq!(stats.usage.cached_tokens, 1500);
        // Preserved :24141 proxy cloudcode & proxy_rss_bytes!
        assert_eq!(stats.proxy_rss_bytes, Some(16_777_216));
        assert_eq!(
            stats.cloudcode.resolved_upstream.as_deref(),
            Some("https://cloudcode-pa.googleapis.com")
        );

        let _ = shutdown_tx.send(true);
        ui_srv.abort();
        ls_srv.abort();
        proxy_srv.abort();
    }
}
