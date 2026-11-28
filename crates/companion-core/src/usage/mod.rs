#[cfg(feature = "sqlite")]
pub mod query;
#[cfg(feature = "sqlite")]
pub mod sqlite;

#[cfg(feature = "sqlite")]
pub use query::{query_usage_summary, query_usage_summary_at};
#[cfg(feature = "sqlite")]
pub use sqlite::SqliteUsageSink;

use std::fmt;

use chrono::{DateTime, Datelike, Duration, SecondsFormat, Utc};
use serde::{Deserialize, Deserializer, Serialize};
use serde_json::Value;
use uuid::Uuid;

pub const USAGE_DB_FILENAME: &str = "usage.db";
pub const USAGE_DB_SCHEMA_VERSION: u32 = 3;
pub const MAX_RESPONSE_ID_LEN: usize = 128;
pub const MIN_CONVERSATION_ID_LEN: usize = 6;
pub const MAX_CONVERSATION_ID_LEN: usize = 64;
pub const MAX_MODEL_DISPLAY_NAME_LEN: usize = 128;

/// Normalizes any valid RFC-3339 timestamp with a 4-digit UTC year (`0000..=9999`)
/// to strict `YYYY-MM-DDTHH:MM:SSZ` format required by `WINDOW_FILTER_SQL`.
pub fn normalize_utc_timestamp(raw: &str) -> Option<String> {
    let trimmed = raw.trim();
    let parsed = DateTime::parse_from_rfc3339(trimmed)
        .ok()?
        .with_timezone(&Utc);
    let year = parsed.year();
    if !(0..=9999).contains(&year) {
        return None;
    }
    Some(parsed.to_rfc3339_opts(SecondsFormat::Secs, true))
}

/// Sanitizes an upstream `responseId` / `response.responseId` before persistence:
/// trims ASCII whitespace, rejects empty values, values longer than `MAX_RESPONSE_ID_LEN`,
/// or any byte outside printable non-space ASCII (`0x21..=0x7E`).
pub fn sanitize_response_id(raw: &str) -> Option<String> {
    let trimmed = raw.trim();
    if trimmed.is_empty() || trimmed.len() > MAX_RESPONSE_ID_LEN {
        return None;
    }
    if !trimmed.bytes().all(|b| (0x21..=0x7E).contains(&b)) {
        return None;
    }
    Some(trimmed.to_string())
}

/// Sanitizes a conversation / cascade identifier to `[0-9a-zA-Z_-]{6,64}`.
pub fn sanitize_conversation_id(raw: &str) -> Option<String> {
    let trimmed = raw.trim();
    if !(MIN_CONVERSATION_ID_LEN..=MAX_CONVERSATION_ID_LEN).contains(&trimmed.len()) {
        return None;
    }
    if !trimmed
        .bytes()
        .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
    {
        return None;
    }
    Some(trimmed.to_string())
}

/// Sanitizes an optional human-readable model display name by stripping control
/// characters, trimming outer whitespace, and capping at `MAX_MODEL_DISPLAY_NAME_LEN` chars.
pub fn sanitize_model_display_name(raw: &str) -> Option<String> {
    let cleaned: String = raw
        .chars()
        .filter(|c| !c.is_control())
        .collect::<String>()
        .trim()
        .chars()
        .take(MAX_MODEL_DISPLAY_NAME_LEN)
        .collect();
    let final_trimmed = cleaned.trim();
    if final_trimmed.is_empty() {
        None
    } else {
        Some(final_trimmed.to_string())
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TelemetrySource {
    #[default]
    Proxy,
    Ls,
    Reconciled,
}

impl TelemetrySource {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Proxy => "proxy",
            Self::Ls => "ls",
            Self::Reconciled => "reconciled",
        }
    }

    pub fn parse(raw: &str) -> Option<Self> {
        match raw {
            "proxy" => Some(Self::Proxy),
            "ls" => Some(Self::Ls),
            "reconciled" => Some(Self::Reconciled),
            _ => None,
        }
    }
}

impl fmt::Display for TelemetrySource {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LsSyncWatermark {
    pub cascade_id: String,
    pub trajectory_id: String,
    pub step_offset: u32,
    pub last_modified_utc: String,
    pub updated_at_utc: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ObservedModel {
    pub provider: String,
    pub model: String,
    pub compaction_limit_tokens: Option<u64>,
    pub updated_at_utc: String,
}

fn deserialize_null_default<'de, D, T>(deserializer: D) -> Result<T, D::Error>
where
    T: Default + Deserialize<'de>,
    D: Deserializer<'de>,
{
    Ok(Option::<T>::deserialize(deserializer)?.unwrap_or_default())
}

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct GeminiUsageMetadata {
    #[serde(
        rename = "promptTokenCount",
        default,
        deserialize_with = "deserialize_null_default"
    )]
    pub prompt_token_count: u64,
    #[serde(
        rename = "candidatesTokenCount",
        default,
        deserialize_with = "deserialize_null_default"
    )]
    pub candidates_token_count: u64,
    #[serde(
        rename = "cachedContentTokenCount",
        default,
        deserialize_with = "deserialize_null_default"
    )]
    pub cached_content_token_count: u64,
    #[serde(
        rename = "thoughtsTokenCount",
        default,
        deserialize_with = "deserialize_null_default"
    )]
    pub thoughts_token_count: u64,
}

impl GeminiUsageMetadata {
    fn extract_from_object(value: &Value) -> Option<GeminiUsageMetadata> {
        let obj = value.as_object()?;
        let usage = obj
            .get("usageMetadata")
            .filter(|v| v.is_object())
            .or_else(|| {
                obj.get("response")
                    .and_then(Value::as_object)
                    .and_then(|resp| resp.get("usageMetadata"))
                    .filter(|v| v.is_object())
            })?;
        serde_json::from_value(usage.clone()).ok()
    }

    fn extract_response_id_from_object(value: &Value) -> Option<String> {
        let obj = value.as_object()?;
        obj.get("responseId")
            .and_then(Value::as_str)
            .and_then(sanitize_response_id)
            .or_else(|| {
                obj.get("response")
                    .and_then(Value::as_object)
                    .and_then(|resp| resp.get("responseId"))
                    .and_then(Value::as_str)
                    .and_then(sanitize_response_id)
            })
    }

    pub fn parse_from_json(bytes: &[u8]) -> Option<GeminiUsageMetadata> {
        Self::parse_usage_and_response_id_from_json(bytes).0
    }

    /// Parses `usageMetadata` and optional sanitized `responseId` (top-level or wrapped under
    /// `response.*`) from a JSON object or JSON array chunk without mutating the payload.
    pub fn parse_usage_and_response_id_from_json(
        bytes: &[u8],
    ) -> (Option<GeminiUsageMetadata>, Option<String>) {
        let Ok(value) = serde_json::from_slice::<Value>(bytes) else {
            return (None, None);
        };
        match &value {
            Value::Object(_) => (
                Self::extract_from_object(&value),
                Self::extract_response_id_from_object(&value),
            ),
            Value::Array(items) => (
                items.iter().rev().find_map(Self::extract_from_object),
                items
                    .iter()
                    .rev()
                    .find_map(Self::extract_response_id_from_object),
            ),
            _ => (None, None),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct UsageEvent {
    pub timestamp_utc: String,
    pub request_id: String,
    pub provider: String,
    pub model: String,
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub cached_tokens: u64,
    pub reasoning_tokens: u64,
    pub latency_ms: u64,
    pub source: String,
    #[serde(default)]
    pub response_id: Option<String>,
    #[serde(default)]
    pub conversation_id: Option<String>,
    #[serde(default)]
    pub root_conversation_id: Option<String>,
    #[serde(default)]
    pub parent_conversation_id: Option<String>,
    #[serde(default)]
    pub cache_write_tokens: u64,
    #[serde(default)]
    pub ttft_ms: Option<u64>,
    #[serde(default)]
    pub model_display_name: Option<String>,
    #[serde(default)]
    pub telemetry_source: TelemetrySource,
    #[serde(default)]
    pub cascade_step_index: Option<u32>,
}

impl UsageEvent {
    fn from_parts(
        provider: &str,
        source: &str,
        model: impl Into<String>,
        usage: &GeminiUsageMetadata,
        latency_ms: u64,
    ) -> Self {
        Self {
            timestamp_utc: Utc::now().to_rfc3339_opts(SecondsFormat::Secs, true),
            request_id: format!("proxy:{}", Uuid::new_v4()),
            provider: provider.to_string(),
            model: model.into(),
            input_tokens: usage.prompt_token_count,
            output_tokens: usage.candidates_token_count,
            cached_tokens: usage.cached_content_token_count,
            reasoning_tokens: usage.thoughts_token_count,
            latency_ms,
            source: source.to_string(),
            response_id: None,
            conversation_id: None,
            root_conversation_id: None,
            parent_conversation_id: None,
            cache_write_tokens: 0,
            ttft_ms: None,
            model_display_name: None,
            telemetry_source: TelemetrySource::Proxy,
            cascade_step_index: None,
        }
    }

    pub fn from_gemini(
        model: impl Into<String>,
        usage: &GeminiUsageMetadata,
        latency_ms: u64,
    ) -> Self {
        Self::from_parts("gemini", "proxy", model, usage, latency_ms)
    }

    pub fn from_codex(
        model: impl Into<String>,
        usage: &GeminiUsageMetadata,
        latency_ms: u64,
    ) -> Self {
        Self::from_parts("openai-codex", "proxy", model, usage, latency_ms)
    }

    pub fn from_antigravity_builtin(
        model: impl Into<String>,
        usage: &GeminiUsageMetadata,
        latency_ms: u64,
    ) -> Self {
        Self::from_parts("antigravity", "builtin", model, usage, latency_ms)
    }

    pub fn with_response_id(mut self, response_id: Option<String>) -> Self {
        self.response_id = response_id.as_deref().and_then(sanitize_response_id);
        self
    }

    pub fn with_conversation_id(mut self, conversation_id: Option<String>) -> Self {
        self.conversation_id = conversation_id
            .as_deref()
            .and_then(sanitize_conversation_id);
        self
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub enum UsageRange {
    #[serde(rename = "1d")]
    Today,
    #[default]
    #[serde(rename = "7d")]
    Days7,
    #[serde(rename = "30d")]
    Days30,
}

impl UsageRange {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Today => "1d",
            Self::Days7 => "7d",
            Self::Days30 => "30d",
        }
    }

    pub fn start_date_at(self, now: DateTime<Utc>) -> chrono::NaiveDate {
        let today = now.date_naive();
        match self {
            Self::Today => today,
            Self::Days7 => today - Duration::days(6),
            Self::Days30 => today - Duration::days(29),
        }
    }

    pub fn since_utc_at(self, now: DateTime<Utc>) -> String {
        self.start_date_at(now)
            .and_hms_opt(0, 0, 0)
            .expect("00:00:00 is always valid")
            .and_utc()
            .to_rfc3339_opts(SecondsFormat::Secs, true)
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct UsageBucket {
    pub label: String,
    pub requests: u64,
    pub input_tokens: u64,
    pub cached_tokens: u64,
    pub output_tokens: u64,
    pub reasoning_tokens: u64,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct UsageBreakdownRow {
    pub key: String,
    pub provider: String,
    pub source: Option<String>,
    pub model_count: Option<u64>,
    pub requests: u64,
    pub input_tokens: u64,
    pub cached_tokens: u64,
    pub output_tokens: u64,
    pub reasoning_tokens: u64,
    pub avg_latency_ms: u64,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct UsageSummary {
    pub range: UsageRange,
    pub since_utc: String,
    pub totals: UsageBucket,
    pub buckets: Vec<UsageBucket>,
    pub by_model: Vec<UsageBreakdownRow>,
    pub by_provider: Vec<UsageBreakdownRow>,
}

impl UsageSummary {
    pub fn empty(range: UsageRange) -> Self {
        Self::empty_at(range, Utc::now())
    }

    pub fn empty_at(range: UsageRange, now: DateTime<Utc>) -> Self {
        Self {
            range,
            since_utc: range.since_utc_at(now),
            totals: UsageBucket::default(),
            buckets: Vec::new(),
            by_model: Vec::new(),
            by_provider: Vec::new(),
        }
    }
}

pub trait UsageSink: Send + Sync {
    fn record(&self, event: UsageEvent);
}

#[derive(Debug, Default, Clone, Copy)]
pub struct NoopUsageSink;

impl UsageSink for NoopUsageSink {
    fn record(&self, _event: UsageEvent) {}
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use super::*;
    use crate::config::CoreError;

    #[test]
    fn parses_gemini_usage_metadata_with_cached_and_thoughts() {
        let payload = br#"{
            "candidates": [{"content": {"parts": [{"text": "hello"}]}}],
            "usageMetadata": {
                "promptTokenCount": 1200,
                "candidatesTokenCount": 350,
                "cachedContentTokenCount": 900,
                "thoughtsTokenCount": 120,
                "totalTokenCount": 1670,
                "promptTokensDetails": [{"modality": "TEXT", "tokenCount": 1200}],
                "serviceTier": "STANDARD"
            }
        }"#;

        let parsed = GeminiUsageMetadata::parse_from_json(payload).expect("parsed usageMetadata");
        assert_eq!(parsed.prompt_token_count, 1200);
        assert_eq!(parsed.candidates_token_count, 350);
        assert_eq!(parsed.cached_content_token_count, 900);
        assert_eq!(parsed.thoughts_token_count, 120);
    }

    #[test]
    fn parses_usage_from_array_handles_missing_and_null_fields() {
        let array_payload = br#"[
            {"candidates": [{"content": {"parts": [{"text": "part1"}]}}], "usageMetadata": {"promptTokenCount": 100, "candidatesTokenCount": 10}},
            {"candidates": [{"content": {"parts": [{"text": "part2"}]}}], "usageMetadata": {"promptTokenCount": 100, "candidatesTokenCount": 42, "cachedContentTokenCount": null, "thoughtsTokenCount": null}}
        ]"#;
        let parsed = GeminiUsageMetadata::parse_from_json(array_payload).expect("array usage");
        assert_eq!(parsed.prompt_token_count, 100);
        assert_eq!(parsed.candidates_token_count, 42);
        assert_eq!(parsed.cached_content_token_count, 0);
        assert_eq!(parsed.thoughts_token_count, 0);

        let partial = br#"{"usageMetadata": {"promptTokenCount": 50}}"#;
        let parsed_partial = GeminiUsageMetadata::parse_from_json(partial).expect("partial usage");
        assert_eq!(parsed_partial.prompt_token_count, 50);
        assert_eq!(parsed_partial.candidates_token_count, 0);

        assert_eq!(
            GeminiUsageMetadata::parse_from_json(br#"{"error": {"code": 400, "message": "bad"}}"#),
            None
        );
        assert_eq!(
            GeminiUsageMetadata::parse_from_json(br#"{"candidates": []}"#),
            None
        );
        assert_eq!(
            GeminiUsageMetadata::parse_from_json(br#"{"usageMetadata": null}"#),
            None
        );
        assert_eq!(GeminiUsageMetadata::parse_from_json(b"not-json"), None);
    }

    #[test]
    fn noop_sink_accepts_events() {
        let sink: Box<dyn UsageSink> = Box::new(NoopUsageSink);
        let usage = GeminiUsageMetadata {
            prompt_token_count: 10,
            candidates_token_count: 5,
            cached_content_token_count: 2,
            thoughts_token_count: 1,
        };
        let event = UsageEvent::from_gemini("gemini-4-hydrogen", &usage, 123)
            .with_response_id(Some("  resp-abc_123  ".to_string()))
            .with_conversation_id(Some("conv_123456".to_string()));
        assert!(event.request_id.starts_with("proxy:"));
        assert_eq!(event.provider, "gemini");
        assert_eq!(event.source, "proxy");
        assert_eq!(event.telemetry_source, TelemetrySource::Proxy);
        assert_eq!(event.response_id.as_deref(), Some("resp-abc_123"));
        assert_eq!(event.conversation_id.as_deref(), Some("conv_123456"));
        assert_eq!(event.cache_write_tokens, 0);
        assert_eq!(event.ttft_ms, None);
        assert_eq!(event.cascade_step_index, None);
        assert_eq!(event.input_tokens, 10);
        assert_eq!(event.output_tokens, 5);
        assert_eq!(event.cached_tokens, 2);
        assert_eq!(event.reasoning_tokens, 1);
        assert_eq!(event.latency_ms, 123);
        let builtin = UsageEvent::from_antigravity_builtin("gemini-3.8-pro", &usage, 456);
        assert!(builtin.request_id.starts_with("proxy:"));
        assert_eq!(builtin.provider, "antigravity");
        assert_eq!(builtin.source, "builtin");
        assert_eq!(builtin.telemetry_source, TelemetrySource::Proxy);
        assert_eq!(builtin.model, "gemini-3.8-pro");
        assert_eq!(builtin.input_tokens, 10);
        assert_eq!(builtin.output_tokens, 5);
        assert_eq!(builtin.cached_tokens, 2);
        assert_eq!(builtin.reasoning_tokens, 1);
        assert_eq!(builtin.latency_ms, 456);
        let codex = UsageEvent::from_codex("gpt-6.2-sol", &usage, 789);
        assert_eq!(codex.provider, "openai-codex");
        assert_eq!(codex.source, "proxy");
        assert_eq!(codex.model, "gpt-6.2-sol");
        assert_eq!(codex.input_tokens, 10);
        assert_eq!(codex.output_tokens, 5);
        assert_eq!(codex.cached_tokens, 2);
        assert_eq!(codex.reasoning_tokens, 1);
        assert_eq!(codex.latency_ms, 789);
        sink.record(event);
        sink.record(codex);
        sink.record(builtin);
    }

    #[test]
    fn sanitization_helpers_and_response_id_extraction_enforce_bounds() {
        assert_eq!(
            normalize_utc_timestamp("2026-05-15T10:30:45.987654-07:00"),
            Some("2026-05-15T17:30:45Z".to_string())
        );
        assert_eq!(
            normalize_utc_timestamp(" 2026-05-15T00:00:00+00:00 "),
            Some("2026-05-15T00:00:00Z".to_string())
        );
        assert_eq!(normalize_utc_timestamp("0000-01-01T00:00:00+05:00"), None);
        assert_eq!(normalize_utc_timestamp("not-a-timestamp"), None);

        assert_eq!(
            sanitize_response_id("  G4EIa_test-123  "),
            Some("G4EIa_test-123".to_string())
        );
        assert_eq!(sanitize_response_id("   "), None);
        assert_eq!(sanitize_response_id("has space"), None);
        assert_eq!(sanitize_response_id("bad\nctrl"), None);
        assert_eq!(sanitize_response_id("café"), None);
        assert_eq!(
            sanitize_response_id(&"a".repeat(MAX_RESPONSE_ID_LEN)),
            Some("a".repeat(MAX_RESPONSE_ID_LEN))
        );
        assert_eq!(
            sanitize_response_id(&"a".repeat(MAX_RESPONSE_ID_LEN + 1)),
            None
        );

        assert_eq!(
            sanitize_conversation_id("a1262862-1234_abcd"),
            Some("a1262862-1234_abcd".to_string())
        );
        assert_eq!(sanitize_conversation_id("short"), None);
        assert_eq!(sanitize_conversation_id("../etc/passwd"), None);
        assert_eq!(
            sanitize_conversation_id(&"c".repeat(MAX_CONVERSATION_ID_LEN + 1)),
            None
        );

        assert_eq!(
            sanitize_model_display_name("  Gemini\x00 3.8\t Pro \n "),
            Some("Gemini 3.8 Pro".to_string())
        );
        assert_eq!(sanitize_model_display_name("\x00\x07  "), None);
        let long_name = "M".repeat(MAX_MODEL_DISPLAY_NAME_LEN + 20);
        assert_eq!(
            sanitize_model_display_name(&long_name).map(|s| s.len()),
            Some(MAX_MODEL_DISPLAY_NAME_LEN)
        );

        for (variant, wire) in [
            (TelemetrySource::Proxy, "proxy"),
            (TelemetrySource::Ls, "ls"),
            (TelemetrySource::Reconciled, "reconciled"),
        ] {
            assert_eq!(variant.as_str(), wire);
            assert_eq!(variant.to_string(), wire);
            assert_eq!(TelemetrySource::parse(wire), Some(variant));
            let json = serde_json::to_string(&variant).expect("serialize");
            assert_eq!(json, format!("\"{wire}\""));
            assert_eq!(
                serde_json::from_str::<TelemetrySource>(&json).expect("deserialize"),
                variant
            );
        }
        assert_eq!(TelemetrySource::parse("unknown"), None);

        // Top-level responseId + usageMetadata
        let (usage, rid) = GeminiUsageMetadata::parse_usage_and_response_id_from_json(
            br#"{"responseId":"G4EIa123","usageMetadata":{"promptTokenCount":12,"candidatesTokenCount":3}}"#,
        );
        assert_eq!(usage.map(|u| u.prompt_token_count), Some(12));
        assert_eq!(rid.as_deref(), Some("G4EIa123"));

        // Wrapped CloudCode response.responseId without usageMetadata
        let (usage2, rid2) = GeminiUsageMetadata::parse_usage_and_response_id_from_json(
            br#"{"response":{"responseId":"AJ8Ia456","candidates":[]}}"#,
        );
        assert_eq!(usage2, None);
        assert_eq!(rid2.as_deref(), Some("AJ8Ia456"));

        // Array payload picks last valid responseId and usageMetadata
        let (usage3, rid3) = GeminiUsageMetadata::parse_usage_and_response_id_from_json(
            br#"[{"responseId":"first_id"},{"response":{"responseId":"  ","usageMetadata":{"promptTokenCount":9}}}]"#,
        );
        assert_eq!(usage3.map(|u| u.prompt_token_count), Some(9));
        assert_eq!(rid3.as_deref(), Some("first_id"));
    }

    #[test]
    fn parses_wrapped_cloudcode_usage_metadata_envelopes() {
        let wrapped = br#"{
            "response": {
                "candidates": [{"content": {"parts": [{"text": "ok"}]}}],
                "usageMetadata": {
                    "promptTokenCount": 2000,
                    "candidatesTokenCount": 400,
                    "cachedContentTokenCount": 1500,
                    "thoughtsTokenCount": 90
                }
            }
        }"#;
        let parsed = GeminiUsageMetadata::parse_from_json(wrapped).expect("wrapped usageMetadata");
        assert_eq!(parsed.prompt_token_count, 2000);
        assert_eq!(parsed.candidates_token_count, 400);
        assert_eq!(parsed.cached_content_token_count, 1500);
        assert_eq!(parsed.thoughts_token_count, 90);

        let wrapped_array = br#"[
            {"response": {"candidates": []}},
            {"response": {"usageMetadata": {"promptTokenCount": 300, "candidatesTokenCount": 50}}}
        ]"#;
        let parsed_arr =
            GeminiUsageMetadata::parse_from_json(wrapped_array).expect("wrapped array usage");
        assert_eq!(parsed_arr.prompt_token_count, 300);
        assert_eq!(parsed_arr.candidates_token_count, 50);

        // Top-level null falls through to wrapped response.usageMetadata
        let fallthrough = br#"{
            "usageMetadata": null,
            "response": {"usageMetadata": {"promptTokenCount": 77, "candidatesTokenCount": 11}}
        }"#;
        let parsed_ft =
            GeminiUsageMetadata::parse_from_json(fallthrough).expect("fallthrough usage");
        assert_eq!(parsed_ft.prompt_token_count, 77);
        assert_eq!(parsed_ft.candidates_token_count, 11);

        // Wrapped null returns None
        assert_eq!(
            GeminiUsageMetadata::parse_from_json(br#"{"response": {"usageMetadata": null}}"#),
            None
        );
    }

    #[test]
    fn usage_range_serde_and_since_utc_math() {
        for (variant, expected) in [
            (UsageRange::Today, "\"1d\""),
            (UsageRange::Days7, "\"7d\""),
            (UsageRange::Days30, "\"30d\""),
        ] {
            let json = serde_json::to_string(&variant).expect("serialize range");
            assert_eq!(json, expected);
            let parsed: UsageRange = serde_json::from_str(&json).expect("deserialize range");
            assert_eq!(parsed, variant);
        }
        assert!(serde_json::from_str::<UsageRange>("\"90d\"").is_err());

        let now = DateTime::parse_from_rfc3339("2026-05-15T14:32:10Z")
            .expect("rfc3339")
            .with_timezone(&Utc);
        assert_eq!(UsageRange::Today.since_utc_at(now), "2026-05-15T00:00:00Z");
        assert_eq!(UsageRange::Days7.since_utc_at(now), "2026-05-09T00:00:00Z");
        assert_eq!(UsageRange::Days30.since_utc_at(now), "2026-04-16T00:00:00Z");

        let empty = UsageSummary::empty_at(UsageRange::Days7, now);
        assert_eq!(empty.range, UsageRange::Days7);
        assert_eq!(empty.since_utc, "2026-05-09T00:00:00Z");
        assert_eq!(empty.totals, UsageBucket::default());
        assert!(empty.buckets.is_empty());
        assert!(empty.by_model.is_empty());
        assert!(empty.by_provider.is_empty());
    }

    #[test]
    fn usage_database_error_display_is_unconditional() {
        let db_err = CoreError::UsageDatabase {
            path: PathBuf::from("/tmp/usage.db"),
            reason: "boom".to_string(),
        };
        assert_eq!(
            db_err.to_string(),
            "usage database error at '/tmp/usage.db': boom"
        );
    }
}
