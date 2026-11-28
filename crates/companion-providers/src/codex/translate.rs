use std::collections::{HashMap, HashSet, VecDeque};

use base64::Engine;
use base64::engine::general_purpose::{STANDARD, STANDARD_NO_PAD, URL_SAFE, URL_SAFE_NO_PAD};
use bytes::Bytes;
use ring::digest::{Context as DigestContext, SHA256};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};

use crate::ProviderError;

pub const DEFAULT_CODEX_INSTRUCTIONS: &str = "You are a helpful coding assistant.";
pub const MAX_CODEX_CALL_ID_LEN: usize = 64;
pub const MAX_CODEX_CACHE_KEY_LEN: usize = 64;
/// Namespaced prefix identifying encrypted OpenAI Codex reasoning items serialized inside a
/// Gemini `thoughtSignature` for stateless (`store: false`) multi-turn circulation.
pub const CODEX_THOUGHT_SIGNATURE_PREFIX: &str = "agyc:codex:v1:";
/// Maximum encoded byte length of a Codex `thoughtSignature` (OOM guard).
pub const MAX_CODEX_REASONING_SIGNATURE_BYTES: usize = 512 * 1024;
const MAX_CODEX_REASONING_ID_LEN: usize = 256;
const MAX_CODEX_REASONING_SUMMARY_ITEMS: usize = 128;

/// Maximum per-line / per-event byte buffer in [`CodexSseTranslator`] (S4/S7 OOM guard).
pub const DEFAULT_MAX_CODEX_SSE_EVENT_BYTES: usize = 2 * 1024 * 1024;
/// Maximum buffered JSON arguments size for a single streamed `function_call` item.
pub const DEFAULT_MAX_TOOL_CALL_ARGS_BYTES: usize = 2 * 1024 * 1024;
/// Maximum aggregate buffered JSON arguments across all concurrently open tool calls.
pub const DEFAULT_MAX_TOTAL_TOOL_CALL_ARGS_BYTES: usize = 4 * 1024 * 1024;
/// Maximum simultaneously open tool calls in a single Codex response stream.
pub const MAX_CONCURRENT_TOOL_CALLS: usize = 128;
/// Maximum simultaneously tracked in-flight reasoning items for summary deduplication.
const MAX_TRACKED_REASONING_ITEMS: usize = 128;
/// Maximum aggregated payload budget for unary `:generateContent` translation.
pub const DEFAULT_MAX_UNARY_RESPONSE_BYTES: usize = 16 * 1024 * 1024;

const RETAIN_BUFFER_CAPACITY_LIMIT: usize = 64 * 1024;
const MAX_SCHEMA_NORMALIZE_DEPTH: usize = 64;
const UTF8_BOM: [u8; 3] = [0xEF, 0xBB, 0xBF];

const ALLOWED_STRING_FORMATS: &[&str] = &[
    "date-time",
    "date",
    "time",
    "email",
    "uri",
    "uuid",
    "ipv4",
    "ipv6",
    "hostname",
];

/// Translates an inbound Gemini GenerateContent JSON payload directly into an OpenAI Codex
/// Responses API JSON payload (`store: false`, `stream: true`) with a deterministic fallback
/// `prompt_cache_key`.
pub fn translate_gemini_to_codex_request(
    model: &str,
    gemini_body: &[u8],
) -> Result<Value, ProviderError> {
    translate_gemini_to_codex_request_with_cache_key(model, gemini_body, None)
}

/// Translates an inbound Gemini GenerateContent JSON payload directly into an OpenAI Codex
/// Responses API JSON payload (`store: false`, `stream: true`) with prompt-cache optimization:
/// - Keeps top-level `instructions` as the short constant [`DEFAULT_CODEX_INSTRUCTIONS`] and
///   prepends non-empty Gemini `systemInstruction` as `input[0]` (`role: "developer"`), because
///   `chatgpt.com/backend-api/codex/responses` places implicit prompt-cache breakpoints after
///   initial `developer` messages in `input[]` and does not cache top-level `instructions`.
/// - Sets `prompt_cache_key` from the validated `explicit_cache_key` (Antigravity conversation ID),
///   falling back to a deterministic SHA-256 prefix hash (`agyc-<32 hex>`) over
///   `(model, developer_instructions, first_content_item)` so multi-turn callers without a
///   conversation ID still route to the same KV-cache shard.
pub fn translate_gemini_to_codex_request_with_cache_key(
    model: &str,
    gemini_body: &[u8],
    explicit_cache_key: Option<&str>,
) -> Result<Value, ProviderError> {
    let parsed: Value = serde_json::from_slice(gemini_body).map_err(|_| {
        ProviderError::InvalidRequest("request body must be valid JSON".to_string())
    })?;
    let root = parsed.as_object().ok_or_else(|| {
        ProviderError::InvalidRequest("request body must be a JSON object".to_string())
    })?;

    let developer_instructions = extract_developer_instructions(root);
    let content_items = translate_contents(root)?;
    let prompt_cache_key = explicit_cache_key
        .and_then(sanitize_codex_cache_key)
        .unwrap_or_else(|| {
            derive_fallback_cache_key(
                model,
                developer_instructions.as_deref(),
                content_items.first(),
            )
        });

    let mut input =
        Vec::with_capacity(content_items.len() + usize::from(developer_instructions.is_some()));
    if let Some(dev_text) = developer_instructions {
        input.push(json!({
            "type": "message",
            "role": "developer",
            "content": [{ "type": "input_text", "text": dev_text }]
        }));
    }
    input.extend(content_items);

    let mut out = Map::new();
    out.insert("model".to_string(), Value::String(model.to_string()));
    out.insert(
        "instructions".to_string(),
        Value::String(DEFAULT_CODEX_INSTRUCTIONS.to_string()),
    );
    out.insert(
        "prompt_cache_key".to_string(),
        Value::String(prompt_cache_key),
    );
    out.insert("input".to_string(), Value::Array(input));
    out.insert("store".to_string(), Value::Bool(false));
    out.insert("stream".to_string(), Value::Bool(true));

    let (tool_choice, allowed_function_names) = extract_tool_config(root);
    let tools = translate_tools(root, allowed_function_names.as_ref())?;
    if !tools.is_empty() {
        out.insert("tools".to_string(), Value::Array(tools));
    }
    if let Some(choice) = tool_choice {
        out.insert("tool_choice".to_string(), Value::String(choice.to_string()));
    }

    if let Some(reasoning_cfg) = extract_reasoning_config(root) {
        let mut reasoning_obj = Map::new();
        if let Some(effort) = reasoning_cfg.effort {
            reasoning_obj.insert("effort".to_string(), Value::String(effort.to_string()));
        }
        if reasoning_cfg.include_summary {
            reasoning_obj.insert("summary".to_string(), Value::String("auto".to_string()));
        }
        if !reasoning_obj.is_empty() {
            out.insert("reasoning".to_string(), Value::Object(reasoning_obj));
        }
        out.insert(
            "include".to_string(),
            json!(["reasoning.encrypted_content"]),
        );
    }

    Ok(Value::Object(out))
}

fn sanitize_codex_cache_key(raw: &str) -> Option<String> {
    let trimmed = raw.trim();
    if trimmed.is_empty() || trimmed.len() > MAX_CODEX_CACHE_KEY_LEN {
        return None;
    }
    if !trimmed
        .bytes()
        .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
    {
        return None;
    }
    Some(trimmed.to_string())
}

fn derive_fallback_cache_key(
    model: &str,
    developer_instructions: Option<&str>,
    first_content: Option<&Value>,
) -> String {
    let mut ctx = DigestContext::new(&SHA256);
    ctx.update(model.as_bytes());
    ctx.update(&[0]);
    if let Some(dev) = developer_instructions {
        ctx.update(dev.as_bytes());
    }
    ctx.update(&[0]);
    if let Some(first) = first_content
        && let Ok(bytes) = serde_json::to_vec(first)
    {
        ctx.update(&bytes);
    }
    let digest = ctx.finish();
    let mut out = String::with_capacity(5 + 32);
    out.push_str("agyc-");
    for byte in &digest.as_ref()[..16] {
        use std::fmt::Write as _;
        let _ = write!(&mut out, "{byte:02x}");
    }
    out
}

fn extract_developer_instructions(root: &Map<String, Value>) -> Option<String> {
    let sys = root
        .get("systemInstruction")
        .or_else(|| root.get("system_instruction"))?;

    let joined = match sys {
        Value::String(text) => text.clone(),
        Value::Object(obj) => {
            let mut segments = Vec::new();
            if let Some(parts) = obj.get("parts").and_then(Value::as_array) {
                for part in parts {
                    if part.get("thought").and_then(Value::as_bool) == Some(true) {
                        continue;
                    }
                    if let Some(text) = part.get("text").and_then(Value::as_str)
                        && !text.is_empty()
                    {
                        segments.push(text);
                    }
                }
            }
            segments.join("\n\n")
        }
        _ => String::new(),
    };

    if joined.trim().is_empty() {
        None
    } else {
        Some(joined)
    }
}

struct CallIdCorrelator {
    pending_by_name: HashMap<String, VecDeque<String>>,
    explicit_id_map: HashMap<String, String>,
    used_ids: HashSet<String>,
    fallback_seq: u64,
}

impl CallIdCorrelator {
    fn new() -> Self {
        Self {
            pending_by_name: HashMap::new(),
            explicit_id_map: HashMap::new(),
            used_ids: HashSet::new(),
            fallback_seq: 0,
        }
    }

    fn normalize_explicit_id(&mut self, raw: &str) -> String {
        if let Some(mapped) = self.explicit_id_map.get(raw) {
            return mapped.clone();
        }
        let candidate = if !raw.is_empty()
            && raw.len() <= MAX_CODEX_CALL_ID_LEN
            && raw.bytes().all(|b| (0x21..=0x7E).contains(&b))
        {
            raw.to_string()
        } else {
            format!("call_h{:016x}", fnv1a_64(raw.as_bytes()))
        };
        self.explicit_id_map
            .insert(raw.to_string(), candidate.clone());
        candidate
    }

    fn allocate_synthetic_id(&mut self, prefix: &str, turn_idx: usize, part_idx: usize) -> String {
        let base = format!("{prefix}_{turn_idx}_{part_idx}");
        if base.len() <= MAX_CODEX_CALL_ID_LEN && self.used_ids.insert(base.clone()) {
            return base;
        }
        loop {
            self.fallback_seq = self.fallback_seq.wrapping_add(1);
            let candidate = format!("{prefix}_{}_{}", self.fallback_seq, part_idx);
            let bounded = if candidate.len() <= MAX_CODEX_CALL_ID_LEN {
                candidate
            } else {
                format!("call_s{:016x}", fnv1a_64(candidate.as_bytes()))
            };
            if self.used_ids.insert(bounded.clone()) {
                return bounded;
            }
        }
    }

    fn register_call(
        &mut self,
        explicit_id: Option<&str>,
        name: &str,
        turn_idx: usize,
        part_idx: usize,
    ) -> String {
        let call_id = match explicit_id.map(str::trim).filter(|s| !s.is_empty()) {
            Some(raw) => {
                let id = self.normalize_explicit_id(raw);
                self.used_ids.insert(id.clone());
                id
            }
            None => self.allocate_synthetic_id("call", turn_idx, part_idx),
        };
        self.pending_by_name
            .entry(name.to_string())
            .or_default()
            .push_back(call_id.clone());
        call_id
    }

    fn resolve_response(
        &mut self,
        explicit_id: Option<&str>,
        name: &str,
        turn_idx: usize,
        part_idx: usize,
    ) -> String {
        if let Some(raw) = explicit_id.map(str::trim).filter(|s| !s.is_empty()) {
            let id = self.normalize_explicit_id(raw);
            if let Some(queue) = self.pending_by_name.get_mut(name)
                && let Some(pos) = queue.iter().position(|queued| queued == &id)
            {
                queue.remove(pos);
            }
            return id;
        }
        if let Some(queue) = self.pending_by_name.get_mut(name)
            && let Some(call_id) = queue.pop_front()
        {
            return call_id;
        }
        self.allocate_synthetic_id("call_unmatched", turn_idx, part_idx)
    }
}

fn translate_contents(root: &Map<String, Value>) -> Result<Vec<Value>, ProviderError> {
    let Some(contents_val) = root.get("contents") else {
        return Ok(Vec::new());
    };
    let contents = contents_val.as_array().ok_or_else(|| {
        ProviderError::InvalidRequest("'contents' must be a JSON array".to_string())
    })?;

    let mut input = Vec::new();
    let mut correlator = CallIdCorrelator::new();
    let mut seen_reasoning_ids = HashSet::new();

    for (turn_idx, content) in contents.iter().enumerate() {
        let Some(content_obj) = content.as_object() else {
            return Err(ProviderError::InvalidRequest(
                "each entry in 'contents' must be a JSON object".to_string(),
            ));
        };

        let role = content_obj
            .get("role")
            .and_then(Value::as_str)
            .unwrap_or("user");
        let parts = match content_obj.get("parts") {
            Some(Value::Array(arr)) => arr.as_slice(),
            Some(_) => {
                return Err(ProviderError::InvalidRequest(
                    "'parts' in a content turn must be a JSON array".to_string(),
                ));
            }
            None => &[],
        };

        match role {
            "model" | "assistant" => {
                translate_model_parts(
                    parts,
                    turn_idx,
                    &mut correlator,
                    &mut seen_reasoning_ids,
                    &mut input,
                )?;
            }
            "system" | "developer" => {
                translate_user_like_parts(
                    "developer",
                    parts,
                    turn_idx,
                    &mut correlator,
                    &mut input,
                )?;
            }
            _ => {
                // "user", "function", "tool", or omitted role
                translate_user_like_parts("user", parts, turn_idx, &mut correlator, &mut input)?;
            }
        }
    }

    Ok(input)
}

fn translate_user_like_parts(
    out_role: &str,
    parts: &[Value],
    turn_idx: usize,
    correlator: &mut CallIdCorrelator,
    input: &mut Vec<Value>,
) -> Result<(), ProviderError> {
    let mut message_parts: Vec<Value> = Vec::new();

    for (part_idx, part) in parts.iter().enumerate() {
        let Some(part_obj) = part.as_object() else {
            return Err(ProviderError::InvalidRequest(
                "each part in 'contents' must be a JSON object".to_string(),
            ));
        };

        if part_obj.get("thought").and_then(Value::as_bool) == Some(true) {
            continue;
        }

        if part_obj.contains_key("fileData") || part_obj.contains_key("file_data") {
            return Err(ProviderError::InvalidRequest(
                "fileData parts are not supported by OpenAI Codex".to_string(),
            ));
        }

        if let Some(inline_data) = part_obj
            .get("inlineData")
            .or_else(|| part_obj.get("inline_data"))
        {
            let image_item = translate_inline_data_part(inline_data)?;
            message_parts.push(image_item);
        }

        if let Some(text) = part_obj.get("text").and_then(Value::as_str) {
            message_parts.push(json!({
                "type": "input_text",
                "text": text
            }));
        }

        if let Some(func_resp) = part_obj
            .get("functionResponse")
            .or_else(|| part_obj.get("function_response"))
        {
            flush_message(out_role, &mut message_parts, input);
            let output_item =
                translate_function_response_part(func_resp, turn_idx, part_idx, correlator)?;
            input.push(output_item);
        }
    }

    flush_message(out_role, &mut message_parts, input);
    Ok(())
}

fn translate_model_parts(
    parts: &[Value],
    turn_idx: usize,
    correlator: &mut CallIdCorrelator,
    seen_reasoning_ids: &mut HashSet<String>,
    input: &mut Vec<Value>,
) -> Result<(), ProviderError> {
    let mut assistant_parts: Vec<Value> = Vec::new();

    for (part_idx, part) in parts.iter().enumerate() {
        let Some(part_obj) = part.as_object() else {
            return Err(ProviderError::InvalidRequest(
                "each part in 'contents' must be a JSON object".to_string(),
            ));
        };

        // Replay stateless encrypted Codex reasoning items carried in our namespaced
        // `agyc:codex:v1:` `thoughtSignature` (whether on a `thought: true` part or a sibling
        // part). Foreign Gemini signatures and malformed payloads are safely ignored.
        if let Some(sig) = part_obj
            .get("thoughtSignature")
            .or_else(|| part_obj.get("thought_signature"))
            .and_then(Value::as_str)
            && let Some(payload) = decode_codex_reasoning_signature(sig)
        {
            let dedup_key = payload.id.clone().unwrap_or_else(|| {
                format!("ec:{:016x}", fnv1a_64(payload.encrypted_content.as_bytes()))
            });
            if seen_reasoning_ids.insert(dedup_key) {
                flush_message("assistant", &mut assistant_parts, input);
                let mut item = Map::new();
                item.insert("type".to_string(), Value::String("reasoning".to_string()));
                if let Some(id) = payload.id {
                    item.insert("id".to_string(), Value::String(id));
                }
                item.insert("summary".to_string(), Value::Array(payload.summary));
                item.insert(
                    "encrypted_content".to_string(),
                    Value::String(payload.encrypted_content),
                );
                input.push(Value::Object(item));
            }
        }

        // Skip visible thought text blocks (Codex consumes encrypted reasoning items above,
        // never plain assistant text for chain-of-thought summaries).
        if part_obj.get("thought").and_then(Value::as_bool) == Some(true) {
            continue;
        }

        if part_obj.contains_key("fileData") || part_obj.contains_key("file_data") {
            return Err(ProviderError::InvalidRequest(
                "fileData parts are not supported by OpenAI Codex".to_string(),
            ));
        }

        if let Some(inline_data) = part_obj
            .get("inlineData")
            .or_else(|| part_obj.get("inline_data"))
        {
            // Validate MIME fail-closed rather than silently dropping unsupported binary payloads.
            let _ = translate_inline_data_part(inline_data)?;
        }

        if let Some(text) = part_obj.get("text").and_then(Value::as_str) {
            assistant_parts.push(json!({
                "type": "output_text",
                "text": text
            }));
        }

        if let Some(func_call) = part_obj
            .get("functionCall")
            .or_else(|| part_obj.get("function_call"))
        {
            flush_message("assistant", &mut assistant_parts, input);
            let call_item =
                translate_function_call_part(func_call, turn_idx, part_idx, correlator)?;
            input.push(call_item);
        }

        if let Some(func_resp) = part_obj
            .get("functionResponse")
            .or_else(|| part_obj.get("function_response"))
        {
            flush_message("assistant", &mut assistant_parts, input);
            let output_item =
                translate_function_response_part(func_resp, turn_idx, part_idx, correlator)?;
            input.push(output_item);
        }
    }

    flush_message("assistant", &mut assistant_parts, input);
    Ok(())
}

fn flush_message(role: &str, parts: &mut Vec<Value>, input: &mut Vec<Value>) {
    if parts.is_empty() {
        return;
    }
    input.push(json!({
        "type": "message",
        "role": role,
        "content": std::mem::take(parts)
    }));
}

fn translate_inline_data_part(inline_data: &Value) -> Result<Value, ProviderError> {
    let obj = inline_data.as_object().ok_or_else(|| {
        ProviderError::InvalidRequest("inlineData part must be a JSON object".to_string())
    })?;
    let raw_mime = obj
        .get("mimeType")
        .or_else(|| obj.get("mime_type"))
        .and_then(Value::as_str)
        .unwrap_or("")
        .trim();
    let lower_mime = raw_mime.to_ascii_lowercase();

    match lower_mime.as_str() {
        "image/png" | "image/jpeg" | "image/webp" | "image/gif" => {}
        _ => {
            let safe_mime: String = raw_mime
                .chars()
                .filter(|c| c.is_ascii_graphic())
                .take(64)
                .collect();
            let detail = if safe_mime.is_empty() {
                "<missing>".to_string()
            } else {
                safe_mime
            };
            return Err(ProviderError::InvalidRequest(format!(
                "unsupported inlineData MIME type for OpenAI Codex: {detail}"
            )));
        }
    }

    let data = obj
        .get("data")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .ok_or_else(|| {
            ProviderError::InvalidRequest("inlineData image is missing base64 data".to_string())
        })?;

    Ok(json!({
        "type": "input_image",
        "image_url": format!("data:{lower_mime};base64,{data}")
    }))
}

fn translate_function_call_part(
    func_call: &Value,
    turn_idx: usize,
    part_idx: usize,
    correlator: &mut CallIdCorrelator,
) -> Result<Value, ProviderError> {
    let obj = func_call.as_object().ok_or_else(|| {
        ProviderError::InvalidRequest("functionCall part must be a JSON object".to_string())
    })?;
    let name = obj
        .get("name")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .ok_or_else(|| {
            ProviderError::InvalidRequest("functionCall is missing a function name".to_string())
        })?;
    let explicit_id = obj.get("id").and_then(Value::as_str);
    let call_id = correlator.register_call(explicit_id, name, turn_idx, part_idx);

    let arguments = match obj.get("args") {
        None | Some(Value::Null) => "{}".to_string(),
        Some(val) => serde_json::to_string(val).map_err(|_| {
            ProviderError::InvalidRequest("functionCall args could not be serialized".to_string())
        })?,
    };

    Ok(json!({
        "type": "function_call",
        "call_id": call_id,
        "name": name,
        "arguments": arguments
    }))
}

fn translate_function_response_part(
    func_resp: &Value,
    turn_idx: usize,
    part_idx: usize,
    correlator: &mut CallIdCorrelator,
) -> Result<Value, ProviderError> {
    let obj = func_resp.as_object().ok_or_else(|| {
        ProviderError::InvalidRequest("functionResponse part must be a JSON object".to_string())
    })?;
    let name = obj
        .get("name")
        .and_then(Value::as_str)
        .map(str::trim)
        .unwrap_or("");
    let explicit_id = obj.get("id").and_then(Value::as_str);
    let call_id = correlator.resolve_response(explicit_id, name, turn_idx, part_idx);

    let output = match obj.get("response") {
        None | Some(Value::Null) => "{}".to_string(),
        Some(Value::String(s)) => s.clone(),
        Some(val) => serde_json::to_string(val).map_err(|_| {
            ProviderError::InvalidRequest(
                "functionResponse payload could not be serialized".to_string(),
            )
        })?,
    };

    Ok(json!({
        "type": "function_call_output",
        "call_id": call_id,
        "output": output
    }))
}

fn translate_tools(
    root: &Map<String, Value>,
    allowed_names: Option<&HashSet<String>>,
) -> Result<Vec<Value>, ProviderError> {
    let Some(tools_val) = root.get("tools") else {
        return Ok(Vec::new());
    };
    let tools_arr = tools_val
        .as_array()
        .ok_or_else(|| ProviderError::InvalidRequest("'tools' must be a JSON array".to_string()))?;

    let mut out = Vec::new();
    for tool in tools_arr {
        let Some(tool_obj) = tool.as_object() else {
            continue;
        };
        let Some(decls) = tool_obj
            .get("functionDeclarations")
            .or_else(|| tool_obj.get("function_declarations"))
            .and_then(Value::as_array)
        else {
            continue;
        };

        for decl in decls {
            let Some(decl_obj) = decl.as_object() else {
                continue;
            };
            let Some(name) = decl_obj
                .get("name")
                .and_then(Value::as_str)
                .map(str::trim)
                .filter(|s| !s.is_empty())
            else {
                return Err(ProviderError::InvalidRequest(
                    "functionDeclaration is missing a non-empty name".to_string(),
                ));
            };
            if let Some(allowed) = allowed_names
                && !allowed.contains(name)
            {
                continue;
            }

            let mut params = decl_obj
                .get("parameters")
                .or_else(|| decl_obj.get("parametersJsonSchema"))
                .or_else(|| decl_obj.get("parameters_json_schema"))
                .cloned()
                .unwrap_or_else(|| json!({ "type": "object", "properties": {} }));
            normalize_gemini_schema(&mut params);

            let mut item = Map::new();
            item.insert("type".to_string(), Value::String("function".to_string()));
            item.insert("name".to_string(), Value::String(name.to_string()));
            if let Some(desc) = decl_obj.get("description").and_then(Value::as_str) {
                item.insert("description".to_string(), Value::String(desc.to_string()));
            }
            item.insert("parameters".to_string(), params);
            out.push(Value::Object(item));
        }
    }

    Ok(out)
}

/// Recursively normalizes a Gemini Schema object into standard JSON Schema accepted by
/// OpenAI Codex Responses API:
/// - Uppercase Gemini types (`OBJECT`, `STRING`, `ARRAY`, `INTEGER`, `NUMBER`, `BOOLEAN`) → lowercase.
/// - `nullable: true` → `type: [<type>, "null"]` (without duplicating `"null"`), removing `nullable`.
/// - Strips `propertyOrdering` / `property_ordering` and incompatible Gemini `format` values.
pub fn normalize_gemini_schema(schema: &mut Value) {
    normalize_gemini_schema_depth(schema, 0);
}

fn normalize_gemini_type_name(raw: &str) -> Option<&'static str> {
    match raw.to_ascii_uppercase().as_str() {
        "OBJECT" => Some("object"),
        "STRING" => Some("string"),
        "ARRAY" => Some("array"),
        "INTEGER" => Some("integer"),
        "NUMBER" => Some("number"),
        "BOOLEAN" => Some("boolean"),
        "NULL" => Some("null"),
        _ => None,
    }
}

fn schema_type_includes_string(type_val: Option<&Value>) -> bool {
    match type_val {
        Some(Value::String(s)) => s == "string",
        Some(Value::Array(arr)) => arr.iter().any(|v| v.as_str() == Some("string")),
        _ => false,
    }
}

fn normalize_gemini_schema_depth(schema: &mut Value, depth: usize) {
    if depth >= MAX_SCHEMA_NORMALIZE_DEPTH {
        return;
    }

    match schema {
        Value::Object(obj) => {
            obj.remove("propertyOrdering");
            obj.remove("property_ordering");

            if let Some(type_val) = obj.get_mut("type") {
                match type_val {
                    Value::String(s) => {
                        if let Some(lower) = normalize_gemini_type_name(s) {
                            *s = lower.to_string();
                        }
                    }
                    Value::Array(arr) => {
                        for item in arr.iter_mut() {
                            if let Value::String(s) = item
                                && let Some(lower) = normalize_gemini_type_name(s)
                            {
                                *s = lower.to_string();
                            }
                        }
                    }
                    _ => {}
                }
            }

            let is_nullable = obj
                .remove("nullable")
                .and_then(|v| v.as_bool())
                .unwrap_or(false);
            if is_nullable {
                match obj.get_mut("type") {
                    Some(Value::String(t)) if t != "null" => {
                        let existing = t.clone();
                        obj.insert("type".to_string(), json!([existing, "null"]));
                    }
                    Some(Value::Array(arr)) if !arr.iter().any(|v| v.as_str() == Some("null")) => {
                        arr.push(Value::String("null".to_string()));
                    }
                    None => {
                        let variant_key = if obj.contains_key("anyOf") {
                            Some("anyOf")
                        } else if obj.contains_key("oneOf") {
                            Some("oneOf")
                        } else {
                            None
                        };
                        if let Some(key) = variant_key
                            && let Some(Value::Array(variants)) = obj.get_mut(key)
                            && !variants
                                .iter()
                                .any(|v| v.get("type").and_then(Value::as_str) == Some("null"))
                        {
                            variants.push(json!({ "type": "null" }));
                        }
                    }
                    _ => {}
                }
            }

            if let Some(fmt_val) = obj.get("format").and_then(Value::as_str) {
                let keep = schema_type_includes_string(obj.get("type"))
                    && ALLOWED_STRING_FORMATS.contains(&fmt_val);
                if !keep {
                    obj.remove("format");
                }
            }

            for map_key in ["properties", "patternProperties", "$defs", "definitions"] {
                if let Some(Value::Object(map)) = obj.get_mut(map_key) {
                    for val in map.values_mut() {
                        normalize_gemini_schema_depth(val, depth + 1);
                    }
                }
            }

            for sub_key in ["items", "additionalProperties", "not"] {
                if let Some(sub) = obj.get_mut(sub_key)
                    && (sub.is_object() || sub.is_array())
                {
                    normalize_gemini_schema_depth(sub, depth + 1);
                }
            }

            for arr_key in ["anyOf", "oneOf", "allOf", "prefixItems"] {
                if let Some(Value::Array(items)) = obj.get_mut(arr_key) {
                    for item in items.iter_mut() {
                        normalize_gemini_schema_depth(item, depth + 1);
                    }
                }
            }
        }
        Value::Array(items) => {
            for item in items.iter_mut() {
                normalize_gemini_schema_depth(item, depth + 1);
            }
        }
        _ => {}
    }
}

fn extract_tool_config(
    root: &Map<String, Value>,
) -> (Option<&'static str>, Option<HashSet<String>>) {
    let Some(tool_cfg) = root
        .get("toolConfig")
        .or_else(|| root.get("tool_config"))
        .and_then(Value::as_object)
    else {
        return (None, None);
    };
    let Some(fn_cfg) = tool_cfg
        .get("functionCallingConfig")
        .or_else(|| tool_cfg.get("function_calling_config"))
        .and_then(Value::as_object)
    else {
        return (None, None);
    };

    let mode = fn_cfg.get("mode").and_then(Value::as_str).and_then(|m| {
        match m.trim().to_ascii_uppercase().as_str() {
            "AUTO" => Some("auto"),
            "NONE" => Some("none"),
            "ANY" => Some("required"),
            _ => None,
        }
    });

    let allowed = fn_cfg
        .get("allowedFunctionNames")
        .or_else(|| fn_cfg.get("allowed_function_names"))
        .and_then(Value::as_array)
        .map(|arr| {
            arr.iter()
                .filter_map(Value::as_str)
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(str::to_string)
                .collect::<HashSet<_>>()
        })
        .filter(|set| !set.is_empty());

    (mode, allowed)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct ReasoningRequest {
    effort: Option<&'static str>,
    include_summary: bool,
}

fn extract_reasoning_config(root: &Map<String, Value>) -> Option<ReasoningRequest> {
    let thinking_cfg = root
        .get("generationConfig")
        .or_else(|| root.get("generation_config"))
        .and_then(Value::as_object)
        .and_then(|gen_cfg| {
            gen_cfg
                .get("thinkingConfig")
                .or_else(|| gen_cfg.get("thinking_config"))
                .and_then(Value::as_object)
        });

    let Some(thinking_cfg) = thinking_cfg else {
        // Default for reasoning-capable Codex models when thinkingConfig is omitted:
        // request model-default effort with auto thought summaries + encrypted reasoning.
        return Some(ReasoningRequest {
            effort: None,
            include_summary: true,
        });
    };

    let include_summary = thinking_cfg
        .get("includeThoughts")
        .or_else(|| thinking_cfg.get("include_thoughts"))
        .and_then(Value::as_bool)
        .unwrap_or(true);

    if let Some(level) = thinking_cfg
        .get("thinkingLevel")
        .or_else(|| thinking_cfg.get("thinking_level"))
    {
        match level {
            Value::String(s) => {
                let normalized = s.trim().to_ascii_uppercase();
                let bare = normalized
                    .strip_prefix("THINKING_LEVEL_")
                    .unwrap_or(normalized.as_str());
                match bare {
                    "NONE" | "OFF" => return None,
                    "MINIMAL" | "LOW" => {
                        return Some(ReasoningRequest {
                            effort: Some("low"),
                            include_summary,
                        });
                    }
                    "MEDIUM" => {
                        return Some(ReasoningRequest {
                            effort: Some("medium"),
                            include_summary,
                        });
                    }
                    "HIGH" => {
                        return Some(ReasoningRequest {
                            effort: Some("high"),
                            include_summary,
                        });
                    }
                    "XHIGH" | "EXTRA_HIGH" | "VERY_HIGH" | "EXTRA" | "MAX" => {
                        return Some(ReasoningRequest {
                            effort: Some("xhigh"),
                            include_summary,
                        });
                    }
                    "MODEL_DEFAULT" | "UNSPECIFIED" => {
                        // Fall through to optional thinkingBudget, or default effort with summary.
                    }
                    _ => {}
                }
            }
            Value::Number(n) => match n.as_i64() {
                Some(1) => {
                    return Some(ReasoningRequest {
                        effort: Some("low"),
                        include_summary,
                    });
                }
                Some(2) => {
                    return Some(ReasoningRequest {
                        effort: Some("medium"),
                        include_summary,
                    });
                }
                Some(3) => {
                    return Some(ReasoningRequest {
                        effort: Some("high"),
                        include_summary,
                    });
                }
                Some(7) => {
                    return Some(ReasoningRequest {
                        effort: Some("xhigh"),
                        include_summary,
                    });
                }
                // 0 is THINKING_LEVEL_UNSPECIFIED in Antigravity's proto enum.
                _ => {}
            },
            _ => {}
        }
    }

    if let Some(budget) = thinking_cfg
        .get("thinkingBudget")
        .or_else(|| thinking_cfg.get("thinking_budget"))
        .and_then(Value::as_i64)
    {
        if budget <= 0 {
            return None;
        }
        let effort = if budget <= 2048 {
            "low"
        } else if budget <= 8192 {
            "medium"
        } else {
            "high"
        };
        return Some(ReasoningRequest {
            effort: Some(effort),
            include_summary,
        });
    }

    Some(ReasoningRequest {
        effort: None,
        include_summary,
    })
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct CodexReasoningSignaturePayload {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    id: Option<String>,
    #[serde(default)]
    summary: Vec<Value>,
    encrypted_content: String,
}

fn encode_codex_reasoning_signature(
    id: Option<&str>,
    summary: Vec<Value>,
    encrypted_content: &str,
) -> Option<String> {
    if encrypted_content.trim().is_empty() {
        return None;
    }
    let clean_id = id
        .map(str::trim)
        .filter(|s| !s.is_empty() && s.len() <= MAX_CODEX_REASONING_ID_LEN)
        .map(str::to_string);
    let clean_summary: Vec<Value> = summary
        .into_iter()
        .filter(Value::is_object)
        .take(MAX_CODEX_REASONING_SUMMARY_ITEMS)
        .collect();

    let payload = CodexReasoningSignaturePayload {
        id: clean_id,
        summary: clean_summary,
        encrypted_content: encrypted_content.to_string(),
    };
    let json_bytes = serde_json::to_vec(&payload).ok()?;
    let raw_len = CODEX_THOUGHT_SIGNATURE_PREFIX
        .len()
        .saturating_add(json_bytes.len());
    let encoded_len = raw_len.div_ceil(3).saturating_mul(4);
    if encoded_len > MAX_CODEX_REASONING_SIGNATURE_BYTES {
        tracing::warn!(
            encoded_len,
            max_bytes = MAX_CODEX_REASONING_SIGNATURE_BYTES,
            "dropping oversized Codex reasoning signature"
        );
        return None;
    }
    // Encode the entire `agyc:codex:v1:<json>` envelope as standard base64 so Gemini proto3
    // `bytes thought_signature` decoders (e.g. Antigravity's Go `protojson`) accept and
    // round-trip it cleanly without rejecting non-base64 separator characters.
    let mut envelope = Vec::with_capacity(raw_len);
    envelope.extend_from_slice(CODEX_THOUGHT_SIGNATURE_PREFIX.as_bytes());
    envelope.extend_from_slice(&json_bytes);
    let out = STANDARD.encode(&envelope);
    if out.len() > MAX_CODEX_REASONING_SIGNATURE_BYTES {
        return None;
    }
    Some(out)
}

fn decode_base64_tolerant(input: &str) -> Option<Vec<u8>> {
    STANDARD
        .decode(input)
        .or_else(|_| STANDARD_NO_PAD.decode(input))
        .or_else(|_| URL_SAFE.decode(input))
        .or_else(|_| URL_SAFE_NO_PAD.decode(input))
        .ok()
}

fn decode_codex_reasoning_signature(sig: &str) -> Option<CodexReasoningSignaturePayload> {
    if sig.is_empty() || sig.len() > MAX_CODEX_REASONING_SIGNATURE_BYTES {
        return None;
    }

    let json_bytes = if let Some(encoded) = sig.strip_prefix(CODEX_THOUGHT_SIGNATURE_PREFIX) {
        if encoded.is_empty() {
            return None;
        }
        decode_base64_tolerant(encoded)?
    } else {
        let envelope = decode_base64_tolerant(sig)?;
        let rest = envelope.strip_prefix(CODEX_THOUGHT_SIGNATURE_PREFIX.as_bytes())?;
        rest.to_vec()
    };

    let mut payload: CodexReasoningSignaturePayload = serde_json::from_slice(&json_bytes).ok()?;
    if payload.encrypted_content.trim().is_empty() {
        return None;
    }
    if let Some(ref id) = payload.id
        && (id.trim().is_empty() || id.len() > MAX_CODEX_REASONING_ID_LEN)
    {
        return None;
    }
    payload.summary.retain(Value::is_object);
    if payload.summary.len() > MAX_CODEX_REASONING_SUMMARY_ITEMS {
        payload.summary.truncate(MAX_CODEX_REASONING_SUMMARY_ITEMS);
    }
    Some(payload)
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

/// Extracts strictly sanitized `(error_code, error_type)` slug metadata from an upstream Codex
/// error JSON payload (`error.{code,type}` or `response.error.{code,type}`) for safe logging (S2).
/// Never returns `error.message`, secrets, whitespace, or strings longer than 64 ASCII slug chars.
pub(crate) fn extract_safe_codex_error_fields(body: &[u8]) -> (Option<String>, Option<String>) {
    let Ok(parsed) = serde_json::from_slice::<Value>(body) else {
        return (None, None);
    };
    extract_safe_codex_error_fields_from_value(&parsed)
}

fn extract_safe_codex_error_fields_from_value(v: &Value) -> (Option<String>, Option<String>) {
    let Some(err_obj) = v
        .get("error")
        .or_else(|| v.get("response").and_then(|r| r.get("error")))
    else {
        return (None, None);
    };
    let code = sanitize_codex_error_slug(err_obj.get("code").and_then(Value::as_str));
    let err_type = sanitize_codex_error_slug(err_obj.get("type").and_then(Value::as_str));
    (code, err_type)
}

fn sanitize_codex_error_slug(raw: Option<&str>) -> Option<String> {
    let trimmed = raw?.trim();
    if trimmed.is_empty() || trimmed.len() > 64 {
        return None;
    }
    if trimmed.starts_with("AIza")
        || trimmed.starts_with("sk-")
        || trimmed.starts_with("eyJ")
        || trimmed.starts_with("ghp_")
    {
        return None;
    }
    if !trimmed
        .bytes()
        .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
    {
        return None;
    }
    Some(trimmed.to_string())
}

/// Maps an upstream Codex HTTP status and bounded error body (or SSE `response.failed`/`error`
/// JSON) into a sanitized `(http_status, gemini_error_json)` tuple with zero token/credential
/// reflection.
pub fn sanitize_codex_error_json(status: u16, upstream_body: &[u8]) -> (u16, Value) {
    let parsed = serde_json::from_slice::<Value>(upstream_body).ok();
    let code_str = parsed
        .as_ref()
        .and_then(|v| {
            v.get("error")
                .or_else(|| v.get("response").and_then(|r| r.get("error")))
                .and_then(|e| {
                    e.get("code")
                        .and_then(Value::as_str)
                        .or_else(|| e.get("type").and_then(Value::as_str))
                })
        })
        .unwrap_or("");

    let (mapped_status, grpc_status, message) = match (status, code_str) {
        (401, _)
        | (_, "authentication_error" | "invalid_api_key" | "token_expired" | "unauthorized") => (
            401,
            "UNAUTHENTICATED",
            "OpenAI Codex session expired — sign in again",
        ),
        (429, _)
        | (
            _,
            "rate_limit_exceeded" | "insufficient_quota" | "usage_limit_reached" | "quota_exceeded",
        ) => (
            429,
            "RESOURCE_EXHAUSTED",
            "ChatGPT Codex quota limit reached",
        ),
        (_, "context_length_exceeded") => (
            400,
            "INVALID_ARGUMENT",
            "Request exceeds OpenAI Codex model context limit",
        ),
        (_, "model_not_found") => (
            404,
            "NOT_FOUND",
            "Requested OpenAI Codex model is not available",
        ),
        (400, _) | (_, "invalid_request_error") => (
            400,
            "INVALID_ARGUMENT",
            "OpenAI Codex rejected the request as invalid",
        ),
        (403, _) => (
            403,
            "PERMISSION_DENIED",
            "OpenAI Codex access denied for this account or model",
        ),
        (404, _) => (404, "NOT_FOUND", "OpenAI Codex endpoint or model not found"),
        (code, _) if (400..500).contains(&code) => (
            code,
            "INVALID_ARGUMENT",
            "OpenAI Codex rejected the request",
        ),
        (code, _) if (500..600).contains(&code) => {
            (code, "INTERNAL", "OpenAI Codex upstream service error")
        }
        _ => (502, "INTERNAL", "OpenAI Codex upstream service error"),
    };

    (
        mapped_status,
        json!({
            "error": {
                "code": mapped_status,
                "message": message,
                "status": grpc_status
            }
        }),
    )
}

#[derive(Debug)]
struct PendingFunctionCall {
    item_id: Option<String>,
    output_index: Option<u64>,
    call_id: String,
    name: String,
    arguments: String,
}

#[derive(Debug)]
struct StreamedReasoningItem {
    item_id: Option<String>,
    output_index: Option<u64>,
}

/// Bounded incremental SSE parser and translator (`Codex Responses SSE → Gemini SSE JSON chunks`).
#[derive(Debug)]
pub struct CodexSseTranslator {
    max_event_bytes: usize,
    max_tool_args_bytes: usize,
    max_total_tool_args_bytes: usize,
    line_buf: Vec<u8>,
    event_data: Vec<u8>,
    has_data_field: bool,
    pending_cr: bool,
    first_line_seen: bool,
    comment_line_discarded: bool,
    pending_calls: Vec<PendingFunctionCall>,
    total_tool_args_bytes: usize,
    synthetic_call_seq: u64,
    streamed_reasoning_items: Vec<StreamedReasoningItem>,
    needs_reasoning_part_separator: bool,
    reasoning_trailing_newlines: u8,
}

impl Default for CodexSseTranslator {
    fn default() -> Self {
        Self::new()
    }
}

impl CodexSseTranslator {
    pub fn new() -> Self {
        Self::with_limits(
            DEFAULT_MAX_CODEX_SSE_EVENT_BYTES,
            DEFAULT_MAX_TOOL_CALL_ARGS_BYTES,
        )
    }

    pub fn with_limits(max_event_bytes: usize, max_tool_args_bytes: usize) -> Self {
        let total_cap = max_tool_args_bytes
            .saturating_mul(2)
            .min(DEFAULT_MAX_TOTAL_TOOL_CALL_ARGS_BYTES)
            .max(max_tool_args_bytes);
        Self {
            max_event_bytes,
            max_tool_args_bytes,
            max_total_tool_args_bytes: total_cap,
            line_buf: Vec::new(),
            event_data: Vec::new(),
            has_data_field: false,
            pending_cr: false,
            first_line_seen: false,
            comment_line_discarded: false,
            pending_calls: Vec::new(),
            total_tool_args_bytes: 0,
            synthetic_call_seq: 0,
            streamed_reasoning_items: Vec::new(),
            needs_reasoning_part_separator: false,
            reasoning_trailing_newlines: 0,
        }
    }

    /// Formats a translated Gemini chunk JSON value into a wire SSE frame (`data: <json>\n\n`).
    pub fn format_gemini_sse_frame(chunk: &Value) -> Result<Bytes, ProviderError> {
        let json_str = serde_json::to_string(chunk).map_err(|_| {
            ProviderError::Upstream("failed to serialize translated Gemini SSE chunk".to_string())
        })?;
        let mut frame = String::with_capacity(json_str.len() + 8);
        frame.push_str("data: ");
        frame.push_str(&json_str);
        frame.push_str("\n\n");
        Ok(Bytes::from(frame))
    }

    /// Feeds raw bytes from the upstream Codex SSE stream and returns any translated Gemini
    /// response chunks completed by this byte slice.
    pub fn push_chunk(&mut self, mut chunk: &[u8]) -> Result<Vec<Value>, ProviderError> {
        let mut out = Vec::new();
        while !chunk.is_empty() {
            if self.pending_cr {
                self.pending_cr = false;
                if chunk[0] == b'\n' {
                    chunk = &chunk[1..];
                    continue;
                }
            }

            match chunk.iter().position(|&b| b == b'\n' || b == b'\r') {
                Some(delim_idx) => {
                    let segment = &chunk[..delim_idx];
                    let delim = chunk[delim_idx];
                    self.append_line_bytes(segment)?;
                    if delim == b'\r' {
                        self.pending_cr = true;
                    }
                    self.finish_line(&mut out)?;
                    chunk = &chunk[delim_idx + 1..];
                }
                None => {
                    self.append_line_bytes(chunk)?;
                    break;
                }
            }
        }
        Ok(out)
    }

    /// Flushes any unterminated trailing line or event at EOF.
    pub fn finish(&mut self) -> Result<Vec<Value>, ProviderError> {
        let mut out = Vec::new();
        self.pending_cr = false;
        if self.comment_line_discarded {
            self.first_line_seen = true;
            self.comment_line_discarded = false;
        } else if !self.line_buf.is_empty() {
            let line = std::mem::take(&mut self.line_buf);
            self.process_line(&line, &mut out)?;
        }
        self.flush_event(&mut out)?;
        Ok(out)
    }

    fn append_line_bytes(&mut self, segment: &[u8]) -> Result<(), ProviderError> {
        if self.comment_line_discarded || segment.is_empty() {
            return Ok(());
        }

        if self.line_buf.len().saturating_add(segment.len()) > self.max_event_bytes {
            let mut prefix = [0u8; 4];
            let mut count = 0;
            for &b in self.line_buf.iter().chain(segment.iter()).take(4) {
                prefix[count] = b;
                count += 1;
            }
            let slice = &prefix[..count];
            let effective = if !self.first_line_seen {
                slice.strip_prefix(&UTF8_BOM).unwrap_or(slice)
            } else {
                slice
            };
            if effective.first() == Some(&b':') {
                // Ignore oversized SSE keep-alive comments without corrupting the active event.
                self.line_buf = Vec::new();
                self.comment_line_discarded = true;
                return Ok(());
            }

            self.line_buf = Vec::new();
            self.event_data = Vec::new();
            self.has_data_field = false;
            return Err(ProviderError::Upstream(
                "upstream Codex SSE line exceeded maximum size limit".to_string(),
            ));
        }

        self.line_buf.extend_from_slice(segment);
        Ok(())
    }

    fn finish_line(&mut self, out: &mut Vec<Value>) -> Result<(), ProviderError> {
        if self.comment_line_discarded {
            self.first_line_seen = true;
            self.comment_line_discarded = false;
            return Ok(());
        }

        let mut line = std::mem::take(&mut self.line_buf);
        let res = self.process_line(&line, out);
        if line.capacity() <= RETAIN_BUFFER_CAPACITY_LIMIT {
            line.clear();
            self.line_buf = line;
        }
        res
    }

    fn process_line(&mut self, raw_line: &[u8], out: &mut Vec<Value>) -> Result<(), ProviderError> {
        let line = if !self.first_line_seen {
            self.first_line_seen = true;
            raw_line.strip_prefix(&UTF8_BOM).unwrap_or(raw_line)
        } else {
            raw_line
        };

        if line.is_empty() {
            return self.flush_event(out);
        }

        if line[0] == b':' {
            return Ok(());
        }

        let (field_name, field_value) = match line.iter().position(|&b| b == b':') {
            Some(colon) => {
                let raw_val = &line[colon + 1..];
                let val = raw_val.strip_prefix(b" ").unwrap_or(raw_val);
                (&line[..colon], val)
            }
            None => (line, &[][..]),
        };

        if field_name == b"data" {
            let extra = usize::from(self.has_data_field).saturating_add(field_value.len());
            if self.event_data.len().saturating_add(extra) > self.max_event_bytes {
                self.event_data = Vec::new();
                self.has_data_field = false;
                return Err(ProviderError::Upstream(
                    "upstream Codex SSE event exceeded maximum size limit".to_string(),
                ));
            }
            if self.has_data_field {
                self.event_data.push(b'\n');
            }
            self.event_data.extend_from_slice(field_value);
            self.has_data_field = true;
        }

        Ok(())
    }

    fn flush_event(&mut self, out: &mut Vec<Value>) -> Result<(), ProviderError> {
        let had_data = self.has_data_field;
        self.has_data_field = false;

        let mut data = std::mem::take(&mut self.event_data);
        let res = if had_data && !data.is_empty() {
            let trimmed = trim_ascii_whitespace(&data);
            if trimmed.is_empty() || trimmed == b"[DONE]" {
                Ok(())
            } else {
                self.translate_event_payload(trimmed, out)
            }
        } else {
            Ok(())
        };

        if data.capacity() <= RETAIN_BUFFER_CAPACITY_LIMIT {
            data.clear();
            self.event_data = data;
        }
        res
    }

    fn find_pending_call_index(
        &self,
        item_id: Option<&str>,
        output_index: Option<u64>,
    ) -> Option<usize> {
        if let Some(id) = item_id
            && let Some(pos) = self
                .pending_calls
                .iter()
                .position(|c| c.item_id.as_deref() == Some(id))
        {
            return Some(pos);
        }
        if let Some(idx) = output_index {
            return self
                .pending_calls
                .iter()
                .position(|c| c.output_index == Some(idx));
        }
        None
    }

    fn reset_reasoning_part_separator(&mut self) {
        self.needs_reasoning_part_separator = false;
        self.reasoning_trailing_newlines = 0;
    }

    fn find_streamed_reasoning_index(
        &self,
        item_id: Option<&str>,
        output_index: Option<u64>,
    ) -> Option<usize> {
        if let Some(id) = item_id
            && let Some(pos) = self
                .streamed_reasoning_items
                .iter()
                .position(|item| item.item_id.as_deref() == Some(id))
        {
            return Some(pos);
        }
        if let Some(idx) = output_index
            && let Some(pos) = self.streamed_reasoning_items.iter().position(|item| {
                item.output_index == Some(idx) && (item_id.is_none() || item.item_id.is_none())
            })
        {
            return Some(pos);
        }
        if let Some(pos) = self
            .streamed_reasoning_items
            .iter()
            .position(|item| item.item_id.is_none() && item.output_index.is_none())
        {
            return Some(pos);
        }
        if item_id.is_none() && output_index.is_none() {
            return self.streamed_reasoning_items.len().checked_sub(1);
        }
        None
    }

    fn record_streamed_reasoning_item(&mut self, item_id: Option<&str>, output_index: Option<u64>) {
        if let Some(pos) = self.find_streamed_reasoning_index(item_id, output_index) {
            let entry = &mut self.streamed_reasoning_items[pos];
            if entry.item_id.is_none() {
                entry.item_id = item_id.map(str::to_owned);
            }
            if entry.output_index.is_none() {
                entry.output_index = output_index;
            }
            return;
        }
        if self.streamed_reasoning_items.len() >= MAX_TRACKED_REASONING_ITEMS {
            self.streamed_reasoning_items.remove(0);
        }
        self.streamed_reasoning_items.push(StreamedReasoningItem {
            item_id: item_id.map(str::to_owned),
            output_index,
        });
    }

    fn translate_event_payload(
        &mut self,
        payload: &[u8],
        out: &mut Vec<Value>,
    ) -> Result<(), ProviderError> {
        let event: Value = serde_json::from_slice(payload).map_err(|_| {
            ProviderError::Upstream("upstream Codex SSE event contained malformed JSON".to_string())
        })?;
        let Some(event_type) = event.get("type").and_then(Value::as_str) else {
            return Ok(());
        };

        match event_type {
            "response.reasoning_summary_part.added" | "response.reasoning_summary_part.done" => {
                let item_id = non_empty_str(event.get("item_id"));
                let output_index = event.get("output_index").and_then(Value::as_u64);
                if self
                    .find_streamed_reasoning_index(item_id, output_index)
                    .is_some()
                {
                    self.needs_reasoning_part_separator = true;
                }
            }
            "response.reasoning_summary_text.delta" => {
                if let Some(delta) = event.get("delta").and_then(Value::as_str)
                    && !delta.is_empty()
                {
                    let emitted = if self.needs_reasoning_part_separator {
                        self.needs_reasoning_part_separator = false;
                        let leading_newlines = delta.bytes().take_while(|&b| b == b'\n').count();
                        let existing_newlines = usize::from(self.reasoning_trailing_newlines)
                            .saturating_add(leading_newlines);
                        let needed = 2usize.saturating_sub(existing_newlines);
                        if needed == 0 {
                            delta.to_string()
                        } else {
                            let mut combined = String::with_capacity(delta.len() + needed);
                            for _ in 0..needed {
                                combined.push('\n');
                            }
                            combined.push_str(delta);
                            combined
                        }
                    } else {
                        delta.to_string()
                    };
                    let item_id = non_empty_str(event.get("item_id"));
                    let output_index = event.get("output_index").and_then(Value::as_u64);
                    self.record_streamed_reasoning_item(item_id, output_index);
                    self.reasoning_trailing_newlines =
                        update_trailing_newlines(self.reasoning_trailing_newlines, &emitted);
                    out.push(gemini_thought_chunk(&emitted));
                }
            }
            "response.output_text.delta" => {
                if let Some(delta) = event.get("delta").and_then(Value::as_str)
                    && !delta.is_empty()
                {
                    out.push(json!({
                        "candidates": [{
                            "content": {
                                "role": "model",
                                "parts": [{ "text": delta }]
                            },
                            "index": 0
                        }]
                    }));
                }
            }
            "response.output_item.added" => {
                if let Some(item) = event.get("item").and_then(Value::as_object) {
                    if item.get("type").and_then(Value::as_str) == Some("reasoning") {
                        self.reset_reasoning_part_separator();
                    } else if item.get("type").and_then(Value::as_str) == Some("function_call") {
                        let item_id = item.get("id").and_then(Value::as_str).map(str::to_string);
                        let output_index = event.get("output_index").and_then(Value::as_u64);
                        let call_id = item
                            .get("call_id")
                            .and_then(Value::as_str)
                            .unwrap_or("")
                            .to_string();
                        let name = item
                            .get("name")
                            .and_then(Value::as_str)
                            .unwrap_or("")
                            .to_string();
                        let initial_args =
                            item.get("arguments").and_then(Value::as_str).unwrap_or("");
                        self.check_args_budget(0, initial_args.len())?;

                        if let Some(pos) =
                            self.find_pending_call_index(item_id.as_deref(), output_index)
                        {
                            let entry = &mut self.pending_calls[pos];
                            if entry.call_id.is_empty() {
                                entry.call_id = call_id;
                            }
                            if entry.name.is_empty() {
                                entry.name = name;
                            }
                            if entry.arguments.is_empty() && !initial_args.is_empty() {
                                self.total_tool_args_bytes = self
                                    .total_tool_args_bytes
                                    .saturating_add(initial_args.len());
                                entry.arguments.push_str(initial_args);
                            }
                        } else {
                            if self.pending_calls.len() >= MAX_CONCURRENT_TOOL_CALLS {
                                return Err(ProviderError::Upstream(
                                    "upstream Codex stream exceeded maximum concurrent tool calls"
                                        .to_string(),
                                ));
                            }
                            self.total_tool_args_bytes = self
                                .total_tool_args_bytes
                                .saturating_add(initial_args.len());
                            self.pending_calls.push(PendingFunctionCall {
                                item_id,
                                output_index,
                                call_id,
                                name,
                                arguments: initial_args.to_string(),
                            });
                        }
                    }
                }
            }
            "response.function_call_arguments.delta" => {
                if let Some(delta) = event.get("delta").and_then(Value::as_str)
                    && !delta.is_empty()
                {
                    let item_id = event.get("item_id").and_then(Value::as_str);
                    let output_index = event.get("output_index").and_then(Value::as_u64);
                    let pos = match self.find_pending_call_index(item_id, output_index) {
                        Some(pos) => pos,
                        None => {
                            if self.pending_calls.len() >= MAX_CONCURRENT_TOOL_CALLS {
                                return Err(ProviderError::Upstream(
                                    "upstream Codex stream exceeded maximum concurrent tool calls"
                                        .to_string(),
                                ));
                            }
                            self.pending_calls.push(PendingFunctionCall {
                                item_id: item_id.map(str::to_string),
                                output_index,
                                call_id: String::new(),
                                name: String::new(),
                                arguments: String::new(),
                            });
                            self.pending_calls.len() - 1
                        }
                    };
                    let current_len = self.pending_calls[pos].arguments.len();
                    self.check_args_budget(current_len, delta.len())?;
                    self.pending_calls[pos].arguments.push_str(delta);
                    self.total_tool_args_bytes =
                        self.total_tool_args_bytes.saturating_add(delta.len());
                }
            }
            "response.function_call_arguments.done" => {
                if let Some(full_args) = event.get("arguments").and_then(Value::as_str)
                    && !full_args.is_empty()
                {
                    let item_id = event.get("item_id").and_then(Value::as_str);
                    let output_index = event.get("output_index").and_then(Value::as_u64);
                    if let Some(pos) = self.find_pending_call_index(item_id, output_index)
                        && self.pending_calls[pos].arguments.is_empty()
                    {
                        self.check_args_budget(0, full_args.len())?;
                        self.pending_calls[pos].arguments = full_args.to_string();
                        self.total_tool_args_bytes =
                            self.total_tool_args_bytes.saturating_add(full_args.len());
                    }
                }
            }
            "response.output_item.done" => {
                if let Some(item) = event.get("item").and_then(Value::as_object) {
                    match item.get("type").and_then(Value::as_str) {
                        Some("reasoning") => {
                            let id = non_empty_str(item.get("id"));
                            let output_index = event.get("output_index").and_then(Value::as_u64);
                            let summary = item.get("summary").and_then(Value::as_array);
                            if let Some(pos) = self.find_streamed_reasoning_index(id, output_index)
                            {
                                self.streamed_reasoning_items.remove(pos);
                                if pos == self.streamed_reasoning_items.len() {
                                    self.reset_reasoning_part_separator();
                                }
                            } else if let Some(summary) = summary {
                                let text = join_reasoning_summary_parts(summary);
                                if !text.is_empty() {
                                    out.push(gemini_thought_chunk(&text));
                                }
                            }
                            if let Some(encrypted_content) =
                                item.get("encrypted_content").and_then(Value::as_str)
                                && !encrypted_content.trim().is_empty()
                                && let Some(sig) = encode_codex_reasoning_signature(
                                    id,
                                    summary.cloned().unwrap_or_default(),
                                    encrypted_content,
                                )
                            {
                                out.push(json!({
                                    "candidates": [{
                                        "content": {
                                            "role": "model",
                                            "parts": [{
                                                "text": "",
                                                "thought": true,
                                                "thoughtSignature": sig
                                            }]
                                        },
                                        "index": 0
                                    }]
                                }));
                            }
                        }
                        Some("function_call") => {
                            let item_id = item.get("id").and_then(Value::as_str);
                            let output_index = event.get("output_index").and_then(Value::as_u64);
                            let pending = self
                                .find_pending_call_index(item_id, output_index)
                                .map(|pos| self.pending_calls.remove(pos));
                            if let Some(ref entry) = pending {
                                self.total_tool_args_bytes = self
                                    .total_tool_args_bytes
                                    .saturating_sub(entry.arguments.len());
                            }

                            let call_id = item
                                .get("call_id")
                                .and_then(Value::as_str)
                                .filter(|s| !s.is_empty())
                                .map(str::to_string)
                                .or_else(|| {
                                    pending
                                        .as_ref()
                                        .map(|p| p.call_id.clone())
                                        .filter(|s| !s.is_empty())
                                })
                                .or_else(|| item_id.filter(|s| !s.is_empty()).map(str::to_string))
                                .unwrap_or_else(|| {
                                    self.synthetic_call_seq =
                                        self.synthetic_call_seq.wrapping_add(1);
                                    format!("call_out_{}", self.synthetic_call_seq)
                                });

                            let name = item
                                .get("name")
                                .and_then(Value::as_str)
                                .filter(|s| !s.is_empty())
                                .map(str::to_string)
                                .or_else(|| {
                                    pending
                                        .as_ref()
                                        .map(|p| p.name.clone())
                                        .filter(|s| !s.is_empty())
                                })
                                .ok_or_else(|| {
                                    ProviderError::Upstream(
                                        "upstream Codex function_call item missing name"
                                            .to_string(),
                                    )
                                })?;

                            let raw_args = pending
                                .as_ref()
                                .map(|p| p.arguments.as_str())
                                .filter(|s| !s.is_empty())
                                .or_else(|| item.get("arguments").and_then(Value::as_str))
                                .unwrap_or("");
                            if raw_args.len() > self.max_tool_args_bytes {
                                return Err(ProviderError::Upstream(
                            "upstream Codex function call arguments exceeded maximum size limit"
                                .to_string(),
                        ));
                            }

                            let parsed_args = if raw_args.trim().is_empty() {
                                Value::Object(Map::new())
                            } else {
                                let val: Value = serde_json::from_str(raw_args).map_err(|_| {
                                    ProviderError::Upstream(
                                "upstream Codex function call emitted malformed JSON arguments"
                                    .to_string(),
                            )
                                })?;
                                if !val.is_object() {
                                    return Err(ProviderError::Upstream(
                                "upstream Codex function call arguments must be a JSON object"
                                    .to_string(),
                            ));
                                }
                                val
                            };

                            out.push(json!({
                                "candidates": [{
                                    "content": {
                                        "role": "model",
                                        "parts": [{
                                            "functionCall": {
                                                "id": call_id,
                                                "name": name,
                                                "args": parsed_args
                                            }
                                        }]
                                    },
                                    "index": 0
                                }]
                            }));
                        }
                        _ => {}
                    }
                }
            }
            "response.completed" | "response.incomplete" => {
                let resp = event.get("response").unwrap_or(&event);
                let incomplete_reason = resp
                    .get("incomplete_details")
                    .and_then(|d| d.get("reason"))
                    .and_then(Value::as_str);
                let finish_reason = match incomplete_reason {
                    Some("max_output_tokens") => "MAX_TOKENS",
                    Some("content_filter") => "SAFETY",
                    _ if event_type == "response.incomplete" => "MAX_TOKENS",
                    _ => "STOP",
                };

                let mut chunk = Map::new();
                chunk.insert(
                    "candidates".to_string(),
                    json!([{
                        "finishReason": finish_reason,
                        "index": 0
                    }]),
                );
                if let Some(resp_id) = resp
                    .get("id")
                    .and_then(Value::as_str)
                    .and_then(companion_core::sanitize_response_id)
                {
                    chunk.insert("responseId".to_string(), Value::String(resp_id));
                }

                if let Some(usage) = resp.get("usage").and_then(Value::as_object) {
                    let input_tokens = usage
                        .get("input_tokens")
                        .and_then(Value::as_u64)
                        .unwrap_or(0);
                    let cached_tokens = usage
                        .get("input_tokens_details")
                        .and_then(|d| d.get("cached_tokens"))
                        .and_then(Value::as_u64)
                        .unwrap_or(0);
                    let output_tokens = usage
                        .get("output_tokens")
                        .and_then(Value::as_u64)
                        .unwrap_or(0);
                    let reasoning_tokens = usage
                        .get("output_tokens_details")
                        .and_then(|d| d.get("reasoning_tokens"))
                        .and_then(Value::as_u64)
                        .unwrap_or(0);
                    let total_tokens = usage
                        .get("total_tokens")
                        .and_then(Value::as_u64)
                        .unwrap_or_else(|| input_tokens.saturating_add(output_tokens));

                    chunk.insert(
                        "usageMetadata".to_string(),
                        json!({
                            "promptTokenCount": input_tokens,
                            "cachedContentTokenCount": cached_tokens,
                            "candidatesTokenCount": output_tokens,
                            "thoughtsTokenCount": reasoning_tokens,
                            "totalTokenCount": total_tokens
                        }),
                    );
                }

                out.push(Value::Object(chunk));
            }
            "response.failed" | "error" => {
                let (mapped_status, err_json) = sanitize_codex_error_json(502, payload);
                let (error_code, error_type) = extract_safe_codex_error_fields_from_value(&event);
                let grpc_status = err_json
                    .get("error")
                    .and_then(|e| e.get("status"))
                    .and_then(Value::as_str)
                    .unwrap_or("INTERNAL");
                tracing::warn!(
                    event_type,
                    mapped_status,
                    grpc_status,
                    error_code = error_code.as_deref().unwrap_or("none"),
                    error_type = error_type.as_deref().unwrap_or("none"),
                    "OpenAI Codex SSE stream reported error event"
                );
                out.push(err_json);
            }
            _ => {}
        }

        Ok(())
    }

    fn check_args_budget(
        &mut self,
        current_item_bytes: usize,
        added_bytes: usize,
    ) -> Result<(), ProviderError> {
        if current_item_bytes.saturating_add(added_bytes) > self.max_tool_args_bytes
            || self.total_tool_args_bytes.saturating_add(added_bytes)
                > self.max_total_tool_args_bytes
        {
            self.pending_calls.clear();
            self.total_tool_args_bytes = 0;
            return Err(ProviderError::Upstream(
                "upstream Codex function call arguments exceeded maximum size limit".to_string(),
            ));
        }
        Ok(())
    }
}

fn non_empty_str(val: Option<&Value>) -> Option<&str> {
    val.and_then(Value::as_str).filter(|s| !s.is_empty())
}

fn join_reasoning_summary_parts(summary: &[Value]) -> String {
    let mut combined = String::new();
    let mut trailing_newlines = 0u8;
    for text in summary
        .iter()
        .filter(|part| part.get("type").and_then(Value::as_str) == Some("summary_text"))
        .filter_map(|part| part.get("text").and_then(Value::as_str))
        .filter(|text| !text.trim().is_empty())
    {
        if !combined.is_empty() {
            let leading_newlines = text.bytes().take_while(|&b| b == b'\n').count();
            let existing_newlines = usize::from(trailing_newlines).saturating_add(leading_newlines);
            let needed = 2usize.saturating_sub(existing_newlines);
            for _ in 0..needed {
                combined.push('\n');
            }
        }
        combined.push_str(text);
        trailing_newlines = update_trailing_newlines(trailing_newlines, text);
    }
    combined
}

fn gemini_thought_chunk(text: &str) -> Value {
    json!({
        "candidates": [{
            "content": {
                "role": "model",
                "parts": [{ "text": text, "thought": true }]
            },
            "index": 0
        }]
    })
}

fn update_trailing_newlines(prev: u8, emitted: &str) -> u8 {
    let mut count = prev;
    for b in emitted.bytes() {
        if b == b'\n' {
            count = count.saturating_add(1).min(2);
        } else {
            count = 0;
        }
    }
    count
}

fn trim_ascii_whitespace(bytes: &[u8]) -> &[u8] {
    let start = bytes
        .iter()
        .position(|b| !b.is_ascii_whitespace())
        .unwrap_or(bytes.len());
    let end = bytes
        .iter()
        .rposition(|b| !b.is_ascii_whitespace())
        .map_or(start, |i| i + 1);
    &bytes[start..end]
}

/// Aggregates translated Gemini SSE chunks into a single unary Gemini `:generateContent` JSON
/// response with a bounded memory budget.
#[derive(Debug)]
pub struct UnaryGeminiAggregator {
    parts: Vec<Value>,
    finish_reason: Option<String>,
    usage_metadata: Option<Value>,
    response_id: Option<String>,
    error_frame: Option<(u16, Value)>,
    accumulated_bytes: usize,
    max_bytes: usize,
}

impl Default for UnaryGeminiAggregator {
    fn default() -> Self {
        Self::new()
    }
}

impl UnaryGeminiAggregator {
    pub fn new() -> Self {
        Self::with_max_bytes(DEFAULT_MAX_UNARY_RESPONSE_BYTES)
    }

    pub fn with_max_bytes(max_bytes: usize) -> Self {
        Self {
            parts: Vec::new(),
            finish_reason: None,
            usage_metadata: None,
            response_id: None,
            error_frame: None,
            accumulated_bytes: 0,
            max_bytes,
        }
    }

    pub fn push_chunk(&mut self, chunk: Value) -> Result<(), ProviderError> {
        if let Some(err_obj) = chunk.get("error").and_then(Value::as_object) {
            let code = err_obj
                .get("code")
                .and_then(Value::as_u64)
                .and_then(|c| u16::try_from(c).ok())
                .unwrap_or(502);
            self.error_frame = Some((code, chunk));
            return Ok(());
        }

        if let Some(resp_id) = chunk
            .get("responseId")
            .and_then(Value::as_str)
            .and_then(companion_core::sanitize_response_id)
        {
            self.response_id = Some(resp_id);
        }

        if let Some(usage) = chunk.get("usageMetadata")
            && usage.is_object()
        {
            self.usage_metadata = Some(usage.clone());
        }

        if let Some(candidate) = chunk
            .get("candidates")
            .and_then(Value::as_array)
            .and_then(|arr| arr.first())
            .and_then(Value::as_object)
        {
            if let Some(reason) = candidate.get("finishReason").and_then(Value::as_str) {
                self.finish_reason = Some(reason.to_string());
            }

            if let Some(parts) = candidate
                .get("content")
                .and_then(|c| c.get("parts"))
                .and_then(Value::as_array)
            {
                for part in parts {
                    self.push_part(part)?;
                }
            }
        }

        Ok(())
    }

    fn push_part(&mut self, part: &Value) -> Result<(), ProviderError> {
        let Some(obj) = part.as_object() else {
            return Ok(());
        };

        if let Some(text) = obj.get("text").and_then(Value::as_str) {
            let thought_sig = obj
                .get("thoughtSignature")
                .or_else(|| obj.get("thought_signature"))
                .and_then(Value::as_str);
            let added_bytes = text.len().saturating_add(thought_sig.map_or(0, str::len));
            self.account_bytes(added_bytes)?;
            let is_thought = obj.get("thought").and_then(Value::as_bool) == Some(true);
            if let Some(last) = self.parts.last_mut().and_then(Value::as_object_mut)
                && last.get("text").is_some()
                && (last.get("thought").and_then(Value::as_bool) == Some(true)) == is_thought
                && !last.contains_key("thoughtSignature")
            {
                if let Some(Value::String(existing)) = last.get_mut("text") {
                    existing.push_str(text);
                }
                if let Some(sig) = thought_sig {
                    last.insert(
                        "thoughtSignature".to_string(),
                        Value::String(sig.to_string()),
                    );
                }
                return Ok(());
            }
            let mut part_map = Map::new();
            part_map.insert("text".to_string(), Value::String(text.to_string()));
            if is_thought {
                part_map.insert("thought".to_string(), Value::Bool(true));
            }
            if let Some(sig) = thought_sig {
                part_map.insert(
                    "thoughtSignature".to_string(),
                    Value::String(sig.to_string()),
                );
            }
            self.parts.push(Value::Object(part_map));
            return Ok(());
        }

        if obj.contains_key("functionCall") {
            let approx_len = serde_json::to_string(part).map(|s| s.len()).unwrap_or(0);
            self.account_bytes(approx_len)?;
            self.parts.push(part.clone());
        }

        Ok(())
    }

    fn account_bytes(&mut self, added: usize) -> Result<(), ProviderError> {
        let next = self.accumulated_bytes.saturating_add(added);
        if next > self.max_bytes {
            return Err(ProviderError::Upstream(
                "upstream Codex unary response exceeded maximum size limit".to_string(),
            ));
        }
        self.accumulated_bytes = next;
        Ok(())
    }

    pub fn finish(self) -> Result<(u16, Value), ProviderError> {
        if let Some((status, err_json)) = self.error_frame {
            return Ok((status, err_json));
        }

        let mut candidate = Map::new();
        candidate.insert(
            "content".to_string(),
            json!({
                "role": "model",
                "parts": self.parts
            }),
        );
        if let Some(reason) = self.finish_reason {
            candidate.insert("finishReason".to_string(), Value::String(reason));
        }
        candidate.insert("index".to_string(), json!(0));

        let mut root = Map::new();
        root.insert(
            "candidates".to_string(),
            Value::Array(vec![Value::Object(candidate)]),
        );
        if let Some(usage) = self.usage_metadata {
            root.insert("usageMetadata".to_string(), usage);
        }
        if let Some(resp_id) = self.response_id {
            root.insert("responseId".to_string(), Value::String(resp_id));
        }

        Ok((200, Value::Object(root)))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn translates_instructions_defaults_store_stream_and_omits_sampling_params() {
        let body = json!({
            "contents": [
                { "role": "user", "parts": [{ "text": "hello" }] }
            ],
            "generationConfig": {
                "temperature": 0.7,
                "topP": 0.9,
                "topK": 40,
                "maxOutputTokens": 4096,
                "stopSequences": ["END"]
            }
        });

        let out = translate_gemini_to_codex_request("gpt-6.2-sol", body.to_string().as_bytes())
            .expect("translate minimal");
        assert_eq!(out["model"], "gpt-6.2-sol");
        assert_eq!(out["instructions"], DEFAULT_CODEX_INSTRUCTIONS);
        assert_eq!(out["store"], false);
        assert_eq!(out["stream"], true);
        assert!(out.get("temperature").is_none());
        assert!(out.get("topP").is_none());
        assert!(out.get("top_p").is_none());
        assert!(out.get("maxOutputTokens").is_none());
        assert!(out.get("max_output_tokens").is_none());
        assert!(out.get("stopSequences").is_none());

        // Blank systemInstruction falls back to default instructions.
        let blank_sys = json!({
            "systemInstruction": { "parts": [{ "text": "   \n\t  " }] },
            "contents": [{ "role": "user", "parts": [{ "text": "hi" }] }]
        });
        let out_blank =
            translate_gemini_to_codex_request("gpt-6.2-astra", blank_sys.to_string().as_bytes())
                .expect("blank sys fallback");
        assert_eq!(out_blank["instructions"], DEFAULT_CODEX_INSTRUCTIONS);

        // Multi-part systemInstruction joins with \n\n and is emitted as input[0] role="developer"
        // while keeping top-level `instructions` constant so Codex backend prefix caching hits.
        let multi_sys = json!({
            "systemInstruction": {
                "parts": [
                    { "text": "Rule one." },
                    { "text": "Rule two." }
                ]
            },
            "contents": [{ "role": "user", "parts": [{ "text": "hi" }] }]
        });
        let out_multi =
            translate_gemini_to_codex_request("gpt-6.2-astra", multi_sys.to_string().as_bytes())
                .expect("multi sys");
        assert_eq!(out_multi["instructions"], DEFAULT_CODEX_INSTRUCTIONS);
        assert_eq!(
            out_multi["input"][0],
            json!({
                "type": "message",
                "role": "developer",
                "content": [{ "type": "input_text", "text": "Rule one.\n\nRule two." }]
            })
        );
        assert_eq!(out_multi["input"][1]["role"], "user");
        assert_eq!(out_blank["input"].as_array().unwrap().len(), 1);
        assert_eq!(out_blank["input"][0]["role"], "user");
    }

    #[test]
    fn emits_prompt_cache_key_from_conversation_id_or_stable_prefix_hash() {
        let turn1 = json!({
            "systemInstruction": { "parts": [{ "text": "You follow strict workspace rules." }] },
            "contents": [{ "role": "user", "parts": [{ "text": "Fix the bug" }] }]
        });
        let turn2 = json!({
            "systemInstruction": { "parts": [{ "text": "You follow strict workspace rules." }] },
            "contents": [
                { "role": "user", "parts": [{ "text": "Fix the bug" }] },
                { "role": "model", "parts": [{ "text": "Looking at it." }] },
                { "role": "user", "parts": [{ "text": "Now add tests." }] }
            ]
        });
        let other_conv = json!({
            "systemInstruction": { "parts": [{ "text": "You follow strict workspace rules." }] },
            "contents": [{ "role": "user", "parts": [{ "text": "Different task" }] }]
        });

        // Explicit valid conversation ID is preserved verbatim.
        let out_explicit = translate_gemini_to_codex_request_with_cache_key(
            "gpt-6.2-sol",
            turn1.to_string().as_bytes(),
            Some("ecfc4539-0691-4383-9bed-d13c75277276"),
        )
        .expect("explicit key");
        assert_eq!(
            out_explicit["prompt_cache_key"],
            "ecfc4539-0691-4383-9bed-d13c75277276"
        );

        // Missing conversation ID derives a deterministic fallback key identical across Turn 1 & Turn 2,
        // and distinct for a different conversation.
        let out_t1 = translate_gemini_to_codex_request_with_cache_key(
            "gpt-6.2-sol",
            turn1.to_string().as_bytes(),
            None,
        )
        .expect("turn1 fallback");
        let out_t2 = translate_gemini_to_codex_request_with_cache_key(
            "gpt-6.2-sol",
            turn2.to_string().as_bytes(),
            None,
        )
        .expect("turn2 fallback");
        let out_other = translate_gemini_to_codex_request_with_cache_key(
            "gpt-6.2-sol",
            other_conv.to_string().as_bytes(),
            None,
        )
        .expect("other conv fallback");

        let k1 = out_t1["prompt_cache_key"].as_str().expect("fallback str");
        let k2 = out_t2["prompt_cache_key"].as_str().expect("fallback str");
        let k_other = out_other["prompt_cache_key"].as_str().expect("other str");
        assert!(k1.starts_with("agyc-"), "expected agyc- prefix, got {k1}");
        assert_eq!(k1.len(), 37);
        assert_eq!(k1, k2, "Turn 1 and Turn 2 must share fallback cache key");
        assert_ne!(
            k1, k_other,
            "Different conversations must get distinct fallback keys"
        );

        // Unsafe explicit conversation ID falls back cleanly to derived key instead of injecting raw chars.
        let out_unsafe = translate_gemini_to_codex_request_with_cache_key(
            "gpt-6.2-sol",
            turn1.to_string().as_bytes(),
            Some("bad\r\nheader-injection"),
        )
        .expect("unsafe id fallback");
        assert_eq!(out_unsafe["prompt_cache_key"], k1);
    }

    #[test]
    fn translates_images_and_rejects_unsupported_binary_and_file_data_fail_closed() {
        for mime in ["image/png", "image/jpeg", "image/webp", "image/gif"] {
            let body = json!({
                "contents": [{
                    "role": "user",
                    "parts": [
                        { "text": "Describe this" },
                        { "inlineData": { "mimeType": mime, "data": "QUJDRA==" } }
                    ]
                }]
            });
            let out = translate_gemini_to_codex_request("gpt-6.2-sol", body.to_string().as_bytes())
                .expect("supported image");
            assert_eq!(
                out["input"][0]["content"][1],
                json!({
                    "type": "input_image",
                    "image_url": format!("data:{mime};base64,QUJDRA==")
                })
            );
        }

        for bad_mime in ["application/pdf", "video/mp4", "audio/mpeg", "image/heic"] {
            let body = json!({
                "contents": [{
                    "role": "user",
                    "parts": [
                        { "inlineData": { "mimeType": bad_mime, "data": "QUJD" } }
                    ]
                }]
            });
            let err = translate_gemini_to_codex_request("gpt-6.2-sol", body.to_string().as_bytes())
                .expect_err("unsupported MIME must fail");
            assert!(
                matches!(err, ProviderError::InvalidRequest(ref msg) if msg.contains(bad_mime)),
                "unexpected error for {bad_mime}: {err:?}"
            );
        }

        let file_data_body = json!({
            "contents": [{
                "role": "user",
                "parts": [{ "fileData": { "mimeType": "image/png", "fileUri": "gs://bucket/a.png" } }]
            }]
        });
        let err =
            translate_gemini_to_codex_request("gpt-6.2-sol", file_data_body.to_string().as_bytes())
                .expect_err("fileData must fail");
        assert!(matches!(err, ProviderError::InvalidRequest(_)));
    }

    #[test]
    fn skips_thoughts_strips_thought_signatures_and_correlates_fifo_and_long_call_ids() {
        let huge_id = format!("explicit_{}", "x".repeat(90));
        let body = json!({
            "contents": [
                { "role": "user", "parts": [{ "text": "Check files" }] },
                {
                    "role": "model",
                    "parts": [
                        { "text": "secret chain of thought", "thought": true, "thoughtSignature": "sig-thought" },
                        { "text": "Let me read both files.", "thoughtSignature": "sig-text" },
                        { "functionCall": { "name": "read_file", "args": { "path": "a.rs" } }, "thoughtSignature": "sig-fc1" },
                        { "functionCall": { "name": "read_file", "args": { "path": "b.rs" } } },
                        { "functionCall": { "id": huge_id, "name": "search", "args": { "q": "foo" } } }
                    ]
                },
                {
                    "role": "user",
                    "parts": [
                        { "functionResponse": { "name": "read_file", "response": { "content": "A" } } },
                        { "functionResponse": { "name": "read_file", "response": { "content": "B" } } },
                        { "functionResponse": { "id": huge_id, "name": "search", "response": { "hits": 2 } } },
                        { "functionResponse": { "name": "orphan_tool", "response": { "ok": true } } }
                    ]
                }
            ]
        });

        let out = translate_gemini_to_codex_request("gpt-6.2-astra", body.to_string().as_bytes())
            .expect("translate multi-turn tools");
        let serialized = out.to_string();
        assert!(!serialized.contains("secret chain of thought"));
        assert!(!serialized.contains("thoughtSignature"));

        let items = out["input"].as_array().expect("input array");
        assert_eq!(items.len(), 9);
        assert_eq!(items[0]["role"], "user");
        assert_eq!(items[1]["role"], "assistant");
        assert_eq!(items[1]["content"][0]["type"], "output_text");
        assert_eq!(items[1]["content"][0]["text"], "Let me read both files.");

        // FIFO correlation for id-less read_file calls:
        let call1_id = items[2]["call_id"].as_str().expect("call1_id");
        let call2_id = items[3]["call_id"].as_str().expect("call2_id");
        let call3_id = items[4]["call_id"].as_str().expect("call3_id");
        assert_eq!(call1_id, "call_1_2");
        assert_eq!(call2_id, "call_1_3");
        assert!(call3_id.len() <= MAX_CODEX_CALL_ID_LEN);
        assert_ne!(call1_id, call2_id);

        assert_eq!(items[5]["type"], "function_call_output");
        assert_eq!(items[5]["call_id"], call1_id);
        assert_eq!(items[5]["output"], "{\"content\":\"A\"}");

        assert_eq!(items[6]["type"], "function_call_output");
        assert_eq!(items[6]["call_id"], call2_id);
        assert_eq!(items[6]["output"], "{\"content\":\"B\"}");

        // Long explicit ID deterministically normalized <= 64 chars and matched identically.
        assert_eq!(items[7]["type"], "function_call_output");
        assert_eq!(items[7]["call_id"], call3_id);

        // Unmatched functionResponse handled cleanly with bounded call_id.
        let orphan_id = items[8]["call_id"].as_str().expect("orphan_id");
        assert!(orphan_id.len() <= MAX_CODEX_CALL_ID_LEN);
        assert_eq!(items[8]["type"], "function_call_output");
    }

    #[test]
    fn normalizes_gemini_schema_recursively_and_maps_tool_config_and_thinking() {
        let body = json!({
            "contents": [{ "role": "user", "parts": [{ "text": "go" }] }],
            "tools": [{
                "functionDeclarations": [
                    {
                        "name": "edit_file",
                        "description": "Edit a file",
                        "parameters": {
                            "type": "OBJECT",
                            "propertyOrdering": ["path", "lines", "meta"],
                            "properties": {
                                "path": { "type": "STRING", "format": "uri" },
                                "mode": { "type": "STRING", "format": "enum", "enum": ["a", "b"], "nullable": true },
                                "count": { "type": "INTEGER", "format": "int64", "nullable": true },
                                "lines": {
                                    "type": "ARRAY",
                                    "items": { "type": "STRING", "nullable": true }
                                },
                                "meta": {
                                    "type": "OBJECT",
                                    "additionalProperties": { "type": "NUMBER", "format": "double" },
                                    "anyOf": [
                                        { "type": "BOOLEAN" },
                                        { "type": ["STRING", "NULL"], "nullable": true }
                                    ]
                                }
                            },
                            "required": ["path"]
                        }
                    },
                    {
                        "name": "unused_tool",
                        "parameters": { "type": "OBJECT" }
                    }
                ]
            }],
            "toolConfig": {
                "functionCallingConfig": {
                    "mode": "ANY",
                    "allowedFunctionNames": ["edit_file"]
                }
            },
            "generationConfig": {
                "thinkingConfig": {
                    "thinkingLevel": 7
                }
            }
        });

        let out = translate_gemini_to_codex_request("gpt-6.2-astra", body.to_string().as_bytes())
            .expect("translate schema + toolConfig + thinking");
        assert_eq!(out["tool_choice"], "required");
        assert_eq!(
            out["reasoning"],
            json!({ "effort": "xhigh", "summary": "auto" })
        );

        let tools = out["tools"].as_array().expect("tools");
        assert_eq!(tools.len(), 1);
        assert_eq!(tools[0]["name"], "edit_file");
        let params = &tools[0]["parameters"];
        assert_eq!(params["type"], "object");
        assert!(params.get("propertyOrdering").is_none());
        assert_eq!(params["properties"]["path"]["type"], "string");
        assert_eq!(params["properties"]["path"]["format"], "uri");
        assert_eq!(
            params["properties"]["mode"]["type"],
            json!(["string", "null"])
        );
        assert!(params["properties"]["mode"].get("format").is_none());
        assert!(params["properties"]["mode"].get("nullable").is_none());
        assert_eq!(
            params["properties"]["count"]["type"],
            json!(["integer", "null"])
        );
        assert!(params["properties"]["count"].get("format").is_none());
        assert_eq!(params["properties"]["lines"]["type"], "array");
        assert_eq!(
            params["properties"]["lines"]["items"]["type"],
            json!(["string", "null"])
        );
        assert_eq!(
            params["properties"]["meta"]["additionalProperties"]["type"],
            "number"
        );
        assert!(
            params["properties"]["meta"]["additionalProperties"]
                .get("format")
                .is_none()
        );
        assert_eq!(params["properties"]["meta"]["anyOf"][0]["type"], "boolean");
        // No duplicate "null" when array type already included NULL.
        assert_eq!(
            params["properties"]["meta"]["anyOf"][1]["type"],
            json!(["string", "null"])
        );
    }

    #[test]
    fn maps_all_thinking_levels_and_budget_buckets() {
        let cases = [
            (json!({ "thinkingLevel": "LOW" }), Some("low")),
            (json!({ "thinkingLevel": "MEDIUM" }), Some("medium")),
            (json!({ "thinkingLevel": "HIGH" }), Some("high")),
            (json!({ "thinkingLevel": "XHIGH" }), Some("xhigh")),
            (json!({ "thinkingLevel": 1 }), Some("low")),
            (json!({ "thinkingLevel": 2 }), Some("medium")),
            (json!({ "thinkingLevel": 3 }), Some("high")),
            (json!({ "thinkingLevel": 7 }), Some("xhigh")),
            (json!({ "thinkingBudget": 0 }), None),
            (json!({ "thinkingBudget": 1024 }), Some("low")),
            (json!({ "thinkingBudget": 2048 }), Some("low")),
            (json!({ "thinkingBudget": 2049 }), Some("medium")),
            (json!({ "thinkingBudget": 8192 }), Some("medium")),
            (json!({ "thinkingBudget": 8193 }), Some("high")),
        ];
        for (thinking_cfg, expected) in cases {
            let body = json!({
                "contents": [{ "role": "user", "parts": [{ "text": "hi" }] }],
                "generationConfig": { "thinkingConfig": thinking_cfg }
            });
            let out = translate_gemini_to_codex_request("gpt-6.2-sol", body.to_string().as_bytes())
                .expect("thinking mapping");
            match expected {
                Some(effort) => {
                    assert_eq!(out["reasoning"]["effort"], effort);
                    assert_eq!(out["reasoning"]["summary"], "auto");
                }
                None => assert!(out.get("reasoning").is_none()),
            }
        }
    }

    #[test]
    fn translates_codex_sse_split_utf8_reasoning_text_tools_and_usage() {
        let stream = concat!(
            "\u{feff}: keepalive comment\r\n",
            "event: response.reasoning_summary_text.delta\r\n",
            "data: {\"type\":\"response.reasoning_summary_text.delta\",\"delta\":\"Thinking 🌍 \"}\r\n\r\n",
            "data: {\"type\":\"response.output_text.delta\",\"delta\":\"Hello 世界\"}\n\n",
            "data: {\"type\":\"response.output_item.added\",\"output_index\":1,\"item\":{\"id\":\"fc_1\",\"type\":\"function_call\",\"call_id\":\"call_abc\",\"name\":\"read_file\",\"arguments\":\"\"}}\n\n",
            "data: {\"type\":\"response.function_call_arguments.delta\",\"item_id\":\"fc_1\",\"output_index\":1,\"delta\":\"{\\\"path\\\":\"}\n\n",
            "data: {\"type\":\"response.function_call_arguments.delta\",\"item_id\":\"fc_1\",\"output_index\":1,\"delta\":\"\\\"main.rs\\\"}\"}\n\n",
            "data: {\"type\":\"response.output_item.done\",\"output_index\":1,\"item\":{\"id\":\"fc_1\",\"type\":\"function_call\",\"call_id\":\"call_abc\",\"name\":\"read_file\"}}\n\n",
            "data: {\"type\":\"response.completed\",\"response\":{\"usage\":{\"input_tokens\":1000,\"input_tokens_details\":{\"cached_tokens\":600},\"output_tokens\":250,\"output_tokens_details\":{\"reasoning_tokens\":80},\"total_tokens\":1250}}}\n\n",
            "data: [DONE]\n\n"
        )
        .as_bytes();

        let mut translator = CodexSseTranslator::new();
        let mut chunks = Vec::new();
        // Feed 1 byte at a time to stress CRLF splitting and multi-byte UTF-8 boundaries.
        for byte in stream {
            chunks.extend(
                translator
                    .push_chunk(std::slice::from_ref(byte))
                    .expect("byte chunk"),
            );
        }
        chunks.extend(translator.finish().expect("finish"));

        assert_eq!(chunks.len(), 4);
        assert_eq!(
            chunks[0]["candidates"][0]["content"]["parts"][0],
            json!({ "text": "Thinking 🌍 ", "thought": true })
        );
        assert_eq!(
            chunks[1]["candidates"][0]["content"]["parts"][0],
            json!({ "text": "Hello 世界" })
        );
        assert_eq!(
            chunks[2]["candidates"][0]["content"]["parts"][0],
            json!({
                "functionCall": {
                    "id": "call_abc",
                    "name": "read_file",
                    "args": { "path": "main.rs" }
                }
            })
        );
        assert_eq!(chunks[3]["candidates"][0]["finishReason"], "STOP");
        assert_eq!(
            chunks[3]["usageMetadata"],
            json!({
                "promptTokenCount": 1000,
                "cachedContentTokenCount": 600,
                "candidatesTokenCount": 250,
                "thoughtsTokenCount": 80,
                "totalTokenCount": 1250
            })
        );

        // Verify unary aggregator merges these exact chunks cleanly.
        let mut agg = UnaryGeminiAggregator::new();
        for c in chunks {
            agg.push_chunk(c).expect("agg push");
        }
        let (status, unary) = agg.finish().expect("agg finish");
        assert_eq!(status, 200);
        let parts = unary["candidates"][0]["content"]["parts"]
            .as_array()
            .expect("parts");
        assert_eq!(parts.len(), 3);
        assert_eq!(parts[0], json!({ "text": "Thinking 🌍 ", "thought": true }));
        assert_eq!(parts[1], json!({ "text": "Hello 世界" }));
        assert_eq!(parts[2]["functionCall"]["name"], "read_file");
        assert_eq!(unary["candidates"][0]["finishReason"], "STOP");
        assert_eq!(unary["usageMetadata"]["cachedContentTokenCount"], 600);
    }

    #[test]
    fn handles_empty_tool_args_fallback_item_args_max_tokens_and_corrupt_json() {
        let stream = concat!(
            "data: {\"type\":\"response.output_item.done\",\"output_index\":0,\"item\":{\"id\":\"fc_empty\",\"type\":\"function_call\",\"call_id\":\"c1\",\"name\":\"ping\",\"arguments\":\"\"}}\n\n",
            "data: {\"type\":\"response.output_item.done\",\"output_index\":1,\"item\":{\"id\":\"fc_full\",\"type\":\"function_call\",\"call_id\":\"c2\",\"name\":\"echo\",\"arguments\":\"{\\\"msg\\\":\\\"hi\\\"}\"}}\n\n",
            "data: {\"type\":\"response.incomplete\",\"response\":{\"incomplete_details\":{\"reason\":\"max_output_tokens\"},\"usage\":{\"input_tokens\":50,\"output_tokens\":100}}}\n\n"
        );
        let mut tr = CodexSseTranslator::new();
        let mut chunks = tr.push_chunk(stream.as_bytes()).expect("push");
        chunks.extend(tr.finish().expect("finish"));
        assert_eq!(chunks.len(), 3);
        assert_eq!(
            chunks[0]["candidates"][0]["content"]["parts"][0]["functionCall"]["args"],
            json!({})
        );
        assert_eq!(
            chunks[1]["candidates"][0]["content"]["parts"][0]["functionCall"]["args"],
            json!({ "msg": "hi" })
        );
        assert_eq!(chunks[2]["candidates"][0]["finishReason"], "MAX_TOKENS");

        // Corrupt JSON arguments fail cleanly.
        let mut bad_tr = CodexSseTranslator::new();
        let err = bad_tr
            .push_chunk(
                b"data: {\"type\":\"response.output_item.done\",\"item\":{\"type\":\"function_call\",\"call_id\":\"c3\",\"name\":\"broken\",\"arguments\":\"{not-json\"}}\n\n",
            )
            .expect_err("corrupt JSON args must fail");
        assert!(matches!(err, ProviderError::Upstream(_)));
    }

    #[test]
    fn enforces_bounded_sse_event_and_tool_args_memory_caps() {
        // 1. Oversized SSE line fails cleanly without OOM.
        let mut tr = CodexSseTranslator::with_limits(64, 64);
        let err = tr
            .push_chunk(format!("data: {}\n\n", "a".repeat(128)).as_bytes())
            .expect_err("oversized SSE line must fail");
        assert!(matches!(err, ProviderError::Upstream(_)));

        // 2. Oversized comment line is ignored and does not fail valid following event.
        let mut tr_comment = CodexSseTranslator::with_limits(128, 64);
        let chunks = tr_comment
            .push_chunk(
                format!(
                    ":{}\ndata: {{\"type\":\"response.output_text.delta\",\"delta\":\"ok\"}}\n\n",
                    "x".repeat(512)
                )
                .as_bytes(),
            )
            .expect("oversized comment ignored");
        assert_eq!(chunks.len(), 1);
        assert_eq!(
            chunks[0]["candidates"][0]["content"]["parts"][0]["text"],
            "ok"
        );

        // 3. Oversized function_call_arguments.delta fails cleanly without OOM.
        let mut tr_args = CodexSseTranslator::with_limits(4096, 32);
        tr_args
            .push_chunk(
                b"data: {\"type\":\"response.output_item.added\",\"output_index\":0,\"item\":{\"id\":\"fc_big\",\"type\":\"function_call\",\"call_id\":\"c1\",\"name\":\"big\"}}\n\n",
            )
            .expect("added");
        let err = tr_args
            .push_chunk(
                format!(
                    "data: {{\"type\":\"response.function_call_arguments.delta\",\"item_id\":\"fc_big\",\"delta\":\"{}\"}}\n\n",
                    "z".repeat(40)
                )
                .as_bytes(),
            )
            .expect_err("oversized tool args must fail");
        assert!(matches!(err, ProviderError::Upstream(_)));
    }

    #[test]
    fn sanitizes_codex_errors_with_zero_token_or_raw_body_leakage() {
        let secret_body = br#"{"error":{"code":"rate_limit_exceeded","message":"Bearer eyJhbGciOiJSUzI1NiJ9.SUPER_SECRET_TOKEN"}}"#;
        let (status, err_json) = sanitize_codex_error_json(429, secret_body);
        assert_eq!(status, 429);
        assert_eq!(err_json["error"]["code"], 429);
        assert_eq!(
            err_json["error"]["message"],
            "ChatGPT Codex quota limit reached"
        );
        assert!(!err_json.to_string().contains("SUPER_SECRET_TOKEN"));
        assert!(!err_json.to_string().contains("Bearer"));

        let mut tr = CodexSseTranslator::new();
        let frames = tr
            .push_chunk(
                b"data: {\"type\":\"response.failed\",\"response\":{\"error\":{\"code\":\"insufficient_quota\",\"message\":\"secret-refresh-tok-xyz\"}}}\n\n",
            )
            .expect("failed event translates to sanitized error frame");
        assert_eq!(frames.len(), 1);
        assert_eq!(frames[0]["error"]["code"], 429);
        assert!(!frames[0].to_string().contains("secret-refresh-tok-xyz"));
    }

    #[test]
    fn translates_function_response_in_model_turn_explicit_id_and_fifo_fallback() {
        // 1. Captured Antigravity pattern: standalone functionResponse inside role: "model"
        //    with explicit call ID matching an earlier role: "model" functionCall.
        let explicit_id = "call_12658be792e746a4a55dc9fd30d2e2f7";
        let body_explicit = json!({
            "contents": [
                { "role": "user", "parts": [{ "text": "Read Cargo.toml" }] },
                {
                    "role": "model",
                    "parts": [{
                        "functionCall": {
                            "id": explicit_id,
                            "name": "read_file",
                            "args": { "path": "Cargo.toml" }
                        }
                    }]
                },
                {
                    "role": "model",
                    "parts": [{
                        "functionResponse": {
                            "id": explicit_id,
                            "name": "read_file",
                            "response": { "content": "[workspace]\n" }
                        }
                    }]
                }
            ]
        });

        let out_explicit =
            translate_gemini_to_codex_request("gpt-6.2-sol", body_explicit.to_string().as_bytes())
                .expect("model functionResponse explicit id");
        let items_explicit = out_explicit["input"].as_array().expect("input array");
        assert_eq!(
            items_explicit.len(),
            3,
            "expected user + function_call + function_call_output, got {items_explicit:?}"
        );
        assert_eq!(items_explicit[0]["role"], "user");
        assert_eq!(
            items_explicit[1],
            json!({
                "type": "function_call",
                "call_id": explicit_id,
                "name": "read_file",
                "arguments": "{\"path\":\"Cargo.toml\"}"
            })
        );
        assert_eq!(
            items_explicit[2],
            json!({
                "type": "function_call_output",
                "call_id": explicit_id,
                "output": "{\"content\":\"[workspace]\\n\"}"
            })
        );

        // 2. Snake-case function_response inside role: "model" without explicit id correlates via FIFO.
        let body_fifo = json!({
            "contents": [
                { "role": "user", "parts": [{ "text": "Check version" }] },
                {
                    "role": "model",
                    "parts": [{
                        "function_call": {
                            "name": "read_file",
                            "args": { "path": "Cargo.toml" }
                        }
                    }]
                },
                {
                    "role": "model",
                    "parts": [{
                        "function_response": {
                            "name": "read_file",
                            "response": "0.1.1-dev.5"
                        }
                    }]
                }
            ]
        });

        let out_fifo =
            translate_gemini_to_codex_request("gpt-6.2-sol", body_fifo.to_string().as_bytes())
                .expect("model function_response fifo");
        let items_fifo = out_fifo["input"].as_array().expect("input array");
        assert_eq!(items_fifo.len(), 3);
        let call_id = items_fifo[1]["call_id"]
            .as_str()
            .expect("synthetic call_id");
        assert_eq!(items_fifo[1]["type"], "function_call");
        assert_eq!(
            items_fifo[2],
            json!({
                "type": "function_call_output",
                "call_id": call_id,
                "output": "0.1.1-dev.5"
            })
        );
    }

    #[test]
    fn translates_mixed_model_and_assistant_turns_preserving_order_and_stripping_thoughts() {
        for role in ["model", "assistant"] {
            let body = json!({
                "contents": [
                    { "role": "user", "parts": [{ "text": "Inspect file" }] },
                    {
                        "role": role,
                        "parts": [
                            { "text": "internal thought", "thought": true, "thoughtSignature": "sig-thought" },
                            { "text": "Checking now.", "thoughtSignature": "sig-before" },
                            {
                                "functionCall": {
                                    "name": "read_file",
                                    "args": { "path": "lib.rs" }
                                },
                                "thoughtSignature": "sig-call"
                            },
                            {
                                "functionResponse": {
                                    "name": "read_file",
                                    "response": { "ok": true }
                                },
                                "thoughtSignature": "sig-resp"
                            },
                            { "text": "All clear." }
                        ]
                    }
                ]
            });

            let out = translate_gemini_to_codex_request("gpt-6.2-sol", body.to_string().as_bytes())
                .expect("mixed model/assistant turn");
            let serialized = out.to_string();
            assert!(!serialized.contains("internal thought"));
            assert!(!serialized.contains("thoughtSignature"));

            let items = out["input"].as_array().expect("input array");
            assert_eq!(items.len(), 5, "role={role} items={items:?}");
            assert_eq!(items[0]["role"], "user");
            assert_eq!(
                items[1],
                json!({
                    "type": "message",
                    "role": "assistant",
                    "content": [{ "type": "output_text", "text": "Checking now." }]
                })
            );
            let call_id = items[2]["call_id"].as_str().expect("call_id");
            assert_eq!(items[2]["type"], "function_call");
            assert_eq!(items[2]["name"], "read_file");
            assert_eq!(
                items[3],
                json!({
                    "type": "function_call_output",
                    "call_id": call_id,
                    "output": "{\"ok\":true}"
                })
            );
            assert_eq!(
                items[4],
                json!({
                    "type": "message",
                    "role": "assistant",
                    "content": [{ "type": "output_text", "text": "All clear." }]
                })
            );
        }
    }

    #[test]
    fn extracts_safe_codex_error_fields_and_rejects_secrets_and_malformed_slugs() {
        // 1. Valid top-level and nested response.error slugs are extracted; message is ignored.
        let top_level = br#"{"error":{"code":"invalid_request_error","type":"invalid_request","message":"Bearer eyJSECRET_TOKEN_HERE"}}"#;
        assert_eq!(
            extract_safe_codex_error_fields(top_level),
            (
                Some("invalid_request_error".to_string()),
                Some("invalid_request".to_string())
            )
        );

        let nested = br#"{"type":"response.failed","response":{"error":{"code":"rate_limit_exceeded","type":"tokens-64_ok","message":"sk-secret"}}}"#;
        assert_eq!(
            extract_safe_codex_error_fields(nested),
            (
                Some("rate_limit_exceeded".to_string()),
                Some("tokens-64_ok".to_string())
            )
        );

        // 2. Boundary: 64 ASCII slug chars accepted, 65 rejected.
        let exact_64 = "a".repeat(64);
        let over_64 = "a".repeat(65);
        assert_eq!(
            sanitize_codex_error_slug(Some(&exact_64)),
            Some(exact_64.clone())
        );
        assert_eq!(sanitize_codex_error_slug(Some(&over_64)), None);

        // 3. Rejects secret prefixes, whitespace/newlines, punctuation, and empty strings.
        for unsafe_val in [
            "",
            "   ",
            "AIzaSyFakeSecretPrefix1234567890",
            "sk-proj-secret123",
            "eyJhbGciOiJSUzI1NiJ9",
            "ghp_secret1234567890",
            "bad\ncode",
            "Bearer token",
            "err.with.dot",
            "err/slash",
        ] {
            assert_eq!(
                sanitize_codex_error_slug(Some(unsafe_val)),
                None,
                "expected slug rejection for {unsafe_val:?}"
            );
        }

        // 4. Malformed JSON or missing error object returns (None, None).
        assert_eq!(extract_safe_codex_error_fields(b"not-json"), (None, None));
        assert_eq!(
            extract_safe_codex_error_fields(br#"{"message":"plain"}"#),
            (None, None)
        );
    }

    #[test]
    fn propagates_response_id_into_sse_completed_and_unary_responses() {
        let stream = concat!(
            "data: {\"type\":\"response.output_text.delta\",\"delta\":\"Hi\"}\n\n",
            "data: {\"type\":\"response.completed\",\"response\":{\"id\":\"resp_codex_12345\",\"status\":\"completed\",\"usage\":{\"input_tokens\":100,\"input_tokens_details\":{\"cached_tokens\":40},\"output_tokens\":20}}}\n\n"
        );
        let mut tr = CodexSseTranslator::new();
        let mut chunks = tr.push_chunk(stream.as_bytes()).expect("push");
        chunks.extend(tr.finish().expect("finish"));
        assert_eq!(chunks.len(), 2);
        assert_eq!(chunks[1]["responseId"], "resp_codex_12345");

        let mut agg = UnaryGeminiAggregator::new();
        for chunk in chunks {
            agg.push_chunk(chunk).expect("agg push");
        }
        let (status, unary) = agg.finish().expect("agg finish");
        assert_eq!(status, 200);
        assert_eq!(unary["responseId"], "resp_codex_12345");
    }

    #[test]
    fn maps_prefixed_thinking_levels_defaults_summary_auto_and_include_encrypted_reasoning() {
        let effort_cases = [
            (
                json!({ "thinkingLevel": "THINKING_LEVEL_MINIMAL" }),
                Some("low"),
            ),
            (
                json!({ "thinkingLevel": "THINKING_LEVEL_LOW" }),
                Some("low"),
            ),
            (
                json!({ "thinkingLevel": "THINKING_LEVEL_MEDIUM" }),
                Some("medium"),
            ),
            (
                json!({ "thinkingLevel": "THINKING_LEVEL_HIGH" }),
                Some("high"),
            ),
            (
                json!({ "thinkingLevel": "THINKING_LEVEL_XHIGH" }),
                Some("xhigh"),
            ),
            (
                json!({ "thinkingLevel": "THINKING_LEVEL_EXTRA_HIGH" }),
                Some("xhigh"),
            ),
            (
                json!({ "thinkingLevel": "THINKING_LEVEL_VERY_HIGH" }),
                Some("xhigh"),
            ),
            (
                json!({ "thinkingLevel": "THINKING_LEVEL_EXTRA" }),
                Some("xhigh"),
            ),
            (
                json!({ "thinkingLevel": "THINKING_LEVEL_MAX" }),
                Some("xhigh"),
            ),
            (json!({ "thinkingLevel": "EXTRA_HIGH" }), Some("xhigh")),
            (json!({ "thinkingLevel": "VERY_HIGH" }), Some("xhigh")),
            (
                json!({ "thinkingLevel": "THINKING_LEVEL_MODEL_DEFAULT" }),
                None,
            ),
            (
                json!({ "thinkingLevel": "THINKING_LEVEL_UNSPECIFIED" }),
                None,
            ),
            (json!({ "thinkingLevel": 0 }), None),
            (json!({ "includeThoughts": true }), None),
        ];

        for (thinking_cfg, expected_effort) in effort_cases {
            let body = json!({
                "contents": [{ "role": "user", "parts": [{ "text": "hi" }] }],
                "generationConfig": { "thinkingConfig": thinking_cfg }
            });
            let out =
                translate_gemini_to_codex_request("gpt-6.2-astra", body.to_string().as_bytes())
                    .expect("thinking config translation");
            assert_eq!(
                out["reasoning"]["summary"], "auto",
                "expected summary=auto for {thinking_cfg}"
            );
            match expected_effort {
                Some(effort) => assert_eq!(
                    out["reasoning"]["effort"], effort,
                    "expected effort={effort} for {thinking_cfg}"
                ),
                None => assert!(
                    out["reasoning"].get("effort").is_none(),
                    "expected omitted effort for {thinking_cfg}, got {:?}",
                    out["reasoning"]
                ),
            }
            assert_eq!(
                out["include"],
                json!(["reasoning.encrypted_content"]),
                "expected include encrypted_content for {thinking_cfg}"
            );
        }

        // Absent thinkingConfig still requests summary=auto + include encrypted_content by default.
        let bare = json!({ "contents": [{ "role": "user", "parts": [{ "text": "hi" }] }] });
        let out_bare =
            translate_gemini_to_codex_request("gpt-6.2-sol", bare.to_string().as_bytes())
                .expect("bare request");
        assert_eq!(out_bare["reasoning"], json!({ "summary": "auto" }));
        assert_eq!(out_bare["include"], json!(["reasoning.encrypted_content"]));

        // Explicitly disabled thinking omits both reasoning and include.
        for disabled_cfg in [
            json!({ "thinkingLevel": "NONE" }),
            json!({ "thinkingLevel": "OFF" }),
            json!({ "thinkingLevel": "THINKING_LEVEL_NONE" }),
            json!({ "thinkingLevel": "THINKING_LEVEL_OFF" }),
            json!({ "thinkingBudget": 0 }),
            json!({ "thinkingBudget": -1 }),
        ] {
            let body = json!({
                "contents": [{ "role": "user", "parts": [{ "text": "hi" }] }],
                "generationConfig": { "thinkingConfig": disabled_cfg }
            });
            let out = translate_gemini_to_codex_request("gpt-6.2-sol", body.to_string().as_bytes())
                .expect("disabled thinking");
            assert!(
                out.get("reasoning").is_none(),
                "expected no reasoning for {disabled_cfg}, got {:?}",
                out.get("reasoning")
            );
            assert!(
                out.get("include").is_none(),
                "expected no include for {disabled_cfg}, got {:?}",
                out.get("include")
            );
        }

        // includeThoughts: false keeps explicit effort + encrypted reasoning but omits summary.
        let no_summary = json!({
            "contents": [{ "role": "user", "parts": [{ "text": "hi" }] }],
            "generationConfig": {
                "thinkingConfig": { "thinkingLevel": "THINKING_LEVEL_HIGH", "includeThoughts": false }
            }
        });
        let out_no_summary =
            translate_gemini_to_codex_request("gpt-6.2-astra", no_summary.to_string().as_bytes())
                .expect("no summary");
        assert_eq!(out_no_summary["reasoning"], json!({ "effort": "high" }));
        assert_eq!(
            out_no_summary["include"],
            json!(["reasoning.encrypted_content"])
        );
    }

    #[test]
    fn separates_multiple_reasoning_summary_parts_with_paragraph_breaks() {
        let stream = concat!(
            "data: {\"type\":\"response.reasoning_summary_part.added\",\"item_id\":\"rs_1\",\"output_index\":0,\"summary_index\":0,\"part\":{\"type\":\"summary_text\",\"text\":\"\"}}\n\n",
            "data: {\"type\":\"response.reasoning_summary_text.delta\",\"item_id\":\"rs_1\",\"output_index\":0,\"summary_index\":0,\"delta\":\"**Step 1** Checking files.\"}\n\n",
            "data: {\"type\":\"response.reasoning_summary_part.done\",\"item_id\":\"rs_1\",\"output_index\":0,\"summary_index\":0,\"part\":{\"type\":\"summary_text\",\"text\":\"**Step 1** Checking files.\"}}\n\n",
            "data: {\"type\":\"response.reasoning_summary_part.added\",\"item_id\":\"rs_1\",\"output_index\":0,\"summary_index\":1,\"part\":{\"type\":\"summary_text\",\"text\":\"\"}}\n\n",
            "data: {\"type\":\"response.reasoning_summary_text.delta\",\"item_id\":\"rs_1\",\"output_index\":0,\"summary_index\":1,\"delta\":\"**Step 2** Applying fix.\\n\"}\n\n",
            "data: {\"type\":\"response.reasoning_summary_part.done\",\"item_id\":\"rs_1\",\"output_index\":0,\"summary_index\":1,\"part\":{\"type\":\"summary_text\",\"text\":\"**Step 2** Applying fix.\\n\"}}\n\n",
            "data: {\"type\":\"response.reasoning_summary_part.added\",\"item_id\":\"rs_1\",\"output_index\":0,\"summary_index\":2,\"part\":{\"type\":\"summary_text\",\"text\":\"\"}}\n\n",
            "data: {\"type\":\"response.reasoning_summary_text.delta\",\"item_id\":\"rs_1\",\"output_index\":0,\"summary_index\":2,\"delta\":\"**Step 3** Running tests.\"}\n\n",
            "data: {\"type\":\"response.reasoning_summary_part.done\",\"item_id\":\"rs_1\",\"output_index\":0,\"summary_index\":2,\"part\":{\"type\":\"summary_text\",\"text\":\"**Step 3** Running tests.\"}}\n\n"
        );
        let mut tr = CodexSseTranslator::new();
        let mut chunks = tr.push_chunk(stream.as_bytes()).expect("push");
        chunks.extend(tr.finish().expect("finish"));

        let mut agg = UnaryGeminiAggregator::new();
        for c in chunks {
            agg.push_chunk(c).expect("agg push");
        }
        let (_, unary) = agg.finish().expect("agg finish");
        let parts = unary["candidates"][0]["content"]["parts"]
            .as_array()
            .expect("parts");
        assert_eq!(parts.len(), 1);
        assert_eq!(parts[0]["thought"], true);
        assert_eq!(
            parts[0]["text"],
            "**Step 1** Checking files.\n\n**Step 2** Applying fix.\n\n**Step 3** Running tests."
        );
    }

    #[test]
    fn emits_final_reasoning_summary_without_deltas_or_encrypted_content() {
        let sse = concat!(
            "data: {\"type\":\"response.output_item.added\",\"output_index\":0,\"item\":{\"id\":\"rs_final\",\"type\":\"reasoning\",\"summary\":[]}}\n\n",
            "data: {\"type\":\"response.output_item.done\",\"output_index\":0,\"item\":{\"id\":\"rs_final\",\"type\":\"reasoning\",\"summary\":[{\"type\":\"summary_text\",\"text\":\"Planning edit.\"}]}}\n\n"
        );
        let mut tr = CodexSseTranslator::new();
        let chunks = tr.push_chunk(sse.as_bytes()).expect("push");

        assert_eq!(
            chunks,
            vec![json!({
                "candidates": [{
                    "content": {
                        "role": "model",
                        "parts": [{ "text": "Planning edit.", "thought": true }]
                    },
                    "index": 0
                }]
            })],
            "final reasoning summary should emit a Gemini thought without a signature"
        );
    }

    #[test]
    fn final_reasoning_summary_deduplicates_by_item_and_precedes_its_signature() {
        let sse = concat!(
            "data: {\"type\":\"response.output_item.added\",\"output_index\":0,\"item\":{\"id\":\"rs_streamed\",\"type\":\"reasoning\",\"summary\":[]}}\n\n",
            "data: {\"type\":\"response.reasoning_summary_text.delta\",\"item_id\":\"rs_streamed\",\"output_index\":0,\"delta\":\"Streamed.\"}\n\n",
            "data: {\"type\":\"response.output_item.added\",\"output_index\":1,\"item\":{\"id\":\"rs_final\",\"type\":\"reasoning\",\"summary\":[]}}\n\n",
            "data: {\"type\":\"response.output_item.done\",\"output_index\":1,\"item\":{\"id\":\"rs_final\",\"type\":\"reasoning\",\"summary\":[{\"type\":\"summary_text\",\"text\":\"First.\\n\"},{\"type\":\"summary_text\",\"text\":\" \"},{\"type\":\"other\",\"text\":\"Ignore.\"},{\"type\":\"summary_text\",\"text\":\"Second.\"}],\"encrypted_content\":\"encrypted_fixture\"}}\n\n",
            "data: {\"type\":\"response.output_item.done\",\"output_index\":0,\"item\":{\"id\":\"rs_streamed\",\"type\":\"reasoning\",\"summary\":[{\"type\":\"summary_text\",\"text\":\"Streamed.\"}]}}\n\n"
        );
        let mut tr = CodexSseTranslator::new();
        let chunks = tr.push_chunk(sse.as_bytes()).expect("push");
        let parts: Vec<_> = chunks
            .iter()
            .map(|chunk| &chunk["candidates"][0]["content"]["parts"][0])
            .collect();
        assert_eq!(parts.len(), 3);
        assert_eq!(*parts[0], json!({ "text": "Streamed.", "thought": true }));
        assert_eq!(
            *parts[1],
            json!({ "text": "First.\n\nSecond.", "thought": true })
        );
        assert_eq!(parts[2]["text"], "");
        assert_eq!(parts[2]["thought"], true);
        assert!(parts[2]["thoughtSignature"].is_string());
    }

    #[test]
    fn deduplicates_multiple_concurrently_streamed_reasoning_items() {
        let sse = concat!(
            "data: {\"type\":\"response.output_item.added\",\"output_index\":0,\"item\":{\"id\":\"rs_0\",\"type\":\"reasoning\",\"summary\":[]}}\n\n",
            "data: {\"type\":\"response.reasoning_summary_text.delta\",\"item_id\":\"rs_0\",\"output_index\":0,\"delta\":\"First reasoning.\"}\n\n",
            "data: {\"type\":\"response.output_item.added\",\"output_index\":1,\"item\":{\"id\":\"rs_1\",\"type\":\"reasoning\",\"summary\":[]}}\n\n",
            "data: {\"type\":\"response.reasoning_summary_text.delta\",\"item_id\":\"rs_1\",\"output_index\":1,\"delta\":\"Second reasoning.\"}\n\n",
            "data: {\"type\":\"response.output_item.done\",\"output_index\":1,\"item\":{\"id\":\"rs_1\",\"type\":\"reasoning\",\"summary\":[{\"type\":\"summary_text\",\"text\":\"Second reasoning.\"}]}}\n\n",
            "data: {\"type\":\"response.output_item.done\",\"output_index\":0,\"item\":{\"id\":\"rs_0\",\"type\":\"reasoning\",\"summary\":[{\"type\":\"summary_text\",\"text\":\"First reasoning.\"}]}}\n\n"
        );
        let mut tr = CodexSseTranslator::new();
        let chunks = tr.push_chunk(sse.as_bytes()).expect("push");
        let parts: Vec<_> = chunks
            .iter()
            .map(|chunk| &chunk["candidates"][0]["content"]["parts"][0])
            .collect();
        assert_eq!(parts.len(), 2, "neither streamed item should be duplicated");
        assert_eq!(
            *parts[0],
            json!({ "text": "First reasoning.", "thought": true })
        );
        assert_eq!(
            *parts[1],
            json!({ "text": "Second reasoning.", "thought": true })
        );
    }

    #[test]
    fn empty_reasoning_item_id_falls_back_to_output_index_for_dedup() {
        let sse = concat!(
            "data: {\"type\":\"response.reasoning_summary_text.delta\",\"item_id\":\"\",\"output_index\":0,\"delta\":\"Streamed without id.\"}\n\n",
            "data: {\"type\":\"response.output_item.done\",\"output_index\":0,\"item\":{\"id\":\"rs_1\",\"type\":\"reasoning\",\"summary\":[{\"type\":\"summary_text\",\"text\":\"Streamed without id.\"}]}}\n\n"
        );
        let mut tr = CodexSseTranslator::new();
        let chunks = tr.push_chunk(sse.as_bytes()).expect("push");
        let parts: Vec<_> = chunks
            .iter()
            .map(|chunk| &chunk["candidates"][0]["content"]["parts"][0])
            .collect();
        assert_eq!(parts.len(), 1, "empty item_id should match by output_index");
        assert_eq!(
            *parts[0],
            json!({ "text": "Streamed without id.", "thought": true })
        );
    }

    #[test]
    fn unary_aggregator_merges_fallback_reasoning_summary_and_signature_and_handles_empty_summary()
    {
        // Case 1: Fallback summary + encrypted_content merges into one thought part in unary mode.
        let sse_with_summary = concat!(
            "data: {\"type\":\"response.output_item.done\",\"output_index\":0,\"item\":{\"id\":\"rs_fallback\",\"type\":\"reasoning\",\"summary\":[{\"type\":\"summary_text\",\"text\":\"Final only.\"}],\"encrypted_content\":\"gAAAAAB_fallback_sig\"}}\n\n",
            "data: {\"type\":\"response.output_text.delta\",\"delta\":\"Answer.\"}\n\n",
            "data: {\"type\":\"response.completed\",\"response\":{\"usage\":{\"input_tokens\":10,\"output_tokens\":5}}}\n\n"
        );
        let mut tr = CodexSseTranslator::new();
        let mut chunks = tr.push_chunk(sse_with_summary.as_bytes()).expect("push");
        chunks.extend(tr.finish().expect("finish"));

        let mut agg = UnaryGeminiAggregator::new();
        for c in chunks {
            agg.push_chunk(c).expect("agg push");
        }
        let (status, unary) = agg.finish().expect("agg finish");
        assert_eq!(status, 200);
        let parts = unary["candidates"][0]["content"]["parts"]
            .as_array()
            .expect("parts");
        assert_eq!(parts.len(), 2);
        assert_eq!(parts[0]["text"], "Final only.");
        assert_eq!(parts[0]["thought"], true);
        assert!(parts[0]["thoughtSignature"].is_string());
        assert_eq!(parts[1], json!({ "text": "Answer." }));

        // Case 2: includeThoughts: false -> summary: [] + encrypted_content emits signature-only thought part.
        let sse_no_summary = concat!(
            "data: {\"type\":\"response.output_item.done\",\"output_index\":0,\"item\":{\"id\":\"rs_hidden\",\"type\":\"reasoning\",\"summary\":[],\"encrypted_content\":\"gAAAAAB_hidden_sig\"}}\n\n",
            "data: {\"type\":\"response.output_text.delta\",\"delta\":\"Visible.\"}\n\n",
            "data: {\"type\":\"response.completed\",\"response\":{\"usage\":{\"input_tokens\":10,\"output_tokens\":5}}}\n\n"
        );
        let mut tr2 = CodexSseTranslator::new();
        let mut chunks2 = tr2.push_chunk(sse_no_summary.as_bytes()).expect("push");
        chunks2.extend(tr2.finish().expect("finish"));
        // Chunk 0: signature-only thought, Chunk 1: text delta, Chunk 2: completed.
        assert_eq!(chunks2.len(), 3);
        let sig_part = &chunks2[0]["candidates"][0]["content"]["parts"][0];
        assert_eq!(sig_part["text"], "");
        assert_eq!(sig_part["thought"], true);
        assert!(sig_part["thoughtSignature"].is_string());
    }

    #[test]
    fn round_trips_encrypted_reasoning_signature_in_sse_unary_and_multi_turn_input() {
        let sse = concat!(
            "data: {\"type\":\"response.reasoning_summary_text.delta\",\"delta\":\"Planning edit.\"}\n\n",
            "data: {\"type\":\"response.output_item.done\",\"output_index\":0,\"item\":{\"id\":\"rs_turn1_abc\",\"type\":\"reasoning\",\"summary\":[{\"type\":\"summary_text\",\"text\":\"Planning edit.\"}],\"encrypted_content\":\"gAAAAAB_encrypted_blob_turn1\"}}\n\n",
            "data: {\"type\":\"response.output_text.delta\",\"delta\":\"Done!\"}\n\n",
            "data: {\"type\":\"response.completed\",\"response\":{\"usage\":{\"input_tokens\":100,\"output_tokens\":20}}}\n\n"
        );

        let mut tr = CodexSseTranslator::new();
        let mut chunks = tr.push_chunk(sse.as_bytes()).expect("push");
        chunks.extend(tr.finish().expect("finish"));

        // Chunk 0: thought summary delta; Chunk 1: thoughtSignature part; Chunk 2: text delta; Chunk 3: completed.
        assert_eq!(chunks.len(), 4);
        let sig_part = &chunks[1]["candidates"][0]["content"]["parts"][0];
        assert_eq!(sig_part["text"], "");
        assert_eq!(sig_part["thought"], true);
        let sig = sig_part["thoughtSignature"]
            .as_str()
            .expect("codex thoughtSignature")
            .to_string();
        // Emitted signature must be valid standard base64 (proto3 `bytes` compatible)
        // whose decoded envelope carries the `agyc:codex:v1:` namespace prefix.
        let decoded_sig = STANDARD
            .decode(&sig)
            .expect("thoughtSignature must be valid standard base64");
        assert!(
            decoded_sig.starts_with(CODEX_THOUGHT_SIGNATURE_PREFIX.as_bytes()),
            "expected decoded {CODEX_THOUGHT_SIGNATURE_PREFIX} prefix"
        );

        // Unary aggregator merges thought text + thoughtSignature cleanly onto the thought part.
        let mut agg = UnaryGeminiAggregator::new();
        for c in chunks {
            agg.push_chunk(c).expect("agg push");
        }
        let (status, unary) = agg.finish().expect("agg finish");
        assert_eq!(status, 200);
        let unary_parts = unary["candidates"][0]["content"]["parts"]
            .as_array()
            .expect("unary parts");
        assert_eq!(unary_parts.len(), 2);
        assert_eq!(unary_parts[0]["text"], "Planning edit.");
        assert_eq!(unary_parts[0]["thought"], true);
        assert_eq!(unary_parts[0]["thoughtSignature"], sig);
        assert_eq!(unary_parts[1], json!({ "text": "Done!" }));

        // Turn 2: pass Turn 1's assistant response (plus duplicate sig on sibling part & foreign Gemini sig) back.
        let turn2 = json!({
            "contents": [
                { "role": "user", "parts": [{ "text": "Do task 1" }] },
                {
                    "role": "model",
                    "parts": [
                        { "text": "Planning edit.", "thought": true, "thoughtSignature": sig },
                        { "text": "Done!", "thoughtSignature": sig },
                        { "text": "Foreign sig part", "thoughtSignature": "foreign_gemini_sig_xyz" }
                    ]
                },
                { "role": "user", "parts": [{ "text": "Now task 2" }] }
            ]
        });
        let out_t2 =
            translate_gemini_to_codex_request("gpt-6.2-astra", turn2.to_string().as_bytes())
                .expect("turn 2 translation");
        let input = out_t2["input"].as_array().expect("input array");
        // Expect: user, reasoning (deduplicated!), assistant message, user.
        assert_eq!(input.len(), 4, "got input: {input:?}");
        assert_eq!(input[0]["role"], "user");
        assert_eq!(
            input[1],
            json!({
                "type": "reasoning",
                "id": "rs_turn1_abc",
                "summary": [{ "type": "summary_text", "text": "Planning edit." }],
                "encrypted_content": "gAAAAAB_encrypted_blob_turn1"
            })
        );
        assert_eq!(input[2]["role"], "assistant");
        assert_eq!(input[2]["content"][0]["text"], "Done!");
        assert_eq!(input[2]["content"][1]["text"], "Foreign sig part");
        assert_eq!(input[3]["role"], "user");
    }

    #[test]
    fn enforces_size_cap_and_ignores_malformed_codex_thought_signatures() {
        // Oversized encrypted_content in SSE output_item.done is dropped cleanly without error/OOM.
        let huge_blob = "A".repeat(MAX_CODEX_REASONING_SIGNATURE_BYTES);
        let sse = format!(
            "data: {{\"type\":\"response.output_item.done\",\"output_index\":0,\"item\":{{\"id\":\"rs_huge\",\"type\":\"reasoning\",\"summary\":[],\"encrypted_content\":\"{huge_blob}\"}}}}\n\n"
        );
        let mut tr = CodexSseTranslator::new();
        let chunks = tr
            .push_chunk(sse.as_bytes())
            .expect("oversized sig dropped");
        assert!(
            chunks.is_empty(),
            "oversized reasoning signature must not be emitted"
        );

        // Malformed or oversized inbound agyc:codex:v1: signatures are ignored cleanly.
        let bad_sig = format!("{CODEX_THOUGHT_SIGNATURE_PREFIX}!!!not-valid-base64!!!");
        let body = json!({
            "contents": [
                { "role": "user", "parts": [{ "text": "hi" }] },
                {
                    "role": "model",
                    "parts": [
                        { "text": "", "thought": true, "thoughtSignature": bad_sig },
                        { "text": "Hello" }
                    ]
                }
            ]
        });
        let out = translate_gemini_to_codex_request("gpt-6.2-sol", body.to_string().as_bytes())
            .expect("malformed sig ignored");
        let input = out["input"].as_array().expect("input");
        assert_eq!(input.len(), 2);
        assert_eq!(input[1]["role"], "assistant");
    }
}
