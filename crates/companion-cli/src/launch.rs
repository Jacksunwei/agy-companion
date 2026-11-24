use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use companion_proxy::HealthResponse;

use crate::{CliError, load_config_checked, resolve_cli_config_path};

pub use companion_core::PROXY_PLACEHOLDER_API_KEY;
pub const MAX_PROBE_HEALTH_BODY_BYTES: usize = 64 * 1024;
pub const DEFAULT_PROBE_HEALTH_TIMEOUT: Duration = Duration::from_secs(2);
#[cfg(target_os = "macos")]
const DEFAULT_APP_BIN: &str = "/Applications/Antigravity.app/Contents/MacOS/Antigravity";
#[cfg(not(target_os = "macos"))]
const DEFAULT_APP_BIN: &str = "antigravity";
const HEALTH_POLL_TIMEOUT: Duration = Duration::from_secs(5);
const HEALTH_POLL_INTERVAL: Duration = Duration::from_millis(100);

pub async fn probe_running_companion_proxy_with_timeout(
    port: u16,
    timeout: Duration,
) -> Option<HealthResponse> {
    if timeout.is_zero() {
        return None;
    }
    let client = reqwest::Client::builder()
        .no_proxy()
        .redirect(reqwest::redirect::Policy::none())
        .connect_timeout(timeout)
        .timeout(timeout)
        .user_agent(concat!("agy-companion/", env!("CARGO_PKG_VERSION")))
        .build()
        .ok()?;
    let url = format!("http://127.0.0.1:{port}/health");
    let mut resp = client.get(url).send().await.ok()?;
    if resp.status() != reqwest::StatusCode::OK {
        return None;
    }
    if resp
        .content_length()
        .is_some_and(|len| len > MAX_PROBE_HEALTH_BODY_BYTES as u64)
    {
        return None;
    }
    let mut body = Vec::new();
    let mut total_bytes: usize = 0;
    while let Some(chunk) = resp.chunk().await.ok()? {
        total_bytes = total_bytes.saturating_add(chunk.len());
        if total_bytes > MAX_PROBE_HEALTH_BODY_BYTES {
            return None;
        }
        body.extend_from_slice(&chunk);
    }
    let parsed: HealthResponse = serde_json::from_slice(&body).ok()?;
    if parsed.status == "ok" && parsed.protocol_version > 0 {
        Some(parsed)
    } else {
        None
    }
}

pub async fn probe_running_companion_proxy(port: u16) -> Option<HealthResponse> {
    probe_running_companion_proxy_with_timeout(port, DEFAULT_PROBE_HEALTH_TIMEOUT).await
}

pub fn default_antigravity_app_path() -> PathBuf {
    PathBuf::from(DEFAULT_APP_BIN)
}

pub fn format_hybrid_launch_command(port: u16, app_path: &Path) -> String {
    let app_display = shell_quote(&app_path.display().to_string());
    format!(
        "CLOUD_CODE_URL=\"http://127.0.0.1:{port}\" \\\nGOOGLE_GEMINI_BASE_URL=\"http://127.0.0.1:{port}\" \\\nGEMINI_API_KEY=\"{PROXY_PLACEHOLDER_API_KEY}\" \\\n{app_display}"
    )
}

fn shell_quote(raw: &str) -> String {
    if raw
        .bytes()
        .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'/' | b'.' | b'_' | b'-' | b':'))
    {
        raw.to_string()
    } else {
        format!("'{}'", raw.replace('\'', "'\\''"))
    }
}

pub async fn run_launch<W: Write>(
    port_override: Option<u16>,
    config_override: Option<PathBuf>,
    app_override: Option<PathBuf>,
    print_env: bool,
    out: &mut W,
) -> Result<(), CliError> {
    if port_override == Some(0) {
        return Err(CliError::InvalidPort(0));
    }

    let config_path = resolve_cli_config_path(config_override.as_deref())?;
    let mut config = load_config_checked(&config_path)?;
    if let Some(port) = port_override {
        config.listen_port = port;
    }
    config.validate()?;

    let app_path = match app_override {
        Some(path) => {
            companion_core::validate_path(&path)?;
            path
        }
        None => default_antigravity_app_path(),
    };

    let port = config.listen_port;
    if print_env {
        writeln!(out, "{}", format_hybrid_launch_command(port, &app_path))?;
        return Ok(());
    }

    if probe_running_companion_proxy(port).await.is_none() {
        let exe = std::env::current_exe()?;
        let mut serve_cmd = Command::new(exe);
        serve_cmd
            .arg("serve")
            .arg("--port")
            .arg(port.to_string())
            .arg("--config")
            .arg(&config_path)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        serve_cmd.spawn()?;

        let deadline = Instant::now() + HEALTH_POLL_TIMEOUT;
        let mut ready = false;
        while Instant::now() < deadline {
            if probe_running_companion_proxy(port).await.is_some() {
                ready = true;
                break;
            }
            tokio::time::sleep(HEALTH_POLL_INTERVAL).await;
        }
        if !ready {
            return Err(CliError::Io(std::io::Error::new(
                std::io::ErrorKind::TimedOut,
                format!("timed out waiting for agy-companion proxy on 127.0.0.1:{port}"),
            )));
        }
    }

    let proxy_url = format!("http://127.0.0.1:{port}");
    let mut app_cmd = Command::new(&app_path);
    app_cmd
        .env("CLOUD_CODE_URL", &proxy_url)
        .env("GOOGLE_GEMINI_BASE_URL", &proxy_url)
        .env("GEMINI_API_KEY", PROXY_PLACEHOLDER_API_KEY)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    app_cmd.spawn()?;

    writeln!(
        out,
        "Launched {} in Hybrid Mode via {} (original Google AI Pro models + proxy models).",
        app_path.display(),
        proxy_url
    )?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use super::*;

    #[test]
    fn formats_hybrid_launch_command_with_all_required_env_vars() {
        let rendered = format_hybrid_launch_command(
            4141,
            Path::new("/Applications/Antigravity.app/Contents/MacOS/Antigravity"),
        );
        assert!(rendered.contains("CLOUD_CODE_URL=\"http://127.0.0.1:4141\""));
        assert!(rendered.contains("GOOGLE_GEMINI_BASE_URL=\"http://127.0.0.1:4141\""));
        assert!(rendered.contains("GEMINI_API_KEY=\"agy-companion-local\""));
        assert!(rendered.contains("/Applications/Antigravity.app/Contents/MacOS/Antigravity"));

        let quoted = format_hybrid_launch_command(4242, Path::new("/tmp/My App/Antigravity"));
        assert!(quoted.contains("'/tmp/My App/Antigravity'"));
    }

    #[tokio::test]
    async fn run_launch_print_env_emits_command_without_spawning() {
        let tmp = tempfile::TempDir::new().expect("tempdir");
        let cfg_path = tmp.path().join("config.json");
        let mut buf = Vec::new();
        run_launch(
            Some(4321),
            Some(cfg_path),
            Some(PathBuf::from("/opt/antigravity")),
            true,
            &mut buf,
        )
        .await
        .expect("print-env succeeds");
        let out = String::from_utf8(buf).expect("utf8");
        assert!(out.contains("CLOUD_CODE_URL=\"http://127.0.0.1:4321\""));
        assert!(out.contains("GOOGLE_GEMINI_BASE_URL=\"http://127.0.0.1:4321\""));
        assert!(out.contains("GEMINI_API_KEY=\"agy-companion-local\""));
        assert!(out.contains("/opt/antigravity"));
    }

    async fn spawn_mock_router(router: axum::Router) -> (u16, tokio::task::JoinHandle<()>) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind");
        let port = listener.local_addr().expect("addr").port();
        let handle = tokio::spawn(async move {
            let _ = axum::serve(listener, router).await;
        });
        (port, handle)
    }

    #[tokio::test]
    async fn probe_running_companion_proxy_with_timeout_rejects_body_over_64_kib() {
        use axum::routing::get;
        use tokio::io::{AsyncReadExt, AsyncWriteExt};

        // 1. Oversized with Content-Length header
        let big = format!(
            r#"{{"status":"ok","version":"0.1.0","protocol_version":1,"pad":"{}"}}"#,
            "a".repeat(MAX_PROBE_HEALTH_BODY_BYTES + 128)
        );
        let router = axum::Router::new().route("/health", get(move || async move { big.clone() }));
        let (cl_port, cl_handle) = spawn_mock_router(router).await;
        assert_eq!(
            probe_running_companion_proxy_with_timeout(cl_port, Duration::from_secs(2)).await,
            None
        );
        cl_handle.abort();

        // 2. Oversized chunked response without Content-Length header
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind raw");
        let chunked_port = listener.local_addr().expect("addr").port();
        let chunked_handle = tokio::spawn(async move {
            if let Ok((mut stream, _)) = listener.accept().await {
                let mut req_buf = [0u8; 1024];
                let _ = stream.read(&mut req_buf).await;
                let _ = stream
                    .write_all(
                        b"HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nTransfer-Encoding: chunked\r\n\r\n",
                    )
                    .await;
                let chunk = vec![b'x'; 16 * 1024];
                let chunk_header = format!("{:x}\r\n", chunk.len());
                for _ in 0..5 {
                    if stream.write_all(chunk_header.as_bytes()).await.is_err()
                        || stream.write_all(&chunk).await.is_err()
                        || stream.write_all(b"\r\n").await.is_err()
                    {
                        break;
                    }
                }
                let _ = stream.write_all(b"0\r\n\r\n").await;
            }
        });
        assert_eq!(
            probe_running_companion_proxy_with_timeout(chunked_port, Duration::from_secs(2)).await,
            None
        );
        chunked_handle.abort();
    }

    #[tokio::test]
    async fn probe_running_companion_proxy_with_timeout_respects_fast_timeout() {
        use axum::routing::get;

        let router = axum::Router::new().route(
            "/health",
            get(|| async {
                tokio::time::sleep(Duration::from_secs(1)).await;
                axum::Json(HealthResponse {
                    status: "ok".to_string(),
                    version: env!("CARGO_PKG_VERSION").to_string(),
                    protocol_version: 1,
                })
            }),
        );
        let (port, handle) = spawn_mock_router(router).await;

        assert_eq!(
            probe_running_companion_proxy_with_timeout(port, Duration::ZERO).await,
            None
        );

        let started = Instant::now();
        let res =
            probe_running_companion_proxy_with_timeout(port, Duration::from_millis(250)).await;
        let elapsed = started.elapsed();
        assert_eq!(res, None);
        assert!(
            elapsed < Duration::from_millis(800),
            "expected fast timeout (<800ms), got {elapsed:?}"
        );
        handle.abort();
    }

    #[tokio::test]
    async fn probe_running_companion_proxy_accepts_healthy_and_rejects_non_200_or_redirect() {
        use axum::http::StatusCode;
        use axum::response::Redirect;
        use axum::routing::get;

        // 1. Valid 200 OK health response
        let ok_router = axum::Router::new().route(
            "/health",
            get(|| async {
                axum::Json(HealthResponse {
                    status: "ok".to_string(),
                    version: env!("CARGO_PKG_VERSION").to_string(),
                    protocol_version: 1,
                })
            }),
        );
        let (ok_port, ok_handle) = spawn_mock_router(ok_router).await;
        let health = probe_running_companion_proxy(ok_port)
            .await
            .expect("healthy proxy should return Some");
        assert_eq!(health.status, "ok");
        assert_eq!(health.version, env!("CARGO_PKG_VERSION"));
        assert_eq!(health.protocol_version, 1);
        ok_handle.abort();

        // 2. Redirect (302) rejected
        let redirect_router =
            axum::Router::new().route("/health", get(|| async { Redirect::temporary("/other") }));
        let (redir_port, redir_handle) = spawn_mock_router(redirect_router).await;
        assert_eq!(probe_running_companion_proxy(redir_port).await, None);
        redir_handle.abort();

        // 3. Non-200 (204 No Content and 503 Service Unavailable) rejected
        for status in [StatusCode::NO_CONTENT, StatusCode::SERVICE_UNAVAILABLE] {
            let err_router =
                axum::Router::new().route("/health", get(move || async move { status }));
            let (err_port, err_handle) = spawn_mock_router(err_router).await;
            assert_eq!(probe_running_companion_proxy(err_port).await, None);
            err_handle.abort();
        }
    }
}
