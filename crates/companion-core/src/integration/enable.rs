use std::fs;
use std::io::ErrorKind;
use std::path::PathBuf;

use crate::config::CoreError;
use crate::fs_safe::{self, DirChainStatus, FsError};

use super::fs_inspect::{
    DirTargetState, RcInspection, SymlinkTargetKind, default_fish_dir_chain, inspect_rc_file,
    inspect_symlinked_dir_chain_target, rc_write_options,
};
use super::lifecycle::{
    LifecycleEnv, build_unsupported_status, ensure_env_script, render_block_for_context,
    resolve_context, validate_env_script_dir_chain,
};
use super::markers::{MarkerState, upsert_marker_block};
use super::{DEFAULT_RC_FILE_MODE, IntegrationOptions, IntegrationStatus, ShellKind};

pub(super) fn enable_with_env(
    opts: &IntegrationOptions,
    env: &LifecycleEnv<'_>,
) -> Result<IntegrationStatus, CoreError> {
    let hybrid_active = env.hybrid_active;
    if !env.platform_supported {
        return Ok(build_unsupported_status(hybrid_active));
    }

    let ctx = resolve_context(opts, env)?;
    validate_env_script_dir_chain(&ctx)?;

    // Inspect RC and render snippet BEFORE writing env.sh so fail-closed errors
    // (corrupted markers, FIFO/oversized/non-UTF-8 RC, unfollowable symlink) never
    // leave an orphan env.sh on disk.
    let snippet = render_block_for_context(&ctx)?;
    let mut followed_dir_symlink_target: Option<PathBuf> = None;
    if ctx.shell == ShellKind::Fish && opts.rc_file.is_none() {
        let (fish_root, fish_rel_dirs) =
            default_fish_dir_chain(&ctx.home, ctx.xdg_config_home.as_deref())?;
        match fs_safe::inspect_dir_chain(&fish_root, fish_rel_dirs)? {
            DirChainStatus::SafeDir | DirChainStatus::Missing => {}
            DirChainStatus::NotDirectory { path } => {
                return Err(FsError::NotDirectory { path }.into());
            }
            DirChainStatus::Symlink {
                path: symlink_dir,
                link_target,
            } => {
                let target_display = link_target.or_else(|| fs::canonicalize(&symlink_dir).ok());
                if !opts.follow_rc_symlink {
                    let (existing_content, has_valid_markers) =
                        match inspect_symlinked_dir_chain_target(&fish_root, fish_rel_dirs)? {
                            DirTargetState::Ready => match inspect_rc_file(&ctx.rc_path)? {
                                RcInspection::Regular {
                                    file,
                                    markers: MarkerState::Valid { .. },
                                } => (Some(file.content), true),
                                RcInspection::Symlink {
                                    target:
                                        SymlinkTargetKind::Regular {
                                            file,
                                            markers: MarkerState::Valid { .. },
                                            ..
                                        },
                                    ..
                                } => (Some(file.content), true),
                                _ => (None, false),
                            },
                            DirTargetState::Missing | DirTargetState::NotDirectory { .. } => {
                                (None, false)
                            }
                        };
                    if has_valid_markers {
                        let fish_needs_update = existing_content
                            .as_deref()
                            .is_some_and(|content| !content.contains(&snippet));
                        return if fish_needs_update {
                            Ok(IntegrationStatus::manual_required(
                                ctx.into_paths(),
                                true,
                                target_display,
                                Some(snippet),
                                hybrid_active,
                                "Fish configuration directory is a symlink; existing hook could not be updated automatically.",
                            ))
                        } else {
                            Ok(IntegrationStatus::enabled(
                                ctx.into_paths(),
                                target_display,
                                hybrid_active,
                            ))
                        };
                    }
                    return Ok(IntegrationStatus::manual_required(
                        ctx.into_paths(),
                        false,
                        target_display,
                        Some(snippet),
                        hybrid_active,
                        "Fish configuration directory is a symlink; left untouched. Add the snippet manually or re-run with --follow-rc-symlink.",
                    ));
                }

                let sym_check = fs_safe::normalize_path(&symlink_dir);
                match fs::metadata(&sym_check) {
                    Ok(meta) if meta.is_dir() => {}
                    Ok(_) => {
                        return Err(CoreError::InvalidPath {
                            reason: format!(
                                "Fish directory symlink '{}' does not point to a directory",
                                symlink_dir.display()
                            ),
                        });
                    }
                    Err(err) if err.kind() == ErrorKind::NotFound => {
                        return Err(CoreError::InvalidPath {
                            reason: format!(
                                "cannot follow dangling Fish directory symlink '{}'",
                                symlink_dir.display()
                            ),
                        });
                    }
                    Err(source) => {
                        return Err(CoreError::Io {
                            path: symlink_dir,
                            source,
                        });
                    }
                }
                if let DirTargetState::NotDirectory { path } =
                    inspect_symlinked_dir_chain_target(&fish_root, fish_rel_dirs)?
                {
                    return Err(FsError::NotDirectory { path }.into());
                }
                followed_dir_symlink_target = fs::canonicalize(&sym_check).ok().or(target_display);
            }
        }
    }
    let inspection = inspect_rc_file(&ctx.rc_path)?;

    match inspection {
        RcInspection::Symlink {
            link_target,
            target,
        } => {
            if !opts.follow_rc_symlink {
                ensure_env_script(&ctx)?;
                let (canonical_target, file, markers) = match target {
                    SymlinkTargetKind::Regular {
                        canonical,
                        file,
                        markers,
                    } => (Some(canonical), Some(file), markers),
                    SymlinkTargetKind::Dangling | SymlinkTargetKind::NonRegular => {
                        (None, None, MarkerState::Absent)
                    }
                };
                let target_display = link_target.or(canonical_target);
                if matches!(markers, MarkerState::Valid { .. }) {
                    let fish_needs_update = ctx.shell == ShellKind::Fish
                        && file
                            .as_ref()
                            .is_some_and(|loaded| !loaded.content.contains(&snippet));
                    return if fish_needs_update {
                        Ok(IntegrationStatus::manual_required(
                            ctx.into_paths(),
                            true,
                            target_display,
                            Some(snippet),
                            hybrid_active,
                            "Fish drop-in symlink already contains a hook but could not be updated automatically.",
                        ))
                    } else {
                        Ok(IntegrationStatus::enabled(
                            ctx.into_paths(),
                            target_display,
                            hybrid_active,
                        ))
                    };
                }
                return Ok(IntegrationStatus::manual_required(
                    ctx.into_paths(),
                    false,
                    target_display,
                    Some(snippet),
                    hybrid_active,
                    "RC file is a symlink; left untouched. Add the snippet manually or re-run with --follow-rc-symlink.",
                ));
            }

            let (canonical, loaded, markers) = match target {
                SymlinkTargetKind::Regular {
                    canonical,
                    file,
                    markers,
                } => (canonical, file, markers),
                SymlinkTargetKind::Dangling => {
                    return Err(CoreError::InvalidPath {
                        reason: format!(
                            "cannot follow dangling RC symlink '{}'",
                            ctx.rc_path.display()
                        ),
                    });
                }
                SymlinkTargetKind::NonRegular => {
                    return Err(CoreError::InvalidPath {
                        reason: format!(
                            "RC symlink '{}' does not point to a regular file",
                            ctx.rc_path.display()
                        ),
                    });
                }
            };
            fs_safe::validate_path(&canonical)?;
            if loaded.readonly {
                ensure_env_script(&ctx)?;
                return Ok(IntegrationStatus::manual_required(
                    ctx.into_paths(),
                    matches!(markers, MarkerState::Valid { .. }),
                    Some(canonical),
                    Some(snippet),
                    hybrid_active,
                    "Canonical RC symlink target is read-only; left untouched. Add the snippet manually.",
                ));
            }

            let updated = upsert_marker_block(&canonical, &loaded.content, &snippet)?;
            ensure_env_script(&ctx)?;
            match fs_safe::write_atomic(
                &canonical,
                updated.as_bytes(),
                rc_write_options(loaded.unix_mode),
            ) {
                Ok(()) => Ok(IntegrationStatus::enabled(
                    ctx.into_paths(),
                    Some(canonical),
                    hybrid_active,
                )),
                Err(FsError::Io { source, .. }) if source.kind() == ErrorKind::PermissionDenied => {
                    Ok(IntegrationStatus::manual_required(
                        ctx.into_paths(),
                        matches!(markers, MarkerState::Valid { .. }),
                        Some(canonical),
                        Some(snippet),
                        hybrid_active,
                        "Canonical RC symlink target is not writable; left untouched. Add the snippet manually.",
                    ))
                }
                Err(err) => Err(err.into()),
            }
        }
        RcInspection::Regular { file, markers } => {
            if file.readonly {
                ensure_env_script(&ctx)?;
                let already_present = matches!(markers, MarkerState::Valid { .. });
                let manual_required = !already_present
                    || (ctx.shell == ShellKind::Fish && !file.content.contains(&snippet));
                return if manual_required {
                    Ok(IntegrationStatus::manual_required(
                        ctx.into_paths(),
                        already_present,
                        followed_dir_symlink_target,
                        Some(snippet),
                        hybrid_active,
                        "RC file is read-only; left untouched. Add the snippet manually.",
                    ))
                } else {
                    Ok(IntegrationStatus::enabled(
                        ctx.into_paths(),
                        followed_dir_symlink_target,
                        hybrid_active,
                    ))
                };
            }

            let updated = upsert_marker_block(&ctx.rc_path, &file.content, &snippet)?;
            ensure_env_script(&ctx)?;
            match fs_safe::write_atomic(
                &ctx.rc_path,
                updated.as_bytes(),
                rc_write_options(file.unix_mode),
            ) {
                Ok(()) => Ok(IntegrationStatus::enabled(
                    ctx.into_paths(),
                    followed_dir_symlink_target,
                    hybrid_active,
                )),
                Err(FsError::Io { source, .. }) if source.kind() == ErrorKind::PermissionDenied => {
                    Ok(IntegrationStatus::manual_required(
                        ctx.into_paths(),
                        matches!(markers, MarkerState::Valid { .. }),
                        followed_dir_symlink_target,
                        Some(snippet),
                        hybrid_active,
                        "RC file directory is not writable; left untouched. Add the snippet manually.",
                    ))
                }
                Err(err) => Err(err.into()),
            }
        }
        RcInspection::Missing => {
            let updated = upsert_marker_block(&ctx.rc_path, "", &snippet)?;
            ensure_env_script(&ctx)?;
            match fs_safe::write_atomic(
                &ctx.rc_path,
                updated.as_bytes(),
                rc_write_options(DEFAULT_RC_FILE_MODE),
            ) {
                Ok(()) => Ok(IntegrationStatus::enabled(
                    ctx.into_paths(),
                    followed_dir_symlink_target,
                    hybrid_active,
                )),
                Err(FsError::Io { source, .. }) if source.kind() == ErrorKind::PermissionDenied => {
                    Ok(IntegrationStatus::manual_required(
                        ctx.into_paths(),
                        false,
                        followed_dir_symlink_target,
                        Some(snippet),
                        hybrid_active,
                        "RC file parent directory is not writable; add the snippet manually.",
                    ))
                }
                Err(err) => Err(err.into()),
            }
        }
    }
}
