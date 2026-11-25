# `companion-cli`

Unified `agy-companion` binary wiring together the loopback proxy, MCP App server, diagnostics, installer, and Dock/login-shell lifecycle helpers.

## Modules

| File | Responsibility |
| --- | --- |
| `src/main.rs` | Binary entrypoint, tracing setup (with `rmcp` capped at `INFO` so secrets are never logged), and exit-code handling |
| `src/lib.rs` | Subcommand dispatch (`serve`, `ui-mcp`, `doctor`, `models`, `launch`, `hook-env`, `integration`, `install`, `uninstall`) |
| `src/cli.rs` | `clap` CLI argument and flag definitions |
| `src/daemon.rs` | Detached daemon spawning, `--adopt-running`, atomic `0600` `proxy.pid`, and rotating `5 MiB` `proxy.log` |
| `src/doctor.rs` | Local diagnostics (config validation, permissions, placeholder-key quarantine, proxy `/health` & version-skew checks) |
| `src/models.rs` | `customModelsConfig` generator for Antigravity Gateway Mode |
| `src/launch.rs` | `agy-companion launch` Hybrid Mode launcher with MCP env restoration |
| `src/hook_env.rs` | Fast login-shell helper (`hook-env`) that ensures the proxy daemon is running and prints its port |
| `src/integration.rs` | `integration enable\|disable\|status` CLI formatting and orchestration |
| `src/install.rs` | Embedded `plugin/` bundle installer (`install` / `uninstall`, Unix + Windows parity) |
| `tests/cli_integration.rs` | End-to-end Rust CLI integration test suite |
