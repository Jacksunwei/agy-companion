use std::fmt;

use serde::{Deserialize, Serialize};

use crate::config::{CloudCodeUpstream, DEFAULT_CLOUDCODE_UPSTREAM_URL};

pub const PROTOCOL_VERSION: u32 = 1;
pub const MAX_RECENT_CALLS: usize = 32;
pub const MAX_SESSION_MODELS: usize = 32;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HealthResponse {
    pub status: String,
    pub version: String,
    pub protocol_version: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub role: Option<String>,
}

impl HealthResponse {
    pub fn ok(version: impl Into<String>) -> Self {
        Self {
            status: "ok".to_string(),
            version: version.into(),
            protocol_version: PROTOCOL_VERSION,
            role: None,
        }
    }

    pub fn with_role(mut self, role: impl Into<String>) -> Self {
        self.role = Some(role.into());
        self
    }

    pub fn with_optional_role(mut self, role: Option<String>) -> Self {
        self.role = role;
        self
    }
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct UsageStatsSnapshot {
    pub requests: u64,
    pub prompt_tokens: u64,
    pub cached_tokens: u64,
    pub output_tokens: u64,
    pub reasoning_tokens: u64,
    pub cache_hit_ratio: f64,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct CloudCodeStatsSnapshot {
    #[serde(default)]
    pub mode: String,
    #[serde(default)]
    pub resolved_upstream: Option<String>,
    #[serde(default)]
    pub last_catalog_refresh_at_ms: Option<u64>,
    #[serde(default)]
    pub last_catalog_included_proxy_models: Option<bool>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RecentCallSnapshot {
    pub timestamp_ms: u64,
    pub provider: String,
    pub source: String,
    pub model: String,
    pub input_tokens: u64,
    pub cached_tokens: u64,
    pub output_tokens: u64,
    pub reasoning_tokens: u64,
    pub latency_ms: u64,
    #[serde(default)]
    pub cold_start: bool,
    #[serde(default)]
    pub cache_break: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ModelSessionStats {
    pub provider: String,
    pub source: String,
    pub model: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model_display_name: Option<String>,
    pub requests: u64,
    pub input_tokens: u64,
    pub cached_tokens: u64,
    pub output_tokens: u64,
    pub reasoning_tokens: u64,
    pub total_latency_ms: u64,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct StatsSnapshot {
    pub usage: UsageStatsSnapshot,
    #[serde(default)]
    pub cloudcode: CloudCodeStatsSnapshot,
    #[serde(default)]
    pub proxy_rss_bytes: Option<u64>,
    #[serde(default)]
    pub max_context_tokens: Option<u64>,
    #[serde(default)]
    pub recent_calls: Vec<RecentCallSnapshot>,
    #[serde(default)]
    pub by_model: Vec<ModelSessionStats>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProxyHealthSummary {
    pub reachable: bool,
    pub status: Option<String>,
    pub version: Option<String>,
    pub protocol_version: Option<u32>,
    pub error: Option<String>,
}

/// Strips control characters and caps token length to 64 chars,
/// returning `"<empty>"` if no printable characters remain.
pub fn sanitize_health_token(raw: &str) -> String {
    let cleaned: String = raw.chars().filter(|ch| !ch.is_control()).take(64).collect();
    if cleaned.is_empty() {
        "<empty>".to_string()
    } else {
        cleaned
    }
}

impl ProxyHealthSummary {
    pub fn unreachable(error: impl Into<String>) -> Self {
        Self {
            reachable: false,
            status: None,
            version: None,
            protocol_version: None,
            error: Some(error.into()),
        }
    }

    pub fn from_health(health: &HealthResponse) -> Self {
        let status_ok = health.status == "ok";
        Self {
            reachable: true,
            status: Some(sanitize_health_token(&health.status)),
            version: Some(sanitize_health_token(&health.version)),
            protocol_version: Some(health.protocol_version),
            error: if status_ok {
                None
            } else {
                Some("proxy reported non-ok health status".to_string())
            },
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CloudCodeStatus {
    pub mode: String,
    pub configured_upstream: String,
    pub resolved_upstream: Option<String>,
}

impl CloudCodeStatus {
    /// Constructs CloudCode routing status from disk config and optional live proxy stats.
    ///
    /// Invariant: `resolved_upstream` NEVER emits `Some("auto")` (case/whitespace-insensitive),
    /// even if stale proxy stats report `mode == "fixed"` while disk config is `Auto`.
    pub fn from_config_and_stats(
        configured: &CloudCodeUpstream,
        stats: Option<&CloudCodeStatsSnapshot>,
    ) -> Self {
        let mode = stats
            .map(|s| s.mode.trim())
            .filter(|m| !m.is_empty())
            .map(str::to_string)
            .unwrap_or_else(|| configured.mode_label().to_string());

        let resolved_upstream = stats
            .and_then(|s| s.resolved_upstream.as_deref())
            .map(str::trim)
            .filter(|u| !u.is_empty() && !u.eq_ignore_ascii_case(DEFAULT_CLOUDCODE_UPSTREAM_URL))
            .map(str::to_string)
            .or_else(|| {
                if mode == "fixed" {
                    configured
                        .fixed_url()
                        .map(str::trim)
                        .filter(|u| {
                            !u.is_empty() && !u.eq_ignore_ascii_case(DEFAULT_CLOUDCODE_UPSTREAM_URL)
                        })
                        .map(str::to_string)
                } else {
                    None
                }
            });

        Self {
            mode,
            configured_upstream: configured.as_str().to_string(),
            resolved_upstream,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum HostPlatform {
    Macos,
    Linux,
    Windows,
}

impl HostPlatform {
    pub const fn current() -> Self {
        #[cfg(target_os = "macos")]
        {
            Self::Macos
        }
        #[cfg(target_os = "windows")]
        {
            Self::Windows
        }
        #[cfg(not(any(target_os = "macos", target_os = "windows")))]
        {
            Self::Linux
        }
    }

    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Macos => "macos",
            Self::Linux => "linux",
            Self::Windows => "windows",
        }
    }
}

impl Default for HostPlatform {
    fn default() -> Self {
        Self::current()
    }
}

impl fmt::Display for HostPlatform {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn health_response_ok_and_json_roundtrip() {
        let health = HealthResponse::ok("0.1.0");
        assert_eq!(health.status, "ok");
        assert_eq!(health.version, "0.1.0");
        assert_eq!(health.protocol_version, PROTOCOL_VERSION);
        assert_eq!(health.role, None);

        let json = serde_json::to_string(&health).expect("serialize health");
        assert!(
            !json.contains("\"role\""),
            "role must be omitted from JSON when None: {json}"
        );
        let parsed: HealthResponse = serde_json::from_str(&json).expect("deserialize health");
        assert_eq!(parsed, health);

        // Legacy 3-field payload without `role` deserializes cleanly.
        let legacy: HealthResponse =
            serde_json::from_str(r#"{"status":"ok","version":"0.1.0","protocol_version":1}"#)
                .expect("deserialize legacy 3-field health");
        assert_eq!(legacy, health);

        // Additive `role` roundtrips cleanly.
        let with_role = HealthResponse::ok("0.1.0").with_role("bootstrap");
        let role_json = serde_json::to_string(&with_role).expect("serialize health with role");
        assert!(role_json.contains("\"role\":\"bootstrap\""));
        let role_parsed: HealthResponse =
            serde_json::from_str(&role_json).expect("deserialize health with role");
        assert_eq!(role_parsed, with_role);
    }

    #[test]
    fn stats_snapshot_exact_json_shape_and_partial_deserialization() {
        let snap = StatsSnapshot {
            usage: UsageStatsSnapshot {
                requests: 5,
                prompt_tokens: 1000,
                cached_tokens: 600,
                output_tokens: 250,
                reasoning_tokens: 80,
                cache_hit_ratio: 0.6,
            },
            cloudcode: CloudCodeStatsSnapshot {
                mode: "auto".to_string(),
                resolved_upstream: Some("https://daily-cloudcode-pa.googleapis.com".to_string()),
                last_catalog_refresh_at_ms: Some(1_700_000_000_000),
                last_catalog_included_proxy_models: Some(true),
            },
            proxy_rss_bytes: Some(24 * 1024 * 1024),
            max_context_tokens: Some(512_000),
            recent_calls: vec![RecentCallSnapshot {
                timestamp_ms: 1_700_000_000_123,
                provider: "gemini".to_string(),
                source: "proxy".to_string(),
                model: "gemini-4-carbon".to_string(),
                input_tokens: 1000,
                cached_tokens: 600,
                output_tokens: 250,
                reasoning_tokens: 80,
                latency_ms: 420,
                cold_start: false,
                cache_break: false,
            }],
            by_model: vec![ModelSessionStats {
                provider: "gemini".to_string(),
                source: "proxy".to_string(),
                model: "gemini-4-carbon".to_string(),
                model_display_name: None,
                requests: 5,
                input_tokens: 1000,
                cached_tokens: 600,
                output_tokens: 250,
                reasoning_tokens: 80,
                total_latency_ms: 2100,
            }],
        };

        let val = serde_json::to_value(&snap).expect("to_value");
        let root = val.as_object().expect("root object");
        assert_eq!(root.len(), 6);
        for k in [
            "usage",
            "cloudcode",
            "proxy_rss_bytes",
            "max_context_tokens",
            "recent_calls",
            "by_model",
        ] {
            assert!(root.contains_key(k), "missing root key {k}");
        }
        let usage = root["usage"].as_object().expect("usage object");
        assert_eq!(usage.len(), 6);
        for k in [
            "requests",
            "prompt_tokens",
            "cached_tokens",
            "output_tokens",
            "reasoning_tokens",
            "cache_hit_ratio",
        ] {
            assert!(usage.contains_key(k), "missing usage key {k}");
        }
        let cc = root["cloudcode"].as_object().expect("cloudcode object");
        assert_eq!(cc.len(), 4);
        for k in [
            "mode",
            "resolved_upstream",
            "last_catalog_refresh_at_ms",
            "last_catalog_included_proxy_models",
        ] {
            assert!(cc.contains_key(k), "missing cloudcode key {k}");
        }

        let roundtrip: StatsSnapshot =
            serde_json::from_value(val).expect("roundtrip full StatsSnapshot");
        assert_eq!(roundtrip, snap);

        // Partial {"usage": {...}} deserializes cleanly with default additive fields
        let partial: StatsSnapshot = serde_json::from_str(
            r#"{"usage":{"requests":1,"prompt_tokens":10,"cached_tokens":0,"output_tokens":5,"reasoning_tokens":0,"cache_hit_ratio":0.0}}"#,
        )
        .expect("partial stats");
        assert_eq!(partial.usage.requests, 1);
        assert_eq!(partial.cloudcode, CloudCodeStatsSnapshot::default());
        assert_eq!(partial.proxy_rss_bytes, None);
        assert_eq!(partial.max_context_tokens, None);
        assert!(partial.recent_calls.is_empty());
        assert!(partial.by_model.is_empty());

        // Empty object {} must fail deserialization
        assert!(serde_json::from_str::<StatsSnapshot>("{}").is_err());
    }

    #[test]
    fn stats_snapshot_worst_case_payload_stays_under_probe_budget() {
        let long_model = "m".repeat(128);
        let snap = StatsSnapshot {
            usage: UsageStatsSnapshot {
                requests: u64::MAX,
                prompt_tokens: u64::MAX,
                cached_tokens: u64::MAX,
                output_tokens: u64::MAX,
                reasoning_tokens: u64::MAX,
                cache_hit_ratio: 0.999_999_999_999_999_9,
            },
            cloudcode: CloudCodeStatsSnapshot {
                mode: "fixed".to_string(),
                resolved_upstream: Some(format!("https://{}.example.com", "a".repeat(200))),
                last_catalog_refresh_at_ms: Some(u64::MAX),
                last_catalog_included_proxy_models: Some(true),
            },
            proxy_rss_bytes: Some(u64::MAX),
            max_context_tokens: Some(u64::MAX),
            recent_calls: (0..MAX_RECENT_CALLS)
                .map(|_| RecentCallSnapshot {
                    timestamp_ms: u64::MAX,
                    provider: "antigravity".to_string(),
                    source: "builtin".to_string(),
                    model: long_model.clone(),
                    input_tokens: u64::MAX,
                    cached_tokens: u64::MAX,
                    output_tokens: u64::MAX,
                    reasoning_tokens: u64::MAX,
                    latency_ms: u64::MAX,
                    cold_start: true,
                    cache_break: true,
                })
                .collect(),
            by_model: (0..MAX_SESSION_MODELS)
                .map(|_| ModelSessionStats {
                    provider: "antigravity".to_string(),
                    source: "builtin".to_string(),
                    model: long_model.clone(),
                    model_display_name: None,
                    requests: u64::MAX,
                    input_tokens: u64::MAX,
                    cached_tokens: u64::MAX,
                    output_tokens: u64::MAX,
                    reasoning_tokens: u64::MAX,
                    total_latency_ms: u64::MAX,
                })
                .collect(),
        };

        let bytes = serde_json::to_vec(&snap).expect("serialize worst-case StatsSnapshot");
        assert!(
            bytes.len() < 48 * 1024,
            "worst-case StatsSnapshot JSON ({} bytes) must stay strictly below 48 KiB (< 64 KiB MAX_PROBE_BODY_BYTES)",
            bytes.len()
        );
    }

    #[test]
    fn proxy_health_summary_unreachable_ok_degraded_and_control_char_sanitization() {
        let unreach = ProxyHealthSummary::unreachable("connection refused");
        assert!(!unreach.reachable);
        assert_eq!(unreach.status, None);
        assert_eq!(unreach.error.as_deref(), Some("connection refused"));

        let ok = ProxyHealthSummary::from_health(&HealthResponse::ok("0.1.0"));
        assert!(ok.reachable);
        assert_eq!(ok.status.as_deref(), Some("ok"));
        assert_eq!(ok.version.as_deref(), Some("0.1.0"));
        assert_eq!(ok.protocol_version, Some(PROTOCOL_VERSION));
        assert_eq!(ok.error, None);

        let degraded = ProxyHealthSummary::from_health(&HealthResponse {
            status: "degraded".to_string(),
            version: "0.1.0".to_string(),
            protocol_version: PROTOCOL_VERSION,
            role: None,
        });
        assert!(degraded.reachable);
        assert_eq!(degraded.status.as_deref(), Some("degraded"));
        assert_eq!(
            degraded.error.as_deref(),
            Some("proxy reported non-ok health status")
        );

        // Control chars in status fail strict `status == "ok"` check AND are sanitized in output.
        let injected = ProxyHealthSummary::from_health(&HealthResponse {
            status: "ok\r\n".to_string(),
            version: "\x1b[31m0.1.0\n".to_string(),
            protocol_version: PROTOCOL_VERSION,
            role: None,
        });
        assert_eq!(injected.status.as_deref(), Some("ok"));
        assert_eq!(injected.version.as_deref(), Some("[31m0.1.0"));
        assert_eq!(
            injected.error.as_deref(),
            Some("proxy reported non-ok health status")
        );

        assert_eq!(sanitize_health_token(""), "<empty>");
        assert_eq!(sanitize_health_token("\r\n\t\0"), "<empty>");
        assert_eq!(sanitize_health_token(&"a".repeat(100)).len(), 64);
    }

    #[test]
    fn cloudcode_status_never_emits_auto_as_resolved_upstream() {
        let auto = CloudCodeUpstream::Auto;
        let fixed_url = "https://daily-cloudcode-pa.googleapis.com";
        let fixed = CloudCodeUpstream::Fixed(fixed_url.to_string());

        // Auto + None
        let s1 = CloudCodeStatus::from_config_and_stats(&auto, None);
        assert_eq!(s1.mode, "auto");
        assert_eq!(s1.configured_upstream, "auto");
        assert_eq!(s1.resolved_upstream, None);

        // Fixed + None
        let s2 = CloudCodeStatus::from_config_and_stats(&fixed, None);
        assert_eq!(s2.mode, "fixed");
        assert_eq!(s2.configured_upstream, fixed_url);
        assert_eq!(s2.resolved_upstream.as_deref(), Some(fixed_url));

        // Stale stats report mode == "fixed" with resolved_upstream == None while disk is Auto:
        // MUST NOT emit Some("auto")!
        let stale_fixed = CloudCodeStatsSnapshot {
            mode: "fixed".to_string(),
            resolved_upstream: None,
            ..Default::default()
        };
        let s3 = CloudCodeStatus::from_config_and_stats(&auto, Some(&stale_fixed));
        assert_eq!(s3.mode, "fixed");
        assert_eq!(s3.configured_upstream, "auto");
        assert_eq!(s3.resolved_upstream, None);

        // Stats reporting "auto" or " AUTO " as resolved_upstream are filtered out
        for bogus in ["auto", " AUTO ", "   "] {
            let bogus_stats = CloudCodeStatsSnapshot {
                mode: "auto".to_string(),
                resolved_upstream: Some(bogus.to_string()),
                ..Default::default()
            };
            let s = CloudCodeStatus::from_config_and_stats(&auto, Some(&bogus_stats));
            assert_eq!(s.resolved_upstream, None, "failed for bogus='{bogus}'");
        }

        // Auto + resolved live upstream
        let live_stats = CloudCodeStatsSnapshot {
            mode: "auto".to_string(),
            resolved_upstream: Some(fixed_url.to_string()),
            ..Default::default()
        };
        let s4 = CloudCodeStatus::from_config_and_stats(&auto, Some(&live_stats));
        assert_eq!(s4.mode, "auto");
        assert_eq!(s4.configured_upstream, "auto");
        assert_eq!(s4.resolved_upstream.as_deref(), Some(fixed_url));

        // Manually constructed CloudCodeUpstream::Fixed("auto") / Fixed("  AUTO  ") / Fixed("   ")
        // MUST still uphold the unconditional NEVER Some("auto") invariant.
        for bogus_fixed in ["auto", "  AUTO  ", "   "] {
            let manual_fixed = CloudCodeUpstream::Fixed(bogus_fixed.to_string());
            let s = CloudCodeStatus::from_config_and_stats(&manual_fixed, None);
            assert_eq!(s.mode, "fixed");
            assert_eq!(
                s.resolved_upstream, None,
                "expected None resolved_upstream for manually constructed Fixed('{bogus_fixed}')"
            );
        }
    }

    #[test]
    fn host_platform_serde_and_default() {
        for (variant, expected) in [
            (HostPlatform::Macos, "\"macos\""),
            (HostPlatform::Linux, "\"linux\""),
            (HostPlatform::Windows, "\"windows\""),
        ] {
            let json = serde_json::to_string(&variant).expect("serialize");
            assert_eq!(json, expected);
            let parsed: HostPlatform = serde_json::from_str(&json).expect("deserialize");
            assert_eq!(parsed, variant);
            assert_eq!(variant.to_string(), variant.as_str());
        }

        assert!(serde_json::from_str::<HostPlatform>("\"darwin\"").is_err());
        assert!(serde_json::from_str::<HostPlatform>("\"win32\"").is_err());
        assert_eq!(HostPlatform::default(), HostPlatform::current());
    }

    #[test]
    fn model_session_stats_optional_display_name_is_backwards_compatible() {
        let legacy_json = r#"{
            "provider": "antigravity",
            "source": "builtin",
            "model": "gemini-5-nova",
            "requests": 2,
            "input_tokens": 1000,
            "cached_tokens": 800,
            "output_tokens": 100,
            "reasoning_tokens": 0,
            "total_latency_ms": 250
        }"#;
        let parsed: ModelSessionStats =
            serde_json::from_str(legacy_json).expect("deserialize without model_display_name");
        assert_eq!(parsed.model_display_name, None);
        let serialized = serde_json::to_value(&parsed).expect("serialize");
        assert!(serialized.get("model_display_name").is_none());

        let with_display = ModelSessionStats {
            model_display_name: Some("Gemini 5 Nova (High)".to_string()),
            ..parsed
        };
        let ser_with = serde_json::to_value(&with_display).expect("serialize with display name");
        assert_eq!(
            ser_with.get("model_display_name").and_then(|v| v.as_str()),
            Some("Gemini 5 Nova (High)")
        );
    }
}
