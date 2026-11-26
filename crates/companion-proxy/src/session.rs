use serde_json::Value;

pub const MIN_CONVERSATION_ID_LEN: usize = 6;
pub const MAX_CONVERSATION_ID_LEN: usize = 64;
const MAX_CONTENT_PART_SCAN_BYTES: usize = 4096;

/// Validates and normalizes an Antigravity conversation/session ID.
///
/// Only ASCII `[0-9a-zA-Z_-]` identifiers of length `6..=64` are accepted so
/// values are safe to use as map keys and URL query parameters without encoding
/// ambiguity or unbounded memory growth.
pub fn sanitize_conversation_id(raw: &str) -> Option<String> {
    let trimmed = raw.trim();
    if !(MIN_CONVERSATION_ID_LEN..=MAX_CONVERSATION_ID_LEN).contains(&trimmed.len()) {
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

fn is_id_byte(b: u8) -> bool {
    b.is_ascii_alphanumeric() || b == b'-' || b == b'_'
}

fn extract_id_after_marker(haystack: &str, marker: &str, skip_ws: bool) -> Option<String> {
    let bytes = haystack.as_bytes();
    let mut offset = 0;
    while let Some(pos) = haystack[offset..].find(marker) {
        let mut start = offset + pos + marker.len();
        if skip_ws {
            while start < bytes.len() && bytes[start].is_ascii_whitespace() {
                start += 1;
            }
        }
        let mut end = start;
        while end < bytes.len() && is_id_byte(bytes[end]) && (end - start) < MAX_CONVERSATION_ID_LEN
        {
            end += 1;
        }
        if let Some(id) = sanitize_conversation_id(&haystack[start..end]) {
            return Some(id);
        }
        offset = offset + pos + marker.len();
    }
    None
}

fn extract_conversation_id_from_text(text: &str) -> Option<String> {
    extract_id_after_marker(text, "Conversation ID:", true)
}

fn extract_brain_id_from_text(text: &str) -> Option<String> {
    extract_id_after_marker(text, "/brain/", false)
        .or_else(|| extract_id_after_marker(text, "\\brain\\", false))
}

fn truncate_utf8_prefix(s: &str, max_bytes: usize) -> &str {
    if s.len() <= max_bytes {
        return s;
    }
    let mut end = max_bytes;
    while end > 0 && !s.is_char_boundary(end) {
        end -= 1;
    }
    &s[..end]
}

fn collect_part_texts<'a>(value: &'a Value, max_bytes: Option<usize>, out: &mut Vec<&'a str>) {
    let Some(obj) = value.as_object() else {
        if let Some(text) = value.as_str() {
            out.push(match max_bytes {
                Some(limit) => truncate_utf8_prefix(text, limit),
                None => text,
            });
        }
        return;
    };
    if let Some(parts) = obj.get("parts").and_then(Value::as_array) {
        for part in parts.iter().rev() {
            if let Some(text) = part.get("text").and_then(Value::as_str) {
                out.push(match max_bytes {
                    Some(limit) => truncate_utf8_prefix(text, limit),
                    None => text,
                });
            }
        }
    }
}

/// Passively extracts an Antigravity conversation ID from an inbound JSON
/// generation request body without mutating forwarded payload bytes.
///
/// Precedence:
/// 1. Explicit `session_id` / `sessionId` (top-level or nested under `request`).
/// 2. `Conversation ID: <id>` in `systemInstruction` / `system_instruction` or
///    first/last `contents` text parts.
/// 3. Fallback `/brain/<id>` (or Windows `\brain\<id>`) artifact path.
pub fn extract_conversation_id(body: &[u8]) -> Option<String> {
    if body.is_empty() {
        return None;
    }
    let parsed: Value = serde_json::from_slice(body).ok()?;
    let root = parsed.as_object()?;
    let inner = root
        .get("request")
        .and_then(Value::as_object)
        .unwrap_or(root);

    for candidate in [
        root.get("session_id").and_then(Value::as_str),
        root.get("sessionId").and_then(Value::as_str),
        inner.get("session_id").and_then(Value::as_str),
        inner.get("sessionId").and_then(Value::as_str),
    ]
    .into_iter()
    .flatten()
    {
        if let Some(id) = sanitize_conversation_id(candidate) {
            return Some(id);
        }
    }

    let mut texts: Vec<&str> = Vec::new();
    if let Some(sys_inst) = inner
        .get("systemInstruction")
        .or_else(|| inner.get("system_instruction"))
    {
        collect_part_texts(sys_inst, None, &mut texts);
    }

    if let Some(contents) = inner.get("contents").and_then(Value::as_array)
        && !contents.is_empty()
    {
        collect_part_texts(&contents[0], Some(MAX_CONTENT_PART_SCAN_BYTES), &mut texts);
        if contents.len() > 1 {
            collect_part_texts(
                &contents[contents.len() - 1],
                Some(MAX_CONTENT_PART_SCAN_BYTES),
                &mut texts,
            );
        }
    }

    for text in &texts {
        if let Some(id) = extract_conversation_id_from_text(text) {
            return Some(id);
        }
    }
    for text in &texts {
        if let Some(id) = extract_brain_id_from_text(text) {
            return Some(id);
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sanitize_accepts_uuid_and_rejects_short_or_unsafe_ids() {
        assert_eq!(
            sanitize_conversation_id("  a25ce165-c93a-42be-a648-ac6144c7acba \n"),
            Some("a25ce165-c93a-42be-a648-ac6144c7acba".to_string())
        );
        assert_eq!(sanitize_conversation_id("short"), None);
        assert_eq!(sanitize_conversation_id("../etc/passwd"), None);
        assert_eq!(sanitize_conversation_id("conv?foo=bar"), None);
        assert_eq!(sanitize_conversation_id(&"a".repeat(65)), None);
    }

    #[test]
    fn extracts_explicit_session_id_first() {
        let body = br#"{
            "request": {
                "sessionId": "sess-12345678",
                "systemInstruction": {
                    "parts": [{"text": "Conversation ID: a25ce165-c93a-42be-a648-ac6144c7acba"}]
                }
            }
        }"#;
        assert_eq!(
            extract_conversation_id(body),
            Some("sess-12345678".to_string())
        );
    }

    #[test]
    fn extracts_conversation_id_from_system_instruction() {
        let body = br#"{
            "systemInstruction": {
                "parts": [
                    {"text": "<user_information>\nConversation ID: a25ce165-c93a-42be-a648-ac6144c7acba\nArtifact Directory Path: /Users/test/.gemini/antigravity/brain/other-999999\n</user_information>"}
                ]
            },
            "contents": [{"role": "user", "parts": [{"text": "hello"}]}]
        }"#;
        assert_eq!(
            extract_conversation_id(body),
            Some("a25ce165-c93a-42be-a648-ac6144c7acba".to_string())
        );
    }

    #[test]
    fn falls_back_to_brain_path_on_unix_and_windows() {
        let unix_body = br#"{
            "systemInstruction": {
                "parts": [{"text": "Artifact Directory Path: /home/user/.gemini/antigravity/brain/conv-unix-123456"}]
            }
        }"#;
        assert_eq!(
            extract_conversation_id(unix_body),
            Some("conv-unix-123456".to_string())
        );

        let win_body = br#"{
            "systemInstruction": {
                "parts": [{"text": "Artifact Directory Path: C:\\Users\\u\\.gemini\\antigravity\\brain\\conv-win-654321"}]
            }
        }"#;
        assert_eq!(
            extract_conversation_id(win_body),
            Some("conv-win-654321".to_string())
        );
    }

    #[test]
    fn returns_none_for_non_json_or_missing_id() {
        assert_eq!(extract_conversation_id(b""), None);
        assert_eq!(extract_conversation_id(b"not-json"), None);
        assert_eq!(
            extract_conversation_id(br#"{"contents":[{"role":"user","parts":[{"text":"hi"}]}]}"#),
            None
        );
    }
}
