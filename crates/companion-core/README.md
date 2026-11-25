# `companion-core`

Shared domain types, configuration persistence, secret redaction, usage logging, and Dock/login-shell integration for `agy-companion`.

## Modules

| File | Responsibility |
| --- | --- |
| `src/config.rs` | Nested v1 `Config` (`ProxyConfig { host: LoopbackHost, port: NonZeroU16 }`, `CloudCodeConfig { upstream: CloudCodeUpstream }`, `ProvidersConfig { gemini: GeminiConfig }`, `usage_dir`), `Config::apply_patch`, XDG/Windows path precedence (`S6`), and atomic `0600` persistence (`S2`) |
| `src/fs_safe.rs` | Typed `FsError`, `normalize_path`, `validate_path`, `TempPathGuard`, `rename_with_retry`, `write_atomic` / `write_atomic_secure`, symlink/FIFO/dir-chain guards, TOCTOU `(dev, ino)` bounded reads & `FileStamp`, and `0600` regular-file open/tighten helpers |
| `src/protocol.rs` | Shared compiled wire DTOs (`PROTOCOL_VERSION`, `HealthResponse`, structured `StatsSnapshot { usage, cloudcode }`, `ProxyHealthSummary`, `CloudCodeStatus`, `HostPlatform`) |
| `src/validation.rs` | `PROXY_PLACEHOLDER_API_KEY`, `ApiKey` newtype, `validate_api_key_str`, `validate_trimmed_ascii_field`, RFC 3986 URL/path/header validators, and `sanitize_base_url` |
| `src/models.rs` | Unified Gemini 3.8+/4+ catalog (`DEFAULT_GEMINI_MODEL`, `CURATED_GEMINI_MODELS`, `ModelPreset`, `curated_model_presets`, `is_supported_gemini_model`, `validate_gemini_model`, `parse_gemini_version_pair`, `model_capability_sort_key`, `is_discoverable_chat_model`) |
| `src/redaction.rs` | Nested `RedactedConfig` / `RedactedProvidersConfig` / `RedactedGeminiConfig`, `mask_api_key`, and `api_key_hint` |
| `src/usage/mod.rs` & `src/usage/sqlite.rs` | Unconditional `GeminiUsageMetadata`, `UsageEvent`, `UsageSink`, `NoopUsageSink`; optional `sqlite` Cargo feature gating fail-closed WAL-mode `SqliteUsageSink` (`usage.db`, enabled by `companion-cli` and `companion-proxy` dev-dependencies; zero `rusqlite` in `companion-providers` or `companion-ui-mcp`) |
| `src/integration.rs` | Zero-`eval` login-shell integration (`env.sh`, `~/.zlogin` / `~/.profile` / `fish/conf.d`), canonical `IntegrationStatus.state` enum with safe constructors & `mark_hybrid_active()`, SSH guard, and collision-safe MCP env restore |
| `src/lib.rs` | Crate re-exports and `#![forbid(unsafe_code)]` enforcement |
