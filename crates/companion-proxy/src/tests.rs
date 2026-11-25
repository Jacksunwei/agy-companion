use std::fs;
use std::num::NonZeroU16;
use std::path::Path;
use std::sync::Mutex;

use axum::Router;
use axum::body::Body;
use axum::response::Response;
use axum::routing::post;
use companion_core::{
    ApiKey, LoopbackHost, MAX_CONFIG_FILE_BYTES, NoopUsageSink, SqliteUsageSink, USAGE_DB_FILENAME,
};
use http::{HeaderMap, StatusCode, header};
use tempfile::TempDir;
use tokio::sync::oneshot;

use super::*;

fn test_http_client() -> reqwest::Client {
    reqwest::Client::builder()
        .no_proxy()
        .build()
        .expect("build loopback client")
}

async fn spawn_mock_upstream(
    tag: &'static str,
) -> (
    SocketAddr,
    Arc<Mutex<Vec<Option<String>>>>,
    tokio::task::JoinHandle<()>,
) {
    let keys = Arc::new(Mutex::new(Vec::new()));
    let keys_clone = Arc::clone(&keys);

    let app = Router::new().route(
        "/v1beta/models/{*rest}",
        post(move |headers: HeaderMap| {
            let keys_clone = Arc::clone(&keys_clone);
            async move {
                let key = headers
                    .get("x-goog-api-key")
                    .and_then(|v| v.to_str().ok())
                    .map(str::to_string);
                keys_clone.lock().unwrap().push(key);
                let body = format!(
                    "data: {{\"candidates\":[{{\"text\":\"{tag}\"}}],\"usageMetadata\":{{\"promptTokenCount\":40,\"candidatesTokenCount\":10,\"cachedContentTokenCount\":20}}}}\n\n"
                );
                Response::builder()
                    .status(StatusCode::OK)
                    .header(header::CONTENT_TYPE, "text/event-stream")
                    .body(Body::from(body))
                    .unwrap()
            }
        }),
    );

    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let handle = tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    (addr, keys, handle)
}

#[tokio::test]
async fn rejects_non_loopback_hosts_ports_and_listeners() {
    let path = Path::new("config.json");
    assert!(matches!(
        Config::from_json_str(r#"{"schema_version":1,"proxy":{"host":"0.0.0.0"}}"#, path),
        Err(CoreError::InvalidListenHost { .. })
    ));
    assert!(matches!(
        Config::from_json_str(r#"{"schema_version":1,"proxy":{"host":"::"}}"#, path),
        Err(CoreError::InvalidListenHost { .. })
    ));
    assert!(matches!(
        Config::from_json_str(r#"{"schema_version":1,"proxy":{"port":0}}"#, path),
        Err(CoreError::InvalidListenPort)
    ));

    // "localhost" must still bind strictly to 127.0.0.1:<port>.
    let mut cfg_localhost = Config::default();
    cfg_localhost.proxy.host = LoopbackHost::Localhost;
    cfg_localhost.proxy.port = NonZeroU16::new(4141).unwrap();
    assert_eq!(
        loopback_socket_addr(&cfg_localhost).unwrap(),
        SocketAddr::from((Ipv4Addr::LOCALHOST, 4141))
    );

    // Defense-in-depth: serve_on_listener rejects a listener bound to 0.0.0.0.
    let non_loopback_listener = TcpListener::bind("0.0.0.0:0").await.unwrap();
    let state = ProxyState::from_config_with_options(
        Config::default(),
        Arc::new(NoopUsageSink),
        ProxyStateOptions {
            config_path: None,
            env_fallback: false,
        },
    )
    .unwrap();
    let err = serve_on_listener(non_loopback_listener, state, async {})
        .await
        .unwrap_err();
    assert!(matches!(
        err,
        ProxyError::Core(CoreError::InvalidListenHost { .. })
    ));
}

#[tokio::test]
async fn serve_on_listener_end_to_end_over_real_tcp_with_graceful_shutdown() {
    let (upstream_addr, upstream_keys, upstream_handle) = spawn_mock_upstream("live-e2e").await;

    let tmp = TempDir::new().unwrap();
    let usage_dir = tmp.path().join("usage");
    let sink = Arc::new(SqliteUsageSink::new(&usage_dir).unwrap());
    let sink_reader = Arc::clone(&sink);

    let mut cfg = Config::default();
    cfg.providers.gemini.base_url = format!("http://{upstream_addr}");
    cfg.providers.gemini.api_key = Some(ApiKey::parse_strict("AIzaLiveLoopbackKey12345").unwrap());

    let state = ProxyState::from_config_with_options(
        cfg,
        sink,
        ProxyStateOptions {
            config_path: None,
            env_fallback: false,
        },
    )
    .unwrap();
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let proxy_addr = listener.local_addr().unwrap();

    let (tx_shutdown, rx_shutdown) = oneshot::channel::<()>();
    let server = tokio::spawn(async move {
        serve_on_listener(listener, state, async move {
            let _ = rx_shutdown.await;
        })
        .await
        .unwrap();
    });

    let client = test_http_client();

    // 1. GET /health
    let health_resp = client
        .get(format!("http://{proxy_addr}/health"))
        .send()
        .await
        .unwrap();
    assert_eq!(health_resp.status(), StatusCode::OK);
    assert_eq!(
        health_resp
            .headers()
            .get("x-content-type-options")
            .and_then(|v| v.to_str().ok()),
        Some("nosniff")
    );
    assert_eq!(
        health_resp
            .headers()
            .get("cache-control")
            .and_then(|v| v.to_str().ok()),
        Some("no-store")
    );
    let health: HealthResponse = health_resp.json().await.unwrap();
    assert_eq!(health.status, "ok");
    assert_eq!(health.protocol_version, PROTOCOL_VERSION);

    // 2. POST SSE pass-through
    let proxy_resp = client
        .post(format!(
            "http://{proxy_addr}/v1beta/models/gemini-4-hydrogen:streamGenerateContent?alt=sse"
        ))
        .header("content-type", "application/json")
        .body(r#"{"contents":[]}"#)
        .send()
        .await
        .unwrap();
    assert_eq!(proxy_resp.status(), StatusCode::OK);
    let sse_text = proxy_resp.text().await.unwrap();
    assert!(sse_text.contains("live-e2e"));
    assert_eq!(
        upstream_keys.lock().unwrap().as_slice(),
        &[Some("AIzaLiveLoopbackKey12345".to_string())]
    );

    // 3. GET /api/stats reflects recorded usage immediately.
    let snap: StatsSnapshot = client
        .get(format!("http://{proxy_addr}/api/stats"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(snap.usage.requests, 1);
    assert_eq!(snap.usage.prompt_tokens, 40);
    assert_eq!(snap.usage.cached_tokens, 20);
    assert_eq!(snap.usage.output_tokens, 10);
    assert!((snap.usage.cache_hit_ratio - 0.5).abs() < 1e-12);

    // 4. SqliteUsageSink persisted 1 event with zero secrets and 0600 permissions.
    let db_path = usage_dir.join(USAGE_DB_FILENAME);
    assert!(
        db_path.is_file(),
        "expected usage.db at {}",
        db_path.display()
    );

    let mut raw_bytes = Vec::new();
    for name in [
        "usage.db",
        "usage.db-wal",
        "usage.db-shm",
        "usage.db-journal",
    ] {
        let path = usage_dir.join(name);
        if path.is_file() {
            raw_bytes.extend_from_slice(&fs::read(&path).unwrap());
        }
    }
    let secret = b"AIzaLiveLoopbackKey12345";
    assert!(
        !raw_bytes.windows(secret.len()).any(|w| w == secret),
        "API key must never appear in SQLite storage bytes"
    );

    let events = sink_reader.read_all_events().unwrap();
    assert_eq!(events.len(), 1);
    let event = &events[0];
    assert_eq!(event.model, "gemini-4-hydrogen");
    assert_eq!(event.input_tokens, 40);
    assert_eq!(event.cached_tokens, 20);

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        for name in ["usage.db", "usage.db-wal", "usage.db-shm"] {
            let path = usage_dir.join(name);
            if path.exists() {
                let mode = fs::metadata(&path).unwrap().permissions().mode() & 0o777;
                assert_eq!(mode, 0o600, "expected 0600 on {name}, got {mode:o}");
            }
        }
    }

    let _ = tx_shutdown.send(());
    server.await.unwrap();
    upstream_handle.abort();
}

#[tokio::test]
async fn hot_reloads_key_rotation_revocation_base_url_and_file_deletion() {
    let (upstream1_addr, upstream1_keys, upstream1_handle) =
        spawn_mock_upstream("upstream-1").await;
    let (upstream2_addr, upstream2_keys, upstream2_handle) =
        spawn_mock_upstream("upstream-2").await;

    let tmp = TempDir::new().unwrap();
    let config_path = tmp.path().join("config.json");

    let mut cfg = Config::default();
    cfg.providers.gemini.base_url = format!("http://{upstream1_addr}");
    cfg.providers.gemini.api_key = Some(ApiKey::parse_strict("AIzaInitialKey11111111").unwrap());
    cfg.save_secure(&config_path).unwrap();

    // env_fallback = false keeps revocation deterministic even when host exports GEMINI_API_KEY.
    let state = ProxyState::from_config_with_options(
        cfg.clone(),
        Arc::new(NoopUsageSink),
        ProxyStateOptions {
            config_path: Some(config_path.clone()),
            env_fallback: false,
        },
    )
    .unwrap();

    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let proxy_addr = listener.local_addr().unwrap();
    let (tx_shutdown, rx_shutdown) = oneshot::channel::<()>();
    let server = tokio::spawn(async move {
        serve_on_listener(listener, state, async move {
            let _ = rx_shutdown.await;
        })
        .await
        .unwrap();
    });

    let client = test_http_client();
    let url = format!(
        "http://{proxy_addr}/v1beta/models/gemini-4-hydrogen:streamGenerateContent?alt=sse"
    );

    // 1. Initial key hits upstream 1.
    let r1 = client.post(&url).body("{}").send().await.unwrap();
    assert_eq!(r1.status(), StatusCode::OK);
    assert!(r1.text().await.unwrap().contains("upstream-1"));
    assert_eq!(
        upstream1_keys.lock().unwrap().last().cloned().flatten(),
        Some("AIzaInitialKey11111111".to_string())
    );

    // 2. Immediate same-millisecond key rotation via Config::save_secure.
    cfg.providers.gemini.api_key = Some(ApiKey::parse_strict("AIzaRotatedKey22222222").unwrap());
    cfg.save_secure(&config_path).unwrap();
    let r2 = client.post(&url).body("{}").send().await.unwrap();
    assert_eq!(r2.status(), StatusCode::OK);
    let _ = r2.text().await.unwrap();
    assert_eq!(
        upstream1_keys.lock().unwrap().last().cloned().flatten(),
        Some("AIzaRotatedKey22222222".to_string())
    );

    // 3. Base URL switch to upstream 2.
    cfg.providers.gemini.base_url = format!("http://{upstream2_addr}");
    cfg.save_secure(&config_path).unwrap();
    let r3 = client.post(&url).body("{}").send().await.unwrap();
    assert_eq!(r3.status(), StatusCode::OK);
    assert!(r3.text().await.unwrap().contains("upstream-2"));
    assert_eq!(
        upstream2_keys.lock().unwrap().last().cloned().flatten(),
        Some("AIzaRotatedKey22222222".to_string())
    );

    // 4. Revoke API key (clear_api_key -> None) -> 401 Unauthorized immediately.
    cfg.providers.gemini.api_key = None;
    cfg.save_secure(&config_path).unwrap();
    let r4 = client.post(&url).body("{}").send().await.unwrap();
    assert_eq!(r4.status(), StatusCode::UNAUTHORIZED);

    // 5. Re-add key, confirm 200, then delete config file -> immediately revokes key -> 401.
    cfg.providers.gemini.api_key = Some(ApiKey::parse_strict("AIzaReaddedKey33333333").unwrap());
    cfg.save_secure(&config_path).unwrap();
    let r5 = client.post(&url).body("{}").send().await.unwrap();
    assert_eq!(r5.status(), StatusCode::OK);
    let _ = r5.text().await.unwrap();

    fs::remove_file(&config_path).unwrap();
    let r6 = client.post(&url).body("{}").send().await.unwrap();
    assert_eq!(r6.status(), StatusCode::UNAUTHORIZED);

    let _ = tx_shutdown.send(());
    server.await.unwrap();
    upstream1_handle.abort();
    upstream2_handle.abort();
}

#[tokio::test]
async fn hot_reload_survives_corrupt_and_invalid_config_and_recovers_without_leaking_secrets() {
    let (upstream_addr, upstream_keys, upstream_handle) = spawn_mock_upstream("resilient").await;

    let tmp = TempDir::new().unwrap();
    let config_path = tmp.path().join("config.json");

    let mut cfg = Config::default();
    cfg.providers.gemini.base_url = format!("http://{upstream_addr}");
    cfg.providers.gemini.api_key = Some(ApiKey::parse_strict("AIzaValidBeforeCorrupt1").unwrap());
    cfg.save_secure(&config_path).unwrap();

    let state = ProxyState::from_config_with_options(
        cfg.clone(),
        Arc::new(NoopUsageSink),
        ProxyStateOptions {
            config_path: Some(config_path.clone()),
            env_fallback: false,
        },
    )
    .unwrap();

    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let proxy_addr = listener.local_addr().unwrap();
    let (tx_shutdown, rx_shutdown) = oneshot::channel::<()>();
    let server = tokio::spawn(async move {
        serve_on_listener(listener, state, async move {
            let _ = rx_shutdown.await;
        })
        .await
        .unwrap();
    });

    let client = test_http_client();
    let url = format!(
        "http://{proxy_addr}/v1beta/models/gemini-4-hydrogen:streamGenerateContent?alt=sse"
    );

    // 1. Write corrupt JSON containing a secret -> proxy keeps last valid provider cleanly.
    fs::write(&config_path, "{ \"api_key\": \"AIzaCORRUPTSECRET\", broken").unwrap();
    let r1 = client.post(&url).body("{}").send().await.unwrap();
    assert_eq!(r1.status(), StatusCode::OK);
    let body1 = r1.text().await.unwrap();
    assert!(body1.contains("resilient"));
    assert!(!body1.contains("AIzaCORRUPTSECRET"));
    assert_eq!(
        upstream_keys.lock().unwrap().last().cloned().flatten(),
        Some("AIzaValidBeforeCorrupt1".to_string())
    );

    // 2. Write schema-invalid config (0.0.0.0) -> proxy still retains last valid provider.
    let bad_json = serde_json::json!({
        "schema_version": 1,
        "proxy": {
            "host": "0.0.0.0",
            "port": 4141
        },
        "providers": {
            "gemini": {
                "base_url": format!("http://{upstream_addr}"),
                "api_key": "AIzaINVALIDHOSTSECRET",
                "default_model": "gemini-4-hydrogen",
                "timeout_secs": 900
            }
        }
    });
    fs::write(&config_path, bad_json.to_string()).unwrap();
    let r2 = client.post(&url).body("{}").send().await.unwrap();
    assert_eq!(r2.status(), StatusCode::OK);
    let _ = r2.text().await.unwrap();
    assert_eq!(
        upstream_keys.lock().unwrap().last().cloned().flatten(),
        Some("AIzaValidBeforeCorrupt1".to_string())
    );

    // 3. Write valid updated config -> proxy immediately recovers and adopts new key.
    cfg.providers.gemini.api_key = Some(ApiKey::parse_strict("AIzaRecoveredKey9999999").unwrap());
    cfg.save_secure(&config_path).unwrap();
    let r3 = client.post(&url).body("{}").send().await.unwrap();
    assert_eq!(r3.status(), StatusCode::OK);
    let _ = r3.text().await.unwrap();
    assert_eq!(
        upstream_keys.lock().unwrap().last().cloned().flatten(),
        Some("AIzaRecoveredKey9999999".to_string())
    );

    let _ = tx_shutdown.send(());
    server.await.unwrap();
    upstream_handle.abort();
}

#[cfg(unix)]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn hot_reload_rejects_post_startup_symlink_dangling_symlink_and_fifo_without_blocking() {
    use std::os::unix::fs::symlink;

    let (upstream1_addr, upstream1_keys, upstream1_handle) =
        spawn_mock_upstream("good-upstream").await;
    let (upstream2_addr, upstream2_keys, upstream2_handle) =
        spawn_mock_upstream("evil-upstream").await;

    let tmp = TempDir::new().unwrap();
    let config_path = tmp.path().join("config.json");
    let evil_path = tmp.path().join("evil.json");

    let mut good_cfg = Config::default();
    good_cfg.providers.gemini.base_url = format!("http://{upstream1_addr}");
    good_cfg.providers.gemini.api_key =
        Some(ApiKey::parse_strict("AIzaGoodOriginalKey1111").unwrap());
    good_cfg.save_secure(&config_path).unwrap();

    let mut evil_cfg = Config::default();
    evil_cfg.providers.gemini.base_url = format!("http://{upstream2_addr}");
    evil_cfg.providers.gemini.api_key =
        Some(ApiKey::parse_strict("AIzaEvilHijackKey22222").unwrap());
    evil_cfg.save_secure(&evil_path).unwrap();

    // Startup must also reject an initial symlink or FIFO config_path cleanly.
    let initial_sym = tmp.path().join("initial_sym.json");
    symlink(&evil_path, &initial_sym).unwrap();
    assert!(matches!(
        ProxyState::from_config_with_options(
            good_cfg.clone(),
            Arc::new(NoopUsageSink),
            ProxyStateOptions {
                config_path: Some(initial_sym),
                env_fallback: false,
            },
        ),
        Err(ProxyError::Core(CoreError::Fs(
            FsError::SymlinkRejected { .. }
        )))
    ));

    let state = ProxyState::from_config_with_options(
        good_cfg.clone(),
        Arc::new(NoopUsageSink),
        ProxyStateOptions {
            config_path: Some(config_path.clone()),
            env_fallback: false,
        },
    )
    .unwrap();

    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let proxy_addr = listener.local_addr().unwrap();
    let (tx_shutdown, rx_shutdown) = oneshot::channel::<()>();
    let server = tokio::spawn(async move {
        serve_on_listener(listener, state, async move {
            let _ = rx_shutdown.await;
        })
        .await
        .unwrap();
    });

    let client = reqwest::Client::builder()
        .no_proxy()
        .timeout(Duration::from_secs(2))
        .build()
        .expect("build loopback client");
    let url = format!(
        "http://{proxy_addr}/v1beta/models/gemini-4-hydrogen:streamGenerateContent?alt=sse"
    );

    // Baseline: hits upstream 1 with original key.
    let r0 = client.post(&url).body("{}").send().await.unwrap();
    assert_eq!(r0.status(), StatusCode::OK);
    assert!(r0.text().await.unwrap().contains("good-upstream"));

    // (a) Post-startup swap to symlink pointing to evil.json -> must retain original upstream & key.
    fs::remove_file(&config_path).unwrap();
    symlink(&evil_path, &config_path).unwrap();
    let r1 = client.post(&url).body("{}").send().await.unwrap();
    assert_eq!(r1.status(), StatusCode::OK);
    let body1 = r1.text().await.unwrap();
    assert!(body1.contains("good-upstream"), "body1={body1}");
    assert!(!body1.contains("evil-upstream"));
    assert!(
        upstream2_keys.lock().unwrap().is_empty(),
        "evil upstream must receive 0 hits"
    );
    assert_eq!(
        upstream1_keys.lock().unwrap().last().cloned().flatten(),
        Some("AIzaGoodOriginalKey1111".to_string())
    );

    // (b) Post-startup swap to dangling symlink -> must retain original config (NOT 401 wipe!).
    fs::remove_file(&config_path).unwrap();
    symlink(tmp.path().join("does-not-exist.json"), &config_path).unwrap();
    let r2 = client.post(&url).body("{}").send().await.unwrap();
    assert_eq!(
        r2.status(),
        StatusCode::OK,
        "dangling symlink must retain active config instead of wiping to 401"
    );
    assert!(r2.text().await.unwrap().contains("good-upstream"));

    // (c) Post-startup swap to FIFO -> must NOT hang and must retain active config.
    fs::remove_file(&config_path).unwrap();
    let mkfifo_status = std::process::Command::new("mkfifo")
        .arg(&config_path)
        .status()
        .expect("run mkfifo");
    assert!(mkfifo_status.success(), "mkfifo failed");
    let r3 = tokio::time::timeout(Duration::from_secs(1), client.post(&url).body("{}").send())
        .await
        .expect("proxy deadlocked on FIFO config swap")
        .expect("request succeeded");
    assert_eq!(r3.status(), StatusCode::OK);
    assert!(r3.text().await.unwrap().contains("good-upstream"));

    // (d) Clean recovery when replaced back with regular file via save_secure.
    fs::remove_file(&config_path).unwrap();
    good_cfg.providers.gemini.api_key =
        Some(ApiKey::parse_strict("AIzaRecoveredAfterSwap33").unwrap());
    good_cfg.save_secure(&config_path).unwrap();
    let r4 = client.post(&url).body("{}").send().await.unwrap();
    assert_eq!(r4.status(), StatusCode::OK);
    let _ = r4.text().await.unwrap();
    assert_eq!(
        upstream1_keys.lock().unwrap().last().cloned().flatten(),
        Some("AIzaRecoveredAfterSwap33".to_string())
    );

    let _ = tx_shutdown.send(());
    server.await.unwrap();
    upstream1_handle.abort();
    upstream2_handle.abort();
}

#[tokio::test]
async fn hot_reload_preserves_cloudcode_routing_cache_when_upstream_and_timeout_unchanged_and_rebuilds_on_change()
 {
    let tmp = TempDir::new().unwrap();
    let config_path = tmp.path().join("config.json");

    let mut cfg = Config::default();
    cfg.cloudcode.upstream = CloudCodeUpstream::Auto;
    cfg.providers.gemini.timeout_secs = 900;
    cfg.save_secure(&config_path).unwrap();

    let state = ProxyState::from_config_with_options(
        cfg.clone(),
        Arc::new(NoopUsageSink),
        ProxyStateOptions {
            config_path: Some(config_path.clone()),
            env_fallback: false,
        },
    )
    .unwrap();

    let initial_cloudcode = Arc::clone(&state.active_provider().await.cloudcode);
    assert_eq!(initial_cloudcode.mode_label(), "auto");

    // Saving a new API key without changing cloudcode.upstream or timeout_secs
    // must preserve the exact same Arc<CloudCodeClient> (and its routing LRU cache).
    cfg.providers.gemini.api_key = Some(ApiKey::parse_strict("AIzaSyReloadPreservesLru1").unwrap());
    cfg.save_secure(&config_path).unwrap();
    let after_key_save = state.active_provider().await;
    assert!(after_key_save.has_configured_api_key);
    assert!(
        Arc::ptr_eq(&initial_cloudcode, &after_key_save.cloudcode),
        "CloudCodeClient Arc must be preserved across API key hot-reload"
    );

    // Changing timeout_secs must rebuild CloudCodeClient.
    cfg.providers.gemini.timeout_secs = 600;
    cfg.save_secure(&config_path).unwrap();
    let after_timeout_change = state.active_provider().await;
    assert!(
        !Arc::ptr_eq(&initial_cloudcode, &after_timeout_change.cloudcode),
        "CloudCodeClient Arc must rebuild when timeout_secs changes"
    );

    // Changing cloudcode.upstream must also rebuild CloudCodeClient.
    let prev = Arc::clone(&after_timeout_change.cloudcode);
    cfg.cloudcode.upstream =
        CloudCodeUpstream::Fixed("https://cloudcode-pa.googleapis.com".to_string());
    cfg.save_secure(&config_path).unwrap();
    let after_url_change = state.active_provider().await;
    assert!(
        !Arc::ptr_eq(&prev, &after_url_change.cloudcode),
        "CloudCodeClient Arc must rebuild when cloudcode_upstream_url changes"
    );
    assert_eq!(after_url_change.cloudcode.mode_label(), "fixed");
}

#[tokio::test]
async fn activity_tracker_keeps_in_flight_active_across_long_sse_stream_and_resets_idle_after_eof_and_disconnect()
 {
    use axum::body::Body;
    use axum::http::Request;
    use bytes::Bytes;
    use http_body_util::BodyExt;
    use tokio::sync::{Notify, mpsc};
    use tower::ServiceExt;

    let chunk1_sent = Arc::new(Notify::new());
    let release_chunk2 = Arc::new(Notify::new());
    let c1 = Arc::clone(&chunk1_sent);
    let r2 = Arc::clone(&release_chunk2);

    let stream_handler = move || {
        let c1 = Arc::clone(&c1);
        let r2 = Arc::clone(&r2);
        async move {
            let (tx, rx) = mpsc::channel::<Result<Bytes, std::io::Error>>(4);
            tokio::spawn(async move {
                let _ = tx
                    .send(Ok(Bytes::from_static(b"data: {\"chunk\":1}\n\n")))
                    .await;
                c1.notify_one();
                r2.notified().await;
                let _ = tx
                    .send(Ok(Bytes::from_static(b"data: {\"chunk\":2}\n\n")))
                    .await;
            });
            let body_stream = futures_util::stream::unfold(rx, |mut rx| async move {
                rx.recv().await.map(|item| (item, rx))
            });
            (
                StatusCode::OK,
                [(header::CONTENT_TYPE, "text/event-stream")],
                Body::from_stream(body_stream),
            )
        }
    };

    let upstream = Router::new()
        .route(
            "/v1beta/models/gemini-4-hydrogen:streamGenerateContent",
            post(stream_handler.clone()),
        )
        .route("/v1internal:streamGenerateContent", post(stream_handler));

    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let upstream_addr = listener.local_addr().unwrap();
    let upstream_srv = tokio::spawn(async move {
        let _ = axum::serve(listener, upstream).await;
    });

    let mut cfg = Config::default();
    cfg.providers.gemini.base_url = format!("http://{upstream_addr}");
    cfg.providers.gemini.api_key = Some(ApiKey::parse_strict("AIzaSyActivityTestKey123").unwrap());
    cfg.cloudcode.upstream = CloudCodeUpstream::Fixed(format!("http://{upstream_addr}"));

    let state = ProxyState::from_config_with_options(
        cfg,
        Arc::new(NoopUsageSink),
        ProxyStateOptions {
            config_path: None,
            env_fallback: false,
        },
    )
    .unwrap();
    assert_eq!(state.in_flight_requests(), 0);
    let app = build_router(state.clone());

    // 1. /v1beta/models/* SSE stream: stays in_flight == 1 while paused mid-stream, returns to 0 after EOF.
    let resp = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/v1beta/models/gemini-4-hydrogen:streamGenerateContent?alt=sse")
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from("{}"))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let mut body = resp.into_body();
    let first_frame = body.frame().await.expect("chunk 1").unwrap();
    assert!(!first_frame.into_data().unwrap().is_empty());
    chunk1_sent.notified().await;

    assert_eq!(state.in_flight_requests(), 1);
    tokio::time::sleep(Duration::from_millis(30)).await;
    assert_eq!(
        state.idle_duration(),
        Duration::ZERO,
        "idle_duration must stay ZERO while SSE stream is active"
    );

    release_chunk2.notify_one();
    while body.frame().await.is_some() {}
    drop(body);
    assert_eq!(state.in_flight_requests(), 0);
    tokio::time::sleep(Duration::from_millis(25)).await;
    assert!(
        state.idle_duration() >= Duration::from_millis(15),
        "idle_duration must advance once stream completes"
    );

    // 2. /v1internal:streamGenerateContent early client disconnect releases in_flight back to 0.
    let cc_resp = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/v1internal:streamGenerateContent?alt=sse")
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from("{}"))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(cc_resp.status(), StatusCode::OK);
    let mut cc_body = cc_resp.into_body();
    let _ = cc_body.frame().await.expect("cc chunk 1").unwrap();
    chunk1_sent.notified().await;
    assert_eq!(state.in_flight_requests(), 1);
    assert_eq!(state.idle_duration(), Duration::ZERO);

    // Drop response body early without waiting for chunk 2!
    drop(cc_body);
    assert_eq!(
        state.in_flight_requests(),
        0,
        "early client disconnect must decrement in_flight back to 0"
    );
    release_chunk2.notify_one();

    upstream_srv.abort();
}

#[tokio::test]
async fn passive_health_and_stats_probes_do_not_reset_idle_duration() {
    use axum::http::Request;
    use tower::ServiceExt;

    let state = ProxyState::from_config_with_options(
        Config::default(),
        Arc::new(NoopUsageSink),
        ProxyStateOptions {
            config_path: None,
            env_fallback: false,
        },
    )
    .unwrap();
    let app = build_router(state.clone());

    tokio::time::sleep(Duration::from_millis(60)).await;
    let idle_before = state.idle_duration();
    assert!(
        idle_before >= Duration::from_millis(45),
        "expected idle_before >= 45ms, got {idle_before:?}"
    );

    let health_resp = app
        .clone()
        .oneshot(
            Request::builder()
                .method("GET")
                .uri("/health")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(health_resp.status(), StatusCode::OK);

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

    assert_eq!(state.in_flight_requests(), 0);
    let idle_after = state.idle_duration();
    assert!(
        idle_after >= idle_before,
        "passive /health and /api/stats probes must not reset idle_duration (before={idle_before:?}, after={idle_after:?})"
    );
}

#[tokio::test]
async fn proxy_state_constructors_support_result_new_with_cloudcode_di_and_options_defaults() {
    let opts = ProxyStateOptions::default();
    assert_eq!(opts.config_path, None);
    assert!(opts.env_fallback);

    let provider: Arc<dyn ProviderAdapter> = Arc::new(
        GeminiProvider::new("https://generativelanguage.googleapis.com", None, 30)
            .unwrap()
            .with_env_fallback(false),
    );
    let state_new = ProxyState::new(Arc::clone(&provider), Arc::new(NoopUsageSink), false)
        .expect("ProxyState::new must return Ok for default CloudCode client");
    let active_new = state_new.active_provider().await;
    assert_eq!(active_new.cloudcode.mode_label(), "auto");
    assert!(!active_new.has_configured_api_key);

    let custom_cc = Arc::new(
        CloudCodeClient::new("https://cloudcode-pa.googleapis.com", 120)
            .expect("build custom CloudCodeClient"),
    );
    let state_di = ProxyState::with_cloudcode(
        Arc::clone(&provider),
        Arc::clone(&custom_cc),
        Arc::new(NoopUsageSink),
        true,
    );
    let active_di = state_di.active_provider().await;
    assert!(
        Arc::ptr_eq(&custom_cc, &active_di.cloudcode),
        "ProxyState::with_cloudcode must inject the exact CloudCodeClient Arc"
    );
    assert!(active_di.has_configured_api_key);

    let state_cfg = ProxyState::from_config(Config::default(), Arc::new(NoopUsageSink))
        .expect("ProxyState::from_config must delegate cleanly");
    assert_eq!(state_cfg.in_flight_requests(), 0);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn hot_reload_concurrent_readers_see_immediate_rotation_with_zero_sleep_and_deduplicate_rebuilds()
 {
    use tokio::sync::Barrier;

    let tmp = TempDir::new().unwrap();
    let config_path = tmp.path().join("config.json");

    let mut cfg = Config::default();
    cfg.providers.gemini.api_key = Some(ApiKey::parse_strict("AIzaConcurrentInit11111").unwrap());
    cfg.save_secure(&config_path).unwrap();

    let state = ProxyState::from_config_with_options(
        cfg.clone(),
        Arc::new(NoopUsageSink),
        ProxyStateOptions {
            config_path: Some(config_path.clone()),
            env_fallback: false,
        },
    )
    .unwrap();

    let initial = state.active_provider().await;
    let initial_cloudcode = Arc::clone(&initial.cloudcode);
    let initial_provider_ptr = Arc::as_ptr(&initial.provider).cast::<()>();

    // Rotate key immediately with zero sleep and fire 16 concurrent readers behind a barrier.
    cfg.providers.gemini.api_key = Some(ApiKey::parse_strict("AIzaConcurrentNext22222").unwrap());
    cfg.save_secure(&config_path).unwrap();

    let tasks = 16usize;
    let barrier = Arc::new(Barrier::new(tasks));
    let mut handles = Vec::with_capacity(tasks);
    for _ in 0..tasks {
        let state = state.clone();
        let barrier = Arc::clone(&barrier);
        handles.push(tokio::spawn(async move {
            barrier.wait().await;
            state.active_provider().await
        }));
    }

    let mut results = Vec::with_capacity(tasks);
    for handle in handles {
        results.push(handle.await.unwrap());
    }

    let expected_ptr = Arc::as_ptr(&results[0].provider).cast::<()>();
    assert_ne!(
        expected_ptr, initial_provider_ptr,
        "rotated config must produce a new provider immediately with zero sleep"
    );
    for active in &results {
        assert!(active.has_configured_api_key);
        assert!(
            Arc::ptr_eq(&initial_cloudcode, &active.cloudcode),
            "CloudCodeClient Arc must stay shared when upstream/timeout are unchanged"
        );
        assert_eq!(
            Arc::as_ptr(&active.provider).cast::<()>(),
            expected_ptr,
            "concurrent reloaders must deduplicate rebuilds to a single provider instance"
        );
    }
}

#[tokio::test]
async fn hot_reload_detects_same_second_in_place_edit_with_identical_length() {
    let (upstream_addr, upstream_keys, upstream_handle) = spawn_mock_upstream("in-place").await;

    let tmp = TempDir::new().unwrap();
    let config_path = tmp.path().join("config.json");

    let make_json = |key: &str| {
        serde_json::json!({
            "schema_version": 1,
            "proxy": { "host": "127.0.0.1", "port": 24141 },
            "cloudcode": { "upstream": "auto" },
            "providers": {
                "gemini": {
                    "base_url": format!("http://{upstream_addr}"),
                    "api_key": key,
                    "default_model": "gemini-4-hydrogen",
                    "timeout_secs": 900
                }
            }
        })
        .to_string()
    };

    let json_v1 = make_json("AIzaSameLenKeyAAAAA1111");
    let json_v2 = make_json("AIzaSameLenKeyBBBBB2222");
    assert_eq!(json_v1.len(), json_v2.len());
    fs::write(&config_path, &json_v1).unwrap();

    let cfg_v1 = Config::from_json_str(&json_v1, &config_path).unwrap();
    let state = ProxyState::from_config_with_options(
        cfg_v1,
        Arc::new(NoopUsageSink),
        ProxyStateOptions {
            config_path: Some(config_path.clone()),
            env_fallback: false,
        },
    )
    .unwrap();

    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let proxy_addr = listener.local_addr().unwrap();
    let (tx_shutdown, rx_shutdown) = oneshot::channel::<()>();
    let server = tokio::spawn(async move {
        serve_on_listener(listener, state, async move {
            let _ = rx_shutdown.await;
        })
        .await
        .unwrap();
    });

    let client = test_http_client();
    let url = format!(
        "http://{proxy_addr}/v1beta/models/gemini-4-hydrogen:streamGenerateContent?alt=sse"
    );

    let r1 = client.post(&url).body("{}").send().await.unwrap();
    assert_eq!(r1.status(), StatusCode::OK);
    let _ = r1.text().await.unwrap();
    assert_eq!(
        upstream_keys.lock().unwrap().last().cloned().flatten(),
        Some("AIzaSameLenKeyAAAAA1111".to_string())
    );

    // Same-second in-place overwrite with identical byte length, zero sleep.
    fs::write(&config_path, &json_v2).unwrap();
    let r2 = client.post(&url).body("{}").send().await.unwrap();
    assert_eq!(r2.status(), StatusCode::OK);
    let _ = r2.text().await.unwrap();
    assert_eq!(
        upstream_keys.lock().unwrap().last().cloned().flatten(),
        Some("AIzaSameLenKeyBBBBB2222".to_string())
    );

    let _ = tx_shutdown.send(());
    server.await.unwrap();
    upstream_handle.abort();
}

#[tokio::test]
async fn hot_reload_retains_active_provider_on_oversized_config_and_recovers() {
    let tmp = TempDir::new().unwrap();
    let config_path = tmp.path().join("config.json");

    let mut cfg = Config::default();
    cfg.providers.gemini.api_key = Some(ApiKey::parse_strict("AIzaBeforeOversize11111").unwrap());
    cfg.save_secure(&config_path).unwrap();

    let state = ProxyState::from_config_with_options(
        cfg.clone(),
        Arc::new(NoopUsageSink),
        ProxyStateOptions {
            config_path: Some(config_path.clone()),
            env_fallback: false,
        },
    )
    .unwrap();

    let initial = state.active_provider().await;
    let initial_ptr = Arc::as_ptr(&initial.provider).cast::<()>();
    assert!(initial.has_configured_api_key);

    // Write > MAX_CONFIG_FILE_BYTES -> must retain active provider without swapping or panicking.
    let oversized = vec![b' '; (MAX_CONFIG_FILE_BYTES as usize) + 64];
    fs::write(&config_path, oversized).unwrap();
    let during_oversize = state.active_provider().await;
    assert_eq!(
        Arc::as_ptr(&during_oversize.provider).cast::<()>(),
        initial_ptr,
        "oversized config must retain the previous active provider"
    );

    // Recover immediately with a valid updated config.
    cfg.providers.gemini.api_key = None;
    cfg.save_secure(&config_path).unwrap();
    let recovered = state.active_provider().await;
    assert_ne!(
        Arc::as_ptr(&recovered.provider).cast::<()>(),
        initial_ptr,
        "valid config after oversized file must hot-reload immediately"
    );
    assert!(!recovered.has_configured_api_key);
}

#[tokio::test]
async fn hot_reload_updates_enabled_models_immediately_and_preserves_cloudcode_client_arc() {
    let tmp = TempDir::new().unwrap();
    let config_path = tmp.path().join("config.json");

    let mut cfg = Config::default();
    cfg.providers.gemini.api_key = Some(ApiKey::parse_strict("AIzaEnabledReloadTest11").unwrap());
    cfg.save_secure(&config_path).unwrap();

    let state = ProxyState::from_config_with_options(
        cfg.clone(),
        Arc::new(NoopUsageSink),
        ProxyStateOptions {
            config_path: Some(config_path.clone()),
            env_fallback: false,
        },
    )
    .unwrap();

    let initial = state.active_provider().await;
    assert_eq!(
        initial.enabled_models,
        companion_core::default_enabled_gemini_models()
    );

    // Pause all models (enabled_models: []) -> updates immediately while preserving CloudCodeClient Arc.
    cfg.providers.gemini.enabled_models = Vec::new();
    cfg.save_secure(&config_path).unwrap();
    let paused = state.active_provider().await;
    assert!(paused.enabled_models.is_empty());
    assert!(
        Arc::ptr_eq(&initial.cloudcode, &paused.cloudcode),
        "enabled_models hot-reload must preserve CloudCodeClient routing cache"
    );

    // Enable Argon -> updates immediately while preserving CloudCodeClient Arc.
    cfg.providers.gemini.enabled_models = vec!["gemini-4-argon".to_string()];
    cfg.save_secure(&config_path).unwrap();
    let updated = state.active_provider().await;
    assert_eq!(updated.enabled_models, vec!["gemini-4-argon".to_string()]);
    assert!(Arc::ptr_eq(&initial.cloudcode, &updated.cloudcode));
}
