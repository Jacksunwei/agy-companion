# `companion-core`

Shared domain types, configuration persistence, secret redaction, usage logging, and Dock/login-shell integration for `agy-companion`.

## Modules

| File | Responsibility |
| --- | --- |
| `src/config.rs` | Nested v1 non-secret `Config` (`ProxyConfig { host: LoopbackHost, port: NonZeroU16 }`, `CloudCodeConfig { upstream: CloudCodeUpstream }`, `ProvidersConfig { gemini: GeminiConfig, codex: CodexConfig }`, `usage_dir`), `Config::apply_patch`, `Config::redacted_with_auth`, XDG/Windows path precedence (`S6`), and atomic `0600` persistence (`S2`) |
| `src/auth.rs` | Unified provider credential storage (`AuthStore`, `FileAuthStore`, `MemoryAuthStore`, `SecretString`, `GeminiCredentials` (`auth/gemini.json`), `CodexAuthMode`/`CodexCredentials` (`auth/codex.json`), sibling `auth_dir_for_config_path`, `0700`/`0600` atomic writes, and cross-process `CodexRefreshLock` (`auth/.codex-refresh.lock`)) |
| `src/fs_safe.rs` | Typed `FsError`, `normalize_path`, `validate_path`, `TempPathGuard`, `rename_with_retry`, `write_atomic` / `write_atomic_secure`, symlink/FIFO/dir-chain guards, TOCTOU `(dev, ino)` bounded reads & `FileStamp`, and `0600` regular-file open/tighten helpers |
| `src/protocol.rs` | Shared compiled wire DTOs (`PROTOCOL_VERSION`, `HealthResponse`, structured `StatsSnapshot { usage, cloudcode }`, `ProxyHealthSummary`, `CloudCodeStatus`, `HostPlatform`) |
| `src/validation.rs` | `PROXY_PLACEHOLDER_API_KEY`, `ApiKey` newtype, redacted `CsrfToken` newtype (`MAX_CSRF_TOKEN_BYTES`), zero-DNS loopback-only `LsEndpoint`/`LsHost` parser (`S1`/`S3`), `validate_api_key_str`, `validate_trimmed_ascii_field`, RFC 3986 URL/path/header validators, and `sanitize_base_url` |
| `src/models.rs` | Curated Gemini 3.8+/4+ & GPT-6.x Codex catalogs (`CURATED_GEMINI_MODELS`, `CURATED_CODEX_MODELS`, `compute_compaction_window`, `COMPACTION_WINDOW_PERCENT`, `ModelPreset`, `curated_model_presets`, `curated_codex_model_presets`, `validate_gemini_model`, `validate_codex_model`, `is_codex_model`, `normalize_enabled_gemini_models`, `normalize_enabled_codex_models`) |
| `src/redaction.rs` | Nested `RedactedConfig` / `RedactedProvidersConfig` / `RedactedGeminiConfig` / `RedactedCodexConfig`, `Config::redacted_with_auth`, `mask_api_key`, `api_key_hint`, and `mask_account_id` |
| `src/usage/mod.rs` & `src/usage/sqlite.rs` | Unconditional `GeminiUsageMetadata`, `UsageEvent`, `TelemetrySource`, `LsSyncWatermark`, `ObservedModel`, pre-bind sanitization/UTC helpers, `UsageSink`, `NoopUsageSink`; optional `sqlite` Cargo feature gating fail-closed WAL-mode SQLite v3 `SqliteUsageSink` (`usage.db`, sequential `v1→v2→v3` / `v2→v3` in-place auto-migration, commutative multi-clause `ON CONFLICT` reconciliation, `ls_sync_watermarks`, `observed_models` compaction-limit persistence, and `-wal`/`-shm` `ConcurrentModification` retry; zero `rusqlite` in `companion-providers`) |
| `src/integration/` | Modular zero-`eval` login-shell integration (`mod.rs` types/facades/path resolution, `templates.rs` POSIX `env.sh` & Fish drop-in rendering, `markers.rs` EOL-preserving marker parsing/upserting/stripping, `fs_inspect.rs` symlink/dir-chain/RC inspection, `lifecycle.rs` `LifecycleEnv` & status evaluation, `enable.rs` enable workflow, `disable.rs` disable workflow, `strip.rs` known-RC cleanup, `tests.rs` unit tests); canonical `IntegrationStatus.state` enum with safe constructors & `mark_hybrid_active()`, SSH guard, and collision-safe MCP env restore |
| `src/lib.rs` | Crate re-exports and `#![forbid(unsafe_code)]` enforcement |
