# `companion-cli`

Unified `agy-companion` binary wiring together the loopback proxy, Sidecar Web UI server, diagnostics, installer, and Dock/login-shell lifecycle helpers.

## Modules

| File | Responsibility |
| --- | --- |
| `src/main.rs` | Binary entrypoint, unified generic `init_tracing<W>` setup (capping `hyper`, `hyper_util`, `h2`, and `reqwest` at `INFO` so secrets/tokens are never logged), and exit-code handling |
| `src/lib.rs` | Crate root (`#![forbid(unsafe_code)]`), `CliError`, config/auth/data-dir prechecks (`precheck_existing_auth_dir`), and public re-exports |
| `src/cli.rs` | `clap` CLI argument and flag definitions |
| `src/serve.rs` | `ServeOptions`, `ADOPT_RUNNING_POLL_INTERVAL`, `WATCH_PID_POLL_INTERVAL`, `run_serve` / `run_serve_configured` (per-window `LsCollector` & Sidecar Web UI startup, `--adopt-running` unified version-skew & plugin-manifest self-upgrade handoff across standby and active-listener states, `--replace-stale` smooth zero-drop SSE port reclamation, `--watch-pid` parent-exit drain, idle shutdown, `role` reporting, `proxy.pid` & `proxy.log` lifecycle) |
| `src/plugin_upgrade.rs` | S6-validated plugin directory resolution (`AGY_COMPANION_PLUGIN_DIR` -> `AGY_PLUGINS_DIR` -> default plugins dir), fail-closed `plugin.json` version reader (`<= 64 KiB`, `SymlinkPolicy::Reject`), passive local candidate binary discovery, and `(len, mtime)`-cached scrubbed `--version` probing (`BinaryVersionProbeCache`) |
| `src/ls_collector.rs` | Per-window Antigravity LS telemetry collector (`JetboxSubscribeToSummaries` multi-frame boot baseline + `30s` `GetAllCascadeTrajectories` fallback, tail `GetCascadeTrajectorySteps(PROD_UI)`, per-port advisory lock, token/latency normalization, 3-tier attribution, `CHECKPOINT` fold, per-root subagent Monitor rollup, circuit-breaker fail-open) |
| `src/sidecar_ui.rs` | Loopback Axum Sidecar Web UI server (`GET /` embedded HTML + CSP, token-gated `POST /_api/{get_settings,save_settings,test_connection,set_launcher_integration,get_usage}` with live LS Monitor overlay & `:24141` fail-open) |
| `src/daemon.rs` | Detached daemon spawning (`DetachedDaemonOptions` with `--watch-pid`/`--replace-stale` and sidecar/LS env scrubbing), unified `wait_for_daemon_healthy` (with `child.try_wait()` early-exit detection and bind-race final probe), atomic `0600` `proxy.pid` (PID-checked RAII guard), and rotating `5 MiB` `proxy.log` |
| `src/process.rs` | Shared zero-unsafe 4-layer PID identity verification, OS process inspection (Linux/macOS/Windows), graceful-only `SIGTERM` helper (`signal_verified_companion_graceful_stop`), zombie/PID-reuse-safe `WatchedProcessIdentity`, `Antigravity` ancestor PID walker, `remove_pid_file_if_matches`, `$PATH` lookup, and Antigravity process detection |
| `src/doctor.rs` | Local diagnostics (config & `auth/gemini.json` validation/permissions, placeholder-key quarantine, proxy `/health` & version/protocol-skew checks via `companion-providers::probe_client`) |
| `src/models.rs` | `customModelsConfig` generator for Antigravity Gateway Mode |
| `src/launch.rs` | `agy-companion launch` Hybrid Mode launcher with MCP env restoration |
| `src/hook_env.rs` | Fast login-shell helper (`hook-env`) that detects the `Antigravity` ancestor PID under `ANTIGRAVITY_RESOLVING_ENVIRONMENT=1`, ensures the proxy daemon is running with `--watch-pid`, and prints its port |
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
| `tests/cli_integration.rs` | Unified integration test crate (`#![forbid(unsafe_code)]`) covering `serve`, `doctor`, `models`, `hook-env`, `integration`, `launch`, `install`/`uninstall`, idle shutdown, `--watch-pid`/`--replace-stale`/`--adopt-running` proxy lifecycle handoffs, and symlink/FIFO safety |
