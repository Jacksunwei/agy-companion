#[cfg(any(unix, test))]
use std::collections::HashSet;
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
    if let Ok(stat_raw) = fs::read_to_string(format!("/proc/{pid}/stat"))
        && let Some((state, _ppid, _starttime)) =
            parse_linux_proc_stat_state_ppid_starttime(&stat_raw)
        && matches!(state, 'Z' | 'X' | 'x')
    {
        return Ok(None);
    }

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
    let comm_out = run_ps(&["-ww", "-o", "state=,comm=", "-p", &pid_str])?;
    if !comm_out.status.success() {
        return Ok(None);
    }
    let comm_stdout = String::from_utf8_lossy(&comm_out.stdout);
    let Some(comm_line) = comm_stdout.lines().find(|l| !l.trim().is_empty()) else {
        return Ok(None);
    };
    let Some((comm_state, comm)) = parse_unix_ps_state_lstart(comm_line) else {
        return Ok(None);
    };
    if is_unix_ps_zombie_or_exiting_state(comm_state) {
        return Ok(None);
    }
    let comm = comm.to_string();

    let cmd_out = run_ps(&["-ww", "-o", "state=,command=", "-p", &pid_str])?;
    if !cmd_out.status.success() {
        return Ok(None);
    }
    let cmd_stdout = String::from_utf8_lossy(&cmd_out.stdout);
    let Some(cmd_line) = cmd_stdout.lines().find(|l| !l.trim().is_empty()) else {
        return Ok(None);
    };
    let Some((cmd_state, cmd)) = parse_unix_ps_state_lstart(cmd_line) else {
        return Ok(None);
    };
    if is_unix_ps_zombie_or_exiting_state(cmd_state) {
        return Ok(None);
    }

    Ok(Some((comm, cmd.to_string())))
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

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum GracefulStopOutcome {
    Signaled,
    AlreadyDead,
    UnrelatedProcess { reason: String },
}

/// Verifies that `info` points to a live `agy-companion serve` process using 4-layer OS
/// identity verification, then sends a single non-blocking graceful stop signal (`SIGTERM`
/// on Unix) without ever escalating to `SIGKILL`.
pub(crate) fn signal_verified_companion_graceful_stop(
    info: &ProxyPidInfo,
) -> Result<GracefulStopOutcome, CliError> {
    match verify_proxy_pid_process(info)? {
        PidVerifyResult::VerifiedCompanion { .. } => {
            signal_process_stop(info.pid, false)?;
            Ok(GracefulStopOutcome::Signaled)
        }
        PidVerifyResult::AlreadyDead => Ok(GracefulStopOutcome::AlreadyDead),
        PidVerifyResult::UnrelatedProcess { reason } => {
            Ok(GracefulStopOutcome::UnrelatedProcess { reason })
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct WatchedProcessIdentity {
    pub pid: u32,
    pub start_token: Option<String>,
}

#[cfg(any(unix, test))]
pub(crate) const MAX_ANCESTOR_HOPS: usize = 6;

#[cfg(any(unix, test))]
pub(crate) fn is_antigravity_main_basename(raw: &str) -> bool {
    extract_exec_basename(raw).eq_ignore_ascii_case("antigravity")
}

#[cfg(any(target_os = "linux", test))]
pub(crate) fn parse_linux_proc_stat_state_ppid_starttime(raw: &str) -> Option<(char, u32, &str)> {
    let close_paren = raw.rfind(')')?;
    let after = raw.get(close_paren + 1..)?.trim_start();
    let mut fields = after.split_ascii_whitespace();
    let state = fields.next()?.chars().next()?;
    let ppid: u32 = fields.next()?.parse().ok()?;
    // Field 3 is `state` (index 0), field 4 is `ppid` (index 1), field 22 is `starttime` (index 19).
    // After consuming 2 fields, `starttime` is at nth(17).
    let starttime = fields.nth(17)?;
    if starttime.is_empty() {
        return None;
    }
    Some((state, ppid, starttime))
}

#[cfg(any(target_os = "linux", test))]
pub(crate) fn parse_linux_proc_stat_comm(raw: &str) -> Option<&str> {
    let open = raw.find('(')?;
    let close = raw.rfind(')')?;
    if close <= open {
        return None;
    }
    Some(&raw[open + 1..close])
}

#[cfg(any(all(unix, not(target_os = "linux")), test))]
pub(crate) fn is_unix_ps_zombie_or_exiting_state(state: &str) -> bool {
    let trimmed = state.trim();
    trimmed.starts_with('Z') || trimmed.starts_with('X')
}

#[cfg(any(all(unix, not(target_os = "linux")), test))]
pub(crate) fn parse_unix_ps_state_lstart(line: &str) -> Option<(&str, &str)> {
    let trimmed = line.trim();
    if trimmed.is_empty() {
        return None;
    }
    let split_idx = trimmed.find(|c: char| c.is_ascii_whitespace())?;
    let state = &trimmed[..split_idx];
    let lstart = trimmed[split_idx..].trim();
    if state.is_empty() || lstart.is_empty() {
        return None;
    }
    Some((state, lstart))
}

#[cfg(any(all(unix, not(target_os = "linux")), test))]
pub(crate) fn parse_unix_ps_ppid_comm(line: &str) -> Option<(u32, String)> {
    let trimmed = line.trim();
    if trimmed.is_empty() {
        return None;
    }
    let split_idx = trimmed.find(|c: char| c.is_ascii_whitespace())?;
    let ppid: u32 = trimmed[..split_idx].parse().ok()?;
    let comm = trimmed[split_idx..].trim();
    if comm.is_empty() {
        return None;
    }
    Some((ppid, comm.to_string()))
}

#[cfg(any(windows, test))]
pub(crate) fn parse_windows_tasklist_csv_matches_pid(stdout: &str, pid: u32) -> bool {
    let expected = pid.to_string();
    stdout.lines().any(|line| {
        let trimmed = line.trim();
        if !trimmed.starts_with('"') {
            return false;
        }
        let mut fields = trimmed.split("\",\"");
        let _image = fields.next();
        fields
            .next()
            .map(|f| f.trim_matches('"').trim())
            .is_some_and(|field| field == expected)
    })
}

pub(crate) fn snapshot_watched_process(pid: u32) -> Option<WatchedProcessIdentity> {
    if pid <= 1 || pid > i32::MAX as u32 {
        return None;
    }

    #[cfg(target_os = "linux")]
    {
        let raw = fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
        let (state, _ppid, starttime) = parse_linux_proc_stat_state_ppid_starttime(&raw)?;
        if matches!(state, 'Z' | 'X' | 'x') {
            return None;
        }
        Some(WatchedProcessIdentity {
            pid,
            start_token: Some(starttime.to_string()),
        })
    }

    #[cfg(all(unix, not(target_os = "linux")))]
    {
        let pid_str = pid.to_string();
        let out = run_ps(&["-o", "state=,lstart=", "-p", &pid_str]).ok()?;
        if !out.status.success() {
            return None;
        }
        let stdout = String::from_utf8_lossy(&out.stdout);
        let first_line = stdout.lines().find(|l| !l.trim().is_empty())?;
        let (state, lstart) = parse_unix_ps_state_lstart(first_line)?;
        if is_unix_ps_zombie_or_exiting_state(state) {
            return None;
        }
        Some(WatchedProcessIdentity {
            pid,
            start_token: Some(lstart.to_string()),
        })
    }

    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;

        let filter = format!("PID eq {pid}");
        let out = Command::new("tasklist")
            .args(["/FI", &filter, "/FO", "CSV", "/NH"])
            .creation_flags(CREATE_NO_WINDOW)
            .output()
            .ok()?;
        if !out.status.success() {
            return None;
        }
        let stdout = String::from_utf8_lossy(&out.stdout);
        if parse_windows_tasklist_csv_matches_pid(&stdout, pid) {
            Some(WatchedProcessIdentity {
                pid,
                start_token: None,
            })
        } else {
            None
        }
    }
}

pub(crate) fn is_watched_process_alive(identity: &WatchedProcessIdentity) -> bool {
    let Some(current) = snapshot_watched_process(identity.pid) else {
        return false;
    };
    match (&identity.start_token, &current.start_token) {
        (Some(expected), Some(actual)) => expected == actual,
        _ => true,
    }
}

#[cfg(any(unix, test))]
pub(crate) fn find_matching_ancestor_pid_with<F>(
    start_pid: u32,
    max_hops: usize,
    mut inspect_parent_and_name: F,
) -> Option<u32>
where
    F: FnMut(u32) -> Option<(u32, String)>,
{
    if start_pid <= 1 || max_hops == 0 {
        return None;
    }
    let (mut ppid, _) = inspect_parent_and_name(start_pid)?;
    let mut visited = HashSet::with_capacity(max_hops + 1);
    visited.insert(start_pid);

    for _ in 0..max_hops {
        if ppid <= 1 || !visited.insert(ppid) {
            return None;
        }
        let (next_ppid, name) = inspect_parent_and_name(ppid)?;
        if is_antigravity_main_basename(&name) {
            return Some(ppid);
        }
        ppid = next_ppid;
    }
    None
}

pub(crate) fn find_antigravity_ancestor_pid() -> Option<u32> {
    #[cfg(target_os = "linux")]
    {
        find_matching_ancestor_pid_with(std::process::id(), MAX_ANCESTOR_HOPS, |pid| {
            let raw = fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
            let (_state, ppid, _starttime) = parse_linux_proc_stat_state_ppid_starttime(&raw)?;
            let name = fs::read_link(format!("/proc/{pid}/exe"))
                .ok()
                .map(|p| {
                    let s = p.to_string_lossy().into_owned();
                    s.strip_suffix(" (deleted)").unwrap_or(&s).to_string()
                })
                .or_else(|| parse_linux_proc_stat_comm(&raw).map(str::to_string))?;
            Some((ppid, name))
        })
    }

    #[cfg(all(unix, not(target_os = "linux")))]
    {
        find_matching_ancestor_pid_with(std::process::id(), MAX_ANCESTOR_HOPS, |pid| {
            let pid_str = pid.to_string();
            let out = run_ps(&["-o", "ppid=,comm=", "-p", &pid_str]).ok()?;
            if !out.status.success() {
                return None;
            }
            let stdout = String::from_utf8_lossy(&out.stdout);
            let first_line = stdout.lines().find(|l| !l.trim().is_empty())?;
            parse_unix_ps_ppid_comm(first_line)
        })
    }

    #[cfg(windows)]
    {
        None
    }
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

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use super::*;

    #[test]
    fn parse_linux_proc_stat_handles_spaces_parens_and_zombies() {
        // Real-world Linux /proc/pid/stat where comm contains spaces and parentheses.
        let line = "4242 (Antigravity (Main) App) S 1000 4242 4242 0 -1 4194304 100 0 0 0 10 20 0 0 20 0 1 0 987654321 12345 100";
        assert_eq!(
            parse_linux_proc_stat_state_ppid_starttime(line),
            Some(('S', 1000, "987654321"))
        );
        assert_eq!(
            parse_linux_proc_stat_comm(line),
            Some("Antigravity (Main) App")
        );

        let zombie =
            "4243 (agy-companion) Z 1000 4243 4243 0 -1 4194304 0 0 0 0 0 0 0 0 20 0 1 0 55555 0 0";
        assert_eq!(
            parse_linux_proc_stat_state_ppid_starttime(zombie),
            Some(('Z', 1000, "55555"))
        );

        assert_eq!(
            parse_linux_proc_stat_state_ppid_starttime("malformed"),
            None
        );
    }

    #[test]
    fn parse_unix_ps_helpers_parse_state_lstart_and_ppid_comm() {
        assert_eq!(
            parse_unix_ps_state_lstart("  Ss   Thu Nov 26 21:00:01 2026  "),
            Some(("Ss", "Thu Nov 26 21:00:01 2026"))
        );
        assert_eq!(
            parse_unix_ps_state_lstart("Z+   Thu Nov 26 21:00:01 2026"),
            Some(("Z+", "Thu Nov 26 21:00:01 2026"))
        );
        assert_eq!(parse_unix_ps_state_lstart("   "), None);
        assert_eq!(parse_unix_ps_state_lstart("Ss"), None);

        // Zombie / exiting ps states (e.g. macOS `(agy-companion)` during exit) must be treated as dead.
        let (zombie_state, zombie_cmd) =
            parse_unix_ps_state_lstart("  Z+   (agy-companion)  ").expect("parse zombie state/cmd");
        assert!(is_unix_ps_zombie_or_exiting_state(zombie_state));
        assert_eq!(zombie_cmd, "(agy-companion)");
        assert!(is_unix_ps_zombie_or_exiting_state("Z"));
        assert!(is_unix_ps_zombie_or_exiting_state("ZN"));
        assert!(is_unix_ps_zombie_or_exiting_state("X"));
        assert!(is_unix_ps_zombie_or_exiting_state("X+"));
        assert!(!is_unix_ps_zombie_or_exiting_state("Ss"));
        assert!(!is_unix_ps_zombie_or_exiting_state("R+"));
        assert!(!is_unix_ps_zombie_or_exiting_state(""));

        assert_eq!(
            parse_unix_ps_ppid_comm(
                "  1234 /Applications/Antigravity.app/Contents/MacOS/Antigravity Helper "
            ),
            Some((
                1234,
                "/Applications/Antigravity.app/Contents/MacOS/Antigravity Helper".to_string()
            ))
        );
        assert_eq!(parse_unix_ps_ppid_comm("notapid /bin/zsh"), None);
        assert_eq!(parse_unix_ps_ppid_comm("1234   "), None);
    }

    #[test]
    fn parse_windows_tasklist_csv_matches_exact_pid() {
        let csv = "\"Antigravity.exe\",\"4321\",\"Console\",\"1\",\"120,000 K\"\r\n";
        assert!(parse_windows_tasklist_csv_matches_pid(csv, 4321));
        assert!(!parse_windows_tasklist_csv_matches_pid(csv, 432));
        assert!(!parse_windows_tasklist_csv_matches_pid(
            "INFO: No tasks are running which match the specified criteria.\r\n",
            4321
        ));
    }

    #[test]
    fn find_matching_ancestor_pid_walks_hops_skips_helpers_and_guards_cycles() {
        assert!(is_antigravity_main_basename(
            "/Applications/Antigravity.app/Contents/MacOS/Antigravity"
        ));
        assert!(is_antigravity_main_basename("antigravity"));
        assert!(!is_antigravity_main_basename(
            "/Applications/Antigravity.app/Contents/Frameworks/Antigravity Helper.app/Contents/MacOS/Antigravity Helper"
        ));

        // 3-hop chain: hook-env (500) -> zsh (400) -> Antigravity Helper (300) -> Antigravity (200) -> launchd (1)
        let tree: HashMap<u32, (u32, &str)> = HashMap::from([
            (500, (400, "/usr/local/bin/agy-companion")),
            (400, (300, "/bin/zsh")),
            (
                300,
                (
                    200,
                    "/Applications/Antigravity.app/Contents/Frameworks/Antigravity Helper.app/Contents/MacOS/Antigravity Helper",
                ),
            ),
            (
                200,
                (
                    1,
                    "/Applications/Antigravity.app/Contents/MacOS/Antigravity",
                ),
            ),
        ]);
        let found = find_matching_ancestor_pid_with(500, MAX_ANCESTOR_HOPS, |pid| {
            tree.get(&pid).map(|(p, n)| (*p, (*n).to_string()))
        });
        assert_eq!(found, Some(200));

        // Respects max_hops bound (2 hops stops at Antigravity Helper, never reaches 200).
        let bounded = find_matching_ancestor_pid_with(500, 2, |pid| {
            tree.get(&pid).map(|(p, n)| (*p, (*n).to_string()))
        });
        assert_eq!(bounded, None);

        // Guards against cycles in parent pointers.
        let cycle: HashMap<u32, (u32, &str)> = HashMap::from([
            (10, (20, "agy-companion")),
            (20, (30, "zsh")),
            (30, (20, "sh")),
        ]);
        let cycle_res = find_matching_ancestor_pid_with(10, MAX_ANCESTOR_HOPS, |pid| {
            cycle.get(&pid).map(|(p, n)| (*p, (*n).to_string()))
        });
        assert_eq!(cycle_res, None);
    }

    #[test]
    fn watched_process_identity_detects_live_self_and_rejects_pid_reuse_token_mismatch() {
        let self_pid = std::process::id();
        let id = snapshot_watched_process(self_pid).expect("snapshot self");
        assert_eq!(id.pid, self_pid);
        assert!(is_watched_process_alive(&id));

        // Simulated PID reuse with a different start token must report not alive on Unix.
        #[cfg(unix)]
        {
            let reused = WatchedProcessIdentity {
                pid: self_pid,
                start_token: Some("0000-wrong-start-token".to_string()),
            };
            assert!(!is_watched_process_alive(&reused));
        }
    }
}
