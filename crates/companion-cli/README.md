# `companion-cli`

Unified `agy-companion` binary wiring together the loopback proxy, MCP App server, diagnostics, installer, and Dock/login-shell lifecycle helpers.

## Modules

| File | Responsibility |
| --- | --- |
| `src/main.rs` | Binary entrypoint, unified generic `init_tracing<W>` setup (with `rmcp` capped at `INFO` so secrets are never logged), and exit-code handling |
| `src/lib.rs` | Crate root (`#![forbid(unsafe_code)]`), `CliError`, config/data-dir prechecks, `run_ui_mcp`, and public re-exports |
| `src/cli.rs` | `clap` CLI argument and flag definitions |
| `src/serve.rs` | `ServeOptions`, `ADOPT_RUNNING_POLL_INTERVAL`, `run_serve` / `run_serve_configured` (signal handling, `--adopt-running`, idle shutdown, `proxy.pid` & `proxy.log` lifecycle) |
| `src/daemon.rs` | Detached daemon spawning, unified `wait_for_daemon_healthy` (with `child.try_wait()` early-exit detection and bind-race final probe), atomic `0600` `proxy.pid` (RAII guard delegating to `process::remove_pid_file_if_matches`), and rotating `5 MiB` `proxy.log` |
| `src/process.rs` | Shared zero-unsafe 4-layer PID identity verification, OS process inspection (Linux/macOS/Windows), safe proxy termination, `remove_pid_file_if_matches`, `$PATH` lookup, and Antigravity process detection |
| `src/doctor.rs` | Local diagnostics (config validation, permissions, placeholder-key quarantine, proxy `/health` & version/protocol-skew checks via `companion-providers::probe_client`) |
| `src/models.rs` | `customModelsConfig` generator for Antigravity Gateway Mode |
| `src/launch.rs` | `agy-companion launch` Hybrid Mode launcher with MCP env restoration |
| `src/hook_env.rs` | Fast login-shell helper (`hook-env`) that ensures the proxy daemon is running and prints its port |
| `src/integration.rs` | `integration enable\|disable\|status` CLI formatting and orchestration |
| `src/install/mod.rs` | `InstallOptions`, `UninstallOptions`, `InstallReport`, `UninstallReport`, and `run_install` orchestration (with `--config` support) |
| `src/install/uninstall.rs` | `run_uninstall` lifecycle (RC stripping, `env.sh`, plugin-before-proxy removal, `--dry-run`, `--purge`) |
| `src/install/bundle.rs` | `EMBEDDED_PLUGIN_FILES` (`include_str!` byte-for-byte `plugin/` bundle), staging, `~/.local/bin` symlink management, fresh-install port fallback, and two-step atomic swaps with `companion-core::fs_safe` Windows rename retry |
| `src/install/safety.rs` | Directory-chain symlink checks (delegating to `companion-core::fs_safe`), safe removal guards, and path normalization helpers |
| `src/install/proxy.rs` | Install-time proxy startup and version-skew replacement via `daemon::wait_for_daemon_healthy` |
| `src/install/windows.rs` | Windows manifest derivation (parsing & patching embedded `plugin/mcp_config.json` and `plugin/sidecars/proxy/sidecar.json` without drift) and Start Menu `.lnk` shortcut management |
| `src/install/format.rs` | Human-readable and JSON formatting for install/uninstall reports |
| `src/install/tests.rs` | Unit tests for embedded bundle parity, Windows manifest derivation, PID safety, two-step swap rollback, and directory-chain guards |
| `tests/common/mod.rs` | Shared integration test harness (`ChildGuard`, `DetachedPidFileGuard`, PID-partitioned `free_loopback_port`, `spawn_serve_*`, `run_ui_mcp_bounded*`) |
| `tests/cli_serve_doctor.rs` | Integration tests for `serve`, `doctor`, `models`, config precedence, `--adopt-running`, and idle shutdown (`#![forbid(unsafe_code)]`) |
| `tests/cli_ui_mcp.rs` | Integration tests for `ui-mcp` stdio JSON-RPC, secret redaction, FIFO/symlink rejection, and degraded startup (`#![forbid(unsafe_code)]`) |
| `tests/cli_daemon_integration.rs` | Integration tests for `hook-env`, `integration`, `launch`, and concurrent cold-start daemon lifecycle (`#![forbid(unsafe_code)]`) |
| `tests/cli_install_lifecycle.rs` | Integration tests for `install`/`uninstall` full lifecycle, step-1 preconditions, and explicit `--config` (`#![forbid(unsafe_code)]`) |
| `tests/cli_install_symlink_safety.rs` | Integration tests for symlink-traversal refusal across plugins, bin, runtime data, XDG, Fish, and `env.sh` paths (`#![forbid(unsafe_code)]`) |
