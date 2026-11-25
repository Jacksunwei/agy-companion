use axum::Json;
use axum::extract::State;
use axum::response::{IntoResponse, Response};
use companion_core::HealthResponse;
use companion_providers::{curated_gemini_models, filter_and_order_models};
use http::{HeaderMap, StatusCode};

use crate::ProxyState;

use super::json_error_response;
use super::middleware::validate_inbound_api_key;

pub(super) async fn health_handler() -> Json<HealthResponse> {
    Json(HealthResponse::ok(env!("CARGO_PKG_VERSION")))
}

pub(super) async fn stats_handler(State(state): State<ProxyState>) -> impl IntoResponse {
    let active = state.active_provider().await;
    Json(state.stats().snapshot_with_cloudcode(
        active.cloudcode.mode_label(),
        active.cloudcode.resolved_upstream_url(),
    ))
}

pub(super) async fn list_models_handler(
    State(state): State<ProxyState>,
    headers: HeaderMap,
) -> Response {
    let active = state.active_provider().await;
    let api_key_override = match validate_inbound_api_key(&headers, active.has_configured_api_key) {
        Ok(key) => key,
        Err(msg) => return json_error_response(StatusCode::BAD_REQUEST, msg),
    };

    let catalog = match active.provider.list_models(api_key_override).await {
        Ok(list) => {
            let ordered = filter_and_order_models(list.models, Some(&active.default_model));
            if ordered.models.is_empty() {
                curated_gemini_models(Some(&active.default_model))
            } else {
                ordered
            }
        }
        Err(err) => {
            tracing::debug!(
                error = %err,
                "upstream model discovery unavailable; serving curated Gemini 4/3 catalog"
            );
            curated_gemini_models(Some(&active.default_model))
        }
    };

    Json(catalog).into_response()
}
