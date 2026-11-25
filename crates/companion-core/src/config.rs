use std::fmt;
use std::fs;
use std::io::{ErrorKind, Read, Write};
use std::net::Ipv4Addr;
use std::path::{Component, Path, PathBuf};

use serde::{Deserialize, Serialize};
use thiserror::Error;
use url::{Host, Url};
use uuid::Uuid;

use crate::redaction::{RedactedConfig, api_key_hint};

pub const CURRENT_SCHEMA_VERSION: u32 = 1;
pub const DEFAULT_LISTEN_HOST: &str = "127.0.0.1";
pub const DEFAULT_LISTEN_PORT: u16 = 4141;
pub const DEFAULT_GEMINI_BASE_URL: &str = "https://generativelanguage.googleapis.com";
pub const CLOUDCODE_DAILY_UPSTREAM_URL: &str = "https://daily-cloudcode-pa.googleapis.com";
pub const CLOUDCODE_PROD_UPSTREAM_URL: &str = "https://cloudcode-pa.googleapis.com";
pub const DEFAULT_CLOUDCODE_UPSTREAM_URL: &str = "auto";
pub const DEFAULT_GEMINI_MODEL: &str = "gemini-4-hydrogen";
pub const DEFAULT_TIMEOUT_SECS: u64 = 900;
pub const MAX_CONFIG_FILE_BYTES: u64 = 1024 * 1024;
pub const CONFIG_PATH_ENV_VAR: &str = "AGY_COMPANION_CONFIG";
pub const XDG_CONFIG_HOME_ENV_VAR: &str = "XDG_CONFIG_HOME";
pub const XDG_DATA_HOME_ENV_VAR: &str = "XDG_DATA_HOME";
pub const PROXY_PLACEHOLDER_API_KEY: &str = "agy-companion-local";

#[derive(Debug, Error)]
pub enum CoreError {
    #[error("unsupported config schema version {found} (supported: {supported})")]
    UnsupportedSchemaVersion { found: u32, supported: u32 },
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
    #[error("invalid upstream base URL: {reason}")]
    InvalidBaseUrl { reason: String },
    #[error("invalid path: {reason}")]
    InvalidPath { reason: String },
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
    #[error("failed to serialize usage event: {reason}")]
    SerializeUsage { reason: String },
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

#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct GeminiConfig {
    pub base_url: String,
    pub api_key: Option<String>,
    pub default_model: String,
    pub timeout_secs: u64,
}

impl Default for GeminiConfig {
    fn default() -> Self {
        Self {
            base_url: DEFAULT_GEMINI_BASE_URL.to_string(),
            api_key: None,
            default_model: DEFAULT_GEMINI_MODEL.to_string(),
            timeout_secs: DEFAULT_TIMEOUT_SECS,
        }
    }
}

impl fmt::Debug for GeminiConfig {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("GeminiConfig")
            .field("base_url", &self.base_url)
            .field("api_key", &api_key_hint(self.api_key.as_deref()))
            .field("default_model", &self.default_model)
            .field("timeout_secs", &self.timeout_secs)
            .finish()
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct Config {
    pub schema_version: u32,
    pub listen_host: String,
    pub listen_port: u16,
    pub gemini: GeminiConfig,
    pub cloudcode_upstream_url: String,
    pub usage_dir: Option<PathBuf>,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            schema_version: CURRENT_SCHEMA_VERSION,
            listen_host: DEFAULT_LISTEN_HOST.to_string(),
            listen_port: DEFAULT_LISTEN_PORT,
            gemini: GeminiConfig::default(),
            cloudcode_upstream_url: DEFAULT_CLOUDCODE_UPSTREAM_URL.to_string(),
            usage_dir: None,
        }
    }
}

pub fn validate_listen_host(host: &str) -> Result<(), CoreError> {
    match host {
        "127.0.0.1" | "localhost" => Ok(()),
        _ => Err(CoreError::InvalidListenHost {
            host: host.to_string(),
        }),
    }
}

/// Returns `true` when `key` (ignoring surrounding whitespace) equals the
/// internal local-proxy placeholder sentinel (`"agy-companion-local"`).
pub fn is_placeholder_api_key(key: &str) -> bool {
    key.trim() == PROXY_PLACEHOLDER_API_KEY
}

fn decode_ascii_hex_nibble(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        b'A'..=b'F' => Some(byte - b'A' + 10),
        _ => None,
    }
}

fn is_safe_path_segment_byte(byte: u8) -> bool {
    byte.is_ascii_alphanumeric()
        || matches!(
            byte,
            b'-' | b'.'
                | b'_'
                | b'~'
                | b'!'
                | b'$'
                | b'&'
                | b'\''
                | b'('
                | b')'
                | b'*'
                | b'+'
                | b','
                | b'='
                | b':'
                | b'@'
        )
}

fn validate_url_path(path: &str) -> Result<(), CoreError> {
    if !path.is_empty() && !path.starts_with('/') {
        return Err(CoreError::InvalidBaseUrl {
            reason: "base URL path must be empty or start with '/'".to_string(),
        });
    }

    let bytes = path.as_bytes();
    let mut decoded_segment = Vec::new();
    let mut i = 0;

    while i < bytes.len() {
        let byte = bytes[i];
        if byte == b'/' {
            if decoded_segment.as_slice() == b"." || decoded_segment.as_slice() == b".." {
                return Err(CoreError::InvalidBaseUrl {
                    reason: "base URL path must not contain '.' or '..' dot segments".to_string(),
                });
            }
            decoded_segment.clear();
            i += 1;
            continue;
        }

        if byte == b'%' {
            let (Some(&hi_byte), Some(&lo_byte)) = (bytes.get(i + 1), bytes.get(i + 2)) else {
                return Err(CoreError::InvalidBaseUrl {
                    reason: "base URL path contains a truncated percent-encoding sequence"
                        .to_string(),
                });
            };
            let (Some(hi), Some(lo)) = (
                decode_ascii_hex_nibble(hi_byte),
                decode_ascii_hex_nibble(lo_byte),
            ) else {
                return Err(CoreError::InvalidBaseUrl {
                    reason: "base URL path contains a malformed percent-encoding sequence"
                        .to_string(),
                });
            };
            let decoded = (hi << 4) | lo;
            if !is_safe_path_segment_byte(decoded) {
                return Err(CoreError::InvalidBaseUrl {
                    reason: "base URL path contains disallowed or nested percent-encoding"
                        .to_string(),
                });
            }
            decoded_segment.push(decoded);
            i += 3;
            continue;
        }

        if !is_safe_path_segment_byte(byte) {
            return Err(CoreError::InvalidBaseUrl {
                reason: "base URL path contains disallowed characters".to_string(),
            });
        }
        decoded_segment.push(byte);
        i += 1;
    }

    if decoded_segment.as_slice() == b"." || decoded_segment.as_slice() == b".." {
        return Err(CoreError::InvalidBaseUrl {
            reason: "base URL path must not contain '.' or '..' dot segments".to_string(),
        });
    }

    Ok(())
}

fn validate_and_extract_raw_host(raw_authority: &str) -> Result<&str, CoreError> {
    if raw_authority.ends_with(':') {
        return Err(CoreError::InvalidBaseUrl {
            reason: "URL must not contain an empty port".to_string(),
        });
    }

    let (raw_host, raw_port) = if raw_authority.starts_with('[') {
        let Some(close_bracket) = raw_authority.find(']') else {
            return Err(CoreError::InvalidBaseUrl {
                reason: "malformed IPv6 host authority".to_string(),
            });
        };
        let host = &raw_authority[..=close_bracket];
        let rest = &raw_authority[close_bracket + 1..];
        if rest.is_empty() {
            (host, None)
        } else if let Some(port) = rest.strip_prefix(':') {
            (host, Some(port))
        } else {
            return Err(CoreError::InvalidBaseUrl {
                reason: "malformed host/port authority".to_string(),
            });
        }
    } else {
        if raw_authority.contains(['[', ']']) {
            return Err(CoreError::InvalidBaseUrl {
                reason: "malformed host/port authority".to_string(),
            });
        }
        match raw_authority.split_once(':') {
            Some((host, port)) => {
                if port.contains(':') {
                    return Err(CoreError::InvalidBaseUrl {
                        reason: "malformed host/port authority".to_string(),
                    });
                }
                (host, Some(port))
            }
            None => (raw_authority, None),
        }
    };

    if raw_host.is_empty() {
        return Err(CoreError::InvalidBaseUrl {
            reason: "URL must contain a host".to_string(),
        });
    }

    if let Some(port) = raw_port
        && (port.is_empty()
            || port.starts_with('0')
            || !port.bytes().all(|b| b.is_ascii_digit())
            || port.parse::<u16>().ok().filter(|&p| p > 0).is_none())
    {
        return Err(CoreError::InvalidBaseUrl {
            reason: "URL port must be a valid integer between 1 and 65535".to_string(),
        });
    }

    Ok(raw_host)
}

pub fn validate_upstream_url(raw: &str) -> Result<(), CoreError> {
    if raw.is_empty() {
        return Err(CoreError::InvalidBaseUrl {
            reason: "URL must not be empty".to_string(),
        });
    }
    if !raw.bytes().all(|b| (0x21..=0x7E).contains(&b)) {
        return Err(CoreError::InvalidBaseUrl {
            reason: "URL must contain only printable ASCII characters without whitespace or control characters"
                .to_string(),
        });
    }
    if raw.contains('\\') {
        return Err(CoreError::InvalidBaseUrl {
            reason: "URL must not contain backslashes".to_string(),
        });
    }
    if raw.contains(['?', '#']) {
        return Err(CoreError::InvalidBaseUrl {
            reason: "base URL must not include query parameters or fragments".to_string(),
        });
    }

    let Some((raw_scheme, after_scheme)) = raw.split_once("://") else {
        return Err(CoreError::InvalidBaseUrl {
            reason: "URL must include an explicit scheme (https:// or http://)".to_string(),
        });
    };
    if !(raw_scheme.eq_ignore_ascii_case("https") || raw_scheme.eq_ignore_ascii_case("http")) {
        return Err(CoreError::InvalidBaseUrl {
            reason: "URL must include an explicit scheme (https:// or http://)".to_string(),
        });
    }

    let authority_end = after_scheme.find('/').unwrap_or(after_scheme.len());
    let raw_authority = &after_scheme[..authority_end];
    let raw_path = &after_scheme[authority_end..];

    if raw_authority.is_empty() {
        return Err(CoreError::InvalidBaseUrl {
            reason: "URL must contain a host".to_string(),
        });
    }
    if raw_authority.contains('@') {
        return Err(CoreError::InvalidBaseUrl {
            reason: "URL must not contain embedded credentials or '@'".to_string(),
        });
    }

    let raw_host = validate_and_extract_raw_host(raw_authority)?;
    validate_url_path(raw_path)?;

    let parsed = Url::parse(raw).map_err(|err| CoreError::InvalidBaseUrl {
        reason: format!("malformed URL: {err}"),
    })?;

    if !parsed.username().is_empty() || parsed.password().is_some() {
        return Err(CoreError::InvalidBaseUrl {
            reason: "URL must not contain embedded credentials".to_string(),
        });
    }
    if parsed.query().is_some() || parsed.fragment().is_some() {
        return Err(CoreError::InvalidBaseUrl {
            reason: "base URL must not include query parameters or fragments".to_string(),
        });
    }

    let expected_parsed_path = if raw_path.is_empty() { "/" } else { raw_path };
    if parsed.path() != expected_parsed_path {
        return Err(CoreError::InvalidBaseUrl {
            reason:
                "base URL path must already be normalized without traversal or special encoding"
                    .to_string(),
        });
    }
    validate_url_path(parsed.path())?;

    if parsed.port() == Some(0) {
        return Err(CoreError::InvalidBaseUrl {
            reason: "URL port must be greater than 0".to_string(),
        });
    }

    match parsed.scheme() {
        "https" => {
            if parsed.host().is_none() {
                return Err(CoreError::InvalidBaseUrl {
                    reason: "https URL must include a valid host".to_string(),
                });
            }
            Ok(())
        }
        "http" => {
            let parsed_is_loopback = matches!(
                parsed.host(),
                Some(Host::Ipv4(ip)) if ip == Ipv4Addr::LOCALHOST
            ) || matches!(parsed.host(), Some(Host::Domain("localhost")));

            if !parsed_is_loopback {
                return Err(CoreError::InvalidBaseUrl {
                    reason: "http:// is only permitted for 127.0.0.1 or localhost".to_string(),
                });
            }

            if raw_host.starts_with('[') {
                return Err(CoreError::InvalidBaseUrl {
                    reason: "http:// is only permitted for 127.0.0.1 or localhost".to_string(),
                });
            }

            if raw_host == "127.0.0.1" || raw_host.eq_ignore_ascii_case("localhost") {
                Ok(())
            } else {
                Err(CoreError::InvalidBaseUrl {
                    reason: "http:// requires literal 127.0.0.1 or localhost host".to_string(),
                })
            }
        }
        scheme => Err(CoreError::InvalidBaseUrl {
            reason: format!("unsupported URL scheme '{scheme}'"),
        }),
    }
}

pub fn validate_cloudcode_upstream_url(raw: &str) -> Result<(), CoreError> {
    if raw == DEFAULT_CLOUDCODE_UPSTREAM_URL {
        return Ok(());
    }
    validate_upstream_url(raw)
}

pub fn validate_path(path: &Path) -> Result<(), CoreError> {
    if path.as_os_str().is_empty() || path.to_str().is_some_and(|s| s.trim().is_empty()) {
        return Err(CoreError::InvalidPath {
            reason: "path must not be empty".to_string(),
        });
    }

    #[cfg(unix)]
    {
        use std::os::unix::ffi::OsStrExt;
        if path.as_os_str().as_bytes().contains(&0) {
            return Err(CoreError::InvalidPath {
                reason: "path must not contain NUL bytes".to_string(),
            });
        }
    }
    #[cfg(not(unix))]
    {
        if path.to_string_lossy().contains('\0') {
            return Err(CoreError::InvalidPath {
                reason: "path must not contain NUL bytes".to_string(),
            });
        }
    }

    if path
        .components()
        .any(|component| matches!(component, Component::ParentDir))
    {
        return Err(CoreError::InvalidPath {
            reason: "path must not contain parent directory ('..') traversal".to_string(),
        });
    }

    Ok(())
}

fn reject_symlink_target_file(path: &Path, label: &str) -> Result<(), CoreError> {
    // Strip trailing separators/dot components so POSIX lstat(2) inspects the symlink itself.
    let check_path: PathBuf = path.components().collect();
    match fs::symlink_metadata(&check_path) {
        Ok(meta) if meta.file_type().is_symlink() => Err(CoreError::InvalidPath {
            reason: format!("symbolic links are not allowed for {label}"),
        }),
        Ok(_) => Ok(()),
        Err(err) if err.kind() == ErrorKind::NotFound => Ok(()),
        Err(source) => Err(CoreError::Io {
            path: path.to_path_buf(),
            source,
        }),
    }
}

fn inspect_existing_config_file(path: &Path) -> Result<Option<fs::Metadata>, CoreError> {
    // Strip trailing separators/dot components so POSIX lstat(2) inspects the entry itself
    // and rejects symlinks, FIFOs, sockets, devices, directories, and oversized files before open(2).
    let check_path: PathBuf = path.components().collect();
    match fs::symlink_metadata(&check_path) {
        Ok(meta) => {
            let file_type = meta.file_type();
            if file_type.is_symlink() {
                return Err(CoreError::InvalidPath {
                    reason: "symbolic links are not allowed for config files".to_string(),
                });
            }
            if !file_type.is_file() {
                return Err(CoreError::InvalidPath {
                    reason: "config path must be a regular file".to_string(),
                });
            }
            if meta.len() > MAX_CONFIG_FILE_BYTES {
                return Err(CoreError::InvalidPath {
                    reason: format!(
                        "config file exceeds maximum allowed size of {MAX_CONFIG_FILE_BYTES} bytes"
                    ),
                });
            }
            Ok(Some(meta))
        }
        Err(err) if err.kind() == ErrorKind::NotFound => Ok(None),
        Err(source) => Err(CoreError::Io {
            path: path.to_path_buf(),
            source,
        }),
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
    validate_path(dir)?;
    if dir
        .to_str()
        .is_some_and(|raw| has_parent_dir_traversal(raw, platform))
    {
        return Err(CoreError::InvalidPath {
            reason: "path must not contain parent directory ('..') traversal".to_string(),
        });
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
        return Err(CoreError::InvalidPath {
            reason: "path must not contain NUL bytes".to_string(),
        });
    }

    if has_parent_dir_traversal(raw, platform) {
        return Err(CoreError::InvalidPath {
            reason: "path must not contain parent directory ('..') traversal".to_string(),
        });
    }

    if !is_absolute_for_platform(raw, platform) {
        return Ok(None);
    }

    let candidate = PathBuf::from(raw);
    validate_path(&candidate)?;
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
                return Err(CoreError::InvalidPath {
                    reason: "path must not contain NUL bytes".to_string(),
                });
            }
            if has_parent_dir_traversal(trimmed, platform) {
                return Err(CoreError::InvalidPath {
                    reason: "path must not contain parent directory ('..') traversal".to_string(),
                });
            }
            let path = PathBuf::from(trimmed);
            validate_path(&path)?;
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

pub fn resolve_config_path(
    env_override: Option<&str>,
    home: Option<PathBuf>,
) -> Result<PathBuf, CoreError> {
    resolve_config_path_for_platform(env_override, None, None, home, TargetPlatform::current())
}

pub fn resolve_data_dir(home: Option<PathBuf>) -> Result<PathBuf, CoreError> {
    resolve_data_dir_for_platform(None, None, home, TargetPlatform::current())
}

pub fn resolve_usage_dir(home: Option<PathBuf>) -> Result<PathBuf, CoreError> {
    resolve_usage_dir_for_platform(None, None, home, TargetPlatform::current())
}

pub fn resolve_plugins_dir(home: Option<PathBuf>) -> Result<PathBuf, CoreError> {
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
    let platform_config_dir = dirs::config_dir();
    #[cfg(not(windows))]
    let platform_config_dir = None;

    resolve_config_path_for_platform(
        None,
        xdg_config_home.as_deref(),
        platform_config_dir,
        dirs::home_dir(),
        platform,
    )
}

pub fn default_data_dir() -> Result<PathBuf, CoreError> {
    let platform = TargetPlatform::current();
    let xdg_data_home = read_optional_env(XDG_DATA_HOME_ENV_VAR)?;
    #[cfg(windows)]
    let platform_data_local_dir = dirs::data_local_dir();
    #[cfg(not(windows))]
    let platform_data_local_dir = None;

    resolve_data_dir_for_platform(
        xdg_data_home.as_deref(),
        platform_data_local_dir,
        dirs::home_dir(),
        platform,
    )
}

pub fn default_usage_dir() -> Result<PathBuf, CoreError> {
    Ok(default_data_dir()?.join("usage"))
}

pub fn default_plugins_dir() -> Result<PathBuf, CoreError> {
    resolve_plugins_dir_for_platform(dirs::home_dir(), TargetPlatform::current())
}

/// Returns `true` if `model` is Gemini 3.8 or above (e.g. `gemini-3.8-flash`, `gemini-4-hydrogen`).
pub fn is_supported_gemini_model(model: &str) -> bool {
    let Some(rest) = model.strip_prefix("gemini-") else {
        return false;
    };
    let major_end = rest.bytes().take_while(|b| b.is_ascii_digit()).count();
    if major_end == 0 {
        return false;
    }
    let Ok(major) = rest[..major_end].parse::<u32>() else {
        return false;
    };
    let after_major = &rest[major_end..];
    if major == 4 {
        return after_major
            .strip_prefix('-')
            .or_else(|| after_major.strip_prefix('.'))
            .is_some_and(|suffix| !suffix.is_empty());
    }
    if major == 3 {
        let Some(after_dot) = after_major.strip_prefix('.') else {
            return false;
        };
        let minor_end = after_dot.bytes().take_while(|b| b.is_ascii_digit()).count();
        if minor_end == 0 {
            return false;
        }
        let Ok(minor) = after_dot[..minor_end].parse::<u32>() else {
            return false;
        };
        if minor < 8 {
            return false;
        }
        let after_minor = &after_dot[minor_end..];
        return after_minor.is_empty()
            || after_minor
                .strip_prefix('-')
                .or_else(|| after_minor.strip_prefix('.'))
                .is_some_and(|suffix| !suffix.is_empty());
    }
    false
}

/// Validates a Gemini model identifier for syntax safety and Gemini 3.8+ / Gemini 4+ membership.
pub fn validate_gemini_model(model: &str) -> Result<(), CoreError> {
    if model.is_empty()
        || !model.bytes().all(|b| (0x21..=0x7E).contains(&b))
        || model == "."
        || model.contains("..")
        || model.contains(['?', '#', '/', '\\', '%', ';'])
    {
        return Err(CoreError::InvalidModel {
            reason:
                "default_model must be non-empty printable ASCII free of whitespace, control, or path/query characters"
                    .to_string(),
        });
    }
    if !is_supported_gemini_model(model) {
        return Err(CoreError::InvalidModel {
            reason:
                "only Gemini 3.8+ and Gemini 4+ models are supported (e.g. gemini-4-hydrogen, gemini-4-argon, gemini-3.8-flash)"
                    .to_string(),
        });
    }
    Ok(())
}

impl Config {
    pub fn validate(&self) -> Result<(), CoreError> {
        if self.schema_version != CURRENT_SCHEMA_VERSION {
            return Err(CoreError::UnsupportedSchemaVersion {
                found: self.schema_version,
                supported: CURRENT_SCHEMA_VERSION,
            });
        }
        validate_listen_host(&self.listen_host)?;
        if self.listen_port == 0 {
            return Err(CoreError::InvalidListenPort);
        }
        if self.gemini.timeout_secs == 0 {
            return Err(CoreError::InvalidTimeout);
        }

        validate_gemini_model(&self.gemini.default_model)?;

        validate_upstream_url(&self.gemini.base_url)?;
        validate_cloudcode_upstream_url(&self.cloudcode_upstream_url)?;
        if let Some(ref key) = self.gemini.api_key
            && (!key.bytes().all(|b| (0x20..=0x7E).contains(&b))
                || !key
                    .trim_matches(' ')
                    .bytes()
                    .all(|b| (0x21..=0x7E).contains(&b)))
        {
            return Err(CoreError::InvalidApiKey);
        }
        if let Some(ref usage_dir) = self.usage_dir {
            validate_path(usage_dir)?;
        }

        Ok(())
    }

    pub fn effective_usage_dir(&self) -> Result<PathBuf, CoreError> {
        if let Some(ref usage_dir) = self.usage_dir {
            validate_path(usage_dir)?;
            Ok(usage_dir.clone())
        } else {
            default_usage_dir()
        }
    }

    pub fn redacted(&self) -> RedactedConfig {
        let hint = api_key_hint(self.gemini.api_key.as_deref());
        RedactedConfig {
            schema_version: self.schema_version,
            listen_host: self.listen_host.clone(),
            listen_port: self.listen_port,
            gemini_base_url: self.gemini.base_url.clone(),
            gemini_default_model: self.gemini.default_model.clone(),
            gemini_timeout_secs: self.gemini.timeout_secs,
            cloudcode_upstream_url: self.cloudcode_upstream_url.clone(),
            api_key_configured: hint.is_some(),
            api_key_hint: hint,
        }
    }

    pub fn load_or_default(path: &Path) -> Result<Config, CoreError> {
        validate_path(path)?;
        let Some(lstat_meta) = inspect_existing_config_file(path)? else {
            return Ok(Config::default());
        };

        // Open the original path so trailing slashes on a regular file still fail safely (ENOTDIR),
        // while symlinks, FIFOs, directories, and oversized files were already rejected by lstat above.
        let file = match fs::File::open(path) {
            Ok(file) => file,
            Err(err) if err.kind() == ErrorKind::NotFound => return Ok(Config::default()),
            Err(source) => {
                return Err(CoreError::Io {
                    path: path.to_path_buf(),
                    source,
                });
            }
        };

        let open_meta = file.metadata().map_err(|source| CoreError::Io {
            path: path.to_path_buf(),
            source,
        })?;
        if !open_meta.file_type().is_file() {
            return Err(CoreError::InvalidPath {
                reason: "config path must be a regular file".to_string(),
            });
        }
        #[cfg(unix)]
        {
            use std::os::unix::fs::MetadataExt;
            if open_meta.dev() != lstat_meta.dev() || open_meta.ino() != lstat_meta.ino() {
                return Err(CoreError::InvalidPath {
                    reason: "config file changed concurrently during open".to_string(),
                });
            }
        }
        #[cfg(not(unix))]
        let _ = lstat_meta;

        let mut buf = Vec::new();
        file.take(MAX_CONFIG_FILE_BYTES + 1)
            .read_to_end(&mut buf)
            .map_err(|source| CoreError::Io {
                path: path.to_path_buf(),
                source,
            })?;

        if (buf.len() as u64) > MAX_CONFIG_FILE_BYTES {
            return Err(CoreError::InvalidPath {
                reason: format!(
                    "config file exceeds maximum allowed size of {MAX_CONFIG_FILE_BYTES} bytes"
                ),
            });
        }

        let raw = String::from_utf8(buf).map_err(|_| CoreError::Io {
            path: path.to_path_buf(),
            source: std::io::Error::new(
                ErrorKind::InvalidData,
                "config file did not contain valid UTF-8",
            ),
        })?;

        let config: Config = serde_json::from_str(&raw).map_err(|err| CoreError::ParseConfig {
            path: path.to_path_buf(),
            line: err.line(),
            column: err.column(),
        })?;
        config.validate()?;
        Ok(config)
    }

    pub fn save_secure(&self, path: &Path) -> Result<(), CoreError> {
        validate_path(path)?;
        self.validate()?;
        reject_symlink_target_file(path, "config files")?;

        let mut payload =
            serde_json::to_string_pretty(self).map_err(|err| CoreError::SerializeConfig {
                reason: err.to_string(),
            })?;
        payload.push('\n');

        write_file_atomic_secure(path, payload.as_bytes())
    }
}

/// Atomically writes `payload` to `path` with mode `0600` (and `0700` parent dir on Unix),
/// rejecting path traversal, NUL bytes, and symlink targets fail-closed.
pub fn write_file_atomic_secure(path: &Path, payload: &[u8]) -> Result<(), CoreError> {
    write_file_atomic_with_mode(path, payload, 0o600, 0o700, false, true, false)
}

/// Atomically writes `payload` to `path` using a sibling temp file + `sync_all()` + `fs::rename`,
/// enforcing explicit Unix file/directory modes and rejecting symlink/non-regular target entries.
pub(crate) fn write_file_atomic_with_mode(
    path: &Path,
    payload: &[u8],
    file_mode: u32,
    dir_mode: u32,
    tighten_existing_parent: bool,
    allow_symlink_parent: bool,
    reject_non_regular_target: bool,
) -> Result<(), CoreError> {
    #[cfg(not(unix))]
    let _ = (file_mode, dir_mode, tighten_existing_parent);

    validate_path(path)?;
    reject_symlink_target_file(path, "target files")?;

    if reject_non_regular_target {
        let check_path: PathBuf = path.components().collect();
        match fs::symlink_metadata(&check_path) {
            Ok(meta) if !meta.file_type().is_file() => {
                return Err(CoreError::InvalidPath {
                    reason: "target path must be a regular file".to_string(),
                });
            }
            Ok(_) => {}
            Err(err) if err.kind() == ErrorKind::NotFound => {}
            Err(source) => {
                return Err(CoreError::Io {
                    path: path.to_path_buf(),
                    source,
                });
            }
        }
    }

    let file_name = path
        .file_name()
        .and_then(|name| name.to_str())
        .ok_or_else(|| CoreError::InvalidPath {
            reason: "target path must include a valid UTF-8 file name".to_string(),
        })?;

    let parent = path
        .parent()
        .filter(|dir| !dir.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));

    let parent_check: PathBuf = parent.components().collect();
    match fs::symlink_metadata(&parent_check) {
        Ok(meta) => {
            if !allow_symlink_parent && meta.file_type().is_symlink() {
                return Err(CoreError::InvalidPath {
                    reason: "symbolic links are not allowed for parent directories".to_string(),
                });
            }
            if !(meta.file_type().is_dir()
                || (allow_symlink_parent && meta.file_type().is_symlink()))
            {
                return Err(CoreError::InvalidPath {
                    reason: "parent path must be a directory".to_string(),
                });
            }
            #[cfg(unix)]
            if tighten_existing_parent {
                use std::os::unix::fs::PermissionsExt;
                fs::set_permissions(&parent_check, fs::Permissions::from_mode(dir_mode)).map_err(
                    |source| CoreError::Io {
                        path: parent.to_path_buf(),
                        source,
                    },
                )?;
            }
        }
        Err(err) if err.kind() == ErrorKind::NotFound => {
            let mut dir_builder = fs::DirBuilder::new();
            dir_builder.recursive(true);
            #[cfg(unix)]
            {
                use std::os::unix::fs::DirBuilderExt;
                dir_builder.mode(dir_mode);
            }
            dir_builder.create(parent).map_err(|source| CoreError::Io {
                path: parent.to_path_buf(),
                source,
            })?;
            #[cfg(unix)]
            if tighten_existing_parent {
                use std::os::unix::fs::PermissionsExt;
                fs::set_permissions(&parent_check, fs::Permissions::from_mode(dir_mode)).map_err(
                    |source| CoreError::Io {
                        path: parent.to_path_buf(),
                        source,
                    },
                )?;
            }
        }
        Err(source) => {
            return Err(CoreError::Io {
                path: parent.to_path_buf(),
                source,
            });
        }
    }

    let tmp_path = parent.join(format!(".{file_name}.tmp.{}", Uuid::new_v4()));

    let mut open_opts = fs::OpenOptions::new();
    open_opts.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        open_opts.mode(file_mode);
    }

    let mut file = open_opts.open(&tmp_path).map_err(|source| CoreError::Io {
        path: tmp_path.clone(),
        source,
    })?;

    let write_result = (|| -> std::io::Result<()> {
        file.write_all(payload)?;
        file.sync_all()?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            file.set_permissions(fs::Permissions::from_mode(file_mode))?;
        }
        Ok(())
    })();
    drop(file);

    if let Err(source) = write_result {
        let _ = fs::remove_file(&tmp_path);
        return Err(CoreError::Io {
            path: tmp_path,
            source,
        });
    }

    #[cfg(windows)]
    let rename_result = retry_rename_on_permission_denied(&tmp_path, path);
    #[cfg(not(windows))]
    let rename_result = fs::rename(&tmp_path, path);

    if let Err(source) = rename_result {
        let _ = fs::remove_file(&tmp_path);
        return Err(CoreError::Io {
            path: path.to_path_buf(),
            source,
        });
    }

    Ok(())
}

#[cfg(any(windows, test))]
const WINDOWS_RENAME_MAX_ATTEMPTS: usize = 3;

#[cfg(any(windows, test))]
const WINDOWS_RENAME_BACKOFFS: [std::time::Duration; WINDOWS_RENAME_MAX_ATTEMPTS - 1] = [
    std::time::Duration::from_millis(25),
    std::time::Duration::from_millis(50),
];

#[cfg(any(windows, test))]
fn retry_rename_with_backoff<F, S>(
    backoffs: &[std::time::Duration],
    mut rename_op: F,
    mut sleep_op: S,
) -> std::io::Result<()>
where
    F: FnMut() -> std::io::Result<()>,
    S: FnMut(std::time::Duration),
{
    let mut attempt = 0;
    loop {
        match rename_op() {
            Ok(()) => return Ok(()),
            Err(err) if err.kind() == ErrorKind::PermissionDenied && attempt < backoffs.len() => {
                sleep_op(backoffs[attempt]);
                attempt += 1;
            }
            Err(err) => return Err(err),
        }
    }
}

#[cfg(any(windows, test))]
fn retry_rename_on_permission_denied(from: &Path, to: &Path) -> std::io::Result<()> {
    retry_rename_with_backoff(
        &WINDOWS_RENAME_BACKOFFS,
        || fs::rename(from, to),
        std::thread::sleep,
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::TempDir;

    #[test]
    fn rejects_non_loopback_bind_host() {
        for bad in [
            "0.0.0.0",
            "::",
            "::1",
            "[::1]",
            "192.168.1.10",
            "127.0.0.2",
            "",
            " 127.0.0.1 ",
            "example.com",
        ] {
            let err = validate_listen_host(bad).expect_err(&format!("expected reject for '{bad}'"));
            assert!(
                matches!(err, CoreError::InvalidListenHost { .. }),
                "unexpected error for '{bad}': {err:?}"
            );
        }

        assert!(validate_listen_host("127.0.0.1").is_ok());
        assert!(validate_listen_host("localhost").is_ok());
    }

    #[test]
    fn rejects_http_remote_upstream_allows_https_and_localhost() {
        for bad in [
            "http://example.com",
            "http://generativelanguage.googleapis.com",
            "http://generativelanguage.googleapis.com/v1beta",
        ] {
            let err =
                validate_upstream_url(bad).expect_err(&format!("expected reject for '{bad}'"));
            assert!(
                matches!(err, CoreError::InvalidBaseUrl { .. }),
                "unexpected error for '{bad}': {err:?}"
            );
        }

        for good in [
            "https://generativelanguage.googleapis.com",
            "https://generativelanguage.googleapis.com/v1beta",
            "http://127.0.0.1:9999",
            "http://localhost:11434/v1",
        ] {
            assert!(
                validate_upstream_url(good).is_ok(),
                "expected valid upstream URL for '{good}'"
            );
        }
    }

    #[test]
    fn defaults_to_schema_version_1() {
        let cfg = Config::default();
        assert_eq!(cfg.schema_version, 1);
        assert_eq!(cfg.listen_host, "127.0.0.1");
        assert_eq!(cfg.listen_port, 4141);
        assert_eq!(
            cfg.gemini.base_url,
            "https://generativelanguage.googleapis.com"
        );
        assert_eq!(cfg.gemini.default_model, "gemini-4-hydrogen");
        assert_eq!(cfg.gemini.timeout_secs, 900);
        assert!(cfg.gemini.api_key.is_none());
        assert!(cfg.usage_dir.is_none());
        assert!(cfg.validate().is_ok());

        let from_empty: Config = serde_json::from_str("{}").expect("empty object deserializes");
        assert_eq!(from_empty, cfg);
    }

    #[test]
    fn rejects_ssrf_host_confusion_and_credential_urls() {
        let bad_urls = [
            "http://localhost@evil.com",
            "http://localhost.evil.com",
            "http://127.0.0.1.evil.com",
            "http://127.0.0.1:80@evil.com",
            "http://@evil.com",
            "https://user:pass@generativelanguage.googleapis.com",
            "http://2130706433:8080",
            "http://0x7f000001:8080",
            "http://[::1]:8080",
            "http://169.254.169.254/latest/meta-data",
            "http://127.0.0.1:0",
            "file:///etc/passwd",
            "ftp://example.com",
            "https://generativelanguage.googleapis.com?key=secret",
            "https://generativelanguage.googleapis.com#frag",
            "https://generativelanguage.googleapis.com/v1beta/../../evil",
            "https://generativelanguage.googleapis.com/v1beta/%2e%2e/evil",
            "https://generativelanguage.googleapis.com\r\nX-Injected: 1",
            "http://localhost\\@evil.com",
            "",
        ];

        for bad in bad_urls {
            let err =
                validate_upstream_url(bad).expect_err(&format!("expected reject for '{bad}'"));
            assert!(
                matches!(err, CoreError::InvalidBaseUrl { .. }),
                "unexpected error for '{bad}': {err:?}"
            );
        }
    }

    #[test]
    fn rejects_zero_port_zero_timeout_bad_schema_and_bad_model() {
        let cfg = Config {
            listen_port: 0,
            ..Default::default()
        };
        assert!(matches!(cfg.validate(), Err(CoreError::InvalidListenPort)));

        let mut cfg = Config::default();
        cfg.gemini.timeout_secs = 0;
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
            "gemini-2.5-pro",
            "gemini-2.5-flash",
            "gemini-2.0-flash",
            "gemini-1.5-pro",
            "gemini-30-pro",
            "gemini-40-flash",
            "gemini-3",
            "gemini-4",
            "gemini-3-",
            "gemini-4.",
            "gemma3:4b",
            "gpt-4o",
        ] {
            let mut cfg = Config::default();
            cfg.gemini.default_model = bad_model.to_string();
            assert!(
                matches!(cfg.validate(), Err(CoreError::InvalidModel { .. })),
                "expected InvalidModel for '{bad_model}'"
            );
        }
    }

    #[test]
    fn rejects_path_traversal_in_config_and_usage_paths() {
        for bad in ["../evil.json", "/tmp/../etc/passwd", "", "   ", "bad\0path"] {
            let err =
                validate_path(Path::new(bad)).expect_err(&format!("expected reject for '{bad}'"));
            assert!(matches!(err, CoreError::InvalidPath { .. }));
        }

        let mut cfg = Config {
            usage_dir: Some(PathBuf::from("../usage")),
            ..Default::default()
        };
        assert!(matches!(cfg.validate(), Err(CoreError::InvalidPath { .. })));
        assert!(matches!(
            cfg.effective_usage_dir(),
            Err(CoreError::InvalidPath { .. })
        ));

        cfg.usage_dir = Some(PathBuf::from("/tmp/custom-usage"));
        assert_eq!(
            cfg.effective_usage_dir().expect("valid override"),
            PathBuf::from("/tmp/custom-usage")
        );

        assert!(validate_path(Path::new("my..config.json")).is_ok());
        assert!(validate_path(Path::new("./local.json")).is_ok());
    }

    #[test]
    fn save_secure_writes_atomic_0600_and_round_trips() {
        let tmp = TempDir::new().expect("temp dir");
        let nested_dir = tmp.path().join("nested").join("agy-companion");
        let cfg_path = nested_dir.join("config.json");

        let cfg = Config {
            listen_port: 4242,
            gemini: GeminiConfig {
                api_key: Some("AIzaSyRoundTripSecretKey1234".to_string()),
                ..Default::default()
            },
            ..Default::default()
        };

        cfg.save_secure(&cfg_path).expect("save_secure succeeds");

        let loaded = Config::load_or_default(&cfg_path).expect("load_or_default succeeds");
        assert_eq!(loaded, cfg);

        let sibling_temps: Vec<_> = fs::read_dir(&nested_dir)
            .expect("read_dir")
            .filter_map(|entry| entry.ok())
            .filter(|entry| entry.file_name().to_string_lossy().contains(".tmp."))
            .collect();
        assert!(
            sibling_temps.is_empty(),
            "temp files left behind: {sibling_temps:?}"
        );

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;

            let file_mode = fs::metadata(&cfg_path)
                .expect("file meta")
                .permissions()
                .mode()
                & 0o777;
            assert_eq!(file_mode, 0o600);

            let dir_mode = fs::metadata(&nested_dir)
                .expect("dir meta")
                .permissions()
                .mode()
                & 0o777;
            assert_eq!(dir_mode, 0o700);

            fs::set_permissions(&cfg_path, fs::Permissions::from_mode(0o644)).expect("set 0644");
            cfg.save_secure(&cfg_path).expect("overwrite save_secure");
            let tightened = fs::metadata(&cfg_path)
                .expect("file meta")
                .permissions()
                .mode()
                & 0o777;
            assert_eq!(tightened, 0o600);
        }

        let invalid = Config {
            listen_host: "0.0.0.0".to_string(),
            ..Default::default()
        };
        let invalid_path = tmp.path().join("should_not_exist.json");
        assert!(invalid.save_secure(&invalid_path).is_err());
        assert!(!invalid_path.exists());
    }

    #[test]
    fn load_or_default_returns_default_when_missing_and_rejects_invalid_file() {
        let tmp = TempDir::new().expect("temp dir");
        let missing = tmp.path().join("does-not-exist.json");
        let loaded = Config::load_or_default(&missing).expect("missing returns default");
        assert_eq!(loaded, Config::default());

        let invalid_host_path = tmp.path().join("bad-host.json");
        fs::write(&invalid_host_path, r#"{"listen_host":"0.0.0.0"}"#).expect("write bad host");
        let err = Config::load_or_default(&invalid_host_path).expect_err("rejects 0.0.0.0");
        assert!(matches!(err, CoreError::InvalidListenHost { .. }));

        let corrupt_path = tmp.path().join("corrupt.json");
        fs::write(&corrupt_path, "{ not valid json").expect("write corrupt");
        let err = Config::load_or_default(&corrupt_path).expect_err("rejects corrupt json");
        assert!(matches!(err, CoreError::ParseConfig { .. }));
    }

    #[test]
    fn debug_redacts_api_key_and_parse_error_leaks_nothing() {
        let secret = "AIzaSyTopSecretKey9999";
        let mut cfg = Config::default();
        cfg.gemini.api_key = Some(secret.to_string());

        let cfg_debug = format!("{cfg:?}");
        let gemini_debug = format!("{:?}", cfg.gemini);
        assert!(
            !cfg_debug.contains(secret),
            "Config Debug leaked secret: {cfg_debug}"
        );
        assert!(
            !gemini_debug.contains(secret),
            "GeminiConfig Debug leaked secret: {gemini_debug}"
        );
        assert!(cfg_debug.contains("AIza...9999"));
        assert!(gemini_debug.contains("AIza...9999"));

        let tmp = TempDir::new().expect("temp dir");
        let corrupt_path = tmp.path().join("corrupt-secret.json");
        fs::write(
            &corrupt_path,
            format!(r#"{{"gemini": {{"api_key": "{secret}", broken"#),
        )
        .expect("write corrupt file");
        let err = Config::load_or_default(&corrupt_path).expect_err("corrupt file fails");
        assert!(matches!(err, CoreError::ParseConfig { .. }));
        let display = format!("{err}");
        let debug = format!("{err:?}");
        assert!(
            !display.contains(secret),
            "ParseConfig Display leaked secret: {display}"
        );
        assert!(
            !debug.contains(secret),
            "ParseConfig Debug leaked secret: {debug}"
        );
    }

    #[test]
    fn resolves_default_paths_and_env_override_without_macos_library_drift() {
        let home = PathBuf::from("/Users/test");

        let default_cfg =
            resolve_config_path(None, Some(home.clone())).expect("default config path");
        assert_eq!(
            default_cfg,
            PathBuf::from("/Users/test/.config/agy-companion/config.json")
        );
        assert!(
            !default_cfg
                .to_string_lossy()
                .contains("Library/Application Support")
        );

        let overridden = resolve_config_path(Some("/custom/cfg.json"), Some(home.clone()))
            .expect("override path");
        assert_eq!(overridden, PathBuf::from("/custom/cfg.json"));

        for blank in ["", "   "] {
            let fallback =
                resolve_config_path(Some(blank), Some(home.clone())).expect("blank falls back");
            assert_eq!(
                fallback,
                PathBuf::from("/Users/test/.config/agy-companion/config.json")
            );
        }

        let err = resolve_config_path(Some("../escape.json"), Some(home.clone()))
            .expect_err("traversal rejected");
        assert!(matches!(err, CoreError::InvalidPath { .. }));

        let err = resolve_config_path(None, None).expect_err("missing home fails");
        assert!(matches!(err, CoreError::HomeDirUnavailable));

        let usage = resolve_usage_dir(Some(home)).expect("default usage dir");
        assert_eq!(
            usage,
            PathBuf::from("/Users/test/.local/share/agy-companion/usage")
        );
        assert!(
            !usage
                .to_string_lossy()
                .contains("Library/Application Support")
        );
        assert!(matches!(
            resolve_usage_dir(None),
            Err(CoreError::HomeDirUnavailable)
        ));
    }

    #[test]
    fn rejects_unanchored_scheme_parser_differential_urls() {
        let bypass_urls = [
            "https:evil.com/v1beta/../secret/://x",
            "https:@evil.com/x://y",
            "https:evil.com/x://y",
            "http:0x7f000001:8080/x://127.0.0.1",
            "http:2130706433:8080/x://127.0.0.1",
            "http:127.1:8080/x://127.0.0.1",
            "http:@127.0.0.1:8080/x://127.0.0.1",
            "http:localhost:8080/x://localhost",
        ];

        for bad in bypass_urls {
            let err =
                validate_upstream_url(bad).expect_err(&format!("expected reject for '{bad}'"));
            assert!(
                matches!(err, CoreError::InvalidBaseUrl { .. }),
                "unexpected error for '{bad}': {err:?}"
            );
        }
    }

    #[test]
    fn rejects_encoded_traversal_nul_empty_ports_and_unicode_whitespace() {
        let bad_urls = [
            "https://generativelanguage.googleapis.com/v1beta/%2e%2e%2fevil",
            "https://generativelanguage.googleapis.com/v1beta/%2E%2E%2Fevil",
            "https://generativelanguage.googleapis.com/v1beta/..%2fevil",
            "https://generativelanguage.googleapis.com/v1beta/a%2f..%2fevil",
            "https://generativelanguage.googleapis.com/v1beta/%2e%2e%5cevil",
            "https://generativelanguage.googleapis.com/v1beta/..%5cevil",
            "http://127.0.0.1:8080/..%2fevil",
            "https://generativelanguage.googleapis.com/v1beta/%252e%252e/evil",
            "https://generativelanguage.googleapis.com/v1beta/..;param/evil",
            "https://generativelanguage.googleapis.com/v1beta/%2e%2e;param/evil",
            "https://generativelanguage.googleapis.com/v1beta/%00evil",
            "https://generativelanguage.googleapis.com/\u{0085}",
            "https://generativelanguage.googleapis.com/\u{00a0}",
            "https://generativelanguage.googleapis.com/\u{200b}",
            "https://generativelanguage.googleapis.com/\u{200c}",
            "https://generativelanguage.googleapis.com/\u{200d}",
            "https://generativelanguage.googleapis.com/\u{feff}",
            "http://127.0.0.1:",
            "http://localhost:",
            "https://generativelanguage.googleapis.com:",
            "https://[::1]:",
            "https://[::1]:0",
            "https://generativelanguage.googleapis.com:abc",
        ];

        for bad in bad_urls {
            let err =
                validate_upstream_url(bad).expect_err(&format!("expected reject for '{bad}'"));
            assert!(
                matches!(err, CoreError::InvalidBaseUrl { .. }),
                "unexpected error for '{bad}': {err:?}"
            );
        }

        let valid_urls = [
            "https://generativelanguage.googleapis.com",
            "https://generativelanguage.googleapis.com/v1beta",
            "https://[::1]:8443",
            "http://127.0.0.1:4141",
            "http://localhost:11434/v1",
        ];
        for good in valid_urls {
            assert!(
                validate_upstream_url(good).is_ok(),
                "expected valid upstream URL for '{good}'"
            );
        }
    }

    #[test]
    fn rejects_control_characters_in_api_key_without_leaking_secret() {
        for bad_key in [
            "AIzaSyValidPrefix\r\nX-Injected: evil",
            "AIzaSyValidPrefix\0",
            "AIzaSyValidPrefix\u{0085}",
        ] {
            let cfg = Config {
                gemini: GeminiConfig {
                    api_key: Some(bad_key.to_string()),
                    ..Default::default()
                },
                ..Default::default()
            };

            let err = cfg
                .validate()
                .expect_err("expected InvalidApiKey for key with control characters");
            assert!(
                matches!(err, CoreError::InvalidApiKey),
                "unexpected error variant: {err:?}"
            );

            let display = format!("{err}");
            let debug = format!("{err:?}");
            assert!(
                !display.contains("AIzaSyValidPrefix") && !display.contains("X-Injected"),
                "Display leaked api_key: {display}"
            );
            assert!(
                !debug.contains("AIzaSyValidPrefix") && !debug.contains("X-Injected"),
                "Debug leaked api_key: {debug}"
            );

            let tmp = TempDir::new().expect("temp dir");
            let cfg_path = tmp.path().join("rejected-key.json");
            assert!(matches!(
                cfg.save_secure(&cfg_path),
                Err(CoreError::InvalidApiKey)
            ));
            assert!(!cfg_path.exists());
        }

        let valid_cfg = Config {
            gemini: GeminiConfig {
                api_key: Some("AIzaSyValidKey1234567890".to_string()),
                ..Default::default()
            },
            ..Default::default()
        };
        assert!(valid_cfg.validate().is_ok());
    }

    #[test]
    fn rejects_multi_encoded_malformed_percent_and_control_byte_url_paths() {
        let bad_urls = [
            // All 14 double/triple-encoded payloads from r2_challenger_1 Finding 1
            "https://generativelanguage.googleapis.com/v1beta/..%252fevil",
            "https://generativelanguage.googleapis.com/v1beta/%2e%2e%252fevil",
            "http://127.0.0.1:8080/v1beta/..%252fevil",
            "https://generativelanguage.googleapis.com/v1beta/%252f",
            "https://generativelanguage.googleapis.com/v1beta/%252F",
            "https://generativelanguage.googleapis.com/v1beta/..%255cevil",
            "https://generativelanguage.googleapis.com/v1beta/%2e%2e%255cevil",
            "https://generativelanguage.googleapis.com/v1beta/%255c",
            "https://generativelanguage.googleapis.com/v1beta/%255C",
            "https://generativelanguage.googleapis.com/v1beta/%2500evil",
            "https://generativelanguage.googleapis.com/v1beta/..%253bparam/evil",
            "https://generativelanguage.googleapis.com/v1beta/%2e%2e%253bparam/evil",
            "https://generativelanguage.googleapis.com/v1beta/%25252e%25252e/evil",
            "https://generativelanguage.googleapis.com/v1beta/%25252fevil",
            // Quadruple-encoded (guards against finite-pass denylists)
            "https://generativelanguage.googleapis.com/v1beta/%2525252e%2525252e/evil",
            "https://generativelanguage.googleapis.com/v1beta/%2525252fevil",
            "https://generativelanguage.googleapis.com/v1beta/%2525255cevil",
            "https://generativelanguage.googleapis.com/v1beta/%25252500evil",
            // Uppercase & mixed-case hex
            "https://generativelanguage.googleapis.com/v1beta/%252E%252Fevil",
            "https://generativelanguage.googleapis.com/v1beta/%2E%2e%252Fevil",
            "https://generativelanguage.googleapis.com/v1beta/..%253Bparam/evil",
            "https://generativelanguage.googleapis.com/v1beta/%25252E%25252e/evil",
            // Malformed percent sequences
            "https://generativelanguage.googleapis.com/v1beta/%",
            "https://generativelanguage.googleapis.com/v1beta/%2",
            "https://generativelanguage.googleapis.com/v1beta/%ZZ",
            "https://generativelanguage.googleapis.com/v1beta/%2g",
            "https://generativelanguage.googleapis.com/v1beta/%g2",
            "https://generativelanguage.googleapis.com/v1beta/%/evil",
            "https://generativelanguage.googleapis.com/v1beta/foo%2/bar",
            "http://127.0.0.1:8080/v1beta/%ZZ",
            // Bare-% smuggling & non-standard %u encoding
            "https://generativelanguage.googleapis.com/v1beta/%%32%65%%32%65/evil",
            "https://generativelanguage.googleapis.com/v1beta/..%%32%66evil",
            "https://generativelanguage.googleapis.com/v1beta/%%32%35",
            "https://generativelanguage.googleapis.com/v1beta/%2%35",
            "https://generativelanguage.googleapis.com/v1beta/%u002e",
            // Encoded query ('?'), fragment ('#'), and space (%20) smuggling
            "https://generativelanguage.googleapis.com/v1beta%3fkey=secret",
            "https://generativelanguage.googleapis.com/v1beta%3Fkey=secret",
            "https://generativelanguage.googleapis.com/v1beta/%3f",
            "https://generativelanguage.googleapis.com/v1beta%23frag",
            "https://generativelanguage.googleapis.com/v1beta/%23",
            "https://generativelanguage.googleapis.com/v1beta/%20",
            "https://generativelanguage.googleapis.com/v1beta/..%20",
            "http://127.0.0.1:8080/v1beta%3fkey=1",
            "http://127.0.0.1:8080/v1beta%23frag",
            // Percent-encoded non-ASCII & overlong UTF-8
            "https://generativelanguage.googleapis.com/v1beta/%ef%bc%8e%ef%bc%8e/evil",
            "https://generativelanguage.googleapis.com/v1beta/%c0%af",
            "https://generativelanguage.googleapis.com/v1beta/%e0%80%af",
            "https://generativelanguage.googleapis.com/v1beta/%e2%80%ae",
            "https://generativelanguage.googleapis.com/v1beta/%c2%85",
            // WHATWG PATH encode-set & non-pchar ASCII characters
            "https://generativelanguage.googleapis.com/v1beta/{foo}",
            "https://generativelanguage.googleapis.com/v1beta/[foo]",
            "https://generativelanguage.googleapis.com/v1beta/a^b",
            "https://generativelanguage.googleapis.com/v1beta/a|b",
            "https://generativelanguage.googleapis.com/v1beta/a<b",
            "https://generativelanguage.googleapis.com/v1beta/a\"b",
            "https://generativelanguage.googleapis.com/v1beta/a`b",
        ];

        for bad in bad_urls {
            let err =
                validate_upstream_url(bad).expect_err(&format!("expected reject for '{bad}'"));
            assert!(
                matches!(err, CoreError::InvalidBaseUrl { .. }),
                "unexpected error for '{bad}': {err:?}"
            );
        }

        // Exhaustive percent-encoded C0 (0x00..=0x1F) and DEL (0x7F) in lower & upper hex
        for byte in (0x00u8..=0x1Fu8).chain(std::iter::once(0x7Fu8)) {
            for encoded in [format!("%{byte:02x}"), format!("%{byte:02X}")] {
                let url = format!("https://generativelanguage.googleapis.com/v1beta/a{encoded}b");
                let err = validate_upstream_url(&url)
                    .expect_err(&format!("expected reject for control byte '{encoded}'"));
                assert!(
                    matches!(err, CoreError::InvalidBaseUrl { .. }),
                    "unexpected error for '{url}': {err:?}"
                );
            }
        }
    }

    #[test]
    fn rejects_single_dot_and_matrix_parameter_url_paths() {
        let bad_urls = [
            // All 5 Finding 2 payloads from r2_challenger_1
            "https://generativelanguage.googleapis.com/v1beta/.%3bfoo",
            "https://generativelanguage.googleapis.com/v1beta/%2e%3b",
            "https://generativelanguage.googleapis.com/v1beta/.;foo/bar",
            "https://generativelanguage.googleapis.com/v1beta/./bar",
            "https://generativelanguage.googleapis.com/v1beta/%2e/bar",
            // Trailing dot / double-dot / matrix / encoded variants
            "https://generativelanguage.googleapis.com/v1beta/.",
            "https://generativelanguage.googleapis.com/.",
            "http://127.0.0.1:8080/.",
            "https://generativelanguage.googleapis.com/v1beta/..",
            "https://generativelanguage.googleapis.com/..",
            "http://127.0.0.1:8080/..",
            "https://generativelanguage.googleapis.com/v1beta/.;",
            "https://generativelanguage.googleapis.com/.;",
            "https://generativelanguage.googleapis.com/v1beta/..;",
            "https://generativelanguage.googleapis.com/..;",
            "https://generativelanguage.googleapis.com/v1beta/%2e",
            "https://generativelanguage.googleapis.com/v1beta/%2E",
            "https://generativelanguage.googleapis.com/v1beta/%2e/",
            "https://generativelanguage.googleapis.com/v1beta/%2E/",
            "https://generativelanguage.googleapis.com/v1beta/%2e;foo",
            "https://generativelanguage.googleapis.com/v1beta/%2E;foo/bar",
            "https://generativelanguage.googleapis.com/v1beta/%2e%3bfoo",
            "https://generativelanguage.googleapis.com/v1beta/%2E%3Bfoo",
            "https://generativelanguage.googleapis.com/v1beta/%2e%2e%3b",
            "https://generativelanguage.googleapis.com/v1beta/%2e%2e%3bfoo",
            "https://generativelanguage.googleapis.com/v1beta/.%2e%3bfoo",
            "https://generativelanguage.googleapis.com/v1beta/%2e.%3bfoo",
            "https://generativelanguage.googleapis.com/v1beta/%2E%2E%3Bfoo",
            "https://generativelanguage.googleapis.com/v1beta/..;foo",
            "https://generativelanguage.googleapis.com/v1beta/.;foo",
            "https://generativelanguage.googleapis.com/v1beta/;../admin",
            "https://generativelanguage.googleapis.com/v1beta/foo;bar",
            "http://127.0.0.1:8080/v1beta/./bar",
            "http://127.0.0.1:8080/v1beta/.;foo",
        ];

        for bad in bad_urls {
            let err =
                validate_upstream_url(bad).expect_err(&format!("expected reject for '{bad}'"));
            assert!(
                matches!(err, CoreError::InvalidBaseUrl { .. }),
                "unexpected error for '{bad}': {err:?}"
            );
        }
    }

    #[test]
    fn rejects_non_ascii_lookalike_and_bidi_invisible_url_chars() {
        let bad_urls = [
            // All 9 Finding 3 payloads from r2_challenger_1
            "https://generativelanguage.googleapis.com/v1beta/\u{ff0e}\u{ff0e}/evil",
            "https://generativelanguage.googleapis.com/v1beta/\u{ff0f}evil",
            "https://generativelanguage.googleapis.com/v1beta/\u{2215}evil",
            "https://generativelanguage.googleapis.com/v1beta/\u{2044}evil",
            "https://generativelanguage.googleapis.com/v1beta/\u{3002}\u{3002}/evil",
            "https://generativelanguage.googleapis.com/v1beta/\u{ff3c}evil",
            "https://generativelanguage.googleapis.com/v1beta/\u{2060}evil",
            "https://generativelanguage.googleapis.com/v1beta/\u{00ad}evil",
            "https://generativelanguage.googleapis.com/v1beta/\u{202e}evil",
            // Additional bidi control / directionality characters & loopback variant
            "https://generativelanguage.googleapis.com/v1beta/\u{200e}evil",
            "https://generativelanguage.googleapis.com/v1beta/\u{200f}evil",
            "https://generativelanguage.googleapis.com/v1beta/\u{061c}evil",
            "https://generativelanguage.googleapis.com/v1beta/\u{202a}evil",
            "https://generativelanguage.googleapis.com/v1beta/\u{202b}evil",
            "https://generativelanguage.googleapis.com/v1beta/\u{202c}evil",
            "https://generativelanguage.googleapis.com/v1beta/\u{202d}evil",
            "https://generativelanguage.googleapis.com/v1beta/\u{2066}evil",
            "https://generativelanguage.googleapis.com/v1beta/\u{2067}evil",
            "https://generativelanguage.googleapis.com/v1beta/\u{2068}evil",
            "https://generativelanguage.googleapis.com/v1beta/\u{2069}evil",
            "http://127.0.0.1:8080/v1beta/\u{ff0e}\u{ff0e}/evil",
        ];

        for bad in bad_urls {
            let err =
                validate_upstream_url(bad).expect_err(&format!("expected reject for '{bad:?}'"));
            assert!(
                matches!(err, CoreError::InvalidBaseUrl { .. }),
                "unexpected error for '{bad:?}': {err:?}"
            );
        }
    }

    #[test]
    fn rejects_unicode_control_whitespace_and_smuggling_in_default_model() {
        let bad_models = [
            // Unicode C1 control & Unicode whitespace
            "gemini-4-hydrogen\u{0085}",
            "gemini-4-hydrogen\u{0090}",
            "gemini-4-hydrogen\u{009b}",
            "gemini-4-hydrogen\u{00a0}",
            "gemini-4-hydrogen\u{2003}",
            "gemini-4-hydrogen\u{3000}",
            // Invisible, zero-width & bidi format chars
            "gemini-4-hydrogen\u{200b}",
            "gemini-4-hydrogen\u{200c}",
            "gemini-4-hydrogen\u{200d}",
            "gemini-4-hydrogen\u{feff}",
            "gemini-4-hydrogen\u{2060}",
            "gemini-4-hydrogen\u{00ad}",
            "gemini-4-hydrogen\u{202e}",
            // ASCII control & whitespace
            "gemini-4-hydrogen\0",
            "gemini-4-hydrogen\r",
            "gemini-4-hydrogen\x7f",
            "gemini-4-hydrogen\t",
            " gemini-4-hydrogen",
            "gemini-4-hydrogen ",
            // Bare dot & percent-encoded / matrix smuggling in model segment
            ".",
            "gemini-4-hydrogen%2f..%2fevil",
            "gemini-4-hydrogen%3fkey=secret",
            "gemini-4-hydrogen%23frag",
            "gemini-4-hydrogen;param",
        ];

        for bad_model in bad_models {
            let mut cfg = Config::default();
            cfg.gemini.default_model = bad_model.to_string();
            assert!(
                matches!(cfg.validate(), Err(CoreError::InvalidModel { .. })),
                "expected InvalidModel for '{bad_model:?}'"
            );
        }
    }

    #[test]
    fn rejects_leading_zero_and_malformed_ports_in_upstream_urls() {
        let bad_urls = [
            "http://127.0.0.1:0",
            "http://127.0.0.1:0000",
            "http://127.0.0.1:080",
            "http://127.0.0.1:0100",
            "http://127.0.0.1:00080",
            "http://127.0.0.1:0000000000000080",
            "http://localhost:04141",
            "https://generativelanguage.googleapis.com:0443",
            "https://[::1]:08443",
            "http://127.0.0.1:65536",
            "http://127.0.0.1:999999999999999999999",
            "http://127.0.0.1:+80",
            "http://127.0.0.1:-80",
            "http://127.0.0.1:abc",
            "http://127.0.0.1:",
            "https://[::1]:",
        ];

        for bad in bad_urls {
            let err =
                validate_upstream_url(bad).expect_err(&format!("expected reject for '{bad}'"));
            assert!(
                matches!(err, CoreError::InvalidBaseUrl { .. }),
                "unexpected error for '{bad}': {err:?}"
            );
        }
    }

    #[test]
    fn accepts_valid_upstream_urls_and_default_models_without_false_positives() {
        let valid_urls = [
            // Standard & trailing-slash Gemini base URLs
            "https://generativelanguage.googleapis.com",
            "https://generativelanguage.googleapis.com/v1beta",
            "https://generativelanguage.googleapis.com/v1beta/",
            "https://generativelanguage.googleapis.com:443/v1beta",
            "https://generativelanguage.googleapis.com:8443/v1beta",
            // Legitimate dotted, multi-dot, unreserved, double-slash, and sub-delim paths
            "https://api.example.com/v1.2/gemini",
            "https://api.example.com/v1.2.3/v1beta",
            "https://api.example.com/a..b/gemini",
            "https://api.example.com/.../gemini",
            "https://api.example.com/.well-known/gemini",
            "https://api.example.com/v1beta//models",
            "https://api.example.com/v1-beta_2/proxy~1",
            "https://api.example.com/path@ok",
            "https://aiplatform.googleapis.com/v1/projects/p/locations/us-central1/publishers/google",
            "https://xn--bcher-kva.example/v1beta",
            // Benign single-pass RFC 3986 %HH encoding of safe pchar bytes
            "https://api.example.com/v1beta/%7Edev",
            "https://api.example.com/org%2Bprod/v1beta",
            "https://api.example.com/v1%2Dbeta/gemini",
            // Valid IPv6 HTTPS & port boundaries
            "https://[::1]",
            "https://[::1]/v1beta",
            "https://[::1]:1",
            "https://[::1]:8443",
            "https://[::1]:65535/v1beta",
            "https://[2001:db8::1]:443/v1beta",
            // Valid loopback HTTP, port boundaries, and mixed-case scheme/host
            "http://127.0.0.1",
            "http://127.0.0.1:1",
            "http://127.0.0.1:80",
            "http://127.0.0.1:4141",
            "http://127.0.0.1:9999",
            "http://127.0.0.1:65535/v1beta",
            "http://localhost",
            "http://localhost:11434/v1",
            "HTTP://LOCALHOST:8080",
            "HTTPS://generativelanguage.googleapis.com",
            "HtTp://LoCaLhOsT:8080/v1beta",
        ];

        for good in valid_urls {
            assert!(
                validate_upstream_url(good).is_ok(),
                "false positive rejection for valid upstream URL '{good}'"
            );
        }

        let valid_models = [
            "gemini-4-hydrogen",
            "gemini-4-argon",
            "gemini-4-neon",
            "gemini-4-carbon",
            "gemini-4.1-pro",
            "gemini-3.8-flash",
            "gemini-3.8-flash-preview",
            "gemini-3.9-pro",
            "gemini-3.10-flash",
        ];

        for good_model in valid_models {
            let mut cfg = Config::default();
            cfg.gemini.default_model = good_model.to_string();
            assert!(
                cfg.validate().is_ok(),
                "false positive rejection for valid default_model '{good_model}'"
            );
        }

        let pre_38_models = [
            "gemini-3.7-flash",
            "gemini-3.6-flash",
            "gemini-3.5-flash-lite",
            "gemini-3.1-pro-preview",
            "gemini-3.1-flash-lite-preview",
            "gemini-3.0-pro",
            "gemini-3-flash-preview",
            "gemini-2.5-pro",
            "gemini-3.8-",
            "gemini-3.8flash",
        ];

        for old_model in pre_38_models {
            let mut cfg = Config::default();
            cfg.gemini.default_model = old_model.to_string();
            assert!(
                cfg.validate().is_err(),
                "expected pre-3.8 or malformed model '{old_model}' to be rejected"
            );
        }
    }

    #[test]
    fn resolves_unix_xdg_config_and_usage_precedence() {
        assert_eq!(XDG_CONFIG_HOME_ENV_VAR, "XDG_CONFIG_HOME");
        assert_eq!(XDG_DATA_HOME_ENV_VAR, "XDG_DATA_HOME");

        let home = PathBuf::from("/Users/test");
        let macos_app_support = PathBuf::from("/Users/test/Library/Application Support");

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
        assert!(matches!(invalid_env, CoreError::InvalidPath { .. }));

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

        // NUL bytes and '..' traversal fail closed on both absolute and relative XDG inputs.
        for bad in [
            "/tmp/xdg/../evil",
            "/var/..",
            "/../etc",
            "../escape",
            "..",
            "foo/../bar",
            "/tmp/xdg\0evil",
            "rel\0evil",
        ] {
            let cfg_err = resolve_config_path_for_platform(
                None,
                Some(bad),
                None,
                Some(home.clone()),
                TargetPlatform::Unix,
            )
            .expect_err(&format!("expected InvalidPath for XDG_CONFIG_HOME='{bad}'"));
            assert!(
                matches!(cfg_err, CoreError::InvalidPath { .. }),
                "unexpected error for XDG_CONFIG_HOME='{bad}': {cfg_err:?}"
            );

            let usage_err = resolve_usage_dir_for_platform(
                Some(bad),
                None,
                Some(home.clone()),
                TargetPlatform::Unix,
            )
            .expect_err(&format!("expected InvalidPath for XDG_DATA_HOME='{bad}'"));
            assert!(
                matches!(usage_err, CoreError::InvalidPath { .. }),
                "unexpected error for XDG_DATA_HOME='{bad}': {usage_err:?}"
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
        assert!(matches!(bad_env_cfg, CoreError::InvalidPath { .. }));

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

        // Windows traversal & NUL payloads fail closed even when tests run on macOS/Linux.
        for bad_win_xdg in [
            r"C:\Users\alice\..\evil",
            "C:/Users/alice/../evil",
            r"C:\a\b\..",
            r"C:..\evil",
            "C:..",
            r"..\escape",
            r"\\server\share\..\evil",
            r"\\?\C:\Users\..\Admin",
            "C:\\Users\\alice\\xdg\0bad",
        ] {
            let cfg_err = resolve_config_path_for_platform(
                None,
                Some(bad_win_xdg),
                Some(appdata.clone()),
                Some(userprofile.clone()),
                TargetPlatform::Windows,
            )
            .expect_err(&format!(
                "expected InvalidPath for Windows XDG_CONFIG_HOME='{bad_win_xdg}'"
            ));
            assert!(
                matches!(cfg_err, CoreError::InvalidPath { .. }),
                "unexpected error for '{bad_win_xdg}': {cfg_err:?}"
            );

            let usage_err = resolve_usage_dir_for_platform(
                Some(bad_win_xdg),
                Some(localappdata.clone()),
                Some(userprofile.clone()),
                TargetPlatform::Windows,
            )
            .expect_err(&format!(
                "expected InvalidPath for Windows XDG_DATA_HOME='{bad_win_xdg}'"
            ));
            assert!(
                matches!(usage_err, CoreError::InvalidPath { .. }),
                "unexpected error for '{bad_win_xdg}': {usage_err:?}"
            );
        }
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

        // 2. Traversal and NUL in home fail closed with InvalidPath on Unix (and public wrappers).
        for bad_unix_home in ["/Users/alice/../evil", "/Users/alice\0evil"] {
            let bad_home = Some(PathBuf::from(bad_unix_home));

            let cfg_err = resolve_config_path_for_platform(
                None,
                None,
                None,
                bad_home.clone(),
                TargetPlatform::Unix,
            )
            .unwrap_err();
            assert!(
                matches!(cfg_err, CoreError::InvalidPath { .. }),
                "expected InvalidPath for Unix config home='{bad_unix_home}', got {cfg_err:?}"
            );

            let usage_err =
                resolve_usage_dir_for_platform(None, None, bad_home.clone(), TargetPlatform::Unix)
                    .unwrap_err();
            assert!(
                matches!(usage_err, CoreError::InvalidPath { .. }),
                "expected InvalidPath for Unix usage home='{bad_unix_home}', got {usage_err:?}"
            );

            let pub_cfg_err = resolve_config_path(None, bad_home.clone()).unwrap_err();
            assert!(
                matches!(pub_cfg_err, CoreError::InvalidPath { .. }),
                "expected InvalidPath for public resolve_config_path home='{bad_unix_home}', got {pub_cfg_err:?}"
            );

            let pub_usage_err = resolve_usage_dir(bad_home).unwrap_err();
            assert!(
                matches!(pub_usage_err, CoreError::InvalidPath { .. }),
                "expected InvalidPath for public resolve_usage_dir home='{bad_unix_home}', got {pub_usage_err:?}"
            );
        }

        // 3. Traversal and NUL in Windows home fail closed with InvalidPath.
        for bad_win_home in [r"C:\Users\..\evil", "C:\\Users\\alice\0evil"] {
            let bad_home = Some(PathBuf::from(bad_win_home));

            let cfg_err = resolve_config_path_for_platform(
                None,
                None,
                None,
                bad_home.clone(),
                TargetPlatform::Windows,
            )
            .unwrap_err();
            assert!(
                matches!(cfg_err, CoreError::InvalidPath { .. }),
                "expected InvalidPath for Windows config home='{bad_win_home}', got {cfg_err:?}"
            );

            let usage_err =
                resolve_usage_dir_for_platform(None, None, bad_home, TargetPlatform::Windows)
                    .unwrap_err();
            assert!(
                matches!(usage_err, CoreError::InvalidPath { .. }),
                "expected InvalidPath for Windows usage home='{bad_win_home}', got {usage_err:?}"
            );
        }

        // 4. Traversal and NUL in Windows platform_config_dir / platform_data_local_dir fail closed
        //    even when a valid home fallback is present.
        for bad_known_folder in [r"C:\Users\..\evil", "C:\\Users\\alice\0evil"] {
            let bad_dir = Some(PathBuf::from(bad_known_folder));

            let cfg_err = resolve_config_path_for_platform(
                None,
                None,
                bad_dir.clone(),
                Some(valid_win_home.clone()),
                TargetPlatform::Windows,
            )
            .unwrap_err();
            assert!(
                matches!(cfg_err, CoreError::InvalidPath { .. }),
                "expected InvalidPath for Windows platform_config_dir='{bad_known_folder}', got {cfg_err:?}"
            );

            let usage_err = resolve_usage_dir_for_platform(
                None,
                bad_dir,
                Some(valid_win_home.clone()),
                TargetPlatform::Windows,
            )
            .unwrap_err();
            assert!(
                matches!(usage_err, CoreError::InvalidPath { .. }),
                "expected InvalidPath for Windows platform_data_local_dir='{bad_known_folder}', got {usage_err:?}"
            );
        }
    }

    #[test]
    fn retry_rename_with_backoff_retries_only_permission_denied() {
        use std::time::Duration;

        // 1. Succeeds on attempt 1 -> 1 call, 0 sleeps.
        let mut attempts = 0;
        let mut sleeps = Vec::new();
        let res = retry_rename_with_backoff(
            &WINDOWS_RENAME_BACKOFFS,
            || {
                attempts += 1;
                Ok(())
            },
            |d| sleeps.push(d),
        );
        assert!(res.is_ok());
        assert_eq!(attempts, 1);
        assert!(sleeps.is_empty());

        // 2. PermissionDenied on attempt 1, succeeds on attempt 2 -> 2 calls, [25ms].
        let mut attempts = 0;
        let mut sleeps = Vec::new();
        let res = retry_rename_with_backoff(
            &WINDOWS_RENAME_BACKOFFS,
            || {
                attempts += 1;
                if attempts == 1 {
                    Err(std::io::Error::new(ErrorKind::PermissionDenied, "locked"))
                } else {
                    Ok(())
                }
            },
            |d| sleeps.push(d),
        );
        assert!(res.is_ok());
        assert_eq!(attempts, 2);
        assert_eq!(sleeps, vec![Duration::from_millis(25)]);

        // 3. PermissionDenied on attempts 1 & 2, succeeds on attempt 3 -> 3 calls, [25ms, 50ms].
        let mut attempts = 0;
        let mut sleeps = Vec::new();
        let res = retry_rename_with_backoff(
            &WINDOWS_RENAME_BACKOFFS,
            || {
                attempts += 1;
                if attempts < 3 {
                    Err(std::io::Error::new(ErrorKind::PermissionDenied, "locked"))
                } else {
                    Ok(())
                }
            },
            |d| sleeps.push(d),
        );
        assert!(res.is_ok());
        assert_eq!(attempts, 3);
        assert_eq!(
            sleeps,
            vec![Duration::from_millis(25), Duration::from_millis(50)]
        );

        // 4. Exhausted (all 3 PermissionDenied) -> Err(PermissionDenied), 3 calls, NO 3rd sleep.
        let mut attempts = 0;
        let mut sleeps = Vec::new();
        let err = retry_rename_with_backoff(
            &WINDOWS_RENAME_BACKOFFS,
            || {
                attempts += 1;
                Err(std::io::Error::new(ErrorKind::PermissionDenied, "locked"))
            },
            |d| sleeps.push(d),
        )
        .expect_err("exhausted retries must fail");
        assert_eq!(err.kind(), ErrorKind::PermissionDenied);
        assert_eq!(attempts, WINDOWS_RENAME_MAX_ATTEMPTS);
        assert_eq!(
            sleeps,
            vec![Duration::from_millis(25), Duration::from_millis(50)]
        );

        // 5. Non-PermissionDenied on attempt 1 aborts immediately -> 1 call, 0 sleeps.
        for kind in [
            ErrorKind::NotFound,
            ErrorKind::AlreadyExists,
            ErrorKind::InvalidInput,
            ErrorKind::Other,
        ] {
            let mut attempts = 0;
            let mut sleeps = Vec::new();
            let err = retry_rename_with_backoff(
                &WINDOWS_RENAME_BACKOFFS,
                || {
                    attempts += 1;
                    Err(std::io::Error::new(kind, "non-retryable"))
                },
                |d| sleeps.push(d),
            )
            .expect_err("non-PermissionDenied must fail immediately");
            assert_eq!(err.kind(), kind);
            assert_eq!(attempts, 1);
            assert!(sleeps.is_empty());
        }

        // 6. PermissionDenied on attempt 1, then NotFound on attempt 2 -> immediate abort.
        let mut attempts = 0;
        let mut sleeps = Vec::new();
        let err = retry_rename_with_backoff(
            &WINDOWS_RENAME_BACKOFFS,
            || {
                attempts += 1;
                if attempts == 1 {
                    Err(std::io::Error::new(ErrorKind::PermissionDenied, "locked"))
                } else {
                    Err(std::io::Error::new(ErrorKind::NotFound, "gone"))
                }
            },
            |d| sleeps.push(d),
        )
        .expect_err("mid-sequence non-PermissionDenied must abort");
        assert_eq!(err.kind(), ErrorKind::NotFound);
        assert_eq!(attempts, 2);
        assert_eq!(sleeps, vec![Duration::from_millis(25)]);

        // 7. Real filesystem smoke test of retry_rename_on_permission_denied replacing a file.
        let tmp = TempDir::new().expect("temp dir");
        let src = tmp.path().join("config.json.tmp");
        let dst = tmp.path().join("config.json");
        fs::write(&dst, "old").expect("write old dst");
        fs::write(&src, "new").expect("write new src");

        retry_rename_on_permission_denied(&src, &dst).expect("rename smoke test succeeds");
        assert!(!src.exists());
        assert_eq!(fs::read_to_string(&dst).expect("read dst"), "new");
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
        assert!(matches!(err, CoreError::Io { .. }));

        let leftover_temps: Vec<_> = fs::read_dir(tmp.path())
            .expect("read_dir")
            .filter_map(|entry| entry.ok())
            .filter(|entry| entry.file_name().to_string_lossy().contains(".tmp."))
            .collect();
        assert!(
            leftover_temps.is_empty(),
            "temp files left behind after failed rename: {leftover_temps:?}"
        );
    }

    #[cfg(unix)]
    #[test]
    fn load_and_save_reject_symlink_config_files_without_mutating_target() {
        use std::os::unix::fs::symlink;

        let tmp = TempDir::new().expect("temp dir");

        // (i) Symlink to a valid config file.
        let target = tmp.path().join("victim.json");
        let mut target_cfg = Config::default();
        target_cfg.gemini.default_model = "gemini-4-victim-sentinel".to_string();
        target_cfg
            .save_secure(&target)
            .expect("write target config");
        let original_target_bytes = fs::read(&target).expect("read original target");

        let sym_to_file = tmp.path().join("sym_config.json");
        symlink(&target, &sym_to_file).expect("create symlink to target file");

        // (ii) Dangling symlink.
        let dangling_target = tmp.path().join("does_not_exist.json");
        let sym_dangling = tmp.path().join("dangling_config.json");
        symlink(&dangling_target, &sym_dangling).expect("create dangling symlink");

        // (iii) Trailing-slash symlink to file (and trailing-slash symlink to dir).
        let sym_trailing_slash = PathBuf::from(format!("{}/", sym_to_file.display()));
        let real_dir = tmp.path().join("real_dir");
        fs::create_dir(&real_dir).expect("create real dir");
        let sym_to_dir = tmp.path().join("sym_dir");
        symlink(&real_dir, &sym_to_dir).expect("create symlink to dir");
        let sym_dir_trailing_slash = PathBuf::from(format!("{}/", sym_to_dir.display()));

        let mut updated_cfg = Config::default();
        updated_cfg.gemini.default_model = "gemini-3.8-flash".to_string();

        for (label, path) in [
            ("symlink to valid config", &sym_to_file),
            ("dangling symlink", &sym_dangling),
            ("trailing-slash symlink to file", &sym_trailing_slash),
            ("trailing-slash symlink to dir", &sym_dir_trailing_slash),
        ] {
            let load_err = Config::load_or_default(path)
                .expect_err(&format!("load_or_default must reject {label}"));
            assert!(
                matches!(
                    &load_err,
                    CoreError::InvalidPath { reason } if reason.contains("symbolic link")
                ),
                "load_or_default({label}) returned unexpected error: {load_err:?}"
            );

            let save_err = updated_cfg
                .save_secure(path)
                .expect_err(&format!("save_secure must reject {label}"));
            assert!(
                matches!(
                    &save_err,
                    CoreError::InvalidPath { reason } if reason.contains("symbolic link")
                ),
                "save_secure({label}) returned unexpected error: {save_err:?}"
            );
        }

        // Target file must remain byte-for-byte untouched, symlinks untouched, zero .tmp. files created.
        assert_eq!(
            fs::read(&target).expect("re-read target"),
            original_target_bytes,
            "target file must remain byte-for-byte untouched"
        );
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
    fn rejects_non_ascii_and_internal_space_in_api_key_without_leaking_secret() {
        for bad_key in [
            "AIzaSy Space Key 1234",
            "AIzaSy\u{00A0}Key1234",
            "AIzaSy\u{1F511}Key1234",
            "\tAIzaSyKey1234",
            "AIzaSyKey1234\t",
            "AIzaSy\u{007F}Key1234",
        ] {
            let cfg = Config {
                gemini: GeminiConfig {
                    api_key: Some(bad_key.to_string()),
                    ..Default::default()
                },
                ..Default::default()
            };

            let err = cfg
                .validate()
                .expect_err(&format!("expected InvalidApiKey for '{bad_key:?}'"));
            assert!(
                matches!(err, CoreError::InvalidApiKey),
                "unexpected error for '{bad_key:?}': {err:?}"
            );

            let display = format!("{err}");
            let debug = format!("{err:?}");
            assert!(
                !display.contains("AIzaSy") && !debug.contains("AIzaSy"),
                "error leaked secret: display={display}, debug={debug}"
            );

            let tmp = TempDir::new().expect("temp dir");
            let cfg_path = tmp.path().join("rejected-key.json");
            assert!(matches!(
                cfg.save_secure(&cfg_path),
                Err(CoreError::InvalidApiKey)
            ));
            assert!(!cfg_path.exists());
        }

        for good_key in ["", "   ", "  AIzaSyValidKey1234  ", "AIzaSyValidKey1234"] {
            let cfg = Config {
                gemini: GeminiConfig {
                    api_key: Some(good_key.to_string()),
                    ..Default::default()
                },
                ..Default::default()
            };
            assert!(
                cfg.validate().is_ok(),
                "expected valid config for api_key='{good_key}'"
            );
        }
    }

    #[test]
    fn load_or_default_rejects_oversized_file_directory_and_fifo_without_blocking() {
        let tmp = TempDir::new().expect("temp dir");

        // 1. Oversized (1 MiB + 1 byte) regular file -> rejected before reading full file.
        let oversized_path = tmp.path().join("oversized.json");
        let mut oversized = br#"{"schema_version":1}"#.to_vec();
        oversized.resize(1024 * 1024 + 1, b' ');
        fs::write(&oversized_path, oversized).expect("write oversized");
        let err = Config::load_or_default(&oversized_path).expect_err("oversized must fail");
        assert!(
            matches!(err, CoreError::InvalidPath { .. }),
            "expected InvalidPath for oversized file, got {err:?}"
        );

        // 2. Directory at config path -> rejected with InvalidPath.
        let dir_path = tmp.path().join("config_dir");
        fs::create_dir(&dir_path).expect("create dir");
        let err = Config::load_or_default(&dir_path).expect_err("directory must fail");
        assert!(
            matches!(err, CoreError::InvalidPath { .. }),
            "expected InvalidPath for directory, got {err:?}"
        );

        // 3. Unix FIFO -> rejected immediately (< 1s) without blocking open(2).
        #[cfg(unix)]
        {
            let fifo_path = tmp.path().join("config.fifo");
            let status = std::process::Command::new("mkfifo")
                .arg(&fifo_path)
                .status()
                .expect("run mkfifo");
            assert!(status.success(), "mkfifo failed");

            let (tx, rx) = std::sync::mpsc::channel();
            let fifo_clone = fifo_path.clone();
            let handle = std::thread::spawn(move || {
                let res = Config::load_or_default(&fifo_clone);
                let _ = tx.send(res);
            });

            let res = rx
                .recv_timeout(std::time::Duration::from_secs(1))
                .expect("load_or_default hung on FIFO");
            handle.join().expect("thread join");
            let err = res.expect_err("FIFO must fail");
            assert!(
                matches!(err, CoreError::InvalidPath { .. }),
                "expected InvalidPath for FIFO, got {err:?}"
            );
        }
    }

    #[test]
    fn cloudcode_upstream_url_defaults_deserializes_compatibly_and_validates_s3() {
        let cfg = Config::default();
        assert_eq!(cfg.cloudcode_upstream_url, DEFAULT_CLOUDCODE_UPSTREAM_URL);
        assert_eq!(
            cfg.redacted().cloudcode_upstream_url,
            DEFAULT_CLOUDCODE_UPSTREAM_URL
        );

        // Legacy config JSON without cloudcode_upstream_url deserializes to default.
        let legacy: Config = serde_json::from_str(
            r#"{
                "schema_version": 1,
                "listen_host": "127.0.0.1",
                "listen_port": 4141,
                "gemini": {
                    "base_url": "https://generativelanguage.googleapis.com",
                    "default_model": "gemini-4-hydrogen",
                    "timeout_secs": 900
                }
            }"#,
        )
        .expect("legacy config must deserialize");
        assert_eq!(
            legacy.cloudcode_upstream_url,
            DEFAULT_CLOUDCODE_UPSTREAM_URL
        );
        legacy.validate().expect("legacy config must validate");

        // "auto", loopback http, and https prod URLs are accepted.
        for ok_url in [
            "auto",
            "https://daily-cloudcode-pa.googleapis.com",
            "https://cloudcode-pa.googleapis.com",
            "http://127.0.0.1:49999",
            "http://localhost:49999",
        ] {
            let c = Config {
                cloudcode_upstream_url: ok_url.to_string(),
                ..Default::default()
            };
            c.validate()
                .unwrap_or_else(|e| panic!("expected {ok_url} to be valid, got {e}"));
        }

        // Non-loopback http, embedded creds, and traversal are rejected (S3).
        for bad_url in [
            "http://daily-cloudcode-pa.googleapis.com",
            "https://user:pass@cloudcode-pa.googleapis.com",
            "https://cloudcode-pa.googleapis.com/../secret",
            "ftp://cloudcode-pa.googleapis.com",
            "",
        ] {
            let c = Config {
                cloudcode_upstream_url: bad_url.to_string(),
                ..Default::default()
            };
            assert!(
                matches!(c.validate(), Err(CoreError::InvalidBaseUrl { .. })),
                "expected InvalidBaseUrl for {bad_url}"
            );
        }
    }

    #[test]
    fn placeholder_api_key_predicate_and_validation_behavior() {
        assert!(is_placeholder_api_key(PROXY_PLACEHOLDER_API_KEY));
        assert!(is_placeholder_api_key("  agy-companion-local  "));
        assert!(is_placeholder_api_key("\tagy-companion-local\n"));

        for non_placeholder in [
            "",
            "   ",
            "agy-companion-local-2",
            "AGY-COMPANION-LOCAL",
            "AIzaSyRealKeyTest1234",
        ] {
            assert!(
                !is_placeholder_api_key(non_placeholder),
                "expected false for '{non_placeholder}'"
            );
        }

        // Legacy configs containing syntactically valid placeholder still validate so users
        // aren't locked out of loading/clearing config, while redacted() treats it as unset.
        for legacy_placeholder in [PROXY_PLACEHOLDER_API_KEY, "  agy-companion-local  "] {
            let cfg = Config {
                gemini: GeminiConfig {
                    api_key: Some(legacy_placeholder.to_string()),
                    ..Default::default()
                },
                ..Default::default()
            };
            cfg.validate()
                .expect("legacy placeholder stays syntactically loadable");
            let redacted = cfg.redacted();
            assert!(!redacted.api_key_configured);
            assert_eq!(redacted.api_key_hint, None);
        }

        // Control characters / non-ASCII variants of the placeholder still fail header validation.
        for bad_placeholder in [
            "\tagy-companion-local\n",
            "agy-companion-local\r\n",
            "agy-companion-local\u{00A0}",
        ] {
            let cfg = Config {
                gemini: GeminiConfig {
                    api_key: Some(bad_placeholder.to_string()),
                    ..Default::default()
                },
                ..Default::default()
            };
            assert!(
                matches!(cfg.validate(), Err(CoreError::InvalidApiKey)),
                "expected InvalidApiKey for '{bad_placeholder:?}'"
            );
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
        for bad in ["/tmp/../escape", "/tmp/bad\0data"] {
            assert!(matches!(
                resolve_data_dir_for_platform(
                    Some(bad),
                    None,
                    Some(PathBuf::from("/Users/test")),
                    TargetPlatform::Unix,
                ),
                Err(CoreError::InvalidPath { .. })
            ));
        }
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
            Err(CoreError::InvalidPath { .. })
        ));
    }

    #[test]
    fn write_file_atomic_secure_enforces_permissions_symlink_rejection_and_cleanup() {
        let tmp = TempDir::new().expect("temp dir");
        let nested_dir = tmp.path().join("state-dir");
        let target = nested_dir.join("proxy.pid");

        write_file_atomic_secure(&target, b"{\"pid\":123}\n").expect("atomic write succeeds");
        assert_eq!(fs::read(&target).expect("read target"), b"{\"pid\":123}\n");

        // Zero leftover .tmp. files in parent directory.
        let leftover_tmps: Vec<_> = fs::read_dir(&nested_dir)
            .expect("read dir")
            .filter_map(Result::ok)
            .filter(|entry| entry.file_name().to_string_lossy().contains(".tmp."))
            .collect();
        assert!(
            leftover_tmps.is_empty(),
            "leftover tmp files: {leftover_tmps:?}"
        );

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let dir_mode = fs::metadata(&nested_dir)
                .expect("dir meta")
                .permissions()
                .mode()
                & 0o777;
            assert_eq!(dir_mode, 0o700);
            let file_mode = fs::metadata(&target)
                .expect("file meta")
                .permissions()
                .mode()
                & 0o777;
            assert_eq!(file_mode, 0o600);

            // Reject live symlink, dangling symlink, and trailing-slash symlink without mutating target.
            let real_target = tmp.path().join("real.txt");
            fs::write(&real_target, b"original").expect("write real target");
            let live_link = tmp.path().join("live.link");
            std::os::unix::fs::symlink(&real_target, &live_link).expect("create live symlink");
            let err = write_file_atomic_secure(&live_link, b"overwritten")
                .expect_err("live symlink must be rejected");
            assert!(
                matches!(&err, CoreError::InvalidPath { reason } if reason.contains("symbolic link")),
                "unexpected error: {err:?}"
            );
            assert_eq!(fs::read(&real_target).expect("read real"), b"original");

            let dangling_link = tmp.path().join("dangling.link");
            std::os::unix::fs::symlink(tmp.path().join("missing.txt"), &dangling_link)
                .expect("create dangling symlink");
            let err = write_file_atomic_secure(&dangling_link, b"data")
                .expect_err("dangling symlink must be rejected");
            assert!(
                matches!(&err, CoreError::InvalidPath { reason } if reason.contains("symbolic link")),
                "unexpected error: {err:?}"
            );

            let trailing_link = PathBuf::from(format!("{}/", live_link.display()));
            let err = write_file_atomic_secure(&trailing_link, b"data")
                .expect_err("trailing-slash symlink must be rejected");
            assert!(
                matches!(&err, CoreError::InvalidPath { reason } if reason.contains("symbolic link")),
                "unexpected error: {err:?}"
            );
        }

        // Reject .. traversal and NUL bytes.
        assert!(matches!(
            write_file_atomic_secure(&tmp.path().join("../escape.txt"), b"x"),
            Err(CoreError::InvalidPath { .. })
        ));
        assert!(matches!(
            write_file_atomic_secure(Path::new("/tmp/bad\0file"), b"x"),
            Err(CoreError::InvalidPath { .. })
        ));

        // Renaming over an existing directory fails with CoreError::Io and cleans up temp file.
        let dir_target = tmp.path().join("existing_dir");
        fs::create_dir(&dir_target).expect("create dir target");
        let err = write_file_atomic_secure(&dir_target, b"data")
            .expect_err("rename over directory must fail");
        assert!(
            matches!(err, CoreError::Io { .. }),
            "expected CoreError::Io, got {err:?}"
        );
        let leftover_root_tmps: Vec<_> = fs::read_dir(tmp.path())
            .expect("read tmp root")
            .filter_map(Result::ok)
            .filter(|entry| entry.file_name().to_string_lossy().contains(".tmp."))
            .collect();
        assert!(
            leftover_root_tmps.is_empty(),
            "leftover tmp files after rename error: {leftover_root_tmps:?}"
        );
    }
}
