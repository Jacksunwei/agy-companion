# `crates/` — Rust Workspace

The 5-crate Rust workspace (`edition = "2024"`, `#![forbid(unsafe_code)]`) powering the unified `agy-companion` binary.

| Crate | Role | Depends On |
| --- | --- | --- |
| [`companion-core`](companion-core/README.md) | Nested v1 config schema, `fs_safe` TOCTOU/atomic guards, `protocol` wire DTOs, `validation`, unified Gemini 3.8+/4+ `models` catalog, secret redaction, usage seam + optional `sqlite` feature, and zero-`eval` login-shell integration | — |
| [`companion-providers`](companion-providers/README.md) | `ProviderAdapter` trait, raw zero-compression `GeminiProvider`, and Gemini 3.8+/4+ catalog filtering & capability ordering (delegating to `companion-core::models`) | `companion-core` |
| [`companion-proxy`](companion-proxy/README.md) | Loopback Axum HTTP proxy, CloudCode Hybrid Mode bridge & catalog merging, SSE usage tee, and idle-shutdown tracking | `companion-core`, `companion-providers` |
| [`companion-ui-mcp`](companion-ui-mcp/README.md) | Stdio MCP App server (`rmcp`) serving the embedded settings UI and configuration/health/launcher tools | `companion-core`, `companion-providers` |
| [`companion-cli`](companion-cli/README.md) | Unified `agy-companion` CLI entrypoint (`serve`, `ui-mcp`, `doctor`, `models`, `launch`, `hook-env`, `integration`, `install`, `uninstall`) | `companion-core`, `companion-proxy`, `companion-ui-mcp` |

See [`docs/design/architecture.md`](../docs/design/architecture.md) for the end-to-end architecture and [`AGENTS.md`](../AGENTS.md) for workspace invariants (`S1`–`S8`).
