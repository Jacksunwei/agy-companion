use serde::{Deserialize, Serialize};

use crate::config::is_placeholder_api_key;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RedactedConfig {
    pub schema_version: u32,
    pub listen_host: String,
    pub listen_port: u16,
    pub gemini_base_url: String,
    pub gemini_default_model: String,
    pub gemini_timeout_secs: u64,
    pub cloudcode_upstream_url: String,
    pub api_key_configured: bool,
    pub api_key_hint: Option<String>,
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
    use crate::Config;

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
    fn redacted_config_never_exposes_plaintext_api_key() {
        let secret = "AIzaSyD1234567890abcd";
        let mut cfg = Config::default();
        cfg.gemini.api_key = Some(secret.to_string());

        let redacted = cfg.redacted();
        assert!(redacted.api_key_configured);
        assert_eq!(redacted.api_key_hint.as_deref(), Some("AIza...abcd"));

        let json = serde_json::to_string(&redacted).expect("serialize redacted");
        assert!(!json.contains(secret));
        assert!(!json.contains("SyD1234567890"));

        cfg.gemini.api_key = Some("   ".to_string());
        let blank_redacted = cfg.redacted();
        assert!(!blank_redacted.api_key_configured);
        assert_eq!(blank_redacted.api_key_hint, None);
    }

    #[test]
    fn api_key_hint_and_redacted_quarantine_proxy_placeholder_sentinel() {
        use crate::PROXY_PLACEHOLDER_API_KEY;

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

        for raw in [PROXY_PLACEHOLDER_API_KEY, "  agy-companion-local  "] {
            let mut cfg = Config::default();
            cfg.gemini.api_key = Some(raw.to_string());

            let redacted = cfg.redacted();
            assert!(!redacted.api_key_configured);
            assert_eq!(redacted.api_key_hint, None);

            let debug = format!("{:?}", cfg.gemini);
            assert!(
                debug.contains("api_key: None"),
                "expected GeminiConfig Debug to treat placeholder as None, got {debug}"
            );
        }
    }
}
