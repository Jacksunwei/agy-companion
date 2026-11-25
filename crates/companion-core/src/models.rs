use std::cmp::Reverse;

use serde::{Deserialize, Serialize};

use crate::config::CoreError;

pub const DEFAULT_GEMINI_MODEL: &str = "gemini-4-hydrogen";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CuratedGeminiModel {
    pub id: &'static str,
    pub display_name: &'static str,
    pub short_label: &'static str,
}

/// Canonical capability-ordered curated Gemini catalog (matches native Antigravity picker & UI pills).
pub const CURATED_GEMINI_MODELS: &[CuratedGeminiModel] = &[
    CuratedGeminiModel {
        id: "gemini-4-carbon",
        display_name: "Gemini 4 Carbon",
        short_label: "4 Carbon",
    },
    CuratedGeminiModel {
        id: "gemini-4-neon",
        display_name: "Gemini 4 Neon",
        short_label: "4 Neon",
    },
    CuratedGeminiModel {
        id: "gemini-4-hydrogen",
        display_name: "Gemini 4 Hydrogen",
        short_label: "4 Hydrogen",
    },
    CuratedGeminiModel {
        id: "gemini-4-argon",
        display_name: "Gemini 4 Argon",
        short_label: "4 Argon",
    },
    CuratedGeminiModel {
        id: "gemini-3.8-flash",
        display_name: "Gemini 3.8 Flash",
        short_label: "3.8 Flash",
    },
];

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ModelPreset {
    pub id: String,
    pub display_name: String,
    pub short_label: String,
}

pub fn curated_model_presets() -> Vec<ModelPreset> {
    CURATED_GEMINI_MODELS
        .iter()
        .map(|m| ModelPreset {
            id: m.id.to_string(),
            display_name: m.display_name.to_string(),
            short_label: m.short_label.to_string(),
        })
        .collect()
}

/// Returns `true` if `model` is Gemini 3.8+ or Gemini 4+ (including Gemini 3.9, 3.10+, 4+, 5+):
/// - `major >= 4`: requires `-<non-empty>` or `.<non-empty>` suffix (rejects bare `gemini-4`, `gemini-5`, `gemini-5-`, `gemini-5.`, `gemini-5pro`).
/// - `major == 3`: requires `.<minor>` with `minor >= 8`, followed by end-of-string or `-<non-empty>` / `.<non-empty>`.
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
    if major >= 4 {
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

/// Parses the `(major, minor)` numeric version pair from `gemini-<major>[.<minor>]...`
/// (stripping optional `"models/"` prefix if present). Returns `(0, 0)` on non-matching input.
pub fn parse_gemini_version_pair(model_id: &str) -> (u32, u32) {
    let clean = model_id.strip_prefix("models/").unwrap_or(model_id);
    let Some(rest) = clean.strip_prefix("gemini-") else {
        return (0, 0);
    };
    let major_end = rest.bytes().take_while(|b| b.is_ascii_digit()).count();
    let major = rest[..major_end].parse::<u32>().unwrap_or(0);
    let minor = rest[major_end..]
        .strip_prefix('.')
        .map(|after_dot| {
            let minor_end = after_dot.bytes().take_while(|b| b.is_ascii_digit()).count();
            after_dot[..minor_end].parse::<u32>().unwrap_or(0)
        })
        .unwrap_or(0);
    (major, minor)
}

/// Orders Gemini models from most capable to least capable, matching Antigravity's native picker:
/// Carbon (0) -> Neon (1) -> Hydrogen (2) -> Argon (3) -> other Gemini 4+ (4, newest version first)
/// -> Gemini 3.9+ (5) -> Gemini 3.8 Flash (6) -> other Gemini 3.8 (7) -> fallback (8).
pub fn model_capability_sort_key(model_id: &str) -> (u8, Reverse<(u32, u32)>, &str) {
    let clean = model_id.strip_prefix("models/").unwrap_or(model_id);
    let version = parse_gemini_version_pair(clean);
    let tier = match clean {
        "gemini-4-carbon" => 0,
        "gemini-4-neon" => 1,
        "gemini-4-hydrogen" => 2,
        "gemini-4-argon" => 3,
        "gemini-3.8-flash" => 6,
        _ => {
            let (major, minor) = version;
            if major >= 4 {
                4
            } else if major == 3 && minor >= 9 {
                5
            } else if major == 3 && minor == 8 {
                7
            } else {
                8
            }
        }
    };
    (tier, Reverse(version), clean)
}

/// Returns `true` when `model_id` (without `models/` prefix) is a supported chat-capable model,
/// filtering out syntax-invalid IDs and specialized non-chat endpoints
/// (TTS, transcription, image, live, translate, embedding).
pub fn is_discoverable_chat_model(model_id: &str) -> bool {
    if validate_gemini_model(model_id).is_err() {
        return false;
    }
    let lower = model_id.to_ascii_lowercase();
    const NON_CHAT_MARKERS: &[&str] = &[
        "-tts",
        "-transcribe",
        "-image",
        "-live",
        "-translate",
        "-embedding",
    ];
    !NON_CHAT_MARKERS.iter().any(|marker| lower.contains(marker))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn curated_catalog_order_labels_and_presets_roundtrip() {
        assert_eq!(CURATED_GEMINI_MODELS.len(), 5);
        let ids: Vec<&str> = CURATED_GEMINI_MODELS.iter().map(|m| m.id).collect();
        assert_eq!(
            ids,
            vec![
                "gemini-4-carbon",
                "gemini-4-neon",
                "gemini-4-hydrogen",
                "gemini-4-argon",
                "gemini-3.8-flash",
            ]
        );
        let labels: Vec<&str> = CURATED_GEMINI_MODELS
            .iter()
            .map(|m| m.short_label)
            .collect();
        assert_eq!(
            labels,
            vec!["4 Carbon", "4 Neon", "4 Hydrogen", "4 Argon", "3.8 Flash"]
        );
        assert!(ids.contains(&DEFAULT_GEMINI_MODEL));
        assert!(validate_gemini_model(DEFAULT_GEMINI_MODEL).is_ok());

        let presets = curated_model_presets();
        assert_eq!(presets.len(), 5);
        let json = serde_json::to_string(&presets).expect("serialize presets");
        let parsed: Vec<ModelPreset> = serde_json::from_str(&json).expect("deserialize presets");
        assert_eq!(parsed, presets);
    }

    #[test]
    fn is_supported_gemini_model_accepts_3_8_plus_and_4_plus_and_rejects_legacy_or_malformed() {
        for ok in [
            "gemini-4-carbon",
            "gemini-4-neon",
            "gemini-4-hydrogen",
            "gemini-4-argon",
            "gemini-4.1-pro",
            "gemini-5-pro",
            "gemini-5.0-flash",
            "gemini-6-ultra",
            "gemini-3.8-flash",
            "gemini-3.8-flash-preview",
            "gemini-3.8",
            "gemini-3.9-pro",
            "gemini-3.10-flash",
        ] {
            assert!(is_supported_gemini_model(ok), "expected supported: '{ok}'");
        }

        for bad in [
            "gemini-3.7-flash",
            "gemini-3.0-pro",
            "gemini-3-pro",
            "gemini-2.5-pro",
            "gemini-2.0-flash",
            "gemini-1.5-pro",
            "gemini-3",
            "gemini-4",
            "gemini-5",
            "gemini-4-",
            "gemini-4.",
            "gemini-5-",
            "gemini-5.",
            "gemini-4hydrogen",
            "gemini-5pro",
            "gemini-3.8-",
            "gemini-3.8flash",
            "gemini-",
            "gemini-abc",
            "models/gemini-4-hydrogen",
            "gemma3:4b",
            "gpt-4o",
            "",
        ] {
            assert!(
                !is_supported_gemini_model(bad),
                "expected rejected: '{bad}'"
            );
        }
    }

    #[test]
    fn validate_gemini_model_rejects_syntax_and_unsupported_accepts_5_and_3_9() {
        for ok in ["gemini-5-pro", "gemini-5.0-flash", "gemini-3.9-pro"] {
            assert!(validate_gemini_model(ok).is_ok(), "expected ok for '{ok}'");
        }

        for bad in [
            "../evil",
            "gemini-4-hydrogen?key=x",
            "gemini-4-hydrogen#f",
            "gemini-4-hydrogen/foo",
            "gemini-4-hydrogen%2f",
            "gemini-4-hydrogen;x",
            "gemini-4-hydrogen\n",
            "  gemini-4-hydrogen",
            "gemini-3.7-flash",
            "gemini-5",
            "gemini-5-",
            "gemini-5.",
            "gemini-5pro",
        ] {
            assert!(
                matches!(
                    validate_gemini_model(bad),
                    Err(CoreError::InvalidModel { .. })
                ),
                "expected InvalidModel for '{bad}'"
            );
        }
    }

    #[test]
    fn parse_gemini_version_pair_handles_major_minor_and_models_prefix() {
        assert_eq!(parse_gemini_version_pair("gemini-4-hydrogen"), (4, 0));
        assert_eq!(parse_gemini_version_pair("gemini-4.1-pro"), (4, 1));
        assert_eq!(parse_gemini_version_pair("gemini-5-pro"), (5, 0));
        assert_eq!(parse_gemini_version_pair("models/gemini-5.2-flash"), (5, 2));
        assert_eq!(parse_gemini_version_pair("gemini-3.8-flash"), (3, 8));
        assert_eq!(parse_gemini_version_pair("gemini-3.10-flash"), (3, 10));
        assert_eq!(parse_gemini_version_pair("gpt-4o"), (0, 0));
        assert_eq!(parse_gemini_version_pair("gemini-pro"), (0, 0));
    }

    #[test]
    fn model_capability_sort_key_orders_by_capability_and_prefix_invariance() {
        let mut models = vec![
            "gpt-4o",
            "gemini-3.8-pro",
            "gemini-3.8-flash",
            "gemini-3.9-pro",
            "gemini-3.10-pro",
            "gemini-4-alpha",
            "gemini-4.1-pro",
            "gemini-5-pro",
            "gemini-4-argon",
            "gemini-4-hydrogen",
            "gemini-4-neon",
            "gemini-4-carbon",
        ];
        models.sort_by_key(|m| model_capability_sort_key(m));
        assert_eq!(
            models,
            vec![
                "gemini-4-carbon",
                "gemini-4-neon",
                "gemini-4-hydrogen",
                "gemini-4-argon",
                "gemini-5-pro",
                "gemini-4.1-pro",
                "gemini-4-alpha",
                "gemini-3.10-pro",
                "gemini-3.9-pro",
                "gemini-3.8-flash",
                "gemini-3.8-pro",
                "gpt-4o",
            ]
        );

        assert_eq!(
            model_capability_sort_key("models/gemini-4-hydrogen"),
            model_capability_sort_key("gemini-4-hydrogen")
        );
    }

    #[test]
    fn is_discoverable_chat_model_filters_non_chat_and_legacy() {
        for ok in [
            "gemini-4-hydrogen",
            "gemini-5-pro",
            "gemini-3.9-pro",
            "gemini-3.8-flash",
        ] {
            assert!(
                is_discoverable_chat_model(ok),
                "expected chat model: '{ok}'"
            );
        }
        for bad in [
            "gemini-4-flash-tts",
            "gemini-4-flash-TTS",
            "gemini-4-transcribe",
            "gemini-4-image",
            "gemini-4-live",
            "gemini-4-translate",
            "gemini-4-embedding",
            "gemini-2.5-pro",
            "gemini-5-pro/../x",
            "gemini-5-pro?k=v",
            "gemini-5-pro#frag",
            "gemini-5-pro%2f",
            "gemini-5-pro;param",
        ] {
            assert!(
                !is_discoverable_chat_model(bad),
                "expected filtered out: '{bad}'"
            );
        }
    }

    #[test]
    fn rejects_unicode_control_whitespace_and_smuggling_in_default_model() {
        use crate::config::Config;

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
            assert!(
                matches!(
                    validate_gemini_model(bad_model),
                    Err(CoreError::InvalidModel { .. })
                ),
                "expected InvalidModel from validate_gemini_model for '{bad_model:?}'"
            );
            let mut cfg = Config::default();
            cfg.providers.gemini.default_model = bad_model.to_string();
            assert!(
                matches!(cfg.validate(), Err(CoreError::InvalidModel { .. })),
                "expected InvalidModel from Config::validate for '{bad_model:?}'"
            );
        }
    }

    #[test]
    fn accepts_valid_default_models_and_rejects_pre_38_models_without_false_positives() {
        use crate::config::Config;

        let valid_models = [
            "gemini-4-hydrogen",
            "gemini-4-argon",
            "gemini-4-neon",
            "gemini-4-carbon",
            "gemini-4.1-pro",
            "gemini-5-pro",
            "gemini-5.0-flash",
            "gemini-30-pro",
            "gemini-40-flash",
            "gemini-3.8-flash",
            "gemini-3.8-flash-preview",
            "gemini-3.9-pro",
            "gemini-3.10-flash",
        ];

        for good_model in valid_models {
            assert!(
                validate_gemini_model(good_model).is_ok(),
                "false positive rejection for valid model '{good_model}'"
            );
            let mut cfg = Config::default();
            cfg.providers.gemini.default_model = good_model.to_string();
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
            assert!(
                matches!(
                    validate_gemini_model(old_model),
                    Err(CoreError::InvalidModel { .. })
                ),
                "expected pre-3.8 or malformed model '{old_model}' to be rejected"
            );
            let mut cfg = Config::default();
            cfg.providers.gemini.default_model = old_model.to_string();
            assert!(
                matches!(cfg.validate(), Err(CoreError::InvalidModel { .. })),
                "expected Config::validate to reject '{old_model}'"
            );
        }
    }
}
