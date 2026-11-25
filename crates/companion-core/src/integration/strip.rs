use std::fs;
use std::io::ErrorKind;
use std::path::{Path, PathBuf};

use crate::config::CoreError;
use crate::fs_safe::{self, DirChainStatus, FsError};

use super::fs_inspect::{
    DirTargetState, RcInspection, SymlinkTargetKind, default_fish_dir_chain, inspect_rc_file,
    inspect_symlinked_dir_chain_target, rc_write_options,
};
use super::markers::{MarkerState, is_deletable_when_empty, strip_marker_block};
use super::{FISH_DROPIN_FILENAME, StripReport, resolve_optional_absolute_dir};

pub(super) fn collect_known_rc_paths(
    home: &Path,
    zdotdir: Option<&Path>,
    xdg_config_home: Option<&Path>,
    include_fish: bool,
) -> Result<Vec<PathBuf>, CoreError> {
    fs_safe::validate_path(home)?;
    let mut paths = Vec::new();
    let mut push_unique = |candidate: PathBuf| {
        if !paths.contains(&candidate) {
            paths.push(candidate);
        }
    };

    if let Some(zdir) = resolve_optional_absolute_dir(zdotdir)? {
        for name in [".zlogin", ".zprofile", ".zshrc"] {
            push_unique(zdir.join(name));
        }
    }

    for name in [
        ".zlogin",
        ".zprofile",
        ".zshrc",
        ".bash_profile",
        ".bash_login",
        ".profile",
    ] {
        push_unique(home.join(name));
    }

    if include_fish {
        let fish_base =
            resolve_optional_absolute_dir(xdg_config_home)?.unwrap_or_else(|| home.join(".config"));
        push_unique(
            fish_base
                .join("fish")
                .join("conf.d")
                .join(FISH_DROPIN_FILENAME),
        );
    }

    Ok(paths)
}

pub fn strip_all_known_rc_files(
    home: &Path,
    zdotdir: Option<&Path>,
    xdg_config_home: Option<&Path>,
    dry_run: bool,
) -> Result<StripReport, CoreError> {
    let candidates = collect_known_rc_paths(home, zdotdir, xdg_config_home, true)?;
    let (fish_root, fish_rel_dirs) = default_fish_dir_chain(home, xdg_config_home)?;
    let mut fish_dropin_path = fish_root.clone();
    for seg in fish_rel_dirs {
        fish_dropin_path.push(seg);
    }
    fish_dropin_path.push(FISH_DROPIN_FILENAME);
    let norm_fish_dropin = fs_safe::normalize_path(&fish_dropin_path);
    let mut report = StripReport::default();

    for path in candidates {
        if fs_safe::normalize_path(&path) == norm_fish_dropin {
            match fs_safe::inspect_dir_chain(&fish_root, fish_rel_dirs) {
                Ok(DirChainStatus::SafeDir) => {}
                Ok(DirChainStatus::Missing) | Ok(DirChainStatus::NotDirectory { .. }) | Err(_) => {
                    continue;
                }
                Ok(DirChainStatus::Symlink { .. }) => {
                    match inspect_symlinked_dir_chain_target(&fish_root, fish_rel_dirs) {
                        Ok(DirTargetState::Ready) => match inspect_rc_file(&path) {
                            Ok(RcInspection::Regular {
                                markers: MarkerState::Valid { .. },
                                ..
                            })
                            | Ok(RcInspection::Symlink {
                                target:
                                    SymlinkTargetKind::Regular {
                                        markers: MarkerState::Valid { .. },
                                        ..
                                    },
                                ..
                            }) => {
                                report.skipped_manual_files.push(path);
                            }
                            Ok(_) => {}
                            Err(_) => {
                                let check_path = fs_safe::normalize_path(&path);
                                if fs::symlink_metadata(&check_path).is_ok() {
                                    report.skipped_manual_files.push(path);
                                }
                            }
                        },
                        Ok(DirTargetState::Missing)
                        | Ok(DirTargetState::NotDirectory { .. })
                        | Err(_) => {}
                    }
                    continue;
                }
            }
        }

        let inspection = match inspect_rc_file(&path) {
            Ok(inspection) => inspection,
            Err(_) => {
                // Do not let an unrelated corrupt, oversized, non-UTF-8, FIFO, or
                // unreadable RC file abort cleaning the remaining known RC files.
                let check_path = fs_safe::normalize_path(&path);
                if fs::symlink_metadata(&check_path).is_ok() {
                    report.skipped_manual_files.push(path);
                }
                continue;
            }
        };
        match inspection {
            RcInspection::Missing => {}
            RcInspection::Symlink { target, .. } => {
                if matches!(
                    target,
                    SymlinkTargetKind::Regular {
                        markers: MarkerState::Valid { .. },
                        ..
                    }
                ) {
                    report.skipped_manual_files.push(path);
                }
            }
            RcInspection::Regular { file, markers } => {
                if !matches!(markers, MarkerState::Valid { .. }) {
                    continue;
                }
                if file.readonly {
                    report.skipped_manual_files.push(path);
                    continue;
                }
                let (stripped, changed) = strip_marker_block(&path, &file.content)?;
                if !changed {
                    continue;
                }
                let should_delete = stripped.trim().is_empty() && is_deletable_when_empty(&path);
                if dry_run {
                    if should_delete {
                        report.deleted_files.push(path);
                    } else {
                        report.modified_files.push(path);
                    }
                    continue;
                }

                if should_delete {
                    match fs::remove_file(&path) {
                        Ok(()) => report.deleted_files.push(path),
                        Err(err) if err.kind() == ErrorKind::PermissionDenied => {
                            report.skipped_manual_files.push(path);
                        }
                        Err(source) => return Err(CoreError::Io { path, source }),
                    }
                } else {
                    match fs_safe::write_atomic(
                        &path,
                        stripped.as_bytes(),
                        rc_write_options(file.unix_mode),
                    ) {
                        Ok(()) => report.modified_files.push(path),
                        Err(FsError::Io { source, .. })
                            if source.kind() == ErrorKind::PermissionDenied =>
                        {
                            report.skipped_manual_files.push(path);
                        }
                        Err(err) => return Err(err.into()),
                    }
                }
            }
        }
    }

    Ok(report)
}
