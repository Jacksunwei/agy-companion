#![forbid(unsafe_code)]

use std::process::ExitCode;

use clap::Parser;
use companion_cli::cli::{Cli, Commands};
use companion_cli::daemon::ServeLogTarget;
use companion_cli::doctor::execute_doctor;
use companion_cli::hook_env::{HookEnvOptions, run_hook_env};
use companion_cli::install::{InstallOptions, UninstallOptions, run_install, run_uninstall};
use companion_cli::integration::run_integration;
use companion_cli::launch::run_launch;
use companion_cli::models::run_models;
use companion_cli::{ServeOptions, run_serve_configured, run_ui_mcp_with_options};
use tracing_subscriber::filter::{FilterExt, filter_fn};
use tracing_subscriber::layer::SubscriberExt;
use tracing_subscriber::util::SubscriberInitExt;
use tracing_subscriber::{EnvFilter, Layer};

fn rmcp_ceiling_allows(target: &str, level: &tracing::Level) -> bool {
    let is_rmcp = target == "rmcp" || target.starts_with("rmcp::");
    !(is_rmcp && *level > tracing::Level::INFO)
}

fn init_tracing<W>(default_level: &str, writer: W)
where
    W: for<'a> tracing_subscriber::fmt::MakeWriter<'a> + Send + Sync + 'static,
{
    let env_filter =
        EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new(default_level));
    // Hard ceiling: rmcp DEBUG/TRACE logs raw JSON-RPC request payloads (including save_settings
    // plaintext api_key) before companion-ui-mcp deserializes into redacted argument structs.
    let rmcp_ceiling = filter_fn(|meta| rmcp_ceiling_allows(meta.target(), meta.level()));
    let fmt_layer = tracing_subscriber::fmt::layer()
        .with_writer(writer)
        .with_ansi(false)
        .with_filter(env_filter.and(rmcp_ceiling));
    let _ = tracing_subscriber::registry().with(fmt_layer).try_init();
}

#[tokio::main]
async fn main() -> ExitCode {
    let cli = Cli::parse();

    match cli.command {
        Commands::Serve {
            port,
            config,
            adopt_running,
            auto_enable_launcher,
            idle_shutdown_secs,
        } => {
            let log_target = ServeLogTarget::default();
            init_tracing("info", log_target.clone());
            if let Err(err) = run_serve_configured(ServeOptions {
                port_override: port,
                config_override: config,
                adopt_running,
                auto_enable_launcher,
                idle_shutdown_secs,
                log_target: Some(log_target),
            })
            .await
            {
                eprintln!("error: {err}");
                return ExitCode::FAILURE;
            }
            ExitCode::SUCCESS
        }
        Commands::UiMcp {
            data_dir,
            config,
            auto_enable_launcher,
        } => {
            init_tracing("warn", std::io::stderr);
            if let Err(err) = run_ui_mcp_with_options(data_dir, config, auto_enable_launcher).await
            {
                eprintln!("error: {err}");
                return ExitCode::FAILURE;
            }
            ExitCode::SUCCESS
        }
        Commands::Doctor { config, port, json } => {
            init_tracing("warn", std::io::stderr);
            let mut stdout = std::io::stdout().lock();
            match execute_doctor(config, port, json, &mut stdout).await {
                Ok(true) => ExitCode::SUCCESS,
                Ok(false) => ExitCode::FAILURE,
                Err(err) => {
                    eprintln!("error: {err}");
                    ExitCode::FAILURE
                }
            }
        }
        Commands::Models { json, port, config } => {
            init_tracing("warn", std::io::stderr);
            let mut stdout = std::io::stdout().lock();
            if let Err(err) = run_models(json, port, config, &mut stdout) {
                eprintln!("error: {err}");
                return ExitCode::FAILURE;
            }
            ExitCode::SUCCESS
        }
        Commands::Launch {
            port,
            config,
            app,
            print_env,
        } => {
            init_tracing("warn", std::io::stderr);
            let mut stdout = std::io::stdout().lock();
            if let Err(err) = run_launch(port, config, app, print_env, &mut stdout).await {
                eprintln!("error: {err}");
                return ExitCode::FAILURE;
            }
            ExitCode::SUCCESS
        }
        Commands::HookEnv {
            ensure_running,
            print_port,
            port,
            config,
            plugins_dir,
        } => {
            init_tracing("warn", std::io::stderr);
            let mut stdout = std::io::stdout().lock();
            let mut stderr = std::io::stderr().lock();
            run_hook_env(
                HookEnvOptions {
                    ensure_running,
                    print_port,
                    port_override: port,
                    config_override: config,
                    plugins_dir_override: plugins_dir,
                },
                &mut stdout,
                &mut stderr,
            )
            .await;
            ExitCode::SUCCESS
        }
        Commands::Integration { action } => {
            init_tracing("warn", std::io::stderr);
            let mut stdout = std::io::stdout().lock();
            if let Err(err) = run_integration(action, &mut stdout) {
                eprintln!("error: {err}");
                return ExitCode::FAILURE;
            }
            ExitCode::SUCCESS
        }
        Commands::Install {
            config,
            plugins_dir,
            no_launcher_hook,
            no_start_proxy,
            force,
            json,
        } => {
            init_tracing("warn", std::io::stderr);
            let mut stdout = std::io::stdout().lock();
            if let Err(err) = run_install(
                InstallOptions {
                    config,
                    plugins_dir,
                    no_launcher_hook,
                    no_start_proxy,
                    force,
                    json,
                },
                &mut stdout,
            )
            .await
            {
                eprintln!("error: {err}");
                return ExitCode::FAILURE;
            }
            ExitCode::SUCCESS
        }
        Commands::Uninstall {
            config,
            plugins_dir,
            purge,
            dry_run,
            json,
        } => {
            init_tracing("warn", std::io::stderr);
            let mut stdout = std::io::stdout().lock();
            if let Err(err) = run_uninstall(
                UninstallOptions {
                    config,
                    plugins_dir,
                    purge,
                    dry_run,
                    json,
                },
                &mut stdout,
            )
            .await
            {
                eprintln!("error: {err}");
                return ExitCode::FAILURE;
            }
            ExitCode::SUCCESS
        }
    }
}

#[cfg(test)]
mod tests {
    use std::io;
    use std::sync::{Arc, Mutex};

    use tracing_subscriber::filter::{FilterExt, filter_fn};
    use tracing_subscriber::layer::SubscriberExt;
    use tracing_subscriber::{EnvFilter, Layer};

    use super::rmcp_ceiling_allows;

    #[derive(Clone, Default)]
    struct SharedBuf(Arc<Mutex<Vec<u8>>>);

    struct BufWriter(Arc<Mutex<Vec<u8>>>);

    impl io::Write for BufWriter {
        fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
            self.0.lock().expect("lock buf").extend_from_slice(buf);
            Ok(buf.len())
        }

        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for SharedBuf {
        type Writer = BufWriter;

        fn make_writer(&'a self) -> Self::Writer {
            BufWriter(Arc::clone(&self.0))
        }
    }

    #[test]
    fn rmcp_ceiling_blocks_rmcp_debug_and_trace_but_preserves_companion_and_info() {
        assert!(!rmcp_ceiling_allows("rmcp", &tracing::Level::TRACE));
        assert!(!rmcp_ceiling_allows("rmcp", &tracing::Level::DEBUG));
        assert!(rmcp_ceiling_allows("rmcp", &tracing::Level::INFO));
        assert!(rmcp_ceiling_allows("rmcp", &tracing::Level::WARN));
        assert!(rmcp_ceiling_allows("rmcp", &tracing::Level::ERROR));

        assert!(!rmcp_ceiling_allows(
            "rmcp::service",
            &tracing::Level::TRACE
        ));
        assert!(!rmcp_ceiling_allows(
            "rmcp::service",
            &tracing::Level::DEBUG
        ));
        assert!(rmcp_ceiling_allows("rmcp::service", &tracing::Level::INFO));

        // Companion crates retain full debug/trace observability.
        assert!(rmcp_ceiling_allows("companion_cli", &tracing::Level::TRACE));
        assert!(rmcp_ceiling_allows(
            "companion_proxy::router",
            &tracing::Level::DEBUG
        ));
        // Unrelated target sharing only a prefix without `::` is not blocked.
        assert!(rmcp_ceiling_allows("rmcp_other", &tracing::Level::DEBUG));
    }

    #[test]
    fn composed_env_filter_and_rmcp_ceiling_drops_rmcp_debug_trace_even_under_rmcp_trace() {
        let buf = SharedBuf::default();
        let env_filter = EnvFilter::new("companion_cli=trace,rmcp=trace");
        let rmcp_ceiling = filter_fn(|meta| rmcp_ceiling_allows(meta.target(), meta.level()));
        let subscriber = tracing_subscriber::registry().with(
            tracing_subscriber::fmt::layer()
                .with_writer(buf.clone())
                .with_ansi(false)
                .with_filter(env_filter.and(rmcp_ceiling)),
        );

        tracing::subscriber::with_default(subscriber, || {
            tracing::trace!(target: "rmcp", "LEAK_RMCP_ROOT_TRACE");
            tracing::debug!(target: "rmcp::service", "LEAK_RMCP_SERVICE_DEBUG");
            tracing::trace!(target: "rmcp::service", "LEAK_RMCP_SERVICE_TRACE");
            tracing::info!(target: "rmcp::service", "ALLOW_RMCP_SERVICE_INFO");
            tracing::debug!(target: "companion_cli", "ALLOW_COMPANION_DEBUG");
            tracing::trace!(target: "companion_cli", "ALLOW_COMPANION_TRACE");
        });

        let captured = String::from_utf8(buf.0.lock().expect("lock buf").clone()).expect("utf8");
        assert!(!captured.contains("LEAK_RMCP_ROOT_TRACE"), "{captured}");
        assert!(!captured.contains("LEAK_RMCP_SERVICE_DEBUG"), "{captured}");
        assert!(!captured.contains("LEAK_RMCP_SERVICE_TRACE"), "{captured}");
        assert!(captured.contains("ALLOW_RMCP_SERVICE_INFO"), "{captured}");
        assert!(captured.contains("ALLOW_COMPANION_DEBUG"), "{captured}");
        assert!(captured.contains("ALLOW_COMPANION_TRACE"), "{captured}");
    }
}
