use std::collections::HashSet;
use std::ffi::{OsStr, OsString};
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::{Arc, RwLock};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use companion_core::fs_safe::{self, OpenRegularMode, SymlinkPolicy};
use companion_core::{
    CONFIG_PATH_ENV_VAR, Config, InstallChannel, LastUpgradeRecord, SemVer, UpdateState,
    UpgradeOutcome, default_config_path, default_plugins_dir, load_update_state, save_update_state,
    update_state_path, validate_upstream_url, write_atomic_secure,
};
use companion_ui::{UpdateLifecycleState, UpdateStatusSnapshot};
use serde::{Deserialize, Serialize};

pub const NO_UPDATE_CHECK_ENV: &str = "AGY_COMPANION_NO_UPDATE_CHECK";
pub const NPM_REGISTRY_LATEST_URL: &str = "https://registry.npmjs.org/agy-companion/latest";
pub const PYPI_REGISTRY_JSON_URL: &str = "https://pypi.org/pypi/agy-companion/json";
#[cfg(debug_assertions)]
pub const TEST_NPM_REGISTRY_URL_ENV: &str = "AGY_COMPANION_TEST_NPM_REGISTRY_URL";
#[cfg(debug_assertions)]
pub const TEST_PYPI_REGISTRY_URL_ENV: &str = "AGY_COMPANION_TEST_PYPI_REGISTRY_URL";

pub const UPGRADE_LOCK_FILENAME: &str = "upgrade.lock";
pub const UPGRADE_LOG_FILENAME: &str = "upgrade.log";

pub const DEFAULT_STARTUP_DELAY: Duration = Duration::from_secs(15);
pub const DEFAULT_CHECK_INTERVAL: Duration = Duration::from_secs(3600);
pub const DEFAULT_CHECK_TTL: Duration = Duration::from_secs(12 * 3600);
pub const DEFAULT_CLOCK_SKEW_TOLERANCE: Duration = Duration::from_secs(5 * 60);
pub const DEFAULT_MANUAL_COOLDOWN: Duration = Duration::from_secs(60);
pub const DEFAULT_ERROR_BACKOFF: Duration = Duration::from_secs(3600);
pub const DEFAULT_UPGRADE_TIMEOUT: Duration = Duration::from_secs(180);

pub const MAX_NPM_RESPONSE_BYTES: usize = 256 * 1024;
pub const MAX_PYPI_RESPONSE_BYTES: usize = 1024 * 1024;
pub const MAX_UPGRADE_LOCK_BYTES: u64 = 4096;
pub const MAX_SANITIZED_ERROR_CHARS: usize = 200;

const REGISTRY_CONNECT_TIMEOUT: Duration = Duration::from_secs(3);
const REGISTRY_REQUEST_TIMEOUT: Duration = Duration::from_secs(5);
const UPGRADE_POLL_INTERVAL: Duration = Duration::from_millis(100);

const NPX_UPGRADE_ARGS: &[&str] = &["-y", "agy-companion@latest", "install", "--channel", "npx"];
const UVX_UPGRADE_ARGS: &[&str] = &["--refresh", "agy-companion", "install", "--channel", "uvx"];

#[cfg(unix)]
const UNIX_STANDARD_SYSTEM_BIN_DIRS: &[&str] = &["/opt/homebrew/bin", "/usr/local/bin"];

#[cfg(unix)]
const UNIX_STANDARD_HOME_BIN_SUBDIRS: &[&str] = &[
    ".local/bin",
    ".cargo/bin",
    ".volta/bin",
    ".local/share/mise/shims",
    ".asdf/shims",
];

const SCRUBBED_EXACT_ENV_VARS: &[&str] = &[
    "CLOUD_CODE_URL",
    "GOOGLE_GEMINI_BASE_URL",
    "GEMINI_API_KEY",
    "OPENAI_API_KEY",
    "ANTHROPIC_API_KEY",
    "OLLAMA_API_KEY",
];

const SCRUBBED_ENV_PREFIXES: &[&str] = &[
    "ANTIGRAVITY_SIDECAR_",
    "ANTIGRAVITY_LS_",
    "AGY_COMPANION_",
    "JETSKI_MCP_ENV_RESTORE_",
];

pub fn is_env_truthy(raw: &str) -> bool {
    let trimmed = raw.trim();
    !(trimmed.is_empty() || trimmed == "0" || trimmed.eq_ignore_ascii_case("false"))
}

pub fn is_update_check_disabled_by_env() -> bool {
    std::env::var(NO_UPDATE_CHECK_ENV)
        .ok()
        .is_some_and(|v| is_env_truthy(&v))
        || std::env::var("CI").ok().is_some_and(|v| is_env_truthy(&v))
}

pub fn should_refresh_update_check(
    checked_at_ms: Option<u64>,
    now_ms: u64,
    ttl_ms: u64,
    clock_skew_tolerance_ms: u64,
) -> bool {
    let Some(checked_at) = checked_at_ms else {
        return true;
    };
    if checked_at > now_ms.saturating_add(clock_skew_tolerance_ms) {
        return true;
    }
    now_ms.saturating_sub(checked_at) >= ttl_ms
}

pub fn sanitize_error_message(raw: &str) -> String {
    let mut cleaned = String::with_capacity(raw.len().min(MAX_SANITIZED_ERROR_CHARS));
    let mut prev_space = true;
    for ch in raw.chars() {
        let mapped = if ch.is_control() || ch.is_whitespace() {
            ' '
        } else {
            ch
        };
        if mapped == ' ' {
            if !prev_space {
                cleaned.push(' ');
                prev_space = true;
            }
        } else {
            cleaned.push(mapped);
            prev_space = false;
        }
        if cleaned.chars().count() >= MAX_SANITIZED_ERROR_CHARS {
            break;
        }
    }
    cleaned.trim().to_string()
}

fn now_epoch_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| u64::try_from(d.as_millis()).unwrap_or(u64::MAX))
        .unwrap_or(0)
}

fn version_is_newer(candidate: &str, current: &str) -> bool {
    match (SemVer::parse(candidate), SemVer::parse(current)) {
        (Ok(c), Ok(cur)) => c > cur,
        _ => false,
    }
}

fn version_reached_target(running: &str, target: &str) -> bool {
    match (SemVer::parse(running), SemVer::parse(target)) {
        (Ok(r), Ok(t)) => r >= t,
        _ => running == target,
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct UpgradeLockInfo {
    pub pid: u32,
    pub started_at_ms: u64,
}

fn read_upgrade_lock(path: &Path) -> Option<UpgradeLockInfo> {
    let raw = match fs_safe::read_regular_file_utf8_bounded(
        path,
        MAX_UPGRADE_LOCK_BYTES,
        SymlinkPolicy::Reject,
    ) {
        Ok(Some((raw, _))) => raw,
        _ => return None,
    };
    serde_json::from_str(&raw).ok()
}

fn write_upgrade_lock(path: &Path, info: &UpgradeLockInfo) -> Result<(), String> {
    let mut payload =
        serde_json::to_string_pretty(info).map_err(|err| format!("serialize lock: {err}"))?;
    payload.push('\n');
    write_atomic_secure(path, payload.as_bytes()).map_err(|err| format!("write lock: {err}"))
}

fn remove_upgrade_lock(path: &Path) {
    let _ = fs::remove_file(path);
}

fn is_upgrade_lock_active(
    lock: &UpgradeLockInfo,
    now_ms: u64,
    max_age_ms: u64,
    pid_alive: impl Fn(u32) -> bool,
) -> bool {
    if lock.pid <= 1 || lock.pid > i32::MAX as u32 {
        return false;
    }
    let skew_ms = DEFAULT_CLOCK_SKEW_TOLERANCE.as_millis() as u64;
    if lock.started_at_ms > now_ms.saturating_add(skew_ms) {
        return false;
    }
    if now_ms.saturating_sub(lock.started_at_ms) > max_age_ms {
        return false;
    }
    pid_alive(lock.pid)
}

fn default_pid_alive(pid: u32) -> bool {
    crate::process::snapshot_watched_process(pid).is_some()
}

#[cfg(unix)]
fn is_dir_world_writable(dir: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    match fs::metadata(dir) {
        Ok(meta) => meta.is_dir() && (meta.permissions().mode() & 0o002 != 0),
        Err(_) => false,
    }
}

#[cfg(not(unix))]
fn is_dir_world_writable(_dir: &Path) -> bool {
    false
}

fn is_executable_file(path: &Path) -> bool {
    let Ok(meta) = fs::metadata(path) else {
        return false;
    };
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        meta.is_file() && (meta.permissions().mode() & 0o111 != 0)
    }
    #[cfg(not(unix))]
    {
        meta.is_file()
    }
}

fn channel_executable_names(channel: InstallChannel) -> &'static [&'static str] {
    #[cfg(windows)]
    {
        match channel {
            InstallChannel::Npx => &["npx.cmd", "npx.exe"],
            InstallChannel::Uvx => &["uvx.exe"],
        }
    }
    #[cfg(not(windows))]
    {
        match channel {
            InstallChannel::Npx => &["npx"],
            InstallChannel::Uvx => &["uvx"],
        }
    }
}

fn channel_upgrade_args(channel: InstallChannel) -> &'static [&'static str] {
    match channel {
        InstallChannel::Npx => NPX_UPGRADE_ARGS,
        InstallChannel::Uvx => UVX_UPGRADE_ARGS,
    }
}

pub fn resolve_upgrade_executable_with_env(
    channel: InstallChannel,
    path_env: Option<&OsStr>,
    home_dir: Option<&Path>,
) -> Option<PathBuf> {
    let mut dirs: Vec<PathBuf> = Vec::new();
    let mut seen: HashSet<PathBuf> = HashSet::new();

    if let Some(raw_path) = path_env {
        for dir in std::env::split_paths(raw_path) {
            if !dir.as_os_str().is_empty() && seen.insert(dir.clone()) {
                dirs.push(dir);
            }
        }
    }

    #[cfg(unix)]
    {
        if let Some(home) = home_dir {
            let local_bin = home.join(".local/bin");
            if seen.insert(local_bin.clone()) {
                dirs.push(local_bin);
            }
        }
        for sys_dir in UNIX_STANDARD_SYSTEM_BIN_DIRS {
            let p = PathBuf::from(sys_dir);
            if seen.insert(p.clone()) {
                dirs.push(p);
            }
        }
        if let Some(home) = home_dir {
            for sub in &UNIX_STANDARD_HOME_BIN_SUBDIRS[1..] {
                let p = home.join(sub);
                if seen.insert(p.clone()) {
                    dirs.push(p);
                }
            }
        }
    }
    #[cfg(not(unix))]
    let _ = home_dir;

    let names = channel_executable_names(channel);
    for dir in dirs {
        if !dir.is_dir() || is_dir_world_writable(&dir) {
            continue;
        }
        for name in names {
            let candidate = dir.join(name);
            if is_executable_file(&candidate) {
                return Some(candidate);
            }
        }
    }
    None
}

pub fn resolve_upgrade_executable(channel: InstallChannel) -> Option<PathBuf> {
    let path_env = std::env::var_os("PATH");
    let home = fs_safe::default_home_dir();
    resolve_upgrade_executable_with_env(channel, path_env.as_deref(), home.as_deref())
}

pub fn build_child_path(resolved_exe: &Path, inherited_path: Option<&OsStr>) -> Option<OsString> {
    let mut dirs: Vec<PathBuf> = Vec::new();
    let mut seen: HashSet<PathBuf> = HashSet::new();

    if let Some(parent) = resolved_exe.parent()
        && !parent.as_os_str().is_empty()
        && seen.insert(parent.to_path_buf())
    {
        dirs.push(parent.to_path_buf());
    }

    #[cfg(unix)]
    {
        for sys_dir in UNIX_STANDARD_SYSTEM_BIN_DIRS {
            let p = PathBuf::from(sys_dir);
            if p.is_dir() && !is_dir_world_writable(&p) && seen.insert(p.clone()) {
                dirs.push(p);
            }
        }
    }

    if let Some(raw) = inherited_path {
        for dir in std::env::split_paths(raw) {
            if !dir.as_os_str().is_empty() && seen.insert(dir.clone()) {
                dirs.push(dir);
            }
        }
    }

    std::env::join_paths(dirs).ok()
}

pub fn build_upgrade_command(
    resolved_exe: &Path,
    channel: InstallChannel,
    config_path: &Path,
    inherited_path: Option<&OsStr>,
    current_env: impl IntoIterator<Item = (OsString, OsString)>,
) -> Command {
    let mut cmd = Command::new(resolved_exe);
    cmd.args(channel_upgrade_args(channel));

    let default_cfg = default_config_path().ok();
    let default_plugins = default_plugins_dir().ok();
    let mut non_default_plugins_dir: Option<OsString> = None;

    for (key, val) in current_env {
        let key_str = key.to_string_lossy();
        if key_str == "AGY_PLUGINS_DIR" {
            let is_default = default_plugins
                .as_ref()
                .is_some_and(|def| Path::new(&val) == def.as_path());
            if !is_default && !val.is_empty() {
                non_default_plugins_dir = Some(val);
            } else {
                cmd.env_remove(&key);
            }
            continue;
        }
        if SCRUBBED_ENV_PREFIXES
            .iter()
            .any(|prefix| key_str.starts_with(prefix))
            || SCRUBBED_EXACT_ENV_VARS
                .iter()
                .any(|exact| key_str == *exact)
        {
            cmd.env_remove(&key);
        }
    }

    // Explicitly scrub known vars even if absent from current_env iterator.
    for exact in SCRUBBED_EXACT_ENV_VARS {
        cmd.env_remove(exact);
    }

    let is_default_cfg = default_cfg
        .as_ref()
        .is_some_and(|def| def.as_path() == config_path);
    if !is_default_cfg {
        cmd.env(CONFIG_PATH_ENV_VAR, config_path);
    }
    if let Some(plugins_dir) = non_default_plugins_dir {
        cmd.env("AGY_PLUGINS_DIR", plugins_dir);
    }

    if let Some(child_path) = build_child_path(resolved_exe, inherited_path) {
        cmd.env("PATH", child_path);
    }

    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        cmd.process_group(0);
    }
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        const CREATE_NEW_PROCESS_GROUP: u32 = 0x0000_0200;
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        cmd.creation_flags(CREATE_NEW_PROCESS_GROUP | CREATE_NO_WINDOW);
    }

    cmd
}

#[derive(Debug, Clone)]
pub struct UpdateManagerOptions {
    pub current_version: String,
    pub runtime_data_dir: PathBuf,
    pub config_path: PathBuf,
    pub npm_registry_url: String,
    pub pypi_registry_url: String,
    pub startup_delay: Duration,
    pub check_interval: Duration,
    pub check_ttl: Duration,
    pub clock_skew_tolerance: Duration,
    pub manual_cooldown: Duration,
    pub error_backoff: Duration,
    pub upgrade_timeout: Duration,
    pub disabled_by_env: Option<bool>,
    pub executable_override: Option<Option<PathBuf>>,
}

impl UpdateManagerOptions {
    pub fn for_paths(config_path: PathBuf, runtime_data_dir: PathBuf) -> Self {
        let npm_registry_url = resolve_registry_url_override(
            #[cfg(debug_assertions)]
            Some(TEST_NPM_REGISTRY_URL_ENV),
            #[cfg(not(debug_assertions))]
            None,
            NPM_REGISTRY_LATEST_URL,
        );
        let pypi_registry_url = resolve_registry_url_override(
            #[cfg(debug_assertions)]
            Some(TEST_PYPI_REGISTRY_URL_ENV),
            #[cfg(not(debug_assertions))]
            None,
            PYPI_REGISTRY_JSON_URL,
        );
        Self {
            current_version: env!("CARGO_PKG_VERSION").to_string(),
            runtime_data_dir,
            config_path,
            npm_registry_url,
            pypi_registry_url,
            startup_delay: DEFAULT_STARTUP_DELAY,
            check_interval: DEFAULT_CHECK_INTERVAL,
            check_ttl: DEFAULT_CHECK_TTL,
            clock_skew_tolerance: DEFAULT_CLOCK_SKEW_TOLERANCE,
            manual_cooldown: DEFAULT_MANUAL_COOLDOWN,
            error_backoff: DEFAULT_ERROR_BACKOFF,
            upgrade_timeout: DEFAULT_UPGRADE_TIMEOUT,
            disabled_by_env: None,
            executable_override: None,
        }
    }
}

fn resolve_registry_url_override(env_var: Option<&str>, default_url: &str) -> String {
    if let Some(var_name) = env_var
        && let Ok(raw) = std::env::var(var_name)
    {
        let trimmed = raw.trim();
        let is_loopback =
            trimmed.starts_with("http://127.0.0.1:") || trimmed.starts_with("http://localhost:");
        if is_loopback && validate_upstream_url(trimmed).is_ok() {
            return trimmed.to_string();
        }
    }
    default_url.to_string()
}

struct ManagerInner {
    state: UpdateState,
    reconciled_upgraded: bool,
    last_manual_check: Option<Instant>,
    last_error_at: Option<Instant>,
}

pub struct UpdateManager {
    options: UpdateManagerOptions,
    http_client: reqwest::Client,
    inner: RwLock<ManagerInner>,
    check_mutex: tokio::sync::Mutex<()>,
    upgrade_mutex: tokio::sync::Mutex<()>,
}

impl UpdateManager {
    pub fn new(options: UpdateManagerOptions) -> Result<Self, String> {
        let http_client = reqwest::Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .connect_timeout(REGISTRY_CONNECT_TIMEOUT)
            .timeout(REGISTRY_REQUEST_TIMEOUT)
            .user_agent(format!("agy-companion/{}", options.current_version))
            .build()
            .map_err(|err| format!("failed to build update HTTP client: {err}"))?;

        let state_path = update_state_path(&options.runtime_data_dir);
        let lock_path = options.runtime_data_dir.join(UPGRADE_LOCK_FILENAME);
        let mut state = match load_update_state(&state_path) {
            Ok(Some(s)) => s,
            Ok(None) => UpdateState::default(),
            Err(err) => {
                tracing::debug!(
                    error = %err,
                    path = %state_path.display(),
                    "failed to load update-state.json; falling back to default"
                );
                UpdateState::default()
            }
        };

        let mut reconciled_upgraded = false;
        let now_ms = now_epoch_ms();
        let max_age_ms = u64::try_from(options.upgrade_timeout.as_millis()).unwrap_or(180_000);

        if let Some(ref mut last) = state.last_upgrade
            && last.outcome == UpgradeOutcome::Upgrading
        {
            if version_reached_target(&options.current_version, &last.target_version) {
                last.outcome = UpgradeOutcome::Succeeded;
                last.finished_at_ms = Some(now_ms);
                last.error = None;
                reconciled_upgraded = true;
                remove_upgrade_lock(&lock_path);
                let _ = save_update_state(&state_path, &state);
            } else {
                let lock_active = read_upgrade_lock(&lock_path).is_some_and(|lock| {
                    is_upgrade_lock_active(&lock, now_ms, max_age_ms, default_pid_alive)
                });
                if !lock_active {
                    last.outcome = UpgradeOutcome::Failed;
                    last.finished_at_ms = Some(now_ms);
                    last.error = Some(sanitize_error_message("previous upgrade did not complete"));
                    remove_upgrade_lock(&lock_path);
                    let _ = save_update_state(&state_path, &state);
                }
            }
        }

        Ok(Self {
            options,
            http_client,
            inner: RwLock::new(ManagerInner {
                state,
                reconciled_upgraded,
                last_manual_check: None,
                last_error_at: None,
            }),
            check_mutex: tokio::sync::Mutex::new(()),
            upgrade_mutex: tokio::sync::Mutex::new(()),
        })
    }

    fn state_path(&self) -> PathBuf {
        update_state_path(&self.options.runtime_data_dir)
    }

    fn lock_path(&self) -> PathBuf {
        self.options.runtime_data_dir.join(UPGRADE_LOCK_FILENAME)
    }

    fn log_path(&self) -> PathBuf {
        self.options.runtime_data_dir.join(UPGRADE_LOG_FILENAME)
    }

    fn is_disabled_for_config(&self, cfg: Option<&Config>) -> bool {
        let env_disabled = self
            .options
            .disabled_by_env
            .unwrap_or_else(is_update_check_disabled_by_env);
        if env_disabled {
            return true;
        }
        cfg.is_some_and(|c| c.check_for_updates == Some(false))
    }

    fn load_config_disabled(&self) -> bool {
        if self
            .options
            .disabled_by_env
            .unwrap_or_else(is_update_check_disabled_by_env)
        {
            return true;
        }
        Config::load_or_default(&self.options.config_path)
            .ok()
            .is_some_and(|c| c.check_for_updates == Some(false))
    }

    fn resolve_exe_for_channel(&self, channel: InstallChannel) -> Option<PathBuf> {
        match &self.options.executable_override {
            Some(override_val) => override_val.clone(),
            None => resolve_upgrade_executable(channel),
        }
    }

    fn build_snapshot_from_inner(
        &self,
        inner: &ManagerInner,
        disabled: bool,
    ) -> UpdateStatusSnapshot {
        let channel = inner.state.install_channel;
        let can_one_click = self.resolve_exe_for_channel(channel).is_some();
        let fallback_command = channel.fallback_command().to_string();
        let current_version = self.options.current_version.clone();
        let latest_version = inner.state.latest_version.clone();
        let checked_at_ms = inner.state.checked_at_ms;

        if disabled {
            return UpdateStatusSnapshot {
                state: UpdateLifecycleState::Disabled,
                channel,
                current_version,
                latest_version,
                can_one_click_upgrade: can_one_click,
                fallback_command,
                checked_at_ms,
                error: None,
            };
        }

        let mut error = None;
        let lifecycle = if let Some(ref last) = inner.state.last_upgrade {
            match last.outcome {
                UpgradeOutcome::Upgrading => UpdateLifecycleState::Upgrading,
                UpgradeOutcome::Succeeded if inner.reconciled_upgraded => {
                    if latest_version.as_deref().is_some_and(|lv| {
                        version_is_newer(lv, &current_version)
                            && version_is_newer(lv, &last.target_version)
                    }) {
                        UpdateLifecycleState::Available
                    } else {
                        UpdateLifecycleState::Upgraded
                    }
                }
                UpgradeOutcome::Failed => {
                    // Surface failure if it corresponds to an update still newer than current.
                    let latest_newer = latest_version
                        .as_deref()
                        .is_some_and(|lv| version_is_newer(lv, &current_version));
                    if latest_newer
                        || !version_reached_target(&current_version, &last.target_version)
                    {
                        error = last.error.as_deref().map(sanitize_error_message);
                        UpdateLifecycleState::Failed
                    } else {
                        UpdateLifecycleState::UpToDate
                    }
                }
                UpgradeOutcome::Succeeded => {
                    if latest_version
                        .as_deref()
                        .is_some_and(|lv| version_is_newer(lv, &current_version))
                    {
                        UpdateLifecycleState::Available
                    } else {
                        UpdateLifecycleState::UpToDate
                    }
                }
            }
        } else if latest_version
            .as_deref()
            .is_some_and(|lv| version_is_newer(lv, &current_version))
        {
            UpdateLifecycleState::Available
        } else {
            UpdateLifecycleState::UpToDate
        };

        UpdateStatusSnapshot {
            state: lifecycle,
            channel,
            current_version,
            latest_version,
            can_one_click_upgrade: can_one_click,
            fallback_command,
            checked_at_ms,
            error,
        }
    }

    pub fn snapshot_for_config(&self, cfg: &Config) -> UpdateStatusSnapshot {
        let disabled = self.is_disabled_for_config(Some(cfg));
        let inner = self
            .inner
            .read()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        self.build_snapshot_from_inner(&inner, disabled)
    }

    pub fn get_status(&self) -> UpdateStatusSnapshot {
        let disabled = self.load_config_disabled();
        let inner = self
            .inner
            .read()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        self.build_snapshot_from_inner(&inner, disabled)
    }

    pub async fn check_update_manual(&self) -> UpdateStatusSnapshot {
        let _guard = self.check_mutex.lock().await;
        if self.load_config_disabled() {
            return self.get_status();
        }
        {
            let inner = self
                .inner
                .read()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            if inner
                .state
                .last_upgrade
                .as_ref()
                .is_some_and(|u| u.outcome == UpgradeOutcome::Upgrading)
            {
                return self.build_snapshot_from_inner(&inner, false);
            }
            if inner
                .last_manual_check
                .is_some_and(|last| last.elapsed() < self.options.manual_cooldown)
            {
                return self.build_snapshot_from_inner(&inner, false);
            }
        }

        {
            let mut inner = self
                .inner
                .write()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            inner.last_manual_check = Some(Instant::now());
        }

        self.perform_registry_check().await;
        self.get_status()
    }

    pub async fn check_if_due(&self) {
        let _guard = self.check_mutex.lock().await;
        if self.load_config_disabled() {
            return;
        }
        let now_ms = now_epoch_ms();
        let ttl_ms = u64::try_from(self.options.check_ttl.as_millis()).unwrap_or(12 * 3600 * 1000);
        let skew_ms =
            u64::try_from(self.options.clock_skew_tolerance.as_millis()).unwrap_or(5 * 60 * 1000);

        let should_check = {
            let inner = self
                .inner
                .read()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            let upgrading = inner
                .state
                .last_upgrade
                .as_ref()
                .is_some_and(|u| u.outcome == UpgradeOutcome::Upgrading);
            let in_backoff = inner
                .last_error_at
                .is_some_and(|last| last.elapsed() < self.options.error_backoff);
            if upgrading || in_backoff {
                false
            } else {
                should_refresh_update_check(inner.state.checked_at_ms, now_ms, ttl_ms, skew_ms)
            }
        };

        if should_check {
            self.perform_registry_check().await;
        }
    }

    async fn perform_registry_check(&self) {
        let channel = {
            let inner = self
                .inner
                .read()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            inner.state.install_channel
        };

        match self.fetch_latest_version(channel).await {
            Ok(latest) => {
                let now_ms = now_epoch_ms();
                let state_to_save = {
                    let mut inner = self
                        .inner
                        .write()
                        .unwrap_or_else(|poisoned| poisoned.into_inner());
                    inner.state.checked_at_ms = Some(now_ms);
                    inner.state.latest_version = Some(latest);
                    inner.last_error_at = None;
                    if inner
                        .state
                        .last_upgrade
                        .as_ref()
                        .is_some_and(|u| u.outcome == UpgradeOutcome::Failed)
                    {
                        inner.state.last_upgrade = None;
                    }
                    inner.state.clone()
                };
                let state_path = self.state_path();
                if let Err(err) = tokio::task::spawn_blocking(move || {
                    save_update_state(&state_path, &state_to_save)
                })
                .await
                .unwrap_or_else(|e| {
                    Err(companion_core::CoreError::SerializeConfig {
                        reason: e.to_string(),
                    })
                }) {
                    tracing::debug!(error = %err, "failed to persist update-state.json after check");
                }
            }
            Err(err) => {
                tracing::debug!(
                    channel = %channel,
                    error = %err,
                    "update check failed; backing off silently"
                );
                let mut inner = self
                    .inner
                    .write()
                    .unwrap_or_else(|poisoned| poisoned.into_inner());
                inner.last_error_at = Some(Instant::now());
            }
        }
    }

    pub async fn fetch_latest_version(&self, channel: InstallChannel) -> Result<String, String> {
        let (url, max_bytes) = match channel {
            InstallChannel::Npx => (&self.options.npm_registry_url, MAX_NPM_RESPONSE_BYTES),
            InstallChannel::Uvx => (&self.options.pypi_registry_url, MAX_PYPI_RESPONSE_BYTES),
        };
        fetch_latest_version_from_url(&self.http_client, channel, url, max_bytes).await
    }

    pub fn spawn_background_loop(
        self: &Arc<Self>,
        mut shutdown: tokio::sync::watch::Receiver<bool>,
    ) -> tokio::task::JoinHandle<()> {
        let manager = Arc::clone(self);
        let startup_delay = self.options.startup_delay;
        let check_interval = self.options.check_interval;

        tokio::spawn(async move {
            let stopped_early = tokio::select! {
                _ = async { let _ = shutdown.wait_for(|&stopped| stopped).await; } => true,
                _ = tokio::time::sleep(startup_delay) => false,
            };
            if stopped_early {
                return;
            }

            manager.check_if_due().await;

            let mut interval = tokio::time::interval(check_interval);
            // Consume the immediate first tick so the next tick fires after `check_interval`.
            interval.tick().await;
            loop {
                let should_stop = tokio::select! {
                    _ = async { let _ = shutdown.wait_for(|&stopped| stopped).await; } => true,
                    _ = interval.tick() => false,
                };
                if should_stop {
                    break;
                }
                manager.check_if_due().await;
            }
        })
    }

    pub async fn start_upgrade(self: &Arc<Self>) -> UpdateStatusSnapshot {
        let _guard = self.upgrade_mutex.lock().await;

        if self.load_config_disabled() {
            return self.get_status();
        }

        let now_ms = now_epoch_ms();
        let max_age_ms = u64::try_from(self.options.upgrade_timeout.as_millis()).unwrap_or(180_000);
        let lock_path = self.lock_path();

        // Idempotency: if already upgrading in-memory OR active cross-process lock exists, return Upgrading.
        {
            let mut inner = self
                .inner
                .write()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            if inner
                .state
                .last_upgrade
                .as_ref()
                .is_some_and(|u| u.outcome == UpgradeOutcome::Upgrading)
            {
                return self.build_snapshot_from_inner(&inner, false);
            }
            if let Some(lock) = read_upgrade_lock(&lock_path) {
                if is_upgrade_lock_active(&lock, now_ms, max_age_ms, default_pid_alive) {
                    let target = inner
                        .state
                        .latest_version
                        .clone()
                        .unwrap_or_else(|| self.options.current_version.clone());
                    inner.state.last_upgrade = Some(LastUpgradeRecord {
                        target_version: target,
                        started_at_ms: lock.started_at_ms,
                        finished_at_ms: None,
                        outcome: UpgradeOutcome::Upgrading,
                        error: None,
                    });
                    inner.reconciled_upgraded = false;
                    return self.build_snapshot_from_inner(&inner, false);
                }
                remove_upgrade_lock(&lock_path);
            }
        }

        let (channel, target_version) = {
            let inner = self
                .inner
                .read()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            let target = inner
                .state
                .latest_version
                .clone()
                .unwrap_or_else(|| self.options.current_version.clone());
            (inner.state.install_channel, target)
        };

        let Some(resolved_exe) = self.resolve_exe_for_channel(channel) else {
            return self.record_upgrade_failure(
                &target_version,
                now_ms,
                format!(
                    "one-click upgrade unavailable: {} not found on PATH",
                    channel.as_str()
                ),
            );
        };

        // Ensure runtime data dir exists securely and open truncated 0600 upgrade.log.
        if let Err(err) = fs_safe::ensure_dir_no_symlink(&self.options.runtime_data_dir, 0o700) {
            return self.record_upgrade_failure(
                &target_version,
                now_ms,
                format!("failed to prepare runtime directory: {err}"),
            );
        }

        let log_path = self.log_path();
        let (log_stdout, log_stderr) = match open_truncated_upgrade_log(&log_path) {
            Ok(pair) => pair,
            Err(err) => {
                return self.record_upgrade_failure(
                    &target_version,
                    now_ms,
                    format!("failed to open upgrade log: {err}"),
                );
            }
        };

        let inherited_path = std::env::var_os("PATH");
        let mut cmd = build_upgrade_command(
            &resolved_exe,
            channel,
            &self.options.config_path,
            inherited_path.as_deref(),
            std::env::vars_os(),
        );
        cmd.stdin(Stdio::null())
            .stdout(Stdio::from(log_stdout))
            .stderr(Stdio::from(log_stderr));

        let mut child = match cmd.spawn() {
            Ok(c) => c,
            Err(err) => {
                return self.record_upgrade_failure(
                    &target_version,
                    now_ms,
                    format!("failed to spawn upgrade command: {err}"),
                );
            }
        };

        let child_pid = child.id();
        let lock_info = UpgradeLockInfo {
            pid: child_pid,
            started_at_ms: now_ms,
        };
        if let Err(err) = write_upgrade_lock(&lock_path, &lock_info) {
            tracing::debug!(error = %err, "failed to write upgrade.lock");
        }

        let state_to_save = {
            let mut inner = self
                .inner
                .write()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            inner.reconciled_upgraded = false;
            inner.state.last_upgrade = Some(LastUpgradeRecord {
                target_version: target_version.clone(),
                started_at_ms: now_ms,
                finished_at_ms: None,
                outcome: UpgradeOutcome::Upgrading,
                error: None,
            });
            inner.state.clone()
        };
        let _ = save_update_state(&self.state_path(), &state_to_save);

        let manager = Arc::clone(self);
        let timeout = self.options.upgrade_timeout;
        tokio::spawn(async move {
            let deadline = Instant::now() + timeout;
            let outcome: Result<(), String> = loop {
                match child.try_wait() {
                    Ok(Some(status)) => {
                        if status.success() {
                            break Ok(());
                        }
                        let detail = match status.code() {
                            Some(code) => format!("upgrade command exited with status {code}"),
                            None => "upgrade command terminated by signal".to_string(),
                        };
                        break Err(detail);
                    }
                    Ok(None) => {
                        if Instant::now() >= deadline {
                            let _ = child.kill();
                            let _ = child.wait();
                            break Err(format!("upgrade timed out after {}s", timeout.as_secs()));
                        }
                        tokio::time::sleep(UPGRADE_POLL_INTERVAL).await;
                    }
                    Err(err) => {
                        break Err(format!("failed waiting for upgrade process: {err}"));
                    }
                }
            };

            let finished_ms = now_epoch_ms();
            remove_upgrade_lock(&manager.lock_path());

            let updated_state = {
                let mut inner = manager
                    .inner
                    .write()
                    .unwrap_or_else(|poisoned| poisoned.into_inner());
                match outcome {
                    Ok(()) => {
                        inner.reconciled_upgraded = true;
                        inner.state.last_upgrade = Some(LastUpgradeRecord {
                            target_version,
                            started_at_ms: now_ms,
                            finished_at_ms: Some(finished_ms),
                            outcome: UpgradeOutcome::Succeeded,
                            error: None,
                        });
                    }
                    Err(reason) => {
                        inner.reconciled_upgraded = false;
                        inner.state.last_upgrade = Some(LastUpgradeRecord {
                            target_version,
                            started_at_ms: now_ms,
                            finished_at_ms: Some(finished_ms),
                            outcome: UpgradeOutcome::Failed,
                            error: Some(sanitize_error_message(&reason)),
                        });
                    }
                }
                inner.state.clone()
            };
            let state_path = manager.state_path();
            let _ =
                tokio::task::spawn_blocking(move || save_update_state(&state_path, &updated_state))
                    .await;
        });

        self.get_status()
    }

    fn record_upgrade_failure(
        &self,
        target_version: &str,
        now_ms: u64,
        reason: String,
    ) -> UpdateStatusSnapshot {
        let sanitized = sanitize_error_message(&reason);
        let state_to_save = {
            let mut inner = self
                .inner
                .write()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            inner.reconciled_upgraded = false;
            inner.state.last_upgrade = Some(LastUpgradeRecord {
                target_version: target_version.to_string(),
                started_at_ms: now_ms,
                finished_at_ms: Some(now_ms),
                outcome: UpgradeOutcome::Failed,
                error: Some(sanitized),
            });
            inner.state.clone()
        };
        let _ = save_update_state(&self.state_path(), &state_to_save);
        self.get_status()
    }
}

fn open_truncated_upgrade_log(path: &Path) -> Result<(fs::File, fs::File), String> {
    let (file, _) =
        fs_safe::open_verified_regular_file_0600(path, OpenRegularMode::ReadWriteCreate)
            .map_err(|err| err.to_string())?;
    file.set_len(0).map_err(|err| err.to_string())?;
    let stderr_file = file.try_clone().map_err(|err| err.to_string())?;
    Ok((file, stderr_file))
}

pub async fn fetch_latest_version_from_url(
    client: &reqwest::Client,
    channel: InstallChannel,
    url: &str,
    max_bytes: usize,
) -> Result<String, String> {
    validate_upstream_url(url).map_err(|err| format!("invalid registry URL: {err}"))?;

    let mut resp = client
        .get(url)
        .header(http::header::ACCEPT, "application/json")
        .send()
        .await
        .map_err(|err| format!("registry request failed: {err}"))?;

    if resp.status() != reqwest::StatusCode::OK {
        return Err(format!("unexpected registry HTTP status {}", resp.status()));
    }

    let mut body: Vec<u8> = Vec::new();
    while let Some(chunk) = resp
        .chunk()
        .await
        .map_err(|err| format!("failed reading registry response: {err}"))?
    {
        if body.len().saturating_add(chunk.len()) > max_bytes {
            return Err(format!("registry response exceeds {max_bytes} byte limit"));
        }
        body.extend_from_slice(&chunk);
    }

    parse_registry_version(channel, &body)
}

pub fn parse_registry_version(channel: InstallChannel, body: &[u8]) -> Result<String, String> {
    let value: serde_json::Value =
        serde_json::from_slice(body).map_err(|err| format!("invalid registry JSON: {err}"))?;

    let raw_version = match channel {
        InstallChannel::Npx => value
            .get("version")
            .and_then(|v| v.as_str())
            .ok_or_else(|| "missing npm .version string".to_string())?,
        InstallChannel::Uvx => {
            let info = value
                .get("info")
                .and_then(|v| v.as_object())
                .ok_or_else(|| "missing PyPI .info object".to_string())?;
            if info.get("yanked").and_then(|v| v.as_bool()) == Some(true) {
                return Err("PyPI latest release is yanked".to_string());
            }
            info.get("version")
                .and_then(|v| v.as_str())
                .ok_or_else(|| "missing PyPI .info.version string".to_string())?
        }
    };

    let parsed = SemVer::parse(raw_version)
        .map_err(|err| format!("invalid remote version '{raw_version}': {err}"))?;
    if parsed.is_prerelease() {
        return Err(format!("ignoring remote prerelease version '{parsed}'"));
    }
    Ok(parsed.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::Router;
    use axum::http::StatusCode;
    use axum::response::IntoResponse;
    use axum::routing::get;
    use tempfile::TempDir;
    use tokio::net::TcpListener;

    async fn spawn_mock_registry(router: Router) -> (String, tokio::task::JoinHandle<()>) {
        let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
        let addr = listener.local_addr().expect("addr");
        let handle = tokio::spawn(async move {
            let _ = axum::serve(listener, router).await;
        });
        (format!("http://{addr}"), handle)
    }

    fn test_client() -> reqwest::Client {
        reqwest::Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .connect_timeout(Duration::from_secs(2))
            .timeout(Duration::from_secs(2))
            .no_proxy()
            .build()
            .expect("client")
    }

    #[test]
    fn env_truthy_and_ttl_clock_skew_logic() {
        assert!(!is_env_truthy(""));
        assert!(!is_env_truthy("   "));
        assert!(!is_env_truthy("0"));
        assert!(!is_env_truthy("false"));
        assert!(!is_env_truthy("FALSE"));
        assert!(!is_env_truthy(" False "));
        assert!(is_env_truthy("1"));
        assert!(is_env_truthy("true"));
        assert!(is_env_truthy("yes"));

        let ttl = 12 * 3600 * 1000;
        let skew = 5 * 60 * 1000;
        let now = 2_000_000_000_000u64;

        // None -> must refresh.
        assert!(should_refresh_update_check(None, now, ttl, skew));
        // Fresh -> no refresh.
        assert!(!should_refresh_update_check(
            Some(now - 1000),
            now,
            ttl,
            skew
        ));
        // Exactly at TTL -> refresh.
        assert!(should_refresh_update_check(Some(now - ttl), now, ttl, skew));
        // Slightly in future within skew tolerance -> no refresh.
        assert!(!should_refresh_update_check(
            Some(now + skew),
            now,
            ttl,
            skew
        ));
        // Far in future (> 5m clock skew) -> refresh.
        assert!(should_refresh_update_check(
            Some(now + skew + 1),
            now,
            ttl,
            skew
        ));
    }

    #[test]
    fn sanitize_error_strips_controls_and_truncates_to_200_chars() {
        let noisy = format!("line1\n\t\x1b[31merror\x00detail {}", "x".repeat(300));
        let sanitized = sanitize_error_message(&noisy);
        assert!(sanitized.chars().count() <= MAX_SANITIZED_ERROR_CHARS);
        assert!(!sanitized.chars().any(|c| c.is_control()));
        assert!(sanitized.starts_with("line1"));
    }

    #[tokio::test]
    async fn mock_npm_and_pypi_parsing_rejects_prerelease_yanked_redirects_and_oversized() {
        let router = Router::new()
            .route("/npm/ok", get(|| async { r#"{"version":"0.2.0"}"# }))
            .route(
                "/npm/prerelease",
                get(|| async { r#"{"version":"0.2.0-dev.1"}"# }),
            )
            .route(
                "/npm/redirect",
                get(|| async {
                    (
                        StatusCode::FOUND,
                        [(http::header::LOCATION, "http://127.0.0.1/npm/ok")],
                    )
                        .into_response()
                }),
            )
            .route(
                "/npm/huge",
                get(|| async { " ".repeat(MAX_NPM_RESPONSE_BYTES + 16) }),
            )
            .route(
                "/pypi/ok",
                get(|| async { r#"{"info":{"version":"0.2.1","yanked":false}}"# }),
            )
            .route(
                "/pypi/yanked",
                get(|| async { r#"{"info":{"version":"0.2.1","yanked":true}}"# }),
            )
            .route(
                "/pypi/prerelease",
                get(|| async { r#"{"info":{"version":"0.2.1-rc.1","yanked":false}}"# }),
            );

        let (base, handle) = spawn_mock_registry(router).await;
        let client = test_client();

        assert_eq!(
            fetch_latest_version_from_url(
                &client,
                InstallChannel::Npx,
                &format!("{base}/npm/ok"),
                MAX_NPM_RESPONSE_BYTES,
            )
            .await
            .expect("npm ok"),
            "0.2.0"
        );
        assert!(
            fetch_latest_version_from_url(
                &client,
                InstallChannel::Npx,
                &format!("{base}/npm/prerelease"),
                MAX_NPM_RESPONSE_BYTES,
            )
            .await
            .is_err()
        );
        assert!(
            fetch_latest_version_from_url(
                &client,
                InstallChannel::Npx,
                &format!("{base}/npm/redirect"),
                MAX_NPM_RESPONSE_BYTES,
            )
            .await
            .is_err()
        );
        assert!(
            fetch_latest_version_from_url(
                &client,
                InstallChannel::Npx,
                &format!("{base}/npm/huge"),
                MAX_NPM_RESPONSE_BYTES,
            )
            .await
            .is_err()
        );

        assert_eq!(
            fetch_latest_version_from_url(
                &client,
                InstallChannel::Uvx,
                &format!("{base}/pypi/ok"),
                MAX_PYPI_RESPONSE_BYTES,
            )
            .await
            .expect("pypi ok"),
            "0.2.1"
        );
        assert!(
            fetch_latest_version_from_url(
                &client,
                InstallChannel::Uvx,
                &format!("{base}/pypi/yanked"),
                MAX_PYPI_RESPONSE_BYTES,
            )
            .await
            .is_err()
        );
        assert!(
            fetch_latest_version_from_url(
                &client,
                InstallChannel::Uvx,
                &format!("{base}/pypi/prerelease"),
                MAX_PYPI_RESPONSE_BYTES,
            )
            .await
            .is_err()
        );

        handle.abort();
    }

    #[test]
    fn dev_build_semver_comparison_is_not_suppressed() {
        assert!(version_is_newer("0.1.3", "0.1.3-dev.2"));
        assert!(!version_is_newer("0.1.2", "0.1.3-dev.2"));
        assert!(!version_is_newer("0.1.3", "0.1.3"));
        assert!(version_is_newer("0.1.4", "0.1.3"));
    }

    #[cfg(unix)]
    #[test]
    fn gui_path_resolution_rejects_world_writable_allows_symlinks_and_prepends_parent() {
        use std::os::unix::fs::{PermissionsExt, symlink};

        let tmp = TempDir::new().expect("tmp");
        let unsafe_dir = tmp.path().join("world-writable");
        let safe_dir = tmp.path().join("safe-bin");
        let real_bin_dir = tmp.path().join("real-bin");
        fs::create_dir_all(&unsafe_dir).unwrap();
        fs::create_dir_all(&safe_dir).unwrap();
        fs::create_dir_all(&real_bin_dir).unwrap();

        let bad_npx = unsafe_dir.join("npx");
        fs::write(&bad_npx, "#!/bin/sh\nexit 0\n").unwrap();
        fs::set_permissions(&bad_npx, fs::Permissions::from_mode(0o755)).unwrap();
        fs::set_permissions(&unsafe_dir, fs::Permissions::from_mode(0o777)).unwrap();

        let real_npx = real_bin_dir.join("npx-target");
        fs::write(&real_npx, "#!/bin/sh\nexit 0\n").unwrap();
        fs::set_permissions(&real_npx, fs::Permissions::from_mode(0o755)).unwrap();
        let symlink_npx = safe_dir.join("npx");
        symlink(&real_npx, &symlink_npx).unwrap();
        fs::set_permissions(&safe_dir, fs::Permissions::from_mode(0o755)).unwrap();

        let path_env = std::env::join_paths([&unsafe_dir, &safe_dir]).unwrap();
        let resolved =
            resolve_upgrade_executable_with_env(InstallChannel::Npx, Some(&path_env), None)
                .expect("should skip world-writable dir and resolve symlinked npx");
        assert_eq!(resolved, symlink_npx);

        let child_path =
            build_child_path(&resolved, Some(OsStr::new("/usr/bin"))).expect("child PATH");
        let parts: Vec<PathBuf> = std::env::split_paths(&child_path).collect();
        assert_eq!(parts.first(), Some(&safe_dir));
        assert!(parts.contains(&PathBuf::from("/usr/bin")));
    }

    #[test]
    fn build_upgrade_command_scrubs_sensitive_env_and_uses_static_argv() {
        let tmp = TempDir::new().expect("tmp");
        let exe = tmp.path().join("bin").join("npx");
        let custom_cfg = tmp.path().join("custom-config.json");
        let custom_plugins = tmp.path().join("custom-plugins");

        let env_pairs = vec![
            (
                OsString::from("ANTIGRAVITY_SIDECAR_WEB_PORT"),
                OsString::from("12345"),
            ),
            (
                OsString::from("ANTIGRAVITY_SIDECAR_UI_TOKEN"),
                OsString::from("secret-tok"),
            ),
            (
                OsString::from("ANTIGRAVITY_LS_ADDRESS"),
                OsString::from("127.0.0.1:9999"),
            ),
            (
                OsString::from("AGY_COMPANION_NO_UPDATE_CHECK"),
                OsString::from("1"),
            ),
            (
                OsString::from("AGY_COMPANION_HOOK_ACTIVE"),
                OsString::from("1"),
            ),
            (
                OsString::from("JETSKI_MCP_ENV_RESTORE_VARS"),
                OsString::from("FOO"),
            ),
            (
                OsString::from("CLOUD_CODE_URL"),
                OsString::from("http://127.0.0.1:24141"),
            ),
            (
                OsString::from("GOOGLE_GEMINI_BASE_URL"),
                OsString::from("http://127.0.0.1:24141"),
            ),
            (
                OsString::from("GEMINI_API_KEY"),
                OsString::from("AIzaFakeTestKeyForScrubCheck123456"),
            ),
            (OsString::from("OPENAI_API_KEY"), OsString::from("sk-test")),
            (
                OsString::from("ANTHROPIC_API_KEY"),
                OsString::from("sk-ant-test"),
            ),
            (
                OsString::from("OLLAMA_API_KEY"),
                OsString::from("ollama-test"),
            ),
            (
                OsString::from("AGY_PLUGINS_DIR"),
                custom_plugins.as_os_str().to_os_string(),
            ),
        ];

        let cmd = build_upgrade_command(
            &exe,
            InstallChannel::Npx,
            &custom_cfg,
            Some(OsStr::new("/usr/bin")),
            env_pairs,
        );

        let args: Vec<&OsStr> = cmd.get_args().collect();
        assert_eq!(
            args,
            vec![
                OsStr::new("-y"),
                OsStr::new("agy-companion@latest"),
                OsStr::new("install"),
                OsStr::new("--channel"),
                OsStr::new("npx"),
            ]
        );

        let envs: std::collections::HashMap<&OsStr, Option<&OsStr>> = cmd.get_envs().collect();
        for removed in [
            "ANTIGRAVITY_SIDECAR_WEB_PORT",
            "ANTIGRAVITY_SIDECAR_UI_TOKEN",
            "ANTIGRAVITY_LS_ADDRESS",
            "AGY_COMPANION_NO_UPDATE_CHECK",
            "AGY_COMPANION_HOOK_ACTIVE",
            "JETSKI_MCP_ENV_RESTORE_VARS",
            "CLOUD_CODE_URL",
            "GOOGLE_GEMINI_BASE_URL",
            "GEMINI_API_KEY",
            "OPENAI_API_KEY",
            "ANTHROPIC_API_KEY",
            "OLLAMA_API_KEY",
        ] {
            assert_eq!(
                envs.get(OsStr::new(removed)),
                Some(&None),
                "{removed} must be scrubbed"
            );
        }
        assert_eq!(
            envs.get(OsStr::new(CONFIG_PATH_ENV_VAR)),
            Some(&Some(custom_cfg.as_os_str()))
        );
        assert_eq!(
            envs.get(OsStr::new("AGY_PLUGINS_DIR")),
            Some(&Some(custom_plugins.as_os_str()))
        );
    }

    #[test]
    fn startup_reconciliation_handles_execve_success_and_stale_lock_failure() {
        let tmp = TempDir::new().expect("tmp");
        let data_dir = tmp.path().join("data");
        let cfg_path = tmp.path().join("config.json");
        let state_path = update_state_path(&data_dir);
        let lock_path = data_dir.join(UPGRADE_LOCK_FILENAME);

        // Case 1: execve landed on target version -> marks Succeeded, clears lock, surfaces Upgraded.
        let upgrading_state = UpdateState {
            schema_version: companion_core::UPDATE_STATE_SCHEMA_VERSION,
            install_channel: InstallChannel::Npx,
            checked_at_ms: Some(now_epoch_ms()),
            latest_version: Some("0.2.0".to_string()),
            last_upgrade: Some(LastUpgradeRecord {
                target_version: "0.2.0".to_string(),
                started_at_ms: now_epoch_ms().saturating_sub(5_000),
                finished_at_ms: None,
                outcome: UpgradeOutcome::Upgrading,
                error: None,
            }),
        };
        save_update_state(&state_path, &upgrading_state).unwrap();
        write_upgrade_lock(
            &lock_path,
            &UpgradeLockInfo {
                pid: 999_999,
                started_at_ms: now_epoch_ms().saturating_sub(5_000),
            },
        )
        .unwrap();

        let mut opts = UpdateManagerOptions::for_paths(cfg_path.clone(), data_dir.clone());
        opts.current_version = "0.2.0".to_string();
        opts.disabled_by_env = Some(false);
        opts.executable_override = Some(Some(PathBuf::from("/usr/local/bin/npx")));

        let mgr = UpdateManager::new(opts).expect("manager");
        let snap = mgr.get_status();
        assert_eq!(snap.state, UpdateLifecycleState::Upgraded);
        assert!(
            !lock_path.exists(),
            "upgrade.lock must be cleared on reconciliation"
        );
        let persisted = load_update_state(&state_path).unwrap().unwrap();
        assert_eq!(
            persisted.last_upgrade.unwrap().outcome,
            UpgradeOutcome::Succeeded
        );

        // If a subsequent check discovers a release newer than both current_version and target_version,
        // state must transition from Upgraded -> Available rather than staying stuck on Upgraded.
        {
            let mut inner = mgr.inner.write().unwrap();
            inner.state.latest_version = Some("0.3.0".to_string());
        }
        assert_eq!(mgr.get_status().state, UpdateLifecycleState::Available);

        // Case 2: still on old version and lock is stale/expired -> marks Failed and clears lock.
        save_update_state(&state_path, &upgrading_state).unwrap();
        write_upgrade_lock(
            &lock_path,
            &UpgradeLockInfo {
                pid: 999_999,
                started_at_ms: now_epoch_ms().saturating_sub(300_000),
            },
        )
        .unwrap();
        let mut opts2 = UpdateManagerOptions::for_paths(cfg_path, data_dir);
        opts2.current_version = "0.1.0".to_string();
        opts2.disabled_by_env = Some(false);
        opts2.executable_override = Some(Some(PathBuf::from("/usr/local/bin/npx")));

        let mgr2 = UpdateManager::new(opts2).expect("manager2");
        let snap2 = mgr2.get_status();
        assert_eq!(snap2.state, UpdateLifecycleState::Failed);
        assert!(snap2.error.is_some());
        assert!(!lock_path.exists(), "stale upgrade.lock must be removed");
        let persisted2 = load_update_state(&state_path).unwrap().unwrap();
        assert_eq!(
            persisted2.last_upgrade.unwrap().outcome,
            UpgradeOutcome::Failed
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn start_upgrade_is_idempotent_writes_0600_log_and_completes() {
        use std::os::unix::fs::PermissionsExt;

        let tmp = TempDir::new().expect("tmp");
        let data_dir = tmp.path().join("data");
        let cfg_path = tmp.path().join("config.json");
        let fake_npx = tmp.path().join("fake-npx");
        let marker = tmp.path().join("argv.txt");

        let script = format!(
            "#!/bin/sh\nprintf '%s\\n' \"$@\" > '{}'\nsleep 0.2\necho 'upgrade ok'\nexit 0\n",
            marker.display()
        );
        fs::write(&fake_npx, script).unwrap();
        fs::set_permissions(&fake_npx, fs::Permissions::from_mode(0o755)).unwrap();

        let mut opts = UpdateManagerOptions::for_paths(cfg_path, data_dir.clone());
        opts.current_version = "0.1.0".to_string();
        opts.disabled_by_env = Some(false);
        opts.executable_override = Some(Some(fake_npx));
        opts.upgrade_timeout = Duration::from_secs(10);

        let mgr = Arc::new(UpdateManager::new(opts).expect("manager"));
        let first = mgr.start_upgrade().await;
        assert_eq!(first.state, UpdateLifecycleState::Upgrading);

        // Second call while in flight must be idempotent.
        let second = mgr.start_upgrade().await;
        assert_eq!(second.state, UpdateLifecycleState::Upgrading);

        // Wait for background waiter to finish.
        for _ in 0..50 {
            if mgr.get_status().state == UpdateLifecycleState::Upgraded {
                break;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        assert_eq!(mgr.get_status().state, UpdateLifecycleState::Upgraded);
        assert!(!data_dir.join(UPGRADE_LOCK_FILENAME).exists());

        let log_path = data_dir.join(UPGRADE_LOG_FILENAME);
        let log_mode = fs::metadata(&log_path).unwrap().permissions().mode() & 0o777;
        assert_eq!(log_mode, 0o600);
        let log_text = fs::read_to_string(&log_path).unwrap();
        assert!(log_text.contains("upgrade ok"));

        let argv = fs::read_to_string(&marker).unwrap();
        assert_eq!(argv, "-y\nagy-companion@latest\ninstall\n--channel\nnpx\n");
    }

    #[tokio::test]
    async fn config_opt_out_disables_checks_and_errors_back_off_without_clobbering_cache() {
        let tmp = TempDir::new().expect("tmp");
        let data_dir = tmp.path().join("data");
        let cfg_path = tmp.path().join("config.json");

        // 1. config.check_for_updates == Some(false) disables status and skips network.
        let cfg = Config {
            check_for_updates: Some(false),
            ..Default::default()
        };
        cfg.save_secure(&cfg_path).unwrap();

        let mut opts = UpdateManagerOptions::for_paths(cfg_path.clone(), data_dir.clone());
        opts.current_version = "0.1.0".to_string();
        opts.disabled_by_env = Some(false);
        opts.npm_registry_url = "http://127.0.0.1:1/unreachable".to_string();
        let mgr = UpdateManager::new(opts).expect("manager");
        assert_eq!(mgr.get_status().state, UpdateLifecycleState::Disabled);
        assert_eq!(
            mgr.check_update_manual().await.state,
            UpdateLifecycleState::Disabled
        );

        // 2. Re-enable in config, seed cached state, trigger offline error -> cache preserved & backoff active.
        let enabled_cfg = Config {
            check_for_updates: Some(true),
            ..Default::default()
        };
        enabled_cfg.save_secure(&cfg_path).unwrap();

        let state_path = update_state_path(&data_dir);
        save_update_state(
            &state_path,
            &UpdateState {
                schema_version: companion_core::UPDATE_STATE_SCHEMA_VERSION,
                install_channel: InstallChannel::Npx,
                checked_at_ms: Some(1_000),
                latest_version: Some("0.2.0".to_string()),
                last_upgrade: None,
            },
        )
        .unwrap();

        let mut opts2 = UpdateManagerOptions::for_paths(cfg_path, data_dir);
        opts2.current_version = "0.1.0".to_string();
        opts2.disabled_by_env = Some(false);
        opts2.npm_registry_url = "http://127.0.0.1:1/unreachable".to_string();
        opts2.error_backoff = Duration::from_secs(3600);
        let mgr2 = UpdateManager::new(opts2).expect("manager2");
        mgr2.check_if_due().await;
        let after = mgr2.get_status();
        assert_eq!(after.state, UpdateLifecycleState::Available);
        assert_eq!(after.latest_version.as_deref(), Some("0.2.0"));
        assert_eq!(after.checked_at_ms, Some(1_000));
    }
}
