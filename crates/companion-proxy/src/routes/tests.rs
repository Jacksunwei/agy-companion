use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use axum::Router;
use axum::body::Body;
use axum::extract::OriginalUri;
use axum::response::Response;
use axum::routing::post;
use bytes::Bytes;
use companion_core::{
    ApiKey, CloudCodeUpstream, HealthResponse, NoopUsageSink, UsageEvent, UsageSink,
};
use companion_providers::{
    BoxFuture, ForwardRequest, ForwardResponse, GeminiProvider, ProviderAdapter, ProviderError,
};
use futures_util::{StreamExt, stream};
use http::header::{self, HeaderMap, HeaderValue};
use http::{Request, StatusCode};
use http_body_util::BodyExt;
use tokio::net::TcpListener;
use tokio::sync::{Notify, mpsc};
use tower::ServiceExt;

use super::middleware::{
    BodyReadError, X_GOOG_API_KEY_HEADER, read_bounded_request_body, validate_inbound_api_key,
};
use super::thought_sig::{
    SniffedErrorPrefix, THOUGHT_SIG_ERROR_SNIFF_MAX_BYTES, replay_buffered_stream,
    sniff_error_prefix,
};
use super::{DEFAULT_MAX_JSON_TEE_BYTES, MAX_PROXY_BODY_BYTES, build_router};
use crate::{PROTOCOL_VERSION, ProxyState, ProxyStateOptions, StatsSnapshot};

fn test_proxy_state(
    provider: Arc<dyn ProviderAdapter>,
    usage_sink: Arc<dyn UsageSink>,
    has_configured_api_key: bool,
) -> ProxyState {
    ProxyState::new(provider, usage_sink, has_configured_api_key)
        .expect("build test ProxyState with default CloudCodeClient")
}

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
    let state = test_proxy_state(provider, Arc::new(NoopUsageSink), true);
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
    assert_eq!(health.role, None);
    let raw_json = std::str::from_utf8(&body).unwrap();
    assert!(!raw_json.contains("\"role\""));
}

#[tokio::test]
async fn health_includes_configured_role_when_set_in_options() {
    let state = ProxyState::from_config_with_options(
        companion_core::Config::default(),
        Arc::new(NoopUsageSink),
        ProxyStateOptions {
            env_fallback: false,
            role: Some("bootstrap".to_string()),
            ..ProxyStateOptions::default()
        },
    )
    .expect("build ProxyState with role");
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
    let body = resp.into_body().collect().await.unwrap().to_bytes();
    let health: HealthResponse = serde_json::from_slice(&body).unwrap();
    assert_eq!(health.role.as_deref(), Some("bootstrap"));
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
    let state = test_proxy_state(provider, sink.clone(), true);
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
    assert_eq!(snap.usage.requests, 1);
    assert_eq!(snap.usage.prompt_tokens, 100);
    assert_eq!(snap.usage.cached_tokens, 60);
    assert_eq!(snap.usage.output_tokens, 25);
    assert_eq!(snap.usage.reasoning_tokens, 8);
    assert!((snap.usage.cache_hit_ratio - 0.6).abs() < 1e-12);
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
    let state = test_proxy_state(provider, sink.clone(), true);
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
    let state = test_proxy_state(provider.clone(), sink.clone(), true);
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
    let state2 = test_proxy_state(provider2, sink2.clone(), true);
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
    let app_configured = build_router(test_proxy_state(
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
    let app_unconfigured = build_router(test_proxy_state(
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
    let app_unconfigured = build_router(test_proxy_state(
        provider.clone(),
        Arc::new(NoopUsageSink),
        false,
    ));
    let app_configured = build_router(test_proxy_state(
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
        let app = build_router(test_proxy_state(provider, Arc::new(NoopUsageSink), true));
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
    let app = build_router(test_proxy_state(gemini, Arc::new(NoopUsageSink), true));
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
        let state = test_proxy_state(provider, sink.clone(), true);
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
        assert_eq!(state.stats().snapshot().usage.requests, 0);
        assert_eq!(state.stats().snapshot().usage.prompt_tokens, 0);
    }
}

#[tokio::test]
async fn enforces_http_method_allowlist_and_404_with_security_headers() {
    let provider = Arc::new(StubProvider::ok_stream(None, Vec::new()));
    let app = build_router(test_proxy_state(
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
            let json: serde_json::Value =
                serde_json::from_slice(&body).unwrap_or_else(|err| panic!("{method} {uri}: {err}"));
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
    let app = build_router(test_proxy_state(
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
    let app = build_router(test_proxy_state(provider, sink.clone(), true));

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
    let app2 = build_router(test_proxy_state(bad_ct_provider, sink2.clone(), true));
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
    let state_err = test_proxy_state(provider_err, sink_err.clone(), true);
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
    assert_eq!(state_err.stats().snapshot().usage.requests, 0);

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
    let state_drop = test_proxy_state(provider_drop, sink_drop.clone(), true);
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
    assert_eq!(state_drop.stats().snapshot().usage.requests, 0);
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
    let state = test_proxy_state(provider, sink.clone(), true);
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
    assert_eq!(state.stats().snapshot().usage.requests, 0);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn rejects_oversized_content_length_upfront_before_reading_body() {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpStream;

    let provider = Arc::new(StubProvider::ok_stream(
        Some("application/json"),
        vec![Bytes::from_static(b"{}")],
    ));
    let state = test_proxy_state(provider.clone(), Arc::new(NoopUsageSink), true);
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
    let app = build_router(test_proxy_state(
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
    let app = build_router(test_proxy_state(
        Arc::clone(&provider) as Arc<dyn companion_providers::ProviderAdapter>,
        Arc::clone(&sink) as Arc<dyn UsageSink>,
        true,
    ));

    for uri in [
        "/v1beta/models/gemini-2.5-pro:generateContent",
        "/v1beta/models/gemini-2.5-flash:streamGenerateContent?alt=sse",
        "/v1beta/models/gemini-2.0-flash:generateContent",
        "/v1beta/models/gemini-1.5-pro:generateContent",
        "/v1beta/models/gemini-3.7-flash:generateContent",
        "/v1beta/models/gemini-3.1-pro-preview:generateContent",
        "/v1beta/models/models/gemini-2.5-pro:generateContent",
        "/v1beta/models/gemini-3-pro:generateContent",
        "/v1beta/models/gemini-5:generateContent",
        "/v1beta/models/gemini-5-:generateContent",
        "/v1beta/models/gemini-5pro:generateContent",
        "/v1beta/models/llama-3-70b:generateContent",
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
            text.contains("Gemini 3.8+ and Gemini 4+")
                && text.contains("unset GOOGLE_GEMINI_BASE_URL to reach Gemini API directly"),
            "expected clear error with unset hint for {uri}, got: {text}"
        );
    }

    assert_eq!(provider.hit_count(), 0, "upstream must receive 0 hits");
    assert!(sink.snapshot().is_empty(), "must record 0 usage events");
}

#[tokio::test]
async fn accepts_gemini_5_and_3_9_models_and_rejects_bare_gemini_5() {
    let sink = Arc::new(RecordingSink::default());
    let provider = Arc::new(StubProvider::ok_stream(
        Some("application/json"),
        vec![Bytes::from_static(
            br#"{"candidates":[{"content":{"parts":[{"text":"ok"}]}}]}"#,
        )],
    ));
    let app = build_router(test_proxy_state(
        Arc::clone(&provider) as Arc<dyn companion_providers::ProviderAdapter>,
        Arc::clone(&sink) as Arc<dyn UsageSink>,
        true,
    ));

    for uri in [
        "/v1beta/models/gemini-5-pro:generateContent",
        "/v1beta/models/gemini-3.9-pro:generateContent",
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
        assert_eq!(resp.status(), StatusCode::OK, "uri={uri}");
    }
    assert_eq!(
        provider.hit_count(),
        2,
        "both gemini-5-pro and gemini-3.9-pro must reach upstream"
    );

    let bare_resp = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/v1beta/models/gemini-5:generateContent")
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from("{}"))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(bare_resp.status(), StatusCode::BAD_REQUEST);
    assert_eq!(
        provider.hit_count(),
        2,
        "bare gemini-5 must be rejected before hitting upstream"
    );
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
    let app = build_router(test_proxy_state(
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
    cfg.providers.gemini.base_url = format!("http://{upstream_addr}");
    cfg.providers.gemini.default_model = "gemini-3.8-flash".to_string();
    let auth_store = Arc::new(companion_core::MemoryAuthStore::with_gemini_key(
        ApiKey::parse_strict("AIzaSyGatewayDiscovery123").unwrap(),
    ));
    let live_state = ProxyState::from_config_with_options(
        cfg,
        Arc::new(NoopUsageSink),
        ProxyStateOptions {
            config_path: None,
            auth_store: Some(auth_store),
            env_fallback: false,
            ..Default::default()
        },
    )
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

    let mut cfg = companion_core::Config::default();
    cfg.cloudcode.upstream = CloudCodeUpstream::Fixed(format!("http://{ccpa_addr}"));
    cfg.providers.gemini.base_url = format!("http://{ccpa_addr}");
    cfg.providers.gemini.default_model = "gemini-4-hydrogen".to_string();
    let auth_store = Arc::new(companion_core::MemoryAuthStore::with_gemini_key(
        ApiKey::parse_strict("AIzaSyCloudCodeMergeKey1").unwrap(),
    ));
    let state = ProxyState::from_config_with_options(
        cfg,
        Arc::new(NoopUsageSink),
        ProxyStateOptions {
            config_path: None,
            auth_store: Some(auth_store),
            env_fallback: false,
            ..Default::default()
        },
    )
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
        "Gemini 4 Hydrogen (AI Studio) (XHigh)"
    );
    assert_eq!(
        parsed["groupedModels"]["gemini-4-carbon"]["displayName"],
        "Gemini 4 Carbon"
    );
    assert_eq!(
        parsed["groupedModels"]["companion/gemini-4-hydrogen"]["displayName"],
        "Gemini 4 Hydrogen (AI Studio)"
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
    assert!(stats_snap.cloudcode.last_catalog_refresh_at_ms.is_some());
    assert_eq!(
        stats_snap.cloudcode.last_catalog_included_proxy_models,
        Some(true)
    );
    assert_eq!(stats_snap.cloudcode.mode, "fixed");
    assert_eq!(
        stats_snap.cloudcode.resolved_upstream.as_deref(),
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

    // Test Day-0 across: (1) missing auth, (2) legacy config.json with api_key ignored,
    // and (3) auth/gemini.json containing quarantined PROXY_PLACEHOLDER_API_KEY.
    let tmp = tempfile::TempDir::new().unwrap();
    for (idx, raw_api_key) in [None, Some(companion_core::PROXY_PLACEHOLDER_API_KEY)]
        .into_iter()
        .enumerate()
    {
        let case_dir = tmp.path().join(format!("day0-{idx}"));
        std::fs::create_dir_all(&case_dir).unwrap();
        let cfg_path = case_dir.join("config.json");
        let json = serde_json::json!({
            "schema_version": 1,
            "cloudcode": {
                "upstream": format!("http://{ccpa_addr}")
            },
            "providers": {
                "gemini": {
                    "base_url": format!("http://{ccpa_addr}"),
                    "api_key": raw_api_key
                }
            }
        })
        .to_string();
        std::fs::write(&cfg_path, &json).unwrap();
        if let Some(ph) = raw_api_key {
            let auth_dir = companion_core::auth_dir_for_config_path(&cfg_path).unwrap();
            std::fs::create_dir_all(&auth_dir).unwrap();
            std::fs::write(
                auth_dir.join(companion_core::GEMINI_AUTH_FILENAME),
                serde_json::json!({ "schema_version": 1, "api_key": ph }).to_string(),
            )
            .unwrap();
        }
        let cfg = companion_core::Config::from_json_str(&json, &cfg_path).unwrap();
        let state = ProxyState::from_config_with_options(
            cfg,
            Arc::new(NoopUsageSink),
            ProxyStateOptions {
                config_path: Some(cfg_path),
                auth_store: None,
                env_fallback: false,
                ..Default::default()
            },
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
        assert!(snap.cloudcode.last_catalog_refresh_at_ms.is_some());
        assert_eq!(
            snap.cloudcode.last_catalog_included_proxy_models,
            Some(false)
        );
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

    let mut cfg = companion_core::Config::default();
    cfg.cloudcode.upstream = CloudCodeUpstream::Fixed(format!("http://{ccpa_addr}"));
    cfg.providers.gemini.base_url = format!("http://{ccpa_addr}");
    let auth_store = Arc::new(companion_core::MemoryAuthStore::with_gemini_key(
        ApiKey::parse_strict("AIzaSyMergeFallbackKey12").unwrap(),
    ));
    let state = ProxyState::from_config_with_options(
        cfg,
        Arc::new(NoopUsageSink),
        ProxyStateOptions {
            config_path: None,
            auth_store: Some(auth_store),
            env_fallback: false,
            ..Default::default()
        },
    )
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
    assert_eq!(
        snap.cloudcode.last_catalog_included_proxy_models,
        Some(false)
    );

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
    let app = build_router(test_proxy_state(
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

#[tokio::test]
async fn retries_once_without_thought_signature_on_400_invalid_thought_signature() {
    struct ThoughtSigFallbackProvider {
        seen: std::sync::Mutex<Vec<Bytes>>,
    }

    impl ProviderAdapter for ThoughtSigFallbackProvider {
        fn name(&self) -> &'static str {
            "thought-sig-test"
        }

        fn forward_gemini(
            &self,
            req: ForwardRequest,
        ) -> BoxFuture<'_, Result<ForwardResponse, ProviderError>> {
            let attempt = {
                let mut guard = self.seen.lock().unwrap();
                guard.push(req.body.clone());
                guard.len()
            };
            Box::pin(async move {
                if attempt == 1 {
                    let err_body = Bytes::from_static(
                        br#"{"error":{"code":400,"message":"Corrupted thought signature.","status":"INVALID_ARGUMENT","details":[{"reason":"INVALID_THOUGHT_SIGNATURE"}]}}"#,
                    );
                    Ok(ForwardResponse {
                        status: 400,
                        content_type: Some("application/json".to_string()),
                        stream: Box::pin(futures_util::stream::once(async move { Ok(err_body) })),
                    })
                } else {
                    let ok_body = Bytes::from_static(
                        br#"{"candidates":[],"usageMetadata":{"promptTokenCount":12,"candidatesTokenCount":5,"totalTokenCount":17}}"#,
                    );
                    Ok(ForwardResponse {
                        status: 200,
                        content_type: Some("application/json".to_string()),
                        stream: Box::pin(futures_util::stream::once(async move { Ok(ok_body) })),
                    })
                }
            })
        }

        fn health_check(&self) -> BoxFuture<'_, Result<(), ProviderError>> {
            Box::pin(async { Ok(()) })
        }
    }

    let provider = Arc::new(ThoughtSigFallbackProvider {
        seen: std::sync::Mutex::new(Vec::new()),
    });
    let sink = Arc::new(RecordingSink::default());
    let app = build_router(test_proxy_state(
        Arc::clone(&provider) as Arc<dyn companion_providers::ProviderAdapter>,
        Arc::clone(&sink) as Arc<dyn UsageSink>,
        true,
    ));

    let payload = br#"{"contents":[{"role":"model","parts":[{"text":"hi","thoughtSignature":"vertex-sig-abc"},{"thought":true,"text":"plan","thought_signature":"vertex-sig-xyz"}]}]}"#;
    let resp = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/v1beta/models/gemini-4-neon:generateContent")
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(Bytes::from_static(payload)))
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(resp.status(), StatusCode::OK);
    let resp_bytes = axum::body::to_bytes(resp.into_body(), 64 * 1024)
        .await
        .unwrap();
    assert!(resp_bytes.windows(13).any(|w| w == b"usageMetadata"));

    let seen = provider.seen.lock().unwrap();
    assert_eq!(seen.len(), 2, "expected initial attempt + 1 stripped retry");
    assert_eq!(
        seen[0].as_ref(),
        payload,
        "first attempt must be byte-for-byte untouched"
    );
    let retried: serde_json::Value = serde_json::from_slice(&seen[1]).unwrap();
    let parts = retried["contents"][0]["parts"].as_array().unwrap();
    assert!(parts[0].get("thoughtSignature").is_none());
    assert_eq!(parts[0]["text"], "hi");
    assert!(parts[1].get("thought_signature").is_none());
    assert_eq!(parts[1]["text"], "plan");
}

#[tokio::test]
async fn does_not_retry_unrelated_400_even_when_thought_signature_present() {
    let sink = Arc::new(RecordingSink::default());
    let err_body = Bytes::from_static(br#"{"error":{"code":400,"message":"Bad schema"}}"#);
    let stub = Arc::new(StubProvider::with_status(
        400,
        Some("application/json"),
        err_body.clone(),
    ));
    let app = build_router(test_proxy_state(
        Arc::clone(&stub) as Arc<dyn companion_providers::ProviderAdapter>,
        Arc::clone(&sink) as Arc<dyn UsageSink>,
        true,
    ));

    let resp = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/v1beta/models/gemini-4-neon:generateContent")
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(
                    r#"{"contents":[{"parts":[{"thoughtSignature":"abc"}]}]}"#,
                ))
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    let body = axum::body::to_bytes(resp.into_body(), 64 * 1024)
        .await
        .unwrap();
    assert_eq!(body, err_body);
    assert_eq!(stub.captured_requests().len(), 1);
}

#[tokio::test]
async fn thought_sig_400_marker_past_64kib_does_not_retry_and_replays_256kib_intact() {
    struct Large400Provider {
        hits: AtomicUsize,
        err_body: Bytes,
    }

    impl ProviderAdapter for Large400Provider {
        fn name(&self) -> &'static str {
            "large-400-thought-sig-test"
        }

        fn forward_gemini(
            &self,
            _req: ForwardRequest,
        ) -> BoxFuture<'_, Result<ForwardResponse, ProviderError>> {
            let attempt = self.hits.fetch_add(1, Ordering::SeqCst) + 1;
            let err_body = self.err_body.clone();
            Box::pin(async move {
                if attempt == 1 {
                    Ok(ForwardResponse {
                        status: 400,
                        content_type: Some("application/json".to_string()),
                        stream: Box::pin(futures_util::stream::once(async move { Ok(err_body) })),
                    })
                } else {
                    Ok(ForwardResponse {
                        status: 200,
                        content_type: Some("application/json".to_string()),
                        stream: Box::pin(futures_util::stream::once(async {
                            Ok(Bytes::from_static(b"{\"candidates\":[]}"))
                        })),
                    })
                }
            })
        }

        fn health_check(&self) -> BoxFuture<'_, Result<(), ProviderError>> {
            Box::pin(async { Ok(()) })
        }
    }

    let mut payload = vec![b'A'; 256 * 1024];
    let marker = b"INVALID_THOUGHT_SIGNATURE";
    let offset = 100 * 1024;
    payload[offset..offset + marker.len()].copy_from_slice(marker);
    let expected_err_body = Bytes::from(payload);

    let provider = Arc::new(Large400Provider {
        hits: AtomicUsize::new(0),
        err_body: expected_err_body.clone(),
    });
    let sink = Arc::new(RecordingSink::default());
    let app = build_router(test_proxy_state(
        Arc::clone(&provider) as Arc<dyn companion_providers::ProviderAdapter>,
        Arc::clone(&sink) as Arc<dyn UsageSink>,
        true,
    ));

    let resp = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/v1beta/models/gemini-4-neon:generateContent")
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(
                    r#"{"contents":[{"parts":[{"thoughtSignature":"abc"}]}]}"#,
                ))
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(
        resp.status(),
        StatusCode::BAD_REQUEST,
        "marker past 64 KiB must NOT trigger a retry"
    );
    assert_eq!(provider.hits.load(Ordering::SeqCst), 1);
    let body = axum::body::to_bytes(resp.into_body(), 512 * 1024)
        .await
        .unwrap();
    assert_eq!(body, expected_err_body);
}

#[tokio::test]
async fn cloudcode_fallback_maps_client_disconnect_to_400_and_streaming_overflow_to_413() {
    let mut cfg = companion_core::Config::default();
    cfg.cloudcode.upstream = CloudCodeUpstream::Fixed("http://127.0.0.1:1".to_string());
    let state = ProxyState::from_config_with_options(
        cfg,
        Arc::new(NoopUsageSink),
        ProxyStateOptions {
            config_path: None,
            auth_store: None,
            env_fallback: false,
            ..Default::default()
        },
    )
    .unwrap();
    let app = build_router(state);

    // (a) Immediate stream error -> 400 Bad Request
    let immediate_err_stream = stream::iter(vec![Err::<Bytes, std::io::Error>(
        std::io::Error::other("simulated disconnect"),
    )]);
    let resp = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/v1internal:streamGenerateContent")
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from_stream(immediate_err_stream))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(
        resp.status(),
        StatusCode::BAD_REQUEST,
        "client disconnect must return 400, not 413"
    );
    assert_security_headers(resp.headers());
    let body = resp.into_body().collect().await.unwrap().to_bytes();
    let err_json: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(err_json["error"]["message"], "failed to read request body");

    // (b) Mid-body stream error -> 400 Bad Request
    let mid_err_stream = stream::iter(vec![
        Ok(Bytes::from_static(b"{\"partial\":")),
        Err(std::io::Error::other("reset")),
    ]);
    let resp = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/v1internal:streamGenerateContent")
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from_stream(mid_err_stream))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    assert_security_headers(resp.headers());
    let body = resp.into_body().collect().await.unwrap().to_bytes();
    let err_json: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(err_json["error"]["message"], "failed to read request body");

    // (c) Streaming body > 64 MiB without Content-Length -> 413 Payload Too Large
    let one_mib = Bytes::from(vec![b'x'; 1024 * 1024]);
    let mut chunks: Vec<Result<Bytes, std::io::Error>> =
        (0..64).map(|_| Ok(one_mib.clone())).collect();
    chunks.push(Ok(Bytes::from_static(b"x")));
    let overflow_stream = stream::iter(chunks);
    let resp = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/v1internal:streamGenerateContent")
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from_stream(overflow_stream))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::PAYLOAD_TOO_LARGE);
    assert_security_headers(resp.headers());
    let body = resp.into_body().collect().await.unwrap().to_bytes();
    let err_json: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(
        err_json["error"]["message"],
        "request body exceeds maximum allowed size"
    );
}

#[tokio::test]
async fn thought_sig_non_matching_256kib_400_and_retry_failure_replay_byte_for_byte() {
    // (a) Single 256 KiB non-thought-sig chunk + trailing 8 KiB chunk -> 400, 1 hit, exact 264 KiB match.
    let chunk1: Vec<u8> = (0usize..(256 * 1024))
        .map(|i| ((i.wrapping_mul(31).wrapping_add(7)) % 251) as u8)
        .collect();
    let chunk2: Vec<u8> = (0usize..(8 * 1024))
        .map(|i| ((i.wrapping_mul(17).wrapping_add(13)) % 251) as u8)
        .collect();
    let mut expected_264k = Vec::with_capacity(chunk1.len() + chunk2.len());
    expected_264k.extend_from_slice(&chunk1);
    expected_264k.extend_from_slice(&chunk2);

    let stub = Arc::new(StubProvider {
        status: 400,
        content_type: Some("application/json".to_string()),
        chunks: vec![Ok(Bytes::from(chunk1)), Ok(Bytes::from(chunk2))],
        error_kind: None,
        hits: AtomicUsize::new(0),
        captured: Mutex::new(Vec::new()),
    });
    let app = build_router(test_proxy_state(
        Arc::clone(&stub) as Arc<dyn ProviderAdapter>,
        Arc::new(NoopUsageSink),
        true,
    ));
    let resp = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/v1beta/models/gemini-4-neon:generateContent")
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(
                    r#"{"contents":[{"parts":[{"thoughtSignature":"abc"}]}]}"#,
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    assert_eq!(stub.hit_count(), 1);
    let body = axum::body::to_bytes(resp.into_body(), 512 * 1024)
        .await
        .unwrap();
    assert_eq!(body.as_ref(), expected_264k.as_slice());

    // (b) Single 128 KiB chunk with INVALID_THOUGHT_SIGNATURE at offset 100 where retry fails ->
    // 400, 2 hits, exact 128 KiB byte-for-byte replay (leftover preserved on retry failure).
    struct FailingRetryProvider {
        hits: AtomicUsize,
        err_body: Bytes,
    }
    impl ProviderAdapter for FailingRetryProvider {
        fn name(&self) -> &'static str {
            "failing-retry-test"
        }
        fn forward_gemini(
            &self,
            _req: ForwardRequest,
        ) -> BoxFuture<'_, Result<ForwardResponse, ProviderError>> {
            let attempt = self.hits.fetch_add(1, Ordering::SeqCst) + 1;
            let err_body = self.err_body.clone();
            Box::pin(async move {
                if attempt == 1 {
                    Ok(ForwardResponse {
                        status: 400,
                        content_type: Some("application/json".to_string()),
                        stream: Box::pin(stream::once(async move { Ok(err_body) })),
                    })
                } else {
                    Err(ProviderError::Upstream("retry fail".to_string()))
                }
            })
        }
        fn health_check(&self) -> BoxFuture<'_, Result<(), ProviderError>> {
            Box::pin(async { Ok(()) })
        }
    }

    let mut payload_128k: Vec<u8> = (0..(128 * 1024)).map(|i| b'a' + (i % 26) as u8).collect();
    let marker = b"INVALID_THOUGHT_SIGNATURE";
    payload_128k[100..100 + marker.len()].copy_from_slice(marker);
    let expected_128k = Bytes::from(payload_128k);

    let provider = Arc::new(FailingRetryProvider {
        hits: AtomicUsize::new(0),
        err_body: expected_128k.clone(),
    });
    let app = build_router(test_proxy_state(
        Arc::clone(&provider) as Arc<dyn ProviderAdapter>,
        Arc::new(NoopUsageSink),
        true,
    ));
    let resp = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/v1beta/models/gemini-4-neon:generateContent")
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(
                    r#"{"contents":[{"parts":[{"thoughtSignature":"abc"}]}]}"#,
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    assert_eq!(provider.hits.load(Ordering::SeqCst), 2);
    let body = axum::body::to_bytes(resp.into_body(), 256 * 1024)
        .await
        .unwrap();
    assert_eq!(body, expected_128k);
}

#[tokio::test]
async fn sniff_error_prefix_handles_exact_64kib_plus_one_multichunk_empty_and_midstream_errors() {
    let max = THOUGHT_SIG_ERROR_SNIFF_MAX_BYTES;

    // 1. Exact 64 KiB chunk + trailing 1 KiB chunk: trailing chunk must NOT be polled during sniff.
    let polled_chunks = Arc::new(AtomicUsize::new(0));
    let polled_clone = Arc::clone(&polled_chunks);
    let exact_64k = Bytes::from(vec![b'E'; max]);
    let tail_1k = Bytes::from(vec![b'T'; 1024]);
    let items: Vec<Result<Bytes, ProviderError>> = vec![Ok(exact_64k.clone()), Ok(tail_1k.clone())];
    let tracked = Box::pin(stream::iter(items).inspect(move |_| {
        polled_clone.fetch_add(1, Ordering::SeqCst);
    }));
    let SniffedErrorPrefix {
        sniffed,
        leftover,
        stream_err,
        remainder,
    } = sniff_error_prefix(tracked, max).await;
    assert_eq!(sniffed.len(), max);
    assert!(leftover.is_none());
    assert!(stream_err.is_none());
    assert_eq!(
        polled_chunks.load(Ordering::SeqCst),
        1,
        "trailing chunk must remain unpolled after hitting exact 64 KiB"
    );
    let mut replayed =
        replay_buffered_stream(Bytes::from(sniffed), leftover, stream_err, remainder);
    let mut out = Vec::new();
    while let Some(item) = replayed.next().await {
        out.extend_from_slice(&item.unwrap());
    }
    assert_eq!(out.len(), max + 1024);
    assert_eq!(&out[..max], exact_64k.as_ref());
    assert_eq!(&out[max..], tail_1k.as_ref());

    // 2. 64 KiB + 1 single chunk -> 64 KiB sniffed + 1 byte leftover, exact 65_537 bytes replayed.
    let mut plus_one = vec![b'P'; max];
    plus_one.push(b'Z');
    let plus_one_chunk = Bytes::from(plus_one.clone());
    let SniffedErrorPrefix {
        sniffed,
        leftover,
        stream_err,
        remainder,
    } = sniff_error_prefix(
        Box::pin(stream::once(async move { Ok(plus_one_chunk) })),
        max,
    )
    .await;
    assert_eq!(sniffed.len(), max);
    assert_eq!(leftover.as_ref().map(Bytes::len), Some(1));
    assert_eq!(leftover.as_deref(), Some(&b"Z"[..]));
    assert!(stream_err.is_none());
    let mut replayed =
        replay_buffered_stream(Bytes::from(sniffed), leftover, stream_err, remainder);
    let mut out = Vec::new();
    while let Some(item) = replayed.next().await {
        out.extend_from_slice(&item.unwrap());
    }
    assert_eq!(out, plus_one);

    // 3. Multi-chunk [empty, 40 KiB, empty, 40 KiB, 16 KiB] -> 64 KiB sniffed + 16 KiB leftover, 96 KiB total.
    let c1 = Bytes::from(vec![b'1'; 40 * 1024]);
    let c2 = Bytes::from(vec![b'2'; 40 * 1024]);
    let c3 = Bytes::from(vec![b'3'; 16 * 1024]);
    let SniffedErrorPrefix {
        sniffed,
        leftover,
        stream_err,
        remainder,
    } = sniff_error_prefix(
        Box::pin(stream::iter(vec![
            Ok(Bytes::new()),
            Ok(c1.clone()),
            Ok(Bytes::new()),
            Ok(c2.clone()),
            Ok(c3.clone()),
        ])),
        max,
    )
    .await;
    assert_eq!(sniffed.len(), max);
    assert_eq!(leftover.as_ref().map(Bytes::len), Some(16 * 1024));
    assert!(stream_err.is_none());
    let mut replayed =
        replay_buffered_stream(Bytes::from(sniffed), leftover, stream_err, remainder);
    let mut out = Vec::new();
    while let Some(item) = replayed.next().await {
        out.extend_from_slice(&item.unwrap());
    }
    assert_eq!(out.len(), 96 * 1024);
    assert_eq!(&out[..40 * 1024], c1.as_ref());
    assert_eq!(&out[40 * 1024..80 * 1024], c2.as_ref());
    assert_eq!(&out[80 * 1024..], c3.as_ref());

    // 4. 80 KiB chunk followed by Err("after-leftover") -> Ok(64 KiB) -> Ok(16 KiB) -> Err.
    let chunk_80k = Bytes::from(vec![b'8'; 80 * 1024]);
    let SniffedErrorPrefix {
        sniffed,
        leftover,
        stream_err,
        remainder,
    } = sniff_error_prefix(
        Box::pin(stream::iter(vec![
            Ok(chunk_80k),
            Err(ProviderError::Upstream("after-leftover".to_string())),
        ])),
        max,
    )
    .await;
    assert_eq!(sniffed.len(), max);
    assert_eq!(leftover.as_ref().map(Bytes::len), Some(16 * 1024));
    assert!(stream_err.is_none());
    let mut replayed =
        replay_buffered_stream(Bytes::from(sniffed), leftover, stream_err, remainder);
    assert_eq!(replayed.next().await.unwrap().unwrap().len(), max);
    assert_eq!(replayed.next().await.unwrap().unwrap().len(), 16 * 1024);
    match replayed.next().await.unwrap() {
        Err(ProviderError::Upstream(msg)) => assert_eq!(msg, "after-leftover"),
        other => panic!("expected upstream error after leftover, got {other:?}"),
    }

    // 5. 10 KiB chunk followed by Err("before-cap") -> Ok(10 KiB) -> Err -> None.
    let chunk_10k = Bytes::from(vec![b'B'; 10 * 1024]);
    let SniffedErrorPrefix {
        sniffed,
        leftover,
        stream_err,
        remainder,
    } = sniff_error_prefix(
        Box::pin(stream::iter(vec![
            Ok(chunk_10k),
            Err(ProviderError::Upstream("before-cap".to_string())),
        ])),
        max,
    )
    .await;
    assert_eq!(sniffed.len(), 10 * 1024);
    assert!(leftover.is_none());
    assert!(stream_err.is_some());
    let mut replayed =
        replay_buffered_stream(Bytes::from(sniffed), leftover, stream_err, remainder);
    assert_eq!(replayed.next().await.unwrap().unwrap().len(), 10 * 1024);
    match replayed.next().await.unwrap() {
        Err(ProviderError::Upstream(msg)) => assert_eq!(msg, "before-cap"),
        other => panic!("expected upstream error before cap, got {other:?}"),
    }
    assert!(replayed.next().await.is_none());
}

#[tokio::test]
async fn read_bounded_request_body_enforces_exact_boundary_and_stream_errors() {
    // Empty body -> Ok(empty)
    assert_eq!(
        read_bounded_request_body(Body::empty(), 64).await,
        Ok(Bytes::new())
    );

    // Exact 64 bytes single chunk -> Ok(64 bytes)
    let exact_single = Bytes::from(vec![b'a'; 64]);
    assert_eq!(
        read_bounded_request_body(Body::from(exact_single.clone()), 64).await,
        Ok(exact_single)
    );

    // Exact 64 bytes across multiple chunks with empty chunks interleaved -> Ok(64 bytes)
    let multi_exact = Body::from_stream(stream::iter(vec![
        Ok::<Bytes, std::io::Error>(Bytes::new()),
        Ok(Bytes::from(vec![b'x'; 30])),
        Ok(Bytes::new()),
        Ok(Bytes::from(vec![b'y'; 34])),
    ]));
    let collected = read_bounded_request_body(multi_exact, 64).await.unwrap();
    assert_eq!(collected.len(), 64);
    assert_eq!(&collected[..30], &[b'x'; 30]);
    assert_eq!(&collected[30..], &[b'y'; 34]);

    // 65 bytes single chunk -> TooLarge
    assert_eq!(
        read_bounded_request_body(Body::from(vec![b'z'; 65]), 64).await,
        Err(BodyReadError::TooLarge)
    );

    // 65 bytes across 2 chunks -> TooLarge
    let two_chunk_over = Body::from_stream(stream::iter(vec![
        Ok::<Bytes, std::io::Error>(Bytes::from(vec![b'a'; 40])),
        Ok(Bytes::from(vec![b'b'; 25])),
    ]));
    assert_eq!(
        read_bounded_request_body(two_chunk_over, 64).await,
        Err(BodyReadError::TooLarge)
    );

    // 65 bytes across 3 chunks -> TooLarge
    let three_chunk_over = Body::from_stream(stream::iter(vec![
        Ok::<Bytes, std::io::Error>(Bytes::from(vec![b'a'; 20])),
        Ok(Bytes::from(vec![b'b'; 20])),
        Ok(Bytes::from(vec![b'c'; 25])),
    ]));
    assert_eq!(
        read_bounded_request_body(three_chunk_over, 64).await,
        Err(BodyReadError::TooLarge)
    );

    // Stream error after 32 bytes -> ReadFailed
    let mid_err = Body::from_stream(stream::iter(vec![
        Ok::<Bytes, std::io::Error>(Bytes::from(vec![b'a'; 32])),
        Err(std::io::Error::other("connection reset")),
    ]));
    assert_eq!(
        read_bounded_request_body(mid_err, 64).await,
        Err(BodyReadError::ReadFailed)
    );
}

#[tokio::test]
async fn cloudcode_fetch_available_models_empty_enabled_models_returns_upstream_untouched() {
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

    let mut cfg = companion_core::Config::default();
    cfg.cloudcode.upstream =
        companion_core::CloudCodeUpstream::Fixed(format!("http://{ccpa_addr}"));
    cfg.providers.gemini.base_url = format!("http://{ccpa_addr}");
    cfg.providers.gemini.enabled_models = Vec::new();
    let auth_store = Arc::new(companion_core::MemoryAuthStore::with_gemini_key(
        companion_core::ApiKey::parse_strict("AIzaSyEmptyEnabledTest12").unwrap(),
    ));

    let state = ProxyState::from_config_with_options(
        cfg,
        Arc::new(NoopUsageSink),
        ProxyStateOptions {
            config_path: None,
            auth_store: Some(auth_store),
            env_fallback: false,
            ..Default::default()
        },
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
    assert!(snap.cloudcode.last_catalog_refresh_at_ms.is_some());
    assert_eq!(
        snap.cloudcode.last_catalog_included_proxy_models,
        Some(false)
    );

    ccpa_server.abort();
}

#[tokio::test]
async fn cloudcode_fetch_available_models_injects_strictly_enabled_models_only() {
    use std::sync::atomic::{AtomicUsize, Ordering};

    let list_models_hits = Arc::new(AtomicUsize::new(0));
    let hits_clone = Arc::clone(&list_models_hits);

    let raw_upstream = r#"{"models":{"gemini-4-carbon-high":{"model":"MODEL_PLACEHOLDER_M427","displayName":"Gemini 4 Carbon (High)","thinkingLevel":3}},"defaultAgentModelId":"gemini-4-carbon-high"}"#;
    let mock_server = Router::new()
        .route(
            "/v1internal:fetchAvailableModels",
            axum::routing::post(move || async move {
                (
                    StatusCode::OK,
                    [(header::CONTENT_TYPE, "application/json")],
                    raw_upstream,
                )
            }),
        )
        .route(
            "/v1beta/models",
            axum::routing::get(move || {
                let hits = Arc::clone(&hits_clone);
                async move {
                    hits.fetch_add(1, Ordering::SeqCst);
                    (
                        StatusCode::OK,
                        [(header::CONTENT_TYPE, "application/json")],
                        r#"{"models":[{"name":"models/gemini-5-pro"}]}"#,
                    )
                }
            }),
        );
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let handle = tokio::spawn(async move {
        let _ = axum::serve(listener, mock_server).await;
    });

    let mut cfg = companion_core::Config::default();
    cfg.cloudcode.upstream = companion_core::CloudCodeUpstream::Fixed(format!("http://{addr}"));
    cfg.providers.gemini.base_url = format!("http://{addr}");
    cfg.providers.gemini.default_model = "gemini-4-hydrogen".to_string();
    cfg.providers.gemini.enabled_models = vec!["gemini-4-argon".to_string()];
    let auth_store = Arc::new(companion_core::MemoryAuthStore::with_gemini_key(
        companion_core::ApiKey::parse_strict("AIzaSyStrictEnabledOnly1").unwrap(),
    ));

    let state = ProxyState::from_config_with_options(
        cfg,
        Arc::new(NoopUsageSink),
        ProxyStateOptions {
            config_path: None,
            auth_store: Some(auth_store),
            env_fallback: false,
            ..Default::default()
        },
    )
    .unwrap();
    let app = build_router(state);

    let resp = app
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
    let body = axum::body::to_bytes(resp.into_body(), 1024 * 1024)
        .await
        .unwrap();
    let parsed: serde_json::Value = serde_json::from_slice(&body).unwrap();
    let mut injected: Vec<String> = parsed["models"]
        .as_object()
        .unwrap()
        .keys()
        .filter(|k| k.starts_with("companion/"))
        .cloned()
        .collect();
    injected.sort();

    assert_eq!(
        injected,
        vec![
            "companion/gemini-4-argon-high",
            "companion/gemini-4-argon-low",
            "companion/gemini-4-argon-medium",
        ]
    );
    assert_eq!(
        list_models_hits.load(Ordering::SeqCst),
        0,
        "fetchAvailableModels must never call upstream /v1beta/models discovery"
    );

    handle.abort();
}

#[tokio::test]
async fn builtin_antigravity_passive_tee_tracks_sse_unary_path_fallback_and_rss() {
    let sse_payload = concat!(
        "data: {\"response\":{\"candidates\":[{\"content\":{\"parts\":[{\"text\":\"hi\"}]}}]}}\n\n",
        "data: {\"response\":{\"usageMetadata\":{\"promptTokenCount\":1200,\"candidatesTokenCount\":300,\"cachedContentTokenCount\":800,\"thoughtsTokenCount\":40}}}\n\n"
    );
    let unary_payload = r#"{"response":{"candidates":[{"content":{"parts":[{"text":"ok"}]}}],"usageMetadata":{"promptTokenCount":500,"candidatesTokenCount":120,"cachedContentTokenCount":0,"thoughtsTokenCount":10}}}"#;

    let mock_ccpa = Router::new()
        .route(
            "/v1internal:streamGenerateContent",
            post(move || async move {
                (
                    StatusCode::OK,
                    [(header::CONTENT_TYPE, "text/event-stream")],
                    sse_payload,
                )
            }),
        )
        .route(
            "/v1internal:generateContent",
            post(move || async move {
                (
                    StatusCode::OK,
                    [(header::CONTENT_TYPE, "application/json")],
                    unary_payload,
                )
            }),
        )
        .route(
            "/v1internal/projects/p/locations/l/models/gemini-4-carbon:generateContent",
            post(move || async move {
                (
                    StatusCode::OK,
                    [(header::CONTENT_TYPE, "application/json")],
                    unary_payload,
                )
            }),
        );
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let server = tokio::spawn(async move {
        let _ = axum::serve(listener, mock_ccpa).await;
    });

    let mut cfg = companion_core::Config::default();
    cfg.cloudcode.upstream = CloudCodeUpstream::Fixed(format!("http://{addr}"));
    cfg.providers.gemini.base_url = format!("http://{addr}");
    // Day-0: NO Google AI Studio API key configured — built-in tracking still works!
    let sink = Arc::new(RecordingSink::default());
    let state = ProxyState::from_config_with_options(
        cfg,
        sink.clone(),
        ProxyStateOptions {
            config_path: None,
            auth_store: None,
            env_fallback: false,
            ..Default::default()
        },
    )
    .unwrap();
    let app = build_router(state);

    // 1. SSE streamGenerateContent with JSON body model
    let resp1 = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/v1internal:streamGenerateContent?alt=sse")
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(
                    r#"{"model":"models/gemini-3.8-pro","request":{"contents":[]}}"#,
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp1.status(), StatusCode::OK);
    let body1 = axum::body::to_bytes(resp1.into_body(), 1024 * 1024)
        .await
        .unwrap();
    assert_eq!(body1.as_ref(), sse_payload.as_bytes());

    // 2. Unary generateContent with non-JSON body -> fallback "antigravity-builtin"
    let resp2 = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/v1internal:generateContent")
                .header(header::CONTENT_TYPE, "application/octet-stream")
                .body(Body::from("raw-non-json"))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp2.status(), StatusCode::OK);
    let body2 = axum::body::to_bytes(resp2.into_body(), 1024 * 1024)
        .await
        .unwrap();
    assert_eq!(body2.as_ref(), unary_payload.as_bytes());

    // 3. Path-based model extraction when body omits "model"
    let resp3 = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/v1internal/projects/p/locations/l/models/gemini-4-carbon:generateContent")
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(r#"{"request":{}}"#))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp3.status(), StatusCode::OK);
    let _ = axum::body::to_bytes(resp3.into_body(), 1024 * 1024)
        .await
        .unwrap();

    // Verify /api/stats (usage, recent_calls, by_model, proxy_rss_bytes)
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
    let stats_bytes = axum::body::to_bytes(stats_resp.into_body(), 1024 * 1024)
        .await
        .unwrap();
    let snap: StatsSnapshot = serde_json::from_slice(&stats_bytes).unwrap();
    assert_eq!(snap.usage.requests, 3);
    assert_eq!(snap.usage.prompt_tokens, 2200);
    assert_eq!(snap.usage.cached_tokens, 800);
    assert_eq!(snap.usage.output_tokens, 540);
    assert_eq!(snap.usage.reasoning_tokens, 60);
    #[cfg(any(target_os = "macos", target_os = "linux"))]
    assert!(snap.proxy_rss_bytes.is_some_and(|b| b > 0));

    assert_eq!(snap.recent_calls.len(), 3);
    assert_eq!(snap.recent_calls[0].provider, "antigravity");
    assert_eq!(snap.recent_calls[0].source, "builtin");
    assert_eq!(snap.recent_calls[0].model, "gemini-3.8-pro");
    assert_eq!(snap.recent_calls[1].model, "antigravity-builtin");
    assert_eq!(snap.recent_calls[2].model, "gemini-4-carbon");
    assert_eq!(snap.by_model.len(), 3);
    assert_eq!(snap.by_model[0].model, "gemini-3.8-pro");

    // Verify SQLite/RecordingSink received all 3 builtin events
    let events = sink.snapshot();
    assert_eq!(events.len(), 3);
    assert_eq!(events[0].provider, "antigravity");
    assert_eq!(events[0].source, "builtin");
    assert_eq!(events[0].model, "gemini-3.8-pro");
    assert_eq!(events[1].model, "antigravity-builtin");
    assert_eq!(events[2].model, "gemini-4-carbon");

    server.abort();
}

#[tokio::test]
async fn builtin_antigravity_passive_tee_skips_compressed_errors_and_non_generation_rpcs() {
    let raw_compressed: &[u8] = b"\x1f\x8b\x08\x00fake-gzip-stream-payload";
    let identity_sse = "data: {\"response\":{\"usageMetadata\":{\"promptTokenCount\":321,\"candidatesTokenCount\":45}}}\n\n";

    let mock_ccpa = Router::new()
        .route(
            "/v1internal:streamGenerateContent",
            post(move |OriginalUri(uri): OriginalUri| async move {
                let query = uri.query().unwrap_or("");
                let mode = query
                    .split('&')
                    .find_map(|kv| kv.strip_prefix("mode="))
                    .unwrap_or("gzip");
                match mode {
                    "gzip" => Response::builder()
                        .status(StatusCode::OK)
                        .header(header::CONTENT_TYPE, "text/event-stream")
                        .header(header::CONTENT_ENCODING, "gzip")
                        .body(Body::from(raw_compressed))
                        .unwrap(),
                    "br" => Response::builder()
                        .status(StatusCode::OK)
                        .header(header::CONTENT_TYPE, "text/event-stream")
                        .header(header::CONTENT_ENCODING, "br")
                        .body(Body::from(raw_compressed))
                        .unwrap(),
                    "mixed" => Response::builder()
                        .status(StatusCode::OK)
                        .header(header::CONTENT_TYPE, "text/event-stream")
                        .header(header::CONTENT_ENCODING, "identity, gzip")
                        .body(Body::from(raw_compressed))
                        .unwrap(),
                    "identity" => Response::builder()
                        .status(StatusCode::OK)
                        .header(header::CONTENT_TYPE, "text/event-stream")
                        .header(header::CONTENT_ENCODING, "Identity")
                        .body(Body::from(identity_sse))
                        .unwrap(),
                    _ => Response::builder()
                        .status(StatusCode::INTERNAL_SERVER_ERROR)
                        .header(header::CONTENT_TYPE, "application/json")
                        .body(Body::from(
                            r#"{"response":{"usageMetadata":{"promptTokenCount":999}}}"#,
                        ))
                        .unwrap(),
                }
            }),
        )
        .route(
            "/v1internal:loadCodeAssist",
            post(|| async {
                (
                    StatusCode::OK,
                    [(header::CONTENT_TYPE, "application/json")],
                    r#"{"response":{"usageMetadata":{"promptTokenCount":777}}}"#,
                )
            }),
        );

    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let server = tokio::spawn(async move {
        let _ = axum::serve(listener, mock_ccpa).await;
    });

    let mut cfg = companion_core::Config::default();
    cfg.cloudcode.upstream = CloudCodeUpstream::Fixed(format!("http://{addr}"));
    cfg.providers.gemini.base_url = format!("http://{addr}");

    let sink = Arc::new(RecordingSink::default());
    let state = ProxyState::from_config_with_options(
        cfg,
        sink.clone(),
        ProxyStateOptions {
            config_path: None,
            auth_store: None,
            env_fallback: false,
            ..Default::default()
        },
    )
    .unwrap();
    let app = build_router(state.clone());

    // Compressed responses (gzip, br, identity+gzip) stream byte-for-byte untouched & skip tee
    for mode in ["gzip", "br", "mixed"] {
        let resp = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri(format!(
                        "/v1internal:streamGenerateContent?alt=sse&mode={mode}"
                    ))
                    .body(Body::from(r#"{"model":"gemini-3.8-pro"}"#))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let bytes = axum::body::to_bytes(resp.into_body(), 1024 * 1024)
            .await
            .unwrap();
        assert_eq!(bytes.as_ref(), raw_compressed, "mode={mode}");
    }

    // Error status (500) skips tee
    let err_resp = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/v1internal:streamGenerateContent?alt=sse&mode=err")
                .body(Body::from(r#"{"model":"gemini-3.8-pro"}"#))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(err_resp.status(), StatusCode::INTERNAL_SERVER_ERROR);
    let _ = axum::body::to_bytes(err_resp.into_body(), 1024 * 1024)
        .await
        .unwrap();

    // Non-generation RPC (/v1internal:loadCodeAssist) skips tee
    let meta_resp = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/v1internal:loadCodeAssist")
                .body(Body::from("{}"))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(meta_resp.status(), StatusCode::OK);
    let _ = axum::body::to_bytes(meta_resp.into_body(), 1024 * 1024)
        .await
        .unwrap();

    assert_eq!(state.stats().snapshot().usage.requests, 0);
    assert!(sink.snapshot().is_empty());

    // Content-Encoding: Identity DOES tee!
    let id_resp = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/v1internal:streamGenerateContent?alt=sse&mode=identity")
                .body(Body::from(r#"{"model":"gemini-3.8-pro"}"#))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(id_resp.status(), StatusCode::OK);
    let id_bytes = axum::body::to_bytes(id_resp.into_body(), 1024 * 1024)
        .await
        .unwrap();
    assert_eq!(id_bytes.as_ref(), identity_sse.as_bytes());
    assert_eq!(state.stats().snapshot().usage.requests, 1);
    assert_eq!(state.stats().snapshot().usage.prompt_tokens, 321);
    assert_eq!(sink.snapshot().len(), 1);

    server.abort();
}

#[tokio::test]
async fn oversized_gemini_model_id_is_rejected_and_stats_stay_within_probe_budget() {
    let sse_body =
        "data: {\"usageMetadata\":{\"promptTokenCount\":100,\"candidatesTokenCount\":10}}\n\n";
    let sink = Arc::new(RecordingSink::default());
    let provider = Arc::new(StubProvider::ok_stream(
        Some("text/event-stream"),
        vec![Bytes::from_static(sse_body.as_bytes())],
    ));
    let state = test_proxy_state(provider.clone(), sink.clone(), true);
    let app = build_router(state.clone());

    // 1200+ char Gemini model ID must be rejected with 400 and never reach upstream or inflate stats.
    let huge_model = format!("gemini-4-00-{}", "a".repeat(1200));
    let resp = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri(format!("/v1beta/models/{huge_model}:generateContent"))
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(r#"{"contents":[]}"#))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    assert_eq!(provider.hit_count(), 0);
    assert_eq!(state.stats().snapshot().usage.requests, 0);

    // 32 valid 128-char Gemini model IDs must succeed and keep /api/stats well under 64 KiB.
    for i in 0..32 {
        let prefix = format!("gemini-4-{i:02}-");
        let max_model = format!("{prefix}{}", "a".repeat(128 - prefix.len()));
        assert_eq!(max_model.len(), 128);
        let ok_resp = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri(format!(
                        "/v1beta/models/{max_model}:streamGenerateContent?alt=sse"
                    ))
                    .header(header::CONTENT_TYPE, "application/json")
                    .body(Body::from(r#"{"contents":[]}"#))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(ok_resp.status(), StatusCode::OK);
        let _ = axum::body::to_bytes(ok_resp.into_body(), 1024 * 1024)
            .await
            .unwrap();
    }

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
    let stats_bytes = axum::body::to_bytes(stats_resp.into_body(), 1024 * 1024)
        .await
        .unwrap();
    assert!(
        stats_bytes.len() < companion_providers::MAX_PROBE_BODY_BYTES,
        "/api/stats payload {} bytes exceeded {} byte probe limit",
        stats_bytes.len(),
        companion_providers::MAX_PROBE_BODY_BYTES
    );
}

#[tokio::test]
async fn builtin_tee_preserves_split_content_encoding_and_fails_closed_on_non_ascii_encoding() {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    let sse_payload = "data: {\"response\":{\"usageMetadata\":{\"promptTokenCount\":777,\"candidatesTokenCount\":77}}}\n\n";

    // Raw TCP mock upstream so we can emit both split Content-Encoding headers AND a raw non-ASCII
    // Content-Encoding header byte sequence on the wire.
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let raw_server = tokio::spawn(async move {
        for _ in 0..2 {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut req_buf = Vec::with_capacity(4096);
            let mut chunk = [0u8; 1024];
            let mut expected_total: Option<usize> = None;
            loop {
                let n = socket.read(&mut chunk).await.unwrap();
                if n == 0 {
                    break;
                }
                req_buf.extend_from_slice(&chunk[..n]);
                if expected_total.is_none()
                    && let Some(pos) = req_buf.windows(4).position(|w| w == b"\r\n\r\n")
                {
                    let header_end = pos + 4;
                    let headers_str = String::from_utf8_lossy(&req_buf[..pos]);
                    let content_len = headers_str
                        .lines()
                        .find_map(|line| {
                            let (k, v) = line.split_once(':')?;
                            k.eq_ignore_ascii_case("content-length")
                                .then(|| v.trim().parse::<usize>().ok())
                                .flatten()
                        })
                        .unwrap_or(0);
                    expected_total = Some(header_end + content_len);
                }
                if let Some(total) = expected_total
                    && req_buf.len() >= total
                {
                    break;
                }
            }
            let req_text = String::from_utf8_lossy(&req_buf);
            let resp_bytes = if req_text.contains("mode=split") {
                format!(
                    "HTTP/1.1 200 OK\r\n\
                     Content-Type: text/event-stream\r\n\
                     Content-Encoding: identity\r\n\
                     Content-Encoding: br\r\n\
                     Content-Length: {}\r\n\
                     Connection: close\r\n\r\n{}",
                    sse_payload.len(),
                    sse_payload
                )
                .into_bytes()
            } else {
                let mut raw = Vec::new();
                raw.extend_from_slice(
                    b"HTTP/1.1 200 OK\r\n\
                      Content-Type: text/event-stream\r\n\
                      Content-Encoding: \x80br\r\n",
                );
                raw.extend_from_slice(
                    format!(
                        "Content-Length: {}\r\nConnection: close\r\n\r\n{}",
                        sse_payload.len(),
                        sse_payload
                    )
                    .as_bytes(),
                );
                raw
            };
            socket.write_all(&resp_bytes).await.unwrap();
            let _ = socket.shutdown().await;
        }
    });

    let mut cfg = companion_core::Config::default();
    cfg.cloudcode.upstream = CloudCodeUpstream::Fixed(format!("http://{addr}"));
    cfg.providers.gemini.base_url = format!("http://{addr}");

    let sink = Arc::new(RecordingSink::default());
    let state = ProxyState::from_config_with_options(
        cfg,
        sink.clone(),
        ProxyStateOptions {
            config_path: None,
            auth_store: None,
            env_fallback: false,
            ..Default::default()
        },
    )
    .unwrap();
    let app = build_router(state.clone());

    // 1. Split headers: Content-Encoding: identity + Content-Encoding: br -> BOTH preserved, tee skipped.
    let split_resp = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/v1internal:streamGenerateContent?alt=sse&mode=split")
                .body(Body::from(r#"{"model":"gemini-3.8-pro"}"#))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(split_resp.status(), StatusCode::OK);
    let ce_values: Vec<&str> = split_resp
        .headers()
        .get_all(header::CONTENT_ENCODING)
        .iter()
        .map(|v| v.to_str().expect("ascii"))
        .collect();
    assert_eq!(
        ce_values,
        vec!["identity", "br"],
        "proxy must preserve both Content-Encoding headers"
    );
    let split_body = axum::body::to_bytes(split_resp.into_body(), 1024 * 1024)
        .await
        .unwrap();
    assert_eq!(split_body.as_ref(), sse_payload.as_bytes());
    assert_eq!(
        state.stats().snapshot().usage.requests,
        0,
        "split identity+br response must skip passive tee"
    );
    assert!(sink.snapshot().is_empty());

    // 2. Non-ASCII Content-Encoding: stripped from outbound headers, AND fails closed to skip tee.
    let non_ascii_resp = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/v1internal:streamGenerateContent?alt=sse&mode=nonascii")
                .body(Body::from(r#"{"model":"gemini-3.8-pro"}"#))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(non_ascii_resp.status(), StatusCode::OK);
    assert!(
        non_ascii_resp
            .headers()
            .get(header::CONTENT_ENCODING)
            .is_none(),
        "non-ASCII Content-Encoding should still be filtered from outbound headers"
    );
    let non_ascii_body = axum::body::to_bytes(non_ascii_resp.into_body(), 1024 * 1024)
        .await
        .unwrap();
    assert_eq!(non_ascii_body.as_ref(), sse_payload.as_bytes());
    assert_eq!(
        state.stats().snapshot().usage.requests,
        0,
        "non-ASCII Content-Encoding must fail closed and skip passive tee"
    );
    assert!(sink.snapshot().is_empty());

    raw_server.abort();
}

#[tokio::test]
async fn proxy_tee_propagates_response_id_and_conversation_id_for_sse_and_unary_json() {
    use companion_core::TelemetrySource;

    // 1. Streaming SSE response with top-level responseId and conversation header
    let sse_chunks = vec![
        Bytes::from_static(
            b"data: {\"responseId\":\"G4EIa_sse_001\",\"candidates\":[{\"content\":{\"parts\":[{\"text\":\"hello\"}]}}]}\n\n",
        ),
        Bytes::from_static(
            b"data: {\"usageMetadata\":{\"promptTokenCount\":400,\"candidatesTokenCount\":55,\"cachedContentTokenCount\":300}}\n\n",
        ),
        Bytes::from_static(b"data: [DONE]\n\n"),
    ];
    let expected_sse_bytes: Vec<u8> = sse_chunks.iter().flat_map(|c| c.iter().copied()).collect();
    let sse_provider = Arc::new(StubProvider::ok_stream(
        Some("text/event-stream"),
        sse_chunks,
    ));
    let sse_sink = Arc::new(RecordingSink::default());
    let sse_app = build_router(test_proxy_state(sse_provider, sse_sink.clone(), true));

    let sse_resp = sse_app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/v1beta/models/gemini-4-hydrogen:streamGenerateContent?alt=sse")
                .header(X_GOOG_API_KEY_HEADER.clone(), "test-key")
                .body(Body::from(
                    r#"{"sessionId":"conv_sse_123456","contents":[{"parts":[{"text":"hi"}]}]}"#,
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(sse_resp.status(), StatusCode::OK);
    let sse_body = sse_resp.into_body().collect().await.unwrap().to_bytes();
    assert_eq!(
        sse_body.as_ref(),
        expected_sse_bytes.as_slice(),
        "S4: SSE bytes must pass through byte-for-byte untouched"
    );

    let sse_events = sse_sink.snapshot();
    assert_eq!(sse_events.len(), 1);
    assert!(sse_events[0].request_id.starts_with("proxy:"));
    assert_eq!(sse_events[0].telemetry_source, TelemetrySource::Proxy);
    assert_eq!(sse_events[0].response_id.as_deref(), Some("G4EIa_sse_001"));
    assert_eq!(
        sse_events[0].conversation_id.as_deref(),
        Some("conv_sse_123456")
    );
    assert_eq!(sse_events[0].input_tokens, 400);
    assert_eq!(sse_events[0].output_tokens, 55);

    // 2. Unary JSON response with wrapped response.responseId (CloudCode style)
    let unary_payload = Bytes::from_static(
        br#"{"response":{"responseId":"AJ8Ia_unary_002","usageMetadata":{"promptTokenCount":180,"candidatesTokenCount":25,"thoughtsTokenCount":7}}}"#,
    );
    let unary_provider = Arc::new(StubProvider::with_status(
        200,
        Some("application/json"),
        unary_payload.clone(),
    ));
    let unary_sink = Arc::new(RecordingSink::default());
    let unary_app = build_router(test_proxy_state(unary_provider, unary_sink.clone(), true));

    let unary_resp = unary_app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/v1beta/models/gemini-4-carbon:generateContent")
                .header(X_GOOG_API_KEY_HEADER.clone(), "test-key")
                .body(Body::from(
                    r#"{"sessionId":"conv_unary_789","contents":[{"parts":[{"text":"hi"}]}]}"#,
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(unary_resp.status(), StatusCode::OK);
    let unary_body = unary_resp.into_body().collect().await.unwrap().to_bytes();
    assert_eq!(
        unary_body.as_ref(),
        unary_payload.as_ref(),
        "S4: unary JSON bytes must pass through byte-for-byte untouched"
    );

    let unary_events = unary_sink.snapshot();
    assert_eq!(unary_events.len(), 1);
    assert!(unary_events[0].request_id.starts_with("proxy:"));
    assert_eq!(unary_events[0].telemetry_source, TelemetrySource::Proxy);
    assert_eq!(
        unary_events[0].response_id.as_deref(),
        Some("AJ8Ia_unary_002")
    );
    assert_eq!(
        unary_events[0].conversation_id.as_deref(),
        Some("conv_unary_789")
    );
    assert_eq!(unary_events[0].input_tokens, 180);
    assert_eq!(unary_events[0].output_tokens, 25);
    assert_eq!(unary_events[0].reasoning_tokens, 7);
}

fn sample_codex_creds_for_route_test(token: &str) -> companion_core::CodexCredentials {
    companion_core::CodexCredentials {
        schema_version: companion_core::AUTH_SCHEMA_VERSION,
        auth_mode: companion_core::CodexAuthMode::ChatGptOAuth,
        access_token: companion_core::SecretString::new(token),
        refresh_token: companion_core::SecretString::new("refresh_tok_route_test"),
        id_token: None,
        account_id: "acct_route_test_1234".to_string(),
        expires_at_ms: 4_102_444_800_000,
        plan_type: Some("pro".to_string()),
        email_hint: Some("r***@example.com".to_string()),
        last_refresh_utc: "2026-11-26T00:00:00Z".to_string(),
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn codex_proxy_dispatches_sse_and_unary_records_codex_usage_and_conversation_id() {
    use axum::routing::post;
    use companion_core::{Config, MemoryAuthStore, SqliteUsageSink};
    use std::net::{Ipv4Addr, SocketAddr};
    use std::sync::atomic::{AtomicUsize, Ordering};
    use tokio::net::TcpListener;

    let upstream_hits = Arc::new(AtomicUsize::new(0));
    let hits_clone = Arc::clone(&upstream_hits);
    let mock_codex_app = axum::Router::new().route(
        "/backend-api/codex/responses",
        post(move |_body: Bytes| {
            let hits = Arc::clone(&hits_clone);
            async move {
                let n = hits.fetch_add(1, Ordering::SeqCst);
                if n == 2 {
                    // 3rd call simulates 400 to verify Gemini INVALID_THOUGHT_SIGNATURE strip-retry
                    // does NOT run for Codex models.
                    return axum::response::Response::builder()
                        .status(StatusCode::BAD_REQUEST)
                        .header(header::CONTENT_TYPE, "application/json")
                        .body(Body::from(
                            r#"{"error":{"code":"invalid_request_error","message":"INVALID_THOUGHT_SIGNATURE"}}"#,
                        ))
                        .unwrap();
                }
                let sse = concat!(
                    "data: {\"type\":\"response.reasoning_summary_text.delta\",\"delta\":\"Thinking...\"}\n\n",
                    "data: {\"type\":\"response.output_text.delta\",\"delta\":\"Codex says hi\"}\n\n",
                    "data: {\"type\":\"response.completed\",\"response\":{\"usage\":{\"input_tokens\":400,\"input_tokens_details\":{\"cached_tokens\":250},\"output_tokens\":60,\"output_tokens_details\":{\"reasoning_tokens\":20},\"total_tokens\":460}}}\n\n",
                    "data: [DONE]\n\n"
                );
                axum::response::Response::builder()
                    .status(StatusCode::OK)
                    .header(header::CONTENT_TYPE, "text/event-stream")
                    .body(Body::from(sse))
                    .unwrap()
            }
        }),
    );

    let listener = TcpListener::bind(SocketAddr::from((Ipv4Addr::LOCALHOST, 0)))
        .await
        .expect("bind mock codex");
    let port = listener.local_addr().unwrap().port();
    let server_handle = tokio::spawn(async move {
        let _ = axum::serve(listener, mock_codex_app).await;
    });
    tokio::task::yield_now().await;

    let tmp = tempfile::TempDir::new().unwrap();
    let sqlite_sink = Arc::new(SqliteUsageSink::new(tmp.path().join("usage")).unwrap());
    let auth_store = Arc::new(MemoryAuthStore::with_codex_credentials(
        sample_codex_creds_for_route_test("codex_access_tok_1"),
    ));

    let mut cfg = Config::default();
    cfg.providers.codex.base_url = format!("http://127.0.0.1:{port}/backend-api/codex");

    let state = ProxyState::from_config_with_options(
        cfg,
        sqlite_sink.clone(),
        ProxyStateOptions {
            config_path: None,
            auth_store: Some(auth_store),
            env_fallback: false,
            ..Default::default()
        },
    )
    .expect("proxy state");
    let app = build_router(state.clone());

    // 1. Streaming SSE to Codex model WITHOUT any x-goog-api-key header + with conversation_id.
    let sse_req_body = serde_json::json!({
        "systemInstruction": {
            "parts": [{ "text": "Conversation ID: conv-codex-123456\nBe concise." }]
        },
        "contents": [{ "role": "user", "parts": [{ "text": "hello" }] }]
    });
    let sse_resp = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/v1beta/models/gpt-6.2-astra:streamGenerateContent?alt=sse")
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(sse_req_body.to_string()))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(sse_resp.status(), StatusCode::OK);
    let sse_body = axum::body::to_bytes(sse_resp.into_body(), 1024 * 1024)
        .await
        .unwrap();
    let sse_text = String::from_utf8_lossy(&sse_body);
    assert!(sse_text.contains("\"thought\":true"));
    assert!(sse_text.contains("Codex says hi"));

    // 2. Unary :generateContent to Codex model with placeholder Gemini key ignored cleanly.
    let unary_resp = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/v1beta/models/gpt-6.2-sol:generateContent")
                .header(header::CONTENT_TYPE, "application/json")
                .header("x-goog-api-key", companion_core::PROXY_PLACEHOLDER_API_KEY)
                .body(Body::from(sse_req_body.to_string()))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(unary_resp.status(), StatusCode::OK);
    let unary_bytes = axum::body::to_bytes(unary_resp.into_body(), 1024 * 1024)
        .await
        .unwrap();
    let unary_json: serde_json::Value = serde_json::from_slice(&unary_bytes).unwrap();
    assert_eq!(
        unary_json["candidates"][0]["content"]["parts"][1]["text"],
        "Codex says hi"
    );

    // Verify both calls recorded in SQLite under provider="openai-codex", source="proxy".
    let events = sqlite_sink.read_all_events().expect("read sqlite events");
    assert_eq!(events.len(), 2);
    for ev in &events {
        assert_eq!(ev.provider, "openai-codex");
        assert_eq!(ev.source, "proxy");
        assert_eq!(ev.input_tokens, 400);
        assert_eq!(ev.cached_tokens, 250);
        assert_eq!(ev.output_tokens, 60);
        assert_eq!(ev.reasoning_tokens, 20);
    }
    assert_eq!(events[0].model, "gpt-6.2-astra");
    assert_eq!(events[1].model, "gpt-6.2-sol");

    // Verify per-conversation stats isolation works identically for Codex!
    let stats_resp = app
        .clone()
        .oneshot(
            Request::builder()
                .method("GET")
                .uri("/api/stats?conversation_id=conv-codex-123456")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(stats_resp.status(), StatusCode::OK);
    let stats_bytes = axum::body::to_bytes(stats_resp.into_body(), 1024 * 1024)
        .await
        .unwrap();
    let stats_snap: StatsSnapshot = serde_json::from_slice(&stats_bytes).unwrap();
    assert_eq!(stats_snap.usage.requests, 2);
    assert_eq!(stats_snap.usage.cached_tokens, 500);

    // 3. Unsupported binary MIME fails with clean 400 and 0 extra upstream hits.
    let hits_before_bad_mime = upstream_hits.load(Ordering::SeqCst);
    let pdf_resp = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/v1beta/models/gpt-6.2-sol:generateContent")
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(
                    serde_json::json!({
                        "contents": [{
                            "role": "user",
                            "parts": [{ "inlineData": { "mimeType": "application/pdf", "data": "JVBERi0=" } }]
                        }]
                    })
                    .to_string(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(pdf_resp.status(), StatusCode::BAD_REQUEST);
    assert_eq!(upstream_hits.load(Ordering::SeqCst), hits_before_bad_mime);

    // 4. Codex 400 with thoughtSignature in request body must NOT trigger Gemini strip-retry.
    let ts_resp = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/v1beta/models/gpt-6.2-sol:generateContent")
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(
                    serde_json::json!({
                        "contents": [{
                            "role": "model",
                            "parts": [{ "text": "prev", "thoughtSignature": "sig-abc" }]
                        }]
                    })
                    .to_string(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(ts_resp.status(), StatusCode::BAD_REQUEST);
    assert_eq!(
        upstream_hits.load(Ordering::SeqCst),
        hits_before_bad_mime + 1,
        "Gemini INVALID_THOUGHT_SIGNATURE strip-retry must never fire for Codex"
    );

    server_handle.abort();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cloudcode_fetch_available_models_gates_day0_codex_only_and_both_providers() {
    use axum::routing::post;
    use companion_core::{ApiKey, AuthStore, CloudCodeUpstream, Config, MemoryAuthStore};
    use std::net::{Ipv4Addr, SocketAddr};
    use tokio::net::TcpListener;

    let raw_upstream_payload = br#"{"models":{"native-gem":{"model":"MODEL_PLACEHOLDER_M427","displayName":"Native"}},"agentModelSorts":[{"displayName":"Recommended","groups":[{"displayName":"Models","modelIds":["native-gem"]}]}]}"#;
    let mock_cc = axum::Router::new().route(
        "/v1internal:fetchAvailableModels",
        post(move || async move {
            axum::response::Response::builder()
                .status(StatusCode::OK)
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(raw_upstream_payload.as_slice()))
                .unwrap()
        }),
    );

    let listener = TcpListener::bind(SocketAddr::from((Ipv4Addr::LOCALHOST, 0)))
        .await
        .expect("bind mock cloudcode");
    let port = listener.local_addr().unwrap().port();
    let cc_handle = tokio::spawn(async move {
        let _ = axum::serve(listener, mock_cc).await;
    });
    tokio::task::yield_now().await;

    let mut base_cfg = Config::default();
    base_cfg.cloudcode.upstream =
        CloudCodeUpstream::parse(&format!("http://127.0.0.1:{port}")).unwrap();

    // Case 1: Neither ready (Day-0) -> untouched byte-for-byte.
    let day0_state = ProxyState::from_config_with_options(
        base_cfg.clone(),
        Arc::new(NoopUsageSink),
        ProxyStateOptions {
            config_path: None,
            auth_store: Some(Arc::new(MemoryAuthStore::new())),
            env_fallback: false,
            ..Default::default()
        },
    )
    .unwrap();
    let day0_resp = build_router(day0_state)
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/v1internal:fetchAvailableModels")
                .body(Body::from("{}"))
                .unwrap(),
        )
        .await
        .unwrap();
    let day0_bytes = axum::body::to_bytes(day0_resp.into_body(), 1024 * 1024)
        .await
        .unwrap();
    assert_eq!(day0_bytes.as_ref(), raw_upstream_payload.as_slice());

    // Case 2: Codex-only authenticated -> injects ONLY enabled Codex models (default 8 slots).
    let codex_only_state = ProxyState::from_config_with_options(
        base_cfg.clone(),
        Arc::new(NoopUsageSink),
        ProxyStateOptions {
            config_path: None,
            auth_store: Some(Arc::new(MemoryAuthStore::with_codex_credentials(
                sample_codex_creds_for_route_test("tok_only"),
            ))),
            env_fallback: false,
            ..Default::default()
        },
    )
    .unwrap();
    let codex_resp = build_router(codex_only_state)
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/v1internal:fetchAvailableModels")
                .body(Body::from("{}"))
                .unwrap(),
        )
        .await
        .unwrap();
    let codex_json: serde_json::Value = serde_json::from_slice(
        &axum::body::to_bytes(codex_resp.into_body(), 1024 * 1024)
            .await
            .unwrap(),
    )
    .unwrap();
    let codex_models = codex_json["models"].as_object().unwrap();
    assert!(codex_models.contains_key("companion/gpt-6.2-astra-high"));
    assert!(codex_models.contains_key("companion/gpt-6.2-sol-xhigh"));
    assert!(
        !codex_models
            .keys()
            .any(|k| k.starts_with("companion/gemini-")),
        "Codex-only must not inject Gemini models"
    );

    // Case 3: Both Gemini + Codex ready -> injects both without slot collisions.
    let both_store = Arc::new(MemoryAuthStore::with_gemini_key(
        ApiKey::parse_strict("AIzaBothProvidersTestKey1").unwrap(),
    ));
    both_store
        .save_codex(&sample_codex_creds_for_route_test("tok_both"))
        .unwrap();
    let both_state = ProxyState::from_config_with_options(
        base_cfg,
        Arc::new(NoopUsageSink),
        ProxyStateOptions {
            config_path: None,
            auth_store: Some(both_store),
            env_fallback: false,
            ..Default::default()
        },
    )
    .unwrap();
    let both_resp = build_router(both_state)
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/v1internal:fetchAvailableModels")
                .body(Body::from("{}"))
                .unwrap(),
        )
        .await
        .unwrap();
    let both_json: serde_json::Value = serde_json::from_slice(
        &axum::body::to_bytes(both_resp.into_body(), 1024 * 1024)
            .await
            .unwrap(),
    )
    .unwrap();
    let both_models = both_json["models"].as_object().unwrap();
    assert!(both_models.contains_key("companion/gemini-4-hydrogen-high"));
    assert!(both_models.contains_key("companion/gpt-6.2-sol-high"));

    cc_handle.abort();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn fetch_available_models_captures_upstream_and_injected_entries_into_shared_catalog() {
    use axum::routing::post;
    use companion_core::{
        AuthStore, CloudCodeUpstream, Config, MemoryAuthStore, UpstreamModelCatalog,
    };
    use std::net::{Ipv4Addr, SocketAddr};
    use tokio::net::TcpListener;

    let raw_upstream_payload = br#"{
        "models": {
            "gemini-4-hydrogen-tiered-high": {
                "model": "MODEL_PLACEHOLDER_M530",
                "displayName": "Gemini 4 Hydrogen (High)",
                "modelProvider": "MODEL_PROVIDER_GOOGLE",
                "apiProvider": "API_PROVIDER_GOOGLE_GEMINI"
            }
        }
    }"#;
    let mock_cc = axum::Router::new().route(
        "/v1internal:fetchAvailableModels",
        post(move || async move {
            axum::response::Response::builder()
                .status(StatusCode::OK)
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(raw_upstream_payload.as_slice()))
                .unwrap()
        }),
    );

    let listener = TcpListener::bind(SocketAddr::from((Ipv4Addr::LOCALHOST, 0)))
        .await
        .expect("bind mock cloudcode");
    let port = listener.local_addr().unwrap().port();
    let cc_handle = tokio::spawn(async move {
        let _ = axum::serve(listener, mock_cc).await;
    });
    tokio::task::yield_now().await;

    let mut cfg = Config::default();
    cfg.cloudcode.upstream = CloudCodeUpstream::parse(&format!("http://127.0.0.1:{port}")).unwrap();

    // 1. Day-0 unmerged still captures upstream M530 into shared UpstreamModelCatalog
    let shared_catalog = Arc::new(UpstreamModelCatalog::new());
    let day0_state = ProxyState::from_config_with_options(
        cfg.clone(),
        Arc::new(NoopUsageSink),
        ProxyStateOptions {
            config_path: None,
            auth_store: Some(Arc::new(MemoryAuthStore::new())),
            env_fallback: false,
            upstream_catalog: Some(Arc::clone(&shared_catalog)),
            ..Default::default()
        },
    )
    .unwrap();
    let resp0 = build_router(day0_state.clone())
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/v1internal:fetchAvailableModels")
                .body(Body::from("{}"))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp0.status(), StatusCode::OK);
    let snap0 = shared_catalog.snapshot();
    assert_eq!(
        snap0
            .get("MODEL_PLACEHOLDER_M530")
            .and_then(|e| e.model_id.as_deref()),
        Some("gemini-4-hydrogen-tiered-high")
    );

    // 2. With Codex enabled, merged response also captures injected companion/gpt-* placeholders
    let codex_store = Arc::new(MemoryAuthStore::new());
    codex_store
        .save_codex(&sample_codex_creds_for_route_test("tok_cat"))
        .unwrap();
    let shared_catalog_2 = Arc::new(UpstreamModelCatalog::new());
    let codex_state = ProxyState::from_config_with_options(
        cfg,
        Arc::new(NoopUsageSink),
        ProxyStateOptions {
            config_path: None,
            auth_store: Some(codex_store),
            env_fallback: false,
            upstream_catalog: Some(Arc::clone(&shared_catalog_2)),
            ..Default::default()
        },
    )
    .unwrap();
    let resp1 = build_router(codex_state.clone())
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/v1internal:fetchAvailableModels")
                .body(Body::from("{}"))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp1.status(), StatusCode::OK);
    let snap1 = codex_state.upstream_catalog().snapshot();
    assert_eq!(
        snap1
            .get("MODEL_PLACEHOLDER_M530")
            .and_then(|e| e.model_id.as_deref()),
        Some("gemini-4-hydrogen-tiered-high")
    );
    assert!(
        snap1
            .values()
            .any(|e| { e.model_url.as_deref() == Some("gemini-api://gpt-6.2-sol") }),
        "expected injected Codex modelUrl in shared upstream catalog: {snap1:?}"
    );

    cc_handle.abort();
}
