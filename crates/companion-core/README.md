# `companion-core`

Shared domain types, configuration persistence, secret redaction, usage logging, and Dock/login-shell integration for `agy-companion`.

## Modules

| File | Responsibility |
| --- | --- |
| `src/config.rs` | Nested v1 non-secret `Config` (`ProxyConfig { host: LoopbackHost, port: NonZeroU16 }`, `CloudCodeConfig { upstream: CloudCodeUpstream }`, `ProvidersConfig { gemini: GeminiConfig }`, `usage_dir`), `Config::apply_patch`, XDG/Windows path precedence (`S6`), and atomic `0600` persistence (`S2`) |
| `src/auth.rs` | Unified provider credential storage (`AuthStore`, `FileAuthStore`, `MemoryAuthStore`, `SecretString`, `GeminiCredentials`, `auth_dir_for_config_path`) storing secrets outside `config.json` in sibling `0700` `auth/` / `0600` `auth/gemini.json` |
| `src/fs_safe.rs` | Typed `FsError`, `normalize_path`, `validate_path`, `TempPathGuard`, `rename_with_retry`, `write_atomic` / `write_atomic_secure`, symlink/FIFO/dir-chain guards, TOCTOU `(dev, ino)` bounded reads & `FileStamp`, and `0600` regular-file open/tighten helpers |
| `src/protocol.rs` | Shared compiled wire DTOs (`PROTOCOL_VERSION`, `HealthResponse`, structured `StatsSnapshot { usage, cloudcode }`, `ProxyHealthSummary`, `CloudCodeStatus`, `HostPlatform`) |
| `src/validation.rs` | `PROXY_PLACEHOLDER_API_KEY`, `ApiKey` newtype, `validate_api_key_str`, `validate_trimmed_ascii_field`, RFC 3986 URL/path/header validators, and `sanitize_base_url` |
| `src/models.rs` | Unified Gemini 3.8+/4+ catalog (`DEFAULT_GEMINI_MODEL`, `CURATED_GEMINI_MODELS`, `ModelPreset`, `curated_model_presets`, `is_supported_gemini_model`, `validate_gemini_model`, `parse_gemini_version_pair`, `model_capability_sort_key`, `is_discoverable_chat_model`) |
| `src/redaction.rs` | Nested `RedactedConfig` / `RedactedProvidersConfig` / `RedactedGeminiConfig`, `Config::redacted_with_auth`, `mask_api_key`, and `api_key_hint` |
| `src/usage/mod.rs` & `src/usage/sqlite.rs` | Unconditional `GeminiUsageMetadata`, `UsageEvent`, `UsageSink`, `NoopUsageSink`; optional `sqlite` Cargo feature gating fail-closed WAL-mode `SqliteUsageSink` (`usage.db`, enabled by `companion-cli` and `companion-proxy` dev-dependencies; zero `rusqlite` in `companion-providers`) |
| `src/integration/` | Modular zero-`eval` login-shell integration (`mod.rs` types/facades/path resolution, `templates.rs` POSIX `env.sh` & Fish drop-in rendering, `markers.rs` EOL-preserving marker parsing/upserting/stripping, `fs_inspect.rs` symlink/dir-chain/RC inspection, `lifecycle.rs` `LifecycleEnv` & status evaluation, `enable.rs` enable workflow, `disable.rs` disable workflow, `strip.rs` known-RC cleanup, `tests.rs` unit tests); canonical `IntegrationStatus.state` enum with safe constructors & `mark_hybrid_active()`, SSH guard, and collision-safe MCP env restore |
| `src/lib.rs` | Crate re-exports and `#![forbid(unsafe_code)]` enforcement |
