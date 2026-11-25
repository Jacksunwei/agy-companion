use std::fs;
use std::io::ErrorKind;
use std::path::{Path, PathBuf};

use crate::config::CoreError;
use crate::fs_safe::{
    self, AtomicWriteOptions, DirChainStatus, FsError, ParentDirPolicy, SymlinkPolicy,
    TargetFilePolicy,
};

#[cfg(not(unix))]
use super::DEFAULT_RC_FILE_MODE;
use super::markers::{MarkerState, parse_markers, split_lines_preserving_eol};
use super::{DEFAULT_RC_DIR_MODE, MAX_RC_FILE_BYTES, resolve_optional_absolute_dir};

#[derive(Debug)]
pub(super) struct LoadedRegularFile {
    pub(super) content: String,
    pub(super) unix_mode: u32,
    pub(super) readonly: bool,
}

#[derive(Debug)]
pub(super) enum SymlinkTargetKind {
    Regular {
        canonical: PathBuf,
        file: LoadedRegularFile,
        markers: MarkerState,
    },
    Dangling,
    NonRegular,
}

#[derive(Debug)]
pub(super) enum RcInspection {
    Missing,
    Regular {
        file: LoadedRegularFile,
        markers: MarkerState,
    },
    Symlink {
        link_target: Option<PathBuf>,
        target: SymlinkTargetKind,
    },
}

#[derive(Debug)]
pub(super) enum DirTargetState {
    Ready,
    Missing,
    NotDirectory { path: PathBuf },
}

pub(super) fn rc_write_options(file_mode: u32) -> AtomicWriteOptions {
    AtomicWriteOptions {
        file_mode,
        dir_mode: DEFAULT_RC_DIR_MODE,
        parent_policy: ParentDirPolicy::AllowSymlink,
        target_policy: TargetFilePolicy::RejectSymlinkAndNonRegular,
    }
}

pub(super) fn read_regular_file_checked(
    path: &Path,
) -> Result<Option<LoadedRegularFile>, CoreError> {
    let Some((content, meta)) =
        fs_safe::read_regular_file_utf8_bounded(path, MAX_RC_FILE_BYTES, SymlinkPolicy::Reject)?
    else {
        return Ok(None);
    };

    #[cfg(unix)]
    let unix_mode = {
        use std::os::unix::fs::PermissionsExt;
        meta.permissions().mode() & 0o777
    };
    #[cfg(not(unix))]
    let unix_mode = DEFAULT_RC_FILE_MODE;

    let readonly = meta.permissions().readonly();
    Ok(Some(LoadedRegularFile {
        content,
        unix_mode,
        readonly,
    }))
}

pub(super) fn default_fish_dir_chain(
    home: &Path,
    xdg_config_home: Option<&Path>,
) -> Result<(PathBuf, &'static [&'static str]), CoreError> {
    if let Some(xdg) = resolve_optional_absolute_dir(xdg_config_home)? {
        let default_dot_config = home.join(".config");
        if fs_safe::normalize_path(&xdg) != fs_safe::normalize_path(&default_dot_config) {
            return Ok((xdg, &["fish", "conf.d"]));
        }
    }
    Ok((home.to_path_buf(), &[".config", "fish", "conf.d"]))
}

pub(super) fn validate_data_dir_chain(
    data_dir: &Path,
    home: Option<&Path>,
) -> Result<(), CoreError> {
    fs_safe::validate_path(data_dir)?;
    let norm_data_dir = fs_safe::normalize_path(data_dir);
    if let Some(home_dir) = home {
        let default_data_dir =
            fs_safe::normalize_path(&home_dir.join(".local").join("share").join("agy-companion"));
        if norm_data_dir == default_data_dir {
            match fs_safe::inspect_dir_chain(home_dir, &[".local", "share", "agy-companion"])? {
                DirChainStatus::SafeDir | DirChainStatus::Missing => return Ok(()),
                DirChainStatus::Symlink { path, .. } => {
                    return Err(FsError::SymlinkRejected { path }.into());
                }
                DirChainStatus::NotDirectory { path } => {
                    return Err(FsError::NotDirectory { path }.into());
                }
            }
        }
    }
    let _ = fs_safe::inspect_existing_dir(data_dir)?;
    Ok(())
}

pub(super) fn validate_env_script_parent_dir(
    env_path: &Path,
    home: Option<&Path>,
) -> Result<(), CoreError> {
    let data_dir = env_path
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .ok_or_else(|| CoreError::InvalidPath {
            reason: format!(
                "env script path '{}' must include a parent directory",
                env_path.display()
            ),
        })?;
    validate_data_dir_chain(data_dir, home)
}

pub(super) fn inspect_symlinked_dir_chain_target(
    root: &Path,
    rel_dirs: &[&str],
) -> Result<DirTargetState, CoreError> {
    let mut current = root.to_path_buf();
    for segment in rel_dirs {
        current.push(segment);
        let check_path = fs_safe::normalize_path(&current);
        match fs::metadata(&check_path) {
            Ok(meta) if meta.is_dir() => {}
            Ok(_) => return Ok(DirTargetState::NotDirectory { path: current }),
            Err(err) if err.kind() == ErrorKind::NotFound => return Ok(DirTargetState::Missing),
            Err(source) => {
                return Err(CoreError::Io {
                    path: current,
                    source,
                });
            }
        }
    }
    Ok(DirTargetState::Ready)
}

pub(super) fn inspect_rc_file(path: &Path) -> Result<RcInspection, CoreError> {
    fs_safe::validate_path(path)?;
    let check_path = fs_safe::normalize_path(path);
    let lstat_meta = match fs::symlink_metadata(&check_path) {
        Ok(meta) => meta,
        Err(err) if err.kind() == ErrorKind::NotFound => return Ok(RcInspection::Missing),
        Err(source) => {
            return Err(CoreError::Io {
                path: path.to_path_buf(),
                source,
            });
        }
    };

    if lstat_meta.file_type().is_symlink() {
        let link_target = fs::read_link(&check_path).ok();
        match fs::metadata(&check_path) {
            Ok(target_meta) => {
                if !target_meta.file_type().is_file() {
                    return Ok(RcInspection::Symlink {
                        link_target,
                        target: SymlinkTargetKind::NonRegular,
                    });
                }
                let canonical = fs::canonicalize(&check_path).map_err(|source| CoreError::Io {
                    path: path.to_path_buf(),
                    source,
                })?;
                let loaded = read_regular_file_checked(&canonical)?;
                let target = match loaded {
                    Some(file) => {
                        let (lines, _, _) = split_lines_preserving_eol(&file.content);
                        let markers = parse_markers(&canonical, &lines)?;
                        SymlinkTargetKind::Regular {
                            canonical,
                            file,
                            markers,
                        }
                    }
                    None => SymlinkTargetKind::Dangling,
                };
                Ok(RcInspection::Symlink {
                    link_target,
                    target,
                })
            }
            Err(err) if err.kind() == ErrorKind::NotFound => Ok(RcInspection::Symlink {
                link_target,
                target: SymlinkTargetKind::Dangling,
            }),
            Err(source) => Err(CoreError::Io {
                path: path.to_path_buf(),
                source,
            }),
        }
    } else {
        let Some(loaded) = read_regular_file_checked(path)? else {
            return Ok(RcInspection::Missing);
        };
        let (lines, _, _) = split_lines_preserving_eol(&loaded.content);
        let markers = parse_markers(path, &lines)?;
        Ok(RcInspection::Regular {
            file: loaded,
            markers,
        })
    }
}

pub(super) fn is_regular_file_existing(path: &Path) -> bool {
    let check_path = fs_safe::normalize_path(path);
    fs::symlink_metadata(&check_path).is_ok_and(|meta| meta.file_type().is_file())
}
