use serde::{Deserialize, Serialize};

use crate::config::{CloudCodeConfig, ProxyConfig};
use crate::models::default_enabled_gemini_models;
use crate::validation::is_placeholder_api_key;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RedactedGeminiConfig {
    pub base_url: String,
    pub default_model: String,
    #[serde(default = "default_enabled_gemini_models")]
    pub enabled_models: Vec<String>,
    pub timeout_secs: u64,
    pub api_key_configured: bool,
    pub api_key_hint: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RedactedProvidersConfig {
    pub gemini: RedactedGeminiConfig,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RedactedConfig {
    pub schema_version: u32,
    pub proxy: ProxyConfig,
    pub cloudcode: CloudCodeConfig,
    pub providers: RedactedProvidersConfig,
    #[serde(default)]
    pub launcher_auto_start: Option<bool>,
}

pub fn mask_api_key(key: &str) -> String {
    let chars: Vec<char> = key.trim().chars().collect();
    if chars.is_empty() {
        return String::new();
    }
    if chars.len() >= 12 {
        let prefix: String = chars[..4].iter().collect();
        let suffix: String = chars[chars.len() - 4..].iter().collect();
        format!("{prefix}...{suffix}")
    } else {
        "****".to_string()
    }
}

pub fn api_key_hint(key: Option<&str>) -> Option<String> {
    key.map(str::trim)
        .filter(|trimmed| !trimmed.is_empty() && !is_placeholder_api_key(trimmed))
        .map(mask_api_key)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{ApiKey, Config, GeminiCredentials, PROXY_PLACEHOLDER_API_KEY};

    #[test]
    fn masks_api_key_without_leaking_middle() {
        let secret = "AIzaSyD1234567890abcd";
        let masked = mask_api_key(secret);
        assert_eq!(masked, "AIza...abcd");
        assert!(!masked.contains("SyD1234567890"));

        assert_eq!(mask_api_key("short"), "****");
        assert_eq!(mask_api_key("12345678901"), "****");
        assert_eq!(api_key_hint(Some("")), None);
        assert_eq!(api_key_hint(None), None);
    }

    #[test]
    fn handles_boundary_lengths_whitespace_and_multibyte_utf8() {
        assert_eq!(mask_api_key("12345678901"), "****");
        assert_eq!(mask_api_key("123456789012"), "1234...9012");
        assert_eq!(mask_api_key(""), "");
        assert_eq!(mask_api_key("   \t\n "), "");
        assert_eq!(api_key_hint(Some("   \t\n ")), None);

        assert_eq!(mask_api_key("αβγδεζηθικλμ"), "αβγδ...ικλμ");
        assert_eq!(mask_api_key("🔑🔑🔑🔑12345678"), "🔑🔑🔑🔑...5678");
        assert_eq!(mask_api_key("🔑🔑🔑"), "****");
    }

    #[test]
    fn redacted_config_never_exposes_plaintext_api_key_and_matches_nested_shape() {
        let secret = "AIzaSyD1234567890abcd";
        let cfg = Config::default();
        let creds = GeminiCredentials::new(ApiKey::parse_strict(secret).unwrap());

        let redacted = cfg.redacted_with_auth(Some(&creds));
        assert!(redacted.providers.gemini.api_key_configured);
        assert_eq!(
            redacted.providers.gemini.api_key_hint.as_deref(),
            Some("AIza...abcd")
        );

        let json = serde_json::to_string(&redacted).expect("serialize redacted");
        assert!(!json.contains(secret));
        assert!(!json.contains("SyD1234567890"));

        let val: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert_eq!(val["schema_version"], 1);
        assert_eq!(val["proxy"]["host"], "127.0.0.1");
        assert_eq!(val["proxy"]["port"], 24141);
        assert_eq!(val["cloudcode"]["upstream"], "auto");
        assert_eq!(val["providers"]["gemini"]["api_key_configured"], true);
        assert_eq!(val["providers"]["gemini"]["api_key_hint"], "AIza...abcd");

        let blank_redacted = cfg.redacted_with_auth(None);
        assert!(!blank_redacted.providers.gemini.api_key_configured);
        assert_eq!(blank_redacted.providers.gemini.api_key_hint, None);
    }

    #[test]
    fn api_key_hint_and_redacted_quarantine_proxy_placeholder_sentinel() {
        for raw in [
            PROXY_PLACEHOLDER_API_KEY,
            "  agy-companion-local  ",
            "\tagy-companion-local\n",
        ] {
            assert_eq!(
                api_key_hint(Some(raw)),
                None,
                "expected None hint for placeholder '{raw}'"
            );
        }

        let cfg = Config::default();
        for raw in [PROXY_PLACEHOLDER_API_KEY, "  agy-companion-local  "] {
            let json = format!(r#"{{"schema_version":1,"api_key":"{raw}"}}"#);
            let creds =
                GeminiCredentials::from_json_str(&json, std::path::Path::new("gemini.json"))
                    .expect("disk placeholder normalizes to None");
            assert!(creds.is_none());

            let redacted = cfg.redacted_with_auth(creds.as_ref());
            assert!(!redacted.providers.gemini.api_key_configured);
            assert_eq!(redacted.providers.gemini.api_key_hint, None);
        }
    }
}
