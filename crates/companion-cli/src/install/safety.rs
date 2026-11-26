use std::fs;
use std::path::{Path, PathBuf};

use companion_core::CoreError;
use companion_core::fs_safe::{
    DirChainStatus, FsError, default_home_dir, ensure_dir_no_symlink, inspect_dir_chain,
    inspect_existing_dir as core_inspect_existing_dir, inspect_regular_file_if_exists,
};

use super::PLUGIN_DIR_NAME;
use crate::{CliError, normalized_metadata_path};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ExistingDirSafety {
    SafeDir,
    NotFound,
    Unsafe,
}

pub(crate) fn resolve_home_dir() -> Result<PathBuf, CliError> {
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

pub(crate) fn resolve_plugins_root(override_dir: Option<&Path>) -> Result<PathBuf, CliError> {
    match override_dir {
        Some(path) => {
            companion_core::validate_path(path)?;
            Ok(path.to_path_buf())
        }
        None => Ok(companion_core::default_plugins_dir()?),
    }
}

pub(crate) fn ensure_non_symlink_dir(dir: &Path, unix_mode: u32) -> Result<(), CliError> {
    match ensure_dir_no_symlink(dir, unix_mode) {
        Ok(()) => Ok(()),
        Err(FsError::SymlinkRejected { path }) => Err(CliError::Install(format!(
            "refusing to use symlinked directory '{}'",
            path.display()
        ))),
        Err(FsError::NotDirectory { path }) => Err(CliError::Install(format!(
            "expected directory at '{}', found non-directory",
            path.display()
        ))),
        Err(FsError::Io { source, .. }) => Err(CliError::Io(source)),
        Err(err) => Err(CliError::from(err)),
    }
}

/// Inspects an existing directory path via `companion_core::fs_safe::inspect_existing_dir`
/// (with trailing slashes normalized) so callers never traverse a symlinked or non-directory component.
pub(crate) fn inspect_existing_dir(
    dir: &Path,
    label: &str,
    warnings: &mut Vec<String>,
) -> Result<ExistingDirSafety, CliError> {
    match core_inspect_existing_dir(dir) {
        Ok(Some(_)) => Ok(ExistingDirSafety::SafeDir),
        Ok(None) => Ok(ExistingDirSafety::NotFound),
        Err(FsError::SymlinkRejected { .. } | FsError::NotDirectory { .. }) => {
            warnings.push(format!(
                "refusing to traverse symlinked/non-directory {label} '{}'",
                dir.display()
            ));
            Ok(ExistingDirSafety::Unsafe)
        }
        Err(FsError::Io { source, .. }) => Err(CliError::Io(source)),
        Err(err) => Err(CliError::from(err)),
    }
}

/// Inspects every relative intermediate directory under `root` (`root/rel[0]`, `root/rel[0]/rel[1]`,
/// ...) via `companion_core::fs_safe::inspect_dir_chain` so install/uninstall never traverses a
/// symlinked intermediate component beneath a trusted base directory (such as `$HOME` or `%APPDATA%`).
pub(crate) fn inspect_non_symlink_rel_chain(
    root: &Path,
    rel_dirs: &[&str],
    label: &str,
    warnings: &mut Vec<String>,
) -> Result<ExistingDirSafety, CliError> {
    match inspect_dir_chain(root, rel_dirs) {
        Ok(DirChainStatus::SafeDir) => Ok(ExistingDirSafety::SafeDir),
        Ok(DirChainStatus::Missing) => Ok(ExistingDirSafety::NotFound),
        Ok(DirChainStatus::Symlink { path, .. } | DirChainStatus::NotDirectory { path }) => {
            warnings.push(format!(
                "refusing to traverse symlinked/non-directory {label} '{}'",
                path.display()
            ));
            Ok(ExistingDirSafety::Unsafe)
        }
        Err(FsError::Io { source, .. }) => Err(CliError::Io(source)),
        Err(err) => Err(CliError::from(err)),
    }
}

/// Inspects `data_dir` without traversing symlinks. When `data_dir` resolves to the default
/// `$HOME/.local/share/agy-companion` path, validates every intermediate component under `home`.
pub(crate) fn inspect_data_dir(
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
pub(crate) fn inspect_config_dir(
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
pub(crate) fn usage_dir_under_dir(usage_dir: &Path, dir: &Path) -> bool {
    let norm_usage = normalized_metadata_path(usage_dir);
    let norm_dir = normalized_metadata_path(dir);
    !norm_dir.as_os_str().is_empty() && norm_usage.starts_with(&norm_dir)
}

pub(crate) fn deduplicate_paths(paths: Vec<PathBuf>) -> Vec<PathBuf> {
    let mut out: Vec<PathBuf> = Vec::new();
    for path in paths {
        let norm = normalized_metadata_path(&path);
        if !out.iter().any(|p| normalized_metadata_path(p) == norm) {
            out.push(path);
        }
    }
    out
}

pub(crate) fn remove_regular_file_if_present(
    path: &Path,
    dry_run: bool,
    warnings: &mut Vec<String>,
) -> Result<bool, CliError> {
    match inspect_regular_file_if_exists(path, None) {
        Ok(Some(_)) => {
            if !dry_run {
                fs::remove_file(path)?;
            }
            Ok(true)
        }
        Ok(None) => Ok(false),
        Err(FsError::SymlinkRejected { .. }) => {
            warnings.push(format!(
                "refusing to delete symlinked file '{}'",
                path.display()
            ));
            Ok(false)
        }
        Err(FsError::NotRegularFile { .. }) => {
            warnings.push(format!(
                "refusing to delete non-regular path '{}'",
                path.display()
            ));
            Ok(false)
        }
        Err(FsError::Io { source, .. }) => Err(CliError::Io(source)),
        Err(err) => Err(CliError::from(err)),
    }
}

pub(crate) fn remove_dir_if_non_symlink(
    path: &Path,
    dry_run: bool,
    warnings: &mut Vec<String>,
) -> Result<bool, CliError> {
    match core_inspect_existing_dir(path) {
        Ok(Some(_)) => {
            if !dry_run {
                fs::remove_dir_all(path)?;
            }
            Ok(true)
        }
        Ok(None) => Ok(false),
        Err(FsError::SymlinkRejected { .. }) => {
            warnings.push(format!(
                "refusing to recursively delete symlinked directory '{}'",
                path.display()
            ));
            Ok(false)
        }
        Err(FsError::NotDirectory { .. }) => {
            warnings.push(format!(
                "refusing to delete non-directory path '{}'",
                path.display()
            ));
            Ok(false)
        }
        Err(FsError::Io { source, .. }) => Err(CliError::Io(source)),
        Err(err) => Err(CliError::from(err)),
    }
}
