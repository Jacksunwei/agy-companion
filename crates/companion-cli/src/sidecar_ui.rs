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
use companion_ui::{CompanionUiServer, GetUsageArgs, SaveSettingsArgs, UiServerOptions};
use http::{HeaderMap, HeaderValue, StatusCode, header};
use serde::Deserialize;
use tokio::net::TcpListener;

use crate::CliError;

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
        Ok(snapshot) => api_ok(&snapshot),
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
        Ok(snapshot) => api_ok(&snapshot),
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
        Ok(snapshot) => api_ok(&snapshot),
        Err(msg) => api_error(StatusCode::BAD_REQUEST, msg),
    }
}

pub fn build_sidecar_ui_router(ui: Arc<CompanionUiServer>, token: Option<String>) -> Router {
    let state = SidecarUiState {
        ui,
        token: token.map(Arc::new),
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
    let router = build_sidecar_ui_router(ui, sidecar_cfg.token);
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
}
