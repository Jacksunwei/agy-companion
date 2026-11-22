use std::fmt;
use std::fs;
use std::io::{ErrorKind, Write};
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
pub const DEFAULT_GEMINI_MODEL: &str = "gemini-2.5-pro";
pub const DEFAULT_TIMEOUT_SECS: u64 = 900;
pub const CONFIG_PATH_ENV_VAR: &str = "AGY_COMPANION_CONFIG";

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
    #[error("invalid gemini.api_key: must not contain control characters")]
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
    pub usage_dir: Option<PathBuf>,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            schema_version: CURRENT_SCHEMA_VERSION,
            listen_host: DEFAULT_LISTEN_HOST.to_string(),
            listen_port: DEFAULT_LISTEN_PORT,
            gemini: GeminiConfig::default(),
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

pub fn resolve_config_path(
    env_override: Option<&str>,
    home: Option<PathBuf>,
) -> Result<PathBuf, CoreError> {
    if let Some(raw_override) = env_override {
        let trimmed = raw_override.trim();
        if !trimmed.is_empty() {
            let path = PathBuf::from(trimmed);
            validate_path(&path)?;
            return Ok(path);
        }
    }

    let home_dir = home.ok_or(CoreError::HomeDirUnavailable)?;
    Ok(home_dir
        .join(".config")
        .join("agy-companion")
        .join("config.json"))
}

pub fn resolve_usage_dir(home: Option<PathBuf>) -> Result<PathBuf, CoreError> {
    let home_dir = home.ok_or(CoreError::HomeDirUnavailable)?;
    Ok(home_dir
        .join(".local")
        .join("share")
        .join("agy-companion")
        .join("usage"))
}

pub fn default_config_path() -> Result<PathBuf, CoreError> {
    match std::env::var(CONFIG_PATH_ENV_VAR) {
        Ok(value) => resolve_config_path(Some(&value), dirs::home_dir()),
        Err(std::env::VarError::NotPresent) => resolve_config_path(None, dirs::home_dir()),
        Err(std::env::VarError::NotUnicode(_)) => Err(CoreError::InvalidPath {
            reason: format!("{CONFIG_PATH_ENV_VAR} contains invalid UTF-8"),
        }),
    }
}

pub fn default_usage_dir() -> Result<PathBuf, CoreError> {
    resolve_usage_dir(dirs::home_dir())
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

        let model = &self.gemini.default_model;
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

        validate_upstream_url(&self.gemini.base_url)?;
        if let Some(ref key) = self.gemini.api_key
            && key.chars().any(char::is_control)
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
            api_key_configured: hint.is_some(),
            api_key_hint: hint,
        }
    }

    pub fn load_or_default(path: &Path) -> Result<Config, CoreError> {
        validate_path(path)?;

        let raw = match fs::read_to_string(path) {
            Ok(contents) => contents,
            Err(err) if err.kind() == ErrorKind::NotFound => return Ok(Config::default()),
            Err(source) => {
                return Err(CoreError::Io {
                    path: path.to_path_buf(),
                    source,
                });
            }
        };

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

        let file_name = path
            .file_name()
            .and_then(|name| name.to_str())
            .ok_or_else(|| CoreError::InvalidPath {
                reason: "config path must include a valid UTF-8 file name".to_string(),
            })?;

        let parent = path
            .parent()
            .filter(|dir| !dir.as_os_str().is_empty())
            .unwrap_or_else(|| Path::new("."));

        if !parent.exists() {
            let mut dir_builder = fs::DirBuilder::new();
            dir_builder.recursive(true);
            #[cfg(unix)]
            {
                use std::os::unix::fs::DirBuilderExt;
                dir_builder.mode(0o700);
            }
            dir_builder.create(parent).map_err(|source| CoreError::Io {
                path: parent.to_path_buf(),
                source,
            })?;
        }

        let mut payload =
            serde_json::to_string_pretty(self).map_err(|err| CoreError::SerializeConfig {
                reason: err.to_string(),
            })?;
        payload.push('\n');

        let tmp_path = parent.join(format!(".{file_name}.tmp.{}", Uuid::new_v4()));

        let mut open_opts = fs::OpenOptions::new();
        open_opts.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            open_opts.mode(0o600);
        }

        let mut file = open_opts.open(&tmp_path).map_err(|source| CoreError::Io {
            path: tmp_path.clone(),
            source,
        })?;

        let write_result = (|| -> std::io::Result<()> {
            file.write_all(payload.as_bytes())?;
            file.sync_all()?;
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                file.set_permissions(fs::Permissions::from_mode(0o600))?;
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

        if let Err(source) = fs::rename(&tmp_path, path) {
            let _ = fs::remove_file(&tmp_path);
            return Err(CoreError::Io {
                path: path.to_path_buf(),
                source,
            });
        }

        Ok(())
    }
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
        assert_eq!(cfg.gemini.default_model, "gemini-2.5-pro");
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
            "gemini-2.5-pro?key=secret",
            "gemini-2.5-pro#frag",
            "../evil",
            "models/gemini-2.5-pro",
            "gemini-2.5-pro\nX-Test: 1",
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
            "gemini-2.5-pro\u{0085}",
            "gemini-2.5-pro\u{0090}",
            "gemini-2.5-pro\u{009b}",
            "gemini-2.5-pro\u{00a0}",
            "gemini-2.5-pro\u{2003}",
            "gemini-2.5-pro\u{3000}",
            // Invisible, zero-width & bidi format chars
            "gemini-2.5-pro\u{200b}",
            "gemini-2.5-pro\u{200c}",
            "gemini-2.5-pro\u{200d}",
            "gemini-2.5-pro\u{feff}",
            "gemini-2.5-pro\u{2060}",
            "gemini-2.5-pro\u{00ad}",
            "gemini-2.5-pro\u{202e}",
            // ASCII control & whitespace
            "gemini-2.5-pro\0",
            "gemini-2.5-pro\r",
            "gemini-2.5-pro\x7f",
            "gemini-2.5-pro\t",
            " gemini-2.5-pro",
            "gemini-2.5-pro ",
            // Bare dot & percent-encoded / matrix smuggling in model segment
            ".",
            "gemini%2f..%2fevil",
            "gemini%3fkey=secret",
            "gemini%23frag",
            "gemini-2.5-pro;param",
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
            "gemini-2.5-pro",
            "gemini-2.5-flash",
            "gemini-2.5-flash-preview-05-20",
            "gemini-2.0-flash-lite-001",
            "gemini_2.5_pro.v1",
            "gemma3:4b",
        ];

        for good_model in valid_models {
            let mut cfg = Config::default();
            cfg.gemini.default_model = good_model.to_string();
            assert!(
                cfg.validate().is_ok(),
                "false positive rejection for valid default_model '{good_model}'"
            );
        }
    }
}
