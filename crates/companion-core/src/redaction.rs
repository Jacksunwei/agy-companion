use serde::{Deserialize, Serialize};

use crate::config::{CloudCodeConfig, DEFAULT_CODEX_BASE_URL, DEFAULT_TIMEOUT_SECS, ProxyConfig};
use crate::models::{
    DEFAULT_CODEX_MODEL, default_enabled_codex_models, default_enabled_gemini_models,
};
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
pub struct RedactedCodexConfig {
    pub base_url: String,
    pub default_model: String,
    #[serde(default = "default_enabled_codex_models")]
    pub enabled_models: Vec<String>,
    pub timeout_secs: u64,
    pub authenticated: bool,
    pub account_hint: Option<String>,
    pub plan_type: Option<String>,
    pub expires_at_ms: Option<u64>,
}

impl Default for RedactedCodexConfig {
    fn default() -> Self {
        Self {
            base_url: DEFAULT_CODEX_BASE_URL.to_string(),
            default_model: DEFAULT_CODEX_MODEL.to_string(),
            enabled_models: default_enabled_codex_models(),
            timeout_secs: DEFAULT_TIMEOUT_SECS,
            authenticated: false,
            account_hint: None,
            plan_type: None,
            expires_at_ms: None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RedactedProvidersConfig {
    pub gemini: RedactedGeminiConfig,
    #[serde(default)]
    pub codex: RedactedCodexConfig,
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

/// Masks a ChatGPT account ID to a short non-identifying suffix (`...wxyz` for `len >= 8`,
/// `"****"` for shorter non-empty IDs, `""` for blank input).
pub fn mask_account_id(account_id: &str) -> String {
    let chars: Vec<char> = account_id.trim().chars().collect();
    if chars.is_empty() {
        return String::new();
    }
    if chars.len() >= 8 {
        let suffix: String = chars[chars.len() - 4..].iter().collect();
        format!("...{suffix}")
    } else {
        "****".to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        AUTH_SCHEMA_VERSION, ApiKey, CodexAuthMode, CodexCredentials, Config, GeminiCredentials,
        PROXY_PLACEHOLDER_API_KEY, SecretString,
    };

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

        assert_eq!(mask_account_id(""), "");
        assert_eq!(mask_account_id("   "), "");
        assert_eq!(mask_account_id("short"), "****");
        assert_eq!(mask_account_id("1234567"), "****");
        assert_eq!(mask_account_id("12345678"), "...5678");
        assert_eq!(mask_account_id("acct_abcdef1234wxyz"), "...wxyz");
    }

    #[test]
    fn redacted_config_never_exposes_plaintext_api_key_and_matches_nested_shape() {
        let secret = "AIzaSyD1234567890abcd";
        let cfg = Config::default();
        let creds = GeminiCredentials::new(ApiKey::parse_strict(secret).unwrap());

        let redacted = cfg.redacted_with_auth(Some(&creds), None);
        assert!(redacted.providers.gemini.api_key_configured);
        assert_eq!(
            redacted.providers.gemini.api_key_hint.as_deref(),
            Some("AIza...abcd")
        );
        assert!(!redacted.providers.codex.authenticated);
        assert_eq!(redacted.providers.codex.account_hint, None);
        assert_eq!(redacted.providers.codex.plan_type, None);
        assert_eq!(redacted.providers.codex.expires_at_ms, None);

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
        assert_eq!(
            val["providers"]["codex"]["base_url"],
            "https://chatgpt.com/backend-api/codex"
        );
        assert_eq!(val["providers"]["codex"]["default_model"], "gpt-6.2-sol");
        assert_eq!(val["providers"]["codex"]["authenticated"], false);

        let blank_redacted = cfg.redacted_with_auth(None, None);
        assert!(!blank_redacted.providers.gemini.api_key_configured);
        assert_eq!(blank_redacted.providers.gemini.api_key_hint, None);
        assert!(!blank_redacted.providers.codex.authenticated);
    }

    #[test]
    fn redacted_codex_config_exposes_only_redacted_metadata_never_tokens() {
        let cfg = Config::default();
        let codex_creds = CodexCredentials {
            schema_version: AUTH_SCHEMA_VERSION,
            auth_mode: CodexAuthMode::ChatGptOAuth,
            access_token: SecretString::new("eyJhbGciOiJSUzI1NiJ9.super_secret_access.sig"),
            refresh_token: SecretString::new("rt_super_secret_refresh_token"),
            id_token: Some(SecretString::new(
                "eyJhbGciOiJSUzI1NiJ9.super_secret_id.sig",
            )),
            account_id: "acct_org_99887766wxyz".to_string(),
            expires_at_ms: 1_900_000_000_000,
            plan_type: Some("pro".to_string()),
            email_hint: Some("j***@example.com".to_string()),
            last_refresh_utc: "2026-11-26T22:00:00Z".to_string(),
        };

        let redacted = cfg.redacted_with_auth(None, Some(&codex_creds));
        assert!(redacted.providers.codex.authenticated);
        assert_eq!(
            redacted.providers.codex.account_hint.as_deref(),
            Some("j***@example.com")
        );
        assert_eq!(redacted.providers.codex.plan_type.as_deref(), Some("pro"));
        assert_eq!(
            redacted.providers.codex.expires_at_ms,
            Some(1_900_000_000_000)
        );

        let json = serde_json::to_string(&redacted).expect("serialize");
        for forbidden in [
            "super_secret_access",
            "super_secret_refresh_token",
            "super_secret_id",
            "acct_org_99887766wxyz",
        ] {
            assert!(
                !json.contains(forbidden),
                "redacted config leaked '{forbidden}': {json}"
            );
        }

        // Without email_hint, falls back to masked account_id suffix
        let mut no_email = codex_creds;
        no_email.email_hint = None;
        let redacted_fallback = cfg.redacted_with_auth(None, Some(&no_email));
        assert_eq!(
            redacted_fallback.providers.codex.account_hint.as_deref(),
            Some("...wxyz")
        );
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

            let redacted = cfg.redacted_with_auth(creds.as_ref(), None);
            assert!(!redacted.providers.gemini.api_key_configured);
            assert_eq!(redacted.providers.gemini.api_key_hint, None);
        }
    }
}
