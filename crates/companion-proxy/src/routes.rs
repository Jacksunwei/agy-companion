use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use axum::body::Body;
use axum::extract::rejection::BytesRejection;
use axum::extract::{DefaultBodyLimit, OriginalUri, Request, State};
use axum::middleware::{self, Next};
use axum::response::{IntoResponse, Response};
use axum::routing::{MethodFilter, on};
use axum::{Json, Router};
use bytes::Bytes;
use companion_core::{GeminiUsageMetadata, UsageEvent, UsageSink};
use companion_providers::{
    ByteStream, ForwardRequest, ProviderError, curated_gemini_models, filter_and_order_models,
};
use futures_util::Stream;
use http::header::{self, HeaderMap, HeaderName, HeaderValue};
use http::{Method, StatusCode};
use serde::{Deserialize, Serialize};
use tower_http::set_header::SetResponseHeaderLayer;

use crate::cloudcode::{
    CloudCodeError, is_cloudcode_path, merge_available_models, validate_cloudcode_path,
};
use crate::sse::SseUsageTracker;
use crate::stats::ProxyStats;
use crate::{ActiveProvider, InFlightGuard, PROTOCOL_VERSION, ProxyState};

pub const MAX_PROXY_BODY_BYTES: usize = 64 * 1024 * 1024;
pub const DEFAULT_MAX_JSON_TEE_BYTES: usize = 8 * 1024 * 1024;
const CLOUDCODE_MODEL_DISCOVERY_TIMEOUT: Duration = Duration::from_secs(3);

const GEMINI_MODELS_PREFIX: &str = "/v1beta/models/";
static X_GOOG_API_KEY_HEADER: HeaderName = HeaderName::from_static("x-goog-api-key");

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HealthResponse {
    pub status: String,
    pub version: String,
    pub protocol_version: u32,
}

pub fn build_router(state: ProxyState) -> Router {
    let internal_security_headers = (
        SetResponseHeaderLayer::overriding(
            header::X_CONTENT_TYPE_OPTIONS,
            HeaderValue::from_static("nosniff"),
        ),
        SetResponseHeaderLayer::overriding(
            header::CACHE_CONTROL,
            HeaderValue::from_static("no-store"),
        ),
    );

    Router::new()
        .route(
            "/health",
            on(MethodFilter::GET, health_handler).layer(internal_security_headers.clone()),
        )
        .route(
            "/api/stats",
            on(MethodFilter::GET, stats_handler).layer(internal_security_headers.clone()),
        )
        .route(
            "/v1beta/models",
            on(MethodFilter::GET, list_models_handler).layer(internal_security_headers),
        )
        .route(
            "/v1beta/models/{*rest}",
            on(MethodFilter::POST, proxy_gemini_handler)
                .layer(DefaultBodyLimit::max(MAX_PROXY_BODY_BYTES))
                .layer(middleware::from_fn(enforce_content_length_limit)),
        )
        .route(
            "/v1internal:fetchAvailableModels",
            on(MethodFilter::POST, fetch_available_models_handler)
                .layer(DefaultBodyLimit::max(MAX_PROXY_BODY_BYTES))
                .layer(middleware::from_fn(enforce_content_length_limit)),
        )
        .method_not_allowed_fallback(method_not_allowed_handler)
        .route_layer(middleware::from_fn(enforce_get_post_only))
        .fallback(cloudcode_or_not_found_handler)
        .layer(middleware::from_fn_with_state(
            state.clone(),
            track_request_activity,
        ))
        .with_state(state)
}

async fn track_request_activity(
    State(state): State<ProxyState>,
    req: Request,
    next: Next,
) -> Response {
    // Passive liveness/telemetry probes (including `serve --adopt-running`'s 500ms `/health` poll)
    // must not reset `idle_duration()` or starve `--idle-shutdown-secs`.
    if matches!(req.uri().path(), "/health" | "/api/stats") {
        return next.run(req).await;
    }
    let _guard = state.enter_request();
    next.run(req).await
}

fn unix_timestamp_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .ok()
        .and_then(|d| u64::try_from(d.as_millis()).ok())
        .unwrap_or(0)
}

fn validate_content_length_headers(headers: &HeaderMap) -> Result<(), (StatusCode, &'static str)> {
    let mut expected_len: Option<u64> = None;
    let mut saw_overflow = false;
    for value in headers.get_all(header::CONTENT_LENGTH) {
        let raw_bytes = value.as_bytes();
        if raw_bytes.is_empty() || !raw_bytes.iter().all(u8::is_ascii_digit) {
            return Err((StatusCode::BAD_REQUEST, "invalid Content-Length header"));
        }
        let Ok(raw_str) = std::str::from_utf8(raw_bytes) else {
            return Err((StatusCode::BAD_REQUEST, "invalid Content-Length header"));
        };
        match raw_str.parse::<u64>() {
            Ok(parsed) => {
                if saw_overflow {
                    return Err((
                        StatusCode::BAD_REQUEST,
                        "conflicting Content-Length headers",
                    ));
                }
                if let Some(prev) = expected_len
                    && prev != parsed
                {
                    return Err((
                        StatusCode::BAD_REQUEST,
                        "conflicting Content-Length headers",
                    ));
                }
                expected_len = Some(parsed);
            }
            Err(_) => {
                if expected_len.is_some() {
                    return Err((
                        StatusCode::BAD_REQUEST,
                        "conflicting Content-Length headers",
                    ));
                }
                saw_overflow = true;
            }
        }
    }

    if saw_overflow || expected_len.is_some_and(|len| len > MAX_PROXY_BODY_BYTES as u64) {
        return Err((
            StatusCode::PAYLOAD_TOO_LARGE,
            "request body exceeds maximum allowed size",
        ));
    }

    Ok(())
}

async fn enforce_content_length_limit(req: Request, next: Next) -> Response {
    if *req.method() != Method::POST {
        return next.run(req).await;
    }
    if let Err((status, message)) = validate_content_length_headers(req.headers()) {
        return json_error_response(status, message);
    }
    next.run(req).await
}

async fn enforce_get_post_only(req: Request, next: Next) -> Response {
    match *req.method() {
        Method::GET | Method::POST => next.run(req).await,
        _ => json_error_response(StatusCode::METHOD_NOT_ALLOWED, "method not allowed"),
    }
}

async fn method_not_allowed_handler() -> Response {
    json_error_response(StatusCode::METHOD_NOT_ALLOWED, "method not allowed")
}

async fn not_found_handler() -> Response {
    json_error_response(StatusCode::NOT_FOUND, "not found")
}

async fn resolve_proxy_model_ids(active: &ActiveProvider) -> Vec<String> {
    let curated = curated_gemini_models(Some(&active.default_model));
    let mut ids: Vec<String> = curated.models.into_iter().map(|m| m.name).collect();

    if active.has_configured_api_key
        && let Ok(Ok(list)) = tokio::time::timeout(
            CLOUDCODE_MODEL_DISCOVERY_TIMEOUT,
            active.provider.list_models(None),
        )
        .await
    {
        let ordered = filter_and_order_models(list.models, Some(&active.default_model));
        if !ordered.models.is_empty() {
            // Keep default_model and curated Gemini 4/3.8 models prioritized ahead of
            // additional discovered models so the 32-slot placeholder pool always carries
            // the primary models first.
            for model in ordered.models {
                if !ids.contains(&model.name) {
                    ids.push(model.name);
                }
            }
        }
    }

    ids
}

async fn fetch_available_models_handler(
    State(state): State<ProxyState>,
    OriginalUri(uri): OriginalUri,
    headers: HeaderMap,
    body: Result<Bytes, BytesRejection>,
) -> Response {
    let body = match body {
        Ok(bytes) => bytes,
        Err(rejection) if rejection.status() == StatusCode::PAYLOAD_TOO_LARGE => {
            return json_error_response(
                StatusCode::PAYLOAD_TOO_LARGE,
                "request body exceeds maximum allowed size",
            );
        }
        Err(_) => {
            return json_error_response(StatusCode::BAD_REQUEST, "failed to read request body");
        }
    };

    let path_and_query = uri
        .path_and_query()
        .map(|pq| pq.as_str())
        .unwrap_or_else(|| uri.path());
    if validate_cloudcode_path(path_and_query).is_err() {
        return json_error_response(StatusCode::BAD_REQUEST, "invalid request path");
    }

    let active = state.active_provider();
    let upstream = match active
        .cloudcode
        .forward_fetch_available_models(path_and_query, &headers, body)
        .await
    {
        Ok(resp) => resp,
        Err(CloudCodeError::InvalidPath(_)) => {
            return json_error_response(StatusCode::BAD_REQUEST, "invalid request path");
        }
        Err(CloudCodeError::InvalidConfig(_)) => {
            return json_error_response(
                StatusCode::INTERNAL_SERVER_ERROR,
                "provider configuration error",
            );
        }
        Err(CloudCodeError::ResponseTooLarge | CloudCodeError::Upstream(_)) => {
            return json_error_response(
                StatusCode::BAD_GATEWAY,
                "upstream cloudcode request failed",
            );
        }
    };

    if upstream.status != StatusCode::OK {
        let mut builder = Response::builder().status(upstream.status);
        for (name, value) in &upstream.headers {
            builder = builder.header(name, value);
        }
        return builder.body(Body::from(upstream.body)).unwrap_or_else(|_| {
            json_error_response(
                StatusCode::INTERNAL_SERVER_ERROR,
                "failed to build proxy response",
            )
        });
    }

    let refresh_ts_ms = unix_timestamp_ms();
    if !active.has_configured_api_key {
        state.stats().record_catalog_refresh(refresh_ts_ms, false);
        let mut builder = Response::builder()
            .status(StatusCode::OK)
            .header(header::X_CONTENT_TYPE_OPTIONS, "nosniff")
            .header(header::CACHE_CONTROL, "no-store");
        for (name, value) in &upstream.headers {
            builder = builder.header(name, value);
        }
        return builder.body(Body::from(upstream.body)).unwrap_or_else(|_| {
            json_error_response(
                StatusCode::INTERNAL_SERVER_ERROR,
                "failed to build proxy response",
            )
        });
    }

    let proxy_model_ids = resolve_proxy_model_ids(&active).await;
    let (final_body, was_merged) = match merge_available_models(
        &upstream.body,
        &proxy_model_ids,
        &active.default_model,
    ) {
        Ok(merged) => (Bytes::from(merged), true),
        Err(err) => {
            tracing::warn!(
                error = %err,
                "failed to merge proxy models into fetchAvailableModels; returning upstream response untouched"
            );
            (upstream.body, false)
        }
    };
    state
        .stats()
        .record_catalog_refresh(refresh_ts_ms, was_merged);

    let mut builder = Response::builder()
        .status(StatusCode::OK)
        .header(header::X_CONTENT_TYPE_OPTIONS, "nosniff")
        .header(header::CACHE_CONTROL, "no-store");
    if was_merged {
        builder = builder.header(header::CONTENT_TYPE, "application/json");
        for (name, value) in &upstream.headers {
            if *name != header::CONTENT_TYPE && *name != header::CONTENT_ENCODING {
                builder = builder.header(name, value);
            }
        }
    } else {
        for (name, value) in &upstream.headers {
            builder = builder.header(name, value);
        }
    }
    builder.body(Body::from(final_body)).unwrap_or_else(|_| {
        json_error_response(
            StatusCode::INTERNAL_SERVER_ERROR,
            "failed to build proxy response",
        )
    })
}

async fn cloudcode_or_not_found_handler(State(state): State<ProxyState>, req: Request) -> Response {
    let uri = req.uri().clone();
    if !is_cloudcode_path(uri.path()) {
        return not_found_handler().await;
    }

    let method = req.method().clone();
    if method != Method::GET && method != Method::POST {
        return json_error_response(StatusCode::METHOD_NOT_ALLOWED, "method not allowed");
    }

    let path_and_query = uri
        .path_and_query()
        .map(|pq| pq.as_str())
        .unwrap_or_else(|| uri.path())
        .to_string();
    if validate_cloudcode_path(&path_and_query).is_err() {
        return json_error_response(StatusCode::BAD_REQUEST, "invalid request path");
    }

    let headers = req.headers().clone();
    if method == Method::POST
        && let Err((status, message)) = validate_content_length_headers(&headers)
    {
        return json_error_response(status, message);
    }

    let body_bytes = if method == Method::POST {
        match axum::body::to_bytes(req.into_body(), MAX_PROXY_BODY_BYTES).await {
            Ok(bytes) => bytes,
            Err(_) => {
                return json_error_response(
                    StatusCode::PAYLOAD_TOO_LARGE,
                    "request body exceeds maximum allowed size",
                );
            }
        }
    } else {
        Bytes::new()
    };

    let active = state.active_provider();
    let upstream = match active
        .cloudcode
        .forward_raw(method, &path_and_query, &headers, body_bytes)
        .await
    {
        Ok(resp) => resp,
        Err(CloudCodeError::InvalidPath(_)) => {
            return json_error_response(StatusCode::BAD_REQUEST, "invalid request path");
        }
        Err(CloudCodeError::InvalidConfig(_)) => {
            return json_error_response(
                StatusCode::INTERNAL_SERVER_ERROR,
                "provider configuration error",
            );
        }
        Err(CloudCodeError::ResponseTooLarge | CloudCodeError::Upstream(_)) => {
            return json_error_response(
                StatusCode::BAD_GATEWAY,
                "upstream cloudcode request failed",
            );
        }
    };

    let mut builder = Response::builder().status(upstream.status);
    for (name, value) in &upstream.headers {
        builder = builder.header(name, value);
    }
    let stream_guard = state.enter_request();
    builder
        .body(Body::from_stream(TrackedStream {
            inner: upstream.stream,
            guard: stream_guard,
        }))
        .unwrap_or_else(|_| {
            json_error_response(
                StatusCode::INTERNAL_SERVER_ERROR,
                "failed to build proxy response",
            )
        })
}

async fn health_handler() -> Json<HealthResponse> {
    Json(HealthResponse {
        status: "ok".to_string(),
        version: env!("CARGO_PKG_VERSION").to_string(),
        protocol_version: PROTOCOL_VERSION,
    })
}

async fn stats_handler(State(state): State<ProxyState>) -> impl IntoResponse {
    let active = state.active_provider();
    Json(state.stats().snapshot_with_cloudcode(
        active.cloudcode.mode_label(),
        active.cloudcode.resolved_upstream_url(),
    ))
}

async fn list_models_handler(State(state): State<ProxyState>, headers: HeaderMap) -> Response {
    let active = state.active_provider();
    let api_key_override = match validate_inbound_api_key(&headers, active.has_configured_api_key) {
        Ok(key) => key,
        Err(msg) => return json_error_response(StatusCode::BAD_REQUEST, msg),
    };

    let catalog = match active.provider.list_models(api_key_override).await {
        Ok(list) => {
            let ordered = filter_and_order_models(list.models, Some(&active.default_model));
            if ordered.models.is_empty() {
                curated_gemini_models(Some(&active.default_model))
            } else {
                ordered
            }
        }
        Err(err) => {
            tracing::debug!(
                error = %err,
                "upstream model discovery unavailable; serving curated Gemini 4/3 catalog"
            );
            curated_gemini_models(Some(&active.default_model))
        }
    };

    Json(catalog).into_response()
}

async fn proxy_gemini_handler(
    State(state): State<ProxyState>,
    OriginalUri(uri): OriginalUri,
    headers: HeaderMap,
    body: Result<Bytes, BytesRejection>,
) -> Response {
    let body = match body {
        Ok(bytes) => bytes,
        Err(rejection) if rejection.status() == StatusCode::PAYLOAD_TOO_LARGE => {
            return json_error_response(
                StatusCode::PAYLOAD_TOO_LARGE,
                "request body exceeds maximum allowed size",
            );
        }
        Err(_) => {
            return json_error_response(StatusCode::BAD_REQUEST, "failed to read request body");
        }
    };

    let path_and_query = uri
        .path_and_query()
        .map(|pq| pq.as_str())
        .unwrap_or_else(|| uri.path());
    if !path_and_query.starts_with(GEMINI_MODELS_PREFIX) {
        return json_error_response(StatusCode::BAD_REQUEST, "invalid request path");
    }

    let model = extract_model_name(path_and_query);
    if model == "unknown"
        || model == "."
        || model.contains("..")
        || model.contains(['?', '#', '/', '\\', '%', ';'])
        || !model.bytes().all(|b| (0x21..=0x7E).contains(&b))
    {
        return json_error_response(StatusCode::BAD_REQUEST, "invalid request path");
    }
    if !companion_core::is_supported_gemini_model(&model) {
        return json_error_response(
            StatusCode::BAD_REQUEST,
            "unsupported model: only Gemini 3 and Gemini 4 models are supported (unset GOOGLE_GEMINI_BASE_URL to reach Gemini API directly)",
        );
    }

    let content_type = match validate_inbound_content_type(&headers) {
        Ok(ct) => ct,
        Err(msg) => return json_error_response(StatusCode::BAD_REQUEST, msg),
    };

    let active = state.active_provider();
    let api_key_override = match validate_inbound_api_key(&headers, active.has_configured_api_key) {
        Ok(key) => key,
        Err(msg) => return json_error_response(StatusCode::BAD_REQUEST, msg),
    };

    let started_at = Instant::now();
    let forward_req = ForwardRequest {
        path_and_query: path_and_query.to_string(),
        body,
        content_type,
        api_key_override,
    };

    let upstream = match active.provider.forward_gemini(forward_req).await {
        Ok(resp) => resp,
        Err(ProviderError::InvalidPath(_)) => {
            return json_error_response(StatusCode::BAD_REQUEST, "invalid request path");
        }
        Err(ProviderError::MissingApiKey) => {
            return json_error_response(
                StatusCode::UNAUTHORIZED,
                "missing Gemini API key: configure gemini.api_key or set GEMINI_API_KEY",
            );
        }
        Err(ProviderError::InvalidConfig(_)) => {
            return json_error_response(
                StatusCode::INTERNAL_SERVER_ERROR,
                "provider configuration error",
            );
        }
        Err(ProviderError::Upstream(_)) => {
            return json_error_response(
                StatusCode::BAD_GATEWAY,
                "upstream provider request failed",
            );
        }
    };

    let status = StatusCode::from_u16(upstream.status).unwrap_or(StatusCode::BAD_GATEWAY);
    let mut response_builder = Response::builder().status(status);
    if let Some(ct_header) = sanitize_outbound_content_type(upstream.content_type.as_deref()) {
        response_builder = response_builder.header(header::CONTENT_TYPE, ct_header);
    }

    let stream_guard = state.enter_request();
    let body = if status.is_success() {
        let collector = match select_tee_mode(upstream.content_type.as_deref(), path_and_query) {
            UsageTeeMode::Sse => UsageCollector::Sse(SseUsageTracker::new()),
            UsageTeeMode::UnaryJson => UsageCollector::UnaryJson {
                buf: Vec::new(),
                max_bytes: DEFAULT_MAX_JSON_TEE_BYTES,
                overflowed: false,
            },
        };
        let model = extract_model_name(path_and_query);
        Body::from_stream(UsageTeeStream {
            inner: upstream.stream,
            collector,
            stats: Arc::clone(state.stats()),
            usage_sink: Arc::clone(state.usage_sink()),
            model,
            started_at,
            finalized: false,
            errored: false,
            guard: stream_guard,
        })
    } else {
        Body::from_stream(TrackedStream {
            inner: upstream.stream,
            guard: stream_guard,
        })
    };

    match response_builder.body(body) {
        Ok(resp) => resp,
        Err(_) => json_error_response(
            StatusCode::INTERNAL_SERVER_ERROR,
            "failed to build proxy response",
        ),
    }
}

fn json_error_response(status: StatusCode, message: &'static str) -> Response {
    let payload = serde_json::json!({
        "error": {
            "message": message,
        }
    });
    let body = Body::from(payload.to_string());
    let mut resp = Response::new(body);
    *resp.status_mut() = status;
    let headers = resp.headers_mut();
    headers.insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("application/json"),
    );
    headers.insert(
        header::X_CONTENT_TYPE_OPTIONS,
        HeaderValue::from_static("nosniff"),
    );
    headers.insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    resp
}

fn is_printable_ascii_with_space(bytes: &[u8]) -> bool {
    bytes.iter().all(|&b| (0x20..=0x7E).contains(&b))
}

fn validate_inbound_content_type(headers: &HeaderMap) -> Result<Option<String>, &'static str> {
    let mut iter = headers.get_all(header::CONTENT_TYPE).iter();
    let Some(first) = iter.next() else {
        return Ok(None);
    };
    if iter.next().is_some() {
        return Err("duplicate Content-Type header");
    }

    let raw_bytes = first.as_bytes();
    if !is_printable_ascii_with_space(raw_bytes) {
        return Err("invalid Content-Type header");
    }
    let raw = std::str::from_utf8(raw_bytes).map_err(|_| "invalid Content-Type header")?;
    let trimmed = raw.trim_matches(' ');
    if trimmed.is_empty() {
        return Err("invalid Content-Type header");
    }
    Ok(Some(trimmed.to_string()))
}

fn validate_inbound_api_key(
    headers: &HeaderMap,
    has_configured_api_key: bool,
) -> Result<Option<String>, &'static str> {
    let mut iter = headers.get_all(&X_GOOG_API_KEY_HEADER).iter();
    let Some(first) = iter.next() else {
        return Ok(None);
    };
    if iter.next().is_some() {
        return Err("duplicate x-goog-api-key header");
    }

    let raw_bytes = first.as_bytes();
    if !is_printable_ascii_with_space(raw_bytes) {
        return Err("invalid x-goog-api-key header");
    }
    let raw = std::str::from_utf8(raw_bytes).map_err(|_| "invalid x-goog-api-key header")?;

    if has_configured_api_key {
        return Ok(None);
    }

    let trimmed = raw.trim_matches(' ');
    if trimmed.is_empty() || companion_core::is_placeholder_api_key(trimmed) {
        return Ok(None);
    }
    if !trimmed.bytes().all(|b| (0x21..=0x7E).contains(&b)) {
        return Err("invalid x-goog-api-key header");
    }
    Ok(Some(trimmed.to_string()))
}

fn sanitize_outbound_content_type(raw: Option<&str>) -> Option<HeaderValue> {
    let raw = raw?;
    if !is_printable_ascii_with_space(raw.as_bytes()) {
        return None;
    }
    let trimmed = raw.trim_matches(' ');
    if trimmed.is_empty() {
        return None;
    }
    HeaderValue::from_str(trimmed).ok()
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum UsageTeeMode {
    Sse,
    UnaryJson,
}

fn has_alt_sse(path_and_query: &str) -> bool {
    let Some((_, query)) = path_and_query.split_once('?') else {
        return false;
    };
    query.split('&').any(|pair| {
        pair.split_once('=')
            .is_some_and(|(k, v)| k.eq_ignore_ascii_case("alt") && v.eq_ignore_ascii_case("sse"))
    })
}

fn select_tee_mode(upstream_content_type: Option<&str>, path_and_query: &str) -> UsageTeeMode {
    let media_type = upstream_content_type.and_then(|ct| {
        if !is_printable_ascii_with_space(ct.as_bytes()) {
            return None;
        }
        let essence = ct
            .split(';')
            .next()
            .unwrap_or("")
            .trim_matches(|c| c == ' ' || c == '\t');
        if essence.is_empty() {
            None
        } else {
            Some(essence)
        }
    });

    match media_type {
        Some(mt) if mt.eq_ignore_ascii_case("text/event-stream") => UsageTeeMode::Sse,
        Some(mt) if mt.eq_ignore_ascii_case("application/json") => UsageTeeMode::UnaryJson,
        None => {
            let path = path_and_query
                .split_once('?')
                .map_or(path_and_query, |(p, _)| p);
            if has_alt_sse(path_and_query) || path.ends_with(":streamGenerateContent") {
                UsageTeeMode::Sse
            } else {
                UsageTeeMode::UnaryJson
            }
        }
        Some(_) => {
            if has_alt_sse(path_and_query) {
                UsageTeeMode::Sse
            } else {
                UsageTeeMode::UnaryJson
            }
        }
    }
}

fn extract_model_name(path_and_query: &str) -> String {
    let path = path_and_query
        .split_once('?')
        .map_or(path_and_query, |(p, _)| p);
    let after_prefix = path.strip_prefix(GEMINI_MODELS_PREFIX).unwrap_or(path);
    let after_optional_models = after_prefix.strip_prefix("models/").unwrap_or(after_prefix);
    let before_action = after_optional_models
        .rsplit_once(':')
        .map_or(after_optional_models, |(model, _)| model);
    let cleaned = before_action.trim_matches('/');
    if cleaned.is_empty() {
        "unknown".to_string()
    } else {
        cleaned.to_string()
    }
}

enum UsageCollector {
    Sse(SseUsageTracker),
    UnaryJson {
        buf: Vec<u8>,
        max_bytes: usize,
        overflowed: bool,
    },
}

impl UsageCollector {
    fn push_chunk(&mut self, chunk: &[u8]) {
        match self {
            Self::Sse(tracker) => tracker.push_chunk(chunk),
            Self::UnaryJson {
                buf,
                max_bytes,
                overflowed,
            } => {
                if *overflowed {
                    return;
                }
                if buf.len().saturating_add(chunk.len()) > *max_bytes {
                    *buf = Vec::new();
                    *overflowed = true;
                    return;
                }
                buf.extend_from_slice(chunk);
            }
        }
    }

    fn finish(&mut self) -> Option<GeminiUsageMetadata> {
        match self {
            Self::Sse(tracker) => tracker.finish(),
            Self::UnaryJson {
                buf, overflowed, ..
            } => {
                if *overflowed || buf.is_empty() {
                    return None;
                }
                let usage = GeminiUsageMetadata::parse_from_json(buf);
                *buf = Vec::new();
                usage
            }
        }
    }
}

struct TrackedStream {
    inner: ByteStream,
    guard: InFlightGuard,
}

impl Stream for TrackedStream {
    type Item = Result<Bytes, ProviderError>;

    fn poll_next(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        let this = self.get_mut();
        match Pin::new(&mut this.inner).poll_next(cx) {
            Poll::Pending => Poll::Pending,
            Poll::Ready(Some(item)) => {
                this.guard.touch();
                Poll::Ready(Some(item))
            }
            Poll::Ready(None) => Poll::Ready(None),
        }
    }
}

struct UsageTeeStream {
    inner: ByteStream,
    collector: UsageCollector,
    stats: Arc<ProxyStats>,
    usage_sink: Arc<dyn UsageSink>,
    model: String,
    started_at: Instant,
    finalized: bool,
    errored: bool,
    guard: InFlightGuard,
}

impl Stream for UsageTeeStream {
    type Item = Result<Bytes, ProviderError>;

    fn poll_next(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        let this = self.get_mut();
        match Pin::new(&mut this.inner).poll_next(cx) {
            Poll::Pending => Poll::Pending,
            Poll::Ready(Some(Ok(bytes))) => {
                this.guard.touch();
                this.collector.push_chunk(bytes.as_ref());
                Poll::Ready(Some(Ok(bytes)))
            }
            Poll::Ready(Some(Err(err))) => {
                this.guard.touch();
                this.errored = true;
                this.finalized = true;
                Poll::Ready(Some(Err(err)))
            }
            Poll::Ready(None) => {
                if !this.finalized && !this.errored {
                    this.finalized = true;
                    if let Some(usage) = this.collector.finish() {
                        let latency_ms = u64::try_from(this.started_at.elapsed().as_millis())
                            .unwrap_or(u64::MAX);
                        this.stats.record(&usage);
                        this.usage_sink.record(UsageEvent::from_gemini(
                            &this.model,
                            &usage,
                            latency_ms,
                        ));
                    }
                }
                Poll::Ready(None)
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::{Arc, Mutex};

    use axum::body::Body;
    use axum::routing::post;
    use bytes::Bytes;
    use companion_core::{NoopUsageSink, UsageEvent, UsageSink};
    use companion_providers::{
        BoxFuture, ForwardRequest, ForwardResponse, GeminiProvider, ProviderAdapter, ProviderError,
    };
    use futures_util::stream;
    use http::{Request, StatusCode};
    use http_body_util::BodyExt;
    use tokio::net::TcpListener;
    use tokio::sync::{Notify, mpsc};
    use tower::ServiceExt;

    use super::*;
    use crate::{PROTOCOL_VERSION, StatsSnapshot};

    #[derive(Default)]
    struct RecordingSink {
        events: Mutex<Vec<UsageEvent>>,
    }

    impl RecordingSink {
        fn snapshot(&self) -> Vec<UsageEvent> {
            self.events.lock().unwrap().clone()
        }
    }

    impl UsageSink for RecordingSink {
        fn record(&self, event: UsageEvent) {
            self.events.lock().unwrap().push(event);
        }
    }

    struct StubProvider {
        status: u16,
        content_type: Option<String>,
        chunks: Vec<Result<Bytes, String>>,
        error_kind: Option<StubError>,
        hits: AtomicUsize,
        captured: Mutex<Vec<ForwardRequest>>,
    }

    #[derive(Clone, Copy)]
    enum StubError {
        InvalidPath,
        MissingApiKey,
        InvalidConfig,
        Upstream,
    }

    impl StubProvider {
        fn ok_stream(content_type: Option<&str>, chunks: Vec<Bytes>) -> Self {
            Self {
                status: 200,
                content_type: content_type.map(str::to_string),
                chunks: chunks.into_iter().map(Ok).collect(),
                error_kind: None,
                hits: AtomicUsize::new(0),
                captured: Mutex::new(Vec::new()),
            }
        }

        fn with_status(status: u16, content_type: Option<&str>, body: Bytes) -> Self {
            Self {
                status,
                content_type: content_type.map(str::to_string),
                chunks: vec![Ok(body)],
                error_kind: None,
                hits: AtomicUsize::new(0),
                captured: Mutex::new(Vec::new()),
            }
        }

        fn with_error(err: StubError) -> Self {
            Self {
                status: 200,
                content_type: None,
                chunks: Vec::new(),
                error_kind: Some(err),
                hits: AtomicUsize::new(0),
                captured: Mutex::new(Vec::new()),
            }
        }

        fn hit_count(&self) -> usize {
            self.hits.load(Ordering::SeqCst)
        }

        fn captured_requests(&self) -> Vec<ForwardRequest> {
            self.captured.lock().unwrap().clone()
        }
    }

    impl ProviderAdapter for StubProvider {
        fn name(&self) -> &'static str {
            "gemini"
        }

        fn forward_gemini(
            &self,
            req: ForwardRequest,
        ) -> BoxFuture<'_, Result<ForwardResponse, ProviderError>> {
            self.hits.fetch_add(1, Ordering::SeqCst);
            self.captured.lock().unwrap().push(req);
            let error_kind = self.error_kind;
            let status = self.status;
            let content_type = self.content_type.clone();
            let chunks: Vec<Result<Bytes, ProviderError>> = self
                .chunks
                .iter()
                .map(|item| match item {
                    Ok(bytes) => Ok(bytes.clone()),
                    Err(msg) => Err(ProviderError::Upstream(msg.clone())),
                })
                .collect();

            Box::pin(async move {
                if let Some(err) = error_kind {
                    return Err(match err {
                        StubError::InvalidPath => {
                            ProviderError::InvalidPath("SECRET_PATH_TOKEN /etc/passwd".to_string())
                        }
                        StubError::MissingApiKey => ProviderError::MissingApiKey,
                        StubError::InvalidConfig => {
                            ProviderError::InvalidConfig("AIzaSUPERSECRETCONFIG".to_string())
                        }
                        StubError::Upstream => ProviderError::Upstream(
                            "connection failed with AIzaUPSTREAMSECRET".to_string(),
                        ),
                    });
                }
                Ok(ForwardResponse {
                    status,
                    content_type,
                    stream: Box::pin(stream::iter(chunks)),
                })
            })
        }

        fn health_check(&self) -> BoxFuture<'_, Result<(), ProviderError>> {
            Box::pin(async { Ok(()) })
        }
    }

    fn assert_security_headers(headers: &HeaderMap) {
        assert_eq!(
            headers
                .get(header::X_CONTENT_TYPE_OPTIONS)
                .and_then(|v| v.to_str().ok()),
            Some("nosniff")
        );
        assert_eq!(
            headers
                .get(header::CACHE_CONTROL)
                .and_then(|v| v.to_str().ok()),
            Some("no-store")
        );
    }

    #[tokio::test]
    async fn health_returns_version_and_protocol_version() {
        let provider = Arc::new(StubProvider::ok_stream(None, Vec::new()));
        let state = ProxyState::new(provider, Arc::new(NoopUsageSink), true);
        let app = build_router(state);

        let resp = app
            .oneshot(
                Request::builder()
                    .method("GET")
                    .uri("/health")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(resp.status(), StatusCode::OK);
        assert_security_headers(resp.headers());
        let body = resp.into_body().collect().await.unwrap().to_bytes();
        let health: HealthResponse = serde_json::from_slice(&body).unwrap();
        assert_eq!(health.status, "ok");
        assert_eq!(health.version, env!("CARGO_PKG_VERSION"));
        assert_eq!(health.protocol_version, PROTOCOL_VERSION);
    }

    #[tokio::test]
    async fn proxies_stream_generate_content_and_records_usage_event() {
        let sse_body = concat!(
            "data: {\"candidates\":[{\"content\":{\"parts\":[{\"text\":\"Hi\"}]}}],\"usageMetadata\":{\"promptTokenCount\":100,\"candidatesTokenCount\":5}}\n\n",
            "data: {\"candidates\":[{\"content\":{\"parts\":[{\"text\":\" there\"}]}}],\"usageMetadata\":{\"promptTokenCount\":100,\"candidatesTokenCount\":25,\"cachedContentTokenCount\":60,\"thoughtsTokenCount\":8}}\n\n"
        );
        let chunks = vec![
            Bytes::from(sse_body[..40].to_string()),
            Bytes::from(sse_body[40..].to_string()),
        ];

        let sink = Arc::new(RecordingSink::default());
        let provider = Arc::new(StubProvider::ok_stream(Some("text/event-stream"), chunks));
        let state = ProxyState::new(provider, sink.clone(), true);
        let app = build_router(state);

        let resp = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/v1beta/models/gemini-4-hydrogen:streamGenerateContent?alt=sse")
                    .header("content-type", "application/json")
                    .body(Body::from(r#"{"contents":[]}"#))
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(resp.status(), StatusCode::OK);
        assert_eq!(
            resp.headers()
                .get("content-type")
                .and_then(|v| v.to_str().ok()),
            Some("text/event-stream")
        );
        // Proxied upstream responses must NOT get extra security headers injected.
        assert!(resp.headers().get(header::X_CONTENT_TYPE_OPTIONS).is_none());
        assert!(resp.headers().get(header::CACHE_CONTROL).is_none());

        let body = resp.into_body().collect().await.unwrap().to_bytes();
        assert_eq!(body.as_ref(), sse_body.as_bytes());

        let events = sink.snapshot();
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].provider, "gemini");
        assert_eq!(events[0].model, "gemini-4-hydrogen");
        assert_eq!(events[0].input_tokens, 100);
        assert_eq!(events[0].output_tokens, 25);
        assert_eq!(events[0].cached_tokens, 60);
        assert_eq!(events[0].reasoning_tokens, 8);
        assert_eq!(events[0].source, "proxy");

        let stats_resp = app
            .oneshot(
                Request::builder()
                    .method("GET")
                    .uri("/api/stats")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(stats_resp.status(), StatusCode::OK);
        assert_security_headers(stats_resp.headers());
        let stats_body = stats_resp.into_body().collect().await.unwrap().to_bytes();
        let snap: StatsSnapshot = serde_json::from_slice(&stats_body).unwrap();
        assert_eq!(snap.requests, 1);
        assert_eq!(snap.prompt_tokens, 100);
        assert_eq!(snap.cached_tokens, 60);
        assert_eq!(snap.output_tokens, 25);
        assert_eq!(snap.reasoning_tokens, 8);
        assert!((snap.cache_hit_ratio - 0.6).abs() < 1e-12);
    }

    #[tokio::test]
    async fn streams_sse_unbuffered_incrementally_before_upstream_closes() {
        struct ChannelProvider {
            rx: Mutex<Option<mpsc::Receiver<Result<Bytes, ProviderError>>>>,
        }

        impl ProviderAdapter for ChannelProvider {
            fn name(&self) -> &'static str {
                "gemini"
            }

            fn forward_gemini(
                &self,
                _req: ForwardRequest,
            ) -> BoxFuture<'_, Result<ForwardResponse, ProviderError>> {
                let mut rx = self.rx.lock().unwrap().take().expect("single call");
                let stream = stream::poll_fn(move |cx| rx.poll_recv(cx));
                Box::pin(async move {
                    Ok(ForwardResponse {
                        status: 200,
                        content_type: Some("text/event-stream; charset=utf-8".to_string()),
                        stream: Box::pin(stream),
                    })
                })
            }

            fn health_check(&self) -> BoxFuture<'_, Result<(), ProviderError>> {
                Box::pin(async { Ok(()) })
            }
        }

        let (tx, rx) = mpsc::channel(4);
        let release_chunk2 = Arc::new(Notify::new());
        let release_chunk2_tx = Arc::clone(&release_chunk2);

        let chunk1 = Bytes::from_static(b"data: {\"candidates\":[{\"text\":\"first\"}]}\n\n");
        let chunk2 = Bytes::from_static(
            b"data: {\"usageMetadata\":{\"promptTokenCount\":50,\"candidatesTokenCount\":10,\"cachedContentTokenCount\":25}}\n\n",
        );

        let c1 = chunk1.clone();
        let c2 = chunk2.clone();
        let producer = tokio::spawn(async move {
            tx.send(Ok(c1)).await.unwrap();
            // Do not send chunk 2 until the client proves it already received chunk 1!
            release_chunk2_tx.notified().await;
            tx.send(Ok(c2)).await.unwrap();
        });

        let sink = Arc::new(RecordingSink::default());
        let provider = Arc::new(ChannelProvider {
            rx: Mutex::new(Some(rx)),
        });
        let state = ProxyState::new(provider, sink.clone(), true);
        let app = build_router(state);

        let resp = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/v1beta/models/gemini-3.8-flash:streamGenerateContent?alt=sse")
                    .body(Body::from("{}"))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);

        let mut body = resp.into_body();
        let first_frame = tokio::time::timeout(std::time::Duration::from_secs(5), body.frame())
            .await
            .expect("chunk 1 must arrive without waiting for chunk 2")
            .expect("frame present")
            .expect("frame ok")
            .into_data()
            .expect("data frame");
        assert_eq!(first_frame, chunk1);
        // Before EOF, usage must not have been recorded yet.
        assert!(sink.snapshot().is_empty());

        // Now release chunk 2 and EOF.
        release_chunk2.notify_one();
        let second_frame = body
            .frame()
            .await
            .expect("second frame")
            .expect("frame ok")
            .into_data()
            .expect("data frame");
        assert_eq!(second_frame, chunk2);
        assert!(body.frame().await.is_none());
        producer.await.unwrap();

        let events = sink.snapshot();
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].model, "gemini-3.8-flash");
        assert_eq!(events[0].input_tokens, 50);
        assert_eq!(events[0].output_tokens, 10);
        assert_eq!(events[0].cached_tokens, 25);
    }

    #[tokio::test]
    async fn proxies_unary_generate_content_and_json_array_stream_byte_for_byte() {
        // 1. Unary :generateContent JSON object with arbitrary whitespace/key order preserved byte-for-byte.
        let req_bytes =
            Bytes::from_static(b"{\n  \"contents\": [{\"parts\":[{\"text\":\"hello\"}]}]\n}");
        let unary_resp_bytes = Bytes::from_static(
            b"{\n  \"candidates\": [{\"content\":{\"parts\":[{\"text\":\"world\"}]}}],\n  \"usageMetadata\": {\"promptTokenCount\": 200, \"candidatesTokenCount\": 40, \"cachedContentTokenCount\": 100, \"thoughtsTokenCount\": 15}\n}",
        );
        let sink = Arc::new(RecordingSink::default());
        let provider = Arc::new(StubProvider::ok_stream(
            Some("application/json; charset=UTF-8"),
            vec![unary_resp_bytes.slice(..32), unary_resp_bytes.slice(32..)],
        ));
        let state = ProxyState::new(provider.clone(), sink.clone(), true);
        let app = build_router(state);

        let resp = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/v1beta/models/models/gemini-4-hydrogen:generateContent")
                    .header("content-type", "application/json")
                    .body(Body::from(req_bytes.clone()))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let body = resp.into_body().collect().await.unwrap().to_bytes();
        assert_eq!(body, unary_resp_bytes);
        assert_eq!(provider.captured_requests()[0].body, req_bytes);
        assert_eq!(sink.snapshot().len(), 1);
        assert_eq!(sink.snapshot()[0].model, "gemini-4-hydrogen");
        assert_eq!(sink.snapshot()[0].input_tokens, 200);
        assert_eq!(sink.snapshot()[0].output_tokens, 40);
        assert_eq!(sink.snapshot()[0].cached_tokens, 100);
        assert_eq!(sink.snapshot()[0].reasoning_tokens, 15);

        // 2. :streamGenerateContent WITHOUT ?alt=sse returning application/json array.
        let array_resp_bytes = Bytes::from_static(
            b"[{\"usageMetadata\":{\"promptTokenCount\":80,\"candidatesTokenCount\":5}},{\"usageMetadata\":{\"promptTokenCount\":80,\"candidatesTokenCount\":22,\"cachedContentTokenCount\":40}}]",
        );
        let sink2 = Arc::new(RecordingSink::default());
        let provider2 = Arc::new(StubProvider::ok_stream(
            Some("application/json"),
            vec![array_resp_bytes.clone()],
        ));
        let state2 = ProxyState::new(provider2, sink2.clone(), true);
        let app2 = build_router(state2);

        let resp2 = app2
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/v1beta/models/gemini-3.8-flash:preview:streamGenerateContent")
                    .body(Body::from("{}"))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp2.status(), StatusCode::OK);
        let body2 = resp2.into_body().collect().await.unwrap().to_bytes();
        assert_eq!(body2, array_resp_bytes);
        let events2 = sink2.snapshot();
        assert_eq!(events2.len(), 1);
        assert_eq!(events2[0].model, "gemini-3.8-flash:preview");
        assert_eq!(events2[0].input_tokens, 80);
        assert_eq!(events2[0].output_tokens, 22);
        assert_eq!(events2[0].cached_tokens, 40);
    }

    #[tokio::test]
    async fn api_key_precedence_config_wins_over_placeholder_and_header_fallback_when_unset() {
        // Case 1: Saved config has API key -> client placeholder x-goog-api-key MUST be ignored (api_key_override: None).
        let provider_configured = Arc::new(StubProvider::ok_stream(
            Some("application/json"),
            vec![Bytes::from_static(b"{}")],
        ));
        let app_configured = build_router(ProxyState::new(
            provider_configured.clone(),
            Arc::new(NoopUsageSink),
            true,
        ));
        let resp = app_configured
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/v1beta/models/gemini-4-hydrogen:generateContent")
                    .header("x-goog-api-key", "placeholder key from agy")
                    .body(Body::from("{}"))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        assert_eq!(
            provider_configured.captured_requests()[0].api_key_override,
            None
        );

        // Case 2: Saved config has NO API key -> valid inbound x-goog-api-key (with leading/trailing ASCII spaces) forwarded.
        let provider_unconfigured = Arc::new(StubProvider::ok_stream(
            Some("application/json"),
            vec![Bytes::from_static(b"{}")],
        ));
        let app_unconfigured = build_router(ProxyState::new(
            provider_unconfigured.clone(),
            Arc::new(NoopUsageSink),
            false,
        ));
        let resp2 = app_unconfigured
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/v1beta/models/gemini-4-hydrogen:generateContent")
                    .header("x-goog-api-key", "  AIzaClientFallbackKey1234  ")
                    .body(Body::from("{}"))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp2.status(), StatusCode::OK);
        assert_eq!(
            provider_unconfigured.captured_requests()[0]
                .api_key_override
                .as_deref(),
            Some("AIzaClientFallbackKey1234")
        );

        // Case 3: Unconfigured + ASCII spaces-only header -> api_key_override: None (allows env fallback or 401).
        let resp3 = app_unconfigured
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/v1beta/models/gemini-4-hydrogen:generateContent")
                    .header("x-goog-api-key", "   ")
                    .body(Body::from("{}"))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp3.status(), StatusCode::OK);
        assert_eq!(
            provider_unconfigured.captured_requests()[1].api_key_override,
            None
        );
    }

    #[tokio::test]
    async fn rejects_malformed_or_duplicate_inbound_headers_with_400_and_zero_upstream_hits() {
        let provider = Arc::new(StubProvider::ok_stream(
            Some("application/json"),
            vec![Bytes::from_static(b"{}")],
        ));
        let app_unconfigured = build_router(ProxyState::new(
            provider.clone(),
            Arc::new(NoopUsageSink),
            false,
        ));
        let app_configured = build_router(ProxyState::new(
            provider.clone(),
            Arc::new(NoopUsageSink),
            true,
        ));

        let bad_header_bytes: Vec<&[u8]> = vec![
            "\u{00A0}AIzaSecret12345".as_bytes(),
            "\u{2028}AIzaSecret12345".as_bytes(),
            "\u{2029}AIzaSecret12345".as_bytes(),
            "\u{3000}AIzaSecret12345".as_bytes(),
            b"\x80AIzaSecret12345",
            b"\xFFAIzaSecret12345",
            b"\tAIzaSecret12345",
        ];

        // Bad x-goog-api-key bytes rejected with 400 in BOTH configured and unconfigured modes.
        for bad in &bad_header_bytes {
            for app in [&app_unconfigured, &app_configured] {
                let hv = HeaderValue::from_bytes(bad).expect("valid http obs-text/tab header");
                let resp = app
                    .clone()
                    .oneshot(
                        Request::builder()
                            .method("POST")
                            .uri("/v1beta/models/gemini-4-hydrogen:generateContent")
                            .header("x-goog-api-key", hv)
                            .body(Body::from("{}"))
                            .unwrap(),
                    )
                    .await
                    .unwrap();
                assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
                assert_security_headers(resp.headers());
                let body = resp.into_body().collect().await.unwrap().to_bytes();
                let text = String::from_utf8(body.to_vec()).unwrap();
                assert!(!text.contains("AIzaSecret12345"));
            }
        }

        // Internal space in x-goog-api-key when unconfigured -> 400 (never 500).
        let resp_space = app_unconfigured
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/v1beta/models/gemini-4-hydrogen:generateContent")
                    .header("x-goog-api-key", "AIzaSecret12345 extra")
                    .body(Body::from("{}"))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp_space.status(), StatusCode::BAD_REQUEST);
        assert_security_headers(resp_space.headers());
        let body_space = resp_space.into_body().collect().await.unwrap().to_bytes();
        assert!(!String::from_utf8_lossy(&body_space).contains("AIzaSecret12345"));

        // Duplicate x-goog-api-key -> 400 in both modes.
        for app in [&app_unconfigured, &app_configured] {
            let resp_dup_key = app
                .clone()
                .oneshot(
                    Request::builder()
                        .method("POST")
                        .uri("/v1beta/models/gemini-4-hydrogen:generateContent")
                        .header("x-goog-api-key", "AIzaFirstKey12345")
                        .header("x-goog-api-key", "AIzaSecondKey12345")
                        .body(Body::from("{}"))
                        .unwrap(),
                )
                .await
                .unwrap();
            assert_eq!(resp_dup_key.status(), StatusCode::BAD_REQUEST);
            assert_security_headers(resp_dup_key.headers());
        }

        // Bad Content-Type bytes, empty/spaces-only Content-Type, and duplicate Content-Type -> 400.
        for bad in &bad_header_bytes {
            let hv = HeaderValue::from_bytes(bad).unwrap();
            let resp = app_unconfigured
                .clone()
                .oneshot(
                    Request::builder()
                        .method("POST")
                        .uri("/v1beta/models/gemini-4-hydrogen:generateContent")
                        .header(header::CONTENT_TYPE, hv)
                        .body(Body::from("{}"))
                        .unwrap(),
                )
                .await
                .unwrap();
            assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
            assert_security_headers(resp.headers());
        }

        for empty_ct in ["", "   "] {
            let resp = app_unconfigured
                .clone()
                .oneshot(
                    Request::builder()
                        .method("POST")
                        .uri("/v1beta/models/gemini-4-hydrogen:generateContent")
                        .header(header::CONTENT_TYPE, empty_ct)
                        .body(Body::from("{}"))
                        .unwrap(),
                )
                .await
                .unwrap();
            assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
            assert_security_headers(resp.headers());
        }

        let resp_dup_ct = app_unconfigured
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/v1beta/models/gemini-4-hydrogen:generateContent")
                    .header(header::CONTENT_TYPE, "application/json")
                    .header(header::CONTENT_TYPE, "text/plain")
                    .body(Body::from("{}"))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp_dup_ct.status(), StatusCode::BAD_REQUEST);
        assert_security_headers(resp_dup_ct.headers());

        assert_eq!(provider.hit_count(), 0);
    }

    #[tokio::test]
    async fn maps_provider_errors_and_traversal_paths_to_sanitized_json_statuses() {
        let cases = [
            (StubError::InvalidPath, StatusCode::BAD_REQUEST),
            (StubError::MissingApiKey, StatusCode::UNAUTHORIZED),
            (StubError::InvalidConfig, StatusCode::INTERNAL_SERVER_ERROR),
            (StubError::Upstream, StatusCode::BAD_GATEWAY),
        ];

        for (err_kind, expected_status) in cases {
            let provider = Arc::new(StubProvider::with_error(err_kind));
            let app = build_router(ProxyState::new(provider, Arc::new(NoopUsageSink), true));
            let resp = app
                .oneshot(
                    Request::builder()
                        .method("POST")
                        .uri("/v1beta/models/gemini-4-hydrogen:generateContent")
                        .body(Body::from("{}"))
                        .unwrap(),
                )
                .await
                .unwrap();
            assert_eq!(resp.status(), expected_status);
            assert_security_headers(resp.headers());
            let body = resp.into_body().collect().await.unwrap().to_bytes();
            let text = String::from_utf8(body.to_vec()).unwrap();
            let json: serde_json::Value = serde_json::from_str(&text).unwrap();
            assert!(json["error"]["message"].as_str().is_some());
            assert!(!text.contains("SECRET_PATH_TOKEN"));
            assert!(!text.contains("AIzaSUPERSECRETCONFIG"));
            assert!(!text.contains("AIzaUPSTREAMSECRET"));
        }

        // Real GeminiProvider must reject raw `..` and `%2e%2e` traversal with 400 (0 network calls).
        let gemini = Arc::new(
            GeminiProvider::new(
                "http://127.0.0.1:1",
                Some("AIzaTestKey123456".to_string()),
                5,
            )
            .unwrap()
            .with_env_fallback(false),
        );
        let app = build_router(ProxyState::new(gemini, Arc::new(NoopUsageSink), true));
        for bad_uri in [
            "/v1beta/models/../secret",
            "/v1beta/models/%2e%2e/secret",
            "/v1beta/models/gemini-4-hydrogen/../../secret",
        ] {
            let resp = app
                .clone()
                .oneshot(
                    Request::builder()
                        .method("POST")
                        .uri(bad_uri)
                        .body(Body::from("{}"))
                        .unwrap(),
                )
                .await
                .unwrap();
            assert_eq!(resp.status(), StatusCode::BAD_REQUEST, "uri={bad_uri}");
            assert_security_headers(resp.headers());
        }
    }

    #[tokio::test]
    async fn propagates_non_2xx_upstream_status_and_body_without_recording_fake_usage() {
        for status in [400u16, 401, 429, 500] {
            // Even if upstream non-2xx body happens to contain a usageMetadata object, proxy MUST NOT record it.
            let raw_error_body = Bytes::from_static(
                b"{\"error\":{\"code\":429,\"message\":\"quota exceeded\"},\"usageMetadata\":{\"promptTokenCount\":999}}",
            );
            let sink = Arc::new(RecordingSink::default());
            let provider = Arc::new(StubProvider::with_status(
                status,
                Some("application/json"),
                raw_error_body.clone(),
            ));
            let state = ProxyState::new(provider, sink.clone(), true);
            let app = build_router(state.clone());

            let resp = app
                .oneshot(
                    Request::builder()
                        .method("POST")
                        .uri("/v1beta/models/gemini-4-hydrogen:generateContent")
                        .body(Body::from("{}"))
                        .unwrap(),
                )
                .await
                .unwrap();
            assert_eq!(resp.status().as_u16(), status);
            assert_eq!(
                resp.headers()
                    .get(header::CONTENT_TYPE)
                    .and_then(|v| v.to_str().ok()),
                Some("application/json")
            );
            assert!(resp.headers().get(header::X_CONTENT_TYPE_OPTIONS).is_none());
            let body = resp.into_body().collect().await.unwrap().to_bytes();
            assert_eq!(body, raw_error_body);
            assert!(sink.snapshot().is_empty());
            assert_eq!(state.stats().snapshot().requests, 0);
            assert_eq!(state.stats().snapshot().prompt_tokens, 0);
        }
    }

    #[tokio::test]
    async fn enforces_http_method_allowlist_and_404_with_security_headers() {
        let provider = Arc::new(StubProvider::ok_stream(None, Vec::new()));
        let app = build_router(ProxyState::new(
            provider.clone(),
            Arc::new(NoopUsageSink),
            true,
        ));

        let disallowed = [
            ("HEAD", "/health"),
            ("HEAD", "/api/stats"),
            ("HEAD", "/v1beta/models/gemini-4-hydrogen:generateContent"),
            ("PUT", "/health"),
            ("DELETE", "/api/stats"),
            ("PATCH", "/v1beta/models/gemini-4-hydrogen:generateContent"),
            ("OPTIONS", "/health"),
            ("TRACE", "/health"),
            ("POST", "/health"),
            ("POST", "/api/stats"),
            ("GET", "/v1beta/models/gemini-4-hydrogen:generateContent"),
        ];

        for (method, uri) in disallowed {
            let resp = app
                .clone()
                .oneshot(
                    Request::builder()
                        .method(method)
                        .uri(uri)
                        .body(Body::empty())
                        .unwrap(),
                )
                .await
                .unwrap();
            assert_eq!(
                resp.status(),
                StatusCode::METHOD_NOT_ALLOWED,
                "{method} {uri}"
            );
            assert_security_headers(resp.headers());
            assert_eq!(
                resp.headers()
                    .get(header::CONTENT_TYPE)
                    .and_then(|v| v.to_str().ok()),
                Some("application/json"),
                "{method} {uri}"
            );
            let body = resp.into_body().collect().await.unwrap().to_bytes();
            if method == "HEAD" {
                // RFC 9110 §9.3.2: HEAD responses must not carry a message body.
                assert!(body.is_empty(), "{method} {uri}");
            } else {
                let json: serde_json::Value = serde_json::from_slice(&body)
                    .unwrap_or_else(|err| panic!("{method} {uri}: {err}"));
                assert_eq!(json["error"]["message"], "method not allowed");
            }
        }

        for (method, uri) in [
            ("GET", "/unknown"),
            ("POST", "/unknown"),
            ("POST", "/v1beta/models/"),
            ("GET", "/v1beta/models/"),
        ] {
            let resp = app
                .clone()
                .oneshot(
                    Request::builder()
                        .method(method)
                        .uri(uri)
                        .body(Body::empty())
                        .unwrap(),
                )
                .await
                .unwrap();
            assert_eq!(resp.status(), StatusCode::NOT_FOUND, "{method} {uri}");
            assert_security_headers(resp.headers());
            let body = resp.into_body().collect().await.unwrap().to_bytes();
            let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
            assert_eq!(json["error"]["message"], "not found");
        }

        assert_eq!(provider.hit_count(), 0);
    }

    #[tokio::test]
    async fn enforces_64mib_body_limit_allowing_3mib_and_rejecting_over_64mib_with_413() {
        let provider = Arc::new(StubProvider::ok_stream(
            Some("application/json"),
            vec![Bytes::from_static(b"{}")],
        ));
        let app = build_router(ProxyState::new(
            provider.clone(),
            Arc::new(NoopUsageSink),
            true,
        ));

        // 3 MiB passes (proves Axum's default 2 MiB limit was raised).
        let payload_3mib = Bytes::from(vec![b'a'; 3 * 1024 * 1024]);
        let resp_ok = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/v1beta/models/gemini-4-hydrogen:generateContent")
                    .body(Body::from(payload_3mib.clone()))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp_ok.status(), StatusCode::OK);
        assert_eq!(provider.hit_count(), 1);
        assert_eq!(
            provider.captured_requests()[0].body.len(),
            payload_3mib.len()
        );

        // 64 MiB + 1 byte -> 413 Payload Too Large JSON + security headers, 0 additional upstream hits.
        let payload_over = Bytes::from(vec![b'b'; MAX_PROXY_BODY_BYTES + 1]);
        let resp_413 = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/v1beta/models/gemini-4-hydrogen:generateContent")
                    .body(Body::from(payload_over))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp_413.status(), StatusCode::PAYLOAD_TOO_LARGE);
        assert_security_headers(resp_413.headers());
        let body_413 = resp_413.into_body().collect().await.unwrap().to_bytes();
        let json_413: serde_json::Value = serde_json::from_slice(&body_413).unwrap();
        assert_eq!(
            json_413["error"]["message"],
            "request body exceeds maximum allowed size"
        );
        assert_eq!(provider.hit_count(), 1);
    }

    #[tokio::test]
    async fn preserves_alt_sse_strips_key_query_and_handles_invalid_upstream_content_type() {
        // 1. Real GeminiProvider against loopback mock upstream verifies ?alt=sse preserved & ?key= stripped.
        let seen_uri = Arc::new(Mutex::new(None::<String>));
        let seen_key = Arc::new(Mutex::new(None::<String>));
        let seen_uri_clone = Arc::clone(&seen_uri);
        let seen_key_clone = Arc::clone(&seen_key);

        let upstream_router = Router::new().route(
            "/v1beta/models/{*rest}",
            post(move |OriginalUri(uri): OriginalUri, headers: HeaderMap| {
                let seen_uri_clone = Arc::clone(&seen_uri_clone);
                let seen_key_clone = Arc::clone(&seen_key_clone);
                async move {
                    *seen_uri_clone.lock().unwrap() = Some(uri.to_string());
                    *seen_key_clone.lock().unwrap() = headers
                        .get("x-goog-api-key")
                        .and_then(|v| v.to_str().ok())
                        .map(str::to_string);
                    Response::builder()
                        .status(StatusCode::OK)
                        .header(header::CONTENT_TYPE, "text/event-stream")
                        .body(Body::from(
                            "data: {\"usageMetadata\":{\"promptTokenCount\":12,\"candidatesTokenCount\":4}}\n\n",
                        ))
                        .unwrap()
                }
            }),
        );

        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let server_handle = tokio::spawn(async move {
            axum::serve(listener, upstream_router).await.unwrap();
        });

        let provider = Arc::new(
            GeminiProvider::new(
                format!("http://{addr}"),
                Some("AIzaRealConfiguredKey9999".to_string()),
                10,
            )
            .unwrap()
            .with_env_fallback(false),
        );
        let sink = Arc::new(RecordingSink::default());
        let app = build_router(ProxyState::new(provider, sink.clone(), true));

        let resp = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/v1beta/models/gemini-4-hydrogen:streamGenerateContent?alt=sse&key=SECRET_QUERY_LEAK")
                    .body(Body::from("{}"))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let _ = resp.into_body().collect().await.unwrap();
        server_handle.abort();

        let forwarded_uri = seen_uri.lock().unwrap().clone().expect("upstream hit");
        assert!(forwarded_uri.contains("alt=sse"));
        assert!(!forwarded_uri.contains("key="));
        assert!(!forwarded_uri.contains("SECRET_QUERY_LEAK"));
        assert_eq!(
            seen_key.lock().unwrap().as_deref(),
            Some("AIzaRealConfiguredKey9999")
        );
        assert_eq!(sink.snapshot().len(), 1);

        // 2. Malformed upstream Content-Type (\r\n header injection) safely omitted without panic.
        let bad_ct_provider = Arc::new(StubProvider::ok_stream(
            Some("text/event-stream\r\nX-Injected: evil"),
            vec![Bytes::from_static(
                b"data: {\"usageMetadata\":{\"promptTokenCount\":7,\"candidatesTokenCount\":3}}\n\n",
            )],
        ));
        let sink2 = Arc::new(RecordingSink::default());
        let app2 = build_router(ProxyState::new(bad_ct_provider, sink2.clone(), true));
        let resp2 = app2
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/v1beta/models/gemini-4-hydrogen:streamGenerateContent?alt=sse")
                    .body(Body::from("{}"))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp2.status(), StatusCode::OK);
        assert!(resp2.headers().get(header::CONTENT_TYPE).is_none());
        assert!(resp2.headers().get("x-injected").is_none());
        let _ = resp2.into_body().collect().await.unwrap();
        assert_eq!(sink2.snapshot().len(), 1);
    }

    #[tokio::test]
    async fn mid_stream_upstream_error_and_early_client_disconnect_do_not_record_partial_usage() {
        // 1. Mid-stream upstream error after chunk 1.
        let sink_err = Arc::new(RecordingSink::default());
        let provider_err = Arc::new(StubProvider {
            status: 200,
            content_type: Some("text/event-stream".to_string()),
            chunks: vec![
                Ok(Bytes::from_static(
                    b"data: {\"usageMetadata\":{\"promptTokenCount\":100,\"candidatesTokenCount\":5}}\n\n",
                )),
                Err("connection reset mid-stream".to_string()),
            ],
            error_kind: None,
            hits: AtomicUsize::new(0),
            captured: Mutex::new(Vec::new()),
        });
        let state_err = ProxyState::new(provider_err, sink_err.clone(), true);
        let app_err = build_router(state_err.clone());

        let resp_err = app_err
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/v1beta/models/gemini-4-hydrogen:streamGenerateContent?alt=sse")
                    .body(Body::from("{}"))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp_err.status(), StatusCode::OK);
        let collect_res = resp_err.into_body().collect().await;
        assert!(collect_res.is_err());
        assert!(sink_err.snapshot().is_empty());
        assert_eq!(state_err.stats().snapshot().requests, 0);

        // 2. Early client disconnect (drop response body after reading only chunk 1 before EOF).
        let sink_drop = Arc::new(RecordingSink::default());
        let provider_drop = Arc::new(StubProvider::ok_stream(
            Some("text/event-stream"),
            vec![
                Bytes::from_static(
                    b"data: {\"usageMetadata\":{\"promptTokenCount\":100,\"candidatesTokenCount\":5}}\n\n",
                ),
                Bytes::from_static(
                    b"data: {\"usageMetadata\":{\"promptTokenCount\":100,\"candidatesTokenCount\":25}}\n\n",
                ),
            ],
        ));
        let state_drop = ProxyState::new(provider_drop, sink_drop.clone(), true);
        let app_drop = build_router(state_drop.clone());

        let resp_drop = app_drop
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/v1beta/models/gemini-4-hydrogen:streamGenerateContent?alt=sse")
                    .body(Body::from("{}"))
                    .unwrap(),
            )
            .await
            .unwrap();
        let mut body_drop = resp_drop.into_body();
        let _first = body_drop.frame().await.expect("first chunk").unwrap();
        drop(body_drop);

        assert!(sink_drop.snapshot().is_empty());
        assert_eq!(state_drop.stats().snapshot().requests, 0);
    }

    #[tokio::test]
    async fn unary_json_tee_overflow_still_streams_all_bytes_verbatim() {
        let oversized_len = DEFAULT_MAX_JSON_TEE_BYTES + 1024;
        let chunk_a = Bytes::from(vec![b'x'; DEFAULT_MAX_JSON_TEE_BYTES]);
        let chunk_b = Bytes::from(vec![b'y'; 1024]);

        let sink = Arc::new(RecordingSink::default());
        let provider = Arc::new(StubProvider::ok_stream(
            Some("application/json"),
            vec![chunk_a.clone(), chunk_b.clone()],
        ));
        let state = ProxyState::new(provider, sink.clone(), true);
        let app = build_router(state.clone());

        let resp = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/v1beta/models/gemini-4-hydrogen:generateContent")
                    .body(Body::from("{}"))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let body = resp.into_body().collect().await.unwrap().to_bytes();
        assert_eq!(body.len(), oversized_len);
        assert_eq!(&body[..DEFAULT_MAX_JSON_TEE_BYTES], chunk_a.as_ref());
        assert_eq!(&body[DEFAULT_MAX_JSON_TEE_BYTES..], chunk_b.as_ref());
        assert!(sink.snapshot().is_empty());
        assert_eq!(state.stats().snapshot().requests, 0);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn rejects_oversized_content_length_upfront_before_reading_body() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        use tokio::net::TcpStream;

        let provider = Arc::new(StubProvider::ok_stream(
            Some("application/json"),
            vec![Bytes::from_static(b"{}")],
        ));
        let state = ProxyState::new(provider.clone(), Arc::new(NoopUsageSink), true);
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let app = build_router(state);
        let server = tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });

        let oversized_len = (MAX_PROXY_BODY_BYTES as u64) + 1;

        for (label, raw_request) in [
            (
                "single oversized Content-Length with 0 body bytes",
                format!(
                    "POST /v1beta/models/gemini-4-hydrogen:generateContent HTTP/1.1\r\nHost: 127.0.0.1\r\nContent-Type: application/json\r\nContent-Length: {oversized_len}\r\n\r\n"
                ),
            ),
            (
                "duplicate identical oversized Content-Length with 0 body bytes",
                format!(
                    "POST /v1beta/models/gemini-4-hydrogen:generateContent HTTP/1.1\r\nHost: 127.0.0.1\r\nContent-Type: application/json\r\nContent-Length: {oversized_len}\r\nContent-Length: {oversized_len}\r\n\r\n"
                ),
            ),
        ] {
            let mut stream = TcpStream::connect(addr).await.unwrap();
            stream.write_all(raw_request.as_bytes()).await.unwrap();
            stream.flush().await.unwrap();

            let mut buf = vec![0u8; 4096];
            let n = tokio::time::timeout(std::time::Duration::from_secs(1), stream.read(&mut buf))
                .await
                .unwrap_or_else(|_| panic!("{label}: timed out waiting for upfront 413 response"))
                .expect("read response");
            assert!(n > 0, "{label}: empty response");
            let resp_text = String::from_utf8_lossy(&buf[..n]);
            assert!(
                resp_text.starts_with("HTTP/1.1 413 "),
                "{label}: expected HTTP 413, got: {resp_text}"
            );
            let lower = resp_text.to_ascii_lowercase();
            assert!(
                lower.contains("x-content-type-options: nosniff"),
                "{label}: missing nosniff header: {resp_text}"
            );
            assert!(
                lower.contains("cache-control: no-store"),
                "{label}: missing no-store header: {resp_text}"
            );
            assert!(
                resp_text.contains("request body exceeds maximum allowed size"),
                "{label}: missing JSON error message: {resp_text}"
            );
        }

        assert_eq!(provider.hit_count(), 0, "upstream must receive 0 hits");
        server.abort();
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn rejects_chunked_body_over_64mib_with_413_and_malformed_content_length_with_400() {
        let provider = Arc::new(StubProvider::ok_stream(
            Some("application/json"),
            vec![Bytes::from_static(b"{}")],
        ));
        let app = build_router(ProxyState::new(
            provider.clone(),
            Arc::new(NoopUsageSink),
            true,
        ));

        // 1. Streaming (no Content-Length / chunked) body > 64 MiB -> 413 via DefaultBodyLimit.
        let chunk_size = 1024 * 1024;
        let chunk = Bytes::from(vec![b'a'; chunk_size]);
        let chunks: Vec<Result<Bytes, std::io::Error>> = (0..64)
            .map(|_| Ok(chunk.clone()))
            .chain(std::iter::once(Ok(Bytes::from_static(b"b"))))
            .collect();
        let streaming_body = Body::from_stream(stream::iter(chunks));
        let resp = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/v1beta/models/gemini-4-hydrogen:generateContent")
                    .body(streaming_body)
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::PAYLOAD_TOO_LARGE);
        assert_security_headers(resp.headers());

        // 2. Malformed Content-Length values -> 400 Bad Request via upfront guard.
        for bad_cl in ["abc", "-1", "", "12 34"] {
            let resp = app
                .clone()
                .oneshot(
                    Request::builder()
                        .method("POST")
                        .uri("/v1beta/models/gemini-4-hydrogen:generateContent")
                        .header(header::CONTENT_LENGTH, bad_cl)
                        .body(Body::empty())
                        .unwrap(),
                )
                .await
                .unwrap();
            assert_eq!(
                resp.status(),
                StatusCode::BAD_REQUEST,
                "expected 400 for Content-Length '{bad_cl}'"
            );
            assert_security_headers(resp.headers());
        }

        // 3. Conflicting Content-Length headers -> 400 Bad Request.
        let resp_conflict = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/v1beta/models/gemini-4-hydrogen:generateContent")
                    .header(header::CONTENT_LENGTH, "10")
                    .header(header::CONTENT_LENGTH, "20")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp_conflict.status(), StatusCode::BAD_REQUEST);
        assert_security_headers(resp_conflict.headers());

        // 4. u64::MAX + 1 Content-Length overflow -> rejected upfront with 413 (or 400 over wire Hyper).
        let resp_overflow = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/v1beta/models/gemini-4-hydrogen:generateContent")
                    .header(header::CONTENT_LENGTH, "18446744073709551616")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp_overflow.status(), StatusCode::PAYLOAD_TOO_LARGE);
        assert_security_headers(resp_overflow.headers());

        // 5. Real TCP client sending malformed Content-Length (u64::MAX + 1, abc, -1) -> 400 from Hyper.
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        for wire_bad in ["abc", "-1", "18446744073709551616"] {
            use tokio::io::{AsyncReadExt, AsyncWriteExt};
            let mut tcp = tokio::net::TcpStream::connect(addr).await.unwrap();
            let raw = format!(
                "POST /v1beta/models/gemini-4-hydrogen:generateContent HTTP/1.1\r\nHost: 127.0.0.1\r\nContent-Length: {wire_bad}\r\n\r\n"
            );
            tcp.write_all(raw.as_bytes()).await.unwrap();
            let mut buf = vec![0u8; 1024];
            let n = tokio::time::timeout(std::time::Duration::from_secs(1), tcp.read(&mut buf))
                .await
                .expect("read 400 response")
                .unwrap();
            let text = String::from_utf8_lossy(&buf[..n]);
            assert!(
                text.starts_with("HTTP/1.1 400 "),
                "wire Content-Length '{wire_bad}' expected 400, got: {text}"
            );
        }
        server.abort();

        assert_eq!(provider.hit_count(), 0, "upstream must receive 0 hits");
    }

    #[tokio::test]
    async fn rejects_unsupported_non_gemini_3_or_4_models_with_400_and_zero_upstream_hits() {
        let sink = Arc::new(RecordingSink::default());
        let provider = Arc::new(StubProvider::with_status(
            200,
            Some("application/json"),
            Bytes::from_static(b"{\"candidates\":[]}"),
        ));
        let app = build_router(ProxyState::new(
            Arc::clone(&provider) as Arc<dyn companion_providers::ProviderAdapter>,
            Arc::clone(&sink) as Arc<dyn UsageSink>,
            true,
        ));

        for uri in [
            "/v1beta/models/gemini-2.5-pro:generateContent",
            "/v1beta/models/gemini-2.5-flash:streamGenerateContent?alt=sse",
            "/v1beta/models/gemini-2.0-flash:generateContent",
            "/v1beta/models/gemini-1.5-pro:generateContent",
            "/v1beta/models/models/gemini-2.5-pro:generateContent",
            "/v1beta/models/gemini-30-pro:generateContent",
            "/v1beta/models/gpt-4o:generateContent",
        ] {
            let resp = app
                .clone()
                .oneshot(
                    Request::builder()
                        .method("POST")
                        .uri(uri)
                        .header(header::CONTENT_TYPE, "application/json")
                        .body(Body::from("{}"))
                        .unwrap(),
                )
                .await
                .unwrap();
            assert_eq!(resp.status(), StatusCode::BAD_REQUEST, "uri={uri}");
            assert_security_headers(resp.headers());
            let body = axum::body::to_bytes(resp.into_body(), 1024 * 1024)
                .await
                .unwrap();
            let text = String::from_utf8_lossy(&body);
            assert!(
                text.contains("Gemini 3 and Gemini 4")
                    && text.contains("unset GOOGLE_GEMINI_BASE_URL to reach Gemini API directly"),
                "expected clear error with unset hint for {uri}, got: {text}"
            );
        }

        assert_eq!(provider.hit_count(), 0, "upstream must receive 0 hits");
        assert!(sink.snapshot().is_empty(), "must record 0 usage events");
    }

    #[tokio::test]
    async fn get_v1beta_models_returns_curated_fallback_and_live_filtered_catalog_with_security_headers()
     {
        // 1. StubProvider falls back cleanly to curated Gemini 4/3 catalog with security headers.
        let sink = Arc::new(RecordingSink::default());
        let stub = Arc::new(StubProvider::with_status(
            200,
            Some("application/json"),
            Bytes::from_static(b"{}"),
        ));
        let app = build_router(ProxyState::new(
            Arc::clone(&stub) as Arc<dyn companion_providers::ProviderAdapter>,
            Arc::clone(&sink) as Arc<dyn UsageSink>,
            true,
        ));

        let resp = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("GET")
                    .uri("/v1beta/models?pageSize=1000")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        assert_security_headers(resp.headers());
        let body = axum::body::to_bytes(resp.into_body(), 1024 * 1024)
            .await
            .unwrap();
        let parsed: serde_json::Value = serde_json::from_slice(&body).unwrap();
        let models = parsed["models"].as_array().expect("models array");
        assert_eq!(models[0]["name"], "models/gemini-4-hydrogen");
        assert_eq!(models[0]["thinking"], true);
        assert!(
            models
                .iter()
                .any(|m| m["name"] == "models/gemini-3.8-flash")
        );

        // POST /v1beta/models must still return 405 Method Not Allowed.
        let post_resp = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/v1beta/models")
                    .body(Body::from("{}"))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(post_resp.status(), StatusCode::METHOD_NOT_ALLOWED);
        assert_security_headers(post_resp.headers());

        // 2. Real GeminiProvider against a mock upstream returns filtered live catalog with default_model first.
        let upstream_router = Router::new().route(
            "/v1beta/models",
            axum::routing::get(|| async {
                let payload = serde_json::json!({
                    "models": [
                        {
                            "name": "models/gemini-2.5-pro",
                            "displayName": "Gemini 2.5 Pro",
                            "supportedGenerationMethods": ["generateContent"]
                        },
                        {
                            "name": "models/gemini-4-carbon",
                            "displayName": "Gemini 4 Carbon",
                            "inputTokenLimit": 1048576,
                            "outputTokenLimit": 65536,
                            "supportedGenerationMethods": ["generateContent"]
                        },
                        {
                            "name": "models/gemini-3.8-flash",
                            "displayName": "Gemini 3.8 Flash",
                            "inputTokenLimit": 1048576,
                            "outputTokenLimit": 65536,
                            "supportedGenerationMethods": ["generateContent"]
                        }
                    ]
                });
                (
                    StatusCode::OK,
                    [(header::CONTENT_TYPE, "application/json")],
                    payload.to_string(),
                )
            }),
        );
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let upstream_addr = listener.local_addr().unwrap();
        let upstream_server = tokio::spawn(async move {
            axum::serve(listener, upstream_router).await.unwrap();
        });

        let mut cfg = companion_core::Config::default();
        cfg.gemini.base_url = format!("http://{upstream_addr}");
        cfg.gemini.api_key = Some("AIzaSyGatewayDiscovery123".to_string());
        cfg.gemini.default_model = "gemini-3.8-flash".to_string();
        let live_state =
            ProxyState::from_config_with_env_fallback(cfg, None, Arc::new(NoopUsageSink), false)
                .unwrap();
        let live_app = build_router(live_state);

        let live_resp = live_app
            .oneshot(
                Request::builder()
                    .method("GET")
                    .uri("/v1beta/models?pageSize=1000")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(live_resp.status(), StatusCode::OK);
        assert_security_headers(live_resp.headers());
        let live_body = axum::body::to_bytes(live_resp.into_body(), 1024 * 1024)
            .await
            .unwrap();
        let live_parsed: serde_json::Value = serde_json::from_slice(&live_body).unwrap();
        let live_models = live_parsed["models"].as_array().expect("live models array");
        let names: Vec<&str> = live_models
            .iter()
            .filter_map(|m| m["name"].as_str())
            .collect();
        assert_eq!(
            names,
            vec!["models/gemini-3.8-flash", "models/gemini-4-carbon"]
        );

        upstream_server.abort();
    }

    #[tokio::test]
    async fn cloudcode_fetch_available_models_merges_and_v1internal_streams_passthrough() {
        let seen_auth = Arc::new(Mutex::new(Vec::<String>::new()));
        let seen_auth_clone = Arc::clone(&seen_auth);

        let mock_ccpa = Router::new()
            .route(
                "/v1internal:fetchAvailableModels",
                axum::routing::post({
                    let seen_auth = Arc::clone(&seen_auth_clone);
                    move |headers: HeaderMap, body: Bytes| {
                        let seen_auth = Arc::clone(&seen_auth);
                        async move {
                            if let Some(auth) =
                                headers.get(header::AUTHORIZATION).and_then(|v| v.to_str().ok())
                            {
                                seen_auth.lock().unwrap().push(auth.to_string());
                            }
                            assert_eq!(
                                headers
                                    .get(header::ACCEPT_ENCODING)
                                    .and_then(|v| v.to_str().ok()),
                                Some("identity")
                            );
                            let _ = body;
                            (
                                StatusCode::OK,
                                [(header::CONTENT_TYPE, "application/json")],
                                serde_json::json!({
                                    "models": {
                                        "gemini-4-carbon-high": {
                                            "model": "MODEL_PLACEHOLDER_M427",
                                            "displayName": "Gemini 4 Carbon (High)",
                                            "thinkingLevel": 3
                                        }
                                    },
                                    "defaultAgentModelId": "gemini-4-carbon-high",
                                    "agentModelSorts": [{
                                        "displayName": "Recommended",
                                        "groups": [{
                                            "displayName": "Google AI Pro",
                                            "modelIds": ["gemini-4-carbon-high"]
                                        }]
                                    }],
                                    "groupedModels": {
                                        "gemini-4-carbon": {
                                            "displayName": "Gemini 4 Carbon",
                                            "supportedThinkingLevels": [
                                                { "thinkingSuffix": "high", "thinkingDisplayName": "High" }
                                            ]
                                        }
                                    }
                                })
                                .to_string(),
                            )
                        }
                    }
                }),
            )
            .route(
                "/v1internal:streamGenerateContent",
                axum::routing::post({
                    let seen_auth = Arc::clone(&seen_auth_clone);
                    move |headers: HeaderMap, OriginalUri(uri): OriginalUri, body: Bytes| {
                        let seen_auth = Arc::clone(&seen_auth);
                        async move {
                            if let Some(auth) =
                                headers.get(header::AUTHORIZATION).and_then(|v| v.to_str().ok())
                            {
                                seen_auth.lock().unwrap().push(auth.to_string());
                            }
                            assert_eq!(uri.query(), Some("alt=sse"));
                            assert_eq!(body.as_ref(), br#"{"model":"gemini-4-carbon-high"}"#);
                            (
                                StatusCode::OK,
                                [(header::CONTENT_TYPE, "text/event-stream")],
                                "data: {\"response\":{\"text\":\"ok\"}}\n\n",
                            )
                        }
                    }
                }),
            );

        let ccpa_listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let ccpa_addr = ccpa_listener.local_addr().unwrap();
        let ccpa_server = tokio::spawn(async move {
            let _ = axum::serve(ccpa_listener, mock_ccpa).await;
        });

        let mut cfg = companion_core::Config {
            cloudcode_upstream_url: format!("http://{ccpa_addr}"),
            ..Default::default()
        };
        cfg.gemini.base_url = format!("http://{ccpa_addr}");
        cfg.gemini.api_key = Some("AIzaSyCloudCodeMergeKey1".to_string());
        cfg.gemini.default_model = "gemini-4-hydrogen".to_string();
        let state =
            ProxyState::from_config_with_env_fallback(cfg, None, Arc::new(NoopUsageSink), false)
                .unwrap();
        let app = build_router(state);

        // 1. POST /v1internal:fetchAvailableModels preserves originals & injects proxy models.
        let fetch_resp = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/v1internal:fetchAvailableModels")
                    .header(header::AUTHORIZATION, "Bearer google-ai-pro-token")
                    .header(header::CONTENT_TYPE, "application/json")
                    .body(Body::from(r#"{"project":"personal"}"#))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(fetch_resp.status(), StatusCode::OK);
        assert_security_headers(fetch_resp.headers());
        let fetch_body = axum::body::to_bytes(fetch_resp.into_body(), 1024 * 1024)
            .await
            .unwrap();
        let parsed: serde_json::Value = serde_json::from_slice(&fetch_body).unwrap();
        assert_eq!(
            parsed["models"]["gemini-4-carbon-high"]["displayName"],
            "Gemini 4 Carbon (High)"
        );
        assert_eq!(
            parsed["models"]["companion/gemini-4-hydrogen-xhigh"]["modelUrl"],
            "gemini-api://gemini-4-hydrogen"
        );
        assert_eq!(
            parsed["models"]["companion/gemini-4-hydrogen-xhigh"]["thinkingLevel"],
            7
        );
        assert_eq!(
            parsed["models"]["companion/gemini-4-hydrogen-xhigh"]["displayName"],
            "Gemini 4 Hydrogen (Proxy) (XHigh)"
        );
        assert_eq!(
            parsed["groupedModels"]["gemini-4-carbon"]["displayName"],
            "Gemini 4 Carbon"
        );
        assert_eq!(
            parsed["groupedModels"]["companion/gemini-4-hydrogen"]["displayName"],
            "Gemini 4 Hydrogen (Proxy)"
        );
        let group0_ids: Vec<&str> = parsed["agentModelSorts"][0]["groups"][0]["modelIds"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(serde_json::Value::as_str)
            .collect();
        assert_eq!(group0_ids[0], "gemini-4-carbon-high");
        assert!(group0_ids.contains(&"companion/gemini-4-hydrogen-xhigh"));

        // Verify /api/stats reports catalog refresh & CloudCode metadata.
        let stats_resp = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("GET")
                    .uri("/api/stats")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(stats_resp.status(), StatusCode::OK);
        let stats_body = axum::body::to_bytes(stats_resp.into_body(), 1024 * 1024)
            .await
            .unwrap();
        let stats_snap: crate::StatsSnapshot = serde_json::from_slice(&stats_body).unwrap();
        assert!(stats_snap.last_catalog_refresh_at_ms.is_some());
        assert_eq!(stats_snap.last_catalog_included_proxy_models, Some(true));
        assert_eq!(stats_snap.cloudcode_mode, "fixed");
        assert_eq!(
            stats_snap.cloudcode_resolved_upstream.as_deref(),
            Some(format!("http://{ccpa_addr}").as_str())
        );

        // 2. POST /v1internal:streamGenerateContent?alt=sse passes through byte-for-byte.
        let stream_resp = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/v1internal:streamGenerateContent?alt=sse")
                    .header(header::AUTHORIZATION, "Bearer google-ai-pro-token")
                    .header(header::CONTENT_TYPE, "application/json")
                    .body(Body::from(r#"{"model":"gemini-4-carbon-high"}"#))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(stream_resp.status(), StatusCode::OK);
        let stream_body = axum::body::to_bytes(stream_resp.into_body(), 1024 * 1024)
            .await
            .unwrap();
        assert_eq!(
            stream_body.as_ref(),
            b"data: {\"response\":{\"text\":\"ok\"}}\n\n"
        );

        // 3. S3/S7 rejections on /v1internal paths.
        let put_resp = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("PUT")
                    .uri("/v1internal:loadCodeAssist")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(put_resp.status(), StatusCode::METHOD_NOT_ALLOWED);

        let traversal_resp = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/v1internal/foo/../secret")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(traversal_resp.status(), StatusCode::BAD_REQUEST);

        let big_resp = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/v1internal:loadCodeAssist")
                    .header(
                        header::CONTENT_LENGTH,
                        (MAX_PROXY_BODY_BYTES + 1).to_string(),
                    )
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(big_resp.status(), StatusCode::PAYLOAD_TOO_LARGE);

        assert_eq!(
            *seen_auth.lock().unwrap(),
            vec![
                "Bearer google-ai-pro-token".to_string(),
                "Bearer google-ai-pro-token".to_string(),
            ]
        );

        ccpa_server.abort();
    }

    #[tokio::test]
    async fn cloudcode_fetch_available_models_day0_returns_upstream_untouched_preserving_content_encoding()
     {
        let raw_upstream = r#"{"models":{"gemini-4-carbon-high":{"model":"MODEL_PLACEHOLDER_M427","displayName":"Gemini 4 Carbon (High)"}},"defaultAgentModelId":"gemini-4-carbon-high"}"#;
        let mock_ccpa = Router::new().route(
            "/v1internal:fetchAvailableModels",
            axum::routing::post(move || async move {
                (
                    StatusCode::OK,
                    [
                        (header::CONTENT_TYPE, "application/json; charset=utf-8"),
                        (header::CONTENT_ENCODING, "gzip"),
                    ],
                    raw_upstream,
                )
            }),
        );
        let ccpa_listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let ccpa_addr = ccpa_listener.local_addr().unwrap();
        let ccpa_server = tokio::spawn(async move {
            let _ = axum::serve(ccpa_listener, mock_ccpa).await;
        });

        for api_key in [
            None,
            Some(companion_core::PROXY_PLACEHOLDER_API_KEY.to_string()),
        ] {
            let mut cfg = companion_core::Config {
                cloudcode_upstream_url: format!("http://{ccpa_addr}"),
                ..Default::default()
            };
            cfg.gemini.base_url = format!("http://{ccpa_addr}");
            cfg.gemini.api_key = api_key;
            let state = ProxyState::from_config_with_env_fallback(
                cfg,
                None,
                Arc::new(NoopUsageSink),
                false,
            )
            .unwrap();
            let app = build_router(state);

            let resp = app
                .clone()
                .oneshot(
                    Request::builder()
                        .method("POST")
                        .uri("/v1internal:fetchAvailableModels")
                        .header(header::CONTENT_TYPE, "application/json")
                        .body(Body::from("{}"))
                        .unwrap(),
                )
                .await
                .unwrap();
            assert_eq!(resp.status(), StatusCode::OK);
            assert_security_headers(resp.headers());
            assert_eq!(
                resp.headers()
                    .get(header::CONTENT_TYPE)
                    .and_then(|v| v.to_str().ok()),
                Some("application/json; charset=utf-8")
            );
            assert_eq!(
                resp.headers()
                    .get(header::CONTENT_ENCODING)
                    .and_then(|v| v.to_str().ok()),
                Some("gzip")
            );
            let body = axum::body::to_bytes(resp.into_body(), 1024 * 1024)
                .await
                .unwrap();
            assert_eq!(body.as_ref(), raw_upstream.as_bytes());
            assert!(!String::from_utf8_lossy(&body).contains("companion/"));

            let stats_resp = app
                .oneshot(
                    Request::builder()
                        .method("GET")
                        .uri("/api/stats")
                        .body(Body::empty())
                        .unwrap(),
                )
                .await
                .unwrap();
            let stats_body = axum::body::to_bytes(stats_resp.into_body(), 1024 * 1024)
                .await
                .unwrap();
            let snap: crate::StatsSnapshot = serde_json::from_slice(&stats_body).unwrap();
            assert!(snap.last_catalog_refresh_at_ms.is_some());
            assert_eq!(snap.last_catalog_included_proxy_models, Some(false));
        }

        ccpa_server.abort();
    }

    #[tokio::test]
    async fn cloudcode_fetch_available_models_preserves_content_encoding_on_merge_error_fallback() {
        let raw_non_json = b"\x1f\x8bnot-json-compressed-payload";
        let mock_ccpa = Router::new().route(
            "/v1internal:fetchAvailableModels",
            axum::routing::post(move || async move {
                (
                    StatusCode::OK,
                    [
                        (header::CONTENT_TYPE, "application/json"),
                        (header::CONTENT_ENCODING, "gzip"),
                    ],
                    raw_non_json.as_slice(),
                )
            }),
        );
        let ccpa_listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let ccpa_addr = ccpa_listener.local_addr().unwrap();
        let ccpa_server = tokio::spawn(async move {
            let _ = axum::serve(ccpa_listener, mock_ccpa).await;
        });

        let mut cfg = companion_core::Config {
            cloudcode_upstream_url: format!("http://{ccpa_addr}"),
            ..Default::default()
        };
        cfg.gemini.base_url = format!("http://{ccpa_addr}");
        cfg.gemini.api_key = Some("AIzaSyMergeFallbackKey12".to_string());
        let state =
            ProxyState::from_config_with_env_fallback(cfg, None, Arc::new(NoopUsageSink), false)
                .unwrap();
        let app = build_router(state);

        let resp = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/v1internal:fetchAvailableModels")
                    .body(Body::from("{}"))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        assert_security_headers(resp.headers());
        assert_eq!(
            resp.headers()
                .get(header::CONTENT_ENCODING)
                .and_then(|v| v.to_str().ok()),
            Some("gzip")
        );
        let body = axum::body::to_bytes(resp.into_body(), 1024 * 1024)
            .await
            .unwrap();
        assert_eq!(body.as_ref(), raw_non_json);

        let stats_resp = app
            .oneshot(
                Request::builder()
                    .method("GET")
                    .uri("/api/stats")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        let stats_body = axum::body::to_bytes(stats_resp.into_body(), 1024 * 1024)
            .await
            .unwrap();
        let snap: crate::StatsSnapshot = serde_json::from_slice(&stats_body).unwrap();
        assert_eq!(snap.last_catalog_included_proxy_models, Some(false));

        ccpa_server.abort();
    }

    #[tokio::test]
    async fn validate_inbound_api_key_quarantines_placeholder_sentinel() {
        let mut h = HeaderMap::new();
        h.insert(
            &X_GOOG_API_KEY_HEADER,
            HeaderValue::from_static("agy-companion-local"),
        );
        assert_eq!(validate_inbound_api_key(&h, false), Ok(None));
        assert_eq!(validate_inbound_api_key(&h, true), Ok(None));

        let mut h_padded = HeaderMap::new();
        h_padded.insert(
            &X_GOOG_API_KEY_HEADER,
            HeaderValue::from_static("  agy-companion-local  "),
        );
        assert_eq!(validate_inbound_api_key(&h_padded, false), Ok(None));

        // End-to-end unconfigured proxy: placeholder x-goog-api-key yields api_key_override == None.
        let sink = Arc::new(RecordingSink::default());
        let stub = Arc::new(StubProvider::with_status(
            200,
            Some("application/json"),
            Bytes::from_static(b"{}"),
        ));
        let app = build_router(ProxyState::new(
            Arc::clone(&stub) as Arc<dyn companion_providers::ProviderAdapter>,
            Arc::clone(&sink) as Arc<dyn UsageSink>,
            false,
        ));

        let resp = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/v1beta/models/gemini-4-hydrogen:generateContent")
                    .header("x-goog-api-key", "agy-companion-local")
                    .header(header::CONTENT_TYPE, "application/json")
                    .body(Body::from("{}"))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let seen = stub.captured_requests();
        assert_eq!(seen.len(), 1);
        assert_eq!(seen[0].api_key_override, None);
    }
}
