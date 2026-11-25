use std::io::Write;

use companion_core::DEFAULT_LISTEN_PORT;

use super::{InstallReport, UninstallReport};
use crate::CliError;

pub(crate) fn format_install_report<W: Write>(
    report: &InstallReport,
    json: bool,
    out: &mut W,
) -> Result<(), CliError> {
    if json {
        let json_out = serde_json::to_string_pretty(report)
            .map_err(|e| CliError::Serialization(e.to_string()))?;
        writeln!(out, "{json_out}")?;
        return Ok(());
    }

    writeln!(out, "Installed Antigravity Companion v{}", report.version)?;
    writeln!(out, "  Binary:   {}", report.binary_path.display())?;
    if let Some(ref link) = report.local_bin_symlink {
        writeln!(out, "  Symlink:  {}", link.display())?;
    }
    writeln!(out, "  Plugin:   {}", report.plugin_dir.display())?;
    if report.port_auto_selected {
        writeln!(
            out,
            "  Port:     {} (auto-selected; default {} was occupied)",
            report.listen_port, DEFAULT_LISTEN_PORT
        )?;
    } else {
        writeln!(out, "  Port:     {}", report.listen_port)?;
    }
    if let Some(ref integ) = report.launcher_integration {
        if let Some(ref rc_path) = integ.rc_path {
            writeln!(
                out,
                "  Launcher: {:?} ({})",
                integ.state(),
                rc_path.display()
            )?;
        } else {
            writeln!(out, "  Launcher: {:?}", integ.state())?;
        }
        if let Some(ref snippet) = integ.manual_snippet {
            writeln!(out, "  Manual RC snippet:\n{snippet}")?;
        }
    }
    if let Some(ref shortcut) = report.windows_shortcut {
        writeln!(out, "  Shortcut: {}", shortcut.display())?;
    }
    if report.proxy_replaced {
        writeln!(
            out,
            "  Proxy:    upgraded and running on http://127.0.0.1:{}",
            report.listen_port
        )?;
    } else if report.proxy_started {
        writeln!(
            out,
            "  Proxy:    running on http://127.0.0.1:{}",
            report.listen_port
        )?;
    } else if let Some(ref ver) = report.proxy_version {
        writeln!(
            out,
            "  Proxy:    already running v{ver} on http://127.0.0.1:{}",
            report.listen_port
        )?;
    }

    writeln!(out)?;
    if cfg!(target_os = "macos") {
        if report.antigravity_running {
            writeln!(
                out,
                "Next step: Antigravity is currently running — quit completely (Cmd+Q; closing the window is not enough) and reopen Antigravity from Dock or Spotlight."
            )?;
        } else {
            writeln!(
                out,
                "Next step: Open Antigravity from Dock or Spotlight (if it is running in the menu bar, quit completely with Cmd+Q first)."
            )?;
        }
    } else if cfg!(windows) {
        writeln!(
            out,
            "Next step: Quit Antigravity completely and launch 'Antigravity (Companion)' from the Start Menu (or run 'agy-companion launch')."
        )?;
    } else {
        writeln!(
            out,
            "Next step: Quit Antigravity completely and reopen it from your desktop launcher."
        )?;
    }
    writeln!(out, "Uninstall anytime: {}", report.uninstall_command)?;

    for warning in &report.warnings {
        writeln!(out, "warning: {warning}")?;
    }
    Ok(())
}

pub(crate) fn format_uninstall_report<W: Write>(
    report: &UninstallReport,
    json: bool,
    out: &mut W,
) -> Result<(), CliError> {
    if json {
        let json_out = serde_json::to_string_pretty(report)
            .map_err(|e| CliError::Serialization(e.to_string()))?;
        writeln!(out, "{json_out}")?;
        return Ok(());
    }

    if report.dry_run {
        writeln!(out, "Dry run (no files modified, no processes stopped):")?;
    } else {
        writeln!(out, "Uninstalled Antigravity Companion")?;
    }
    writeln!(
        out,
        "  Plugin removed:       {}",
        if report.plugin_dir_removed {
            "yes"
        } else {
            "no"
        }
    )?;
    writeln!(
        out,
        "  env.sh removed:       {}",
        if report.env_script_removed {
            "yes"
        } else {
            "no"
        }
    )?;
    writeln!(
        out,
        "  Proxy stopped:        {}",
        if report.proxy_stopped { "yes" } else { "no" }
    )?;
    writeln!(
        out,
        "  Binary removed:       {}",
        if report.binary_removed { "yes" } else { "no" }
    )?;
    if !report.preserved_paths.is_empty() {
        writeln!(out, "  Preserved (re-run with --purge to delete):")?;
        for path in &report.preserved_paths {
            writeln!(out, "    - {}", path.display())?;
        }
    }
    if !report.purged_paths.is_empty() {
        writeln!(out, "  Purged:")?;
        for path in &report.purged_paths {
            writeln!(out, "    - {}", path.display())?;
        }
    }
    if !report.rc_manual_remaining.is_empty() {
        writeln!(
            out,
            "  Manual RC cleanup required (symlinked/read-only files):"
        )?;
        for path in &report.rc_manual_remaining {
            writeln!(out, "    - {}", path.display())?;
        }
    }
    for warning in &report.warnings {
        writeln!(out, "warning: {warning}")?;
    }
    Ok(())
}
