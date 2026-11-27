use companion_core::{find_curated_codex_model, is_supported_gemini_model};
use serde_json::{Map, Value, json};

use super::slots::{
    ThinkingVariant, assign_deterministic_placeholder_enums, available_placeholder_slots,
    collect_used_model_enums, select_multi_provider_models_for_available_slots, variants_for_model,
};

pub fn pretty_model_name(model_id: &str) -> String {
    model_id
        .split('-')
        .filter(|part| !part.is_empty())
        .map(|part| {
            let mut chars = part.chars();
            match chars.next() {
                Some(first) if first.is_ascii_alphabetic() => {
                    let mut word = String::with_capacity(part.len());
                    word.push(first.to_ascii_uppercase());
                    word.push_str(chars.as_str());
                    word
                }
                _ => part.to_string(),
            }
        })
        .collect::<Vec<_>>()
        .join(" ")
}

fn codex_supported_mime_types() -> Value {
    json!({
        "text/plain": true,
        "image/png": true,
        "image/jpeg": true,
        "image/webp": true
    })
}

fn pretty_codex_model_name(model_id: &str) -> String {
    if let Some(curated) = find_curated_codex_model(model_id) {
        return curated.display_name.to_string();
    }
    model_id
        .split('-')
        .filter(|part| !part.is_empty())
        .map(|part| {
            if part.eq_ignore_ascii_case("gpt") {
                "GPT".to_string()
            } else {
                let mut chars = part.chars();
                match chars.next() {
                    Some(first) if first.is_ascii_alphabetic() => {
                        let mut word = String::with_capacity(part.len());
                        word.push(first.to_ascii_uppercase());
                        word.push_str(chars.as_str());
                        word
                    }
                    _ => part.to_string(),
                }
            }
        })
        .collect::<Vec<_>>()
        .join(" ")
}

fn build_injected_codex_variant_details(
    raw_id: &str,
    variant: &ThinkingVariant,
    enum_name: &str,
    display_name: &str,
) -> Value {
    let curated = find_curated_codex_model(raw_id);
    let max_tokens = curated.map_or(272_000, |m| m.context_window);
    let max_output_tokens = curated.map_or(128_000, |m| m.max_output_tokens);

    // Explicit Codex metadata: NEVER copy Gemini upstream templates, `modelExperimentIds`,
    // or `supportsThoughtCirculation: true` (Codex does not use Gemini cryptographic
    // thought signatures).
    let mut details_map = Map::new();
    details_map.insert("model".to_string(), json!(enum_name));
    details_map.insert("displayName".to_string(), json!(display_name));
    details_map.insert(
        "modelUrl".to_string(),
        json!(format!("gemini-api://{raw_id}")),
    );
    details_map.insert("maxTokens".to_string(), json!(max_tokens));
    details_map.insert("maxOutputTokens".to_string(), json!(max_output_tokens));
    details_map.insert("supportsThinking".to_string(), json!(true));
    details_map.insert("thinkingLevel".to_string(), json!(variant.thinking_level));
    details_map.insert("supportsThoughtCirculation".to_string(), json!(false));
    details_map.insert("supportsImages".to_string(), json!(true));
    details_map.insert("supportsVideo".to_string(), json!(false));
    details_map.insert("supportsPdf".to_string(), json!(false));
    details_map.insert(
        "supportedMimeTypes".to_string(),
        codex_supported_mime_types(),
    );
    details_map.insert(
        "apiProvider".to_string(),
        json!("API_PROVIDER_GOOGLE_GEMINI"),
    );
    details_map.insert("modelProvider".to_string(), json!("MODEL_PROVIDER_GOOGLE"));
    details_map.insert(
        "toolFormatterType".to_string(),
        json!("TOOL_FORMATTER_TYPE_NONE"),
    );
    details_map.insert("quotaInfo".to_string(), json!({ "remainingFraction": 1.0 }));
    Value::Object(details_map)
}

fn gemini_supported_mime_types() -> Value {
    json!({
        "text/plain": true,
        "image/png": true,
        "image/jpeg": true,
        "image/webp": true,
        "image/heic": true,
        "image/heif": true,
        "application/pdf": true,
        "video/mp4": true
    })
}

fn find_upstream_model_template<'a>(
    models: &'a Map<String, Value>,
    raw_id: &str,
    suffix: &str,
) -> Option<&'a Map<String, Value>> {
    // 1. Exact effort variant match (e.g. "gemini-4-neon-high").
    let exact_variant = format!("{raw_id}-{suffix}");
    if let Some(obj) = models.get(&exact_variant).and_then(Value::as_object) {
        return Some(obj);
    }
    // 2. Exact base model match (e.g. "gemini-4-neon").
    if let Some(obj) = models.get(raw_id).and_then(Value::as_object) {
        return Some(obj);
    }
    // 3. Another effort variant of the same base model (prefer high -> medium -> low).
    for fallback_suffix in ["high", "medium", "low", "xhigh"] {
        let candidate = format!("{raw_id}-{fallback_suffix}");
        if let Some(obj) = models.get(&candidate).and_then(Value::as_object) {
            return Some(obj);
        }
    }
    let prefix = format!("{raw_id}-");
    let mut same_family: Vec<&String> = models
        .keys()
        .filter(|k| !k.starts_with("companion/") && k.starts_with(&prefix))
        .collect();
    same_family.sort();
    for key in same_family {
        if let Some(obj) = models.get(key).and_then(Value::as_object) {
            return Some(obj);
        }
    }
    // 4. Fallback to any upstream supported Gemini 3.8+ / 4+ / 5+ model carrying experiment/compaction
    // metadata so newly discovered Gemini models still inherit the native checkpointer config.
    let mut gemini_candidates: Vec<&String> = models
        .keys()
        .filter(|k| !k.starts_with("companion/") && is_supported_gemini_model(k))
        .collect();
    gemini_candidates.sort();
    for key in &gemini_candidates {
        if let Some(obj) = models.get(*key).and_then(Value::as_object)
            && (obj.contains_key("modelExperimentIds") || obj.contains_key("modelExperiments"))
        {
            return Some(obj);
        }
    }
    for key in gemini_candidates {
        if let Some(obj) = models.get(key).and_then(Value::as_object) {
            return Some(obj);
        }
    }
    None
}

fn build_injected_variant_details(
    upstream_models_snapshot: &Map<String, Value>,
    raw_id: &str,
    variant: &ThinkingVariant,
    enum_name: &str,
    display_name: &str,
) -> Value {
    // Inherit upstream model metadata (modelExperimentIds / modelExperiments /
    // ignoreExperimentNames / tokenizerType / supportsCumulativeContext /
    // supportsThoughtCirculation / maxTokens / maxOutputTokens) so Antigravity
    // applies the same compaction window and capabilities as the native model.
    let mut details_map =
        find_upstream_model_template(upstream_models_snapshot, raw_id, variant.suffix)
            .cloned()
            .unwrap_or_default();

    for strip_key in [
        "vertexModelId",
        "tagTitle",
        "tagDescription",
        "disabled",
        "recommended",
        "deprecated",
        "isInternal",
    ] {
        details_map.remove(strip_key);
    }

    let has_valid_max_tokens = details_map
        .get("maxTokens")
        .and_then(Value::as_u64)
        .is_some_and(|v| v > 0);
    if !has_valid_max_tokens {
        details_map.insert("maxTokens".to_string(), json!(1048576));
    }
    let has_valid_max_output = details_map
        .get("maxOutputTokens")
        .and_then(Value::as_u64)
        .is_some_and(|v| v > 0);
    if !has_valid_max_output {
        details_map.insert("maxOutputTokens".to_string(), json!(65536));
    }
    if !details_map.contains_key("supportedMimeTypes") {
        details_map.insert(
            "supportedMimeTypes".to_string(),
            gemini_supported_mime_types(),
        );
    }

    details_map.insert("model".to_string(), json!(enum_name));
    details_map.insert("displayName".to_string(), json!(display_name));
    details_map.insert(
        "modelUrl".to_string(),
        json!(format!("gemini-api://{raw_id}")),
    );
    details_map.insert("supportsThinking".to_string(), json!(true));
    details_map.insert("thinkingLevel".to_string(), json!(variant.thinking_level));
    details_map.insert("supportsImages".to_string(), json!(true));
    details_map.insert("supportsVideo".to_string(), json!(true));
    details_map.insert("supportsPdf".to_string(), json!(true));
    details_map.insert(
        "apiProvider".to_string(),
        json!("API_PROVIDER_GOOGLE_GEMINI"),
    );
    details_map.insert("modelProvider".to_string(), json!("MODEL_PROVIDER_GOOGLE"));
    details_map.insert(
        "toolFormatterType".to_string(),
        json!("TOOL_FORMATTER_TYPE_NONE"),
    );
    details_map.insert("quotaInfo".to_string(), json!({ "remainingFraction": 1.0 }));
    Value::Object(details_map)
}

fn append_injected_model_ids_to_sorts(
    root_obj: &mut Map<String, Value>,
    injected_model_ids: Vec<String>,
) {
    let sorts = root_obj
        .entry("agentModelSorts".to_string())
        .or_insert_with(|| Value::Array(Vec::new()));
    if !sorts.is_array() {
        *sorts = Value::Array(Vec::new());
    }
    if let Some(sorts_arr) = sorts.as_array_mut() {
        if sorts_arr.is_empty() {
            sorts_arr.push(json!({
                "displayName": "Recommended",
                "groups": [{
                    "displayName": "Models",
                    "modelIds": injected_model_ids
                }]
            }));
        } else {
            // Antigravity UI reads Recommended sort's groups[0] exclusively.
            let target_idx = sorts_arr
                .iter()
                .position(|s| {
                    s.get("displayName")
                        .and_then(Value::as_str)
                        .is_some_and(|name| name.eq_ignore_ascii_case("recommended"))
                })
                .unwrap_or(0);

            if let Some(target_sort) = sorts_arr[target_idx].as_object_mut() {
                let groups = target_sort
                    .entry("groups".to_string())
                    .or_insert_with(|| Value::Array(Vec::new()));
                if !groups.is_array() {
                    *groups = Value::Array(Vec::new());
                }
                if let Some(groups_arr) = groups.as_array_mut() {
                    if groups_arr.is_empty() {
                        groups_arr.push(json!({
                            "displayName": "Models",
                            "modelIds": injected_model_ids
                        }));
                    } else if let Some(first_group) = groups_arr[0].as_object_mut() {
                        let model_ids = first_group
                            .entry("modelIds".to_string())
                            .or_insert_with(|| Value::Array(Vec::new()));
                        if !model_ids.is_array() {
                            *model_ids = Value::Array(Vec::new());
                        }
                        if let Some(ids_arr) = model_ids.as_array_mut() {
                            for id in injected_model_ids {
                                ids_arr.push(Value::String(id));
                            }
                        }
                    }
                }
            }
        }
    }
}

/// Merge proxy-backed Gemini 3/4 models into an upstream Cloud Code
/// `FetchAvailableModelsResponse` JSON payload while preserving every original
/// Google AI Pro / Workspace model, sort order, and `groupedModels` entry intact.
///
/// Key Antigravity App UI contracts satisfied here:
/// 1. `useModelGroups` renders only `Recommended` sort's `groups[0]`, so injected
///    proxy model IDs are appended directly to the end of `groups[0].modelIds`
///    immediately after all original models.
/// 2. `parseLabelSlug` collapses variants into the native secondary "Thinking effort"
///    flyout when `displayName` ends with `(Low|Medium|High|XHigh|Max)`. Using
///    `"<Model> (Proxy) (<Effort>)"` gives base label `"<Model> (Proxy)"` with a
///    working Low / Medium / High / Extra submenu.
/// 3. Each injected variant sets `modelUrl: "gemini-api://<model>"` so Antigravity's
///    `RoutingClient` routes only `(Proxy)` models to `GOOGLE_GEMINI_BASE_URL`
///    (`http://127.0.0.1:<port>`) while original Google models route untouched.
pub fn merge_available_models(
    upstream_body: &[u8],
    proxy_models: &[String],
    default_model: &str,
) -> Result<Vec<u8>, serde_json::Error> {
    merge_available_models_multi_provider(upstream_body, proxy_models, default_model, &[], "")
}

/// Merge proxy-backed Gemini and/or OpenAI Codex models into an upstream Cloud Code
/// `FetchAvailableModelsResponse` JSON payload with collision-free placeholder slots
/// (`M334..=M350` Gemini, `M351..=M364` Codex).
pub fn merge_available_models_multi_provider(
    upstream_body: &[u8],
    gemini_models: &[String],
    gemini_default_model: &str,
    codex_models: &[String],
    codex_default_model: &str,
) -> Result<Vec<u8>, serde_json::Error> {
    let mut root: Value = serde_json::from_slice(upstream_body)?;
    let Some(root_obj) = root.as_object_mut() else {
        return serde_json::to_vec(&root);
    };

    if !root_obj.get("models").is_some_and(Value::is_object) {
        root_obj.insert("models".to_string(), Value::Object(Map::new()));
    }
    // Only populate `groupedModels` when upstream already returned a non-empty map.
    // Creating a partial `groupedModels` when upstream omitted it disables Antigravity's
    // legacy label parser and flattens all original models that are not in `groupedModels`.
    let upstream_has_grouped_models = root_obj
        .get("groupedModels")
        .and_then(Value::as_object)
        .is_some_and(|m| !m.is_empty());

    let used_enums = root_obj
        .get("models")
        .and_then(Value::as_object)
        .map(collect_used_model_enums)
        .unwrap_or_default();
    let available_slots = available_placeholder_slots(&used_enums);
    let (selected_gemini, selected_codex) = select_multi_provider_models_for_available_slots(
        gemini_models,
        gemini_default_model,
        codex_models,
        codex_default_model,
        available_slots,
    );

    if selected_gemini.is_empty() && selected_codex.is_empty() {
        return serde_json::to_vec(&root);
    }

    let mut all_selected = Vec::with_capacity(selected_gemini.len() + selected_codex.len());
    all_selected.extend(selected_gemini.iter().cloned());
    all_selected.extend(selected_codex.iter().cloned());
    let slot_map = assign_deterministic_placeholder_enums(&all_selected, &used_enums);

    let mut injected_model_ids: Vec<String> = Vec::new();
    let mut injected_details: Vec<(String, Value)> = Vec::new();
    let mut injected_groups: Vec<(String, Value)> = Vec::new();

    let upstream_models_snapshot = root_obj
        .get("models")
        .and_then(Value::as_object)
        .cloned()
        .unwrap_or_default();

    for raw_id in &selected_gemini {
        let variants = variants_for_model(raw_id);
        let base_id = format!("companion/{raw_id}");
        let display_base = format!("{} (Proxy)", pretty_model_name(raw_id));
        let mut level_entries = Vec::with_capacity(variants.len());

        for variant in variants {
            let variant_key = format!("{raw_id}-{}", variant.suffix);
            let Some(enum_name) = slot_map.get(&variant_key) else {
                break;
            };

            let variant_id = format!("{base_id}-{}", variant.suffix);
            let display_name = format!("{display_base} ({})", variant.label_effort);
            let details = build_injected_variant_details(
                &upstream_models_snapshot,
                raw_id,
                variant,
                enum_name,
                &display_name,
            );

            injected_model_ids.push(variant_id.clone());
            injected_details.push((variant_id, details));
            level_entries.push(json!({
                "thinkingSuffix": variant.suffix,
                "thinkingDisplayName": variant.grouped_display_name
            }));
        }

        if !level_entries.is_empty() {
            injected_groups.push((
                base_id,
                json!({
                    "displayName": display_base,
                    "supportedThinkingLevels": level_entries
                }),
            ));
        }
    }

    for raw_id in &selected_codex {
        let variants = variants_for_model(raw_id);
        let base_id = format!("companion/{raw_id}");
        let display_base = format!("{} (Codex)", pretty_codex_model_name(raw_id));
        let mut level_entries = Vec::with_capacity(variants.len());

        for variant in variants {
            let variant_key = format!("{raw_id}-{}", variant.suffix);
            let Some(enum_name) = slot_map.get(&variant_key) else {
                break;
            };

            let variant_id = format!("{base_id}-{}", variant.suffix);
            let display_name = format!("{display_base} ({})", variant.label_effort);
            let details =
                build_injected_codex_variant_details(raw_id, variant, enum_name, &display_name);

            injected_model_ids.push(variant_id.clone());
            injected_details.push((variant_id, details));
            level_entries.push(json!({
                "thinkingSuffix": variant.suffix,
                "thinkingDisplayName": variant.grouped_display_name
            }));
        }

        if !level_entries.is_empty() {
            injected_groups.push((
                base_id,
                json!({
                    "displayName": display_base,
                    "supportedThinkingLevels": level_entries
                }),
            ));
        }
    }

    if injected_model_ids.is_empty() {
        return serde_json::to_vec(&root);
    }

    if let Some(models_obj) = root_obj.get_mut("models").and_then(Value::as_object_mut) {
        for (id, details) in injected_details {
            models_obj.insert(id, details);
        }
    }

    if upstream_has_grouped_models
        && let Some(grouped_obj) = root_obj
            .get_mut("groupedModels")
            .and_then(Value::as_object_mut)
    {
        for (base_id, group_val) in injected_groups {
            grouped_obj.insert(base_id, group_val);
        }
    }

    append_injected_model_ids_to_sorts(root_obj, injected_model_ids);

    serde_json::to_vec(&root)
}
