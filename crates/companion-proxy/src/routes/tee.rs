use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};
use std::time::Instant;

use bytes::Bytes;
use companion_core::{GeminiUsageMetadata, UsageEvent, UsageSink, is_printable_ascii_with_space};
use companion_providers::{ByteStream, ProviderError};
use futures_util::Stream;

use crate::InFlightGuard;
use crate::sse::SseUsageTracker;
use crate::stats::ProxyStats;

pub const DEFAULT_MAX_JSON_TEE_BYTES: usize = 8 * 1024 * 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum UsageOrigin {
    GeminiProxy,
    AntigravityBuiltin,
}

impl UsageOrigin {
    pub(super) const fn provider(self) -> &'static str {
        match self {
            Self::GeminiProxy => "gemini",
            Self::AntigravityBuiltin => "antigravity",
        }
    }

    pub(super) const fn source(self) -> &'static str {
        match self {
            Self::GeminiProxy => "proxy",
            Self::AntigravityBuiltin => "builtin",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum UsageTeeMode {
    Sse,
    UnaryJson,
}

fn has_alt_sse(path_and_query: &str) -> bool {
    let Some((_, query)) = path_and_query.split_once('?') else {
        return false;
    };
    query.split('&').any(|pair| {
        pair.split_once('=')
            .is_some_and(|(k, v)| k.eq_ignore_ascii_case("alt") && v.eq_ignore_ascii_case("sse"))
    })
}

pub(super) fn select_tee_mode(
    upstream_content_type: Option<&str>,
    path_and_query: &str,
) -> UsageTeeMode {
    let media_type = upstream_content_type.and_then(|ct| {
        if !is_printable_ascii_with_space(ct.as_bytes()) {
            return None;
        }
        let essence = ct
            .split(';')
            .next()
            .unwrap_or("")
            .trim_matches(|c| c == ' ' || c == '\t');
        if essence.is_empty() {
            None
        } else {
            Some(essence)
        }
    });

    match media_type {
        Some(mt) if mt.eq_ignore_ascii_case("text/event-stream") => UsageTeeMode::Sse,
        Some(mt) if mt.eq_ignore_ascii_case("application/json") => UsageTeeMode::UnaryJson,
        None => {
            let path = path_and_query
                .split_once('?')
                .map_or(path_and_query, |(p, _)| p);
            if has_alt_sse(path_and_query) || path.ends_with(":streamGenerateContent") {
                UsageTeeMode::Sse
            } else {
                UsageTeeMode::UnaryJson
            }
        }
        Some(_) => {
            if has_alt_sse(path_and_query) {
                UsageTeeMode::Sse
            } else {
                UsageTeeMode::UnaryJson
            }
        }
    }
}

pub(super) enum UsageCollector {
    Sse(SseUsageTracker),
    UnaryJson {
        buf: Vec<u8>,
        max_bytes: usize,
        overflowed: bool,
    },
}

impl UsageCollector {
    pub(super) fn from_mode(mode: UsageTeeMode) -> Self {
        match mode {
            UsageTeeMode::Sse => Self::Sse(SseUsageTracker::new()),
            UsageTeeMode::UnaryJson => Self::UnaryJson {
                buf: Vec::new(),
                max_bytes: DEFAULT_MAX_JSON_TEE_BYTES,
                overflowed: false,
            },
        }
    }

    fn push_chunk(&mut self, chunk: &[u8]) {
        match self {
            Self::Sse(tracker) => tracker.push_chunk(chunk),
            Self::UnaryJson {
                buf,
                max_bytes,
                overflowed,
            } => {
                if *overflowed {
                    return;
                }
                if buf.len().saturating_add(chunk.len()) > *max_bytes {
                    *buf = Vec::new();
                    *overflowed = true;
                    return;
                }
                buf.extend_from_slice(chunk);
            }
        }
    }

    fn finish(&mut self) -> (Option<GeminiUsageMetadata>, Option<String>) {
        match self {
            Self::Sse(tracker) => tracker.finish_with_response_id(),
            Self::UnaryJson {
                buf, overflowed, ..
            } => {
                if *overflowed || buf.is_empty() {
                    return (None, None);
                }
                let parsed = GeminiUsageMetadata::parse_usage_and_response_id_from_json(buf);
                *buf = Vec::new();
                parsed
            }
        }
    }
}

pub(super) struct TrackedStream {
    pub(super) inner: ByteStream,
    pub(super) guard: InFlightGuard,
}

impl Stream for TrackedStream {
    type Item = Result<Bytes, ProviderError>;

    fn poll_next(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        let this = self.get_mut();
        match Pin::new(&mut this.inner).poll_next(cx) {
            Poll::Pending => Poll::Pending,
            Poll::Ready(Some(item)) => {
                this.guard.touch();
                Poll::Ready(Some(item))
            }
            Poll::Ready(None) => Poll::Ready(None),
        }
    }
}

pub(super) struct UsageTeeStream {
    pub(super) inner: ByteStream,
    pub(super) collector: UsageCollector,
    pub(super) stats: Arc<ProxyStats>,
    pub(super) usage_sink: Arc<dyn UsageSink>,
    pub(super) origin: UsageOrigin,
    pub(super) model: String,
    pub(super) conversation_id: Option<String>,
    pub(super) started_at: Instant,
    pub(super) finalized: bool,
    pub(super) errored: bool,
    pub(super) guard: InFlightGuard,
}

impl Stream for UsageTeeStream {
    type Item = Result<Bytes, ProviderError>;

    fn poll_next(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        let this = self.get_mut();
        match Pin::new(&mut this.inner).poll_next(cx) {
            Poll::Pending => Poll::Pending,
            Poll::Ready(Some(Ok(bytes))) => {
                this.guard.touch();
                this.collector.push_chunk(bytes.as_ref());
                Poll::Ready(Some(Ok(bytes)))
            }
            Poll::Ready(Some(Err(err))) => {
                this.guard.touch();
                this.errored = true;
                this.finalized = true;
                Poll::Ready(Some(Err(err)))
            }
            Poll::Ready(None) => {
                if !this.finalized && !this.errored {
                    this.finalized = true;
                    let (usage, response_id) = this.collector.finish();
                    if let Some(usage) = usage {
                        let latency_ms = u64::try_from(this.started_at.elapsed().as_millis())
                            .unwrap_or(u64::MAX);
                        this.stats.record_call_for_session(
                            this.origin.provider(),
                            this.origin.source(),
                            &this.model,
                            &usage,
                            latency_ms,
                            this.conversation_id.as_deref(),
                        );
                        let event = match this.origin {
                            UsageOrigin::GeminiProxy => {
                                UsageEvent::from_gemini(&this.model, &usage, latency_ms)
                            }
                            UsageOrigin::AntigravityBuiltin => {
                                UsageEvent::from_antigravity_builtin(
                                    &this.model,
                                    &usage,
                                    latency_ms,
                                )
                            }
                        }
                        .with_response_id(response_id)
                        .with_conversation_id(this.conversation_id.clone());
                        this.usage_sink.record(event);
                    }
                }
                Poll::Ready(None)
            }
        }
    }
}
