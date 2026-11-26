use std::fs;
#[cfg(unix)]
use std::io::ErrorKind;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, Instant};

use companion_providers::LoopbackProxyClient;

use crate::daemon::{ProxyPidInfo, read_proxy_pid};
use crate::{BINARY_NAME, CliError};

pub(crate) const FRESH_PORT_PROBE_TIMEOUT: Duration = Duration::from_millis(250);
const PROXY_STOP_TERM_TIMEOUT: Duration = Duration::from_millis(2_000);
const PROXY_STOP_KILL_TIMEOUT: Duration = Duration::from_millis(1_000);
pub(crate) const PROXY_STOP_POLL_INTERVAL: Duration = Duration::from_millis(50);

#[cfg(any(windows, test))]
pub(crate) const CREATE_NO_WINDOW: u32 = 0x0800_0000;

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum PidVerifyResult {
    VerifiedCompanion { cmdline: String },
    AlreadyDead,
    UnrelatedProcess { reason: String },
}

pub(crate) fn extract_exec_basename(raw: &str) -> &str {
    let trimmed = raw.trim().trim_matches('"');
    let base = trimmed.rsplit(['/', '\\']).next().unwrap_or(trimmed);
    if base.len() >= 4 && base.as_bytes()[base.len() - 4..].eq_ignore_ascii_case(b".exe") {
        &base[..base.len() - 4]
    } else {
        base
    }
}

fn is_companion_exec_basename(base: &str) -> bool {
    base.eq_ignore_ascii_case(BINARY_NAME)
        || base.starts_with(&format!(".{BINARY_NAME}.old."))
        || base.starts_with(&format!("{BINARY_NAME}.old."))
}

/// Pure cross-platform Layer 3 process identity predicate.
///
/// Requires BOTH:
/// 1. The executable basename (`argv[0]` / OS executable path) is `agy-companion`
///    (or a Windows rename-aside `.agy-companion.old.*` binary), AND
/// 2. The first command-line argument (`argv[1]`) after the executable is the exact
///    `serve` subcommand (`Cli` defines no top-level global flags before the subcommand).
pub(crate) fn is_companion_serve_process(
    argv0: &str,
    full_cmdline: &str,
    expected_bin: &str,
) -> bool {
    let base = extract_exec_basename(argv0);
    let base_is_companion = is_companion_exec_basename(base)
        || (!expected_bin.is_empty()
            && extract_exec_basename(expected_bin).eq_ignore_ascii_case(BINARY_NAME)
            && base.eq_ignore_ascii_case(extract_exec_basename(expected_bin)));
    if !base_is_companion {
        return false;
    }

    if full_cmdline.contains('\0') {
        let parts: Vec<&str> = full_cmdline.split('\0').collect();
        return parts.first().is_some_and(|p| !p.is_empty()) && parts.get(1) == Some(&"serve");
    }

    let trimmed_cmd = full_cmdline.trim();
    if trimmed_cmd.is_empty() {
        return false;
    }

    let trimmed_argv0 = argv0.trim().trim_matches('"');
    let rest = if let Some(after_open) = trimmed_cmd.strip_prefix('"') {
        let Some(close_idx) = after_open.find('"') else {
            return false;
        };
        let after_close = &after_open[close_idx + 1..];
        if !after_close.is_empty() && !after_close.starts_with(|c: char| c.is_ascii_whitespace()) {
            return false;
        }
        after_close
    } else if !trimmed_argv0.is_empty()
        && trimmed_cmd.len() >= trimmed_argv0.len()
        && trimmed_cmd.is_char_boundary(trimmed_argv0.len())
        && trimmed_cmd[..trimmed_argv0.len()].eq_ignore_ascii_case(trimmed_argv0)
        && (trimmed_cmd.len() == trimmed_argv0.len()
            || trimmed_cmd[trimmed_argv0.len()..].starts_with(|c: char| c.is_ascii_whitespace()))
    {
        &trimmed_cmd[trimmed_argv0.len()..]
    } else {
        match trimmed_cmd.find(|c: char| c.is_ascii_whitespace()) {
            Some(idx) => {
                if !is_companion_exec_basename(extract_exec_basename(&trimmed_cmd[..idx])) {
                    return false;
                }
                &trimmed_cmd[idx..]
            }
            None => return false,
        }
    };

    rest.split_ascii_whitespace().next() == Some("serve")
}

#[cfg(target_os = "linux")]
fn inspect_os_process(pid: u32) -> Result<Option<(String, String)>, CliError> {
    let cmdline_path = format!("/proc/{pid}/cmdline");
    let raw = match fs::read(&cmdline_path) {
        Ok(bytes) => bytes,
        Err(err) if err.kind() == ErrorKind::NotFound => return Ok(None),
        Err(err) => return Err(CliError::Io(err)),
    };
    if raw.is_empty() {
        return Ok(None);
    }
    let parts: Vec<String> = raw
        .split(|b| *b == 0)
        .filter(|slice| !slice.is_empty())
        .map(|slice| String::from_utf8_lossy(slice).into_owned())
        .collect();
    if parts.is_empty() {
        return Ok(None);
    }
    let exe_str = fs::read_link(format!("/proc/{pid}/exe"))
        .ok()
        .map(|p| {
            let s = p.to_string_lossy().into_owned();
            s.strip_suffix(" (deleted)").unwrap_or(&s).to_string()
        })
        .unwrap_or_else(|| parts[0].clone());
    // Always append a trailing NUL so 1-arg Linux cmdlines unconditionally use the exact
    // NUL-delimited branch in is_companion_serve_process instead of whitespace splitting.
    let mut full = parts.join("\0");
    full.push('\0');
    Ok(Some((exe_str, full)))
}

#[cfg(all(unix, not(target_os = "linux")))]
fn run_ps(args: &[&str]) -> Result<std::process::Output, CliError> {
    match Command::new("/bin/ps").args(args).output() {
        Ok(out) => Ok(out),
        Err(err) if err.kind() == ErrorKind::NotFound => {
            Ok(Command::new("ps").args(args).output()?)
        }
        Err(err) => Err(CliError::Io(err)),
    }
}

#[cfg(all(unix, not(target_os = "linux")))]
fn inspect_os_process(pid: u32) -> Result<Option<(String, String)>, CliError> {
    let pid_str = pid.to_string();
    let comm_out = run_ps(&["-ww", "-o", "comm=", "-p", &pid_str])?;
    let comm = String::from_utf8_lossy(&comm_out.stdout).trim().to_string();
    if !comm_out.status.success() || comm.is_empty() {
        return Ok(None);
    }

    let cmd_out = run_ps(&["-ww", "-o", "command=", "-p", &pid_str])?;
    let cmd = String::from_utf8_lossy(&cmd_out.stdout).trim().to_string();
    if !cmd_out.status.success() || cmd.is_empty() {
        return Ok(None);
    }

    Ok(Some((comm, cmd)))
}

#[cfg(windows)]
fn inspect_os_process(pid: u32) -> Result<Option<(String, String)>, CliError> {
    use std::os::windows::process::CommandExt;

    let query = format!(
        "Get-CimInstance Win32_Process -Filter 'ProcessId = {pid}' | Select-Object ExecutablePath,CommandLine | ConvertTo-Json -Compress"
    );
    let output = Command::new("powershell.exe")
        .stdin(std::process::Stdio::null())
        .args([
            "-NoProfile",
            "-NonInteractive",
            "-InputFormat",
            "None",
            "-WindowStyle",
            "Hidden",
            "-Command",
            &query,
        ])
        .creation_flags(CREATE_NO_WINDOW)
        .output()?;

    if !output.status.success() {
        return Ok(None);
    }
    let stdout = String::from_utf8_lossy(&output.stdout).trim().to_string();
    if stdout.is_empty() {
        return Ok(None);
    }
    let parsed: serde_json::Value = match serde_json::from_str(&stdout) {
        Ok(v) => v,
        Err(_) => return Ok(None),
    };
    let exe = parsed
        .get("ExecutablePath")
        .and_then(|v| v.as_str())
        .unwrap_or_default()
        .to_string();
    let cmd = parsed
        .get("CommandLine")
        .and_then(|v| v.as_str())
        .unwrap_or_default()
        .to_string();
    if exe.is_empty() && cmd.is_empty() {
        return Ok(None);
    }
    Ok(Some((exe, cmd)))
}

pub(crate) fn verify_proxy_pid_process(info: &ProxyPidInfo) -> Result<PidVerifyResult, CliError> {
    // Layer 1: strict PID sanity guard (never signal PID 0, PID 1, overflow, or self).
    if info.pid <= 1 || info.pid > i32::MAX as u32 || info.pid == std::process::id() {
        return Ok(PidVerifyResult::UnrelatedProcess {
            reason: format!("PID {} failed sanity checks", info.pid),
        });
    }

    // Layer 3: OS process identity check (pure safe std::fs / std::process::Command).
    let Some((argv0, full_cmdline)) = inspect_os_process(info.pid)? else {
        return Ok(PidVerifyResult::AlreadyDead);
    };

    if is_companion_serve_process(&argv0, &full_cmdline, &info.binary_path) {
        Ok(PidVerifyResult::VerifiedCompanion {
            cmdline: full_cmdline,
        })
    } else {
        Ok(PidVerifyResult::UnrelatedProcess {
            reason: format!(
                "PID {} is '{}' (not agy-companion serve)",
                info.pid,
                extract_exec_basename(&argv0)
            ),
        })
    }
}

#[cfg(unix)]
fn signal_process_stop(pid: u32, force: bool) -> Result<(), CliError> {
    let signal_flag = if force { "-KILL" } else { "-TERM" };
    let pid_str = pid.to_string();
    let status = match Command::new("/bin/kill")
        .args([signal_flag, &pid_str])
        .output()
    {
        Ok(out) => out.status,
        Err(err) if err.kind() == ErrorKind::NotFound => {
            Command::new("kill")
                .args([signal_flag, &pid_str])
                .output()?
                .status
        }
        Err(err) => return Err(CliError::Io(err)),
    };
    let _ = status;
    Ok(())
}

#[cfg(windows)]
fn signal_process_stop(pid: u32, force: bool) -> Result<(), CliError> {
    use std::os::windows::process::CommandExt;

    let pid_str = pid.to_string();
    let mut cmd = Command::new("taskkill");
    if force {
        cmd.arg("/F");
    }
    let _ = cmd
        .args(["/PID", &pid_str])
        .creation_flags(CREATE_NO_WINDOW)
        .output();
    Ok(())
}

async fn wait_until_companion_stopped(
    info: &ProxyPidInfo,
    timeout: Duration,
) -> Result<bool, CliError> {
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        tokio::time::sleep(PROXY_STOP_POLL_INTERVAL).await;
        if !matches!(
            verify_proxy_pid_process(info)?,
            PidVerifyResult::VerifiedCompanion { .. }
        ) {
            return Ok(true);
        }
    }
    Ok(!matches!(
        verify_proxy_pid_process(info)?,
        PidVerifyResult::VerifiedCompanion { .. }
    ))
}

async fn stop_verified_companion_pid(
    probe: &LoopbackProxyClient,
    info: &ProxyPidInfo,
) -> Result<bool, CliError> {
    // Layer 2: optional corroborating /health probe (never sufficient alone without Layer 3).
    let _ = probe
        .probe_healthy_companion(info.port, Some(FRESH_PORT_PROBE_TIMEOUT))
        .await;

    // Layer 3 check before sending any signal.
    match verify_proxy_pid_process(info)? {
        PidVerifyResult::VerifiedCompanion { .. } => {}
        PidVerifyResult::AlreadyDead => return Ok(true),
        PidVerifyResult::UnrelatedProcess { .. } => return Ok(false),
    }

    // Layer 4: graceful termination -> bounded poll -> re-verified force termination.
    signal_process_stop(info.pid, false)?;
    if wait_until_companion_stopped(info, PROXY_STOP_TERM_TIMEOUT).await? {
        return Ok(true);
    }
    // Re-verify immediately before escalating to forced termination.
    if matches!(
        verify_proxy_pid_process(info)?,
        PidVerifyResult::VerifiedCompanion { .. }
    ) {
        signal_process_stop(info.pid, true)?;
        return wait_until_companion_stopped(info, PROXY_STOP_KILL_TIMEOUT).await;
    }
    Ok(true)
}

pub(crate) fn remove_pid_file_if_matches(pid_path: &Path, expected_pid: u32) {
    if matches!(read_proxy_pid(pid_path), Ok(Some(cur)) if cur.pid == expected_pid) {
        let _ = fs::remove_file(pid_path);
    }
}

pub(crate) async fn stop_proxy_from_pid_files(
    probe: &LoopbackProxyClient,
    pid_paths: &[PathBuf],
    expected_port: Option<u16>,
    dry_run: bool,
    warnings: &mut Vec<String>,
) -> (bool, Option<u32>) {
    let mut stopped_any = false;
    let mut stopped_pid = None;

    for pid_path in pid_paths {
        let info = match read_proxy_pid(pid_path) {
            Ok(Some(info)) => info,
            Ok(None) => continue,
            Err(err) => {
                warnings.push(format!(
                    "skipping unreadable proxy PID file '{}': {err}",
                    pid_path.display()
                ));
                continue;
            }
        };

        if let Some(target_port) = expected_port
            && info.port != target_port
        {
            warnings.push(format!(
                "refusing to signal PID {} from '{}': proxy.pid port {} does not match target listen_port {target_port}",
                info.pid,
                pid_path.display(),
                info.port
            ));
            continue;
        }

        match verify_proxy_pid_process(&info) {
            Ok(PidVerifyResult::VerifiedCompanion { .. }) => {
                if dry_run {
                    stopped_any = true;
                    stopped_pid = Some(info.pid);
                } else {
                    match stop_verified_companion_pid(probe, &info).await {
                        Ok(true) => {
                            stopped_any = true;
                            stopped_pid = Some(info.pid);
                            remove_pid_file_if_matches(pid_path, info.pid);
                        }
                        Ok(false) => {
                            warnings.push(format!(
                                "proxy PID {} from '{}' did not exit cleanly",
                                info.pid,
                                pid_path.display()
                            ));
                        }
                        Err(err) => {
                            warnings.push(format!(
                                "failed stopping proxy PID {} from '{}': {err}",
                                info.pid,
                                pid_path.display()
                            ));
                        }
                    }
                }
            }
            Ok(PidVerifyResult::AlreadyDead) => {
                if !dry_run {
                    remove_pid_file_if_matches(pid_path, info.pid);
                }
            }
            Ok(PidVerifyResult::UnrelatedProcess { reason }) => {
                warnings.push(format!(
                    "refusing to signal PID {} from '{}': {reason}",
                    info.pid,
                    pid_path.display()
                ));
            }
            Err(err) => {
                warnings.push(format!(
                    "failed inspecting PID {} from '{}': {err}",
                    info.pid,
                    pid_path.display()
                ));
            }
        }
    }

    (stopped_any, stopped_pid)
}

pub(crate) fn is_binary_on_path(binary_name: &str) -> bool {
    let Some(path_var) = std::env::var_os("PATH") else {
        return false;
    };
    for dir in std::env::split_paths(&path_var) {
        let candidate = dir.join(binary_name);
        if let Ok(meta) = fs::metadata(&candidate)
            && meta.is_file()
        {
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                if meta.permissions().mode() & 0o111 != 0 {
                    return true;
                }
            }
            #[cfg(not(unix))]
            {
                return true;
            }
        }
    }
    false
}

pub(crate) fn detect_antigravity_running() -> bool {
    #[cfg(unix)]
    {
        let output = match Command::new("/bin/ps").args(["-axo", "comm="]).output() {
            Ok(out) => out,
            Err(_) => match Command::new("ps").args(["-axo", "comm="]).output() {
                Ok(out) => out,
                Err(_) => return false,
            },
        };
        if !output.status.success() {
            return false;
        }
        let stdout = String::from_utf8_lossy(&output.stdout);
        stdout.lines().any(|line| {
            let base = extract_exec_basename(line);
            base == "Antigravity" || base == "antigravity"
        })
    }
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;

        let Ok(output) = Command::new("tasklist")
            .args(["/FI", "IMAGENAME eq Antigravity.exe", "/NH"])
            .creation_flags(CREATE_NO_WINDOW)
            .output()
        else {
            return false;
        };
        if !output.status.success() {
            return false;
        }
        String::from_utf8_lossy(&output.stdout)
            .to_ascii_lowercase()
            .contains("antigravity.exe")
    }
}
