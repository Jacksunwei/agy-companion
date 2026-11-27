# `companion-cli`

Unified `agy-companion` binary wiring together the loopback proxy, Sidecar Web UI server, diagnostics, installer, and Dock/login-shell lifecycle helpers.

## Modules

| File | Responsibility |
| --- | --- |
| `src/main.rs` | Binary entrypoint, unified generic `init_tracing<W>` setup (capping `hyper`, `hyper_util`, `h2`, and `reqwest` at `INFO` so secrets/tokens are never logged), and exit-code handling |
| `src/lib.rs` | Crate root (`#![forbid(unsafe_code)]`), `CliError`, config/auth/data-dir prechecks (`precheck_existing_auth_dir`), and public re-exports |
| `src/cli.rs` | `clap` CLI argument and flag definitions |
| `src/serve.rs` | `ServeOptions`, `ADOPT_RUNNING_POLL_INTERVAL`, `run_serve` / `run_serve_configured` (Sidecar Web UI startup, signal handling, `--adopt-running`, idle shutdown, `proxy.pid` & `proxy.log` lifecycle) |
| `src/sidecar_ui.rs` | Loopback Axum Sidecar Web UI server (`GET /` embedded HTML + CSP, token-gated `POST /_api/{get_settings,save_settings,test_connection,set_launcher_integration,get_usage}`) |
| `src/daemon.rs` | Detached daemon spawning, unified `wait_for_daemon_healthy` (with `child.try_wait()` early-exit detection and bind-race final probe), atomic `0600` `proxy.pid` (RAII guard delegating to `process::remove_pid_file_if_matches`), and rotating `5 MiB` `proxy.log` |
| `src/process.rs` | Shared zero-unsafe 4-layer PID identity verification, OS process inspection (Linux/macOS/Windows), safe proxy termination, `remove_pid_file_if_matches`, `$PATH` lookup, and Antigravity process detection |
| `src/doctor.rs` | Local diagnostics (config & `auth/gemini.json` validation/permissions, placeholder-key quarantine, proxy `/health` & version/protocol-skew checks via `companion-providers::probe_client`) |
| `src/models.rs` | `customModelsConfig` generator for Antigravity Gateway Mode |
| `src/launch.rs` | `agy-companion launch` Hybrid Mode launcher with MCP env restoration |
| `src/hook_env.rs` | Fast login-shell helper (`hook-env`) that ensures the proxy daemon is running and prints its port |
| `src/integration.rs` | `integration enable\|disable\|status` CLI formatting and orchestration |
| `src/install/mod.rs` | `InstallOptions`, `UninstallOptions`, `InstallReport`, `UninstallReport`, and `run_install` orchestration (with `--config` support) |
| `src/install/uninstall.rs` | `run_uninstall` lifecycle (RC stripping, `env.sh`, plugin-before-proxy removal, `--dry-run`, `--purge`) |
| `src/install/bundle.rs` | `EMBEDDED_PLUGIN_FILES` (`include_str!` byte-for-byte `plugin/` bundle), staging, `~/.local/bin` symlink management, fresh-install port fallback, and two-step atomic swaps with `companion-core::fs_safe` Windows rename retry |
| `src/install/safety.rs` | Directory-chain symlink checks (delegating to `companion-core::fs_safe`), safe removal guards, and path normalization helpers |
| `src/install/proxy.rs` | Install-time proxy startup and version-skew replacement via `daemon::wait_for_daemon_healthy` |
| `src/install/windows.rs` | Windows manifest derivation (parsing & patching embedded `plugin/sidecars/proxy/sidecar.json` without drift) and Start Menu `.lnk` shortcut management |
| `src/install/format.rs` | Human-readable and JSON formatting for install/uninstall reports |
| `src/install/tests.rs` | Unit tests for embedded bundle parity, Windows manifest derivation, PID safety, two-step swap rollback, and directory-chain guards |
| `tests/common/mod.rs` | Shared integration test harness (`ChildGuard`, `DetachedPidFileGuard`, PID-partitioned `free_loopback_port`, `spawn_serve_*`) |
| `tests/cli_integration.rs` | Unified integration test crate (`#![forbid(unsafe_code)]`) covering `serve`, `doctor`, `models`, `hook-env`, `integration`, `launch`, `install`/`uninstall`, idle shutdown, and symlink/FIFO safety |
