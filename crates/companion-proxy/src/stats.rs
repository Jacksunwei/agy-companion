use std::sync::Mutex;

use companion_core::GeminiUsageMetadata;
use serde::{Deserialize, Serialize};

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

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct StatsSnapshot {
    pub requests: u64,
    pub prompt_tokens: u64,
    pub cached_tokens: u64,
    pub output_tokens: u64,
    pub reasoning_tokens: u64,
    pub cache_hit_ratio: f64,
    #[serde(default)]
    pub last_catalog_refresh_at_ms: Option<u64>,
    #[serde(default)]
    pub last_catalog_included_proxy_models: Option<bool>,
    #[serde(default)]
    pub cloudcode_mode: String,
    #[serde(default)]
    pub cloudcode_resolved_upstream: Option<String>,
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
            requests: c.requests,
            prompt_tokens: c.prompt_tokens,
            cached_tokens: c.cached_tokens,
            output_tokens: c.output_tokens,
            reasoning_tokens: c.reasoning_tokens,
            cache_hit_ratio,
            last_catalog_refresh_at_ms: c.last_catalog_refresh_at_ms,
            last_catalog_included_proxy_models: c.last_catalog_included_proxy_models,
            cloudcode_mode: cloudcode_mode.to_string(),
            cloudcode_resolved_upstream,
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
        assert_eq!(snap.requests, 2);
        assert_eq!(snap.prompt_tokens, 2000);
        assert_eq!(snap.cached_tokens, 1000);
        assert_eq!(snap.output_tokens, 300);
        assert_eq!(snap.reasoning_tokens, 75);
        assert!((snap.cache_hit_ratio - 0.5).abs() < 1e-12);

        // Verify exact serialized JSON keys.
        let value = serde_json::to_value(snap).expect("serialize snapshot");
        let obj = value.as_object().expect("json object");
        assert_eq!(obj.len(), 10);
        assert!(obj.contains_key("requests"));
        assert!(obj.contains_key("prompt_tokens"));
        assert!(obj.contains_key("cached_tokens"));
        assert!(obj.contains_key("output_tokens"));
        assert!(obj.contains_key("reasoning_tokens"));
        assert!(obj.contains_key("cache_hit_ratio"));
        assert!(obj.contains_key("last_catalog_refresh_at_ms"));
        assert!(obj.contains_key("last_catalog_included_proxy_models"));
        assert!(obj.contains_key("cloudcode_mode"));
        assert!(obj.contains_key("cloudcode_resolved_upstream"));
    }

    #[test]
    fn records_catalog_refresh_and_deserializes_legacy_six_field_snapshot() {
        let stats = ProxyStats::new();
        let initial = stats.snapshot_with_cloudcode("auto", None);
        assert_eq!(initial.last_catalog_refresh_at_ms, None);
        assert_eq!(initial.last_catalog_included_proxy_models, None);
        assert_eq!(initial.cloudcode_mode, "auto");
        assert_eq!(initial.cloudcode_resolved_upstream, None);

        stats.record_catalog_refresh(1_700_000_000_123, false);
        let day0 = stats.snapshot_with_cloudcode(
            "auto",
            Some("https://daily-cloudcode-pa.googleapis.com".to_string()),
        );
        assert_eq!(day0.last_catalog_refresh_at_ms, Some(1_700_000_000_123));
        assert_eq!(day0.last_catalog_included_proxy_models, Some(false));
        assert_eq!(day0.cloudcode_mode, "auto");
        assert_eq!(
            day0.cloudcode_resolved_upstream.as_deref(),
            Some("https://daily-cloudcode-pa.googleapis.com")
        );

        stats.record_catalog_refresh(1_700_000_005_000, true);
        let merged = stats.snapshot_with_cloudcode(
            "fixed",
            Some("https://cloudcode-pa.googleapis.com".to_string()),
        );
        assert_eq!(merged.last_catalog_refresh_at_ms, Some(1_700_000_005_000));
        assert_eq!(merged.last_catalog_included_proxy_models, Some(true));
        assert_eq!(merged.cloudcode_mode, "fixed");
        assert_eq!(
            merged.cloudcode_resolved_upstream.as_deref(),
            Some("https://cloudcode-pa.googleapis.com")
        );

        // Legacy 6-field JSON payload must deserialize cleanly via #[serde(default)].
        let legacy: StatsSnapshot = serde_json::from_str(
            r#"{
                "requests": 3,
                "prompt_tokens": 300,
                "cached_tokens": 150,
                "output_tokens": 45,
                "reasoning_tokens": 10,
                "cache_hit_ratio": 0.5
            }"#,
        )
        .expect("legacy 6-field snapshot must deserialize");
        assert_eq!(legacy.requests, 3);
        assert_eq!(legacy.last_catalog_refresh_at_ms, None);
        assert_eq!(legacy.last_catalog_included_proxy_models, None);
        assert_eq!(legacy.cloudcode_mode, "");
        assert_eq!(legacy.cloudcode_resolved_upstream, None);
    }

    #[test]
    fn zero_prompt_tokens_returns_zero_ratio_without_nan_or_inf() {
        let stats = ProxyStats::new();
        let initial = stats.snapshot();
        assert_eq!(initial.requests, 0);
        assert_eq!(initial.cache_hit_ratio, 0.0);
        assert!(initial.cache_hit_ratio.is_finite());

        stats.record(&GeminiUsageMetadata {
            prompt_token_count: 0,
            candidates_token_count: 10,
            cached_content_token_count: 5,
            thoughts_token_count: 2,
        });
        let after_zero_prompt = stats.snapshot();
        assert_eq!(after_zero_prompt.requests, 1);
        assert_eq!(after_zero_prompt.cache_hit_ratio, 0.0);
        assert!(after_zero_prompt.cache_hit_ratio.is_finite());

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
        assert_eq!(saturated.requests, 3);
        assert_eq!(saturated.prompt_tokens, u64::MAX);
        assert_eq!(saturated.cached_tokens, u64::MAX);
        assert_eq!(saturated.output_tokens, u64::MAX);
        assert_eq!(saturated.reasoning_tokens, u64::MAX);
        assert!(saturated.cache_hit_ratio.is_finite());
        assert!((saturated.cache_hit_ratio - 1.0).abs() < 1e-12);
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
                    assert_eq!(snap.prompt_tokens, snap.requests * 100);
                    assert_eq!(snap.cached_tokens, snap.requests * 50);
                    assert_eq!(snap.output_tokens, snap.requests * 20);
                    assert_eq!(snap.reasoning_tokens, snap.requests * 5);
                    if snap.requests > 0 {
                        assert!((snap.cache_hit_ratio - 0.5).abs() < 1e-12);
                    } else {
                        assert_eq!(snap.cache_hit_ratio, 0.0);
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
        assert_eq!(final_snap.requests, 1600);
        assert_eq!(final_snap.prompt_tokens, 160_000);
        assert_eq!(final_snap.cached_tokens, 80_000);
        assert_eq!(final_snap.output_tokens, 32_000);
        assert_eq!(final_snap.reasoning_tokens, 8_000);
        assert!((final_snap.cache_hit_ratio - 0.5).abs() < 1e-12);
    }
}
