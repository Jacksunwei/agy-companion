use std::collections::HashMap;
use std::path::Path;
use std::time::Duration;

use chrono::{DateTime, SecondsFormat, Timelike, Utc};
use rusqlite::{Connection, OpenFlags, params};

use super::sqlite::{
    SQLITE_BUSY_TIMEOUT_MS, check_schema_version, db_error, read_user_version_with_busy_retry,
    sqlite_i64_to_u64, validate_sqlite_paths,
};
use super::{USAGE_DB_FILENAME, UsageBreakdownRow, UsageBucket, UsageRange, UsageSummary};
use crate::config::CoreError;
use crate::fs_safe;

const MAX_BREAKDOWN_MODELS: usize = 64;
const MAX_BREAKDOWN_PROVIDERS: usize = 32;

const WINDOW_FILTER_SQL: &str = "timestamp_utc >= ?1 AND timestamp_utc < ?2 \
    AND timestamp_utc GLOB '[0-9][0-9][0-9][0-9]-[0-9][0-9]-[0-9][0-9]T[0-2][0-9]:[0-5][0-9]:[0-5][0-9]*' \
    AND substr(timestamp_utc, 12, 2) BETWEEN '00' AND '23' \
    AND date(substr(timestamp_utc, 1, 10)) = substr(timestamp_utc, 1, 10)";

fn avg_f64_to_u64(val: f64) -> u64 {
    if !val.is_finite() || val <= 0.0 {
        0
    } else if val >= u64::MAX as f64 {
        u64::MAX
    } else {
        val.round() as u64
    }
}

fn until_utc_at(now: DateTime<Utc>) -> String {
    let today = now.date_naive();
    let next_day = today.succ_opt().unwrap_or(today);
    next_day
        .and_hms_opt(0, 0, 0)
        .expect("00:00:00 is always valid")
        .and_utc()
        .to_rfc3339_opts(SecondsFormat::Secs, true)
}

pub fn query_usage_summary(usage_dir: &Path, range: UsageRange) -> Result<UsageSummary, CoreError> {
    query_usage_summary_at(usage_dir, range, Utc::now())
}

pub fn query_usage_summary_at(
    usage_dir: &Path,
    range: UsageRange,
    now: DateTime<Utc>,
) -> Result<UsageSummary, CoreError> {
    if fs_safe::inspect_existing_dir(usage_dir)?.is_none() {
        return Ok(UsageSummary::empty_at(range, now));
    }

    let db_path = usage_dir.join(USAGE_DB_FILENAME);
    if validate_sqlite_paths(&db_path)?.is_none() {
        return Ok(UsageSummary::empty_at(range, now));
    }

    let flags = OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX;
    let conn =
        Connection::open_with_flags(&db_path, flags).map_err(|err| db_error(&db_path, err))?;
    conn.busy_timeout(Duration::from_millis(SQLITE_BUSY_TIMEOUT_MS))
        .map_err(|err| db_error(&db_path, err))?;

    let version = read_user_version_with_busy_retry(&db_path, &conn)?;
    check_schema_version(&db_path, version)?;
    if version == 0 {
        return Ok(UsageSummary::empty_at(range, now));
    }

    let since_utc = range.since_utc_at(now);
    let until_utc = until_utc_at(now);

    let totals = query_totals(&db_path, &conn, &since_utc, &until_utc)?;
    if totals.requests == 0 {
        return Ok(UsageSummary::empty_at(range, now));
    }

    let buckets = query_buckets(&db_path, &conn, range, now, &since_utc, &until_utc)?;
    let by_model = query_by_model(&db_path, &conn, &since_utc, &until_utc)?;
    let by_provider = query_by_provider(&db_path, &conn, &since_utc, &until_utc)?;

    Ok(UsageSummary {
        range,
        since_utc,
        totals,
        buckets,
        by_model,
        by_provider,
    })
}

fn query_totals(
    db_path: &Path,
    conn: &Connection,
    since_utc: &str,
    until_utc: &str,
) -> Result<UsageBucket, CoreError> {
    let sql = format!(
        "SELECT COUNT(*), \
         COALESCE(SUM(input_tokens), 0), \
         COALESCE(SUM(cached_tokens), 0), \
         COALESCE(SUM(output_tokens), 0), \
         COALESCE(SUM(reasoning_tokens), 0) \
         FROM usage_events WHERE {WINDOW_FILTER_SQL}"
    );
    conn.query_row(&sql, params![since_utc, until_utc], |row| {
        Ok(UsageBucket {
            label: String::new(),
            requests: sqlite_i64_to_u64(row.get(0)?),
            input_tokens: sqlite_i64_to_u64(row.get(1)?),
            cached_tokens: sqlite_i64_to_u64(row.get(2)?),
            output_tokens: sqlite_i64_to_u64(row.get(3)?),
            reasoning_tokens: sqlite_i64_to_u64(row.get(4)?),
        })
    })
    .map_err(|err| db_error(db_path, err))
}

fn query_buckets(
    db_path: &Path,
    conn: &Connection,
    range: UsageRange,
    now: DateTime<Utc>,
    since_utc: &str,
    until_utc: &str,
) -> Result<Vec<UsageBucket>, CoreError> {
    let key_expr = match range {
        UsageRange::Today => "substr(timestamp_utc, 12, 2)",
        UsageRange::Days7 | UsageRange::Days30 => "substr(timestamp_utc, 1, 10)",
    };
    let sql = format!(
        "SELECT {key_expr} AS bucket_key, \
         COUNT(*), \
         COALESCE(SUM(input_tokens), 0), \
         COALESCE(SUM(cached_tokens), 0), \
         COALESCE(SUM(output_tokens), 0), \
         COALESCE(SUM(reasoning_tokens), 0) \
         FROM usage_events WHERE {WINDOW_FILTER_SQL} \
         GROUP BY bucket_key"
    );

    let mut stmt = conn.prepare(&sql).map_err(|err| db_error(db_path, err))?;
    let rows = stmt
        .query_map(params![since_utc, until_utc], |row| {
            Ok((
                row.get::<_, String>(0)?,
                UsageBucket {
                    label: String::new(),
                    requests: sqlite_i64_to_u64(row.get(1)?),
                    input_tokens: sqlite_i64_to_u64(row.get(2)?),
                    cached_tokens: sqlite_i64_to_u64(row.get(3)?),
                    output_tokens: sqlite_i64_to_u64(row.get(4)?),
                    reasoning_tokens: sqlite_i64_to_u64(row.get(5)?),
                },
            ))
        })
        .map_err(|err| db_error(db_path, err))?;

    let mut by_key: HashMap<String, UsageBucket> = HashMap::new();
    for row in rows {
        let (key, bucket) = row.map_err(|err| db_error(db_path, err))?;
        by_key.insert(key, bucket);
    }

    match range {
        UsageRange::Today => {
            let max_seen_hour = by_key
                .keys()
                .filter_map(|k| k.parse::<u32>().ok())
                .filter(|&h| h <= 23)
                .max()
                .unwrap_or(0);
            let end_hour = now.hour().max(max_seen_hour).min(23);
            let mut buckets = Vec::with_capacity(usize::try_from(end_hour).unwrap_or(0) + 1);
            for hour in 0..=end_hour {
                let key = format!("{hour:02}");
                let mut bucket = by_key.remove(&key).unwrap_or_default();
                bucket.label = format!("{hour:02}:00");
                buckets.push(bucket);
            }
            Ok(buckets)
        }
        UsageRange::Days7 | UsageRange::Days30 => {
            let start_date = range.start_date_at(now);
            let today = now.date_naive();
            let days = match range {
                UsageRange::Days7 => 7,
                UsageRange::Days30 => 30,
                UsageRange::Today => unreachable!(),
            };
            let mut buckets = Vec::with_capacity(days);
            let mut current = start_date;
            while current <= today {
                let key = current.format("%Y-%m-%d").to_string();
                let mut bucket = by_key.remove(&key).unwrap_or_default();
                bucket.label = key;
                buckets.push(bucket);
                match current.succ_opt() {
                    Some(next) => current = next,
                    None => break,
                }
            }
            Ok(buckets)
        }
    }
}

fn query_by_model(
    db_path: &Path,
    conn: &Connection,
    since_utc: &str,
    until_utc: &str,
) -> Result<Vec<UsageBreakdownRow>, CoreError> {
    let sql = format!(
        "SELECT model, provider, source, \
         COUNT(*) AS requests, \
         COALESCE(SUM(input_tokens), 0) AS input_tokens, \
         COALESCE(SUM(cached_tokens), 0), \
         COALESCE(SUM(output_tokens), 0), \
         COALESCE(SUM(reasoning_tokens), 0), \
         COALESCE(AVG(latency_ms), 0.0) \
         FROM usage_events WHERE {WINDOW_FILTER_SQL} \
         GROUP BY provider, source, model \
         ORDER BY input_tokens DESC, requests DESC, model ASC \
         LIMIT {MAX_BREAKDOWN_MODELS}"
    );
    let mut stmt = conn.prepare(&sql).map_err(|err| db_error(db_path, err))?;
    let rows = stmt
        .query_map(params![since_utc, until_utc], |row| {
            Ok(UsageBreakdownRow {
                key: row.get(0)?,
                provider: row.get(1)?,
                source: Some(row.get(2)?),
                model_count: None,
                requests: sqlite_i64_to_u64(row.get(3)?),
                input_tokens: sqlite_i64_to_u64(row.get(4)?),
                cached_tokens: sqlite_i64_to_u64(row.get(5)?),
                output_tokens: sqlite_i64_to_u64(row.get(6)?),
                reasoning_tokens: sqlite_i64_to_u64(row.get(7)?),
                avg_latency_ms: avg_f64_to_u64(row.get(8)?),
            })
        })
        .map_err(|err| db_error(db_path, err))?;

    let mut out = Vec::new();
    for row in rows {
        out.push(row.map_err(|err| db_error(db_path, err))?);
    }
    Ok(out)
}

fn query_by_provider(
    db_path: &Path,
    conn: &Connection,
    since_utc: &str,
    until_utc: &str,
) -> Result<Vec<UsageBreakdownRow>, CoreError> {
    let sql = format!(
        "SELECT provider, \
         COUNT(DISTINCT model) AS model_count, \
         COUNT(*) AS requests, \
         COALESCE(SUM(input_tokens), 0) AS input_tokens, \
         COALESCE(SUM(cached_tokens), 0), \
         COALESCE(SUM(output_tokens), 0), \
         COALESCE(SUM(reasoning_tokens), 0), \
         COALESCE(AVG(latency_ms), 0.0) \
         FROM usage_events WHERE {WINDOW_FILTER_SQL} \
         GROUP BY provider \
         ORDER BY input_tokens DESC, requests DESC, provider ASC \
         LIMIT {MAX_BREAKDOWN_PROVIDERS}"
    );
    let mut stmt = conn.prepare(&sql).map_err(|err| db_error(db_path, err))?;
    let rows = stmt
        .query_map(params![since_utc, until_utc], |row| {
            let provider: String = row.get(0)?;
            Ok(UsageBreakdownRow {
                key: provider.clone(),
                provider,
                source: None,
                model_count: Some(sqlite_i64_to_u64(row.get(1)?)),
                requests: sqlite_i64_to_u64(row.get(2)?),
                input_tokens: sqlite_i64_to_u64(row.get(3)?),
                cached_tokens: sqlite_i64_to_u64(row.get(4)?),
                output_tokens: sqlite_i64_to_u64(row.get(5)?),
                reasoning_tokens: sqlite_i64_to_u64(row.get(6)?),
                avg_latency_ms: avg_f64_to_u64(row.get(7)?),
            })
        })
        .map_err(|err| db_error(db_path, err))?;

    let mut out = Vec::new();
    for row in rows {
        out.push(row.map_err(|err| db_error(db_path, err))?);
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::path::PathBuf;

    use tempfile::TempDir;

    use super::*;
    use crate::fs_safe::FsError;
    use crate::usage::{SqliteUsageSink, TelemetrySource, UsageEvent};

    fn parse_utc(ts: &str) -> DateTime<Utc> {
        DateTime::parse_from_rfc3339(ts)
            .expect("valid rfc3339")
            .with_timezone(&Utc)
    }

    #[allow(clippy::too_many_arguments)]
    fn make_event(
        ts: &str,
        id: &str,
        provider: &str,
        source: &str,
        model: &str,
        input: u64,
        cached: u64,
        output: u64,
        reasoning: u64,
        latency: u64,
    ) -> UsageEvent {
        UsageEvent {
            timestamp_utc: ts.to_string(),
            request_id: id.to_string(),
            provider: provider.to_string(),
            model: model.to_string(),
            input_tokens: input,
            output_tokens: output,
            cached_tokens: cached,
            reasoning_tokens: reasoning,
            latency_ms: latency,
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

    #[test]
    fn missing_dir_and_missing_db_return_empty_without_creating_files() {
        let tmp = TempDir::new().expect("tempdir");
        let missing_dir = tmp.path().join("does-not-exist");
        let now = parse_utc("2026-05-15T14:30:00Z");

        let summary = query_usage_summary_at(&missing_dir, UsageRange::Days7, now)
            .expect("missing dir returns empty");
        assert_eq!(summary, UsageSummary::empty_at(UsageRange::Days7, now));
        assert!(!missing_dir.exists(), "must not create usage_dir");

        let existing_dir = tmp.path().join("empty-dir");
        fs::create_dir_all(&existing_dir).expect("mkdir");
        let summary2 = query_usage_summary_at(&existing_dir, UsageRange::Today, now)
            .expect("missing db returns empty");
        assert_eq!(summary2, UsageSummary::empty_at(UsageRange::Today, now));
        assert!(
            !existing_dir.join(USAGE_DB_FILENAME).exists(),
            "must not create usage.db"
        );
    }

    #[test]
    fn schema_version_and_corrupt_db_handling() {
        let tmp = TempDir::new().expect("tempdir");
        let now = parse_utc("2026-05-15T14:30:00Z");

        // user_version == 0 empty DB returns empty summary
        let v0_dir = tmp.path().join("v0");
        fs::create_dir_all(&v0_dir).expect("mkdir");
        {
            let conn = Connection::open(v0_dir.join(USAGE_DB_FILENAME)).expect("create v0 db");
            conn.execute_batch("PRAGMA user_version = 0;").expect("v0");
        }
        let s0 = query_usage_summary_at(&v0_dir, UsageRange::Days7, now)
            .expect("v0 returns empty summary");
        assert_eq!(s0.totals.requests, 0);
        assert!(s0.buckets.is_empty());

        // user_version == 3 fails closed
        let v3_dir = tmp.path().join("v3");
        fs::create_dir_all(&v3_dir).expect("mkdir");
        {
            let conn = Connection::open(v3_dir.join(USAGE_DB_FILENAME)).expect("create v3 db");
            conn.execute_batch("PRAGMA user_version = 3;").expect("v3");
        }
        let err_v3 = query_usage_summary_at(&v3_dir, UsageRange::Days7, now)
            .expect_err("v3 must fail closed");
        assert!(matches!(err_v3, CoreError::UsageDatabase { .. }));

        // negative user_version fails closed
        let neg_dir = tmp.path().join("neg");
        fs::create_dir_all(&neg_dir).expect("mkdir");
        {
            let conn = Connection::open(neg_dir.join(USAGE_DB_FILENAME)).expect("create neg db");
            conn.execute_batch("PRAGMA user_version = -1;")
                .expect("neg");
        }
        let err_neg = query_usage_summary_at(&neg_dir, UsageRange::Days7, now)
            .expect_err("negative user_version must fail closed");
        assert!(matches!(err_neg, CoreError::UsageDatabase { .. }));

        // corrupt non-sqlite file fails closed
        let corrupt_dir = tmp.path().join("corrupt");
        fs::create_dir_all(&corrupt_dir).expect("mkdir");
        fs::write(corrupt_dir.join(USAGE_DB_FILENAME), b"not a sqlite file").expect("write");
        let err_corrupt = query_usage_summary_at(&corrupt_dir, UsageRange::Days7, now)
            .expect_err("corrupt db must fail closed");
        assert!(matches!(err_corrupt, CoreError::UsageDatabase { .. }));
    }

    #[test]
    fn traversal_and_nul_paths_rejected() {
        let now = parse_utc("2026-05-15T14:30:00Z");
        let err_dotdot = query_usage_summary_at(Path::new("/tmp/../etc"), UsageRange::Days7, now)
            .expect_err(".. traversal rejected");
        assert!(matches!(
            err_dotdot,
            CoreError::Fs(FsError::ParentDirTraversal)
        ));

        let err_nul =
            query_usage_summary_at(&PathBuf::from("/tmp/bad\0dir"), UsageRange::Days7, now)
                .expect_err("NUL rejected");
        assert!(matches!(err_nul, CoreError::Fs(FsError::ContainsNul)));
    }

    #[cfg(unix)]
    #[test]
    fn unix_symlinks_and_fifos_rejected_without_blocking() {
        use std::os::unix::fs::symlink;
        use std::process::Command;

        let tmp = TempDir::new().expect("tempdir");
        let now = parse_utc("2026-05-15T14:30:00Z");

        // Symlinked usage_dir
        let real_dir = tmp.path().join("real");
        fs::create_dir_all(&real_dir).expect("mkdir");
        let link_dir = tmp.path().join("link_dir");
        symlink(&real_dir, &link_dir).expect("symlink dir");
        let err = query_usage_summary_at(&link_dir, UsageRange::Days7, now)
            .expect_err("symlink dir rejected");
        assert!(matches!(
            err,
            CoreError::Fs(FsError::SymlinkRejected { .. })
        ));

        // Symlinked usage.db
        let db_dir = tmp.path().join("db_link");
        fs::create_dir_all(&db_dir).expect("mkdir");
        let target_file = tmp.path().join("target.db");
        fs::write(&target_file, b"").expect("write");
        symlink(&target_file, db_dir.join(USAGE_DB_FILENAME)).expect("symlink db");
        let err = query_usage_summary_at(&db_dir, UsageRange::Days7, now)
            .expect_err("symlink db rejected");
        assert!(matches!(
            err,
            CoreError::Fs(FsError::SymlinkRejected { .. })
        ));

        // Symlinked -wal sidecar
        let wal_dir = tmp.path().join("wal_link");
        fs::create_dir_all(&wal_dir).expect("mkdir");
        let sink = SqliteUsageSink::new(&wal_dir).expect("sink");
        sink.record_checked(&make_event(
            "2026-05-15T10:00:00Z",
            "req-wal",
            "gemini",
            "proxy",
            "gemini-4-carbon",
            100,
            0,
            10,
            0,
            50,
        ))
        .expect("seed");
        drop(sink);
        let wal_path = fs_safe::append_os_suffix(&wal_dir.join(USAGE_DB_FILENAME), "-wal");
        let _ = fs::remove_file(&wal_path);
        symlink(&target_file, &wal_path).expect("symlink wal");
        let err = query_usage_summary_at(&wal_dir, UsageRange::Days7, now)
            .expect_err("symlink wal rejected");
        assert!(matches!(
            err,
            CoreError::Fs(FsError::SymlinkRejected { .. })
        ));

        // FIFO usage.db must fail immediately without blocking
        let fifo_dir = tmp.path().join("fifo_db");
        fs::create_dir_all(&fifo_dir).expect("mkdir");
        let fifo_path = fifo_dir.join(USAGE_DB_FILENAME);
        let status = Command::new("mkfifo")
            .arg(&fifo_path)
            .status()
            .expect("mkfifo");
        assert!(status.success());
        let err = query_usage_summary_at(&fifo_dir, UsageRange::Days7, now)
            .expect_err("fifo db rejected");
        assert!(matches!(err, CoreError::Fs(FsError::NotRegularFile { .. })));
    }

    #[test]
    fn aggregates_today_7d_30d_and_excludes_out_of_range_and_corrupt_timestamps() {
        let tmp = TempDir::new().expect("tempdir");
        let sink = SqliteUsageSink::new(tmp.path()).expect("sink");
        let now = parse_utc("2026-05-15T14:30:00Z");

        let events = [
            // Today hour 09 — built-in Antigravity
            make_event(
                "2026-05-15T09:15:00Z",
                "r1",
                "antigravity",
                "builtin",
                "gemini-3.8-pro",
                1000,
                400,
                200,
                50,
                300,
            ),
            // Today hour 14 — BYOM Gemini
            make_event(
                "2026-05-15T14:05:00Z",
                "r2",
                "gemini",
                "proxy",
                "gemini-4-carbon",
                2000,
                1500,
                500,
                100,
                500,
            ),
            // 3 days ago — BYOM Gemini second model
            make_event(
                "2026-05-12T11:00:00Z",
                "r3",
                "gemini",
                "proxy",
                "gemini-4-neon",
                500,
                100,
                150,
                20,
                100,
            ),
            // 20 days ago — inside 30d, outside 7d
            make_event(
                "2026-04-25T08:00:00Z",
                "r4",
                "antigravity",
                "builtin",
                "gemini-3.8-pro",
                400,
                0,
                80,
                0,
                200,
            ),
            // 40 days ago — outside 30d
            make_event(
                "2026-04-05T08:00:00Z",
                "r5",
                "gemini",
                "proxy",
                "gemini-4-carbon",
                9999,
                0,
                999,
                0,
                999,
            ),
            // Extreme future / garbage timestamps — MUST be excluded
            make_event(
                "9999-12-31T23:59:59Z",
                "r6",
                "gemini",
                "proxy",
                "gemini-4-carbon",
                8888,
                0,
                888,
                0,
                888,
            ),
            make_event(
                "../../etc/passwd",
                "r7",
                "gemini",
                "proxy",
                "gemini-4-carbon",
                7777,
                0,
                777,
                0,
                777,
            ),
        ];

        for ev in &events {
            sink.record_checked(ev).expect("record");
        }

        // 1d (Today): r1 + r2, 15 hourly buckets ("00:00"..="14:00")
        let s1d = query_usage_summary_at(tmp.path(), UsageRange::Today, now).expect("1d");
        assert_eq!(s1d.range, UsageRange::Today);
        assert_eq!(s1d.since_utc, "2026-05-15T00:00:00Z");
        assert_eq!(s1d.totals.requests, 2);
        assert_eq!(s1d.totals.input_tokens, 3000);
        assert_eq!(s1d.totals.cached_tokens, 1900);
        assert_eq!(s1d.totals.output_tokens, 700);
        assert_eq!(s1d.totals.reasoning_tokens, 150);
        assert_eq!(s1d.buckets.len(), 15);
        assert_eq!(s1d.buckets[0].label, "00:00");
        assert_eq!(s1d.buckets[9].label, "09:00");
        assert_eq!(s1d.buckets[9].requests, 1);
        assert_eq!(s1d.buckets[14].label, "14:00");
        assert_eq!(s1d.buckets[14].requests, 1);
        assert_eq!(s1d.by_model.len(), 2);
        assert_eq!(s1d.by_model[0].key, "gemini-4-carbon");
        assert_eq!(s1d.by_model[0].provider, "gemini");
        assert_eq!(s1d.by_model[0].source.as_deref(), Some("proxy"));
        assert_eq!(s1d.by_model[0].model_count, None);
        assert_eq!(s1d.by_model[0].avg_latency_ms, 500);
        assert_eq!(s1d.by_model[1].key, "gemini-3.8-pro");
        assert_eq!(s1d.by_model[1].provider, "antigravity");
        assert_eq!(s1d.by_model[1].source.as_deref(), Some("builtin"));

        // 7d: r1 + r2 + r3, 7 daily buckets ("2026-05-09"..="2026-05-15")
        let s7d = query_usage_summary_at(tmp.path(), UsageRange::Days7, now).expect("7d");
        assert_eq!(s7d.totals.requests, 3);
        assert_eq!(s7d.totals.input_tokens, 3500);
        assert_eq!(s7d.buckets.len(), 7);
        assert_eq!(s7d.buckets[0].label, "2026-05-09");
        assert_eq!(s7d.buckets[3].label, "2026-05-12");
        assert_eq!(s7d.buckets[3].requests, 1);
        assert_eq!(s7d.buckets[6].label, "2026-05-15");
        assert_eq!(s7d.buckets[6].requests, 2);
        assert_eq!(s7d.by_provider.len(), 2);
        assert_eq!(s7d.by_provider[0].key, "gemini");
        assert_eq!(s7d.by_provider[0].source, None);
        assert_eq!(s7d.by_provider[0].model_count, Some(2));
        assert_eq!(s7d.by_provider[0].avg_latency_ms, 300); // (500 + 100) / 2
        assert_eq!(s7d.by_provider[1].key, "antigravity");
        assert_eq!(s7d.by_provider[1].model_count, Some(1));

        // 30d: r1 + r2 + r3 + r4, 30 daily buckets
        let s30d = query_usage_summary_at(tmp.path(), UsageRange::Days30, now).expect("30d");
        assert_eq!(s30d.totals.requests, 4);
        assert_eq!(s30d.totals.input_tokens, 3900);
        assert_eq!(s30d.buckets.len(), 30);
        assert_eq!(s30d.buckets[0].label, "2026-04-16");
        assert_eq!(s30d.buckets[29].label, "2026-05-15");
        assert_eq!(s30d.by_provider[1].avg_latency_ms, 250); // (300 + 200) / 2
    }

    #[test]
    fn corrupt_hour_and_date_timestamps_excluded_consistently_across_ranges() {
        let tmp = TempDir::new().expect("tempdir");
        let sink = SqliteUsageSink::new(tmp.path()).expect("sink");
        let now = parse_utc("2026-06-02T14:30:00Z");

        let events = [
            // Valid today
            make_event(
                "2026-06-02T09:00:00Z",
                "ok-1",
                "gemini",
                "proxy",
                "gemini-4-carbon",
                1000,
                500,
                200,
                10,
                100,
            ),
            // Valid today with fractional seconds
            make_event(
                "2026-06-02T14:15:30.456Z",
                "ok-2",
                "antigravity",
                "builtin",
                "gemini-3.8-pro",
                500,
                100,
                100,
                0,
                200,
            ),
            // Valid 3 days ago (in 7d & 30d)
            make_event(
                "2026-05-30T10:00:00Z",
                "ok-3",
                "gemini",
                "proxy",
                "gemini-4-neon",
                300,
                0,
                50,
                0,
                150,
            ),
            // Corrupt hour 99:99:99 today — MUST be excluded from totals, buckets, by_model, by_provider
            make_event(
                "2026-06-02T99:99:99Z",
                "bad-hour-99",
                "gemini",
                "proxy",
                "gemini-4-carbon",
                9999,
                0,
                999,
                0,
                999,
            ),
            // Corrupt hour 24:00:00 today — MUST be excluded
            make_event(
                "2026-06-02T24:00:00Z",
                "bad-hour-24",
                "gemini",
                "proxy",
                "gemini-4-carbon",
                8888,
                0,
                888,
                0,
                888,
            ),
            // Corrupt cross-month day 2026-05-99 inside 7d/30d lexicographic window — MUST be excluded
            make_event(
                "2026-05-99T12:00:00Z",
                "bad-date-99",
                "gemini",
                "proxy",
                "gemini-4-carbon",
                7777,
                0,
                777,
                0,
                777,
            ),
        ];
        for ev in &events {
            sink.record_checked(ev).expect("record");
        }

        for (range, expected_requests, expected_input) in [
            (UsageRange::Today, 2u64, 1500u64),
            (UsageRange::Days7, 3u64, 1800u64),
            (UsageRange::Days30, 3u64, 1800u64),
        ] {
            let summary = query_usage_summary_at(tmp.path(), range, now).expect("summary");
            let bucket_requests: u64 = summary.buckets.iter().map(|b| b.requests).sum();
            let bucket_input: u64 = summary.buckets.iter().map(|b| b.input_tokens).sum();
            let model_requests: u64 = summary.by_model.iter().map(|m| m.requests).sum();
            let provider_requests: u64 = summary.by_provider.iter().map(|p| p.requests).sum();

            assert_eq!(summary.totals.requests, expected_requests, "{range:?}");
            assert_eq!(summary.totals.input_tokens, expected_input, "{range:?}");
            assert_eq!(
                summary.totals.requests, bucket_requests,
                "totals.requests must equal sum(buckets) for {range:?}"
            );
            assert_eq!(
                summary.totals.input_tokens, bucket_input,
                "totals.input_tokens must equal sum(buckets) for {range:?}"
            );
            assert_eq!(summary.totals.requests, model_requests, "{range:?}");
            assert_eq!(summary.totals.requests, provider_requests, "{range:?}");
        }
    }

    #[test]
    fn v2_reconciled_and_normalized_utc_rows_aggregate_without_double_counting() {
        let tmp = TempDir::new().expect("tempdir");
        let sink = SqliteUsageSink::new(tmp.path()).expect("sink");
        let now = parse_utc("2026-05-15T20:00:00Z");

        // 1. Proxy row followed by matching LS row -> reconciles to 1 row (no double-count).
        let mut proxy_ev = make_event(
            "2026-05-15T10:15:30.999Z",
            "proxy:resp-1",
            "gemini",
            "proxy",
            "gemini-4-carbon",
            1000,
            600,
            200,
            50,
            400,
        );
        proxy_ev.response_id = Some("G4EIa-q1".to_string());
        proxy_ev.telemetry_source = TelemetrySource::Proxy;

        let mut ls_match = make_event(
            "2026-05-15T10:15:31.123Z",
            "ls:conv_q1:1:0",
            "antigravity",
            "builtin",
            "gemini-4-carbon",
            1000,
            600,
            200,
            50,
            420,
        );
        ls_match.response_id = Some("G4EIa-q1".to_string());
        ls_match.conversation_id = Some("conv_q1".to_string());
        ls_match.telemetry_source = TelemetrySource::Ls;

        // 2. LS-only row with non-UTC offset (-07:00) -> normalized to 2026-05-15T18:00:00Z.
        let mut ls_offset = make_event(
            "2026-05-15T11:00:00.456-07:00",
            "ls:conv_q2:2:0",
            "vertex_claude",
            "builtin",
            "claude-sonnet-4-6",
            2000,
            1000,
            300,
            0,
            600,
        );
        ls_offset.response_id = Some("AJ8Ia-q2".to_string());
        ls_offset.conversation_id = Some("conv_q2".to_string());
        ls_offset.telemetry_source = TelemetrySource::Ls;

        // 3. SIDE_QUESTION clone of ls_offset -> must not double-count.
        let mut side_q = ls_offset.clone();
        side_q.request_id = "ls:conv_sideq:2:0".to_string();
        side_q.conversation_id = Some("conv_sideq".to_string());

        sink.record_batch_checked(&[proxy_ev, ls_match, ls_offset, side_q], None)
            .expect("record v2 batch");

        let s1d = query_usage_summary_at(tmp.path(), UsageRange::Today, now).expect("1d");
        assert_eq!(s1d.totals.requests, 2);
        assert_eq!(s1d.totals.input_tokens, 3000);
        assert_eq!(s1d.totals.cached_tokens, 1600);
        assert_eq!(s1d.buckets[10].label, "10:00");
        assert_eq!(s1d.buckets[10].requests, 1);
        assert_eq!(s1d.buckets[18].label, "18:00");
        assert_eq!(s1d.buckets[18].requests, 1);

        let s7d = query_usage_summary_at(tmp.path(), UsageRange::Days7, now).expect("7d");
        assert_eq!(s7d.totals.requests, 2);
        let s30d = query_usage_summary_at(tmp.path(), UsageRange::Days30, now).expect("30d");
        assert_eq!(s30d.totals.requests, 2);
    }
}
