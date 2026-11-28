use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::Mutex;
use std::time::{SystemTime, UNIX_EPOCH};

use companion_core::{
    CloudCodeStatsSnapshot, GeminiUsageMetadata, MAX_RECENT_CALLS, MAX_SESSION_MODELS,
    ModelSessionStats, RecentCallSnapshot, StatsSnapshot, UsageStatsSnapshot,
};

const MAX_TRACKED_MODELS: usize = 128;
const MAX_TRACKED_SESSIONS: usize = 64;
const MAX_STATS_MODEL_LEN: usize = 128;

fn sanitize_stats_model(raw: &str) -> String {
    let cleaned: String = raw
        .chars()
        .filter(|c| c.is_ascii_graphic() && *c != '"' && *c != '\\')
        .take(MAX_STATS_MODEL_LEN)
        .collect();
    if cleaned.is_empty() {
        "unknown".to_string()
    } else {
        cleaned
    }
}

fn unix_timestamp_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .ok()
        .and_then(|d| u64::try_from(d.as_millis()).ok())
        .unwrap_or(0)
}

#[derive(Debug, Default)]
struct StatsBucket {
    requests: u64,
    prompt_tokens: u64,
    cached_tokens: u64,
    output_tokens: u64,
    reasoning_tokens: u64,
    recent_calls: VecDeque<RecentCallSnapshot>,
    by_model: HashMap<(String, String, String), ModelSessionStats>,
    seen_models: HashSet<(String, String)>,
}

impl StatsBucket {
    fn record_call(
        &mut self,
        provider: &str,
        source: &str,
        model: &str,
        usage: &GeminiUsageMetadata,
        latency_ms: u64,
        timestamp_ms: u64,
    ) {
        self.requests = self.requests.saturating_add(1);
        self.prompt_tokens = self.prompt_tokens.saturating_add(usage.prompt_token_count);
        self.cached_tokens = self
            .cached_tokens
            .saturating_add(usage.cached_content_token_count);
        self.output_tokens = self
            .output_tokens
            .saturating_add(usage.candidates_token_count);
        self.reasoning_tokens = self
            .reasoning_tokens
            .saturating_add(usage.thoughts_token_count);

        let seen_key = (provider.to_string(), model.to_string());
        let has_prior = self.seen_models.contains(&seen_key);
        let (cold_start, cache_break) = if !has_prior {
            (usage.cached_content_token_count == 0, false)
        } else {
            (
                false,
                usage.prompt_token_count > 0 && usage.cached_content_token_count == 0,
            )
        };
        if !has_prior && self.seen_models.len() < MAX_TRACKED_MODELS {
            self.seen_models.insert(seen_key);
        }

        if self.recent_calls.len() >= MAX_RECENT_CALLS {
            self.recent_calls.pop_front();
        }
        self.recent_calls.push_back(RecentCallSnapshot {
            timestamp_ms,
            provider: provider.to_string(),
            source: source.to_string(),
            model: model.to_string(),
            input_tokens: usage.prompt_token_count,
            cached_tokens: usage.cached_content_token_count,
            output_tokens: usage.candidates_token_count,
            reasoning_tokens: usage.thoughts_token_count,
            latency_ms,
            cold_start,
            cache_break,
        });

        let model_key = (provider.to_string(), source.to_string(), model.to_string());
        if let Some(entry) = self.by_model.get_mut(&model_key) {
            entry.requests = entry.requests.saturating_add(1);
            entry.input_tokens = entry.input_tokens.saturating_add(usage.prompt_token_count);
            entry.cached_tokens = entry
                .cached_tokens
                .saturating_add(usage.cached_content_token_count);
            entry.output_tokens = entry
                .output_tokens
                .saturating_add(usage.candidates_token_count);
            entry.reasoning_tokens = entry
                .reasoning_tokens
                .saturating_add(usage.thoughts_token_count);
            entry.total_latency_ms = entry.total_latency_ms.saturating_add(latency_ms);
        } else {
            if self.by_model.len() >= MAX_TRACKED_MODELS
                && let Some(evict_key) = self
                    .by_model
                    .iter()
                    .min_by_key(|(_, v)| (v.input_tokens, v.requests))
                    .map(|(k, _)| k.clone())
            {
                self.by_model.remove(&evict_key);
            }
            self.by_model.insert(
                model_key,
                ModelSessionStats {
                    provider: provider.to_string(),
                    source: source.to_string(),
                    model: model.to_string(),
                    model_display_name: None,
                    requests: 1,
                    input_tokens: usage.prompt_token_count,
                    cached_tokens: usage.cached_content_token_count,
                    output_tokens: usage.candidates_token_count,
                    reasoning_tokens: usage.thoughts_token_count,
                    total_latency_ms: latency_ms,
                },
            );
        }
    }

    fn to_usage_and_rows(
        &self,
    ) -> (
        UsageStatsSnapshot,
        Vec<RecentCallSnapshot>,
        Vec<ModelSessionStats>,
    ) {
        let cache_hit_ratio = if self.prompt_tokens > 0 {
            self.cached_tokens as f64 / self.prompt_tokens as f64
        } else {
            0.0
        };
        let recent_calls: Vec<RecentCallSnapshot> = self.recent_calls.iter().cloned().collect();
        let mut by_model: Vec<ModelSessionStats> = self.by_model.values().cloned().collect();
        by_model.sort_by(|a, b| {
            b.input_tokens
                .cmp(&a.input_tokens)
                .then_with(|| b.requests.cmp(&a.requests))
                .then_with(|| a.model.cmp(&b.model))
                .then_with(|| a.provider.cmp(&b.provider))
                .then_with(|| a.source.cmp(&b.source))
        });
        by_model.truncate(MAX_SESSION_MODELS);
        (
            UsageStatsSnapshot {
                requests: self.requests,
                prompt_tokens: self.prompt_tokens,
                cached_tokens: self.cached_tokens,
                output_tokens: self.output_tokens,
                reasoning_tokens: self.reasoning_tokens,
                cache_hit_ratio,
            },
            recent_calls,
            by_model,
        )
    }
}

#[derive(Debug, Default)]
struct StatsState {
    global: StatsBucket,
    sessions: HashMap<String, (u64, StatsBucket)>,
    touch_seq: u64,
    last_catalog_refresh_at_ms: Option<u64>,
    last_catalog_included_proxy_models: Option<bool>,
}

#[derive(Debug, Default)]
pub struct ProxyStats {
    state: Mutex<StatsState>,
}

impl ProxyStats {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn record(&self, usage: &GeminiUsageMetadata) {
        self.record_call("gemini", "proxy", "unknown", usage, 0);
    }

    pub fn record_call(
        &self,
        provider: &str,
        source: &str,
        model: &str,
        usage: &GeminiUsageMetadata,
        latency_ms: u64,
    ) {
        self.record_call_for_session(provider, source, model, usage, latency_ms, None);
    }

    pub fn record_call_for_session(
        &self,
        provider: &str,
        source: &str,
        model: &str,
        usage: &GeminiUsageMetadata,
        latency_ms: u64,
        conversation_id: Option<&str>,
    ) {
        let mut s = self.state.lock().unwrap_or_else(|e| e.into_inner());
        let model = sanitize_stats_model(model);
        let timestamp_ms = unix_timestamp_ms();
        s.global
            .record_call(provider, source, &model, usage, latency_ms, timestamp_ms);

        if let Some(conv_id) = conversation_id.and_then(crate::session::sanitize_conversation_id) {
            s.touch_seq = s.touch_seq.saturating_add(1);
            let seq = s.touch_seq;
            if let Some((last_seq, bucket)) = s.sessions.get_mut(&conv_id) {
                *last_seq = seq;
                bucket.record_call(provider, source, &model, usage, latency_ms, timestamp_ms);
            } else {
                if s.sessions.len() >= MAX_TRACKED_SESSIONS
                    && let Some(evict_id) = s
                        .sessions
                        .iter()
                        .min_by_key(|(_, (last_seq, _))| *last_seq)
                        .map(|(k, _)| k.clone())
                {
                    s.sessions.remove(&evict_id);
                }
                let mut bucket = StatsBucket::default();
                bucket.record_call(provider, source, &model, usage, latency_ms, timestamp_ms);
                s.sessions.insert(conv_id, (seq, bucket));
            }
        }
    }

    pub fn record_catalog_refresh(&self, timestamp_ms: u64, included_proxy_models: bool) {
        let mut s = self.state.lock().unwrap_or_else(|e| e.into_inner());
        s.last_catalog_refresh_at_ms = Some(timestamp_ms);
        s.last_catalog_included_proxy_models = Some(included_proxy_models);
    }

    pub fn snapshot(&self) -> StatsSnapshot {
        self.snapshot_with_cloudcode_and_rss("auto", None, None)
    }

    pub fn snapshot_with_cloudcode(
        &self,
        cloudcode_mode: &str,
        cloudcode_resolved_upstream: Option<String>,
    ) -> StatsSnapshot {
        self.snapshot_with_cloudcode_and_rss(cloudcode_mode, cloudcode_resolved_upstream, None)
    }

    pub fn snapshot_with_cloudcode_and_rss(
        &self,
        cloudcode_mode: &str,
        cloudcode_resolved_upstream: Option<String>,
        proxy_rss_bytes: Option<u64>,
    ) -> StatsSnapshot {
        self.snapshot_for_session(
            None,
            cloudcode_mode,
            cloudcode_resolved_upstream,
            proxy_rss_bytes,
        )
    }

    pub fn snapshot_for_session(
        &self,
        conversation_id: Option<&str>,
        cloudcode_mode: &str,
        cloudcode_resolved_upstream: Option<String>,
        proxy_rss_bytes: Option<u64>,
    ) -> StatsSnapshot {
        let s = self.state.lock().unwrap_or_else(|e| e.into_inner());
        let (usage, recent_calls, by_model) =
            match conversation_id.and_then(crate::session::sanitize_conversation_id) {
                Some(conv_id) => match s.sessions.get(&conv_id) {
                    Some((_, bucket)) => bucket.to_usage_and_rows(),
                    None => (UsageStatsSnapshot::default(), Vec::new(), Vec::new()),
                },
                None => s.global.to_usage_and_rows(),
            };

        StatsSnapshot {
            usage,
            cloudcode: CloudCodeStatsSnapshot {
                mode: cloudcode_mode.to_string(),
                resolved_upstream: cloudcode_resolved_upstream,
                last_catalog_refresh_at_ms: s.last_catalog_refresh_at_ms,
                last_catalog_included_proxy_models: s.last_catalog_included_proxy_models,
            },
            proxy_rss_bytes,
            max_context_tokens: None,
            recent_calls,
            by_model,
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::thread;

    use super::*;

    #[test]
    fn computes_cache_hit_ratio_accurately() {
        let stats = ProxyStats::new();
        stats.record(&GeminiUsageMetadata {
            prompt_token_count: 1000,
            candidates_token_count: 200,
            cached_content_token_count: 750,
            thoughts_token_count: 50,
        });
        stats.record(&GeminiUsageMetadata {
            prompt_token_count: 1000,
            candidates_token_count: 100,
            cached_content_token_count: 250,
            thoughts_token_count: 25,
        });

        let snap = stats.snapshot();
        assert_eq!(snap.usage.requests, 2);
        assert_eq!(snap.usage.prompt_tokens, 2000);
        assert_eq!(snap.usage.cached_tokens, 1000);
        assert_eq!(snap.usage.output_tokens, 300);
        assert_eq!(snap.usage.reasoning_tokens, 75);
        assert!((snap.usage.cache_hit_ratio - 0.5).abs() < 1e-12);

        // Verify exact serialized JSON structure.
        let value = serde_json::to_value(snap).expect("serialize snapshot");
        let obj = value.as_object().expect("json object");
        assert_eq!(obj.len(), 6);
        assert!(obj.contains_key("proxy_rss_bytes"));
        assert!(obj.contains_key("max_context_tokens"));
        assert!(obj.contains_key("recent_calls"));
        assert!(obj.contains_key("by_model"));
        let usage = obj
            .get("usage")
            .and_then(|v| v.as_object())
            .expect("usage object");
        assert_eq!(usage.len(), 6);
        assert!(usage.contains_key("requests"));
        assert!(usage.contains_key("prompt_tokens"));
        assert!(usage.contains_key("cached_tokens"));
        assert!(usage.contains_key("output_tokens"));
        assert!(usage.contains_key("reasoning_tokens"));
        assert!(usage.contains_key("cache_hit_ratio"));
        let cloudcode = obj
            .get("cloudcode")
            .and_then(|v| v.as_object())
            .expect("cloudcode object");
        assert_eq!(cloudcode.len(), 4);
        assert!(cloudcode.contains_key("mode"));
        assert!(cloudcode.contains_key("resolved_upstream"));
        assert!(cloudcode.contains_key("last_catalog_refresh_at_ms"));
        assert!(cloudcode.contains_key("last_catalog_included_proxy_models"));
    }

    #[test]
    fn records_catalog_refresh_and_deserializes_partial_usage_only_snapshot() {
        let stats = ProxyStats::new();
        let initial = stats.snapshot_with_cloudcode("auto", None);
        assert_eq!(initial.cloudcode.last_catalog_refresh_at_ms, None);
        assert_eq!(initial.cloudcode.last_catalog_included_proxy_models, None);
        assert_eq!(initial.cloudcode.mode, "auto");
        assert_eq!(initial.cloudcode.resolved_upstream, None);

        stats.record_catalog_refresh(1_700_000_000_123, false);
        let day0 = stats.snapshot_with_cloudcode(
            "auto",
            Some("https://daily-cloudcode-pa.googleapis.com".to_string()),
        );
        assert_eq!(
            day0.cloudcode.last_catalog_refresh_at_ms,
            Some(1_700_000_000_123)
        );
        assert_eq!(
            day0.cloudcode.last_catalog_included_proxy_models,
            Some(false)
        );
        assert_eq!(day0.cloudcode.mode, "auto");
        assert_eq!(
            day0.cloudcode.resolved_upstream.as_deref(),
            Some("https://daily-cloudcode-pa.googleapis.com")
        );

        stats.record_catalog_refresh(1_700_000_005_000, true);
        let merged = stats.snapshot_with_cloudcode(
            "fixed",
            Some("https://cloudcode-pa.googleapis.com".to_string()),
        );
        assert_eq!(
            merged.cloudcode.last_catalog_refresh_at_ms,
            Some(1_700_000_005_000)
        );
        assert_eq!(
            merged.cloudcode.last_catalog_included_proxy_models,
            Some(true)
        );
        assert_eq!(merged.cloudcode.mode, "fixed");
        assert_eq!(
            merged.cloudcode.resolved_upstream.as_deref(),
            Some("https://cloudcode-pa.googleapis.com")
        );

        // Minimal structured JSON payload omitting `cloudcode` must deserialize via #[serde(default)].
        let partial: StatsSnapshot = serde_json::from_str(
            r#"{
                "usage": {
                    "requests": 3,
                    "prompt_tokens": 300,
                    "cached_tokens": 150,
                    "output_tokens": 45,
                    "reasoning_tokens": 10,
                    "cache_hit_ratio": 0.5
                }
            }"#,
        )
        .expect("partial usage-only snapshot must deserialize");
        assert_eq!(partial.usage.requests, 3);
        assert_eq!(partial.cloudcode, CloudCodeStatsSnapshot::default());
    }

    #[test]
    fn zero_prompt_tokens_returns_zero_ratio_without_nan_or_inf() {
        let stats = ProxyStats::new();
        let initial = stats.snapshot();
        assert_eq!(initial.usage.requests, 0);
        assert_eq!(initial.usage.cache_hit_ratio, 0.0);
        assert!(initial.usage.cache_hit_ratio.is_finite());

        stats.record(&GeminiUsageMetadata {
            prompt_token_count: 0,
            candidates_token_count: 10,
            cached_content_token_count: 5,
            thoughts_token_count: 2,
        });
        let after_zero_prompt = stats.snapshot();
        assert_eq!(after_zero_prompt.usage.requests, 1);
        assert_eq!(after_zero_prompt.usage.cache_hit_ratio, 0.0);
        assert!(after_zero_prompt.usage.cache_hit_ratio.is_finite());

        // Saturating arithmetic on u64::MAX must never wrap or overflow.
        stats.record(&GeminiUsageMetadata {
            prompt_token_count: u64::MAX,
            candidates_token_count: u64::MAX,
            cached_content_token_count: u64::MAX,
            thoughts_token_count: u64::MAX,
        });
        stats.record(&GeminiUsageMetadata {
            prompt_token_count: 10,
            candidates_token_count: 10,
            cached_content_token_count: 10,
            thoughts_token_count: 10,
        });
        let saturated = stats.snapshot();
        assert_eq!(saturated.usage.requests, 3);
        assert_eq!(saturated.usage.prompt_tokens, u64::MAX);
        assert_eq!(saturated.usage.cached_tokens, u64::MAX);
        assert_eq!(saturated.usage.output_tokens, u64::MAX);
        assert_eq!(saturated.usage.reasoning_tokens, u64::MAX);
        assert!(saturated.usage.cache_hit_ratio.is_finite());
        assert!((saturated.usage.cache_hit_ratio - 1.0).abs() < 1e-12);
    }

    #[test]
    fn concurrent_updates_and_snapshots_stay_consistent() {
        let stats = Arc::new(ProxyStats::new());
        let mut writers = Vec::new();
        for _ in 0..8 {
            let s = Arc::clone(&stats);
            writers.push(thread::spawn(move || {
                for _ in 0..200 {
                    s.record(&GeminiUsageMetadata {
                        prompt_token_count: 100,
                        candidates_token_count: 20,
                        cached_content_token_count: 50,
                        thoughts_token_count: 5,
                    });
                }
            }));
        }

        let mut readers = Vec::new();
        for _ in 0..4 {
            let s = Arc::clone(&stats);
            readers.push(thread::spawn(move || {
                for _ in 0..400 {
                    let snap = s.snapshot();
                    // Atomic point-in-time snapshot invariants: never torn!
                    assert_eq!(snap.usage.prompt_tokens, snap.usage.requests * 100);
                    assert_eq!(snap.usage.cached_tokens, snap.usage.requests * 50);
                    assert_eq!(snap.usage.output_tokens, snap.usage.requests * 20);
                    assert_eq!(snap.usage.reasoning_tokens, snap.usage.requests * 5);
                    if snap.usage.requests > 0 {
                        assert!((snap.usage.cache_hit_ratio - 0.5).abs() < 1e-12);
                    } else {
                        assert_eq!(snap.usage.cache_hit_ratio, 0.0);
                    }
                }
            }));
        }

        for handle in writers {
            handle.join().expect("writer join");
        }
        for handle in readers {
            handle.join().expect("reader join");
        }

        let final_snap = stats.snapshot();
        assert_eq!(final_snap.usage.requests, 1600);
        assert_eq!(final_snap.usage.prompt_tokens, 160_000);
        assert_eq!(final_snap.usage.cached_tokens, 80_000);
        assert_eq!(final_snap.usage.output_tokens, 32_000);
        assert_eq!(final_snap.usage.reasoning_tokens, 8_000);
        assert!((final_snap.usage.cache_hit_ratio - 0.5).abs() < 1e-12);
    }

    #[test]
    fn tracks_cold_start_cache_break_ring_buffer_and_sorted_by_model_caps() {
        let stats = ProxyStats::new();
        let u = |prompt, cached, out, think| GeminiUsageMetadata {
            prompt_token_count: prompt,
            cached_content_token_count: cached,
            candidates_token_count: out,
            thoughts_token_count: think,
        };

        // Call 1 on model A with cached=0 -> cold_start=true, cache_break=false
        stats.record_call(
            "gemini",
            "proxy",
            "gemini-4-carbon",
            &u(1000, 0, 100, 10),
            200,
        );
        // Call 2 on model A with cached=500 -> cold_start=false, cache_break=false
        stats.record_call(
            "gemini",
            "proxy",
            "gemini-4-carbon",
            &u(1000, 500, 100, 10),
            150,
        );
        // Call 3 on model A with prompt=1000, cached=0 -> cold_start=false, cache_break=true
        stats.record_call(
            "gemini",
            "proxy",
            "gemini-4-carbon",
            &u(1000, 0, 100, 10),
            180,
        );
        // Call 4 on model A with prompt=0, cached=0 -> cold_start=false, cache_break=false
        stats.record_call("gemini", "proxy", "gemini-4-carbon", &u(0, 0, 50, 0), 90);
        // Call 1 on model B (antigravity/builtin) with cached=0 -> cold_start=true, cache_break=false
        stats.record_call(
            "antigravity",
            "builtin",
            "gemini-3.8-pro",
            &u(500, 0, 80, 0),
            120,
        );

        let snap = stats.snapshot_with_cloudcode_and_rss("auto", None, Some(12_345_678));
        assert_eq!(snap.proxy_rss_bytes, Some(12_345_678));
        assert_eq!(snap.recent_calls.len(), 5);
        assert_eq!(
            (
                snap.recent_calls[0].cold_start,
                snap.recent_calls[0].cache_break
            ),
            (true, false)
        );
        assert_eq!(
            (
                snap.recent_calls[1].cold_start,
                snap.recent_calls[1].cache_break
            ),
            (false, false)
        );
        assert_eq!(
            (
                snap.recent_calls[2].cold_start,
                snap.recent_calls[2].cache_break
            ),
            (false, true)
        );
        assert_eq!(
            (
                snap.recent_calls[3].cold_start,
                snap.recent_calls[3].cache_break
            ),
            (false, false)
        );
        assert_eq!(
            (
                snap.recent_calls[4].cold_start,
                snap.recent_calls[4].cache_break
            ),
            (true, false)
        );
        assert_eq!(snap.recent_calls[4].provider, "antigravity");
        assert_eq!(snap.recent_calls[4].source, "builtin");

        // Ring buffer caps at MAX_RECENT_CALLS (32), evicting oldest chronologically.
        let ring = ProxyStats::new();
        for i in 0..40u64 {
            ring.record_call(
                "gemini",
                "proxy",
                &format!("model-{i:02}"),
                &u(100 + i, 10, 5, 0),
                i,
            );
        }
        let ring_snap = ring.snapshot();
        assert_eq!(ring_snap.recent_calls.len(), MAX_RECENT_CALLS);
        assert_eq!(ring_snap.recent_calls[0].model, "model-08");
        assert_eq!(
            ring_snap.recent_calls[MAX_RECENT_CALLS - 1].model,
            "model-39"
        );

        // by_model caps at MAX_SESSION_MODELS (32), sorted descending by (input_tokens, requests).
        assert_eq!(ring_snap.by_model.len(), MAX_SESSION_MODELS);
        assert_eq!(ring_snap.by_model[0].model, "model-39");
        assert_eq!(ring_snap.by_model[0].input_tokens, 139);
        assert_eq!(ring_snap.by_model[MAX_SESSION_MODELS - 1].model, "model-08");
    }

    #[test]
    fn record_call_sanitizes_and_caps_model_ids_to_stay_under_probe_budget_and_sorts_by_source() {
        let stats = ProxyStats::new();
        let usage = GeminiUsageMetadata {
            prompt_token_count: 1000,
            cached_content_token_count: 500,
            candidates_token_count: 100,
            thoughts_token_count: 10,
        };

        // 32 distinct 1200+ char model IDs must be truncated to <= 128 chars so /api/stats stays < 64 KiB.
        for i in 0..32 {
            let huge = format!("gemini-4-{i:02}-{}", "a".repeat(1200));
            stats.record_call("gemini", "proxy", &huge, &usage, 50);
        }
        let snap = stats.snapshot();
        assert_eq!(snap.recent_calls.len(), 32);
        assert_eq!(snap.by_model.len(), 32);
        for call in &snap.recent_calls {
            assert!(
                call.model.len() <= 128,
                "expected model len <= 128, got {}",
                call.model.len()
            );
        }
        for entry in &snap.by_model {
            assert!(
                entry.model.len() <= 128,
                "expected by_model len <= 128, got {}",
                entry.model.len()
            );
        }
        let json_bytes = serde_json::to_vec(&snap).expect("serialize snapshot");
        assert!(
            json_bytes.len() < companion_providers::MAX_PROBE_BODY_BYTES,
            "stats snapshot must stay below 64 KiB probe cap, got {} bytes",
            json_bytes.len()
        );

        // Control chars, quotes, backslashes, and non-ASCII stripped; empty falls back to "unknown".
        let sanitized = ProxyStats::new();
        sanitized.record_call("gemini", "proxy", "mod\"el\\\n\r\t\x00🌍-ok", &usage, 10);
        sanitized.record_call("gemini", "proxy", " \n\t\"\\", &usage, 10);
        let san_snap = sanitized.snapshot();
        assert_eq!(san_snap.recent_calls[0].model, "model-ok");
        assert_eq!(san_snap.recent_calls[1].model, "unknown");

        // Tie-breaker sorts deterministically by source when (input_tokens, requests, model, provider) tie.
        let tie = ProxyStats::new();
        tie.record_call("gemini", "z-source", "gemini-4-carbon", &usage, 10);
        tie.record_call("gemini", "a-source", "gemini-4-carbon", &usage, 10);
        let tie_snap = tie.snapshot();
        assert_eq!(tie_snap.by_model.len(), 2);
        assert_eq!(tie_snap.by_model[0].source, "a-source");
        assert_eq!(tie_snap.by_model[1].source, "z-source");
    }

    #[test]
    fn isolates_per_conversation_stats_while_preserving_global_and_lru_cap() {
        let stats = ProxyStats::new();
        stats.record_catalog_refresh(1_700_000_000_000, true);
        let usage_a = GeminiUsageMetadata {
            prompt_token_count: 1000,
            candidates_token_count: 100,
            cached_content_token_count: 800,
            thoughts_token_count: 20,
        };
        let usage_b = GeminiUsageMetadata {
            prompt_token_count: 500,
            candidates_token_count: 50,
            cached_content_token_count: 0,
            thoughts_token_count: 10,
        };

        stats.record_call_for_session(
            "gemini",
            "proxy",
            "gemini-4-neon",
            &usage_a,
            100,
            Some("conv-session-aaaaaa"),
        );
        stats.record_call_for_session(
            "gemini",
            "proxy",
            "gemini-4-carbon",
            &usage_b,
            200,
            Some("conv-session-bbbbbb"),
        );

        let global = stats.snapshot_for_session(
            None,
            "auto",
            Some("https://cloudcode-pa.googleapis.com".to_string()),
            Some(20_000_000),
        );
        assert_eq!(global.usage.requests, 2);
        assert_eq!(global.recent_calls.len(), 2);
        assert_eq!(global.proxy_rss_bytes, Some(20_000_000));

        let snap_a = stats.snapshot_for_session(
            Some("conv-session-aaaaaa"),
            "auto",
            Some("https://cloudcode-pa.googleapis.com".to_string()),
            Some(20_000_000),
        );
        assert_eq!(snap_a.usage.requests, 1);
        assert_eq!(snap_a.usage.prompt_tokens, 1000);
        assert_eq!(snap_a.recent_calls.len(), 1);
        assert_eq!(snap_a.recent_calls[0].model, "gemini-4-neon");
        assert_eq!(snap_a.proxy_rss_bytes, Some(20_000_000));
        assert_eq!(
            snap_a.cloudcode.last_catalog_included_proxy_models,
            Some(true)
        );

        let snap_unknown = stats.snapshot_for_session(
            Some("conv-session-empty1"),
            "auto",
            Some("https://cloudcode-pa.googleapis.com".to_string()),
            Some(20_000_000),
        );
        assert_eq!(snap_unknown.usage.requests, 0);
        assert!(snap_unknown.recent_calls.is_empty());
        assert!(snap_unknown.by_model.is_empty());
        assert_eq!(snap_unknown.proxy_rss_bytes, Some(20_000_000));

        // Eviction past MAX_TRACKED_SESSIONS drops oldest session cleanly.
        for i in 0..(MAX_TRACKED_SESSIONS + 5) {
            let id = format!("conv-lru-{i:04}");
            stats.record_call_for_session(
                "gemini",
                "proxy",
                "gemini-4-neon",
                &usage_b,
                10,
                Some(&id),
            );
        }
        let evicted = stats.snapshot_for_session(Some("conv-session-aaaaaa"), "auto", None, None);
        assert_eq!(evicted.usage.requests, 0);
        let newest = stats.snapshot_for_session(
            Some(&format!("conv-lru-{:04}", MAX_TRACKED_SESSIONS + 4)),
            "auto",
            None,
            None,
        );
        assert_eq!(newest.usage.requests, 1);
    }
}
