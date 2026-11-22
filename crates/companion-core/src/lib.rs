#![forbid(unsafe_code)]

pub mod config;
pub mod redaction;
pub mod usage;

pub use config::{
    CONFIG_PATH_ENV_VAR, CURRENT_SCHEMA_VERSION, Config, CoreError, DEFAULT_GEMINI_BASE_URL,
    DEFAULT_GEMINI_MODEL, DEFAULT_LISTEN_HOST, DEFAULT_LISTEN_PORT, DEFAULT_TIMEOUT_SECS,
    GeminiConfig, default_config_path, default_usage_dir, resolve_config_path, resolve_usage_dir,
    validate_listen_host, validate_path, validate_upstream_url,
};
pub use redaction::{RedactedConfig, api_key_hint, mask_api_key};
pub use usage::{GeminiUsageMetadata, JsonlUsageSink, NoopUsageSink, UsageEvent, UsageSink};
