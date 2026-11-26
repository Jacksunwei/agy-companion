use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use axum::Router;
use axum::body::Body;
use axum::extract::OriginalUri;
use axum::http::{HeaderMap, StatusCode};
use axum::response::Response;
use axum::routing::{any, get, post};
use bytes::Bytes;
use companion_core::{ApiKey, DEFAULT_TIMEOUT_SECS, GeminiConfig};
use futures_util::{StreamExt, stream};
use tokio::sync::{Mutex, mpsc};

use super::*;

struct MockServer {
    base_url: String,
    handle: tokio::task::JoinHandle<()>,
}

impl Drop for MockServer {
    fn drop(&mut self) {
        self.handle.abort();
    }
}

async fn spawn_mock_server(router: Router) -> MockServer {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind loopback listener");
    let addr = listener.local_addr().expect("local_addr");
    let handle = tokio::spawn(async move {
        let _ = axum::serve(listener, router).await;
    });
    MockServer {
        base_url: format!("http://{addr}"),
        handle,
    }
}

fn config_for(base_url: &str, timeout_secs: u64) -> GeminiConfig {
    GeminiConfig {
        base_url: base_url.to_string(),
        timeout_secs,
        ..GeminiConfig::default()
    }
}

fn provider_for(base_url: &str, api_key: Option<&str>, env_key: Option<&str>) -> GeminiProvider {
    GeminiProvider::build(
        base_url.to_string(),
        api_key.map(str::to_owned),
        Duration::from_secs(DEFAULT_TIMEOUT_SECS),
        EnvKeySource::Fixed(env_key.map(str::to_owned)),
    )
    .expect("construct test provider")
}

async fn collect_stream(mut response: ForwardResponse) -> Vec<u8> {
    let mut out = Vec::new();
    while let Some(chunk) = response.stream.next().await {
        out.extend_from_slice(&chunk.expect("stream chunk"));
    }
    out
}

#[derive(Clone, Debug)]
struct CapturedRequest {
    uri: String,
    api_key_header: Option<String>,
    content_type: Option<String>,
    body: Bytes,
}

// 1. Task 2 Core: byte-for-byte forwarding + x-goog-api-key header + ?key= strip
#[tokio::test]
async fn forwards_exact_request_bytes_and_sets_x_goog_api_key_header() {
    let captured = Arc::new(Mutex::new(None::<CapturedRequest>));
    let captured_clone = Arc::clone(&captured);

    let router = Router::new().route(
        "/v1beta/models/{*rest}",
        post(
            move |OriginalUri(uri): OriginalUri, headers: HeaderMap, body: Bytes| {
                let captured = Arc::clone(&captured_clone);
                async move {
                    let api_key_header = headers
                        .get("x-goog-api-key")
                        .and_then(|v| v.to_str().ok())
                        .map(str::to_owned);
                    let content_type = headers
                        .get(http::header::CONTENT_TYPE)
                        .and_then(|v| v.to_str().ok())
                        .map(str::to_owned);
                    *captured.lock().await = Some(CapturedRequest {
                        uri: uri.to_string(),
                        api_key_header,
                        content_type,
                        body,
                    });
                    Response::builder()
                        .status(StatusCode::OK)
                        .header(
                            http::header::CONTENT_TYPE,
                            "application/json; charset=UTF-8",
                        )
                        .body(Body::from(
                            r#"{"candidates":[{"content":{"parts":[{"text":"ok"}]}}]}"#,
                        ))
                        .unwrap()
                }
            },
        ),
    );
    let server = spawn_mock_server(router).await;
    let provider = provider_for(&server.base_url, Some("AIzaSyTestConfigured1234"), None);

    let raw_payload = Bytes::from_static(
        b"{\n  \"z_first\": 1,\n  \"contents\": [{\"parts\": [{\"text\": \"hello \xF0\x9F\x8C\x8D\"}]}]\n}\n",
    );

    let resp = provider
        .forward_gemini(ForwardRequest {
            path_and_query: "/v1beta/models/gemini-4-hydrogen:generateContent?key=LEAK_ME&alt=json"
                .to_string(),
            body: raw_payload.clone(),
            content_type: Some("application/json; charset=utf-8".to_string()),
            api_key_override: None,
        })
        .await
        .expect("forward_gemini should succeed");

    assert_eq!(resp.status, 200);
    assert_eq!(
        resp.content_type.as_deref(),
        Some("application/json; charset=UTF-8")
    );
    let resp_body = collect_stream(resp).await;
    assert_eq!(
        resp_body,
        br#"{"candidates":[{"content":{"parts":[{"text":"ok"}]}}]}"#
    );

    let seen = captured
        .lock()
        .await
        .clone()
        .expect("upstream should have received request");
    assert_eq!(seen.body, raw_payload);
    assert_eq!(
        seen.api_key_header.as_deref(),
        Some("AIzaSyTestConfigured1234")
    );
    assert_eq!(
        seen.content_type.as_deref(),
        Some("application/json; charset=utf-8")
    );
    assert_eq!(
        seen.uri,
        "/v1beta/models/gemini-4-hydrogen:generateContent?alt=json"
    );
    assert!(!seen.uri.contains("LEAK_ME"));
    assert!(!seen.uri.to_ascii_lowercase().contains("key="));
}

// 2. Task 2 Core: true unbuffered SSE streaming proof before EOF
#[tokio::test]
async fn streams_sse_chunks_without_mutating_payload() {
    let (tx, rx) = mpsc::channel::<Result<Bytes, std::io::Error>>(4);
    let rx_slot = Arc::new(Mutex::new(Some(rx)));

    let router = Router::new().route(
        "/v1beta/models/{*rest}",
        post(move || {
            let rx_slot = Arc::clone(&rx_slot);
            async move {
                let rx = rx_slot.lock().await.take().expect("receiver available");
                let body_stream = stream::unfold(rx, |mut rx| async move {
                    rx.recv().await.map(|item| (item, rx))
                });
                Response::builder()
                    .status(StatusCode::OK)
                    .header(
                        http::header::CONTENT_TYPE,
                        "text/event-stream; charset=utf-8",
                    )
                    .body(Body::from_stream(body_stream))
                    .unwrap()
            }
        }),
    );
    let server = spawn_mock_server(router).await;
    let provider = provider_for(&server.base_url, Some("AIzaSyStreamKey123456"), None);

    let mut resp = provider
        .forward_gemini(ForwardRequest {
            path_and_query: "/v1beta/models/gemini-4-hydrogen:streamGenerateContent?alt=sse"
                .to_string(),
            body: Bytes::from_static(br#"{"contents":[]}"#),
            content_type: None,
            api_key_override: None,
        })
        .await
        .expect("headers should arrive before stream closes");

    assert_eq!(resp.status, 200);
    assert_eq!(
        resp.content_type.as_deref(),
        Some("text/event-stream; charset=utf-8")
    );

    let chunk1 = Bytes::from_static(b"data: {\"text\":\"part-1\"}\r\n\r\n");
    let chunk2 = Bytes::from_static(
        b"data: {\"text\":\"part-2\",\"usageMetadata\":{\"promptTokenCount\":12,\"candidatesTokenCount\":5}}\r\n\r\n",
    );

    // Send chunk1 ONLY while keeping tx open: receiving chunk1 proves unbuffered streaming!
    tx.send(Ok(chunk1.clone())).await.expect("send chunk1");

    let mut received = Vec::new();
    tokio::time::timeout(Duration::from_secs(2), async {
        while received.len() < chunk1.len() {
            let next = resp
                .stream
                .next()
                .await
                .expect("stream must stay open")
                .expect("chunk1 ok");
            received.extend_from_slice(&next);
        }
    })
    .await
    .expect("chunk1 must stream immediately before EOF");
    assert_eq!(received, chunk1.as_ref());

    // Now send chunk2 and close stream.
    tx.send(Ok(chunk2.clone())).await.expect("send chunk2");
    drop(tx);

    while let Some(item) = resp.stream.next().await {
        received.extend_from_slice(&item.expect("chunk2 ok"));
    }

    let mut expected = Vec::new();
    expected.extend_from_slice(&chunk1);
    expected.extend_from_slice(&chunk2);
    assert_eq!(received, expected);
}

// 3. Task 2 Core: verbatim upstream error status & body propagation
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn propagates_upstream_error_status_and_body() {
    for status_code in [400u16, 401, 403, 429, 500] {
        let expected_body = format!(
            r#"{{"error":{{"code":{status_code},"message":"upstream error {status_code}"}}}}"#
        );
        let body_for_handler = expected_body.clone();
        let router = Router::new().route(
            "/v1beta/models/{*rest}",
            post(move || {
                let body_for_handler = body_for_handler.clone();
                async move {
                    Response::builder()
                        .status(StatusCode::from_u16(status_code).unwrap())
                        .header(
                            http::header::CONTENT_TYPE,
                            "application/json; charset=UTF-8",
                        )
                        .body(Body::from(body_for_handler))
                        .unwrap()
                }
            }),
        );
        let server = spawn_mock_server(router).await;
        let provider = provider_for(&server.base_url, Some("AIzaSyErrorProp123456"), None);

        let resp = provider
            .forward_gemini(ForwardRequest {
                path_and_query: "/v1beta/models/gemini-4-hydrogen:generateContent".to_string(),
                body: Bytes::from_static(b"{}"),
                content_type: None,
                api_key_override: None,
            })
            .await
            .expect("forward_gemini must propagate non-2xx as Ok(ForwardResponse)");

        assert_eq!(resp.status, status_code);
        assert_eq!(
            resp.content_type.as_deref(),
            Some("application/json; charset=UTF-8")
        );
        let body = collect_stream(resp).await;
        assert_eq!(body, expected_body.as_bytes());
    }
}

// 4. Task 2 Core: rejects path traversal & non-/v1beta/models/ paths
#[tokio::test]
async fn rejects_path_traversal_in_model_path() {
    let hits = Arc::new(AtomicUsize::new(0));
    let hits_clone = Arc::clone(&hits);
    let router = Router::new().fallback(any(move || {
        let hits = Arc::clone(&hits_clone);
        async move {
            hits.fetch_add(1, Ordering::SeqCst);
            StatusCode::OK
        }
    }));
    let server = spawn_mock_server(router).await;
    let provider = provider_for(&server.base_url, Some("AIzaSyTraversalTest123"), None);

    let bad_paths = [
        "/v1beta/models/../../etc/passwd",
        "/v1beta/models/../secret",
        "/v1beta/models/.",
        "/v1beta/models/..",
        "/v1beta/models/gemini/../../admin",
        "/v1beta/models/foo/./bar",
        "/v1beta/other/endpoint",
        "/v1/models/gemini-4-hydrogen:generateContent",
        "/v1beta/models",
        "/v1beta/models/",
        "/v1beta/models/?alt=sse",
        "/V1BETA/models/gemini-4-hydrogen:generateContent",
        "//v1beta/models/gemini-4-hydrogen:generateContent",
        "https://evil.example/v1beta/models/gemini-4-hydrogen:generateContent",
        "",
    ];

    for bad in bad_paths {
        let with_secret_query = format!("{bad}?key=SUPER_SECRET_NEVER_ECHO");
        for candidate in [bad, with_secret_query.as_str()] {
            let err = provider
                .forward_gemini(ForwardRequest {
                    path_and_query: candidate.to_string(),
                    body: Bytes::from_static(b"{}"),
                    content_type: None,
                    api_key_override: None,
                })
                .await
                .expect_err(&format!("must reject path: {candidate:?}"));
            assert!(
                matches!(err, ProviderError::InvalidPath(_)),
                "expected InvalidPath for {candidate:?}, got {err:?}"
            );
            assert!(
                !err.to_string().contains("SUPER_SECRET_NEVER_ECHO"),
                "error leaked query secret for {candidate:?}: {err}"
            );
            assert!(
                !format!("{err:?}").contains("SUPER_SECRET_NEVER_ECHO"),
                "debug error leaked query secret for {candidate:?}: {err:?}"
            );
        }
    }
    assert_eq!(hits.load(Ordering::SeqCst), 0);
}

// 5. Base URL normalization matrix & config validation
#[tokio::test]
async fn normalizes_base_url_with_and_without_v1beta_and_trailing_slashes() {
    let cases = [
        (
            "https://generativelanguage.googleapis.com",
            "https://generativelanguage.googleapis.com",
        ),
        (
            "https://generativelanguage.googleapis.com/",
            "https://generativelanguage.googleapis.com",
        ),
        (
            "https://generativelanguage.googleapis.com///",
            "https://generativelanguage.googleapis.com",
        ),
        (
            "https://generativelanguage.googleapis.com/v1beta",
            "https://generativelanguage.googleapis.com",
        ),
        (
            "https://generativelanguage.googleapis.com/v1beta/",
            "https://generativelanguage.googleapis.com",
        ),
        (
            "https://generativelanguage.googleapis.com:443/v1beta",
            "https://generativelanguage.googleapis.com",
        ),
        (
            "http://127.0.0.1:4141/custom/v1beta",
            "http://127.0.0.1:4141/custom",
        ),
        (
            "http://127.0.0.1:4141/custom/v1beta/",
            "http://127.0.0.1:4141/custom",
        ),
        (
            "http://127.0.0.1:4141/custom//v1beta",
            "http://127.0.0.1:4141/custom",
        ),
        (
            "http://127.0.0.1:4141/v1beta/v1beta",
            "http://127.0.0.1:4141",
        ),
        (
            "http://127.0.0.1:4141/v1beta/v1beta/",
            "http://127.0.0.1:4141",
        ),
        (
            "http://127.0.0.1:4141/myv1beta",
            "http://127.0.0.1:4141/myv1beta",
        ),
        (
            "http://127.0.0.1:4141/v1beta/custom",
            "http://127.0.0.1:4141/v1beta/custom",
        ),
        ("https://[::1]:8443/v1beta/", "https://[::1]:8443"),
        ("HTTP://LOCALHOST:8080/v1beta", "http://localhost:8080"),
    ];

    for (raw, expected) in cases {
        let p = GeminiProvider::new(raw, Some("AIzaSyNormTest123456".to_string()), 60)
            .unwrap_or_else(|err| panic!("valid base_url {raw:?} failed: {err:?}"));
        assert_eq!(p.normalized_base_url(), expected, "raw={raw}");
        assert_eq!(p.timeout(), Duration::from_secs(60));
        // Idempotence check
        let p2 = GeminiProvider::new(
            p.normalized_base_url(),
            Some("AIzaSyNormTest123456".to_string()),
            60,
        )
        .expect("normalized base_url must remain valid");
        assert_eq!(p2.normalized_base_url(), expected);
    }

    // Invalid base_urls and timeout_secs == 0
    for bad_url in [
        "",
        "http://example.com",
        "http://127.0.0.1:0",
        "https://user:pass@generativelanguage.googleapis.com",
        "https://generativelanguage.googleapis.com?key=1",
        "https://generativelanguage.googleapis.com/v1beta/%252e%252e/evil",
    ] {
        let err = GeminiProvider::new(bad_url, None, 60)
            .expect_err(&format!("must reject base_url: {bad_url}"));
        assert!(matches!(err, ProviderError::InvalidConfig(_)));
    }
    let err = GeminiProvider::new("http://127.0.0.1:4141", None, 0)
        .expect_err("must reject timeout_secs == 0");
    assert!(matches!(err, ProviderError::InvalidConfig(_)));
    let err = GeminiProvider::new_with_timeout("http://127.0.0.1:4141", None, Duration::ZERO)
        .expect_err("must reject Duration::ZERO");
    assert!(matches!(err, ProviderError::InvalidConfig(_)));
    let err = GeminiProvider::from_config_with_timeout(
        &config_for("http://127.0.0.1:4141", 30),
        None,
        Duration::ZERO,
    )
    .expect_err("must reject from_config_with_timeout Duration::ZERO");
    assert!(matches!(err, ProviderError::InvalidConfig(_)));
    let sub_sec = GeminiProvider::from_config_with_timeout(
        &config_for("http://127.0.0.1:4141", 30),
        None,
        Duration::from_millis(150),
    )
    .expect("construct sub-second timeout provider");
    assert_eq!(sub_sec.timeout(), Duration::from_millis(150));

    // End-to-end wire URI verification across trailing `/`, `/v1beta`, `/v1beta/`, `/gw/v1beta`, `/myv1beta`
    let seen_uris = Arc::new(Mutex::new(Vec::<String>::new()));
    let seen_clone = Arc::clone(&seen_uris);
    let router = Router::new().fallback(any(move |OriginalUri(uri): OriginalUri| {
        let seen = Arc::clone(&seen_clone);
        async move {
            seen.lock().await.push(uri.to_string());
            StatusCode::OK
        }
    }));
    let server = spawn_mock_server(router).await;

    let wire_cases = [
        (
            server.base_url.clone(),
            "/v1beta/models/gemini-4-hydrogen:generateContent",
            "/v1beta/models?pageSize=1",
        ),
        (
            format!("{}/", server.base_url),
            "/v1beta/models/gemini-4-hydrogen:generateContent",
            "/v1beta/models?pageSize=1",
        ),
        (
            format!("{}/v1beta", server.base_url),
            "/v1beta/models/gemini-4-hydrogen:generateContent",
            "/v1beta/models?pageSize=1",
        ),
        (
            format!("{}/v1beta/", server.base_url),
            "/v1beta/models/gemini-4-hydrogen:generateContent",
            "/v1beta/models?pageSize=1",
        ),
        (
            format!("{}/v1beta/v1beta", server.base_url),
            "/v1beta/models/gemini-4-hydrogen:generateContent",
            "/v1beta/models?pageSize=1",
        ),
        (
            format!("{}/gw/v1beta/", server.base_url),
            "/gw/v1beta/models/gemini-4-hydrogen:generateContent",
            "/gw/v1beta/models?pageSize=1",
        ),
        (
            format!("{}/myv1beta", server.base_url),
            "/myv1beta/v1beta/models/gemini-4-hydrogen:generateContent",
            "/myv1beta/v1beta/models?pageSize=1",
        ),
    ];

    for (base, expected_post_uri, expected_health_uri) in wire_cases {
        seen_uris.lock().await.clear();
        let provider = provider_for(&base, Some("AIzaSyWireNorm123456"), None);
        let resp = provider
            .forward_gemini(ForwardRequest {
                path_and_query: "/v1beta/models/gemini-4-hydrogen:generateContent".to_string(),
                body: Bytes::from_static(b"{}"),
                content_type: None,
                api_key_override: None,
            })
            .await
            .expect("forward ok");
        assert_eq!(resp.status, 200);
        provider.health_check().await.expect("health_check ok");

        let captured = seen_uris.lock().await.clone();
        assert_eq!(
            captured,
            vec![
                expected_post_uri.to_string(),
                expected_health_uri.to_string()
            ],
            "base={base}"
        );
    }
}

// 6. Query hygiene: strip any key variant, preserve safe params, reject smuggled/malformed queries
#[tokio::test]
async fn strips_key_query_params_case_insensitively_and_preserves_alt_sse() {
    let seen_uri = Arc::new(Mutex::new(None::<String>));
    let seen_clone = Arc::clone(&seen_uri);
    let router = Router::new().route(
        "/v1beta/models/{*rest}",
        post(move |OriginalUri(uri): OriginalUri| {
            let seen = Arc::clone(&seen_clone);
            async move {
                *seen.lock().await = Some(uri.to_string());
                StatusCode::OK
            }
        }),
    );
    let server = spawn_mock_server(router).await;
    let provider = provider_for(&server.base_url, Some("AIzaSyQueryHygiene12"), None);

    let base_path = "/v1beta/models/gemini-4-hydrogen:streamGenerateContent";
    let accept_cases = [
        ("?alt=sse&key=SECRET_1", format!("{base_path}?alt=sse")),
        ("?key=SECRET_1&alt=sse", format!("{base_path}?alt=sse")),
        ("?KEY=SECRET_1&alt=sse", format!("{base_path}?alt=sse")),
        ("?Key=SECRET_1", base_path.to_string()),
        ("?%6bey=SECRET_1&alt=sse", format!("{base_path}?alt=sse")),
        ("?%4B%45%59=SECRET_1", base_path.to_string()),
        ("?k%65y=SECRET_1&alt=sse", format!("{base_path}?alt=sse")),
        ("?key", base_path.to_string()),
        ("?KEY", base_path.to_string()),
        ("?%6bey", base_path.to_string()),
        ("?+key+=SECRET_1&alt=sse", format!("{base_path}?alt=sse")),
        (
            "?%20key%20=SECRET_1&alt=sse",
            format!("{base_path}?alt=sse"),
        ),
        (
            "?%c2%a0key=SECRET_1&alt=sse",
            format!("{base_path}?alt=sse"),
        ),
        ("?key=SECRET_1&KEY=SECRET_2", base_path.to_string()),
        ("?", base_path.to_string()),
        ("?&&", base_path.to_string()),
        (
            "?alt=sse&foo=1&foo=2",
            format!("{base_path}?alt=sse&foo=1&foo=2"),
        ),
        ("?alt=sse&flag", format!("{base_path}?alt=sse&flag")),
        ("?alt=sse&flag=", format!("{base_path}?alt=sse&flag=")),
        (
            "?alt=sse&filter=50%25",
            format!("{base_path}?alt=sse&filter=50%25"),
        ),
        ("?alt=sse&q=a?b", format!("{base_path}?alt=sse&q=a%3Fb")),
    ];

    for (query, expected_uri) in accept_cases {
        *seen_uri.lock().await = None;
        let resp = provider
            .forward_gemini(ForwardRequest {
                path_and_query: format!("{base_path}{query}"),
                body: Bytes::from_static(b"{}"),
                content_type: None,
                api_key_override: None,
            })
            .await
            .unwrap_or_else(|err| panic!("query {query:?} should succeed: {err:?}"));
        assert_eq!(resp.status, 200);
        let actual = seen_uri.lock().await.clone().expect("request captured");
        assert_eq!(actual, expected_uri, "query={query}");
        assert!(!actual.contains("SECRET_1"));
        assert!(!actual.contains("SECRET_2"));
    }

    // Must-reject queries (double-encoded key smuggling, invisible Unicode key, control chars, bad %HH/UTF-8)
    let reject_queries = [
        "?%256bey=SECRET_1",
        "?%25256bey=SECRET_1",
        "?%e2%80%8bkey=SECRET_1",
        "?alt=sse&x=%00",
        "?alt=sse&x=%0d%0a",
        "?alt=sse&x=%7f",
        "?alt=sse&x=%c2%85",
        "?alt=sse&x=%",
        "?alt=sse&x=%2",
        "?alt=sse&x=%ZZ",
        "?alt=sse&x=%FF",
        "?alt=sse&x=%C0%AF",
        "?key=%00",
        "?%00=1",
        "?=val",
        "?alt=sse&=val",
    ];
    for bad_q in reject_queries {
        let err = provider
            .forward_gemini(ForwardRequest {
                path_and_query: format!("{base_path}{bad_q}"),
                body: Bytes::from_static(b"{}"),
                content_type: None,
                api_key_override: None,
            })
            .await
            .expect_err(&format!("query must be rejected: {bad_q}"));
        assert!(
            matches!(err, ProviderError::InvalidPath(_)),
            "expected InvalidPath for {bad_q}, got {err:?}"
        );
        assert!(!err.to_string().contains("SECRET_1"));
    }
}

// 7. Hardened path security + exhaustive 256-byte path-segment probe
#[tokio::test]
async fn rejects_encoded_traversal_matrix_fragment_empty_and_malformed_paths() {
    let hits = Arc::new(AtomicUsize::new(0));
    let hits_clone = Arc::clone(&hits);
    let router = Router::new().fallback(any(move || {
        let hits = Arc::clone(&hits_clone);
        async move {
            hits.fetch_add(1, Ordering::SeqCst);
            StatusCode::OK
        }
    }));
    let server = spawn_mock_server(router).await;
    let provider = provider_for(&server.base_url, Some("AIzaSyPathHardening12"), None);

    let rejected = [
        "/v1beta/models/%2e",
        "/v1beta/models/%2E",
        "/v1beta/models/%2e%2e/admin",
        "/v1beta/models/.%2e/admin",
        "/v1beta/models/%2e%2e%2fadmin",
        "/v1beta/models/%2E%2E%2Fadmin",
        "/v1beta/models/..%2fadmin",
        "/v1beta/models/..%2Fadmin",
        "/v1beta/models/..%5cadmin",
        "/v1beta/models/..%5Cadmin",
        "/v1beta/models/..%252fadmin",
        "/v1beta/models/%252e%252e/admin",
        "/v1beta/models/%25252e%25252e/admin",
        "/v1beta/models/%2525252e%2525252e/admin",
        "/v1beta/models/..;param/admin",
        "/v1beta/models/.;param",
        "/v1beta/models/..%3bparam",
        "/v1beta/models/gemini;v=1",
        "/v1beta/models/gemini%00",
        "/v1beta/models/gemini\0pro",
        "/v1beta/models/gemini%20pro",
        "/v1beta/models/gemini pro",
        "/v1beta/models/gemini%3falt=sse",
        "/v1beta/models/gemini%23frag",
        "/v1beta/models/gemini#frag",
        "/v1beta/models/gemini%",
        "/v1beta/models/gemini%2",
        "/v1beta/models/gemini%ZZ",
        "/v1beta/models/%%32%65",
        "/v1beta/models/gemini\\..\\admin",
        "/v1beta/models/gemini\r\nX-Hdr: 1",
        "/v1beta/models/\u{ff0e}\u{ff0e}/admin",
        "/v1beta/models/gemini\u{200b}",
        "/v1beta/models//gemini-4-hydrogen",
        "/v1beta/models/gemini-4-hydrogen/",
        "/v1beta/models/a//b",
    ];

    for bad in rejected {
        let err = provider
            .forward_gemini(ForwardRequest {
                path_and_query: bad.to_string(),
                body: Bytes::from_static(b"{}"),
                content_type: None,
                api_key_override: None,
            })
            .await
            .expect_err(&format!("must reject: {bad:?}"));
        assert!(
            matches!(err, ProviderError::InvalidPath(_)),
            "expected InvalidPath for {bad:?}, got {err:?}"
        );
    }
    assert_eq!(hits.load(Ordering::SeqCst), 0);

    // Valid paths with `:`, `.`, `-`, `_`, multi-segment, and valid single-pass %HH into safe alphabet
    let valid_paths = [
        "/v1beta/models/gemini-4-hydrogen:generateContent",
        "/v1beta/models/gemini-4-hydrogen:streamGenerateContent?alt=sse",
        "/v1beta/models/gemini-4-hydrogen",
        "/v1beta/models/gemma3:4b:generateContent",
        "/v1beta/models/gemini_2.5_pro.v1:countTokens",
        "/v1beta/models/a..b:generateContent",
        "/v1beta/models/gemini%2D2.5-pro:generateContent",
        "/v1beta/models/org/model:generateContent",
    ];
    for ok_path in valid_paths {
        let resp = provider
            .forward_gemini(ForwardRequest {
                path_and_query: ok_path.to_string(),
                body: Bytes::from_static(b"{}"),
                content_type: None,
                api_key_override: None,
            })
            .await
            .unwrap_or_else(|err| panic!("must accept valid path {ok_path:?}: {err:?}"));
        assert_eq!(resp.status, 200);
    }
    assert_eq!(hits.load(Ordering::SeqCst), valid_paths.len());
}

// 7b. Exhaustive 256-byte path-segment probe (literal + %00..%FF lower & upper hex)
#[test]
fn exhaustive_256_byte_path_segment_probe_matches_rfc3986_closed_set() {
    let provider = GeminiProvider::new(
        "https://generativelanguage.googleapis.com",
        Some("AIzaSyProbeKey123456".to_string()),
        60,
    )
    .expect("construct provider");

    let is_expected_safe = |b: u8| -> bool {
        b.is_ascii_alphanumeric()
            || matches!(
                b,
                b'-' | b'.'
                    | b'_'
                    | b'~'
                    | b'!'
                    | b'$'
                    | b'&'
                    | b'\''
                    | b'('
                    | b')'
                    | b'*'
                    | b'+'
                    | b','
                    | b'='
                    | b':'
                    | b'@'
            )
    };

    let mut safe_count = 0usize;
    for byte in 0u8..=255u8 {
        let expected = is_expected_safe(byte);
        if expected {
            safe_count += 1;
        }

        // Test %xx (lower) and %XX (upper) embedded inside a non-dot segment `a<enc>b`
        for encoded in [format!("%{byte:02x}"), format!("%{byte:02X}")] {
            let candidate = format!("/v1beta/models/a{encoded}b:generateContent");
            let res = provider.build_target_url(&candidate);
            assert_eq!(
                res.is_ok(),
                expected,
                "percent-encoded byte 0x{byte:02X} ({encoded}) mismatch: {res:?}"
            );
        }

        // Test literal ASCII bytes embedded inside `a<char>b` (skipping `/` and `?` which have structural roles)
        if byte <= 0x7F && byte != b'/' && byte != b'?' {
            let ch = byte as char;
            let candidate = format!("/v1beta/models/a{ch}b:generateContent");
            let res = provider.build_target_url(&candidate);
            assert_eq!(
                res.is_ok(),
                expected,
                "literal byte 0x{byte:02X} ({ch:?}) mismatch: {res:?}"
            );
        }
    }
    assert_eq!(
        safe_count, 78,
        "exact RFC 3986 pchar \\ {{';', '%'}} cardinality must be 78"
    );
}

// 8. Edition 2024 zero-unsafe API key precedence & MissingApiKey
#[tokio::test]
async fn resolves_api_key_precedence_and_returns_missing_api_key_without_unsafe() {
    let seen_key = Arc::new(Mutex::new(None::<String>));
    let hits = Arc::new(AtomicUsize::new(0));
    let seen_clone = Arc::clone(&seen_key);
    let hits_clone = Arc::clone(&hits);
    let router = Router::new().fallback(any(move |headers: HeaderMap| {
        let seen = Arc::clone(&seen_clone);
        let hits = Arc::clone(&hits_clone);
        async move {
            hits.fetch_add(1, Ordering::SeqCst);
            *seen.lock().await = headers
                .get("x-goog-api-key")
                .and_then(|v| v.to_str().ok())
                .map(str::to_owned);
            StatusCode::OK
        }
    }));
    let server = spawn_mock_server(router).await;

    // (1) override beats configured & env
    let p = provider_for(
        &server.base_url,
        Some("AIzaSyConfiguredKey12"),
        Some("AIzaSyEnvFallbackKey1"),
    );
    p.forward_gemini(ForwardRequest {
        path_and_query: "/v1beta/models/gemini-4-hydrogen:generateContent".to_string(),
        body: Bytes::from_static(b"{}"),
        content_type: None,
        api_key_override: Some("  AIzaSyOverrideKey1234  ".to_string()),
    })
    .await
    .expect("override should succeed");
    assert_eq!(
        seen_key.lock().await.as_deref(),
        Some("AIzaSyOverrideKey1234")
    );

    // (2) whitespace-only override falls back to configured
    p.forward_gemini(ForwardRequest {
        path_and_query: "/v1beta/models/gemini-4-hydrogen:generateContent".to_string(),
        body: Bytes::from_static(b"{}"),
        content_type: None,
        api_key_override: Some("   ".to_string()),
    })
    .await
    .expect("configured fallback should succeed");
    assert_eq!(
        seen_key.lock().await.as_deref(),
        Some("AIzaSyConfiguredKey12")
    );

    // (3) None or whitespace-only configured key falls back to env (trimmed)
    for cfg_key in [None, Some("   ")] {
        let p_env = provider_for(&server.base_url, cfg_key, Some("  AIzaSyEnvOnlyKey12345  "));
        p_env
            .forward_gemini(ForwardRequest {
                path_and_query: "/v1beta/models/gemini-4-hydrogen:generateContent".to_string(),
                body: Bytes::from_static(b"{}"),
                content_type: None,
                api_key_override: None,
            })
            .await
            .expect("env fallback should succeed");
        assert_eq!(
            seen_key.lock().await.as_deref(),
            Some("AIzaSyEnvOnlyKey12345")
        );
        p_env
            .health_check()
            .await
            .expect("health_check should use env fallback");
        assert_eq!(
            seen_key.lock().await.as_deref(),
            Some("AIzaSyEnvOnlyKey12345")
        );
    }

    // (4) all None / blank (and public `.with_env_fallback(false)` even if host GEMINI_API_KEY is set)
    let hits_before = hits.load(Ordering::SeqCst);
    let p_missing_fixed = provider_for(&server.base_url, Some("   "), Some("   "));
    let p_missing_disabled = GeminiProvider::new(&server.base_url, None, DEFAULT_TIMEOUT_SECS)
        .expect("construct without key")
        .with_env_fallback(false);

    for provider in [&p_missing_fixed, &p_missing_disabled] {
        let err = provider
            .forward_gemini(ForwardRequest {
                path_and_query: "/v1beta/models/gemini-4-hydrogen:generateContent".to_string(),
                body: Bytes::from_static(b"{}"),
                content_type: None,
                api_key_override: None,
            })
            .await
            .expect_err("must return MissingApiKey");
        assert!(matches!(err, ProviderError::MissingApiKey));

        let err = provider
            .health_check()
            .await
            .expect_err("health_check must return MissingApiKey");
        assert!(matches!(err, ProviderError::MissingApiKey));
    }
    assert_eq!(hits.load(Ordering::SeqCst), hits_before);
}

// 9. M1 carry-forward: reject non-ASCII & control-char API keys across all 3 sources without secret echo
#[tokio::test]
async fn rejects_non_ascii_and_control_api_keys_without_leaking_secret() {
    let bad_keys = [
        "AIzaSySecretNonAscii\u{1F511}1234",
        "AIzaSySecret\r\nX-Injected: 1",
        "AIzaSySecretValid\n",
        "AIzaSySecret\0tail",
        "AIzaSySecret\t1234",
        "AIzaSySecret\u{0085}1234",
        "AIzaSySecret space",
        "\r\n",
    ];

    for bad in bad_keys {
        // Source 1: configured api_key (eager validation in new & ApiKey::parse_from_disk)
        let parse_err =
            ApiKey::parse_from_disk(bad).expect_err(&format!("must reject disk key: {bad:?}"));
        assert!(!parse_err.to_string().contains("AIzaSySecret"));
        assert!(!parse_err.to_string().contains("X-Injected"));
        assert!(!format!("{parse_err:?}").contains("AIzaSySecret"));
        assert!(!format!("{parse_err:?}").contains('\u{1F511}'));

        let err = GeminiProvider::new(
            "http://127.0.0.1:4141",
            Some(bad.to_string()),
            DEFAULT_TIMEOUT_SECS,
        )
        .expect_err(&format!("must reject configured key: {bad:?}"));
        assert!(matches!(err, ProviderError::InvalidConfig(_)));
        assert!(!err.to_string().contains("AIzaSySecret"));
        assert!(!err.to_string().contains("X-Injected"));
        assert!(!format!("{err:?}").contains("AIzaSySecret"));
        assert!(!format!("{err:?}").contains('\u{1F511}'));

        // Source 2: request api_key_override
        let p = provider_for("http://127.0.0.1:4141", Some("AIzaSyGoodKey1234567"), None);
        let err = p
            .forward_gemini(ForwardRequest {
                path_and_query: "/v1beta/models/gemini-4-hydrogen:generateContent".to_string(),
                body: Bytes::from_static(b"{}"),
                content_type: None,
                api_key_override: Some(bad.to_string()),
            })
            .await
            .expect_err(&format!("must reject override key: {bad:?}"));
        assert!(matches!(err, ProviderError::InvalidConfig(_)));
        assert!(!err.to_string().contains("AIzaSySecret"));
        assert!(!format!("{err:?}").contains("AIzaSySecret"));

        // Source 3: env key fallback
        let p_env = provider_for("http://127.0.0.1:4141", None, Some(bad));
        let err = p_env
            .forward_gemini(ForwardRequest {
                path_and_query: "/v1beta/models/gemini-4-hydrogen:generateContent".to_string(),
                body: Bytes::from_static(b"{}"),
                content_type: None,
                api_key_override: None,
            })
            .await
            .expect_err(&format!("must reject env key: {bad:?}"));
        assert!(matches!(err, ProviderError::InvalidConfig(_)));
        assert!(!err.to_string().contains("AIzaSySecret"));
        assert!(!format!("{err:?}").contains("AIzaSySecret"));
    }
}

// 10. Custom Debug redaction on GeminiProvider, ForwardRequest, ForwardResponse
#[test]
fn debug_redacts_api_keys_on_gemini_provider_and_forward_request() {
    let key = ApiKey::parse_strict("AIzaSyTopSecretConfigured9999").unwrap();
    let provider = GeminiProvider::from_config(
        &GeminiConfig {
            base_url: "https://generativelanguage.googleapis.com/v1beta/".to_string(),
            timeout_secs: 900,
            ..GeminiConfig::default()
        },
        Some(&key),
    )
    .expect("construct provider from_config")
    .with_env_fallback(true);
    assert_eq!(provider.api_key_hint().as_deref(), Some("AIza...9999"));
    assert_eq!(provider.timeout(), Duration::from_secs(900));

    let provider_dbg = format!("{provider:?}");
    assert!(!provider_dbg.contains("TopSecretConfigured"));
    assert!(!provider_dbg.contains("AIzaSyTopSecretConfigured9999"));
    assert!(provider_dbg.contains("AIza...9999"));
    assert!(provider_dbg.contains("https://generativelanguage.googleapis.com"));

    let req = ForwardRequest {
        path_and_query:
            "/v1beta/models/gemini-4-hydrogen:generateContent?key=QUERY_SECRET_VALUE&alt=sse"
                .to_string(),
        body: Bytes::from_static(b"SUPER_PRIVATE_PROMPT_BODY"),
        content_type: Some("application/json".to_string()),
        api_key_override: Some("AIzaSyTopSecretOverride8888".to_string()),
    };
    let req_dbg = format!("{req:?}");
    assert!(!req_dbg.contains("TopSecretOverride"));
    assert!(!req_dbg.contains("QUERY_SECRET_VALUE"));
    assert!(!req_dbg.contains("SUPER_PRIVATE_PROMPT_BODY"));
    assert!(req_dbg.contains("AIza...8888"));
    assert!(req_dbg.contains("?<redacted>"));
    assert!(req_dbg.contains("body_len: 25"));

    // Path without query string should not gain a bogus `?<redacted>` suffix
    let req_no_q = ForwardRequest {
        path_and_query: "/v1beta/models/gemini-4-hydrogen:generateContent".to_string(),
        body: Bytes::new(),
        content_type: None,
        api_key_override: None,
    };
    let req_no_q_dbg = format!("{req_no_q:?}");
    assert!(!req_no_q_dbg.contains("?<redacted>"));
    assert!(req_no_q_dbg.contains("/v1beta/models/gemini-4-hydrogen:generateContent"));

    // ForwardResponse Debug compiles and formats cleanly
    let resp = ForwardResponse {
        status: 200,
        content_type: Some("application/json".to_string()),
        stream: Box::pin(stream::empty()),
    };
    let resp_dbg = format!("{resp:?}");
    assert!(resp_dbg.contains("<ByteStream>"));
}

// 11. Content-Type default and invalid header handling
#[tokio::test]
async fn handles_default_and_invalid_content_type_header() {
    let seen_ct = Arc::new(Mutex::new(None::<String>));
    let hits = Arc::new(AtomicUsize::new(0));
    let seen_clone = Arc::clone(&seen_ct);
    let hits_clone = Arc::clone(&hits);
    let router = Router::new().route(
        "/v1beta/models/{*rest}",
        post(move |headers: HeaderMap| {
            let seen = Arc::clone(&seen_clone);
            let hits = Arc::clone(&hits_clone);
            async move {
                hits.fetch_add(1, Ordering::SeqCst);
                *seen.lock().await = headers
                    .get(http::header::CONTENT_TYPE)
                    .and_then(|v| v.to_str().ok())
                    .map(str::to_owned);
                StatusCode::OK
            }
        }),
    );
    let server = spawn_mock_server(router).await;
    let provider = provider_for(&server.base_url, Some("AIzaSyCtTestKey12345"), None);

    // None defaults to application/json
    provider
        .forward_gemini(ForwardRequest {
            path_and_query: "/v1beta/models/gemini-4-hydrogen:generateContent".to_string(),
            body: Bytes::from_static(b"{}"),
            content_type: None,
            api_key_override: None,
        })
        .await
        .expect("None content_type defaults to application/json");
    assert_eq!(seen_ct.lock().await.as_deref(), Some("application/json"));

    // Valid MIME with parameters is preserved
    provider
        .forward_gemini(ForwardRequest {
            path_and_query: "/v1beta/models/gemini-4-hydrogen:generateContent".to_string(),
            body: Bytes::from_static(b"{}"),
            content_type: Some("application/json; charset=utf-8".to_string()),
            api_key_override: None,
        })
        .await
        .expect("valid content_type succeeds");
    assert_eq!(
        seen_ct.lock().await.as_deref(),
        Some("application/json; charset=utf-8")
    );

    let hits_before_invalid = hits.load(Ordering::SeqCst);
    let bad_cts = [
        "",
        "   ",
        "application/json\r\nX-Evil: 1",
        "application/json\n",
        "text/plain\0",
        "text/plain\t",
        "application/json\u{1F511}",
    ];
    for bad in bad_cts {
        let err = provider
            .forward_gemini(ForwardRequest {
                path_and_query: "/v1beta/models/gemini-4-hydrogen:generateContent".to_string(),
                body: Bytes::from_static(b"{}"),
                content_type: Some(bad.to_string()),
                api_key_override: None,
            })
            .await
            .expect_err(&format!("must reject bad content_type: {bad:?}"));
        assert!(matches!(err, ProviderError::InvalidConfig(_)));
        assert!(!err.to_string().contains("X-Evil"));
        assert!(!err.to_string().contains('\u{1F511}'));
    }
    assert_eq!(hits.load(Ordering::SeqCst), hits_before_invalid);
}

// 12. health_check: 200 OK, Gemini 400 API_KEY_INVALID, key-reflection resistance (incl. all-caps key), non-2xx, closed port
#[tokio::test]
async fn health_check_succeeds_on_200_and_sanitizes_errors_including_key_reflection() {
    // (a) 200 OK on GET /v1beta/models?pageSize=1 with x-goog-api-key
    let seen = Arc::new(Mutex::new(None::<(String, Option<String>)>));
    let seen_clone = Arc::clone(&seen);
    let ok_server = spawn_mock_server(Router::new().route(
        "/v1beta/models",
        get(move |OriginalUri(uri): OriginalUri, headers: HeaderMap| {
            let seen = Arc::clone(&seen_clone);
            async move {
                let key = headers
                    .get("x-goog-api-key")
                    .and_then(|v| v.to_str().ok())
                    .map(str::to_owned);
                *seen.lock().await = Some((uri.to_string(), key));
                (StatusCode::OK, r#"{"models":[]}"#)
            }
        }),
    ))
    .await;
    let provider = provider_for(&ok_server.base_url, Some("AIzaSyHealthOk123456"), None);
    provider
        .health_check()
        .await
        .expect("200 health check should succeed");
    let (uri, key) = seen.lock().await.clone().expect("health check captured");
    assert_eq!(uri, "/v1beta/models?pageSize=1");
    assert_eq!(key.as_deref(), Some("AIzaSyHealthOk123456"));

    // (b) Real Gemini 400 API_KEY_INVALID
    let invalid_key_server = spawn_mock_server(Router::new().route(
        "/v1beta/models",
        get(|| async {
            (
                StatusCode::BAD_REQUEST,
                r#"{"error":{"code":400,"message":"API key not valid.","status":"INVALID_ARGUMENT","details":[{"@type":"type.googleapis.com/google.rpc.ErrorInfo","reason":"API_KEY_INVALID"}]}}"#,
            )
        }),
    ))
    .await;
    let provider = provider_for(
        &invalid_key_server.base_url,
        Some("AIzaSyBadKey12345678"),
        None,
    );
    let err = provider
        .health_check()
        .await
        .expect_err("400 must fail health_check");
    assert!(matches!(err, ProviderError::Upstream(_)));
    let msg = err.to_string();
    assert!(msg.contains("400"), "msg={msg}");
    assert!(msg.contains("API_KEY_INVALID"), "msg={msg}");
    assert!(!msg.contains("AIzaSyBadKey12345678"));

    // (b2) Gemini 429 JSON error with status fallback (no details array)
    let quota_server = spawn_mock_server(Router::new().route(
        "/v1beta/models",
        get(|| async {
            (
                StatusCode::TOO_MANY_REQUESTS,
                r#"{"error":{"code":429,"message":"Quota exceeded.","status":"RESOURCE_EXHAUSTED"}}"#,
            )
        }),
    ))
    .await;
    let provider = provider_for(&quota_server.base_url, Some("AIzaSyQuotaKey123456"), None);
    let err = provider
        .health_check()
        .await
        .expect_err("429 must fail health_check");
    assert!(matches!(err, ProviderError::Upstream(_)));
    let msg = err.to_string();
    assert!(msg.contains("429"), "msg={msg}");
    assert!(msg.contains("RESOURCE_EXHAUSTED"), "msg={msg}");
    assert!(!msg.contains("Quota exceeded"));

    // (c) Adversarial 400 echoing normal key AND all-caps key across status/message/reason + huge body (>8KiB)
    for secret in ["AIzaSyReflectedKey9999", "SUPER_SECRET_KEY_9999"] {
        let secret_owned = secret.to_string();
        let hostile_server = spawn_mock_server(Router::new().route(
            "/v1beta/models",
            get(move || {
                let secret_owned = secret_owned.clone();
                async move {
                    let padding = "A".repeat(16 * 1024);
                    let body = format!(
                        r#"{{"error":{{"status":"{secret_owned}","message":"bad key {secret_owned}","details":[{{"reason":"{secret_owned}"}}]}}}}{padding}"#
                    );
                    (StatusCode::BAD_REQUEST, body)
                }
            }),
        ))
        .await;
        let p = provider_for(&hostile_server.base_url, Some(secret), None);
        let err = p
            .health_check()
            .await
            .expect_err("hostile 400 must fail health_check");
        assert!(matches!(err, ProviderError::Upstream(_)));
        let disp = err.to_string();
        let dbg = format!("{err:?}");
        assert!(disp.contains("400"), "disp={disp}");
        assert!(!disp.contains(secret), "leaked secret in Display: {disp}");
        assert!(!dbg.contains(secret), "leaked secret in Debug: {dbg}");
    }

    // (d) 403 / 429 / 500 -> Upstream with status code
    for status_code in [403u16, 429, 500] {
        let server = spawn_mock_server(Router::new().route(
            "/v1beta/models",
            get(move || async move {
                (
                    StatusCode::from_u16(status_code).unwrap(),
                    "plain text failure body",
                )
            }),
        ))
        .await;
        let p = provider_for(&server.base_url, Some("AIzaSyStatusCheck123"), None);
        let err = p.health_check().await.expect_err("non-2xx must fail");
        assert!(matches!(err, ProviderError::Upstream(_)));
        assert!(err.to_string().contains(&status_code.to_string()));
        assert!(!err.to_string().contains("plain text failure body"));
    }

    // (e) Closed loopback port -> Upstream with zero key/URL leak
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind ephemeral port");
    let closed_addr = listener.local_addr().unwrap();
    drop(listener);
    let p = provider_for(
        &format!("http://{closed_addr}"),
        Some("AIzaSyClosedPortKey99"),
        None,
    );
    let err = p
        .health_check()
        .await
        .expect_err("closed port must return Upstream error");
    assert!(matches!(err, ProviderError::Upstream(_)));
    assert!(!err.to_string().contains("AIzaSyClosedPortKey99"));
    assert!(!err.to_string().contains("/v1beta/models"));
}

// 13. Redirect refusal: 2-server 302 test proving redirect target is NEVER contacted
#[tokio::test]
async fn refuses_redirects_and_never_forwards_api_key_to_redirect_target() {
    let target_hits = Arc::new(AtomicUsize::new(0));
    let target_hits_clone = Arc::clone(&target_hits);
    let target_server = spawn_mock_server(Router::new().fallback(any(move || {
        let hits = Arc::clone(&target_hits_clone);
        async move {
            hits.fetch_add(1, Ordering::SeqCst);
            StatusCode::OK
        }
    })))
    .await;

    let location = format!("{}/steal", target_server.base_url);
    let redirect_server = spawn_mock_server(Router::new().fallback(any(move || {
        let location = location.clone();
        async move {
            Response::builder()
                .status(StatusCode::FOUND)
                .header(http::header::LOCATION, location)
                .body(Body::from("redirecting"))
                .unwrap()
        }
    })))
    .await;

    let provider = provider_for(
        &redirect_server.base_url,
        Some("AIzaSyDoNotExfiltrate9"),
        None,
    );

    let resp = provider
        .forward_gemini(ForwardRequest {
            path_and_query: "/v1beta/models/gemini-4-hydrogen:generateContent".to_string(),
            body: Bytes::from_static(b"{}"),
            content_type: None,
            api_key_override: None,
        })
        .await
        .expect("forward_gemini propagates 302 verbatim");
    assert_eq!(resp.status, 302);
    assert_eq!(collect_stream(resp).await, b"redirecting");
    assert_eq!(target_hits.load(Ordering::SeqCst), 0);

    let err = provider
        .health_check()
        .await
        .expect_err("health_check rejects 302");
    assert!(matches!(err, ProviderError::Upstream(_)));
    assert!(err.to_string().contains("302"));
    assert_eq!(target_hits.load(Ordering::SeqCst), 0);
}

// 14. Request timeout enforcement & sanitized error
#[tokio::test]
async fn enforces_request_timeout_and_sanitizes_timeout_error() {
    let slow_server = spawn_mock_server(Router::new().route(
        "/v1beta/models/{*rest}",
        post(|| async {
            tokio::time::sleep(Duration::from_secs(5)).await;
            StatusCode::OK
        }),
    ))
    .await;

    let provider = GeminiProvider::new_with_timeout(
        &slow_server.base_url,
        Some("AIzaSyTimeoutSecret99".to_string()),
        Duration::from_millis(150),
    )
    .expect("construct 150ms timeout provider")
    .with_env_fallback(false);

    let start = Instant::now();
    let err = provider
        .forward_gemini(ForwardRequest {
            path_and_query: "/v1beta/models/gemini-4-hydrogen:generateContent".to_string(),
            body: Bytes::from_static(b"{}"),
            content_type: None,
            api_key_override: None,
        })
        .await
        .expect_err("slow upstream must time out");
    let elapsed = start.elapsed();

    assert!(
        elapsed < Duration::from_millis(1500),
        "timeout took too long: {elapsed:?}"
    );
    assert!(matches!(err, ProviderError::Upstream(_)));
    let msg = err.to_string();
    assert!(msg.contains("timed out"), "msg={msg}");
    assert!(!msg.contains("AIzaSyTimeoutSecret99"));
    assert!(!msg.contains("/v1beta/models"));
}

// 15. Dyn-compatibility (`Arc<dyn ProviderAdapter>`)
#[tokio::test]
async fn provider_adapter_is_object_safe_behind_arc_dyn() {
    let server = spawn_mock_server(
        Router::new()
            .route("/v1beta/models", get(|| async { StatusCode::OK }))
            .route(
                "/v1beta/models/{*rest}",
                post(|| async { (StatusCode::OK, r#"{"ok":true}"#) }),
            ),
    )
    .await;

    let provider = provider_for(&server.base_url, Some("AIzaSyDynSafeKey12345"), None);
    let adapter: Arc<dyn ProviderAdapter> = Arc::new(provider);

    assert_eq!(adapter.name(), "gemini");
    adapter.health_check().await.expect("health_check via dyn");
    let resp = adapter
        .forward_gemini(ForwardRequest {
            path_and_query: "/v1beta/models/gemini-4-hydrogen:generateContent".to_string(),
            body: Bytes::from_static(b"{}"),
            content_type: None,
            api_key_override: None,
        })
        .await
        .expect("forward_gemini via dyn");
    assert_eq!(resp.status, 200);
    assert_eq!(collect_stream(resp).await, br#"{"ok":true}"#);
}

// 16. Regression: reject non-ASCII Unicode whitespace in API keys across all 3 sources
// without silent credential fallback or secret echo, while preserving ASCII space trimming.
#[tokio::test]
async fn rejects_unicode_whitespace_in_api_keys_without_silent_fallback_or_echo() {
    let seen_key = Arc::new(Mutex::new(None::<String>));
    let hits = Arc::new(AtomicUsize::new(0));
    let seen_clone = Arc::clone(&seen_key);
    let hits_clone = Arc::clone(&hits);

    let router = Router::new().fallback(any(move |headers: HeaderMap| {
        let seen = Arc::clone(&seen_clone);
        let hits = Arc::clone(&hits_clone);
        async move {
            hits.fetch_add(1, Ordering::SeqCst);
            *seen.lock().await = headers
                .get("x-goog-api-key")
                .and_then(|v| v.to_str().ok())
                .map(str::to_owned);
            StatusCode::OK
        }
    }));
    let server = spawn_mock_server(router).await;

    let unicode_ws_keys = [
        "\u{00A0}",
        "\u{00A0}AIzaSyValid1234",
        "AIzaSyValid1234\u{00A0}",
        "\u{2028}AIzaSyValid1234",
        "AIzaSyValid1234\u{2029}",
        "\u{3000}",
    ];

    let assert_sanitized_invalid_config = |err: ProviderError, label: &str, bad: &str| {
        assert!(
            matches!(err, ProviderError::InvalidConfig(_)),
            "expected InvalidConfig for {label} ({bad:?}), got {err:?}"
        );
        let disp = err.to_string();
        let dbg = format!("{err:?}");
        for forbidden in [
            "AIzaSyValid1234",
            "AIzaSyConfiguredFallback1234",
            "\u{00A0}",
            "\u{2028}",
            "\u{2029}",
            "\u{3000}",
        ] {
            assert!(
                !disp.contains(forbidden),
                "Display leaked {forbidden:?} for {label} ({bad:?}): {disp}"
            );
            assert!(
                !dbg.contains(forbidden),
                "Debug leaked {forbidden:?} for {label} ({bad:?}): {dbg}"
            );
        }
    };

    let provider_with_configured_key = GeminiProvider::new(
        &server.base_url,
        Some("AIzaSyConfiguredFallback1234".to_string()),
        DEFAULT_TIMEOUT_SECS,
    )
    .expect("construct provider with valid configured key")
    .with_env_fallback(false);

    for bad in unicode_ws_keys {
        // Source 1: GeminiProvider::new configured api_key
        let err = GeminiProvider::new(
            &server.base_url,
            Some(bad.to_string()),
            DEFAULT_TIMEOUT_SECS,
        )
        .expect_err(&format!("GeminiProvider::new must reject {bad:?}"));
        assert_sanitized_invalid_config(err, "GeminiProvider::new", bad);

        // Source 2: ForwardRequest.api_key_override with a valid configured key present
        // (must fail closed with InvalidConfig instead of silently falling back!)
        let err = provider_with_configured_key
            .forward_gemini(ForwardRequest {
                path_and_query: "/v1beta/models/gemini-4-hydrogen:generateContent".to_string(),
                body: Bytes::from_static(b"{}"),
                content_type: None,
                api_key_override: Some(bad.to_string()),
            })
            .await
            .expect_err(&format!("api_key_override must reject {bad:?}"));
        assert_sanitized_invalid_config(err, "api_key_override", bad);

        // Source 3: with_env_api_key env fallback (forward_gemini + health_check)
        let cfg_no_key = config_for(&server.base_url, DEFAULT_TIMEOUT_SECS);
        let env_provider = GeminiProvider::with_env_api_key(&cfg_no_key, None, Some(bad))
            .expect("construct provider with deferred env key");

        let err = env_provider
            .forward_gemini(ForwardRequest {
                path_and_query: "/v1beta/models/gemini-4-hydrogen:generateContent".to_string(),
                body: Bytes::from_static(b"{}"),
                content_type: None,
                api_key_override: None,
            })
            .await
            .expect_err(&format!("env api_key forward_gemini must reject {bad:?}"));
        assert_sanitized_invalid_config(err, "env forward_gemini", bad);

        let err = env_provider
            .health_check()
            .await
            .expect_err(&format!("env api_key health_check must reject {bad:?}"));
        assert_sanitized_invalid_config(err, "env health_check", bad);
    }
    assert_eq!(
        hits.load(Ordering::SeqCst),
        0,
        "zero upstream hits allowed for invalid Unicode whitespace API keys"
    );

    // Positive ASCII space trimming checks across all 3 sources
    let p_trimmed_cfg = GeminiProvider::new(
        &server.base_url,
        Some("  AIzaSyValid1234  ".to_string()),
        DEFAULT_TIMEOUT_SECS,
    )
    .expect("ASCII-padded configured key should trim cleanly")
    .with_env_fallback(false);
    p_trimmed_cfg
        .forward_gemini(ForwardRequest {
            path_and_query: "/v1beta/models/gemini-4-hydrogen:generateContent".to_string(),
            body: Bytes::from_static(b"{}"),
            content_type: None,
            api_key_override: None,
        })
        .await
        .expect("trimmed configured key forward ok");
    assert_eq!(seen_key.lock().await.as_deref(), Some("AIzaSyValid1234"));

    // ASCII spaces-only configured key stays unconfigured (None) and falls back cleanly to env
    let p_blank_cfg = GeminiProvider::new(
        &server.base_url,
        Some("   ".to_string()),
        DEFAULT_TIMEOUT_SECS,
    )
    .expect("ASCII spaces-only configured key should remain unconfigured")
    .with_env_fallback(false);
    assert_eq!(p_blank_cfg.api_key_hint(), None);

    // ASCII spaces-only override falls back cleanly to configured key
    provider_with_configured_key
        .forward_gemini(ForwardRequest {
            path_and_query: "/v1beta/models/gemini-4-hydrogen:generateContent".to_string(),
            body: Bytes::from_static(b"{}"),
            content_type: None,
            api_key_override: Some("   ".to_string()),
        })
        .await
        .expect("ASCII spaces-only override should fall back cleanly");
    assert_eq!(
        seen_key.lock().await.as_deref(),
        Some("AIzaSyConfiguredFallback1234")
    );

    // ASCII-padded override trims cleanly
    provider_with_configured_key
        .forward_gemini(ForwardRequest {
            path_and_query: "/v1beta/models/gemini-4-hydrogen:generateContent".to_string(),
            body: Bytes::from_static(b"{}"),
            content_type: None,
            api_key_override: Some("  AIzaSyValid1234  ".to_string()),
        })
        .await
        .expect("ASCII-padded override should trim cleanly");
    assert_eq!(seen_key.lock().await.as_deref(), Some("AIzaSyValid1234"));

    // ASCII-padded env key trims cleanly
    let cfg_blank = config_for(&server.base_url, DEFAULT_TIMEOUT_SECS);
    let p_trimmed_env =
        GeminiProvider::with_env_api_key(&cfg_blank, None, Some("  AIzaSyValid1234  "))
            .expect("construct provider with ASCII-padded env key");
    p_trimmed_env
        .forward_gemini(ForwardRequest {
            path_and_query: "/v1beta/models/gemini-4-hydrogen:generateContent".to_string(),
            body: Bytes::from_static(b"{}"),
            content_type: None,
            api_key_override: None,
        })
        .await
        .expect("ASCII-padded env key should trim cleanly");
    assert_eq!(seen_key.lock().await.as_deref(), Some("AIzaSyValid1234"));
}

// 17. Regression: reject non-ASCII Unicode whitespace in Content-Type with 0 hits,
// while trimming ASCII spaces cleanly.
#[tokio::test]
async fn rejects_unicode_whitespace_in_content_type_and_trims_ascii_spaces() {
    let seen_ct = Arc::new(Mutex::new(None::<String>));
    let hits = Arc::new(AtomicUsize::new(0));
    let seen_clone = Arc::clone(&seen_ct);
    let hits_clone = Arc::clone(&hits);

    let router = Router::new().route(
        "/v1beta/models/{*rest}",
        post(move |headers: HeaderMap| {
            let seen = Arc::clone(&seen_clone);
            let hits = Arc::clone(&hits_clone);
            async move {
                hits.fetch_add(1, Ordering::SeqCst);
                *seen.lock().await = headers
                    .get(http::header::CONTENT_TYPE)
                    .and_then(|v| v.to_str().ok())
                    .map(str::to_owned);
                StatusCode::OK
            }
        }),
    );
    let server = spawn_mock_server(router).await;
    let provider = provider_for(&server.base_url, Some("AIzaSyCtUnicode12345"), None);

    let unicode_ws_cts = [
        "\u{00A0}application/json",
        "application/json\u{00A0}",
        "\u{2028}application/json",
        "application/json\u{2029}",
        "\u{00A0}",
        "\u{3000}",
    ];

    for bad in unicode_ws_cts {
        let err = provider
            .forward_gemini(ForwardRequest {
                path_and_query: "/v1beta/models/gemini-4-hydrogen:generateContent".to_string(),
                body: Bytes::from_static(b"{}"),
                content_type: Some(bad.to_string()),
                api_key_override: None,
            })
            .await
            .expect_err(&format!("Content-Type must reject {bad:?}"));
        assert!(
            matches!(err, ProviderError::InvalidConfig(_)),
            "expected InvalidConfig for Content-Type {bad:?}, got {err:?}"
        );
        let disp = err.to_string();
        let dbg = format!("{err:?}");
        for forbidden in [
            "application/json",
            "\u{00A0}",
            "\u{2028}",
            "\u{2029}",
            "\u{3000}",
        ] {
            assert!(
                !disp.contains(forbidden),
                "Display leaked {forbidden:?} for Content-Type {bad:?}: {disp}"
            );
            assert!(
                !dbg.contains(forbidden),
                "Debug leaked {forbidden:?} for Content-Type {bad:?}: {dbg}"
            );
        }
    }
    assert_eq!(
        hits.load(Ordering::SeqCst),
        0,
        "zero upstream hits allowed for invalid Unicode whitespace Content-Type"
    );

    // Positive ASCII space trimming check
    provider
        .forward_gemini(ForwardRequest {
            path_and_query: "/v1beta/models/gemini-4-hydrogen:generateContent".to_string(),
            body: Bytes::from_static(b"{}"),
            content_type: Some("  application/json  ".to_string()),
            api_key_override: None,
        })
        .await
        .expect("ASCII-padded Content-Type should trim cleanly");
    assert_eq!(hits.load(Ordering::SeqCst), 1);
    assert_eq!(seen_ct.lock().await.as_deref(), Some("application/json"));
}

#[tokio::test]
async fn list_models_filters_to_chat_gemini_3_and_4_and_preserves_metadata() {
    let seen_key = Arc::new(Mutex::new(None::<String>));
    let seen_key_handler = Arc::clone(&seen_key);
    let router = Router::new().route(
        "/v1beta/models",
        get(move |headers: HeaderMap| {
            let seen_key_handler = Arc::clone(&seen_key_handler);
            async move {
                *seen_key_handler.lock().await = headers
                    .get("x-goog-api-key")
                    .and_then(|v| v.to_str().ok())
                    .map(str::to_owned);
                let payload = serde_json::json!({
                    "models": [
                        {
                            "name": "models/gemini-2.5-pro",
                            "displayName": "Gemini 2.5 Pro",
                            "supportedGenerationMethods": ["generateContent"]
                        },
                        {
                            "name": "models/gemini-4-hydrogen",
                            "displayName": "Gemini 4 Hydrogen",
                            "inputTokenLimit": 1048576,
                            "outputTokenLimit": 65536,
                            "supportedGenerationMethods": ["generateContent"]
                        },
                        {
                            "name": "models/gemini-3.8-flash",
                            "displayName": "Gemini 3.8 Flash",
                            "inputTokenLimit": 1048576,
                            "outputTokenLimit": 65536,
                            "supportedGenerationMethods": ["generateContent"],
                            "thinking": true
                        },
                        {
                            "name": "models/gemini-3.8-flash-tts",
                            "displayName": "Gemini 3.8 Flash TTS",
                            "supportedGenerationMethods": ["generateContent"]
                        },
                        {
                            "name": "models/gemini-3-pro-image",
                            "displayName": "Nano Banana Pro",
                            "supportedGenerationMethods": ["generateContent"]
                        },
                        {
                            "name": "models/gemini-3.5-transcribe",
                            "displayName": "Gemini 3.5 Transcribe",
                            "supportedGenerationMethods": ["generateContent"]
                        },
                        {
                            "name": "models/gemini-4-embed",
                            "displayName": "Embed Only",
                            "supportedGenerationMethods": ["embedContent"]
                        }
                    ]
                });
                (
                    StatusCode::OK,
                    [(header::CONTENT_TYPE, "application/json")],
                    payload.to_string(),
                )
            }
        }),
    );
    let server = spawn_mock_server(router).await;
    let provider = provider_for(&server.base_url, Some("AIzaSyListModelsKey123"), None);

    let list = provider.list_models(None).await.expect("list_models ok");
    assert_eq!(
        seen_key.lock().await.as_deref(),
        Some("AIzaSyListModelsKey123")
    );
    let names: Vec<&str> = list.models.iter().map(|m| m.name.as_str()).collect();
    assert_eq!(
        names,
        vec!["models/gemini-4-hydrogen", "models/gemini-3.8-flash"]
    );
    assert_eq!(list.models[0].input_token_limit, Some(1_048_576));
    assert_eq!(list.models[0].output_token_limit, Some(65_536));
    assert_eq!(list.models[0].thinking, Some(true));
}

#[tokio::test]
async fn list_models_rejects_non_200_invalid_json_and_missing_key_without_leaking_secret() {
    let router = Router::new().route(
        "/v1beta/models",
        get(|| async {
            (
                StatusCode::UNAUTHORIZED,
                "bad key AIzaSyListModelsSecret999",
            )
        }),
    );
    let server = spawn_mock_server(router).await;
    let provider = provider_for(&server.base_url, Some("AIzaSyListModelsSecret999"), None);
    let err = provider.list_models(None).await.expect_err("must fail");
    let msg = err.to_string();
    assert!(msg.contains("401"), "unexpected error: {msg}");
    assert!(!msg.contains("AIzaSyListModelsSecret999"));

    let no_key = provider_for(&server.base_url, None, None);
    assert!(matches!(
        no_key.list_models(None).await,
        Err(ProviderError::MissingApiKey)
    ));
}

#[tokio::test]
async fn quarantines_placeholder_api_key_across_config_override_and_env() {
    let hits = Arc::new(AtomicUsize::new(0));
    let seen_key = Arc::new(Mutex::new(None::<String>));
    let hits_models = Arc::clone(&hits);
    let seen_models = Arc::clone(&seen_key);
    let hits_gen = Arc::clone(&hits);
    let seen_gen = Arc::clone(&seen_key);

    let router = Router::new()
        .route(
            "/v1beta/models",
            get(move |headers: HeaderMap| {
                let hits_models = Arc::clone(&hits_models);
                let seen_models = Arc::clone(&seen_models);
                async move {
                    hits_models.fetch_add(1, Ordering::SeqCst);
                    *seen_models.lock().await = headers
                        .get("x-goog-api-key")
                        .and_then(|v| v.to_str().ok())
                        .map(str::to_owned);
                    (
                        StatusCode::OK,
                        [(header::CONTENT_TYPE, "application/json")],
                        r#"{"models":[{"name":"models/gemini-4-hydrogen","supportedGenerationMethods":["generateContent"]}]}"#,
                    )
                }
            }),
        )
        .route(
            "/v1beta/models/{*rest}",
            post(move |headers: HeaderMap| {
                let hits_gen = Arc::clone(&hits_gen);
                let seen_gen = Arc::clone(&seen_gen);
                async move {
                    hits_gen.fetch_add(1, Ordering::SeqCst);
                    *seen_gen.lock().await = headers
                        .get("x-goog-api-key")
                        .and_then(|v| v.to_str().ok())
                        .map(str::to_owned);
                    (
                        StatusCode::OK,
                        [(header::CONTENT_TYPE, "application/json")],
                        r#"{"candidates":[]}"#,
                    )
                }
            }),
        );
    let server = spawn_mock_server(router).await;

    let make_req = |override_key: Option<&str>| ForwardRequest {
        path_and_query: "/v1beta/models/gemini-4-hydrogen:generateContent".to_string(),
        body: Bytes::from_static(b"{}"),
        content_type: Some("application/json".to_string()),
        api_key_override: override_key.map(str::to_owned),
    };

    // 1. Placeholder in config -> api_key_hint() == None, MissingApiKey across all 3 RPCs, 0 hits.
    for placeholder in [
        companion_core::PROXY_PLACEHOLDER_API_KEY,
        "  agy-companion-local  ",
    ] {
        let p = provider_for(&server.base_url, Some(placeholder), None);
        assert_eq!(p.api_key_hint(), None);
        assert!(matches!(
            p.forward_gemini(make_req(None)).await,
            Err(ProviderError::MissingApiKey)
        ));
        assert!(matches!(
            p.health_check().await,
            Err(ProviderError::MissingApiKey)
        ));
        assert!(matches!(
            p.list_models(None).await,
            Err(ProviderError::MissingApiKey)
        ));
    }

    // 2. Placeholder in env -> MissingApiKey across all 3 RPCs, 0 hits.
    for placeholder in [
        companion_core::PROXY_PLACEHOLDER_API_KEY,
        "  agy-companion-local  ",
    ] {
        let p = provider_for(&server.base_url, None, Some(placeholder));
        assert!(matches!(
            p.forward_gemini(make_req(None)).await,
            Err(ProviderError::MissingApiKey)
        ));
        assert!(matches!(
            p.health_check().await,
            Err(ProviderError::MissingApiKey)
        ));
        assert!(matches!(
            p.list_models(None).await,
            Err(ProviderError::MissingApiKey)
        ));
    }

    // 3. Placeholder in override -> MissingApiKey, 0 hits.
    for placeholder in [
        companion_core::PROXY_PLACEHOLDER_API_KEY,
        "  agy-companion-local  ",
    ] {
        let p = provider_for(&server.base_url, None, None);
        assert!(matches!(
            p.forward_gemini(make_req(Some(placeholder))).await,
            Err(ProviderError::MissingApiKey)
        ));
        assert!(matches!(
            p.list_models(Some(placeholder.to_string())).await,
            Err(ProviderError::MissingApiKey)
        ));
    }

    // 4. Control-char / non-ASCII padded placeholder -> fail-closed InvalidConfig, 0 hits.
    for bad_placeholder in ["\tagy-companion-local\n", "\u{00A0}agy-companion-local"] {
        assert!(matches!(
            GeminiProvider::build(
                server.base_url.clone(),
                Some(bad_placeholder.to_string()),
                Duration::from_secs(30),
                EnvKeySource::Disabled,
            ),
            Err(ProviderError::InvalidConfig(_))
        ));
        let p_override = provider_for(&server.base_url, None, None);
        assert!(matches!(
            p_override
                .forward_gemini(make_req(Some(bad_placeholder)))
                .await,
            Err(ProviderError::InvalidConfig(_))
        ));
        let p_env = provider_for(&server.base_url, None, Some(bad_placeholder));
        assert!(matches!(
            p_env.health_check().await,
            Err(ProviderError::InvalidConfig(_))
        ));
    }

    assert_eq!(
        hits.load(Ordering::SeqCst),
        0,
        "placeholder sentinel must never reach upstream"
    );

    // 5. Real configured key is used when override is the placeholder.
    let p_cfg = provider_for(&server.base_url, Some("AIzaSyRealCfg12345"), None);
    p_cfg
        .forward_gemini(make_req(Some("agy-companion-local")))
        .await
        .expect("falls back from placeholder override to real config key");
    assert_eq!(seen_key.lock().await.as_deref(), Some("AIzaSyRealCfg12345"));

    // 6. Real env key is used when config holds a legacy placeholder.
    let p_env = provider_for(
        &server.base_url,
        Some("agy-companion-local"),
        Some("AIzaSyRealEnv12345"),
    );
    p_env
        .health_check()
        .await
        .expect("falls back from placeholder config to real env key");
    assert_eq!(seen_key.lock().await.as_deref(), Some("AIzaSyRealEnv12345"));
    assert_eq!(hits.load(Ordering::SeqCst), 2);
}
