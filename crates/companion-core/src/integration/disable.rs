use std::fs;
use std::io::ErrorKind;
use std::path::{Path, PathBuf};

use crate::config::CoreError;
use crate::fs_safe::{self, DirChainStatus, FsError};

use super::fs_inspect::{
    DirTargetState, RcInspection, SymlinkTargetKind, default_fish_dir_chain, inspect_rc_file,
    inspect_symlinked_dir_chain_target, rc_write_options,
};
use super::lifecycle::{
    LifecycleEnv, ResolvedContext, build_unsupported_status, resolve_context,
    validate_env_script_dir_chain,
};
use super::markers::{MarkerState, is_deletable_when_empty, strip_marker_block};
use super::strip::collect_known_rc_paths;
use super::{IntegrationOptions, IntegrationStatus, ShellKind};

fn env_script_still_referenced_elsewhere(
    ctx: &ResolvedContext,
    excluded_rc: &Path,
) -> Result<bool, CoreError> {
    if !ctx.use_portable_env_expr {
        return Ok(false);
    }
    let candidates = collect_known_rc_paths(
        &ctx.home,
        ctx.zdotdir.as_deref(),
        ctx.xdg_config_home.as_deref(),
        false,
    )?;
    for path in candidates {
        if path == excluded_rc {
            continue;
        }
        match inspect_rc_file(&path) {
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
            }) => return Ok(true),
            Ok(_) => {}
            Err(_) => {
                // Do not let an unrelated corrupt/unreadable shell RC prevent disabling this shell.
            }
        }
    }
    Ok(false)
}

fn remove_env_script_if_unused(ctx: &ResolvedContext) -> Result<(), CoreError> {
    let Some(env_path) = ctx.env_script_path.as_deref() else {
        return Ok(());
    };
    validate_env_script_dir_chain(ctx)?;
    if env_script_still_referenced_elsewhere(ctx, &ctx.rc_path)? {
        return Ok(());
    }
    if fs_safe::inspect_regular_file_if_exists(env_path, None)?.is_some() {
        fs::remove_file(fs_safe::normalize_path(env_path)).map_err(|source| FsError::Io {
            path: env_path.to_path_buf(),
            source,
        })?;
    }
    Ok(())
}

pub(super) fn disable_with_env(
    opts: &IntegrationOptions,
    env: &LifecycleEnv<'_>,
) -> Result<IntegrationStatus, CoreError> {
    let hybrid_active = env.hybrid_active;
    if !env.platform_supported {
        return Ok(build_unsupported_status(hybrid_active));
    }

    let ctx = resolve_context(opts, env)?;
    validate_env_script_dir_chain(&ctx)?;

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
                match inspect_symlinked_dir_chain_target(&fish_root, fish_rel_dirs)? {
                    DirTargetState::Missing | DirTargetState::NotDirectory { .. } => {
                        return Ok(IntegrationStatus::disabled(
                            ctx.into_paths(),
                            target_display,
                            hybrid_active,
                        ));
                    }
                    DirTargetState::Ready => {
                        if !opts.follow_rc_symlink {
                            let has_valid_markers = matches!(
                                inspect_rc_file(&ctx.rc_path)?,
                                RcInspection::Regular {
                                    markers: MarkerState::Valid { .. },
                                    ..
                                } | RcInspection::Symlink {
                                    target: SymlinkTargetKind::Regular {
                                        markers: MarkerState::Valid { .. },
                                        ..
                                    },
                                    ..
                                }
                            );
                            if !has_valid_markers {
                                return Ok(IntegrationStatus::disabled(
                                    ctx.into_paths(),
                                    target_display,
                                    hybrid_active,
                                ));
                            }
                            return Ok(IntegrationStatus::manual_required(
                                ctx.into_paths(),
                                true,
                                target_display,
                                None,
                                hybrid_active,
                                "Fish configuration directory is a symlink and was left untouched. Remove the hook manually or re-run with --follow-rc-symlink.",
                            ));
                        }
                        let sym_check = fs_safe::normalize_path(&symlink_dir);
                        followed_dir_symlink_target =
                            fs::canonicalize(&sym_check).ok().or(target_display);
                    }
                }
            }
        }
    }

    match inspect_rc_file(&ctx.rc_path)? {
        RcInspection::Missing => {
            remove_env_script_if_unused(&ctx)?;
            Ok(IntegrationStatus::disabled(
                ctx.into_paths(),
                followed_dir_symlink_target,
                hybrid_active,
            ))
        }
        RcInspection::Symlink {
            link_target,
            target,
        } => match target {
            SymlinkTargetKind::Dangling | SymlinkTargetKind::NonRegular => {
                remove_env_script_if_unused(&ctx)?;
                Ok(IntegrationStatus::disabled(
                    ctx.into_paths(),
                    link_target,
                    hybrid_active,
                ))
            }
            SymlinkTargetKind::Regular {
                canonical,
                file: loaded,
                markers,
            } => {
                let target_display = link_target.or_else(|| Some(canonical.clone()));
                if !matches!(markers, MarkerState::Valid { .. }) {
                    remove_env_script_if_unused(&ctx)?;
                    return Ok(IntegrationStatus::disabled(
                        ctx.into_paths(),
                        target_display,
                        hybrid_active,
                    ));
                }

                if !opts.follow_rc_symlink {
                    remove_env_script_if_unused(&ctx)?;
                    let still_enabled = ctx.shell == ShellKind::Fish;
                    let msg = if ctx.shell == ShellKind::Fish {
                        "Fish drop-in is a symlink and was left untouched. Remove the hook manually or re-run with --follow-rc-symlink."
                    } else {
                        "Removed env.sh (neutralizing the hook), but left the symlinked RC file untouched. Remove the managed lines manually or re-run with --follow-rc-symlink."
                    };
                    return Ok(IntegrationStatus::manual_required(
                        ctx.into_paths(),
                        still_enabled,
                        target_display,
                        None,
                        hybrid_active,
                        msg,
                    ));
                }

                fs_safe::validate_path(&canonical)?;
                if loaded.readonly {
                    remove_env_script_if_unused(&ctx)?;
                    let still_enabled = ctx.shell == ShellKind::Fish;
                    return Ok(IntegrationStatus::manual_required(
                        ctx.into_paths(),
                        still_enabled,
                        Some(canonical),
                        None,
                        hybrid_active,
                        "Canonical RC symlink target is read-only; left untouched.",
                    ));
                }

                let (stripped, _) = strip_marker_block(&canonical, &loaded.content)?;
                fs_safe::write_atomic(
                    &canonical,
                    stripped.as_bytes(),
                    rc_write_options(loaded.unix_mode),
                )?;
                remove_env_script_if_unused(&ctx)?;
                Ok(IntegrationStatus::disabled(
                    ctx.into_paths(),
                    Some(canonical),
                    hybrid_active,
                ))
            }
        },
        RcInspection::Regular { file, markers } => {
            if !matches!(markers, MarkerState::Valid { .. }) {
                remove_env_script_if_unused(&ctx)?;
                return Ok(IntegrationStatus::disabled(
                    ctx.into_paths(),
                    followed_dir_symlink_target,
                    hybrid_active,
                ));
            }

            if file.readonly {
                remove_env_script_if_unused(&ctx)?;
                let still_enabled = ctx.shell == ShellKind::Fish;
                return Ok(IntegrationStatus::manual_required(
                    ctx.into_paths(),
                    still_enabled,
                    followed_dir_symlink_target,
                    None,
                    hybrid_active,
                    "RC file is read-only; left untouched. Remove the managed lines manually.",
                ));
            }

            let (stripped, _) = strip_marker_block(&ctx.rc_path, &file.content)?;
            if stripped.trim().is_empty() && is_deletable_when_empty(&ctx.rc_path) {
                match fs::remove_file(&ctx.rc_path) {
                    Ok(()) => {}
                    Err(err) if err.kind() == ErrorKind::NotFound => {}
                    Err(err) if err.kind() == ErrorKind::PermissionDenied => {
                        remove_env_script_if_unused(&ctx)?;
                        let still_enabled = ctx.shell == ShellKind::Fish;
                        return Ok(IntegrationStatus::manual_required(
                            ctx.into_paths(),
                            still_enabled,
                            followed_dir_symlink_target,
                            None,
                            hybrid_active,
                            "RC file could not be removed due to permissions; remove it manually.",
                        ));
                    }
                    Err(source) => {
                        return Err(CoreError::Io {
                            path: ctx.rc_path,
                            source,
                        });
                    }
                }
            } else {
                match fs_safe::write_atomic(
                    &ctx.rc_path,
                    stripped.as_bytes(),
                    rc_write_options(file.unix_mode),
                ) {
                    Ok(()) => {}
                    Err(FsError::Io { source, .. })
                        if source.kind() == ErrorKind::PermissionDenied =>
                    {
                        remove_env_script_if_unused(&ctx)?;
                        let still_enabled = ctx.shell == ShellKind::Fish;
                        return Ok(IntegrationStatus::manual_required(
                            ctx.into_paths(),
                            still_enabled,
                            followed_dir_symlink_target,
                            None,
                            hybrid_active,
                            "RC file directory is not writable; remove the managed lines manually.",
                        ));
                    }
                    Err(err) => return Err(err.into()),
                }
            }

            remove_env_script_if_unused(&ctx)?;
            Ok(IntegrationStatus::disabled(
                ctx.into_paths(),
                followed_dir_symlink_target,
                hybrid_active,
            ))
        }
    }
}
