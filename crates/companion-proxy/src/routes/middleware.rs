use axum::body::Body;
use axum::extract::rejection::BytesRejection;
use axum::extract::{Request, State};
use axum::middleware::Next;
use axum::response::Response;
use bytes::Bytes;
use companion_core::{is_printable_ascii_with_space, validate_api_key_str};
use futures_util::StreamExt;
use http::header::{self, HeaderMap, HeaderName, HeaderValue};
use http::{Method, StatusCode};

use crate::ProxyState;

use super::{MAX_PROXY_BODY_BYTES, json_error_response};

pub(super) static X_GOOG_API_KEY_HEADER: HeaderName = HeaderName::from_static("x-goog-api-key");

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum BodyReadError {
    TooLarge,
    ReadFailed,
}

pub(super) async fn track_request_activity(
    State(state): State<ProxyState>,
    req: Request,
    next: Next,
) -> Response {
    // Passive liveness/telemetry probes (including `serve --adopt-running`'s 500ms `/health` poll)
    // must not reset `idle_duration()` or starve `--idle-shutdown-secs`.
    if matches!(req.uri().path(), "/health" | "/api/stats") {
        return next.run(req).await;
    }
    let _guard = state.enter_request();
    next.run(req).await
}

pub(super) fn validate_content_length_headers(
    headers: &HeaderMap,
) -> Result<(), (StatusCode, &'static str)> {
    let mut expected_len: Option<u64> = None;
    let mut saw_overflow = false;
    for value in headers.get_all(header::CONTENT_LENGTH) {
        let raw_bytes = value.as_bytes();
        if raw_bytes.is_empty() || !raw_bytes.iter().all(u8::is_ascii_digit) {
            return Err((StatusCode::BAD_REQUEST, "invalid Content-Length header"));
        }
        let Ok(raw_str) = std::str::from_utf8(raw_bytes) else {
            return Err((StatusCode::BAD_REQUEST, "invalid Content-Length header"));
        };
        match raw_str.parse::<u64>() {
            Ok(parsed) => {
                if saw_overflow {
                    return Err((
                        StatusCode::BAD_REQUEST,
                        "conflicting Content-Length headers",
                    ));
                }
                if let Some(prev) = expected_len
                    && prev != parsed
                {
                    return Err((
                        StatusCode::BAD_REQUEST,
                        "conflicting Content-Length headers",
                    ));
                }
                expected_len = Some(parsed);
            }
            Err(_) => {
                if expected_len.is_some() {
                    return Err((
                        StatusCode::BAD_REQUEST,
                        "conflicting Content-Length headers",
                    ));
                }
                saw_overflow = true;
            }
        }
    }

    if saw_overflow || expected_len.is_some_and(|len| len > MAX_PROXY_BODY_BYTES as u64) {
        return Err((
            StatusCode::PAYLOAD_TOO_LARGE,
            "request body exceeds maximum allowed size",
        ));
    }

    Ok(())
}

pub(super) async fn enforce_content_length_limit(req: Request, next: Next) -> Response {
    if *req.method() != Method::POST {
        return next.run(req).await;
    }
    if let Err((status, message)) = validate_content_length_headers(req.headers()) {
        return json_error_response(status, message);
    }
    next.run(req).await
}

pub(super) async fn enforce_get_post_only(req: Request, next: Next) -> Response {
    match *req.method() {
        Method::GET | Method::POST => next.run(req).await,
        _ => json_error_response(StatusCode::METHOD_NOT_ALLOWED, "method not allowed"),
    }
}

pub(super) async fn method_not_allowed_handler() -> Response {
    json_error_response(StatusCode::METHOD_NOT_ALLOWED, "method not allowed")
}

pub(super) async fn not_found_handler() -> Response {
    json_error_response(StatusCode::NOT_FOUND, "not found")
}

pub(super) async fn read_bounded_request_body(
    body: Body,
    max_bytes: usize,
) -> Result<Bytes, BodyReadError> {
    let mut stream = body.into_data_stream();
    let mut first: Option<Bytes> = None;
    let mut buf: Vec<u8> = Vec::new();

    while let Some(next) = stream.next().await {
        let chunk = next.map_err(|_| BodyReadError::ReadFailed)?;
        if chunk.is_empty() {
            continue;
        }
        if let Some(prev) = first.take() {
            let Some(total) = prev.len().checked_add(chunk.len()) else {
                return Err(BodyReadError::TooLarge);
            };
            if total > max_bytes {
                return Err(BodyReadError::TooLarge);
            }
            buf.reserve(total);
            buf.extend_from_slice(&prev);
            buf.extend_from_slice(&chunk);
        } else if buf.is_empty() {
            if chunk.len() > max_bytes {
                return Err(BodyReadError::TooLarge);
            }
            first = Some(chunk);
        } else {
            let Some(total) = buf.len().checked_add(chunk.len()) else {
                return Err(BodyReadError::TooLarge);
            };
            if total > max_bytes {
                return Err(BodyReadError::TooLarge);
            }
            buf.extend_from_slice(&chunk);
        }
    }

    Ok(match first {
        Some(single) => single,
        None => Bytes::from(buf),
    })
}

pub(super) fn body_read_error_response(err: BodyReadError) -> Response {
    match err {
        BodyReadError::TooLarge => json_error_response(
            StatusCode::PAYLOAD_TOO_LARGE,
            "request body exceeds maximum allowed size",
        ),
        BodyReadError::ReadFailed => {
            json_error_response(StatusCode::BAD_REQUEST, "failed to read request body")
        }
    }
}

pub(super) fn unwrap_bytes_body(
    body: Result<Bytes, BytesRejection>,
) -> Result<Bytes, BodyReadError> {
    match body {
        Ok(bytes) => Ok(bytes),
        Err(rejection) if rejection.status() == StatusCode::PAYLOAD_TOO_LARGE => {
            Err(BodyReadError::TooLarge)
        }
        Err(_) => Err(BodyReadError::ReadFailed),
    }
}

pub(super) fn validate_inbound_content_type(
    headers: &HeaderMap,
) -> Result<Option<String>, &'static str> {
    let mut iter = headers.get_all(header::CONTENT_TYPE).iter();
    let Some(first) = iter.next() else {
        return Ok(None);
    };
    if iter.next().is_some() {
        return Err("duplicate Content-Type header");
    }

    let raw_bytes = first.as_bytes();
    if !is_printable_ascii_with_space(raw_bytes) {
        return Err("invalid Content-Type header");
    }
    let raw = std::str::from_utf8(raw_bytes).map_err(|_| "invalid Content-Type header")?;
    let trimmed = raw.trim_matches(' ');
    if trimmed.is_empty() {
        return Err("invalid Content-Type header");
    }
    Ok(Some(trimmed.to_string()))
}

pub(super) fn validate_inbound_api_key(
    headers: &HeaderMap,
    has_configured_api_key: bool,
) -> Result<Option<String>, &'static str> {
    let mut iter = headers.get_all(&X_GOOG_API_KEY_HEADER).iter();
    let Some(first) = iter.next() else {
        return Ok(None);
    };
    if iter.next().is_some() {
        return Err("duplicate x-goog-api-key header");
    }

    let raw_bytes = first.as_bytes();
    if !is_printable_ascii_with_space(raw_bytes) {
        return Err("invalid x-goog-api-key header");
    }
    let raw = std::str::from_utf8(raw_bytes).map_err(|_| "invalid x-goog-api-key header")?;

    if has_configured_api_key {
        return Ok(None);
    }

    match validate_api_key_str(raw) {
        Ok(Some(trimmed)) => Ok(Some(trimmed.to_string())),
        Ok(None) => Ok(None),
        Err(_) => Err("invalid x-goog-api-key header"),
    }
}

pub(super) fn sanitize_outbound_content_type(raw: Option<&str>) -> Option<HeaderValue> {
    let raw = raw?;
    if !is_printable_ascii_with_space(raw.as_bytes()) {
        return None;
    }
    let trimmed = raw.trim_matches(' ');
    if trimmed.is_empty() {
        return None;
    }
    HeaderValue::from_str(trimmed).ok()
}
