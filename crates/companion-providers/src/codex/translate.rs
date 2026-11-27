use std::collections::{HashMap, HashSet, VecDeque};

use bytes::Bytes;
use ring::digest::{Context as DigestContext, SHA256};
use serde_json::{Map, Value, json};

use crate::ProviderError;

pub const DEFAULT_CODEX_INSTRUCTIONS: &str = "You are a helpful coding assistant.";
pub const MAX_CODEX_CALL_ID_LEN: usize = 64;
pub const MAX_CODEX_CACHE_KEY_LEN: usize = 64;

/// Maximum per-line / per-event byte buffer in [`CodexSseTranslator`] (S4/S7 OOM guard).
pub const DEFAULT_MAX_CODEX_SSE_EVENT_BYTES: usize = 2 * 1024 * 1024;
/// Maximum buffered JSON arguments size for a single streamed `function_call` item.
pub const DEFAULT_MAX_TOOL_CALL_ARGS_BYTES: usize = 2 * 1024 * 1024;
/// Maximum aggregate buffered JSON arguments across all concurrently open tool calls.
pub const DEFAULT_MAX_TOTAL_TOOL_CALL_ARGS_BYTES: usize = 4 * 1024 * 1024;
/// Maximum simultaneously open tool calls in a single Codex response stream.
pub const MAX_CONCURRENT_TOOL_CALLS: usize = 128;
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

    if let Some(effort) = extract_reasoning_effort(root) {
        out.insert(
            "reasoning".to_string(),
            json!({
                "effort": effort,
                "summary": "auto"
            }),
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
                translate_model_parts(parts, turn_idx, &mut correlator, &mut input)?;
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
    input: &mut Vec<Value>,
) -> Result<(), ProviderError> {
    let mut assistant_parts: Vec<Value> = Vec::new();

    for (part_idx, part) in parts.iter().enumerate() {
        let Some(part_obj) = part.as_object() else {
            return Err(ProviderError::InvalidRequest(
                "each part in 'contents' must be a JSON object".to_string(),
            ));
        };

        // Always skip Gemini reasoning/thought blocks; ignore sibling thoughtSignature fields
        // without dropping non-thought text or functionCall parts.
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

fn extract_reasoning_effort(root: &Map<String, Value>) -> Option<&'static str> {
    let gen_cfg = root
        .get("generationConfig")
        .or_else(|| root.get("generation_config"))
        .and_then(Value::as_object)?;
    let thinking_cfg = gen_cfg
        .get("thinkingConfig")
        .or_else(|| gen_cfg.get("thinking_config"))
        .and_then(Value::as_object)?;

    if let Some(level) = thinking_cfg
        .get("thinkingLevel")
        .or_else(|| thinking_cfg.get("thinking_level"))
    {
        match level {
            Value::String(s) => match s.trim().to_ascii_uppercase().as_str() {
                "LOW" | "MINIMAL" => return Some("low"),
                "MEDIUM" => return Some("medium"),
                "HIGH" => return Some("high"),
                "XHIGH" | "EXTRA" | "MAX" => return Some("xhigh"),
                "NONE" | "OFF" => return None,
                _ => {}
            },
            Value::Number(n) => match n.as_i64() {
                Some(1) => return Some("low"),
                Some(2) => return Some("medium"),
                Some(3) => return Some("high"),
                Some(7) => return Some("xhigh"),
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
        return Some(if budget <= 2048 {
            "low"
        } else if budget <= 8192 {
            "medium"
        } else {
            "high"
        });
    }

    None
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
            "response.reasoning_summary_text.delta" => {
                if let Some(delta) = event.get("delta").and_then(Value::as_str)
                    && !delta.is_empty()
                {
                    out.push(json!({
                        "candidates": [{
                            "content": {
                                "role": "model",
                                "parts": [{ "text": delta, "thought": true }]
                            },
                            "index": 0
                        }]
                    }));
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
                if let Some(item) = event.get("item").and_then(Value::as_object)
                    && item.get("type").and_then(Value::as_str) == Some("function_call")
                {
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
                    let initial_args = item.get("arguments").and_then(Value::as_str).unwrap_or("");
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
                if let Some(item) = event.get("item").and_then(Value::as_object)
                    && item.get("type").and_then(Value::as_str) == Some("function_call")
                {
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
                            self.synthetic_call_seq = self.synthetic_call_seq.wrapping_add(1);
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
                                "upstream Codex function_call item missing name".to_string(),
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
                let (_status, err_json) = sanitize_codex_error_json(502, payload);
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
            self.account_bytes(text.len())?;
            let is_thought = obj.get("thought").and_then(Value::as_bool) == Some(true);
            if let Some(last) = self.parts.last_mut().and_then(Value::as_object_mut)
                && last.get("text").is_some()
                && (last.get("thought").and_then(Value::as_bool) == Some(true)) == is_thought
                && let Some(Value::String(existing)) = last.get_mut("text")
            {
                existing.push_str(text);
                return Ok(());
            }
            if is_thought {
                self.parts.push(json!({ "text": text, "thought": true }));
            } else {
                self.parts.push(json!({ "text": text }));
            }
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
}
