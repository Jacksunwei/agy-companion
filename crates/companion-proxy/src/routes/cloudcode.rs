use std::time::{Duration, SystemTime, UNIX_EPOCH};

use axum::body::Body;
use axum::extract::rejection::BytesRejection;
use axum::extract::{OriginalUri, Request, State};
use axum::response::Response;
use bytes::Bytes;
use companion_providers::{curated_gemini_models, filter_and_order_models};
use http::{HeaderMap, Method, StatusCode, header};

use crate::cloudcode::{
    CloudCodeError, is_cloudcode_path, merge_available_models, validate_cloudcode_path,
};
use crate::{ActiveProvider, ProxyState};

use super::middleware::{
    body_read_error_response, not_found_handler, read_bounded_request_body, unwrap_bytes_body,
    validate_content_length_headers,
};
use super::tee::TrackedStream;
use super::{MAX_PROXY_BODY_BYTES, json_error_response};

const CLOUDCODE_MODEL_DISCOVERY_TIMEOUT: Duration = Duration::from_secs(3);

fn unix_timestamp_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .ok()
        .and_then(|d| u64::try_from(d.as_millis()).ok())
        .unwrap_or(0)
}

async fn resolve_proxy_model_ids(active: &ActiveProvider) -> Vec<String> {
    let curated = curated_gemini_models(Some(&active.default_model));
    let mut ids: Vec<String> = curated.models.into_iter().map(|m| m.name).collect();

    if active.has_configured_api_key
        && let Ok(Ok(list)) = tokio::time::timeout(
            CLOUDCODE_MODEL_DISCOVERY_TIMEOUT,
            active.provider.list_models(None),
        )
        .await
    {
        let ordered = filter_and_order_models(list.models, Some(&active.default_model));
        if !ordered.models.is_empty() {
            // Keep default_model and curated Gemini 4/3.8 models prioritized ahead of
            // additional discovered models so the 32-slot placeholder pool always carries
            // the primary models first.
            for model in ordered.models {
                if !ids.contains(&model.name) {
                    ids.push(model.name);
                }
            }
        }
    }

    ids
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

    let refresh_ts_ms = unix_timestamp_ms();
    if !active.has_configured_api_key {
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

    let proxy_model_ids = resolve_proxy_model_ids(&active).await;
    let (final_body, was_merged) = match merge_available_models(
        &upstream.body,
        &proxy_model_ids,
        &active.default_model,
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
    builder
        .body(Body::from_stream(TrackedStream {
            inner: upstream.stream,
            guard: stream_guard,
        }))
        .unwrap_or_else(|_| {
            json_error_response(
                StatusCode::INTERNAL_SERVER_ERROR,
                "failed to build proxy response",
            )
        })
}
