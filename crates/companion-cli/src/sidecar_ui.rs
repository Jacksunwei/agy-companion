use std::future::Future;
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::path::PathBuf;
use std::sync::Arc;

use axum::Router;
use axum::body::Body;
use axum::extract::{DefaultBodyLimit, State};
use axum::response::Response;
use axum::routing::{get, post};
use companion_core::{AuthStore, UsageRange};
use companion_providers::CodexAuthManager;
use companion_ui::{
    CompanionUiServer, GetUsageArgs, SaveSettingsArgs, SettingsSnapshot, TestConnectionArgs,
    UiServerOptions,
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
struct EmptyHttpArgs {}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct GetSettingsHttpArgs {
    #[serde(default)]
    conversation_id: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct CompleteCodexLoginHttpArgs {
    callback_input: String,
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
    body: axum::body::Bytes,
) -> Response {
    if let Some(resp) = reject_invalid_api_request(&state, &headers) {
        return resp;
    }
    let args: TestConnectionArgs = if body.is_empty() {
        TestConnectionArgs::default()
    } else {
        match serde_json::from_slice(&body) {
            Ok(parsed) => parsed,
            Err(err) => {
                return api_error(StatusCode::BAD_REQUEST, format!("invalid request: {err}"));
            }
        }
    };
    match state.ui.test_connection_payload(args.provider).await {
        Ok(payload) => api_ok(&payload),
        Err(msg) => api_error(StatusCode::BAD_REQUEST, msg),
    }
}

async fn start_codex_login_handler(
    State(state): State<SidecarUiState>,
    headers: HeaderMap,
    body: axum::body::Bytes,
) -> Response {
    if let Some(resp) = reject_invalid_api_request(&state, &headers) {
        return resp;
    }
    if !body.is_empty()
        && let Err(err) = serde_json::from_slice::<EmptyHttpArgs>(&body)
    {
        return api_error(StatusCode::BAD_REQUEST, format!("invalid request: {err}"));
    }
    match state.ui.start_codex_login().await {
        Ok(payload) => api_ok(&payload),
        Err(msg) => api_error(StatusCode::BAD_REQUEST, msg),
    }
}

async fn cancel_codex_login_handler(
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
        .cancel_codex_login_snapshot(args.conversation_id.as_deref())
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

async fn complete_codex_login_handler(
    State(state): State<SidecarUiState>,
    headers: HeaderMap,
    body: axum::body::Bytes,
) -> Response {
    if let Some(resp) = reject_invalid_api_request(&state, &headers) {
        return resp;
    }
    let args: CompleteCodexLoginHttpArgs = match serde_json::from_slice(&body) {
        Ok(parsed) => parsed,
        Err(err) => return api_error(StatusCode::BAD_REQUEST, format!("invalid request: {err}")),
    };
    match state
        .ui
        .complete_codex_login_snapshot(&args.callback_input, args.conversation_id.as_deref())
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

async fn disconnect_codex_handler(
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
        .disconnect_codex_snapshot(args.conversation_id.as_deref())
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
        .route("/_api/start_codex_login", post(start_codex_login_handler))
        .route("/_api/cancel_codex_login", post(cancel_codex_login_handler))
        .route(
            "/_api/complete_codex_login",
            post(complete_codex_login_handler),
        )
        .route("/_api/disconnect_codex", post(disconnect_codex_handler))
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
    bind_and_spawn_sidecar_ui_with_auth_and_ls(config_path, sidecar_cfg, None, None, None, shutdown)
        .await
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
    bind_and_spawn_sidecar_ui_with_auth_and_ls(
        config_path,
        sidecar_cfg,
        None,
        None,
        ls_collector,
        shutdown,
    )
    .await
}

pub async fn bind_and_spawn_sidecar_ui_with_auth_and_ls<F>(
    config_path: PathBuf,
    sidecar_cfg: SidecarUiConfig,
    auth_store: Option<Arc<dyn AuthStore>>,
    codex_auth: Option<Arc<CodexAuthManager>>,
    ls_collector: Option<LsCollectorHandle>,
    shutdown: F,
) -> Result<(SocketAddr, tokio::task::JoinHandle<()>), CliError>
where
    F: Future<Output = ()> + Send + 'static,
{
    let mut ui_options = UiServerOptions::default();
    if let Some(store) = auth_store {
        ui_options = ui_options.with_auth_store_override(store);
    }
    if let Some(manager) = codex_auth {
        ui_options = ui_options.with_codex_auth_override(manager);
    }
    let ui = Arc::new(CompanionUiServer::with_options(config_path, ui_options)?);
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

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn sidecar_ui_codex_oauth_endpoints_and_provider_test_connection() {
        use std::num::NonZeroU16;

        use companion_core::{Config, FileAuthStore};
        use companion_providers::CodexAuthOptions;

        let tmp = TempDir::new().expect("tmp");
        let cfg_path = tmp.path().join("config.json");

        fn make_jwt(payload_json: &str) -> String {
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

        let access_jwt = make_jwt(
            r#"{"exp":4102444800,"https://api.openai.com/auth":{"chatgpt_account_id":"acct_sidecar_999","chatgpt_plan_type":"plus"},"https://api.openai.com/profile":{"email":"dev@example.com"}}"#,
        );
        let refresh_secret = "rt_sidecar_secret_token";
        let captured_conv_ids: Arc<tokio::sync::Mutex<Vec<String>>> =
            Arc::new(tokio::sync::Mutex::new(Vec::new()));
        let captured_conv_ids_clone = Arc::clone(&captured_conv_ids);
        let access_for_oauth = access_jwt.clone();
        let access_for_responses = access_jwt.clone();

        let mock_router = Router::new()
            .route(
                "/health",
                get(|| async {
                    axum::Json(serde_json::json!({
                        "status": "ok",
                        "version": env!("CARGO_PKG_VERSION"),
                        "protocol_version": 1
                    }))
                }),
            )
            .route(
                "/api/stats",
                get(move |query: axum::extract::RawQuery| {
                    let convs = Arc::clone(&captured_conv_ids_clone);
                    async move {
                        if let Some(q) = query.0 {
                            convs.lock().await.push(q);
                        }
                        axum::Json(serde_json::json!({
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
                                "resolved_upstream": null,
                                "last_catalog_refresh_at_ms": null,
                                "last_catalog_included_proxy_models": null
                            },
                            "proxy_rss_bytes": null,
                            "recent_calls": [],
                            "by_model": []
                        }))
                    }
                }),
            )
            .route(
                "/oauth/token",
                post(move || {
                    let access = access_for_oauth.clone();
                    async move {
                        axum::Json(serde_json::json!({
                            "access_token": access,
                            "refresh_token": refresh_secret,
                            "expires_in": 3600
                        }))
                    }
                }),
            )
            .route(
                "/responses",
                post(move |headers: HeaderMap| {
                    let expected = format!("Bearer {access_for_responses}");
                    async move {
                        let ok = headers
                            .get(header::AUTHORIZATION)
                            .and_then(|v| v.to_str().ok())
                            == Some(expected.as_str());
                        if ok {
                            let sse = "data: {\"type\":\"response.output_text.delta\",\"delta\":\"hi\"}\n\ndata: {\"type\":\"response.completed\",\"response\":{\"status\":\"completed\",\"usage\":{\"input_tokens\":4,\"output_tokens\":1}}}\n\n";
                            (
                                StatusCode::OK,
                                [(header::CONTENT_TYPE, "text/event-stream")],
                                sse.to_string(),
                            )
                        } else {
                            (
                                StatusCode::UNAUTHORIZED,
                                [(header::CONTENT_TYPE, "application/json")],
                                r#"{"error":{"message":"unauthorized"}}"#.to_string(),
                            )
                        }
                    }
                }),
            );

        let mock_listener = TcpListener::bind("127.0.0.1:0").await.expect("bind mock");
        let mock_port = mock_listener.local_addr().expect("mock addr").port();
        let mock_server = tokio::spawn(async move {
            let _ = axum::serve(mock_listener, mock_router).await;
        });

        let mut cfg = Config::default();
        cfg.proxy.port = NonZeroU16::new(mock_port).expect("non-zero");
        cfg.providers.codex.base_url = format!("http://127.0.0.1:{mock_port}");
        cfg.save_secure(&cfg_path).expect("save config");

        let auth_store: Arc<dyn AuthStore> =
            Arc::new(FileAuthStore::for_config_path(&cfg_path).expect("auth store"));
        let codex_auth = Arc::new(
            CodexAuthManager::with_options(
                Arc::clone(&auth_store),
                CodexAuthOptions {
                    oauth_issuer: format!("http://127.0.0.1:{mock_port}"),
                    callback_bind_port: 0,
                    ..CodexAuthOptions::default()
                },
            )
            .expect("codex auth"),
        );

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
            auth_store_override: Some(Arc::clone(&auth_store)),
            codex_auth_override: Some(Arc::clone(&codex_auth)),
            ..UiServerOptions::default()
        };
        let ui = Arc::new(CompanionUiServer::with_options(cfg_path, opts).expect("ui server"));
        let token = "codex-sidecar-token-456";
        let router = build_sidecar_ui_router(ui, Some(token.to_string()));
        let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind ui");
        let addr = listener.local_addr().expect("ui addr");
        let server = tokio::spawn(async move {
            let _ = axum::serve(listener, router).await;
        });
        tokio::task::yield_now().await;

        let client = reqwest::Client::builder()
            .no_proxy()
            .build()
            .expect("client");

        // 1. Auth (401), Content-Type (415), and deny_unknown_fields (400) across all 5 endpoints.
        for path in [
            "/_api/start_codex_login",
            "/_api/cancel_codex_login",
            "/_api/complete_codex_login",
            "/_api/disconnect_codex",
            "/_api/test_connection",
        ] {
            let no_tok = client
                .post(format!("http://{addr}{path}"))
                .header(header::CONTENT_TYPE, "application/json")
                .body("{}")
                .send()
                .await
                .expect("no_tok");
            assert_eq!(no_tok.status(), StatusCode::UNAUTHORIZED, "{path} 401");

            let bad_ct = client
                .post(format!("http://{addr}{path}"))
                .header(SIDECAR_TOKEN_HEADER, token)
                .header(header::CONTENT_TYPE, "text/plain")
                .body("{}")
                .send()
                .await
                .expect("bad_ct");
            assert_eq!(
                bad_ct.status(),
                StatusCode::UNSUPPORTED_MEDIA_TYPE,
                "{path} 415"
            );

            let unknown = client
                .post(format!("http://{addr}{path}"))
                .header(SIDECAR_TOKEN_HEADER, token)
                .header(header::CONTENT_TYPE, "application/json")
                .body(r#"{"unknown_field":1}"#)
                .send()
                .await
                .expect("unknown");
            assert_eq!(unknown.status(), StatusCode::BAD_REQUEST, "{path} 400");
        }

        // 2. Empty body and explicit provider on /_api/test_connection work; unauth codex returns ok=false.
        let default_conn = client
            .post(format!("http://{addr}/_api/test_connection"))
            .header(SIDECAR_TOKEN_HEADER, token)
            .header(header::CONTENT_TYPE, "application/json")
            .body("")
            .send()
            .await
            .expect("default test_connection");
        assert_eq!(default_conn.status(), StatusCode::OK);
        let default_json: serde_json::Value = default_conn.json().await.expect("default json");
        assert_eq!(default_json["ok"].as_bool(), Some(false));

        let codex_unauth = client
            .post(format!("http://{addr}/_api/test_connection"))
            .header(SIDECAR_TOKEN_HEADER, token)
            .header(header::CONTENT_TYPE, "application/json")
            .body(r#"{"provider":"codex"}"#)
            .send()
            .await
            .expect("codex unauth");
        assert_eq!(codex_unauth.status(), StatusCode::OK);
        assert_eq!(
            codex_unauth.json::<serde_json::Value>().await.unwrap()["ok"].as_bool(),
            Some(false)
        );

        // 3. start_codex_login -> cancel_codex_login (with conversation_id forwarding).
        let start1 = client
            .post(format!("http://{addr}/_api/start_codex_login"))
            .header(SIDECAR_TOKEN_HEADER, token)
            .header(header::CONTENT_TYPE, "application/json")
            .body("{}")
            .send()
            .await
            .expect("start1");
        assert_eq!(start1.status(), StatusCode::OK);
        let start1_json: serde_json::Value = start1.json().await.expect("start1 json");
        assert_eq!(start1_json["callback_listening"].as_bool(), Some(true));
        assert_eq!(start1_json["manual_fallback"].as_bool(), Some(false));

        let cancel = client
            .post(format!("http://{addr}/_api/cancel_codex_login"))
            .header(SIDECAR_TOKEN_HEADER, token)
            .header(header::CONTENT_TYPE, "application/json")
            .body(r#"{"conversation_id":"conv-cancel-1111"}"#)
            .send()
            .await
            .expect("cancel");
        assert_eq!(cancel.status(), StatusCode::OK);
        let cancel_json: serde_json::Value = cancel.json().await.expect("cancel json");
        assert_eq!(cancel_json["codex_login_pending"].as_bool(), Some(false));

        // 4. start_codex_login -> complete_codex_login (with conversation_id forwarding) -> test_connection(codex) ok.
        let start2 = client
            .post(format!("http://{addr}/_api/start_codex_login"))
            .header(SIDECAR_TOKEN_HEADER, token)
            .header(header::CONTENT_TYPE, "application/json")
            .body("{}")
            .send()
            .await
            .expect("start2");
        let start2_json: serde_json::Value = start2.json().await.expect("start2 json");
        let auth_url = start2_json["authorize_url"]
            .as_str()
            .expect("authorize_url");
        let state_param = auth_url
            .split('&')
            .find_map(|kv| kv.strip_prefix("state="))
            .expect("state");

        let complete = client
            .post(format!("http://{addr}/_api/complete_codex_login"))
            .header(SIDECAR_TOKEN_HEADER, token)
            .header(header::CONTENT_TYPE, "application/json")
            .body(
                serde_json::json!({
                    "callback_input": format!("http://localhost:1455/auth/callback?code=sc-code-1&state={state_param}"),
                    "conversation_id": "conv-complete-2222"
                })
                .to_string(),
            )
            .send()
            .await
            .expect("complete");
        assert_eq!(complete.status(), StatusCode::OK);
        let complete_raw = complete.text().await.expect("complete text");
        assert!(!complete_raw.contains(&access_jwt));
        assert!(!complete_raw.contains(refresh_secret));
        let complete_json: serde_json::Value =
            serde_json::from_str(&complete_raw).expect("complete json");
        assert_eq!(
            complete_json["config"]["providers"]["codex"]["authenticated"].as_bool(),
            Some(true)
        );
        assert_eq!(
            complete_json["config"]["providers"]["codex"]["plan_type"].as_str(),
            Some("plus")
        );

        let codex_ok = client
            .post(format!("http://{addr}/_api/test_connection"))
            .header(SIDECAR_TOKEN_HEADER, token)
            .header(header::CONTENT_TYPE, "application/json")
            .body(r#"{"provider":"codex"}"#)
            .send()
            .await
            .expect("codex ok");
        assert_eq!(codex_ok.status(), StatusCode::OK);
        let codex_ok_json: serde_json::Value = codex_ok.json().await.expect("codex ok json");
        assert_eq!(codex_ok_json["ok"].as_bool(), Some(true));

        // 5. disconnect_codex clears auth/codex.json and forwards conversation_id.
        let disc = client
            .post(format!("http://{addr}/_api/disconnect_codex"))
            .header(SIDECAR_TOKEN_HEADER, token)
            .header(header::CONTENT_TYPE, "application/json")
            .body(r#"{"conversation_id":"conv-disconnect-3333"}"#)
            .send()
            .await
            .expect("disconnect");
        assert_eq!(disc.status(), StatusCode::OK);
        let disc_json: serde_json::Value = disc.json().await.expect("disc json");
        assert_eq!(
            disc_json["config"]["providers"]["codex"]["authenticated"].as_bool(),
            Some(false)
        );

        let queries = captured_conv_ids.lock().await.clone();
        assert!(
            queries.iter().any(|q| q.contains("conv-cancel-1111")),
            "cancel_codex_login must forward conversation_id: {queries:?}"
        );
        assert!(
            queries.iter().any(|q| q.contains("conv-complete-2222")),
            "complete_codex_login must forward conversation_id: {queries:?}"
        );
        assert!(
            queries.iter().any(|q| q.contains("conv-disconnect-3333")),
            "disconnect_codex must forward conversation_id: {queries:?}"
        );

        server.abort();
        mock_server.abort();
    }
}
