use std::sync::Arc;
use std::time::{Instant, SystemTime, UNIX_EPOCH};

use axum::body::Body;
use axum::extract::rejection::BytesRejection;
use axum::extract::{OriginalUri, Request, State};
use axum::response::Response;
use bytes::Bytes;
use http::{HeaderMap, Method, StatusCode, header};

use crate::ProxyState;
use crate::cloudcode::{
    CloudCodeError, is_cloudcode_path, is_uncompressed_content_encoding,
    merge_available_models_multi_provider, validate_cloudcode_path,
};

use super::middleware::{
    body_read_error_response, not_found_handler, read_bounded_request_body, unwrap_bytes_body,
    validate_content_length_headers,
};
use super::tee::{TrackedStream, UsageCollector, UsageOrigin, UsageTeeStream, select_tee_mode};
use super::{MAX_PROXY_BODY_BYTES, json_error_response};

const DEFAULT_BUILTIN_MODEL: &str = "antigravity-builtin";
const MAX_BUILTIN_MODEL_LEN: usize = 128;

fn is_builtin_generation_path(path_only: &str) -> bool {
    matches!(
        path_only,
        "/v1internal:streamGenerateContent" | "/v1internal:generateContent"
    ) || (path_only.starts_with("/v1internal/")
        && (path_only.ends_with(":streamGenerateContent")
            || path_only.ends_with(":generateContent")))
}

fn sanitize_builtin_model_id(raw: &str) -> Option<String> {
    let trimmed = raw.trim();
    let after_slash_models = trimmed
        .rsplit_once("/models/")
        .map_or(trimmed, |(_, rest)| rest);
    let after_models = after_slash_models
        .strip_prefix("models/")
        .unwrap_or(after_slash_models);
    let cleaned: String = after_models
        .trim_matches('/')
        .chars()
        .filter(|c| c.is_ascii_graphic() && *c != '"' && *c != '\\')
        .take(MAX_BUILTIN_MODEL_LEN)
        .collect();
    if cleaned.is_empty() {
        None
    } else {
        Some(cleaned)
    }
}

#[derive(serde::Deserialize)]
struct BuiltinModelProbe {
    #[serde(default)]
    model: Option<String>,
}

fn extract_builtin_model_name(path_only: &str, body_bytes: &[u8]) -> String {
    if let Some((_, after_models)) = path_only.rsplit_once("/models/") {
        let candidate = after_models
            .rsplit_once(':')
            .map_or(after_models, |(m, _)| m);
        if let Some(model) = sanitize_builtin_model_id(candidate) {
            return model;
        }
    }
    if !body_bytes.is_empty()
        && let Some(model) = serde_json::from_slice::<BuiltinModelProbe>(body_bytes)
            .ok()
            .and_then(|probe| probe.model)
            .and_then(|raw| sanitize_builtin_model_id(&raw))
    {
        return model;
    }
    DEFAULT_BUILTIN_MODEL.to_string()
}

fn unix_timestamp_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .ok()
        .and_then(|d| u64::try_from(d.as_millis()).ok())
        .unwrap_or(0)
}

pub(super) async fn fetch_available_models_handler(
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
    if validate_cloudcode_path(path_and_query).is_err() {
        return json_error_response(StatusCode::BAD_REQUEST, "invalid request path");
    }

    let active = state.active_provider().await;
    let upstream = match active
        .cloudcode
        .forward_fetch_available_models(path_and_query, &headers, body)
        .await
    {
        Ok(resp) => resp,
        Err(CloudCodeError::InvalidPath(_)) => {
            return json_error_response(StatusCode::BAD_REQUEST, "invalid request path");
        }
        Err(CloudCodeError::InvalidConfig(_)) => {
            return json_error_response(
                StatusCode::INTERNAL_SERVER_ERROR,
                "provider configuration error",
            );
        }
        Err(CloudCodeError::ResponseTooLarge | CloudCodeError::Upstream(_)) => {
            return json_error_response(
                StatusCode::BAD_GATEWAY,
                "upstream cloudcode request failed",
            );
        }
    };

    if upstream.status != StatusCode::OK {
        let mut builder = Response::builder().status(upstream.status);
        for (name, value) in &upstream.headers {
            builder = builder.header(name, value);
        }
        return builder.body(Body::from(upstream.body)).unwrap_or_else(|_| {
            json_error_response(
                StatusCode::INTERNAL_SERVER_ERROR,
                "failed to build proxy response",
            )
        });
    }

    let gemini_ready = active.has_configured_api_key && !active.enabled_models.is_empty();
    let codex_ready = active.codex_authenticated && !active.codex_enabled_models.is_empty();

    let refresh_ts_ms = unix_timestamp_ms();
    if !gemini_ready && !codex_ready {
        if upstream.response_is_uncompressed {
            state
                .upstream_catalog()
                .update_from_fetch_available_models(&upstream.body);
        }
        state.stats().record_catalog_refresh(refresh_ts_ms, false);
        let mut builder = Response::builder()
            .status(StatusCode::OK)
            .header(header::X_CONTENT_TYPE_OPTIONS, "nosniff")
            .header(header::CACHE_CONTROL, "no-store");
        for (name, value) in &upstream.headers {
            builder = builder.header(name, value);
        }
        return builder.body(Body::from(upstream.body)).unwrap_or_else(|_| {
            json_error_response(
                StatusCode::INTERNAL_SERVER_ERROR,
                "failed to build proxy response",
            )
        });
    }

    let empty_models: &[String] = &[];
    let gemini_models = if gemini_ready {
        active.enabled_models.as_slice()
    } else {
        empty_models
    };
    let codex_models = if codex_ready {
        active.codex_enabled_models.as_slice()
    } else {
        empty_models
    };

    let (final_body, was_merged) = match merge_available_models_multi_provider(
        &upstream.body,
        gemini_models,
        &active.default_model,
        codex_models,
        &active.codex_default_model,
    ) {
        Ok(merged) => (Bytes::from(merged), true),
        Err(err) => {
            tracing::warn!(
                error = %err,
                "failed to merge proxy models into fetchAvailableModels; returning upstream response untouched"
            );
            (upstream.body, false)
        }
    };
    state
        .stats()
        .record_catalog_refresh(refresh_ts_ms, was_merged);
    if upstream.response_is_uncompressed || was_merged {
        state
            .upstream_catalog()
            .update_from_fetch_available_models(&final_body);
    }

    let mut builder = Response::builder()
        .status(StatusCode::OK)
        .header(header::X_CONTENT_TYPE_OPTIONS, "nosniff")
        .header(header::CACHE_CONTROL, "no-store");
    if was_merged {
        builder = builder.header(header::CONTENT_TYPE, "application/json");
        for (name, value) in &upstream.headers {
            if *name != header::CONTENT_TYPE && *name != header::CONTENT_ENCODING {
                builder = builder.header(name, value);
            }
        }
    } else {
        for (name, value) in &upstream.headers {
            builder = builder.header(name, value);
        }
    }
    builder.body(Body::from(final_body)).unwrap_or_else(|_| {
        json_error_response(
            StatusCode::INTERNAL_SERVER_ERROR,
            "failed to build proxy response",
        )
    })
}

pub(super) async fn cloudcode_or_not_found_handler(
    State(state): State<ProxyState>,
    req: Request,
) -> Response {
    let uri = req.uri().clone();
    if !is_cloudcode_path(uri.path()) {
        return not_found_handler().await;
    }

    let method = req.method().clone();
    if method != Method::GET && method != Method::POST {
        return json_error_response(StatusCode::METHOD_NOT_ALLOWED, "method not allowed");
    }

    let path_and_query = uri
        .path_and_query()
        .map(|pq| pq.as_str())
        .unwrap_or_else(|| uri.path())
        .to_string();
    if validate_cloudcode_path(&path_and_query).is_err() {
        return json_error_response(StatusCode::BAD_REQUEST, "invalid request path");
    }

    let headers = req.headers().clone();
    if method == Method::POST
        && let Err((status, message)) = validate_content_length_headers(&headers)
    {
        return json_error_response(status, message);
    }

    let body_bytes = if method == Method::POST {
        match read_bounded_request_body(req.into_body(), MAX_PROXY_BODY_BYTES).await {
            Ok(bytes) => bytes,
            Err(err) => return body_read_error_response(err),
        }
    } else {
        Bytes::new()
    };

    let path_only = path_and_query
        .split_once('?')
        .map_or(path_and_query.as_str(), |(p, _)| p);
    let should_tee_request = method == Method::POST && is_builtin_generation_path(path_only);
    let builtin_model =
        should_tee_request.then(|| extract_builtin_model_name(path_only, &body_bytes));
    let conversation_id = should_tee_request
        .then(|| crate::session::extract_conversation_id(&body_bytes))
        .flatten();
    let started_at = Instant::now();

    let active = state.active_provider().await;
    let upstream = match active
        .cloudcode
        .forward_raw(method, &path_and_query, &headers, body_bytes)
        .await
    {
        Ok(resp) => resp,
        Err(CloudCodeError::InvalidPath(_)) => {
            return json_error_response(StatusCode::BAD_REQUEST, "invalid request path");
        }
        Err(CloudCodeError::InvalidConfig(_)) => {
            return json_error_response(
                StatusCode::INTERNAL_SERVER_ERROR,
                "provider configuration error",
            );
        }
        Err(CloudCodeError::ResponseTooLarge | CloudCodeError::Upstream(_)) => {
            return json_error_response(
                StatusCode::BAD_GATEWAY,
                "upstream cloudcode request failed",
            );
        }
    };

    let mut builder = Response::builder().status(upstream.status);
    for (name, value) in &upstream.headers {
        builder = builder.header(name, value);
    }
    let stream_guard = state.enter_request();
    let should_tee_response = should_tee_request
        && upstream.status.is_success()
        && upstream.response_is_uncompressed
        && is_uncompressed_content_encoding(&upstream.headers);
    let body = if should_tee_response {
        let upstream_ct = upstream
            .headers
            .get(header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok());
        let collector = UsageCollector::from_mode(select_tee_mode(upstream_ct, &path_and_query));
        Body::from_stream(UsageTeeStream {
            inner: upstream.stream,
            collector,
            stats: Arc::clone(state.stats()),
            usage_sink: Arc::clone(state.usage_sink()),
            origin: UsageOrigin::AntigravityBuiltin,
            model: builtin_model.unwrap_or_else(|| DEFAULT_BUILTIN_MODEL.to_string()),
            conversation_id,
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
    builder.body(body).unwrap_or_else(|_| {
        json_error_response(
            StatusCode::INTERNAL_SERVER_ERROR,
            "failed to build proxy response",
        )
    })
}
