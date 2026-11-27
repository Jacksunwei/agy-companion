use std::fmt;
use std::num::NonZeroU16;
use std::path::{Path, PathBuf};
use std::str::FromStr;

use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::auth::{CodexCredentials, GeminiCredentials};
use crate::fs_safe::{self, FsError, SymlinkPolicy};
use crate::models::{
    DEFAULT_CODEX_MODEL, DEFAULT_GEMINI_MODEL, default_enabled_codex_models,
    default_enabled_gemini_models, normalize_enabled_codex_models, normalize_enabled_gemini_models,
    validate_codex_model, validate_gemini_model,
};
use crate::redaction::{
    RedactedCodexConfig, RedactedConfig, RedactedGeminiConfig, RedactedProvidersConfig,
    api_key_hint,
};
use crate::validation::{validate_trimmed_ascii_field, validate_upstream_url};

pub const CURRENT_SCHEMA_VERSION: u32 = 1;
pub const DEFAULT_LISTEN_HOST: &str = "127.0.0.1";
pub const DEFAULT_LISTEN_PORT: u16 = 24141;
pub const DEFAULT_LISTEN_PORT_NONZERO: NonZeroU16 =
    NonZeroU16::new(DEFAULT_LISTEN_PORT).expect("DEFAULT_LISTEN_PORT is non-zero");
pub const DEFAULT_GEMINI_BASE_URL: &str = "https://generativelanguage.googleapis.com";
pub const DEFAULT_CODEX_BASE_URL: &str = "https://chatgpt.com/backend-api/codex";
pub const CLOUDCODE_DAILY_UPSTREAM_URL: &str = "https://daily-cloudcode-pa.googleapis.com";
pub const CLOUDCODE_PROD_UPSTREAM_URL: &str = "https://cloudcode-pa.googleapis.com";
pub const DEFAULT_CLOUDCODE_UPSTREAM_URL: &str = "auto";
pub const DEFAULT_TIMEOUT_SECS: u64 = 900;
pub const MAX_CONFIG_FILE_BYTES: u64 = 1024 * 1024;
pub const CONFIG_PATH_ENV_VAR: &str = "AGY_COMPANION_CONFIG";
pub(crate) const XDG_CONFIG_HOME_ENV_VAR: &str = "XDG_CONFIG_HOME";
pub(crate) const XDG_DATA_HOME_ENV_VAR: &str = "XDG_DATA_HOME";

#[derive(Debug, Error)]
pub enum CoreError {
    #[error("unsupported config schema version {found} (supported: {supported})")]
    UnsupportedSchemaVersion { found: u32, supported: u32 },
    #[error("unsupported auth schema version {found} (supported: {supported})")]
    UnsupportedAuthSchemaVersion { found: u32, supported: u32 },
    #[error("invalid listen host '{host}': only 127.0.0.1 and localhost are allowed")]
    InvalidListenHost { host: String },
    #[error("invalid listen port: port must be greater than 0")]
    InvalidListenPort,
    #[error("invalid timeout: timeout_secs must be greater than 0")]
    InvalidTimeout,
    #[error("invalid default model: {reason}")]
    InvalidModel { reason: String },
    #[error(
        "invalid gemini.api_key: must contain only printable ASCII characters without internal spaces or control characters"
    )]
    InvalidApiKey,
    #[error(
        "invalid Antigravity CSRF token: must be 1..=512 printable ASCII bytes without whitespace or control characters"
    )]
    InvalidCsrfToken,
    #[error("invalid Antigravity LS endpoint: {reason}")]
    InvalidLsEndpoint { reason: String },
    #[error(
        "api_key cannot be set to the internal proxy placeholder ('agy-companion-local'); enter a real Gemini API key"
    )]
    PlaceholderApiKey,
    #[error("{field} {reason}")]
    InvalidField { field: String, reason: String },
    #[error("invalid upstream base URL: {reason}")]
    InvalidBaseUrl { reason: String },
    #[error("invalid path: {reason}")]
    InvalidPath { reason: String },
    #[error(transparent)]
    Fs(#[from] FsError),
    #[error("unable to determine user home directory")]
    HomeDirUnavailable,
    #[error("failed to parse config file '{path}' at line {line}, column {column}")]
    ParseConfig {
        path: PathBuf,
        line: usize,
        column: usize,
    },
    #[error("failed to serialize config: {reason}")]
    SerializeConfig { reason: String },
    #[error("failed to parse auth file '{path}' at line {line}, column {column}")]
    ParseAuth {
        path: PathBuf,
        line: usize,
        column: usize,
    },
    #[error("invalid auth credentials: {reason}")]
    InvalidAuthCredentials { reason: String },
    #[error("failed to serialize auth credentials: {reason}")]
    SerializeAuth { reason: String },
    #[error("usage database error at '{path}': {reason}")]
    UsageDatabase { path: PathBuf, reason: String },
    #[error("corrupted integration markers in '{path}': {reason}")]
    CorruptedMarkers { path: PathBuf, reason: String },
    #[error("unsupported shell '{shell}': expected zsh, bash, or fish")]
    InvalidShell { shell: String },
    #[error("I/O error at '{path}': {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum LoopbackHost {
    #[default]
    Ipv4Loopback,
    Localhost,
}

impl LoopbackHost {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Ipv4Loopback => "127.0.0.1",
            Self::Localhost => "localhost",
        }
    }
}

impl fmt::Display for LoopbackHost {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl FromStr for LoopbackHost {
    type Err = CoreError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "127.0.0.1" => Ok(Self::Ipv4Loopback),
            "localhost" => Ok(Self::Localhost),
            _ => Err(CoreError::InvalidListenHost {
                host: s.to_string(),
            }),
        }
    }
}

impl Serialize for LoopbackHost {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(self.as_str())
    }
}

impl<'de> Deserialize<'de> for LoopbackHost {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let raw = String::deserialize(deserializer)?;
        raw.parse::<Self>().map_err(serde::de::Error::custom)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct ProxyConfig {
    pub host: LoopbackHost,
    pub port: NonZeroU16,
}

impl Default for ProxyConfig {
    fn default() -> Self {
        Self {
            host: LoopbackHost::Ipv4Loopback,
            port: DEFAULT_LISTEN_PORT_NONZERO,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub enum CloudCodeUpstream {
    #[default]
    Auto,
    Fixed(String),
}

impl CloudCodeUpstream {
    pub fn parse(raw: &str) -> Result<Self, CoreError> {
        if raw == DEFAULT_CLOUDCODE_UPSTREAM_URL {
            return Ok(Self::Auto);
        }
        validate_upstream_url(raw)?;
        Ok(Self::Fixed(raw.to_string()))
    }

    pub const fn mode_label(&self) -> &'static str {
        match self {
            Self::Auto => "auto",
            Self::Fixed(_) => "fixed",
        }
    }

    pub fn as_str(&self) -> &str {
        match self {
            Self::Auto => DEFAULT_CLOUDCODE_UPSTREAM_URL,
            Self::Fixed(url) => url.as_str(),
        }
    }

    pub fn fixed_url(&self) -> Option<&str> {
        match self {
            Self::Auto => None,
            Self::Fixed(url) => Some(url.as_str()),
        }
    }

    pub const fn is_auto(&self) -> bool {
        matches!(self, Self::Auto)
    }
}

impl fmt::Display for CloudCodeUpstream {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl FromStr for CloudCodeUpstream {
    type Err = CoreError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Self::parse(s)
    }
}

impl Serialize for CloudCodeUpstream {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(self.as_str())
    }
}

impl<'de> Deserialize<'de> for CloudCodeUpstream {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let raw = String::deserialize(deserializer)?;
        Self::parse(&raw).map_err(serde::de::Error::custom)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct CloudCodeConfig {
    pub upstream: CloudCodeUpstream,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct GeminiConfig {
    pub base_url: String,
    pub default_model: String,
    #[serde(default = "default_enabled_gemini_models")]
    pub enabled_models: Vec<String>,
    pub timeout_secs: u64,
}

impl Default for GeminiConfig {
    fn default() -> Self {
        Self {
            base_url: DEFAULT_GEMINI_BASE_URL.to_string(),
            default_model: DEFAULT_GEMINI_MODEL.to_string(),
            enabled_models: default_enabled_gemini_models(),
            timeout_secs: DEFAULT_TIMEOUT_SECS,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct CodexConfig {
    pub base_url: String,
    pub default_model: String,
    #[serde(default = "default_enabled_codex_models")]
    pub enabled_models: Vec<String>,
    pub timeout_secs: u64,
}

impl Default for CodexConfig {
    fn default() -> Self {
        Self {
            base_url: DEFAULT_CODEX_BASE_URL.to_string(),
            default_model: DEFAULT_CODEX_MODEL.to_string(),
            enabled_models: default_enabled_codex_models(),
            timeout_secs: DEFAULT_TIMEOUT_SECS,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct ProvidersConfig {
    pub gemini: GeminiConfig,
    pub codex: CodexConfig,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct Config {
    pub schema_version: u32,
    pub proxy: ProxyConfig,
    pub cloudcode: CloudCodeConfig,
    pub providers: ProvidersConfig,
    pub usage_dir: Option<PathBuf>,
    #[serde(default)]
    pub launcher_auto_start: Option<bool>,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            schema_version: CURRENT_SCHEMA_VERSION,
            proxy: ProxyConfig::default(),
            cloudcode: CloudCodeConfig::default(),
            providers: ProvidersConfig::default(),
            usage_dir: None,
            launcher_auto_start: None,
        }
    }
}

#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ApiKeyPatch {
    Clear,
    Set(String),
}

impl fmt::Debug for ApiKeyPatch {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Clear => f.write_str("ApiKeyPatch::Clear"),
            Self::Set(raw) => f
                .debug_tuple("ApiKeyPatch::Set")
                .field(&api_key_hint(Some(raw)))
                .finish(),
        }
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ProxyConfigPatch {
    pub host: Option<String>,
    pub port: Option<u16>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct CloudCodeConfigPatch {
    pub upstream: Option<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct GeminiConfigPatch {
    pub base_url: Option<String>,
    pub default_model: Option<String>,
    pub enabled_models: Option<Vec<String>>,
    pub timeout_secs: Option<u64>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct CodexConfigPatch {
    pub base_url: Option<String>,
    pub default_model: Option<String>,
    pub enabled_models: Option<Vec<String>>,
    pub timeout_secs: Option<u64>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ProvidersConfigPatch {
    pub gemini: Option<GeminiConfigPatch>,
    pub codex: Option<CodexConfigPatch>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ConfigPatch {
    pub proxy: Option<ProxyConfigPatch>,
    pub cloudcode: Option<CloudCodeConfigPatch>,
    pub providers: Option<ProvidersConfigPatch>,
    pub launcher_auto_start: Option<Option<bool>>,
}

#[derive(Deserialize)]
#[serde(default)]
struct RawProxyConfig {
    host: String,
    port: u16,
}

impl Default for RawProxyConfig {
    fn default() -> Self {
        Self {
            host: DEFAULT_LISTEN_HOST.to_string(),
            port: DEFAULT_LISTEN_PORT,
        }
    }
}

#[derive(Deserialize)]
#[serde(default)]
struct RawCloudCodeConfig {
    upstream: String,
}

impl Default for RawCloudCodeConfig {
    fn default() -> Self {
        Self {
            upstream: DEFAULT_CLOUDCODE_UPSTREAM_URL.to_string(),
        }
    }
}

#[derive(Deserialize)]
#[serde(default)]
struct RawGeminiConfig {
    base_url: String,
    default_model: String,
    enabled_models: Vec<String>,
    timeout_secs: u64,
}

impl Default for RawGeminiConfig {
    fn default() -> Self {
        Self {
            base_url: DEFAULT_GEMINI_BASE_URL.to_string(),
            default_model: DEFAULT_GEMINI_MODEL.to_string(),
            enabled_models: default_enabled_gemini_models(),
            timeout_secs: DEFAULT_TIMEOUT_SECS,
        }
    }
}

#[derive(Deserialize)]
#[serde(default)]
struct RawCodexConfig {
    base_url: String,
    default_model: String,
    enabled_models: Vec<String>,
    timeout_secs: u64,
}

impl Default for RawCodexConfig {
    fn default() -> Self {
        Self {
            base_url: DEFAULT_CODEX_BASE_URL.to_string(),
            default_model: DEFAULT_CODEX_MODEL.to_string(),
            enabled_models: default_enabled_codex_models(),
            timeout_secs: DEFAULT_TIMEOUT_SECS,
        }
    }
}

#[derive(Default, Deserialize)]
#[serde(default)]
struct RawProvidersConfig {
    gemini: RawGeminiConfig,
    codex: RawCodexConfig,
}

#[derive(Deserialize)]
#[serde(default)]
struct RawConfig {
    schema_version: u32,
    proxy: RawProxyConfig,
    cloudcode: RawCloudCodeConfig,
    providers: RawProvidersConfig,
    usage_dir: Option<PathBuf>,
    launcher_auto_start: Option<bool>,
}

impl Default for RawConfig {
    fn default() -> Self {
        Self {
            schema_version: CURRENT_SCHEMA_VERSION,
            proxy: RawProxyConfig::default(),
            cloudcode: RawCloudCodeConfig::default(),
            providers: RawProvidersConfig::default(),
            usage_dir: None,
            launcher_auto_start: None,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[cfg_attr(not(test), allow(dead_code))]
pub(crate) enum TargetPlatform {
    Unix,
    Windows,
}

impl TargetPlatform {
    pub(crate) const fn current() -> Self {
        #[cfg(windows)]
        {
            Self::Windows
        }
        #[cfg(not(windows))]
        {
            Self::Unix
        }
    }
}

fn is_windows_drive_rooted(raw: &str, allow_forward_slash: bool) -> bool {
    let bytes = raw.as_bytes();
    bytes.len() >= 3
        && bytes[0].is_ascii_alphabetic()
        && bytes[1] == b':'
        && (bytes[2] == b'\\' || (allow_forward_slash && bytes[2] == b'/'))
}

fn is_windows_absolute(raw: &str) -> bool {
    if is_windows_drive_rooted(raw, true) {
        return true;
    }

    if let Some(verbatim_rest) = raw.strip_prefix(r"\\?\") {
        if verbatim_rest
            .as_bytes()
            .get(..4)
            .is_some_and(|prefix| prefix[..3].eq_ignore_ascii_case(b"UNC") && prefix[3] == b'\\')
        {
            let mut parts = verbatim_rest[4..].split('\\');
            let server = parts.next().unwrap_or("").trim();
            let share = parts.next().unwrap_or("").trim();
            return !server.is_empty()
                && !matches!(server, "." | "?")
                && !share.is_empty()
                && !matches!(share, "." | "?");
        }
        return is_windows_drive_rooted(verbatim_rest, false);
    }

    if let Some(unc_rest) = raw.strip_prefix(r"\\").or_else(|| raw.strip_prefix("//")) {
        let mut parts = unc_rest.split(['\\', '/']);
        let server = parts.next().unwrap_or("").trim();
        let share = parts.next().unwrap_or("").trim();
        return !server.is_empty()
            && !matches!(server, "." | "?")
            && !share.is_empty()
            && !matches!(share, "." | "?");
    }

    false
}

fn is_absolute_for_platform(raw: &str, platform: TargetPlatform) -> bool {
    match platform {
        TargetPlatform::Unix => raw.starts_with('/'),
        TargetPlatform::Windows => is_windows_absolute(raw),
    }
}

fn strip_optional_drive_prefix(segment: &str) -> &str {
    let bytes = segment.as_bytes();
    if bytes.len() >= 2 && bytes[0].is_ascii_alphabetic() && bytes[1] == b':' {
        &segment[2..]
    } else {
        segment
    }
}

fn has_parent_dir_traversal(raw: &str, platform: TargetPlatform) -> bool {
    match platform {
        TargetPlatform::Unix => raw.split('/').any(|seg| seg == ".."),
        TargetPlatform::Windows => raw
            .split(['/', '\\'])
            .any(|seg| strip_optional_drive_prefix(seg) == ".."),
    }
}

fn is_non_empty_dir(dir: &Path) -> bool {
    !dir.as_os_str().is_empty() && !dir.to_str().is_some_and(|s| s.trim().is_empty())
}

fn validate_platform_dir(dir: &Path, platform: TargetPlatform) -> Result<(), CoreError> {
    fs_safe::validate_path(dir)?;
    if dir
        .to_str()
        .is_some_and(|raw| has_parent_dir_traversal(raw, platform))
    {
        return Err(FsError::ParentDirTraversal.into());
    }
    Ok(())
}

pub(crate) fn resolve_xdg_base(
    xdg_env: Option<&str>,
    platform: TargetPlatform,
) -> Result<Option<PathBuf>, CoreError> {
    let Some(raw) = xdg_env else {
        return Ok(None);
    };

    if raw.trim().is_empty() {
        return Ok(None);
    }

    if raw.contains('\0') {
        return Err(FsError::ContainsNul.into());
    }

    if has_parent_dir_traversal(raw, platform) {
        return Err(FsError::ParentDirTraversal.into());
    }

    if !is_absolute_for_platform(raw, platform) {
        return Ok(None);
    }

    let candidate = PathBuf::from(raw);
    fs_safe::validate_path(&candidate)?;
    Ok(Some(candidate))
}

fn resolve_config_path_for_platform(
    env_override: Option<&str>,
    xdg_config_home: Option<&str>,
    platform_config_dir: Option<PathBuf>,
    home: Option<PathBuf>,
    platform: TargetPlatform,
) -> Result<PathBuf, CoreError> {
    if let Some(raw_override) = env_override {
        let trimmed = raw_override.trim();
        if !trimmed.is_empty() {
            if trimmed.contains('\0') {
                return Err(FsError::ContainsNul.into());
            }
            if has_parent_dir_traversal(trimmed, platform) {
                return Err(FsError::ParentDirTraversal.into());
            }
            let path = PathBuf::from(trimmed);
            fs_safe::validate_path(&path)?;
            return Ok(path);
        }
    }

    if let Some(xdg_base) = resolve_xdg_base(xdg_config_home, platform)? {
        return Ok(xdg_base.join("agy-companion").join("config.json"));
    }

    match platform {
        TargetPlatform::Windows => {
            if let Some(config_dir) = platform_config_dir.filter(|dir| is_non_empty_dir(dir)) {
                validate_platform_dir(&config_dir, platform)?;
                return Ok(config_dir.join("agy-companion").join("config.json"));
            }
        }
        TargetPlatform::Unix => {}
    }

    let home_dir = home
        .filter(|dir| is_non_empty_dir(dir))
        .ok_or(CoreError::HomeDirUnavailable)?;
    validate_platform_dir(&home_dir, platform)?;
    Ok(home_dir
        .join(".config")
        .join("agy-companion")
        .join("config.json"))
}

fn resolve_data_dir_for_platform(
    xdg_data_home: Option<&str>,
    platform_data_local_dir: Option<PathBuf>,
    home: Option<PathBuf>,
    platform: TargetPlatform,
) -> Result<PathBuf, CoreError> {
    if let Some(xdg_base) = resolve_xdg_base(xdg_data_home, platform)? {
        return Ok(xdg_base.join("agy-companion"));
    }

    match platform {
        TargetPlatform::Windows => {
            if let Some(data_local_dir) =
                platform_data_local_dir.filter(|dir| is_non_empty_dir(dir))
            {
                validate_platform_dir(&data_local_dir, platform)?;
                return Ok(data_local_dir.join("agy-companion"));
            }
        }
        TargetPlatform::Unix => {}
    }

    let home_dir = home
        .filter(|dir| is_non_empty_dir(dir))
        .ok_or(CoreError::HomeDirUnavailable)?;
    validate_platform_dir(&home_dir, platform)?;
    Ok(home_dir.join(".local").join("share").join("agy-companion"))
}

#[cfg(test)]
fn resolve_usage_dir_for_platform(
    xdg_data_home: Option<&str>,
    platform_data_local_dir: Option<PathBuf>,
    home: Option<PathBuf>,
    platform: TargetPlatform,
) -> Result<PathBuf, CoreError> {
    Ok(
        resolve_data_dir_for_platform(xdg_data_home, platform_data_local_dir, home, platform)?
            .join("usage"),
    )
}

fn resolve_plugins_dir_for_platform(
    home: Option<PathBuf>,
    platform: TargetPlatform,
) -> Result<PathBuf, CoreError> {
    let home_dir = home
        .filter(|dir| is_non_empty_dir(dir))
        .ok_or(CoreError::HomeDirUnavailable)?;
    validate_platform_dir(&home_dir, platform)?;
    Ok(home_dir.join(".gemini").join("config").join("plugins"))
}

pub(crate) fn read_optional_env(var_name: &str) -> Result<Option<String>, CoreError> {
    match std::env::var(var_name) {
        Ok(value) => Ok(Some(value)),
        Err(std::env::VarError::NotPresent) => Ok(None),
        Err(std::env::VarError::NotUnicode(_)) => Err(CoreError::InvalidPath {
            reason: format!("{var_name} contains invalid UTF-8"),
        }),
    }
}

#[cfg(test)]
pub(crate) fn resolve_config_path(
    env_override: Option<&str>,
    home: Option<PathBuf>,
) -> Result<PathBuf, CoreError> {
    resolve_config_path_for_platform(env_override, None, None, home, TargetPlatform::current())
}

#[cfg(test)]
pub(crate) fn resolve_data_dir(home: Option<PathBuf>) -> Result<PathBuf, CoreError> {
    resolve_data_dir_for_platform(None, None, home, TargetPlatform::current())
}

#[cfg(test)]
pub(crate) fn resolve_usage_dir(home: Option<PathBuf>) -> Result<PathBuf, CoreError> {
    resolve_usage_dir_for_platform(None, None, home, TargetPlatform::current())
}

#[cfg(test)]
pub(crate) fn resolve_plugins_dir(home: Option<PathBuf>) -> Result<PathBuf, CoreError> {
    resolve_plugins_dir_for_platform(home, TargetPlatform::current())
}

pub fn default_config_path() -> Result<PathBuf, CoreError> {
    let platform = TargetPlatform::current();
    let env_override = read_optional_env(CONFIG_PATH_ENV_VAR)?;
    if let Some(ref raw) = env_override
        && !raw.trim().is_empty()
    {
        return resolve_config_path_for_platform(Some(raw), None, None, None, platform);
    }

    let xdg_config_home = read_optional_env(XDG_CONFIG_HOME_ENV_VAR)?;
    #[cfg(windows)]
    let platform_config_dir = fs_safe::default_platform_config_dir();
    #[cfg(not(windows))]
    let platform_config_dir = None;

    resolve_config_path_for_platform(
        None,
        xdg_config_home.as_deref(),
        platform_config_dir,
        fs_safe::default_home_dir(),
        platform,
    )
}

pub fn default_data_dir() -> Result<PathBuf, CoreError> {
    let platform = TargetPlatform::current();
    let xdg_data_home = read_optional_env(XDG_DATA_HOME_ENV_VAR)?;
    #[cfg(windows)]
    let platform_data_local_dir = fs_safe::default_platform_data_local_dir();
    #[cfg(not(windows))]
    let platform_data_local_dir = None;

    resolve_data_dir_for_platform(
        xdg_data_home.as_deref(),
        platform_data_local_dir,
        fs_safe::default_home_dir(),
        platform,
    )
}

pub fn default_usage_dir() -> Result<PathBuf, CoreError> {
    Ok(default_data_dir()?.join("usage"))
}

pub fn default_plugins_dir() -> Result<PathBuf, CoreError> {
    resolve_plugins_dir_for_platform(fs_safe::default_home_dir(), TargetPlatform::current())
}

impl Config {
    pub fn validate(&self) -> Result<(), CoreError> {
        if self.schema_version != CURRENT_SCHEMA_VERSION {
            return Err(CoreError::UnsupportedSchemaVersion {
                found: self.schema_version,
                supported: CURRENT_SCHEMA_VERSION,
            });
        }
        if self.providers.gemini.timeout_secs == 0 || self.providers.codex.timeout_secs == 0 {
            return Err(CoreError::InvalidTimeout);
        }

        validate_gemini_model(&self.providers.gemini.default_model)?;
        for model in &self.providers.gemini.enabled_models {
            validate_gemini_model(model)?;
        }
        validate_upstream_url(&self.providers.gemini.base_url)?;

        validate_codex_model(&self.providers.codex.default_model)?;
        for model in &self.providers.codex.enabled_models {
            validate_codex_model(model)?;
        }
        validate_upstream_url(&self.providers.codex.base_url)?;

        if let CloudCodeUpstream::Fixed(ref url) = self.cloudcode.upstream {
            validate_upstream_url(url)?;
        }
        if let Some(ref usage_dir) = self.usage_dir {
            fs_safe::validate_path(usage_dir)?;
        }

        Ok(())
    }

    pub fn effective_usage_dir(&self) -> Result<PathBuf, CoreError> {
        if let Some(ref usage_dir) = self.usage_dir {
            fs_safe::validate_path(usage_dir)?;
            Ok(usage_dir.clone())
        } else {
            default_usage_dir()
        }
    }

    pub fn redacted_with_auth(
        &self,
        gemini_creds: Option<&GeminiCredentials>,
        codex_creds: Option<&CodexCredentials>,
    ) -> RedactedConfig {
        let hint = gemini_creds.map(|creds| creds.api_key.hint());
        let (codex_authenticated, codex_account_hint, codex_plan_type, codex_expires_at_ms) =
            match codex_creds {
                Some(creds) => (
                    true,
                    creds.account_hint(),
                    creds.plan_type.clone(),
                    Some(creds.expires_at_ms),
                ),
                None => (false, None, None, None),
            };
        RedactedConfig {
            schema_version: self.schema_version,
            proxy: self.proxy,
            cloudcode: self.cloudcode.clone(),
            providers: RedactedProvidersConfig {
                gemini: RedactedGeminiConfig {
                    base_url: self.providers.gemini.base_url.clone(),
                    default_model: self.providers.gemini.default_model.clone(),
                    enabled_models: self.providers.gemini.enabled_models.clone(),
                    timeout_secs: self.providers.gemini.timeout_secs,
                    api_key_configured: hint.is_some(),
                    api_key_hint: hint,
                },
                codex: RedactedCodexConfig {
                    base_url: self.providers.codex.base_url.clone(),
                    default_model: self.providers.codex.default_model.clone(),
                    enabled_models: self.providers.codex.enabled_models.clone(),
                    timeout_secs: self.providers.codex.timeout_secs,
                    authenticated: codex_authenticated,
                    account_hint: codex_account_hint,
                    plan_type: codex_plan_type,
                    expires_at_ms: codex_expires_at_ms,
                },
            },
            launcher_auto_start: self.launcher_auto_start,
        }
    }

    /// Parses and validates a JSON string from `path` using dual-layer raw deserialization so
    /// domain errors (`InvalidListenHost`, `InvalidListenPort`, `InvalidBaseUrl`)
    /// preserve exact non-leaking `CoreError` variants instead of collapsing into `ParseConfig`.
    pub fn from_json_str(raw: &str, path: &Path) -> Result<Config, CoreError> {
        let raw_cfg: RawConfig =
            serde_json::from_str(raw).map_err(|err| CoreError::ParseConfig {
                path: path.to_path_buf(),
                line: err.line(),
                column: err.column(),
            })?;

        if raw_cfg.schema_version != CURRENT_SCHEMA_VERSION {
            return Err(CoreError::UnsupportedSchemaVersion {
                found: raw_cfg.schema_version,
                supported: CURRENT_SCHEMA_VERSION,
            });
        }

        let host = raw_cfg.proxy.host.parse::<LoopbackHost>()?;
        let port = NonZeroU16::new(raw_cfg.proxy.port).ok_or(CoreError::InvalidListenPort)?;
        let upstream = CloudCodeUpstream::parse(&raw_cfg.cloudcode.upstream)?;
        let enabled_models =
            normalize_enabled_gemini_models(raw_cfg.providers.gemini.enabled_models)?;
        let codex_enabled_models =
            normalize_enabled_codex_models(raw_cfg.providers.codex.enabled_models)?;

        let config = Config {
            schema_version: raw_cfg.schema_version,
            proxy: ProxyConfig { host, port },
            cloudcode: CloudCodeConfig { upstream },
            providers: ProvidersConfig {
                gemini: GeminiConfig {
                    base_url: raw_cfg.providers.gemini.base_url,
                    default_model: raw_cfg.providers.gemini.default_model,
                    enabled_models,
                    timeout_secs: raw_cfg.providers.gemini.timeout_secs,
                },
                codex: CodexConfig {
                    base_url: raw_cfg.providers.codex.base_url,
                    default_model: raw_cfg.providers.codex.default_model,
                    enabled_models: codex_enabled_models,
                    timeout_secs: raw_cfg.providers.codex.timeout_secs,
                },
            },
            usage_dir: raw_cfg.usage_dir,
            launcher_auto_start: raw_cfg.launcher_auto_start,
        };
        config.validate()?;
        Ok(config)
    }

    /// Parses and validates raw UTF-8 JSON bytes from `path`.
    pub fn from_json_bytes(bytes: &[u8], path: &Path) -> Result<Config, CoreError> {
        let raw = std::str::from_utf8(bytes).map_err(|_| FsError::InvalidUtf8 {
            path: path.to_path_buf(),
        })?;
        Self::from_json_str(raw, path)
    }

    pub fn load_or_default(path: &Path) -> Result<Config, CoreError> {
        let Some((raw, _meta)) = fs_safe::read_regular_file_utf8_bounded(
            path,
            MAX_CONFIG_FILE_BYTES,
            SymlinkPolicy::Reject,
        )?
        else {
            return Ok(Config::default());
        };

        Self::from_json_str(&raw, path)
    }

    pub fn save_secure(&self, path: &Path) -> Result<(), CoreError> {
        self.validate()?;

        let mut payload =
            serde_json::to_string_pretty(self).map_err(|err| CoreError::SerializeConfig {
                reason: err.to_string(),
            })?;
        payload.push('\n');

        fs_safe::write_atomic_secure(path, payload.as_bytes())?;
        Ok(())
    }

    /// Transactionally applies a validated patch to `self`. On any validation error,
    /// `self` remains completely unmodified.
    pub fn apply_patch(&mut self, patch: ConfigPatch) -> Result<(), CoreError> {
        let mut next = self.clone();

        if let Some(providers) = patch.providers {
            if let Some(gemini) = providers.gemini {
                if let Some(raw_model) = gemini.default_model {
                    let trimmed = validate_trimmed_ascii_field(&raw_model, "default_model")?;
                    validate_gemini_model(&trimmed)?;
                    next.providers.gemini.default_model = trimmed;
                }
                if let Some(raw_models) = gemini.enabled_models {
                    let mut trimmed_models = Vec::with_capacity(raw_models.len());
                    for raw_model in raw_models {
                        let trimmed = validate_trimmed_ascii_field(&raw_model, "enabled_models")?;
                        trimmed_models.push(trimmed);
                    }
                    next.providers.gemini.enabled_models =
                        normalize_enabled_gemini_models(trimmed_models)?;
                }
                if let Some(raw_base_url) = gemini.base_url {
                    let trimmed = validate_trimmed_ascii_field(&raw_base_url, "base_url")?;
                    validate_upstream_url(&trimmed)?;
                    next.providers.gemini.base_url = trimmed;
                }
                if let Some(secs) = gemini.timeout_secs {
                    if secs == 0 {
                        return Err(CoreError::InvalidTimeout);
                    }
                    next.providers.gemini.timeout_secs = secs;
                }
            }
            if let Some(codex) = providers.codex {
                if let Some(raw_model) = codex.default_model {
                    let trimmed = validate_trimmed_ascii_field(&raw_model, "codex.default_model")?;
                    validate_codex_model(&trimmed)?;
                    next.providers.codex.default_model = trimmed;
                }
                if let Some(raw_models) = codex.enabled_models {
                    let mut trimmed_models = Vec::with_capacity(raw_models.len());
                    for raw_model in raw_models {
                        let trimmed =
                            validate_trimmed_ascii_field(&raw_model, "codex.enabled_models")?;
                        trimmed_models.push(trimmed);
                    }
                    next.providers.codex.enabled_models =
                        normalize_enabled_codex_models(trimmed_models)?;
                }
                if let Some(raw_base_url) = codex.base_url {
                    let trimmed = validate_trimmed_ascii_field(&raw_base_url, "codex.base_url")?;
                    validate_upstream_url(&trimmed)?;
                    next.providers.codex.base_url = trimmed;
                }
                if let Some(secs) = codex.timeout_secs {
                    if secs == 0 {
                        return Err(CoreError::InvalidTimeout);
                    }
                    next.providers.codex.timeout_secs = secs;
                }
            }
        }

        if let Some(cloudcode) = patch.cloudcode
            && let Some(raw_upstream) = cloudcode.upstream
        {
            let trimmed = validate_trimmed_ascii_field(&raw_upstream, "cloudcode.upstream")?;
            next.cloudcode.upstream = CloudCodeUpstream::parse(&trimmed)?;
        }

        if let Some(proxy) = patch.proxy {
            if let Some(raw_host) = proxy.host {
                let trimmed = validate_trimmed_ascii_field(&raw_host, "proxy.host")?;
                next.proxy.host = trimmed.parse::<LoopbackHost>()?;
            }
            if let Some(port) = proxy.port {
                next.proxy.port = NonZeroU16::new(port).ok_or(CoreError::InvalidListenPort)?;
            }
        }

        if let Some(auto_start) = patch.launcher_auto_start {
            next.launcher_auto_start = auto_start;
        }

        next.validate()?;
        *self = next;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use std::fs;

    use tempfile::TempDir;

    use super::*;

    #[test]
    fn defaults_to_schema_version_1_nested_layout() {
        let cfg = Config::default();
        assert_eq!(cfg.schema_version, 1);
        assert_eq!(cfg.proxy.host, LoopbackHost::Ipv4Loopback);
        assert_eq!(cfg.proxy.host.as_str(), "127.0.0.1");
        assert_eq!(cfg.proxy.port.get(), 24141);
        assert_eq!(cfg.cloudcode.upstream, CloudCodeUpstream::Auto);
        assert_eq!(
            cfg.providers.gemini.base_url,
            "https://generativelanguage.googleapis.com"
        );
        assert_eq!(cfg.providers.gemini.default_model, "gemini-4-hydrogen");
        assert_eq!(cfg.providers.gemini.timeout_secs, 900);
        assert!(cfg.usage_dir.is_none());
        assert!(cfg.validate().is_ok());

        let from_empty: Config = serde_json::from_str("{}").expect("empty object deserializes");
        assert_eq!(from_empty, cfg);
    }

    #[test]
    fn rejects_zero_timeout_bad_schema_and_bad_model_including_bug4_cases() {
        let mut cfg = Config::default();
        cfg.providers.gemini.timeout_secs = 0;
        assert!(matches!(cfg.validate(), Err(CoreError::InvalidTimeout)));

        for bad_schema in [0, 2] {
            let cfg = Config {
                schema_version: bad_schema,
                ..Default::default()
            };
            assert!(matches!(
                cfg.validate(),
                Err(CoreError::UnsupportedSchemaVersion {
                    found,
                    supported: 1
                }) if found == bad_schema
            ));
        }

        for bad_model in [
            "",
            "   ",
            "gemini-4-hydrogen?key=secret",
            "gemini-4-hydrogen#frag",
            "../evil",
            "models/gemini-4-hydrogen",
            "gemini-4-hydrogen\nX-Test: 1",
            "gemini-3.7-flash",
            "gemini-3-pro",
            "gemini-2.5-pro",
            "gemini-2.5-flash",
            "gemini-2.0-flash",
            "gemini-1.5-pro",
            "gemini-3",
            "gemini-4",
            "gemini-5",
            "gemini-3-",
            "gemini-4.",
            "gemini-5-",
            "gemini-5.",
            "gemini-5pro",
            "gemma3:4b",
            "gpt-4o",
        ] {
            let mut cfg = Config::default();
            cfg.providers.gemini.default_model = bad_model.to_string();
            assert!(
                matches!(cfg.validate(), Err(CoreError::InvalidModel { .. })),
                "expected InvalidModel for '{bad_model}'"
            );
        }

        for good_model in [
            "gemini-4-hydrogen",
            "gemini-4-argon",
            "gemini-4-neon",
            "gemini-4-carbon",
            "gemini-4.1-pro",
            "gemini-5-pro",
            "gemini-5.0-flash",
            "gemini-3.8-flash",
            "gemini-3.9-pro",
            "gemini-3.10-flash",
        ] {
            let mut cfg = Config::default();
            cfg.providers.gemini.default_model = good_model.to_string();
            assert!(
                cfg.validate().is_ok(),
                "expected valid default_model '{good_model}'"
            );
        }
    }

    #[test]
    fn rejects_path_traversal_in_config_and_usage_paths() {
        for bad in ["../evil.json", "/tmp/../etc/passwd", "", "   ", "bad\0path"] {
            assert!(fs_safe::validate_path(Path::new(bad)).is_err());
        }

        let mut cfg = Config {
            usage_dir: Some(PathBuf::from("../usage")),
            ..Default::default()
        };
        assert!(matches!(
            cfg.validate(),
            Err(CoreError::Fs(FsError::ParentDirTraversal))
        ));
        assert!(matches!(
            cfg.effective_usage_dir(),
            Err(CoreError::Fs(FsError::ParentDirTraversal))
        ));

        cfg.usage_dir = Some(PathBuf::from("/tmp/custom-usage"));
        assert_eq!(
            cfg.effective_usage_dir().expect("valid override"),
            PathBuf::from("/tmp/custom-usage")
        );
    }

    #[test]
    fn save_secure_writes_atomic_0600_and_round_trips_nested_json() {
        let tmp = TempDir::new().expect("temp dir");
        let nested_dir = tmp.path().join("nested").join("agy-companion");
        let cfg_path = nested_dir.join("config.json");

        let cfg = Config {
            proxy: ProxyConfig {
                host: LoopbackHost::Localhost,
                port: NonZeroU16::new(4242).unwrap(),
            },
            providers: ProvidersConfig {
                gemini: GeminiConfig {
                    default_model: "gemini-4-neon".to_string(),
                    ..Default::default()
                },
                ..Default::default()
            },
            ..Default::default()
        };

        cfg.save_secure(&cfg_path).expect("save_secure succeeds");

        let loaded = Config::load_or_default(&cfg_path).expect("load_or_default succeeds");
        assert_eq!(loaded, cfg);

        // Verify exact nested JSON shape on disk and zero api_key field in config.json
        let raw_text = fs::read_to_string(&cfg_path).unwrap();
        assert!(
            !raw_text.contains("api_key"),
            "config.json must never persist api_key: {raw_text}"
        );
        let raw_val: serde_json::Value = serde_json::from_str(&raw_text).unwrap();
        assert_eq!(raw_val["schema_version"], 1);
        assert_eq!(raw_val["proxy"]["host"], "localhost");
        assert_eq!(raw_val["proxy"]["port"], 4242);
        assert_eq!(raw_val["cloudcode"]["upstream"], "auto");
        assert_eq!(
            raw_val["providers"]["gemini"]["default_model"],
            "gemini-4-neon"
        );
        assert!(raw_val["providers"]["gemini"].get("api_key").is_none());

        let sibling_temps: Vec<_> = fs::read_dir(&nested_dir)
            .expect("read_dir")
            .filter_map(|entry| entry.ok())
            .filter(|entry| entry.file_name().to_string_lossy().contains(".tmp."))
            .collect();
        assert!(sibling_temps.is_empty());

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;

            let file_mode = fs::metadata(&cfg_path).unwrap().permissions().mode() & 0o777;
            assert_eq!(file_mode, 0o600);
            let dir_mode = fs::metadata(&nested_dir).unwrap().permissions().mode() & 0o777;
            assert_eq!(dir_mode, 0o700);

            fs::set_permissions(&cfg_path, fs::Permissions::from_mode(0o644)).expect("set 0644");
            cfg.save_secure(&cfg_path).expect("overwrite save_secure");
            let tightened = fs::metadata(&cfg_path).unwrap().permissions().mode() & 0o777;
            assert_eq!(tightened, 0o600);
        }

        // Invalid manually constructed Fixed upstream fails save_secure without writing
        let invalid = Config {
            cloudcode: CloudCodeConfig {
                upstream: CloudCodeUpstream::Fixed("http://evil.example.com".to_string()),
            },
            ..Default::default()
        };
        let invalid_path = tmp.path().join("should_not_exist.json");
        assert!(matches!(
            invalid.save_secure(&invalid_path),
            Err(CoreError::InvalidBaseUrl { .. })
        ));
        assert!(!invalid_path.exists());
    }

    #[test]
    fn load_or_default_preserves_domain_errors_and_ignores_legacy_api_key_field() {
        let tmp = TempDir::new().expect("temp dir");
        let missing = tmp.path().join("does-not-exist.json");
        let loaded = Config::load_or_default(&missing).expect("missing returns default");
        assert_eq!(loaded, Config::default());

        let invalid_host_path = tmp.path().join("bad-host.json");
        fs::write(&invalid_host_path, r#"{"proxy":{"host":"0.0.0.0"}}"#).expect("write bad host");
        let err = Config::load_or_default(&invalid_host_path).expect_err("rejects 0.0.0.0");
        assert!(matches!(err, CoreError::InvalidListenHost { .. }));

        let zero_port_path = tmp.path().join("zero-port.json");
        fs::write(&zero_port_path, r#"{"proxy":{"port":0}}"#).expect("write zero port");
        let err = Config::load_or_default(&zero_port_path).expect_err("rejects port 0");
        assert!(matches!(err, CoreError::InvalidListenPort));

        // Legacy config.json containing providers.gemini.api_key loads cleanly (field ignored)
        // and omits api_key completely when re-saved.
        let legacy_path = tmp.path().join("legacy-key.json");
        fs::write(
            &legacy_path,
            r#"{"schema_version":1,"providers":{"gemini":{"api_key":"AIzaSyLegacyIgnored01","default_model":"gemini-4-neon"}}}"#,
        )
        .expect("write legacy config");
        let loaded = Config::load_or_default(&legacy_path).expect("legacy api_key field ignored");
        assert_eq!(loaded.providers.gemini.default_model, "gemini-4-neon");
        let redacted = loaded.redacted_with_auth(None, None);
        assert!(!redacted.providers.gemini.api_key_configured);
        assert_eq!(redacted.providers.gemini.api_key_hint, None);

        loaded
            .save_secure(&legacy_path)
            .expect("resave scrubs legacy field");
        let resaved_raw = fs::read_to_string(&legacy_path).unwrap();
        assert!(!resaved_raw.contains("api_key"));
        assert!(!resaved_raw.contains("AIzaSyLegacyIgnored01"));

        let corrupt_path = tmp.path().join("corrupt.json");
        fs::write(&corrupt_path, "{ not valid json").expect("write corrupt");
        let err = Config::load_or_default(&corrupt_path).expect_err("rejects corrupt json");
        assert!(matches!(err, CoreError::ParseConfig { .. }));

        // from_json_bytes invalid UTF-8
        assert!(matches!(
            Config::from_json_bytes(&[0xff, 0xfe], Path::new("bad.json")),
            Err(CoreError::Fs(FsError::InvalidUtf8 { .. }))
        ));
    }

    #[test]
    fn debug_redacts_api_key_patch_and_parse_error_leaks_nothing() {
        let secret = "AIzaSyTopSecretKey9999";

        let patch_debug = format!("{:?}", ApiKeyPatch::Set(secret.to_string()));
        assert!(!patch_debug.contains(secret));
        assert!(patch_debug.contains("AIza...9999"));

        let tmp = TempDir::new().expect("temp dir");
        let corrupt_path = tmp.path().join("corrupt-secret.json");
        fs::write(
            &corrupt_path,
            format!(r#"{{"providers": {{"gemini": {{"api_key": "{secret}", broken"#),
        )
        .expect("write corrupt file");
        let err = Config::load_or_default(&corrupt_path).expect_err("corrupt file fails");
        assert!(matches!(err, CoreError::ParseConfig { .. }));
        assert!(!format!("{err}").contains(secret));
        assert!(!format!("{err:?}").contains(secret));
    }

    #[test]
    fn apply_patch_is_transactional_trims_host_and_rolls_back_on_error() {
        let mut cfg = Config::default();

        // Successful patch with padded host, model, base_url, upstream, and timeout
        cfg.apply_patch(ConfigPatch {
            proxy: Some(ProxyConfigPatch {
                host: Some("  localhost  ".to_string()),
                port: Some(25000),
            }),
            cloudcode: Some(CloudCodeConfigPatch {
                upstream: Some("  https://daily-cloudcode-pa.googleapis.com  ".to_string()),
            }),
            providers: Some(ProvidersConfigPatch {
                gemini: Some(GeminiConfigPatch {
                    base_url: Some(
                        "  https://generativelanguage.googleapis.com/v1beta  ".to_string(),
                    ),
                    default_model: Some("  gemini-5-pro  ".to_string()),
                    enabled_models: None,
                    timeout_secs: Some(600),
                }),
                codex: None,
            }),
            launcher_auto_start: None,
        })
        .expect("valid patch succeeds");

        assert_eq!(cfg.proxy.host, LoopbackHost::Localhost);
        assert_eq!(cfg.proxy.port.get(), 25000);
        assert_eq!(
            cfg.cloudcode.upstream,
            CloudCodeUpstream::Fixed("https://daily-cloudcode-pa.googleapis.com".to_string())
        );
        assert_eq!(cfg.providers.gemini.default_model, "gemini-5-pro");
        assert_eq!(cfg.providers.gemini.timeout_secs, 600);

        // Failing patch leaves `cfg` completely unchanged (transactional)
        let before = cfg.clone();
        let err = cfg
            .apply_patch(ConfigPatch {
                proxy: Some(ProxyConfigPatch {
                    host: Some("127.0.0.1".to_string()),
                    port: Some(26000),
                }),
                cloudcode: None,
                providers: Some(ProvidersConfigPatch {
                    gemini: Some(GeminiConfigPatch {
                        timeout_secs: Some(0),
                        ..Default::default()
                    }),
                    codex: None,
                }),
                launcher_auto_start: None,
            })
            .expect_err("zero timeout must fail");
        assert!(matches!(err, CoreError::InvalidTimeout));
        assert_eq!(cfg, before, "failed patch must not partially mutate Config");
    }

    #[test]
    fn save_secure_cleans_up_temp_file_when_rename_fails() {
        let tmp = TempDir::new().expect("temp dir");
        let target_is_dir = tmp.path().join("config.json");
        fs::create_dir(&target_is_dir).expect("create directory at target path");

        let cfg = Config::default();
        let err = cfg
            .save_secure(&target_is_dir)
            .expect_err("renaming file over directory must fail");
        assert!(matches!(err, CoreError::Fs(FsError::Io { .. })));

        let leftover_temps: Vec<_> = fs::read_dir(tmp.path())
            .expect("read_dir")
            .filter_map(|entry| entry.ok())
            .filter(|entry| entry.file_name().to_string_lossy().contains(".tmp."))
            .collect();
        assert!(leftover_temps.is_empty());
    }

    #[cfg(unix)]
    #[test]
    fn load_and_save_reject_symlink_config_files_without_mutating_target() {
        use std::os::unix::fs::symlink;

        let tmp = TempDir::new().expect("temp dir");

        // (i) Symlink to a valid config file.
        let target = tmp.path().join("victim.json");
        let mut target_cfg = Config::default();
        target_cfg.providers.gemini.default_model = "gemini-4-victim-sentinel".to_string();
        target_cfg.save_secure(&target).expect("write target");
        let original_target_bytes = fs::read(&target).expect("read original");

        let sym_to_file = tmp.path().join("sym_config.json");
        symlink(&target, &sym_to_file).expect("symlink to file");

        // (ii) Dangling symlink.
        let dangling_target = tmp.path().join("does_not_exist.json");
        let sym_dangling = tmp.path().join("dangling_config.json");
        symlink(&dangling_target, &sym_dangling).expect("dangling symlink");

        // (iii) Trailing-slash symlink to file and trailing-slash symlink to dir.
        let sym_trailing = PathBuf::from(format!("{}/", sym_to_file.display()));
        let real_dir = tmp.path().join("real_dir");
        fs::create_dir(&real_dir).expect("create real dir");
        let sym_to_dir = tmp.path().join("sym_dir");
        symlink(&real_dir, &sym_to_dir).expect("create symlink to dir");
        let sym_dir_trailing = PathBuf::from(format!("{}/", sym_to_dir.display()));

        let mut updated_cfg = Config::default();
        updated_cfg.providers.gemini.default_model = "gemini-3.8-flash".to_string();

        for (label, path) in [
            ("symlink to valid config", &sym_to_file),
            ("dangling symlink", &sym_dangling),
            ("trailing-slash symlink to file", &sym_trailing),
            ("trailing-slash symlink to dir", &sym_dir_trailing),
        ] {
            let load_err = Config::load_or_default(path)
                .expect_err(&format!("load_or_default must reject {label}"));
            assert!(
                matches!(&load_err, CoreError::Fs(FsError::SymlinkRejected { .. }))
                    && load_err.to_string().contains("symbolic link"),
                "load_or_default({label}) returned unexpected error: {load_err:?}"
            );
            let save_err = updated_cfg
                .save_secure(path)
                .expect_err(&format!("save_secure must reject {label}"));
            assert!(
                matches!(&save_err, CoreError::Fs(FsError::SymlinkRejected { .. }))
                    && save_err.to_string().contains("symbolic link"),
                "save_secure({label}) returned unexpected error: {save_err:?}"
            );
        }

        // Target file must remain byte-for-byte untouched, symlinks untouched, zero .tmp. files created.
        assert_eq!(fs::read(&target).unwrap(), original_target_bytes);
        assert!(
            fs::symlink_metadata(&sym_to_file)
                .expect("lstat sym_to_file")
                .file_type()
                .is_symlink(),
            "sym_to_file must remain a symlink"
        );
        assert!(
            fs::symlink_metadata(&sym_dangling)
                .expect("lstat sym_dangling")
                .file_type()
                .is_symlink(),
            "sym_dangling must remain a symlink"
        );
        assert!(
            !dangling_target.exists(),
            "dangling target must not be created"
        );
        assert!(
            fs::symlink_metadata(&sym_to_dir)
                .expect("lstat sym_to_dir")
                .file_type()
                .is_symlink(),
            "sym_to_dir must remain a symlink"
        );

        let leftover_temps: Vec<_> = fs::read_dir(tmp.path())
            .expect("read_dir")
            .filter_map(|entry| entry.ok())
            .filter(|entry| entry.file_name().to_string_lossy().contains(".tmp."))
            .collect();
        assert!(
            leftover_temps.is_empty(),
            "no .tmp. files may be created when rejecting symlinks: {leftover_temps:?}"
        );
    }

    #[test]
    fn load_or_default_rejects_oversized_file_directory_and_fifo_without_blocking() {
        let tmp = TempDir::new().expect("temp dir");

        let oversized_path = tmp.path().join("oversized.json");
        let mut oversized = br#"{"schema_version":1}"#.to_vec();
        oversized.resize(1024 * 1024 + 1, b' ');
        fs::write(&oversized_path, oversized).expect("write oversized");
        assert!(matches!(
            Config::load_or_default(&oversized_path),
            Err(CoreError::Fs(FsError::SizeLimitExceeded { .. }))
        ));

        let dir_path = tmp.path().join("config_dir");
        fs::create_dir(&dir_path).expect("create dir");
        assert!(matches!(
            Config::load_or_default(&dir_path),
            Err(CoreError::Fs(FsError::NotRegularFile { .. }))
        ));

        #[cfg(unix)]
        {
            let fifo_path = tmp.path().join("config.fifo");
            let status = std::process::Command::new("mkfifo")
                .arg(&fifo_path)
                .status()
                .expect("mkfifo");
            assert!(status.success());

            let (tx, rx) = std::sync::mpsc::channel();
            let fifo_clone = fifo_path.clone();
            let handle = std::thread::spawn(move || {
                let _ = tx.send(Config::load_or_default(&fifo_clone));
            });
            let res = rx
                .recv_timeout(std::time::Duration::from_secs(1))
                .expect("load_or_default hung on FIFO");
            handle.join().expect("join");
            assert!(matches!(
                res,
                Err(CoreError::Fs(FsError::NotRegularFile { .. }))
            ));
        }
    }

    #[test]
    fn cloudcode_upstream_url_defaults_deserializes_compatibly_and_validates_s3() {
        let cfg = Config::default();
        assert_eq!(cfg.cloudcode.upstream, CloudCodeUpstream::Auto);
        assert_eq!(
            cfg.cloudcode.upstream.as_str(),
            DEFAULT_CLOUDCODE_UPSTREAM_URL
        );
        assert_eq!(
            cfg.redacted_with_auth(None, None).cloudcode.upstream,
            CloudCodeUpstream::Auto
        );

        // Config JSON omitting cloudcode deserializes to Auto and validates cleanly.
        let omitted = Config::from_json_str(
            r#"{
                "schema_version": 1,
                "proxy": { "host": "127.0.0.1", "port": 24141 },
                "providers": {
                    "gemini": {
                        "base_url": "https://generativelanguage.googleapis.com",
                        "default_model": "gemini-4-hydrogen",
                        "timeout_secs": 900
                    }
                }
            }"#,
            Path::new("omitted-cloudcode.json"),
        )
        .expect("config omitting cloudcode must deserialize");
        assert_eq!(omitted.cloudcode.upstream, CloudCodeUpstream::Auto);
        omitted.validate().expect("omitted cloudcode must validate");

        // "auto", loopback http, and https prod URLs are accepted.
        for ok_url in [
            "auto",
            "https://daily-cloudcode-pa.googleapis.com",
            "https://cloudcode-pa.googleapis.com",
            "http://127.0.0.1:49999",
            "http://localhost:49999",
        ] {
            let parsed = CloudCodeUpstream::parse(ok_url)
                .unwrap_or_else(|e| panic!("expected {ok_url} to parse, got {e}"));
            let c = Config {
                cloudcode: CloudCodeConfig { upstream: parsed },
                ..Default::default()
            };
            c.validate()
                .unwrap_or_else(|e| panic!("expected {ok_url} to be valid, got {e}"));
        }

        // Non-loopback http, embedded creds, traversal, ftp, empty, and manually constructed
        // Fixed("auto") / Fixed("  AUTO  ") are rejected by Config::validate (S3).
        for bad_url in [
            "http://daily-cloudcode-pa.googleapis.com",
            "https://user:pass@cloudcode-pa.googleapis.com",
            "https://cloudcode-pa.googleapis.com/../secret",
            "ftp://cloudcode-pa.googleapis.com",
            "",
            "auto",
            "  AUTO  ",
        ] {
            let c = Config {
                cloudcode: CloudCodeConfig {
                    upstream: CloudCodeUpstream::Fixed(bad_url.to_string()),
                },
                ..Default::default()
            };
            assert!(
                matches!(c.validate(), Err(CoreError::InvalidBaseUrl { .. })),
                "expected InvalidBaseUrl for Fixed('{bad_url}')"
            );
        }
    }

    #[test]
    fn resolves_unix_xdg_config_and_usage_precedence() {
        assert_eq!(XDG_CONFIG_HOME_ENV_VAR, "XDG_CONFIG_HOME");
        assert_eq!(XDG_DATA_HOME_ENV_VAR, "XDG_DATA_HOME");

        let home = PathBuf::from("/Users/test");
        let macos_app_support = PathBuf::from("/Users/test/Library/Application Support");

        // Direct wrapper sanity
        assert_eq!(
            resolve_config_path(None, Some(home.clone())).unwrap(),
            PathBuf::from("/Users/test/.config/agy-companion/config.json")
        );
        assert_eq!(
            resolve_config_path(Some("/custom/cfg.json"), Some(home.clone())).unwrap(),
            PathBuf::from("/custom/cfg.json")
        );
        assert!(matches!(
            resolve_config_path(Some("../escape.json"), Some(home.clone())),
            Err(CoreError::Fs(FsError::ParentDirTraversal))
        ));
        assert!(matches!(
            resolve_config_path(None, None),
            Err(CoreError::HomeDirUnavailable)
        ));

        // AGY_COMPANION_CONFIG overrides XDG_CONFIG_HOME and short-circuits invalid XDG.
        let env_wins = resolve_config_path_for_platform(
            Some("/custom/cfg.json"),
            Some("/tmp/../bad"),
            Some(macos_app_support.clone()),
            None,
            TargetPlatform::Unix,
        )
        .expect("env override short-circuits XDG");
        assert_eq!(env_wins, PathBuf::from("/custom/cfg.json"));

        // Invalid AGY_COMPANION_CONFIG fails closed and never falls through to XDG.
        let invalid_env = resolve_config_path_for_platform(
            Some("../escape.json"),
            Some("/xdg/config"),
            None,
            Some(home.clone()),
            TargetPlatform::Unix,
        )
        .expect_err("invalid env override must fail closed");
        assert!(matches!(
            invalid_env,
            CoreError::Fs(FsError::ParentDirTraversal)
        ));

        // Blank/whitespace AGY_COMPANION_CONFIG falls through to valid XDG_CONFIG_HOME.
        for blank in ["", "   ", "\t\n"] {
            let from_xdg = resolve_config_path_for_platform(
                Some(blank),
                Some("/xdg/config"),
                Some(macos_app_support.clone()),
                None,
                TargetPlatform::Unix,
            )
            .expect("blank env override falls through to XDG");
            assert_eq!(
                from_xdg,
                PathBuf::from("/xdg/config")
                    .join("agy-companion")
                    .join("config.json")
            );
        }

        // Valid XDG_CONFIG_HOME with trailing slash, spaces, and benign dotted names.
        for xdg in [
            "/xdg/cfg/",
            "/Users/Jane Doe/xdg config",
            "/tmp/my..dir",
            "/tmp/...",
        ] {
            let resolved = resolve_config_path_for_platform(
                None,
                Some(xdg),
                Some(macos_app_support.clone()),
                None,
                TargetPlatform::Unix,
            )
            .expect("valid XDG_CONFIG_HOME resolves without home");
            assert_eq!(
                resolved,
                PathBuf::from(xdg).join("agy-companion").join("config.json")
            );
        }

        // Unset XDG on Unix ignores platform_config_dir (no macOS Library/Application Support drift).
        let unix_default_cfg = resolve_config_path_for_platform(
            None,
            None,
            Some(macos_app_support.clone()),
            Some(home.clone()),
            TargetPlatform::Unix,
        )
        .expect("unix default config path");
        assert_eq!(
            unix_default_cfg,
            home.join(".config")
                .join("agy-companion")
                .join("config.json")
        );
        assert!(
            !unix_default_cfg
                .to_string_lossy()
                .contains("Library/Application Support")
        );

        // XDG_DATA_HOME resolves without home; unset XDG ignores platform_data_local_dir.
        let xdg_usage = resolve_usage_dir_for_platform(
            Some("/xdg/data"),
            Some(macos_app_support.clone()),
            None,
            TargetPlatform::Unix,
        )
        .expect("valid XDG_DATA_HOME resolves without home");
        assert_eq!(
            xdg_usage,
            PathBuf::from("/xdg/data")
                .join("agy-companion")
                .join("usage")
        );

        let unix_default_usage = resolve_usage_dir_for_platform(
            None,
            Some(macos_app_support),
            Some(home.clone()),
            TargetPlatform::Unix,
        )
        .expect("unix default usage dir");
        assert_eq!(
            unix_default_usage,
            home.join(".local")
                .join("share")
                .join("agy-companion")
                .join("usage")
        );
        assert!(
            !unix_default_usage
                .to_string_lossy()
                .contains("Library/Application Support")
        );
    }

    #[test]
    fn ignores_empty_and_relative_xdg_paths_and_rejects_traversal() {
        let home = PathBuf::from("/Users/test");
        let expected_cfg = home
            .join(".config")
            .join("agy-companion")
            .join("config.json");
        let expected_usage = home
            .join(".local")
            .join("share")
            .join("agy-companion")
            .join("usage");

        // Unset, empty, whitespace, benign relative, leading-space, or Windows paths on Unix
        // are ignored per XDG spec and fall back to ~/.config & ~/.local/share.
        for ignored in [
            "",
            "   ",
            "\t\n",
            "relative/dir",
            "./xdg",
            " /tmp/xdg",
            r"C:\Users\alice\cfg",
        ] {
            let cfg = resolve_config_path_for_platform(
                None,
                Some(ignored),
                None,
                Some(home.clone()),
                TargetPlatform::Unix,
            )
            .unwrap_or_else(|err| {
                panic!("expected fallback for XDG_CONFIG_HOME='{ignored}': {err:?}")
            });
            assert_eq!(
                cfg, expected_cfg,
                "config fallback mismatch for '{ignored}'"
            );

            let usage = resolve_usage_dir_for_platform(
                Some(ignored),
                None,
                Some(home.clone()),
                TargetPlatform::Unix,
            )
            .unwrap_or_else(|err| {
                panic!("expected fallback for XDG_DATA_HOME='{ignored}': {err:?}")
            });
            assert_eq!(
                usage, expected_usage,
                "usage fallback mismatch for '{ignored}'"
            );
        }

        // '..' traversal fails closed on both absolute and relative XDG inputs.
        for bad in [
            "/tmp/xdg/../evil",
            "/var/..",
            "/../etc",
            "../escape",
            "..",
            "foo/../bar",
        ] {
            let cfg_err = resolve_config_path_for_platform(
                None,
                Some(bad),
                None,
                Some(home.clone()),
                TargetPlatform::Unix,
            )
            .expect_err(&format!(
                "expected ParentDirTraversal for XDG_CONFIG_HOME='{bad}'"
            ));
            assert!(
                matches!(cfg_err, CoreError::Fs(FsError::ParentDirTraversal)),
                "unexpected error for XDG_CONFIG_HOME='{bad}': {cfg_err:?}"
            );

            let usage_err = resolve_usage_dir_for_platform(
                Some(bad),
                None,
                Some(home.clone()),
                TargetPlatform::Unix,
            )
            .expect_err(&format!(
                "expected ParentDirTraversal for XDG_DATA_HOME='{bad}'"
            ));
            assert!(
                matches!(usage_err, CoreError::Fs(FsError::ParentDirTraversal)),
                "unexpected error for XDG_DATA_HOME='{bad}': {usage_err:?}"
            );
        }

        // NUL bytes fail closed on both absolute and relative XDG inputs.
        for bad_nul in ["/tmp/xdg\0evil", "rel\0evil"] {
            let cfg_err = resolve_config_path_for_platform(
                None,
                Some(bad_nul),
                None,
                Some(home.clone()),
                TargetPlatform::Unix,
            )
            .expect_err(&format!(
                "expected ContainsNul for XDG_CONFIG_HOME='{bad_nul:?}'"
            ));
            assert!(
                matches!(cfg_err, CoreError::Fs(FsError::ContainsNul)),
                "unexpected error for XDG_CONFIG_HOME='{bad_nul:?}': {cfg_err:?}"
            );

            let usage_err = resolve_usage_dir_for_platform(
                Some(bad_nul),
                None,
                Some(home.clone()),
                TargetPlatform::Unix,
            )
            .expect_err(&format!(
                "expected ContainsNul for XDG_DATA_HOME='{bad_nul:?}'"
            ));
            assert!(
                matches!(usage_err, CoreError::Fs(FsError::ContainsNul)),
                "unexpected error for XDG_DATA_HOME='{bad_nul:?}': {usage_err:?}"
            );
        }

        // HomeDirUnavailable when XDG is unset/ignored and home is None.
        assert!(matches!(
            resolve_config_path_for_platform(None, None, None, None, TargetPlatform::Unix),
            Err(CoreError::HomeDirUnavailable)
        ));
        assert!(matches!(
            resolve_config_path_for_platform(
                None,
                Some("relative/cfg"),
                None,
                None,
                TargetPlatform::Unix
            ),
            Err(CoreError::HomeDirUnavailable)
        ));
        assert!(matches!(
            resolve_usage_dir_for_platform(None, None, None, TargetPlatform::Unix),
            Err(CoreError::HomeDirUnavailable)
        ));
        assert!(matches!(
            resolve_usage_dir_for_platform(Some("./rel"), None, None, TargetPlatform::Unix),
            Err(CoreError::HomeDirUnavailable)
        ));
    }

    #[test]
    fn resolves_windows_known_folders_xdg_and_userprofile_fallback() {
        let appdata = PathBuf::from(r"C:\Users\alice\AppData\Roaming");
        let localappdata = PathBuf::from(r"C:\Users\alice\AppData\Local");
        let userprofile = PathBuf::from(r"C:\Users\alice");

        // Known folders (%APPDATA% and %LOCALAPPDATA%) resolve even when home is None.
        let win_cfg = resolve_config_path_for_platform(
            None,
            None,
            Some(appdata.clone()),
            None,
            TargetPlatform::Windows,
        )
        .expect("Windows APPDATA config path");
        assert_eq!(win_cfg, appdata.join("agy-companion").join("config.json"));

        let win_usage = resolve_usage_dir_for_platform(
            None,
            Some(localappdata.clone()),
            None,
            TargetPlatform::Windows,
        )
        .expect("Windows LOCALAPPDATA usage dir");
        assert_eq!(win_usage, localappdata.join("agy-companion").join("usage"));

        // Fallback to %USERPROFILE% when platform Known Folder is None or empty/blank.
        for missing_dir in [None, Some(PathBuf::from("")), Some(PathBuf::from("   "))] {
            let fallback_cfg = resolve_config_path_for_platform(
                None,
                None,
                missing_dir.clone(),
                Some(userprofile.clone()),
                TargetPlatform::Windows,
            )
            .expect("Windows USERPROFILE config fallback");
            assert_eq!(
                fallback_cfg,
                userprofile
                    .join(".config")
                    .join("agy-companion")
                    .join("config.json")
            );

            let fallback_usage = resolve_usage_dir_for_platform(
                None,
                missing_dir,
                Some(userprofile.clone()),
                TargetPlatform::Windows,
            )
            .expect("Windows USERPROFILE usage fallback");
            assert_eq!(
                fallback_usage,
                userprofile
                    .join(".local")
                    .join("share")
                    .join("agy-companion")
                    .join("usage")
            );
        }

        // HomeDirUnavailable when both Known Folder and home are None.
        assert!(matches!(
            resolve_config_path_for_platform(None, None, None, None, TargetPlatform::Windows),
            Err(CoreError::HomeDirUnavailable)
        ));
        assert!(matches!(
            resolve_usage_dir_for_platform(None, None, None, TargetPlatform::Windows),
            Err(CoreError::HomeDirUnavailable)
        ));

        // AGY_COMPANION_CONFIG overrides Windows XDG & %APPDATA% (and short-circuits bad XDG).
        let env_cfg = resolve_config_path_for_platform(
            Some(r"D:\work\agy-config.json"),
            Some(r"C:\Users\..\Admin"),
            Some(appdata.clone()),
            Some(userprofile.clone()),
            TargetPlatform::Windows,
        )
        .expect("AGY_COMPANION_CONFIG overrides Windows XDG and APPDATA");
        assert_eq!(env_cfg, PathBuf::from(r"D:\work\agy-config.json"));

        // Invalid backslash traversal in AGY_COMPANION_CONFIG fails closed on Windows.
        let bad_env_cfg = resolve_config_path_for_platform(
            Some(r"C:\Users\alice\..\evil.json"),
            Some(r"C:\Users\alice\xdg"),
            Some(appdata.clone()),
            Some(userprofile.clone()),
            TargetPlatform::Windows,
        )
        .expect_err("Windows backslash traversal in env override rejected");
        assert!(matches!(
            bad_env_cfg,
            CoreError::Fs(FsError::ParentDirTraversal)
        ));

        // Valid absolute Windows XDG values (drive-rooted, lowercase drive, forward-slash drive,
        // UNC including UTF-8, verbatim, benign dotted segments) override %APPDATA% / %LOCALAPPDATA%.
        for valid_xdg in [
            r"C:\Users\alice\xdg",
            r"c:\foo",
            "D:/Users/alice/xdg",
            r"\\server\share\xdg",
            "//server/share/xdg",
            r"\\é\ø",
            r"\\?\C:\Users\alice\xdg",
            r"\\?\c:\",
            r"\\?\UNC\server\share\xdg",
            r"\\?\UNC\é\ø",
            r"C:\Users\my..dir",
            r"C:\Users\...",
        ] {
            let xdg_cfg = resolve_config_path_for_platform(
                None,
                Some(valid_xdg),
                Some(appdata.clone()),
                None,
                TargetPlatform::Windows,
            )
            .unwrap_or_else(|err| {
                panic!("valid Windows XDG_CONFIG_HOME '{valid_xdg}' failed: {err:?}")
            });
            assert_eq!(
                xdg_cfg,
                PathBuf::from(valid_xdg)
                    .join("agy-companion")
                    .join("config.json")
            );

            let xdg_usage = resolve_usage_dir_for_platform(
                Some(valid_xdg),
                Some(localappdata.clone()),
                None,
                TargetPlatform::Windows,
            )
            .unwrap_or_else(|err| {
                panic!("valid Windows XDG_DATA_HOME '{valid_xdg}' failed: {err:?}")
            });
            assert_eq!(
                xdg_usage,
                PathBuf::from(valid_xdg).join("agy-companion").join("usage")
            );
        }

        // Non-absolute Windows XDG values fall back to %APPDATA% / %LOCALAPPDATA%.
        for ignored_win_xdg in [
            "",
            "   ",
            r"relative\dir",
            r".\cfg",
            "C:relative",
            r"\rooted",
            "/unix/path",
            r"\\",
            r"\\server",
            r"\\server\",
            r"\\server\.",
            r"\\server\?",
            r"\\ . \share",
            r"\\ ? \share",
            r"\\.\COM1",
            r"\\.\pipe\foo",
            "//./COM1",
            r"\\?\COM1",
            r"\\?\C:/foo",
            r"\\?\UNC\server\",
            r"\\?\UNC\server\.",
            r"\\?\UNC\server\?",
            r"\\?\UNC\ . \share",
            "\\\\?\\é\\foo",
            "é:\\foo",
            "C:é",
            "é:",
            "🔥:\\foo",
            r" C:\Users\alice\xdg",
        ] {
            let cfg = resolve_config_path_for_platform(
                None,
                Some(ignored_win_xdg),
                Some(appdata.clone()),
                Some(userprofile.clone()),
                TargetPlatform::Windows,
            )
            .unwrap_or_else(|err| {
                panic!(
                    "expected APPDATA fallback for Windows XDG_CONFIG_HOME='{ignored_win_xdg}': {err:?}"
                )
            });
            assert_eq!(
                cfg,
                appdata.join("agy-companion").join("config.json"),
                "Windows config fallback mismatch for '{ignored_win_xdg}'"
            );

            let usage = resolve_usage_dir_for_platform(
                Some(ignored_win_xdg),
                Some(localappdata.clone()),
                Some(userprofile.clone()),
                TargetPlatform::Windows,
            )
            .unwrap_or_else(|err| {
                panic!(
                    "expected LOCALAPPDATA fallback for Windows XDG_DATA_HOME='{ignored_win_xdg}': {err:?}"
                )
            });
            assert_eq!(
                usage,
                localappdata.join("agy-companion").join("usage"),
                "Windows usage fallback mismatch for '{ignored_win_xdg}'"
            );
        }

        // Windows traversal payloads fail closed even when tests run on macOS/Linux.
        for bad_win_xdg in [
            r"C:\Users\alice\..\evil",
            "C:/Users/alice/../evil",
            r"C:\a\b\..",
            r"C:..\evil",
            "C:..",
            r"..\escape",
            r"\\server\share\..\evil",
            r"\\?\C:\Users\..\Admin",
        ] {
            let cfg_err = resolve_config_path_for_platform(
                None,
                Some(bad_win_xdg),
                Some(appdata.clone()),
                Some(userprofile.clone()),
                TargetPlatform::Windows,
            )
            .expect_err(&format!(
                "expected ParentDirTraversal for Windows XDG_CONFIG_HOME='{bad_win_xdg}'"
            ));
            assert!(
                matches!(cfg_err, CoreError::Fs(FsError::ParentDirTraversal)),
                "unexpected error for '{bad_win_xdg}': {cfg_err:?}"
            );

            let usage_err = resolve_usage_dir_for_platform(
                Some(bad_win_xdg),
                Some(localappdata.clone()),
                Some(userprofile.clone()),
                TargetPlatform::Windows,
            )
            .expect_err(&format!(
                "expected ParentDirTraversal for Windows XDG_DATA_HOME='{bad_win_xdg}'"
            ));
            assert!(
                matches!(usage_err, CoreError::Fs(FsError::ParentDirTraversal)),
                "unexpected error for '{bad_win_xdg}': {usage_err:?}"
            );
        }

        // Windows NUL payload fails closed with ContainsNul.
        let win_nul = "C:\\Users\\alice\\xdg\0bad";
        assert!(matches!(
            resolve_config_path_for_platform(
                None,
                Some(win_nul),
                Some(appdata),
                Some(userprofile.clone()),
                TargetPlatform::Windows,
            ),
            Err(CoreError::Fs(FsError::ContainsNul))
        ));
        assert!(matches!(
            resolve_usage_dir_for_platform(
                Some(win_nul),
                Some(localappdata),
                Some(userprofile),
                TargetPlatform::Windows,
            ),
            Err(CoreError::Fs(FsError::ContainsNul))
        ));
    }

    #[test]
    fn rejects_empty_whitespace_traversal_and_nul_in_home_and_known_folders() {
        let valid_win_home = PathBuf::from(r"C:\Users\alice");

        // 1. Empty and whitespace-only home -> HomeDirUnavailable on both Unix & Windows
        //    and via public resolve_config_path / resolve_usage_dir wrappers.
        for blank in ["", "   ", "\t\n"] {
            let blank_home = Some(PathBuf::from(blank));

            for platform in [TargetPlatform::Unix, TargetPlatform::Windows] {
                let cfg_err = resolve_config_path_for_platform(
                    None,
                    None,
                    None,
                    blank_home.clone(),
                    platform,
                )
                .unwrap_err();
                assert!(
                    matches!(cfg_err, CoreError::HomeDirUnavailable),
                    "expected HomeDirUnavailable for {platform:?} config home='{blank}', got {cfg_err:?}"
                );

                let usage_err =
                    resolve_usage_dir_for_platform(None, None, blank_home.clone(), platform)
                        .unwrap_err();
                assert!(
                    matches!(usage_err, CoreError::HomeDirUnavailable),
                    "expected HomeDirUnavailable for {platform:?} usage home='{blank}', got {usage_err:?}"
                );
            }

            let pub_cfg_err = resolve_config_path(None, blank_home.clone()).unwrap_err();
            assert!(
                matches!(pub_cfg_err, CoreError::HomeDirUnavailable),
                "expected HomeDirUnavailable for public resolve_config_path home='{blank}', got {pub_cfg_err:?}"
            );

            let pub_usage_err = resolve_usage_dir(blank_home).unwrap_err();
            assert!(
                matches!(pub_usage_err, CoreError::HomeDirUnavailable),
                "expected HomeDirUnavailable for public resolve_usage_dir home='{blank}', got {pub_usage_err:?}"
            );
        }

        // 2. Traversal and NUL in home fail closed on Unix (and public wrappers).
        for (bad_unix_home, is_nul) in [
            ("/Users/alice/../evil", false),
            ("/Users/alice\0evil", true),
        ] {
            let bad_home = Some(PathBuf::from(bad_unix_home));
            let check = |err: CoreError, label: &str| {
                if is_nul {
                    assert!(
                        matches!(err, CoreError::Fs(FsError::ContainsNul)),
                        "expected ContainsNul for {label}, got {err:?}"
                    );
                } else {
                    assert!(
                        matches!(err, CoreError::Fs(FsError::ParentDirTraversal)),
                        "expected ParentDirTraversal for {label}, got {err:?}"
                    );
                }
            };

            check(
                resolve_config_path_for_platform(
                    None,
                    None,
                    None,
                    bad_home.clone(),
                    TargetPlatform::Unix,
                )
                .unwrap_err(),
                "Unix config home",
            );
            check(
                resolve_usage_dir_for_platform(None, None, bad_home.clone(), TargetPlatform::Unix)
                    .unwrap_err(),
                "Unix usage home",
            );
            check(
                resolve_config_path(None, bad_home.clone()).unwrap_err(),
                "public resolve_config_path home",
            );
            check(
                resolve_usage_dir(bad_home).unwrap_err(),
                "public resolve_usage_dir home",
            );
        }

        // 3. Traversal and NUL in Windows home fail closed.
        for (bad_win_home, is_nul) in [
            (r"C:\Users\..\evil", false),
            ("C:\\Users\\alice\0evil", true),
        ] {
            let bad_home = Some(PathBuf::from(bad_win_home));
            let cfg_err = resolve_config_path_for_platform(
                None,
                None,
                None,
                bad_home.clone(),
                TargetPlatform::Windows,
            )
            .unwrap_err();
            let usage_err =
                resolve_usage_dir_for_platform(None, None, bad_home, TargetPlatform::Windows)
                    .unwrap_err();
            if is_nul {
                assert!(matches!(cfg_err, CoreError::Fs(FsError::ContainsNul)));
                assert!(matches!(usage_err, CoreError::Fs(FsError::ContainsNul)));
            } else {
                assert!(matches!(
                    cfg_err,
                    CoreError::Fs(FsError::ParentDirTraversal)
                ));
                assert!(matches!(
                    usage_err,
                    CoreError::Fs(FsError::ParentDirTraversal)
                ));
            }
        }

        // 4. Traversal and NUL in Windows platform_config_dir / platform_data_local_dir fail closed
        //    even when a valid home fallback is present.
        for (bad_known_folder, is_nul) in [
            (r"C:\Users\..\evil", false),
            ("C:\\Users\\alice\0evil", true),
        ] {
            let bad_dir = Some(PathBuf::from(bad_known_folder));
            let cfg_err = resolve_config_path_for_platform(
                None,
                None,
                bad_dir.clone(),
                Some(valid_win_home.clone()),
                TargetPlatform::Windows,
            )
            .unwrap_err();
            let usage_err = resolve_usage_dir_for_platform(
                None,
                bad_dir,
                Some(valid_win_home.clone()),
                TargetPlatform::Windows,
            )
            .unwrap_err();
            if is_nul {
                assert!(matches!(cfg_err, CoreError::Fs(FsError::ContainsNul)));
                assert!(matches!(usage_err, CoreError::Fs(FsError::ContainsNul)));
            } else {
                assert!(matches!(
                    cfg_err,
                    CoreError::Fs(FsError::ParentDirTraversal)
                ));
                assert!(matches!(
                    usage_err,
                    CoreError::Fs(FsError::ParentDirTraversal)
                ));
            }
        }
    }

    #[test]
    fn resolves_data_dir_and_plugins_dir_with_s6_precedence() {
        // Unix XDG_DATA_HOME precedence & fallback (no macOS Library/Application Support drift).
        let unix_xdg = resolve_data_dir_for_platform(
            Some("/tmp/xdg-data"),
            Some(PathBuf::from("/Users/test/Library/Application Support")),
            Some(PathBuf::from("/Users/test")),
            TargetPlatform::Unix,
        )
        .expect("unix xdg data dir");
        assert_eq!(unix_xdg, PathBuf::from("/tmp/xdg-data/agy-companion"));

        let unix_default = resolve_data_dir_for_platform(
            Some("relative/path"),
            Some(PathBuf::from("/Users/test/Library/Application Support")),
            Some(PathBuf::from("/Users/test")),
            TargetPlatform::Unix,
        )
        .expect("unix relative xdg falls back to ~/.local/share");
        assert_eq!(
            unix_default,
            PathBuf::from("/Users/test/.local/share/agy-companion")
        );

        let unix_blank = resolve_data_dir_for_platform(
            Some("   "),
            None,
            Some(PathBuf::from("/Users/test")),
            TargetPlatform::Unix,
        )
        .expect("blank xdg falls back");
        assert_eq!(
            unix_blank,
            PathBuf::from("/Users/test/.local/share/agy-companion")
        );

        // Windows precedence: XDG_DATA_HOME -> %LOCALAPPDATA% -> %USERPROFILE%\.local\share.
        let win_xdg = resolve_data_dir_for_platform(
            Some(r"C:\XdgData"),
            Some(PathBuf::from(r"C:\Users\Alice\AppData\Local")),
            Some(PathBuf::from(r"C:\Users\Alice")),
            TargetPlatform::Windows,
        )
        .expect("windows xdg data dir");
        assert_eq!(win_xdg, PathBuf::from(r"C:\XdgData").join("agy-companion"));

        let win_local = resolve_data_dir_for_platform(
            None,
            Some(PathBuf::from(r"C:\Users\Alice\AppData\Local")),
            Some(PathBuf::from(r"C:\Users\Alice")),
            TargetPlatform::Windows,
        )
        .expect("windows localappdata");
        assert_eq!(
            win_local,
            PathBuf::from(r"C:\Users\Alice\AppData\Local").join("agy-companion")
        );

        let win_home = resolve_data_dir_for_platform(
            None,
            None,
            Some(PathBuf::from(r"C:\Users\Alice")),
            TargetPlatform::Windows,
        )
        .expect("windows home fallback");
        assert_eq!(
            win_home,
            PathBuf::from(r"C:\Users\Alice")
                .join(".local")
                .join("share")
                .join("agy-companion")
        );

        // Fail-closed traversal / NUL / missing home checks.
        assert!(matches!(
            resolve_data_dir_for_platform(
                Some("/tmp/../escape"),
                None,
                Some(PathBuf::from("/Users/test")),
                TargetPlatform::Unix,
            ),
            Err(CoreError::Fs(FsError::ParentDirTraversal))
        ));
        assert!(matches!(
            resolve_data_dir_for_platform(
                Some("/tmp/bad\0data"),
                None,
                Some(PathBuf::from("/Users/test")),
                TargetPlatform::Unix,
            ),
            Err(CoreError::Fs(FsError::ContainsNul))
        ));
        assert!(matches!(
            resolve_data_dir_for_platform(None, None, None, TargetPlatform::Unix),
            Err(CoreError::HomeDirUnavailable)
        ));

        // Parity between default_usage_dir() and default_data_dir().join("usage").
        if let (Ok(data_dir), Ok(usage_dir)) = (default_data_dir(), default_usage_dir()) {
            assert_eq!(usage_dir, data_dir.join("usage"));
        }
        let resolved_data =
            resolve_data_dir(Some(PathBuf::from("/Users/test"))).expect("resolve_data_dir");
        let resolved_usage =
            resolve_usage_dir(Some(PathBuf::from("/Users/test"))).expect("resolve_usage_dir");
        assert_eq!(resolved_usage, resolved_data.join("usage"));

        // Plugins dir resolution on Unix & Windows + fail-closed checks.
        assert_eq!(
            resolve_plugins_dir_for_platform(
                Some(PathBuf::from("/Users/test")),
                TargetPlatform::Unix
            )
            .expect("unix plugins dir"),
            PathBuf::from("/Users/test/.gemini/config/plugins")
        );
        assert_eq!(
            resolve_plugins_dir_for_platform(
                Some(PathBuf::from(r"C:\Users\Alice")),
                TargetPlatform::Windows
            )
            .expect("windows plugins dir"),
            PathBuf::from(r"C:\Users\Alice")
                .join(".gemini")
                .join("config")
                .join("plugins")
        );
        assert_eq!(
            resolve_plugins_dir(Some(PathBuf::from("/Users/test"))).expect("resolve_plugins_dir"),
            PathBuf::from("/Users/test/.gemini/config/plugins")
        );
        assert!(matches!(
            resolve_plugins_dir_for_platform(None, TargetPlatform::Unix),
            Err(CoreError::HomeDirUnavailable)
        ));
        assert!(matches!(
            resolve_plugins_dir_for_platform(Some(PathBuf::from("   ")), TargetPlatform::Unix),
            Err(CoreError::HomeDirUnavailable)
        ));
        assert!(matches!(
            resolve_plugins_dir_for_platform(
                Some(PathBuf::from("/Users/../escape")),
                TargetPlatform::Unix
            ),
            Err(CoreError::Fs(FsError::ParentDirTraversal))
        ));
    }

    #[test]
    fn enabled_models_and_launcher_auto_start_defaults_dedup_validation_and_roundtrip() {
        // Legacy v1 JSON omitting enabled_models and launcher_auto_start loads defaults.
        let legacy_json = r#"{
            "schema_version": 1,
            "proxy": { "host": "127.0.0.1", "port": 24141 },
            "providers": { "gemini": { "default_model": "gemini-4-hydrogen" } }
        }"#;
        let legacy = Config::from_json_str(legacy_json, Path::new("legacy.json")).unwrap();
        assert_eq!(
            legacy.providers.gemini.enabled_models,
            default_enabled_gemini_models()
        );
        assert_eq!(legacy.launcher_auto_start, None);
        let redacted = legacy.redacted_with_auth(None, None);
        assert_eq!(
            redacted.providers.gemini.enabled_models,
            default_enabled_gemini_models()
        );
        assert_eq!(redacted.launcher_auto_start, None);

        // Deduplicates preserving order and supports launcher_auto_start = true / false / null.
        let dedup_json = r#"{
            "schema_version": 1,
            "launcher_auto_start": false,
            "providers": {
                "gemini": {
                    "enabled_models": ["gemini-4-neon", "gemini-4-carbon", "gemini-4-neon"]
                }
            }
        }"#;
        let dedup_cfg = Config::from_json_str(dedup_json, Path::new("dedup.json")).unwrap();
        assert_eq!(
            dedup_cfg.providers.gemini.enabled_models,
            vec!["gemini-4-neon".to_string(), "gemini-4-carbon".to_string()]
        );
        assert_eq!(dedup_cfg.launcher_auto_start, Some(false));
        assert_eq!(
            dedup_cfg.redacted_with_auth(None, None).launcher_auto_start,
            Some(false)
        );

        // Empty enabled_models is allowed and round-trips through save_secure / load_or_default.
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("config.json");
        let mut roundtrip = Config::default();
        roundtrip.providers.gemini.enabled_models = Vec::new();
        roundtrip.launcher_auto_start = Some(true);
        roundtrip.save_secure(&path).unwrap();
        let reloaded = Config::load_or_default(&path).unwrap();
        assert!(reloaded.providers.gemini.enabled_models.is_empty());
        assert_eq!(reloaded.launcher_auto_start, Some(true));

        // Invalid model in enabled_models fails closed; malformed launcher_auto_start yields ParseConfig.
        let bad_model_json = r#"{
            "schema_version": 1,
            "providers": { "gemini": { "enabled_models": ["gemini-4-carbon", "gemini-2.5-pro"] } }
        }"#;
        assert!(matches!(
            Config::from_json_str(bad_model_json, Path::new("bad.json")),
            Err(CoreError::InvalidModel { .. })
        ));
        let bad_flag_json = r#"{ "schema_version": 1, "launcher_auto_start": "yes" }"#;
        assert!(matches!(
            Config::from_json_str(bad_flag_json, Path::new("bad.json")),
            Err(CoreError::ParseConfig { .. })
        ));

        // Config::validate also rejects invalid enabled_models set directly on struct.
        let mut direct = Config::default();
        direct.providers.gemini.enabled_models = vec!["../evil".to_string()];
        assert!(matches!(
            direct.validate(),
            Err(CoreError::InvalidModel { .. })
        ));
    }

    #[test]
    fn apply_patch_updates_enabled_models_and_launcher_auto_start_transactionally() {
        let mut cfg = Config::default();
        cfg.apply_patch(ConfigPatch {
            providers: Some(ProvidersConfigPatch {
                gemini: Some(GeminiConfigPatch {
                    enabled_models: Some(vec![
                        "  gemini-4-argon  ".to_string(),
                        "gemini-4-carbon".to_string(),
                        "gemini-4-argon".to_string(),
                    ]),
                    ..Default::default()
                }),
                codex: None,
            }),
            launcher_auto_start: Some(Some(false)),
            ..Default::default()
        })
        .unwrap();
        assert_eq!(
            cfg.providers.gemini.enabled_models,
            vec!["gemini-4-argon".to_string(), "gemini-4-carbon".to_string()]
        );
        assert_eq!(cfg.launcher_auto_start, Some(false));

        // Empty enabled_models clears the list; launcher_auto_start: None leaves it unchanged.
        cfg.apply_patch(ConfigPatch {
            providers: Some(ProvidersConfigPatch {
                gemini: Some(GeminiConfigPatch {
                    enabled_models: Some(Vec::new()),
                    ..Default::default()
                }),
                codex: None,
            }),
            launcher_auto_start: None,
            ..Default::default()
        })
        .unwrap();
        assert!(cfg.providers.gemini.enabled_models.is_empty());
        assert_eq!(cfg.launcher_auto_start, Some(false));

        // Invalid model in patch rolls back completely (including launcher_auto_start).
        let before = cfg.clone();
        let err = cfg
            .apply_patch(ConfigPatch {
                providers: Some(ProvidersConfigPatch {
                    gemini: Some(GeminiConfigPatch {
                        enabled_models: Some(vec!["gemini-2.5-pro".to_string()]),
                        ..Default::default()
                    }),
                    codex: None,
                }),
                launcher_auto_start: Some(Some(true)),
                ..Default::default()
            })
            .unwrap_err();
        assert!(matches!(err, CoreError::InvalidModel { .. }));
        assert_eq!(cfg, before);
    }

    #[test]
    fn codex_config_defaults_additive_v1_load_validation_and_transactional_patch() {
        let def = Config::default();
        assert_eq!(def.providers.codex.base_url, DEFAULT_CODEX_BASE_URL);
        assert_eq!(def.providers.codex.default_model, DEFAULT_CODEX_MODEL);
        assert_eq!(
            def.providers.codex.enabled_models,
            default_enabled_codex_models()
        );
        assert_eq!(def.providers.codex.timeout_secs, DEFAULT_TIMEOUT_SECS);
        def.validate().expect("default codex config validates");

        // Existing v1 JSON omitting `providers.codex` loads Codex defaults seamlessly.
        let v1_without_codex = r#"{
            "schema_version": 1,
            "proxy": { "host": "127.0.0.1", "port": 24141 },
            "providers": { "gemini": { "default_model": "gemini-4-hydrogen" } }
        }"#;
        let loaded =
            Config::from_json_str(v1_without_codex, Path::new("v1-no-codex.json")).unwrap();
        assert_eq!(loaded.providers.codex, CodexConfig::default());

        // Deduplicates codex enabled_models and supports empty pause.
        let with_codex = r#"{
            "schema_version": 1,
            "providers": {
                "codex": {
                    "base_url": "https://chatgpt.com/backend-api/codex",
                    "default_model": "gpt-6.2-astra",
                    "enabled_models": ["gpt-6.2-sol", "gpt-6.2-astra", "gpt-6.2-sol"],
                    "timeout_secs": 300
                }
            }
        }"#;
        let parsed = Config::from_json_str(with_codex, Path::new("codex.json")).unwrap();
        assert_eq!(parsed.providers.codex.default_model, "gpt-6.2-astra");
        assert_eq!(
            parsed.providers.codex.enabled_models,
            vec!["gpt-6.2-sol".to_string(), "gpt-6.2-astra".to_string()]
        );
        assert_eq!(parsed.providers.codex.timeout_secs, 300);

        // Validation rejects zero timeout, non-HTTPS remote base_url (S3), and Gemini collision.
        let mut bad_timeout = Config::default();
        bad_timeout.providers.codex.timeout_secs = 0;
        assert!(matches!(
            bad_timeout.validate(),
            Err(CoreError::InvalidTimeout)
        ));

        let mut bad_url = Config::default();
        bad_url.providers.codex.base_url = "http://chatgpt.com/backend-api/codex".to_string();
        assert!(matches!(
            bad_url.validate(),
            Err(CoreError::InvalidBaseUrl { .. })
        ));

        let mut gemini_collision = Config::default();
        gemini_collision.providers.codex.default_model = "gemini-4-hydrogen".to_string();
        assert!(matches!(
            gemini_collision.validate(),
            Err(CoreError::InvalidModel { .. })
        ));

        // Transactional Codex patch applies cleanly and rolls back on failure.
        let mut cfg = Config::default();
        cfg.apply_patch(ConfigPatch {
            providers: Some(ProvidersConfigPatch {
                gemini: None,
                codex: Some(CodexConfigPatch {
                    base_url: Some("  http://127.0.0.1:18080/codex  ".to_string()),
                    default_model: Some("  gpt-6.2-astra  ".to_string()),
                    enabled_models: Some(vec![
                        "  gpt-6-luna  ".to_string(),
                        "gpt-6.2-astra".to_string(),
                        "gpt-6-luna".to_string(),
                    ]),
                    timeout_secs: Some(120),
                }),
            }),
            ..Default::default()
        })
        .expect("valid codex patch succeeds");
        assert_eq!(cfg.providers.codex.base_url, "http://127.0.0.1:18080/codex");
        assert_eq!(cfg.providers.codex.default_model, "gpt-6.2-astra");
        assert_eq!(
            cfg.providers.codex.enabled_models,
            vec!["gpt-6-luna".to_string(), "gpt-6.2-astra".to_string()]
        );
        assert_eq!(cfg.providers.codex.timeout_secs, 120);

        let before = cfg.clone();
        let err = cfg
            .apply_patch(ConfigPatch {
                providers: Some(ProvidersConfigPatch {
                    gemini: Some(GeminiConfigPatch {
                        timeout_secs: Some(300),
                        ..Default::default()
                    }),
                    codex: Some(CodexConfigPatch {
                        enabled_models: Some(vec!["gemini-4-carbon".to_string()]),
                        ..Default::default()
                    }),
                }),
                ..Default::default()
            })
            .expect_err("gemini collision in codex patch must fail");
        assert!(matches!(err, CoreError::InvalidModel { .. }));
        assert_eq!(cfg, before, "failed codex patch must roll back completely");
    }
}
