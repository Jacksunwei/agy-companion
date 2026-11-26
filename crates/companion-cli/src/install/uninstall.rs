use std::fs;
use std::io::Write;
use std::path::PathBuf;

use companion_core::Config;
use companion_core::fs_safe::{FsError, PathLeafKind, inspect_leaf_kind, unlink_symlink};
#[cfg(windows)]
use companion_core::fs_safe::{
    inspect_regular_file_if_exists, rename_with_retry, unique_temp_suffix,
};
use companion_providers::LoopbackProxyClient;

#[cfg(unix)]
use super::bundle::symlink_points_to_path;
use super::bundle::{PLUGIN_DIR_NAME, STABLE_BIN_NAME, is_companion_plugin_dir};
use super::format::format_uninstall_report;
use super::safety::{
    ExistingDirSafety, deduplicate_paths, inspect_config_dir, inspect_data_dir,
    inspect_existing_dir, inspect_non_symlink_rel_chain, remove_dir_if_non_symlink,
    remove_regular_file_if_present, resolve_home_dir, resolve_plugins_root, usage_dir_under_dir,
};
#[cfg(windows)]
use super::windows::remove_windows_start_menu_shortcut;
use super::{INSTALL_HEALTH_PROBE_TIMEOUT, UninstallOptions, UninstallReport};
use crate::daemon::{
    PROXY_LOG_FILENAME, PROXY_LOG_ROTATED_FILENAME, PROXY_PID_FILENAME, resolve_runtime_data_dir,
};
use crate::process::stop_proxy_from_pid_files;
use crate::{
    BINARY_NAME, CliError, load_config_checked, normalized_metadata_path, resolve_cli_config_path,
};

pub async fn run_uninstall<W: Write>(opts: UninstallOptions, out: &mut W) -> Result<(), CliError> {
    let mut warnings: Vec<String> = Vec::new();

    // 1. Validate inputs fail-closed before touching anything.
    let home = resolve_home_dir()?;
    let plugins_root = resolve_plugins_root(opts.plugins_dir.as_deref())?;
    let plugin_target = plugins_root.join(PLUGIN_DIR_NAME);

    let config_path = resolve_cli_config_path(opts.config.as_deref())?;
    let norm_config_path = normalized_metadata_path(&config_path);
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
        match inspect_leaf_kind(&plugin_target) {
            Ok(PathLeafKind::Symlink) => {
                // Unlink symlink ONLY; never delete the symlink target directory.
                if !opts.dry_run {
                    unlink_symlink(&plugin_target)?;
                }
                plugin_dir_removed = true;
            }
            Ok(PathLeafKind::Directory) => match is_companion_plugin_dir(&plugin_target) {
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
            },
            Ok(PathLeafKind::RegularFile | PathLeafKind::Other) => {
                warnings.push(format!(
                    "refusing to remove non-directory plugin path '{}'",
                    plugin_target.display()
                ));
            }
            Ok(PathLeafKind::Missing) => {}
            Err(FsError::Io { source, .. }) => return Err(CliError::Io(source)),
            Err(err) => return Err(CliError::from(err)),
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
    let (proxy_stopped, proxy_pid) = if pid_candidates
        .iter()
        .any(|p| !matches!(inspect_leaf_kind(p), Ok(PathLeafKind::Missing)))
    {
        let probe =
            LoopbackProxyClient::new(INSTALL_HEALTH_PROBE_TIMEOUT, INSTALL_HEALTH_PROBE_TIMEOUT)?;
        stop_proxy_from_pid_files(&probe, &pid_candidates, None, opts.dry_run, &mut warnings).await
    } else {
        (false, None)
    };

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
        match inspect_leaf_kind(&local_bin_link) {
            Ok(PathLeafKind::Symlink) => {
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
            Ok(PathLeafKind::RegularFile | PathLeafKind::Directory | PathLeafKind::Other) => {
                warnings.push(format!(
                    "preserving non-symlink file '{}'",
                    local_bin_link.display()
                ));
                false
            }
            Ok(PathLeafKind::Missing) => false,
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
            match inspect_regular_file_if_exists(&stable_bin, None) {
                Ok(Some(_)) => {
                    if opts.dry_run {
                        true
                    } else if let Err(rm_err) = fs::remove_file(&stable_bin) {
                        let renamed = bin_dir
                            .join(format!(".{BINARY_NAME}.old.{}.exe", unique_temp_suffix()));
                        if rename_with_retry(&stable_bin, &renamed).is_ok() {
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
                Ok(None)
                | Err(FsError::SymlinkRejected { .. } | FsError::NotRegularFile { .. }) => false,
                Err(FsError::Io { source, .. }) => return Err(CliError::Io(source)),
                Err(err) => return Err(CliError::from(err)),
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
    let auth_dir = companion_core::auth_dir_for_config_path(&norm_config_path)?;

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
        if config_dir_safety == ExistingDirSafety::SafeDir
            && remove_dir_if_non_symlink(&auth_dir, opts.dry_run, &mut warnings)?
        {
            purged_paths.push(auth_dir.clone());
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
        if data_dir_safety == ExistingDirSafety::SafeDir {
            let lock_path = data_dir.join(companion_core::integration::INTEGRATION_LOCK_FILENAME);
            if remove_regular_file_if_present(&lock_path, opts.dry_run, &mut warnings)? {
                purged_paths.push(lock_path);
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
            && inspect_leaf_kind(&config_path).is_ok_and(|k| k != PathLeafKind::Missing)
        {
            preserved_paths.push(config_path.clone());
        }
        if config_dir_safety == ExistingDirSafety::SafeDir
            && inspect_leaf_kind(&auth_dir).is_ok_and(|k| k != PathLeafKind::Missing)
        {
            preserved_paths.push(auth_dir.clone());
        }
        for usage_dir in &usage_dirs {
            if (runtime_data_dir_safety == ExistingDirSafety::Unsafe
                && usage_dir_under_dir(usage_dir, &runtime_data_dir))
                || (data_dir_safety == ExistingDirSafety::Unsafe
                    && usage_dir_under_dir(usage_dir, &data_dir))
            {
                continue;
            }
            if inspect_leaf_kind(usage_dir).is_ok_and(|k| k != PathLeafKind::Missing) {
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

    format_uninstall_report(&report, opts.json, out)
}
