use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use bytes::Bytes;
use http::header::{self, HeaderMap, HeaderName, HeaderValue};
use http::{Method, StatusCode};
use serde_json::{Map, Value, json};

use super::routing::{
    AccountRoute, CloudCodeRoutingCache, detect_auth_method_from_headers,
    detect_prod_from_load_code_assist, is_loopback_url,
};
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
        detect_prod_from_load_code_assist(br#"{"currentTier":{"id":"agy-business-paygo-tier"}}"#),
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
    // First-pass loadCodeAssist responses without paidTier/currentTier must not force daily.
    assert_eq!(
        detect_prod_from_load_code_assist(br#"{"cloudaicompanionProject":"weisun-cai"}"#),
        None
    );
    assert_eq!(detect_prod_from_load_code_assist(br#"{}"#), None);
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

#[tokio::test]
async fn fetch_available_models_defaults_to_daily_even_with_non_empty_project_field() {
    use axum::Router;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use tokio::net::TcpListener;

    let daily_hits = Arc::new(AtomicUsize::new(0));
    let prod_hits = Arc::new(AtomicUsize::new(0));

    let daily_hits_c = Arc::clone(&daily_hits);
    let mock_daily = Router::new().route(
        "/v1internal:fetchAvailableModels",
        axum::routing::post(move || {
            let daily_hits = Arc::clone(&daily_hits_c);
            async move {
                daily_hits.fetch_add(1, Ordering::SeqCst);
                (
                    StatusCode::OK,
                    [(header::CONTENT_TYPE, "application/json")],
                    r#"{"models":{"gemini-4-carbon-high":{"displayName":"Gemini 4 Carbon (High)"}}}"#,
                )
            }
        }),
    );
    let daily_listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let daily_addr = daily_listener.local_addr().unwrap();
    let daily_srv = tokio::spawn(async move {
        let _ = axum::serve(daily_listener, mock_daily).await;
    });

    let prod_hits_c = Arc::clone(&prod_hits);
    let mock_prod = Router::new().route(
        "/v1internal:fetchAvailableModels",
        axum::routing::post(move || {
            let prod_hits = Arc::clone(&prod_hits_c);
            async move {
                prod_hits.fetch_add(1, Ordering::SeqCst);
                (
                    StatusCode::OK,
                    [(header::CONTENT_TYPE, "application/json")],
                    r#"{"models":{"prod-only":{"displayName":"Prod Only"}}}"#,
                )
            }
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
    assert_eq!(client.mode_label(), "auto");
    assert_eq!(client.resolved_upstream_url(), None);

    let mut headers = HeaderMap::new();
    headers.insert(
        header::AUTHORIZATION,
        HeaderValue::from_static("Bearer ai-pro-consumer-token"),
    );

    // Consumer Google AI Pro accounts send a non-empty managed `project` field;
    // unresolved auto must still try daily first and never touch prod.
    let resp = client
        .forward_fetch_available_models(
            "/v1internal:fetchAvailableModels",
            &headers,
            Bytes::from_static(br#"{"project":"managed-caic-123"}"#),
        )
        .await
        .expect("daily request succeeds");
    assert_eq!(resp.status, StatusCode::OK);
    assert_eq!(daily_hits.load(Ordering::SeqCst), 1);
    assert_eq!(prod_hits.load(Ordering::SeqCst), 0);
    assert_eq!(
        client.resolved_upstream_url().as_deref(),
        Some(format!("http://{daily_addr}").as_str())
    );

    daily_srv.abort();
    prod_srv.abort();
}

#[tokio::test]
async fn fetch_available_models_routes_enterprise_entitlement_and_strips_user_project() {
    use axum::Router;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use tokio::net::TcpListener;

    let daily_hits = Arc::new(AtomicUsize::new(0));
    let prod_hits = Arc::new(AtomicUsize::new(0));
    let saw_user_project = Arc::new(Mutex::new(Vec::<Option<String>>::new()));

    let daily_hits_c = Arc::clone(&daily_hits);
    let mock_daily = Router::new().route(
        "/v1internal:fetchAvailableModels",
        axum::routing::post(move || {
            let daily_hits = Arc::clone(&daily_hits_c);
            async move {
                daily_hits.fetch_add(1, Ordering::SeqCst);
                (StatusCode::INTERNAL_SERVER_ERROR, "unexpected daily hit")
            }
        }),
    );
    let daily_listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let daily_addr = daily_listener.local_addr().unwrap();
    let daily_srv = tokio::spawn(async move {
        let _ = axum::serve(daily_listener, mock_daily).await;
    });

    let prod_hits_c = Arc::clone(&prod_hits);
    let saw_up_c = Arc::clone(&saw_user_project);
    let mock_prod = Router::new().route(
        "/v1internal:fetchAvailableModels",
        axum::routing::post(move |headers: HeaderMap| {
            let prod_hits = Arc::clone(&prod_hits_c);
            let saw_up = Arc::clone(&saw_up_c);
            async move {
                prod_hits.fetch_add(1, Ordering::SeqCst);
                let up = headers
                    .get("x-goog-user-project")
                    .and_then(|v| v.to_str().ok())
                    .map(str::to_owned);
                saw_up.lock().unwrap().push(up);
                (
                    StatusCode::OK,
                    [(header::CONTENT_TYPE, "application/json")],
                    r#"{"models":{"enterprise-model":{"displayName":"Enterprise Model"}}}"#,
                )
            }
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

    let mut headers = HeaderMap::new();
    headers.insert(
        header::AUTHORIZATION,
        HeaderValue::from_static("Bearer enterprise-token"),
    );
    headers.insert(
        HeaderName::from_static("x-goog-user-project"),
        HeaderValue::from_static("corp-project-1"),
    );

    let resp = client
        .forward_fetch_available_models(
            "/v1internal:fetchAvailableModels",
            &headers,
            Bytes::from_static(br#"{"entitlement":{"userTier":"ENTERPRISE"}}"#),
        )
        .await
        .expect("enterprise request succeeds");
    assert_eq!(resp.status, StatusCode::OK);
    assert_eq!(daily_hits.load(Ordering::SeqCst), 0);
    assert_eq!(prod_hits.load(Ordering::SeqCst), 1);
    assert_eq!(*saw_user_project.lock().unwrap(), vec![None]);
    assert_eq!(
        client.resolved_upstream_url().as_deref(),
        Some(format!("http://{prod_addr}").as_str())
    );

    daily_srv.abort();
    prod_srv.abort();
}

#[tokio::test]
async fn fetch_available_models_honors_user_agent_auth_method_gcp_adc_and_consumer() {
    use axum::Router;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use tokio::net::TcpListener;

    // Unit check boundary parsing first.
    let check_ua = |ua: &str| {
        let mut h = HeaderMap::new();
        h.insert(header::USER_AGENT, HeaderValue::from_str(ua).unwrap());
        detect_auth_method_from_headers(&h)
    };
    assert_eq!(check_ua("antigravity/2.34.0 auth_method=gcp"), Some(true));
    assert_eq!(
        check_ua("antigravity/2.34.0 (auth_method=ADC; macos)"),
        Some(true)
    );
    assert_eq!(
        check_ua("antigravity/2.34.0 auth_method=consumer"),
        Some(false)
    );
    assert_eq!(check_ua("antigravity/2.34.0 custom_auth_method=gcp"), None);
    assert_eq!(check_ua("antigravity/2.34.0 auth_method=gcpx"), None);

    let daily_hits = Arc::new(AtomicUsize::new(0));
    let prod_hits = Arc::new(AtomicUsize::new(0));

    let daily_hits_c = Arc::clone(&daily_hits);
    let mock_daily = Router::new().route(
        "/v1internal:fetchAvailableModels",
        axum::routing::post(move || {
            let daily_hits = Arc::clone(&daily_hits_c);
            async move {
                daily_hits.fetch_add(1, Ordering::SeqCst);
                (
                    StatusCode::OK,
                    [(header::CONTENT_TYPE, "application/json")],
                    r#"{"models":{"daily-model":{"displayName":"Daily"}}}"#,
                )
            }
        }),
    );
    let daily_listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let daily_addr = daily_listener.local_addr().unwrap();
    let daily_srv = tokio::spawn(async move {
        let _ = axum::serve(daily_listener, mock_daily).await;
    });

    let prod_hits_c = Arc::clone(&prod_hits);
    let mock_prod = Router::new().route(
        "/v1internal:fetchAvailableModels",
        axum::routing::post(move || {
            let prod_hits = Arc::clone(&prod_hits_c);
            async move {
                prod_hits.fetch_add(1, Ordering::SeqCst);
                (
                    StatusCode::OK,
                    [(header::CONTENT_TYPE, "application/json")],
                    r#"{"models":{"prod-model":{"displayName":"Prod"}}}"#,
                )
            }
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

    // auth_method=gcp -> prod directly (0 daily hits).
    let mut gcp_headers = HeaderMap::new();
    gcp_headers.insert(
        header::AUTHORIZATION,
        HeaderValue::from_static("Bearer gcp-acct"),
    );
    gcp_headers.insert(
        header::USER_AGENT,
        HeaderValue::from_static("antigravity/2.34.0 auth_method=gcp"),
    );
    let r_gcp = client
        .forward_fetch_available_models(
            "/v1internal:fetchAvailableModels",
            &gcp_headers,
            Bytes::from_static(b"{}"),
        )
        .await
        .unwrap();
    assert!(String::from_utf8_lossy(&r_gcp.body).contains("prod-model"));
    assert_eq!(prod_hits.load(Ordering::SeqCst), 1);
    assert_eq!(daily_hits.load(Ordering::SeqCst), 0);

    // auth_method=adc -> prod directly.
    let mut adc_headers = HeaderMap::new();
    adc_headers.insert(
        header::AUTHORIZATION,
        HeaderValue::from_static("Bearer adc-acct"),
    );
    adc_headers.insert(
        header::USER_AGENT,
        HeaderValue::from_static("antigravity/2.34.0 auth_method=adc"),
    );
    let r_adc = client
        .forward_fetch_available_models(
            "/v1internal:fetchAvailableModels",
            &adc_headers,
            Bytes::from_static(b"{}"),
        )
        .await
        .unwrap();
    assert!(String::from_utf8_lossy(&r_adc.body).contains("prod-model"));
    assert_eq!(prod_hits.load(Ordering::SeqCst), 2);
    assert_eq!(daily_hits.load(Ordering::SeqCst), 0);

    // auth_method=consumer -> daily directly (even though last_resolved_prod is true!).
    let mut consumer_headers = HeaderMap::new();
    consumer_headers.insert(
        header::AUTHORIZATION,
        HeaderValue::from_static("Bearer consumer-acct"),
    );
    consumer_headers.insert(
        header::USER_AGENT,
        HeaderValue::from_static("antigravity/2.34.0 auth_method=consumer"),
    );
    let r_consumer = client
        .forward_fetch_available_models(
            "/v1internal:fetchAvailableModels",
            &consumer_headers,
            Bytes::from_static(b"{}"),
        )
        .await
        .unwrap();
    assert!(String::from_utf8_lossy(&r_consumer.body).contains("daily-model"));
    assert_eq!(daily_hits.load(Ordering::SeqCst), 1);
    assert_eq!(prod_hits.load(Ordering::SeqCst), 2);

    daily_srv.abort();
    prod_srv.abort();
}

#[tokio::test]
async fn load_code_assist_detects_current_tier_uses_gcp_tos_and_gcp_prefix_and_falls_back_prod_to_daily()
 {
    use axum::Router;
    use tokio::net::TcpListener;

    // Pure detection checks for currentTier.usesGcpTos and gcp-* prefixes.
    assert_eq!(
        detect_prod_from_load_code_assist(br#"{"currentTier":{"usesGcpTos":true}}"#),
        Some(true)
    );
    assert_eq!(
        detect_prod_from_load_code_assist(br#"{"currentTier":{"id":"gcp-standard-tier"}}"#),
        Some(true)
    );
    assert_eq!(
        detect_prod_from_load_code_assist(br#"{"paidTier":{"id":"gcp-ge-1"}}"#),
        Some(true)
    );

    // Prod returns 500 -> loadCodeAssist must fall back to daily and preserve Content-Encoding.
    let mock_prod = Router::new().route(
        "/v1internal:loadCodeAssist",
        axum::routing::post(|| async { (StatusCode::INTERNAL_SERVER_ERROR, "prod down") }),
    );
    let prod_listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let prod_addr = prod_listener.local_addr().unwrap();
    let prod_srv = tokio::spawn(async move {
        let _ = axum::serve(prod_listener, mock_prod).await;
    });

    let mock_daily = Router::new().route(
        "/v1internal:loadCodeAssist",
        axum::routing::post(|| async {
            (
                StatusCode::OK,
                [
                    (header::CONTENT_TYPE, "application/json"),
                    (header::CONTENT_ENCODING, "gzip"),
                ],
                r#"{"currentTier":{"id":"free-tier"}}"#,
            )
        }),
    );
    let daily_listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let daily_addr = daily_listener.local_addr().unwrap();
    let daily_srv = tokio::spawn(async move {
        let _ = axum::serve(daily_listener, mock_daily).await;
    });

    let client = CloudCodeClient::new_auto_with_urls(
        &format!("http://{daily_addr}"),
        &format!("http://{prod_addr}"),
        30,
    )
    .expect("auto client");

    let mut headers = HeaderMap::new();
    headers.insert(
        header::AUTHORIZATION,
        HeaderValue::from_static("Bearer fallback-acct"),
    );

    let resp = client
        .forward_raw(
            Method::POST,
            "/v1internal:loadCodeAssist",
            &headers,
            Bytes::from_static(b"{}"),
        )
        .await
        .expect("prod -> daily fallback succeeds");
    assert_eq!(resp.status, StatusCode::OK);
    assert_eq!(
        resp.headers
            .get(header::CONTENT_ENCODING)
            .and_then(|v| v.to_str().ok()),
        Some("gzip"),
        "Content-Encoding must be preserved verbatim on loadCodeAssist pass-through"
    );
    assert_eq!(
        client.resolved_upstream_url().as_deref(),
        Some(format!("http://{daily_addr}").as_str())
    );

    daily_srv.abort();
    prod_srv.abort();
}

#[tokio::test]
async fn enterprise_403_retries_once_without_user_project_on_metadata_rpcs_only_and_caches_per_account()
 {
    use axum::Router;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use tokio::net::TcpListener;

    let fetch_attempts = Arc::new(AtomicUsize::new(0));
    let lca_attempts = Arc::new(AtomicUsize::new(0));
    let stream_attempts = Arc::new(AtomicUsize::new(0));
    let stream_seen_user_projects = Arc::new(Mutex::new(Vec::<Option<String>>::new()));
    let daily_stream_hits = Arc::new(AtomicUsize::new(0));

    let daily_stream_c = Arc::clone(&daily_stream_hits);
    let mock_daily = Router::new()
        .route(
            "/v1internal:fetchAvailableModels",
            axum::routing::post(|| async {
                (
                    StatusCode::OK,
                    [(header::CONTENT_TYPE, "application/json")],
                    r#"{"models":{}}"#,
                )
            }),
        )
        .route(
            "/v1internal:streamGenerateContent",
            axum::routing::post(move || {
                let daily_stream = Arc::clone(&daily_stream_c);
                async move {
                    daily_stream.fetch_add(1, Ordering::SeqCst);
                    (StatusCode::OK, "daily-stream")
                }
            }),
        );
    let daily_listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let daily_addr = daily_listener.local_addr().unwrap();
    let daily_srv = tokio::spawn(async move {
        let _ = axum::serve(daily_listener, mock_daily).await;
    });

    let fetch_attempts_c = Arc::clone(&fetch_attempts);
    let lca_attempts_c = Arc::clone(&lca_attempts);
    let stream_attempts_c = Arc::clone(&stream_attempts);
    let stream_up_c = Arc::clone(&stream_seen_user_projects);
    let mock_prod = Router::new()
        .route(
            "/v1internal:loadCodeAssist",
            axum::routing::post(move |headers: HeaderMap| {
                let lca_attempts = Arc::clone(&lca_attempts_c);
                async move {
                    lca_attempts.fetch_add(1, Ordering::SeqCst);
                    if headers.contains_key("x-goog-user-project") {
                        return (
                            StatusCode::FORBIDDEN,
                            [(header::CONTENT_TYPE, "application/json")],
                            r#"{"error":{"status":"PERMISSION_DENIED"}}"#,
                        );
                    }
                    (
                        StatusCode::OK,
                        [(header::CONTENT_TYPE, "application/json")],
                        r#"{"currentTier":{"id":"gcp-ge-enterprise","usesGcpTos":true}}"#,
                    )
                }
            }),
        )
        .route(
            "/v1internal:fetchAvailableModels",
            axum::routing::post(move |headers: HeaderMap| {
                let fetch_attempts = Arc::clone(&fetch_attempts_c);
                async move {
                    fetch_attempts.fetch_add(1, Ordering::SeqCst);
                    let auth = headers
                        .get(header::AUTHORIZATION)
                        .and_then(|v| v.to_str().ok())
                        .unwrap_or("");
                    let has_up = headers.contains_key("x-goog-user-project");
                    // Enterprise account gets 403 whenever x-goog-user-project is present.
                    if auth == "Bearer ent-403-acct" && has_up {
                        return (
                            StatusCode::FORBIDDEN,
                            [(header::CONTENT_TYPE, "application/json")],
                            r#"{"error":{"status":"PERMISSION_DENIED"}}"#,
                        );
                    }
                    (
                        StatusCode::OK,
                        [(header::CONTENT_TYPE, "application/json")],
                        r#"{"models":{"prod-model":{"displayName":"Prod Model"}}}"#,
                    )
                }
            }),
        )
        .route(
            "/v1internal:streamGenerateContent",
            axum::routing::post(move |headers: HeaderMap| {
                let stream_attempts = Arc::clone(&stream_attempts_c);
                let stream_up = Arc::clone(&stream_up_c);
                async move {
                    stream_attempts.fetch_add(1, Ordering::SeqCst);
                    let up = headers
                        .get("x-goog-user-project")
                        .and_then(|v| v.to_str().ok())
                        .map(str::to_owned);
                    stream_up.lock().unwrap().push(up.clone());
                    let auth = headers
                        .get(header::AUTHORIZATION)
                        .and_then(|v| v.to_str().ok())
                        .unwrap_or("");
                    if auth == "Bearer uncached-403-acct" {
                        // Even on 403 with x-goog-user-project present, generation RPC must NEVER retry!
                        return (StatusCode::FORBIDDEN, "no retry on generation");
                    }
                    (StatusCode::OK, "ok-stream")
                }
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

    // 1. Enterprise account hits prod -> 403 -> retries ONCE without x-goog-user-project -> 200.
    let mut ent_headers = HeaderMap::new();
    ent_headers.insert(
        header::AUTHORIZATION,
        HeaderValue::from_static("Bearer ent-403-acct"),
    );
    ent_headers.insert(
        header::USER_AGENT,
        HeaderValue::from_static("antigravity/2.34.0 auth_method=gcp"),
    );
    ent_headers.insert(
        HeaderName::from_static("x-goog-user-project"),
        HeaderValue::from_static("billing-proj"),
    );
    let resp = client
        .forward_fetch_available_models(
            "/v1internal:fetchAvailableModels",
            &ent_headers,
            Bytes::from_static(b"{}"),
        )
        .await
        .expect("403 retry succeeds");
    assert_eq!(resp.status, StatusCode::OK);
    assert_eq!(fetch_attempts.load(Ordering::SeqCst), 2);

    // 2. Subsequent forward_raw for ent-403-acct strips x-goog-user-project on attempt 1!
    let stream_resp = client
        .forward_raw(
            Method::POST,
            "/v1internal:streamGenerateContent?alt=sse",
            &ent_headers,
            Bytes::from_static(b"{}"),
        )
        .await
        .expect("cached enterprise stream succeeds");
    assert_eq!(stream_resp.status, StatusCode::OK);
    assert_eq!(stream_attempts.load(Ordering::SeqCst), 1);
    assert_eq!(*stream_seen_user_projects.lock().unwrap(), vec![None]);

    // 3. Separate Workspace account keeps x-goog-user-project intact (per-account isolation!).
    let mut ws_headers = HeaderMap::new();
    ws_headers.insert(
        header::AUTHORIZATION,
        HeaderValue::from_static("Bearer workspace-acct"),
    );
    ws_headers.insert(
        header::USER_AGENT,
        HeaderValue::from_static("antigravity/2.34.0 auth_method=gcp"),
    );
    ws_headers.insert(
        HeaderName::from_static("x-goog-user-project"),
        HeaderValue::from_static("ws-proj"),
    );
    let ws_fetch = client
        .forward_fetch_available_models(
            "/v1internal:fetchAvailableModels",
            &ws_headers,
            Bytes::from_static(b"{}"),
        )
        .await
        .unwrap();
    assert_eq!(ws_fetch.status, StatusCode::OK);
    let ws_stream = client
        .forward_raw(
            Method::POST,
            "/v1internal:streamGenerateContent?alt=sse",
            &ws_headers,
            Bytes::from_static(b"{}"),
        )
        .await
        .unwrap();
    assert_eq!(ws_stream.status, StatusCode::OK);
    assert_eq!(
        *stream_seen_user_projects.lock().unwrap(),
        vec![None, Some("ws-proj".to_string())]
    );

    // 4. Uncached account getting 403 on streamGenerateContent must NEVER retry or fall back to daily!
    let mut no_retry_headers = HeaderMap::new();
    no_retry_headers.insert(
        header::AUTHORIZATION,
        HeaderValue::from_static("Bearer uncached-403-acct"),
    );
    no_retry_headers.insert(
        header::USER_AGENT,
        HeaderValue::from_static("antigravity/2.34.0 auth_method=gcp"),
    );
    no_retry_headers.insert(
        HeaderName::from_static("x-goog-user-project"),
        HeaderValue::from_static("some-proj"),
    );
    let before = stream_attempts.load(Ordering::SeqCst);
    let err_stream = client
        .forward_raw(
            Method::POST,
            "/v1internal:streamGenerateContent?alt=sse",
            &no_retry_headers,
            Bytes::from_static(b"{}"),
        )
        .await
        .unwrap();
    assert_eq!(err_stream.status, StatusCode::FORBIDDEN);
    assert_eq!(
        stream_attempts.load(Ordering::SeqCst),
        before + 1,
        "generation RPC must execute at most once (zero retries)"
    );
    assert_eq!(
        daily_stream_hits.load(Ordering::SeqCst),
        0,
        "generation RPC must never fall back across hosts"
    );

    // 5. loadCodeAssist also retries ONCE on 403 without x-goog-user-project and caches strip_user_project!
    let mut lca_headers = HeaderMap::new();
    lca_headers.insert(
        header::AUTHORIZATION,
        HeaderValue::from_static("Bearer ent-lca-acct"),
    );
    lca_headers.insert(
        HeaderName::from_static("x-goog-user-project"),
        HeaderValue::from_static("corp-lca-proj"),
    );
    let lca_resp = client
        .forward_raw(
            Method::POST,
            "/v1internal:loadCodeAssist",
            &lca_headers,
            Bytes::from_static(b"{}"),
        )
        .await
        .expect("loadCodeAssist 403 retry succeeds");
    assert_eq!(lca_resp.status, StatusCode::OK);
    assert_eq!(lca_attempts.load(Ordering::SeqCst), 2);
    let lca_stream = client
        .forward_raw(
            Method::POST,
            "/v1internal:streamGenerateContent?alt=sse",
            &lca_headers,
            Bytes::from_static(b"{}"),
        )
        .await
        .expect("cached loadCodeAssist enterprise stream succeeds");
    assert_eq!(lca_stream.status, StatusCode::OK);
    assert_eq!(
        stream_seen_user_projects.lock().unwrap().last().cloned(),
        Some(None),
        "loadCodeAssist 403 retry must cache strip_user_project for subsequent forward_raw"
    );

    daily_srv.abort();
    prod_srv.abort();
}

#[tokio::test]
async fn per_account_lru_evicts_after_8_entries_and_preserves_last_resolved_prod_on_oauth_refresh()
{
    use axum::Router;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use tokio::net::TcpListener;

    // Direct LRU capacity test (8 entries max, evicts least-recently-used).
    let cache = CloudCodeRoutingCache::new();
    let fp = |tok: &str| {
        let mut h = HeaderMap::new();
        h.insert(header::AUTHORIZATION, HeaderValue::from_str(tok).unwrap());
        cache.auth_fingerprint(&h).unwrap()
    };
    for i in 0..8 {
        cache.put(
            fp(&format!("Bearer tok-{i}")),
            AccountRoute {
                use_prod: i % 2 == 0,
                strip_user_project: false,
                authoritative: true,
            },
        );
    }
    // Touch tok-0 so tok-1 becomes LRU.
    assert!(cache.get(fp("Bearer tok-0")).is_some());
    cache.put(
        fp("Bearer tok-8"),
        AccountRoute {
            use_prod: true,
            strip_user_project: true,
            authoritative: true,
        },
    );
    assert!(cache.get(fp("Bearer tok-0")).is_some());
    assert!(
        cache.get(fp("Bearer tok-1")).is_none(),
        "tok-1 should be evicted"
    );
    assert!(cache.get(fp("Bearer tok-8")).is_some());

    // End-to-end OAuth token refresh check:
    // PayGo resolves to prod -> hourly OAuth token rotates -> forward_raw still routes to prod via last_resolved_prod,
    // while a new account's fetchAvailableModels still probes daily first.
    let daily_fetch_hits = Arc::new(AtomicUsize::new(0));
    let daily_stream_hits = Arc::new(AtomicUsize::new(0));
    let prod_stream_hits = Arc::new(AtomicUsize::new(0));

    let df_c = Arc::clone(&daily_fetch_hits);
    let ds_c = Arc::clone(&daily_stream_hits);
    let mock_daily = Router::new()
        .route(
            "/v1internal:fetchAvailableModels",
            axum::routing::post(move |headers: HeaderMap| {
                let df = Arc::clone(&df_c);
                async move {
                    df.fetch_add(1, Ordering::SeqCst);
                    let auth = headers
                        .get(header::AUTHORIZATION)
                        .and_then(|v| v.to_str().ok())
                        .unwrap_or("");
                    if auth == "Bearer personal-pro-new" {
                        (
                            StatusCode::OK,
                            [(header::CONTENT_TYPE, "application/json")],
                            r#"{"models":{"pro-model":{"displayName":"Pro"}}}"#,
                        )
                    } else {
                        (
                            StatusCode::OK,
                            [(header::CONTENT_TYPE, "application/json")],
                            r#"{"models":{}}"#,
                        )
                    }
                }
            }),
        )
        .route(
            "/v1internal:streamGenerateContent",
            axum::routing::post(move || {
                let ds = Arc::clone(&ds_c);
                async move {
                    ds.fetch_add(1, Ordering::SeqCst);
                    (StatusCode::OK, "daily")
                }
            }),
        );
    let daily_listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let daily_addr = daily_listener.local_addr().unwrap();
    let daily_srv = tokio::spawn(async move {
        let _ = axum::serve(daily_listener, mock_daily).await;
    });

    let ps_c = Arc::clone(&prod_stream_hits);
    let mock_prod = Router::new()
        .route(
            "/v1internal:fetchAvailableModels",
            axum::routing::post(|| async {
                (
                    StatusCode::OK,
                    [(header::CONTENT_TYPE, "application/json")],
                    r#"{"models":{"paygo-model":{"displayName":"PayGo"}}}"#,
                )
            }),
        )
        .route(
            "/v1internal:streamGenerateContent",
            axum::routing::post(move || {
                let ps = Arc::clone(&ps_c);
                async move {
                    ps.fetch_add(1, Ordering::SeqCst);
                    (StatusCode::OK, "prod")
                }
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

    // PayGo resolves to prod.
    let mut h1 = HeaderMap::new();
    h1.insert(
        header::AUTHORIZATION,
        HeaderValue::from_static("Bearer paygo-hour-1"),
    );
    client
        .forward_fetch_available_models(
            "/v1internal:fetchAvailableModels",
            &h1,
            Bytes::from_static(b"{}"),
        )
        .await
        .unwrap();
    assert_eq!(
        client.resolved_upstream_url().as_deref(),
        Some(format!("http://{prod_addr}").as_str())
    );

    // OAuth token rotates -> forward_raw with new token (no UA auth_method) hits prod via last_resolved_prod.
    let mut h2 = HeaderMap::new();
    h2.insert(
        header::AUTHORIZATION,
        HeaderValue::from_static("Bearer paygo-hour-2"),
    );
    let r = client
        .forward_raw(
            Method::POST,
            "/v1internal:streamGenerateContent?alt=sse",
            &h2,
            Bytes::from_static(b"{}"),
        )
        .await
        .unwrap();
    assert_eq!(r.status, StatusCode::OK);
    assert_eq!(prod_stream_hits.load(Ordering::SeqCst), 1);
    assert_eq!(daily_stream_hits.load(Ordering::SeqCst), 0);

    // New personal account fetchAvailableModels still probes daily first (does NOT get stuck on last_resolved_prod).
    let mut h3 = HeaderMap::new();
    h3.insert(
        header::AUTHORIZATION,
        HeaderValue::from_static("Bearer personal-pro-new"),
    );
    let r3 = client
        .forward_fetch_available_models(
            "/v1internal:fetchAvailableModels",
            &h3,
            Bytes::from_static(b"{}"),
        )
        .await
        .unwrap();
    assert!(String::from_utf8_lossy(&r3.body).contains("pro-model"));
    assert_eq!(
        client.resolved_upstream_url().as_deref(),
        Some(format!("http://{daily_addr}").as_str())
    );
    assert_eq!(daily_fetch_hits.load(Ordering::SeqCst), 2);

    daily_srv.abort();
    prod_srv.abort();
}

#[tokio::test]
async fn allowed_request_headers_forwards_request_reason_and_content_encoding() {
    use axum::Router;
    use tokio::net::TcpListener;

    let seen = Arc::new(Mutex::new((None::<String>, None::<String>)));
    let seen_c = Arc::clone(&seen);
    let mock = Router::new().route(
        "/v1internal:streamGenerateContent",
        axum::routing::post(move |headers: HeaderMap| {
            let seen = Arc::clone(&seen_c);
            async move {
                let reason = headers
                    .get("x-goog-request-reason")
                    .and_then(|v| v.to_str().ok())
                    .map(str::to_owned);
                let ce = headers
                    .get(header::CONTENT_ENCODING)
                    .and_then(|v| v.to_str().ok())
                    .map(str::to_owned);
                *seen.lock().unwrap() = (reason, ce);
                (StatusCode::OK, "ok")
            }
        }),
    );
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let srv = tokio::spawn(async move {
        let _ = axum::serve(listener, mock).await;
    });

    let client = CloudCodeClient::new(&format!("http://{addr}"), 30).unwrap();
    let mut headers = HeaderMap::new();
    headers.insert(
        HeaderName::from_static("x-goog-request-reason"),
        HeaderValue::from_static("ide-chat"),
    );
    headers.insert(header::CONTENT_ENCODING, HeaderValue::from_static("gzip"));

    let resp = client
        .forward_raw(
            Method::POST,
            "/v1internal:streamGenerateContent",
            &headers,
            Bytes::from_static(b"raw-compressed"),
        )
        .await
        .unwrap();
    assert_eq!(resp.status, StatusCode::OK);
    assert_eq!(
        *seen.lock().unwrap(),
        (Some("ide-chat".to_string()), Some("gzip".to_string()))
    );

    srv.abort();
}

#[tokio::test]
async fn send_buffered_enforces_metadata_timeout() {
    use axum::Router;
    use tokio::net::TcpListener;

    let mock = Router::new().route(
        "/v1internal:fetchAvailableModels",
        axum::routing::post(|| async {
            tokio::time::sleep(Duration::from_secs(5)).await;
            (StatusCode::OK, r#"{"models":{}}"#)
        }),
    );
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let srv = tokio::spawn(async move {
        let _ = axum::serve(listener, mock).await;
    });

    let client = CloudCodeClient::new(&format!("http://{addr}"), 30)
        .unwrap()
        .with_buffered_timeout(Duration::from_millis(100));

    let err = client
        .forward_fetch_available_models(
            "/v1internal:fetchAvailableModels",
            &HeaderMap::new(),
            Bytes::from_static(b"{}"),
        )
        .await
        .err()
        .expect("slowloris metadata call must time out");
    assert!(
        matches!(err, CloudCodeError::Upstream(ref msg) if msg.contains("timed out")),
        "unexpected error: {err:?}"
    );

    srv.abort();
}

#[test]
fn is_loopback_url_distinguishes_loopback_from_remote_https() {
    assert!(is_loopback_url("http://127.0.0.1:4141"));
    assert!(is_loopback_url("http://localhost:4141"));
    assert!(is_loopback_url("http://LOCALHOST:4141"));
    assert!(is_loopback_url("http://[::1]:4141"));
    assert!(!is_loopback_url("https://cloudcode-pa.googleapis.com"));
    assert!(!is_loopback_url(
        "https://daily-cloudcode-pa.googleapis.com"
    ));
    assert!(!is_loopback_url("not-a-url"));
}

#[test]
fn merge_available_models_assigns_deterministic_slots_independent_of_default_model_and_adds_neon_xhigh()
 {
    let upstream = json!({
        "models": {
            "custom-existing": {
                "model": "MODEL_PLACEHOLDER_M334",
                "displayName": "Existing Custom"
            }
        },
        "agentModelSorts": [{
            "displayName": "Recommended",
            "groups": [{ "displayName": "Models", "modelIds": [] }]
        }]
    });
    let upstream_bytes = serde_json::to_vec(&upstream).unwrap();
    let proxy_models = vec![
        "gemini-4-hydrogen".to_string(),
        "gemini-4-argon".to_string(),
        "gemini-4-neon".to_string(),
        "gemini-4-carbon".to_string(),
        "gemini-3.8-flash".to_string(),
        "gemini-4-discovered-zeta".to_string(),
    ];

    let extract_slots = |default_model: &str| -> (Value, HashMap<String, String>) {
        let merged_bytes = merge_available_models(&upstream_bytes, &proxy_models, default_model)
            .expect("merge succeeds");
        let merged: Value = serde_json::from_slice(&merged_bytes).unwrap();
        let mut map = HashMap::new();
        for (k, v) in merged["models"].as_object().unwrap() {
            if k.starts_with("companion/") {
                map.insert(k.clone(), v["model"].as_str().unwrap().to_string());
            }
        }
        (merged, map)
    };

    let (m_hydrogen, slots_hydrogen) = extract_slots("gemini-4-hydrogen");
    let (_, slots_neon) = extract_slots("gemini-4-neon");
    let (_, slots_flash) = extract_slots("gemini-3.8-flash");
    let (_, slots_discovered) = extract_slots("gemini-4-discovered-zeta");

    // All slots must remain 100% identical regardless of default_model ordering!
    assert_eq!(slots_hydrogen, slots_neon);
    assert_eq!(slots_hydrogen, slots_flash);
    assert_eq!(slots_hydrogen, slots_discovered);

    // Every assigned slot must be unique and never reuse upstream M334.
    let unique: HashSet<&String> = slots_hydrogen.values().collect();
    assert_eq!(unique.len(), slots_hydrogen.len());
    assert!(!unique.contains(&"MODEL_PLACEHOLDER_M334".to_string()));
    assert_eq!(
        slots_hydrogen
            .get("companion/gemini-4-hydrogen-low")
            .map(String::as_str),
        Some("MODEL_PLACEHOLDER_M335")
    );

    // gemini-4-neon must include XHigh (thinkingLevel: 7).
    let neon_xhigh = &m_hydrogen["models"]["companion/gemini-4-neon-xhigh"];
    assert_eq!(neon_xhigh["thinkingLevel"], 7);
    assert_eq!(neon_xhigh["displayName"], "Gemini 4 Neon (Proxy) (XHigh)");
    assert_eq!(neon_xhigh["modelUrl"], "gemini-api://gemini-4-neon");
}

#[test]
fn detect_auth_method_from_headers_routes_corp_and_paygo_to_prod_and_consumer_to_daily() {
    for ua in [
        "antigravity/2.35.1 (aidev_client; auth_method=corp)",
        "antigravity/2.35.1 (aidev_client; auth_method=gcp)",
        "antigravity/2.35.1 (aidev_client; auth_method=adc)",
        "antigravity/2.35.1 (aidev_client; auth_method=paygo)",
        "antigravity/2.35.1 (aidev_client; auth_method=enterprise)",
        "antigravity/2.35.1 (aidev_client; auth_method=business)",
    ] {
        let mut h = HeaderMap::new();
        h.insert(header::USER_AGENT, HeaderValue::from_str(ua).unwrap());
        assert_eq!(
            detect_auth_method_from_headers(&h),
            Some(true),
            "expected prod for {ua}"
        );
    }

    let mut h_consumer = HeaderMap::new();
    h_consumer.insert(
        header::USER_AGENT,
        HeaderValue::from_static("antigravity/2.35.1 (aidev_client; auth_method=consumer)"),
    );
    assert_eq!(detect_auth_method_from_headers(&h_consumer), Some(false));
}

#[test]
fn merge_available_models_inherits_upstream_compaction_experiments_and_strips_vertex_fields() {
    let upstream = json!({
        "models": {
            "gemini-4-neon-high": {
                "model": "MODEL_PLACEHOLDER_M467",
                "displayName": "Gemini 4 Neon (High)",
                "vertexModelId": "gemini-4-neon",
                "maxTokens": 1048576,
                "maxOutputTokens": 65536,
                "tokenizerType": "TOKENIZER_TYPE_GEMINI",
                "supportsCumulativeContext": true,
                "supportsThoughtCirculation": true,
                "supportsDeferredToolLoading": true,
                "promptTemplaterType": "PROMPT_TEMPLATER_TYPE_GEMINI_V2",
                "modelExperimentIds": ["shared_gemini4_checkpointer"],
                "modelExperiments": {
                    "experiments": {
                        "CASCADE_USE_EXPERIMENT_CHECKPOINTER": { "stringValue": "1m-window" }
                    }
                },
                "ignoreExperimentNames": ["legacy_small_window"],
                "tagTitle": "New",
                "disabled": true,
                "isInternal": true
            }
        },
        "sharedModelExperiments": {
            "shared_gemini4_checkpointer": {
                "experiments": {
                    "context-window-options": { "stringValue": "{}" }
                }
            }
        },
        "agentModelSorts": [
            {
                "displayName": "Recommended",
                "groups": [{ "modelIds": ["gemini-4-neon-high"] }]
            }
        ]
    });
    let upstream_bytes = serde_json::to_vec(&upstream).unwrap();
    let merged_bytes = merge_available_models(
        &upstream_bytes,
        &["gemini-4-neon".to_string(), "gemini-4-new".to_string()],
        "gemini-4-neon",
    )
    .expect("merge succeeds");
    let merged: Value = serde_json::from_slice(&merged_bytes).unwrap();

    let neon_proxy = &merged["models"]["companion/gemini-4-neon-high"];
    assert_eq!(neon_proxy["displayName"], "Gemini 4 Neon (Proxy) (High)");
    assert_eq!(neon_proxy["modelUrl"], "gemini-api://gemini-4-neon");
    assert_eq!(neon_proxy["apiProvider"], "API_PROVIDER_GOOGLE_GEMINI");
    assert_eq!(neon_proxy["maxTokens"], 1048576);
    assert_eq!(neon_proxy["maxOutputTokens"], 65536);
    assert_eq!(neon_proxy["tokenizerType"], "TOKENIZER_TYPE_GEMINI");
    assert_eq!(neon_proxy["supportsCumulativeContext"], true);
    assert_eq!(neon_proxy["supportsThoughtCirculation"], true);
    assert_eq!(neon_proxy["supportsDeferredToolLoading"], true);
    assert_eq!(
        neon_proxy["promptTemplaterType"],
        "PROMPT_TEMPLATER_TYPE_GEMINI_V2"
    );
    assert_eq!(
        neon_proxy["modelExperimentIds"],
        json!(["shared_gemini4_checkpointer"])
    );
    assert_eq!(
        neon_proxy["modelExperiments"]["experiments"]["CASCADE_USE_EXPERIMENT_CHECKPOINTER"]["stringValue"],
        "1m-window"
    );
    assert_eq!(
        neon_proxy["ignoreExperimentNames"],
        json!(["legacy_small_window"])
    );
    // Upstream-only routing/UI flags must be stripped so the proxy entry routes cleanly.
    assert!(neon_proxy.get("vertexModelId").is_none());
    assert!(neon_proxy.get("tagTitle").is_none());
    assert!(neon_proxy.get("disabled").is_none());
    assert!(neon_proxy.get("isInternal").is_none());

    // Even a newly discovered Gemini 4 model inherits the Gemini 4 checkpointer experiments.
    let new_proxy = &merged["models"]["companion/gemini-4-new-medium"];
    assert_eq!(
        new_proxy["modelExperimentIds"],
        json!(["shared_gemini4_checkpointer"])
    );
    assert_eq!(new_proxy["supportsCumulativeContext"], true);
}

#[tokio::test]
async fn list_experiments_probes_prod_first_with_daily_fallback_and_caches_account_route() {
    use axum::routing::post;
    use std::sync::atomic::{AtomicUsize, Ordering};

    let daily_hits = Arc::new(AtomicUsize::new(0));
    let prod_hits = Arc::new(AtomicUsize::new(0));
    let prod_models_hits = Arc::new(AtomicUsize::new(0));

    let daily_hits_c = Arc::clone(&daily_hits);
    let daily_app = axum::Router::new().route(
        "/v1internal:listExperiments",
        post(move || {
            let hits = Arc::clone(&daily_hits_c);
            async move {
                hits.fetch_add(1, Ordering::SeqCst);
                (
                    StatusCode::OK,
                    r#"{"flags":[{"name":"enable-mcp-apps","boolValue":true}]}"#,
                )
            }
        }),
    );
    let prod_hits_c = Arc::clone(&prod_hits);
    let prod_models_hits_c = Arc::clone(&prod_models_hits);
    let prod_app = axum::Router::new()
        .route(
            "/v1internal:listExperiments",
            post(move |headers: HeaderMap| {
                let hits = Arc::clone(&prod_hits_c);
                async move {
                    hits.fetch_add(1, Ordering::SeqCst);
                    let is_consumer = headers
                        .get(header::AUTHORIZATION)
                        .and_then(|v| v.to_str().ok())
                        .is_some_and(|v| v.contains("consumer-token"));
                    if is_consumer {
                        (StatusCode::FORBIDDEN, r#"{"error":"use daily"}"#)
                    } else {
                        (
                            StatusCode::OK,
                            r#"{"flags":[{"name":"enable-mcp-apps","boolValue":true}]}"#,
                        )
                    }
                }
            }),
        )
        .route(
            "/v1internal:fetchAvailableModels",
            post(move || {
                let hits = Arc::clone(&prod_models_hits_c);
                async move {
                    hits.fetch_add(1, Ordering::SeqCst);
                    (
                        StatusCode::OK,
                        r#"{"models":{"gemini-4-neon-high":{"model":"MODEL_PLACEHOLDER_M467"}}}"#,
                    )
                }
            }),
        );

    let daily_listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let daily_addr = daily_listener.local_addr().unwrap();
    let daily_server = tokio::spawn(async move {
        let _ = axum::serve(daily_listener, daily_app).await;
    });

    let prod_listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let prod_addr = prod_listener.local_addr().unwrap();
    let prod_server = tokio::spawn(async move {
        let _ = axum::serve(prod_listener, prod_app).await;
    });

    let client = CloudCodeClient::new_auto_with_urls(
        &format!("http://{daily_addr}"),
        &format!("http://{prod_addr}"),
        30,
    )
    .unwrap();

    let mut paygo_headers = HeaderMap::new();
    paygo_headers.insert(
        header::AUTHORIZATION,
        HeaderValue::from_static("Bearer paygo-token-1"),
    );
    paygo_headers.insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("application/json"),
    );

    let exp_resp = client
        .forward_raw(
            Method::POST,
            "/v1internal:listExperiments",
            &paygo_headers,
            Bytes::from_static(br#"{"project":"weisun-cai"}"#),
        )
        .await
        .unwrap();
    assert_eq!(exp_resp.status, StatusCode::OK);
    assert_eq!(prod_hits.load(Ordering::SeqCst), 1);
    assert_eq!(daily_hits.load(Ordering::SeqCst), 0);
    assert_eq!(
        client.resolved_upstream_url().as_deref(),
        Some(format!("http://{prod_addr}").as_str())
    );

    // Subsequent fetchAvailableModels for the same PayGo account goes directly to cached prod.
    let models_resp = client
        .forward_fetch_available_models(
            "/v1internal:fetchAvailableModels",
            &paygo_headers,
            Bytes::from_static(br#"{"project":"weisun-cai"}"#),
        )
        .await
        .unwrap();
    assert_eq!(models_resp.status, StatusCode::OK);
    assert_eq!(prod_models_hits.load(Ordering::SeqCst), 1);

    // Consumer account rejected on prod falls back cleanly to daily and caches daily.
    let mut consumer_headers = HeaderMap::new();
    consumer_headers.insert(
        header::AUTHORIZATION,
        HeaderValue::from_static("Bearer consumer-token-1"),
    );
    consumer_headers.insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("application/json"),
    );
    let consumer_resp = client
        .forward_raw(
            Method::POST,
            "/v1internal:listExperiments",
            &consumer_headers,
            Bytes::from_static(br#"{}"#),
        )
        .await
        .unwrap();
    assert_eq!(consumer_resp.status, StatusCode::OK);
    assert_eq!(daily_hits.load(Ordering::SeqCst), 1);
    assert_eq!(
        client.resolved_upstream_url().as_deref(),
        Some(format!("http://{daily_addr}").as_str())
    );

    daily_server.abort();
    prod_server.abort();
}

#[test]
fn merge_available_models_orders_proxy_models_from_most_to_least_capable() {
    let upstream = json!({
        "models": {
            "gemini-4-carbon-medium": {
                "model": "MODEL_PLACEHOLDER_M460",
                "displayName": "Gemini 4 Carbon (Medium)"
            }
        },
        "agentModelSorts": [
            {
                "displayName": "Recommended",
                "groups": [{ "modelIds": ["gemini-4-carbon-medium"] }]
            }
        ]
    });
    let upstream_bytes = serde_json::to_vec(&upstream).unwrap();
    let merged_bytes = merge_available_models(
        &upstream_bytes,
        &[
            "gemini-3.8-flash".to_string(),
            "gemini-4-argon".to_string(),
            "gemini-4-hydrogen".to_string(),
            "gemini-4-neon".to_string(),
            "gemini-4-carbon".to_string(),
        ],
        "gemini-4-hydrogen",
    )
    .expect("merge succeeds");
    let merged: Value = serde_json::from_slice(&merged_bytes).unwrap();
    let model_ids: Vec<&str> = merged["agentModelSorts"][0]["groups"][0]["modelIds"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(Value::as_str)
        .collect();

    // Extract distinct proxy base model order after the upstream model.
    let mut proxy_base_order: Vec<&str> = Vec::new();
    for id in model_ids.iter().skip(1) {
        let base = id
            .strip_prefix("companion/")
            .unwrap()
            .rsplit_once('-')
            .unwrap()
            .0;
        if proxy_base_order.last().copied() != Some(base) {
            proxy_base_order.push(base);
        }
    }
    assert_eq!(
        proxy_base_order,
        vec![
            "gemini-4-carbon",
            "gemini-4-neon",
            "gemini-4-hydrogen",
            "gemini-4-argon",
            "gemini-3.8-flash",
        ]
    );
}

#[tokio::test]
async fn speculative_daily_fetch_models_never_downgrades_paygo_list_experiments() {
    use axum::routing::post;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use tokio::sync::Notify;

    let daily_exp_hits = Arc::new(AtomicUsize::new(0));
    let prod_exp_hits = Arc::new(AtomicUsize::new(0));
    let daily_models_started = Arc::new(Notify::new());
    let allow_daily_models_reply = Arc::new(Notify::new());

    let daily_exp_hits_c = Arc::clone(&daily_exp_hits);
    let daily_models_started_c = Arc::clone(&daily_models_started);
    let allow_daily_models_reply_c = Arc::clone(&allow_daily_models_reply);
    let daily_app = axum::Router::new()
        .route(
            "/v1internal:listExperiments",
            post(move || {
                let hits = Arc::clone(&daily_exp_hits_c);
                async move {
                    hits.fetch_add(1, Ordering::SeqCst);
                    (
                        StatusCode::OK,
                        r#"{"flags":[{"name":"enable-mcp-apps","boolValue":false}]}"#,
                    )
                }
            }),
        )
        .route(
            "/v1internal:fetchAvailableModels",
            post(move |headers: HeaderMap| {
                let started = Arc::clone(&daily_models_started_c);
                let allow = Arc::clone(&allow_daily_models_reply_c);
                async move {
                    let is_race = headers
                        .get(header::AUTHORIZATION)
                        .and_then(|v| v.to_str().ok())
                        .is_some_and(|v| v.contains("paygo-race"));
                    if is_race {
                        started.notify_one();
                        allow.notified().await;
                    }
                    (
                        StatusCode::OK,
                        r#"{"models":{"gemini-4-carbon-high":{"model":"MODEL_PLACEHOLDER_M460"}}}"#,
                    )
                }
            }),
        );

    let prod_exp_hits_c = Arc::clone(&prod_exp_hits);
    let prod_app = axum::Router::new()
        .route(
            "/v1internal:listExperiments",
            post(move || {
                let hits = Arc::clone(&prod_exp_hits_c);
                async move {
                    hits.fetch_add(1, Ordering::SeqCst);
                    (
                        StatusCode::OK,
                        r#"{"flags":[{"name":"enable-mcp-apps","boolValue":true}]}"#,
                    )
                }
            }),
        )
        .route(
            "/v1internal:loadCodeAssist",
            post(move || async {
                (
                    StatusCode::OK,
                    r#"{"currentTier":{"id":"g1-pro-tier","usesGcpTos":false}}"#,
                )
            }),
        )
        .route(
            "/v1internal:fetchAvailableModels",
            post(move || async {
                (
                    StatusCode::OK,
                    r#"{"models":{"gemini-4-carbon-high":{"model":"MODEL_PLACEHOLDER_M460"}}}"#,
                )
            }),
        );

    let daily_listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let daily_addr = daily_listener.local_addr().unwrap();
    let daily_server = tokio::spawn(async move {
        let _ = axum::serve(daily_listener, daily_app).await;
    });

    let prod_listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let prod_addr = prod_listener.local_addr().unwrap();
    let prod_server = tokio::spawn(async move {
        let _ = axum::serve(prod_listener, prod_app).await;
    });

    let client = CloudCodeClient::new_auto_with_urls(
        &format!("http://{daily_addr}"),
        &format!("http://{prod_addr}"),
        30,
    )
    .unwrap();

    // Case A: Startup race — fetchAvailableModels starts with empty cache (hits daily),
    // listExperiments completes on prod first, then fetchAvailableModels completes on daily.
    // The speculative daily 200 OK must NOT downgrade the PayGo account to daily.
    let mut race_headers = HeaderMap::new();
    race_headers.insert(
        header::AUTHORIZATION,
        HeaderValue::from_static("Bearer paygo-race-token"),
    );
    race_headers.insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("application/json"),
    );

    let client_clone = client.clone();
    let race_headers_clone = race_headers.clone();
    let fetch_task = tokio::spawn(async move {
        client_clone
            .forward_fetch_available_models(
                "/v1internal:fetchAvailableModels",
                &race_headers_clone,
                Bytes::from_static(br#"{"project":"weisun-cai"}"#),
            )
            .await
            .unwrap()
    });
    daily_models_started.notified().await;

    let exp1 = client
        .forward_raw(
            Method::POST,
            "/v1internal:listExperiments",
            &race_headers,
            Bytes::from_static(br#"{"project":"weisun-cai"}"#),
        )
        .await
        .unwrap();
    assert_eq!(exp1.status, StatusCode::OK);
    assert_eq!(prod_exp_hits.load(Ordering::SeqCst), 1);
    assert_eq!(daily_exp_hits.load(Ordering::SeqCst), 0);

    allow_daily_models_reply.notify_one();
    let _ = fetch_task.await.unwrap();

    // Next 60s listExperiments poll MUST remain on prod (never downgraded to daily).
    let exp2 = client
        .forward_raw(
            Method::POST,
            "/v1internal:listExperiments",
            &race_headers,
            Bytes::from_static(br#"{"project":"weisun-cai"}"#),
        )
        .await
        .unwrap();
    assert_eq!(exp2.status, StatusCode::OK);
    assert_eq!(prod_exp_hits.load(Ordering::SeqCst), 2);
    assert_eq!(daily_exp_hits.load(Ordering::SeqCst), 0);
    assert_eq!(
        client.resolved_upstream_url().as_deref(),
        Some(format!("http://{prod_addr}").as_str())
    );

    // Case B: Reverse order — speculative daily fetchAvailableModels completes FIRST,
    // then listExperiments runs. listExperiments must ignore non-authoritative daily
    // and still probe prod first so PayGo keeps official prod Mendel flags.
    let mut reverse_headers = HeaderMap::new();
    reverse_headers.insert(
        header::AUTHORIZATION,
        HeaderValue::from_static("Bearer paygo-reverse-token"),
    );
    reverse_headers.insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("application/json"),
    );
    let _ = client
        .forward_fetch_available_models(
            "/v1internal:fetchAvailableModels",
            &reverse_headers,
            Bytes::from_static(br#"{"project":"weisun-cai"}"#),
        )
        .await
        .unwrap();
    let exp3 = client
        .forward_raw(
            Method::POST,
            "/v1internal:listExperiments",
            &reverse_headers,
            Bytes::from_static(br#"{"project":"weisun-cai"}"#),
        )
        .await
        .unwrap();
    assert_eq!(exp3.status, StatusCode::OK);
    assert_eq!(prod_exp_hits.load(Ordering::SeqCst), 3);
    assert_eq!(daily_exp_hits.load(Ordering::SeqCst), 0);

    // Case C: Authoritative consumer resolution via loadCodeAssist (usesGcpTos: false)
    // DOES route subsequent listExperiments directly to daily.
    let mut pro_headers = HeaderMap::new();
    pro_headers.insert(
        header::AUTHORIZATION,
        HeaderValue::from_static("Bearer consumer-pro-token"),
    );
    pro_headers.insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("application/json"),
    );
    let _ = client
        .forward_raw(
            Method::POST,
            "/v1internal:loadCodeAssist",
            &pro_headers,
            Bytes::from_static(br#"{}"#),
        )
        .await
        .unwrap();
    let exp4 = client
        .forward_raw(
            Method::POST,
            "/v1internal:listExperiments",
            &pro_headers,
            Bytes::from_static(br#"{}"#),
        )
        .await
        .unwrap();
    assert_eq!(exp4.status, StatusCode::OK);
    assert_eq!(daily_exp_hits.load(Ordering::SeqCst), 1);
    assert_eq!(prod_exp_hits.load(Ordering::SeqCst), 3);

    daily_server.abort();
    prod_server.abort();
}

#[test]
fn merge_available_models_accepts_gemini_5_and_3_9_with_template_inheritance_and_rejects_bare_gemini_5()
 {
    let upstream = serde_json::to_vec(&json!({
        "models": {
            "gemini-5-pro-high": {
                "model": "MODEL_PLACEHOLDER_M501",
                "displayName": "Gemini 5 Pro (High)",
                "thinkingLevel": 3,
                "supportsCumulativeContext": true,
                "modelExperimentIds": ["exp-gemini-5-checkpointer"]
            }
        },
        "defaultAgentModelId": "gemini-5-pro-high",
        "agentModelSorts": [{
            "displayName": "Recommended",
            "groups": [{
                "displayName": "Google AI Pro",
                "modelIds": ["gemini-5-pro-high"]
            }]
        }],
        "groupedModels": {}
    }))
    .unwrap();

    let proxy_models = vec![
        "gemini-5-pro".to_string(),
        "gemini-3.9-pro".to_string(),
        "gemini-5".to_string(),
    ];
    let merged_bytes = merge_available_models(&upstream, &proxy_models, "gemini-5-pro").unwrap();
    let merged: Value = serde_json::from_slice(&merged_bytes).unwrap();
    let models = merged["models"].as_object().expect("models object");

    // gemini-5-pro & gemini-3.9-pro must be injected; bare gemini-5 must be excluded.
    assert!(
        models.contains_key("companion/gemini-5-pro-high"),
        "expected companion/gemini-5-pro-high in merged models"
    );
    assert!(
        models.contains_key("companion/gemini-3.9-pro-high"),
        "expected companion/gemini-3.9-pro-high in merged models"
    );
    assert!(
        !models
            .keys()
            .any(|k| k == "companion/gemini-5" || k == "companion/gemini-5-high"),
        "bare gemini-5 must not be injected into merged catalog"
    );

    // Verify template inheritance (step 4 fallback via is_supported_gemini_model) worked for
    // gemini-3.9-pro even when upstream only carried a gemini-5-pro-high template.
    let inherited = &models["companion/gemini-3.9-pro-high"];
    assert_eq!(inherited["supportsCumulativeContext"], json!(true));
    assert_eq!(
        inherited["modelExperimentIds"],
        json!(["exp-gemini-5-checkpointer"])
    );
}

#[test]
fn merge_available_models_reserves_default_model_under_slot_pressure_and_sorts_display_order() {
    let make_upstream_with_occupied = |end_slot: u32| -> Vec<u8> {
        let mut models = Map::new();
        for slot in 334..=end_slot {
            models.insert(
                format!("upstream-model-{slot}"),
                json!({
                    "model": format!("MODEL_PLACEHOLDER_M{slot}"),
                    "displayName": format!("Upstream {slot}")
                }),
            );
        }
        serde_json::to_vec(&json!({
            "models": Value::Object(models),
            "agentModelSorts": [{
                "displayName": "Recommended",
                "groups": [{
                    "displayName": "Models",
                    "modelIds": ["upstream-model-334"]
                }]
            }]
        }))
        .unwrap()
    };

    let proxy_models = vec![
        "gemini-4-carbon".to_string(),
        "gemini-4-neon".to_string(),
        "gemini-4-hydrogen".to_string(),
        "gemini-4-argon".to_string(),
        "gemini-3.8-flash".to_string(),
    ];

    // Subcase 1: 26 occupied slots (M334..=M359, 6 free), default_model = gemini-3.8-flash.
    // default_model must be reserved first, then gemini-4-carbon fills remaining 3 slots,
    // and picker display order still places gemini-4-carbon before gemini-3.8-flash.
    let upstream_26 = make_upstream_with_occupied(359);
    let merged_bytes =
        merge_available_models(&upstream_26, &proxy_models, "gemini-3.8-flash").unwrap();
    let merged: Value = serde_json::from_slice(&merged_bytes).unwrap();
    let models = merged["models"].as_object().expect("models object");
    let expected_subcase1 = [
        "companion/gemini-4-carbon-low",
        "companion/gemini-4-carbon-medium",
        "companion/gemini-4-carbon-high",
        "companion/gemini-3.8-flash-low",
        "companion/gemini-3.8-flash-medium",
        "companion/gemini-3.8-flash-high",
    ];
    let mut assigned_enums = HashSet::new();
    for key in expected_subcase1 {
        let enum_val = models
            .get(key)
            .and_then(|v| v.get("model"))
            .and_then(Value::as_str)
            .unwrap_or_else(|| panic!("expected {key} to be injected"));
        assert!(
            (360..=365).any(|n| enum_val == format!("MODEL_PLACEHOLDER_M{n}")),
            "unexpected enum {enum_val} for {key}"
        );
        assert!(
            assigned_enums.insert(enum_val.to_string()),
            "duplicate enum {enum_val}"
        );
    }
    assert_eq!(assigned_enums.len(), 6);
    assert!(!models.contains_key("companion/gemini-4-neon-low"));
    assert!(!models.contains_key("companion/gemini-4-hydrogen-low"));
    assert!(!models.contains_key("companion/gemini-4-argon-low"));

    let sort_ids: Vec<&str> = merged["agentModelSorts"][0]["groups"][0]["modelIds"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(Value::as_str)
        .collect();
    let injected_order: Vec<&str> = sort_ids
        .into_iter()
        .filter(|id| id.starts_with("companion/"))
        .collect();
    assert_eq!(injected_order, expected_subcase1);

    // Subcase 2: 26 occupied slots (6 free), default_model = gemini-4-carbon.
    // 4-variant neon/hydrogen must be skipped via `continue` so 3-variant argon fills
    // the remaining 3 slots, with display order carbon -> argon.
    let merged2_bytes =
        merge_available_models(&upstream_26, &proxy_models, "gemini-4-carbon").unwrap();
    let merged2: Value = serde_json::from_slice(&merged2_bytes).unwrap();
    let models2 = merged2["models"].as_object().expect("models object");
    let expected_subcase2 = [
        "companion/gemini-4-carbon-low",
        "companion/gemini-4-carbon-medium",
        "companion/gemini-4-carbon-high",
        "companion/gemini-4-argon-low",
        "companion/gemini-4-argon-medium",
        "companion/gemini-4-argon-high",
    ];
    for key in expected_subcase2 {
        assert!(
            models2.contains_key(key),
            "expected {key} to be injected when 4-variant models are skipped"
        );
    }
    assert!(!models2.contains_key("companion/gemini-4-neon-low"));
    assert!(!models2.contains_key("companion/gemini-4-hydrogen-low"));
    assert!(!models2.contains_key("companion/gemini-3.8-flash-low"));
    let injected_order2: Vec<&str> = merged2["agentModelSorts"][0]["groups"][0]["modelIds"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(Value::as_str)
        .filter(|id| id.starts_with("companion/"))
        .collect();
    assert_eq!(injected_order2, expected_subcase2);

    // Subcase 3: 29 occupied slots (M334..=M362, 3 free), default_model = gemini-3.8-flash.
    // Only gemini-3.8-flash-{low,medium,high} should be injected using M363..=M365.
    let upstream_29 = make_upstream_with_occupied(362);
    let merged3_bytes =
        merge_available_models(&upstream_29, &proxy_models, "gemini-3.8-flash").unwrap();
    let merged3: Value = serde_json::from_slice(&merged3_bytes).unwrap();
    let models3 = merged3["models"].as_object().expect("models object");
    let injected_keys3: Vec<&str> = models3
        .keys()
        .map(String::as_str)
        .filter(|k| k.starts_with("companion/"))
        .collect();
    assert_eq!(injected_keys3.len(), 3);
    let mut enums3 = HashSet::new();
    for suffix in ["low", "medium", "high"] {
        let key = format!("companion/gemini-3.8-flash-{suffix}");
        let enum_val = models3
            .get(&key)
            .and_then(|v| v.get("model"))
            .and_then(Value::as_str)
            .unwrap_or_else(|| panic!("expected {key} to be injected"));
        assert!(
            (363..=365).any(|n| enum_val == format!("MODEL_PLACEHOLDER_M{n}")),
            "unexpected enum {enum_val} for {key}"
        );
        assert!(enums3.insert(enum_val.to_string()));
    }
    assert_eq!(enums3.len(), 3);
}

#[test]
fn merge_available_models_inherits_gemini_3_9_template_for_gemini_5_model() {
    let upstream = serde_json::to_vec(&json!({
        "models": {
            "gemini-3.9-pro-high": {
                "model": "MODEL_PLACEHOLDER_M502",
                "displayName": "Gemini 3.9 Pro (High)",
                "thinkingLevel": 3,
                "supportsCumulativeContext": true,
                "modelExperimentIds": ["exp-gemini-39-checkpointer"]
            }
        },
        "agentModelSorts": [{
            "displayName": "Recommended",
            "groups": [{
                "displayName": "Google AI Pro",
                "modelIds": ["gemini-3.9-pro-high"]
            }]
        }],
        "groupedModels": {}
    }))
    .unwrap();

    let proxy_models = vec!["gemini-5-alpha".to_string()];
    let merged_bytes = merge_available_models(&upstream, &proxy_models, "gemini-5-alpha").unwrap();
    let merged: Value = serde_json::from_slice(&merged_bytes).unwrap();
    let models = merged["models"].as_object().expect("models object");
    let inherited = models
        .get("companion/gemini-5-alpha-high")
        .expect("expected companion/gemini-5-alpha-high");
    assert_eq!(inherited["supportsCumulativeContext"], json!(true));
    assert_eq!(
        inherited["modelExperimentIds"],
        json!(["exp-gemini-39-checkpointer"])
    );
}

#[test]
fn validate_cloudcode_path_enforces_rfc3986_percent_decoding_and_preserves_colons() {
    for good in [
        "/v1internal:fetchAvailableModels",
        "/v1internal:streamGenerateContent?alt=sse",
        "/v1internal/foo:bar",
        "/v1internal/foo%2Dbar",
        "/v1internal:loadCodeAssist?$alt=json%3Benum-encoding%3Dint",
    ] {
        assert!(
            validate_cloudcode_path(good).is_ok(),
            "expected {good:?} to be accepted"
        );
    }

    for bad in [
        "/v1internal/%252e%252e/secret",
        "/v1internal/%2",
        "/v1internal/%zz",
        "/v1internal/foo%3bbar",
        "/v1internal/foo;bar",
        "/v1internal/foo%00bar",
        "/v1internal/foo%0d%0abar",
        "/v1internal/foo%20bar",
        "/v1internal/%2e/secret",
        "/v1internal/.",
        "/v1internal:fetchAvailableModels?alt=%2",
        "/v1internal:fetchAvailableModels?alt=%zz",
        "/v1internal:fetchAvailableModels?alt=%00",
    ] {
        assert!(
            validate_cloudcode_path(bad).is_err(),
            "expected {bad:?} to be rejected"
        );
    }
}

#[tokio::test]
async fn cloudcode_transport_errors_strip_upstream_url_and_query() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    drop(listener);

    let client = CloudCodeClient::new(&format!("http://{addr}"), 5).unwrap();
    let headers = HeaderMap::new();

    let fetch_err = client
        .forward_fetch_available_models(
            "/v1internal:fetchAvailableModels?secret_token=topsecret123",
            &headers,
            Bytes::from_static(br#"{}"#),
        )
        .await
        .err()
        .expect("expected closed upstream to return Err");
    let raw_err = client
        .forward_raw(
            Method::POST,
            "/v1internal:streamGenerateContent?secret_token=topsecret123",
            &headers,
            Bytes::from_static(br#"{}"#),
        )
        .await
        .err()
        .expect("expected closed upstream to return Err");

    for (label, err) in [("fetch", fetch_err), ("raw", raw_err)] {
        let CloudCodeError::Upstream(msg) = err else {
            panic!("expected CloudCodeError::Upstream for {label}, got {err:?}");
        };
        for forbidden in [
            addr.to_string().as_str(),
            "127.0.0.1",
            "secret_token",
            "topsecret123",
            "fetchAvailableModels",
            "streamGenerateContent",
        ] {
            assert!(
                !msg.contains(forbidden),
                "{label} error leaked {forbidden:?} in {msg:?}"
            );
        }
    }
}

#[test]
fn merge_available_models_empty_proxy_models_injects_zero_models_even_with_valid_default_model() {
    let upstream = serde_json::to_vec(&json!({
        "models": {
            "gemini-4-carbon-high": {
                "model": "MODEL_PLACEHOLDER_M427",
                "displayName": "Gemini 4 Carbon (High)",
                "thinkingLevel": 3
            }
        },
        "defaultAgentModelId": "gemini-4-carbon-high"
    }))
    .unwrap();

    let merged = merge_available_models(&upstream, &[], "gemini-4-hydrogen").unwrap();
    let parsed: Value = serde_json::from_slice(&merged).unwrap();
    let models = parsed["models"].as_object().unwrap();
    assert!(
        !models.keys().any(|k| k.starts_with("companion/")),
        "empty enabled_models must inject 0 companion/ models: {models:?}"
    );
    assert!(parsed.get("groupedModels").is_none());
    assert!(parsed.get("agentModelSorts").is_none());
}

#[test]
fn merge_available_models_does_not_force_default_model_when_omitted_from_enabled_models() {
    let upstream = serde_json::to_vec(&json!({
        "models": {
            "gemini-4-carbon-high": {
                "model": "MODEL_PLACEHOLDER_M427",
                "displayName": "Gemini 4 Carbon (High)",
                "thinkingLevel": 3
            }
        },
        "defaultAgentModelId": "gemini-4-carbon-high"
    }))
    .unwrap();

    let merged = merge_available_models(
        &upstream,
        &["gemini-4-carbon".to_string()],
        "gemini-4-hydrogen",
    )
    .unwrap();
    let parsed: Value = serde_json::from_slice(&merged).unwrap();
    let injected: Vec<&String> = parsed["models"]
        .as_object()
        .unwrap()
        .keys()
        .filter(|k| k.starts_with("companion/"))
        .collect();
    assert_eq!(
        injected.len(),
        3,
        "only Carbon's 3 variants should exist: {injected:?}"
    );
    for suffix in ["low", "medium", "high"] {
        assert!(
            parsed["models"]
                .as_object()
                .unwrap()
                .contains_key(&format!("companion/gemini-4-carbon-{suffix}"))
        );
    }
    assert!(
        !injected.iter().any(|k| k.contains("gemini-4-hydrogen")),
        "default_model omitted from enabled_models must not be injected: {injected:?}"
    );
}

#[test]
fn merge_available_models_default_three_enabled_models_use_11_of_32_slots_in_capability_order() {
    let upstream = serde_json::to_vec(&json!({
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
            "groups": [{ "modelIds": ["gemini-4-carbon-high"] }]
        }]
    }))
    .unwrap();

    let enabled = companion_core::default_enabled_gemini_models();
    let merged = merge_available_models(&upstream, &enabled, "gemini-4-hydrogen").unwrap();
    let parsed: Value = serde_json::from_slice(&merged).unwrap();
    let models = parsed["models"].as_object().unwrap();

    let mut assigned_slots: Vec<String> = models
        .iter()
        .filter(|(k, _)| k.starts_with("companion/"))
        .map(|(_, v)| v["model"].as_str().unwrap().to_string())
        .collect();
    assigned_slots.sort();

    assert_eq!(assigned_slots.len(), 11, "default 3 models use 11/32 slots");
    assert_eq!(
        assigned_slots,
        vec![
            "MODEL_PLACEHOLDER_M334",
            "MODEL_PLACEHOLDER_M335",
            "MODEL_PLACEHOLDER_M336",
            "MODEL_PLACEHOLDER_M337",
            "MODEL_PLACEHOLDER_M341",
            "MODEL_PLACEHOLDER_M342",
            "MODEL_PLACEHOLDER_M343",
            "MODEL_PLACEHOLDER_M344",
            "MODEL_PLACEHOLDER_M345",
            "MODEL_PLACEHOLDER_M346",
            "MODEL_PLACEHOLDER_M347",
        ]
    );
    assert!(
        !models
            .keys()
            .any(|k| k.contains("argon") || k.contains("3.8-flash")),
        "disabled curated models must not be injected"
    );

    let sort_ids: Vec<&str> = parsed["agentModelSorts"][0]["groups"][0]["modelIds"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(Value::as_str)
        .filter(|id| id.starts_with("companion/"))
        .collect();
    let first_carbon = sort_ids
        .iter()
        .position(|id| id.contains("carbon"))
        .unwrap();
    let first_neon = sort_ids.iter().position(|id| id.contains("neon")).unwrap();
    let first_hydrogen = sort_ids
        .iter()
        .position(|id| id.contains("hydrogen"))
        .unwrap();
    assert!(
        first_carbon < first_neon && first_neon < first_hydrogen,
        "expected Carbon -> Neon -> Hydrogen order, got {sort_ids:?}"
    );
}
