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
        /// Loopback listen port (1-65535; overrides config.listen_port).
        #[arg(long, value_parser = parse_nonzero_port)]
        port: Option<u16>,
        /// Path to config.json (overrides AGY_COMPANION_CONFIG).
        #[arg(long)]
        config: Option<PathBuf>,
    },
    /// Run the embedded MCP App settings UI server over stdio.
    UiMcp {
        /// Plugin data directory (supports --data-dir <path> and --data-dir=<path>).
        #[arg(long = "data-dir")]
        data_dir: Option<PathBuf>,
        /// Path to config.json (overrides AGY_COMPANION_CONFIG).
        #[arg(long)]
        config: Option<PathBuf>,
    },
    /// Inspect local config permissions, proxy /health, version skew, and environment.
    Doctor {
        /// Path to config.json (overrides AGY_COMPANION_CONFIG).
        #[arg(long)]
        config: Option<PathBuf>,
        /// Proxy port to probe (1-65535; overrides config.listen_port).
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
        /// Proxy port to embed in model URLs (1-65535; overrides config.listen_port).
        #[arg(long, value_parser = parse_nonzero_port)]
        port: Option<u16>,
        /// Path to config.json (overrides AGY_COMPANION_CONFIG).
        #[arg(long)]
        config: Option<PathBuf>,
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
            }
        );

        let serve_defaults =
            Cli::try_parse_from(["agy-companion", "serve"]).expect("serve defaults should parse");
        assert_eq!(
            serve_defaults.command,
            Commands::Serve {
                port: None,
                config: None,
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
            }
        );

        let ui_mcp_space = Cli::try_parse_from(["agy-companion", "ui-mcp", "--data-dir", "/tmp/d"])
            .expect("ui-mcp space form should parse");
        assert_eq!(
            ui_mcp_space.command,
            Commands::UiMcp {
                data_dir: Some(PathBuf::from("/tmp/d")),
                config: None,
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
    }
}
