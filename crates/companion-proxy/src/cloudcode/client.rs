use std::sync::Arc;
use std::time::Duration;

use bytes::Bytes;
use companion_core::{
    CLOUDCODE_DAILY_UPSTREAM_URL, CLOUDCODE_PROD_UPSTREAM_URL, DEFAULT_CLOUDCODE_UPSTREAM_URL,
    DEFAULT_TIMEOUT_SECS, validate_cloudcode_upstream_url, validate_upstream_url,
};
use companion_providers::ByteStream;
use futures_util::TryStreamExt;
use http::header::HeaderMap;
use http::{Method, StatusCode};

use super::routing::{
    AccountRoute, CloudCodeRoutingCache, copy_safe_request_headers, copy_safe_response_headers,
    detect_auth_method_from_headers, is_loopback_url, is_uncompressed_content_encoding,
    validate_cloudcode_path,
};
use super::{
    CloudCodeBufferedResponse, CloudCodeError, CloudCodeStreamResponse, MAX_CLOUDCODE_CATALOG_BYTES,
};

const CLOUDCODE_CONNECT_TIMEOUT_SECS: u64 = 15;
const BUFFERED_REQUEST_TIMEOUT_SECS: u64 = 60;
const MIN_STREAM_READ_TIMEOUT_SECS: u64 = 600;

#[derive(Debug, Clone, Copy)]
pub(super) struct BufferedRequestOptions {
    pub(super) force_identity_encoding: bool,
    pub(super) strip_user_project: bool,
}

pub(super) enum CloudCodeTarget {
    Fixed(String),
    Auto { daily_url: String, prod_url: String },
}

pub(super) struct CloudCodeClientInner {
    pub(super) target: CloudCodeTarget,
    pub(super) cache: CloudCodeRoutingCache,
}

#[derive(Clone)]
pub struct CloudCodeClient {
    pub(super) inner: Arc<CloudCodeClientInner>,
    pub(super) http: reqwest::Client,
    pub(super) buffered_timeout: Duration,
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
    pub(super) fn with_buffered_timeout(mut self, timeout: Duration) -> Self {
        self.buffered_timeout = timeout;
        self
    }

    #[cfg(test)]
    pub(crate) fn seed_prod_route_for_test(&self) {
        self.inner.cache.record_without_key(true, true);
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

    pub(super) fn record_account_route(
        &self,
        auth_key: Option<(u64, u64)>,
        use_prod: bool,
        strip_user_project: bool,
        authoritative: bool,
    ) {
        if let Some(key) = auth_key {
            self.inner.cache.put(
                key,
                AccountRoute {
                    use_prod,
                    strip_user_project,
                    authoritative,
                },
            );
        } else {
            self.inner.cache.record_without_key(use_prod, authoritative);
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

        let path_only = path_and_query
            .split_once('?')
            .map_or(path_and_query, |(p, _)| p);
        if method == Method::POST && path_only == "/v1internal:loadCodeAssist" {
            return self
                .forward_load_code_assist(path_and_query, inbound_headers, body, auth_key, cached)
                .await;
        }
        if method == Method::POST && path_only == "/v1internal:listExperiments" {
            return self
                .forward_list_experiments(path_and_query, inbound_headers, body, auth_key, cached)
                .await;
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

        let resp = req_builder.send().await.map_err(sanitize_reqwest_error)?;
        let status = resp.status();
        let response_is_uncompressed = is_uncompressed_content_encoding(resp.headers());
        let headers = copy_safe_response_headers(resp.headers());
        let stream: ByteStream = Box::pin(resp.bytes_stream().map_err(|err| {
            companion_providers::ProviderError::Upstream(format!(
                "cloudcode stream error: {}",
                err.without_url()
            ))
        }));

        Ok(CloudCodeStreamResponse {
            status,
            headers,
            response_is_uncompressed,
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

        self.execute_fetch_available_models(path_and_query, inbound_headers, body, auth_key, cached)
            .await
    }

    pub(super) async fn send_buffered_metadata_with_403_retry(
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
            let mut resp = req_builder.send().await.map_err(sanitize_reqwest_error)?;
            let status = resp.status();
            let response_is_uncompressed = is_uncompressed_content_encoding(resp.headers());
            let headers = copy_safe_response_headers(resp.headers());

            let mut collected = Vec::new();
            while let Some(chunk) = resp.chunk().await.map_err(sanitize_reqwest_error)? {
                if collected.len().saturating_add(chunk.len()) > MAX_CLOUDCODE_CATALOG_BYTES {
                    return Err(CloudCodeError::ResponseTooLarge);
                }
                collected.extend_from_slice(&chunk);
            }

            Ok(CloudCodeBufferedResponse {
                status,
                headers,
                response_is_uncompressed,
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

pub(super) fn sanitize_reqwest_error(err: reqwest::Error) -> CloudCodeError {
    CloudCodeError::Upstream(err.without_url().to_string())
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
    builder.build().map_err(sanitize_reqwest_error)
}
