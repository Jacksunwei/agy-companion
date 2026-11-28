#![forbid(unsafe_code)]

pub mod auth;
pub mod config;
pub mod fs_safe;
pub mod integration;
pub mod models;
pub mod protocol;
pub mod redaction;
pub mod usage;
pub mod validation;

pub use auth::{
    AUTH_DIR_NAME, AUTH_SCHEMA_VERSION, AuthStore, CODEX_AUTH_FILENAME,
    CODEX_REFRESH_LOCK_FILENAME, CodexAuthMode, CodexCredentials, CodexRefreshLock, FileAuthStore,
    GEMINI_AUTH_FILENAME, GeminiCredentials, MAX_AUTH_FILE_BYTES, MemoryAuthStore, SecretString,
    auth_dir_for_config_path, inspect_auth_file_stamp, read_verified_auth_bytes_for_stamp,
};
pub use config::{
    ApiKeyPatch, CLOUDCODE_DAILY_UPSTREAM_URL, CLOUDCODE_PROD_UPSTREAM_URL, CONFIG_PATH_ENV_VAR,
    CURRENT_SCHEMA_VERSION, CloudCodeConfig, CloudCodeConfigPatch, CloudCodeUpstream, CodexConfig,
    CodexConfigPatch, Config, ConfigPatch, CoreError, DEFAULT_CLOUDCODE_UPSTREAM_URL,
    DEFAULT_CODEX_BASE_URL, DEFAULT_GEMINI_BASE_URL, DEFAULT_LISTEN_HOST, DEFAULT_LISTEN_PORT,
    DEFAULT_LISTEN_PORT_NONZERO, DEFAULT_TIMEOUT_SECS, GeminiConfig, GeminiConfigPatch,
    LoopbackHost, MAX_CONFIG_FILE_BYTES, ProvidersConfig, ProvidersConfigPatch, ProxyConfig,
    ProxyConfigPatch, default_config_path, default_data_dir, default_plugins_dir,
    default_usage_dir,
};
pub use fs_safe::{
    AtomicWriteOptions, BoundedReadResult, DirChainStatus, FileStamp, FsError, OpenRegularMode,
    ParentDirPolicy, PathLeafKind, RejectedReason, SymlinkPolicy, TargetFilePolicy, TempPathGuard,
    inspect_leaf_kind, validate_path, write_atomic, write_atomic_secure,
};
pub use integration::{
    AutoEnableOutcome, DEFAULT_RESTORE_PREFIX, HOOK_BEGIN_MARKER, HOOK_END_MARKER,
    INTEGRATION_LOCK_FILENAME, IntegrationLock, IntegrationOptions, IntegrationPaths,
    IntegrationState, IntegrationStatus, ShellKind, StripReport, disable_and_persist,
    disable_and_persist_with_hybrid_active, enable_and_persist,
    enable_and_persist_with_hybrid_active, ensure_first_run_launcher_enabled,
    ensure_first_run_launcher_enabled_with_hybrid_active, set_launcher_auto_start_locked,
    status_with_hybrid_active,
};
pub use models::{
    CURATED_CODEX_MODELS, CURATED_GEMINI_MODELS, CuratedCodexModel, CuratedGeminiModel,
    DEFAULT_CODEX_MODEL, DEFAULT_ENABLED_CODEX_MODELS, DEFAULT_ENABLED_GEMINI_MODELS,
    DEFAULT_GEMINI_MODEL, MAX_CODEX_MODEL_LEN, MAX_GEMINI_MODEL_LEN, ModelPreset,
    StaticPlaceholderMapping, UpstreamModelCatalog, UpstreamModelEntry,
    curated_codex_model_presets, curated_model_presets, default_enabled_codex_models,
    default_enabled_gemini_models, find_curated_codex_model, is_codex_model,
    is_discoverable_chat_model, is_supported_gemini_model, lookup_static_placeholder,
    model_capability_sort_key, normalize_enabled_codex_models, normalize_enabled_gemini_models,
    normalize_placeholder_enum_key, parse_gemini_version_pair, placeholder_numeric_alias,
    validate_codex_model, validate_gemini_model,
};
pub use protocol::{
    CloudCodeStatsSnapshot, CloudCodeStatus, HealthResponse, HostPlatform, MAX_RECENT_CALLS,
    MAX_SESSION_MODELS, ModelSessionStats, PROTOCOL_VERSION, ProxyHealthSummary,
    RecentCallSnapshot, StatsSnapshot, UsageStatsSnapshot,
};
pub use redaction::{
    RedactedCodexConfig, RedactedConfig, RedactedGeminiConfig, RedactedProvidersConfig,
    api_key_hint, mask_account_id, mask_api_key,
};
pub use usage::{
    GeminiUsageMetadata, LsSyncWatermark, MAX_CONVERSATION_ID_LEN, MAX_MODEL_DISPLAY_NAME_LEN,
    MAX_RESPONSE_ID_LEN, MIN_CONVERSATION_ID_LEN, NoopUsageSink, ObservedModel, TelemetrySource,
    USAGE_DB_FILENAME, USAGE_DB_SCHEMA_VERSION, UsageBreakdownRow, UsageBucket, UsageEvent,
    UsageRange, UsageSink, UsageSummary, normalize_utc_timestamp, sanitize_conversation_id,
    sanitize_model_display_name, sanitize_response_id,
};
#[cfg(feature = "sqlite")]
pub use usage::{SqliteUsageSink, query_usage_summary, query_usage_summary_at};
pub use validation::{
    ApiKey, CsrfToken, LsEndpoint, LsHost, MAX_CSRF_TOKEN_BYTES, PROXY_PLACEHOLDER_API_KEY,
    decode_ascii_hex_nibble, deserialize_optional_api_key, is_placeholder_api_key,
    is_printable_ascii_with_space, is_safe_path_segment_byte, sanitize_base_url,
    validate_api_key_str, validate_cloudcode_upstream_url, validate_listen_host,
    validate_trimmed_ascii_field, validate_upstream_url, validate_url_path,
};
