use std::net::{Ipv4Addr, TcpListener};
use std::path::Path;
use std::time::{Duration, Instant};

use companion_providers::LoopbackProxyClient;

use super::INSTALL_HEALTH_PROBE_TIMEOUT;
use super::safety::{ExistingDirSafety, deduplicate_paths, inspect_existing_dir};
use crate::CliError;
use crate::daemon::{
    DEFAULT_IDLE_SHUTDOWN_SECS, PROXY_PID_FILENAME, WaitHealthyOptions, WaitHealthyOutcome,
    spawn_detached_proxy_daemon_with_exe, wait_for_daemon_healthy,
};
use crate::normalized_metadata_path;
use crate::process::{PROXY_STOP_POLL_INTERVAL, stop_proxy_from_pid_files};

const PROXY_START_WAIT_TIMEOUT: Duration = Duration::from_secs(5);
const PROXY_START_POLL_INTERVAL: Duration = Duration::from_millis(100);
const PORT_RELEASE_WAIT_TIMEOUT: Duration = Duration::from_secs(3);

pub(super) struct InstallProxyContext<'a> {
    pub probe: &'a LoopbackProxyClient,
    pub stable_bin: &'a Path,
    pub config_path: &'a Path,
    pub data_dir: &'a Path,
    pub runtime_data_dir: &'a Path,
    pub listen_port: u16,
    pub version: &'a str,
}

#[derive(Default)]
pub(super) struct InstallProxyOutcome {
    pub started: bool,
    pub replaced: bool,
    pub version: Option<String>,
}

pub(super) async fn ensure_installed_proxy_running(
    ctx: &InstallProxyContext<'_>,
    warnings: &mut Vec<String>,
) -> Result<InstallProxyOutcome, CliError> {
    let InstallProxyContext {
        probe,
        stable_bin,
        config_path,
        data_dir,
        runtime_data_dir,
        listen_port,
        version,
    } = *ctx;
    let mut outcome = InstallProxyOutcome::default();
    let existing_health = probe
        .probe_healthy_companion(listen_port, Some(INSTALL_HEALTH_PROBE_TIMEOUT))
        .await;

    match existing_health {
        Some(health) if health.version == version => {
            outcome.version = Some(health.version);
        }
        Some(stale_health) => {
            // Version skew: stop verified running companion proxy before starting updated binary.
            let runtime_differs =
                normalized_metadata_path(runtime_data_dir) != normalized_metadata_path(data_dir);
            let runtime_data_dir_safety = if runtime_differs {
                inspect_existing_dir(runtime_data_dir, "runtime data directory", warnings)?
            } else {
                ExistingDirSafety::SafeDir
            };
            let mut pid_paths = Vec::new();
            if runtime_data_dir_safety == ExistingDirSafety::SafeDir {
                pid_paths.push(runtime_data_dir.join(PROXY_PID_FILENAME));
            }
            pid_paths.push(data_dir.join(PROXY_PID_FILENAME));
            let pid_candidates = deduplicate_paths(pid_paths);
            let (stopped, _) = stop_proxy_from_pid_files(
                probe,
                &pid_candidates,
                Some(listen_port),
                false,
                warnings,
            )
            .await;
            if stopped {
                let mut port_released = false;
                let release_deadline = Instant::now() + PORT_RELEASE_WAIT_TIMEOUT;
                while Instant::now() < release_deadline {
                    if let Ok(listener) = TcpListener::bind((Ipv4Addr::LOCALHOST, listen_port)) {
                        drop(listener);
                        port_released = true;
                        break;
                    }
                    tokio::time::sleep(PROXY_STOP_POLL_INTERVAL).await;
                }
                if !port_released {
                    outcome.version = Some(stale_health.version.clone());
                    warnings.push(format!(
                        "port {listen_port} did not clear after stopping stale proxy (version {}); restart Antigravity completely to pick up the new proxy",
                        stale_health.version
                    ));
                } else {
                    let mut child = spawn_detached_proxy_daemon_with_exe(
                        stable_bin,
                        listen_port,
                        config_path,
                        DEFAULT_IDLE_SHUTDOWN_SECS,
                    )?;
                    match wait_for_daemon_healthy(
                        probe,
                        &mut child,
                        WaitHealthyOptions {
                            port: listen_port,
                            total_timeout: PROXY_START_WAIT_TIMEOUT,
                            poll_interval: PROXY_START_POLL_INTERVAL,
                            probe_timeout: INSTALL_HEALTH_PROBE_TIMEOUT,
                            expected_version: Some(version),
                        },
                    )
                    .await
                    {
                        WaitHealthyOutcome::Ready(new_health) => {
                            outcome.started = true;
                            outcome.replaced = true;
                            outcome.version = Some(new_health.version);
                        }
                        WaitHealthyOutcome::ExitedEarly {
                            status,
                            last_observed_version,
                        } => {
                            if let Some(stale_ver) = last_observed_version {
                                outcome.version = Some(stale_ver.clone());
                                warnings.push(format!(
                                    "replacement proxy exited early ({status}) while 127.0.0.1:{listen_port} still reported version {stale_ver} instead of {version}; check port {listen_port} and restart Antigravity completely"
                                ));
                            } else {
                                warnings.push(format!(
                                    "replacement proxy on 127.0.0.1:{listen_port} exited early ({status}) before becoming healthy with version {version}; check port {listen_port} and restart Antigravity completely"
                                ));
                            }
                        }
                        WaitHealthyOutcome::TimedOut {
                            last_observed_version,
                        } => {
                            if let Some(stale_ver) = last_observed_version {
                                outcome.version = Some(stale_ver.clone());
                                warnings.push(format!(
                                    "replacement proxy on 127.0.0.1:{listen_port} still reported version {stale_ver} instead of {version} after {}s; check port {listen_port} and restart Antigravity completely",
                                    PROXY_START_WAIT_TIMEOUT.as_secs()
                                ));
                            } else {
                                warnings.push(format!(
                                    "replacement proxy on 127.0.0.1:{listen_port} did not become healthy with version {version} within {}s; check port {listen_port} and restart Antigravity completely",
                                    PROXY_START_WAIT_TIMEOUT.as_secs()
                                ));
                            }
                        }
                    }
                }
            } else {
                outcome.version = Some(stale_health.version.clone());
                warnings.push(format!(
                    "running proxy on 127.0.0.1:{listen_port} has version {} (installed {version}), but could not be safely stopped via proxy.pid; restart Antigravity completely to pick up the new proxy",
                    stale_health.version
                ));
            }
        }
        None => {
            let mut child = spawn_detached_proxy_daemon_with_exe(
                stable_bin,
                listen_port,
                config_path,
                DEFAULT_IDLE_SHUTDOWN_SECS,
            )?;
            match wait_for_daemon_healthy(
                probe,
                &mut child,
                WaitHealthyOptions {
                    port: listen_port,
                    total_timeout: PROXY_START_WAIT_TIMEOUT,
                    poll_interval: PROXY_START_POLL_INTERVAL,
                    probe_timeout: INSTALL_HEALTH_PROBE_TIMEOUT,
                    expected_version: None,
                },
            )
            .await
            {
                WaitHealthyOutcome::Ready(new_health) => {
                    outcome.started = true;
                    outcome.version = Some(new_health.version);
                }
                WaitHealthyOutcome::ExitedEarly { status, .. } => {
                    warnings.push(format!(
                        "background proxy on 127.0.0.1:{listen_port} exited early ({status}) before becoming healthy"
                    ));
                }
                WaitHealthyOutcome::TimedOut { .. } => {
                    warnings.push(format!(
                        "background proxy on 127.0.0.1:{listen_port} did not become healthy within {}s",
                        PROXY_START_WAIT_TIMEOUT.as_secs()
                    ));
                }
            }
        }
    }

    Ok(outcome)
}
