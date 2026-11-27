use std::collections::{HashMap, HashSet};

use companion_core::{
    CURATED_CODEX_MODELS, is_codex_model, is_supported_gemini_model, model_capability_sort_key,
};
use serde_json::{Map, Value};

// Dedicated custom placeholder enum range in Antigravity (MODEL_PLACEHOLDER_M334..=M365).
const PLACEHOLDER_ENUM_START: u32 = 334;
const PLACEHOLDER_ENUM_END: u32 = 365;
const PLACEHOLDER_ENUM_COUNT: u32 = PLACEHOLDER_ENUM_END - PLACEHOLDER_ENUM_START + 1;

#[derive(Debug, Clone, Copy)]
pub(super) struct ThinkingVariant {
    pub(super) suffix: &'static str,
    // Must match Antigravity UI EFFORT_LABEL_RE: low|medium|high|xhigh|max (case-insensitive)
    // so variants collapse into a single row with the native "Thinking effort" secondary menu.
    pub(super) label_effort: &'static str,
    // Matches Antigravity EFFORT_LABELS for groupedModels metadata.
    pub(super) grouped_display_name: &'static str,
    pub(super) thinking_level: i32,
}

// Values match Antigravity defaultmodels/gemini_models.go:
// Low = 1, Medium = 2, High = 3, XHigh = 7.
const STANDARD_GEMINI_LEVELS: &[ThinkingVariant] = &[
    ThinkingVariant {
        suffix: "low",
        label_effort: "Low",
        grouped_display_name: "Low",
        thinking_level: 1,
    },
    ThinkingVariant {
        suffix: "medium",
        label_effort: "Medium",
        grouped_display_name: "Medium",
        thinking_level: 2,
    },
    ThinkingVariant {
        suffix: "high",
        label_effort: "High",
        grouped_display_name: "High",
        thinking_level: 3,
    },
];

const XHIGH_GEMINI_LEVELS: &[ThinkingVariant] = &[
    ThinkingVariant {
        suffix: "low",
        label_effort: "Low",
        grouped_display_name: "Low",
        thinking_level: 1,
    },
    ThinkingVariant {
        suffix: "medium",
        label_effort: "Medium",
        grouped_display_name: "Medium",
        thinking_level: 2,
    },
    ThinkingVariant {
        suffix: "high",
        label_effort: "High",
        grouped_display_name: "High",
        thinking_level: 3,
    },
    ThinkingVariant {
        suffix: "xhigh",
        label_effort: "XHigh",
        grouped_display_name: "Extra",
        thinking_level: 7,
    },
];

// Canonical collision-free preferred offsets for all 31 curated variants:
// - Gemini (0..=16 -> M334..=M350, 17 slots): unchanged so persisted Gemini enum bindings never shift.
// - Codex  (17..=30 -> M351..=M364, 14 slots): fits alongside all 5 Gemini models (31/32 slots, M365 spare).
// Allocated in fixed canonical order independent of `default_model` so changing the default
// model never shifts persisted chat model enum bindings.
const CURATED_SLOT_TABLE: &[(&str, u32)] = &[
    // Gemini curated range (0..=16 -> M334..=M350)
    ("gemini-4-hydrogen-low", 0),
    ("gemini-4-hydrogen-medium", 1),
    ("gemini-4-hydrogen-high", 2),
    ("gemini-4-hydrogen-xhigh", 3),
    ("gemini-4-argon-low", 4),
    ("gemini-4-argon-medium", 5),
    ("gemini-4-argon-high", 6),
    ("gemini-4-neon-low", 7),
    ("gemini-4-neon-medium", 8),
    ("gemini-4-neon-high", 9),
    ("gemini-4-neon-xhigh", 10),
    ("gemini-4-carbon-low", 11),
    ("gemini-4-carbon-medium", 12),
    ("gemini-4-carbon-high", 13),
    ("gemini-3.8-flash-low", 14),
    ("gemini-3.8-flash-medium", 15),
    ("gemini-3.8-flash-high", 16),
    // OpenAI Codex curated range (17..=30 -> M351..=M364)
    ("gpt-6.2-astra-low", 17),
    ("gpt-6.2-astra-medium", 18),
    ("gpt-6.2-astra-high", 19),
    ("gpt-6.2-astra-xhigh", 20),
    ("gpt-6.2-sol-low", 21),
    ("gpt-6.2-sol-medium", 22),
    ("gpt-6.2-sol-high", 23),
    ("gpt-6.2-sol-xhigh", 24),
    ("gpt-6.1-terra-low", 25),
    ("gpt-6.1-terra-medium", 26),
    ("gpt-6.1-terra-high", 27),
    ("gpt-6-luna-low", 28),
    ("gpt-6-luna-medium", 29),
    ("gpt-6-luna-high", 30),
];

pub(super) fn variants_for_model(model_id: &str) -> &'static [ThinkingVariant] {
    match model_id {
        "gemini-4-hydrogen" | "gemini-4-neon" | "gpt-6.2-astra" | "gpt-6.2-sol" => {
            XHIGH_GEMINI_LEVELS
        }
        _ => STANDARD_GEMINI_LEVELS,
    }
}

pub(super) fn codex_model_sort_key(model_id: &str) -> (usize, &str) {
    let tier = CURATED_CODEX_MODELS
        .iter()
        .position(|m| m.id == model_id)
        .unwrap_or(CURATED_CODEX_MODELS.len());
    (tier, model_id)
}

pub(super) fn normalize_model_id(raw: &str) -> String {
    let trimmed = raw.trim();
    trimmed
        .strip_prefix("models/")
        .unwrap_or(trimmed)
        .trim()
        .to_string()
}

pub(super) fn collect_used_model_enums(models_obj: &Map<String, Value>) -> HashSet<String> {
    let mut used = HashSet::new();
    for details in models_obj.values() {
        if let Some(model_val) = details.get("model") {
            if let Some(s) = model_val.as_str() {
                used.insert(s.to_string());
            } else if let Some(n) = model_val.as_u64() {
                used.insert(n.to_string());
            }
        }
    }
    used
}

pub(super) fn available_placeholder_slots(used_enums: &HashSet<String>) -> usize {
    let used_in_range = (PLACEHOLDER_ENUM_START..=PLACEHOLDER_ENUM_END)
        .filter(|n| used_enums.contains(&format!("MODEL_PLACEHOLDER_M{n}")))
        .count();
    (PLACEHOLDER_ENUM_COUNT as usize).saturating_sub(used_in_range)
}

/// Selects Gemini and OpenAI Codex models that fit within `available_slots`.
///
/// Under slot pressure, each active provider's `default_model` (if present in its enabled list)
/// is reserved first, followed by remaining Gemini models in capability order and remaining
/// Codex models in curated order. Selected models are returned in deterministic picker order.
pub(super) fn select_multi_provider_models_for_available_slots(
    gemini_models: &[String],
    gemini_default_model: &str,
    codex_models: &[String],
    codex_default_model: &str,
    available_slots: usize,
) -> (Vec<String>, Vec<String>) {
    let mut seen_gemini: HashSet<String> = HashSet::new();
    let mut gemini_candidates: Vec<String> = Vec::new();
    for raw in gemini_models {
        let norm = normalize_model_id(raw);
        if !norm.is_empty() && is_supported_gemini_model(&norm) && seen_gemini.insert(norm.clone())
        {
            gemini_candidates.push(norm);
        }
    }

    let mut seen_codex: HashSet<String> = HashSet::new();
    let mut codex_candidates: Vec<String> = Vec::new();
    for raw in codex_models {
        let norm = normalize_model_id(raw);
        if !norm.is_empty()
            && !is_supported_gemini_model(&norm)
            && is_codex_model(&norm)
            && seen_codex.insert(norm.clone())
        {
            codex_candidates.push(norm);
        }
    }

    if gemini_candidates.is_empty() && codex_candidates.is_empty() {
        return (Vec::new(), Vec::new());
    }

    let gemini_default_norm = normalize_model_id(gemini_default_model);
    let valid_gemini_default = gemini_candidates
        .iter()
        .position(|m| m == &gemini_default_norm)
        .map(|pos| gemini_candidates.remove(pos));

    let codex_default_norm = normalize_model_id(codex_default_model);
    let valid_codex_default = codex_candidates
        .iter()
        .position(|m| m == &codex_default_norm)
        .map(|pos| codex_candidates.remove(pos));

    gemini_candidates
        .sort_by(|a, b| model_capability_sort_key(a).cmp(&model_capability_sort_key(b)));
    codex_candidates.sort_by(|a, b| codex_model_sort_key(a).cmp(&codex_model_sort_key(b)));

    let mut remaining_slots = available_slots;
    let mut selected_gemini: Vec<String> = Vec::new();
    let mut selected_codex: Vec<String> = Vec::new();

    if let Some(def) = valid_gemini_default {
        let needed = variants_for_model(&def).len();
        if remaining_slots >= needed {
            remaining_slots -= needed;
            selected_gemini.push(def);
        }
    }

    if let Some(def) = valid_codex_default {
        let needed = variants_for_model(&def).len();
        if remaining_slots >= needed {
            remaining_slots -= needed;
            selected_codex.push(def);
        }
    }

    for raw_id in gemini_candidates {
        if remaining_slots == 0 {
            break;
        }
        let needed = variants_for_model(&raw_id).len();
        if remaining_slots < needed {
            continue;
        }
        remaining_slots -= needed;
        selected_gemini.push(raw_id);
    }

    for raw_id in codex_candidates {
        if remaining_slots == 0 {
            break;
        }
        let needed = variants_for_model(&raw_id).len();
        if remaining_slots < needed {
            continue;
        }
        remaining_slots -= needed;
        selected_codex.push(raw_id);
    }

    selected_gemini.sort_by(|a, b| model_capability_sort_key(a).cmp(&model_capability_sort_key(b)));
    selected_codex.sort_by(|a, b| codex_model_sort_key(a).cmp(&codex_model_sort_key(b)));
    (selected_gemini, selected_codex)
}

fn fnv1a_64(bytes: &[u8]) -> u64 {
    const FNV_OFFSET_BASIS: u64 = 0xcbf29ce484222325;
    const FNV_PRIME: u64 = 0x100000001b3;
    let mut hash = FNV_OFFSET_BASIS;
    for &b in bytes {
        hash ^= u64::from(b);
        hash = hash.wrapping_mul(FNV_PRIME);
    }
    hash
}

fn claim_placeholder_slot(preferred_offset: u32, used: &mut HashSet<String>) -> Option<String> {
    let start = preferred_offset % PLACEHOLDER_ENUM_COUNT;
    for delta in 0..PLACEHOLDER_ENUM_COUNT {
        let offset = (start + delta) % PLACEHOLDER_ENUM_COUNT;
        let name = format!("MODEL_PLACEHOLDER_M{}", PLACEHOLDER_ENUM_START + offset);
        if used.insert(name.clone()) {
            return Some(name);
        }
    }
    None
}

pub(super) fn assign_deterministic_placeholder_enums(
    selected_models: &[String],
    used_enums: &HashSet<String>,
) -> HashMap<String, String> {
    let mut used = used_enums.clone();
    let mut needed: HashSet<String> = HashSet::new();
    for raw_id in selected_models {
        for variant in variants_for_model(raw_id) {
            needed.insert(format!("{raw_id}-{}", variant.suffix));
        }
    }

    let mut assigned = HashMap::with_capacity(needed.len());

    // Pass 1: Curated variants in fixed canonical order (independent of `default_model`).
    for &(variant_key, preferred_offset) in CURATED_SLOT_TABLE {
        if needed.remove(variant_key)
            && let Some(slot) = claim_placeholder_slot(preferred_offset, &mut used)
        {
            assigned.insert(variant_key.to_string(), slot);
        }
    }

    // Pass 2: Discovered non-curated models in canonical lexicographical order.
    let mut remaining_models: Vec<&String> = selected_models.iter().collect();
    remaining_models.sort();
    for raw_id in remaining_models {
        for variant in variants_for_model(raw_id) {
            let variant_key = format!("{raw_id}-{}", variant.suffix);
            if needed.remove(&variant_key) {
                let curated_count = CURATED_SLOT_TABLE.len() as u32;
                let dynamic_span = PLACEHOLDER_ENUM_COUNT - curated_count;
                let preferred = curated_count
                    + (fnv1a_64(variant_key.as_bytes()) % u64::from(dynamic_span)) as u32;
                if let Some(slot) = claim_placeholder_slot(preferred, &mut used) {
                    assigned.insert(variant_key, slot);
                }
            }
        }
    }

    assigned
}
