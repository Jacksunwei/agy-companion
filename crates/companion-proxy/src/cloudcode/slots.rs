use std::collections::{HashMap, HashSet};

use companion_core::{is_supported_gemini_model, model_capability_sort_key};
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

// Canonical collision-free preferred offsets (0..=16 -> M334..=M350) for the 17 curated variants.
// Allocated in fixed canonical order independent of `default_model` so changing the default
// model never shifts persisted chat model enum bindings.
const CURATED_SLOT_TABLE: &[(&str, u32)] = &[
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
];

pub(super) fn variants_for_model(model_id: &str) -> &'static [ThinkingVariant] {
    match model_id {
        "gemini-4-hydrogen" | "gemini-4-neon" => XHIGH_GEMINI_LEVELS,
        _ => STANDARD_GEMINI_LEVELS,
    }
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

/// Reserve placeholder slots for `default_model` first under slot pressure (only if
/// `default_model` is present in `proxy_models`), fill remaining slots in capability
/// order (skipping oversized 4-variant models so smaller 3-variant models can still fit),
/// then sort selected models by `model_capability_sort_key` for consistent picker display order.
pub(super) fn select_models_for_available_slots(
    proxy_models: &[String],
    default_model: &str,
    available_slots: usize,
) -> Vec<String> {
    let mut seen_models: HashSet<String> = HashSet::new();
    let mut candidates: Vec<String> = Vec::new();
    for raw in proxy_models {
        let norm = normalize_model_id(raw);
        if !norm.is_empty() && is_supported_gemini_model(&norm) && seen_models.insert(norm.clone())
        {
            candidates.push(norm);
        }
    }
    if candidates.is_empty() {
        return Vec::new();
    }

    let default_norm = normalize_model_id(default_model);
    let valid_default = candidates
        .iter()
        .position(|m| m == &default_norm)
        .map(|pos| candidates.remove(pos));

    candidates.sort_by(|a, b| model_capability_sort_key(a).cmp(&model_capability_sort_key(b)));

    let mut remaining_slots = available_slots;
    let mut selected_models: Vec<String> = Vec::new();

    if let Some(def) = valid_default {
        let needed = variants_for_model(&def).len();
        if remaining_slots >= needed {
            remaining_slots -= needed;
            selected_models.push(def);
        }
    }

    for raw_id in candidates {
        if remaining_slots == 0 {
            break;
        }
        let needed = variants_for_model(&raw_id).len();
        if remaining_slots < needed {
            continue;
        }
        remaining_slots -= needed;
        selected_models.push(raw_id);
    }

    selected_models.sort_by(|a, b| model_capability_sort_key(a).cmp(&model_capability_sort_key(b)));
    selected_models
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
