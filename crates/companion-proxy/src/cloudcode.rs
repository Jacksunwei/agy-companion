use std::collections::HashSet;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use bytes::Bytes;
use companion_core::{
    CLOUDCODE_DAILY_UPSTREAM_URL, CLOUDCODE_PROD_UPSTREAM_URL, CoreError,
    DEFAULT_CLOUDCODE_UPSTREAM_URL, DEFAULT_TIMEOUT_SECS, is_supported_gemini_model,
    validate_cloudcode_upstream_url, validate_upstream_url,
};
use companion_providers::ByteStream;
use futures_util::TryStreamExt;
use http::header::{self, HeaderMap, HeaderName, HeaderValue};
use http::{Method, StatusCode};
use serde_json::{Map, Value, json};
use thiserror::Error;

pub const MAX_CLOUDCODE_CATALOG_BYTES: usize = 16 * 1024 * 1024;
const CLOUDCODE_CONNECT_TIMEOUT_SECS: u64 = 15;

// Dedicated custom placeholder enum range in Antigravity (MODEL_PLACEHOLDER_M334..=M365).
const PLACEHOLDER_ENUM_START: u32 = 334;
const PLACEHOLDER_ENUM_END: u32 = 365;

const ALLOWED_REQUEST_HEADERS: &[&str] = &[
    "authorization",
    "content-type",
    "user-agent",
    "x-goog-user-project",
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

#[derive(Debug, Error)]
pub enum CloudCodeError {
    #[error("invalid cloudcode upstream URL: {0}")]
    InvalidConfig(#[from] CoreError),
    #[error("invalid cloudcode request path: {0}")]
    InvalidPath(String),
    #[error("cloudcode upstream request failed: {0}")]
    Upstream(String),
    #[error("cloudcode upstream response exceeded {MAX_CLOUDCODE_CATALOG_BYTES} bytes")]
    ResponseTooLarge,
}

pub struct CloudCodeStreamResponse {
    pub status: StatusCode,
    pub headers: HeaderMap,
    pub stream: ByteStream,
}

pub struct CloudCodeBufferedResponse {
    pub status: StatusCode,
    pub headers: HeaderMap,
    pub body: Bytes,
}

enum CloudCodeTarget {
    Fixed(String),
    Auto {
        daily_url: String,
        prod_url: String,
        resolved_prod: AtomicBool,
        has_resolved: AtomicBool,
    },
}

#[derive(Clone)]
pub struct CloudCodeClient {
    target: Arc<CloudCodeTarget>,
    http: reqwest::Client,
}

impl CloudCodeClient {
    pub fn new(base_url: &str, timeout_secs: u64) -> Result<Self, CloudCodeError> {
        validate_cloudcode_upstream_url(base_url)?;
        if base_url == DEFAULT_CLOUDCODE_UPSTREAM_URL {
            return Self::new_auto_with_urls(
                CLOUDCODE_DAILY_UPSTREAM_URL,
                CLOUDCODE_PROD_UPSTREAM_URL,
                timeout_secs,
            );
        }
        let http = build_http_client(timeout_secs)?;
        Ok(Self {
            target: Arc::new(CloudCodeTarget::Fixed(
                base_url.trim_end_matches('/').to_string(),
            )),
            http,
        })
    }

    pub fn new_auto_with_urls(
        daily_url: &str,
        prod_url: &str,
        timeout_secs: u64,
    ) -> Result<Self, CloudCodeError> {
        validate_upstream_url(daily_url)?;
        validate_upstream_url(prod_url)?;
        let http = build_http_client(timeout_secs)?;
        Ok(Self {
            target: Arc::new(CloudCodeTarget::Auto {
                daily_url: daily_url.trim_end_matches('/').to_string(),
                prod_url: prod_url.trim_end_matches('/').to_string(),
                resolved_prod: AtomicBool::new(false),
                has_resolved: AtomicBool::new(false),
            }),
            http,
        })
    }

    pub fn base_url(&self) -> &str {
        match self.target.as_ref() {
            CloudCodeTarget::Fixed(url) => url,
            CloudCodeTarget::Auto {
                daily_url,
                prod_url,
                resolved_prod,
                ..
            } => {
                if resolved_prod.load(Ordering::Relaxed) {
                    prod_url
                } else {
                    daily_url
                }
            }
        }
    }

    fn record_auto_choice(&self, use_prod: bool) {
        if let CloudCodeTarget::Auto {
            resolved_prod,
            has_resolved,
            ..
        } = self.target.as_ref()
        {
            resolved_prod.store(use_prod, Ordering::Relaxed);
            has_resolved.store(true, Ordering::Relaxed);
        }
    }

    pub async fn forward_raw(
        &self,
        method: Method,
        path_and_query: &str,
        inbound_headers: &HeaderMap,
        body: Bytes,
    ) -> Result<CloudCodeStreamResponse, CloudCodeError> {
        validate_cloudcode_path(path_and_query)?;

        // In "auto" mode, mirror Antigravity's own UpdateEndpointURL bootstrap:
        // send loadCodeAssist to prod first and detect Business/PayGo (usesGcpTos)
        // vs Personal Google AI Pro (daily) without mutating the response.
        let path_only = path_and_query
            .split_once('?')
            .map_or(path_and_query, |(p, _)| p);
        if method == Method::POST
            && path_only == "/v1internal:loadCodeAssist"
            && let CloudCodeTarget::Auto { prod_url, .. } = self.target.as_ref()
        {
            let buffered = self
                .send_buffered(
                    prod_url,
                    Method::POST,
                    path_and_query,
                    inbound_headers,
                    body,
                    true,
                )
                .await?;
            if buffered.status == StatusCode::OK
                && let Some(use_prod) = detect_prod_from_load_code_assist(&buffered.body)
            {
                self.record_auto_choice(use_prod);
            }
            let mut headers = buffered.headers;
            headers.remove(header::CONTENT_ENCODING);
            let stream: ByteStream =
                Box::pin(futures_util::stream::once(async move { Ok(buffered.body) }));
            return Ok(CloudCodeStreamResponse {
                status: buffered.status,
                headers,
                stream,
            });
        }

        let target_url = format!("{}{path_and_query}", self.base_url());
        let mut req_builder = self.http.request(method, target_url);
        for (name, value) in copy_safe_request_headers(inbound_headers, false) {
            req_builder = req_builder.header(name, value);
        }
        if !body.is_empty() {
            req_builder = req_builder.body(body);
        }

        let resp = req_builder
            .send()
            .await
            .map_err(|err| CloudCodeError::Upstream(err.to_string()))?;
        let status = resp.status();
        let headers = copy_safe_response_headers(resp.headers());
        let stream: ByteStream = Box::pin(resp.bytes_stream().map_err(|err| {
            companion_providers::ProviderError::Upstream(format!("cloudcode stream error: {err}"))
        }));

        Ok(CloudCodeStreamResponse {
            status,
            headers,
            stream,
        })
    }

    pub async fn forward_fetch_available_models(
        &self,
        path_and_query: &str,
        inbound_headers: &HeaderMap,
        body: Bytes,
    ) -> Result<CloudCodeBufferedResponse, CloudCodeError> {
        validate_cloudcode_path(path_and_query)?;

        match self.target.as_ref() {
            CloudCodeTarget::Fixed(base) => {
                self.send_buffered(
                    base,
                    Method::POST,
                    path_and_query,
                    inbound_headers,
                    body,
                    true,
                )
                .await
            }
            CloudCodeTarget::Auto {
                daily_url,
                prod_url,
                resolved_prod,
                has_resolved,
            } => {
                let prefer_prod = if has_resolved.load(Ordering::Relaxed) {
                    resolved_prod.load(Ordering::Relaxed)
                } else {
                    request_hints_business_project(&body)
                };

                let (primary_url, primary_is_prod, fallback_url, fallback_is_prod) = if prefer_prod
                {
                    (prod_url.as_str(), true, daily_url.as_str(), false)
                } else {
                    (daily_url.as_str(), false, prod_url.as_str(), true)
                };

                let primary_res = self
                    .send_buffered(
                        primary_url,
                        Method::POST,
                        path_and_query,
                        inbound_headers,
                        body.clone(),
                        true,
                    )
                    .await;

                if let Ok(ref resp) = primary_res
                    && resp.status == StatusCode::OK
                    && response_has_non_empty_models(&resp.body)
                {
                    self.record_auto_choice(primary_is_prod);
                    return primary_res;
                }

                // Fallback to the alternate CloudCode host (e.g. PayGo account hitting daily
                // or personal AI Pro account hitting prod) so original models are never lost.
                let fallback_res = self
                    .send_buffered(
                        fallback_url,
                        Method::POST,
                        path_and_query,
                        inbound_headers,
                        body,
                        true,
                    )
                    .await;

                if let Ok(ref resp) = fallback_res
                    && resp.status == StatusCode::OK
                    && response_has_non_empty_models(&resp.body)
                {
                    self.record_auto_choice(fallback_is_prod);
                    return fallback_res;
                }

                match (primary_res, fallback_res) {
                    (Ok(p), _) if p.status == StatusCode::OK => Ok(p),
                    (_, Ok(f)) if f.status == StatusCode::OK => Ok(f),
                    (Ok(p), _) => Ok(p),
                    (Err(_), Ok(f)) => Ok(f),
                    (Err(e), Err(_)) => Err(e),
                }
            }
        }
    }

    async fn send_buffered(
        &self,
        base_url: &str,
        method: Method,
        path_and_query: &str,
        inbound_headers: &HeaderMap,
        body: Bytes,
        force_identity_encoding: bool,
    ) -> Result<CloudCodeBufferedResponse, CloudCodeError> {
        let target_url = format!("{base_url}{path_and_query}");
        let mut req_builder = self.http.request(method, target_url);
        for (name, value) in copy_safe_request_headers(inbound_headers, force_identity_encoding) {
            req_builder = req_builder.header(name, value);
        }
        if !body.is_empty() {
            req_builder = req_builder.body(body);
        }

        let mut resp = req_builder
            .send()
            .await
            .map_err(|err| CloudCodeError::Upstream(err.to_string()))?;
        let status = resp.status();
        let headers = copy_safe_response_headers(resp.headers());

        let mut collected = Vec::new();
        while let Some(chunk) = resp
            .chunk()
            .await
            .map_err(|err| CloudCodeError::Upstream(err.to_string()))?
        {
            if collected.len().saturating_add(chunk.len()) > MAX_CLOUDCODE_CATALOG_BYTES {
                return Err(CloudCodeError::ResponseTooLarge);
            }
            collected.extend_from_slice(&chunk);
        }

        Ok(CloudCodeBufferedResponse {
            status,
            headers,
            body: Bytes::from(collected),
        })
    }
}

fn build_http_client(timeout_secs: u64) -> Result<reqwest::Client, CloudCodeError> {
    let effective_timeout = if timeout_secs == 0 {
        DEFAULT_TIMEOUT_SECS
    } else {
        timeout_secs
    };
    reqwest::Client::builder()
        .no_proxy()
        .redirect(reqwest::redirect::Policy::none())
        .connect_timeout(Duration::from_secs(CLOUDCODE_CONNECT_TIMEOUT_SECS))
        .timeout(Duration::from_secs(effective_timeout))
        .build()
        .map_err(|err| CloudCodeError::Upstream(err.to_string()))
}

fn detect_prod_from_load_code_assist(body: &[u8]) -> Option<bool> {
    let parsed: Value = serde_json::from_slice(body).ok()?;
    let obj = parsed.as_object()?;
    if obj
        .get("paidTier")
        .and_then(|t| t.get("usesGcpTos"))
        .and_then(Value::as_bool)
        == Some(true)
    {
        return Some(true);
    }
    for key in ["currentTier", "paidTier"] {
        if let Some(id) = obj
            .get(key)
            .and_then(|t| t.get("id"))
            .and_then(Value::as_str)
        {
            let lower = id.to_ascii_lowercase();
            if lower.contains("paygo")
                || lower.contains("business")
                || lower.contains("enterprise")
                || lower.contains("workspace")
            {
                return Some(true);
            }
        }
    }
    Some(false)
}

fn request_hints_business_project(body: &[u8]) -> bool {
    let Ok(parsed) = serde_json::from_slice::<Value>(body) else {
        return false;
    };
    parsed
        .get("project")
        .and_then(Value::as_str)
        .is_some_and(|p| !p.trim().is_empty())
}

fn response_has_non_empty_models(body: &[u8]) -> bool {
    let Ok(parsed) = serde_json::from_slice::<Value>(body) else {
        return false;
    };
    parsed
        .get("models")
        .and_then(Value::as_object)
        .is_some_and(|m| !m.is_empty())
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
    if !path.bytes().all(|b| (0x21..=0x7E).contains(&b))
        || path.contains(['\\', '#'])
        || path.contains("..")
    {
        return Err(CloudCodeError::InvalidPath(
            "path contains disallowed characters or traversal".to_string(),
        ));
    }

    let lower_path = path.to_ascii_lowercase();
    if lower_path.contains("%2e") || lower_path.contains("%2f") || lower_path.contains("%5c") {
        return Err(CloudCodeError::InvalidPath(
            "path contains disallowed percent-encoded traversal characters".to_string(),
        ));
    }
    for segment in path.split('/') {
        if segment == "." || segment == ".." {
            return Err(CloudCodeError::InvalidPath(
                "path must not contain dot segments".to_string(),
            ));
        }
    }

    if let Some(q) = query
        && (!q.bytes().all(|b| (0x21..=0x7E).contains(&b)) || q.contains('#'))
    {
        return Err(CloudCodeError::InvalidPath(
            "query contains disallowed characters".to_string(),
        ));
    }

    Ok(())
}

fn copy_safe_request_headers(
    inbound: &HeaderMap,
    force_identity_encoding: bool,
) -> Vec<(HeaderName, HeaderValue)> {
    let mut out = Vec::new();
    for &allowed in ALLOWED_REQUEST_HEADERS {
        if force_identity_encoding && allowed == "accept-encoding" {
            continue;
        }
        let Ok(name) = HeaderName::from_bytes(allowed.as_bytes()) else {
            continue;
        };
        if let Some(value) = inbound.get(&name)
            && value.as_bytes().iter().all(|&b| (0x20..=0x7E).contains(&b))
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

fn copy_safe_response_headers(upstream: &HeaderMap) -> HeaderMap {
    let mut out = HeaderMap::new();
    for &allowed in ALLOWED_RESPONSE_HEADERS {
        let Ok(name) = HeaderName::from_bytes(allowed.as_bytes()) else {
            continue;
        };
        if let Some(value) = upstream.get(&name)
            && value.as_bytes().iter().all(|&b| (0x20..=0x7E).contains(&b))
        {
            out.insert(name, value.clone());
        }
    }
    out
}

#[derive(Debug, Clone, Copy)]
struct ThinkingVariant {
    suffix: &'static str,
    // Must match Antigravity UI EFFORT_LABEL_RE: low|medium|high|xhigh|max (case-insensitive)
    // so variants collapse into a single row with the native "Thinking effort" secondary menu.
    label_effort: &'static str,
    // Matches Antigravity EFFORT_LABELS for groupedModels metadata.
    grouped_display_name: &'static str,
    thinking_level: i32,
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

const HYDROGEN_GEMINI_LEVELS: &[ThinkingVariant] = &[
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

fn variants_for_model(model_id: &str) -> &'static [ThinkingVariant] {
    if model_id == "gemini-4-hydrogen" {
        HYDROGEN_GEMINI_LEVELS
    } else {
        STANDARD_GEMINI_LEVELS
    }
}

fn normalize_model_id(raw: &str) -> String {
    let trimmed = raw.trim();
    trimmed
        .strip_prefix("models/")
        .unwrap_or(trimmed)
        .trim()
        .to_string()
}

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

fn collect_used_model_enums(models_obj: &Map<String, Value>) -> HashSet<String> {
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

fn available_placeholder_enums(used: &HashSet<String>) -> Vec<String> {
    (PLACEHOLDER_ENUM_START..=PLACEHOLDER_ENUM_END)
        .map(|n| format!("MODEL_PLACEHOLDER_M{n}"))
        .filter(|name| !used.contains(name))
        .collect()
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

    // Deduplicate and order proxy models with default_model first.
    let mut ordered_models: Vec<String> = Vec::new();
    let mut seen_models: HashSet<String> = HashSet::new();
    let default_norm = normalize_model_id(default_model);
    if !default_norm.is_empty()
        && is_supported_gemini_model(&default_norm)
        && seen_models.insert(default_norm.clone())
    {
        ordered_models.push(default_norm);
    }
    for raw in proxy_models {
        let norm = normalize_model_id(raw);
        if !norm.is_empty() && is_supported_gemini_model(&norm) && seen_models.insert(norm.clone())
        {
            ordered_models.push(norm);
        }
    }

    if ordered_models.is_empty() {
        return serde_json::to_vec(&root);
    }

    let used_enums = root_obj
        .get("models")
        .and_then(Value::as_object)
        .map(collect_used_model_enums)
        .unwrap_or_default();
    let mut enum_pool = available_placeholder_enums(&used_enums).into_iter();
    let mut remaining_slots = enum_pool.len();

    let mut injected_model_ids: Vec<String> = Vec::new();
    let mut injected_details: Vec<(String, Value)> = Vec::new();
    let mut injected_groups: Vec<(String, Value)> = Vec::new();

    for raw_id in &ordered_models {
        let variants = variants_for_model(raw_id);
        if remaining_slots < variants.len() {
            break;
        }

        let base_id = format!("companion/{raw_id}");
        let display_base = format!("{} (Proxy)", pretty_model_name(raw_id));
        let mut level_entries = Vec::with_capacity(variants.len());

        for variant in variants {
            let Some(enum_name) = enum_pool.next() else {
                break;
            };
            remaining_slots = remaining_slots.saturating_sub(1);

            let variant_id = format!("{base_id}-{}", variant.suffix);
            let display_name = format!("{display_base} ({})", variant.label_effort);

            let details = json!({
                "model": enum_name,
                "displayName": display_name,
                "modelUrl": format!("gemini-api://{raw_id}"),
                "maxTokens": 1048576,
                "maxOutputTokens": 65536,
                "supportsThinking": true,
                "thinkingLevel": variant.thinking_level,
                "supportsImages": true,
                "supportsVideo": true,
                "supportsPdf": true,
                "supportedMimeTypes": gemini_supported_mime_types(),
                "apiProvider": "API_PROVIDER_GOOGLE_GEMINI",
                "modelProvider": "MODEL_PROVIDER_GOOGLE",
                "toolFormatterType": "TOOL_FORMATTER_TYPE_NONE",
                "quotaInfo": {
                    "remainingFraction": 1.0
                }
            });

            injected_model_ids.push(variant_id.clone());
            injected_details.push((variant_id, details));
            level_entries.push(json!({
                "thinkingSuffix": variant.suffix,
                "thinkingDisplayName": variant.grouped_display_name
            }));
        }

        injected_groups.push((
            base_id,
            json!({
                "displayName": display_base,
                "supportedThinkingLevels": level_entries
            }),
        ));
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

    serde_json::to_vec(&root)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn validates_cloudcode_paths_and_rejects_traversal() {
        assert!(validate_cloudcode_path("/v1internal:fetchAvailableModels").is_ok());
        assert!(validate_cloudcode_path("/v1internal:streamGenerateContent?alt=sse").is_ok());
        assert!(validate_cloudcode_path("/v1internal:loadCodeAssist").is_ok());
        assert!(validate_cloudcode_path("/v1internal/projects/p1/locations/global").is_ok());

        for bad in [
            "/v1beta/models",
            "/v1internal:loadCodeAssist/../secret",
            "/v1internal/foo/..%2fbar",
            "/v1internal/%2e%2e/secret",
            "/v1internal\\evil",
            "/v1internal:foo#frag",
            "/v1internal:foo?a=1#frag",
            "/v1internal:foo\r\nbar",
        ] {
            assert!(
                validate_cloudcode_path(bad).is_err(),
                "expected {bad:?} to be rejected"
            );
        }
    }

    #[test]
    fn pretty_model_name_formats_gemini_ids_cleanly() {
        assert_eq!(pretty_model_name("gemini-4-hydrogen"), "Gemini 4 Hydrogen");
        assert_eq!(pretty_model_name("gemini-3.8-flash"), "Gemini 3.8 Flash");
        assert_eq!(
            pretty_model_name("gemini-3.1-pro-preview"),
            "Gemini 3.1 Pro Preview"
        );
    }

    #[test]
    fn detects_prod_vs_daily_from_load_code_assist_response() {
        assert_eq!(
            detect_prod_from_load_code_assist(br#"{"paidTier":{"usesGcpTos":true}}"#),
            Some(true)
        );
        assert_eq!(
            detect_prod_from_load_code_assist(
                br#"{"currentTier":{"id":"agy-business-paygo-tier"}}"#
            ),
            Some(true)
        );
        assert_eq!(
            detect_prod_from_load_code_assist(
                br#"{"paidTier":{"id":"g1-pro-tier","usesGcpTos":false}}"#
            ),
            Some(false)
        );
        assert_eq!(
            detect_prod_from_load_code_assist(br#"{"currentTier":{"id":"free-tier"}}"#),
            Some(false)
        );
    }

    #[test]
    fn merge_available_models_appends_to_recommended_group0_with_proxy_suffix_and_effort_labels() {
        let upstream = json!({
            "models": {
                "gemini-4-carbon-low": {
                    "model": "MODEL_PLACEHOLDER_M438",
                    "displayName": "Gemini 4 Carbon (Low)",
                    "thinkingLevel": 1
                },
                "gemini-4-carbon-high": {
                    "model": "MODEL_PLACEHOLDER_M427",
                    "displayName": "Gemini 4 Carbon (High)",
                    "thinkingLevel": 3
                },
                "custom-existing": {
                    "model": "MODEL_PLACEHOLDER_M334",
                    "displayName": "Existing Custom"
                }
            },
            "defaultAgentModelId": "gemini-4-carbon-high",
            "agentModelSorts": [
                {
                    "displayName": "Recommended",
                    "groups": [
                        {
                            "displayName": "Google AI Pro",
                            "modelIds": ["gemini-4-carbon-low", "gemini-4-carbon-high"]
                        }
                    ]
                }
            ],
            "groupedModels": {
                "gemini-4-carbon": {
                    "displayName": "Gemini 4 Carbon",
                    "supportedThinkingLevels": [
                        { "thinkingSuffix": "low", "thinkingDisplayName": "Low" },
                        { "thinkingSuffix": "high", "thinkingDisplayName": "High" }
                    ]
                }
            }
        });

        let proxy_models = vec![
            "models/gemini-3.8-flash".to_string(),
            "gemini-4-hydrogen".to_string(),
            "gemini-2.5-pro".to_string(), // unsupported legacy model must be skipped
        ];

        let merged_bytes = merge_available_models(
            &serde_json::to_vec(&upstream).expect("serialize upstream"),
            &proxy_models,
            "gemini-4-hydrogen",
        )
        .expect("merge succeeds");
        let merged: Value = serde_json::from_slice(&merged_bytes).expect("parse merged");

        // Original fields, models, and groupedModels remain intact.
        assert_eq!(merged["defaultAgentModelId"], "gemini-4-carbon-high");
        assert_eq!(
            merged["models"]["gemini-4-carbon-high"]["displayName"],
            "Gemini 4 Carbon (High)"
        );
        assert_eq!(
            merged["groupedModels"]["gemini-4-carbon"]["displayName"],
            "Gemini 4 Carbon"
        );

        // Proxy models use "(Proxy) (Low|Medium|High|XHigh)" labels so Antigravity's
        // parseLabelSlug collapses them into one "(Proxy)" row with Thinking effort submenu.
        let h_low = &merged["models"]["companion/gemini-4-hydrogen-low"];
        let h_xhigh = &merged["models"]["companion/gemini-4-hydrogen-xhigh"];
        assert_eq!(h_low["modelUrl"], "gemini-api://gemini-4-hydrogen");
        assert_eq!(h_low["thinkingLevel"], 1);
        assert_eq!(h_low["displayName"], "Gemini 4 Hydrogen (Proxy) (Low)");
        assert_eq!(h_xhigh["modelUrl"], "gemini-api://gemini-4-hydrogen");
        assert_eq!(h_xhigh["thinkingLevel"], 7);
        assert_eq!(h_xhigh["displayName"], "Gemini 4 Hydrogen (Proxy) (XHigh)");

        // Existing MODEL_PLACEHOLDER_M334 in upstream must NOT be reused.
        assert_eq!(h_low["model"], "MODEL_PLACEHOLDER_M335");

        // Legacy 2.5 model must not be injected.
        assert!(
            merged["models"]
                .get("companion/gemini-2.5-pro-low")
                .is_none()
        );

        // groupedModels stays in sync with "(Proxy)" base name and "Extra" display label.
        let h_group = &merged["groupedModels"]["companion/gemini-4-hydrogen"];
        assert_eq!(h_group["displayName"], "Gemini 4 Hydrogen (Proxy)");
        assert_eq!(
            h_group["supportedThinkingLevels"][0]["thinkingSuffix"],
            "low"
        );
        assert_eq!(
            h_group["supportedThinkingLevels"][3]["thinkingSuffix"],
            "xhigh"
        );
        assert_eq!(
            h_group["supportedThinkingLevels"][3]["thinkingDisplayName"],
            "Extra"
        );

        let f_group = &merged["groupedModels"]["companion/gemini-3.8-flash"];
        assert_eq!(f_group["displayName"], "Gemini 3.8 Flash (Proxy)");
        assert_eq!(
            f_group["supportedThinkingLevels"].as_array().unwrap().len(),
            3
        );

        // Injected model IDs are appended directly to Recommended groups[0] right after originals!
        assert_eq!(
            merged["agentModelSorts"][0]["groups"]
                .as_array()
                .unwrap()
                .len(),
            1
        );
        let ids: Vec<&str> = merged["agentModelSorts"][0]["groups"][0]["modelIds"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(Value::as_str)
            .collect();
        assert_eq!(
            ids,
            vec![
                "gemini-4-carbon-low",
                "gemini-4-carbon-high",
                "companion/gemini-4-hydrogen-low",
                "companion/gemini-4-hydrogen-medium",
                "companion/gemini-4-hydrogen-high",
                "companion/gemini-4-hydrogen-xhigh",
                "companion/gemini-3.8-flash-low",
                "companion/gemini-3.8-flash-medium",
                "companion/gemini-3.8-flash-high",
            ]
        );
    }

    #[tokio::test]
    async fn auto_mode_falls_back_between_daily_and_prod_to_preserve_original_models() {
        use axum::Router;
        use tokio::net::TcpListener;

        // Mock daily returns empty models (simulating a PayGo/Business token hitting daily).
        let mock_daily = Router::new().route(
            "/v1internal:fetchAvailableModels",
            axum::routing::post(|| async {
                (
                    StatusCode::OK,
                    [(header::CONTENT_TYPE, "application/json")],
                    r#"{"models":{}}"#,
                )
            }),
        );
        let daily_listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let daily_addr = daily_listener.local_addr().unwrap();
        let daily_srv = tokio::spawn(async move {
            let _ = axum::serve(daily_listener, mock_daily).await;
        });

        // Mock prod returns real original models.
        let mock_prod = Router::new().route(
            "/v1internal:fetchAvailableModels",
            axum::routing::post(|| async {
                (
                    StatusCode::OK,
                    [(header::CONTENT_TYPE, "application/json")],
                    r#"{"models":{"gemini-4-neon-high":{"displayName":"Gemini 4 Neon (High)"}}}"#,
                )
            }),
        );
        let prod_listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let prod_addr = prod_listener.local_addr().unwrap();
        let prod_srv = tokio::spawn(async move {
            let _ = axum::serve(prod_listener, mock_prod).await;
        });

        let client = CloudCodeClient::new_auto_with_urls(
            &format!("http://{daily_addr}"),
            &format!("http://{prod_addr}"),
            30,
        )
        .expect("auto client");

        // Request without project tries daily first, sees empty models, and automatically falls back to prod!
        let resp = client
            .forward_fetch_available_models(
                "/v1internal:fetchAvailableModels",
                &HeaderMap::new(),
                Bytes::from_static(b"{}"),
            )
            .await
            .expect("auto fallback succeeds");
        assert_eq!(resp.status, StatusCode::OK);
        let parsed: Value = serde_json::from_slice(&resp.body).unwrap();
        assert_eq!(
            parsed["models"]["gemini-4-neon-high"]["displayName"],
            "Gemini 4 Neon (High)"
        );
        assert_eq!(client.base_url(), format!("http://{prod_addr}"));

        daily_srv.abort();
        prod_srv.abort();
    }

    #[test]
    fn merge_available_models_omits_grouped_models_when_upstream_omits_or_empties_it() {
        for upstream in [
            json!({
                "models": {
                    "gemini-4-neon-low": { "model": "MODEL_PLACEHOLDER_M400", "displayName": "Gemini 4 Neon (Low)" },
                    "gemini-4-neon-high": { "model": "MODEL_PLACEHOLDER_M401", "displayName": "Gemini 4 Neon (High)" }
                },
                "agentModelSorts": [{
                    "displayName": "Recommended",
                    "groups": [{ "displayName": "Models", "modelIds": ["gemini-4-neon-low", "gemini-4-neon-high"] }]
                }]
            }),
            json!({
                "models": {
                    "gemini-4-neon-low": { "model": "MODEL_PLACEHOLDER_M400", "displayName": "Gemini 4 Neon (Low)" },
                    "gemini-4-neon-high": { "model": "MODEL_PLACEHOLDER_M401", "displayName": "Gemini 4 Neon (High)" }
                },
                "agentModelSorts": [{
                    "displayName": "Recommended",
                    "groups": [{ "displayName": "Models", "modelIds": ["gemini-4-neon-low", "gemini-4-neon-high"] }]
                }],
                "groupedModels": {}
            }),
        ] {
            let merged_bytes = merge_available_models(
                &serde_json::to_vec(&upstream).unwrap(),
                &["gemini-4-hydrogen".to_string()],
                "gemini-4-hydrogen",
            )
            .expect("merge succeeds");
            let merged: Value = serde_json::from_slice(&merged_bytes).unwrap();

            // Must NOT synthesize a non-empty groupedModels map when upstream had none,
            // otherwise Antigravity disables its label parser and flattens all original models.
            let grouped_is_empty_or_absent = merged
                .get("groupedModels")
                .and_then(Value::as_object)
                .is_none_or(Map::is_empty);
            assert!(
                grouped_is_empty_or_absent,
                "expected absent/empty groupedModels, got: {:?}",
                merged.get("groupedModels")
            );

            // Proxy variants are still present in models & groups[0] with label-parser-compatible names.
            assert_eq!(
                merged["models"]["companion/gemini-4-hydrogen-xhigh"]["displayName"],
                "Gemini 4 Hydrogen (Proxy) (XHigh)"
            );
            let group0 = merged["agentModelSorts"][0]["groups"][0]["modelIds"]
                .as_array()
                .unwrap();
            assert_eq!(group0[0], "gemini-4-neon-low");
            assert!(
                group0
                    .iter()
                    .any(|v| v == "companion/gemini-4-hydrogen-xhigh")
            );
        }
    }
}
