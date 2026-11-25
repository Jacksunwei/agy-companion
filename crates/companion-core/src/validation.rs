use std::fmt;
use std::net::Ipv4Addr;

use serde::{Deserialize, Serialize};
use url::{Host, Url};

use crate::config::{CoreError, DEFAULT_CLOUDCODE_UPSTREAM_URL, LoopbackHost};

pub const PROXY_PLACEHOLDER_API_KEY: &str = "agy-companion-local";

/// Returns `true` when `key` (ignoring surrounding whitespace) equals the
/// internal local-proxy placeholder sentinel (`"agy-companion-local"`).
pub fn is_placeholder_api_key(key: &str) -> bool {
    key.trim() == PROXY_PLACEHOLDER_API_KEY
}

/// Returns `true` iff every byte is printable ASCII including space (`0x20..=0x7E`).
pub fn is_printable_ascii_with_space(bytes: &[u8]) -> bool {
    bytes.iter().all(|&b| (0x20..=0x7E).contains(&b))
}

/// Validates a raw API key string with full S2 parity across `config`, `doctor`, `serve`,
/// `GeminiProvider`, and proxy inbound headers:
/// - Rejects control chars (`\r`, `\n`, `\t`, `\0`, DEL) and non-ASCII (`CoreError::InvalidApiKey`).
/// - Trims leading/trailing ASCII spaces (`0x20`).
/// - Returns `Ok(None)` for empty/spaces-only or quarantined `PROXY_PLACEHOLDER_API_KEY`.
/// - Rejects internal spaces (`CoreError::InvalidApiKey`).
/// - Returns `Ok(Some(trimmed))` for valid keys (zero allocation).
pub fn validate_api_key_str(raw: &str) -> Result<Option<&str>, CoreError> {
    if !is_printable_ascii_with_space(raw.as_bytes()) {
        return Err(CoreError::InvalidApiKey);
    }
    let trimmed = raw.trim_matches(' ');
    if trimmed.is_empty() || is_placeholder_api_key(trimmed) {
        return Ok(None);
    }
    if !trimmed.bytes().all(|b| (0x21..=0x7E).contains(&b)) {
        return Err(CoreError::InvalidApiKey);
    }
    Ok(Some(trimmed))
}

/// Validated, non-empty, non-placeholder, header-safe Gemini API key.
///
/// - `Debug` always emits masked hint via `mask_api_key` (never plaintext).
/// - Intentionally omits `Display`, `Deref`, and `AsRef<str>` to prevent accidental logging.
/// - Callers must explicitly invoke `.expose()` when constructing outbound auth headers.
#[derive(Clone, PartialEq, Eq)]
pub struct ApiKey(String);

impl ApiKey {
    /// Strict parser for user/tool writes (`save_settings`, `Config::apply_patch`, direct Rust API):
    /// - Control chars, non-ASCII, internal spaces, empty/whitespace -> `Err(CoreError::InvalidApiKey)`.
    /// - `PROXY_PLACEHOLDER_API_KEY` (with optional ASCII spaces) -> `Err(CoreError::PlaceholderApiKey)`.
    /// - Valid key (with optional leading/trailing ASCII spaces trimmed) -> `Ok(Self)`.
    pub fn parse_strict(raw: &str) -> Result<Self, CoreError> {
        if !is_printable_ascii_with_space(raw.as_bytes()) {
            return Err(CoreError::InvalidApiKey);
        }
        let trimmed = raw.trim_matches(' ');
        if trimmed.is_empty() {
            return Err(CoreError::InvalidApiKey);
        }
        if !trimmed.bytes().all(|b| (0x21..=0x7E).contains(&b)) {
            return Err(CoreError::InvalidApiKey);
        }
        if is_placeholder_api_key(trimmed) {
            return Err(CoreError::PlaceholderApiKey);
        }
        Ok(Self(trimmed.to_string()))
    }

    /// Disk/config loader parser with S2 quarantine parity:
    /// - Control chars (including `\tagy-companion-local\n`), non-ASCII, internal spaces -> `Err(CoreError::InvalidApiKey)`.
    /// - Empty, ASCII spaces only, or syntactically valid `PROXY_PLACEHOLDER_API_KEY` -> `Ok(None)`.
    /// - Valid key -> `Ok(Some(Self))`.
    pub fn parse_from_disk(raw: &str) -> Result<Option<Self>, CoreError> {
        Ok(validate_api_key_str(raw)?.map(|clean| Self(clean.to_string())))
    }

    /// Returns the validated, trimmed plaintext API key for outbound header construction.
    pub fn expose(&self) -> &str {
        &self.0
    }

    /// Returns the masked display hint (e.g. `"AIza...9x2Q"` or `"****"`).
    pub fn hint(&self) -> String {
        crate::redaction::mask_api_key(&self.0)
    }
}

impl fmt::Debug for ApiKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_tuple("ApiKey")
            .field(&crate::redaction::mask_api_key(&self.0))
            .finish()
    }
}

impl Serialize for ApiKey {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(&self.0)
    }
}

impl<'de> Deserialize<'de> for ApiKey {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let raw = String::deserialize(deserializer)?;
        // Static error message — NEVER echoes `raw`.
        Self::parse_strict(&raw).map_err(|_| serde::de::Error::custom("invalid gemini.api_key"))
    }
}

/// Custom serde deserializer for `Option<ApiKey>` on disk (`GeminiConfig.api_key`):
/// normalizes `null`, `""`, `"   "`, and `"agy-companion-local"` (with optional ASCII spaces)
/// to `None`, while rejecting control chars, non-ASCII, and internal spaces without echoing input.
pub fn deserialize_optional_api_key<'de, D>(deserializer: D) -> Result<Option<ApiKey>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let raw = Option::<String>::deserialize(deserializer)?;
    match raw {
        None => Ok(None),
        Some(s) => ApiKey::parse_from_disk(&s)
            .map_err(|_| serde::de::Error::custom("invalid gemini.api_key")),
    }
}

/// Validates that `raw` contains only printable ASCII (`0x20..=0x7E`) before trimming ASCII spaces.
pub fn validate_trimmed_ascii_field(raw: &str, field_name: &str) -> Result<String, CoreError> {
    if !is_printable_ascii_with_space(raw.as_bytes()) {
        return Err(CoreError::InvalidField {
            field: field_name.to_string(),
            reason: "contains invalid characters".to_string(),
        });
    }
    Ok(raw.trim_matches(' ').to_string())
}

pub fn decode_ascii_hex_nibble(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        b'A'..=b'F' => Some(byte - b'A' + 10),
        _ => None,
    }
}

/// Exact RFC 3986 `pchar \ {';', '%'}` (78 bytes).
pub fn is_safe_path_segment_byte(byte: u8) -> bool {
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

pub fn validate_url_path(path: &str) -> Result<(), CoreError> {
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
            reason:
                "URL must contain only printable ASCII characters without whitespace or control characters"
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

pub fn validate_listen_host(host: &str) -> Result<(), CoreError> {
    host.parse::<LoopbackHost>().map(|_| ())
}

/// Sanitizes an env-provided Gemini base URL for diagnostic output (stripping credentials,
/// query parameters, fragments, and control characters) and reports whether it points to
/// the local loopback proxy on `target_port`.
pub fn sanitize_base_url(raw: &str, target_port: u16) -> Option<(String, bool)> {
    let trimmed = raw.trim();
    if trimmed.chars().any(char::is_control) {
        return None;
    }

    let parsed = Url::parse(trimmed).ok()?;
    let scheme = parsed.scheme();
    if scheme != "http" && scheme != "https" {
        return None;
    }
    let host = parsed.host_str()?;

    let port_suffix = match parsed.port() {
        Some(port) => format!(":{port}"),
        None => String::new(),
    };
    let clean_path: String = parsed.path().chars().filter(|c| !c.is_control()).collect();
    let display_path = if clean_path == "/" {
        String::new()
    } else {
        clean_path.clone()
    };
    let sanitized = format!("{scheme}://{host}{port_suffix}{display_path}");

    let normalized_path = clean_path.trim_end_matches('/');
    let matches_proxy = scheme == "http"
        && (host == "127.0.0.1" || host.eq_ignore_ascii_case("localhost"))
        && parsed.port_or_known_default() == Some(target_port)
        && (normalized_path.is_empty() || normalized_path == "/v1beta");

    Some((sanitized, matches_proxy))
}

#[cfg(test)]
mod tests {
    use super::*;

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
    fn api_key_parse_strict_and_from_disk_enforce_s2_parity() {
        // Strict valid & trimming
        let key = ApiKey::parse_strict("  AIzaSyValidKey1234  ").expect("valid key");
        assert_eq!(key.expose(), "AIzaSyValidKey1234");
        assert_eq!(key.hint(), "AIza...1234");
        let debug = format!("{key:?}");
        assert!(debug.contains("AIza...1234"));
        assert!(!debug.contains("SyValidKey"));

        // Short key masks to ****
        let short = ApiKey::parse_strict("shortkey").expect("short key");
        assert_eq!(short.hint(), "****");
        assert_eq!(format!("{short:?}"), "ApiKey(\"****\")");

        // Strict rejects placeholder with dedicated PlaceholderApiKey containing "placeholder"
        for placeholder in [PROXY_PLACEHOLDER_API_KEY, "  agy-companion-local  "] {
            let err = ApiKey::parse_strict(placeholder).expect_err("placeholder must fail strict");
            assert!(matches!(err, CoreError::PlaceholderApiKey));
            assert!(err.to_string().to_lowercase().contains("placeholder"));
            assert!(err.to_string().contains(PROXY_PLACEHOLDER_API_KEY));
        }

        // Strict rejects empty/spaces with InvalidApiKey
        for empty in ["", "   "] {
            assert!(matches!(
                ApiKey::parse_strict(empty),
                Err(CoreError::InvalidApiKey)
            ));
        }

        // Both strict and disk reject control chars, non-ASCII, and internal spaces (including tabbed placeholder)
        for bad in [
            "AIzaSyBad\r\nKey",
            "AIzaSyBad\tKey",
            "AIzaSyBad\0Key",
            "AIzaSyBad\u{007F}Key",
            "AIzaSyBad\u{00A0}Key",
            "AIzaSyBad Key",
            "AIzaSy Space Key 1234",
            "AIzaSy\u{1F511}Key1234",
            "AIzaSy\u{0085}Key1234",
            "\tAIzaSyKey1234",
            "AIzaSyKey1234\t",
            "\tagy-companion-local\n",
            "agy-companion-local\r\n",
            "agy-companion-local\u{00A0}",
        ] {
            let strict_err = ApiKey::parse_strict(bad).expect_err("strict must reject");
            assert!(matches!(strict_err, CoreError::InvalidApiKey));
            assert!(!strict_err.to_string().contains("AIzaSy"));
            assert!(!format!("{strict_err:?}").contains("AIzaSy"));

            let disk_err = ApiKey::parse_from_disk(bad).expect_err("disk must reject");
            assert!(matches!(disk_err, CoreError::InvalidApiKey));
            assert!(!disk_err.to_string().contains("AIzaSy"));
            assert!(!format!("{disk_err:?}").contains("AIzaSy"));

            assert!(matches!(
                validate_api_key_str(bad),
                Err(CoreError::InvalidApiKey)
            ));
        }

        // Disk loader normalizes blank, spaces, and clean placeholder to Ok(None)
        for none_case in [
            "",
            "   ",
            PROXY_PLACEHOLDER_API_KEY,
            "  agy-companion-local  ",
        ] {
            assert_eq!(ApiKey::parse_from_disk(none_case).expect("disk ok"), None);
            assert_eq!(validate_api_key_str(none_case).expect("validate ok"), None);
        }

        // Serde deserialize_optional_api_key static error never leaks secret
        #[derive(Deserialize)]
        struct Wrapper {
            #[serde(default, deserialize_with = "deserialize_optional_api_key")]
            api_key: Option<ApiKey>,
        }
        let w: Wrapper = serde_json::from_str(r#"{"api_key":"agy-companion-local"}"#).unwrap();
        assert_eq!(w.api_key, None);
        let w2: Wrapper = serde_json::from_str(r#"{"api_key":"  AIzaSyTest123456  "}"#).unwrap();
        assert_eq!(
            w2.api_key.as_ref().map(ApiKey::expose),
            Some("AIzaSyTest123456")
        );
        let err = serde_json::from_str::<Wrapper>(r#"{"api_key":"TOPSECRET\r\nKEY"}"#)
            .err()
            .expect("must fail");
        assert!(!err.to_string().contains("TOPSECRET"));
    }

    #[test]
    fn validate_trimmed_ascii_field_trims_spaces_and_rejects_control_or_non_ascii() {
        assert_eq!(
            validate_trimmed_ascii_field("  hello  ", "field").unwrap(),
            "hello"
        );
        for bad in ["a\tb", "a\r\nb", "a\0b", "héllo"] {
            let err = validate_trimmed_ascii_field(bad, "my_field").unwrap_err();
            assert!(
                matches!(err, CoreError::InvalidField { ref field, .. } if field == "my_field")
            );
        }
    }

    #[test]
    fn validate_url_path_accepts_cloudcode_rpc_and_rejects_traversal_or_bad_pct() {
        for ok in [
            "",
            "/",
            "/v1beta",
            "/v1internal:fetchAvailableModels",
            "/v1beta/models/gemini-4-hydrogen:streamGenerateContent",
            "/api/v1/foo%2Dbar",
        ] {
            assert!(validate_url_path(ok).is_ok(), "expected ok for '{ok}'");
        }
        for bad in [
            "relative",
            "/.",
            "/..",
            "/a/../b",
            "/v1internal/%2e%2e/secret",
            "/v1internal/%252e%252e/secret",
            "/v1/%2",
            "/v1/%zz",
            "/v1/%3b",
            "/v1/%00",
            "/v1\\evil",
            "/v1;param",
        ] {
            assert!(
                matches!(
                    validate_url_path(bad),
                    Err(CoreError::InvalidBaseUrl { .. })
                ),
                "expected reject for '{bad}'"
            );
        }
    }

    #[test]
    fn upstream_url_and_cloudcode_url_validation() {
        for ok in [
            "https://generativelanguage.googleapis.com",
            "https://generativelanguage.googleapis.com/v1beta",
            "http://127.0.0.1:8080",
            "http://localhost:8080/v1beta",
        ] {
            assert!(validate_upstream_url(ok).is_ok(), "expected ok for '{ok}'");
            assert!(validate_cloudcode_upstream_url(ok).is_ok());
        }
        assert!(validate_cloudcode_upstream_url("auto").is_ok());
        assert!(validate_upstream_url("auto").is_err());

        for bad in [
            "",
            "http://example.com",
            "http://127.0.0.2:8080",
            "http://[::1]:8080",
            "https://user:pass@generativelanguage.googleapis.com",
            "https://generativelanguage.googleapis.com?key=abc",
            "https://generativelanguage.googleapis.com#frag",
            "https://generativelanguage.googleapis.com/a/../b",
            "https://generativelanguage.googleapis.com\\foo",
            "ftp://generativelanguage.googleapis.com",
            "https://generativelanguage.googleapis.com:0",
            "https://generativelanguage.googleapis.com:",
        ] {
            assert!(
                matches!(
                    validate_upstream_url(bad),
                    Err(CoreError::InvalidBaseUrl { .. })
                ),
                "expected reject for '{bad}'"
            );
        }
    }

    #[test]
    fn sanitize_base_url_strips_creds_query_fragment_and_detects_loopback_proxy() {
        assert_eq!(
            sanitize_base_url("http://127.0.0.1:24141", 24141),
            Some(("http://127.0.0.1:24141".to_string(), true))
        );
        assert_eq!(
            sanitize_base_url("http://localhost:24141/v1beta/?key=secret#frag", 24141),
            Some(("http://localhost:24141/v1beta/".to_string(), true))
        );
        assert_eq!(
            sanitize_base_url(
                "https://user:pass@generativelanguage.googleapis.com?key=secret",
                24141
            ),
            Some((
                "https://generativelanguage.googleapis.com".to_string(),
                false
            ))
        );
        assert_eq!(
            sanitize_base_url("http://127.0.0.1:9999", 24141),
            Some(("http://127.0.0.1:9999".to_string(), false))
        );
        assert_eq!(sanitize_base_url("ftp://127.0.0.1:24141", 24141), None);
        assert_eq!(
            sanitize_base_url("http://127.0.0.1:24141\n", 24141),
            Some(("http://127.0.0.1:24141".to_string(), true))
        );
        assert_eq!(sanitize_base_url("http://127.0.0.1\r\n:24141", 24141), None);
    }

    #[test]
    fn rejects_ssrf_host_confusion_parser_differentials_and_encoded_smuggling() {
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
            "https://generativelanguage.googleapis.com\r\nX-Injected: 1",
            "http://localhost\\@evil.com",
            // Unanchored scheme parser differentials
            "https:evil.com/v1beta/../secret/://x",
            "https:@evil.com/x://y",
            "https:evil.com/x://y",
            "http:0x7f000001:8080/x://127.0.0.1",
            "http:2130706433:8080/x://127.0.0.1",
            "http:127.1:8080/x://127.0.0.1",
            "http:@127.0.0.1:8080/x://127.0.0.1",
            "http:localhost:8080/x://localhost",
            // Multi-encoded / malformed percent / matrix / dot / bidi / leading-zero ports
            "https://generativelanguage.googleapis.com/v1beta/..%252fevil",
            "https://generativelanguage.googleapis.com/v1beta/%25252e%25252e/evil",
            "https://generativelanguage.googleapis.com/v1beta/%2525252e%2525252e/evil",
            "https://generativelanguage.googleapis.com/v1beta/%252f",
            "https://generativelanguage.googleapis.com/v1beta/%255c",
            "https://generativelanguage.googleapis.com/v1beta/%2500evil",
            "https://generativelanguage.googleapis.com/v1beta/%3fkey=secret",
            "https://generativelanguage.googleapis.com/v1beta/%23frag",
            "https://generativelanguage.googleapis.com/v1beta/%20",
            "https://generativelanguage.googleapis.com/v1beta/.%3bfoo",
            "https://generativelanguage.googleapis.com/v1beta/.;foo/bar",
            "https://generativelanguage.googleapis.com/v1beta/./bar",
            "https://generativelanguage.googleapis.com/v1beta/%2e/bar",
            "https://generativelanguage.googleapis.com/v1beta/\u{ff0e}\u{ff0e}/evil",
            "https://generativelanguage.googleapis.com/v1beta/\u{202e}evil",
            "https://generativelanguage.googleapis.com/\u{0085}",
            "https://generativelanguage.googleapis.com/\u{00a0}",
            "https://generativelanguage.googleapis.com/\u{200b}",
            "http://127.0.0.1:080",
            "http://127.0.0.1:0000",
            "http://localhost:04141",
            "https://[::1]:08443",
            "http://127.0.0.1:65536",
            "http://127.0.0.1:+80",
            "http://127.0.0.1:-80",
        ];

        for bad in bad_urls {
            assert!(
                matches!(
                    validate_upstream_url(bad),
                    Err(CoreError::InvalidBaseUrl { .. })
                ),
                "expected InvalidBaseUrl for '{bad:?}'"
            );
        }

        // Exhaustive percent-encoded C0 (0x00..=0x1F) and DEL (0x7F)
        for byte in (0x00u8..=0x1Fu8).chain(std::iter::once(0x7Fu8)) {
            for encoded in [format!("%{byte:02x}"), format!("%{byte:02X}")] {
                let url = format!("https://generativelanguage.googleapis.com/v1beta/a{encoded}b");
                assert!(
                    matches!(
                        validate_upstream_url(&url),
                        Err(CoreError::InvalidBaseUrl { .. })
                    ),
                    "expected InvalidBaseUrl for '{url}'"
                );
            }
        }

        // Valid URLs without false positives (full 36-URL matrix)
        for good in [
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
        ] {
            assert!(
                validate_upstream_url(good).is_ok(),
                "false positive on '{good}'"
            );
        }
    }

    #[test]
    fn placeholder_api_key_predicate_positive_and_negative_cases() {
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
    }

    #[test]
    fn rejects_multi_encoded_malformed_percent_and_control_byte_url_paths() {
        let bad_urls = [
            // All 14 double/triple-encoded payloads
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
    }

    #[test]
    fn rejects_single_dot_and_matrix_parameter_url_paths() {
        let bad_urls = [
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
}
