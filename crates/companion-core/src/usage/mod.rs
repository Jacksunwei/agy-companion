#[cfg(feature = "sqlite")]
pub mod query;
#[cfg(feature = "sqlite")]
pub mod sqlite;

#[cfg(feature = "sqlite")]
pub use query::{query_usage_summary, query_usage_summary_at};
#[cfg(feature = "sqlite")]
pub use sqlite::SqliteUsageSink;

use chrono::{DateTime, Duration, SecondsFormat, Utc};
use serde::{Deserialize, Deserializer, Serialize};
use serde_json::Value;
use uuid::Uuid;

pub const USAGE_DB_FILENAME: &str = "usage.db";
pub const USAGE_DB_SCHEMA_VERSION: u32 = 1;

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

    pub fn parse_from_json(bytes: &[u8]) -> Option<GeminiUsageMetadata> {
        let value: Value = serde_json::from_slice(bytes).ok()?;
        match &value {
            Value::Object(_) => Self::extract_from_object(&value),
            Value::Array(items) => items.iter().rev().find_map(Self::extract_from_object),
            _ => None,
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
            request_id: Uuid::new_v4().to_string(),
            provider: provider.to_string(),
            model: model.into(),
            input_tokens: usage.prompt_token_count,
            output_tokens: usage.candidates_token_count,
            cached_tokens: usage.cached_content_token_count,
            reasoning_tokens: usage.thoughts_token_count,
            latency_ms,
            source: source.to_string(),
        }
    }

    pub fn from_gemini(
        model: impl Into<String>,
        usage: &GeminiUsageMetadata,
        latency_ms: u64,
    ) -> Self {
        Self::from_parts("gemini", "proxy", model, usage, latency_ms)
    }

    pub fn from_antigravity_builtin(
        model: impl Into<String>,
        usage: &GeminiUsageMetadata,
        latency_ms: u64,
    ) -> Self {
        Self::from_parts("antigravity", "builtin", model, usage, latency_ms)
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
        let event = UsageEvent::from_gemini("gemini-4-hydrogen", &usage, 123);
        assert_eq!(event.provider, "gemini");
        assert_eq!(event.source, "proxy");
        assert_eq!(event.input_tokens, 10);
        assert_eq!(event.output_tokens, 5);
        assert_eq!(event.cached_tokens, 2);
        assert_eq!(event.reasoning_tokens, 1);
        assert_eq!(event.latency_ms, 123);
        let builtin = UsageEvent::from_antigravity_builtin("gemini-3.8-pro", &usage, 456);
        assert_eq!(builtin.provider, "antigravity");
        assert_eq!(builtin.source, "builtin");
        assert_eq!(builtin.model, "gemini-3.8-pro");
        assert_eq!(builtin.input_tokens, 10);
        assert_eq!(builtin.output_tokens, 5);
        assert_eq!(builtin.cached_tokens, 2);
        assert_eq!(builtin.reasoning_tokens, 1);
        assert_eq!(builtin.latency_ms, 456);
        sink.record(event);
        sink.record(builtin);
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
