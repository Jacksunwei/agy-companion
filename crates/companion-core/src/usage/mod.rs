#[cfg(feature = "sqlite")]
pub mod sqlite;

#[cfg(feature = "sqlite")]
pub use sqlite::SqliteUsageSink;

use chrono::{SecondsFormat, Utc};
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
        let usage = value.as_object()?.get("usageMetadata")?;
        if !usage.is_object() {
            return None;
        }
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
    pub fn from_gemini(
        model: impl Into<String>,
        usage: &GeminiUsageMetadata,
        latency_ms: u64,
    ) -> Self {
        Self {
            timestamp_utc: Utc::now().to_rfc3339_opts(SecondsFormat::Secs, true),
            request_id: Uuid::new_v4().to_string(),
            provider: "gemini".to_string(),
            model: model.into(),
            input_tokens: usage.prompt_token_count,
            output_tokens: usage.candidates_token_count,
            cached_tokens: usage.cached_content_token_count,
            reasoning_tokens: usage.thoughts_token_count,
            latency_ms,
            source: "proxy".to_string(),
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
        sink.record(event);
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
