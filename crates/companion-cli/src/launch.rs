use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::Duration;

use companion_core::HealthResponse;
use companion_core::integration::{DEFAULT_RESTORE_PREFIX, is_valid_restore_prefix};
use companion_providers::{DEFAULT_PROBE_TIMEOUT, LoopbackProxyClient};

use crate::daemon::{
    DEFAULT_IDLE_SHUTDOWN_SECS, WaitHealthyOptions, WaitHealthyOutcome,
    spawn_detached_proxy_daemon, wait_for_daemon_healthy,
};
use crate::{CliError, load_config_checked, resolve_cli_config_path};

pub use companion_core::PROXY_PLACEHOLDER_API_KEY;
pub use companion_providers::{
    DEFAULT_PROBE_TIMEOUT as DEFAULT_PROBE_HEALTH_TIMEOUT,
    MAX_PROBE_BODY_BYTES as MAX_PROBE_HEALTH_BODY_BYTES,
};
const HEALTH_POLL_TIMEOUT: Duration = Duration::from_secs(5);
const HEALTH_POLL_INTERVAL: Duration = Duration::from_millis(100);

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct LaunchEnvSnapshot {
    pub existing_restore_prefix: Option<String>,
    pub existing_restore_vars: Option<String>,
    pub existing_cloud_code_url: Option<String>,
    pub existing_google_gemini_base_url: Option<String>,
    /// True only if the resolved restore prefix already had `<prefix>CLOUD_CODE_URL` set.
    pub has_existing_prefixed_cloud_code_url: bool,
    /// True only if the resolved restore prefix already had `<prefix>GOOGLE_GEMINI_BASE_URL` set.
    pub has_existing_prefixed_google_gemini_base_url: bool,
    /// True if `CLOUD_CODE_URL` was already tracked in `JETSKI_MCP_ENV_RESTORE_VARS`.
    pub vars_already_has_cloud_code_url: bool,
    /// True if `GOOGLE_GEMINI_BASE_URL` was already tracked in `JETSKI_MCP_ENV_RESTORE_VARS`.
    pub vars_already_has_google_gemini_base_url: bool,
    /// True only if the resolved restore prefix already had `<prefix>GEMINI_API_KEY` set
    /// AND `GEMINI_API_KEY` was already tracked in `JETSKI_MCP_ENV_RESTORE_VARS`.
    pub preserve_existing_prefixed_gemini_api_key: bool,
    /// True iff `GEMINI_API_KEY` is set, non-empty/non-whitespace, and NOT `PROXY_PLACEHOLDER_API_KEY`.
    /// Deliberately stores only a `bool` (never plaintext secret) per S2.
    pub has_real_gemini_api_key: bool,
}

impl LaunchEnvSnapshot {
    pub fn from_process() -> Self {
        let existing_restore_prefix = std::env::var("JETSKI_MCP_ENV_RESTORE_PREFIX").ok();
        let existing_restore_vars = std::env::var("JETSKI_MCP_ENV_RESTORE_VARS").ok();
        let effective_prefix = existing_restore_prefix
            .as_deref()
            .filter(|p| is_valid_restore_prefix(p))
            .unwrap_or(DEFAULT_RESTORE_PREFIX);

        let existing_cloud_code_url = std::env::var("CLOUD_CODE_URL").ok();
        let existing_google_gemini_base_url = std::env::var("GOOGLE_GEMINI_BASE_URL").ok();
        let has_existing_prefixed_cloud_code_url =
            std::env::var_os(format!("{effective_prefix}CLOUD_CODE_URL")).is_some();
        let has_existing_prefixed_google_gemini_base_url =
            std::env::var_os(format!("{effective_prefix}GOOGLE_GEMINI_BASE_URL")).is_some();
        let vars_already_has = |name: &str| {
            existing_restore_vars
                .as_deref()
                .is_some_and(|raw| raw.split(',').any(|tok| tok.trim() == name))
        };
        let vars_already_has_cloud_code_url = vars_already_has("CLOUD_CODE_URL");
        let vars_already_has_google_gemini_base_url = vars_already_has("GOOGLE_GEMINI_BASE_URL");
        let vars_already_has_gemini_key = vars_already_has("GEMINI_API_KEY");
        let preserve_existing_prefixed_gemini_api_key = vars_already_has_gemini_key
            && std::env::var_os(format!("{effective_prefix}GEMINI_API_KEY")).is_some();
        let has_real_gemini_api_key = std::env::var("GEMINI_API_KEY").ok().is_some_and(|v| {
            let trimmed = v.trim();
            !trimmed.is_empty() && !companion_core::is_placeholder_api_key(trimmed)
        });

        Self {
            existing_restore_prefix,
            existing_restore_vars,
            existing_cloud_code_url,
            existing_google_gemini_base_url,
            has_existing_prefixed_cloud_code_url,
            has_existing_prefixed_google_gemini_base_url,
            vars_already_has_cloud_code_url,
            vars_already_has_google_gemini_base_url,
            preserve_existing_prefixed_gemini_api_key,
            has_real_gemini_api_key,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HybridLaunchEnv {
    pub set: Vec<(String, String)>,
    pub unset: Vec<String>,
}

pub fn build_hybrid_launch_env(port: u16, snapshot: &LaunchEnvSnapshot) -> HybridLaunchEnv {
    let prefix = snapshot
        .existing_restore_prefix
        .as_deref()
        .filter(|p| is_valid_restore_prefix(p))
        .unwrap_or(DEFAULT_RESTORE_PREFIX)
        .to_string();

    let mut vars: Vec<String> = Vec::new();
    if let Some(ref existing) = snapshot.existing_restore_vars {
        for raw in existing.split(',') {
            let tok = raw.trim();
            if !tok.is_empty() && !vars.iter().any(|v| v == tok) {
                vars.push(tok.to_string());
            }
        }
    }

    let mut push_unique = |name: &str| {
        if !vars.iter().any(|v| v == name) {
            vars.push(name.to_string());
        }
    };
    push_unique("CLOUD_CODE_URL");
    push_unique("GOOGLE_GEMINI_BASE_URL");
    if !snapshot.has_real_gemini_api_key {
        push_unique("GEMINI_API_KEY");
    }

    let mut set = Vec::new();
    let mut unset = Vec::new();
    let proxy_url = format!("http://127.0.0.1:{port}");

    set.push(("CLOUD_CODE_URL".to_string(), proxy_url.clone()));
    set.push(("GOOGLE_GEMINI_BASE_URL".to_string(), proxy_url));

    if !snapshot.has_real_gemini_api_key {
        set.push((
            "GEMINI_API_KEY".to_string(),
            PROXY_PLACEHOLDER_API_KEY.to_string(),
        ));
    }

    set.push(("JETSKI_MCP_ENV_RESTORE_PREFIX".to_string(), prefix.clone()));
    set.push(("JETSKI_MCP_ENV_RESTORE_VARS".to_string(), vars.join(",")));

    let existing_vars_has = |name: &str| {
        snapshot
            .existing_restore_vars
            .as_deref()
            .is_some_and(|raw| raw.split(',').any(|tok| tok.trim() == name))
    };
    let preserve_cloud_code_backup = snapshot.has_existing_prefixed_cloud_code_url
        || snapshot.vars_already_has_cloud_code_url
        || existing_vars_has("CLOUD_CODE_URL");
    if !preserve_cloud_code_backup {
        let backup_key = format!("{prefix}CLOUD_CODE_URL");
        if let Some(ref orig) = snapshot.existing_cloud_code_url {
            set.push((backup_key, orig.clone()));
        } else {
            unset.push(backup_key);
        }
    }

    let preserve_google_gemini_base_backup = snapshot.has_existing_prefixed_google_gemini_base_url
        || snapshot.vars_already_has_google_gemini_base_url
        || existing_vars_has("GOOGLE_GEMINI_BASE_URL");
    if !preserve_google_gemini_base_backup {
        let backup_key = format!("{prefix}GOOGLE_GEMINI_BASE_URL");
        if let Some(ref orig) = snapshot.existing_google_gemini_base_url {
            set.push((backup_key, orig.clone()));
        } else {
            unset.push(backup_key);
        }
    }

    if !snapshot.has_real_gemini_api_key && !snapshot.preserve_existing_prefixed_gemini_api_key {
        unset.push(format!("{prefix}GEMINI_API_KEY"));
    }

    HybridLaunchEnv { set, unset }
}

pub async fn probe_running_companion_proxy_with_timeout(
    port: u16,
    timeout: Duration,
) -> Option<HealthResponse> {
    if timeout.is_zero() {
        return None;
    }
    let client = LoopbackProxyClient::new(timeout, timeout).ok()?;
    client.probe_healthy_companion(port, Some(timeout)).await
}

pub async fn probe_running_companion_proxy(port: u16) -> Option<HealthResponse> {
    probe_running_companion_proxy_with_timeout(port, DEFAULT_PROBE_HEALTH_TIMEOUT).await
}

#[cfg(any(windows, test))]
fn is_valid_windows_base_dir(dir: &Path) -> bool {
    !dir.as_os_str().is_empty()
        && !dir.to_string_lossy().trim().is_empty()
        && dir.is_absolute()
        && companion_core::validate_path(dir).is_ok()
}

#[cfg(any(windows, test))]
pub(crate) fn discover_windows_antigravity_exe(
    local_app_data: Option<&Path>,
    program_files: Option<&Path>,
) -> PathBuf {
    let candidates = [
        local_app_data
            .filter(|dir| is_valid_windows_base_dir(dir))
            .map(|dir| {
                dir.join("Programs")
                    .join("Antigravity")
                    .join("Antigravity.exe")
            }),
        program_files
            .filter(|dir| is_valid_windows_base_dir(dir))
            .map(|dir| dir.join("Antigravity").join("Antigravity.exe")),
    ];
    for candidate in candidates.into_iter().flatten() {
        if companion_core::fs_safe::inspect_regular_file_if_exists(&candidate, None)
            .is_ok_and(|meta| meta.is_some())
        {
            return candidate;
        }
    }
    PathBuf::from("antigravity")
}

pub fn default_antigravity_app_path() -> PathBuf {
    #[cfg(target_os = "macos")]
    {
        PathBuf::from("/Applications/Antigravity.app/Contents/MacOS/Antigravity")
    }
    #[cfg(windows)]
    {
        let local_app_data = std::env::var_os("LOCALAPPDATA")
            .filter(|v| !v.to_string_lossy().trim().is_empty())
            .map(PathBuf::from);
        let program_files = std::env::var_os("ProgramFiles")
            .filter(|v| !v.to_string_lossy().trim().is_empty())
            .map(PathBuf::from);
        discover_windows_antigravity_exe(local_app_data.as_deref(), program_files.as_deref())
    }
    #[cfg(all(not(target_os = "macos"), not(windows)))]
    {
        PathBuf::from("antigravity")
    }
}

pub fn format_hybrid_launch_command(port: u16, app_path: &Path) -> String {
    format_hybrid_launch_command_with_env(port, app_path, &LaunchEnvSnapshot::default())
}

pub fn format_hybrid_launch_command_with_env(
    port: u16,
    app_path: &Path,
    snapshot: &LaunchEnvSnapshot,
) -> String {
    let env = build_hybrid_launch_env(port, snapshot);
    let mut lines: Vec<String> = Vec::with_capacity(env.set.len() + 1);
    for (key, value) in &env.set {
        let is_backup_var = key.ends_with("CLOUD_CODE_URL") && key != "CLOUD_CODE_URL"
            || key.ends_with("GOOGLE_GEMINI_BASE_URL") && key != "GOOGLE_GEMINI_BASE_URL";
        let formatted_val = if is_backup_var || !is_safe_double_quoted_value(value) {
            shell_quote_single(value)
        } else {
            format!("\"{value}\"")
        };
        lines.push(format!("{key}={formatted_val} \\"));
    }
    lines.push(shell_quote(&app_path.display().to_string()));
    lines.join("\n")
}

fn is_safe_double_quoted_value(raw: &str) -> bool {
    raw.bytes()
        .all(|b| b.is_ascii_alphanumeric() || matches!(b, b':' | b'/' | b'.' | b'_' | b'-' | b','))
}

fn shell_quote_single(raw: &str) -> String {
    format!("'{}'", raw.replace('\'', "'\\''"))
}

fn shell_quote(raw: &str) -> String {
    if raw
        .bytes()
        .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'/' | b'.' | b'_' | b'-' | b':'))
    {
        raw.to_string()
    } else {
        shell_quote_single(raw)
    }
}

pub async fn run_launch<W: Write>(
    port_override: Option<u16>,
    config_override: Option<PathBuf>,
    app_override: Option<PathBuf>,
    print_env: bool,
    out: &mut W,
) -> Result<(), CliError> {
    let snapshot = LaunchEnvSnapshot::from_process();
    run_launch_with_env(
        port_override,
        config_override,
        app_override,
        print_env,
        &snapshot,
        out,
    )
    .await
}

pub async fn run_launch_with_env<W: Write>(
    port_override: Option<u16>,
    config_override: Option<PathBuf>,
    app_override: Option<PathBuf>,
    print_env: bool,
    snapshot: &LaunchEnvSnapshot,
    out: &mut W,
) -> Result<(), CliError> {
    if port_override == Some(0) {
        return Err(CliError::InvalidPort(0));
    }

    let config_path = resolve_cli_config_path(config_override.as_deref())?;
    let mut config = load_config_checked(&config_path)?;
    if let Some(port) = port_override {
        config.proxy.port = std::num::NonZeroU16::new(port).ok_or(CliError::InvalidPort(0))?;
    }
    config.validate()?;

    let app_path = match app_override {
        Some(path) => {
            companion_core::validate_path(&path)?;
            path
        }
        None => default_antigravity_app_path(),
    };

    let port = config.proxy.port.get();
    if print_env {
        writeln!(
            out,
            "{}",
            format_hybrid_launch_command_with_env(port, &app_path, snapshot)
        )?;
        return Ok(());
    }

    let probe = LoopbackProxyClient::with_default_timeouts()?;
    if probe.probe_healthy_companion(port, None).await.is_none() {
        let mut serve_child =
            spawn_detached_proxy_daemon(port, &config_path, DEFAULT_IDLE_SHUTDOWN_SECS)?;

        match wait_for_daemon_healthy(
            &probe,
            &mut serve_child,
            WaitHealthyOptions {
                port,
                total_timeout: HEALTH_POLL_TIMEOUT,
                poll_interval: HEALTH_POLL_INTERVAL,
                probe_timeout: DEFAULT_PROBE_TIMEOUT,
                expected_version: None,
            },
        )
        .await
        {
            WaitHealthyOutcome::Ready(_) => {}
            WaitHealthyOutcome::ExitedEarly { status, .. } => {
                return Err(CliError::Io(std::io::Error::other(format!(
                    "agy-companion proxy exited early ({status}) before becoming healthy on 127.0.0.1:{port}"
                ))));
            }
            WaitHealthyOutcome::TimedOut { .. } => {
                return Err(CliError::Io(std::io::Error::new(
                    std::io::ErrorKind::TimedOut,
                    format!("timed out waiting for agy-companion proxy on 127.0.0.1:{port}"),
                )));
            }
        }
    }

    let proxy_url = format!("http://127.0.0.1:{port}");
    let hybrid_env = build_hybrid_launch_env(port, snapshot);
    let mut app_cmd = Command::new(&app_path);
    for var in &hybrid_env.unset {
        app_cmd.env_remove(var);
    }
    for (key, val) in &hybrid_env.set {
        app_cmd.env(key, val);
    }
    app_cmd
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
    use std::time::Instant;

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
        assert!(rendered.contains("JETSKI_MCP_ENV_RESTORE_PREFIX=\"AGY_COMPANION_ORIG_\""));
        assert!(rendered.contains(
            "JETSKI_MCP_ENV_RESTORE_VARS=\"CLOUD_CODE_URL,GOOGLE_GEMINI_BASE_URL,GEMINI_API_KEY\""
        ));
        assert!(rendered.contains("/Applications/Antigravity.app/Contents/MacOS/Antigravity"));

        let quoted = format_hybrid_launch_command(4242, Path::new("/tmp/My App/Antigravity"));
        assert!(quoted.contains("'/tmp/My App/Antigravity'"));
    }

    #[test]
    fn build_hybrid_launch_env_respects_real_gemini_key_and_never_touches_google_api_key() {
        let snap = LaunchEnvSnapshot {
            has_real_gemini_api_key: true,
            ..Default::default()
        };
        let env = build_hybrid_launch_env(4141, &snap);
        assert!(!env.set.iter().any(|(k, _)| k == "GEMINI_API_KEY"));
        assert!(!env.set.iter().any(|(k, _)| k.contains("GOOGLE_API_KEY")));
        assert!(!env.unset.iter().any(|k| k.contains("GOOGLE_API_KEY")));
        assert!(!env.unset.iter().any(|k| k.ends_with("GEMINI_API_KEY")));

        let vars = env
            .set
            .iter()
            .find(|(k, _)| k == "JETSKI_MCP_ENV_RESTORE_VARS")
            .map(|(_, v)| v.as_str())
            .unwrap();
        assert_eq!(vars, "CLOUD_CODE_URL,GOOGLE_GEMINI_BASE_URL");
    }

    #[test]
    fn build_hybrid_launch_env_merges_valid_custom_prefix_and_sanitizes_invalid_prefix() {
        let custom = LaunchEnvSnapshot {
            existing_restore_prefix: Some("CUSTOM_".to_string()),
            existing_restore_vars: Some("FOO, BAR".to_string()),
            existing_cloud_code_url: Some("https://orig.example/$(id)".to_string()),
            existing_google_gemini_base_url: Some("https://gen.example".to_string()),
            ..Default::default()
        };
        let env = build_hybrid_launch_env(4242, &custom);
        assert!(env.set.contains(&(
            "JETSKI_MCP_ENV_RESTORE_PREFIX".to_string(),
            "CUSTOM_".to_string()
        )));
        assert!(env.set.contains(&(
            "JETSKI_MCP_ENV_RESTORE_VARS".to_string(),
            "FOO,BAR,CLOUD_CODE_URL,GOOGLE_GEMINI_BASE_URL,GEMINI_API_KEY".to_string()
        )));
        assert!(env.set.contains(&(
            "CUSTOM_CLOUD_CODE_URL".to_string(),
            "https://orig.example/$(id)".to_string()
        )));
        assert!(env.set.contains(&(
            "CUSTOM_GOOGLE_GEMINI_BASE_URL".to_string(),
            "https://gen.example".to_string()
        )));

        // Single-quoted backup value in --print-env prevents shell expansion
        let rendered = format_hybrid_launch_command_with_env(4242, Path::new("/opt/agy"), &custom);
        assert!(rendered.contains("CUSTOM_CLOUD_CODE_URL='https://orig.example/$(id)'"));
        assert!(rendered.contains("CUSTOM_GOOGLE_GEMINI_BASE_URL='https://gen.example'"));

        // Invalid prefix falls back to AGY_COMPANION_ORIG_
        for bad_prefix in ["$(evil)", "123BAD", "BAD-PREFIX", ""] {
            let bad = LaunchEnvSnapshot {
                existing_restore_prefix: Some(bad_prefix.to_string()),
                ..Default::default()
            };
            let env_bad = build_hybrid_launch_env(4141, &bad);
            assert!(env_bad.set.contains(&(
                "JETSKI_MCP_ENV_RESTORE_PREFIX".to_string(),
                DEFAULT_RESTORE_PREFIX.to_string()
            )));
        }
    }

    #[test]
    fn build_hybrid_launch_env_preserves_three_state_unset_backup_when_already_tracked_in_restore_vars()
     {
        let snap = LaunchEnvSnapshot {
            existing_restore_prefix: Some("OUTER_".to_string()),
            existing_restore_vars: Some("FOO, CLOUD_CODE_URL, GOOGLE_GEMINI_BASE_URL".to_string()),
            existing_cloud_code_url: Some("https://wrapper-cc.example".to_string()),
            existing_google_gemini_base_url: Some("https://wrapper-gem.example".to_string()),
            has_existing_prefixed_cloud_code_url: false,
            has_existing_prefixed_google_gemini_base_url: false,
            ..Default::default()
        };
        let env = build_hybrid_launch_env(4242, &snap);
        assert!(env.set.contains(&(
            "JETSKI_MCP_ENV_RESTORE_PREFIX".to_string(),
            "OUTER_".to_string()
        )));
        assert!(env.set.contains(&(
            "JETSKI_MCP_ENV_RESTORE_VARS".to_string(),
            "FOO,CLOUD_CODE_URL,GOOGLE_GEMINI_BASE_URL,GEMINI_API_KEY".to_string()
        )));
        // Neither backup var may be overwritten or explicitly unset — outer wrapper's 3-state
        // originally-unset sentinel must be preserved untouched.
        assert!(!env.set.iter().any(|(k, _)| k == "OUTER_CLOUD_CODE_URL"));
        assert!(!env.unset.iter().any(|k| k == "OUTER_CLOUD_CODE_URL"));
        assert!(
            !env.set
                .iter()
                .any(|(k, _)| k == "OUTER_GOOGLE_GEMINI_BASE_URL")
        );
        assert!(
            !env.unset
                .iter()
                .any(|k| k == "OUTER_GOOGLE_GEMINI_BASE_URL")
        );
    }

    #[tokio::test]
    async fn run_launch_print_env_emits_command_without_spawning() {
        let tmp = tempfile::TempDir::new().expect("tempdir");
        let cfg_path = tmp.path().join("config.json");
        let mut buf = Vec::new();
        run_launch_with_env(
            Some(4321),
            Some(cfg_path),
            Some(PathBuf::from("/opt/antigravity")),
            true,
            &LaunchEnvSnapshot::default(),
            &mut buf,
        )
        .await
        .expect("print-env succeeds");
        let out = String::from_utf8(buf).expect("utf8");
        assert!(out.contains("CLOUD_CODE_URL=\"http://127.0.0.1:4321\""));
        assert!(out.contains("GOOGLE_GEMINI_BASE_URL=\"http://127.0.0.1:4321\""));
        assert!(out.contains("GEMINI_API_KEY=\"agy-companion-local\""));
        assert!(out.contains("JETSKI_MCP_ENV_RESTORE_PREFIX=\"AGY_COMPANION_ORIG_\""));
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
        tokio::task::yield_now().await;
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

    #[test]
    fn discover_windows_antigravity_exe_prefers_local_app_data_then_program_files_then_fallback() {
        let tmp = tempfile::TempDir::new().expect("tempdir");
        let local_app_data = tmp.path().join("LocalAppData");
        let program_files = tmp.path().join("ProgramFiles");

        // Neither installed -> fallback to "antigravity"
        assert_eq!(
            discover_windows_antigravity_exe(Some(&local_app_data), Some(&program_files)),
            PathBuf::from("antigravity")
        );

        // Only ProgramFiles installed -> returns ProgramFiles\Antigravity\Antigravity.exe
        let pf_exe = program_files.join("Antigravity").join("Antigravity.exe");
        std::fs::create_dir_all(pf_exe.parent().unwrap()).expect("mkdir pf");
        std::fs::write(&pf_exe, b"stub").expect("write pf exe");
        assert_eq!(
            discover_windows_antigravity_exe(Some(&local_app_data), Some(&program_files)),
            pf_exe
        );

        // Both installed -> prefers %LOCALAPPDATA%\Programs\Antigravity\Antigravity.exe
        let local_exe = local_app_data
            .join("Programs")
            .join("Antigravity")
            .join("Antigravity.exe");
        std::fs::create_dir_all(local_exe.parent().unwrap()).expect("mkdir local");
        std::fs::write(&local_exe, b"stub").expect("write local exe");
        assert_eq!(
            discover_windows_antigravity_exe(Some(&local_app_data), Some(&program_files)),
            local_exe
        );

        // Empty, whitespace, relative, and traversal env values must fall back to "antigravity".
        for bad in [
            Path::new(""),
            Path::new("   "),
            Path::new("."),
            Path::new("../escape"),
        ] {
            assert_eq!(
                discover_windows_antigravity_exe(Some(bad), Some(bad)),
                PathBuf::from("antigravity"),
                "invalid Windows base dir {:?} must be rejected",
                bad
            );
        }

        // Symlinked Antigravity.exe must be rejected (no symlink following).
        #[cfg(unix)]
        {
            let sym_local = tmp.path().join("SymLocal");
            let sym_pf = tmp.path().join("SymPf");
            let decoy_target = tmp.path().join("decoy.exe");
            std::fs::write(&decoy_target, b"decoy").expect("write decoy");

            let sym_local_exe = sym_local
                .join("Programs")
                .join("Antigravity")
                .join("Antigravity.exe");
            std::fs::create_dir_all(sym_local_exe.parent().unwrap()).expect("mkdir sym local");
            std::os::unix::fs::symlink(&decoy_target, &sym_local_exe).expect("symlink local");

            let sym_pf_exe = sym_pf.join("Antigravity").join("Antigravity.exe");
            std::fs::create_dir_all(sym_pf_exe.parent().unwrap()).expect("mkdir sym pf");
            std::os::unix::fs::symlink(&decoy_target, &sym_pf_exe).expect("symlink pf");

            assert_eq!(
                discover_windows_antigravity_exe(Some(&sym_local), Some(&sym_pf)),
                PathBuf::from("antigravity"),
                "symlinked Antigravity.exe must be rejected"
            );
        }
    }
}
