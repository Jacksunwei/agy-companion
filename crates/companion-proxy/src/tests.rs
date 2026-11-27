use std::fs;
use std::num::NonZeroU16;
use std::path::Path;
use std::sync::Mutex;

use axum::Router;
use axum::body::Body;
use axum::response::Response;
use axum::routing::post;
use companion_core::{
    ApiKey, AuthStore, FileAuthStore, GeminiCredentials, LoopbackHost, MAX_AUTH_FILE_BYTES,
    MAX_CONFIG_FILE_BYTES, MemoryAuthStore, NoopUsageSink, SqliteUsageSink, USAGE_DB_FILENAME,
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
            auth_store: None,
            env_fallback: false,
            ..Default::default()
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
    let auth_store = Arc::new(MemoryAuthStore::with_gemini_key(
        ApiKey::parse_strict("AIzaLiveLoopbackKey12345").unwrap(),
    ));

    let state = ProxyState::from_config_with_options(
        cfg,
        sink,
        ProxyStateOptions {
            config_path: None,
            auth_store: Some(auth_store),
            env_fallback: false,
            ..Default::default()
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
    let auth_store = FileAuthStore::for_config_path(&config_path).unwrap();

    let mut cfg = Config::default();
    cfg.providers.gemini.base_url = format!("http://{upstream1_addr}");
    cfg.save_secure(&config_path).unwrap();
    auth_store
        .save_gemini(&GeminiCredentials::new(
            ApiKey::parse_strict("AIzaInitialKey11111111").unwrap(),
        ))
        .unwrap();

    // env_fallback = false keeps revocation deterministic even when host exports GEMINI_API_KEY.
    let state = ProxyState::from_config_with_options(
        cfg.clone(),
        Arc::new(NoopUsageSink),
        ProxyStateOptions {
            config_path: Some(config_path.clone()),
            auth_store: None,
            env_fallback: false,
            ..Default::default()
        },
    )
    .unwrap();

    let state_for_server = state.clone();
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let proxy_addr = listener.local_addr().unwrap();
    let (tx_shutdown, rx_shutdown) = oneshot::channel::<()>();
    let server = tokio::spawn(async move {
        serve_on_listener(listener, state_for_server, async move {
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

    // 2. Immediate same-millisecond key rotation via FileAuthStore without touching config.json.
    auth_store
        .save_gemini(&GeminiCredentials::new(
            ApiKey::parse_strict("AIzaRotatedKey22222222").unwrap(),
        ))
        .unwrap();
    let r2 = client.post(&url).body("{}").send().await.unwrap();
    assert_eq!(r2.status(), StatusCode::OK);
    let _ = r2.text().await.unwrap();
    assert_eq!(
        upstream1_keys.lock().unwrap().last().cloned().flatten(),
        Some("AIzaRotatedKey22222222".to_string())
    );

    // 3. Base URL switch in config.json preserves stored auth/gemini.json key.
    cfg.providers.gemini.base_url = format!("http://{upstream2_addr}");
    cfg.save_secure(&config_path).unwrap();
    let r3 = client.post(&url).body("{}").send().await.unwrap();
    assert_eq!(r3.status(), StatusCode::OK);
    assert!(r3.text().await.unwrap().contains("upstream-2"));
    assert_eq!(
        upstream2_keys.lock().unwrap().last().cloned().flatten(),
        Some("AIzaRotatedKey22222222".to_string())
    );

    // 4. Revoke API key via FileAuthStore::clear_gemini -> 401 Unauthorized immediately.
    auth_store.clear_gemini().unwrap();
    let r4 = client.post(&url).body("{}").send().await.unwrap();
    assert_eq!(r4.status(), StatusCode::UNAUTHORIZED);

    // 5. Re-add key, confirm 200, then delete auth/gemini.json directly -> immediately 401.
    auth_store
        .save_gemini(&GeminiCredentials::new(
            ApiKey::parse_strict("AIzaReaddedKey33333333").unwrap(),
        ))
        .unwrap();
    let r5 = client.post(&url).body("{}").send().await.unwrap();
    assert_eq!(r5.status(), StatusCode::OK);
    let _ = r5.text().await.unwrap();

    fs::remove_file(auth_store.gemini_path()).unwrap();
    let r6 = client.post(&url).body("{}").send().await.unwrap();
    assert_eq!(r6.status(), StatusCode::UNAUTHORIZED);

    // 6. Re-add key and delete config.json -> resets non-secret defaults while retaining auth/gemini.json!
    auth_store
        .save_gemini(&GeminiCredentials::new(
            ApiKey::parse_strict("AIzaSurvivesCfgDelete1").unwrap(),
        ))
        .unwrap();
    cfg.providers.gemini.default_model = "gemini-4-argon".to_string();
    cfg.save_secure(&config_path).unwrap();
    let state_check_resp = client.post(&url).body("{}").send().await.unwrap();
    assert_eq!(state_check_resp.status(), StatusCode::OK);
    let _ = state_check_resp.text().await.unwrap();

    fs::remove_file(&config_path).unwrap();
    let after_cfg_delete = state.active_provider().await;
    assert_eq!(
        after_cfg_delete.default_model,
        companion_core::DEFAULT_GEMINI_MODEL,
        "deleting config.json must reset non-secret settings to Config::default()"
    );
    assert!(
        after_cfg_delete.has_configured_api_key,
        "deleting config.json must retain auth/gemini.json credentials"
    );
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
    let auth_store = FileAuthStore::for_config_path(&config_path).unwrap();

    let mut cfg = Config::default();
    cfg.providers.gemini.base_url = format!("http://{upstream_addr}");
    cfg.save_secure(&config_path).unwrap();
    auth_store
        .save_gemini(&GeminiCredentials::new(
            ApiKey::parse_strict("AIzaValidBeforeCorrupt1").unwrap(),
        ))
        .unwrap();

    let state = ProxyState::from_config_with_options(
        cfg.clone(),
        Arc::new(NoopUsageSink),
        ProxyStateOptions {
            config_path: Some(config_path.clone()),
            auth_store: None,
            env_fallback: false,
            ..Default::default()
        },
    )
    .unwrap();

    let state_for_server = state.clone();
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let proxy_addr = listener.local_addr().unwrap();
    let (tx_shutdown, rx_shutdown) = oneshot::channel::<()>();
    let server = tokio::spawn(async move {
        serve_on_listener(listener, state_for_server, async move {
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

    // 2b. Write corrupt JSON and oversized (> 64 KiB) auth/gemini.json -> retains last valid key without secret leak.
    fs::write(
        auth_store.gemini_path(),
        "{ \"api_key\": \"AIzaCORRUPTAUTHSECRET\", broken",
    )
    .unwrap();
    let r2b = client.post(&url).body("{}").send().await.unwrap();
    assert_eq!(r2b.status(), StatusCode::OK);
    let body2b = r2b.text().await.unwrap();
    assert!(!body2b.contains("AIzaCORRUPTAUTHSECRET"));
    assert_eq!(
        upstream_keys.lock().unwrap().last().cloned().flatten(),
        Some("AIzaValidBeforeCorrupt1".to_string())
    );

    fs::write(
        auth_store.gemini_path(),
        vec![b' '; (MAX_AUTH_FILE_BYTES as usize) + 64],
    )
    .unwrap();
    let r2c = client.post(&url).body("{}").send().await.unwrap();
    assert_eq!(r2c.status(), StatusCode::OK);
    let _ = r2c.text().await.unwrap();
    assert_eq!(
        upstream_keys.lock().unwrap().last().cloned().flatten(),
        Some("AIzaValidBeforeCorrupt1".to_string())
    );

    // 3. Write valid updated config + auth/gemini.json -> proxy immediately recovers and adopts new key.
    cfg.save_secure(&config_path).unwrap();
    auth_store
        .save_gemini(&GeminiCredentials::new(
            ApiKey::parse_strict("AIzaRecoveredKey9999999").unwrap(),
        ))
        .unwrap();
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

    use companion_core::AUTH_DIR_NAME;

    let (upstream1_addr, upstream1_keys, upstream1_handle) =
        spawn_mock_upstream("good-upstream").await;
    let (upstream2_addr, upstream2_keys, upstream2_handle) =
        spawn_mock_upstream("evil-upstream").await;

    let tmp = TempDir::new().unwrap();
    let good_root = tmp.path().join("good");
    let config_path = good_root.join("config.json");
    let auth_store = FileAuthStore::for_config_path(&config_path).unwrap();
    let evil_dir = tmp.path().join("evil");
    fs::create_dir_all(&evil_dir).unwrap();
    let evil_path = evil_dir.join("evil.json");
    let evil_auth_path = evil_dir.join("gemini.json");

    let mut good_cfg = Config::default();
    good_cfg.providers.gemini.base_url = format!("http://{upstream1_addr}");
    good_cfg.save_secure(&config_path).unwrap();
    auth_store
        .save_gemini(&GeminiCredentials::new(
            ApiKey::parse_strict("AIzaGoodOriginalKey1111").unwrap(),
        ))
        .unwrap();

    let mut evil_cfg = Config::default();
    evil_cfg.providers.gemini.base_url = format!("http://{upstream2_addr}");
    evil_cfg.save_secure(&evil_path).unwrap();
    fs::write(
        &evil_auth_path,
        r#"{"schema_version":1,"api_key":"AIzaEvilHijackKey22222"}"#,
    )
    .unwrap();

    // Startup must reject an initial symlink config_path AND symlinked auth/gemini.json or auth/.
    let initial_sym = tmp.path().join("initial_sym.json");
    symlink(&evil_path, &initial_sym).unwrap();
    assert!(matches!(
        ProxyState::from_config_with_options(
            good_cfg.clone(),
            Arc::new(NoopUsageSink),
            ProxyStateOptions {
                config_path: Some(initial_sym),
                auth_store: None,
                env_fallback: false,
                ..Default::default()
            },
        ),
        Err(ProxyError::Core(CoreError::Fs(
            FsError::SymlinkRejected { .. }
        )))
    ));

    let sym_auth_root = tmp.path().join("startup_sym_auth");
    let sym_auth_cfg_path = sym_auth_root.join("config.json");
    good_cfg.save_secure(&sym_auth_cfg_path).unwrap();
    symlink(&evil_dir, sym_auth_root.join(AUTH_DIR_NAME)).unwrap();
    assert!(matches!(
        ProxyState::from_config_with_options(
            good_cfg.clone(),
            Arc::new(NoopUsageSink),
            ProxyStateOptions {
                config_path: Some(sym_auth_cfg_path),
                auth_store: None,
                env_fallback: false,
                ..Default::default()
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
            auth_store: None,
            env_fallback: false,
            ..Default::default()
        },
    )
    .unwrap();

    let state_for_server = state.clone();
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let proxy_addr = listener.local_addr().unwrap();
    let (tx_shutdown, rx_shutdown) = oneshot::channel::<()>();
    let server = tokio::spawn(async move {
        serve_on_listener(listener, state_for_server, async move {
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

    // (d) Post-startup swaps on auth/gemini.json (symlink, dangling symlink, FIFO) and symlinked auth/ dir
    // must retain original key without blocking or leaking to evil upstream.
    fs::remove_file(&config_path).unwrap();
    good_cfg.save_secure(&config_path).unwrap();

    let auth_file = auth_store.gemini_path();
    fs::remove_file(&auth_file).unwrap();
    symlink(&evil_auth_path, &auth_file).unwrap();
    let r_auth_sym = client.post(&url).body("{}").send().await.unwrap();
    assert_eq!(r_auth_sym.status(), StatusCode::OK);
    let _ = r_auth_sym.text().await.unwrap();
    assert_eq!(
        upstream1_keys.lock().unwrap().last().cloned().flatten(),
        Some("AIzaGoodOriginalKey1111".to_string())
    );

    fs::remove_file(&auth_file).unwrap();
    symlink(tmp.path().join("missing-auth-target.json"), &auth_file).unwrap();
    let r_auth_dangling = client.post(&url).body("{}").send().await.unwrap();
    assert_eq!(r_auth_dangling.status(), StatusCode::OK);
    let _ = r_auth_dangling.text().await.unwrap();
    assert_eq!(
        upstream1_keys.lock().unwrap().last().cloned().flatten(),
        Some("AIzaGoodOriginalKey1111".to_string())
    );

    fs::remove_file(&auth_file).unwrap();
    let auth_fifo_status = std::process::Command::new("mkfifo")
        .arg(&auth_file)
        .status()
        .expect("run mkfifo for auth");
    assert!(auth_fifo_status.success(), "mkfifo auth failed");
    let r_auth_fifo =
        tokio::time::timeout(Duration::from_secs(1), client.post(&url).body("{}").send())
            .await
            .expect("proxy deadlocked on FIFO auth/gemini.json swap")
            .expect("request succeeded");
    assert_eq!(r_auth_fifo.status(), StatusCode::OK);
    let _ = r_auth_fifo.text().await.unwrap();
    assert_eq!(
        upstream1_keys.lock().unwrap().last().cloned().flatten(),
        Some("AIzaGoodOriginalKey1111".to_string())
    );

    // Symlinked auth/ dir swap
    let auth_dir = auth_store.auth_dir().unwrap().to_path_buf();
    fs::remove_dir_all(&auth_dir).unwrap();
    symlink(&evil_dir, &auth_dir).unwrap();
    let r_auth_dir_sym = client.post(&url).body("{}").send().await.unwrap();
    assert_eq!(r_auth_dir_sym.status(), StatusCode::OK);
    let _ = r_auth_dir_sym.text().await.unwrap();
    assert_eq!(
        upstream1_keys.lock().unwrap().last().cloned().flatten(),
        Some("AIzaGoodOriginalKey1111".to_string())
    );
    assert!(upstream2_keys.lock().unwrap().is_empty());

    // (e) Clean recovery when replaced back with regular auth/ and auth/gemini.json via FileAuthStore.
    fs::remove_file(&auth_dir).unwrap();
    auth_store
        .save_gemini(&GeminiCredentials::new(
            ApiKey::parse_strict("AIzaRecoveredAfterSwap33").unwrap(),
        ))
        .unwrap();
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
    let auth_store = FileAuthStore::for_config_path(&config_path).unwrap();

    let mut cfg = Config::default();
    cfg.cloudcode.upstream = CloudCodeUpstream::Auto;
    cfg.providers.gemini.timeout_secs = 900;
    cfg.save_secure(&config_path).unwrap();

    let state = ProxyState::from_config_with_options(
        cfg.clone(),
        Arc::new(NoopUsageSink),
        ProxyStateOptions {
            config_path: Some(config_path.clone()),
            auth_store: None,
            env_fallback: false,
            ..Default::default()
        },
    )
    .unwrap();

    let initial_cloudcode = Arc::clone(&state.active_provider().await.cloudcode);
    assert_eq!(initial_cloudcode.mode_label(), "auto");
    initial_cloudcode.seed_prod_route_for_test();
    assert_eq!(
        initial_cloudcode.base_url(),
        companion_core::CLOUDCODE_PROD_UPSTREAM_URL
    );

    // Saving a new API key in auth/gemini.json without changing cloudcode.upstream or timeout_secs
    // must preserve the CloudCode routing cache across hot-reload.
    auth_store
        .save_gemini(&GeminiCredentials::new(
            ApiKey::parse_strict("AIzaSyReloadPreservesLru1").unwrap(),
        ))
        .unwrap();
    let after_key_save = state.active_provider().await;
    assert!(after_key_save.has_configured_api_key);
    assert_eq!(after_key_save.cloudcode.mode_label(), "auto");
    assert_eq!(after_key_save.cloudcode_timeout_secs, 900);
    assert_eq!(
        after_key_save.cloudcode.base_url(),
        companion_core::CLOUDCODE_PROD_UPSTREAM_URL,
        "CloudCode routing cache must survive API key hot-reload"
    );

    // Changing timeout_secs must rebuild CloudCodeClient with a fresh routing cache and new timeout.
    cfg.providers.gemini.timeout_secs = 600;
    cfg.save_secure(&config_path).unwrap();
    let after_timeout_change = state.active_provider().await;
    assert_eq!(after_timeout_change.cloudcode_timeout_secs, 600);
    assert_eq!(after_timeout_change.cloudcode.mode_label(), "auto");
    assert_eq!(
        after_timeout_change.cloudcode.base_url(),
        companion_core::CLOUDCODE_DAILY_UPSTREAM_URL,
        "CloudCodeClient must rebuild with fresh routing cache when timeout_secs changes"
    );

    // Changing cloudcode.upstream must rebuild CloudCodeClient in fixed mode.
    cfg.cloudcode.upstream =
        CloudCodeUpstream::Fixed("https://cloudcode-pa.googleapis.com".to_string());
    cfg.save_secure(&config_path).unwrap();
    let after_url_change = state.active_provider().await;
    assert_eq!(after_url_change.cloudcode.mode_label(), "fixed");
    assert_eq!(
        after_url_change.cloudcode.base_url(),
        "https://cloudcode-pa.googleapis.com"
    );
    assert_eq!(
        after_url_change.cloudcode_upstream,
        CloudCodeUpstream::Fixed("https://cloudcode-pa.googleapis.com".to_string())
    );
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
    cfg.cloudcode.upstream = CloudCodeUpstream::Fixed(format!("http://{upstream_addr}"));
    let auth_store = Arc::new(MemoryAuthStore::with_gemini_key(
        ApiKey::parse_strict("AIzaSyActivityTestKey123").unwrap(),
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
            auth_store: None,
            env_fallback: false,
            ..Default::default()
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
    assert_eq!(
        active_new.cloudcode.base_url(),
        companion_core::CLOUDCODE_DAILY_UPSTREAM_URL
    );
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
    assert_eq!(active_di.cloudcode.mode_label(), "fixed");
    assert_eq!(
        active_di.cloudcode.base_url(),
        "https://cloudcode-pa.googleapis.com"
    );
    assert!(active_di.has_configured_api_key);

    let state_cfg = ProxyState::from_config(Config::default(), Arc::new(NoopUsageSink))
        .expect("ProxyState::from_config must delegate cleanly");
    assert_eq!(state_cfg.in_flight_requests(), 0);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn hot_reload_concurrent_readers_see_immediate_rotation_with_zero_sleep() {
    use bytes::Bytes;
    use companion_providers::ForwardRequest;
    use tokio::sync::Barrier;

    let (upstream_addr, upstream_keys, upstream_handle) = spawn_mock_upstream("concurrent").await;
    let tmp = TempDir::new().unwrap();
    let config_path = tmp.path().join("config.json");
    let auth_store = FileAuthStore::for_config_path(&config_path).unwrap();

    let mut cfg = Config::default();
    cfg.providers.gemini.base_url = format!("http://{upstream_addr}");
    cfg.save_secure(&config_path).unwrap();
    auth_store
        .save_gemini(&GeminiCredentials::new(
            ApiKey::parse_strict("AIzaConcurrentInit11111").unwrap(),
        ))
        .unwrap();

    let state = ProxyState::from_config_with_options(
        cfg.clone(),
        Arc::new(NoopUsageSink),
        ProxyStateOptions {
            config_path: Some(config_path.clone()),
            auth_store: None,
            env_fallback: false,
            ..Default::default()
        },
    )
    .unwrap();

    let initial = state.active_provider().await;
    initial.cloudcode.seed_prod_route_for_test();
    assert_eq!(
        initial.cloudcode.base_url(),
        companion_core::CLOUDCODE_PROD_UPSTREAM_URL
    );

    // Rotate key in auth/gemini.json and default_model in config.json immediately with zero sleep and fire 16 concurrent readers.
    auth_store
        .save_gemini(&GeminiCredentials::new(
            ApiKey::parse_strict("AIzaConcurrentNext22222").unwrap(),
        ))
        .unwrap();
    cfg.providers.gemini.default_model = "gemini-4-argon".to_string();
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

    for active in &results {
        assert!(active.has_configured_api_key);
        assert_eq!(active.default_model, "gemini-4-argon");
        assert_eq!(
            active.cloudcode.base_url(),
            companion_core::CLOUDCODE_PROD_UPSTREAM_URL,
            "CloudCode routing cache must survive concurrent key rotation"
        );
    }

    // Verify rotated API key is delivered live to the mock upstream.
    let resp = results[0]
        .provider
        .forward_gemini(ForwardRequest {
            path_and_query: "/v1beta/models/gemini-4-argon:streamGenerateContent?alt=sse"
                .to_string(),
            body: Bytes::from_static(b"{}"),
            content_type: Some("application/json".to_string()),
            api_key_override: None,
            conversation_id: None,
        })
        .await
        .expect("forward with rotated provider");
    assert_eq!(resp.status, StatusCode::OK);
    assert_eq!(
        upstream_keys.lock().unwrap().as_slice(),
        &[Some("AIzaConcurrentNext22222".to_string())]
    );
    upstream_handle.abort();
}

#[tokio::test]
async fn hot_reload_detects_same_second_in_place_edit_with_identical_length() {
    let (upstream_addr, upstream_keys, upstream_handle) = spawn_mock_upstream("in-place").await;

    let tmp = TempDir::new().unwrap();
    let config_path = tmp.path().join("config.json");
    let auth_store = FileAuthStore::for_config_path(&config_path).unwrap();

    let make_cfg_json = |model: &str| {
        serde_json::json!({
            "schema_version": 1,
            "proxy": { "host": "127.0.0.1", "port": companion_core::DEFAULT_LISTEN_PORT },
            "cloudcode": { "upstream": "auto" },
            "providers": {
                "gemini": {
                    "base_url": format!("http://{upstream_addr}"),
                    "default_model": model,
                    "timeout_secs": 900
                }
            }
        })
        .to_string()
    };

    let make_auth_json = |key: &str| {
        serde_json::json!({
            "schema_version": 1,
            "api_key": key
        })
        .to_string()
    };

    let cfg_v1_json = make_cfg_json("gemini-4-pro-1");
    let cfg_v2_json = make_cfg_json("gemini-4-pro-2");
    assert_eq!(cfg_v1_json.len(), cfg_v2_json.len());
    fs::write(&config_path, &cfg_v1_json).unwrap();

    let auth_v1 = make_auth_json("AIzaSameLenKeyAAAAA1111");
    let auth_v2 = make_auth_json("AIzaSameLenKeyBBBBB2222");
    assert_eq!(auth_v1.len(), auth_v2.len());
    fs::create_dir_all(auth_store.auth_dir().unwrap()).unwrap();
    fs::write(auth_store.gemini_path(), &auth_v1).unwrap();

    let cfg_v1 = Config::from_json_str(&cfg_v1_json, &config_path).unwrap();
    let state = ProxyState::from_config_with_options(
        cfg_v1,
        Arc::new(NoopUsageSink),
        ProxyStateOptions {
            config_path: Some(config_path.clone()),
            auth_store: None,
            env_fallback: false,
            ..Default::default()
        },
    )
    .unwrap();

    let state_for_server = state.clone();
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let proxy_addr = listener.local_addr().unwrap();
    let (tx_shutdown, rx_shutdown) = oneshot::channel::<()>();
    let server = tokio::spawn(async move {
        serve_on_listener(listener, state_for_server, async move {
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

    // Same-second in-place overwrite with identical byte length on auth/gemini.json, zero sleep.
    fs::write(auth_store.gemini_path(), &auth_v2).unwrap();
    let r2 = client.post(&url).body("{}").send().await.unwrap();
    assert_eq!(r2.status(), StatusCode::OK);
    let _ = r2.text().await.unwrap();
    assert_eq!(
        upstream_keys.lock().unwrap().last().cloned().flatten(),
        Some("AIzaSameLenKeyBBBBB2222".to_string())
    );

    // Same-second in-place overwrite with identical byte length on config.json, zero sleep.
    fs::write(&config_path, &cfg_v2_json).unwrap();
    assert_eq!(
        state.active_provider().await.default_model,
        "gemini-4-pro-2"
    );

    let _ = tx_shutdown.send(());
    server.await.unwrap();
    upstream_handle.abort();
}

#[tokio::test]
async fn hot_reload_retains_active_provider_on_oversized_config_and_recovers() {
    let tmp = TempDir::new().unwrap();
    let config_path = tmp.path().join("config.json");
    let auth_store = FileAuthStore::for_config_path(&config_path).unwrap();

    let mut cfg = Config::default();
    cfg.save_secure(&config_path).unwrap();
    auth_store
        .save_gemini(&GeminiCredentials::new(
            ApiKey::parse_strict("AIzaBeforeOversize11111").unwrap(),
        ))
        .unwrap();

    let state = ProxyState::from_config_with_options(
        cfg.clone(),
        Arc::new(NoopUsageSink),
        ProxyStateOptions {
            config_path: Some(config_path.clone()),
            auth_store: None,
            env_fallback: false,
            ..Default::default()
        },
    )
    .unwrap();

    let initial = state.active_provider().await;
    initial.cloudcode.seed_prod_route_for_test();
    assert!(initial.has_configured_api_key);
    assert_eq!(initial.default_model, companion_core::DEFAULT_GEMINI_MODEL);

    // Write > MAX_CONFIG_FILE_BYTES -> must retain active provider and routing cache without panicking.
    let oversized = vec![b' '; (MAX_CONFIG_FILE_BYTES as usize) + 64];
    fs::write(&config_path, oversized).unwrap();
    let during_oversize = state.active_provider().await;
    assert!(
        during_oversize.has_configured_api_key,
        "oversized config must retain the previous active provider"
    );
    assert_eq!(
        during_oversize.default_model,
        companion_core::DEFAULT_GEMINI_MODEL
    );
    assert_eq!(
        during_oversize.cloudcode.base_url(),
        companion_core::CLOUDCODE_PROD_UPSTREAM_URL
    );

    // Recover immediately with a valid updated config and cleared auth key.
    auth_store.clear_gemini().unwrap();
    cfg.providers.gemini.default_model = "gemini-4-argon".to_string();
    cfg.save_secure(&config_path).unwrap();
    let recovered = state.active_provider().await;
    assert!(
        !recovered.has_configured_api_key,
        "valid config after oversized file must hot-reload immediately"
    );
    assert_eq!(recovered.default_model, "gemini-4-argon");
}

#[tokio::test]
async fn hot_reload_updates_enabled_models_immediately_and_preserves_cloudcode_routing_cache() {
    let tmp = TempDir::new().unwrap();
    let config_path = tmp.path().join("config.json");
    let auth_store = FileAuthStore::for_config_path(&config_path).unwrap();

    let mut cfg = Config::default();
    cfg.save_secure(&config_path).unwrap();
    auth_store
        .save_gemini(&GeminiCredentials::new(
            ApiKey::parse_strict("AIzaEnabledReloadTest11").unwrap(),
        ))
        .unwrap();

    let state = ProxyState::from_config_with_options(
        cfg.clone(),
        Arc::new(NoopUsageSink),
        ProxyStateOptions {
            config_path: Some(config_path.clone()),
            auth_store: None,
            env_fallback: false,
            ..Default::default()
        },
    )
    .unwrap();

    let initial = state.active_provider().await;
    initial.cloudcode.seed_prod_route_for_test();
    assert_eq!(
        initial.enabled_models,
        companion_core::default_enabled_gemini_models()
    );

    // Pause all models (enabled_models: []) -> updates immediately while preserving CloudCode routing cache.
    cfg.providers.gemini.enabled_models = Vec::new();
    cfg.save_secure(&config_path).unwrap();
    let paused = state.active_provider().await;
    assert!(paused.enabled_models.is_empty());
    assert_eq!(
        paused.cloudcode.base_url(),
        companion_core::CLOUDCODE_PROD_UPSTREAM_URL,
        "enabled_models hot-reload must preserve CloudCode routing cache"
    );

    // Enable Argon -> updates immediately while preserving CloudCode routing cache.
    cfg.providers.gemini.enabled_models = vec!["gemini-4-argon".to_string()];
    cfg.save_secure(&config_path).unwrap();
    let updated = state.active_provider().await;
    assert_eq!(updated.enabled_models, vec!["gemini-4-argon".to_string()]);
    assert_eq!(
        updated.cloudcode.base_url(),
        companion_core::CLOUDCODE_PROD_UPSTREAM_URL
    );
}

#[tokio::test]
async fn hot_reload_watches_codex_auth_sign_in_refresh_corrupt_and_sign_out() {
    use companion_core::{
        AUTH_SCHEMA_VERSION, CODEX_AUTH_FILENAME, CodexAuthMode, CodexCredentials, SecretString,
    };

    let tmp = TempDir::new().unwrap();
    let config_path = tmp.path().join("config.json");
    let auth_store = FileAuthStore::for_config_path(&config_path).unwrap();

    let cfg = Config::default();
    cfg.save_secure(&config_path).unwrap();

    let state = ProxyState::from_config_with_options(
        cfg,
        Arc::new(NoopUsageSink),
        ProxyStateOptions {
            config_path: Some(config_path.clone()),
            auth_store: None,
            env_fallback: false,
            ..Default::default()
        },
    )
    .unwrap();

    // 1. Starts signed out.
    let initial = state.active_provider().await;
    assert!(!initial.codex_authenticated);

    // 2. Writing auth/codex.json hot-reloads immediately to authenticated.
    let mut creds = CodexCredentials {
        schema_version: AUTH_SCHEMA_VERSION,
        auth_mode: CodexAuthMode::ChatGptOAuth,
        access_token: SecretString::new("codex_hot_reload_tok_1"),
        refresh_token: SecretString::new("codex_hot_reload_ref_1"),
        id_token: None,
        account_id: "acct_hot_reload_1".to_string(),
        expires_at_ms: 4_102_444_800_000,
        plan_type: Some("pro".to_string()),
        email_hint: Some("h***@example.com".to_string()),
        last_refresh_utc: "2026-11-26T00:00:00Z".to_string(),
    };
    auth_store.save_codex(&creds).unwrap();
    let signed_in = state.active_provider().await;
    assert!(signed_in.codex_authenticated);

    // 3. Updating auth/codex.json (token refresh) stays authenticated cleanly.
    creds.access_token = SecretString::new("codex_hot_reload_tok_2");
    auth_store.save_codex(&creds).unwrap();
    let refreshed = state.active_provider().await;
    assert!(refreshed.codex_authenticated);

    // 4. Corrupt auth/codex.json fails closed and retains previous authenticated state.
    let codex_path = auth_store.auth_dir().unwrap().join(CODEX_AUTH_FILENAME);
    std::fs::write(&codex_path, "{corrupt-json").unwrap();
    let after_corrupt = state.active_provider().await;
    assert!(after_corrupt.codex_authenticated);

    // 5. Deleting auth/codex.json (sign-out) hot-reloads immediately to unauthenticated.
    auth_store.clear_codex().unwrap();
    let signed_out = state.active_provider().await;
    assert!(!signed_out.codex_authenticated);

    // 6. Post-startup symlinked auth/codex.json is ignored fail-closed on Unix.
    #[cfg(unix)]
    {
        let victim = tmp.path().join("victim_codex.json");
        std::fs::write(&victim, serde_json::to_vec(&creds).unwrap()).unwrap();
        std::os::unix::fs::symlink(&victim, &codex_path).unwrap();
        let after_symlink = state.active_provider().await;
        assert!(
            !after_symlink.codex_authenticated,
            "symlinked auth/codex.json must be rejected fail-closed"
        );
    }
}
