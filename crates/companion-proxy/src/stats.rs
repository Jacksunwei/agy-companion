use std::sync::Mutex;

use companion_core::{
    CloudCodeStatsSnapshot, GeminiUsageMetadata, StatsSnapshot, UsageStatsSnapshot,
};

#[derive(Debug, Default, Clone, Copy)]
struct StatsCounters {
    requests: u64,
    prompt_tokens: u64,
    cached_tokens: u64,
    output_tokens: u64,
    reasoning_tokens: u64,
    last_catalog_refresh_at_ms: Option<u64>,
    last_catalog_included_proxy_models: Option<bool>,
}

#[derive(Debug, Default)]
pub struct ProxyStats {
    counters: Mutex<StatsCounters>,
}

impl ProxyStats {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn record(&self, usage: &GeminiUsageMetadata) {
        let mut c = self.counters.lock().unwrap_or_else(|e| e.into_inner());
        c.requests = c.requests.saturating_add(1);
        c.prompt_tokens = c.prompt_tokens.saturating_add(usage.prompt_token_count);
        c.cached_tokens = c
            .cached_tokens
            .saturating_add(usage.cached_content_token_count);
        c.output_tokens = c.output_tokens.saturating_add(usage.candidates_token_count);
        c.reasoning_tokens = c
            .reasoning_tokens
            .saturating_add(usage.thoughts_token_count);
    }

    pub fn record_catalog_refresh(&self, timestamp_ms: u64, included_proxy_models: bool) {
        let mut c = self.counters.lock().unwrap_or_else(|e| e.into_inner());
        c.last_catalog_refresh_at_ms = Some(timestamp_ms);
        c.last_catalog_included_proxy_models = Some(included_proxy_models);
    }

    pub fn snapshot(&self) -> StatsSnapshot {
        self.snapshot_with_cloudcode("auto", None)
    }

    pub fn snapshot_with_cloudcode(
        &self,
        cloudcode_mode: &str,
        cloudcode_resolved_upstream: Option<String>,
    ) -> StatsSnapshot {
        let c = *self.counters.lock().unwrap_or_else(|e| e.into_inner());
        let cache_hit_ratio = if c.prompt_tokens > 0 {
            c.cached_tokens as f64 / c.prompt_tokens as f64
        } else {
            0.0
        };
        StatsSnapshot {
            usage: UsageStatsSnapshot {
                requests: c.requests,
                prompt_tokens: c.prompt_tokens,
                cached_tokens: c.cached_tokens,
                output_tokens: c.output_tokens,
                reasoning_tokens: c.reasoning_tokens,
                cache_hit_ratio,
            },
            cloudcode: CloudCodeStatsSnapshot {
                mode: cloudcode_mode.to_string(),
                resolved_upstream: cloudcode_resolved_upstream,
                last_catalog_refresh_at_ms: c.last_catalog_refresh_at_ms,
                last_catalog_included_proxy_models: c.last_catalog_included_proxy_models,
            },
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
        assert_eq!(obj.len(), 2);
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
}
