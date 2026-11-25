use std::path::PathBuf;

use clap::{Parser, Subcommand};

#[derive(Parser, Debug, PartialEq, Eq)]
#[command(
    name = "agy-companion",
    bin_name = "agy-companion",
    version = env!("CARGO_PKG_VERSION"),
    about = "Antigravity Companion: local Gemini pass-through proxy and MCP App settings UI"
)]
pub struct Cli {
    #[command(subcommand)]
    pub command: Commands,
}

#[derive(Subcommand, Debug, PartialEq, Eq)]
pub enum Commands {
    /// Run the loopback Gemini pass-through HTTP proxy.
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
    /// Run the embedded MCP App settings UI server over stdio.
    UiMcp {
        /// Plugin data directory (supports --data-dir <path> and --data-dir=<path>).
        #[arg(long = "data-dir")]
        data_dir: Option<PathBuf>,
        /// Path to config.json (overrides AGY_COMPANION_CONFIG).
        #[arg(long)]
        config: Option<PathBuf>,
        /// Auto-enable the login-shell launcher hook once on first open_settings call (intended for plugin MCP config).
        #[arg(long = "auto-enable-launcher")]
        auto_enable_launcher: bool,
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

    use super::{Cli, Commands};

    #[test]
    fn parses_serve_ui_mcp_doctor_and_models_subcommands() {
        let serve = Cli::try_parse_from([
            "agy-companion",
            "serve",
            "--port",
            "4242",
            "--config",
            "/tmp/c.json",
        ])
        .expect("serve args should parse");
        assert_eq!(
            serve.command,
            Commands::Serve {
                port: Some(4242),
                config: Some(PathBuf::from("/tmp/c.json")),
                adopt_running: false,
                auto_enable_launcher: false,
                idle_shutdown_secs: None,
            }
        );

        let serve_defaults =
            Cli::try_parse_from(["agy-companion", "serve"]).expect("serve defaults should parse");
        assert_eq!(
            serve_defaults.command,
            Commands::Serve {
                port: None,
                config: None,
                adopt_running: false,
                auto_enable_launcher: false,
                idle_shutdown_secs: None,
            }
        );

        let serve_adopt = Cli::try_parse_from([
            "agy-companion",
            "serve",
            "--adopt-running",
            "--auto-enable-launcher",
        ])
        .expect("serve --adopt-running --auto-enable-launcher should parse");
        assert_eq!(
            serve_adopt.command,
            Commands::Serve {
                port: None,
                config: None,
                adopt_running: true,
                auto_enable_launcher: true,
                idle_shutdown_secs: None,
            }
        );

        let serve_idle =
            Cli::try_parse_from(["agy-companion", "serve", "--idle-shutdown-secs", "1800"])
                .expect("serve --idle-shutdown-secs should parse");
        assert_eq!(
            serve_idle.command,
            Commands::Serve {
                port: None,
                config: None,
                adopt_running: false,
                auto_enable_launcher: false,
                idle_shutdown_secs: Some(1800),
            }
        );

        let serve_idle_zero =
            Cli::try_parse_from(["agy-companion", "serve", "--idle-shutdown-secs=0"])
                .expect("serve --idle-shutdown-secs=0 should parse");
        assert_eq!(
            serve_idle_zero.command,
            Commands::Serve {
                port: None,
                config: None,
                adopt_running: false,
                auto_enable_launcher: false,
                idle_shutdown_secs: Some(0),
            }
        );

        let ui_mcp_eq = Cli::try_parse_from([
            "agy-companion",
            "ui-mcp",
            "--data-dir=/tmp/data",
            "--config",
            "/tmp/c.json",
        ])
        .expect("ui-mcp equals form should parse");
        assert_eq!(
            ui_mcp_eq.command,
            Commands::UiMcp {
                data_dir: Some(PathBuf::from("/tmp/data")),
                config: Some(PathBuf::from("/tmp/c.json")),
                auto_enable_launcher: false,
            }
        );

        let ui_mcp_space = Cli::try_parse_from(["agy-companion", "ui-mcp", "--data-dir", "/tmp/d"])
            .expect("ui-mcp space form should parse");
        assert_eq!(
            ui_mcp_space.command,
            Commands::UiMcp {
                data_dir: Some(PathBuf::from("/tmp/d")),
                config: None,
                auto_enable_launcher: false,
            }
        );

        let ui_mcp_auto = Cli::try_parse_from([
            "agy-companion",
            "ui-mcp",
            "--data-dir=/tmp/data",
            "--auto-enable-launcher",
        ])
        .expect("ui-mcp --auto-enable-launcher should parse");
        assert_eq!(
            ui_mcp_auto.command,
            Commands::UiMcp {
                data_dir: Some(PathBuf::from("/tmp/data")),
                config: None,
                auto_enable_launcher: true,
            }
        );

        let doctor = Cli::try_parse_from([
            "agy-companion",
            "doctor",
            "--config",
            "/tmp/c.json",
            "--port",
            "4242",
            "--json",
        ])
        .expect("doctor args should parse");
        assert_eq!(
            doctor.command,
            Commands::Doctor {
                config: Some(PathBuf::from("/tmp/c.json")),
                port: Some(4242),
                json: true,
            }
        );

        let models = Cli::try_parse_from([
            "agy-companion",
            "models",
            "--json",
            "--port",
            "4242",
            "--config",
            "/tmp/c.json",
        ])
        .expect("models args should parse");
        assert_eq!(
            models.command,
            Commands::Models {
                json: true,
                port: Some(4242),
                config: Some(PathBuf::from("/tmp/c.json")),
            }
        );

        let rendered_version = Cli::command().render_version();
        assert_eq!(
            rendered_version.trim(),
            format!("agy-companion {}", env!("CARGO_PKG_VERSION"))
        );
    }

    #[test]
    fn cli_rejects_unknown_flags_and_supports_equals_form_args() {
        let parsed = Cli::try_parse_from([
            "agy-companion",
            "serve",
            "--port=5151",
            "--config=/tmp/c.json",
        ])
        .expect("equals form args should parse");
        assert_eq!(
            parsed.command,
            Commands::Serve {
                port: Some(5151),
                config: Some(PathBuf::from("/tmp/c.json")),
                adopt_running: false,
                auto_enable_launcher: false,
                idle_shutdown_secs: None,
            }
        );

        let launch = Cli::try_parse_from([
            "agy-companion",
            "launch",
            "--port",
            "4141",
            "--print-env",
            "--app",
            "/opt/antigravity",
        ])
        .expect("launch args should parse");
        assert_eq!(
            launch.command,
            Commands::Launch {
                port: Some(4141),
                config: None,
                app: Some(PathBuf::from("/opt/antigravity")),
                print_env: true,
            }
        );

        let data_dir_eq = Cli::try_parse_from([
            "agy-companion",
            "ui-mcp",
            "--data-dir=/tmp/plugin-data",
            "--config=/tmp/c.json",
        ])
        .expect("ui-mcp equals form should parse");
        assert_eq!(
            data_dir_eq.command,
            Commands::UiMcp {
                data_dir: Some(PathBuf::from("/tmp/plugin-data")),
                config: Some(PathBuf::from("/tmp/c.json")),
                auto_enable_launcher: false,
            }
        );

        assert!(
            Cli::try_parse_from(["agy-companion", "serve", "--host", "0.0.0.0"]).is_err(),
            "non-loopback --host flag must not exist"
        );
        assert!(
            Cli::try_parse_from(["agy-companion"]).is_err(),
            "missing subcommand must fail"
        );
        assert!(
            Cli::try_parse_from(["agy-companion", "serve", "--port", "0"]).is_err(),
            "--port 0 must fail during CLI parsing"
        );
        assert!(
            Cli::try_parse_from(["agy-companion", "doctor", "--port", "0"]).is_err(),
            "doctor --port 0 must fail during CLI parsing"
        );
        assert!(
            Cli::try_parse_from(["agy-companion", "models", "--port", "0"]).is_err(),
            "models --port 0 must fail during CLI parsing"
        );
        assert!(
            Cli::try_parse_from(["agy-companion", "serve", "--idle-shutdown-secs", "abc"]).is_err(),
            "non-numeric --idle-shutdown-secs must fail"
        );
        assert!(
            Cli::try_parse_from(["agy-companion", "serve", "--idle-shutdown-secs", "-1"]).is_err(),
            "negative --idle-shutdown-secs must fail"
        );
    }

    #[test]
    fn parses_hook_env_and_integration_subcommands_and_rejects_invalid_args() {
        use super::{CliShell, IntegrationCommands};

        let hook_env = Cli::try_parse_from([
            "agy-companion",
            "hook-env",
            "--ensure-running",
            "--print-port",
            "--port",
            "4242",
            "--config",
            "/tmp/c.json",
            "--plugins-dir",
            "/tmp/p",
        ])
        .expect("hook-env args should parse");
        assert_eq!(
            hook_env.command,
            Commands::HookEnv {
                ensure_running: true,
                print_port: true,
                port: Some(4242),
                config: Some(PathBuf::from("/tmp/c.json")),
                plugins_dir: Some(PathBuf::from("/tmp/p")),
            }
        );

        let enable = Cli::try_parse_from([
            "agy-companion",
            "integration",
            "enable",
            "--shell",
            "zsh",
            "--rc-file",
            "/tmp/.zlogin",
            "--binary",
            "/tmp/bin/agy-companion",
            "--follow-rc-symlink",
            "--config",
            "/tmp/c.json",
            "--json",
        ])
        .expect("integration enable should parse");
        assert_eq!(
            enable.command,
            Commands::Integration {
                action: IntegrationCommands::Enable {
                    shell: Some(CliShell::Zsh),
                    rc_file: Some(PathBuf::from("/tmp/.zlogin")),
                    binary: Some(PathBuf::from("/tmp/bin/agy-companion")),
                    follow_rc_symlink: true,
                    config: Some(PathBuf::from("/tmp/c.json")),
                    json: true,
                },
            }
        );

        let disable = Cli::try_parse_from([
            "agy-companion",
            "integration",
            "disable",
            "--shell",
            "bash",
            "--follow-rc-symlink",
            "--config=/tmp/d.json",
            "--json",
        ])
        .expect("integration disable should parse");
        assert_eq!(
            disable.command,
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

        let status = Cli::try_parse_from([
            "agy-companion",
            "integration",
            "status",
            "--shell",
            "fish",
            "--rc-file",
            "/tmp/fish.fish",
            "--config",
            "/tmp/s.json",
            "--json",
        ])
        .expect("integration status should parse");
        assert_eq!(
            status.command,
            Commands::Integration {
                action: IntegrationCommands::Status {
                    shell: Some(CliShell::Fish),
                    rc_file: Some(PathBuf::from("/tmp/fish.fish")),
                    config: Some(PathBuf::from("/tmp/s.json")),
                    json: true,
                },
            }
        );

        assert!(
            Cli::try_parse_from(["agy-companion", "hook-env", "--port", "0"]).is_err(),
            "hook-env --port 0 must fail"
        );
        assert!(
            Cli::try_parse_from(["agy-companion", "integration", "enable", "--shell", "tcsh"])
                .is_err(),
            "unsupported --shell must fail"
        );
        assert!(
            Cli::try_parse_from(["agy-companion", "integration"]).is_err(),
            "integration without action must fail"
        );

        let install = Cli::try_parse_from([
            "agy-companion",
            "install",
            "--config",
            "/tmp/custom-config.json",
            "--plugins-dir",
            "/tmp/plugins",
            "--no-launcher-hook",
            "--no-start-proxy",
            "--force",
            "--json",
        ])
        .expect("install should parse");
        assert_eq!(
            install.command,
            Commands::Install {
                config: Some(PathBuf::from("/tmp/custom-config.json")),
                plugins_dir: Some(PathBuf::from("/tmp/plugins")),
                no_launcher_hook: true,
                no_start_proxy: true,
                force: true,
                json: true,
            }
        );

        let install_eq =
            Cli::try_parse_from(["agy-companion", "install", "--config=/tmp/eq-config.json"])
                .expect("install --config= should parse");
        assert_eq!(
            install_eq.command,
            Commands::Install {
                config: Some(PathBuf::from("/tmp/eq-config.json")),
                plugins_dir: None,
                no_launcher_hook: false,
                no_start_proxy: false,
                force: false,
                json: false,
            }
        );

        let install_defaults = Cli::try_parse_from(["agy-companion", "install"])
            .expect("install defaults should parse");
        assert_eq!(
            install_defaults.command,
            Commands::Install {
                config: None,
                plugins_dir: None,
                no_launcher_hook: false,
                no_start_proxy: false,
                force: false,
                json: false,
            }
        );

        let uninstall = Cli::try_parse_from([
            "agy-companion",
            "uninstall",
            "--config",
            "/tmp/custom-config.json",
            "--plugins-dir",
            "/tmp/plugins",
            "--purge",
            "--dry-run",
            "--json",
        ])
        .expect("uninstall should parse");
        assert_eq!(
            uninstall.command,
            Commands::Uninstall {
                config: Some(PathBuf::from("/tmp/custom-config.json")),
                plugins_dir: Some(PathBuf::from("/tmp/plugins")),
                purge: true,
                dry_run: true,
                json: true,
            }
        );

        let uninstall_eq =
            Cli::try_parse_from(["agy-companion", "uninstall", "--config=/tmp/eq-config.json"])
                .expect("uninstall --config= should parse");
        assert_eq!(
            uninstall_eq.command,
            Commands::Uninstall {
                config: Some(PathBuf::from("/tmp/eq-config.json")),
                plugins_dir: None,
                purge: false,
                dry_run: false,
                json: false,
            }
        );

        let uninstall_defaults = Cli::try_parse_from(["agy-companion", "uninstall"])
            .expect("uninstall defaults should parse");
        assert_eq!(
            uninstall_defaults.command,
            Commands::Uninstall {
                config: None,
                plugins_dir: None,
                purge: false,
                dry_run: false,
                json: false,
            }
        );
    }
}
