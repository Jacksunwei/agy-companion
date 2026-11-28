use std::net::{Ipv4Addr, TcpListener};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant, UNIX_EPOCH};

use companion_core::fs_safe::inspect_regular_file_if_exists;
use companion_providers::LoopbackProxyClient;

use super::INSTALL_HEALTH_PROBE_TIMEOUT;
use super::bundle::is_same_file_path;
use super::safety::{ExistingDirSafety, deduplicate_paths, inspect_existing_dir};
use crate::CliError;
use crate::daemon::{
    DEFAULT_IDLE_SHUTDOWN_SECS, PROXY_PID_FILENAME, WaitHealthyOptions, WaitHealthyOutcome,
    read_proxy_pid, spawn_detached_proxy_daemon_with_exe, wait_for_daemon_healthy,
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
    pub binary_changed: bool,
}

#[derive(Debug, Default, PartialEq, Eq)]
pub(super) struct InstallProxyOutcome {
    pub started: bool,
    pub replaced: bool,
    pub version: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum ProxyReplaceReason {
    VersionMismatch,
    BinaryUpdated,
    BinaryPathChanged,
    BinaryNewerThanProxy,
}

impl ProxyReplaceReason {
    fn same_version_label(self) -> &'static str {
        match self {
            Self::VersionMismatch => "version mismatch",
            Self::BinaryUpdated => "updated binary",
            Self::BinaryPathChanged => "relocated binary path",
            Self::BinaryNewerThanProxy => "newer installed binary",
        }
    }
}

fn stable_bin_mtime_ms(stable_bin: &Path) -> Option<u64> {
    let meta = inspect_regular_file_if_exists(stable_bin, None)
        .ok()
        .flatten()?;
    let modified = meta.modified().ok()?;
    let duration = modified.duration_since(UNIX_EPOCH).ok()?;
    Some(u64::try_from(duration.as_millis()).unwrap_or(u64::MAX))
}

/// Decides whether an already-healthy companion proxy on `listen_port` needs replacement.
///
/// Replaces when ANY of the following holds:
/// 1. Running `/health` version differs from `installed_version`.
/// 2. `stable_bin` bytes changed during this install (`binary_changed`).
/// 3. A readable matching-port `proxy.pid` records a non-empty `binary_path` that does not
///    resolve to `stable_bin` (e.g. legacy `<repo>/target/debug/agy-companion`).
/// 4. A readable matching-port `proxy.pid` records a non-zero `started_at_ms` strictly older
///    than `stable_bin`'s modification time (e.g. after a prior `--no-start-proxy` deploy).
pub(super) fn classify_proxy_replacement(
    existing_version: &str,
    installed_version: &str,
    binary_changed: bool,
    stable_bin: &Path,
    listen_port: u16,
    pid_candidates: &[PathBuf],
) -> Option<ProxyReplaceReason> {
    if existing_version != installed_version {
        return Some(ProxyReplaceReason::VersionMismatch);
    }
    if binary_changed {
        return Some(ProxyReplaceReason::BinaryUpdated);
    }

    for pid_path in pid_candidates {
        let Ok(Some(info)) = read_proxy_pid(pid_path) else {
            continue;
        };
        if info.port != listen_port {
            continue;
        }
        if !info.binary_path.is_empty()
            && !is_same_file_path(Path::new(&info.binary_path), stable_bin)
        {
            return Some(ProxyReplaceReason::BinaryPathChanged);
        }
        if info.started_at_ms > 0
            && stable_bin_mtime_ms(stable_bin).is_some_and(|mtime_ms| mtime_ms > info.started_at_ms)
        {
            return Some(ProxyReplaceReason::BinaryNewerThanProxy);
        }
        break;
    }

    None
}

/// Verifies that a newly spawned replacement child actually claimed `listen_port` by
/// confirming the child is still alive and `runtime_pid_path` records its PID on `listen_port`.
///
/// Prevents a concurrent same-version proxy (such as an older recovery bootstrap daemon
/// spawned during the port-release gap) from producing a false-positive `proxy_replaced = true`.
pub(super) fn verify_same_version_replacement_owner(
    child: &mut std::process::Child,
    runtime_pid_path: &Path,
    listen_port: u16,
) -> bool {
    let expected_pid = child.id();
    if !matches!(child.try_wait(), Ok(None)) {
        return false;
    }
    matches!(
        read_proxy_pid(runtime_pid_path),
        Ok(Some(info)) if info.pid == expected_pid && info.port == listen_port
    )
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
        binary_changed,
    } = *ctx;
    let mut outcome = InstallProxyOutcome::default();
    let existing_health = probe
        .probe_healthy_companion(listen_port, Some(INSTALL_HEALTH_PROBE_TIMEOUT))
        .await;

    match existing_health {
        Some(existing_health) => {
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

            let Some(replace_reason) = classify_proxy_replacement(
                &existing_health.version,
                version,
                binary_changed,
                stable_bin,
                listen_port,
                &pid_candidates,
            ) else {
                outcome.version = Some(existing_health.version);
                return Ok(outcome);
            };

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
                    outcome.version = Some(existing_health.version.clone());
                    if replace_reason == ProxyReplaceReason::VersionMismatch {
                        warnings.push(format!(
                            "port {listen_port} did not clear after stopping stale proxy (version {}); restart Antigravity completely to pick up the new proxy",
                            existing_health.version
                        ));
                    } else {
                        warnings.push(format!(
                            "port {listen_port} did not clear after stopping running proxy ({} at version {version}); restart Antigravity completely to pick up the updated binary",
                            replace_reason.same_version_label()
                        ));
                    }
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
                            let runtime_pid_safe =
                                if runtime_data_dir_safety == ExistingDirSafety::NotFound {
                                    inspect_existing_dir(
                                        runtime_data_dir,
                                        "runtime data directory",
                                        warnings,
                                    )? == ExistingDirSafety::SafeDir
                                } else {
                                    runtime_data_dir_safety == ExistingDirSafety::SafeDir
                                };
                            let verified_owner = replace_reason
                                == ProxyReplaceReason::VersionMismatch
                                || (runtime_pid_safe
                                    && verify_same_version_replacement_owner(
                                        &mut child,
                                        &runtime_data_dir.join(PROXY_PID_FILENAME),
                                        listen_port,
                                    ));
                            if verified_owner {
                                outcome.started = true;
                                outcome.replaced = true;
                                outcome.version = Some(new_health.version);
                            } else {
                                outcome.version = Some(new_health.version);
                                warnings.push(format!(
                                    "replacement proxy (pid {}) did not claim 127.0.0.1:{listen_port}; another version {version} proxy took over the port — restart Antigravity completely to pick up the updated binary",
                                    child.id()
                                ));
                            }
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
                outcome.version = Some(existing_health.version.clone());
                if replace_reason == ProxyReplaceReason::VersionMismatch {
                    warnings.push(format!(
                        "running proxy on 127.0.0.1:{listen_port} has version {} (installed {version}), but could not be safely stopped via proxy.pid; restart Antigravity completely to pick up the new proxy",
                        existing_health.version
                    ));
                } else {
                    warnings.push(format!(
                        "running proxy on 127.0.0.1:{listen_port} ({} at version {version}) could not be safely stopped via proxy.pid; restart Antigravity completely to pick up the updated binary",
                        replace_reason.same_version_label()
                    ));
                }
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
