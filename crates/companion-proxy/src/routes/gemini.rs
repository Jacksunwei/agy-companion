use std::sync::Arc;
use std::time::Instant;

use axum::body::Body;
use axum::extract::rejection::BytesRejection;
use axum::extract::{OriginalUri, State};
use axum::response::Response;
use bytes::Bytes;
use companion_providers::{ForwardRequest, ProviderError};
use http::{HeaderMap, StatusCode, header};

use crate::ProxyState;

use super::json_error_response;
use super::middleware::{
    body_read_error_response, sanitize_outbound_content_type, unwrap_bytes_body,
    validate_inbound_api_key, validate_inbound_content_type,
};
use super::tee::{TrackedStream, UsageCollector, UsageOrigin, UsageTeeStream, select_tee_mode};
use super::thought_sig::{
    SniffedErrorPrefix, THOUGHT_SIG_ERROR_SNIFF_MAX_BYTES, body_contains_thought_signature,
    is_invalid_thought_signature_error, replay_buffered_stream, sniff_error_prefix,
    strip_thought_signatures_from_json,
};

const GEMINI_MODELS_PREFIX: &str = "/v1beta/models/";

pub(super) async fn proxy_gemini_handler(
    State(state): State<ProxyState>,
    OriginalUri(uri): OriginalUri,
    headers: HeaderMap,
    body: Result<Bytes, BytesRejection>,
) -> Response {
    let body = match unwrap_bytes_body(body) {
        Ok(bytes) => bytes,
        Err(err) => return body_read_error_response(err),
    };

    let path_and_query = uri
        .path_and_query()
        .map(|pq| pq.as_str())
        .unwrap_or_else(|| uri.path());
    if !path_and_query.starts_with(GEMINI_MODELS_PREFIX) {
        return json_error_response(StatusCode::BAD_REQUEST, "invalid request path");
    }

    let model = extract_model_name(path_and_query);
    if model == "unknown"
        || model.len() > companion_core::MAX_GEMINI_MODEL_LEN
        || model == "."
        || model.contains("..")
        || model.contains(['?', '#', '/', '\\', '%', ';'])
        || !model.bytes().all(|b| (0x21..=0x7E).contains(&b))
    {
        return json_error_response(StatusCode::BAD_REQUEST, "invalid request path");
    }
    if !companion_core::is_supported_gemini_model(&model) {
        return json_error_response(
            StatusCode::BAD_REQUEST,
            "unsupported model: only Gemini 3.8+ and Gemini 4+ models are supported (unset GOOGLE_GEMINI_BASE_URL to reach Gemini API directly)",
        );
    }

    let content_type = match validate_inbound_content_type(&headers) {
        Ok(ct) => ct,
        Err(msg) => return json_error_response(StatusCode::BAD_REQUEST, msg),
    };

    let active = state.active_provider().await;
    let api_key_override = match validate_inbound_api_key(&headers, active.has_configured_api_key) {
        Ok(key) => key,
        Err(msg) => return json_error_response(StatusCode::BAD_REQUEST, msg),
    };

    let started_at = Instant::now();
    let has_thought_sig = body_contains_thought_signature(&body);
    let forward_req = ForwardRequest {
        path_and_query: path_and_query.to_string(),
        body: body.clone(),
        content_type: content_type.clone(),
        api_key_override: api_key_override.clone(),
    };

    let mut upstream = match active.provider.forward_gemini(forward_req).await {
        Ok(resp) => resp,
        Err(ProviderError::InvalidPath(_)) => {
            return json_error_response(StatusCode::BAD_REQUEST, "invalid request path");
        }
        Err(ProviderError::MissingApiKey) => {
            return json_error_response(
                StatusCode::UNAUTHORIZED,
                "missing Gemini API key: configure gemini.api_key or set GEMINI_API_KEY",
            );
        }
        Err(ProviderError::InvalidConfig(_)) => {
            return json_error_response(
                StatusCode::INTERNAL_SERVER_ERROR,
                "provider configuration error",
            );
        }
        Err(ProviderError::Upstream(_)) => {
            return json_error_response(
                StatusCode::BAD_GATEWAY,
                "upstream provider request failed",
            );
        }
    };

    // Mid-conversation switching from Vertex/CloudCode to Gemini Developer API can carry
    // foreign thoughtSignature tokens that generativelanguage.googleapis.com rejects with
    // 400 INVALID_THOUGHT_SIGNATURE. Keep byte-for-byte pass-through on the normal path (S4),
    // and only strip foreign thoughtSignature keys on an explicit 400 INVALID_THOUGHT_SIGNATURE.
    if upstream.status == StatusCode::BAD_REQUEST.as_u16() && has_thought_sig {
        let SniffedErrorPrefix {
            sniffed,
            leftover,
            stream_err,
            remainder,
        } = sniff_error_prefix(upstream.stream, THOUGHT_SIG_ERROR_SNIFF_MAX_BYTES).await;
        if is_invalid_thought_signature_error(&sniffed)
            && let Some(stripped_body) = strip_thought_signatures_from_json(&body)
        {
            tracing::info!(
                model = %model,
                "retrying Gemini request once without foreign thoughtSignature fields after INVALID_THOUGHT_SIGNATURE"
            );
            let retry_req = ForwardRequest {
                path_and_query: path_and_query.to_string(),
                body: stripped_body,
                content_type,
                api_key_override,
            };
            match active.provider.forward_gemini(retry_req).await {
                Ok(retry_resp) => {
                    upstream = retry_resp;
                }
                Err(_) => {
                    let prefix = Bytes::from(sniffed);
                    upstream.stream =
                        replay_buffered_stream(prefix, leftover, stream_err, remainder);
                }
            }
        } else {
            let prefix = Bytes::from(sniffed);
            upstream.stream = replay_buffered_stream(prefix, leftover, stream_err, remainder);
        }
    }

    let status = StatusCode::from_u16(upstream.status).unwrap_or(StatusCode::BAD_GATEWAY);
    let mut response_builder = Response::builder().status(status);
    if let Some(ct_header) = sanitize_outbound_content_type(upstream.content_type.as_deref()) {
        response_builder = response_builder.header(header::CONTENT_TYPE, ct_header);
    }

    let stream_guard = state.enter_request();
    let body = if status.is_success() {
        let collector = UsageCollector::from_mode(select_tee_mode(
            upstream.content_type.as_deref(),
            path_and_query,
        ));
        let model = extract_model_name(path_and_query);
        Body::from_stream(UsageTeeStream {
            inner: upstream.stream,
            collector,
            stats: Arc::clone(state.stats()),
            usage_sink: Arc::clone(state.usage_sink()),
            origin: UsageOrigin::GeminiProxy,
            model,
            started_at,
            finalized: false,
            errored: false,
            guard: stream_guard,
        })
    } else {
        Body::from_stream(TrackedStream {
            inner: upstream.stream,
            guard: stream_guard,
        })
    };

    match response_builder.body(body) {
        Ok(resp) => resp,
        Err(_) => json_error_response(
            StatusCode::INTERNAL_SERVER_ERROR,
            "failed to build proxy response",
        ),
    }
}

fn extract_model_name(path_and_query: &str) -> String {
    let path = path_and_query
        .split_once('?')
        .map_or(path_and_query, |(p, _)| p);
    let after_prefix = path.strip_prefix(GEMINI_MODELS_PREFIX).unwrap_or(path);
    let after_optional_models = after_prefix.strip_prefix("models/").unwrap_or(after_prefix);
    let before_action = after_optional_models
        .rsplit_once(':')
        .map_or(after_optional_models, |(model, _)| model);
    let cleaned = before_action.trim_matches('/');
    if cleaned.is_empty() {
        "unknown".to_string()
    } else {
        cleaned.to_string()
    }
}
