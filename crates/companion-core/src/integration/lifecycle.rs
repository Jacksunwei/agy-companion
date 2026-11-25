use std::fs;
use std::path::{Path, PathBuf};

use crate::config::{CoreError, XDG_CONFIG_HOME_ENV_VAR, default_data_dir, read_optional_env};
use crate::fs_safe::{self, DirChainStatus, FsError};

use super::fs_inspect::{
    DirTargetState, RcInspection, SymlinkTargetKind, default_fish_dir_chain, inspect_rc_file,
    inspect_symlinked_dir_chain_target, is_regular_file_existing, validate_env_script_parent_dir,
};
use super::markers::MarkerState;
use super::templates::{
    render_fish_dropin, render_posix_rc_snippet_internal, validate_shell_safe_path,
    wrap_with_markers, write_env_script,
};
use super::{
    ENV_SCRIPT_FILENAME, IntegrationOptions, IntegrationPaths, IntegrationStatus, SHELL_ENV_VAR,
    ShellKind, ZDOTDIR_ENV_VAR, detect_shell, resolve_rc_path, resolve_stable_binary_path,
};

pub(super) struct ProcessEnvInputs {
    pub(super) home: Option<PathBuf>,
    pub(super) zdotdir: Option<PathBuf>,
    pub(super) xdg_config_home: Option<PathBuf>,
    pub(super) shell_env: Option<String>,
}

#[derive(Debug, Clone, Copy)]
pub(super) struct LifecycleEnv<'a> {
    pub(super) home: Option<&'a Path>,
    pub(super) zdotdir: Option<&'a Path>,
    pub(super) xdg_config_home: Option<&'a Path>,
    pub(super) shell_env: Option<&'a str>,
    pub(super) default_data_override: Option<&'a Path>,
    pub(super) hybrid_active: bool,
    pub(super) platform_supported: bool,
}

impl<'a> LifecycleEnv<'a> {
    pub(super) fn from_process(
        env: &'a ProcessEnvInputs,
        hybrid_active: bool,
        platform_supported: bool,
    ) -> Self {
        Self {
            home: env.home.as_deref(),
            zdotdir: env.zdotdir.as_deref(),
            xdg_config_home: env.xdg_config_home.as_deref(),
            shell_env: env.shell_env.as_deref(),
            default_data_override: None,
            hybrid_active,
            platform_supported,
        }
    }
}

pub(super) struct ResolvedContext {
    pub(super) shell: ShellKind,
    pub(super) rc_path: PathBuf,
    pub(super) env_script_path: Option<PathBuf>,
    pub(super) binary_path: PathBuf,
    pub(super) use_portable_env_expr: bool,
    pub(super) home: PathBuf,
    pub(super) zdotdir: Option<PathBuf>,
    pub(super) xdg_config_home: Option<PathBuf>,
}

impl ResolvedContext {
    pub(super) fn into_paths(self) -> IntegrationPaths {
        IntegrationPaths {
            shell: self.shell,
            rc_path: self.rc_path,
            env_script_path: self.env_script_path,
            binary_path: self.binary_path,
        }
    }
}

pub(super) fn read_process_env_inputs() -> Result<ProcessEnvInputs, CoreError> {
    let home = fs_safe::default_home_dir();
    let zdotdir = read_optional_env(ZDOTDIR_ENV_VAR)?.map(PathBuf::from);
    let xdg_config_home = read_optional_env(XDG_CONFIG_HOME_ENV_VAR)?.map(PathBuf::from);
    let shell_env = std::env::var(SHELL_ENV_VAR).ok();
    Ok(ProcessEnvInputs {
        home,
        zdotdir,
        xdg_config_home,
        shell_env,
    })
}

pub(super) fn build_unsupported_status(hybrid_active: bool) -> IntegrationStatus {
    IntegrationStatus::unsupported(
        hybrid_active,
        "Login-shell RC integration is only used on macOS and Linux. Use `agy-companion launch` or the Windows Start Menu shortcut instead.",
    )
}

pub(super) fn resolve_context(
    opts: &IntegrationOptions,
    env: &LifecycleEnv<'_>,
) -> Result<ResolvedContext, CoreError> {
    let shell = opts.shell.unwrap_or_else(|| detect_shell(env.shell_env));
    let home_dir = env
        .home
        .map(Path::to_path_buf)
        .ok_or(CoreError::HomeDirUnavailable)?;
    fs_safe::validate_path(&home_dir)?;

    let (data_dir, use_portable_env_expr) = match opts.data_dir.as_deref() {
        Some(custom) => {
            fs_safe::validate_path(custom)?;
            (custom.to_path_buf(), false)
        }
        None => match env.default_data_override {
            Some(dir) => {
                fs_safe::validate_path(dir)?;
                (dir.to_path_buf(), true)
            }
            None => (default_data_dir()?, true),
        },
    };

    let rc_path = match opts.rc_file.as_deref() {
        Some(custom_rc) => {
            validate_shell_safe_path(custom_rc)?;
            custom_rc.to_path_buf()
        }
        None => resolve_rc_path(shell, &home_dir, env.zdotdir, env.xdg_config_home)?,
    };

    let binary_path = resolve_stable_binary_path(opts.binary_path.as_deref(), Some(&data_dir))?;
    let env_script_path = match shell {
        ShellKind::Zsh | ShellKind::Bash => Some(data_dir.join(ENV_SCRIPT_FILENAME)),
        ShellKind::Fish => None,
    };

    Ok(ResolvedContext {
        shell,
        rc_path,
        env_script_path,
        binary_path,
        use_portable_env_expr,
        home: home_dir,
        zdotdir: env.zdotdir.map(Path::to_path_buf),
        xdg_config_home: env.xdg_config_home.map(Path::to_path_buf),
    })
}

pub(super) fn validate_env_script_dir_chain(ctx: &ResolvedContext) -> Result<(), CoreError> {
    let Some(env_path) = ctx.env_script_path.as_deref() else {
        return Ok(());
    };
    validate_env_script_parent_dir(env_path, Some(&ctx.home))?;
    let _ = fs_safe::inspect_regular_file_if_exists(env_path, None)?;
    Ok(())
}

pub(super) fn render_block_for_context(ctx: &ResolvedContext) -> Result<String, CoreError> {
    match ctx.shell {
        ShellKind::Zsh | ShellKind::Bash => {
            let env_path = ctx
                .env_script_path
                .as_deref()
                .expect("posix shell always has env_script_path");
            render_posix_rc_snippet_internal(env_path, ctx.use_portable_env_expr)
        }
        ShellKind::Fish => {
            let body = render_fish_dropin(&ctx.binary_path)?;
            Ok(wrap_with_markers(&body))
        }
    }
}

pub(super) fn ensure_env_script(ctx: &ResolvedContext) -> Result<(), CoreError> {
    validate_env_script_dir_chain(ctx)?;
    if let Some(env_path) = ctx.env_script_path.as_deref() {
        write_env_script(env_path, &ctx.binary_path)?;
    }
    Ok(())
}

pub(super) fn status_with_env(
    opts: &IntegrationOptions,
    env: &LifecycleEnv<'_>,
) -> Result<IntegrationStatus, CoreError> {
    let hybrid_active = env.hybrid_active;
    if !env.platform_supported {
        return Ok(build_unsupported_status(hybrid_active));
    }

    let ctx = resolve_context(opts, env)?;
    validate_env_script_dir_chain(&ctx)?;
    let snippet = render_block_for_context(&ctx)?;
    let env_exists = ctx
        .env_script_path
        .as_deref()
        .is_some_and(is_regular_file_existing);

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
                let has_markers =
                    match inspect_symlinked_dir_chain_target(&fish_root, fish_rel_dirs)? {
                        DirTargetState::Ready => matches!(
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
                        ),
                        DirTargetState::Missing | DirTargetState::NotDirectory { .. } => false,
                    };
                let symlink_target = link_target.or_else(|| fs::canonicalize(&symlink_dir).ok());
                return if has_markers {
                    Ok(IntegrationStatus::enabled(
                        ctx.into_paths(),
                        symlink_target,
                        hybrid_active,
                    ))
                } else {
                    Ok(IntegrationStatus::manual_required(
                        ctx.into_paths(),
                        false,
                        symlink_target,
                        Some(snippet),
                        hybrid_active,
                        "Fish configuration directory is a symlink (likely managed by Nix, Stow, or Chezmoi); add the snippet manually.",
                    ))
                };
            }
        }
    }

    match inspect_rc_file(&ctx.rc_path)? {
        RcInspection::Missing => Ok(IntegrationStatus::disabled(
            ctx.into_paths(),
            None,
            hybrid_active,
        )),
        RcInspection::Regular { file, markers } => {
            let has_markers = matches!(markers, MarkerState::Valid { .. });
            let enabled = match ctx.shell {
                ShellKind::Zsh | ShellKind::Bash => has_markers && env_exists,
                ShellKind::Fish => has_markers,
            };
            let manual_required = file.readonly && !has_markers && env_exists;
            if manual_required {
                Ok(IntegrationStatus::manual_required(
                    ctx.into_paths(),
                    false,
                    None,
                    Some(snippet),
                    hybrid_active,
                    "RC file is read-only; add the snippet manually to finish enabling auto-start.",
                ))
            } else if enabled {
                Ok(IntegrationStatus::enabled(
                    ctx.into_paths(),
                    None,
                    hybrid_active,
                ))
            } else {
                Ok(IntegrationStatus::disabled(
                    ctx.into_paths(),
                    None,
                    hybrid_active,
                ))
            }
        }
        RcInspection::Symlink {
            link_target,
            target,
        } => {
            let (canonical_target, has_markers) = match target {
                SymlinkTargetKind::Regular {
                    canonical, markers, ..
                } => (
                    Some(canonical),
                    matches!(markers, MarkerState::Valid { .. }),
                ),
                SymlinkTargetKind::Dangling | SymlinkTargetKind::NonRegular => (None, false),
            };
            let enabled = match ctx.shell {
                ShellKind::Zsh | ShellKind::Bash => has_markers && env_exists,
                ShellKind::Fish => has_markers,
            };
            let manual_required = match ctx.shell {
                ShellKind::Zsh | ShellKind::Bash => !has_markers && env_exists,
                ShellKind::Fish => !has_markers,
            };
            let symlink_target = link_target.or(canonical_target);
            if manual_required {
                Ok(IntegrationStatus::manual_required(
                    ctx.into_paths(),
                    false,
                    symlink_target,
                    Some(snippet),
                    hybrid_active,
                    "RC file is a symlink (likely managed by Nix, Stow, or Chezmoi); add the snippet manually.",
                ))
            } else if enabled {
                Ok(IntegrationStatus::enabled(
                    ctx.into_paths(),
                    symlink_target,
                    hybrid_active,
                ))
            } else {
                Ok(IntegrationStatus::disabled(
                    ctx.into_paths(),
                    symlink_target,
                    hybrid_active,
                ))
            }
        }
    }
}
