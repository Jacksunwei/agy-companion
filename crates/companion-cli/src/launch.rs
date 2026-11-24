use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use companion_proxy::HealthResponse;

use crate::{CliError, load_config_checked, resolve_cli_config_path};

pub const PROXY_PLACEHOLDER_API_KEY: &str = "agy-companion-local";
#[cfg(target_os = "macos")]
const DEFAULT_APP_BIN: &str = "/Applications/Antigravity.app/Contents/MacOS/Antigravity";
#[cfg(not(target_os = "macos"))]
const DEFAULT_APP_BIN: &str = "antigravity";
const HEALTH_POLL_TIMEOUT: Duration = Duration::from_secs(5);
const HEALTH_POLL_INTERVAL: Duration = Duration::from_millis(100);

pub async fn probe_running_companion_proxy(port: u16) -> Option<HealthResponse> {
    let client = reqwest::Client::builder()
        .no_proxy()
        .redirect(reqwest::redirect::Policy::none())
        .connect_timeout(Duration::from_secs(2))
        .timeout(Duration::from_secs(2))
        .build()
        .ok()?;
    let url = format!("http://127.0.0.1:{port}/health");
    let resp = client.get(url).send().await.ok()?;
    if !resp.status().is_success() {
        return None;
    }
    let body = resp.bytes().await.ok()?;
    let parsed: HealthResponse = serde_json::from_slice(&body).ok()?;
    if parsed.status == "ok" && parsed.protocol_version > 0 {
        Some(parsed)
    } else {
        None
    }
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
}
