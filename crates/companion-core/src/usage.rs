use std::fs;
use std::io::{ErrorKind, Write};
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use chrono::{DateTime, Datelike, SecondsFormat, Utc};
use serde::{Deserialize, Deserializer, Serialize};
use serde_json::Value;
use uuid::Uuid;

use crate::config::{CoreError, validate_path};

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

#[derive(Debug)]
pub struct JsonlUsageSink {
    usage_dir: PathBuf,
    write_lock: Mutex<()>,
}

impl JsonlUsageSink {
    pub fn new(usage_dir: impl Into<PathBuf>) -> Result<Self, CoreError> {
        let usage_dir = usage_dir.into();
        validate_path(&usage_dir)?;
        let check_dir: PathBuf = usage_dir.components().collect();
        match fs::symlink_metadata(&check_dir) {
            Ok(meta) if meta.file_type().is_symlink() => {
                return Err(CoreError::InvalidPath {
                    reason: "symbolic links are not allowed for usage directories".to_string(),
                });
            }
            Ok(_) => {}
            Err(err) if err.kind() == ErrorKind::NotFound => {}
            Err(source) => {
                return Err(CoreError::Io {
                    path: usage_dir,
                    source,
                });
            }
        }
        Ok(Self {
            usage_dir,
            write_lock: Mutex::new(()),
        })
    }

    pub fn usage_dir(&self) -> &Path {
        &self.usage_dir
    }

    fn month_bucket(timestamp_utc: &str) -> String {
        DateTime::parse_from_rfc3339(timestamp_utc)
            .map(|dt| dt.with_timezone(&Utc))
            .ok()
            .filter(|dt| (1970..=9999).contains(&dt.year()))
            .unwrap_or_else(Utc::now)
            .format("%Y-%m")
            .to_string()
    }

    pub fn record_checked(&self, event: &UsageEvent) -> Result<PathBuf, CoreError> {
        let month = Self::month_bucket(&event.timestamp_utc);
        let file_path = self.usage_dir.join(format!("{month}.jsonl"));

        let mut line = serde_json::to_string(event).map_err(|err| CoreError::SerializeUsage {
            reason: err.to_string(),
        })?;
        line.push('\n');

        let _guard = self
            .write_lock
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());

        let check_usage_dir: PathBuf = self.usage_dir.components().collect();
        match fs::symlink_metadata(&check_usage_dir) {
            Ok(meta) => {
                if meta.file_type().is_symlink() {
                    return Err(CoreError::InvalidPath {
                        reason: "symbolic links are not allowed for usage directories".to_string(),
                    });
                }
                if !meta.file_type().is_dir() {
                    return Err(CoreError::InvalidPath {
                        reason: "usage directory path must be a directory".to_string(),
                    });
                }
            }
            Err(err) if err.kind() == ErrorKind::NotFound => {
                let mut dir_builder = fs::DirBuilder::new();
                dir_builder.recursive(true);
                #[cfg(unix)]
                {
                    use std::os::unix::fs::DirBuilderExt;
                    dir_builder.mode(0o700);
                }
                dir_builder
                    .create(&self.usage_dir)
                    .map_err(|source| CoreError::Io {
                        path: self.usage_dir.clone(),
                        source,
                    })?;

                let created_meta =
                    fs::symlink_metadata(&check_usage_dir).map_err(|source| CoreError::Io {
                        path: self.usage_dir.clone(),
                        source,
                    })?;
                if created_meta.file_type().is_symlink() || !created_meta.file_type().is_dir() {
                    return Err(CoreError::InvalidPath {
                        reason: "usage directory path must be a non-symlink directory".to_string(),
                    });
                }
            }
            Err(source) => {
                return Err(CoreError::Io {
                    path: self.usage_dir.clone(),
                    source,
                });
            }
        }

        let check_file_path: PathBuf = file_path.components().collect();
        let pre_open_meta = match fs::symlink_metadata(&check_file_path) {
            Ok(meta) => {
                if meta.file_type().is_symlink() {
                    return Err(CoreError::InvalidPath {
                        reason: "symbolic links are not allowed for usage files".to_string(),
                    });
                }
                if !meta.file_type().is_file() {
                    return Err(CoreError::InvalidPath {
                        reason: "usage file path must be a regular file".to_string(),
                    });
                }
                Some(meta)
            }
            Err(err) if err.kind() == ErrorKind::NotFound => None,
            Err(source) => {
                return Err(CoreError::Io {
                    path: file_path.clone(),
                    source,
                });
            }
        };

        let mut open_opts = fs::OpenOptions::new();
        open_opts.create(true).append(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            open_opts.mode(0o600);
        }

        let mut file = open_opts.open(&file_path).map_err(|source| CoreError::Io {
            path: file_path.clone(),
            source,
        })?;

        let open_meta = file.metadata().map_err(|source| CoreError::Io {
            path: file_path.clone(),
            source,
        })?;
        if !open_meta.file_type().is_file() {
            return Err(CoreError::InvalidPath {
                reason: "usage file path must be a regular file".to_string(),
            });
        }

        #[cfg(unix)]
        {
            use std::os::unix::fs::MetadataExt;
            if let Some(lstat_meta) = pre_open_meta {
                if open_meta.dev() != lstat_meta.dev() || open_meta.ino() != lstat_meta.ino() {
                    return Err(CoreError::InvalidPath {
                        reason: "usage file changed concurrently during open".to_string(),
                    });
                }
            } else {
                let post_lstat =
                    fs::symlink_metadata(&check_file_path).map_err(|source| CoreError::Io {
                        path: file_path.clone(),
                        source,
                    })?;
                if post_lstat.file_type().is_symlink()
                    || !post_lstat.file_type().is_file()
                    || post_lstat.dev() != open_meta.dev()
                    || post_lstat.ino() != open_meta.ino()
                {
                    return Err(CoreError::InvalidPath {
                        reason: "usage file changed concurrently during open".to_string(),
                    });
                }
            }
        }
        #[cfg(not(unix))]
        let _ = pre_open_meta;

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            file.set_permissions(fs::Permissions::from_mode(0o600))
                .map_err(|source| CoreError::Io {
                    path: file_path.clone(),
                    source,
                })?;
        }

        file.write_all(line.as_bytes())
            .map_err(|source| CoreError::Io {
                path: file_path.clone(),
                source,
            })?;
        file.flush().map_err(|source| CoreError::Io {
            path: file_path.clone(),
            source,
        })?;

        Ok(file_path)
    }
}

impl UsageSink for JsonlUsageSink {
    fn record(&self, event: UsageEvent) {
        if let Err(err) = self.record_checked(&event) {
            tracing::warn!(error = %err, "failed to append usage event to JSONL sink");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::sync::Arc;
    use tempfile::TempDir;

    fn sample_event(timestamp_utc: &str, request_id: &str) -> UsageEvent {
        UsageEvent {
            timestamp_utc: timestamp_utc.to_string(),
            request_id: request_id.to_string(),
            provider: "gemini".to_string(),
            model: "gemini-2.5-pro".to_string(),
            input_tokens: 1200,
            output_tokens: 350,
            cached_tokens: 900,
            reasoning_tokens: 120,
            latency_ms: 842,
            source: "proxy".to_string(),
        }
    }

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
    fn jsonl_sink_appends_without_body_content() {
        let tmp = TempDir::new().expect("temp dir");
        let sink = JsonlUsageSink::new(tmp.path()).expect("sink init");
        assert_eq!(sink.usage_dir(), tmp.path());

        let e1 = sample_event("2026-11-22T22:00:00Z", "req-1");
        let e2 = sample_event("2026-11-22T22:05:00Z", "req-2");
        sink.record(e1.clone());
        sink.record(e2.clone());

        let file_path = tmp.path().join("2026-11.jsonl");
        let contents = fs::read_to_string(&file_path).expect("read jsonl");
        let lines: Vec<&str> = contents.lines().collect();
        assert_eq!(lines.len(), 2);

        let parsed_1: UsageEvent = serde_json::from_str(lines[0]).expect("parse line 1");
        let parsed_2: UsageEvent = serde_json::from_str(lines[1]).expect("parse line 2");
        assert_eq!(parsed_1, e1);
        assert_eq!(parsed_2, e2);

        for line in &lines {
            let obj: serde_json::Map<String, serde_json::Value> =
                serde_json::from_str(line).expect("json object");
            assert_eq!(obj.len(), 10);
            assert!(!obj.contains_key("prompt"));
            assert!(!obj.contains_key("response"));
            assert!(!obj.contains_key("body"));
            assert!(!obj.contains_key("api_key"));
        }
        assert!(!contents.contains("AIza"));
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
    fn jsonl_sink_enforces_0600_and_concurrent_thread_safe_appends() {
        let tmp = TempDir::new().expect("temp dir");
        let usage_subdir = tmp.path().join("nested").join("usage");
        let sink = Arc::new(JsonlUsageSink::new(&usage_subdir).expect("sink init"));
        let dyn_sink: Arc<dyn UsageSink> = sink.clone();

        let mut handles = Vec::new();
        for t in 0..8 {
            let s = dyn_sink.clone();
            handles.push(std::thread::spawn(move || {
                for i in 0..10 {
                    let event = sample_event("2025-11-22T22:10:00Z", &format!("t{t}-i{i}"));
                    s.record(event);
                }
            }));
        }
        for handle in handles {
            handle.join().expect("thread join");
        }

        dyn_sink.record(sample_event("2025-12-01T00:00:00Z", "dec-1"));

        let nov_path = usage_subdir.join("2025-11.jsonl");
        let dec_path = usage_subdir.join("2025-12.jsonl");
        assert!(nov_path.exists());
        assert!(dec_path.exists());

        let nov_lines: Vec<UsageEvent> = fs::read_to_string(&nov_path)
            .expect("read nov")
            .lines()
            .map(|l| serde_json::from_str(l).expect("valid jsonl line"))
            .collect();
        assert_eq!(nov_lines.len(), 80);

        let dec_lines: Vec<UsageEvent> = fs::read_to_string(&dec_path)
            .expect("read dec")
            .lines()
            .map(|l| serde_json::from_str(l).expect("valid jsonl line"))
            .collect();
        assert_eq!(dec_lines.len(), 1);
        assert_eq!(dec_lines[0].request_id, "dec-1");

        // Malformed/traversal timestamp must fall back safely to UTC current month inside usage_dir
        let fallback_dir = tmp.path().join("fallback");
        let fallback_sink = JsonlUsageSink::new(&fallback_dir).expect("fallback sink init");
        let fallback_path = fallback_sink
            .record_checked(&sample_event("../../etc/passwd", "bad-ts"))
            .expect("invalid timestamp falls back to current UTC month");
        assert_eq!(fallback_path.parent(), Some(fallback_dir.as_path()));

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;

            let dir_mode = fs::metadata(&usage_subdir)
                .expect("dir meta")
                .permissions()
                .mode()
                & 0o777;
            assert_eq!(dir_mode, 0o700);
            let nov_mode = fs::metadata(&nov_path).expect("meta").permissions().mode() & 0o777;
            assert_eq!(nov_mode, 0o600);
            let dec_mode = fs::metadata(&dec_path).expect("meta").permissions().mode() & 0o777;
            assert_eq!(dec_mode, 0o600);
        }
    }

    #[test]
    fn jsonl_sink_handles_io_failure_without_panicking() {
        assert!(matches!(
            JsonlUsageSink::new("../escape"),
            Err(CoreError::InvalidPath { .. })
        ));

        let tmp = TempDir::new().expect("temp dir");
        let blocker_file = tmp.path().join("not-a-dir");
        fs::write(&blocker_file, "occupied").expect("write blocker");

        let sink = JsonlUsageSink::new(&blocker_file).expect("valid path");
        let event = sample_event("2026-11-22T22:15:00Z", "io-err");
        assert!(sink.record_checked(&event).is_err());

        // UsageSink::record must never panic on I/O errors
        sink.record(event);
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
        let event = UsageEvent::from_gemini("gemini-2.5-pro", &usage, 123);
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
    fn jsonl_sink_clamps_extreme_year_boundary_timestamps_to_four_digit_month_files() {
        let tmp = TempDir::new().expect("temp dir");
        let sink = JsonlUsageSink::new(tmp.path()).expect("sink init");

        for (ts, req_id) in [
            ("0000-01-01T00:00:00+05:00", "year-underflow"),
            ("9999-12-31T23:59:59-05:00", "year-overflow"),
            ("1969-12-31T23:59:59Z", "pre-epoch"),
        ] {
            let path = sink
                .record_checked(&sample_event(ts, req_id))
                .expect("fallback write succeeds");
            assert_eq!(path.parent(), Some(tmp.path()));

            let file_name = path
                .file_name()
                .and_then(|n| n.to_str())
                .expect("utf-8 filename");
            assert_eq!(
                file_name.len(),
                13,
                "expected YYYY-MM.jsonl filename, got '{file_name}'"
            );
            assert!(
                file_name.ends_with(".jsonl"),
                "expected .jsonl suffix, got '{file_name}'"
            );
            let bytes = file_name.as_bytes();
            assert!(
                bytes[0..4].iter().all(u8::is_ascii_digit)
                    && bytes[4] == b'-'
                    && bytes[5..7].iter().all(u8::is_ascii_digit),
                "filename must match [0-9]{{4}}-[0-9]{{2}}.jsonl, got '{file_name}'"
            );
        }

        // Valid 4-digit UTC boundary years 1970..=9999 remain intact.
        let epoch_path = sink
            .record_checked(&sample_event("1970-01-01T00:00:00Z", "epoch-start"))
            .expect("1970 write succeeds");
        assert_eq!(
            epoch_path.file_name().and_then(|n| n.to_str()),
            Some("1970-01.jsonl")
        );

        let max_year_path = sink
            .record_checked(&sample_event("9999-12-31T23:59:59Z", "year-9999"))
            .expect("9999 write succeeds");
        assert_eq!(
            max_year_path.file_name().and_then(|n| n.to_str()),
            Some("9999-12.jsonl")
        );

        let serialize_err = CoreError::SerializeUsage {
            reason: "boom".to_string(),
        };
        assert_eq!(
            serialize_err.to_string(),
            "failed to serialize usage event: boom"
        );
    }

    #[cfg(unix)]
    #[test]
    fn jsonl_sink_rejects_symlink_jsonl_file_without_modifying_target_or_permissions() {
        use std::os::unix::fs::{PermissionsExt, symlink};

        let tmp = TempDir::new().expect("temp dir");
        let usage_dir = tmp.path().join("usage");
        fs::create_dir(&usage_dir).expect("mkdir usage");

        let victim = tmp.path().join("victim.txt");
        fs::write(&victim, "DO_NOT_TOUCH\n").expect("write victim");
        fs::set_permissions(&victim, fs::Permissions::from_mode(0o644)).expect("chmod 0644");

        let link_path = usage_dir.join("2026-11.jsonl");
        symlink(&victim, &link_path).expect("symlink jsonl");

        let sink = JsonlUsageSink::new(&usage_dir).expect("sink init");
        let err = sink
            .record_checked(&sample_event("2026-11-23T08:00:00Z", "sym-jsonl"))
            .expect_err("symlinked JSONL file must fail");
        assert!(
            matches!(err, CoreError::InvalidPath { .. }),
            "expected InvalidPath for symlinked JSONL, got {err:?}"
        );

        assert_eq!(
            fs::read_to_string(&victim).expect("read victim"),
            "DO_NOT_TOUCH\n",
            "victim file contents must remain untouched"
        );
        let mode = fs::metadata(&victim)
            .expect("victim meta")
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(mode, 0o644, "victim file permissions must remain 0644");
    }

    #[cfg(unix)]
    #[test]
    fn jsonl_sink_rejects_symlink_usage_dir_and_fifo_without_blocking() {
        use std::os::unix::fs::symlink;

        let tmp = TempDir::new().expect("temp dir");

        // 1. Symlinked usage_dir must be rejected without writing into target dir.
        let target_dir = tmp.path().join("target_dir");
        fs::create_dir(&target_dir).expect("mkdir target_dir");
        let sym_usage_dir = tmp.path().join("sym_usage");
        symlink(&target_dir, &sym_usage_dir).expect("symlink usage_dir");

        // Both construction and post-construction swap must reject symlinks.
        match JsonlUsageSink::new(&sym_usage_dir) {
            Err(CoreError::InvalidPath { .. }) => {}
            Ok(sink) => {
                let err = sink
                    .record_checked(&sample_event("2026-11-23T08:00:00Z", "sym-dir"))
                    .expect_err("symlinked usage_dir must fail on record");
                assert!(
                    matches!(err, CoreError::InvalidPath { .. }),
                    "expected InvalidPath, got {err:?}"
                );
            }
            Err(other) => panic!("unexpected error for symlinked usage_dir: {other:?}"),
        }

        // Also test post-construction swap of usage_dir to a symlink.
        let swapped_dir = tmp.path().join("swapped_usage");
        let swapped_sink = JsonlUsageSink::new(&swapped_dir).expect("missing usage_dir allowed");
        symlink(&target_dir, &swapped_dir).expect("swap usage_dir to symlink");
        let swap_err = swapped_sink
            .record_checked(&sample_event("2026-11-23T08:00:00Z", "swapped-dir"))
            .expect_err("post-construction symlinked usage_dir must fail");
        assert!(
            matches!(swap_err, CoreError::InvalidPath { .. }),
            "expected InvalidPath, got {swap_err:?}"
        );
        assert!(
            fs::read_dir(&target_dir)
                .expect("read target_dir")
                .next()
                .is_none(),
            "target_dir must remain completely empty"
        );

        // 2. FIFO at YYYY-MM.jsonl must fail fast (< 1s) without blocking.
        let fifo_usage_dir = tmp.path().join("fifo_usage");
        fs::create_dir(&fifo_usage_dir).expect("mkdir fifo_usage");
        let fifo_jsonl = fifo_usage_dir.join("2026-11.jsonl");
        let status = std::process::Command::new("mkfifo")
            .arg(&fifo_jsonl)
            .status()
            .expect("run mkfifo");
        assert!(status.success(), "mkfifo failed");

        let fifo_sink = JsonlUsageSink::new(&fifo_usage_dir).expect("sink init");
        let (tx, rx) = std::sync::mpsc::channel();
        let handle = std::thread::spawn(move || {
            let res = fifo_sink.record_checked(&sample_event("2026-11-23T08:00:00Z", "fifo-req"));
            let _ = tx.send(res);
        });
        let res = rx
            .recv_timeout(std::time::Duration::from_secs(1))
            .expect("record_checked hung on FIFO");
        handle.join().expect("thread join");
        let err = res.expect_err("FIFO JSONL must fail");
        assert!(
            matches!(err, CoreError::InvalidPath { .. }),
            "expected InvalidPath for FIFO JSONL, got {err:?}"
        );
    }
}
