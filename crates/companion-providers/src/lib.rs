#![forbid(unsafe_code)]

pub mod gemini;

use std::borrow::Cow;
use std::fmt;
use std::future::Future;
use std::pin::Pin;

use bytes::Bytes;
use companion_core::api_key_hint;
use futures_util::Stream;
use thiserror::Error;

pub use gemini::{GEMINI_API_KEY_ENV_VAR, GeminiProvider};

/// Boxed `Send` future enabling dyn-compatible (`Arc<dyn ProviderAdapter>`) async traits
/// under Rust Edition 2024 without an external macro dependency.
pub type BoxFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

/// Unbuffered upstream response byte stream compatible directly with `axum::body::Body::from_stream`.
pub type ByteStream = Pin<Box<dyn Stream<Item = Result<Bytes, ProviderError>> + Send>>;

#[derive(Debug, Error)]
pub enum ProviderError {
    #[error("invalid request path: {0}")]
    InvalidPath(String),
    #[error("invalid provider configuration: {0}")]
    InvalidConfig(String),
    #[error("missing Gemini API key: configure gemini.api_key or set GEMINI_API_KEY")]
    MissingApiKey,
    #[error("upstream provider request failed: {0}")]
    Upstream(String),
}

#[derive(Clone, PartialEq, Eq)]
pub struct ForwardRequest {
    pub path_and_query: String,
    pub body: Bytes,
    pub content_type: Option<String>,
    pub api_key_override: Option<String>,
}

fn redact_path_and_query_for_debug(path_and_query: &str) -> Cow<'_, str> {
    match path_and_query.split_once('?') {
        Some((raw_path, _)) => Cow::Owned(format!("{raw_path}?<redacted>")),
        None => Cow::Borrowed(path_and_query),
    }
}

impl fmt::Debug for ForwardRequest {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ForwardRequest")
            .field(
                "path_and_query",
                &redact_path_and_query_for_debug(&self.path_and_query),
            )
            .field("body_len", &self.body.len())
            .field("content_type", &self.content_type)
            .field(
                "api_key_override",
                &api_key_hint(self.api_key_override.as_deref()),
            )
            .finish()
    }
}

pub struct ForwardResponse {
    pub status: u16,
    pub content_type: Option<String>,
    pub stream: ByteStream,
}

impl fmt::Debug for ForwardResponse {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ForwardResponse")
            .field("status", &self.status)
            .field("content_type", &self.content_type)
            .field("stream", &"<ByteStream>")
            .finish()
    }
}

pub trait ProviderAdapter: Send + Sync {
    fn name(&self) -> &'static str;

    fn forward_gemini(
        &self,
        req: ForwardRequest,
    ) -> BoxFuture<'_, Result<ForwardResponse, ProviderError>>;

    fn health_check(&self) -> BoxFuture<'_, Result<(), ProviderError>>;
}
