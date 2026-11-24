use std::fmt;
use std::time::Duration;

use companion_core::{GeminiConfig, api_key_hint, is_placeholder_api_key, validate_upstream_url};
use futures_util::TryStreamExt;
use http::header::{self, HeaderName, HeaderValue};
use url::Url;

use crate::{
    BoxFuture, ByteStream, ForwardRequest, ForwardResponse, GeminiModelList, ProviderAdapter,
    ProviderError, filter_and_order_models,
};

pub const GEMINI_API_KEY_ENV_VAR: &str = "GEMINI_API_KEY";

const GEMINI_API_KEY_HEADER: HeaderName = HeaderName::from_static("x-goog-api-key");
const GEMINI_MODELS_PREFIX: &str = "/v1beta/models/";
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
enum EnvKeySource {
    ProcessEnv,
    Disabled,
    #[cfg(test)]
    Fixed(Option<String>),
}

impl EnvKeySource {
    fn read(&self) -> Result<Option<String>, ProviderError> {
        match self {
            Self::ProcessEnv => match std::env::var(GEMINI_API_KEY_ENV_VAR) {
                Ok(value) => Ok(Some(value)),
                Err(std::env::VarError::NotPresent) => Ok(None),
                Err(std::env::VarError::NotUnicode(_)) => Err(ProviderError::InvalidConfig(
                    "GEMINI_API_KEY environment variable contains invalid UTF-8".to_string(),
                )),
            },
            Self::Disabled => Ok(None),
            #[cfg(test)]
            Self::Fixed(value) => Ok(value.clone()),
        }
    }
}

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
    /// Constructs a validated `GeminiProvider` from `companion_core::GeminiConfig`.
    pub fn from_config(config: &GeminiConfig) -> Result<Self, ProviderError> {
        Self::new(
            config.base_url.as_str(),
            config.api_key.clone(),
            config.timeout_secs,
        )
    }

    /// Constructs a validated `GeminiProvider` from explicit parameters (`timeout_secs > 0`).
    pub fn new(
        base_url: impl Into<String>,
        api_key: Option<String>,
        timeout_secs: u64,
    ) -> Result<Self, ProviderError> {
        Self::build(
            base_url.into(),
            api_key,
            timeout_secs,
            EnvKeySource::ProcessEnv,
        )
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
        env_key: Option<&str>,
    ) -> Result<Self, ProviderError> {
        Self::build(
            config.base_url.clone(),
            config.api_key.clone(),
            config.timeout_secs,
            EnvKeySource::Fixed(env_key.map(str::to_owned)),
        )
    }

    fn build(
        raw_base_url: String,
        api_key: Option<String>,
        timeout_secs: u64,
        env_key_source: EnvKeySource,
    ) -> Result<Self, ProviderError> {
        if timeout_secs == 0 {
            return Err(ProviderError::InvalidConfig(
                "timeout_secs must be greater than 0".to_string(),
            ));
        }

        let (base_url, normalized_base_url, normalized_base_path) =
            normalize_base_url(&raw_base_url)?;

        let validated_api_key = match api_key {
            Some(raw) => validate_api_key_string(&raw)?,
            None => None,
        };

        let timeout = Duration::from_secs(timeout_secs);
        let connect_timeout = Duration::from_secs(timeout_secs.min(MAX_CONNECT_TIMEOUT_SECS));

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
        if path_and_query.is_empty()
            || !path_and_query.bytes().all(|b| (0x21..=0x7E).contains(&b))
            || path_and_query.contains('#')
            || path_and_query.contains('\\')
        {
            return Err(ProviderError::InvalidPath(
                "request path must be non-empty printable ASCII without whitespace, controls, '#', or '\\'"
                    .to_string(),
            ));
        }

        let (raw_path, raw_query) = match path_and_query.split_once('?') {
            Some((p, q)) => (p, Some(q)),
            None => (path_and_query, None),
        };

        let Some(rest) = raw_path.strip_prefix(GEMINI_MODELS_PREFIX) else {
            return Err(ProviderError::InvalidPath(
                "request path must start with '/v1beta/models/'".to_string(),
            ));
        };
        validate_model_path_rest(rest)?;

        let sanitized_query = match raw_query {
            Some(q) => sanitize_query(q)?,
            None => None,
        };

        let expected_joined_path = format!("{}{}", self.normalized_base_path, raw_path);
        let candidate = match sanitized_query {
            Some(ref q) => format!("{}{}?{}", self.normalized_base_url, raw_path, q),
            None => format!("{}{}", self.normalized_base_url, raw_path),
        };

        self.parse_and_verify_url(
            &candidate,
            &expected_joined_path,
            sanitized_query.as_deref(),
        )
    }

    fn parse_and_verify_url(
        &self,
        candidate: &str,
        expected_path: &str,
        expected_query: Option<&str>,
    ) -> Result<Url, ProviderError> {
        let parsed = Url::parse(candidate).map_err(|_| {
            ProviderError::InvalidPath("request URL failed canonical parse".to_string())
        })?;

        if parsed.scheme() != self.base_url.scheme()
            || parsed.host_str() != self.base_url.host_str()
            || parsed.port_or_known_default() != self.base_url.port_or_known_default()
            || !parsed.username().is_empty()
            || parsed.password().is_some()
            || parsed.fragment().is_some()
            || parsed.path() != expected_path
            || parsed.query() != expected_query
        {
            return Err(ProviderError::InvalidPath(
                "request URL failed post-parse equivalence check".to_string(),
            ));
        }

        Ok(parsed)
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
            let expected_path = format!("{}/v1beta/models", self.normalized_base_path);
            let candidate = format!("{}/v1beta/models?pageSize=1", self.normalized_base_url);
            let target_url =
                self.parse_and_verify_url(&candidate, &expected_path, Some("pageSize=1"))?;

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
            let expected_path = format!("{}/v1beta/models", self.normalized_base_path);
            let candidate = format!("{}/v1beta/models?pageSize=1000", self.normalized_base_url);
            let target_url =
                self.parse_and_verify_url(&candidate, &expected_path, Some("pageSize=1000"))?;

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

fn is_loopback_url(url: &Url) -> bool {
    match url.host() {
        Some(url::Host::Ipv4(ip)) => ip.is_loopback(),
        Some(url::Host::Ipv6(ip)) => ip.is_loopback(),
        Some(url::Host::Domain(domain)) => domain.eq_ignore_ascii_case("localhost"),
        None => false,
    }
}

fn decode_ascii_hex_nibble(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        b'A'..=b'F' => Some(byte - b'A' + 10),
        _ => None,
    }
}

/// Exact RFC 3986 `pchar \ {';', '%'}` (78 bytes), matching `companion-core`.
fn is_safe_path_segment_byte(byte: u8) -> bool {
    byte.is_ascii_alphanumeric()
        || matches!(
            byte,
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
}

fn normalize_base_url(raw: &str) -> Result<(Url, String, String), ProviderError> {
    validate_upstream_url(raw).map_err(|err| ProviderError::InvalidConfig(err.to_string()))?;

    let parsed = Url::parse(raw)
        .map_err(|err| ProviderError::InvalidConfig(format!("malformed base URL: {err}")))?;
    if !matches!(parsed.origin(), url::Origin::Tuple(..)) {
        return Err(ProviderError::InvalidConfig(
            "base URL must have a tuple origin".to_string(),
        ));
    }

    let origin = parsed.origin().ascii_serialization();
    let mut base_path = parsed.path().trim_end_matches('/');
    while let Some(stripped) = base_path.strip_suffix("/v1beta") {
        base_path = stripped.trim_end_matches('/');
    }

    let normalized_str = format!("{origin}{base_path}");
    let normalized = Url::parse(&normalized_str).map_err(|_| {
        ProviderError::InvalidConfig("normalized base URL failed re-parse".to_string())
    })?;

    let expected_path = if base_path.is_empty() { "/" } else { base_path };
    if normalized.scheme() != parsed.scheme()
        || normalized.host_str() != parsed.host_str()
        || normalized.port_or_known_default() != parsed.port_or_known_default()
        || !normalized.username().is_empty()
        || normalized.password().is_some()
        || normalized.query().is_some()
        || normalized.fragment().is_some()
        || normalized.path() != expected_path
    {
        return Err(ProviderError::InvalidConfig(
            "base URL normalization invariant failed".to_string(),
        ));
    }

    Ok((normalized, normalized_str, base_path.to_string()))
}

fn validate_model_path_rest(rest: &str) -> Result<(), ProviderError> {
    if rest.is_empty() {
        return Err(ProviderError::InvalidPath(
            "request path must include a model target after '/v1beta/models/'".to_string(),
        ));
    }

    let mut decoded_segment = Vec::new();
    for segment in rest.split('/') {
        if segment.is_empty() {
            return Err(ProviderError::InvalidPath(
                "request path must not contain empty segments or a trailing '/'".to_string(),
            ));
        }

        decoded_segment.clear();
        let bytes = segment.as_bytes();
        let mut i = 0;
        while i < bytes.len() {
            let byte = bytes[i];
            if byte == b'%' {
                let (Some(&hi_b), Some(&lo_b)) = (bytes.get(i + 1), bytes.get(i + 2)) else {
                    return Err(ProviderError::InvalidPath(
                        "request path contains a truncated percent-encoding sequence".to_string(),
                    ));
                };
                let (Some(hi), Some(lo)) =
                    (decode_ascii_hex_nibble(hi_b), decode_ascii_hex_nibble(lo_b))
                else {
                    return Err(ProviderError::InvalidPath(
                        "request path contains a malformed percent-encoding sequence".to_string(),
                    ));
                };
                let decoded = (hi << 4) | lo;
                if !is_safe_path_segment_byte(decoded) {
                    return Err(ProviderError::InvalidPath(
                        "request path contains disallowed or nested percent-encoding".to_string(),
                    ));
                }
                decoded_segment.push(decoded);
                i += 3;
            } else {
                if !is_safe_path_segment_byte(byte) {
                    return Err(ProviderError::InvalidPath(
                        "request path contains disallowed characters".to_string(),
                    ));
                }
                decoded_segment.push(byte);
                i += 1;
            }
        }

        if segment == "."
            || segment == ".."
            || decoded_segment.as_slice() == b"."
            || decoded_segment.as_slice() == b".."
        {
            return Err(ProviderError::InvalidPath(
                "request path must not contain '.' or '..' dot segments".to_string(),
            ));
        }
    }

    Ok(())
}

fn decode_form_component(raw: &str) -> Result<String, ProviderError> {
    let bytes = raw.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'+' => {
                out.push(b' ');
                i += 1;
            }
            b'%' => {
                let (Some(&hi_b), Some(&lo_b)) = (bytes.get(i + 1), bytes.get(i + 2)) else {
                    return Err(ProviderError::InvalidPath(
                        "request query contains a truncated percent-encoding sequence".to_string(),
                    ));
                };
                let (Some(hi), Some(lo)) =
                    (decode_ascii_hex_nibble(hi_b), decode_ascii_hex_nibble(lo_b))
                else {
                    return Err(ProviderError::InvalidPath(
                        "request query contains a malformed percent-encoding sequence".to_string(),
                    ));
                };
                out.push((hi << 4) | lo);
                i += 3;
            }
            byte => {
                out.push(byte);
                i += 1;
            }
        }
    }
    String::from_utf8(out).map_err(|_| {
        ProviderError::InvalidPath(
            "request query contains invalid UTF-8 percent-encoding".to_string(),
        )
    })
}

fn sanitize_query(raw_query: &str) -> Result<Option<String>, ProviderError> {
    let mut serializer = url::form_urlencoded::Serializer::new(String::new());
    for pair in raw_query.split('&') {
        if pair.is_empty() {
            continue;
        }
        let (raw_key, raw_val_opt) = match pair.split_once('=') {
            Some((k, v)) => (k, Some(v)),
            None => (pair, None),
        };

        let decoded_key = decode_form_component(raw_key)?;
        if decoded_key.chars().any(char::is_control) {
            return Err(ProviderError::InvalidPath(
                "request query parameter name contains control characters".to_string(),
            ));
        }
        if decoded_key.contains('%') {
            return Err(ProviderError::InvalidPath(
                "request query parameter name contains nested percent-encoding".to_string(),
            ));
        }

        let decoded_val = match raw_val_opt {
            Some(raw_val) => {
                let value = decode_form_component(raw_val)?;
                if value.chars().any(char::is_control) {
                    return Err(ProviderError::InvalidPath(
                        "request query parameter value contains control characters".to_string(),
                    ));
                }
                Some(value)
            }
            None => None,
        };

        if decoded_key.trim().eq_ignore_ascii_case("key") {
            continue;
        }

        if decoded_key.is_empty() || !decoded_key.bytes().all(|b| (0x21..=0x7E).contains(&b)) {
            return Err(ProviderError::InvalidPath(
                "request query parameter name must be non-empty printable ASCII".to_string(),
            ));
        }

        match decoded_val {
            Some(val) => {
                serializer.append_pair(&decoded_key, &val);
            }
            None => {
                serializer.append_key_only(&decoded_key);
            }
        }
    }

    let encoded = serializer.finish();
    Ok(if encoded.is_empty() {
        None
    } else {
        Some(encoded)
    })
}

fn validate_api_key_string(raw: &str) -> Result<Option<String>, ProviderError> {
    if !raw.bytes().all(|b| (0x20..=0x7E).contains(&b)) {
        return Err(ProviderError::InvalidConfig(
            "api key contains invalid header characters".to_string(),
        ));
    }

    let trimmed = raw.trim_matches(' ');
    if trimmed.is_empty() || is_placeholder_api_key(trimmed) {
        return Ok(None);
    }

    if !trimmed.bytes().all(|b| (0x21..=0x7E).contains(&b))
        || HeaderValue::from_str(trimmed).is_err()
    {
        return Err(ProviderError::InvalidConfig(
            "api key contains invalid header characters".to_string(),
        ));
    }

    Ok(Some(trimmed.to_string()))
}

fn build_api_key_header(raw: &str) -> Result<Option<HeaderValue>, ProviderError> {
    let Some(validated) = validate_api_key_string(raw)? else {
        return Ok(None);
    };

    let mut hv = HeaderValue::from_str(&validated).map_err(|_| {
        ProviderError::InvalidConfig("api key contains invalid header characters".to_string())
    })?;
    hv.set_sensitive(true);
    Ok(Some(hv))
}

fn resolve_content_type_header(content_type: Option<&str>) -> Result<HeaderValue, ProviderError> {
    let Some(raw) = content_type else {
        return Ok(HeaderValue::from_static("application/json"));
    };

    if !raw.bytes().all(|b| (0x20..=0x7E).contains(&b)) {
        return Err(ProviderError::InvalidConfig(
            "Content-Type header contains invalid characters".to_string(),
        ));
    }

    let trimmed = raw.trim_matches(' ');
    if trimmed.is_empty() {
        return Err(ProviderError::InvalidConfig(
            "Content-Type header must not be empty".to_string(),
        ));
    }

    HeaderValue::from_str(trimmed).map_err(|_| {
        ProviderError::InvalidConfig("Content-Type header contains invalid characters".to_string())
    })
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

#[cfg(test)]
mod tests {
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
    use companion_core::{DEFAULT_TIMEOUT_SECS, GeminiConfig};
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

    fn config_for(base_url: &str, api_key: Option<&str>, timeout_secs: u64) -> GeminiConfig {
        GeminiConfig {
            base_url: base_url.to_string(),
            api_key: api_key.map(str::to_owned),
            timeout_secs,
            ..GeminiConfig::default()
        }
    }

    fn provider_for(
        base_url: &str,
        api_key: Option<&str>,
        env_key: Option<&str>,
    ) -> GeminiProvider {
        let cfg = config_for(base_url, api_key, DEFAULT_TIMEOUT_SECS);
        GeminiProvider::with_env_api_key(&cfg, env_key).expect("construct test provider")
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
                path_and_query:
                    "/v1beta/models/gemini-4-hydrogen:generateContent?key=LEAK_ME&alt=json"
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
            // Source 1: configured api_key (eager validation in new/from_config)
            let cfg = config_for("http://127.0.0.1:4141", Some(bad), DEFAULT_TIMEOUT_SECS);
            let err = GeminiProvider::from_config(&cfg)
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
        let provider = GeminiProvider::from_config(&GeminiConfig {
            base_url: "https://generativelanguage.googleapis.com/v1beta/".to_string(),
            api_key: Some("AIzaSyTopSecretConfigured9999".to_string()),
            timeout_secs: 900,
            ..GeminiConfig::default()
        })
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

        let cfg = config_for(&slow_server.base_url, Some("AIzaSyTimeoutSecret99"), 1);
        let provider =
            GeminiProvider::with_env_api_key(&cfg, None).expect("construct 1s timeout provider");

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
            elapsed < Duration::from_millis(4000),
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
            let cfg_no_key = config_for(&server.base_url, None, DEFAULT_TIMEOUT_SECS);
            let env_provider = GeminiProvider::with_env_api_key(&cfg_no_key, Some(bad))
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
        let cfg_blank = config_for(&server.base_url, Some("   "), DEFAULT_TIMEOUT_SECS);
        let p_trimmed_env =
            GeminiProvider::with_env_api_key(&cfg_blank, Some("  AIzaSyValid1234  "))
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
                    30,
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
}
