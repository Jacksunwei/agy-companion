use std::collections::VecDeque;
use std::collections::hash_map::RandomState;
use std::hash::BuildHasher;
use std::sync::Mutex;

use companion_core::{
    CoreError, decode_ascii_hex_nibble, is_printable_ascii_with_space, validate_url_path,
};
use http::header::{self, HeaderMap, HeaderName, HeaderValue};
use serde_json::Value;
use url::{Host, Url};

use super::CloudCodeError;

const ROUTING_CACHE_CAPACITY: usize = 8;

const ALLOWED_REQUEST_HEADERS: &[&str] = &[
    "authorization",
    "content-type",
    "content-encoding",
    "user-agent",
    "x-goog-user-project",
    "x-goog-request-reason",
    "x-goog-api-client",
    "x-client-data",
    "x-cloudaicompanion-trace-id",
    "x-server-timeout",
    "accept",
    "accept-language",
    "accept-encoding",
];

const ALLOWED_RESPONSE_HEADERS: &[&str] = &[
    "content-type",
    "content-encoding",
    "x-cloudaicompanion-trace-id",
    "x-goog-sherlog-link",
    "retry-after",
];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct AccountRoute {
    pub(super) use_prod: bool,
    pub(super) strip_user_project: bool,
    pub(super) authoritative: bool,
}

#[derive(Default)]
struct RoutingCacheInner {
    entries: VecDeque<((u64, u64), AccountRoute)>,
    last_resolved_prod: Option<bool>,
    last_resolved_authoritative: bool,
}

pub(super) struct CloudCodeRoutingCache {
    hasher_a: RandomState,
    hasher_b: RandomState,
    inner: Mutex<RoutingCacheInner>,
}

impl CloudCodeRoutingCache {
    pub(super) fn new() -> Self {
        Self {
            hasher_a: RandomState::new(),
            hasher_b: RandomState::new(),
            inner: Mutex::new(RoutingCacheInner::default()),
        }
    }

    pub(super) fn auth_fingerprint(&self, headers: &HeaderMap) -> Option<(u64, u64)> {
        let raw = headers.get(header::AUTHORIZATION)?;
        let trimmed = raw.as_bytes().trim_ascii();
        if trimmed.is_empty() {
            return None;
        }
        Some((
            self.hasher_a.hash_one(trimmed),
            self.hasher_b.hash_one(trimmed),
        ))
    }

    pub(super) fn get(&self, key: (u64, u64)) -> Option<AccountRoute> {
        let mut inner = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        let pos = inner.entries.iter().position(|(k, _)| *k == key)?;
        let (_, route) = inner.entries.remove(pos)?;
        inner.entries.push_back((key, route));
        Some(route)
    }

    pub(super) fn put(&self, key: (u64, u64), route: AccountRoute) {
        let mut inner = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        let existing = inner
            .entries
            .iter()
            .position(|(k, _)| *k == key)
            .and_then(|pos| inner.entries.remove(pos))
            .map(|(_, existing)| existing);

        let effective = match existing {
            Some(prev)
                if !route.authoritative
                    && (prev.authoritative || (prev.use_prod && !route.use_prod)) =>
            {
                // Never let a speculative probe (e.g. concurrent fetchAvailableModels hitting
                // daily) downgrade an authoritative route or an already-working prod route for
                // the same account.
                AccountRoute {
                    use_prod: prev.use_prod,
                    strip_user_project: prev.strip_user_project || route.strip_user_project,
                    authoritative: prev.authoritative,
                }
            }
            _ => route,
        };

        while inner.entries.len() >= ROUTING_CACHE_CAPACITY {
            inner.entries.pop_front();
        }
        inner.entries.push_back((key, effective));

        if effective.authoritative {
            inner.last_resolved_prod = Some(effective.use_prod);
            inner.last_resolved_authoritative = true;
        } else if existing.is_none() || effective.use_prod || !inner.last_resolved_authoritative {
            // Preserve prod if the same account was kept on prod above; otherwise reflect the
            // newly observed account (or an upgrade from speculative daily -> prod).
            if existing.is_none() || effective.use_prod || inner.last_resolved_prod != Some(true) {
                inner.last_resolved_prod = Some(effective.use_prod);
                inner.last_resolved_authoritative = false;
            }
        }
    }

    pub(super) fn record_without_key(&self, use_prod: bool, authoritative: bool) {
        let mut inner = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        if authoritative {
            inner.last_resolved_prod = Some(use_prod);
            inner.last_resolved_authoritative = true;
        } else if !inner.last_resolved_authoritative
            && (use_prod || inner.last_resolved_prod.is_none())
        {
            inner.last_resolved_prod = Some(use_prod);
        }
    }

    pub(super) fn last_resolved_prod(&self) -> Option<bool> {
        self.inner
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .last_resolved_prod
    }

    pub(super) fn last_resolved_prod_for_experiments(&self) -> Option<bool> {
        let inner = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        match inner.last_resolved_prod {
            Some(true) => Some(true),
            Some(false) if inner.last_resolved_authoritative => Some(false),
            _ => None,
        }
    }
}

pub(super) fn is_loopback_url(raw: &str) -> bool {
    let Ok(parsed) = Url::parse(raw) else {
        return false;
    };
    match parsed.host() {
        Some(Host::Ipv4(ip)) => ip.is_loopback(),
        Some(Host::Ipv6(ip)) => ip.is_loopback(),
        Some(Host::Domain(domain)) => domain.eq_ignore_ascii_case("localhost"),
        None => false,
    }
}

pub(super) fn detect_prod_from_load_code_assist(body: &[u8]) -> Option<bool> {
    let parsed: Value = serde_json::from_slice(body).ok()?;
    let obj = parsed.as_object()?;
    for key in ["paidTier", "currentTier"] {
        if obj
            .get(key)
            .and_then(|t| t.get("usesGcpTos"))
            .and_then(Value::as_bool)
            == Some(true)
        {
            return Some(true);
        }
    }
    for key in ["currentTier", "paidTier"] {
        if let Some(id) = obj
            .get(key)
            .and_then(|t| t.get("id"))
            .and_then(Value::as_str)
        {
            let lower = id.to_ascii_lowercase();
            if lower.starts_with("gcp-")
                || lower.contains("paygo")
                || lower.contains("business")
                || lower.contains("enterprise")
                || lower.contains("workspace")
            {
                return Some(true);
            }
        }
    }
    let has_tier_object = ["paidTier", "currentTier"]
        .iter()
        .any(|key| obj.get(*key).is_some_and(Value::is_object));
    if has_tier_object { Some(false) } else { None }
}

pub(super) fn detect_enterprise_from_fetch_body(body: &[u8]) -> bool {
    let Ok(parsed) = serde_json::from_slice::<Value>(body) else {
        return false;
    };
    match parsed.get("entitlement").and_then(|e| e.get("userTier")) {
        Some(Value::String(s)) => {
            let trimmed = s.trim();
            !trimmed.is_empty()
                && !trimmed.eq_ignore_ascii_case("USER_TIER_UNSPECIFIED")
                && !trimmed.eq_ignore_ascii_case("UNSPECIFIED")
        }
        Some(Value::Number(n)) => n.as_i64() != Some(0) && n.as_u64() != Some(0),
        Some(Value::Bool(true) | Value::Object(_) | Value::Array(_)) => true,
        _ => false,
    }
}

pub(super) fn detect_auth_method_from_headers(headers: &HeaderMap) -> Option<bool> {
    let ua = headers.get(header::USER_AGENT)?.to_str().ok()?;
    let lower = ua.to_ascii_lowercase();
    let needle = "auth_method=";
    let mut search_start = 0;
    while let Some(rel_idx) = lower[search_start..].find(needle) {
        let start = search_start + rel_idx;
        let boundary_before = start == 0
            || (!lower.as_bytes()[start - 1].is_ascii_alphanumeric()
                && lower.as_bytes()[start - 1] != b'_');
        let val_start = start + needle.len();
        if boundary_before {
            let rest = &lower[val_start..];
            let val_end = rest
                .find(|c: char| !c.is_ascii_alphanumeric() && c != '_' && c != '-')
                .unwrap_or(rest.len());
            match &rest[..val_end] {
                "gcp" | "adc" | "corp" | "business" | "enterprise" | "paygo" => return Some(true),
                "consumer" => return Some(false),
                _ => {}
            }
        }
        search_start = val_start;
    }
    None
}

pub(super) fn response_has_non_empty_models(body: &[u8]) -> bool {
    let Ok(parsed) = serde_json::from_slice::<Value>(body) else {
        return false;
    };
    parsed
        .get("models")
        .and_then(Value::as_object)
        .is_some_and(|m| !m.is_empty())
}

pub(super) fn response_has_non_empty_flags(body: &[u8]) -> bool {
    let Ok(parsed) = serde_json::from_slice::<Value>(body) else {
        return false;
    };
    parsed
        .get("flags")
        .and_then(Value::as_array)
        .is_some_and(|f| !f.is_empty())
}

pub fn is_cloudcode_path(path: &str) -> bool {
    path.starts_with("/v1internal:") || path.starts_with("/v1internal/")
}

pub fn validate_cloudcode_path(path_and_query: &str) -> Result<(), CloudCodeError> {
    let (path, query) = match path_and_query.split_once('?') {
        Some((p, q)) => (p, Some(q)),
        None => (path_and_query, None),
    };

    if !is_cloudcode_path(path) {
        return Err(CloudCodeError::InvalidPath(
            "path must start with /v1internal: or /v1internal/".to_string(),
        ));
    }

    validate_url_path(path).map_err(|err| match err {
        CoreError::InvalidBaseUrl { reason } => {
            CloudCodeError::InvalidPath(reason.replace("base URL path", "path"))
        }
        other => CloudCodeError::InvalidPath(other.to_string()),
    })?;

    if let Some(q) = query {
        validate_cloudcode_query(q)?;
    }

    Ok(())
}

fn validate_cloudcode_query(query: &str) -> Result<(), CloudCodeError> {
    let bytes = query.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        let byte = bytes[i];
        if !(0x21..=0x7E).contains(&byte) || byte == b'#' {
            return Err(CloudCodeError::InvalidPath(
                "query contains disallowed characters".to_string(),
            ));
        }
        if byte == b'%' {
            let (Some(&hi_b), Some(&lo_b)) = (bytes.get(i + 1), bytes.get(i + 2)) else {
                return Err(CloudCodeError::InvalidPath(
                    "query contains a truncated percent-encoding sequence".to_string(),
                ));
            };
            let (Some(hi), Some(lo)) =
                (decode_ascii_hex_nibble(hi_b), decode_ascii_hex_nibble(lo_b))
            else {
                return Err(CloudCodeError::InvalidPath(
                    "query contains a malformed percent-encoding sequence".to_string(),
                ));
            };
            let decoded = (hi << 4) | lo;
            if decoded < 0x20 || decoded == 0x7F {
                return Err(CloudCodeError::InvalidPath(
                    "query contains percent-encoded control characters".to_string(),
                ));
            }
            i += 3;
            continue;
        }
        i += 1;
    }
    Ok(())
}

pub(super) fn copy_safe_request_headers(
    inbound: &HeaderMap,
    force_identity_encoding: bool,
    strip_user_project: bool,
) -> Vec<(HeaderName, HeaderValue)> {
    let mut out = Vec::new();
    for &allowed in ALLOWED_REQUEST_HEADERS {
        if force_identity_encoding && allowed == "accept-encoding" {
            continue;
        }
        if strip_user_project && allowed == "x-goog-user-project" {
            continue;
        }
        let Ok(name) = HeaderName::from_bytes(allowed.as_bytes()) else {
            continue;
        };
        if let Some(value) = inbound.get(&name)
            && is_printable_ascii_with_space(value.as_bytes())
        {
            out.push((name, value.clone()));
        }
    }
    if force_identity_encoding {
        out.push((
            header::ACCEPT_ENCODING,
            HeaderValue::from_static("identity"),
        ));
    }
    out
}

/// Returns `true` ONLY when `Content-Encoding` is absent, empty/whitespace, or strictly
/// `identity` (ASCII case-insensitive across all comma-separated tokens and all header
/// instances). Non-UTF-8 values or any compression coding (`gzip`, `br`, `deflate`, `zstd`,
/// etc.) return `false` so S4 streaming pass-through skips teeing without buffering or
/// mutating compressed bytes.
pub(crate) fn is_uncompressed_content_encoding(headers: &HeaderMap) -> bool {
    for value in headers.get_all(header::CONTENT_ENCODING) {
        let Ok(raw) = value.to_str() else {
            return false;
        };
        for token in raw.split(',') {
            let trimmed = token.trim();
            if !trimmed.is_empty() && !trimmed.eq_ignore_ascii_case("identity") {
                return false;
            }
        }
    }
    true
}

pub(super) fn copy_safe_response_headers(upstream: &HeaderMap) -> HeaderMap {
    let mut out = HeaderMap::new();
    for &allowed in ALLOWED_RESPONSE_HEADERS {
        let Ok(name) = HeaderName::from_bytes(allowed.as_bytes()) else {
            continue;
        };
        if allowed == "content-encoding" {
            for value in upstream.get_all(&name) {
                if is_printable_ascii_with_space(value.as_bytes()) {
                    out.append(name.clone(), value.clone());
                }
            }
        } else if let Some(value) = upstream.get(&name)
            && is_printable_ascii_with_space(value.as_bytes())
        {
            out.insert(name, value.clone());
        }
    }
    out
}
