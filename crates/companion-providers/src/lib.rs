#![forbid(unsafe_code)]

pub mod gemini;

use std::borrow::Cow;
use std::fmt;
use std::future::Future;
use std::pin::Pin;

use bytes::Bytes;
use companion_core::{api_key_hint, is_supported_gemini_model};
use futures_util::Stream;
use serde::{Deserialize, Serialize};
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

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct GeminiModelInfo {
    pub name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub display_name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub input_token_limit: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub output_token_limit: Option<u64>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub supported_generation_methods: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub thinking: Option<bool>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct GeminiModelList {
    #[serde(default)]
    pub models: Vec<GeminiModelInfo>,
}

const CURATED_GATEWAY_MODELS: &[(&str, &str)] = &[
    ("gemini-4-hydrogen", "Gemini 4 Hydrogen"),
    ("gemini-4-argon", "Gemini 4 Argon"),
    ("gemini-4-neon", "Gemini 4 Neon"),
    ("gemini-4-carbon", "Gemini 4 Carbon"),
    ("gemini-3.8-flash", "Gemini 3.8 Flash"),
];

const DEFAULT_GATEWAY_INPUT_TOKEN_LIMIT: u64 = 1_048_576;
const DEFAULT_GATEWAY_OUTPUT_TOKEN_LIMIT: u64 = 65_536;

/// Returns `true` when `model_id` (without `models/` prefix) is a chat-capable Gemini 3/4 model.
/// Filters out specialized non-chat endpoints (TTS, speech transcription, image generation, live audio)
/// so Antigravity App's model picker only lists interactive coding/agent models.
pub fn is_discoverable_chat_model(model_id: &str) -> bool {
    if !is_supported_gemini_model(model_id) {
        return false;
    }
    let lower = model_id.to_ascii_lowercase();
    let non_chat_markers = [
        "-tts",
        "-transcribe",
        "-image",
        "-live",
        "-translate",
        "-embedding",
    ];
    !non_chat_markers.iter().any(|marker| lower.contains(marker))
}

fn make_curated_model_info(id: &str, display_name: Option<&str>) -> GeminiModelInfo {
    GeminiModelInfo {
        name: format!("models/{id}"),
        display_name: Some(display_name.unwrap_or(id).to_string()),
        description: None,
        input_token_limit: Some(DEFAULT_GATEWAY_INPUT_TOKEN_LIMIT),
        output_token_limit: Some(DEFAULT_GATEWAY_OUTPUT_TOKEN_LIMIT),
        supported_generation_methods: vec!["generateContent".to_string()],
        thinking: Some(true),
    }
}

/// Builds the curated Gemini 4 & 3 model catalog for `GET /v1beta/models` fallback,
/// placing `default_model` first when provided and valid.
pub fn curated_gemini_models(default_model: Option<&str>) -> GeminiModelList {
    let mut models: Vec<GeminiModelInfo> = CURATED_GATEWAY_MODELS
        .iter()
        .map(|(id, label)| make_curated_model_info(id, Some(label)))
        .collect();
    filter_and_order_models(std::mem::take(&mut models), default_model)
}

/// Filters upstream or curated models to chat-capable Gemini 3 & 4 entries, deduplicates by ID,
/// ensures required Antigravity Gateway discovery fields are populated, and places `default_model`
/// first so Antigravity App selects it as the default model.
pub fn filter_and_order_models(
    raw_models: Vec<GeminiModelInfo>,
    default_model: Option<&str>,
) -> GeminiModelList {
    let mut seen = std::collections::HashSet::new();
    let mut filtered = Vec::new();

    for mut entry in raw_models {
        let clean_id = entry
            .name
            .trim()
            .strip_prefix("models/")
            .unwrap_or(entry.name.trim())
            .to_string();
        if clean_id.is_empty() || !is_discoverable_chat_model(&clean_id) {
            continue;
        }
        if !entry.supported_generation_methods.is_empty()
            && !entry
                .supported_generation_methods
                .iter()
                .any(|m| m == "generateContent")
        {
            continue;
        }
        if !seen.insert(clean_id.clone()) {
            continue;
        }
        entry.name = format!("models/{clean_id}");
        if entry.display_name.as_deref().is_none_or(str::is_empty) {
            let curated_label = CURATED_GATEWAY_MODELS
                .iter()
                .find(|(id, _)| *id == clean_id)
                .map(|(_, label)| (*label).to_string());
            entry.display_name = Some(curated_label.unwrap_or_else(|| clean_id.clone()));
        }
        if entry.supported_generation_methods.is_empty() {
            entry.supported_generation_methods = vec!["generateContent".to_string()];
        }
        if entry.thinking.is_none() {
            entry.thinking = Some(true);
        }
        filtered.push(entry);
    }

    if let Some(raw_default) = default_model.map(str::trim).filter(|s| !s.is_empty()) {
        let default_id = raw_default.strip_prefix("models/").unwrap_or(raw_default);
        if is_supported_gemini_model(default_id) {
            let target_name = format!("models/{default_id}");
            if let Some(pos) = filtered.iter().position(|m| m.name == target_name) {
                let item = filtered.remove(pos);
                filtered.insert(0, item);
            } else {
                let curated_label = CURATED_GATEWAY_MODELS
                    .iter()
                    .find(|(id, _)| *id == default_id)
                    .map(|(_, label)| *label);
                filtered.insert(0, make_curated_model_info(default_id, curated_label));
            }
        }
    }

    GeminiModelList { models: filtered }
}

pub trait ProviderAdapter: Send + Sync {
    fn name(&self) -> &'static str;

    fn forward_gemini(
        &self,
        req: ForwardRequest,
    ) -> BoxFuture<'_, Result<ForwardResponse, ProviderError>>;

    fn health_check(&self) -> BoxFuture<'_, Result<(), ProviderError>>;

    fn list_models(
        &self,
        _api_key_override: Option<String>,
    ) -> BoxFuture<'_, Result<GeminiModelList, ProviderError>> {
        Box::pin(async {
            Err(ProviderError::Upstream(
                "upstream model discovery not implemented".to_string(),
            ))
        })
    }
}
