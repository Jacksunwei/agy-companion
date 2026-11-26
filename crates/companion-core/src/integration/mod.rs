mod auto_enable;
mod disable;
mod enable;
mod fs_inspect;
mod lifecycle;
mod lock;
mod markers;
mod strip;
mod templates;
#[cfg(test)]
mod tests;

use std::fmt;
use std::fs;
use std::net::Ipv4Addr;
use std::path::{Path, PathBuf};
use std::str::FromStr;

use serde::{Deserialize, Serialize};
use url::{Host, Url};

use crate::config::{CoreError, TargetPlatform, default_data_dir, resolve_xdg_base};
use crate::fs_safe;

use self::lifecycle::LifecycleEnv;
use self::templates::validate_shell_safe_path;

pub use self::auto_enable::{
    AutoEnableOutcome, disable_and_persist, disable_and_persist_with_hybrid_active,
    enable_and_persist, enable_and_persist_with_hybrid_active, ensure_first_run_launcher_enabled,
    ensure_first_run_launcher_enabled_with_hybrid_active, set_launcher_auto_start_locked,
};
pub use self::lock::{INTEGRATION_LOCK_FILENAME, IntegrationLock};
pub use self::strip::strip_all_known_rc_files;
pub use self::templates::render_rc_snippet;

pub const HOOK_BEGIN_MARKER: &str =
    "# >>> agy-companion (Antigravity Dock/Launcher integration) >>>";
pub const HOOK_END_MARKER: &str = "# <<< agy-companion <<<";
pub(crate) const MAX_RC_FILE_BYTES: u64 = 1024 * 1024;
pub const ENV_SCRIPT_FILENAME: &str = "env.sh";
pub(crate) const FISH_DROPIN_FILENAME: &str = "agy-companion.fish";
pub const DEFAULT_RESTORE_PREFIX: &str = "AGY_COMPANION_ORIG_";
pub(crate) const PORTABLE_ENV_SCRIPT_EXPR: &str =
    "${XDG_DATA_HOME:-$HOME/.local/share}/agy-companion/env.sh";

pub(super) const ZDOTDIR_ENV_VAR: &str = "ZDOTDIR";
pub(super) const SHELL_ENV_VAR: &str = "SHELL";
pub(super) const DEFAULT_RC_FILE_MODE: u32 = 0o644;
pub(super) const DEFAULT_RC_DIR_MODE: u32 = 0o755;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ShellKind {
    Zsh,
    Bash,
    Fish,
}

impl fmt::Display for ShellKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Zsh => f.write_str("zsh"),
            Self::Bash => f.write_str("bash"),
            Self::Fish => f.write_str("fish"),
        }
    }
}

impl FromStr for ShellKind {
    type Err = CoreError;

    fn from_str(raw: &str) -> Result<Self, Self::Err> {
        match raw.trim().to_ascii_lowercase().as_str() {
            "zsh" => Ok(Self::Zsh),
            "bash" => Ok(Self::Bash),
            "fish" => Ok(Self::Fish),
            _ => Err(CoreError::InvalidShell {
                shell: raw.to_string(),
            }),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum IntegrationState {
    Active,
    Enabled,
    ManualRequired,
    Disabled,
    Unsupported,
}

#[derive(Debug, Default, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct IntegrationOptions {
    pub shell: Option<ShellKind>,
    pub rc_file: Option<PathBuf>,
    pub binary_path: Option<PathBuf>,
    pub data_dir: Option<PathBuf>,
    pub follow_rc_symlink: bool,
}

/// Core shell and path metadata shared by `disabled`, `enabled`, and `manual_required` statuses.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IntegrationPaths {
    pub shell: ShellKind,
    pub rc_path: PathBuf,
    pub env_script_path: Option<PathBuf>,
    pub binary_path: PathBuf,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct IntegrationStatus {
    pub state: IntegrationState,
    pub enabled: bool,
    pub hybrid_active: bool,
    pub shell: Option<ShellKind>,
    pub rc_path: Option<PathBuf>,
    pub env_script_path: Option<PathBuf>,
    pub binary_path: Option<PathBuf>,
    pub symlink_target: Option<PathBuf>,
    pub manual_snippet: Option<String>,
    pub message: Option<String>,
}

impl IntegrationStatus {
    pub const fn state(&self) -> IntegrationState {
        self.state
    }

    /// Marks Hybrid Mode as active (e.g., when live CloudCode catalog traffic is observed).
    /// Promotes `Enabled` -> `Active` while leaving `Disabled`, `ManualRequired`, and `Unsupported` intact.
    pub fn mark_hybrid_active(&mut self) {
        self.hybrid_active = true;
        if self.state == IntegrationState::Enabled {
            self.state = IntegrationState::Active;
        }
    }

    pub fn unsupported(hybrid_active: bool, message: impl Into<String>) -> Self {
        Self {
            state: IntegrationState::Unsupported,
            enabled: false,
            hybrid_active,
            shell: None,
            rc_path: None,
            env_script_path: None,
            binary_path: None,
            symlink_target: None,
            manual_snippet: None,
            message: Some(message.into()),
        }
    }

    pub fn error_fallback(hybrid_active: bool, message: impl Into<String>) -> Self {
        Self {
            state: if is_platform_supported() {
                IntegrationState::Disabled
            } else {
                IntegrationState::Unsupported
            },
            enabled: false,
            hybrid_active,
            shell: None,
            rc_path: None,
            env_script_path: None,
            binary_path: None,
            symlink_target: None,
            manual_snippet: None,
            message: Some(message.into()),
        }
    }

    pub fn disabled(
        paths: IntegrationPaths,
        symlink_target: Option<PathBuf>,
        hybrid_active: bool,
    ) -> Self {
        Self {
            state: IntegrationState::Disabled,
            enabled: false,
            hybrid_active,
            shell: Some(paths.shell),
            rc_path: Some(paths.rc_path),
            env_script_path: paths.env_script_path,
            binary_path: Some(paths.binary_path),
            symlink_target,
            manual_snippet: None,
            message: None,
        }
    }

    pub fn enabled(
        paths: IntegrationPaths,
        symlink_target: Option<PathBuf>,
        hybrid_active: bool,
    ) -> Self {
        Self {
            state: if hybrid_active {
                IntegrationState::Active
            } else {
                IntegrationState::Enabled
            },
            enabled: true,
            hybrid_active,
            shell: Some(paths.shell),
            rc_path: Some(paths.rc_path),
            env_script_path: paths.env_script_path,
            binary_path: Some(paths.binary_path),
            symlink_target,
            manual_snippet: None,
            message: None,
        }
    }

    pub fn manual_required(
        paths: IntegrationPaths,
        enabled: bool,
        symlink_target: Option<PathBuf>,
        manual_snippet: Option<String>,
        hybrid_active: bool,
        message: impl Into<String>,
    ) -> Self {
        Self {
            state: IntegrationState::ManualRequired,
            enabled,
            hybrid_active,
            shell: Some(paths.shell),
            rc_path: Some(paths.rc_path),
            env_script_path: paths.env_script_path,
            binary_path: Some(paths.binary_path),
            symlink_target,
            manual_snippet,
            message: Some(message.into()),
        }
    }
}

#[derive(Debug, Default, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StripReport {
    pub modified_files: Vec<PathBuf>,
    pub deleted_files: Vec<PathBuf>,
    pub skipped_manual_files: Vec<PathBuf>,
}

/// Validates that `prefix` is a safe POSIX shell identifier prefix (`^[A-Za-z_][A-Za-z0-9_]*$`).
pub fn is_valid_restore_prefix(prefix: &str) -> bool {
    let mut bytes = prefix.bytes();
    let Some(first) = bytes.next() else {
        return false;
    };
    if !(first.is_ascii_alphabetic() || first == b'_') {
        return false;
    }
    bytes.all(|b| b.is_ascii_alphanumeric() || b == b'_')
}

pub fn detect_shell(shell_env: Option<&str>) -> ShellKind {
    if let Some(raw) = shell_env {
        let base = raw
            .trim()
            .rsplit(['/', '\\'])
            .find(|seg| !seg.is_empty())
            .unwrap_or("");
        match base.to_ascii_lowercase().as_str() {
            "zsh" => return ShellKind::Zsh,
            "bash" => return ShellKind::Bash,
            "fish" => return ShellKind::Fish,
            _ => {}
        }
    }

    #[cfg(target_os = "macos")]
    {
        ShellKind::Zsh
    }
    #[cfg(not(target_os = "macos"))]
    {
        ShellKind::Bash
    }
}

pub(crate) fn is_loopback_hybrid_url(raw: &str) -> bool {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return false;
    }
    let Ok(parsed) = Url::parse(trimmed) else {
        return false;
    };
    if parsed.scheme() != "http" {
        return false;
    }
    if !parsed.username().is_empty() || parsed.password().is_some() {
        return false;
    }
    let host_is_loopback = matches!(
        parsed.host(),
        Some(Host::Ipv4(Ipv4Addr::LOCALHOST)) | Some(Host::Domain("localhost"))
    );
    host_is_loopback && parsed.port().is_some_and(|port| port > 0)
}

fn detect_hybrid_active_from(cloud_code_url: Option<&str>, hook_active: Option<&str>) -> bool {
    if let Some(raw) = cloud_code_url
        && !raw.trim().is_empty()
    {
        return is_loopback_hybrid_url(raw);
    }
    hook_active.map(str::trim) == Some("1")
}

pub fn detect_hybrid_active() -> bool {
    let cloud_code_url = std::env::var("CLOUD_CODE_URL").ok();
    let hook_active = std::env::var("AGY_COMPANION_HOOK_ACTIVE").ok();
    detect_hybrid_active_from(cloud_code_url.as_deref(), hook_active.as_deref())
}

fn is_executable_regular_file(path: &Path) -> bool {
    let Ok(meta) = fs::metadata(path) else {
        return false;
    };
    if !meta.is_file() {
        return false;
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        meta.permissions().mode() & 0o111 != 0
    }
    #[cfg(not(unix))]
    {
        true
    }
}

pub fn resolve_stable_binary_path(
    override_bin: Option<&Path>,
    data_dir: Option<&Path>,
) -> Result<PathBuf, CoreError> {
    resolve_stable_binary_path_with(override_bin, data_dir, std::env::current_exe)
}

fn resolve_stable_binary_path_with<F>(
    override_bin: Option<&Path>,
    data_dir: Option<&Path>,
    current_exe: F,
) -> Result<PathBuf, CoreError>
where
    F: FnOnce() -> std::io::Result<PathBuf>,
{
    if let Some(bin) = override_bin {
        validate_shell_safe_path(bin)?;
        return Ok(bin.to_path_buf());
    }

    let resolved_data = match data_dir {
        Some(dir) => {
            fs_safe::validate_path(dir)?;
            Some(dir.to_path_buf())
        }
        None => default_data_dir().ok(),
    };

    if let Some(dir) = resolved_data {
        let bin_name = if cfg!(windows) {
            "agy-companion.exe"
        } else {
            "agy-companion"
        };
        let stable_bin = dir.join("bin").join(bin_name);
        if is_executable_regular_file(&stable_bin) {
            validate_shell_safe_path(&stable_bin)?;
            return Ok(stable_bin);
        }
    }

    let exe = current_exe().map_err(|source| CoreError::Io {
        path: PathBuf::from("current_exe"),
        source,
    })?;
    let resolved = fs::canonicalize(&exe).unwrap_or(exe);
    validate_shell_safe_path(&resolved)?;
    Ok(resolved)
}

pub(super) fn resolve_optional_absolute_dir(
    candidate: Option<&Path>,
) -> Result<Option<PathBuf>, CoreError> {
    let Some(dir) = candidate else {
        return Ok(None);
    };
    if dir.as_os_str().is_empty() {
        return Ok(None);
    }
    if let Some(raw) = dir.to_str() {
        return resolve_xdg_base(Some(raw), TargetPlatform::current());
    }
    fs_safe::validate_path(dir)?;
    if dir.is_absolute() {
        Ok(Some(dir.to_path_buf()))
    } else {
        Ok(None)
    }
}

pub(crate) fn resolve_rc_path(
    shell: ShellKind,
    home: &Path,
    zdotdir: Option<&Path>,
    xdg_config_home: Option<&Path>,
) -> Result<PathBuf, CoreError> {
    fs_safe::validate_path(home)?;
    match shell {
        ShellKind::Zsh => {
            let base =
                resolve_optional_absolute_dir(zdotdir)?.unwrap_or_else(|| home.to_path_buf());
            Ok(base.join(".zlogin"))
        }
        ShellKind::Bash => {
            for candidate in [".bash_profile", ".bash_login", ".profile"] {
                let path = home.join(candidate);
                let check_path = fs_safe::normalize_path(&path);
                if fs::symlink_metadata(&check_path).is_ok() {
                    return Ok(path);
                }
            }
            Ok(home.join(".profile"))
        }
        ShellKind::Fish => {
            let base = resolve_optional_absolute_dir(xdg_config_home)?
                .unwrap_or_else(|| home.join(".config"));
            Ok(base.join("fish").join("conf.d").join(FISH_DROPIN_FILENAME))
        }
    }
}

pub(super) const fn is_platform_supported() -> bool {
    !cfg!(windows)
}

pub fn status(opts: &IntegrationOptions) -> Result<IntegrationStatus, CoreError> {
    status_with_hybrid_active(opts, None)
}

pub fn status_with_hybrid_active(
    opts: &IntegrationOptions,
    hybrid_active_override: Option<bool>,
) -> Result<IntegrationStatus, CoreError> {
    let inputs = lifecycle::read_process_env_inputs()?;
    let env = LifecycleEnv::from_process(
        &inputs,
        hybrid_active_override.unwrap_or_else(detect_hybrid_active),
        is_platform_supported(),
    );
    lifecycle::status_with_env(opts, &env)
}

pub fn enable(opts: &IntegrationOptions) -> Result<IntegrationStatus, CoreError> {
    let inputs = lifecycle::read_process_env_inputs()?;
    let env = LifecycleEnv::from_process(&inputs, detect_hybrid_active(), is_platform_supported());
    enable::enable_with_env(opts, &env)
}

pub fn disable(opts: &IntegrationOptions) -> Result<IntegrationStatus, CoreError> {
    let inputs = lifecycle::read_process_env_inputs()?;
    let env = LifecycleEnv::from_process(&inputs, detect_hybrid_active(), is_platform_supported());
    disable::disable_with_env(opts, &env)
}
