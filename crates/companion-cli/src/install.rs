use std::fs::{self, File};
use std::io::{ErrorKind, Read, Write};
use std::net::{Ipv4Addr, TcpListener};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, Instant};

#[cfg(windows)]
use companion_core::fs_safe::default_platform_config_dir;
use companion_core::fs_safe::{default_home_dir, unique_temp_suffix};
use companion_core::integration::{IntegrationOptions, IntegrationStatus};
use companion_core::{Config, CoreError, DEFAULT_LISTEN_PORT};
use companion_providers::LoopbackProxyClient;
use serde::{Deserialize, Serialize};

use crate::daemon::{
    DEFAULT_IDLE_SHUTDOWN_SECS, PROXY_LOG_FILENAME, PROXY_LOG_ROTATED_FILENAME, PROXY_PID_FILENAME,
    ProxyPidInfo, WaitHealthyOptions, WaitHealthyOutcome, read_proxy_pid, resolve_runtime_data_dir,
    spawn_detached_proxy_daemon_with_exe, wait_for_daemon_healthy,
};
use crate::{
    BINARY_NAME, CliError, load_config_checked, normalized_metadata_path,
    precheck_existing_config_file, precheck_existing_data_dir, resolve_cli_config_path,
};

pub const PLUGIN_DIR_NAME: &str = "agy-companion";
pub const WINDOWS_SHORTCUT_NAME: &str = "Antigravity (Companion).lnk";
const MAX_PLUGIN_MANIFEST_BYTES: u64 = 1024 * 1024;
const PORT_FALLBACK_SPAN: u16 = 9; // 24141..=24150
const INSTALL_HEALTH_PROBE_TIMEOUT: Duration = Duration::from_millis(500);
const FRESH_PORT_PROBE_TIMEOUT: Duration = Duration::from_millis(250);
const PROXY_START_WAIT_TIMEOUT: Duration = Duration::from_secs(5);
const PROXY_START_POLL_INTERVAL: Duration = Duration::from_millis(100);
const PROXY_STOP_TERM_TIMEOUT: Duration = Duration::from_millis(2_000);
const PROXY_STOP_KILL_TIMEOUT: Duration = Duration::from_millis(1_000);
const PROXY_STOP_POLL_INTERVAL: Duration = Duration::from_millis(50);
const PORT_RELEASE_WAIT_TIMEOUT: Duration = Duration::from_secs(3);

#[cfg(windows)]
const STABLE_BIN_NAME: &str = "agy-companion.exe";
#[cfg(not(windows))]
const STABLE_BIN_NAME: &str = "agy-companion";

#[cfg(any(windows, test))]
const CREATE_NO_WINDOW: u32 = 0x0800_0000;

pub struct EmbeddedPluginFile {
    pub rel_path: &'static str,
    pub content: &'static str,
    pub executable: bool,
}

/// All 7 bundled plugin files, ordered with `plugin.json` LAST so staging writes the
/// manifest only after scripts, assets, docs, and sidecar/MCP configs are already on disk.
pub const EMBEDDED_PLUGIN_FILES: &[EmbeddedPluginFile] = &[
    EmbeddedPluginFile {
        rel_path: "README.md",
        content: include_str!("../../../plugin/README.md"),
        executable: false,
    },
    EmbeddedPluginFile {
        rel_path: "assets/logo.svg",
        content: include_str!("../../../plugin/assets/logo.svg"),
        executable: false,
    },
    EmbeddedPluginFile {
        rel_path: "mcp_config.json",
        content: include_str!("../../../plugin/mcp_config.json"),
        executable: false,
    },
    EmbeddedPluginFile {
        rel_path: "sidecars/proxy/sidecar.json",
        content: include_str!("../../../plugin/sidecars/proxy/sidecar.json"),
        executable: false,
    },
    EmbeddedPluginFile {
        rel_path: "skills/companion/SKILL.md",
        content: include_str!("../../../plugin/skills/companion/SKILL.md"),
        executable: false,
    },
    EmbeddedPluginFile {
        rel_path: "scripts/run-companion.sh",
        content: include_str!("../../../plugin/scripts/run-companion.sh"),
        executable: true,
    },
    EmbeddedPluginFile {
        rel_path: "plugin.json",
        content: include_str!("../../../plugin/plugin.json"),
        executable: false,
    },
];

#[derive(Debug, Clone, Default)]
pub struct InstallOptions {
    pub config: Option<PathBuf>,
    pub plugins_dir: Option<PathBuf>,
    pub no_launcher_hook: bool,
    pub no_start_proxy: bool,
    pub force: bool,
    pub json: bool,
}

#[derive(Debug, Clone, Default)]
pub struct UninstallOptions {
    pub config: Option<PathBuf>,
    pub plugins_dir: Option<PathBuf>,
    pub purge: bool,
    pub dry_run: bool,
    pub json: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct InstallReport {
    pub ok: bool,
    pub version: String,
    pub binary_path: PathBuf,
    pub local_bin_symlink: Option<PathBuf>,
    pub plugin_dir: PathBuf,
    pub listen_port: u16,
    pub port_auto_selected: bool,
    pub launcher_integration: Option<IntegrationStatus>,
    pub windows_shortcut: Option<PathBuf>,
    pub proxy_started: bool,
    pub proxy_replaced: bool,
    pub proxy_version: Option<String>,
    pub antigravity_running: bool,
    pub uninstall_command: String,
    pub warnings: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct UninstallReport {
    pub ok: bool,
    pub dry_run: bool,
    pub purge: bool,
    pub rc_modified: Vec<PathBuf>,
    pub rc_deleted: Vec<PathBuf>,
    pub rc_manual_remaining: Vec<PathBuf>,
    pub env_script_removed: bool,
    pub plugin_dir_removed: bool,
    pub proxy_stopped: bool,
    pub proxy_pid: Option<u32>,
    pub binary_removed: bool,
    pub local_bin_symlink_removed: bool,
    pub windows_shortcut_removed: bool,
    pub purged_paths: Vec<PathBuf>,
    pub preserved_paths: Vec<PathBuf>,
    pub warnings: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum PidVerifyResult {
    VerifiedCompanion { cmdline: String },
    AlreadyDead,
    UnrelatedProcess { reason: String },
}

struct TempPathGuard {
    path: Option<PathBuf>,
    is_dir: bool,
}

impl TempPathGuard {
    fn file(path: PathBuf) -> Self {
        Self {
            path: Some(path),
            is_dir: false,
        }
    }

    fn dir(path: PathBuf) -> Self {
        Self {
            path: Some(path),
            is_dir: true,
        }
    }

    fn disarm(&mut self) {
        self.path = None;
    }
}

impl Drop for TempPathGuard {
    fn drop(&mut self) {
        if let Some(ref path) = self.path {
            if self.is_dir {
                let _ = fs::remove_dir_all(path);
            } else {
                let _ = fs::remove_file(path);
            }
        }
    }
}

fn resolve_home_dir() -> Result<PathBuf, CliError> {
    #[cfg(unix)]
    if let Some(raw) = std::env::var_os("HOME") {
        let trimmed = raw.to_string_lossy();
        if !trimmed.trim().is_empty() {
            let path = PathBuf::from(raw);
            companion_core::validate_path(&path)?;
            return Ok(path);
        }
    }
    let home = default_home_dir().ok_or(CoreError::HomeDirUnavailable)?;
    companion_core::validate_path(&home)?;
    Ok(home)
}

fn resolve_plugins_root(override_dir: Option<&Path>) -> Result<PathBuf, CliError> {
    match override_dir {
        Some(path) => {
            companion_core::validate_path(path)?;
            Ok(path.to_path_buf())
        }
        None => Ok(companion_core::default_plugins_dir()?),
    }
}

fn ensure_non_symlink_dir(
    dir: &Path,
    #[allow(unused_variables)] unix_mode: u32,
) -> Result<(), CliError> {
    companion_core::validate_path(dir)?;
    let check_dir = normalized_metadata_path(dir);
    match fs::symlink_metadata(&check_dir) {
        Ok(meta) => {
            if meta.file_type().is_symlink() {
                return Err(CliError::Install(format!(
                    "refusing to use symlinked directory '{}'",
                    dir.display()
                )));
            }
            if !meta.file_type().is_dir() {
                return Err(CliError::Install(format!(
                    "expected directory at '{}', found non-directory",
                    dir.display()
                )));
            }
            Ok(())
        }
        Err(err) if err.kind() == ErrorKind::NotFound => {
            let mut builder = fs::DirBuilder::new();
            builder.recursive(true);
            #[cfg(unix)]
            {
                use std::os::unix::fs::DirBuilderExt;
                builder.mode(unix_mode);
            }
            builder.create(dir)?;
            let created = fs::symlink_metadata(&check_dir)?;
            if created.file_type().is_symlink() || !created.file_type().is_dir() {
                return Err(CliError::Install(format!(
                    "created path '{}' is not a non-symlink directory",
                    dir.display()
                )));
            }
            Ok(())
        }
        Err(err) => Err(CliError::Io(err)),
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ExistingDirSafety {
    SafeDir,
    NotFound,
    Unsafe,
}

/// Inspects an existing directory path using `fs::symlink_metadata` (with trailing slashes
/// normalized) so callers never traverse a symlinked or non-directory intermediate component.
fn inspect_existing_dir(
    dir: &Path,
    label: &str,
    warnings: &mut Vec<String>,
) -> Result<ExistingDirSafety, CliError> {
    match fs::symlink_metadata(normalized_metadata_path(dir)) {
        Ok(meta) if meta.file_type().is_symlink() || !meta.file_type().is_dir() => {
            warnings.push(format!(
                "refusing to traverse symlinked/non-directory {label} '{}'",
                dir.display()
            ));
            Ok(ExistingDirSafety::Unsafe)
        }
        Ok(_) => Ok(ExistingDirSafety::SafeDir),
        Err(err) if err.kind() == ErrorKind::NotFound => Ok(ExistingDirSafety::NotFound),
        Err(err) => Err(CliError::Io(err)),
    }
}

/// Inspects every relative intermediate directory under `root` (`root/rel[0]`, `root/rel[0]/rel[1]`,
/// ...) with `fs::symlink_metadata` so uninstall never traverses a symlinked intermediate
/// component beneath a trusted base directory (such as `$HOME` or `%APPDATA%`).
fn inspect_non_symlink_rel_chain(
    root: &Path,
    rel_dirs: &[&str],
    label: &str,
    warnings: &mut Vec<String>,
) -> Result<ExistingDirSafety, CliError> {
    let mut current = root.to_path_buf();
    for segment in rel_dirs {
        current.push(segment);
        match inspect_existing_dir(&current, label, warnings)? {
            ExistingDirSafety::SafeDir => {}
            other => return Ok(other),
        }
    }
    Ok(ExistingDirSafety::SafeDir)
}

/// Inspects `data_dir` without traversing symlinks. When `data_dir` resolves to the default
/// `$HOME/.local/share/agy-companion` path, validates every intermediate component under `home`.
fn inspect_data_dir(
    home: &Path,
    data_dir: &Path,
    warnings: &mut Vec<String>,
) -> Result<ExistingDirSafety, CliError> {
    let default_data_dir_under_home = home.join(".local").join("share").join(PLUGIN_DIR_NAME);
    if normalized_metadata_path(data_dir) == normalized_metadata_path(&default_data_dir_under_home)
    {
        inspect_non_symlink_rel_chain(
            home,
            &[".local", "share", PLUGIN_DIR_NAME],
            "data directory",
            warnings,
        )
    } else {
        inspect_existing_dir(data_dir, "data directory", warnings)
    }
}

/// Inspects the parent directory of `norm_config_path` without traversing symlinks. When it
/// resolves to the default `$HOME/.config/agy-companion` path, validates every intermediate
/// component under `home`.
fn inspect_config_dir(
    home: &Path,
    norm_config_path: &Path,
    warnings: &mut Vec<String>,
) -> Result<ExistingDirSafety, CliError> {
    let Some(cfg_parent) = norm_config_path
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
    else {
        return Ok(ExistingDirSafety::SafeDir);
    };
    let default_config_dir_under_home = home.join(".config").join(PLUGIN_DIR_NAME);
    if normalized_metadata_path(cfg_parent)
        == normalized_metadata_path(&default_config_dir_under_home)
    {
        inspect_non_symlink_rel_chain(
            home,
            &[".config", PLUGIN_DIR_NAME],
            "config directory",
            warnings,
        )
    } else {
        inspect_existing_dir(cfg_parent, "config directory", warnings)
    }
}

/// Returns `true` if `usage_dir` equals `dir` or lies beneath `dir` after normalizing
/// trailing slashes and `.` segments.
fn usage_dir_under_dir(usage_dir: &Path, dir: &Path) -> bool {
    let norm_usage = normalized_metadata_path(usage_dir);
    let norm_dir = normalized_metadata_path(dir);
    !norm_dir.as_os_str().is_empty() && norm_usage.starts_with(&norm_dir)
}

/// Checks whether `plugin_dir/plugin.json` is a regular non-symlink JSON file declaring
/// `"name": "agy-companion"`.
pub(crate) fn is_companion_plugin_dir(plugin_dir: &Path) -> Result<bool, CliError> {
    let manifest_path = plugin_dir.join("plugin.json");
    let check_path = normalized_metadata_path(&manifest_path);
    let pre_meta = match fs::symlink_metadata(&check_path) {
        Ok(meta) => {
            if meta.file_type().is_symlink()
                || !meta.file_type().is_file()
                || meta.len() > MAX_PLUGIN_MANIFEST_BYTES
            {
                return Ok(false);
            }
            meta
        }
        Err(err) if err.kind() == ErrorKind::NotFound => return Ok(false),
        Err(err) => return Err(CliError::Io(err)),
    };

    let file = match File::open(&manifest_path) {
        Ok(file) => file,
        Err(err) if err.kind() == ErrorKind::NotFound => return Ok(false),
        Err(err) => return Err(CliError::Io(err)),
    };
    let open_meta = file.metadata()?;
    if !open_meta.file_type().is_file() {
        return Ok(false);
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        if open_meta.dev() != pre_meta.dev() || open_meta.ino() != pre_meta.ino() {
            return Ok(false);
        }
    }
    #[cfg(not(unix))]
    let _ = pre_meta;

    let mut bytes = Vec::new();
    file.take(MAX_PLUGIN_MANIFEST_BYTES + 1)
        .read_to_end(&mut bytes)?;
    if (bytes.len() as u64) > MAX_PLUGIN_MANIFEST_BYTES {
        return Ok(false);
    }
    let parsed: serde_json::Value = match serde_json::from_slice(&bytes) {
        Ok(value) => value,
        Err(_) => return Ok(false),
    };
    Ok(parsed.get("name").and_then(|v| v.as_str()) == Some(PLUGIN_DIR_NAME))
}

/// Probes `start_port..=end_port` on `127.0.0.1` for a fresh install when no `config.json` exists.
///
/// If `check_companion_on_first` is true and `start_port` already hosts a healthy `agy-companion`
/// proxy, `start_port` is selected so `install` can reuse or upgrade it. Otherwise the first
/// bindable port in the inclusive range is returned along with `port_auto_selected`.
pub(crate) async fn select_fresh_install_port(
    probe: &LoopbackProxyClient,
    start_port: u16,
    end_port: u16,
    check_companion_on_first: bool,
) -> Result<(u16, bool), CliError> {
    if check_companion_on_first
        && probe
            .probe_healthy_companion(start_port, Some(FRESH_PORT_PROBE_TIMEOUT))
            .await
            .is_some()
    {
        return Ok((start_port, false));
    }

    for port in start_port..=end_port {
        if let Ok(listener) = TcpListener::bind((Ipv4Addr::LOCALHOST, port)) {
            drop(listener);
            return Ok((port, port != start_port));
        }
    }

    Err(CliError::Install(format!(
        "no free loopback port available in {start_port}..={end_port}; free a port or preconfigure listen_port in config.json"
    )))
}

#[cfg(any(windows, test))]
pub(crate) fn render_windows_mcp_config(binary_path: &Path) -> Result<String, CliError> {
    let payload = serde_json::json!({
        "mcpServers": {
            "hub": {
                "command": binary_path.to_string_lossy(),
                "args": ["ui-mcp", "--data-dir=${PLUGIN_DATA}"]
            }
        }
    });
    let mut rendered = serde_json::to_string_pretty(&payload)
        .map_err(|e| CliError::Serialization(e.to_string()))?;
    rendered.push('\n');
    Ok(rendered)
}

#[cfg(any(windows, test))]
pub(crate) fn render_windows_sidecar_config(binary_path: &Path) -> Result<String, CliError> {
    let payload = serde_json::json!({
        "command": binary_path.to_string_lossy(),
        "args": ["serve", "--adopt-running"],
        "restart_policy": "on-failure",
        "display_name": "Antigravity Companion Proxy",
        "description": "Local loopback Gemini pass-through proxy with SSE usageMetadata telemetry and config hot-reload."
    });
    let mut rendered = serde_json::to_string_pretty(&payload)
        .map_err(|e| CliError::Serialization(e.to_string()))?;
    rendered.push('\n');
    Ok(rendered)
}

#[cfg(any(windows, test))]
pub(crate) fn resolve_windows_shortcut_path(app_data: &Path) -> PathBuf {
    app_data
        .join("Microsoft")
        .join("Windows")
        .join("Start Menu")
        .join("Programs")
        .join(WINDOWS_SHORTCUT_NAME)
}

#[cfg(any(windows, test))]
fn ps_single_quote(raw: &str) -> String {
    // PowerShell 5.1 treats both ASCII apostrophe and Unicode single-quote variants
    // (U+2018, U+2019, U+201A, U+201B) as single-quote string delimiters.
    let mut escaped = String::with_capacity(raw.len() + 2);
    escaped.push('\'');
    for ch in raw.chars() {
        if matches!(ch, '\'' | '\u{2018}' | '\u{2019}' | '\u{201A}' | '\u{201B}') {
            escaped.push(ch);
        }
        escaped.push(ch);
    }
    escaped.push('\'');
    escaped
}

#[cfg(any(windows, test))]
fn reject_ps_path_newlines(path: &Path) -> Result<(), CliError> {
    let text = path.to_string_lossy();
    if text.contains('\r') || text.contains('\n') {
        return Err(CliError::Install(format!(
            "refusing to embed path containing newline characters in PowerShell shortcut script: '{}'",
            path.display()
        )));
    }
    Ok(())
}

#[cfg(any(windows, test))]
pub(crate) fn render_windows_shortcut_ps_script(
    shortcut_path: &Path,
    binary_path: &Path,
    icon_path: Option<&Path>,
) -> String {
    let shortcut_lit = ps_single_quote(&shortcut_path.to_string_lossy());
    let target_lit = ps_single_quote(&binary_path.to_string_lossy());
    let working_dir = binary_path
        .parent()
        .map(|p| p.to_string_lossy().into_owned())
        .unwrap_or_default();
    let working_dir_lit = ps_single_quote(&working_dir);

    let mut script = format!(
        "$ErrorActionPreference = 'Stop'; \
$ws = New-Object -ComObject WScript.Shell; \
$s = $ws.CreateShortcut({shortcut_lit}); \
$s.TargetPath = {target_lit}; \
$s.Arguments = 'launch'; \
$s.WorkingDirectory = {working_dir_lit}; \
$s.WindowStyle = 7; \
$s.Description = 'Launch Antigravity with Antigravity Companion proxy'; "
    );
    if let Some(icon) = icon_path {
        let icon_lit = ps_single_quote(&format!("{},0", icon.to_string_lossy()));
        script.push_str(&format!("$s.IconLocation = {icon_lit}; "));
    }
    script.push_str("$s.Save();");
    script
}

#[cfg(windows)]
fn create_windows_start_menu_shortcut(binary_path: &Path) -> Result<PathBuf, CliError> {
    use std::os::windows::process::CommandExt;

    let app_data = std::env::var_os("APPDATA")
        .filter(|v| !v.to_string_lossy().trim().is_empty())
        .map(PathBuf::from)
        .or_else(default_platform_config_dir)
        .ok_or_else(|| {
            CliError::Install(
                "cannot resolve %APPDATA% for Windows Start Menu shortcut".to_string(),
            )
        })?;
    companion_core::validate_path(&app_data)?;
    let mut shortcut_warnings = Vec::new();
    if inspect_non_symlink_rel_chain(
        &app_data,
        &["Microsoft", "Windows", "Start Menu", "Programs"],
        "Start Menu Programs directory",
        &mut shortcut_warnings,
    )? == ExistingDirSafety::Unsafe
    {
        return Err(CliError::Install(
            shortcut_warnings.into_iter().next().unwrap_or_else(|| {
                "refusing to traverse symlinked Start Menu directory".to_string()
            }),
        ));
    }
    let shortcut_path = resolve_windows_shortcut_path(&app_data);
    if let Some(parent) = shortcut_path.parent() {
        ensure_non_symlink_dir(parent, 0o755)?;
    }
    match fs::symlink_metadata(normalized_metadata_path(&shortcut_path)) {
        Ok(meta) if meta.file_type().is_symlink() || !meta.file_type().is_file() => {
            return Err(CliError::Install(format!(
                "refusing to overwrite symlinked/non-regular Start Menu shortcut '{}'",
                shortcut_path.display()
            )));
        }
        Ok(_) => {}
        Err(err) if err.kind() == ErrorKind::NotFound => {}
        Err(err) => return Err(CliError::Io(err)),
    }

    let discovered_app = crate::launch::default_antigravity_app_path();
    let icon_path = discovered_app.is_file().then_some(discovered_app.as_path());
    reject_ps_path_newlines(&shortcut_path)?;
    reject_ps_path_newlines(binary_path)?;
    if let Some(icon) = icon_path {
        reject_ps_path_newlines(icon)?;
    }
    let script = render_windows_shortcut_ps_script(&shortcut_path, binary_path, icon_path);

    let output = Command::new("powershell.exe")
        .args([
            "-NoProfile",
            "-NonInteractive",
            "-WindowStyle",
            "Hidden",
            "-Command",
            &script,
        ])
        .creation_flags(CREATE_NO_WINDOW)
        .output()?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(CliError::Install(format!(
            "failed to create Start Menu shortcut '{}': {}",
            shortcut_path.display(),
            stderr.trim()
        )));
    }
    Ok(shortcut_path)
}

#[cfg(windows)]
fn remove_windows_start_menu_shortcut(
    dry_run: bool,
    warnings: &mut Vec<String>,
) -> Result<bool, CliError> {
    let Some(app_data) = std::env::var_os("APPDATA")
        .filter(|v| !v.to_string_lossy().trim().is_empty())
        .map(PathBuf::from)
        .or_else(default_platform_config_dir)
    else {
        return Ok(false);
    };
    if companion_core::validate_path(&app_data).is_err() {
        return Ok(false);
    }
    if inspect_non_symlink_rel_chain(
        &app_data,
        &["Microsoft", "Windows", "Start Menu", "Programs"],
        "Start Menu Programs directory",
        warnings,
    )? != ExistingDirSafety::SafeDir
    {
        return Ok(false);
    }
    let shortcut_path = resolve_windows_shortcut_path(&app_data);
    let check_path = normalized_metadata_path(&shortcut_path);
    match fs::symlink_metadata(&check_path) {
        Ok(meta) if meta.file_type().is_file() && !meta.file_type().is_symlink() => {
            if !dry_run {
                fs::remove_file(&shortcut_path)?;
            }
            Ok(true)
        }
        Ok(_) => Ok(false),
        Err(err) if err.kind() == ErrorKind::NotFound => Ok(false),
        Err(err) => Err(CliError::Io(err)),
    }
}

fn stage_binary_copy(src_exe: &Path, bin_dir: &Path) -> Result<(PathBuf, TempPathGuard), CliError> {
    let staged_path = bin_dir.join(format!(".{BINARY_NAME}.new.{}", unique_temp_suffix()));
    let guard = TempPathGuard::file(staged_path.clone());

    let mut reader = File::open(src_exe)?;
    let mut open_opts = fs::OpenOptions::new();
    open_opts.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        open_opts.mode(0o755);
    }
    let mut writer = open_opts.open(&staged_path)?;
    std::io::copy(&mut reader, &mut writer)?;
    writer.sync_all()?;
    drop(writer);

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&staged_path, fs::Permissions::from_mode(0o755))?;
    }

    Ok((staged_path, guard))
}

fn stage_version_file(bin_dir: &Path, version: &str) -> Result<(PathBuf, TempPathGuard), CliError> {
    let staged_path = bin_dir.join(format!(".version.new.{}", unique_temp_suffix()));
    let guard = TempPathGuard::file(staged_path.clone());

    let mut open_opts = fs::OpenOptions::new();
    open_opts.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        open_opts.mode(0o644);
    }
    let mut writer = open_opts.open(&staged_path)?;
    writer.write_all(format!("{version}\n").as_bytes())?;
    writer.sync_all()?;
    drop(writer);

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&staged_path, fs::Permissions::from_mode(0o644))?;
    }

    Ok((staged_path, guard))
}

fn write_staged_plugin_dir(
    plugins_root: &Path,
    #[allow(unused_variables)] stable_bin: &Path,
) -> Result<(PathBuf, TempPathGuard), CliError> {
    let stage_dir = plugins_root.join(format!(".{PLUGIN_DIR_NAME}.stage.{}", unique_temp_suffix()));
    fs::create_dir(&stage_dir)?;
    let guard = TempPathGuard::dir(stage_dir.clone());

    for entry in EMBEDDED_PLUGIN_FILES {
        let dest = stage_dir.join(entry.rel_path);
        if let Some(parent) = dest.parent() {
            fs::create_dir_all(parent)?;
        }

        #[cfg(windows)]
        let rendered_override: Option<String> = match entry.rel_path {
            "mcp_config.json" => Some(render_windows_mcp_config(stable_bin)?),
            "sidecars/proxy/sidecar.json" => Some(render_windows_sidecar_config(stable_bin)?),
            _ => None,
        };
        #[cfg(not(windows))]
        let rendered_override: Option<String> = None;

        let bytes: &[u8] = match rendered_override.as_deref() {
            Some(content) => content.as_bytes(),
            None => entry.content.as_bytes(),
        };

        let mut open_opts = fs::OpenOptions::new();
        open_opts.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            open_opts.mode(if entry.executable { 0o755 } else { 0o644 });
        }
        let mut file = open_opts.open(&dest)?;
        file.write_all(bytes)?;
        file.sync_all()?;
        drop(file);

        #[cfg(unix)]
        if entry.executable {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&dest, fs::Permissions::from_mode(0o755))?;
        }
    }

    Ok((stage_dir, guard))
}

#[cfg(unix)]
fn symlink_points_to_path(link_path: &Path, target_path: &Path) -> bool {
    let Ok(raw_target) = fs::read_link(link_path) else {
        return false;
    };
    let resolved_link = if raw_target.is_absolute() {
        raw_target
    } else {
        link_path
            .parent()
            .unwrap_or_else(|| Path::new("."))
            .join(raw_target)
    };
    if normalized_metadata_path(&resolved_link) == normalized_metadata_path(target_path) {
        return true;
    }
    match (
        fs::canonicalize(&resolved_link),
        fs::canonicalize(target_path),
    ) {
        (Ok(a), Ok(b)) => a == b,
        _ => false,
    }
}

#[cfg(unix)]
fn install_local_bin_symlink(
    home: &Path,
    stable_bin: &Path,
    warnings: &mut Vec<String>,
) -> Option<PathBuf> {
    use std::os::unix::fs::symlink;

    let dot_local_dir = home.join(".local");
    let local_bin_dir = dot_local_dir.join("bin");
    let local_bin_link = local_bin_dir.join(BINARY_NAME);

    // Ensure ~/.local and ~/.local/bin are non-symlink directories (or create them).
    match fs::symlink_metadata(normalized_metadata_path(&dot_local_dir)) {
        Ok(meta) if meta.file_type().is_symlink() || !meta.file_type().is_dir() => {
            warnings.push(format!(
                "skipping {} symlink because '{}' is not a regular directory",
                local_bin_link.display(),
                dot_local_dir.display()
            ));
            return None;
        }
        Ok(_) => {}
        Err(err) if err.kind() == ErrorKind::NotFound => {}
        Err(err) => {
            warnings.push(format!(
                "failed to inspect '{}': {err}",
                dot_local_dir.display()
            ));
            return None;
        }
    }

    match fs::symlink_metadata(normalized_metadata_path(&local_bin_dir)) {
        Ok(meta) => {
            if meta.file_type().is_symlink() || !meta.file_type().is_dir() {
                warnings.push(format!(
                    "skipping {} symlink because '{}' is not a regular directory",
                    local_bin_link.display(),
                    local_bin_dir.display()
                ));
                return None;
            }
        }
        Err(err) if err.kind() == ErrorKind::NotFound => {
            if let Err(create_err) = fs::create_dir_all(&local_bin_dir) {
                warnings.push(format!(
                    "failed to create '{}': {create_err}",
                    local_bin_dir.display()
                ));
                return None;
            }
            match fs::symlink_metadata(normalized_metadata_path(&local_bin_dir)) {
                Ok(meta) if !meta.file_type().is_symlink() && meta.file_type().is_dir() => {}
                _ => {
                    warnings.push(format!(
                        "skipping {} symlink because '{}' is not a regular directory",
                        local_bin_link.display(),
                        local_bin_dir.display()
                    ));
                    return None;
                }
            }
        }
        Err(err) => {
            warnings.push(format!(
                "failed to inspect '{}': {err}",
                local_bin_dir.display()
            ));
            return None;
        }
    }

    match fs::symlink_metadata(normalized_metadata_path(&local_bin_link)) {
        Err(err) if err.kind() == ErrorKind::NotFound => {
            if let Err(sym_err) = symlink(stable_bin, &local_bin_link) {
                warnings.push(format!(
                    "failed to create symlink '{}': {sym_err}",
                    local_bin_link.display()
                ));
                return None;
            }
            Some(local_bin_link)
        }
        Ok(meta) if meta.file_type().is_symlink() => {
            if !symlink_points_to_path(&local_bin_link, stable_bin) {
                warnings.push(format!(
                    "leaving existing foreign symlink '{}' untouched",
                    local_bin_link.display()
                ));
                return None;
            }
            let tmp_link =
                local_bin_dir.join(format!(".{BINARY_NAME}.link.{}", unique_temp_suffix()));
            let mut tmp_guard = TempPathGuard::file(tmp_link.clone());
            if let Err(sym_err) = symlink(stable_bin, &tmp_link) {
                warnings.push(format!(
                    "failed to refresh symlink '{}': {sym_err}",
                    local_bin_link.display()
                ));
                return Some(local_bin_link);
            }
            if let Err(ren_err) = fs::rename(&tmp_link, &local_bin_link) {
                warnings.push(format!(
                    "failed to replace symlink '{}': {ren_err}",
                    local_bin_link.display()
                ));
                return Some(local_bin_link);
            }
            tmp_guard.disarm();
            Some(local_bin_link)
        }
        Ok(_) => {
            warnings.push(format!(
                "leaving existing non-symlink '{}' untouched",
                local_bin_link.display()
            ));
            None
        }
        Err(err) => {
            warnings.push(format!(
                "failed to inspect '{}': {err}",
                local_bin_link.display()
            ));
            None
        }
    }
}

fn swap_staged_binary_and_version(
    staged_bin: Option<(PathBuf, TempPathGuard)>,
    stable_bin: &Path,
    (staged_ver, mut ver_guard): (PathBuf, TempPathGuard),
    version_file: &Path,
) -> Result<(), CliError> {
    if let Some((tmp_bin, mut bin_guard)) = staged_bin {
        #[cfg(not(windows))]
        {
            fs::rename(&tmp_bin, stable_bin)?;
            bin_guard.disarm();
        }
        #[cfg(windows)]
        {
            match fs::rename(&tmp_bin, stable_bin) {
                Ok(()) => bin_guard.disarm(),
                Err(_) => {
                    let old_bin = stable_bin
                        .parent()
                        .unwrap_or_else(|| Path::new("."))
                        .join(format!(".{BINARY_NAME}.old.{}.exe", unique_temp_suffix()));
                    if stable_bin.exists() {
                        fs::rename(stable_bin, &old_bin)?;
                    }
                    if let Err(err) = fs::rename(&tmp_bin, stable_bin) {
                        let _ = fs::rename(&old_bin, stable_bin);
                        return Err(CliError::Io(err));
                    }
                    bin_guard.disarm();
                    let _ = fs::remove_file(&old_bin);
                }
            }
        }
    }

    fs::rename(&staged_ver, version_file)?;
    ver_guard.disarm();
    Ok(())
}

/// Unlinks a verified symlink entry without deleting the symlink target.
///
/// Callers MUST verify `symlink_metadata(path)?.file_type().is_symlink()` first.
/// On Windows, directory symlinks/junctions return `PermissionDenied` from
/// `fs::remove_file` and require `fs::remove_dir` (which unlinks only the reparse point).
fn unlink_symlink(path: &Path) -> std::io::Result<()> {
    #[cfg(not(windows))]
    {
        fs::remove_file(path)
    }
    #[cfg(windows)]
    {
        fs::remove_file(path).or_else(|_| fs::remove_dir(path))
    }
}

fn swap_plugin_dir_two_step(
    plugins_root: &Path,
    plugin_target: &Path,
    stage_dir: &Path,
    mut stage_guard: TempPathGuard,
    warnings: &mut Vec<String>,
) -> Result<(), CliError> {
    let check_target = normalized_metadata_path(plugin_target);
    match fs::symlink_metadata(&check_target) {
        Err(err) if err.kind() == ErrorKind::NotFound => {
            fs::rename(stage_dir, plugin_target)?;
            stage_guard.disarm();
            Ok(())
        }
        Ok(meta) if meta.file_type().is_symlink() => {
            #[cfg(unix)]
            let orig_target = fs::read_link(plugin_target).ok();
            unlink_symlink(plugin_target)?;
            if let Err(rename_err) = fs::rename(stage_dir, plugin_target) {
                #[cfg(unix)]
                if let Some(target) = orig_target {
                    let _ = std::os::unix::fs::symlink(target, plugin_target);
                }
                return Err(CliError::Io(rename_err));
            }
            stage_guard.disarm();
            Ok(())
        }
        Ok(meta) if meta.file_type().is_dir() => {
            let backup_dir = plugins_root.join(format!(
                ".{PLUGIN_DIR_NAME}.backup.{}",
                unique_temp_suffix()
            ));
            fs::rename(plugin_target, &backup_dir)?;
            if let Err(rename_err) = fs::rename(stage_dir, plugin_target) {
                let _ = fs::rename(&backup_dir, plugin_target);
                return Err(CliError::Io(rename_err));
            }
            stage_guard.disarm();
            if let Err(rm_err) = fs::remove_dir_all(&backup_dir) {
                warnings.push(format!(
                    "failed to clean up previous plugin backup '{}': {rm_err}",
                    backup_dir.display()
                ));
            }
            Ok(())
        }
        Ok(_) => Err(CliError::Install(format!(
            "cannot install plugin at '{}': path exists and is not a directory",
            plugin_target.display()
        ))),
        Err(err) => Err(CliError::Io(err)),
    }
}

fn extract_exec_basename(raw: &str) -> &str {
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
        .args([
            "-NoProfile",
            "-NonInteractive",
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
fn send_unix_signal(pid: u32, signal_flag: &str) -> Result<(), CliError> {
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
    #[cfg(unix)]
    {
        send_unix_signal(info.pid, "-TERM")?;
        let deadline = Instant::now() + PROXY_STOP_TERM_TIMEOUT;
        while Instant::now() < deadline {
            tokio::time::sleep(PROXY_STOP_POLL_INTERVAL).await;
            match verify_proxy_pid_process(info)? {
                PidVerifyResult::AlreadyDead | PidVerifyResult::UnrelatedProcess { .. } => {
                    return Ok(true);
                }
                PidVerifyResult::VerifiedCompanion { .. } => {}
            }
        }
        // Re-verify immediately before escalating to SIGKILL.
        if matches!(
            verify_proxy_pid_process(info)?,
            PidVerifyResult::VerifiedCompanion { .. }
        ) {
            send_unix_signal(info.pid, "-KILL")?;
            let kill_deadline = Instant::now() + PROXY_STOP_KILL_TIMEOUT;
            while Instant::now() < kill_deadline {
                tokio::time::sleep(PROXY_STOP_POLL_INTERVAL).await;
                if !matches!(
                    verify_proxy_pid_process(info)?,
                    PidVerifyResult::VerifiedCompanion { .. }
                ) {
                    return Ok(true);
                }
            }
        }
        Ok(!matches!(
            verify_proxy_pid_process(info)?,
            PidVerifyResult::VerifiedCompanion { .. }
        ))
    }

    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;

        let pid_str = info.pid.to_string();
        let _ = Command::new("taskkill")
            .args(["/PID", &pid_str])
            .creation_flags(CREATE_NO_WINDOW)
            .output();
        let deadline = Instant::now() + PROXY_STOP_TERM_TIMEOUT;
        while Instant::now() < deadline {
            tokio::time::sleep(PROXY_STOP_POLL_INTERVAL).await;
            if !matches!(
                verify_proxy_pid_process(info)?,
                PidVerifyResult::VerifiedCompanion { .. }
            ) {
                return Ok(true);
            }
        }
        if matches!(
            verify_proxy_pid_process(info)?,
            PidVerifyResult::VerifiedCompanion { .. }
        ) {
            let _ = Command::new("taskkill")
                .args(["/F", "/PID", &pid_str])
                .creation_flags(CREATE_NO_WINDOW)
                .output();
            let kill_deadline = Instant::now() + PROXY_STOP_KILL_TIMEOUT;
            while Instant::now() < kill_deadline {
                tokio::time::sleep(PROXY_STOP_POLL_INTERVAL).await;
                if !matches!(
                    verify_proxy_pid_process(info)?,
                    PidVerifyResult::VerifiedCompanion { .. }
                ) {
                    return Ok(true);
                }
            }
        }
        Ok(!matches!(
            verify_proxy_pid_process(info)?,
            PidVerifyResult::VerifiedCompanion { .. }
        ))
    }
}

fn deduplicate_paths(paths: Vec<PathBuf>) -> Vec<PathBuf> {
    let mut out: Vec<PathBuf> = Vec::new();
    for path in paths {
        let norm = normalized_metadata_path(&path);
        if !out.iter().any(|p| normalized_metadata_path(p) == norm) {
            out.push(path);
        }
    }
    out
}

fn remove_pid_file_if_matches(pid_path: &Path, expected_pid: u32) {
    if matches!(read_proxy_pid(pid_path), Ok(Some(cur)) if cur.pid == expected_pid) {
        let _ = fs::remove_file(pid_path);
    }
}

async fn stop_proxy_from_pid_files(
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

fn is_binary_on_path(binary_name: &str) -> bool {
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

fn detect_antigravity_running() -> bool {
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

fn remove_regular_file_if_present(
    path: &Path,
    dry_run: bool,
    warnings: &mut Vec<String>,
) -> Result<bool, CliError> {
    let check_path = normalized_metadata_path(path);
    match fs::symlink_metadata(&check_path) {
        Ok(meta) => {
            if meta.file_type().is_symlink() {
                warnings.push(format!(
                    "refusing to delete symlinked file '{}'",
                    path.display()
                ));
                return Ok(false);
            }
            if !meta.file_type().is_file() {
                warnings.push(format!(
                    "refusing to delete non-regular path '{}'",
                    path.display()
                ));
                return Ok(false);
            }
            if !dry_run {
                fs::remove_file(path)?;
            }
            Ok(true)
        }
        Err(err) if err.kind() == ErrorKind::NotFound => Ok(false),
        Err(err) => Err(CliError::Io(err)),
    }
}

fn remove_dir_if_non_symlink(
    path: &Path,
    dry_run: bool,
    warnings: &mut Vec<String>,
) -> Result<bool, CliError> {
    let check_path = normalized_metadata_path(path);
    match fs::symlink_metadata(&check_path) {
        Ok(meta) => {
            if meta.file_type().is_symlink() {
                warnings.push(format!(
                    "refusing to recursively delete symlinked directory '{}'",
                    path.display()
                ));
                return Ok(false);
            }
            if !meta.file_type().is_dir() {
                warnings.push(format!(
                    "refusing to delete non-directory path '{}'",
                    path.display()
                ));
                return Ok(false);
            }
            if !dry_run {
                fs::remove_dir_all(path)?;
            }
            Ok(true)
        }
        Err(err) if err.kind() == ErrorKind::NotFound => Ok(false),
        Err(err) => Err(CliError::Io(err)),
    }
}

fn is_same_file_path(a: &Path, b: &Path) -> bool {
    if normalized_metadata_path(a) == normalized_metadata_path(b) {
        return true;
    }
    match (fs::canonicalize(a), fs::canonicalize(b)) {
        (Ok(ca), Ok(cb)) => ca == cb,
        _ => false,
    }
}

pub async fn run_install<W: Write>(opts: InstallOptions, out: &mut W) -> Result<(), CliError> {
    let mut warnings: Vec<String> = Vec::new();
    let version = env!("CARGO_PKG_VERSION").to_string();

    // 1. Preconditions & S6 validation (read-only — never create directories before checks pass).
    let home = resolve_home_dir()?;
    let plugins_root = resolve_plugins_root(opts.plugins_dir.as_deref())?;
    let default_plugins_under_home = home.join(".gemini").join("config").join("plugins");
    let mut plugin_root_warnings = Vec::new();
    let plugins_root_safety = if opts.plugins_dir.is_none()
        && normalized_metadata_path(&plugins_root)
            == normalized_metadata_path(&default_plugins_under_home)
    {
        inspect_non_symlink_rel_chain(
            &home,
            &[".gemini", "config", "plugins"],
            "plugins root",
            &mut plugin_root_warnings,
        )?
    } else {
        inspect_existing_dir(&plugins_root, "plugins root", &mut plugin_root_warnings)?
    };
    if plugins_root_safety == ExistingDirSafety::Unsafe {
        return Err(CliError::Install(
            plugin_root_warnings.into_iter().next().unwrap_or_else(|| {
                format!(
                    "refusing to traverse symlinked/non-directory plugins root '{}'",
                    plugins_root.display()
                )
            }),
        ));
    }
    let plugin_target = plugins_root.join(PLUGIN_DIR_NAME);

    if plugins_root_safety == ExistingDirSafety::SafeDir {
        match fs::symlink_metadata(normalized_metadata_path(&plugin_target)) {
            Ok(meta) if meta.file_type().is_symlink() => {
                if !opts.force {
                    return Err(CliError::Install(format!(
                        "plugin path '{}' is a symlink (likely linked via scripts/dev-install.sh); re-run with --force to replace the symlink without deleting its target",
                        plugin_target.display()
                    )));
                }
            }
            Ok(meta) if meta.file_type().is_dir() => {
                if !opts.force && !is_companion_plugin_dir(&plugin_target)? {
                    return Err(CliError::Install(format!(
                        "refusing to overwrite '{}' because it does not contain a valid agy-companion plugin.json (re-run with --force to override)",
                        plugin_target.display()
                    )));
                }
            }
            Ok(_) => {
                return Err(CliError::Install(format!(
                    "plugin path '{}' exists and is not a directory",
                    plugin_target.display()
                )));
            }
            Err(err) if err.kind() == ErrorKind::NotFound => {}
            Err(err) => return Err(CliError::Io(err)),
        }
    }

    let config_path = resolve_cli_config_path(opts.config.as_deref())?;
    let norm_config_path = normalized_metadata_path(&config_path);
    let probe =
        LoopbackProxyClient::new(INSTALL_HEALTH_PROBE_TIMEOUT, INSTALL_HEALTH_PROBE_TIMEOUT)?;
    let data_dir = companion_core::default_data_dir()?;
    let mut data_dir_warnings = Vec::new();
    let data_dir_safety = inspect_data_dir(&home, &data_dir, &mut data_dir_warnings)?;
    if data_dir_safety == ExistingDirSafety::Unsafe {
        return Err(CliError::Install(
            data_dir_warnings.into_iter().next().unwrap_or_else(|| {
                format!(
                    "refusing to traverse symlinked/non-directory data directory '{}'",
                    data_dir.display()
                )
            }),
        ));
    }
    precheck_existing_data_dir(&data_dir)?;
    if data_dir_safety == ExistingDirSafety::SafeDir {
        let bin_dir = data_dir.join("bin");
        if inspect_existing_dir(&bin_dir, "bin directory", &mut data_dir_warnings)?
            == ExistingDirSafety::Unsafe
        {
            return Err(CliError::Install(
                data_dir_warnings.into_iter().next().unwrap_or_else(|| {
                    format!(
                        "refusing to traverse symlinked/non-directory bin directory '{}'",
                        bin_dir.display()
                    )
                }),
            ));
        }
    }
    #[cfg(not(windows))]
    if !opts.no_launcher_hook
        && data_dir_safety == ExistingDirSafety::SafeDir
        && companion_core::integration::detect_shell(std::env::var("SHELL").ok().as_deref())
            != companion_core::integration::ShellKind::Fish
    {
        let env_path = data_dir.join(companion_core::integration::ENV_SCRIPT_FILENAME);
        match fs::symlink_metadata(normalized_metadata_path(&env_path)) {
            Ok(meta) if meta.file_type().is_symlink() => {
                return Err(CliError::Install(format!(
                    "refusing to use symlinked env script '{}'",
                    env_path.display()
                )));
            }
            Ok(meta) if !meta.file_type().is_file() => {
                return Err(CliError::Install(format!(
                    "refusing to use non-regular env script '{}'",
                    env_path.display()
                )));
            }
            Ok(_) => {}
            Err(err) if err.kind() == ErrorKind::NotFound => {}
            Err(err) => {
                return Err(CliError::Install(format!(
                    "failed to inspect env script '{}': {err}",
                    env_path.display()
                )));
            }
        }
    }

    let mut config_dir_warnings = Vec::new();
    if inspect_config_dir(&home, &norm_config_path, &mut config_dir_warnings)?
        == ExistingDirSafety::Unsafe
    {
        return Err(CliError::Install(
            config_dir_warnings.into_iter().next().unwrap_or_else(|| {
                format!(
                    "refusing to traverse symlinked/non-directory config directory '{}'",
                    norm_config_path
                        .parent()
                        .unwrap_or(norm_config_path.as_path())
                        .display()
                )
            }),
        ));
    }
    let config_existed = precheck_existing_config_file(&config_path)?;
    let mut config = load_config_checked(&config_path)?;

    // Fresh-install port 24141..=24150 fallback runs even when --no-start-proxy is set.
    // Defer persisting config.json until preconditions, staging, and swaps succeed.
    let mut port_auto_selected = false;
    if !config_existed {
        let (selected_port, auto_selected) = select_fresh_install_port(
            &probe,
            DEFAULT_LISTEN_PORT,
            DEFAULT_LISTEN_PORT + PORT_FALLBACK_SPAN,
            true,
        )
        .await?;
        if auto_selected {
            config.proxy.port =
                std::num::NonZeroU16::new(selected_port).ok_or(CliError::InvalidPort(0))?;
            port_auto_selected = true;
        }
    }
    let listen_port = config.proxy.port.get();
    let runtime_data_dir = resolve_runtime_data_dir(&config)?;

    // 2. Stage binary, .version, and 7-file plugin directory BEFORE swapping anything.
    ensure_non_symlink_dir(&plugins_root, 0o755)?;
    ensure_non_symlink_dir(&data_dir, 0o700)?;
    let bin_dir = data_dir.join("bin");
    ensure_non_symlink_dir(&bin_dir, 0o755)?;
    let stable_bin = bin_dir.join(STABLE_BIN_NAME);
    let version_file = bin_dir.join(".version");

    let current_exe = std::env::current_exe()?;
    let staged_bin = if cfg!(windows) && is_same_file_path(&current_exe, &stable_bin) {
        None
    } else {
        Some(stage_binary_copy(&current_exe, &bin_dir)?)
    };
    let staged_ver = stage_version_file(&bin_dir, &version)?;
    let (stage_dir, stage_guard) = write_staged_plugin_dir(&plugins_root, &stable_bin)?;

    // 3. Tight atomic swap: binary + .version, optional ~/.local/bin symlink, 2-step plugin swap,
    //    and deferred config.json persistence if fresh-install port fallback was selected.
    swap_staged_binary_and_version(staged_bin, &stable_bin, staged_ver, &version_file)?;

    #[cfg(unix)]
    let local_bin_symlink = install_local_bin_symlink(&home, &stable_bin, &mut warnings);
    #[cfg(not(unix))]
    let local_bin_symlink: Option<PathBuf> = {
        let _ = &home;
        None
    };

    swap_plugin_dir_two_step(
        &plugins_root,
        &plugin_target,
        &stage_dir,
        stage_guard,
        &mut warnings,
    )?;

    if port_auto_selected {
        config.save_secure(&config_path)?;
    }

    // 4. Proxy startup or version-skew replacement (unless --no-start-proxy).
    let mut proxy_started = false;
    let mut proxy_replaced = false;
    let mut proxy_version: Option<String> = None;

    if !opts.no_start_proxy {
        let existing_health = probe
            .probe_healthy_companion(listen_port, Some(INSTALL_HEALTH_PROBE_TIMEOUT))
            .await;
        match existing_health {
            Some(health) if health.version == version => {
                proxy_version = Some(health.version);
            }
            Some(stale_health) => {
                // Version skew: stop verified running companion proxy before starting updated binary.
                let runtime_differs = normalized_metadata_path(&runtime_data_dir)
                    != normalized_metadata_path(&data_dir);
                let runtime_data_dir_safety = if runtime_differs {
                    inspect_existing_dir(
                        &runtime_data_dir,
                        "runtime data directory",
                        &mut warnings,
                    )?
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
                    &probe,
                    &pid_candidates,
                    Some(listen_port),
                    false,
                    &mut warnings,
                )
                .await;
                if stopped {
                    let mut port_released = false;
                    let release_deadline = Instant::now() + PORT_RELEASE_WAIT_TIMEOUT;
                    while Instant::now() < release_deadline {
                        if let Ok(listener) = TcpListener::bind((Ipv4Addr::LOCALHOST, listen_port))
                        {
                            drop(listener);
                            port_released = true;
                            break;
                        }
                        tokio::time::sleep(PROXY_STOP_POLL_INTERVAL).await;
                    }
                    if !port_released {
                        proxy_version = Some(stale_health.version.clone());
                        warnings.push(format!(
                            "port {listen_port} did not clear after stopping stale proxy (version {}); restart Antigravity completely to pick up the new proxy",
                            stale_health.version
                        ));
                    } else {
                        let mut child = spawn_detached_proxy_daemon_with_exe(
                            &stable_bin,
                            listen_port,
                            &config_path,
                            DEFAULT_IDLE_SHUTDOWN_SECS,
                        )?;
                        match wait_for_daemon_healthy(
                            &probe,
                            &mut child,
                            WaitHealthyOptions {
                                port: listen_port,
                                total_timeout: PROXY_START_WAIT_TIMEOUT,
                                poll_interval: PROXY_START_POLL_INTERVAL,
                                probe_timeout: INSTALL_HEALTH_PROBE_TIMEOUT,
                                expected_version: Some(&version),
                            },
                        )
                        .await
                        {
                            WaitHealthyOutcome::Ready(new_health) => {
                                proxy_started = true;
                                proxy_replaced = true;
                                proxy_version = Some(new_health.version);
                            }
                            WaitHealthyOutcome::ExitedEarly {
                                status,
                                last_observed_version,
                            } => {
                                if let Some(stale_ver) = last_observed_version {
                                    proxy_version = Some(stale_ver.clone());
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
                                    proxy_version = Some(stale_ver.clone());
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
                    proxy_version = Some(stale_health.version.clone());
                    warnings.push(format!(
                        "running proxy on 127.0.0.1:{listen_port} has version {} (installed {version}), but could not be safely stopped via proxy.pid; restart Antigravity completely to pick up the new proxy",
                        stale_health.version
                    ));
                }
            }
            None => {
                let mut child = spawn_detached_proxy_daemon_with_exe(
                    &stable_bin,
                    listen_port,
                    &config_path,
                    DEFAULT_IDLE_SHUTDOWN_SECS,
                )?;
                match wait_for_daemon_healthy(
                    &probe,
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
                        proxy_started = true;
                        proxy_version = Some(new_health.version);
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
    }

    // 5. Launcher hook (Unix) or Start Menu shortcut (Windows).
    let mut launcher_integration: Option<IntegrationStatus> = None;
    #[cfg_attr(not(windows), allow(unused_mut))]
    let mut windows_shortcut: Option<PathBuf> = None;

    if !opts.no_launcher_hook {
        #[cfg(not(windows))]
        {
            let status = companion_core::integration::enable(&IntegrationOptions {
                shell: None,
                rc_file: None,
                binary_path: Some(stable_bin.clone()),
                data_dir: None,
                follow_rc_symlink: false,
            })?;
            launcher_integration = Some(status);
        }
        #[cfg(windows)]
        {
            launcher_integration = Some(companion_core::integration::status(
                &IntegrationOptions::default(),
            )?);
            match create_windows_start_menu_shortcut(&stable_bin) {
                Ok(path) => windows_shortcut = Some(path),
                Err(err) => warnings.push(err.to_string()),
            }
        }
    }

    // 6. Report ($PATH-aware & Cmd+Q-aware).
    let uninstall_command = if is_binary_on_path(STABLE_BIN_NAME) {
        format!("{BINARY_NAME} uninstall")
    } else {
        "npx -y agy-companion@latest uninstall".to_string()
    };
    let antigravity_running = detect_antigravity_running();

    let report = InstallReport {
        ok: true,
        version,
        binary_path: stable_bin,
        local_bin_symlink,
        plugin_dir: plugin_target,
        listen_port,
        port_auto_selected,
        launcher_integration,
        windows_shortcut,
        proxy_started,
        proxy_replaced,
        proxy_version,
        antigravity_running,
        uninstall_command,
        warnings,
    };

    if opts.json {
        let json_out = serde_json::to_string_pretty(&report)
            .map_err(|e| CliError::Serialization(e.to_string()))?;
        writeln!(out, "{json_out}")?;
        return Ok(());
    }

    writeln!(out, "Installed Antigravity Companion v{}", report.version)?;
    writeln!(out, "  Binary:   {}", report.binary_path.display())?;
    if let Some(ref link) = report.local_bin_symlink {
        writeln!(out, "  Symlink:  {}", link.display())?;
    }
    writeln!(out, "  Plugin:   {}", report.plugin_dir.display())?;
    if report.port_auto_selected {
        writeln!(
            out,
            "  Port:     {} (auto-selected; default {} was occupied)",
            report.listen_port, DEFAULT_LISTEN_PORT
        )?;
    } else {
        writeln!(out, "  Port:     {}", report.listen_port)?;
    }
    if let Some(ref integ) = report.launcher_integration {
        if let Some(ref rc_path) = integ.rc_path {
            writeln!(
                out,
                "  Launcher: {:?} ({})",
                integ.state(),
                rc_path.display()
            )?;
        } else {
            writeln!(out, "  Launcher: {:?}", integ.state())?;
        }
        if let Some(ref snippet) = integ.manual_snippet {
            writeln!(out, "  Manual RC snippet:\n{snippet}")?;
        }
    }
    if let Some(ref shortcut) = report.windows_shortcut {
        writeln!(out, "  Shortcut: {}", shortcut.display())?;
    }
    if report.proxy_replaced {
        writeln!(
            out,
            "  Proxy:    upgraded and running on http://127.0.0.1:{}",
            report.listen_port
        )?;
    } else if report.proxy_started {
        writeln!(
            out,
            "  Proxy:    running on http://127.0.0.1:{}",
            report.listen_port
        )?;
    } else if let Some(ref ver) = report.proxy_version {
        writeln!(
            out,
            "  Proxy:    already running v{ver} on http://127.0.0.1:{}",
            report.listen_port
        )?;
    }

    writeln!(out)?;
    if cfg!(target_os = "macos") {
        if report.antigravity_running {
            writeln!(
                out,
                "Next step: Antigravity is currently running — quit completely (Cmd+Q; closing the window is not enough) and reopen Antigravity from Dock or Spotlight."
            )?;
        } else {
            writeln!(
                out,
                "Next step: Open Antigravity from Dock or Spotlight (if it is running in the menu bar, quit completely with Cmd+Q first)."
            )?;
        }
    } else if cfg!(windows) {
        writeln!(
            out,
            "Next step: Quit Antigravity completely and launch 'Antigravity (Companion)' from the Start Menu (or run 'agy-companion launch')."
        )?;
    } else {
        writeln!(
            out,
            "Next step: Quit Antigravity completely and reopen it from your desktop launcher."
        )?;
    }
    writeln!(out, "Uninstall anytime: {}", report.uninstall_command)?;

    for warning in &report.warnings {
        writeln!(out, "warning: {warning}")?;
    }
    Ok(())
}

pub async fn run_uninstall<W: Write>(opts: UninstallOptions, out: &mut W) -> Result<(), CliError> {
    let mut warnings: Vec<String> = Vec::new();

    // 1. Validate inputs fail-closed before touching anything.
    let home = resolve_home_dir()?;
    let plugins_root = resolve_plugins_root(opts.plugins_dir.as_deref())?;
    let plugin_target = plugins_root.join(PLUGIN_DIR_NAME);

    let config_path = resolve_cli_config_path(opts.config.as_deref())?;
    let norm_config_path = normalized_metadata_path(&config_path);
    let probe =
        LoopbackProxyClient::new(INSTALL_HEALTH_PROBE_TIMEOUT, INSTALL_HEALTH_PROBE_TIMEOUT)?;
    let data_dir = companion_core::default_data_dir()?;
    let data_dir_safety = inspect_data_dir(&home, &data_dir, &mut warnings)?;
    let config_dir_safety = inspect_config_dir(&home, &norm_config_path, &mut warnings)?;

    let config = if config_dir_safety == ExistingDirSafety::SafeDir {
        match load_config_checked(&config_path) {
            Ok(cfg) => cfg,
            Err(err) => {
                warnings.push(format!(
                    "could not load config '{}' ({err}); continuing uninstall with defaults",
                    config_path.display()
                ));
                Config::default()
            }
        }
    } else {
        Config::default()
    };
    let runtime_data_dir = resolve_runtime_data_dir(&config).unwrap_or_else(|_| data_dir.clone());
    let runtime_differs =
        normalized_metadata_path(&runtime_data_dir) != normalized_metadata_path(&data_dir);
    let runtime_data_dir_safety = if !runtime_differs {
        data_dir_safety
    } else if data_dir_safety == ExistingDirSafety::Unsafe
        && usage_dir_under_dir(&runtime_data_dir, &data_dir)
    {
        ExistingDirSafety::Unsafe
    } else {
        inspect_existing_dir(&runtime_data_dir, "runtime data directory", &mut warnings)?
    };

    // Step 1: Strip ALL known shell RC files (Unix).
    #[cfg(not(windows))]
    let (rc_modified, rc_deleted, rc_manual_remaining) = {
        let zdotdir = std::env::var_os("ZDOTDIR")
            .filter(|v| !v.to_string_lossy().trim().is_empty())
            .map(PathBuf::from);
        let xdg_config_home = std::env::var_os("XDG_CONFIG_HOME")
            .filter(|v| !v.to_string_lossy().trim().is_empty())
            .map(PathBuf::from);
        match companion_core::integration::strip_all_known_rc_files(
            &home,
            zdotdir.as_deref(),
            xdg_config_home.as_deref(),
            opts.dry_run,
        ) {
            Ok(strip) => (
                strip.modified_files,
                strip.deleted_files,
                strip.skipped_manual_files,
            ),
            Err(err) => {
                warnings.push(format!("shell RC cleanup warning: {err}"));
                (Vec::new(), Vec::new(), Vec::new())
            }
        }
    };
    #[cfg(windows)]
    let (rc_modified, rc_deleted, rc_manual_remaining): (
        Vec<PathBuf>,
        Vec<PathBuf>,
        Vec<PathBuf>,
    ) = (Vec::new(), Vec::new(), Vec::new());

    // Step 2: Remove owned env.sh (instant global neutralization even on read-only/Nix RCs).
    let env_script = data_dir.join(companion_core::integration::ENV_SCRIPT_FILENAME);
    let env_script_removed = if data_dir_safety == ExistingDirSafety::SafeDir {
        remove_regular_file_if_present(&env_script, opts.dry_run, &mut warnings)?
    } else {
        false
    };

    // Step 3: CRITICAL ORDER — delete plugin dir BEFORE stopping proxy to beat SidecarManager respawn race.
    // Never traverse a symlinked or non-directory plugins_root (applies to both normal and --dry-run).
    let mut plugin_dir_removed = false;
    let default_plugins_under_home = home.join(".gemini").join("config").join("plugins");
    let plugins_root_safe = if opts.plugins_dir.is_none()
        && normalized_metadata_path(&plugins_root)
            == normalized_metadata_path(&default_plugins_under_home)
    {
        inspect_non_symlink_rel_chain(
            &home,
            &[".gemini", "config", "plugins"],
            "plugins root",
            &mut warnings,
        )? == ExistingDirSafety::SafeDir
    } else {
        inspect_existing_dir(&plugins_root, "plugins root", &mut warnings)?
            == ExistingDirSafety::SafeDir
    };
    if plugins_root_safe {
        match fs::symlink_metadata(normalized_metadata_path(&plugin_target)) {
            Ok(meta) if meta.file_type().is_symlink() => {
                // Unlink symlink ONLY; never delete the symlink target directory.
                if !opts.dry_run {
                    unlink_symlink(&plugin_target)?;
                }
                plugin_dir_removed = true;
            }
            Ok(meta) if meta.file_type().is_dir() => {
                match is_companion_plugin_dir(&plugin_target) {
                    Ok(true) => {
                        if !opts.dry_run {
                            fs::remove_dir_all(&plugin_target)?;
                        }
                        plugin_dir_removed = true;
                    }
                    Ok(false) => {
                        warnings.push(format!(
                        "refusing to remove plugin directory '{}': missing or non-companion plugin.json",
                        plugin_target.display()
                    ));
                    }
                    Err(err) => {
                        warnings.push(format!(
                            "refusing to remove plugin directory '{}': {err}",
                            plugin_target.display()
                        ));
                    }
                }
            }
            Ok(_) => {
                warnings.push(format!(
                    "refusing to remove non-directory plugin path '{}'",
                    plugin_target.display()
                ));
            }
            Err(err) if err.kind() == ErrorKind::NotFound => {}
            Err(err) => return Err(CliError::Io(err)),
        }
    }

    // Step 4: Stop verified companion proxy via 4-layer zero-unsafe PID check + remove Windows .lnk.
    let mut pid_paths = Vec::new();
    if runtime_data_dir_safety == ExistingDirSafety::SafeDir {
        pid_paths.push(runtime_data_dir.join(PROXY_PID_FILENAME));
    }
    if data_dir_safety == ExistingDirSafety::SafeDir {
        pid_paths.push(data_dir.join(PROXY_PID_FILENAME));
    }
    let pid_candidates = deduplicate_paths(pid_paths);
    let (proxy_stopped, proxy_pid) =
        stop_proxy_from_pid_files(&probe, &pid_candidates, None, opts.dry_run, &mut warnings).await;

    #[cfg(windows)]
    let windows_shortcut_removed = remove_windows_start_menu_shortcut(opts.dry_run, &mut warnings)?;
    #[cfg(not(windows))]
    let windows_shortcut_removed = false;

    // Step 5: Validate bin_dir before checking ~/.local/bin/agy-companion symlink ownership
    // (macOS /var -> /private/var canonicalization needs stable_bin to still exist) or deleting
    // stable_bin / .version / bin_dir.
    let bin_dir = data_dir.join("bin");
    let stable_bin = bin_dir.join(STABLE_BIN_NAME);
    let version_file = bin_dir.join(".version");
    let bin_dir_safety = match data_dir_safety {
        ExistingDirSafety::SafeDir => {
            inspect_existing_dir(&bin_dir, "bin directory", &mut warnings)?
        }
        ExistingDirSafety::NotFound => ExistingDirSafety::NotFound,
        ExistingDirSafety::Unsafe => ExistingDirSafety::Unsafe,
    };

    #[cfg(unix)]
    let local_bin_symlink_removed = if bin_dir_safety == ExistingDirSafety::Unsafe {
        false
    } else if inspect_non_symlink_rel_chain(
        &home,
        &[".local", "bin"],
        "local bin directory",
        &mut warnings,
    )? == ExistingDirSafety::SafeDir
    {
        let local_bin_link = home.join(".local").join("bin").join(BINARY_NAME);
        match fs::symlink_metadata(normalized_metadata_path(&local_bin_link)) {
            Ok(meta) if meta.file_type().is_symlink() => {
                if symlink_points_to_path(&local_bin_link, &stable_bin) {
                    if !opts.dry_run {
                        fs::remove_file(&local_bin_link)?;
                    }
                    true
                } else {
                    warnings.push(format!(
                        "preserving foreign symlink '{}'",
                        local_bin_link.display()
                    ));
                    false
                }
            }
            Ok(_) => {
                warnings.push(format!(
                    "preserving non-symlink file '{}'",
                    local_bin_link.display()
                ));
                false
            }
            Err(err) if err.kind() == ErrorKind::NotFound => false,
            Err(err) => {
                warnings.push(format!(
                    "failed inspecting '{}': {err}",
                    local_bin_link.display()
                ));
                false
            }
        }
    } else {
        false
    };
    #[cfg(not(unix))]
    let local_bin_symlink_removed = false;

    let binary_removed = if bin_dir_safety == ExistingDirSafety::SafeDir {
        #[cfg(not(windows))]
        {
            remove_regular_file_if_present(&stable_bin, opts.dry_run, &mut warnings)?
        }
        #[cfg(windows)]
        {
            match fs::symlink_metadata(normalized_metadata_path(&stable_bin)) {
                Ok(meta) if meta.file_type().is_file() && !meta.file_type().is_symlink() => {
                    if opts.dry_run {
                        true
                    } else if let Err(rm_err) = fs::remove_file(&stable_bin) {
                        let renamed = bin_dir
                            .join(format!(".{BINARY_NAME}.old.{}.exe", unique_temp_suffix()));
                        if fs::rename(&stable_bin, &renamed).is_ok() {
                            true
                        } else {
                            warnings.push(format!(
                                "could not remove running binary '{}': {rm_err}",
                                stable_bin.display()
                            ));
                            false
                        }
                    } else {
                        true
                    }
                }
                Ok(_) => false,
                Err(err) if err.kind() == ErrorKind::NotFound => false,
                Err(err) => return Err(CliError::Io(err)),
            }
        }
    } else {
        false
    };
    if bin_dir_safety == ExistingDirSafety::SafeDir {
        let _ = remove_regular_file_if_present(&version_file, opts.dry_run, &mut warnings)?;
        if !opts.dry_run {
            let _ = fs::remove_dir(&bin_dir);
        }
    }

    // Step 6: Preserve config & usage by default, or delete everything on --purge.
    // NEVER touch ~/.gemini/config/config.json or /Applications/Antigravity.app.
    let mut purged_paths: Vec<PathBuf> = Vec::new();
    let mut preserved_paths: Vec<PathBuf> = Vec::new();

    let mut usage_dirs: Vec<PathBuf> = Vec::new();
    if let Ok(u) = config.effective_usage_dir() {
        usage_dirs.push(u);
    }
    if let Ok(u) = companion_core::default_usage_dir() {
        usage_dirs.push(u);
    }
    let usage_dirs = deduplicate_paths(usage_dirs);

    if opts.purge {
        if config_dir_safety == ExistingDirSafety::SafeDir
            && remove_regular_file_if_present(&config_path, opts.dry_run, &mut warnings)?
        {
            purged_paths.push(config_path.clone());
        }
        for usage_dir in &usage_dirs {
            if (runtime_data_dir_safety == ExistingDirSafety::Unsafe
                && usage_dir_under_dir(usage_dir, &runtime_data_dir))
                || (data_dir_safety == ExistingDirSafety::Unsafe
                    && usage_dir_under_dir(usage_dir, &data_dir))
            {
                continue;
            }
            if remove_dir_if_non_symlink(usage_dir, opts.dry_run, &mut warnings)? {
                purged_paths.push(usage_dir.clone());
            }
        }
        let mut log_dirs = Vec::new();
        if runtime_data_dir_safety == ExistingDirSafety::SafeDir {
            log_dirs.push(runtime_data_dir.clone());
        }
        if data_dir_safety == ExistingDirSafety::SafeDir {
            log_dirs.push(data_dir.clone());
        }
        for dir in deduplicate_paths(log_dirs) {
            for fname in [PROXY_LOG_FILENAME, PROXY_LOG_ROTATED_FILENAME] {
                let log_path = dir.join(fname);
                if remove_regular_file_if_present(&log_path, opts.dry_run, &mut warnings)? {
                    purged_paths.push(log_path);
                }
            }
        }
        let sidecar_data_dir = home
            .join(".gemini")
            .join("antigravity")
            .join("sidecar_data")
            .join(PLUGIN_DIR_NAME);
        if inspect_non_symlink_rel_chain(
            &home,
            &[".gemini", "antigravity", "sidecar_data"],
            "sidecar data directory",
            &mut warnings,
        )? == ExistingDirSafety::SafeDir
            && remove_dir_if_non_symlink(&sidecar_data_dir, opts.dry_run, &mut warnings)?
        {
            purged_paths.push(sidecar_data_dir);
        }

        if !opts.dry_run {
            if config_dir_safety == ExistingDirSafety::SafeDir
                && let Some(cfg_parent) = norm_config_path.parent()
                && cfg_parent.file_name().and_then(|n| n.to_str()) == Some(PLUGIN_DIR_NAME)
            {
                let _ = fs::remove_dir(cfg_parent);
            }
            if data_dir_safety == ExistingDirSafety::SafeDir {
                let _ = fs::remove_dir(&data_dir);
            }
            if runtime_differs && runtime_data_dir_safety == ExistingDirSafety::SafeDir {
                let _ = fs::remove_dir(&runtime_data_dir);
            }
        }
    } else {
        if config_dir_safety == ExistingDirSafety::SafeDir
            && fs::symlink_metadata(&norm_config_path).is_ok()
        {
            preserved_paths.push(config_path.clone());
        }
        for usage_dir in &usage_dirs {
            if (runtime_data_dir_safety == ExistingDirSafety::Unsafe
                && usage_dir_under_dir(usage_dir, &runtime_data_dir))
                || (data_dir_safety == ExistingDirSafety::Unsafe
                    && usage_dir_under_dir(usage_dir, &data_dir))
            {
                continue;
            }
            if fs::symlink_metadata(normalized_metadata_path(usage_dir)).is_ok() {
                preserved_paths.push(usage_dir.clone());
            }
        }
    }

    let report = UninstallReport {
        ok: true,
        dry_run: opts.dry_run,
        purge: opts.purge,
        rc_modified,
        rc_deleted,
        rc_manual_remaining,
        env_script_removed,
        plugin_dir_removed,
        proxy_stopped,
        proxy_pid,
        binary_removed,
        local_bin_symlink_removed,
        windows_shortcut_removed,
        purged_paths,
        preserved_paths,
        warnings,
    };

    if opts.json {
        let json_out = serde_json::to_string_pretty(&report)
            .map_err(|e| CliError::Serialization(e.to_string()))?;
        writeln!(out, "{json_out}")?;
        return Ok(());
    }

    if report.dry_run {
        writeln!(out, "Dry run (no files modified, no processes stopped):")?;
    } else {
        writeln!(out, "Uninstalled Antigravity Companion")?;
    }
    writeln!(
        out,
        "  Plugin removed:       {}",
        if report.plugin_dir_removed {
            "yes"
        } else {
            "no"
        }
    )?;
    writeln!(
        out,
        "  env.sh removed:       {}",
        if report.env_script_removed {
            "yes"
        } else {
            "no"
        }
    )?;
    writeln!(
        out,
        "  Proxy stopped:        {}",
        if report.proxy_stopped { "yes" } else { "no" }
    )?;
    writeln!(
        out,
        "  Binary removed:       {}",
        if report.binary_removed { "yes" } else { "no" }
    )?;
    if !report.preserved_paths.is_empty() {
        writeln!(out, "  Preserved (re-run with --purge to delete):")?;
        for path in &report.preserved_paths {
            writeln!(out, "    - {}", path.display())?;
        }
    }
    if !report.purged_paths.is_empty() {
        writeln!(out, "  Purged:")?;
        for path in &report.purged_paths {
            writeln!(out, "    - {}", path.display())?;
        }
    }
    if !report.rc_manual_remaining.is_empty() {
        writeln!(
            out,
            "  Manual RC cleanup required (symlinked/read-only files):"
        )?;
        for path in &report.rc_manual_remaining {
            writeln!(out, "    - {}", path.display())?;
        }
    }
    for warning in &report.warnings {
        writeln!(out, "warning: {warning}")?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;
    use tempfile::TempDir;

    fn collect_repo_files(root: &Path, dir: &Path, out: &mut BTreeMap<String, PathBuf>) {
        let mut entries: Vec<_> = fs::read_dir(dir)
            .expect("read_dir")
            .map(|r| r.expect("dir entry"))
            .collect();
        entries.sort_by_key(|e| e.file_name());
        for entry in entries {
            let path = entry.path();
            let meta = fs::symlink_metadata(&path).expect("symlink_metadata");
            assert!(
                !meta.file_type().is_symlink(),
                "plugin directory must not contain symlinks: {}",
                path.display()
            );
            if meta.file_type().is_dir() {
                collect_repo_files(root, &path, out);
            } else {
                assert!(
                    meta.file_type().is_file(),
                    "plugin entry must be a regular file: {}",
                    path.display()
                );
                let rel = path
                    .strip_prefix(root)
                    .expect("strip_prefix")
                    .to_string_lossy()
                    .replace('\\', "/");
                out.insert(rel, path);
            }
        }
    }

    #[test]
    fn embedded_plugin_bundle_matches_repo_plugin_dir_byte_for_byte() {
        let repo_plugin_dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../plugin");
        let mut disk_files = BTreeMap::new();
        collect_repo_files(&repo_plugin_dir, &repo_plugin_dir, &mut disk_files);

        assert_eq!(
            EMBEDDED_PLUGIN_FILES.len(),
            7,
            "expected exactly 7 embedded plugin files"
        );
        assert_eq!(
            EMBEDDED_PLUGIN_FILES.last().expect("non-empty").rel_path,
            "plugin.json",
            "plugin.json must be written last during staging"
        );
        assert_eq!(
            disk_files.len(),
            EMBEDDED_PLUGIN_FILES.len(),
            "repo plugin/ file count drifted from EMBEDDED_PLUGIN_FILES: {:?}",
            disk_files.keys().collect::<Vec<_>>()
        );

        for entry in EMBEDDED_PLUGIN_FILES {
            let disk_path = disk_files
                .get(entry.rel_path)
                .unwrap_or_else(|| panic!("missing repo plugin file '{}'", entry.rel_path));
            let disk_bytes = fs::read(disk_path).expect("read disk plugin file");
            assert_eq!(
                disk_bytes,
                entry.content.as_bytes(),
                "embedded content drifted from disk for '{}'",
                entry.rel_path
            );
            #[cfg(unix)]
            if entry.executable {
                use std::os::unix::fs::PermissionsExt;
                let mode = fs::metadata(disk_path)
                    .expect("metadata")
                    .permissions()
                    .mode();
                assert_ne!(
                    mode & 0o111,
                    0,
                    "expected executable bit on '{}'",
                    entry.rel_path
                );
            }
        }
    }

    #[test]
    fn windows_manifest_generators_emit_direct_exe_json_without_bash() {
        let bin_path =
            Path::new(r"C:\Users\alice\AppData\Local\agy-companion\bin\agy-companion.exe");

        let mcp_raw = render_windows_mcp_config(bin_path).expect("render windows mcp config");
        assert!(
            !mcp_raw.contains("bash") && !mcp_raw.contains("run-companion.sh"),
            "Windows mcp_config.json must not reference bash: {mcp_raw}"
        );
        let mcp_json: serde_json::Value = serde_json::from_str(&mcp_raw).expect("parse mcp json");
        assert_eq!(
            mcp_json["mcpServers"]["hub"]["command"],
            bin_path.to_string_lossy().as_ref()
        );
        assert_eq!(
            mcp_json["mcpServers"]["hub"]["args"],
            serde_json::json!(["ui-mcp", "--data-dir=${PLUGIN_DATA}"])
        );

        let sidecar_raw =
            render_windows_sidecar_config(bin_path).expect("render windows sidecar config");
        assert!(
            !sidecar_raw.contains("bash") && !sidecar_raw.contains("run-companion.sh"),
            "Windows sidecar.json must not reference bash: {sidecar_raw}"
        );
        let sidecar_json: serde_json::Value =
            serde_json::from_str(&sidecar_raw).expect("parse sidecar json");
        assert_eq!(
            sidecar_json.as_object().expect("sidecar object").len(),
            5,
            "Windows sidecar.json must contain only the 5 strict-valid schema keys: {sidecar_raw}"
        );
        assert!(
            sidecar_json.get("name").is_none(),
            "unknown field 'name' fails agy plugin validate --strict"
        );
        assert!(
            sidecar_json.get("health_check").is_none(),
            "unknown field 'health_check' fails agy plugin validate --strict"
        );
        assert_eq!(sidecar_json["command"], bin_path.to_string_lossy().as_ref());
        assert_eq!(
            sidecar_json["args"],
            serde_json::json!(["serve", "--adopt-running"])
        );
        assert_eq!(sidecar_json["restart_policy"], "on-failure");
        assert_eq!(sidecar_json["display_name"], "Antigravity Companion Proxy");
        assert_eq!(
            sidecar_json["description"],
            "Local loopback Gemini pass-through proxy with SSE usageMetadata telemetry and config hot-reload."
        );
    }

    #[test]
    fn windows_shortcut_path_and_ps_script_escape_quotes_and_hide_console() {
        let app_data = Path::new(r"C:\Users\O'Brien\AppData\Roaming");
        let shortcut = resolve_windows_shortcut_path(app_data);
        assert!(
            shortcut.to_string_lossy().ends_with(WINDOWS_SHORTCUT_NAME),
            "shortcut path should end with {WINDOWS_SHORTCUT_NAME}: {}",
            shortcut.display()
        );

        let bin = Path::new(r"C:\Users\O'Brien\AppData\Local\agy-companion\bin\agy-companion.exe");
        let icon =
            Path::new(r"C:\Users\O'Brien\AppData\Local\Programs\Antigravity\Antigravity.exe");
        let script = render_windows_shortcut_ps_script(&shortcut, bin, Some(icon));
        assert!(script.contains("O''Brien"), "single quotes must be doubled");
        assert!(script.contains("$s.Arguments = 'launch'"));
        assert!(script.contains("$s.WindowStyle = 7"));
        assert!(script.contains("IconLocation"));
        assert_eq!(CREATE_NO_WINDOW, 0x0800_0000);

        // PowerShell 5.1 treats Unicode single-quote variants as string delimiters too.
        for q in ['\'', '\u{2018}', '\u{2019}', '\u{201A}', '\u{201B}'] {
            let input = format!("a{q}b");
            let expected = format!("'a{q}{q}b'");
            assert_eq!(
                ps_single_quote(&input),
                expected,
                "PowerShell 5.1 quote U+{:04X} must be doubled",
                q as u32
            );
        }

        // Newline characters must be rejected before embedding paths in PowerShell scripts.
        assert!(reject_ps_path_newlines(bin).is_ok());
        assert!(reject_ps_path_newlines(Path::new("C:\\Users\\alice\nbad\\app.exe")).is_err());
        assert!(reject_ps_path_newlines(Path::new("C:\\Users\\alice\rbad\\app.exe")).is_err());
    }

    #[test]
    fn is_companion_serve_process_handles_quoted_windows_cmdline_with_serve_in_dir_name() {
        assert!(!is_companion_serve_process(
            r"C:\Users\My serve dir\agy-companion.exe",
            r#""C:\Users\My serve dir\agy-companion.exe" ui-mcp"#,
            ""
        ));
        assert!(is_companion_serve_process(
            r"C:\Users\My serve dir\agy-companion.exe",
            r#""C:\Users\My serve dir\agy-companion.exe" serve --adopt-running"#,
            ""
        ));
    }

    #[test]
    fn is_companion_serve_process_requires_serve_as_first_arg_and_handles_quoted_nul_and_mixed_case_exe()
     {
        // 1. ui-mcp with `serve` inside --data-dir value (quoted Windows & unquoted Unix) -> false
        assert!(!is_companion_serve_process(
            r"C:\Users\Alice\agy-companion.exe",
            r#""C:\Users\Alice\agy-companion.exe" ui-mcp --data-dir="C:\Users\my serve dir\data""#,
            ""
        ));
        assert!(!is_companion_serve_process(
            "/Users/alice/agy-companion",
            "/Users/alice/agy-companion ui-mcp --data-dir=/tmp/my serve dir/data",
            ""
        ));

        // 2. ui-mcp with flag value literally named `serve` -> false
        assert!(!is_companion_serve_process(
            "/Users/alice/agy-companion",
            "/Users/alice/agy-companion ui-mcp --config serve",
            ""
        ));

        // 3. Windows case-mismatched quoted CommandLine -> false for ui-mcp, true for serve
        assert!(!is_companion_serve_process(
            r"C:\Users\My serve dir\agy-companion.exe",
            r#""c:\users\my serve dir\agy-companion.exe" ui-mcp"#,
            ""
        ));
        assert!(is_companion_serve_process(
            r"C:\Users\My serve dir\agy-companion.exe",
            r#""c:\users\my serve dir\agy-companion.exe" serve"#,
            ""
        ));

        // 4. Linux NUL-delimited symlinked argv[0] containing ` serve ` -> false for ui-mcp, true for serve
        assert!(!is_companion_serve_process(
            "/private/tmp/my serve dir/agy-companion",
            "/tmp/my serve dir/agy-companion\0ui-mcp\0",
            ""
        ));
        assert!(!is_companion_serve_process(
            "/private/tmp/my serve dir/agy-companion",
            "/tmp/my serve dir/agy-companion\0ui-mcp\0--config\0serve\0",
            ""
        ));
        assert!(is_companion_serve_process(
            "/private/tmp/my serve dir/agy-companion",
            "/tmp/my serve dir/agy-companion\0serve\0",
            ""
        ));

        // 5. Mixed-case .Exe suffix -> true for serve
        assert!(is_companion_serve_process(
            r"C:\Users\Alice\agy-companion.Exe",
            r#""C:\Users\Alice\agy-companion.Exe" serve"#,
            ""
        ));

        // Non-ASCII UTF-8 basename must never panic during suffix check
        assert!(!is_companion_serve_process(
            "/tmp/😀a",
            "/tmp/😀a serve",
            ""
        ));

        // 6. Unquoted mismatched path containing ` serve ` and 1-arg Linux NUL cmdline -> false;
        //    8.3 Windows short path with companion basename -> true.
        assert!(!is_companion_serve_process(
            "/real/bin/agy-companion",
            "/tmp/my serve dir/agy-companion",
            ""
        ));
        assert!(!is_companion_serve_process(
            "/real/bin/agy-companion",
            "/tmp/my serve dir/agy-companion\0",
            ""
        ));
        assert!(!is_companion_serve_process(
            r"C:\real\agy-companion.exe",
            r"C:\Users\my serve dir\agy-companion.exe ui-mcp",
            ""
        ));
        assert!(is_companion_serve_process(
            r"C:\Users\Jane Doe\AppData\Local\agy-companion\bin\agy-companion.exe",
            r"C:\Users\JANEDO~1\AppData\Local\agy-companion\bin\agy-companion.exe serve --port 4141",
            ""
        ));
    }

    #[test]
    fn is_companion_serve_process_accepts_companion_and_rejects_unrelated_processes() {
        assert!(is_companion_serve_process(
            "/Users/alice/.local/share/agy-companion/bin/agy-companion",
            "/Users/alice/.local/share/agy-companion/bin/agy-companion serve --adopt-running --port 4141",
            "/Users/alice/.local/share/agy-companion/bin/agy-companion"
        ));
        assert!(is_companion_serve_process(
            "/Users/alice/My Folder/agy-companion",
            "/Users/alice/My Folder/agy-companion serve --port 4142",
            ""
        ));
        assert!(is_companion_serve_process(
            r"C:\Users\alice\AppData\Local\agy-companion\bin\agy-companion.exe",
            r"C:\Users\alice\AppData\Local\agy-companion\bin\agy-companion.exe serve --port 4141",
            ""
        ));
        assert!(is_companion_serve_process(
            r"C:\Users\alice\AppData\Local\agy-companion\bin\.agy-companion.old.1234.exe",
            r"C:\Users\alice\AppData\Local\agy-companion\bin\.agy-companion.old.1234.exe serve",
            ""
        ));

        // Unrelated processes mentioning agy-companion / serve in arguments MUST be rejected.
        assert!(!is_companion_serve_process(
            "/usr/bin/python3",
            "/usr/bin/python3 -c 'import time; # agy-companion serve; time.sleep(60)'",
            "/Users/alice/.local/share/agy-companion/bin/agy-companion"
        ));
        // Non-serve subcommand of agy-companion MUST be rejected.
        assert!(!is_companion_serve_process(
            "/Users/alice/.local/share/agy-companion/bin/agy-companion",
            "/Users/alice/.local/share/agy-companion/bin/agy-companion ui-mcp",
            ""
        ));
        // PID sanity checks reject PID 0, PID 1, self PID, and overflow PIDs.
        for bad_pid in [0u32, 1u32, std::process::id(), (i32::MAX as u32) + 1] {
            let info = ProxyPidInfo {
                pid: bad_pid,
                port: 4141,
                version: "0.1.0".to_string(),
                started_at_ms: 0,
                binary_path: String::new(),
            };
            assert!(
                matches!(
                    verify_proxy_pid_process(&info).expect("verify"),
                    PidVerifyResult::UnrelatedProcess { .. }
                ),
                "bad PID {bad_pid} must be rejected as UnrelatedProcess"
            );
        }
    }

    #[tokio::test]
    async fn select_fresh_install_port_falls_back_when_first_port_is_occupied_and_fails_when_exhausted()
     {
        // Use two disjoint non-ephemeral loopback port ranges outside macOS (49152..=65535) and
        // Linux (32768..=60999) ephemeral pools, with zero drop-and-rebind windows (which can
        // otherwise race on macOS when a sibling test spawns a subprocess between socket() and
        // FIOCLEX and transiently holds the dropped listener fd).
        let mut exhausted: Option<(u16, TcpListener, TcpListener, TcpListener)> = None;
        for candidate in (24_000u16..26_000).step_by(3) {
            if let (Ok(e1), Ok(e2), Ok(e3)) = (
                TcpListener::bind((Ipv4Addr::LOCALHOST, candidate)),
                TcpListener::bind((Ipv4Addr::LOCALHOST, candidate + 1)),
                TcpListener::bind((Ipv4Addr::LOCALHOST, candidate + 2)),
            ) {
                exhausted = Some((candidate, e1, e2, e3));
                break;
            }
        }
        let (ex_base, e1, e2, e3) =
            exhausted.expect("contiguous non-ephemeral loopback range for exhaustion");

        // All 3 ports held -> exhausted range must fail closed.
        let probe = LoopbackProxyClient::with_default_timeouts().expect("probe client");
        assert!(
            select_fresh_install_port(&probe, ex_base, ex_base + 2, false)
                .await
                .is_err(),
            "exhausted port range must fail closed"
        );
        drop((e1, e2, e3));

        // Separate range for fallback: hold fb_base and fb_base+2 while never binding fb_base+1
        // in the test process, ensuring deterministic fallback to fb_base+1 with zero re-bind.
        let mut fallback: Option<(u16, TcpListener, TcpListener)> = None;
        for candidate in (26_000u16..29_000).step_by(3) {
            if let (Ok(f1), Ok(f3)) = (
                TcpListener::bind((Ipv4Addr::LOCALHOST, candidate)),
                TcpListener::bind((Ipv4Addr::LOCALHOST, candidate + 2)),
            ) && std::net::TcpStream::connect((Ipv4Addr::LOCALHOST, candidate + 1)).is_err()
            {
                fallback = Some((candidate, f1, f3));
                break;
            }
        }
        let (fb_base, f1, f3) =
            fallback.expect("contiguous non-ephemeral loopback range for fallback");

        let (chosen, auto_selected) =
            select_fresh_install_port(&probe, fb_base, fb_base + 2, false)
                .await
                .expect("should select fallback port");
        assert_eq!((chosen, auto_selected), (fb_base + 1, true));
        drop((f1, f3));
    }

    #[test]
    fn two_step_plugin_swap_writes_all_files_replaces_existing_and_leaves_zero_temp_dirs() {
        let tmp = TempDir::new().expect("tempdir");
        let plugins_root = tmp.path().join("plugins");
        fs::create_dir_all(&plugins_root).expect("mkdir plugins");
        let plugin_target = plugins_root.join(PLUGIN_DIR_NAME);
        let stable_bin = tmp.path().join("bin").join(STABLE_BIN_NAME);

        // Fresh swap
        let (stage1, guard1) = write_staged_plugin_dir(&plugins_root, &stable_bin).expect("stage1");
        let mut warnings = Vec::new();
        swap_plugin_dir_two_step(
            &plugins_root,
            &plugin_target,
            &stage1,
            guard1,
            &mut warnings,
        )
        .expect("fresh swap");
        assert!(warnings.is_empty(), "unexpected warnings: {warnings:?}");
        assert!(is_companion_plugin_dir(&plugin_target).expect("valid plugin"));

        // Upgrade swap over existing dir with extra old file -> old file gone, zero .stage./.backup leftovers
        let old_marker = plugin_target.join("stale_marker.txt");
        fs::write(&old_marker, "old").expect("write stale");
        let (stage2, guard2) = write_staged_plugin_dir(&plugins_root, &stable_bin).expect("stage2");
        swap_plugin_dir_two_step(
            &plugins_root,
            &plugin_target,
            &stage2,
            guard2,
            &mut warnings,
        )
        .expect("upgrade swap");
        assert!(!old_marker.exists(), "old plugin files must be replaced");
        assert!(is_companion_plugin_dir(&plugin_target).expect("still valid plugin"));

        let entries: Vec<String> = fs::read_dir(&plugins_root)
            .expect("read_dir")
            .map(|e| e.expect("entry").file_name().to_string_lossy().into_owned())
            .collect();
        assert_eq!(
            entries,
            vec![PLUGIN_DIR_NAME.to_string()],
            "no .stage. or .backup leftovers permitted"
        );
    }

    #[test]
    fn is_companion_plugin_dir_rejects_foreign_and_symlink_manifests() {
        let tmp = TempDir::new().expect("tempdir");
        let foreign = tmp.path().join("foreign");
        fs::create_dir_all(&foreign).expect("mkdir");
        fs::write(
            foreign.join("plugin.json"),
            r#"{"name":"other-plugin","version":"1.0.0"}"#,
        )
        .expect("write");
        assert!(!is_companion_plugin_dir(&foreign).expect("foreign"));

        let missing = tmp.path().join("missing");
        fs::create_dir_all(&missing).expect("mkdir");
        assert!(!is_companion_plugin_dir(&missing).expect("missing"));

        #[cfg(unix)]
        {
            let sym_dir = tmp.path().join("sym_manifest");
            fs::create_dir_all(&sym_dir).expect("mkdir");
            let real_manifest = tmp.path().join("real.json");
            fs::write(
                &real_manifest,
                r#"{"name":"agy-companion","version":"0.1.0"}"#,
            )
            .expect("write real");
            std::os::unix::fs::symlink(&real_manifest, sym_dir.join("plugin.json"))
                .expect("symlink");
            assert!(!is_companion_plugin_dir(&sym_dir).expect("symlink manifest rejected"));
        }
    }

    #[tokio::test]
    async fn stop_proxy_from_pid_files_refuses_mismatched_port_and_preserves_pid_file() {
        let tmp = TempDir::new().expect("tempdir");
        let pid_path = tmp.path().join(PROXY_PID_FILENAME);
        let pid_info = ProxyPidInfo {
            pid: 99_999,
            port: 4142,
            version: "0.0.9".to_string(),
            started_at_ms: 1_700_000_000_000,
            binary_path: "/usr/local/bin/agy-companion".to_string(),
        };
        let encoded = serde_json::to_string_pretty(&pid_info).expect("serialize pid");
        fs::write(&pid_path, encoded).expect("write pid file");

        let probe = LoopbackProxyClient::with_default_timeouts().expect("probe client");
        let mut warnings = Vec::new();
        let (stopped, pid) = stop_proxy_from_pid_files(
            &probe,
            std::slice::from_ref(&pid_path),
            Some(4141),
            false,
            &mut warnings,
        )
        .await;
        assert!(!stopped, "mismatched port PID must not be stopped");
        assert_eq!(pid, None);
        assert!(
            pid_path.exists(),
            "mismatched-port proxy.pid must NOT be deleted"
        );
        assert!(
            warnings
                .iter()
                .any(|w| w.contains("proxy.pid port 4142 does not match target listen_port 4141")),
            "expected port mismatch warning, got: {warnings:?}"
        );
    }

    #[test]
    fn remove_pid_file_if_matches_respects_expected_pid_and_preserves_symlink() {
        let tmp = TempDir::new().expect("tempdir");
        let pid_path = tmp.path().join(PROXY_PID_FILENAME);

        let write_pid = |path: &Path, pid: u32| {
            let info = ProxyPidInfo {
                pid,
                port: 4141,
                version: "0.1.0".to_string(),
                started_at_ms: 1_700_000_000_000,
                binary_path: "/usr/local/bin/agy-companion".to_string(),
            };
            fs::write(path, serde_json::to_string_pretty(&info).expect("json")).expect("write pid");
        };

        // 1. Matching PID -> deletes proxy.pid.
        write_pid(&pid_path, 42_001);
        remove_pid_file_if_matches(&pid_path, 42_001);
        assert!(!pid_path.exists(), "matching proxy.pid should be deleted");

        // 2. Different PID (simulating standby takeover) -> preserves proxy.pid.
        write_pid(&pid_path, 42_002);
        remove_pid_file_if_matches(&pid_path, 42_001);
        assert!(
            pid_path.is_file(),
            "standby proxy.pid with newer PID must be preserved"
        );
        assert_eq!(
            read_proxy_pid(&pid_path).expect("read").expect("some").pid,
            42_002
        );

        // 3. Symlinked proxy.pid pointing to a matching target -> neither symlink nor target unlinked.
        #[cfg(unix)]
        {
            fs::remove_file(&pid_path).expect("clean regular pid");
            let real_target = tmp.path().join("real_target.pid");
            write_pid(&real_target, 42_001);
            std::os::unix::fs::symlink(&real_target, &pid_path).expect("symlink pid");

            remove_pid_file_if_matches(&pid_path, 42_001);
            assert!(
                fs::symlink_metadata(&pid_path)
                    .expect("symlink metadata")
                    .file_type()
                    .is_symlink(),
                "symlinked proxy.pid must not be unlinked"
            );
            assert!(
                real_target.is_file(),
                "symlink target must not be followed or removed"
            );
        }
    }

    #[test]
    fn swap_plugin_dir_two_step_rolls_back_on_directory_and_symlink_failure_and_rejects_non_dir() {
        let tmp = TempDir::new().expect("tempdir");

        // (a) Directory-upgrade rollback: missing stage_dir causes second rename to fail;
        //     backup_dir must be restored to plugin_target with zero .backup leftovers.
        {
            let plugins_root = tmp.path().join("dir_rollback");
            fs::create_dir_all(&plugins_root).expect("mkdir");
            let plugin_target = plugins_root.join(PLUGIN_DIR_NAME);
            fs::create_dir_all(&plugin_target).expect("mkdir target");
            let sentinel = plugin_target.join("original.txt");
            fs::write(&sentinel, "KEEP_ORIGINAL\n").expect("write sentinel");

            let missing_stage = plugins_root.join(".agy-companion.stage.missing");
            let mut warnings = Vec::new();
            let res = swap_plugin_dir_two_step(
                &plugins_root,
                &plugin_target,
                &missing_stage,
                TempPathGuard::dir(missing_stage.clone()),
                &mut warnings,
            );
            assert!(res.is_err(), "swap with missing stage_dir must fail");
            assert_eq!(
                fs::read_to_string(&sentinel).expect("restored sentinel"),
                "KEEP_ORIGINAL\n"
            );
            let entries: Vec<String> = fs::read_dir(&plugins_root)
                .expect("read_dir")
                .map(|e| e.expect("entry").file_name().to_string_lossy().into_owned())
                .collect();
            assert_eq!(
                entries,
                vec![PLUGIN_DIR_NAME.to_string()],
                "rollback must leave zero .backup leftovers"
            );
        }

        // (b) Unix symlink-swap rollback: unlink_symlink succeeds, missing stage_dir fails rename;
        //     original symlink pointing to dev_target must be restored intact.
        #[cfg(unix)]
        {
            let plugins_root = tmp.path().join("sym_rollback");
            let dev_target = tmp.path().join("dev_target");
            fs::create_dir_all(&plugins_root).expect("mkdir");
            fs::create_dir_all(&dev_target).expect("mkdir dev");
            fs::write(dev_target.join("dev.txt"), "DEV_OK\n").expect("write dev");
            let plugin_target = plugins_root.join(PLUGIN_DIR_NAME);
            std::os::unix::fs::symlink(&dev_target, &plugin_target).expect("symlink");

            let missing_stage = plugins_root.join(".agy-companion.stage.missing");
            let mut warnings = Vec::new();
            let res = swap_plugin_dir_two_step(
                &plugins_root,
                &plugin_target,
                &missing_stage,
                TempPathGuard::dir(missing_stage.clone()),
                &mut warnings,
            );
            assert!(
                res.is_err(),
                "symlink swap with missing stage_dir must fail"
            );
            assert!(
                fs::symlink_metadata(&plugin_target)
                    .expect("restored symlink meta")
                    .file_type()
                    .is_symlink(),
                "original symlink must be restored on rollback"
            );
            assert_eq!(
                fs::read_link(&plugin_target).expect("read_link"),
                dev_target
            );
            assert_eq!(
                fs::read_to_string(dev_target.join("dev.txt")).expect("dev file"),
                "DEV_OK\n"
            );
        }

        // (c) Non-directory plugin_target rejection: regular file at plugin_target must be
        //     rejected with CliError::Install, untouched, and stage_dir cleaned up.
        {
            let plugins_root = tmp.path().join("non_dir");
            fs::create_dir_all(&plugins_root).expect("mkdir");
            let plugin_target = plugins_root.join(PLUGIN_DIR_NAME);
            fs::write(&plugin_target, "NOT_A_DIR\n").expect("write file");

            let stable_bin = tmp.path().join("bin").join(STABLE_BIN_NAME);
            let (stage_dir, stage_guard) =
                write_staged_plugin_dir(&plugins_root, &stable_bin).expect("stage");
            let mut warnings = Vec::new();
            let err = swap_plugin_dir_two_step(
                &plugins_root,
                &plugin_target,
                &stage_dir,
                stage_guard,
                &mut warnings,
            )
            .expect_err("non-directory plugin_target must be rejected");
            assert!(
                matches!(err, CliError::Install(_)),
                "expected CliError::Install, got: {err:?}"
            );
            assert_eq!(
                fs::read_to_string(&plugin_target).expect("file untouched"),
                "NOT_A_DIR\n"
            );
            assert!(
                !stage_dir.exists(),
                "stage_guard must clean up stage_dir on error"
            );
        }
    }

    #[test]
    fn dir_safety_helpers_reject_symlinks_non_dirs_and_match_normalized_usage_dir_ancestry() {
        let tmp = TempDir::new().expect("tempdir");
        let real_dir = tmp.path().join("real_dir");
        fs::create_dir_all(&real_dir).expect("mkdir real_dir");
        let reg_file = tmp.path().join("not_a_dir");
        fs::write(&reg_file, "file\n").expect("write file");
        let missing_dir = tmp.path().join("does_not_exist");

        let mut warnings = Vec::new();
        assert_eq!(
            inspect_existing_dir(&real_dir, "bin directory", &mut warnings).expect("real"),
            ExistingDirSafety::SafeDir
        );
        assert_eq!(
            inspect_existing_dir(&missing_dir, "bin directory", &mut warnings).expect("missing"),
            ExistingDirSafety::NotFound
        );
        assert!(warnings.is_empty());

        assert_eq!(
            inspect_existing_dir(&reg_file, "bin directory", &mut warnings).expect("file"),
            ExistingDirSafety::Unsafe
        );
        assert_eq!(warnings.len(), 1);
        assert!(warnings[0].contains("refusing to traverse symlinked/non-directory bin directory"));

        // usage_dir_under_dir normalizes trailing slashes and `.` segments.
        let base = tmp.path().join("sym_parent");
        assert!(usage_dir_under_dir(&base, &base));
        assert!(usage_dir_under_dir(
            &PathBuf::from(format!("{}/", base.display())),
            &base
        ));
        assert!(usage_dir_under_dir(&base.join("usage"), &base));
        assert!(usage_dir_under_dir(
            &PathBuf::from(format!("{}/usage/.", base.display())),
            &base
        ));
        assert!(!usage_dir_under_dir(
            &tmp.path().join("other").join("usage"),
            &base
        ));

        #[cfg(unix)]
        {
            let target_parent = tmp.path().join("target_parent");
            fs::create_dir_all(target_parent.join("child")).expect("mkdir target");
            let chain_root = tmp.path().join("chain_root");
            fs::create_dir_all(chain_root.join("a")).expect("mkdir a");
            std::os::unix::fs::symlink(&target_parent, chain_root.join("a").join("b"))
                .expect("symlink b");

            let mut chain_warnings = Vec::new();
            assert_eq!(
                inspect_non_symlink_rel_chain(
                    &chain_root,
                    &["a", "b", "child"],
                    "sidecar data directory",
                    &mut chain_warnings
                )
                .expect("chain check"),
                ExistingDirSafety::Unsafe
            );
            assert_eq!(chain_warnings.len(), 1);
            assert!(
                chain_warnings[0].contains(
                    "refusing to traverse symlinked/non-directory sidecar data directory"
                )
            );
        }
    }
}
