use std::path::PathBuf;

use clap::{Parser, Subcommand};

#[derive(Parser, Debug, PartialEq, Eq)]
#[command(
    name = "agy-companion",
    bin_name = "agy-companion",
    version = env!("CARGO_PKG_VERSION"),
    about = "Antigravity Companion: local Gemini pass-through proxy and Aux Pane Sidecar Web UI"
)]
pub struct Cli {
    #[command(subcommand)]
    pub command: Commands,
}

#[derive(Subcommand, Debug, PartialEq, Eq)]
pub enum Commands {
    /// Run the loopback Gemini pass-through HTTP proxy and optional Sidecar Web UI.
    Serve {
        /// Loopback listen port (1-65535; overrides config.proxy.port).
        #[arg(long, value_parser = parse_nonzero_port)]
        port: Option<u16>,
        /// Path to config.json (overrides AGY_COMPANION_CONFIG).
        #[arg(long)]
        config: Option<PathBuf>,
        /// If another healthy agy-companion proxy already occupies the port, monitor it
        /// instead of failing with address-in-use (intended for sidecar startup).
        #[arg(long = "adopt-running")]
        adopt_running: bool,
        /// Auto-enable the login-shell launcher hook once on first run (intended for plugin sidecar startup).
        #[arg(long = "auto-enable-launcher")]
        auto_enable_launcher: bool,
        /// Automatically exit cleanly after this many seconds with zero in-flight requests
        /// (0 or omitted disables idle shutdown).
        #[arg(long = "idle-shutdown-secs")]
        idle_shutdown_secs: Option<u64>,
    },
    /// Inspect local config permissions, proxy /health, version skew, and environment.
    Doctor {
        /// Path to config.json (overrides AGY_COMPANION_CONFIG).
        #[arg(long)]
        config: Option<PathBuf>,
        /// Proxy port to probe (1-65535; overrides config.proxy.port).
        #[arg(long, value_parser = parse_nonzero_port)]
        port: Option<u16>,
        /// Emit machine-readable JSON report to stdout.
        #[arg(long)]
        json: bool,
    },
    /// Emit Antigravity CLI customModelsConfig JSON pointing to the local proxy.
    Models {
        /// Emit pure JSON only (suitable for scripting or piping).
        #[arg(long)]
        json: bool,
        /// Proxy port to embed in model URLs (1-65535; overrides config.proxy.port).
        #[arg(long, value_parser = parse_nonzero_port)]
        port: Option<u16>,
        /// Path to config.json (overrides AGY_COMPANION_CONFIG).
        #[arg(long)]
        config: Option<PathBuf>,
    },
    /// Launch Antigravity App in Hybrid Mode (original Google AI Pro models + proxy models).
    Launch {
        /// Proxy port (1-65535; overrides config.proxy.port).
        #[arg(long, value_parser = parse_nonzero_port)]
        port: Option<u16>,
        /// Path to config.json (overrides AGY_COMPANION_CONFIG).
        #[arg(long)]
        config: Option<PathBuf>,
        /// Override path to the Antigravity executable.
        #[arg(long)]
        app: Option<PathBuf>,
        /// Print the shell launch command without starting Antigravity.
        #[arg(long = "print-env")]
        print_env: bool,
    },
    /// Ensure the local proxy daemon is running for login-shell hooks and print its port (fail-open).
    #[command(name = "hook-env")]
    HookEnv {
        /// Spawn the detached Managed On-Demand Daemon if the proxy is not already healthy.
        #[arg(long = "ensure-running")]
        ensure_running: bool,
        /// Print ONLY the validated loopback port (`<port>\n`) when healthy.
        #[arg(long = "print-port")]
        print_port: bool,
        /// Proxy port (1-65535; overrides config.proxy.port).
        #[arg(long, value_parser = parse_nonzero_port)]
        port: Option<u16>,
        /// Path to config.json (overrides AGY_COMPANION_CONFIG).
        #[arg(long)]
        config: Option<PathBuf>,
        /// Override Antigravity plugins directory (defaults to ~/.gemini/config/plugins).
        #[arg(long = "plugins-dir")]
        plugins_dir: Option<PathBuf>,
    },
    /// Manage Antigravity Dock / Launcher login-shell integration.
    Integration {
        #[command(subcommand)]
        action: IntegrationCommands,
    },
    /// Install the embedded Antigravity plugin, stable binary, Dock/Launcher hook, and local proxy.
    Install {
        /// Path to config.json (overrides AGY_COMPANION_CONFIG).
        #[arg(long)]
        config: Option<PathBuf>,
        /// Override Antigravity plugins directory (defaults to ~/.gemini/config/plugins).
        #[arg(long = "plugins-dir")]
        plugins_dir: Option<PathBuf>,
        /// Skip configuring the Dock / Launcher shell hook (and Windows Start Menu shortcut).
        #[arg(long = "no-launcher-hook")]
        no_launcher_hook: bool,
        /// Skip starting or upgrading the background local proxy daemon.
        #[arg(long = "no-start-proxy")]
        no_start_proxy: bool,
        /// Overwrite an existing symlinked plugin directory (e.g. from scripts/dev-install.sh).
        #[arg(long)]
        force: bool,
        /// Emit machine-readable JSON install report to stdout.
        #[arg(long)]
        json: bool,
    },
    /// Uninstall Antigravity Companion plugin, launcher hooks, proxy daemon, and managed binary.
    Uninstall {
        /// Path to config.json (overrides AGY_COMPANION_CONFIG).
        #[arg(long)]
        config: Option<PathBuf>,
        /// Override Antigravity plugins directory (defaults to ~/.gemini/config/plugins).
        #[arg(long = "plugins-dir")]
        plugins_dir: Option<PathBuf>,
        /// Also delete config.json, usage/ database, proxy.log, and sidecar_data/agy-companion.
        #[arg(long)]
        purge: bool,
        /// Report what would be removed without modifying any files or stopping processes.
        #[arg(long = "dry-run")]
        dry_run: bool,
        /// Emit machine-readable JSON uninstall report to stdout.
        #[arg(long)]
        json: bool,
    },
}

#[derive(clap::ValueEnum, Debug, Clone, Copy, PartialEq, Eq)]
pub enum CliShell {
    Zsh,
    Bash,
    Fish,
}

impl From<CliShell> for companion_core::integration::ShellKind {
    fn from(value: CliShell) -> Self {
        match value {
            CliShell::Zsh => Self::Zsh,
            CliShell::Bash => Self::Bash,
            CliShell::Fish => Self::Fish,
        }
    }
}

#[derive(Subcommand, Debug, PartialEq, Eq)]
pub enum IntegrationCommands {
    /// Enable Dock / Launcher auto-start by writing 0600 env.sh and a 1-line RC hook.
    Enable {
        /// Target shell (auto-detected from $SHELL when omitted).
        #[arg(long, value_enum)]
        shell: Option<CliShell>,
        /// Override shell RC file path.
        #[arg(long = "rc-file")]
        rc_file: Option<PathBuf>,
        /// Override agy-companion binary path embedded in env.sh.
        #[arg(long)]
        binary: Option<PathBuf>,
        /// Follow a symlinked RC file and edit its regular-file target.
        #[arg(long = "follow-rc-symlink")]
        follow_rc_symlink: bool,
        /// Path to config.json (overrides AGY_COMPANION_CONFIG).
        #[arg(long)]
        config: Option<PathBuf>,
        /// Emit machine-readable JSON status to stdout.
        #[arg(long)]
        json: bool,
    },
    /// Disable Dock / Launcher auto-start by stripping the managed RC hook and deleting env.sh.
    Disable {
        /// Target shell (auto-detected from $SHELL when omitted).
        #[arg(long, value_enum)]
        shell: Option<CliShell>,
        /// Override shell RC file path.
        #[arg(long = "rc-file")]
        rc_file: Option<PathBuf>,
        /// Follow a symlinked RC file and edit its regular-file target.
        #[arg(long = "follow-rc-symlink")]
        follow_rc_symlink: bool,
        /// Path to config.json (overrides AGY_COMPANION_CONFIG).
        #[arg(long)]
        config: Option<PathBuf>,
        /// Emit machine-readable JSON status to stdout.
        #[arg(long)]
        json: bool,
    },
    /// Inspect Dock / Launcher integration status without modifying files.
    Status {
        /// Target shell (auto-detected from $SHELL when omitted).
        #[arg(long, value_enum)]
        shell: Option<CliShell>,
        /// Override shell RC file path.
        #[arg(long = "rc-file")]
        rc_file: Option<PathBuf>,
        /// Path to config.json (overrides AGY_COMPANION_CONFIG).
        #[arg(long)]
        config: Option<PathBuf>,
        /// Emit machine-readable JSON status to stdout.
        #[arg(long)]
        json: bool,
    },
}

pub fn parse_nonzero_port(raw: &str) -> Result<u16, String> {
    let port: u16 = raw
        .parse()
        .map_err(|_| format!("invalid port '{raw}': expected integer between 1 and 65535"))?;
    if port == 0 {
        return Err("port must be between 1 and 65535 (0 is not allowed)".to_string());
    }
    Ok(port)
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use clap::{CommandFactory, Parser};
    use companion_core::integration::ShellKind;

    use super::{Cli, CliShell, Commands, IntegrationCommands};

    #[test]
    fn cli_rejects_forbidden_host_zero_ports_and_invalid_arguments() {
        Cli::command().debug_assert();

        let rendered_version = Cli::command().render_version();
        assert_eq!(
            rendered_version.trim(),
            format!("agy-companion {}", env!("CARGO_PKG_VERSION"))
        );

        assert!(
            Cli::try_parse_from(["agy-companion", "serve", "--host", "0.0.0.0"]).is_err(),
            "S1: non-loopback --host flag must not exist"
        );
        assert!(
            Cli::try_parse_from(["agy-companion"]).is_err(),
            "missing subcommand must fail"
        );
        assert!(
            Cli::try_parse_from(["agy-companion", "integration"]).is_err(),
            "integration without action must fail"
        );

        // Custom parse_nonzero_port rejects 0, non-numeric, and >65535 across all 5 port-taking subcommands.
        for subcmd in ["serve", "doctor", "models", "launch", "hook-env"] {
            for bad_port in ["0", "abc", "65536"] {
                assert!(
                    Cli::try_parse_from(["agy-companion", subcmd, "--port", bad_port]).is_err(),
                    "{subcmd} --port {bad_port} must fail during CLI parsing"
                );
            }
        }

        for bad_idle in ["abc", "-1"] {
            assert!(
                Cli::try_parse_from(["agy-companion", "serve", "--idle-shutdown-secs", bad_idle])
                    .is_err(),
                "invalid --idle-shutdown-secs '{bad_idle}' must fail"
            );
        }
        assert!(
            Cli::try_parse_from(["agy-companion", "integration", "enable", "--shell", "tcsh"])
                .is_err(),
            "unsupported --shell must fail"
        );
    }

    #[test]
    fn cli_supports_equals_form_and_key_flag_contracts() {
        assert_eq!(
            Cli::try_parse_from([
                "agy-companion",
                "serve",
                "--port=5151",
                "--config=/tmp/c.json",
                "--adopt-running",
                "--auto-enable-launcher",
                "--idle-shutdown-secs=0",
            ])
            .expect("serve equals form")
            .command,
            Commands::Serve {
                port: Some(5151),
                config: Some(PathBuf::from("/tmp/c.json")),
                adopt_running: true,
                auto_enable_launcher: true,
                idle_shutdown_secs: Some(0),
            }
        );

        assert_eq!(
            Cli::try_parse_from([
                "agy-companion",
                "hook-env",
                "--ensure-running",
                "--print-port",
                "--port=4242",
                "--config=/tmp/c.json",
                "--plugins-dir=/tmp/p",
            ])
            .expect("hook-env equals form")
            .command,
            Commands::HookEnv {
                ensure_running: true,
                print_port: true,
                port: Some(4242),
                config: Some(PathBuf::from("/tmp/c.json")),
                plugins_dir: Some(PathBuf::from("/tmp/p")),
            }
        );

        assert_eq!(
            Cli::try_parse_from([
                "agy-companion",
                "integration",
                "disable",
                "--shell",
                "bash",
                "--follow-rc-symlink",
                "--config=/tmp/d.json",
                "--json",
            ])
            .expect("integration disable equals form")
            .command,
            Commands::Integration {
                action: IntegrationCommands::Disable {
                    shell: Some(CliShell::Bash),
                    rc_file: None,
                    follow_rc_symlink: true,
                    config: Some(PathBuf::from("/tmp/d.json")),
                    json: true,
                },
            }
        );
        assert_eq!(ShellKind::from(CliShell::Zsh), ShellKind::Zsh);
        assert_eq!(ShellKind::from(CliShell::Bash), ShellKind::Bash);
        assert_eq!(ShellKind::from(CliShell::Fish), ShellKind::Fish);

        assert_eq!(
            Cli::try_parse_from([
                "agy-companion",
                "install",
                "--config=/tmp/c.json",
                "--plugins-dir=/tmp/p",
                "--no-launcher-hook",
                "--no-start-proxy",
                "--force",
                "--json",
            ])
            .expect("install equals form")
            .command,
            Commands::Install {
                config: Some(PathBuf::from("/tmp/c.json")),
                plugins_dir: Some(PathBuf::from("/tmp/p")),
                no_launcher_hook: true,
                no_start_proxy: true,
                force: true,
                json: true,
            }
        );

        assert_eq!(
            Cli::try_parse_from([
                "agy-companion",
                "uninstall",
                "--config=/tmp/c.json",
                "--plugins-dir=/tmp/p",
                "--purge",
                "--dry-run",
                "--json",
            ])
            .expect("uninstall equals form")
            .command,
            Commands::Uninstall {
                config: Some(PathBuf::from("/tmp/c.json")),
                plugins_dir: Some(PathBuf::from("/tmp/p")),
                purge: true,
                dry_run: true,
                json: true,
            }
        );
    }
}
