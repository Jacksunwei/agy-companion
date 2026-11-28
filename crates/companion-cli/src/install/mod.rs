mod bundle;
mod format;
mod proxy;
mod safety;
mod uninstall;
mod windows;

#[cfg(test)]
mod tests;

use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::Duration;

use companion_core::fs_safe::{self, FsError, PathLeafKind, inspect_leaf_kind};
use companion_core::integration::{IntegrationOptions, IntegrationStatus};
use companion_core::{
    CoreError, DEFAULT_LISTEN_PORT, INSTALL_CHANNEL_ENV_VAR, InstallChannel, UpdateState,
    load_update_state, save_update_state, update_state_path,
};
use companion_providers::LoopbackProxyClient;
use serde::{Deserialize, Serialize};

#[cfg(unix)]
use bundle::install_local_bin_symlink;
use bundle::{
    STABLE_BIN_NAME, files_have_identical_bytes, is_companion_plugin_dir, is_same_file_path,
    is_stable_bin_ready_exact, select_fresh_install_port, stage_binary_copy, stage_version_file,
    swap_plugin_dir_two_step, swap_staged_binary_and_version, write_staged_plugin_dir,
};
use format::format_install_report;
use proxy::{InstallProxyContext, InstallProxyOutcome, ensure_installed_proxy_running};
use safety::{
    ExistingDirSafety, ensure_non_symlink_dir, inspect_config_dir, inspect_data_dir,
    inspect_existing_dir, inspect_non_symlink_rel_chain, resolve_home_dir, resolve_plugins_root,
};
#[cfg(windows)]
use windows::create_windows_start_menu_shortcut;

use crate::daemon::resolve_runtime_data_dir;
use crate::process::{detect_antigravity_running, is_binary_on_path};
use crate::{
    BINARY_NAME, CliError, load_config_checked, normalized_metadata_path,
    precheck_existing_config_file, precheck_existing_data_dir, resolve_cli_config_path,
};

pub use bundle::{EMBEDDED_PLUGIN_FILES, EmbeddedPluginFile, PLUGIN_DIR_NAME};
pub use uninstall::run_uninstall;
pub use windows::WINDOWS_SHORTCUT_NAME;

const PORT_FALLBACK_SPAN: u16 = 9; // 24141..=24150
pub(super) const INSTALL_HEALTH_PROBE_TIMEOUT: Duration = Duration::from_millis(500);

#[derive(Debug, Clone, Default)]
pub struct InstallOptions {
    pub config: Option<PathBuf>,
    pub plugins_dir: Option<PathBuf>,
    pub no_launcher_hook: bool,
    pub no_start_proxy: bool,
    pub force: bool,
    pub channel: Option<InstallChannel>,
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
    pub install_channel: InstallChannel,
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

fn has_uvx_archive_hint(raw: &str) -> bool {
    let normalized = raw.replace('\\', "/").to_ascii_lowercase();
    normalized.contains("uv/archive-v") || normalized.contains("uv/cache/archive-v")
}

pub(crate) fn resolve_install_channel(
    cli_channel: Option<InstallChannel>,
    env_channel: Option<&str>,
    current_exe: Option<&Path>,
    virtual_env: Option<&str>,
    existing_state: Option<&UpdateState>,
) -> InstallChannel {
    if let Some(ch) = cli_channel {
        return ch;
    }
    if let Some(raw) = env_channel
        && let Ok(ch) = raw.parse::<InstallChannel>()
    {
        return ch;
    }
    if current_exe
        .and_then(|p| p.to_str())
        .is_some_and(has_uvx_archive_hint)
        || virtual_env.is_some_and(has_uvx_archive_hint)
    {
        return InstallChannel::Uvx;
    }
    if let Some(state) = existing_state {
        return state.install_channel;
    }
    InstallChannel::Npx
}

pub(crate) fn next_update_state(
    existing: Option<UpdateState>,
    resolved_channel: InstallChannel,
) -> UpdateState {
    match existing {
        Some(mut state) => {
            if state.install_channel != resolved_channel {
                state.install_channel = resolved_channel;
                state.checked_at_ms = None;
                state.latest_version = None;
            }
            state
        }
        None => UpdateState::new(resolved_channel),
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
        match inspect_leaf_kind(&plugin_target) {
            Ok(PathLeafKind::Symlink) => {
                if !opts.force {
                    return Err(CliError::Install(format!(
                        "plugin path '{}' is a symlink (e.g. a legacy dev symlink); re-run with --force to replace the symlink without deleting its target",
                        plugin_target.display()
                    )));
                }
            }
            Ok(PathLeafKind::Directory) => {
                if !opts.force && !is_companion_plugin_dir(&plugin_target)? {
                    return Err(CliError::Install(format!(
                        "refusing to overwrite '{}' because it does not contain a valid agy-companion plugin.json (re-run with --force to override)",
                        plugin_target.display()
                    )));
                }
            }
            Ok(PathLeafKind::RegularFile | PathLeafKind::Other) => {
                return Err(CliError::Install(format!(
                    "plugin path '{}' exists and is not a directory",
                    plugin_target.display()
                )));
            }
            Ok(PathLeafKind::Missing) => {}
            Err(FsError::Io { source, .. }) => return Err(CliError::Io(source)),
            Err(err) => return Err(CliError::from(err)),
        }
    }

    let config_path = resolve_cli_config_path(opts.config.as_deref())?;
    let norm_config_path = normalized_metadata_path(&config_path);
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
        let lock_path = data_dir.join(companion_core::integration::INTEGRATION_LOCK_FILENAME);
        fs_safe::inspect_regular_file_if_exists(&lock_path, None)?;
    }
    #[cfg(not(windows))]
    if !opts.no_launcher_hook
        && data_dir_safety == ExistingDirSafety::SafeDir
        && companion_core::integration::detect_shell(std::env::var("SHELL").ok().as_deref())
            != companion_core::integration::ShellKind::Fish
    {
        let env_path = data_dir.join(companion_core::integration::ENV_SCRIPT_FILENAME);
        match fs_safe::inspect_regular_file_if_exists(&env_path, None) {
            Ok(_) => {}
            Err(FsError::SymlinkRejected { .. }) => {
                return Err(CliError::Install(format!(
                    "refusing to use symlinked env script '{}'",
                    env_path.display()
                )));
            }
            Err(FsError::NotRegularFile { .. }) => {
                return Err(CliError::Install(format!(
                    "refusing to use non-regular env script '{}'",
                    env_path.display()
                )));
            }
            Err(FsError::Io { source, .. }) => {
                return Err(CliError::Install(format!(
                    "failed to inspect env script '{}': {source}",
                    env_path.display()
                )));
            }
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
    // Defer LoopbackProxyClient creation (and TLS root-cert init) until a probe is actually needed,
    // and defer persisting config.json until preconditions, staging, and swaps succeed.
    let mut probe: Option<LoopbackProxyClient> = None;
    let mut port_auto_selected = false;
    if !config_existed {
        let probe_ref = probe.get_or_insert(LoopbackProxyClient::new(
            INSTALL_HEALTH_PROBE_TIMEOUT,
            INSTALL_HEALTH_PROBE_TIMEOUT,
        )?);
        let (selected_port, auto_selected) = select_fresh_install_port(
            probe_ref,
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
    let update_state_file = update_state_path(&runtime_data_dir);
    let existing_update_state = match load_update_state(&update_state_file) {
        Ok(state) => state,
        Err(
            err @ CoreError::Fs(FsError::SymlinkRejected { .. } | FsError::NotRegularFile { .. }),
        ) => {
            return Err(CliError::from(err));
        }
        Err(_) => None,
    };

    // 2. Stage binary, .version, and 6-file plugin directory BEFORE swapping anything.
    ensure_non_symlink_dir(&plugins_root, 0o755)?;
    ensure_non_symlink_dir(&data_dir, 0o700)?;
    let bin_dir = data_dir.join("bin");
    ensure_non_symlink_dir(&bin_dir, 0o755)?;
    let stable_bin = bin_dir.join(STABLE_BIN_NAME);
    let version_file = bin_dir.join(".version");

    let current_exe = std::env::current_exe()?;
    let env_channel = std::env::var(INSTALL_CHANNEL_ENV_VAR).ok();
    let virtual_env = std::env::var("VIRTUAL_ENV").ok();
    let install_channel = resolve_install_channel(
        opts.channel,
        env_channel.as_deref(),
        Some(&current_exe),
        virtual_env.as_deref(),
        existing_update_state.as_ref(),
    );
    let binary_changed = !files_have_identical_bytes(&current_exe, &stable_bin);
    let skip_binary_rewrite = (cfg!(windows) && is_same_file_path(&current_exe, &stable_bin))
        || (!binary_changed && is_stable_bin_ready_exact(&stable_bin));
    let staged_bin = if skip_binary_rewrite {
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
    let next_state = next_update_state(existing_update_state, install_channel);
    save_update_state(&update_state_file, &next_state)?;

    // 4. Proxy startup or version-skew replacement (unless --no-start-proxy).
    let proxy_outcome = if opts.no_start_proxy {
        InstallProxyOutcome::default()
    } else {
        let probe_ref = match probe.as_ref() {
            Some(client) => client,
            None => probe.insert(LoopbackProxyClient::new(
                INSTALL_HEALTH_PROBE_TIMEOUT,
                INSTALL_HEALTH_PROBE_TIMEOUT,
            )?),
        };
        ensure_installed_proxy_running(
            &InstallProxyContext {
                probe: probe_ref,
                stable_bin: &stable_bin,
                config_path: &config_path,
                data_dir: &data_dir,
                runtime_data_dir: &runtime_data_dir,
                listen_port,
                version: &version,
                binary_changed,
            },
            &mut warnings,
        )
        .await?
    };

    // 5. Launcher hook (Unix) or Start Menu shortcut (Windows).
    let mut launcher_integration: Option<IntegrationStatus> = None;
    #[cfg_attr(not(windows), allow(unused_mut))]
    let mut windows_shortcut: Option<PathBuf> = None;

    if opts.no_launcher_hook {
        companion_core::integration::set_launcher_auto_start_locked(
            &config_path,
            Some(&data_dir),
            false,
        )?;
    } else {
        #[cfg(not(windows))]
        {
            let status = companion_core::integration::enable_and_persist(
                &config_path,
                &IntegrationOptions {
                    shell: None,
                    rc_file: None,
                    binary_path: Some(stable_bin.clone()),
                    data_dir: None,
                    follow_rc_symlink: false,
                },
            )?;
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
        install_channel,
        binary_path: stable_bin,
        local_bin_symlink,
        plugin_dir: plugin_target,
        listen_port,
        port_auto_selected,
        launcher_integration,
        windows_shortcut,
        proxy_started: proxy_outcome.started,
        proxy_replaced: proxy_outcome.replaced,
        proxy_version: proxy_outcome.version,
        antigravity_running,
        uninstall_command,
        warnings,
    };

    format_install_report(&report, opts.json, out)
}
