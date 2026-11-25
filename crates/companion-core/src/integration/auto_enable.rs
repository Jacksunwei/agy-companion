use std::path::{Path, PathBuf};

use crate::config::{Config, CoreError, default_data_dir};
use crate::fs_safe;

use super::disable::disable_with_env;
use super::enable::enable_with_env;
use super::lifecycle::{
    LifecycleEnv, build_unsupported_status, read_process_env_inputs, status_with_env,
};
use super::lock::IntegrationLock;
use super::{
    IntegrationOptions, IntegrationState, IntegrationStatus, detect_hybrid_active,
    is_platform_supported,
};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AutoEnableOutcome {
    /// `launcher_auto_start` was already persisted (`Some(true)` or `Some(false)`).
    AlreadyInitialized { launcher_auto_start: bool },
    /// Platform does not use Unix login-shell RC integration (Windows).
    UnsupportedPlatform,
    /// Hook or Hybrid Mode was already active on first run; persisted `Some(true)` without rewriting RC.
    AlreadyEnabled { status: IntegrationStatus },
    /// Ran `integration::enable` (`follow_rc_symlink: false`) and persisted `Some(true)`.
    Attempted { status: IntegrationStatus },
}

fn resolve_lock_data_dir(
    opts: &IntegrationOptions,
    env: &LifecycleEnv<'_>,
) -> Result<PathBuf, CoreError> {
    if let Some(custom) = opts.data_dir.as_deref() {
        fs_safe::validate_path(custom)?;
        return Ok(custom.to_path_buf());
    }
    if let Some(dir) = env.default_data_override {
        fs_safe::validate_path(dir)?;
        return Ok(dir.to_path_buf());
    }
    default_data_dir()
}

/// Runs the locked, idempotent first-run launcher auto-enable state machine.
///
/// Under `<data_dir>/integration.lock`, re-reads `config_path`:
/// - If `launcher_auto_start` is already `Some(_)`, returns `AlreadyInitialized` without touching RC files.
/// - On unsupported platforms (Windows), returns `UnsupportedPlatform` without touching RC files or config.
/// - Otherwise checks `integration::status`; if already enabled or `hybrid_active`, persists `Some(true)`;
///   else runs `integration::enable` with `follow_rc_symlink: false` and persists `Some(true)` (including
///   on `ManualRequired` to avoid retry loops).
pub fn ensure_first_run_launcher_enabled(
    config_path: &Path,
    opts: &IntegrationOptions,
) -> Result<AutoEnableOutcome, CoreError> {
    let inputs = read_process_env_inputs()?;
    let env = LifecycleEnv::from_process(&inputs, detect_hybrid_active(), is_platform_supported());
    ensure_first_run_launcher_enabled_with_env(config_path, opts, &env)
}

pub(super) fn ensure_first_run_launcher_enabled_with_env(
    config_path: &Path,
    opts: &IntegrationOptions,
    env: &LifecycleEnv<'_>,
) -> Result<AutoEnableOutcome, CoreError> {
    fs_safe::validate_path(config_path)?;
    if !env.platform_supported {
        return Ok(AutoEnableOutcome::UnsupportedPlatform);
    }

    let lock_dir = resolve_lock_data_dir(opts, env)?;
    let _lock = IntegrationLock::acquire_with_home(&lock_dir, env.home)?;

    let mut cfg = Config::load_or_default(config_path)?;
    if let Some(launcher_auto_start) = cfg.launcher_auto_start {
        return Ok(AutoEnableOutcome::AlreadyInitialized {
            launcher_auto_start,
        });
    }

    let safe_opts = IntegrationOptions {
        follow_rc_symlink: false,
        ..opts.clone()
    };
    let current = status_with_env(&safe_opts, env)?;
    if current.state() == IntegrationState::Unsupported {
        return Ok(AutoEnableOutcome::UnsupportedPlatform);
    }
    if current.enabled || current.hybrid_active {
        cfg.launcher_auto_start = Some(true);
        cfg.save_secure(config_path)?;
        return Ok(AutoEnableOutcome::AlreadyEnabled { status: current });
    }

    let status = enable_with_env(&safe_opts, env)?;
    if status.state() == IntegrationState::Unsupported {
        return Ok(AutoEnableOutcome::UnsupportedPlatform);
    }
    cfg.launcher_auto_start = Some(true);
    cfg.save_secure(config_path)?;
    Ok(AutoEnableOutcome::Attempted { status })
}

/// Persists `launcher_auto_start = Some(value)` under `<data_dir>/integration.lock`.
pub fn set_launcher_auto_start_locked(
    config_path: &Path,
    data_dir: Option<&Path>,
    value: bool,
) -> Result<Config, CoreError> {
    let home = fs_safe::default_home_dir();
    set_launcher_auto_start_locked_with_home(config_path, data_dir, home.as_deref(), value)
}

pub(super) fn set_launcher_auto_start_locked_with_home(
    config_path: &Path,
    data_dir: Option<&Path>,
    home: Option<&Path>,
    value: bool,
) -> Result<Config, CoreError> {
    fs_safe::validate_path(config_path)?;
    let lock_dir = match data_dir {
        Some(dir) => {
            fs_safe::validate_path(dir)?;
            dir.to_path_buf()
        }
        None => default_data_dir()?,
    };
    let _lock = IntegrationLock::acquire_with_home(&lock_dir, home)?;
    let mut cfg = Config::load_or_default(config_path)?;
    cfg.launcher_auto_start = Some(value);
    cfg.save_secure(config_path)?;
    Ok(cfg)
}

/// Runs `integration::enable` and persists `launcher_auto_start = Some(true)` under `integration.lock`.
/// Validates `config_path` before mutating any RC file (fail-closed S2/S6).
pub fn enable_and_persist(
    config_path: &Path,
    opts: &IntegrationOptions,
) -> Result<IntegrationStatus, CoreError> {
    let inputs = read_process_env_inputs()?;
    let env = LifecycleEnv::from_process(&inputs, detect_hybrid_active(), is_platform_supported());
    enable_and_persist_with_env(config_path, opts, &env)
}

pub(super) fn enable_and_persist_with_env(
    config_path: &Path,
    opts: &IntegrationOptions,
    env: &LifecycleEnv<'_>,
) -> Result<IntegrationStatus, CoreError> {
    fs_safe::validate_path(config_path)?;
    if !env.platform_supported {
        return Ok(build_unsupported_status(env.hybrid_active));
    }

    let lock_dir = resolve_lock_data_dir(opts, env)?;
    let _lock = IntegrationLock::acquire_with_home(&lock_dir, env.home)?;

    // Validate/load config BEFORE mutating RC files so corrupt or symlinked configs fail closed.
    let mut cfg = Config::load_or_default(config_path)?;
    let status = enable_with_env(opts, env)?;
    if matches!(
        status.state(),
        IntegrationState::Enabled | IntegrationState::Active | IntegrationState::ManualRequired
    ) {
        cfg.launcher_auto_start = Some(true);
        cfg.save_secure(config_path)?;
    }
    Ok(status)
}

/// Runs `integration::disable` and persists `launcher_auto_start = Some(false)` under `integration.lock`.
/// Validates `config_path` before mutating any RC file (fail-closed S2/S6).
pub fn disable_and_persist(
    config_path: &Path,
    opts: &IntegrationOptions,
) -> Result<IntegrationStatus, CoreError> {
    let inputs = read_process_env_inputs()?;
    let env = LifecycleEnv::from_process(&inputs, detect_hybrid_active(), is_platform_supported());
    disable_and_persist_with_env(config_path, opts, &env)
}

pub(super) fn disable_and_persist_with_env(
    config_path: &Path,
    opts: &IntegrationOptions,
    env: &LifecycleEnv<'_>,
) -> Result<IntegrationStatus, CoreError> {
    fs_safe::validate_path(config_path)?;
    if !env.platform_supported {
        return Ok(build_unsupported_status(env.hybrid_active));
    }

    let lock_dir = resolve_lock_data_dir(opts, env)?;
    let _lock = IntegrationLock::acquire_with_home(&lock_dir, env.home)?;

    // Validate/load config BEFORE mutating RC files so corrupt or symlinked configs fail closed.
    let mut cfg = Config::load_or_default(config_path)?;
    let status = disable_with_env(opts, env)?;
    if status.state() != IntegrationState::Unsupported {
        cfg.launcher_auto_start = Some(false);
        cfg.save_secure(config_path)?;
    }
    Ok(status)
}
