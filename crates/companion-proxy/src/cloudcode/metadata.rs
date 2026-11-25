use bytes::Bytes;
use companion_providers::ByteStream;
use http::header::HeaderMap;
use http::{Method, StatusCode};

use super::client::{BufferedRequestOptions, CloudCodeClient, CloudCodeTarget};
use super::routing::{
    AccountRoute, detect_auth_method_from_headers, detect_enterprise_from_fetch_body,
    detect_prod_from_load_code_assist, response_has_non_empty_flags, response_has_non_empty_models,
};
use super::{CloudCodeBufferedResponse, CloudCodeError, CloudCodeStreamResponse};

enum MetadataEvaluation {
    Reject,
    AcceptLoadCodeAssist { detected_prod: Option<bool> },
    AcceptTargetRoute,
}

#[derive(Debug, Clone, Copy)]
enum MetadataTiebreak {
    LoadCodeAssist,
    ListExperiments,
    FetchAvailableModels,
}

struct MetadataFallbackPlan {
    rpc_name: &'static str,
    prefer_prod: bool,
    initial_strip: bool,
    fallback_strip: bool,
    force_strip_on_cache: bool,
    fixed_require_exact_200_for_cache: bool,
    primary_authoritative: bool,
    evaluate: fn(&CloudCodeBufferedResponse) -> MetadataEvaluation,
    tiebreak: MetadataTiebreak,
}

impl CloudCodeClient {
    /// Mirror Antigravity's own UpdateEndpointURL bootstrap for `loadCodeAssist`:
    /// probe prod first (with prod -> daily fallback and metadata-only 403 retry)
    /// and detect Business/PayGo/Enterprise (`usesGcpTos` / `gcp-*`) vs Personal Google AI Pro.
    pub(super) async fn forward_load_code_assist(
        &self,
        path_and_query: &str,
        inbound_headers: &HeaderMap,
        body: Bytes,
        auth_key: Option<(u64, u64)>,
        cached: Option<AccountRoute>,
    ) -> Result<CloudCodeStreamResponse, CloudCodeError> {
        let plan = MetadataFallbackPlan {
            rpc_name: "loadCodeAssist",
            prefer_prod: true,
            initial_strip: cached.is_some_and(|route| route.strip_user_project),
            fallback_strip: false,
            force_strip_on_cache: false,
            fixed_require_exact_200_for_cache: false,
            primary_authoritative: true,
            evaluate: evaluate_load_code_assist,
            tiebreak: MetadataTiebreak::LoadCodeAssist,
        };
        let buffered = self
            .execute_buffered_metadata_rpc(path_and_query, inbound_headers, body, auth_key, plan)
            .await?;
        Ok(buffered_to_stream_response(buffered))
    }

    /// Read-only metadata RPC: `/v1internal:listExperiments` carries Mendel feature flags
    /// (including `enable-mcp-apps`). Mirror `fetchAvailableModels`' daily <-> prod fallback
    /// and Enterprise 403 retry so PayGo/Business accounts never fall back to disabled
    /// embedded defaults when daily rejects or returns empty flags.
    pub(super) async fn forward_list_experiments(
        &self,
        path_and_query: &str,
        inbound_headers: &HeaderMap,
        body: Bytes,
        auth_key: Option<(u64, u64)>,
        cached: Option<AccountRoute>,
    ) -> Result<CloudCodeStreamResponse, CloudCodeError> {
        let (prefer_prod, initial_strip, primary_authoritative) = if let Some(route) = cached {
            if route.authoritative || route.use_prod {
                (
                    route.use_prod,
                    route.strip_user_project,
                    route.authoritative,
                )
            } else {
                // Never trust a speculative daily probe (e.g. fetchAvailableModels
                // winning a startup race on a PayGo account): probe prod first so
                // PayGo/Enterprise accounts receive official prod Mendel flags.
                (true, route.strip_user_project, false)
            }
        } else if let Some(ua_prod) = detect_auth_method_from_headers(inbound_headers) {
            (ua_prod, false, true)
        } else if let Some(last_prod) = self.inner.cache.last_resolved_prod_for_experiments() {
            (last_prod, false, false)
        } else {
            // Personal Google AI Pro either populates `use_prod=false` via
            // loadCodeAssist first or fetches flags from aicode.googleapis.com;
            // PayGo/Enterprise needs prod (`cloudcode-pa.googleapis.com`) for
            // Mendel flags (such as `enable-mcp-apps`), falling back to daily if
            // prod rejects.
            (true, false, false)
        };

        let plan = MetadataFallbackPlan {
            rpc_name: "listExperiments",
            prefer_prod,
            initial_strip,
            fallback_strip: false,
            force_strip_on_cache: false,
            fixed_require_exact_200_for_cache: false,
            primary_authoritative,
            evaluate: evaluate_list_experiments,
            tiebreak: MetadataTiebreak::ListExperiments,
        };
        let buffered = self
            .execute_buffered_metadata_rpc(path_and_query, inbound_headers, body, auth_key, plan)
            .await?;
        Ok(buffered_to_stream_response(buffered))
    }

    pub(super) async fn execute_fetch_available_models(
        &self,
        path_and_query: &str,
        inbound_headers: &HeaderMap,
        body: Bytes,
        auth_key: Option<(u64, u64)>,
        cached: Option<AccountRoute>,
    ) -> Result<CloudCodeBufferedResponse, CloudCodeError> {
        let body_hints_enterprise = detect_enterprise_from_fetch_body(&body);
        // Precedence: Enterprise entitlement -> per-account LRU -> User-Agent auth_method
        // -> default daily first (matching Google AI Pro / LS_CLOUD_CODE_ENDPOINT).
        let (prefer_prod, initial_strip, primary_authoritative) = if body_hints_enterprise {
            (true, true, true)
        } else if let Some(route) = cached {
            (
                route.use_prod,
                route.strip_user_project,
                route.authoritative,
            )
        } else if let Some(ua_prod) = detect_auth_method_from_headers(inbound_headers) {
            (ua_prod, false, true)
        } else {
            (false, false, false)
        };

        let plan = MetadataFallbackPlan {
            rpc_name: "fetchAvailableModels",
            prefer_prod,
            initial_strip,
            fallback_strip: body_hints_enterprise,
            force_strip_on_cache: body_hints_enterprise,
            fixed_require_exact_200_for_cache: true,
            primary_authoritative,
            evaluate: evaluate_fetch_available_models,
            tiebreak: MetadataTiebreak::FetchAvailableModels,
        };
        self.execute_buffered_metadata_rpc(path_and_query, inbound_headers, body, auth_key, plan)
            .await
    }

    async fn execute_buffered_metadata_rpc(
        &self,
        path_and_query: &str,
        inbound_headers: &HeaderMap,
        body: Bytes,
        auth_key: Option<(u64, u64)>,
        plan: MetadataFallbackPlan,
    ) -> Result<CloudCodeBufferedResponse, CloudCodeError> {
        match &self.inner.target {
            CloudCodeTarget::Fixed(url) => {
                let (resp, used_strip) = self
                    .send_buffered_metadata_with_403_retry(
                        url,
                        Method::POST,
                        path_and_query,
                        inbound_headers,
                        body,
                        BufferedRequestOptions {
                            force_identity_encoding: true,
                            strip_user_project: plan.initial_strip,
                        },
                    )
                    .await?;
                let status_ok = if plan.fixed_require_exact_200_for_cache {
                    resp.status == StatusCode::OK
                } else {
                    resp.status.is_success()
                };
                if status_ok
                    && (plan.force_strip_on_cache || used_strip)
                    && let Some(key) = auth_key
                {
                    self.inner.cache.put(
                        key,
                        AccountRoute {
                            use_prod: true,
                            strip_user_project: true,
                            authoritative: true,
                        },
                    );
                }
                tracing::info!(
                    rpc = plan.rpc_name,
                    upstream = %url,
                    status = resp.status.as_u16(),
                    "cloudcode metadata routed"
                );
                Ok(resp)
            }
            CloudCodeTarget::Auto {
                daily_url,
                prod_url,
            } => {
                let (primary_url, primary_is_prod, fallback_url, fallback_is_prod) =
                    if plan.prefer_prod {
                        (prod_url.as_str(), true, daily_url.as_str(), false)
                    } else {
                        (daily_url.as_str(), false, prod_url.as_str(), true)
                    };

                let primary_res = self
                    .send_buffered_metadata_with_403_retry(
                        primary_url,
                        Method::POST,
                        path_and_query,
                        inbound_headers,
                        body.clone(),
                        BufferedRequestOptions {
                            force_identity_encoding: true,
                            strip_user_project: plan.initial_strip,
                        },
                    )
                    .await;

                if let Ok((ref resp, used_strip)) = primary_res {
                    match (plan.evaluate)(resp) {
                        MetadataEvaluation::AcceptLoadCodeAssist { detected_prod } => {
                            if let Some(use_prod) = detected_prod {
                                self.record_account_route(auth_key, use_prod, used_strip, true);
                            }
                            tracing::info!(
                                rpc = plan.rpc_name,
                                upstream = %primary_url,
                                status = resp.status.as_u16(),
                                detected_prod = ?detected_prod,
                                "cloudcode metadata routed"
                            );
                            return primary_res.map(|(resp, _)| resp);
                        }
                        MetadataEvaluation::AcceptTargetRoute => {
                            self.record_account_route(
                                auth_key,
                                primary_is_prod,
                                plan.initial_strip || used_strip || plan.force_strip_on_cache,
                                plan.primary_authoritative,
                            );
                            tracing::info!(
                                rpc = plan.rpc_name,
                                upstream = %primary_url,
                                status = resp.status.as_u16(),
                                authoritative = plan.primary_authoritative,
                                "cloudcode metadata routed"
                            );
                            return primary_res.map(|(resp, _)| resp);
                        }
                        MetadataEvaluation::Reject => {}
                    }
                }

                // Metadata-only fallback to the alternate CloudCode host so PayGo/Business
                // and personal Google AI Pro accounts always resolve valid metadata.
                let fallback_res = self
                    .send_buffered_metadata_with_403_retry(
                        fallback_url,
                        Method::POST,
                        path_and_query,
                        inbound_headers,
                        body,
                        BufferedRequestOptions {
                            force_identity_encoding: true,
                            strip_user_project: plan.fallback_strip,
                        },
                    )
                    .await;

                if let Ok((ref resp, used_strip)) = fallback_res {
                    match (plan.evaluate)(resp) {
                        MetadataEvaluation::AcceptLoadCodeAssist { detected_prod } => {
                            let use_prod = detected_prod.unwrap_or(false);
                            self.record_account_route(auth_key, use_prod, used_strip, true);
                            tracing::info!(
                                rpc = plan.rpc_name,
                                upstream = %fallback_url,
                                status = resp.status.as_u16(),
                                fallback = true,
                                detected_prod = use_prod,
                                "cloudcode metadata routed"
                            );
                            return fallback_res.map(|(resp, _)| resp);
                        }
                        MetadataEvaluation::AcceptTargetRoute => {
                            self.record_account_route(
                                auth_key,
                                fallback_is_prod,
                                used_strip || plan.force_strip_on_cache,
                                true,
                            );
                            tracing::info!(
                                rpc = plan.rpc_name,
                                upstream = %fallback_url,
                                status = resp.status.as_u16(),
                                fallback = true,
                                authoritative = true,
                                "cloudcode metadata routed"
                            );
                            return fallback_res.map(|(resp, _)| resp);
                        }
                        MetadataEvaluation::Reject => {}
                    }
                }

                apply_metadata_tiebreak(
                    plan.tiebreak,
                    primary_res.map(|(resp, _)| resp),
                    fallback_res.map(|(resp, _)| resp),
                )
            }
        }
    }
}

fn evaluate_load_code_assist(resp: &CloudCodeBufferedResponse) -> MetadataEvaluation {
    if resp.status.is_success() {
        MetadataEvaluation::AcceptLoadCodeAssist {
            detected_prod: detect_prod_from_load_code_assist(&resp.body),
        }
    } else {
        MetadataEvaluation::Reject
    }
}

fn evaluate_list_experiments(resp: &CloudCodeBufferedResponse) -> MetadataEvaluation {
    if resp.status == StatusCode::OK && response_has_non_empty_flags(&resp.body) {
        MetadataEvaluation::AcceptTargetRoute
    } else {
        MetadataEvaluation::Reject
    }
}

fn evaluate_fetch_available_models(resp: &CloudCodeBufferedResponse) -> MetadataEvaluation {
    if resp.status == StatusCode::OK && response_has_non_empty_models(&resp.body) {
        MetadataEvaluation::AcceptTargetRoute
    } else {
        MetadataEvaluation::Reject
    }
}

fn apply_metadata_tiebreak(
    tiebreak: MetadataTiebreak,
    primary_res: Result<CloudCodeBufferedResponse, CloudCodeError>,
    fallback_res: Result<CloudCodeBufferedResponse, CloudCodeError>,
) -> Result<CloudCodeBufferedResponse, CloudCodeError> {
    match tiebreak {
        MetadataTiebreak::LoadCodeAssist => match (primary_res, fallback_res) {
            (Ok(p), _) => Ok(p),
            (Err(_), Ok(f)) => Ok(f),
            (Err(e), Err(_)) => Err(e),
        },
        MetadataTiebreak::ListExperiments => match (primary_res, fallback_res) {
            (Ok(p), Ok(_)) if p.status.is_success() => Ok(p),
            (_, Ok(f)) => Ok(f),
            (Ok(p), Err(_)) => Ok(p),
            (Err(_), Err(e)) => Err(e),
        },
        MetadataTiebreak::FetchAvailableModels => match (primary_res, fallback_res) {
            (Ok(p), _) if p.status == StatusCode::OK => Ok(p),
            (_, Ok(f)) if f.status == StatusCode::OK => Ok(f),
            (Ok(p), _) => Ok(p),
            (Err(_), Ok(f)) => Ok(f),
            (Err(e), Err(_)) => Err(e),
        },
    }
}

fn buffered_to_stream_response(buffered: CloudCodeBufferedResponse) -> CloudCodeStreamResponse {
    let headers = buffered.headers;
    let stream: ByteStream = Box::pin(futures_util::stream::once(async move { Ok(buffered.body) }));
    CloudCodeStreamResponse {
        status: buffered.status,
        headers,
        stream,
    }
}
