use std::cmp::Reverse;
use std::collections::{HashMap, HashSet};
use std::fmt;
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};

use serde::de::{self, Visitor};
use serde::{Deserialize, Deserializer, Serialize};

use crate::config::CoreError;

pub const DEFAULT_GEMINI_MODEL: &str = "gemini-4-hydrogen";
pub const MAX_GEMINI_MODEL_LEN: usize = 128;

/// Percentage of available input context (`context_window - max_output_tokens`) allocated to the
/// compaction window, leaving headroom for turn overflow, the compaction prompt, and tokenizer variance.
pub const COMPACTION_WINDOW_PERCENT: u64 = 85;
pub const DEFAULT_GEMINI_CONTEXT_WINDOW: u64 = 1_048_576;
pub const DEFAULT_GEMINI_MAX_OUTPUT_TOKENS: u64 = 65_536;
pub const FALLBACK_CODEX_CONTEXT_WINDOW: u64 = 272_000;
pub const FALLBACK_CODEX_MAX_OUTPUT_TOKENS: u64 = 128_000;

/// Derives a model's compaction window from `context_window` and `max_output_tokens`.
///
/// Reserves `max_output_tokens` for the response (`max_input = context_window - max_output_tokens`)
/// and takes 85% of `max_input`, leaving ~15% headroom for turn overflow, the compaction prompt,
/// and tokenizer variance (`compaction_window + overflow + compact_prompt <= max_input`).
pub const fn compute_compaction_window(context_window: u64, max_output_tokens: u64) -> u64 {
    let max_input = context_window.saturating_sub(max_output_tokens);
    ((max_input as u128) * (COMPACTION_WINDOW_PERCENT as u128) / 100) as u64
}

/// Default enabled Gemini models injected into Antigravity's model picker (11/32 placeholder slots).
pub const DEFAULT_ENABLED_GEMINI_MODELS: &[&str] =
    &["gemini-4-carbon", "gemini-4-neon", "gemini-4-hydrogen"];

pub fn default_enabled_gemini_models() -> Vec<String> {
    DEFAULT_ENABLED_GEMINI_MODELS
        .iter()
        .map(|id| (*id).to_string())
        .collect()
}

/// Validates every model ID in `models` via [`validate_gemini_model`] and deduplicates entries
/// while preserving first-seen order. An empty list is valid (non-destructive pause).
pub fn normalize_enabled_gemini_models(models: Vec<String>) -> Result<Vec<String>, CoreError> {
    let mut seen = HashSet::with_capacity(models.len());
    let mut normalized = Vec::with_capacity(models.len());
    for model in models {
        validate_gemini_model(&model)?;
        if seen.insert(model.clone()) {
            normalized.push(model);
        }
    }
    Ok(normalized)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CuratedGeminiModel {
    pub id: &'static str,
    pub display_name: &'static str,
    pub short_label: &'static str,
    pub context_window: u64,
    pub max_output_tokens: u64,
    pub compaction_window: u64,
}

/// Canonical capability-ordered curated Gemini catalog (matches native Antigravity picker & UI pills).
pub const CURATED_GEMINI_MODELS: &[CuratedGeminiModel] = &[
    CuratedGeminiModel {
        id: "gemini-4-carbon",
        display_name: "Gemini 4 Carbon",
        short_label: "4 Carbon",
        context_window: DEFAULT_GEMINI_CONTEXT_WINDOW,
        max_output_tokens: DEFAULT_GEMINI_MAX_OUTPUT_TOKENS,
        compaction_window: compute_compaction_window(
            DEFAULT_GEMINI_CONTEXT_WINDOW,
            DEFAULT_GEMINI_MAX_OUTPUT_TOKENS,
        ),
    },
    CuratedGeminiModel {
        id: "gemini-4-neon",
        display_name: "Gemini 4 Neon",
        short_label: "4 Neon",
        context_window: DEFAULT_GEMINI_CONTEXT_WINDOW,
        max_output_tokens: DEFAULT_GEMINI_MAX_OUTPUT_TOKENS,
        compaction_window: compute_compaction_window(
            DEFAULT_GEMINI_CONTEXT_WINDOW,
            DEFAULT_GEMINI_MAX_OUTPUT_TOKENS,
        ),
    },
    CuratedGeminiModel {
        id: "gemini-4-hydrogen",
        display_name: "Gemini 4 Hydrogen",
        short_label: "4 Hydrogen",
        context_window: DEFAULT_GEMINI_CONTEXT_WINDOW,
        max_output_tokens: DEFAULT_GEMINI_MAX_OUTPUT_TOKENS,
        compaction_window: compute_compaction_window(
            DEFAULT_GEMINI_CONTEXT_WINDOW,
            DEFAULT_GEMINI_MAX_OUTPUT_TOKENS,
        ),
    },
    CuratedGeminiModel {
        id: "gemini-4-argon",
        display_name: "Gemini 4 Argon",
        short_label: "4 Argon",
        context_window: DEFAULT_GEMINI_CONTEXT_WINDOW,
        max_output_tokens: DEFAULT_GEMINI_MAX_OUTPUT_TOKENS,
        compaction_window: compute_compaction_window(
            DEFAULT_GEMINI_CONTEXT_WINDOW,
            DEFAULT_GEMINI_MAX_OUTPUT_TOKENS,
        ),
    },
    CuratedGeminiModel {
        id: "gemini-3.8-flash",
        display_name: "Gemini 3.8 Flash",
        short_label: "3.8 Flash",
        context_window: DEFAULT_GEMINI_CONTEXT_WINDOW,
        max_output_tokens: DEFAULT_GEMINI_MAX_OUTPUT_TOKENS,
        compaction_window: compute_compaction_window(
            DEFAULT_GEMINI_CONTEXT_WINDOW,
            DEFAULT_GEMINI_MAX_OUTPUT_TOKENS,
        ),
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
    if model.len() > MAX_GEMINI_MODEL_LEN {
        return false;
    }
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
    if model.len() > MAX_GEMINI_MODEL_LEN {
        return Err(CoreError::InvalidModel {
            reason: format!("model ID exceeds maximum length of {MAX_GEMINI_MODEL_LEN} characters"),
        });
    }
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

pub const DEFAULT_CODEX_MODEL: &str = "gpt-6.2-sol";
pub const MAX_CODEX_MODEL_LEN: usize = 128;

/// Default enabled OpenAI Codex models injected into Antigravity's model picker (8/32 placeholder slots).
pub const DEFAULT_ENABLED_CODEX_MODELS: &[&str] = &["gpt-6.2-astra", "gpt-6.2-sol"];

pub fn default_enabled_codex_models() -> Vec<String> {
    DEFAULT_ENABLED_CODEX_MODELS
        .iter()
        .map(|id| (*id).to_string())
        .collect()
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CuratedCodexModel {
    pub id: &'static str,
    pub display_name: &'static str,
    pub short_label: &'static str,
    pub context_window: u64,
    pub max_output_tokens: u64,
    pub compaction_window: u64,
    pub reasoning_efforts: &'static [&'static str],
    pub default_enabled: bool,
}

/// Canonical capability-ordered curated GPT-6.x Codex catalog (matches native Antigravity picker & UI pills).
pub const CURATED_CODEX_MODELS: &[CuratedCodexModel] = &[
    CuratedCodexModel {
        id: "gpt-6.2-astra",
        display_name: "GPT-6.2 Astra",
        short_label: "6.2 Astra",
        context_window: 1_050_000,
        max_output_tokens: 128_000,
        compaction_window: compute_compaction_window(1_050_000, 128_000),
        reasoning_efforts: &["low", "medium", "high", "xhigh"],
        default_enabled: true,
    },
    CuratedCodexModel {
        id: "gpt-6.2-sol",
        display_name: "GPT-6.2 Sol",
        short_label: "6.2 Sol",
        context_window: 1_050_000,
        max_output_tokens: 128_000,
        compaction_window: compute_compaction_window(1_050_000, 128_000),
        reasoning_efforts: &["low", "medium", "high", "xhigh"],
        default_enabled: true,
    },
    CuratedCodexModel {
        id: "gpt-6.1-terra",
        display_name: "GPT-6.1 Terra",
        short_label: "6.1 Terra",
        context_window: 1_050_000,
        max_output_tokens: 128_000,
        compaction_window: compute_compaction_window(1_050_000, 128_000),
        reasoning_efforts: &["low", "medium", "high"],
        default_enabled: false,
    },
    CuratedCodexModel {
        id: "gpt-6-luna",
        display_name: "GPT-6 Luna",
        short_label: "6 Luna",
        context_window: 1_050_000,
        max_output_tokens: 128_000,
        compaction_window: compute_compaction_window(1_050_000, 128_000),
        reasoning_efforts: &["low", "medium", "high"],
        default_enabled: false,
    },
];

pub fn curated_codex_model_presets() -> Vec<ModelPreset> {
    CURATED_CODEX_MODELS
        .iter()
        .map(|m| ModelPreset {
            id: m.id.to_string(),
            display_name: m.display_name.to_string(),
            short_label: m.short_label.to_string(),
        })
        .collect()
}

pub fn find_curated_codex_model(id: &str) -> Option<&'static CuratedCodexModel> {
    CURATED_CODEX_MODELS.iter().find(|m| m.id == id)
}

/// Validates an OpenAI Codex model identifier for strict ASCII slug safety and non-collision
/// with Gemini model IDs.
pub fn validate_codex_model(model: &str) -> Result<(), CoreError> {
    if model.len() > MAX_CODEX_MODEL_LEN {
        return Err(CoreError::InvalidModel {
            reason: format!(
                "codex model ID exceeds maximum length of {MAX_CODEX_MODEL_LEN} characters"
            ),
        });
    }
    let bytes = model.as_bytes();
    if bytes.is_empty()
        || !bytes[0].is_ascii_alphanumeric()
        || !bytes
            .iter()
            .all(|b| b.is_ascii_alphanumeric() || matches!(*b, b'.' | b'_' | b'-'))
        || model.contains("..")
    {
        return Err(CoreError::InvalidModel {
            reason:
                "codex model ID must be non-empty ASCII matching [a-zA-Z0-9._-] starting with an alphanumeric character and free of '..'"
                    .to_string(),
        });
    }
    let lower = model.to_ascii_lowercase();
    if lower == "gemini" || lower.starts_with("gemini-") || lower.starts_with("gemini.") {
        return Err(CoreError::InvalidModel {
            reason: "codex model ID must not collide with Gemini model prefixes".to_string(),
        });
    }
    Ok(())
}

/// Returns `true` when `model` is a valid Codex model ID suitable for proxy dispatch to OpenAI
/// Codex (curated GPT-6.x model or `gpt-*` / `codex-*` slug).
pub fn is_codex_model(model: &str) -> bool {
    if validate_codex_model(model).is_err() {
        return false;
    }
    if find_curated_codex_model(model).is_some() {
        return true;
    }
    let lower = model.to_ascii_lowercase();
    lower
        .strip_prefix("gpt-")
        .or_else(|| lower.strip_prefix("codex-"))
        .is_some_and(|rest| {
            rest.as_bytes()
                .first()
                .is_some_and(u8::is_ascii_alphanumeric)
        })
}

/// Validates every model ID in `models` via [`validate_codex_model`] and deduplicates entries
/// while preserving first-seen order. An empty list is valid (non-destructive pause).
pub fn normalize_enabled_codex_models(models: Vec<String>) -> Result<Vec<String>, CoreError> {
    let mut seen = HashSet::with_capacity(models.len());
    let mut normalized = Vec::with_capacity(models.len());
    for model in models {
        validate_codex_model(&model)?;
        if seen.insert(model.clone()) {
            normalized.push(model);
        }
    }
    Ok(normalized)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct StaticPlaceholderMapping {
    pub provider: &'static str,
    pub model_id: &'static str,
    pub display_name: &'static str,
}

/// Normalizes a placeholder enum key:
/// - `"530"` -> `Some("MODEL_PLACEHOLDER_M530")`
/// - `"MODEL_PLACEHOLDER_M530"` -> `Some("MODEL_PLACEHOLDER_M530")`
/// - anything else -> `None`
pub fn normalize_placeholder_enum_key(raw: &str) -> Option<String> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return None;
    }
    if trimmed.bytes().all(|b| b.is_ascii_digit()) {
        return Some(format!("MODEL_PLACEHOLDER_M{trimmed}"));
    }
    if let Some(rest) = trimmed.strip_prefix("MODEL_PLACEHOLDER_M")
        && !rest.is_empty()
        && rest.bytes().all(|b| b.is_ascii_digit())
    {
        return Some(trimmed.to_string());
    }
    None
}

/// Returns the bare numeric alias for a placeholder enum key (e.g. `"MODEL_PLACEHOLDER_M530"` -> `Some("530")`).
pub fn placeholder_numeric_alias(raw: &str) -> Option<String> {
    let trimmed = raw.trim();
    if let Some(rest) = trimmed.strip_prefix("MODEL_PLACEHOLDER_M")
        && !rest.is_empty()
        && rest.bytes().all(|b| b.is_ascii_digit())
    {
        return Some(rest.to_string());
    }
    None
}

/// Static fallback mapping for Antigravity `language_server` `MODEL_PLACEHOLDER_M*` enum slots
/// when dynamic sources (`GetCascadeModelConfigData`, `fetchAvailableModels`,
/// `GetCascadeTrajectoryGeneratorMetadata`, `meta.model_info`) did not resolve the slot.
pub fn lookup_static_placeholder(raw: &str) -> Option<StaticPlaceholderMapping> {
    let canonical = normalize_placeholder_enum_key(raw)?;
    let num: u32 = canonical
        .strip_prefix("MODEL_PLACEHOLDER_M")?
        .parse()
        .ok()?;
    match num {
        525..=531 | 432..=434 | 669 => Some(StaticPlaceholderMapping {
            provider: "antigravity",
            model_id: "gemini-4-hydrogen",
            display_name: "Gemini 4 Hydrogen",
        }),
        532..=536 | 465..=468 => Some(StaticPlaceholderMapping {
            provider: "antigravity",
            model_id: "gemini-4-neon",
            display_name: "Gemini 4 Neon",
        }),
        427 | 437 | 438 => Some(StaticPlaceholderMapping {
            provider: "antigravity",
            model_id: "gemini-4-carbon",
            display_name: "Gemini 4 Carbon",
        }),
        417..=421 | 670 => Some(StaticPlaceholderMapping {
            provider: "antigravity",
            model_id: "gemini-4-argon",
            display_name: "Gemini 4 Argon",
        }),
        318..=320 | 322 => Some(StaticPlaceholderMapping {
            provider: "antigravity",
            model_id: "gemini-3.8-flash",
            display_name: "Gemini 3.8 Flash",
        }),
        400..=402 => Some(StaticPlaceholderMapping {
            provider: "vertex_claude",
            model_id: "claude-opus-5-5",
            display_name: "Claude Opus 5.5",
        }),
        403..=405 => Some(StaticPlaceholderMapping {
            provider: "vertex_claude",
            model_id: "claude-sonnet-5-5",
            display_name: "Claude Sonnet 5.5",
        }),
        50 | 198 => Some(StaticPlaceholderMapping {
            provider: "antigravity",
            model_id: "gemini-2.5-flash",
            display_name: "Gemini 2.5 Flash",
        }),
        537 => Some(StaticPlaceholderMapping {
            provider: "antigravity",
            model_id: "gemini-pro-orchestrator-neon",
            display_name: "Orchestrator (Neon)",
        }),
        667 | 668 => Some(StaticPlaceholderMapping {
            provider: "antigravity",
            model_id: "gemini-pro-orchestrator",
            display_name: "Orchestrator (Carbon)",
        }),
        681 => Some(StaticPlaceholderMapping {
            provider: "antigravity",
            model_id: "gemini-pro-orchestrator-argon",
            display_name: "Orchestrator (Argon)",
        }),
        _ => None,
    }
}

fn deserialize_opt_string_or_i64<'de, D>(deserializer: D) -> Result<Option<String>, D::Error>
where
    D: Deserializer<'de>,
{
    struct OptStringOrI64Visitor;

    impl<'de> Visitor<'de> for OptStringOrI64Visitor {
        type Value = Option<String>;

        fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
            f.write_str("a string, integer enum value, or null")
        }

        fn visit_str<E: de::Error>(self, v: &str) -> Result<Self::Value, E> {
            let trimmed = v.trim();
            if trimmed.is_empty() {
                Ok(None)
            } else {
                Ok(Some(trimmed.to_string()))
            }
        }

        fn visit_string<E: de::Error>(self, v: String) -> Result<Self::Value, E> {
            self.visit_str(&v)
        }

        fn visit_i64<E: de::Error>(self, v: i64) -> Result<Self::Value, E> {
            Ok(Some(v.to_string()))
        }

        fn visit_u64<E: de::Error>(self, v: u64) -> Result<Self::Value, E> {
            Ok(Some(v.to_string()))
        }

        fn visit_unit<E: de::Error>(self) -> Result<Self::Value, E> {
            Ok(None)
        }

        fn visit_none<E: de::Error>(self) -> Result<Self::Value, E> {
            Ok(None)
        }

        fn visit_some<D2: Deserializer<'de>>(self, d: D2) -> Result<Self::Value, D2::Error> {
            d.deserialize_any(self)
        }
    }

    deserializer.deserialize_any(OptStringOrI64Visitor)
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UpstreamModelEntry {
    pub model_id: Option<String>,
    pub display_name: Option<String>,
    pub model_provider: Option<String>,
    pub api_provider: Option<String>,
    pub model_url: Option<String>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct FetchAvailableModelItemDto {
    #[serde(default, deserialize_with = "deserialize_opt_string_or_i64")]
    model: Option<String>,
    #[serde(default)]
    model_id: Option<String>,
    #[serde(default)]
    display_name: Option<String>,
    #[serde(default, deserialize_with = "deserialize_opt_string_or_i64")]
    model_provider: Option<String>,
    #[serde(default, deserialize_with = "deserialize_opt_string_or_i64")]
    api_provider: Option<String>,
    #[serde(default)]
    model_url: Option<String>,
}

#[derive(Default, Deserialize)]
#[serde(rename_all = "camelCase")]
struct FetchAvailableModelsDto {
    #[serde(default)]
    models: HashMap<String, FetchAvailableModelItemDto>,
}

/// Shared in-process cache of `fetchAvailableModels` entries keyed by normalized placeholder enum
/// (e.g. `MODEL_PLACEHOLDER_M530`), bridging `companion-proxy` and `LsCollector`.
#[derive(Debug, Default)]
pub struct UpstreamModelCatalog {
    entries: Mutex<HashMap<String, UpstreamModelEntry>>,
    version: AtomicU64,
}

impl UpstreamModelCatalog {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn version(&self) -> u64 {
        self.version.load(Ordering::Acquire)
    }

    pub fn snapshot(&self) -> HashMap<String, UpstreamModelEntry> {
        self.entries
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone()
    }

    pub fn update_from_fetch_available_models(&self, body: &[u8]) {
        let Ok(parsed) = serde_json::from_slice::<FetchAvailableModelsDto>(body) else {
            return;
        };
        if parsed.models.is_empty() {
            return;
        }
        let mut guard = self
            .entries
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let mut changed = false;
        for (slug, item) in parsed.models {
            let Some(raw_model) = item
                .model
                .as_deref()
                .map(str::trim)
                .filter(|s| !s.is_empty())
            else {
                continue;
            };
            let key =
                normalize_placeholder_enum_key(raw_model).unwrap_or_else(|| raw_model.to_string());
            let model_id = item
                .model_id
                .as_deref()
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(str::to_string)
                .or_else(|| {
                    let s = slug.trim();
                    (!s.is_empty()).then(|| s.to_string())
                });
            let entry = UpstreamModelEntry {
                model_id,
                display_name: item
                    .display_name
                    .as_deref()
                    .map(str::trim)
                    .filter(|s| !s.is_empty())
                    .map(str::to_string),
                model_provider: item.model_provider,
                api_provider: item.api_provider,
                model_url: item
                    .model_url
                    .as_deref()
                    .map(str::trim)
                    .filter(|s| !s.is_empty())
                    .map(str::to_string),
            };
            if guard.get(&key) != Some(&entry) {
                guard.insert(key, entry);
                changed = true;
            }
        }
        if changed {
            self.version.fetch_add(1, Ordering::Release);
        }
    }
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

        for model in CURATED_GEMINI_MODELS {
            assert_eq!(model.context_window, DEFAULT_GEMINI_CONTEXT_WINDOW);
            assert_eq!(model.max_output_tokens, DEFAULT_GEMINI_MAX_OUTPUT_TOKENS);
            assert_eq!(model.compaction_window, 835_584);
            assert!(model.compaction_window < model.context_window - model.max_output_tokens);
        }

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

    #[test]
    fn default_enabled_gemini_models_and_normalization_preserve_order_and_reject_invalid() {
        assert_eq!(
            DEFAULT_ENABLED_GEMINI_MODELS,
            &["gemini-4-carbon", "gemini-4-neon", "gemini-4-hydrogen"]
        );
        let defaults = default_enabled_gemini_models();
        assert_eq!(defaults.len(), 3);
        for model in &defaults {
            assert!(validate_gemini_model(model).is_ok());
        }

        // Empty list is valid (non-destructive pause).
        assert_eq!(
            normalize_enabled_gemini_models(Vec::new()).unwrap(),
            Vec::<String>::new()
        );

        // Duplicates are deduplicated preserving first-seen order.
        let deduped = normalize_enabled_gemini_models(vec![
            "gemini-4-neon".to_string(),
            "gemini-4-carbon".to_string(),
            "gemini-4-neon".to_string(),
            "gemini-3.8-flash".to_string(),
            "gemini-4-carbon".to_string(),
        ])
        .unwrap();
        assert_eq!(
            deduped,
            vec![
                "gemini-4-neon".to_string(),
                "gemini-4-carbon".to_string(),
                "gemini-3.8-flash".to_string(),
            ]
        );

        // Invalid model IDs fail closed with InvalidModel.
        for bad in [
            "gemini-2.5-pro",
            "../evil",
            "",
            "gemini-5",
            " gemini-4-carbon",
        ] {
            assert!(
                matches!(
                    normalize_enabled_gemini_models(vec![
                        "gemini-4-carbon".to_string(),
                        bad.to_string(),
                    ]),
                    Err(CoreError::InvalidModel { .. })
                ),
                "expected InvalidModel for '{bad}'"
            );
        }
    }

    #[test]
    fn gemini_model_id_enforces_128_char_length_cap() {
        let prefix = "gemini-4-";
        let at_limit = format!("{prefix}{}", "a".repeat(128 - prefix.len()));
        assert_eq!(at_limit.len(), 128);
        assert!(is_supported_gemini_model(&at_limit));
        assert!(validate_gemini_model(&at_limit).is_ok());

        let over_limit = format!("{prefix}{}", "a".repeat(129 - prefix.len()));
        assert_eq!(over_limit.len(), 129);
        assert!(!is_supported_gemini_model(&over_limit));
        assert!(matches!(
            validate_gemini_model(&over_limit),
            Err(CoreError::InvalidModel { .. })
        ));

        let huge = format!("{prefix}{}", "a".repeat(1200));
        assert!(!is_supported_gemini_model(&huge));
        assert!(matches!(
            validate_gemini_model(&huge),
            Err(CoreError::InvalidModel { .. })
        ));
    }

    #[test]
    fn curated_codex_catalog_order_labels_metadata_and_presets_roundtrip() {
        assert_eq!(CURATED_CODEX_MODELS.len(), 4);
        let ids: Vec<&str> = CURATED_CODEX_MODELS.iter().map(|m| m.id).collect();
        assert_eq!(
            ids,
            vec![
                "gpt-6.2-astra",
                "gpt-6.2-sol",
                "gpt-6.1-terra",
                "gpt-6-luna",
            ]
        );
        let labels: Vec<&str> = CURATED_CODEX_MODELS.iter().map(|m| m.short_label).collect();
        assert_eq!(labels, vec!["6.2 Astra", "6.2 Sol", "6.1 Terra", "6 Luna"]);
        assert_eq!(DEFAULT_CODEX_MODEL, "gpt-6.2-sol");
        assert_eq!(
            DEFAULT_ENABLED_CODEX_MODELS,
            &["gpt-6.2-astra", "gpt-6.2-sol"]
        );
        assert_eq!(
            default_enabled_codex_models(),
            vec!["gpt-6.2-astra".to_string(), "gpt-6.2-sol".to_string()]
        );

        let astra = find_curated_codex_model("gpt-6.2-astra").expect("astra");
        assert_eq!(astra.context_window, 1_050_000);
        assert_eq!(astra.max_output_tokens, 128_000);
        assert_eq!(astra.compaction_window, 783_700);
        assert_eq!(astra.reasoning_efforts, &["low", "medium", "high", "xhigh"]);
        assert!(astra.default_enabled);

        let sol = find_curated_codex_model("gpt-6.2-sol").expect("sol");
        assert_eq!(sol.context_window, 1_050_000);
        assert_eq!(sol.compaction_window, 783_700);
        assert!(sol.default_enabled);

        let terra = find_curated_codex_model("gpt-6.1-terra").expect("terra");
        assert_eq!(terra.context_window, 1_050_000);
        assert_eq!(terra.compaction_window, 783_700);
        assert_eq!(terra.reasoning_efforts, &["low", "medium", "high"]);
        assert!(!terra.default_enabled);

        let luna = find_curated_codex_model("gpt-6-luna").expect("luna");
        assert_eq!(luna.context_window, 1_050_000);
        assert_eq!(luna.compaction_window, 783_700);
        assert!(!luna.default_enabled);
        assert!(find_curated_codex_model("gpt-5.4").is_none());

        for model in CURATED_CODEX_MODELS {
            assert_eq!(
                model.compaction_window,
                compute_compaction_window(model.context_window, model.max_output_tokens)
            );
            assert!(model.compaction_window < model.context_window - model.max_output_tokens);
        }

        let presets = curated_codex_model_presets();
        assert_eq!(presets.len(), 4);
        let json = serde_json::to_string(&presets).expect("serialize presets");
        let parsed: Vec<ModelPreset> = serde_json::from_str(&json).expect("deserialize presets");
        assert_eq!(parsed, presets);
    }

    #[test]
    fn compute_compaction_window_handles_curated_fallback_underflow_zero_and_u64_max() {
        assert_eq!(COMPACTION_WINDOW_PERCENT, 85);
        // Curated Codex: (1_050_000 - 128_000) * 85 / 100 = 922_000 * 85 / 100 = 783_700
        assert_eq!(compute_compaction_window(1_050_000, 128_000), 783_700);
        // Fallback Codex: (272_000 - 128_000) * 85 / 100 = 144_000 * 85 / 100 = 122_400
        assert_eq!(
            compute_compaction_window(
                FALLBACK_CODEX_CONTEXT_WINDOW,
                FALLBACK_CODEX_MAX_OUTPUT_TOKENS
            ),
            122_400
        );
        // Curated Gemini: (1_048_576 - 65_536) * 85 / 100 = 983_040 * 85 / 100 = 835_584
        assert_eq!(
            compute_compaction_window(
                DEFAULT_GEMINI_CONTEXT_WINDOW,
                DEFAULT_GEMINI_MAX_OUTPUT_TOKENS
            ),
            835_584
        );
        // Underflow / equal bounds saturate at 0
        assert_eq!(compute_compaction_window(128_000, 128_000), 0);
        assert_eq!(compute_compaction_window(64_000, 128_000), 0);
        // Zero inputs
        assert_eq!(compute_compaction_window(0, 0), 0);
        assert_eq!(compute_compaction_window(0, 65_536), 0);
        // Overflow safety on u64::MAX (uses u128 intermediate)
        let expected_max = ((u64::MAX as u128) * 85 / 100) as u64;
        assert_eq!(compute_compaction_window(u64::MAX, 0), expected_max);
    }

    #[test]
    fn validate_and_classify_codex_models_enforce_slug_bounds_and_reject_gemini_collision() {
        for ok in [
            "gpt-6.2-astra",
            "gpt-6.2-sol",
            "gpt-6.1-terra",
            "gpt-6-luna",
            "gpt-7.0-nova_1",
            "codex-mini-latest",
            "custom_model.v2",
        ] {
            assert!(validate_codex_model(ok).is_ok(), "expected valid: '{ok}'");
        }

        // Exact 128-char boundary accepted; 129+ rejected.
        let at_limit = format!("gpt-6-{}", "a".repeat(MAX_CODEX_MODEL_LEN - "gpt-6-".len()));
        assert_eq!(at_limit.len(), MAX_CODEX_MODEL_LEN);
        assert!(validate_codex_model(&at_limit).is_ok());
        assert!(is_codex_model(&at_limit));

        let over_limit = format!(
            "gpt-6-{}",
            "a".repeat(MAX_CODEX_MODEL_LEN + 1 - "gpt-6-".len())
        );
        assert!(matches!(
            validate_codex_model(&over_limit),
            Err(CoreError::InvalidModel { .. })
        ));
        assert!(!is_codex_model(&over_limit));

        // Invalid syntax and Gemini prefix collisions rejected.
        for bad in [
            "",
            " ",
            ".hidden",
            "-leading-dash",
            "_leading_underscore",
            "gpt-6..astra",
            "../evil",
            "gpt-6.2-sol/foo",
            "gpt-6.2-sol?k=v",
            "gpt-6.2-sol#frag",
            "gpt-6.2-sol%2f",
            "gpt-6.2-sol;x",
            "gpt-6.2-sol\n",
            "gpt-6.2-sol\0",
            " gpt-6.2-sol",
            "gpt-6.2-sol ",
            "gemini",
            "GEMINI",
            "gemini-4-hydrogen",
            "Gemini-4-Carbon",
            "gemini.custom",
        ] {
            assert!(
                matches!(
                    validate_codex_model(bad),
                    Err(CoreError::InvalidModel { .. })
                ),
                "expected InvalidModel for '{bad:?}'"
            );
            assert!(
                !is_codex_model(bad),
                "expected !is_codex_model for '{bad:?}'"
            );
        }

        // Non-Gemini arbitrary slugs validate in config but are not auto-dispatched as Codex.
        assert!(validate_codex_model("claude-4-opus").is_ok());
        assert!(!is_codex_model("claude-4-opus"));
        assert!(!is_codex_model("gpt-"));
        assert!(!is_codex_model("codex-"));
        assert!(!is_codex_model("GPT-"));
        assert!(!is_codex_model("CODEX-"));
        assert!(!is_codex_model("gpt--"));
        assert!(!is_codex_model("gpt-."));
        assert!(!is_codex_model("gpt-_"));
        assert!(!is_codex_model("codex--"));
        assert!(is_codex_model("gpt-6.2-astra"));
        assert!(is_codex_model("GPT-6.3-Preview"));
        assert!(is_codex_model("codex-next"));
    }

    #[test]
    fn normalize_enabled_codex_models_supports_empty_pause_dedup_and_fail_closed() {
        assert_eq!(
            normalize_enabled_codex_models(Vec::new()).unwrap(),
            Vec::<String>::new()
        );

        let deduped = normalize_enabled_codex_models(vec![
            "gpt-6.2-sol".to_string(),
            "gpt-6.2-astra".to_string(),
            "gpt-6.2-sol".to_string(),
            "gpt-6-luna".to_string(),
        ])
        .unwrap();
        assert_eq!(
            deduped,
            vec![
                "gpt-6.2-sol".to_string(),
                "gpt-6.2-astra".to_string(),
                "gpt-6-luna".to_string(),
            ]
        );

        for bad in ["gemini-4-hydrogen", "../evil", "", "gpt-6..bad"] {
            assert!(matches!(
                normalize_enabled_codex_models(vec!["gpt-6.2-sol".to_string(), bad.to_string()]),
                Err(CoreError::InvalidModel { .. })
            ));
        }
    }

    #[test]
    fn static_placeholder_lookup_maps_known_enums_and_numeric_aliases() {
        assert_eq!(
            lookup_static_placeholder("MODEL_PLACEHOLDER_M530"),
            Some(StaticPlaceholderMapping {
                provider: "antigravity",
                model_id: "gemini-4-hydrogen",
                display_name: "Gemini 4 Hydrogen",
            })
        );
        assert_eq!(
            lookup_static_placeholder("530"),
            lookup_static_placeholder("MODEL_PLACEHOLDER_M530")
        );
        assert_eq!(
            lookup_static_placeholder("MODEL_PLACEHOLDER_M432").map(|m| m.model_id),
            Some("gemini-4-hydrogen")
        );
        assert_eq!(
            lookup_static_placeholder("MODEL_PLACEHOLDER_M669").map(|m| m.model_id),
            Some("gemini-4-hydrogen")
        );
        assert_eq!(
            lookup_static_placeholder("MODEL_PLACEHOLDER_M535").map(|m| m.model_id),
            Some("gemini-4-neon")
        );
        assert_eq!(
            lookup_static_placeholder("MODEL_PLACEHOLDER_M468").map(|m| m.model_id),
            Some("gemini-4-neon")
        );
        assert_eq!(
            lookup_static_placeholder("MODEL_PLACEHOLDER_M427").map(|m| m.model_id),
            Some("gemini-4-carbon")
        );
        assert_eq!(
            lookup_static_placeholder("MODEL_PLACEHOLDER_M438").map(|m| m.model_id),
            Some("gemini-4-carbon")
        );
        assert_eq!(
            lookup_static_placeholder("MODEL_PLACEHOLDER_M417").map(|m| m.model_id),
            Some("gemini-4-argon")
        );
        assert_eq!(
            lookup_static_placeholder("MODEL_PLACEHOLDER_M670").map(|m| m.model_id),
            Some("gemini-4-argon")
        );
        assert_eq!(
            lookup_static_placeholder("MODEL_PLACEHOLDER_M318").map(|m| m.model_id),
            Some("gemini-3.8-flash")
        );
        assert_eq!(
            lookup_static_placeholder("MODEL_PLACEHOLDER_M322").map(|m| m.model_id),
            Some("gemini-3.8-flash")
        );
        assert_eq!(
            lookup_static_placeholder("MODEL_PLACEHOLDER_M400"),
            Some(StaticPlaceholderMapping {
                provider: "vertex_claude",
                model_id: "claude-opus-5-5",
                display_name: "Claude Opus 5.5",
            })
        );
        assert_eq!(
            lookup_static_placeholder("MODEL_PLACEHOLDER_M405"),
            Some(StaticPlaceholderMapping {
                provider: "vertex_claude",
                model_id: "claude-sonnet-5-5",
                display_name: "Claude Sonnet 5.5",
            })
        );
        assert_eq!(
            lookup_static_placeholder("MODEL_PLACEHOLDER_M50").map(|m| m.model_id),
            Some("gemini-2.5-flash")
        );
        assert_eq!(
            lookup_static_placeholder("MODEL_PLACEHOLDER_M198").map(|m| m.model_id),
            Some("gemini-2.5-flash")
        );
        assert_eq!(
            lookup_static_placeholder("MODEL_PLACEHOLDER_M537").map(|m| m.model_id),
            Some("gemini-pro-orchestrator-neon")
        );
        assert_eq!(
            lookup_static_placeholder("MODEL_PLACEHOLDER_M667").map(|m| m.model_id),
            Some("gemini-pro-orchestrator")
        );
        assert_eq!(
            lookup_static_placeholder("MODEL_PLACEHOLDER_M681").map(|m| m.model_id),
            Some("gemini-pro-orchestrator-argon")
        );
        assert_eq!(lookup_static_placeholder("MODEL_PLACEHOLDER_M9999"), None);
        assert_eq!(
            normalize_placeholder_enum_key("530").as_deref(),
            Some("MODEL_PLACEHOLDER_M530")
        );
        assert_eq!(
            placeholder_numeric_alias("MODEL_PLACEHOLDER_M530").as_deref(),
            Some("530")
        );
    }

    #[test]
    fn upstream_model_catalog_tracks_placeholder_entries_and_versions() {
        let catalog = UpstreamModelCatalog::new();
        assert_eq!(catalog.version(), 0);
        let payload = br#"{
            "models": {
                "gemini-4-hydrogen-tiered-high": {
                    "model": "MODEL_PLACEHOLDER_M530",
                    "displayName": "Gemini 4 Hydrogen (High)",
                    "modelProvider": "MODEL_PROVIDER_GOOGLE",
                    "apiProvider": "API_PROVIDER_GOOGLE_GEMINI"
                },
                "companion/gpt-6.2-sol-medium": {
                    "model": 356,
                    "displayName": "GPT-6.2 Sol (Codex) (Medium)",
                    "modelUrl": "gemini-api://gpt-6.2-sol",
                    "modelProvider": "MODEL_PROVIDER_GOOGLE",
                    "apiProvider": "API_PROVIDER_GOOGLE_GEMINI"
                }
            }
        }"#;
        catalog.update_from_fetch_available_models(payload);
        assert!(catalog.version() > 0);
        let snap = catalog.snapshot();
        let m530 = snap.get("MODEL_PLACEHOLDER_M530").expect("M530 entry");
        assert_eq!(
            m530.model_id.as_deref(),
            Some("gemini-4-hydrogen-tiered-high")
        );
        assert_eq!(
            m530.display_name.as_deref(),
            Some("Gemini 4 Hydrogen (High)")
        );
        let m356 = snap
            .get("MODEL_PLACEHOLDER_M356")
            .expect("M356 numeric normalized");
        assert_eq!(
            m356.model_id.as_deref(),
            Some("companion/gpt-6.2-sol-medium")
        );
        assert_eq!(m356.model_url.as_deref(), Some("gemini-api://gpt-6.2-sol"));
    }
}
