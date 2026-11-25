use std::io::ErrorKind;
use std::num::NonZeroU16;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use companion_core::{SqliteUsageSink, UsageSink};
use companion_providers::LoopbackProxyClient;
use companion_proxy::{ProxyError, ProxyState, ProxyStateOptions};

use crate::daemon::{
    MAX_PROXY_LOG_BYTES, PROXY_LOG_FILENAME, PROXY_PID_FILENAME, ProxyPidInfo, RotatingFileWriter,
    ServeLogTarget, resolve_runtime_data_dir, write_proxy_pid,
};
use crate::{CliError, load_config_checked, precheck_existing_data_dir, resolve_cli_config_path};

pub const ADOPT_RUNNING_POLL_INTERVAL: Duration = Duration::from_millis(500);

#[derive(Default)]
pub struct ServeOptions {
    pub port_override: Option<u16>,
    pub config_override: Option<PathBuf>,
    pub adopt_running: bool,
    pub auto_enable_launcher: bool,
    pub idle_shutdown_secs: Option<u64>,
    pub log_target: Option<ServeLogTarget>,
}

pub async fn run_serve(
    port_override: Option<u16>,
    config_override: Option<PathBuf>,
) -> Result<(), CliError> {
    run_serve_with_options(port_override, config_override, false).await
}

pub async fn run_serve_with_options(
    port_override: Option<u16>,
    config_override: Option<PathBuf>,
    adopt_running: bool,
) -> Result<(), CliError> {
    run_serve_configured(ServeOptions {
        port_override,
        config_override,
        adopt_running,
        auto_enable_launcher: false,
        idle_shutdown_secs: None,
        log_target: None,
    })
    .await
}

pub async fn run_serve_configured(options: ServeOptions) -> Result<(), CliError> {
    if options.port_override == Some(0) {
        return Err(CliError::InvalidPort(0));
    }

    let config_path = resolve_cli_config_path(options.config_override.as_deref())?;
    let mut config = load_config_checked(&config_path)?;
    if let Some(port) = options.port_override {
        config.proxy.port = NonZeroU16::new(port).ok_or(CliError::InvalidPort(0))?;
    }
    config.validate()?;

    let usage_dir = config.effective_usage_dir()?;
    precheck_existing_data_dir(&usage_dir)?;
    let runtime_data_dir = resolve_runtime_data_dir(&config)?;
    precheck_existing_data_dir(&runtime_data_dir)?;

    if options.auto_enable_launcher {
        let auto_cfg_path = config_path.clone();
        match tokio::task::spawn_blocking(move || {
            companion_core::integration::ensure_first_run_launcher_enabled(
                &auto_cfg_path,
                &companion_core::integration::IntegrationOptions::default(),
            )
        })
        .await
        {
            Ok(Ok(outcome)) => {
                tracing::info!(?outcome, "first-run launcher auto-enable completed");
            }
            Ok(Err(err)) => {
                tracing::warn!(
                    error = %err,
                    "first-run launcher auto-enable failed; continuing proxy startup"
                );
            }
            Err(err) => {
                tracing::warn!(
                    error = %err,
                    "first-run launcher auto-enable task panicked; continuing proxy startup"
                );
            }
        }
    }

    let usage_sink: Arc<dyn UsageSink> = Arc::new(SqliteUsageSink::new(usage_dir)?);

    // Pre-register SIGTERM and SIGHUP before binding so host-restart signals are never missed.
    #[cfg(unix)]
    let mut sigterm =
        tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()).ok();
    #[cfg(unix)]
    let mut sighup = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::hangup()).ok();

    let mut signal_shutdown_fut: std::pin::Pin<
        Box<dyn std::future::Future<Output = ()> + Send + 'static>,
    > = Box::pin(async move {
        let ctrl_c = async {
            let _ = tokio::signal::ctrl_c().await;
        };

        #[cfg(unix)]
        let terminate = async {
            if let Some(ref mut stream) = sigterm {
                let _ = stream.recv().await;
            } else {
                std::future::pending::<()>().await;
            }
        };

        #[cfg(not(unix))]
        let terminate = std::future::pending::<()>();

        #[cfg(unix)]
        let hangup = async {
            if let Some(ref mut stream) = sighup {
                let _ = stream.recv().await;
            } else {
                std::future::pending::<()>().await;
            }
        };

        #[cfg(not(unix))]
        let hangup = std::future::pending::<()>();

        tokio::select! {
            _ = ctrl_c => {
                tracing::info!("received Ctrl-C, shutting down proxy");
            }
            _ = terminate => {
                tracing::info!("received SIGTERM, shutting down proxy");
            }
            _ = hangup => {
                tracing::info!("received SIGHUP, shutting down proxy");
            }
        }
    });

    let adopt_probe = if options.adopt_running {
        Some(LoopbackProxyClient::with_default_timeouts()?)
    } else {
        None
    };
    let mut logged_adopt = false;
    let mut takeover_retry_used = false;
    let listener = loop {
        match companion_proxy::bind_loopback_listener(&config).await {
            Ok(listener) => break listener,
            Err(ProxyError::Bind { addr, source })
                if options.adopt_running && source.kind() == ErrorKind::AddrInUse =>
            {
                let probe = adopt_probe
                    .as_ref()
                    .expect("adopt_running guarantees LoopbackProxyClient");
                if let Some(health) = probe
                    .probe_healthy_companion(config.proxy.port.get(), None)
                    .await
                {
                    takeover_retry_used = false;
                    if !logged_adopt {
                        tracing::info!(
                            addr = %addr,
                            running_version = %health.version,
                            "detected running agy-companion proxy; monitoring until shutdown or takeover"
                        );
                        logged_adopt = true;
                    }
                    tokio::select! {
                        _ = &mut signal_shutdown_fut => {
                            return Ok(());
                        }
                        _ = tokio::time::sleep(ADOPT_RUNNING_POLL_INTERVAL) => {}
                    }
                    continue;
                }
                if logged_adopt && !takeover_retry_used {
                    takeover_retry_used = true;
                    tokio::time::sleep(Duration::from_millis(50)).await;
                    continue;
                }
                return Err(CliError::Proxy(ProxyError::Bind { addr, source }));
            }
            Err(err) => return Err(CliError::Proxy(err)),
        }
    };

    let bound_addr = listener.local_addr().map_err(ProxyError::Serve)?;
    let log_writer = RotatingFileWriter::open(
        runtime_data_dir.join(PROXY_LOG_FILENAME),
        MAX_PROXY_LOG_BYTES,
    )?;
    let _log_guard = options
        .log_target
        .as_ref()
        .map(|target| target.attach(log_writer));
    let _pid_guard = write_proxy_pid(
        &runtime_data_dir.join(PROXY_PID_FILENAME),
        &ProxyPidInfo::for_current_process(bound_addr.port()),
    )?;
    let state = ProxyState::from_config_with_options(
        config,
        usage_sink,
        ProxyStateOptions {
            config_path: Some(config_path),
            ..Default::default()
        },
    )?;

    tracing::info!(
        addr = %bound_addr,
        version = env!("CARGO_PKG_VERSION"),
        "agy-companion proxy listening"
    );

    let idle_timeout = options
        .idle_shutdown_secs
        .filter(|&secs| secs > 0)
        .map(Duration::from_secs);
    let idle_state = state.clone();
    let graceful_shutdown = async move {
        match idle_timeout {
            None => signal_shutdown_fut.await,
            Some(timeout) => {
                let poll_interval =
                    (timeout / 4).clamp(Duration::from_millis(50), Duration::from_secs(1));
                let idle_fut = async move {
                    loop {
                        if idle_state.in_flight_requests() == 0
                            && idle_state.idle_duration() >= timeout
                        {
                            tracing::info!(
                                idle_secs = timeout.as_secs(),
                                "proxy idle timeout reached with zero in-flight requests; shutting down"
                            );
                            return;
                        }
                        tokio::time::sleep(poll_interval).await;
                    }
                };
                tokio::select! {
                    _ = signal_shutdown_fut => {}
                    _ = idle_fut => {}
                }
            }
        }
    };

    companion_proxy::serve_on_listener(listener, state, graceful_shutdown).await?;
    Ok(())
}
