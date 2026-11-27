use std::collections::VecDeque;
use std::fmt;
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};
use std::time::Duration;

use bytes::Bytes;
use companion_core::{
    CURATED_CODEX_MODELS, CodexConfig, CodexCredentials, CoreError, DEFAULT_CODEX_MODEL,
    is_codex_model, validate_upstream_url, validate_url_path,
};
use futures_util::{Stream, StreamExt};
use http::header::{self, HeaderName, HeaderValue};
use serde_json::json;
use url::{Origin, Url};

use crate::codex::auth::CodexAuth;
use crate::codex::translate::{
    CodexSseTranslator, UnaryGeminiAggregator, sanitize_codex_error_json,
    translate_gemini_to_codex_request,
};
use crate::gemini::url::is_loopback_url;
use crate::{
    BoxFuture, ByteStream, ForwardRequest, ForwardResponse, GeminiModelInfo, GeminiModelList,
    ProviderAdapter, ProviderError,
};

/// Centralized `OpenAI-Beta` header value sent to `chatgpt.com/backend-api/codex/responses`.
pub const CODEX_OPENAI_BETA_HEADER: &str = "responses=experimental";
/// Client version header satisfying `gpt-6.2-*` `minimal_client_version: "0.166.0"`.
pub const CODEX_CLIENT_VERSION: &str = "0.166.0";
/// Originator header matching Codex CLI / OpenClaw subscription flow.
pub const CODEX_ORIGINATOR_HEADER: &str = "codex_cli_rs";

const GEMINI_MODELS_PREFIX: &str = "/v1beta/models/";
const CHATGPT_ACCOUNT_ID_HEADER: HeaderName = HeaderName::from_static("chatgpt-account-id");
const OPENAI_BETA_HEADER: HeaderName = HeaderName::from_static("openai-beta");
const ORIGINATOR_HEADER: HeaderName = HeaderName::from_static("originator");
const VERSION_HEADER: HeaderName = HeaderName::from_static("version");

const MAX_CONNECT_TIMEOUT_SECS: u64 = 30;
const HEALTH_CHECK_TIMEOUT_SECS: u64 = 15;
const MAX_ERROR_BODY_BYTES: usize = 16 * 1024;
const CODEX_SESSION_EXPIRED_MSG: &str = "OpenAI Codex session expired — sign in again";

/// Direct 1-hop OpenAI Codex Responses API adapter implementing [`ProviderAdapter`].
#[derive(Clone)]
pub struct CodexProvider {
    client: reqwest::Client,
    responses_url: Url,
    normalized_base_url: String,
    auth: Arc<dyn CodexAuth>,
    timeout: Duration,
    default_model: String,
}

impl fmt::Debug for CodexProvider {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("CodexProvider")
            .field("base_url", &self.normalized_base_url)
            .field("default_model", &self.default_model)
            .field("timeout_secs", &self.timeout.as_secs())
            .finish_non_exhaustive()
    }
}

impl CodexProvider {
    /// Constructs a validated `CodexProvider` from [`CodexConfig`] and a [`CodexAuth`] handle.
    pub fn from_config(
        config: &CodexConfig,
        auth: Arc<dyn CodexAuth>,
    ) -> Result<Self, ProviderError> {
        Self::from_config_with_timeout(config, auth, Duration::from_secs(config.timeout_secs))
    }

    /// Constructs a validated `CodexProvider` from [`CodexConfig`] using an explicit timeout.
    pub fn from_config_with_timeout(
        config: &CodexConfig,
        auth: Arc<dyn CodexAuth>,
        timeout: Duration,
    ) -> Result<Self, ProviderError> {
        let mut provider = Self::new_with_timeout(config.base_url.as_str(), auth, timeout)?;
        if is_codex_model(&config.default_model) {
            provider.default_model = config.default_model.clone();
        }
        Ok(provider)
    }

    /// Constructs a validated `CodexProvider` from explicit parameters (`timeout_secs > 0`).
    pub fn new(
        base_url: impl Into<String>,
        auth: Arc<dyn CodexAuth>,
        timeout_secs: u64,
    ) -> Result<Self, ProviderError> {
        Self::new_with_timeout(base_url, auth, Duration::from_secs(timeout_secs))
    }

    /// Constructs a validated `CodexProvider` from explicit parameters (`!timeout.is_zero()`).
    pub fn new_with_timeout(
        base_url: impl Into<String>,
        auth: Arc<dyn CodexAuth>,
        timeout: Duration,
    ) -> Result<Self, ProviderError> {
        if timeout.is_zero() {
            return Err(ProviderError::InvalidConfig(
                "timeout must be greater than 0".to_string(),
            ));
        }

        let (base_url, normalized_base_url, responses_url) =
            normalize_codex_base_url(&base_url.into())?;

        let connect_timeout = timeout.min(Duration::from_secs(MAX_CONNECT_TIMEOUT_SECS));

        let mut builder = reqwest::Client::builder()
            .timeout(timeout)
            .connect_timeout(connect_timeout)
            .redirect(reqwest::redirect::Policy::none())
            .user_agent(concat!("agy-companion/", env!("CARGO_PKG_VERSION")));

        // Bypass ambient HTTP_PROXY/HTTPS_PROXY strictly on loopback upstreams (S3).
        if is_loopback_url(&base_url) {
            builder = builder.no_proxy();
        }

        let client = builder.build().map_err(|err| {
            ProviderError::InvalidConfig(format!(
                "failed to initialize Codex HTTP client: {}",
                err.without_url()
            ))
        })?;

        Ok(Self {
            client,
            responses_url,
            normalized_base_url,
            auth,
            timeout,
            default_model: DEFAULT_CODEX_MODEL.to_string(),
        })
    }

    pub fn normalized_base_url(&self) -> &str {
        &self.normalized_base_url
    }

    pub fn timeout(&self) -> Duration {
        self.timeout
    }

    async fn send_responses_request(
        &self,
        body: Bytes,
        creds: &CodexCredentials,
        timeout_override: Option<Duration>,
    ) -> Result<reqwest::Response, ProviderError> {
        let auth_header = build_bearer_header(creds.access_token.expose())?;
        let account_header = build_account_id_header(&creds.account_id)?;

        let mut req = self
            .client
            .post(self.responses_url.clone())
            .header(header::AUTHORIZATION, auth_header)
            .header(CHATGPT_ACCOUNT_ID_HEADER, account_header)
            .header(
                OPENAI_BETA_HEADER,
                HeaderValue::from_static(CODEX_OPENAI_BETA_HEADER),
            )
            .header(
                ORIGINATOR_HEADER,
                HeaderValue::from_static(CODEX_ORIGINATOR_HEADER),
            )
            .header(
                VERSION_HEADER,
                HeaderValue::from_static(CODEX_CLIENT_VERSION),
            )
            .header(
                header::CONTENT_TYPE,
                HeaderValue::from_static("application/json"),
            )
            .header(
                header::ACCEPT,
                HeaderValue::from_static("text/event-stream"),
            )
            .body(body);

        if let Some(timeout) = timeout_override {
            req = req.timeout(timeout);
        }

        req.send().await.map_err(sanitize_reqwest_error)
    }

    async fn send_with_single_401_retry(
        &self,
        body: Bytes,
        timeout_override: Option<Duration>,
    ) -> Result<reqwest::Response, ProviderError> {
        let creds = self.auth.valid_credentials(false).await?;
        let response = self
            .send_responses_request(body.clone(), &creds, timeout_override)
            .await?;

        if response.status() == reqwest::StatusCode::UNAUTHORIZED {
            tracing::debug!(
                base_url = %self.normalized_base_url,
                "Codex upstream returned 401; attempting single forced token refresh and retry"
            );
            let refreshed = self.auth.valid_credentials(true).await?;
            return self
                .send_responses_request(body, &refreshed, timeout_override)
                .await;
        }

        Ok(response)
    }
}

impl ProviderAdapter for CodexProvider {
    fn name(&self) -> &'static str {
        "openai-codex"
    }

    fn forward_gemini(
        &self,
        req: ForwardRequest,
    ) -> BoxFuture<'_, Result<ForwardResponse, ProviderError>> {
        Box::pin(async move {
            let (model, is_streaming) = parse_gemini_codex_path(&req.path_and_query)?;
            let translated = translate_gemini_to_codex_request(&model, &req.body)?;
            let codex_body = Bytes::from(serde_json::to_vec(&translated).map_err(|_| {
                ProviderError::InvalidRequest(
                    "failed to serialize translated Codex request".to_string(),
                )
            })?);

            tracing::debug!(
                base_url = %self.normalized_base_url,
                model = %model,
                is_streaming,
                body_len = codex_body.len(),
                "forwarding Gemini request to OpenAI Codex Responses API"
            );

            let response = self.send_with_single_401_retry(codex_body, None).await?;
            let status = response.status();

            if !status.is_success() {
                let err_body = read_bounded_body(response, MAX_ERROR_BODY_BYTES).await;
                let (mapped_status, err_json) =
                    sanitize_codex_error_json(status.as_u16(), &err_body);
                let payload = Bytes::from(serde_json::to_vec(&err_json).unwrap_or_else(|_| {
                    br#"{"error":{"code":502,"message":"OpenAI Codex upstream service error","status":"INTERNAL"}}"#.to_vec()
                }));
                return Ok(ForwardResponse {
                    status: mapped_status,
                    content_type: Some("application/json".to_string()),
                    stream: single_bytes_stream(payload),
                });
            }

            if is_streaming {
                let raw_stream = Box::pin(response.bytes_stream());
                let translated_stream: ByteStream = Box::pin(CodexSseByteStream::new(raw_stream));
                Ok(ForwardResponse {
                    status: 200,
                    content_type: Some("text/event-stream".to_string()),
                    stream: translated_stream,
                })
            } else {
                let mut raw_stream = response.bytes_stream();
                let mut translator = CodexSseTranslator::new();
                let mut aggregator = UnaryGeminiAggregator::new();

                while let Some(chunk_res) = raw_stream.next().await {
                    let bytes = chunk_res.map_err(sanitize_reqwest_error)?;
                    for translated_chunk in translator.push_chunk(&bytes)? {
                        aggregator.push_chunk(translated_chunk)?;
                    }
                }
                for translated_chunk in translator.finish()? {
                    aggregator.push_chunk(translated_chunk)?;
                }

                let (final_status, final_json) = aggregator.finish()?;
                let payload = Bytes::from(serde_json::to_vec(&final_json).map_err(|_| {
                    ProviderError::Upstream(
                        "failed to serialize translated Codex unary response".to_string(),
                    )
                })?);
                Ok(ForwardResponse {
                    status: final_status,
                    content_type: Some("application/json".to_string()),
                    stream: single_bytes_stream(payload),
                })
            }
        })
    }

    fn health_check(&self) -> BoxFuture<'_, Result<(), ProviderError>> {
        Box::pin(async move {
            let probe_body = Bytes::from(
                serde_json::to_vec(&json!({
                    "model": self.default_model,
                    "instructions": "Reply with ok.",
                    "input": [{
                        "type": "message",
                        "role": "user",
                        "content": [{ "type": "input_text", "text": "ping" }]
                    }],
                    "store": false,
                    "stream": true,
                    "reasoning": { "effort": "low", "summary": "auto" }
                }))
                .map_err(|_| {
                    ProviderError::InvalidConfig(
                        "failed to build Codex health check payload".to_string(),
                    )
                })?,
            );

            tracing::debug!(
                base_url = %self.normalized_base_url,
                model = %self.default_model,
                "running OpenAI Codex upstream health check"
            );

            let timeout = self
                .timeout
                .min(Duration::from_secs(HEALTH_CHECK_TIMEOUT_SECS));
            let response = self
                .send_with_single_401_retry(probe_body, Some(timeout))
                .await?;

            if response.status().is_success() {
                return Ok(());
            }

            let code = response.status().as_u16();
            match code {
                401 => Err(ProviderError::AuthExpired(
                    CODEX_SESSION_EXPIRED_MSG.to_string(),
                )),
                429 => Err(ProviderError::Upstream(
                    "ChatGPT Codex quota limit reached".to_string(),
                )),
                _ => Err(ProviderError::Upstream(format!(
                    "OpenAI Codex health check failed with HTTP {code}"
                ))),
            }
        })
    }

    fn list_models(
        &self,
        _api_key_override: Option<String>,
    ) -> BoxFuture<'_, Result<GeminiModelList, ProviderError>> {
        Box::pin(async move {
            let models = CURATED_CODEX_MODELS
                .iter()
                .map(|m| GeminiModelInfo {
                    name: format!("models/{}", m.id),
                    display_name: Some(m.display_name.to_string()),
                    description: None,
                    input_token_limit: Some(m.context_window),
                    output_token_limit: Some(m.max_output_tokens),
                    supported_generation_methods: vec!["generateContent".to_string()],
                    thinking: Some(true),
                })
                .collect();
            Ok(GeminiModelList { models })
        })
    }
}

struct CodexSseByteStream {
    inner: Pin<Box<dyn Stream<Item = Result<Bytes, reqwest::Error>> + Send>>,
    translator: CodexSseTranslator,
    buffered_frames: VecDeque<Bytes>,
    inner_done: bool,
    failed: bool,
}

impl CodexSseByteStream {
    fn new(inner: Pin<Box<dyn Stream<Item = Result<Bytes, reqwest::Error>> + Send>>) -> Self {
        Self {
            inner,
            translator: CodexSseTranslator::new(),
            buffered_frames: VecDeque::new(),
            inner_done: false,
            failed: false,
        }
    }
}

impl Stream for CodexSseByteStream {
    type Item = Result<Bytes, ProviderError>;

    fn poll_next(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        let this = self.get_mut();

        loop {
            if let Some(frame) = this.buffered_frames.pop_front() {
                return Poll::Ready(Some(Ok(frame)));
            }
            if this.failed || this.inner_done {
                return Poll::Ready(None);
            }

            match Pin::new(&mut this.inner).poll_next(cx) {
                Poll::Pending => return Poll::Pending,
                Poll::Ready(Some(Ok(chunk))) => match this.translator.push_chunk(&chunk) {
                    Ok(translated) => {
                        for item in translated {
                            match CodexSseTranslator::format_gemini_sse_frame(&item) {
                                Ok(frame) => this.buffered_frames.push_back(frame),
                                Err(err) => {
                                    this.failed = true;
                                    return Poll::Ready(Some(Err(err)));
                                }
                            }
                        }
                    }
                    Err(err) => {
                        this.failed = true;
                        return Poll::Ready(Some(Err(err)));
                    }
                },
                Poll::Ready(Some(Err(req_err))) => {
                    this.failed = true;
                    return Poll::Ready(Some(Err(sanitize_reqwest_error(req_err))));
                }
                Poll::Ready(None) => {
                    this.inner_done = true;
                    match this.translator.finish() {
                        Ok(translated) => {
                            for item in translated {
                                match CodexSseTranslator::format_gemini_sse_frame(&item) {
                                    Ok(frame) => this.buffered_frames.push_back(frame),
                                    Err(err) => {
                                        this.failed = true;
                                        return Poll::Ready(Some(Err(err)));
                                    }
                                }
                            }
                        }
                        Err(err) => {
                            this.failed = true;
                            return Poll::Ready(Some(Err(err)));
                        }
                    }
                }
            }
        }
    }
}

fn single_bytes_stream(bytes: Bytes) -> ByteStream {
    Box::pin(futures_util::stream::once(async move { Ok(bytes) }))
}

fn normalize_codex_base_url(raw: &str) -> Result<(Url, String, Url), ProviderError> {
    validate_upstream_url(raw).map_err(|err| ProviderError::InvalidConfig(err.to_string()))?;

    let parsed = Url::parse(raw)
        .map_err(|err| ProviderError::InvalidConfig(format!("malformed Codex base URL: {err}")))?;
    if !matches!(parsed.origin(), Origin::Tuple(..)) {
        return Err(ProviderError::InvalidConfig(
            "Codex base URL must have a tuple origin".to_string(),
        ));
    }

    let origin = parsed.origin().ascii_serialization();
    let mut base_path = parsed.path().trim_end_matches('/');
    while let Some(stripped) = base_path.strip_suffix("/responses") {
        base_path = stripped.trim_end_matches('/');
    }

    let normalized_str = format!("{origin}{base_path}");
    let normalized = Url::parse(&normalized_str).map_err(|_| {
        ProviderError::InvalidConfig("normalized Codex base URL failed re-parse".to_string())
    })?;

    let expected_base_path = if base_path.is_empty() { "/" } else { base_path };
    if normalized.scheme() != parsed.scheme()
        || normalized.host_str() != parsed.host_str()
        || normalized.port_or_known_default() != parsed.port_or_known_default()
        || !normalized.username().is_empty()
        || normalized.password().is_some()
        || normalized.query().is_some()
        || normalized.fragment().is_some()
        || normalized.path() != expected_base_path
    {
        return Err(ProviderError::InvalidConfig(
            "Codex base URL normalization invariant failed".to_string(),
        ));
    }

    let responses_str = format!("{normalized_str}/responses");
    let responses_url = Url::parse(&responses_str).map_err(|_| {
        ProviderError::InvalidConfig("Codex responses URL failed canonical parse".to_string())
    })?;
    let expected_responses_path = format!("{base_path}/responses");
    if responses_url.scheme() != normalized.scheme()
        || responses_url.host_str() != normalized.host_str()
        || responses_url.port_or_known_default() != normalized.port_or_known_default()
        || !responses_url.username().is_empty()
        || responses_url.password().is_some()
        || responses_url.query().is_some()
        || responses_url.fragment().is_some()
        || responses_url.path() != expected_responses_path
    {
        return Err(ProviderError::InvalidConfig(
            "Codex responses URL failed post-parse equivalence check".to_string(),
        ));
    }

    Ok((normalized, normalized_str, responses_url))
}

fn parse_gemini_codex_path(path_and_query: &str) -> Result<(String, bool), ProviderError> {
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

    let raw_path = path_and_query
        .split_once('?')
        .map_or(path_and_query, |(p, _)| p);

    let Some(rest) = raw_path.strip_prefix(GEMINI_MODELS_PREFIX) else {
        return Err(ProviderError::InvalidPath(
            "request path must start with '/v1beta/models/'".to_string(),
        ));
    };
    if rest.is_empty() || rest.split('/').any(str::is_empty) {
        return Err(ProviderError::InvalidPath(
            "request path must include a valid model target after '/v1beta/models/'".to_string(),
        ));
    }

    validate_url_path(raw_path).map_err(|err| match err {
        CoreError::InvalidBaseUrl { reason } => {
            ProviderError::InvalidPath(reason.replace("base URL path", "request path"))
        }
        other => ProviderError::InvalidPath(other.to_string()),
    })?;

    let target = rest.strip_prefix("models/").unwrap_or(rest);
    let Some((model, action)) = target.rsplit_once(':') else {
        return Err(ProviderError::InvalidPath(
            "request path must include an action (':streamGenerateContent' or ':generateContent')"
                .to_string(),
        ));
    };

    let is_streaming = match action {
        "streamGenerateContent" => true,
        "generateContent" => false,
        _ => {
            return Err(ProviderError::InvalidPath(
                "unsupported action for OpenAI Codex; only :streamGenerateContent and :generateContent are supported"
                    .to_string(),
            ));
        }
    };

    if !is_codex_model(model) {
        return Err(ProviderError::InvalidPath(
            "unsupported OpenAI Codex model".to_string(),
        ));
    }

    Ok((model.to_string(), is_streaming))
}

fn build_bearer_header(access_token: &str) -> Result<HeaderValue, ProviderError> {
    if access_token.is_empty() || !access_token.bytes().all(|b| (0x21..=0x7E).contains(&b)) {
        return Err(ProviderError::InvalidConfig(
            "Codex access token contains invalid header characters".to_string(),
        ));
    }
    let mut hv = HeaderValue::from_str(&format!("Bearer {access_token}")).map_err(|_| {
        ProviderError::InvalidConfig(
            "Codex access token contains invalid header characters".to_string(),
        )
    })?;
    hv.set_sensitive(true);
    Ok(hv)
}

fn build_account_id_header(account_id: &str) -> Result<HeaderValue, ProviderError> {
    if account_id.is_empty() || !account_id.bytes().all(|b| (0x21..=0x7E).contains(&b)) {
        return Err(ProviderError::InvalidConfig(
            "Codex account_id contains invalid header characters".to_string(),
        ));
    }
    let mut hv = HeaderValue::from_str(account_id).map_err(|_| {
        ProviderError::InvalidConfig(
            "Codex account_id contains invalid header characters".to_string(),
        )
    })?;
    hv.set_sensitive(true);
    Ok(hv)
}

fn sanitize_reqwest_error(err: reqwest::Error) -> ProviderError {
    let sanitized = err.without_url();
    if sanitized.is_timeout() {
        ProviderError::Upstream("upstream Codex request timed out".to_string())
    } else if sanitized.is_connect() {
        ProviderError::Upstream("failed to connect to upstream Codex".to_string())
    } else {
        ProviderError::Upstream(format!("upstream Codex transport error: {sanitized}"))
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

#[cfg(test)]
mod tests {
    use std::net::{Ipv4Addr, SocketAddr};
    use std::sync::Mutex;
    use std::sync::atomic::{AtomicUsize, Ordering};

    use axum::Router;
    use axum::body::Body;
    use axum::extract::State;
    use axum::http::{HeaderMap, StatusCode};
    use axum::response::{IntoResponse, Response};
    use axum::routing::post;
    use companion_core::{AUTH_SCHEMA_VERSION, CodexAuthMode, SecretString};
    use tokio::net::TcpListener;

    use super::*;

    struct StubCodexAuth {
        initial: Mutex<Option<Result<CodexCredentials, String>>>,
        refreshed: Mutex<Option<Result<CodexCredentials, String>>>,
        force_refresh_calls: AtomicUsize,
        normal_calls: AtomicUsize,
    }

    impl StubCodexAuth {
        fn with_tokens(initial_token: &str, refreshed_token: &str) -> Self {
            Self {
                initial: Mutex::new(Some(Ok(sample_creds(initial_token, "acct_test_1234")))),
                refreshed: Mutex::new(Some(Ok(sample_creds(refreshed_token, "acct_test_1234")))),
                force_refresh_calls: AtomicUsize::new(0),
                normal_calls: AtomicUsize::new(0),
            }
        }

        fn unauthenticated() -> Self {
            Self {
                initial: Mutex::new(None),
                refreshed: Mutex::new(None),
                force_refresh_calls: AtomicUsize::new(0),
                normal_calls: AtomicUsize::new(0),
            }
        }
    }

    fn sample_creds(token: &str, account_id: &str) -> CodexCredentials {
        CodexCredentials {
            schema_version: AUTH_SCHEMA_VERSION,
            auth_mode: CodexAuthMode::ChatGptOAuth,
            access_token: SecretString::new(token),
            refresh_token: SecretString::new("refresh_tok_secret_value"),
            id_token: None,
            account_id: account_id.to_string(),
            expires_at_ms: 4_102_444_800_000,
            plan_type: Some("pro".to_string()),
            email_hint: Some("u***@example.com".to_string()),
            last_refresh_utc: "2026-11-26T00:00:00Z".to_string(),
        }
    }

    impl CodexAuth for StubCodexAuth {
        fn valid_credentials(
            &self,
            force_refresh: bool,
        ) -> BoxFuture<'_, Result<CodexCredentials, ProviderError>> {
            Box::pin(async move {
                if force_refresh {
                    self.force_refresh_calls.fetch_add(1, Ordering::SeqCst);
                    match self.refreshed.lock().unwrap().as_ref() {
                        Some(Ok(c)) => Ok(c.clone()),
                        Some(Err(msg)) => Err(ProviderError::AuthExpired(msg.clone())),
                        None => Err(ProviderError::NotAuthenticated(
                            "OpenAI Codex is not signed in".to_string(),
                        )),
                    }
                } else {
                    self.normal_calls.fetch_add(1, Ordering::SeqCst);
                    match self.initial.lock().unwrap().as_ref() {
                        Some(Ok(c)) => Ok(c.clone()),
                        Some(Err(msg)) => Err(ProviderError::AuthExpired(msg.clone())),
                        None => Err(ProviderError::NotAuthenticated(
                            "OpenAI Codex is not signed in".to_string(),
                        )),
                    }
                }
            })
        }
    }

    #[derive(Clone, Default)]
    struct MockCodexState {
        calls: Arc<AtomicUsize>,
        captured_headers: Arc<Mutex<Vec<HashMap<String, String>>>>,
        captured_bodies: Arc<Mutex<Vec<serde_json::Value>>>,
        mode: Arc<Mutex<MockMode>>,
    }

    #[derive(Clone, Default)]
    enum MockMode {
        #[default]
        SuccessSse,
        UnauthorizedOnceThenSuccess,
        AlwaysUnauthorized,
        RateLimitedWithSecret,
        Redirect,
    }

    use std::collections::HashMap;

    async fn mock_responses_handler(
        State(state): State<MockCodexState>,
        headers: HeaderMap,
        body: Bytes,
    ) -> Response {
        let call_num = state.calls.fetch_add(1, Ordering::SeqCst);
        let mut hmap = HashMap::new();
        for (k, v) in &headers {
            if let Ok(s) = v.to_str() {
                hmap.insert(k.as_str().to_ascii_lowercase(), s.to_string());
            }
        }
        state.captured_headers.lock().unwrap().push(hmap);
        if let Ok(val) = serde_json::from_slice::<serde_json::Value>(&body) {
            state.captured_bodies.lock().unwrap().push(val);
        }

        let mode = state.mode.lock().unwrap().clone();
        match mode {
            MockMode::SuccessSse => sse_ok_response(),
            MockMode::UnauthorizedOnceThenSuccess => {
                if call_num == 0 {
                    (
                        StatusCode::UNAUTHORIZED,
                        [(header::CONTENT_TYPE, "application/json")],
                        r#"{"error":{"code":"token_expired","message":"Bearer expired_tok_abc"}}"#,
                    )
                        .into_response()
                } else {
                    sse_ok_response()
                }
            }
            MockMode::AlwaysUnauthorized => (
                StatusCode::UNAUTHORIZED,
                [(header::CONTENT_TYPE, "application/json")],
                r#"{"error":{"code":"token_expired","message":"Bearer leaked_secret_tok_999"}}"#,
            )
                .into_response(),
            MockMode::RateLimitedWithSecret => (
                StatusCode::TOO_MANY_REQUESTS,
                [(header::CONTENT_TYPE, "application/json")],
                r#"{"error":{"code":"rate_limit_exceeded","message":"secret-refresh-token-do-not-echo"}}"#,
            )
                .into_response(),
            MockMode::Redirect => Response::builder()
                .status(StatusCode::FOUND)
                .header(header::LOCATION, "https://evil.example.com/steal")
                .body(Body::empty())
                .unwrap(),
        }
    }

    fn sse_ok_response() -> Response {
        let payload = concat!(
            "data: {\"type\":\"response.reasoning_summary_text.delta\",\"delta\":\"Plan step 1.\"}\n\n",
            "data: {\"type\":\"response.output_text.delta\",\"delta\":\"Hello \"}\n\n",
            "data: {\"type\":\"response.output_text.delta\",\"delta\":\"world!\"}\n\n",
            "data: {\"type\":\"response.completed\",\"response\":{\"usage\":{\"input_tokens\":500,\"input_tokens_details\":{\"cached_tokens\":300},\"output_tokens\":40,\"output_tokens_details\":{\"reasoning_tokens\":15},\"total_tokens\":540}}}\n\n",
            "data: [DONE]\n\n"
        );
        Response::builder()
            .status(StatusCode::OK)
            .header(header::CONTENT_TYPE, "text/event-stream")
            .body(Body::from(payload))
            .unwrap()
    }

    async fn spawn_mock_codex(mode: MockMode) -> (String, MockCodexState) {
        let state = MockCodexState {
            mode: Arc::new(Mutex::new(mode)),
            ..Default::default()
        };
        let app = Router::new()
            .route("/backend-api/codex/responses", post(mock_responses_handler))
            .with_state(state.clone());
        let listener = TcpListener::bind(SocketAddr::from((Ipv4Addr::LOCALHOST, 0)))
            .await
            .expect("bind loopback");
        let port = listener.local_addr().unwrap().port();
        tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });
        tokio::task::yield_now().await;
        (format!("http://127.0.0.1:{port}/backend-api/codex"), state)
    }

    async fn collect_stream(mut stream: ByteStream) -> Vec<u8> {
        let mut out = Vec::new();
        while let Some(item) = stream.next().await {
            out.extend_from_slice(&item.expect("stream chunk ok"));
        }
        out
    }

    #[test]
    fn enforces_s3_base_url_and_path_validation() {
        let auth: Arc<dyn CodexAuth> = Arc::new(StubCodexAuth::unauthenticated());
        assert!(
            CodexProvider::new("http://chatgpt.com/backend-api/codex", auth.clone(), 30).is_err()
        );
        assert!(
            CodexProvider::new(
                "https://user:pass@chatgpt.com/backend-api/codex",
                auth.clone(),
                30
            )
            .is_err()
        );
        assert!(
            CodexProvider::new("https://chatgpt.com/backend-api/codex", auth.clone(), 0).is_err()
        );
        assert!(CodexProvider::new("http://127.0.0.1:9999/codex", auth.clone(), 30).is_ok());

        let (model, streaming) =
            parse_gemini_codex_path("/v1beta/models/gpt-6.2-sol:streamGenerateContent?alt=sse")
                .expect("stream path");
        assert_eq!(model, "gpt-6.2-sol");
        assert!(streaming);

        let (model, streaming) =
            parse_gemini_codex_path("/v1beta/models/gpt-6.2-astra:generateContent")
                .expect("unary path");
        assert_eq!(model, "gpt-6.2-astra");
        assert!(!streaming);

        assert!(parse_gemini_codex_path("/v1beta/models/gpt-6.2-sol:countTokens").is_err());
        assert!(
            parse_gemini_codex_path("/v1beta/models/gemini-4-hydrogen:generateContent").is_err()
        );
        assert!(parse_gemini_codex_path("/v1beta/models/../etc:generateContent").is_err());
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn streams_and_aggregates_unary_with_required_codex_headers() {
        let (base_url, mock) = spawn_mock_codex(MockMode::SuccessSse).await;
        let auth = Arc::new(StubCodexAuth::with_tokens("access_tok_1", "access_tok_2"));
        let provider = CodexProvider::new(&base_url, auth, 30).expect("provider");

        // 1. Streaming SSE
        let req = ForwardRequest {
            path_and_query: "/v1beta/models/gpt-6.2-astra:streamGenerateContent?alt=sse"
                .to_string(),
            body: Bytes::from(
                json!({
                    "contents": [{ "role": "user", "parts": [{ "text": "hi" }] }],
                    "generationConfig": { "thinkingConfig": { "thinkingLevel": "HIGH" } }
                })
                .to_string(),
            ),
            content_type: Some("application/json".to_string()),
            api_key_override: None,
        };
        let resp = provider.forward_gemini(req).await.expect("forward sse");
        assert_eq!(resp.status, 200);
        assert_eq!(resp.content_type.as_deref(), Some("text/event-stream"));
        let raw = String::from_utf8(collect_stream(resp.stream).await).expect("utf8");
        assert!(raw.contains("\"thought\":true"));
        assert!(raw.contains("Plan step 1."));
        assert!(raw.contains("Hello "));
        assert!(raw.contains("world!"));
        assert!(raw.contains("\"cachedContentTokenCount\":300"));
        assert!(raw.contains("\"thoughtsTokenCount\":15"));

        // Verify required Codex headers were sent accurately.
        let hdrs = mock.captured_headers.lock().unwrap()[0].clone();
        assert_eq!(
            hdrs.get("authorization").map(String::as_str),
            Some("Bearer access_tok_1")
        );
        assert_eq!(
            hdrs.get("chatgpt-account-id").map(String::as_str),
            Some("acct_test_1234")
        );
        assert_eq!(
            hdrs.get("openai-beta").map(String::as_str),
            Some(CODEX_OPENAI_BETA_HEADER)
        );
        assert_eq!(
            hdrs.get("originator").map(String::as_str),
            Some(CODEX_ORIGINATOR_HEADER)
        );
        assert_eq!(
            hdrs.get("version").map(String::as_str),
            Some(CODEX_CLIENT_VERSION)
        );

        // 2. Unary :generateContent aggregates translated stream into single Gemini JSON.
        let unary_req = ForwardRequest {
            path_and_query: "/v1beta/models/gpt-6.2-sol:generateContent".to_string(),
            body: Bytes::from(
                json!({ "contents": [{ "role": "user", "parts": [{ "text": "hi" }] }] })
                    .to_string(),
            ),
            content_type: Some("application/json".to_string()),
            api_key_override: None,
        };
        let unary_resp = provider
            .forward_gemini(unary_req)
            .await
            .expect("forward unary");
        assert_eq!(unary_resp.status, 200);
        assert_eq!(unary_resp.content_type.as_deref(), Some("application/json"));
        let unary_bytes = collect_stream(unary_resp.stream).await;
        let unary_json: serde_json::Value =
            serde_json::from_slice(&unary_bytes).expect("valid unary json");
        assert_eq!(
            unary_json["candidates"][0]["content"]["parts"][0],
            json!({ "text": "Plan step 1.", "thought": true })
        );
        assert_eq!(
            unary_json["candidates"][0]["content"]["parts"][1],
            json!({ "text": "Hello world!" })
        );
        assert_eq!(unary_json["candidates"][0]["finishReason"], "STOP");
        assert_eq!(unary_json["usageMetadata"]["promptTokenCount"], 500);
        assert_eq!(unary_json["usageMetadata"]["cachedContentTokenCount"], 300);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn retries_401_exactly_once_with_forced_refresh_and_sanitizes_persistent_errors() {
        // Case A: 401 once -> forced refresh -> succeeds on 2nd attempt.
        let (base_url, mock) = spawn_mock_codex(MockMode::UnauthorizedOnceThenSuccess).await;
        let auth = Arc::new(StubCodexAuth::with_tokens("stale_tok", "fresh_tok"));
        let provider = CodexProvider::new(&base_url, auth.clone(), 30).expect("provider");
        let req = ForwardRequest {
            path_and_query: "/v1beta/models/gpt-6.2-sol:generateContent".to_string(),
            body: Bytes::from(r#"{"contents":[{"role":"user","parts":[{"text":"hi"}]}]}"#),
            content_type: None,
            api_key_override: None,
        };
        let resp = provider.forward_gemini(req).await.expect("retry succeeds");
        assert_eq!(resp.status, 200);
        assert_eq!(mock.calls.load(Ordering::SeqCst), 2);
        assert_eq!(auth.force_refresh_calls.load(Ordering::SeqCst), 1);
        let hdrs = mock.captured_headers.lock().unwrap().clone();
        assert_eq!(
            hdrs[0].get("authorization").map(String::as_str),
            Some("Bearer stale_tok")
        );
        assert_eq!(
            hdrs[1].get("authorization").map(String::as_str),
            Some("Bearer fresh_tok")
        );

        // Case B: Persistent 401 stops after 1 retry (2 total calls) and returns sanitized 401.
        let (base_url_b, mock_b) = spawn_mock_codex(MockMode::AlwaysUnauthorized).await;
        let auth_b = Arc::new(StubCodexAuth::with_tokens("stale_b", "still_bad_b"));
        let provider_b = CodexProvider::new(&base_url_b, auth_b.clone(), 30).expect("provider b");
        let resp_b = provider_b
            .forward_gemini(ForwardRequest {
                path_and_query: "/v1beta/models/gpt-6.2-sol:generateContent".to_string(),
                body: Bytes::from(r#"{"contents":[{"role":"user","parts":[{"text":"hi"}]}]}"#),
                content_type: None,
                api_key_override: None,
            })
            .await
            .expect("persistent 401 sanitized");
        assert_eq!(resp_b.status, 401);
        assert_eq!(mock_b.calls.load(Ordering::SeqCst), 2);
        assert_eq!(auth_b.force_refresh_calls.load(Ordering::SeqCst), 1);
        let body_b = String::from_utf8(collect_stream(resp_b.stream).await).unwrap();
        assert!(body_b.contains("OpenAI Codex session expired"));
        assert!(!body_b.contains("leaked_secret_tok_999"));

        // Case C: 429 returns sanitized quota error without leaking body secrets.
        let (base_url_c, _) = spawn_mock_codex(MockMode::RateLimitedWithSecret).await;
        let auth_c = Arc::new(StubCodexAuth::with_tokens("tok_c", "tok_c2"));
        let provider_c = CodexProvider::new(&base_url_c, auth_c, 30).expect("provider c");
        let resp_c = provider_c
            .forward_gemini(ForwardRequest {
                path_and_query: "/v1beta/models/gpt-6.2-sol:streamGenerateContent?alt=sse"
                    .to_string(),
                body: Bytes::from(r#"{"contents":[{"role":"user","parts":[{"text":"hi"}]}]}"#),
                content_type: None,
                api_key_override: None,
            })
            .await
            .expect("429 sanitized");
        assert_eq!(resp_c.status, 429);
        let body_c = String::from_utf8(collect_stream(resp_c.stream).await).unwrap();
        assert!(body_c.contains("ChatGPT Codex quota limit reached"));
        assert!(!body_c.contains("secret-refresh-token-do-not-echo"));

        // Case D: Unauthenticated returns NotAuthenticated.
        let provider_unauth =
            CodexProvider::new(&base_url_c, Arc::new(StubCodexAuth::unauthenticated()), 30)
                .expect("unauth provider");
        let err = provider_unauth
            .forward_gemini(ForwardRequest {
                path_and_query: "/v1beta/models/gpt-6.2-sol:generateContent".to_string(),
                body: Bytes::from(r#"{"contents":[{"role":"user","parts":[{"text":"hi"}]}]}"#),
                content_type: None,
                api_key_override: None,
            })
            .await
            .expect_err("unauth fails");
        assert!(matches!(err, ProviderError::NotAuthenticated(_)));
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn health_check_list_models_and_no_redirect_policy() {
        let (base_url, _) = spawn_mock_codex(MockMode::UnauthorizedOnceThenSuccess).await;
        let auth = Arc::new(StubCodexAuth::with_tokens("stale", "fresh"));
        let provider = CodexProvider::new(&base_url, auth.clone(), 30).expect("provider");
        provider
            .health_check()
            .await
            .expect("health_check succeeds after 401 refresh");
        assert_eq!(auth.force_refresh_calls.load(Ordering::SeqCst), 1);

        let models = provider.list_models(None).await.expect("list_models");
        assert_eq!(models.models.len(), CURATED_CODEX_MODELS.len());
        assert_eq!(models.models[0].name, "models/gpt-6.2-astra");

        // Redirects must not be followed (S3 Policy::none()).
        let (redir_url, redir_mock) = spawn_mock_codex(MockMode::Redirect).await;
        let redir_provider = CodexProvider::new(
            &redir_url,
            Arc::new(StubCodexAuth::with_tokens("tok", "tok2")),
            30,
        )
        .expect("redir provider");
        let err = redir_provider
            .health_check()
            .await
            .expect_err("302 must fail health check without following");
        assert!(matches!(err, ProviderError::Upstream(_)));
        assert_eq!(redir_mock.calls.load(Ordering::SeqCst), 1);
    }
}
