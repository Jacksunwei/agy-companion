# `companion-core`

Shared domain types, configuration persistence, secret redaction, usage logging, and Dock/login-shell integration for `agy-companion`.

## Modules

| File | Responsibility |
| --- | --- |
| `src/config.rs` | `Config` schema, Gemini 3.8+/4.x model validation, XDG/Windows path precedence (`S6`), fail-closed symlink/FIFO/size guards, and atomic `0600` persistence (`S2`) |
| `src/redaction.rs` | Secret masking (`RedactedConfig`, `api_key_hint`), `PROXY_PLACEHOLDER_API_KEY` quarantine, and header-safe key checks |
| `src/usage.rs` | `UsageSink` trait, `UsageEvent`, and fail-closed WAL-mode SQLite writer (`usage.db`) |
| `src/integration.rs` | Zero-`eval` login-shell integration (`env.sh`, `~/.zlogin` / `~/.profile` / `fish/conf.d`), SSH guard, and collision-safe MCP env restore |
| `src/lib.rs` | Crate re-exports and `#![forbid(unsafe_code)]` enforcement |
