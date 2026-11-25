#![forbid(unsafe_code)]

pub mod config;
pub mod integration;
pub mod redaction;
pub mod usage;

pub use config::{
    CLOUDCODE_DAILY_UPSTREAM_URL, CLOUDCODE_PROD_UPSTREAM_URL, CONFIG_PATH_ENV_VAR,
    CURRENT_SCHEMA_VERSION, Config, CoreError, DEFAULT_CLOUDCODE_UPSTREAM_URL,
    DEFAULT_GEMINI_BASE_URL, DEFAULT_GEMINI_MODEL, DEFAULT_LISTEN_HOST, DEFAULT_LISTEN_PORT,
    DEFAULT_TIMEOUT_SECS, GeminiConfig, MAX_CONFIG_FILE_BYTES, PROXY_PLACEHOLDER_API_KEY,
    default_config_path, default_data_dir, default_plugins_dir, default_usage_dir,
    is_placeholder_api_key, is_supported_gemini_model, resolve_config_path, resolve_data_dir,
    resolve_plugins_dir, resolve_usage_dir, validate_cloudcode_upstream_url, validate_gemini_model,
    validate_listen_host, validate_path, validate_upstream_url, write_file_atomic_secure,
};
pub use integration::{
    DEFAULT_RESTORE_PREFIX, HOOK_BEGIN_MARKER, HOOK_END_MARKER, IntegrationOptions,
    IntegrationState, IntegrationStatus, ShellKind, StripReport,
};
pub use redaction::{RedactedConfig, api_key_hint, mask_api_key};
pub use usage::{
    GeminiUsageMetadata, NoopUsageSink, SqliteUsageSink, USAGE_DB_FILENAME,
    USAGE_DB_SCHEMA_VERSION, UsageEvent, UsageSink,
};
