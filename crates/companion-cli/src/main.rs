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
use companion_cli::{ServeOptions, run_serve_configured};
use tracing_subscriber::filter::{FilterExt, filter_fn};
use tracing_subscriber::layer::SubscriberExt;
use tracing_subscriber::util::SubscriberInitExt;
use tracing_subscriber::{EnvFilter, Layer};

fn sensitive_transport_ceiling_allows(target: &str, level: &tracing::Level) -> bool {
    const CAPPED: &[&str] = &["hyper", "hyper_util", "h2", "reqwest"];
    let is_capped = CAPPED
        .iter()
        .any(|prefix| target == *prefix || target.starts_with(&format!("{prefix}::")));
    !(is_capped && *level > tracing::Level::INFO)
}

fn init_tracing<W>(default_level: &str, writer: W)
where
    W: for<'a> tracing_subscriber::fmt::MakeWriter<'a> + Send + Sync + 'static,
{
    let env_filter =
        EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new(default_level));
    // Hard ceiling: HTTP transport DEBUG/TRACE logs can dump raw headers (API keys, sidecar
    // tokens, CSRF tokens) or request/response bodies.
    let ceiling = filter_fn(|meta| sensitive_transport_ceiling_allows(meta.target(), meta.level()));
    let fmt_layer = tracing_subscriber::fmt::layer()
        .with_writer(writer)
        .with_ansi(false)
        .with_filter(env_filter.and(ceiling));
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
            watch_pid,
            replace_stale,
        } => {
            let log_target = ServeLogTarget::default();
            init_tracing("info", log_target.clone());
            if let Err(err) = run_serve_configured(ServeOptions {
                port_override: port,
                config_override: config,
                adopt_running,
                auto_enable_launcher,
                idle_shutdown_secs,
                watch_pid,
                replace_stale,
                log_target: Some(log_target),
            })
            .await
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
            channel,
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
                    channel: channel.map(Into::into),
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

    use super::sensitive_transport_ceiling_allows;

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
    fn sensitive_transport_ceiling_blocks_http_debug_and_trace_but_preserves_companion_and_info() {
        for target in [
            "hyper",
            "hyper::proto",
            "hyper_util::client",
            "h2::codec",
            "reqwest::connect",
        ] {
            assert!(!sensitive_transport_ceiling_allows(
                target,
                &tracing::Level::TRACE
            ));
            assert!(!sensitive_transport_ceiling_allows(
                target,
                &tracing::Level::DEBUG
            ));
            assert!(sensitive_transport_ceiling_allows(
                target,
                &tracing::Level::INFO
            ));
            assert!(sensitive_transport_ceiling_allows(
                target,
                &tracing::Level::WARN
            ));
            assert!(sensitive_transport_ceiling_allows(
                target,
                &tracing::Level::ERROR
            ));
        }

        // Companion crates retain full debug/trace observability.
        assert!(sensitive_transport_ceiling_allows(
            "companion_cli",
            &tracing::Level::TRACE
        ));
        assert!(sensitive_transport_ceiling_allows(
            "companion_proxy::router",
            &tracing::Level::DEBUG
        ));
        // Unrelated target sharing only a prefix without `::` is not blocked.
        assert!(sensitive_transport_ceiling_allows(
            "hyper_other",
            &tracing::Level::DEBUG
        ));
    }

    #[test]
    fn composed_env_filter_and_ceiling_drops_transport_debug_trace_even_under_trace() {
        let buf = SharedBuf::default();
        let env_filter = EnvFilter::new("companion_cli=trace,hyper=trace,reqwest=trace");
        let ceiling =
            filter_fn(|meta| sensitive_transport_ceiling_allows(meta.target(), meta.level()));
        let subscriber = tracing_subscriber::registry().with(
            tracing_subscriber::fmt::layer()
                .with_writer(buf.clone())
                .with_ansi(false)
                .with_filter(env_filter.and(ceiling)),
        );

        tracing::subscriber::with_default(subscriber, || {
            tracing::trace!(target: "hyper", "LEAK_HYPER_ROOT_TRACE");
            tracing::debug!(target: "reqwest::connect", "LEAK_REQWEST_DEBUG");
            tracing::trace!(target: "h2::codec", "LEAK_H2_TRACE");
            tracing::info!(target: "hyper::proto", "ALLOW_HYPER_INFO");
            tracing::debug!(target: "companion_cli", "ALLOW_COMPANION_DEBUG");
            tracing::trace!(target: "companion_cli", "ALLOW_COMPANION_TRACE");
        });

        let captured = String::from_utf8(buf.0.lock().expect("lock buf").clone()).expect("utf8");
        assert!(!captured.contains("LEAK_HYPER_ROOT_TRACE"), "{captured}");
        assert!(!captured.contains("LEAK_REQWEST_DEBUG"), "{captured}");
        assert!(!captured.contains("LEAK_H2_TRACE"), "{captured}");
        assert!(captured.contains("ALLOW_HYPER_INFO"), "{captured}");
        assert!(captured.contains("ALLOW_COMPANION_DEBUG"), "{captured}");
        assert!(captured.contains("ALLOW_COMPANION_TRACE"), "{captured}");
    }
}
