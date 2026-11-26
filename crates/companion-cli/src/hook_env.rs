use std::fs;
use std::io::Write;
use std::path::PathBuf;
use std::time::Duration;

use companion_providers::LoopbackProxyClient;

use crate::daemon::{
    DEFAULT_IDLE_SHUTDOWN_SECS, WaitHealthyOptions, WaitHealthyOutcome,
    spawn_detached_proxy_daemon, wait_for_daemon_healthy,
};
use crate::{CliError, load_config_checked, resolve_cli_config_path};

pub const HOOK_ENV_PROBE_TIMEOUT: Duration = Duration::from_millis(250);
pub const HOOK_ENV_STARTUP_TIMEOUT: Duration = Duration::from_millis(2000);
pub const HOOK_ENV_POLL_INTERVAL: Duration = Duration::from_millis(50);

#[derive(Debug, Clone, Default)]
pub struct HookEnvOptions {
    pub ensure_running: bool,
    pub print_port: bool,
    pub port_override: Option<u16>,
    pub config_override: Option<PathBuf>,
    pub plugins_dir_override: Option<PathBuf>,
}

/// Fail-open login-shell hook helper: checks plugin installation, probes `/health`,
/// optionally spawns the detached Managed On-Demand Daemon, and prints ONLY `<port>\n`
/// to `out` when healthy and `--print-port` is set. Always returns cleanly so the CLI
/// exits `0` and never breaks a user's login shell.
pub async fn run_hook_env<W: Write, E: Write>(opts: HookEnvOptions, out: &mut W, err_out: &mut E) {
    match try_hook_env(&opts).await {
        Ok(Some(port)) => {
            if opts.print_port {
                let _ = writeln!(out, "{port}");
            }
        }
        Ok(None) => {}
        Err(reason) => {
            let _ = writeln!(err_out, "hook-env: {reason}");
        }
    }
}

async fn try_hook_env(opts: &HookEnvOptions) -> Result<Option<u16>, String> {
    if opts.port_override == Some(0) {
        return Err(CliError::InvalidPort(0).to_string());
    }

    let plugins_dir = match opts.plugins_dir_override.as_deref() {
        Some(dir) => {
            companion_core::validate_path(dir).map_err(|e| e.to_string())?;
            dir.to_path_buf()
        }
        None => companion_core::default_plugins_dir().map_err(|e| e.to_string())?,
    };

    let plugin_json = plugins_dir.join("agy-companion").join("plugin.json");
    // Use fs::metadata so dev-install directory symlinks (`~/.gemini/config/plugins/agy-companion -> ./plugin`)
    // work transparently while missing/dangling symlinks, dirs, and FIFOs return false without blocking.
    if !fs::metadata(&plugin_json).is_ok_and(|meta| meta.is_file()) {
        return Err(format!(
            "plugin not installed at '{}'; skipping",
            plugin_json.display()
        ));
    }

    let config_path =
        resolve_cli_config_path(opts.config_override.as_deref()).map_err(|e| e.to_string())?;
    let mut config = load_config_checked(&config_path).map_err(|e| e.to_string())?;
    if let Some(port) = opts.port_override {
        config.proxy.port =
            std::num::NonZeroU16::new(port).ok_or_else(|| CliError::InvalidPort(0).to_string())?;
    }
    config.validate().map_err(|e| e.to_string())?;
    let port = config.proxy.port.get();

    let probe = LoopbackProxyClient::new(HOOK_ENV_PROBE_TIMEOUT, HOOK_ENV_PROBE_TIMEOUT)
        .map_err(|e| e.to_string())?;

    if probe
        .probe_healthy_companion(port, Some(HOOK_ENV_PROBE_TIMEOUT))
        .await
        .is_some()
    {
        return Ok(Some(port));
    }

    if !opts.ensure_running {
        return Ok(None);
    }

    let mut child = spawn_detached_proxy_daemon(port, &config_path, DEFAULT_IDLE_SHUTDOWN_SECS)
        .map_err(|e| e.to_string())?;

    match wait_for_daemon_healthy(
        &probe,
        &mut child,
        WaitHealthyOptions {
            port,
            total_timeout: HOOK_ENV_STARTUP_TIMEOUT,
            poll_interval: HOOK_ENV_POLL_INTERVAL,
            probe_timeout: HOOK_ENV_PROBE_TIMEOUT,
            expected_version: None,
        },
    )
    .await
    {
        WaitHealthyOutcome::Ready(_) => Ok(Some(port)),
        WaitHealthyOutcome::ExitedEarly { .. } => Err(format!(
            "detached proxy daemon exited before becoming healthy on 127.0.0.1:{port}"
        )),
        WaitHealthyOutcome::TimedOut { .. } => Err(format!(
            "timed out waiting for agy-companion proxy on 127.0.0.1:{port}"
        )),
    }
}

#[cfg(test)]
mod tests {
    use std::time::Instant;

    use super::*;
    use axum::http::StatusCode;
    use axum::routing::get;
    #[cfg(unix)]
    use companion_proxy::HealthResponse;
    use tempfile::TempDir;

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

    fn setup_plugin_dir(tmp: &TempDir) -> PathBuf {
        let plugins_dir = tmp.path().join("plugins");
        let plugin_root = plugins_dir.join("agy-companion");
        fs::create_dir_all(&plugin_root).unwrap();
        fs::write(
            plugin_root.join("plugin.json"),
            r#"{"name":"agy-companion"}"#,
        )
        .unwrap();
        plugins_dir
    }

    #[tokio::test]
    async fn hook_env_skips_cleanly_when_plugin_json_is_missing() {
        let tmp = TempDir::new().unwrap();
        let empty_plugins = tmp.path().join("plugins");
        fs::create_dir_all(&empty_plugins).unwrap();

        let mut out = Vec::new();
        let mut err = Vec::new();
        run_hook_env(
            HookEnvOptions {
                ensure_running: false,
                print_port: true,
                port_override: Some(4141),
                config_override: Some(tmp.path().join("config.json")),
                plugins_dir_override: Some(empty_plugins),
            },
            &mut out,
            &mut err,
        )
        .await;

        assert!(out.is_empty(), "stdout must stay empty when plugin missing");
        let err_text = String::from_utf8(err).unwrap();
        assert!(err_text.contains("plugin not installed"));
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn hook_env_supports_dev_symlink_plugin_dir_and_honors_print_port_flag() {
        use std::os::unix::fs::symlink;

        let tmp = TempDir::new().unwrap();
        let real_plugin = tmp.path().join("repo-plugin");
        fs::create_dir_all(&real_plugin).unwrap();
        fs::write(
            real_plugin.join("plugin.json"),
            r#"{"name":"agy-companion"}"#,
        )
        .unwrap();
        let plugins_dir = tmp.path().join("plugins");
        fs::create_dir_all(&plugins_dir).unwrap();
        symlink(&real_plugin, plugins_dir.join("agy-companion")).unwrap();

        let router = axum::Router::new().route(
            "/health",
            get(|| async {
                axum::Json(HealthResponse {
                    status: "ok".to_string(),
                    version: env!("CARGO_PKG_VERSION").to_string(),
                    protocol_version: 1,
                })
            }),
        );
        let (port, handle) = spawn_mock_router(router).await;

        // With --print-port
        let mut out = Vec::new();
        let mut err = Vec::new();
        run_hook_env(
            HookEnvOptions {
                ensure_running: false,
                print_port: true,
                port_override: Some(port),
                config_override: Some(tmp.path().join("config.json")),
                plugins_dir_override: Some(plugins_dir.clone()),
            },
            &mut out,
            &mut err,
        )
        .await;
        assert_eq!(String::from_utf8(out).unwrap(), format!("{port}\n"));
        assert!(
            err.is_empty(),
            "unexpected stderr: {}",
            String::from_utf8_lossy(&err)
        );

        // Without --print-port -> 0 bytes stdout
        let mut out_silent = Vec::new();
        let mut err_silent = Vec::new();
        run_hook_env(
            HookEnvOptions {
                ensure_running: false,
                print_port: false,
                port_override: Some(port),
                config_override: Some(tmp.path().join("config.json")),
                plugins_dir_override: Some(plugins_dir),
            },
            &mut out_silent,
            &mut err_silent,
        )
        .await;
        assert!(out_silent.is_empty());
        assert!(err_silent.is_empty());

        handle.abort();
    }

    #[tokio::test]
    async fn hook_env_rejects_traversal_and_corrupt_config_without_leaking_secret() {
        let tmp = TempDir::new().unwrap();
        let plugins_dir = setup_plugin_dir(&tmp);

        // Traversal --plugins-dir
        let mut out = Vec::new();
        let mut err = Vec::new();
        run_hook_env(
            HookEnvOptions {
                ensure_running: false,
                print_port: true,
                port_override: Some(4141),
                config_override: Some(tmp.path().join("config.json")),
                plugins_dir_override: Some(PathBuf::from("/tmp/../escape")),
            },
            &mut out,
            &mut err,
        )
        .await;
        assert!(out.is_empty());
        assert!(!err.is_empty());

        // Corrupt config containing a synthetic secret must never echo secret to stderr
        let secret = "super-secret-value-never-log";
        let bad_cfg = tmp.path().join("bad-config.json");
        fs::write(
            &bad_cfg,
            format!(r#"{{"providers":{{"gemini":{{"api_key":"{secret}"}}}},"#),
        )
        .unwrap();
        let mut out2 = Vec::new();
        let mut err2 = Vec::new();
        run_hook_env(
            HookEnvOptions {
                ensure_running: false,
                print_port: true,
                port_override: None,
                config_override: Some(bad_cfg),
                plugins_dir_override: Some(plugins_dir),
            },
            &mut out2,
            &mut err2,
        )
        .await;
        assert!(out2.is_empty());
        let err_str = String::from_utf8(err2).unwrap();
        assert!(!err_str.is_empty());
        assert!(!err_str.contains(secret));
    }

    #[tokio::test]
    async fn hook_env_fails_open_on_slow_or_non_companion_health_endpoint() {
        let tmp = TempDir::new().unwrap();
        let plugins_dir = setup_plugin_dir(&tmp);

        // Slow /health (> 250ms) without --ensure-running
        let slow_router = axum::Router::new().route(
            "/health",
            get(|| async {
                tokio::time::sleep(Duration::from_secs(1)).await;
                StatusCode::OK
            }),
        );
        let (slow_port, slow_handle) = spawn_mock_router(slow_router).await;
        let started = Instant::now();
        let mut out = Vec::new();
        let mut err = Vec::new();
        run_hook_env(
            HookEnvOptions {
                ensure_running: false,
                print_port: true,
                port_override: Some(slow_port),
                config_override: Some(tmp.path().join("config.json")),
                plugins_dir_override: Some(plugins_dir.clone()),
            },
            &mut out,
            &mut err,
        )
        .await;
        assert!(out.is_empty());
        assert!(started.elapsed() < Duration::from_millis(800));
        slow_handle.abort();

        // Non-companion server returning 404
        let foreign_router =
            axum::Router::new().route("/health", get(|| async { StatusCode::NOT_FOUND }));
        let (foreign_port, foreign_handle) = spawn_mock_router(foreign_router).await;
        let mut out2 = Vec::new();
        let mut err2 = Vec::new();
        run_hook_env(
            HookEnvOptions {
                ensure_running: false,
                print_port: true,
                port_override: Some(foreign_port),
                config_override: Some(tmp.path().join("config.json")),
                plugins_dir_override: Some(plugins_dir),
            },
            &mut out2,
            &mut err2,
        )
        .await;
        assert!(out2.is_empty());
        foreign_handle.abort();
    }
}
