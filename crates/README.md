# `crates/` — Rust Workspace

The 5-crate Rust workspace (`edition = "2024"`, `#![forbid(unsafe_code)]`) powering the unified `agy-companion` binary.

| Crate | Role | Depends On |
| --- | --- | --- |
| [`companion-core`](companion-core/README.md) | Nested v1 config schema, `fs_safe` TOCTOU/atomic guards, `protocol` wire DTOs, `validation`, unified Gemini 3.8+/4+ `models` catalog, secret redaction, usage seam + optional `sqlite` feature, and modular zero-`eval` `integration/{mod,templates,markers,fs_inspect,lifecycle,enable,disable,strip,tests}` login-shell hooks | — |
| [`companion-providers`](companion-providers/README.md) | `ProviderAdapter` trait, raw zero-compression `GeminiProvider`, shared pooled `LoopbackProxyClient` (`probe_client`), and Gemini 3.8+/4+ catalog filtering & capability ordering (delegating to `companion-core::models`) | `companion-core` |
| [`companion-proxy`](companion-proxy/README.md) | Loopback Axum HTTP proxy, CloudCode Hybrid Mode bridge & catalog merging, SSE usage tee, and idle-shutdown tracking | `companion-core`, `companion-providers` |
| [`companion-ui-mcp`](companion-ui-mcp/README.md) | Stdio MCP App server (`rmcp`) serving the embedded settings UI and configuration/health/launcher tools (zero direct `reqwest` dependency) | `companion-core`, `companion-providers` |
| [`companion-cli`](companion-cli/README.md) | Unified `agy-companion` CLI entrypoint (`serve.rs`, `daemon.rs`, shared `process.rs`, modular `install/{mod,uninstall,bundle,safety,proxy,windows,format,tests}.rs`, `ui-mcp`, `doctor`, `models`, `launch`, `hook-env`, `integration`) and 5 split integration test crates (`tests/{cli_serve_doctor,cli_ui_mcp,cli_daemon_integration,cli_install_lifecycle,cli_install_symlink_safety}.rs` + `tests/common/mod.rs`) | `companion-core`, `companion-providers`, `companion-proxy`, `companion-ui-mcp` |

See [`docs/design/architecture.md`](../docs/design/architecture.md) for the end-to-end architecture and [`AGENTS.md`](../AGENTS.md) for workspace invariants (`S1`–`S8`).
