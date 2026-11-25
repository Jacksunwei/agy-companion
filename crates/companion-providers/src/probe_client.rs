use std::fmt;
use std::time::Duration;

use companion_core::protocol::{HealthResponse, StatsSnapshot, sanitize_health_token};
use serde::de::DeserializeOwned;
use thiserror::Error;

pub const MAX_PROBE_BODY_BYTES: usize = 64 * 1024;
pub const DEFAULT_PROBE_TIMEOUT: Duration = Duration::from_secs(2);

#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum ProbeError {
    #[error("failed to build loopback HTTP probe client")]
    ClientBuild,
    #[error("proxy {endpoint} request timed out")]
    Timeout { endpoint: &'static str },
    #[error("proxy {endpoint} unreachable (connection failed)")]
    Connect { endpoint: &'static str },
    #[error("proxy {endpoint} redirect rejected")]
    Redirect { endpoint: &'static str },
    #[error("proxy {endpoint} request failed")]
    Transport { endpoint: &'static str },
    #[error("proxy {endpoint} returned HTTP {status}")]
    UnexpectedStatus { endpoint: &'static str, status: u16 },
    #[error("proxy {endpoint} response exceeded {max_bytes}-byte size limit")]
    BodyTooLarge {
        endpoint: &'static str,
        max_bytes: usize,
    },
    #[error("proxy {endpoint} returned invalid JSON")]
    InvalidJson { endpoint: &'static str },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HealthEvaluation {
    pub status_ok: bool,
    pub version_skew: bool,
    pub protocol_skew: bool,
    pub health_message: String,
    pub version_message: String,
}

pub fn evaluate_health(
    health: &HealthResponse,
    expected_version: &str,
    expected_protocol: u32,
) -> HealthEvaluation {
    let status_ok = health.status == "ok";
    let version_skew = health.version != expected_version;
    let protocol_skew = health.protocol_version != expected_protocol;

    let safe_status = sanitize_health_token(&health.status);
    let safe_version = sanitize_health_token(&health.version);

    let health_message = if status_ok {
        format!(
            "proxy /health responded ok (version {safe_version}, protocol {})",
            health.protocol_version
        )
    } else {
        format!("proxy /health reported non-ok status '{safe_status}'")
    };

    let version_message = match (version_skew, protocol_skew) {
        (false, false) => format!(
            "binary v{expected_version} matches proxy v{safe_version} (protocol v{expected_protocol})"
        ),
        (true, true) => format!(
            "version skew and protocol skew: binary v{expected_version} (protocol v{expected_protocol}) vs proxy v{safe_version} (protocol v{})",
            health.protocol_version
        ),
        (true, false) => {
            format!("version skew: binary v{expected_version} vs proxy v{safe_version}")
        }
        (false, true) => format!(
            "protocol skew: expected protocol v{expected_protocol} vs proxy protocol v{}",
            health.protocol_version
        ),
    };

    HealthEvaluation {
        status_ok,
        version_skew,
        protocol_skew,
        health_message,
        version_message,
    }
}

#[derive(Clone)]
pub struct LoopbackProxyClient {
    client: reqwest::Client,
    default_timeout: Duration,
}

impl fmt::Debug for LoopbackProxyClient {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("LoopbackProxyClient")
            .field("default_timeout", &self.default_timeout)
            .finish_non_exhaustive()
    }
}

impl LoopbackProxyClient {
    pub fn new(default_timeout: Duration, connect_timeout: Duration) -> Result<Self, ProbeError> {
        let client = reqwest::Client::builder()
            .no_proxy()
            .redirect(reqwest::redirect::Policy::none())
            .timeout(default_timeout)
            .connect_timeout(connect_timeout)
            .user_agent(concat!("agy-companion/", env!("CARGO_PKG_VERSION")))
            .build()
            .map_err(|_| ProbeError::ClientBuild)?;

        Ok(Self {
            client,
            default_timeout,
        })
    }

    pub fn with_default_timeouts() -> Result<Self, ProbeError> {
        Self::new(DEFAULT_PROBE_TIMEOUT, DEFAULT_PROBE_TIMEOUT)
    }

    async fn read_bounded_json<T: DeserializeOwned>(
        &self,
        port: u16,
        endpoint: &'static str,
        timeout_override: Option<Duration>,
    ) -> Result<T, ProbeError> {
        let effective_timeout = timeout_override.unwrap_or(self.default_timeout);
        if effective_timeout.is_zero() {
            return Err(ProbeError::Timeout { endpoint });
        }

        let url = format!("http://127.0.0.1:{port}{endpoint}");
        let mut req = self.client.get(&url);
        if let Some(timeout) = timeout_override {
            req = req.timeout(timeout);
        }

        let mut resp = req
            .send()
            .await
            .map_err(|err| sanitize_transport_error(endpoint, &err))?;

        if resp.status() != reqwest::StatusCode::OK {
            return Err(ProbeError::UnexpectedStatus {
                endpoint,
                status: resp.status().as_u16(),
            });
        }

        if resp
            .content_length()
            .is_some_and(|len| len > MAX_PROBE_BODY_BYTES as u64)
        {
            return Err(ProbeError::BodyTooLarge {
                endpoint,
                max_bytes: MAX_PROBE_BODY_BYTES,
            });
        }

        let mut body = Vec::new();
        let mut total_bytes: usize = 0;
        while let Some(chunk) = resp
            .chunk()
            .await
            .map_err(|err| sanitize_transport_error(endpoint, &err))?
        {
            total_bytes = total_bytes.saturating_add(chunk.len());
            if total_bytes > MAX_PROBE_BODY_BYTES {
                return Err(ProbeError::BodyTooLarge {
                    endpoint,
                    max_bytes: MAX_PROBE_BODY_BYTES,
                });
            }
            body.extend_from_slice(&chunk);
        }

        serde_json::from_slice::<T>(&body).map_err(|_| ProbeError::InvalidJson { endpoint })
    }

    pub async fn probe_health(
        &self,
        port: u16,
        timeout_override: Option<Duration>,
    ) -> Result<HealthResponse, ProbeError> {
        self.read_bounded_json(port, "/health", timeout_override)
            .await
    }

    pub async fn probe_healthy_companion(
        &self,
        port: u16,
        timeout_override: Option<Duration>,
    ) -> Option<HealthResponse> {
        let health = self.probe_health(port, timeout_override).await.ok()?;
        if health.status == "ok" && health.protocol_version > 0 {
            Some(health)
        } else {
            None
        }
    }

    pub async fn probe_stats(
        &self,
        port: u16,
        timeout_override: Option<Duration>,
    ) -> Result<StatsSnapshot, ProbeError> {
        self.read_bounded_json(port, "/api/stats", timeout_override)
            .await
    }
}

fn sanitize_transport_error(endpoint: &'static str, err: &reqwest::Error) -> ProbeError {
    if err.is_timeout() {
        ProbeError::Timeout { endpoint }
    } else if err.is_connect() {
        ProbeError::Connect { endpoint }
    } else if err.is_redirect() {
        ProbeError::Redirect { endpoint }
    } else {
        ProbeError::Transport { endpoint }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::time::{Duration, Instant};

    use axum::Router;
    use axum::http::{HeaderValue, StatusCode, header};
    use axum::response::IntoResponse;
    use axum::routing::get;
    use companion_core::protocol::{
        CloudCodeStatsSnapshot, HealthResponse, PROTOCOL_VERSION, StatsSnapshot, UsageStatsSnapshot,
    };
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;

    use super::*;

    async fn spawn_router(router: Router) -> (u16, tokio::task::JoinHandle<()>) {
        let listener = TcpListener::bind(("127.0.0.1", 0))
            .await
            .expect("bind loopback listener");
        let port = listener.local_addr().expect("local addr").port();
        let handle = tokio::spawn(async move {
            let _ = axum::serve(listener, router).await;
        });
        (port, handle)
    }

    #[tokio::test]
    async fn probe_health_accepts_200_and_rejects_non_200_redirect_and_invalid_json() {
        let client = LoopbackProxyClient::with_default_timeouts().expect("build probe client");

        // 200 OK valid HealthResponse
        let (ok_port, ok_handle) = spawn_router(Router::new().route(
            "/health",
            get(|| async {
                axum::Json(HealthResponse {
                    status: "ok".to_string(),
                    version: "0.1.0".to_string(),
                    protocol_version: PROTOCOL_VERSION,
                })
            }),
        ))
        .await;
        let ok = client
            .probe_health(ok_port, None)
            .await
            .expect("200 OK health");
        assert_eq!(ok.status, "ok");
        assert_eq!(ok.version, "0.1.0");
        assert_eq!(ok.protocol_version, PROTOCOL_VERSION);
        ok_handle.abort();

        // 302 redirect MUST NOT be followed
        let redirect_target_hits = Arc::new(AtomicUsize::new(0));
        let hits_clone = Arc::clone(&redirect_target_hits);
        let (redir_port, redir_handle) = spawn_router(
            Router::new()
                .route(
                    "/health",
                    get(|| async {
                        (
                            StatusCode::FOUND,
                            [(header::LOCATION, HeaderValue::from_static("/target"))],
                            "",
                        )
                    }),
                )
                .route(
                    "/target",
                    get(move || {
                        let hits = Arc::clone(&hits_clone);
                        async move {
                            hits.fetch_add(1, Ordering::SeqCst);
                            axum::Json(HealthResponse::ok("0.1.0"))
                        }
                    }),
                ),
        )
        .await;
        let redir_err = client
            .probe_health(redir_port, None)
            .await
            .expect_err("302 must be rejected");
        assert_eq!(
            redir_err,
            ProbeError::UnexpectedStatus {
                endpoint: "/health",
                status: 302,
            }
        );
        assert!(redir_err.to_string().contains("302"));
        assert_eq!(redirect_target_hits.load(Ordering::SeqCst), 0);
        redir_handle.abort();

        // 204 No Content rejected (strict == 200 OK)
        let (no_content_port, no_content_handle) = spawn_router(Router::new().route(
            "/health",
            get(|| async { StatusCode::NO_CONTENT.into_response() }),
        ))
        .await;
        let err_204 = client
            .probe_health(no_content_port, None)
            .await
            .expect_err("204 must be rejected");
        assert_eq!(
            err_204,
            ProbeError::UnexpectedStatus {
                endpoint: "/health",
                status: 204,
            }
        );
        no_content_handle.abort();

        // 500 Internal Server Error never echoes response body
        let (err500_port, err500_handle) = spawn_router(Router::new().route(
            "/health",
            get(|| async {
                (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    "SENSITIVE_SECRET_500_BODY",
                )
            }),
        ))
        .await;
        let err_500 = client
            .probe_health(err500_port, None)
            .await
            .expect_err("500 must be rejected");
        assert_eq!(
            err_500,
            ProbeError::UnexpectedStatus {
                endpoint: "/health",
                status: 500,
            }
        );
        let msg_500 = err_500.to_string();
        assert!(msg_500.contains("500"));
        assert!(!msg_500.contains("SENSITIVE_SECRET_500_BODY"));
        err500_handle.abort();

        // 200 OK with invalid JSON never echoes body
        let (bad_json_port, bad_json_handle) = spawn_router(Router::new().route(
            "/health",
            get(|| async { (StatusCode::OK, "SECRET_NON_JSON_PAYLOAD") }),
        ))
        .await;
        let err_json = client
            .probe_health(bad_json_port, None)
            .await
            .expect_err("invalid JSON must be rejected");
        assert_eq!(
            err_json,
            ProbeError::InvalidJson {
                endpoint: "/health"
            }
        );
        assert!(!err_json.to_string().contains("SECRET_NON_JSON_PAYLOAD"));
        bad_json_handle.abort();
    }

    #[tokio::test]
    async fn probe_health_enforces_upfront_content_length_and_streaming_chunked_64_kib_cap() {
        let client = LoopbackProxyClient::with_default_timeouts().expect("build probe client");

        // (a) Upfront Content-Length > 64 KiB rejected BEFORE body is even sent
        let cl_listener = TcpListener::bind(("127.0.0.1", 0))
            .await
            .expect("bind raw cl listener");
        let cl_port = cl_listener.local_addr().expect("cl addr").port();
        let cl_handle = tokio::spawn(async move {
            if let Ok((mut sock, _)) = cl_listener.accept().await {
                let mut req_buf = [0u8; 1024];
                let _ = sock.read(&mut req_buf).await;
                let oversized_len = MAX_PROBE_BODY_BYTES + 64;
                let headers = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {oversized_len}\r\n\r\n"
                );
                let _ = sock.write_all(headers.as_bytes()).await;
                // Deliberately hold connection open without sending the full body:
                // upfront Content-Length check must fail immediately without waiting for timeout.
                tokio::time::sleep(Duration::from_secs(5)).await;
            }
        });
        let started = Instant::now();
        let cl_err = client
            .probe_health(cl_port, Some(Duration::from_secs(2)))
            .await
            .expect_err("oversized Content-Length must be rejected upfront");
        assert!(
            started.elapsed() < Duration::from_secs(1),
            "expected upfront Content-Length rejection, took {:?}",
            started.elapsed()
        );
        assert_eq!(
            cl_err,
            ProbeError::BodyTooLarge {
                endpoint: "/health",
                max_bytes: MAX_PROBE_BODY_BYTES,
            }
        );
        assert!(cl_err.to_string().to_ascii_lowercase().contains("limit"));
        cl_handle.abort();

        // (b) Streaming chunked transfer > 64 KiB without Content-Length rejected
        let chunk_listener = TcpListener::bind(("127.0.0.1", 0))
            .await
            .expect("bind raw chunked listener");
        let chunk_port = chunk_listener.local_addr().expect("chunk addr").port();
        let chunk_handle = tokio::spawn(async move {
            if let Ok((mut sock, _)) = chunk_listener.accept().await {
                let mut req_buf = [0u8; 1024];
                let _ = sock.read(&mut req_buf).await;
                let _ = sock
                    .write_all(
                        b"HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nTransfer-Encoding: chunked\r\n\r\n",
                    )
                    .await;
                let chunk = vec![b'x'; 8 * 1024];
                let chunk_hdr = format!("{:x}\r\n", chunk.len());
                for _ in 0..9 {
                    if sock.write_all(chunk_hdr.as_bytes()).await.is_err() {
                        break;
                    }
                    if sock.write_all(&chunk).await.is_err() {
                        break;
                    }
                    if sock.write_all(b"\r\n").await.is_err() {
                        break;
                    }
                }
                let _ = sock.write_all(b"0\r\n\r\n").await;
            }
        });
        let chunk_err = client
            .probe_health(chunk_port, Some(Duration::from_secs(2)))
            .await
            .expect_err("oversized chunked stream must be rejected");
        assert_eq!(
            chunk_err,
            ProbeError::BodyTooLarge {
                endpoint: "/health",
                max_bytes: MAX_PROBE_BODY_BYTES,
            }
        );
        assert!(chunk_err.to_string().to_ascii_lowercase().contains("limit"));
        chunk_handle.abort();
    }

    #[tokio::test]
    async fn probe_health_respects_timeout_override_and_zero_timeout_and_sanitizes_transport_errors()
     {
        let client = LoopbackProxyClient::with_default_timeouts().expect("build probe client");

        // (a) Zero timeout returns Timeout immediately without network I/O
        let started_zero = Instant::now();
        let zero_err = client
            .probe_health(9, Some(Duration::ZERO))
            .await
            .expect_err("zero timeout must fail immediately");
        assert_eq!(
            zero_err,
            ProbeError::Timeout {
                endpoint: "/health"
            }
        );
        assert!(started_zero.elapsed() < Duration::from_millis(100));
        assert!(
            zero_err
                .to_string()
                .to_ascii_lowercase()
                .contains("timed out")
        );

        // (b) Slow server aborted by timeout override
        let (slow_port, slow_handle) = spawn_router(Router::new().route(
            "/health",
            get(|| async {
                tokio::time::sleep(Duration::from_secs(2)).await;
                axum::Json(HealthResponse::ok("0.1.0"))
            }),
        ))
        .await;
        let started = Instant::now();
        let slow_err = client
            .probe_health(slow_port, Some(Duration::from_millis(200)))
            .await
            .expect_err("slow server must time out");
        assert_eq!(
            slow_err,
            ProbeError::Timeout {
                endpoint: "/health"
            }
        );
        assert!(
            started.elapsed() < Duration::from_millis(900),
            "timeout override did not take effect: {:?}",
            started.elapsed()
        );
        let slow_msg = slow_err.to_string();
        assert!(slow_msg.to_ascii_lowercase().contains("timed out"));
        assert!(!slow_msg.contains("http://"));
        assert!(!slow_msg.contains("127.0.0.1"));
        assert!(!slow_msg.contains(&slow_port.to_string()));
        slow_handle.abort();

        // (c) Closed loopback port returns sanitized Connect error with zero URL/port leak
        let closed_listener = TcpListener::bind(("127.0.0.1", 0))
            .await
            .expect("bind ephemeral port");
        let closed_port = closed_listener.local_addr().expect("closed addr").port();
        drop(closed_listener);

        let conn_err = client
            .probe_health(closed_port, Some(Duration::from_secs(1)))
            .await
            .expect_err("closed port must fail");
        assert_eq!(
            conn_err,
            ProbeError::Connect {
                endpoint: "/health"
            }
        );
        let conn_msg = conn_err.to_string();
        assert!(!conn_msg.contains("http://"));
        assert!(!conn_msg.contains("127.0.0.1"));
        assert!(!conn_msg.contains(&closed_port.to_string()));
    }

    #[tokio::test]
    async fn probe_healthy_companion_requires_ok_status_and_nonzero_protocol() {
        let client = LoopbackProxyClient::with_default_timeouts().expect("build probe client");

        let cases = [
            ("ok", 1, true),
            ("degraded", 1, false),
            ("ok\r\n", 1, false),
            ("ok", 0, false),
        ];

        for (status, protocol_version, expected_some) in cases {
            let status_owned = status.to_string();
            let (port, handle) = spawn_router(Router::new().route(
                "/health",
                get(move || {
                    let s = status_owned.clone();
                    async move {
                        axum::Json(HealthResponse {
                            status: s,
                            version: "0.1.0".to_string(),
                            protocol_version,
                        })
                    }
                }),
            ))
            .await;

            let got = client.probe_healthy_companion(port, None).await;
            assert_eq!(
                got.is_some(),
                expected_some,
                "case status={status:?} protocol={protocol_version}"
            );
            handle.abort();
        }
    }

    #[tokio::test]
    async fn probe_stats_parses_structured_stats_snapshot_and_rejects_oversized_or_bad_json() {
        let client = LoopbackProxyClient::with_default_timeouts().expect("build probe client");

        let expected = StatsSnapshot {
            usage: UsageStatsSnapshot {
                requests: 7,
                prompt_tokens: 1200,
                cached_tokens: 600,
                output_tokens: 300,
                reasoning_tokens: 50,
                cache_hit_ratio: 0.5,
            },
            cloudcode: CloudCodeStatsSnapshot {
                mode: "auto".to_string(),
                resolved_upstream: Some("https://cloudcode-pa.googleapis.com".to_string()),
                last_catalog_refresh_at_ms: Some(1_700_000_000_000),
                last_catalog_included_proxy_models: Some(true),
            },
        };
        let payload = expected.clone();
        let (ok_port, ok_handle) = spawn_router(Router::new().route(
            "/api/stats",
            get(move || {
                let p = payload.clone();
                async move { axum::Json(p) }
            }),
        ))
        .await;
        let got = client
            .probe_stats(ok_port, None)
            .await
            .expect("valid stats");
        assert_eq!(got, expected);
        ok_handle.abort();

        // Missing required `usage` field -> InvalidJson
        let (bad_port, bad_handle) = spawn_router(
            Router::new().route("/api/stats", get(|| async { (StatusCode::OK, "{}") })),
        )
        .await;
        assert_eq!(
            client
                .probe_stats(bad_port, None)
                .await
                .expect_err("missing usage field"),
            ProbeError::InvalidJson {
                endpoint: "/api/stats",
            }
        );
        bad_handle.abort();

        // Oversized body -> BodyTooLarge
        let big_body = "x".repeat(MAX_PROBE_BODY_BYTES + 128);
        let (big_port, big_handle) = spawn_router(Router::new().route(
            "/api/stats",
            get(move || {
                let b = big_body.clone();
                async move { (StatusCode::OK, b) }
            }),
        ))
        .await;
        assert_eq!(
            client
                .probe_stats(big_port, None)
                .await
                .expect_err("oversized stats"),
            ProbeError::BodyTooLarge {
                endpoint: "/api/stats",
                max_bytes: MAX_PROBE_BODY_BYTES,
            }
        );
        big_handle.abort();
    }

    #[test]
    fn evaluate_health_detects_non_ok_version_skew_protocol_skew_and_sanitizes_control_chars() {
        let match_eval = evaluate_health(&HealthResponse::ok("0.1.0"), "0.1.0", PROTOCOL_VERSION);
        assert!(match_eval.status_ok);
        assert!(!match_eval.version_skew);
        assert!(!match_eval.protocol_skew);
        assert!(match_eval.version_message.contains("matches"));

        let ver_skew = evaluate_health(&HealthResponse::ok("9.9.9"), "0.1.0", PROTOCOL_VERSION);
        assert!(ver_skew.status_ok);
        assert!(ver_skew.version_skew);
        assert!(!ver_skew.protocol_skew);
        assert!(ver_skew.version_message.contains("version skew"));
        assert!(ver_skew.version_message.contains("9.9.9"));

        let proto_skew = evaluate_health(
            &HealthResponse {
                status: "ok".to_string(),
                version: "0.1.0".to_string(),
                protocol_version: 99,
            },
            "0.1.0",
            PROTOCOL_VERSION,
        );
        assert!(proto_skew.status_ok);
        assert!(!proto_skew.version_skew);
        assert!(proto_skew.protocol_skew);
        assert!(proto_skew.version_message.contains("protocol skew"));
        assert!(proto_skew.version_message.contains("99"));

        let both_skew = evaluate_health(
            &HealthResponse {
                status: "ok".to_string(),
                version: "9.9.9".to_string(),
                protocol_version: 99,
            },
            "0.1.0",
            PROTOCOL_VERSION,
        );
        assert!(both_skew.version_skew);
        assert!(both_skew.protocol_skew);
        assert!(
            both_skew
                .version_message
                .contains("version skew and protocol skew")
        );

        let degraded = evaluate_health(
            &HealthResponse {
                status: "degraded".to_string(),
                version: "0.1.0".to_string(),
                protocol_version: PROTOCOL_VERSION,
            },
            "0.1.0",
            PROTOCOL_VERSION,
        );
        assert!(!degraded.status_ok);
        assert!(degraded.health_message.contains("non-ok status 'degraded'"));

        // Control chars in status/version: raw status != "ok" fails status_ok, tokens sanitized
        let injected = evaluate_health(
            &HealthResponse {
                status: "ok\r\n".to_string(),
                version: "\x1b[31m0.1.0\n".to_string(),
                protocol_version: PROTOCOL_VERSION,
            },
            "0.1.0",
            PROTOCOL_VERSION,
        );
        assert!(!injected.status_ok);
        assert!(injected.version_skew);
        assert!(!injected.health_message.chars().any(char::is_control));
        assert!(!injected.version_message.chars().any(char::is_control));
    }
}
