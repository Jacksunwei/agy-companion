use std::fmt;
use std::time::Duration;

use ::url::Url;
use companion_core::{ApiKey, GeminiConfig, api_key_hint};
use futures_util::TryStreamExt;
use http::header::{self, HeaderName, HeaderValue};

use crate::{
    BoxFuture, ByteStream, ForwardRequest, ForwardResponse, GeminiModelList, ProviderAdapter,
    ProviderError, filter_and_order_models,
};

mod url;

#[cfg(test)]
mod tests;

use self::url::{
    EnvKeySource, build_api_key_header, build_models_list_url, build_target_url, is_loopback_url,
    normalize_base_url, resolve_content_type_header, validate_api_key_string,
};

pub const GEMINI_API_KEY_ENV_VAR: &str = "GEMINI_API_KEY";

const GEMINI_API_KEY_HEADER: HeaderName = HeaderName::from_static("x-goog-api-key");
const MAX_CONNECT_TIMEOUT_SECS: u64 = 30;
const HEALTH_CHECK_ERROR_BODY_LIMIT: usize = 8 * 1024;
const LIST_MODELS_TIMEOUT_SECS: u64 = 10;
const LIST_MODELS_BODY_LIMIT: usize = 4 * 1024 * 1024;
const KNOWN_GEMINI_ERROR_CODES: &[&str] = &[
    "API_KEY_INVALID",
    "API_KEY_HTTP_REFERRER_BLOCKED",
    "API_KEY_SERVICE_BLOCKED",
    "INVALID_ARGUMENT",
    "UNAUTHENTICATED",
    "PERMISSION_DENIED",
    "NOT_FOUND",
    "RESOURCE_EXHAUSTED",
    "FAILED_PRECONDITION",
    "INTERNAL",
    "UNAVAILABLE",
    "DEADLINE_EXCEEDED",
];

#[derive(Clone)]
pub struct GeminiProvider {
    client: reqwest::Client,
    base_url: Url,
    normalized_base_url: String,
    normalized_base_path: String,
    api_key: Option<String>,
    timeout: Duration,
    env_key_source: EnvKeySource,
}

impl fmt::Debug for GeminiProvider {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("GeminiProvider")
            .field("base_url", &self.normalized_base_url)
            .field("api_key", &api_key_hint(self.api_key.as_deref()))
            .field("timeout_secs", &self.timeout.as_secs())
            .finish_non_exhaustive()
    }
}

impl GeminiProvider {
    /// Constructs a validated `GeminiProvider` from `companion_core::GeminiConfig`
    /// and an optional validated `ApiKey` loaded from `AuthStore`.
    pub fn from_config(
        config: &GeminiConfig,
        api_key: Option<&ApiKey>,
    ) -> Result<Self, ProviderError> {
        Self::from_config_with_timeout(config, api_key, Duration::from_secs(config.timeout_secs))
    }

    /// Constructs a validated `GeminiProvider` from `companion_core::GeminiConfig`
    /// and an optional validated `ApiKey` using an explicit request timeout `Duration` (`!timeout.is_zero()`).
    pub fn from_config_with_timeout(
        config: &GeminiConfig,
        api_key: Option<&ApiKey>,
        timeout: Duration,
    ) -> Result<Self, ProviderError> {
        Self::new_with_timeout(
            config.base_url.as_str(),
            api_key.map(|k| k.expose().to_string()),
            timeout,
        )
    }

    /// Constructs a validated `GeminiProvider` from explicit parameters (`timeout_secs > 0`).
    pub fn new(
        base_url: impl Into<String>,
        api_key: Option<String>,
        timeout_secs: u64,
    ) -> Result<Self, ProviderError> {
        Self::new_with_timeout(base_url, api_key, Duration::from_secs(timeout_secs))
    }

    /// Constructs a validated `GeminiProvider` from explicit parameters (`!timeout.is_zero()`).
    pub fn new_with_timeout(
        base_url: impl Into<String>,
        api_key: Option<String>,
        timeout: Duration,
    ) -> Result<Self, ProviderError> {
        Self::build(base_url.into(), api_key, timeout, EnvKeySource::ProcessEnv)
    }

    /// Controls whether `GEMINI_API_KEY` env fallback is consulted when neither
    /// `api_key_override` nor configured `api_key` is set (default: `true`).
    pub fn with_env_fallback(mut self, enabled: bool) -> Self {
        self.env_key_source = if enabled {
            EnvKeySource::ProcessEnv
        } else {
            EnvKeySource::Disabled
        };
        self
    }

    #[cfg(test)]
    pub(crate) fn with_env_api_key(
        config: &GeminiConfig,
        api_key: Option<&ApiKey>,
        env_key: Option<&str>,
    ) -> Result<Self, ProviderError> {
        Self::build(
            config.base_url.clone(),
            api_key.map(|k| k.expose().to_string()),
            Duration::from_secs(config.timeout_secs),
            EnvKeySource::Fixed(env_key.map(str::to_owned)),
        )
    }

    fn build(
        raw_base_url: String,
        api_key: Option<String>,
        timeout: Duration,
        env_key_source: EnvKeySource,
    ) -> Result<Self, ProviderError> {
        if timeout.is_zero() {
            return Err(ProviderError::InvalidConfig(
                "timeout must be greater than 0".to_string(),
            ));
        }

        let (base_url, normalized_base_url, normalized_base_path) =
            normalize_base_url(&raw_base_url)?;

        let validated_api_key = match api_key {
            Some(raw) => validate_api_key_string(&raw)?,
            None => None,
        };

        let connect_timeout = timeout.min(Duration::from_secs(MAX_CONNECT_TIMEOUT_SECS));

        let mut builder = reqwest::Client::builder()
            .timeout(timeout)
            .connect_timeout(connect_timeout)
            .redirect(reqwest::redirect::Policy::none())
            .user_agent(concat!("agy-companion/", env!("CARGO_PKG_VERSION")));

        // Bypass ambient HTTP_PROXY/HTTPS_PROXY strictly for loopback upstreams so local
        // providers and tests are not hijacked while remote HTTPS still honors proxy settings.
        if is_loopback_url(&base_url) {
            builder = builder.no_proxy();
        }

        let client = builder.build().map_err(|err| {
            ProviderError::InvalidConfig(format!(
                "failed to initialize HTTP client: {}",
                err.without_url()
            ))
        })?;

        Ok(Self {
            client,
            base_url,
            normalized_base_url,
            normalized_base_path,
            api_key: validated_api_key,
            timeout,
            env_key_source,
        })
    }

    pub fn normalized_base_url(&self) -> &str {
        &self.normalized_base_url
    }

    pub fn timeout(&self) -> Duration {
        self.timeout
    }

    pub fn api_key_hint(&self) -> Option<String> {
        api_key_hint(self.api_key.as_deref())
    }

    fn resolve_api_key_header(
        &self,
        override_key: Option<&str>,
    ) -> Result<HeaderValue, ProviderError> {
        if let Some(raw_override) = override_key
            && let Some(header_val) = build_api_key_header(raw_override)?
        {
            return Ok(header_val);
        }

        if let Some(configured) = self.api_key.as_deref()
            && let Some(header_val) = build_api_key_header(configured)?
        {
            return Ok(header_val);
        }

        if let Some(env_raw) = self.env_key_source.read()?
            && let Some(header_val) = build_api_key_header(&env_raw)?
        {
            return Ok(header_val);
        }

        Err(ProviderError::MissingApiKey)
    }

    fn build_target_url(&self, path_and_query: &str) -> Result<Url, ProviderError> {
        build_target_url(
            &self.base_url,
            &self.normalized_base_url,
            &self.normalized_base_path,
            path_and_query,
        )
    }
}

impl ProviderAdapter for GeminiProvider {
    fn name(&self) -> &'static str {
        "gemini"
    }

    fn forward_gemini(
        &self,
        req: ForwardRequest,
    ) -> BoxFuture<'_, Result<ForwardResponse, ProviderError>> {
        Box::pin(async move {
            let target_url = self.build_target_url(&req.path_and_query)?;
            let api_key_header = self.resolve_api_key_header(req.api_key_override.as_deref())?;
            let content_type_header = resolve_content_type_header(req.content_type.as_deref())?;

            tracing::debug!(
                base_url = %self.normalized_base_url,
                path = %target_url.path(),
                body_len = req.body.len(),
                "forwarding Gemini request"
            );

            let response = self
                .client
                .post(target_url)
                .header(GEMINI_API_KEY_HEADER, api_key_header)
                .header(header::CONTENT_TYPE, content_type_header)
                .body(req.body)
                .send()
                .await
                .map_err(sanitize_reqwest_error)?;

            let status = response.status().as_u16();
            let content_type = response
                .headers()
                .get(header::CONTENT_TYPE)
                .and_then(|value| value.to_str().ok())
                .map(str::to_owned);
            let stream: ByteStream =
                Box::pin(response.bytes_stream().map_err(sanitize_reqwest_error));

            Ok(ForwardResponse {
                status,
                content_type,
                stream,
            })
        })
    }

    fn health_check(&self) -> BoxFuture<'_, Result<(), ProviderError>> {
        Box::pin(async move {
            let api_key_header = self.resolve_api_key_header(None)?;
            let target_url = build_models_list_url(
                &self.base_url,
                &self.normalized_base_url,
                &self.normalized_base_path,
                1,
            )?;

            tracing::debug!(
                base_url = %self.normalized_base_url,
                "running Gemini upstream health check"
            );

            let response = self
                .client
                .get(target_url)
                .header(GEMINI_API_KEY_HEADER, api_key_header)
                .send()
                .await
                .map_err(sanitize_reqwest_error)?;

            if response.status().is_success() {
                return Ok(());
            }

            let code = response.status().as_u16();
            let body = read_bounded_body(response, HEALTH_CHECK_ERROR_BODY_LIMIT).await;
            if let Some(known) = extract_known_gemini_error_code(&body) {
                Err(ProviderError::Upstream(format!(
                    "upstream health check failed with HTTP {code} ({known})"
                )))
            } else {
                Err(ProviderError::Upstream(format!(
                    "upstream health check failed with HTTP {code}"
                )))
            }
        })
    }

    fn list_models(
        &self,
        api_key_override: Option<String>,
    ) -> BoxFuture<'_, Result<GeminiModelList, ProviderError>> {
        Box::pin(async move {
            let api_key_header = self.resolve_api_key_header(api_key_override.as_deref())?;
            let target_url = build_models_list_url(
                &self.base_url,
                &self.normalized_base_url,
                &self.normalized_base_path,
                1000,
            )?;

            tracing::debug!(
                base_url = %self.normalized_base_url,
                "fetching upstream Gemini model catalog"
            );

            let timeout = self
                .timeout
                .min(Duration::from_secs(LIST_MODELS_TIMEOUT_SECS));
            let response = self
                .client
                .get(target_url)
                .timeout(timeout)
                .header(GEMINI_API_KEY_HEADER, api_key_header)
                .send()
                .await
                .map_err(sanitize_reqwest_error)?;

            if !response.status().is_success() {
                let code = response.status().as_u16();
                return Err(ProviderError::Upstream(format!(
                    "upstream model discovery failed with HTTP {code}"
                )));
            }

            let body = read_bounded_body_strict(response, LIST_MODELS_BODY_LIMIT).await?;
            let parsed: GeminiModelList = serde_json::from_slice(&body).map_err(|_| {
                ProviderError::Upstream(
                    "upstream model discovery returned invalid JSON".to_string(),
                )
            })?;

            Ok(filter_and_order_models(parsed.models, None))
        })
    }
}

fn sanitize_reqwest_error(err: reqwest::Error) -> ProviderError {
    let sanitized = err.without_url();
    if sanitized.is_timeout() {
        ProviderError::Upstream("upstream request timed out".to_string())
    } else if sanitized.is_connect() {
        ProviderError::Upstream("failed to connect to upstream".to_string())
    } else {
        ProviderError::Upstream(format!("upstream transport error: {sanitized}"))
    }
}

async fn read_bounded_body(mut response: reqwest::Response, max_bytes: usize) -> Vec<u8> {
    let mut body = Vec::new();
    while body.len() < max_bytes {
        match response.chunk().await {
            Ok(Some(chunk)) => {
                let remaining = max_bytes - body.len();
                let take = chunk.len().min(remaining);
                body.extend_from_slice(&chunk[..take]);
                if chunk.len() > remaining {
                    break;
                }
            }
            Ok(None) | Err(_) => break,
        }
    }
    body
}

async fn read_bounded_body_strict(
    mut response: reqwest::Response,
    max_bytes: usize,
) -> Result<Vec<u8>, ProviderError> {
    let mut body = Vec::new();
    while let Some(chunk) = response.chunk().await.map_err(sanitize_reqwest_error)? {
        if body.len().saturating_add(chunk.len()) > max_bytes {
            return Err(ProviderError::Upstream(
                "upstream model discovery response exceeded size limit".to_string(),
            ));
        }
        body.extend_from_slice(&chunk);
    }
    Ok(body)
}

fn match_known_gemini_error_code(candidate: &str) -> Option<&'static str> {
    KNOWN_GEMINI_ERROR_CODES
        .iter()
        .copied()
        .find(|&known| known == candidate)
}

fn extract_known_gemini_error_code(body: &[u8]) -> Option<&'static str> {
    let json: serde_json::Value = serde_json::from_slice(body).ok()?;
    let error_obj = json.get("error")?;
    if let Some(details) = error_obj
        .get("details")
        .and_then(serde_json::Value::as_array)
    {
        for detail in details {
            if let Some(reason) = detail.get("reason").and_then(serde_json::Value::as_str)
                && let Some(known) = match_known_gemini_error_code(reason)
            {
                return Some(known);
            }
        }
    }
    error_obj
        .get("status")
        .and_then(serde_json::Value::as_str)
        .and_then(match_known_gemini_error_code)
}
