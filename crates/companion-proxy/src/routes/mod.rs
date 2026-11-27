mod cloudcode;
mod gemini;
mod internal;
mod middleware;
mod tee;
#[cfg(test)]
mod tests;
mod thought_sig;

use axum::Router;
use axum::body::Body;
use axum::extract::DefaultBodyLimit;
use axum::middleware as axum_middleware;
use axum::response::Response;
use axum::routing::{MethodFilter, on};
use http::StatusCode;
use http::header::{self, HeaderValue};
use tower_http::set_header::SetResponseHeaderLayer;

use crate::ProxyState;

use cloudcode::{cloudcode_or_not_found_handler, fetch_available_models_handler};
use gemini::proxy_gemini_handler;
use internal::{health_handler, list_models_handler, stats_handler};
use middleware::{
    enforce_content_length_limit, enforce_get_post_only, method_not_allowed_handler,
    track_request_activity,
};

pub const MAX_PROXY_BODY_BYTES: usize = 64 * 1024 * 1024;
pub use tee::DEFAULT_MAX_JSON_TEE_BYTES;

pub fn build_router(state: ProxyState) -> Router {
    let internal_security_headers = (
        SetResponseHeaderLayer::overriding(
            header::X_CONTENT_TYPE_OPTIONS,
            HeaderValue::from_static("nosniff"),
        ),
        SetResponseHeaderLayer::overriding(
            header::CACHE_CONTROL,
            HeaderValue::from_static("no-store"),
        ),
    );

    Router::new()
        .route(
            "/health",
            on(MethodFilter::GET, health_handler).layer(internal_security_headers.clone()),
        )
        .route(
            "/api/stats",
            on(MethodFilter::GET, stats_handler).layer(internal_security_headers.clone()),
        )
        .route(
            "/v1beta/models",
            on(MethodFilter::GET, list_models_handler).layer(internal_security_headers),
        )
        .route(
            "/v1beta/models/{*rest}",
            on(MethodFilter::POST, proxy_gemini_handler)
                .layer(DefaultBodyLimit::max(MAX_PROXY_BODY_BYTES))
                .layer(axum_middleware::from_fn(enforce_content_length_limit)),
        )
        .route(
            "/v1internal:fetchAvailableModels",
            on(MethodFilter::POST, fetch_available_models_handler)
                .layer(DefaultBodyLimit::max(MAX_PROXY_BODY_BYTES))
                .layer(axum_middleware::from_fn(enforce_content_length_limit)),
        )
        .method_not_allowed_fallback(method_not_allowed_handler)
        .route_layer(axum_middleware::from_fn(enforce_get_post_only))
        .fallback(cloudcode_or_not_found_handler)
        .layer(axum_middleware::from_fn_with_state(
            state.clone(),
            track_request_activity,
        ))
        .with_state(state)
}

pub(super) fn json_error_response(status: StatusCode, message: &str) -> Response {
    let payload = serde_json::json!({
        "error": {
            "message": message,
        }
    });
    let body = Body::from(payload.to_string());
    let mut resp = Response::new(body);
    *resp.status_mut() = status;
    let headers = resp.headers_mut();
    headers.insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("application/json"),
    );
    headers.insert(
        header::X_CONTENT_TYPE_OPTIONS,
        HeaderValue::from_static("nosniff"),
    );
    headers.insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    resp
}
