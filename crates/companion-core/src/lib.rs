#![forbid(unsafe_code)]

pub mod config;
pub mod fs_safe;
pub mod integration;
pub mod models;
pub mod protocol;
pub mod redaction;
pub mod usage;
pub mod validation;

pub use config::{
    ApiKeyPatch, CLOUDCODE_DAILY_UPSTREAM_URL, CLOUDCODE_PROD_UPSTREAM_URL, CONFIG_PATH_ENV_VAR,
    CURRENT_SCHEMA_VERSION, CloudCodeConfig, CloudCodeConfigPatch, CloudCodeUpstream, Config,
    ConfigPatch, CoreError, DEFAULT_CLOUDCODE_UPSTREAM_URL, DEFAULT_GEMINI_BASE_URL,
    DEFAULT_LISTEN_HOST, DEFAULT_LISTEN_PORT, DEFAULT_LISTEN_PORT_NONZERO, DEFAULT_TIMEOUT_SECS,
    GeminiConfig, GeminiConfigPatch, LoopbackHost, MAX_CONFIG_FILE_BYTES, ProvidersConfig,
    ProvidersConfigPatch, ProxyConfig, ProxyConfigPatch, default_config_path, default_data_dir,
    default_plugins_dir, default_usage_dir,
};
pub use fs_safe::{
    AtomicWriteOptions, BoundedReadResult, DirChainStatus, FileStamp, FsError, OpenRegularMode,
    ParentDirPolicy, RejectedReason, SymlinkPolicy, TargetFilePolicy, TempPathGuard, validate_path,
    write_atomic, write_atomic_secure,
};
pub use integration::{
    AutoEnableOutcome, DEFAULT_RESTORE_PREFIX, HOOK_BEGIN_MARKER, HOOK_END_MARKER,
    INTEGRATION_LOCK_FILENAME, IntegrationLock, IntegrationOptions, IntegrationPaths,
    IntegrationState, IntegrationStatus, ShellKind, StripReport, disable_and_persist,
    enable_and_persist, ensure_first_run_launcher_enabled, set_launcher_auto_start_locked,
};
pub use models::{
    CURATED_GEMINI_MODELS, CuratedGeminiModel, DEFAULT_ENABLED_GEMINI_MODELS, DEFAULT_GEMINI_MODEL,
    ModelPreset, curated_model_presets, default_enabled_gemini_models, is_discoverable_chat_model,
    is_supported_gemini_model, model_capability_sort_key, normalize_enabled_gemini_models,
    parse_gemini_version_pair, validate_gemini_model,
};
pub use protocol::{
    CloudCodeStatsSnapshot, CloudCodeStatus, HealthResponse, HostPlatform, PROTOCOL_VERSION,
    ProxyHealthSummary, StatsSnapshot, UsageStatsSnapshot,
};
pub use redaction::{
    RedactedConfig, RedactedGeminiConfig, RedactedProvidersConfig, api_key_hint, mask_api_key,
};
#[cfg(feature = "sqlite")]
pub use usage::SqliteUsageSink;
pub use usage::{
    GeminiUsageMetadata, NoopUsageSink, USAGE_DB_FILENAME, USAGE_DB_SCHEMA_VERSION, UsageEvent,
    UsageSink,
};
pub use validation::{
    ApiKey, PROXY_PLACEHOLDER_API_KEY, decode_ascii_hex_nibble, deserialize_optional_api_key,
    is_placeholder_api_key, is_printable_ascii_with_space, is_safe_path_segment_byte,
    sanitize_base_url, validate_api_key_str, validate_cloudcode_upstream_url, validate_listen_host,
    validate_trimmed_ascii_field, validate_upstream_url, validate_url_path,
};
