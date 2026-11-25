use std::io::Write;
use std::path::{Path, PathBuf};

use companion_core::integration::{
    self, IntegrationOptions, IntegrationState, IntegrationStatus, ShellKind,
};

use crate::CliError;
use crate::cli::IntegrationCommands;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum IntegrationActionKind {
    Enable,
    Disable,
    Status,
}

pub fn resolve_integration_binary(explicit: Option<&Path>) -> Result<PathBuf, CliError> {
    Ok(integration::resolve_stable_binary_path(explicit, None)?)
}

pub fn run_integration<W: Write>(action: IntegrationCommands, out: &mut W) -> Result<(), CliError> {
    match action {
        IntegrationCommands::Enable {
            shell,
            rc_file,
            binary,
            follow_rc_symlink,
            json,
        } => {
            let resolved_bin = resolve_integration_binary(binary.as_deref())?;
            let opts = IntegrationOptions {
                shell: shell.map(ShellKind::from),
                rc_file,
                binary_path: Some(resolved_bin),
                data_dir: None,
                follow_rc_symlink,
            };
            let status = integration::enable(&opts)?;
            write_integration_output(IntegrationActionKind::Enable, &status, json, out)
        }
        IntegrationCommands::Disable {
            shell,
            rc_file,
            follow_rc_symlink,
            json,
        } => {
            let opts = IntegrationOptions {
                shell: shell.map(ShellKind::from),
                rc_file,
                binary_path: None,
                data_dir: None,
                follow_rc_symlink,
            };
            let status = integration::disable(&opts)?;
            write_integration_output(IntegrationActionKind::Disable, &status, json, out)
        }
        IntegrationCommands::Status {
            shell,
            rc_file,
            json,
        } => {
            let opts = IntegrationOptions {
                shell: shell.map(ShellKind::from),
                rc_file,
                binary_path: None,
                data_dir: None,
                follow_rc_symlink: false,
            };
            let status = integration::status(&opts)?;
            write_integration_output(IntegrationActionKind::Status, &status, json, out)
        }
    }
}

fn write_integration_output<W: Write>(
    action: IntegrationActionKind,
    status: &IntegrationStatus,
    json: bool,
    out: &mut W,
) -> Result<(), CliError> {
    if json {
        let rendered = serde_json::to_string_pretty(status)
            .map_err(|err| CliError::Serialization(err.to_string()))?;
        writeln!(out, "{rendered}")?;
        return Ok(());
    }

    writeln!(out, "{}", format_human_integration_output(action, status))?;
    Ok(())
}

fn format_human_integration_output(
    action: IntegrationActionKind,
    status: &IntegrationStatus,
) -> String {
    let mut lines = Vec::new();

    if status.state() == IntegrationState::Unsupported {
        lines.push(
            "Dock / Launcher login-shell integration is not used on this platform.".to_string(),
        );
        if let Some(ref msg) = status.message {
            lines.push(msg.clone());
        } else {
            lines.push(
                "Use `agy-companion launch` or the Windows Start Menu shortcut instead."
                    .to_string(),
            );
        }
        return lines.join("\n");
    }

    let quit_hint = if cfg!(target_os = "macos") {
        "Next step: Quit Antigravity completely (⌘Q on macOS; closing the window is not enough) and reopen it."
    } else {
        "Next step: Quit Antigravity completely (closing the window is not enough) and reopen it."
    };

    if status.state() == IntegrationState::ManualRequired {
        match action {
            IntegrationActionKind::Enable => {
                lines.push(
                    "Dock / Launcher auto-start requires a manual 1-line RC step.".to_string(),
                );
            }
            IntegrationActionKind::Disable => {
                lines.push(
                    "Dock / Launcher auto-start partially disabled (RC file left untouched)."
                        .to_string(),
                );
            }
            IntegrationActionKind::Status => {
                lines.push("Status: Manual step required".to_string());
            }
        }
        if let Some(ref env_path) = status.env_script_path {
            if action == IntegrationActionKind::Disable {
                lines.push(format!(
                    "Env script removed/checked: {}",
                    env_path.display()
                ));
            } else {
                lines.push(format!("Env script (0600): {}", env_path.display()));
            }
        }
        if let Some(ref rc_path) = status.rc_path {
            if let Some(ref target) = status.symlink_target {
                lines.push(format!(
                    "RC file (untouched symlink): {} -> {}",
                    rc_path.display(),
                    target.display()
                ));
            } else {
                lines.push(format!("RC file (untouched): {}", rc_path.display()));
            }
        }
        if let Some(ref msg) = status.message {
            lines.push(msg.clone());
        }
        if let Some(ref snippet) = status.manual_snippet {
            lines.push(String::new());
            lines.push("Add this snippet to your managed shell dotfiles:".to_string());
            lines.push(snippet.clone());
        }
        if status.symlink_target.is_some() && action != IntegrationActionKind::Status {
            lines.push(String::new());
            lines.push(
                "Tip: For mutable symlinked dotfiles (e.g., Stow/Chezmoi), re-run with `--follow-rc-symlink`."
                    .to_string(),
            );
        }
        lines.push(String::new());
        lines.push(quit_hint.to_string());
        return lines.join("\n");
    }

    match action {
        IntegrationActionKind::Enable => {
            lines.push("Dock / Launcher integration enabled.".to_string());
            append_common_fields(&mut lines, status);
            lines.push(String::new());
            lines.push(quit_hint.to_string());
        }
        IntegrationActionKind::Disable => {
            lines.push("Dock / Launcher integration disabled.".to_string());
            append_common_fields(&mut lines, status);
            lines.push(String::new());
            lines.push(quit_hint.to_string());
        }
        IntegrationActionKind::Status => {
            let label = match status.state() {
                IntegrationState::Active => "Active",
                IntegrationState::Enabled => {
                    if cfg!(target_os = "macos") {
                        "Enabled (quit with ⌘Q & reopen Antigravity)"
                    } else {
                        "Enabled (quit completely & reopen Antigravity)"
                    }
                }
                IntegrationState::ManualRequired => "Manual step required",
                IntegrationState::Disabled => "Disabled",
                IntegrationState::Unsupported => "Unsupported",
            };
            lines.push(format!("Status: {label}"));
            append_common_fields(&mut lines, status);
            if let Some(ref snippet) = status.manual_snippet {
                lines.push(String::new());
                lines.push(snippet.clone());
            }
        }
    }

    lines.join("\n")
}

fn append_common_fields(lines: &mut Vec<String>, status: &IntegrationStatus) {
    if let Some(shell) = status.shell {
        lines.push(format!("Shell: {shell}"));
    }
    if let Some(ref rc_path) = status.rc_path {
        lines.push(format!("RC file: {}", rc_path.display()));
    }
    if let Some(ref env_path) = status.env_script_path {
        lines.push(format!("Env script: {}", env_path.display()));
    }
    if let Some(ref bin_path) = status.binary_path {
        lines.push(format!("Binary: {}", bin_path.display()));
    }
    lines.push(format!(
        "Hybrid Mode active: {}",
        if status.hybrid_active { "yes" } else { "no" }
    ));
}

#[cfg(test)]
mod tests {
    use companion_core::integration::IntegrationPaths;

    use super::*;

    fn sample_paths() -> IntegrationPaths {
        IntegrationPaths {
            shell: ShellKind::Zsh,
            rc_path: PathBuf::from("/tmp/home/.zlogin"),
            env_script_path: Some(PathBuf::from("/tmp/data/env.sh")),
            binary_path: PathBuf::from("/tmp/bin/agy-companion"),
        }
    }

    fn sample_status() -> IntegrationStatus {
        IntegrationStatus::enabled(sample_paths(), None, false)
    }

    #[test]
    fn resolve_integration_binary_rejects_traversal() {
        assert!(resolve_integration_binary(Some(Path::new("/tmp/../escape"))).is_err());
        let ok = resolve_integration_binary(Some(Path::new("/opt/agy-companion"))).unwrap();
        assert_eq!(ok, PathBuf::from("/opt/agy-companion"));
    }

    #[test]
    fn formats_human_and_json_outputs_for_all_states() {
        let enabled = sample_status();
        let enable_text = format_human_integration_output(IntegrationActionKind::Enable, &enabled);
        assert!(enable_text.contains("Dock / Launcher integration enabled."));
        assert!(enable_text.contains("Quit Antigravity completely"));
        assert!(enable_text.contains("/tmp/home/.zlogin"));

        let manual = IntegrationStatus::manual_required(
            sample_paths(),
            false,
            Some(PathBuf::from("/nix/store/abc-zlogin")),
            Some("[ -r env.sh ] && . env.sh".to_string()),
            false,
            "RC file is a symlink",
        );
        let manual_text = format_human_integration_output(IntegrationActionKind::Enable, &manual);
        assert!(manual_text.contains("/nix/store/abc-zlogin"));
        assert!(manual_text.contains("[ -r env.sh ] && . env.sh"));
        assert!(manual_text.contains("--follow-rc-symlink"));

        let mut active = enabled.clone();
        active.mark_hybrid_active();
        let status_active = format_human_integration_output(IntegrationActionKind::Status, &active);
        assert!(status_active.contains("Status: Active"));

        let disabled = IntegrationStatus::disabled(sample_paths(), None, false);
        let disable_text =
            format_human_integration_output(IntegrationActionKind::Disable, &disabled);
        assert!(disable_text.contains("Dock / Launcher integration disabled."));

        let unsupported = IntegrationStatus::unsupported(false, "Windows uses Start Menu shortcut");
        let unsup_text =
            format_human_integration_output(IntegrationActionKind::Status, &unsupported);
        assert!(unsup_text.contains("not used on this platform"));

        let mut json_buf = Vec::new();
        write_integration_output(IntegrationActionKind::Status, &active, true, &mut json_buf)
            .unwrap();
        let parsed: IntegrationStatus = serde_json::from_slice(&json_buf).unwrap();
        assert_eq!(parsed, active);
        assert_eq!(parsed.state, IntegrationState::Active);
    }
}
