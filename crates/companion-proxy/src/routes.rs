use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};
use std::time::Instant;

use axum::body::Body;
use axum::extract::rejection::BytesRejection;
use axum::extract::{DefaultBodyLimit, OriginalUri, Request, State};
use axum::middleware::{self, Next};
use axum::response::{IntoResponse, Response};
use axum::routing::{MethodFilter, on};
use axum::{Json, Router};
use bytes::Bytes;
use companion_core::{GeminiUsageMetadata, UsageEvent, UsageSink};
use companion_providers::{ByteStream, ForwardRequest, ProviderError};
use futures_util::Stream;
use http::header::{self, HeaderMap, HeaderName, HeaderValue};
use http::{Method, StatusCode};
use serde::{Deserialize, Serialize};
use tower_http::set_header::SetResponseHeaderLayer;

use crate::sse::SseUsageTracker;
use crate::stats::ProxyStats;
use crate::{PROTOCOL_VERSION, ProxyState};

pub const MAX_PROXY_BODY_BYTES: usize = 64 * 1024 * 1024;
pub const DEFAULT_MAX_JSON_TEE_BYTES: usize = 8 * 1024 * 1024;

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
            on(MethodFilter::GET, stats_handler).layer(internal_security_headers),
        )
        .route(
            "/v1beta/models/{*rest}",
            on(MethodFilter::POST, proxy_gemini_handler)
                .layer(DefaultBodyLimit::max(MAX_PROXY_BODY_BYTES)),
        )
        .method_not_allowed_fallback(method_not_allowed_handler)
        .route_layer(middleware::from_fn(enforce_get_post_only))
        .fallback(not_found_handler)
        .with_state(state)
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

async fn health_handler() -> Json<HealthResponse> {
    Json(HealthResponse {
        status: "ok".to_string(),
        version: env!("CARGO_PKG_VERSION").to_string(),
        protocol_version: PROTOCOL_VERSION,
    })
}

async fn stats_handler(State(state): State<ProxyState>) -> impl IntoResponse {
    Json(state.stats().snapshot())
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
        })
    } else {
        Body::from_stream(upstream.stream)
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
    if trimmed.is_empty() {
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

struct UsageTeeStream {
    inner: ByteStream,
    collector: UsageCollector,
    stats: Arc<ProxyStats>,
    usage_sink: Arc<dyn UsageSink>,
    model: String,
    started_at: Instant,
    finalized: bool,
    errored: bool,
}

impl Stream for UsageTeeStream {
    type Item = Result<Bytes, ProviderError>;

    fn poll_next(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        let this = self.get_mut();
        match Pin::new(&mut this.inner).poll_next(cx) {
            Poll::Pending => Poll::Pending,
            Poll::Ready(Some(Ok(bytes))) => {
                this.collector.push_chunk(bytes.as_ref());
                Poll::Ready(Some(Ok(bytes)))
            }
            Poll::Ready(Some(Err(err))) => {
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
                    .uri("/v1beta/models/gemini-2.5-pro:streamGenerateContent?alt=sse")
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
        assert_eq!(events[0].model, "gemini-2.5-pro");
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
                    .uri("/v1beta/models/gemini-2.5-flash:streamGenerateContent?alt=sse")
                    .body(Body::from("{}"))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);

        let mut body = resp.into_body();
        let first_frame = tokio::time::timeout(std::time::Duration::from_secs(2), body.frame())
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
        assert_eq!(events[0].model, "gemini-2.5-flash");
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
                    .uri("/v1beta/models/models/gemini-2.5-pro:generateContent")
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
        assert_eq!(sink.snapshot()[0].model, "gemini-2.5-pro");
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
                    .uri("/v1beta/models/qwen2.5:7b:streamGenerateContent")
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
        assert_eq!(events2[0].model, "qwen2.5:7b");
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
                    .uri("/v1beta/models/gemini-2.5-pro:generateContent")
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
                    .uri("/v1beta/models/gemini-2.5-pro:generateContent")
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
                    .uri("/v1beta/models/gemini-2.5-pro:generateContent")
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
                            .uri("/v1beta/models/gemini-2.5-pro:generateContent")
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
                    .uri("/v1beta/models/gemini-2.5-pro:generateContent")
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
                        .uri("/v1beta/models/gemini-2.5-pro:generateContent")
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
                        .uri("/v1beta/models/gemini-2.5-pro:generateContent")
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
                        .uri("/v1beta/models/gemini-2.5-pro:generateContent")
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
                    .uri("/v1beta/models/gemini-2.5-pro:generateContent")
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
                        .uri("/v1beta/models/gemini-2.5-pro:generateContent")
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
            "/v1beta/models/gemini-2.5-pro/../../secret",
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
                        .uri("/v1beta/models/gemini-2.5-pro:generateContent")
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
            ("HEAD", "/v1beta/models/gemini-2.5-pro:generateContent"),
            ("PUT", "/health"),
            ("DELETE", "/api/stats"),
            ("PATCH", "/v1beta/models/gemini-2.5-pro:generateContent"),
            ("OPTIONS", "/health"),
            ("TRACE", "/health"),
            ("POST", "/health"),
            ("POST", "/api/stats"),
            ("GET", "/v1beta/models/gemini-2.5-pro:generateContent"),
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
                    .uri("/v1beta/models/gemini-2.5-pro:generateContent")
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
                    .uri("/v1beta/models/gemini-2.5-pro:generateContent")
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
                    .uri("/v1beta/models/gemini-2.5-pro:streamGenerateContent?alt=sse&key=SECRET_QUERY_LEAK")
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
                    .uri("/v1beta/models/gemini-2.5-pro:streamGenerateContent?alt=sse")
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
                    .uri("/v1beta/models/gemini-2.5-pro:streamGenerateContent?alt=sse")
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
                    .uri("/v1beta/models/gemini-2.5-pro:streamGenerateContent?alt=sse")
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
                    .uri("/v1beta/models/gemini-2.5-pro:generateContent")
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
}
