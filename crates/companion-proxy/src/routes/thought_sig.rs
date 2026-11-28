use bytes::Bytes;
use companion_providers::{ByteStream, ProviderError};
use futures_util::StreamExt;

pub(super) const THOUGHT_SIG_ERROR_SNIFF_MAX_BYTES: usize = 64 * 1024;

pub(super) struct SniffedErrorPrefix {
    pub(super) sniffed: Vec<u8>,
    pub(super) leftover: Option<Bytes>,
    pub(super) stream_err: Option<ProviderError>,
    pub(super) remainder: ByteStream,
}

pub(super) async fn sniff_error_prefix(
    mut remainder: ByteStream,
    max_bytes: usize,
) -> SniffedErrorPrefix {
    let mut sniffed = Vec::new();
    let mut leftover: Option<Bytes> = None;
    let mut stream_err: Option<ProviderError> = None;

    while sniffed.len() < max_bytes {
        match remainder.next().await {
            Some(Ok(chunk)) => {
                if chunk.is_empty() {
                    continue;
                }
                let remaining = max_bytes - sniffed.len();
                if chunk.len() > remaining {
                    sniffed.extend_from_slice(&chunk[..remaining]);
                    leftover = Some(chunk.slice(remaining..));
                    break;
                }
                sniffed.extend_from_slice(&chunk);
            }
            Some(Err(err)) => {
                stream_err = Some(err);
                break;
            }
            None => break,
        }
    }

    SniffedErrorPrefix {
        sniffed,
        leftover,
        stream_err,
        remainder,
    }
}

fn contains_bytes(haystack: &[u8], needle: &[u8]) -> bool {
    haystack.windows(needle.len()).any(|w| w == needle)
}

pub(super) fn body_contains_thought_signature(body: &[u8]) -> bool {
    const CAMEL: &[u8] = b"thoughtSignature";
    const SNAKE: &[u8] = b"thought_signature";
    contains_bytes(body, CAMEL) || contains_bytes(body, SNAKE)
}

pub(super) fn is_invalid_thought_signature_error(body: &[u8]) -> bool {
    const REASON: &[u8] = b"INVALID_THOUGHT_SIGNATURE";
    const MSG: &[u8] = b"Corrupted thought signature";
    const TYPE_BYTES: &[u8] = b"TYPE_BYTES";
    const BASE64_FAIL: &[u8] = b"Base64 decoding failed";
    if contains_bytes(body, REASON) || contains_bytes(body, MSG) {
        return true;
    }
    // Google's REST/proto3 JSON transcoder rejects non-base64 foreign thoughtSignature
    // values (such as Codex's `agyc:codex:v1:...` namespace) with 400 INVALID_ARGUMENT
    // mentioning `thought_signature`/`thoughtSignature` + `TYPE_BYTES` / `Base64 decoding failed`.
    body_contains_thought_signature(body)
        && (contains_bytes(body, TYPE_BYTES) || contains_bytes(body, BASE64_FAIL))
}

fn is_empty_or_thought_only_part(map: &serde_json::Map<String, serde_json::Value>) -> bool {
    map.iter()
        .all(|(k, v)| k == "thought" || (k == "text" && v.as_str().is_some_and(str::is_empty)))
}

fn strip_thought_signatures_value(value: &mut serde_json::Value) -> bool {
    let mut removed = false;
    match value {
        serde_json::Value::Object(map) => {
            if map.remove("thoughtSignature").is_some() {
                removed = true;
            }
            if map.remove("thought_signature").is_some() {
                removed = true;
            }
            for (key, child) in map.iter_mut() {
                if key == "parts"
                    && let serde_json::Value::Array(parts) = child
                {
                    let mut retained = Vec::with_capacity(parts.len());
                    for mut part in std::mem::take(parts) {
                        let part_had_sig = if let serde_json::Value::Object(part_map) = &mut part {
                            let direct_removed = part_map.remove("thoughtSignature").is_some()
                                | part_map.remove("thought_signature").is_some();
                            let nested_removed = part_map
                                .values_mut()
                                .fold(false, |acc, v| acc | strip_thought_signatures_value(v));
                            if direct_removed || nested_removed {
                                removed = true;
                            }
                            direct_removed
                        } else {
                            if strip_thought_signatures_value(&mut part) {
                                removed = true;
                            }
                            false
                        };

                        let drop_part = part_had_sig
                            && part.as_object().is_some_and(is_empty_or_thought_only_part);
                        if !drop_part {
                            retained.push(part);
                        }
                    }
                    *parts = retained;
                } else if strip_thought_signatures_value(child) {
                    removed = true;
                }
            }
        }
        serde_json::Value::Array(items) => {
            for item in items {
                if strip_thought_signatures_value(item) {
                    removed = true;
                }
            }
        }
        _ => {}
    }
    removed
}

pub(super) fn strip_thought_signatures_from_json(body: &[u8]) -> Option<Bytes> {
    let mut parsed: serde_json::Value = serde_json::from_slice(body).ok()?;
    if !strip_thought_signatures_value(&mut parsed) {
        return None;
    }
    serde_json::to_vec(&parsed).ok().map(Bytes::from)
}

pub(super) fn replay_buffered_stream(
    prefix: Bytes,
    leftover: Option<Bytes>,
    stream_err: Option<ProviderError>,
    remainder: ByteStream,
) -> ByteStream {
    let mut head: Vec<Result<Bytes, ProviderError>> = Vec::with_capacity(3);
    if !prefix.is_empty() {
        head.push(Ok(prefix));
    }
    if let Some(tail) = leftover
        && !tail.is_empty()
    {
        head.push(Ok(tail));
    }
    if let Some(err) = stream_err {
        head.push(Err(err));
        return Box::pin(futures_util::stream::iter(head));
    }
    Box::pin(futures_util::stream::iter(head).chain(remainder))
}
