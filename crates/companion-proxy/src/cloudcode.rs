use std::collections::hash_map::RandomState;
use std::collections::{HashMap, HashSet, VecDeque};
use std::hash::BuildHasher;
use std::sync::{Arc, Mutex};
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
use url::{Host, Url};

pub const MAX_CLOUDCODE_CATALOG_BYTES: usize = 16 * 1024 * 1024;
const CLOUDCODE_CONNECT_TIMEOUT_SECS: u64 = 15;
const ROUTING_CACHE_CAPACITY: usize = 8;
const BUFFERED_REQUEST_TIMEOUT_SECS: u64 = 60;
const MIN_STREAM_READ_TIMEOUT_SECS: u64 = 600;

// Dedicated custom placeholder enum range in Antigravity (MODEL_PLACEHOLDER_M334..=M365).
const PLACEHOLDER_ENUM_START: u32 = 334;
const PLACEHOLDER_ENUM_END: u32 = 365;
const PLACEHOLDER_ENUM_COUNT: u32 = PLACEHOLDER_ENUM_END - PLACEHOLDER_ENUM_START + 1;

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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct AccountRoute {
    use_prod: bool,
    strip_user_project: bool,
}

#[derive(Default)]
struct RoutingCacheInner {
    entries: VecDeque<((u64, u64), AccountRoute)>,
    last_resolved_prod: Option<bool>,
}

struct CloudCodeRoutingCache {
    hasher_a: RandomState,
    hasher_b: RandomState,
    inner: Mutex<RoutingCacheInner>,
}

impl CloudCodeRoutingCache {
    fn new() -> Self {
        Self {
            hasher_a: RandomState::new(),
            hasher_b: RandomState::new(),
            inner: Mutex::new(RoutingCacheInner::default()),
        }
    }

    fn auth_fingerprint(&self, headers: &HeaderMap) -> Option<(u64, u64)> {
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

    fn get(&self, key: (u64, u64)) -> Option<AccountRoute> {
        let mut inner = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        let pos = inner.entries.iter().position(|(k, _)| *k == key)?;
        let (_, route) = inner.entries.remove(pos)?;
        inner.entries.push_back((key, route));
        Some(route)
    }

    fn put(&self, key: (u64, u64), route: AccountRoute) {
        let mut inner = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(pos) = inner.entries.iter().position(|(k, _)| *k == key) {
            inner.entries.remove(pos);
        }
        while inner.entries.len() >= ROUTING_CACHE_CAPACITY {
            inner.entries.pop_front();
        }
        inner.entries.push_back((key, route));
        inner.last_resolved_prod = Some(route.use_prod);
    }

    fn record_without_key(&self, use_prod: bool) {
        let mut inner = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        inner.last_resolved_prod = Some(use_prod);
    }

    fn last_resolved_prod(&self) -> Option<bool> {
        self.inner
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .last_resolved_prod
    }
}

#[derive(Debug, Clone, Copy)]
struct BufferedRequestOptions {
    force_identity_encoding: bool,
    strip_user_project: bool,
}

enum CloudCodeTarget {
    Fixed(String),
    Auto { daily_url: String, prod_url: String },
}

struct CloudCodeClientInner {
    target: CloudCodeTarget,
    cache: CloudCodeRoutingCache,
}

#[derive(Clone)]
pub struct CloudCodeClient {
    inner: Arc<CloudCodeClientInner>,
    http: reqwest::Client,
    buffered_timeout: Duration,
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
        let no_proxy = is_loopback_url(base_url);
        let http = build_http_client(timeout_secs, no_proxy)?;
        Ok(Self {
            inner: Arc::new(CloudCodeClientInner {
                target: CloudCodeTarget::Fixed(base_url.trim_end_matches('/').to_string()),
                cache: CloudCodeRoutingCache::new(),
            }),
            http,
            buffered_timeout: Duration::from_secs(BUFFERED_REQUEST_TIMEOUT_SECS),
        })
    }

    pub fn new_auto_with_urls(
        daily_url: &str,
        prod_url: &str,
        timeout_secs: u64,
    ) -> Result<Self, CloudCodeError> {
        validate_upstream_url(daily_url)?;
        validate_upstream_url(prod_url)?;
        let no_proxy = is_loopback_url(daily_url) && is_loopback_url(prod_url);
        let http = build_http_client(timeout_secs, no_proxy)?;
        Ok(Self {
            inner: Arc::new(CloudCodeClientInner {
                target: CloudCodeTarget::Auto {
                    daily_url: daily_url.trim_end_matches('/').to_string(),
                    prod_url: prod_url.trim_end_matches('/').to_string(),
                },
                cache: CloudCodeRoutingCache::new(),
            }),
            http,
            buffered_timeout: Duration::from_secs(BUFFERED_REQUEST_TIMEOUT_SECS),
        })
    }

    #[cfg(test)]
    fn with_buffered_timeout(mut self, timeout: Duration) -> Self {
        self.buffered_timeout = timeout;
        self
    }

    pub fn base_url(&self) -> &str {
        match &self.inner.target {
            CloudCodeTarget::Fixed(url) => url,
            CloudCodeTarget::Auto {
                daily_url,
                prod_url,
            } => {
                if self.inner.cache.last_resolved_prod() == Some(true) {
                    prod_url
                } else {
                    daily_url
                }
            }
        }
    }

    pub fn mode_label(&self) -> &'static str {
        match &self.inner.target {
            CloudCodeTarget::Fixed(_) => "fixed",
            CloudCodeTarget::Auto { .. } => "auto",
        }
    }

    pub fn resolved_upstream_url(&self) -> Option<String> {
        match &self.inner.target {
            CloudCodeTarget::Fixed(url) => Some(url.clone()),
            CloudCodeTarget::Auto {
                daily_url,
                prod_url,
            } => self
                .inner
                .cache
                .last_resolved_prod()
                .map(|use_prod| if use_prod { prod_url } else { daily_url }.clone()),
        }
    }

    fn record_account_route(
        &self,
        auth_key: Option<(u64, u64)>,
        use_prod: bool,
        strip_user_project: bool,
    ) {
        if let Some(key) = auth_key {
            self.inner.cache.put(
                key,
                AccountRoute {
                    use_prod,
                    strip_user_project,
                },
            );
        } else {
            self.inner.cache.record_without_key(use_prod);
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

        let auth_key = self.inner.cache.auth_fingerprint(inbound_headers);
        let cached = auth_key.and_then(|key| self.inner.cache.get(key));

        // Mirror Antigravity's own UpdateEndpointURL bootstrap for loadCodeAssist:
        // probe prod first (with prod -> daily fallback and metadata-only 403 retry)
        // and detect Business/PayGo/Enterprise (usesGcpTos / gcp-*) vs Personal Google AI Pro.
        let path_only = path_and_query
            .split_once('?')
            .map_or(path_and_query, |(p, _)| p);
        if method == Method::POST && path_only == "/v1internal:loadCodeAssist" {
            let initial_strip = cached.is_some_and(|route| route.strip_user_project);
            let buffered = match &self.inner.target {
                CloudCodeTarget::Fixed(url) => {
                    let (resp, used_strip) = self
                        .send_buffered_metadata_with_403_retry(
                            url,
                            Method::POST,
                            path_and_query,
                            inbound_headers,
                            body,
                            BufferedRequestOptions {
                                force_identity_encoding: true,
                                strip_user_project: initial_strip,
                            },
                        )
                        .await?;
                    if resp.status.is_success()
                        && used_strip
                        && let Some(key) = auth_key
                    {
                        self.inner.cache.put(
                            key,
                            AccountRoute {
                                use_prod: true,
                                strip_user_project: true,
                            },
                        );
                    }
                    resp
                }
                CloudCodeTarget::Auto {
                    daily_url,
                    prod_url,
                } => {
                    let prod_res = self
                        .send_buffered_metadata_with_403_retry(
                            prod_url,
                            Method::POST,
                            path_and_query,
                            inbound_headers,
                            body.clone(),
                            BufferedRequestOptions {
                                force_identity_encoding: true,
                                strip_user_project: initial_strip,
                            },
                        )
                        .await;

                    match prod_res {
                        Ok((resp, used_strip)) if resp.status.is_success() => {
                            if let Some(use_prod) = detect_prod_from_load_code_assist(&resp.body) {
                                self.record_account_route(auth_key, use_prod, used_strip);
                            }
                            resp
                        }
                        Ok((prod_resp, _)) => {
                            match self
                                .send_buffered_metadata_with_403_retry(
                                    daily_url,
                                    Method::POST,
                                    path_and_query,
                                    inbound_headers,
                                    body,
                                    BufferedRequestOptions {
                                        force_identity_encoding: true,
                                        strip_user_project: false,
                                    },
                                )
                                .await
                            {
                                Ok((daily_resp, daily_strip)) if daily_resp.status.is_success() => {
                                    let use_prod =
                                        detect_prod_from_load_code_assist(&daily_resp.body)
                                            .unwrap_or(false);
                                    self.record_account_route(auth_key, use_prod, daily_strip);
                                    daily_resp
                                }
                                _ => prod_resp,
                            }
                        }
                        Err(prod_err) => {
                            match self
                                .send_buffered_metadata_with_403_retry(
                                    daily_url,
                                    Method::POST,
                                    path_and_query,
                                    inbound_headers,
                                    body,
                                    BufferedRequestOptions {
                                        force_identity_encoding: true,
                                        strip_user_project: false,
                                    },
                                )
                                .await
                            {
                                Ok((daily_resp, daily_strip)) => {
                                    if daily_resp.status.is_success() {
                                        let use_prod =
                                            detect_prod_from_load_code_assist(&daily_resp.body)
                                                .unwrap_or(false);
                                        self.record_account_route(auth_key, use_prod, daily_strip);
                                    }
                                    daily_resp
                                }
                                Err(_) => return Err(prod_err),
                            }
                        }
                    }
                }
            };

            let headers = buffered.headers;
            let stream: ByteStream =
                Box::pin(futures_util::stream::once(async move { Ok(buffered.body) }));
            return Ok(CloudCodeStreamResponse {
                status: buffered.status,
                headers,
                stream,
            });
        }

        // Read-only metadata RPC: /v1internal:listExperiments carries Mendel feature flags
        // (including enable-mcp-apps). Mirror fetchAvailableModels' daily <-> prod fallback
        // and Enterprise 403 retry so PayGo/Business accounts never fall back to disabled
        // embedded defaults when daily rejects or returns empty flags.
        if method == Method::POST && path_only == "/v1internal:listExperiments" {
            let buffered = match &self.inner.target {
                CloudCodeTarget::Fixed(url) => {
                    let initial_strip = cached.is_some_and(|route| route.strip_user_project);
                    let (resp, used_strip) = self
                        .send_buffered_metadata_with_403_retry(
                            url,
                            Method::POST,
                            path_and_query,
                            inbound_headers,
                            body,
                            BufferedRequestOptions {
                                force_identity_encoding: true,
                                strip_user_project: initial_strip,
                            },
                        )
                        .await?;
                    if resp.status.is_success()
                        && used_strip
                        && let Some(key) = auth_key
                    {
                        self.inner.cache.put(
                            key,
                            AccountRoute {
                                use_prod: true,
                                strip_user_project: true,
                            },
                        );
                    }
                    tracing::info!(
                        rpc = "listExperiments",
                        upstream = %url,
                        status = resp.status.as_u16(),
                        "cloudcode metadata routed"
                    );
                    resp
                }
                CloudCodeTarget::Auto {
                    daily_url,
                    prod_url,
                } => {
                    let (prefer_prod, initial_strip) = if let Some(route) = cached {
                        (route.use_prod, route.strip_user_project)
                    } else if let Some(ua_prod) = detect_auth_method_from_headers(inbound_headers) {
                        (ua_prod, false)
                    } else if let Some(last_prod) = self.inner.cache.last_resolved_prod() {
                        (last_prod, false)
                    } else {
                        // Personal Google AI Pro either populates `use_prod=false` via loadCodeAssist
                        // first or fetches flags from aicode.googleapis.com; PayGo/Enterprise skips
                        // loadCodeAssist and needs prod (`cloudcode-pa.googleapis.com`) for Mendel
                        // flags (such as `enable-mcp-apps`), falling back to daily if prod rejects.
                        (true, false)
                    };

                    let (primary_url, primary_is_prod, fallback_url, fallback_is_prod) =
                        if prefer_prod {
                            (prod_url.as_str(), true, daily_url.as_str(), false)
                        } else {
                            (daily_url.as_str(), false, prod_url.as_str(), true)
                        };

                    let primary_res = self
                        .send_buffered_metadata_with_403_retry(
                            primary_url,
                            Method::POST,
                            path_and_query,
                            inbound_headers,
                            body.clone(),
                            BufferedRequestOptions {
                                force_identity_encoding: true,
                                strip_user_project: initial_strip,
                            },
                        )
                        .await;

                    if let Ok((ref resp, used_strip)) = primary_res
                        && resp.status == StatusCode::OK
                        && response_has_non_empty_flags(&resp.body)
                    {
                        self.record_account_route(
                            auth_key,
                            primary_is_prod,
                            initial_strip || used_strip,
                        );
                        tracing::info!(
                            rpc = "listExperiments",
                            upstream = %primary_url,
                            status = resp.status.as_u16(),
                            "cloudcode metadata routed"
                        );
                        primary_res.unwrap().0
                    } else {
                        let fallback_res = self
                            .send_buffered_metadata_with_403_retry(
                                fallback_url,
                                Method::POST,
                                path_and_query,
                                inbound_headers,
                                body,
                                BufferedRequestOptions {
                                    force_identity_encoding: true,
                                    strip_user_project: false,
                                },
                            )
                            .await;
                        match fallback_res {
                            Ok((resp, used_strip))
                                if resp.status == StatusCode::OK
                                    && response_has_non_empty_flags(&resp.body) =>
                            {
                                self.record_account_route(auth_key, fallback_is_prod, used_strip);
                                tracing::info!(
                                    rpc = "listExperiments",
                                    upstream = %fallback_url,
                                    status = resp.status.as_u16(),
                                    fallback = true,
                                    "cloudcode metadata routed"
                                );
                                resp
                            }
                            Ok((resp, _)) => match primary_res {
                                Ok((primary_resp, _)) if primary_resp.status.is_success() => {
                                    primary_resp
                                }
                                _ => resp,
                            },
                            Err(fallback_err) => match primary_res {
                                Ok((primary_resp, _)) => primary_resp,
                                Err(_) => return Err(fallback_err),
                            },
                        }
                    }
                }
            };

            let headers = buffered.headers;
            let stream: ByteStream =
                Box::pin(futures_util::stream::once(async move { Ok(buffered.body) }));
            return Ok(CloudCodeStreamResponse {
                status: buffered.status,
                headers,
                stream,
            });
        }

        // Non-metadata RPCs (generation/streaming): strictly single-attempt pass-through
        // with zero retries and zero host fallback.
        let (target_base, strip_user_project) = match &self.inner.target {
            CloudCodeTarget::Fixed(url) => (
                url.as_str(),
                cached.is_some_and(|route| route.strip_user_project),
            ),
            CloudCodeTarget::Auto {
                daily_url,
                prod_url,
            } => {
                if let Some(route) = cached {
                    (
                        if route.use_prod {
                            prod_url.as_str()
                        } else {
                            daily_url.as_str()
                        },
                        route.strip_user_project,
                    )
                } else if let Some(ua_prod) = detect_auth_method_from_headers(inbound_headers) {
                    (
                        if ua_prod {
                            prod_url.as_str()
                        } else {
                            daily_url.as_str()
                        },
                        false,
                    )
                } else if let Some(last_prod) = self.inner.cache.last_resolved_prod() {
                    (
                        if last_prod {
                            prod_url.as_str()
                        } else {
                            daily_url.as_str()
                        },
                        false,
                    )
                } else {
                    (daily_url.as_str(), false)
                }
            }
        };

        let target_url = format!("{target_base}{path_and_query}");
        let mut req_builder = self.http.request(method, target_url);
        for (name, value) in copy_safe_request_headers(inbound_headers, false, strip_user_project) {
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

        let auth_key = self.inner.cache.auth_fingerprint(inbound_headers);
        let cached = auth_key.and_then(|key| self.inner.cache.get(key));
        let body_hints_enterprise = detect_enterprise_from_fetch_body(&body);

        match &self.inner.target {
            CloudCodeTarget::Fixed(base) => {
                let initial_strip =
                    body_hints_enterprise || cached.is_some_and(|route| route.strip_user_project);
                let (resp, used_strip) = self
                    .send_buffered_metadata_with_403_retry(
                        base,
                        Method::POST,
                        path_and_query,
                        inbound_headers,
                        body,
                        BufferedRequestOptions {
                            force_identity_encoding: true,
                            strip_user_project: initial_strip,
                        },
                    )
                    .await?;
                if resp.status == StatusCode::OK
                    && (body_hints_enterprise || used_strip)
                    && let Some(key) = auth_key
                {
                    self.inner.cache.put(
                        key,
                        AccountRoute {
                            use_prod: true,
                            strip_user_project: true,
                        },
                    );
                }
                Ok(resp)
            }
            CloudCodeTarget::Auto {
                daily_url,
                prod_url,
            } => {
                // Precedence: Enterprise entitlement -> per-account LRU -> User-Agent auth_method
                // -> default daily first (matching Google AI Pro / LS_CLOUD_CODE_ENDPOINT).
                let (prefer_prod, initial_strip) = if body_hints_enterprise {
                    (true, true)
                } else if let Some(route) = cached {
                    (route.use_prod, route.strip_user_project)
                } else if let Some(ua_prod) = detect_auth_method_from_headers(inbound_headers) {
                    (ua_prod, false)
                } else {
                    (false, false)
                };

                let (primary_url, primary_is_prod, fallback_url, fallback_is_prod) = if prefer_prod
                {
                    (prod_url.as_str(), true, daily_url.as_str(), false)
                } else {
                    (daily_url.as_str(), false, prod_url.as_str(), true)
                };

                let primary_res = self
                    .send_buffered_metadata_with_403_retry(
                        primary_url,
                        Method::POST,
                        path_and_query,
                        inbound_headers,
                        body.clone(),
                        BufferedRequestOptions {
                            force_identity_encoding: true,
                            strip_user_project: initial_strip,
                        },
                    )
                    .await;

                if let Ok((ref resp, used_strip)) = primary_res
                    && resp.status == StatusCode::OK
                    && response_has_non_empty_models(&resp.body)
                {
                    self.record_account_route(
                        auth_key,
                        primary_is_prod,
                        used_strip || body_hints_enterprise,
                    );
                    return primary_res.map(|(resp, _)| resp);
                }

                // Metadata-only fallback to the alternate CloudCode host (e.g. PayGo account
                // hitting daily or personal AI Pro account hitting prod) so original models
                // are never lost.
                let fallback_res = self
                    .send_buffered_metadata_with_403_retry(
                        fallback_url,
                        Method::POST,
                        path_and_query,
                        inbound_headers,
                        body,
                        BufferedRequestOptions {
                            force_identity_encoding: true,
                            strip_user_project: body_hints_enterprise,
                        },
                    )
                    .await;

                if let Ok((ref resp, used_strip)) = fallback_res
                    && resp.status == StatusCode::OK
                    && response_has_non_empty_models(&resp.body)
                {
                    self.record_account_route(
                        auth_key,
                        fallback_is_prod,
                        used_strip || body_hints_enterprise,
                    );
                    return fallback_res.map(|(resp, _)| resp);
                }

                match (primary_res, fallback_res) {
                    (Ok((p, _)), _) if p.status == StatusCode::OK => Ok(p),
                    (_, Ok((f, _))) if f.status == StatusCode::OK => Ok(f),
                    (Ok((p, _)), _) => Ok(p),
                    (Err(_), Ok((f, _))) => Ok(f),
                    (Err(e), Err(_)) => Err(e),
                }
            }
        }
    }

    async fn send_buffered_metadata_with_403_retry(
        &self,
        base_url: &str,
        method: Method,
        path_and_query: &str,
        inbound_headers: &HeaderMap,
        body: Bytes,
        opts: BufferedRequestOptions,
    ) -> Result<(CloudCodeBufferedResponse, bool), CloudCodeError> {
        let first = self
            .send_buffered(
                base_url,
                method.clone(),
                path_and_query,
                inbound_headers,
                body.clone(),
                opts,
            )
            .await?;

        if first.status == StatusCode::FORBIDDEN
            && !opts.strip_user_project
            && inbound_headers.contains_key("x-goog-user-project")
        {
            let retry_opts = BufferedRequestOptions {
                strip_user_project: true,
                ..opts
            };
            match self
                .send_buffered(
                    base_url,
                    method,
                    path_and_query,
                    inbound_headers,
                    body,
                    retry_opts,
                )
                .await
            {
                Ok(retry_resp) => {
                    let strip_succeeded = retry_resp.status.is_success();
                    return Ok((retry_resp, strip_succeeded));
                }
                Err(_) => return Ok((first, false)),
            }
        }

        Ok((first, opts.strip_user_project))
    }

    async fn send_buffered(
        &self,
        base_url: &str,
        method: Method,
        path_and_query: &str,
        inbound_headers: &HeaderMap,
        body: Bytes,
        opts: BufferedRequestOptions,
    ) -> Result<CloudCodeBufferedResponse, CloudCodeError> {
        let target_url = format!("{base_url}{path_and_query}");
        let mut req_builder = self.http.request(method, target_url);
        for (name, value) in copy_safe_request_headers(
            inbound_headers,
            opts.force_identity_encoding,
            opts.strip_user_project,
        ) {
            req_builder = req_builder.header(name, value);
        }
        if !body.is_empty() {
            req_builder = req_builder.body(body);
        }

        let timeout = self.buffered_timeout;
        tokio::time::timeout(timeout, async move {
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
        })
        .await
        .map_err(|_| {
            CloudCodeError::Upstream(format!(
                "cloudcode metadata request timed out after {}ms",
                timeout.as_millis()
            ))
        })?
    }
}

fn is_loopback_url(raw: &str) -> bool {
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

fn build_http_client(timeout_secs: u64, no_proxy: bool) -> Result<reqwest::Client, CloudCodeError> {
    let effective_timeout = if timeout_secs == 0 {
        DEFAULT_TIMEOUT_SECS
    } else {
        timeout_secs
    };
    let mut builder = reqwest::Client::builder()
        .http1_only()
        .redirect(reqwest::redirect::Policy::none())
        .connect_timeout(Duration::from_secs(CLOUDCODE_CONNECT_TIMEOUT_SECS))
        .read_timeout(Duration::from_secs(
            effective_timeout.max(MIN_STREAM_READ_TIMEOUT_SECS),
        ));
    if no_proxy {
        builder = builder.no_proxy();
    }
    builder
        .build()
        .map_err(|err| CloudCodeError::Upstream(err.to_string()))
}

fn detect_prod_from_load_code_assist(body: &[u8]) -> Option<bool> {
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
    Some(false)
}

fn detect_enterprise_from_fetch_body(body: &[u8]) -> bool {
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

fn detect_auth_method_from_headers(headers: &HeaderMap) -> Option<bool> {
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

fn response_has_non_empty_models(body: &[u8]) -> bool {
    let Ok(parsed) = serde_json::from_slice::<Value>(body) else {
        return false;
    };
    parsed
        .get("models")
        .and_then(Value::as_object)
        .is_some_and(|m| !m.is_empty())
}

fn response_has_non_empty_flags(body: &[u8]) -> bool {
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

const XHIGH_GEMINI_LEVELS: &[ThinkingVariant] = &[
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

// Canonical collision-free preferred offsets (0..=16 -> M334..=M350) for the 17 curated variants.
// Allocated in fixed canonical order independent of `default_model` so changing the default
// model never shifts persisted chat model enum bindings.
const CURATED_SLOT_TABLE: &[(&str, u32)] = &[
    ("gemini-4-hydrogen-low", 0),
    ("gemini-4-hydrogen-medium", 1),
    ("gemini-4-hydrogen-high", 2),
    ("gemini-4-hydrogen-xhigh", 3),
    ("gemini-4-argon-low", 4),
    ("gemini-4-argon-medium", 5),
    ("gemini-4-argon-high", 6),
    ("gemini-4-neon-low", 7),
    ("gemini-4-neon-medium", 8),
    ("gemini-4-neon-high", 9),
    ("gemini-4-neon-xhigh", 10),
    ("gemini-4-carbon-low", 11),
    ("gemini-4-carbon-medium", 12),
    ("gemini-4-carbon-high", 13),
    ("gemini-3.8-flash-low", 14),
    ("gemini-3.8-flash-medium", 15),
    ("gemini-3.8-flash-high", 16),
];

fn variants_for_model(model_id: &str) -> &'static [ThinkingVariant] {
    match model_id {
        "gemini-4-hydrogen" | "gemini-4-neon" => XHIGH_GEMINI_LEVELS,
        _ => STANDARD_GEMINI_LEVELS,
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

fn find_upstream_model_template<'a>(
    models: &'a Map<String, Value>,
    raw_id: &str,
    suffix: &str,
) -> Option<&'a Map<String, Value>> {
    // 1. Exact effort variant match (e.g. "gemini-4-neon-high").
    let exact_variant = format!("{raw_id}-{suffix}");
    if let Some(obj) = models.get(&exact_variant).and_then(Value::as_object) {
        return Some(obj);
    }
    // 2. Exact base model match (e.g. "gemini-4-neon").
    if let Some(obj) = models.get(raw_id).and_then(Value::as_object) {
        return Some(obj);
    }
    // 3. Another effort variant of the same base model (prefer high -> medium -> low).
    for fallback_suffix in ["high", "medium", "low", "xhigh"] {
        let candidate = format!("{raw_id}-{fallback_suffix}");
        if let Some(obj) = models.get(&candidate).and_then(Value::as_object) {
            return Some(obj);
        }
    }
    let prefix = format!("{raw_id}-");
    let mut same_family: Vec<&String> = models
        .keys()
        .filter(|k| !k.starts_with("companion/") && k.starts_with(&prefix))
        .collect();
    same_family.sort();
    for key in same_family {
        if let Some(obj) = models.get(key).and_then(Value::as_object) {
            return Some(obj);
        }
    }
    // 4. Fallback to any upstream Gemini 4 or Gemini 3.8 model carrying experiment/compaction
    // metadata so newly discovered Gemini models still inherit the native checkpointer config.
    let mut gemini_candidates: Vec<&String> = models
        .keys()
        .filter(|k| {
            !k.starts_with("companion/")
                && (k.starts_with("gemini-4-") || k.starts_with("gemini-3.8-"))
        })
        .collect();
    gemini_candidates.sort();
    for key in &gemini_candidates {
        if let Some(obj) = models.get(*key).and_then(Value::as_object)
            && (obj.contains_key("modelExperimentIds") || obj.contains_key("modelExperiments"))
        {
            return Some(obj);
        }
    }
    for key in gemini_candidates {
        if let Some(obj) = models.get(key).and_then(Value::as_object) {
            return Some(obj);
        }
    }
    None
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

fn fnv1a_64(bytes: &[u8]) -> u64 {
    const FNV_OFFSET_BASIS: u64 = 0xcbf29ce484222325;
    const FNV_PRIME: u64 = 0x100000001b3;
    let mut hash = FNV_OFFSET_BASIS;
    for &b in bytes {
        hash ^= u64::from(b);
        hash = hash.wrapping_mul(FNV_PRIME);
    }
    hash
}

fn claim_placeholder_slot(preferred_offset: u32, used: &mut HashSet<String>) -> Option<String> {
    let start = preferred_offset % PLACEHOLDER_ENUM_COUNT;
    for delta in 0..PLACEHOLDER_ENUM_COUNT {
        let offset = (start + delta) % PLACEHOLDER_ENUM_COUNT;
        let name = format!("MODEL_PLACEHOLDER_M{}", PLACEHOLDER_ENUM_START + offset);
        if used.insert(name.clone()) {
            return Some(name);
        }
    }
    None
}

fn assign_deterministic_placeholder_enums(
    selected_models: &[String],
    used_enums: &HashSet<String>,
) -> HashMap<String, String> {
    let mut used = used_enums.clone();
    let mut needed: HashSet<String> = HashSet::new();
    for raw_id in selected_models {
        for variant in variants_for_model(raw_id) {
            needed.insert(format!("{raw_id}-{}", variant.suffix));
        }
    }

    let mut assigned = HashMap::with_capacity(needed.len());

    // Pass 1: Curated variants in fixed canonical order (independent of `default_model`).
    for &(variant_key, preferred_offset) in CURATED_SLOT_TABLE {
        if needed.remove(variant_key)
            && let Some(slot) = claim_placeholder_slot(preferred_offset, &mut used)
        {
            assigned.insert(variant_key.to_string(), slot);
        }
    }

    // Pass 2: Discovered non-curated models in canonical lexicographical order.
    let mut remaining_models: Vec<&String> = selected_models.iter().collect();
    remaining_models.sort();
    for raw_id in remaining_models {
        for variant in variants_for_model(raw_id) {
            let variant_key = format!("{raw_id}-{}", variant.suffix);
            if needed.remove(&variant_key) {
                let curated_count = CURATED_SLOT_TABLE.len() as u32;
                let dynamic_span = PLACEHOLDER_ENUM_COUNT - curated_count;
                let preferred = curated_count
                    + (fnv1a_64(variant_key.as_bytes()) % u64::from(dynamic_span)) as u32;
                if let Some(slot) = claim_placeholder_slot(preferred, &mut used) {
                    assigned.insert(variant_key, slot);
                }
            }
        }
    }

    assigned
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
    let used_in_range = (PLACEHOLDER_ENUM_START..=PLACEHOLDER_ENUM_END)
        .filter(|n| used_enums.contains(&format!("MODEL_PLACEHOLDER_M{n}")))
        .count();
    let mut remaining_slots = (PLACEHOLDER_ENUM_COUNT as usize).saturating_sub(used_in_range);

    // Select which models fit in the available placeholder pool using `ordered_models`
    // priority (default_model and curated models first), then assign slots deterministically.
    let mut selected_models: Vec<String> = Vec::new();
    for raw_id in &ordered_models {
        let variants = variants_for_model(raw_id);
        if remaining_slots < variants.len() {
            break;
        }
        remaining_slots = remaining_slots.saturating_sub(variants.len());
        selected_models.push(raw_id.clone());
    }

    if selected_models.is_empty() {
        return serde_json::to_vec(&root);
    }

    let slot_map = assign_deterministic_placeholder_enums(&selected_models, &used_enums);

    let mut injected_model_ids: Vec<String> = Vec::new();
    let mut injected_details: Vec<(String, Value)> = Vec::new();
    let mut injected_groups: Vec<(String, Value)> = Vec::new();

    let upstream_models_snapshot = root_obj
        .get("models")
        .and_then(Value::as_object)
        .cloned()
        .unwrap_or_default();

    for raw_id in &selected_models {
        let variants = variants_for_model(raw_id);
        let base_id = format!("companion/{raw_id}");
        let display_base = format!("{} (Proxy)", pretty_model_name(raw_id));
        let mut level_entries = Vec::with_capacity(variants.len());

        for variant in variants {
            let variant_key = format!("{raw_id}-{}", variant.suffix);
            let Some(enum_name) = slot_map.get(&variant_key) else {
                break;
            };

            let variant_id = format!("{base_id}-{}", variant.suffix);
            let display_name = format!("{display_base} ({})", variant.label_effort);

            // Inherit upstream model metadata (modelExperimentIds / modelExperiments /
            // ignoreExperimentNames / tokenizerType / supportsCumulativeContext /
            // supportsThoughtCirculation / maxTokens / maxOutputTokens) so Antigravity
            // applies the same compaction window and capabilities as the native model.
            let mut details_map =
                find_upstream_model_template(&upstream_models_snapshot, raw_id, variant.suffix)
                    .cloned()
                    .unwrap_or_default();

            for strip_key in [
                "vertexModelId",
                "tagTitle",
                "tagDescription",
                "disabled",
                "recommended",
                "deprecated",
                "isInternal",
            ] {
                details_map.remove(strip_key);
            }

            let has_valid_max_tokens = details_map
                .get("maxTokens")
                .and_then(Value::as_u64)
                .is_some_and(|v| v > 0);
            if !has_valid_max_tokens {
                details_map.insert("maxTokens".to_string(), json!(1048576));
            }
            let has_valid_max_output = details_map
                .get("maxOutputTokens")
                .and_then(Value::as_u64)
                .is_some_and(|v| v > 0);
            if !has_valid_max_output {
                details_map.insert("maxOutputTokens".to_string(), json!(65536));
            }
            if !details_map.contains_key("supportedMimeTypes") {
                details_map.insert(
                    "supportedMimeTypes".to_string(),
                    gemini_supported_mime_types(),
                );
            }

            details_map.insert("model".to_string(), json!(enum_name));
            details_map.insert("displayName".to_string(), json!(display_name));
            details_map.insert(
                "modelUrl".to_string(),
                json!(format!("gemini-api://{raw_id}")),
            );
            details_map.insert("supportsThinking".to_string(), json!(true));
            details_map.insert("thinkingLevel".to_string(), json!(variant.thinking_level));
            details_map.insert("supportsImages".to_string(), json!(true));
            details_map.insert("supportsVideo".to_string(), json!(true));
            details_map.insert("supportsPdf".to_string(), json!(true));
            details_map.insert(
                "apiProvider".to_string(),
                json!("API_PROVIDER_GOOGLE_GEMINI"),
            );
            details_map.insert("modelProvider".to_string(), json!("MODEL_PROVIDER_GOOGLE"));
            details_map.insert(
                "toolFormatterType".to_string(),
                json!("TOOL_FORMATTER_TYPE_NONE"),
            );
            details_map.insert("quotaInfo".to_string(), json!({ "remainingFraction": 1.0 }));
            let details = Value::Object(details_map);

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
            let merged_bytes =
                merge_available_models(&upstream_bytes, &proxy_models, default_model)
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
}
