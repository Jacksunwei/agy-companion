use std::fmt;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::Duration;

use rusqlite::{Connection, ErrorCode, OpenFlags, TransactionBehavior, params};

use super::{USAGE_DB_FILENAME, USAGE_DB_SCHEMA_VERSION, UsageEvent, UsageSink};
use crate::config::CoreError;
use crate::fs_safe::{self, FsError, OpenRegularMode};

const SQLITE_BUSY_TIMEOUT_MS: u64 = 5_000;
const SQLITE_INIT_BUSY_RETRIES: u32 = 16;

const SCHEMA_V1_DDL: &str = "
CREATE TABLE IF NOT EXISTS usage_events (
    id               INTEGER PRIMARY KEY AUTOINCREMENT,
    timestamp_utc    TEXT    NOT NULL,
    request_id       TEXT    NOT NULL UNIQUE,
    provider         TEXT    NOT NULL,
    model            TEXT    NOT NULL,
    input_tokens     INTEGER NOT NULL CHECK (input_tokens >= 0),
    output_tokens    INTEGER NOT NULL CHECK (output_tokens >= 0),
    cached_tokens    INTEGER NOT NULL CHECK (cached_tokens >= 0),
    reasoning_tokens INTEGER NOT NULL CHECK (reasoning_tokens >= 0),
    latency_ms       INTEGER NOT NULL CHECK (latency_ms >= 0),
    source           TEXT    NOT NULL
);

CREATE INDEX IF NOT EXISTS idx_usage_events_timestamp_utc
    ON usage_events (timestamp_utc);

CREATE INDEX IF NOT EXISTS idx_usage_events_provider_model
    ON usage_events (provider, model);

PRAGMA user_version = 1;
";

pub struct SqliteUsageSink {
    usage_dir: PathBuf,
    db_path: PathBuf,
    conn: Mutex<Option<Connection>>,
}

impl fmt::Debug for SqliteUsageSink {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("SqliteUsageSink")
            .field("usage_dir", &self.usage_dir)
            .field("db_path", &self.db_path)
            .finish_non_exhaustive()
    }
}

fn u64_to_sqlite_i64(value: u64) -> i64 {
    i64::try_from(value).unwrap_or(i64::MAX)
}

fn sqlite_i64_to_u64(value: i64) -> u64 {
    u64::try_from(value).unwrap_or(0)
}

fn is_busy_or_locked(err: &rusqlite::Error) -> bool {
    matches!(
        err.sqlite_error_code(),
        Some(ErrorCode::DatabaseBusy | ErrorCode::DatabaseLocked)
    )
}

fn should_invalidate_connection(err: &rusqlite::Error) -> bool {
    !matches!(
        err.sqlite_error_code(),
        Some(ErrorCode::ConstraintViolation)
    )
}

impl SqliteUsageSink {
    pub fn new(usage_dir: impl Into<PathBuf>) -> Result<Self, CoreError> {
        let usage_dir = usage_dir.into();
        fs_safe::validate_path(&usage_dir)?;
        // Reject symlink usage directories eagerly while preserving lazy NotDirectory rejection
        // until first record/read operation.
        match fs_safe::inspect_existing_dir(&usage_dir) {
            Ok(_) | Err(FsError::NotDirectory { .. }) => {}
            Err(err) => return Err(err.into()),
        }
        let db_path = usage_dir.join(USAGE_DB_FILENAME);
        Ok(Self {
            usage_dir,
            db_path,
            conn: Mutex::new(None),
        })
    }

    pub fn usage_dir(&self) -> &Path {
        &self.usage_dir
    }

    pub fn db_path(&self) -> &Path {
        &self.db_path
    }

    fn db_error(&self, err: impl fmt::Display) -> CoreError {
        CoreError::UsageDatabase {
            path: self.db_path.clone(),
            reason: err.to_string(),
        }
    }

    fn sqlite_related_paths(&self) -> [PathBuf; 4] {
        [
            self.db_path.clone(),
            fs_safe::append_os_suffix(&self.db_path, "-wal"),
            fs_safe::append_os_suffix(&self.db_path, "-shm"),
            fs_safe::append_os_suffix(&self.db_path, "-journal"),
        ]
    }

    fn validate_sqlite_paths(&self) -> Result<Option<fs::Metadata>, CoreError> {
        let paths = self.sqlite_related_paths();
        let db_meta = fs_safe::inspect_regular_file_if_exists(&paths[0], None)?;
        for sidecar in &paths[1..] {
            let _ = fs_safe::inspect_regular_file_if_exists(sidecar, None)?;
        }
        Ok(db_meta)
    }

    fn tighten_sqlite_file_permissions(&self) -> Result<(), CoreError> {
        for path in self.sqlite_related_paths() {
            fs_safe::tighten_regular_file_mode_0600(&path)?;
        }
        Ok(())
    }

    fn check_schema_version(&self, version: u32) -> Result<(), CoreError> {
        if version > USAGE_DB_SCHEMA_VERSION {
            return Err(CoreError::UsageDatabase {
                path: self.db_path.clone(),
                reason: format!(
                    "unsupported usage database schema version {version} (supported: {USAGE_DB_SCHEMA_VERSION})"
                ),
            });
        }
        Ok(())
    }

    fn read_user_version(&self, conn: &Connection) -> Result<u32, CoreError> {
        let raw: i64 = conn
            .pragma_query_value(None, "user_version", |row| row.get(0))
            .map_err(|err| self.db_error(err))?;
        u32::try_from(raw).map_err(|_| {
            self.db_error(format!(
                "invalid negative or out-of-range user_version {raw}"
            ))
        })
    }

    fn read_user_version_with_busy_retry(&self, conn: &Connection) -> Result<u32, CoreError> {
        let mut last_busy = None;
        for attempt in 0..SQLITE_INIT_BUSY_RETRIES {
            match conn.pragma_query_value(None, "user_version", |row| row.get::<_, i64>(0)) {
                Ok(raw) => {
                    return u32::try_from(raw).map_err(|_| {
                        self.db_error(format!(
                            "invalid negative or out-of-range user_version {raw}"
                        ))
                    });
                }
                Err(err) if is_busy_or_locked(&err) => {
                    last_busy = Some(err);
                    std::thread::sleep(Duration::from_millis(5 * u64::from(attempt + 1)));
                }
                Err(err) => return Err(self.db_error(err)),
            }
        }
        Err(self.db_error(last_busy.expect("SQLITE_INIT_BUSY_RETRIES > 0 guarantees a busy error")))
    }

    fn configure_and_migrate(&self, conn: &mut Connection) -> Result<(), CoreError> {
        conn.busy_timeout(Duration::from_millis(SQLITE_BUSY_TIMEOUT_MS))
            .map_err(|err| self.db_error(err))?;

        // Validate schema version BEFORE executing any mutating PRAGMA (such as
        // `PRAGMA journal_mode = WAL`) so unsupported future or negative
        // user_version databases are rejected fail-closed with zero on-disk mutation.
        let pre_version = self.read_user_version_with_busy_retry(conn)?;
        self.check_schema_version(pre_version)?;

        // Two concurrent cold-start connections upgrading an empty DB into WAL mode
        // can receive an immediate SQLITE_BUSY deadlock-avoidance error from SQLite;
        // retry briefly so multi-instance writers initialize cleanly.
        let mut last_busy = None;
        for attempt in 0..SQLITE_INIT_BUSY_RETRIES {
            match conn.execute_batch(
                "PRAGMA journal_mode = WAL;
                 PRAGMA synchronous = NORMAL;
                 PRAGMA foreign_keys = ON;
                 PRAGMA temp_store = MEMORY;",
            ) {
                Ok(()) => {
                    last_busy = None;
                    break;
                }
                Err(err) if is_busy_or_locked(&err) => {
                    last_busy = Some(err);
                    std::thread::sleep(Duration::from_millis(5 * u64::from(attempt + 1)));
                }
                Err(err) => return Err(self.db_error(err)),
            }
        }
        if let Some(err) = last_busy {
            return Err(self.db_error(err));
        }

        let version = self.read_user_version(conn)?;
        self.check_schema_version(version)?;
        if version == 0 {
            let tx = conn
                .transaction_with_behavior(TransactionBehavior::Immediate)
                .map_err(|err| self.db_error(err))?;
            let tx_version = self.read_user_version(&tx)?;
            self.check_schema_version(tx_version)?;
            if tx_version == 0 {
                tx.execute_batch(SCHEMA_V1_DDL)
                    .map_err(|err| self.db_error(err))?;
            }
            tx.commit().map_err(|err| self.db_error(err))?;
        }

        Ok(())
    }

    fn open_rw_connection(&self) -> Result<Connection, CoreError> {
        fs_safe::ensure_dir_no_symlink(&self.usage_dir, 0o700)?;
        let _ = self.validate_sqlite_paths()?;
        let _ = fs_safe::open_verified_regular_file_0600(
            &self.db_path,
            OpenRegularMode::ReadWriteCreate,
        )?;

        // Explicitly omit SQLITE_OPEN_CREATE and SQLITE_OPEN_URI so SQLite only
        // opens the pre-verified regular file path verbatim.
        let flags = OpenFlags::SQLITE_OPEN_READ_WRITE | OpenFlags::SQLITE_OPEN_NO_MUTEX;
        let mut conn =
            Connection::open_with_flags(&self.db_path, flags).map_err(|err| self.db_error(err))?;
        self.configure_and_migrate(&mut conn)?;
        self.tighten_sqlite_file_permissions()?;
        Ok(conn)
    }

    fn query_all_from_connection(&self, conn: &Connection) -> Result<Vec<UsageEvent>, CoreError> {
        let mut stmt = conn
            .prepare(
                "SELECT timestamp_utc, request_id, provider, model, \
                 input_tokens, output_tokens, cached_tokens, reasoning_tokens, \
                 latency_ms, source \
                 FROM usage_events \
                 ORDER BY id ASC",
            )
            .map_err(|err| self.db_error(err))?;

        let rows = stmt
            .query_map([], |row| {
                Ok(UsageEvent {
                    timestamp_utc: row.get(0)?,
                    request_id: row.get(1)?,
                    provider: row.get(2)?,
                    model: row.get(3)?,
                    input_tokens: sqlite_i64_to_u64(row.get(4)?),
                    output_tokens: sqlite_i64_to_u64(row.get(5)?),
                    cached_tokens: sqlite_i64_to_u64(row.get(6)?),
                    reasoning_tokens: sqlite_i64_to_u64(row.get(7)?),
                    latency_ms: sqlite_i64_to_u64(row.get(8)?),
                    source: row.get(9)?,
                })
            })
            .map_err(|err| self.db_error(err))?;

        let mut events = Vec::new();
        for row in rows {
            events.push(row.map_err(|err| self.db_error(err))?);
        }
        Ok(events)
    }

    pub fn record_checked(&self, event: &UsageEvent) -> Result<PathBuf, CoreError> {
        let mut conn_guard = self
            .conn
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());

        if conn_guard.is_none() {
            *conn_guard = Some(self.open_rw_connection()?);
        }

        let insert_res = {
            let conn = conn_guard
                .as_ref()
                .expect("connection initialized immediately above");
            conn.execute(
                "INSERT INTO usage_events (
                    timestamp_utc,
                    request_id,
                    provider,
                    model,
                    input_tokens,
                    output_tokens,
                    cached_tokens,
                    reasoning_tokens,
                    latency_ms,
                    source
                ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)",
                params![
                    &event.timestamp_utc,
                    &event.request_id,
                    &event.provider,
                    &event.model,
                    u64_to_sqlite_i64(event.input_tokens),
                    u64_to_sqlite_i64(event.output_tokens),
                    u64_to_sqlite_i64(event.cached_tokens),
                    u64_to_sqlite_i64(event.reasoning_tokens),
                    u64_to_sqlite_i64(event.latency_ms),
                    &event.source,
                ],
            )
        };

        if let Err(err) = insert_res {
            if should_invalidate_connection(&err) {
                *conn_guard = None;
            }
            return Err(self.db_error(err));
        }

        if let Err(err) = self.tighten_sqlite_file_permissions() {
            *conn_guard = None;
            return Err(err);
        }

        Ok(self.db_path.clone())
    }

    pub fn read_all_events(&self) -> Result<Vec<UsageEvent>, CoreError> {
        let mut conn_guard = self
            .conn
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());

        if let Some(ref conn) = *conn_guard {
            return match self.query_all_from_connection(conn) {
                Ok(events) => Ok(events),
                Err(err) => {
                    *conn_guard = None;
                    Err(err)
                }
            };
        }

        if fs_safe::inspect_existing_dir(&self.usage_dir)?.is_none() {
            return Ok(Vec::new());
        }
        let Some(_) = self.validate_sqlite_paths()? else {
            return Ok(Vec::new());
        };

        let flags = OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX;
        let conn =
            Connection::open_with_flags(&self.db_path, flags).map_err(|err| self.db_error(err))?;
        conn.busy_timeout(Duration::from_millis(SQLITE_BUSY_TIMEOUT_MS))
            .map_err(|err| self.db_error(err))?;

        let version = self.read_user_version(&conn)?;
        self.check_schema_version(version)?;
        let events = self.query_all_from_connection(&conn)?;
        drop(conn);
        self.tighten_sqlite_file_permissions()?;
        Ok(events)
    }
}

impl UsageSink for SqliteUsageSink {
    fn record(&self, event: UsageEvent) {
        if let Err(err) = self.record_checked(&event) {
            tracing::warn!(error = %err, "failed to record usage event to SQLite sink");
        }
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashSet;
    use std::fs;
    use std::sync::Arc;

    use tempfile::TempDir;

    use super::*;

    fn sample_event(timestamp_utc: &str, request_id: &str) -> UsageEvent {
        UsageEvent {
            timestamp_utc: timestamp_utc.to_string(),
            request_id: request_id.to_string(),
            provider: "gemini".to_string(),
            model: "gemini-4-hydrogen".to_string(),
            input_tokens: 1200,
            output_tokens: 350,
            cached_tokens: 900,
            reasoning_tokens: 120,
            latency_ms: 842,
            source: "proxy".to_string(),
        }
    }

    fn read_all_sqlite_raw_bytes(usage_dir: &Path) -> Vec<u8> {
        let mut combined = Vec::new();
        for name in [
            "usage.db",
            "usage.db-wal",
            "usage.db-shm",
            "usage.db-journal",
        ] {
            let path = usage_dir.join(name);
            if path.is_file() {
                combined.extend_from_slice(&fs::read(&path).expect("read sqlite file bytes"));
            }
        }
        combined
    }

    fn contains_bytes(haystack: &[u8], needle: &[u8]) -> bool {
        !needle.is_empty() && haystack.windows(needle.len()).any(|w| w == needle)
    }

    #[test]
    fn sqlite_sink_round_trips_all_ten_fields_without_body_or_secret_content() {
        let tmp = TempDir::new().expect("temp dir");
        let sink = SqliteUsageSink::new(tmp.path()).expect("sink init");
        assert_eq!(sink.usage_dir(), tmp.path());
        assert_eq!(sink.db_path(), tmp.path().join(USAGE_DB_FILENAME));
        // Lazy init: construction must not create usage.db.
        assert!(!sink.db_path().exists());

        let e1 = sample_event("2026-11-22T22:00:00Z", "req-1");
        let e2 = sample_event("2026-11-22T22:05:00Z", "req-2");
        sink.record(e1.clone());
        sink.record(e2.clone());

        assert!(sink.db_path().is_file());
        let events = sink.read_all_events().expect("read events");
        assert_eq!(events, vec![e1, e2]);

        let raw_bytes = read_all_sqlite_raw_bytes(tmp.path());
        assert!(!raw_bytes.is_empty());
        assert!(!contains_bytes(&raw_bytes, b"AIza"));
        assert!(!contains_bytes(&raw_bytes, b"prompt"));
        assert!(!contains_bytes(&raw_bytes, b"response"));
        assert!(!contains_bytes(&raw_bytes, b"api_key"));

        // Verify PRAGMA user_version, WAL mode, and schema columns directly.
        let conn = Connection::open(sink.db_path()).expect("open db");
        let version: u32 = conn
            .pragma_query_value(None, "user_version", |row| row.get(0))
            .expect("user_version");
        assert_eq!(version, USAGE_DB_SCHEMA_VERSION);
        let journal_mode: String = conn
            .pragma_query_value(None, "journal_mode", |row| row.get(0))
            .expect("journal_mode");
        assert_eq!(journal_mode.to_ascii_lowercase(), "wal");

        let mut stmt = conn
            .prepare("PRAGMA table_info(usage_events)")
            .expect("table_info");
        let columns: Vec<String> = stmt
            .query_map([], |row| row.get(1))
            .expect("query columns")
            .map(|r| r.expect("col name"))
            .collect();
        assert_eq!(
            columns,
            vec![
                "id",
                "timestamp_utc",
                "request_id",
                "provider",
                "model",
                "input_tokens",
                "output_tokens",
                "cached_tokens",
                "reasoning_tokens",
                "latency_ms",
                "source",
            ]
        );
    }

    #[test]
    fn sqlite_sink_enforces_0600_and_concurrent_thread_safe_writes() {
        let tmp = TempDir::new().expect("temp dir");
        let usage_subdir = tmp.path().join("nested").join("usage");
        let sink = Arc::new(SqliteUsageSink::new(&usage_subdir).expect("sink init"));
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

        let events = sink.read_all_events().expect("read 80 events");
        assert_eq!(events.len(), 80);
        let ids: HashSet<&str> = events.iter().map(|e| e.request_id.as_str()).collect();
        assert_eq!(ids.len(), 80);

        // Multi-instance concurrent writers against the same usage_dir.
        let multi_dir = tmp.path().join("multi_instance");
        let mut multi_handles = Vec::new();
        for w in 0..4 {
            let dir = multi_dir.clone();
            multi_handles.push(std::thread::spawn(move || {
                let writer = SqliteUsageSink::new(&dir).expect("writer init");
                for i in 0..10 {
                    writer
                        .record_checked(&sample_event(
                            "2026-11-22T22:20:00Z",
                            &format!("w{w}-i{i}"),
                        ))
                        .expect("multi-instance record");
                }
            }));
        }
        for handle in multi_handles {
            handle.join().expect("multi writer join");
        }
        let reader = SqliteUsageSink::new(&multi_dir).expect("reader init");
        let multi_events = reader.read_all_events().expect("read multi events");
        assert_eq!(multi_events.len(), 40);

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;

            let dir_mode = fs::metadata(&usage_subdir)
                .expect("dir meta")
                .permissions()
                .mode()
                & 0o777;
            assert_eq!(dir_mode, 0o700);
            // Verify 4-file symmetry: an existing loose-mode usage.db-journal is also tightened to 0600.
            let journal_path = usage_subdir.join("usage.db-journal");
            fs::write(&journal_path, b"").expect("create journal sidecar");
            fs::set_permissions(&journal_path, fs::Permissions::from_mode(0o644))
                .expect("set 0644 on journal");
            sink.record_checked(&sample_event("2025-11-22T22:15:00Z", "journal-tighten"))
                .expect("record tightens sidecar permissions");
            for name in [
                "usage.db",
                "usage.db-wal",
                "usage.db-shm",
                "usage.db-journal",
            ] {
                let path = usage_subdir.join(name);
                if path.exists() {
                    let mode = fs::metadata(&path).expect("meta").permissions().mode() & 0o777;
                    assert_eq!(mode, 0o600, "expected 0600 on {name}, got {mode:o}");
                }
            }
        }
    }

    #[test]
    fn read_all_events_on_missing_db_does_not_create_files() {
        let tmp = TempDir::new().expect("temp dir");
        let missing_dir = tmp.path().join("nonexistent_usage");
        let sink = SqliteUsageSink::new(&missing_dir).expect("sink init");
        assert_eq!(
            sink.read_all_events().expect("read missing dir"),
            Vec::new()
        );
        assert!(!missing_dir.exists());

        let empty_dir = tmp.path().join("empty_usage");
        fs::create_dir(&empty_dir).expect("mkdir empty_usage");
        let empty_sink = SqliteUsageSink::new(&empty_dir).expect("empty sink init");
        assert_eq!(
            empty_sink.read_all_events().expect("read empty dir"),
            Vec::new()
        );
        assert!(!empty_dir.join(USAGE_DB_FILENAME).exists());
    }

    #[test]
    fn sqlite_sink_rejects_future_schema_version_fail_closed() {
        for (raw_version, expected_err_substr) in [
            (2_i64, "schema version 2"),
            (-1_i64, "invalid negative or out-of-range user_version -1"),
        ] {
            let tmp = TempDir::new().expect("temp dir");
            let db_path = tmp.path().join(USAGE_DB_FILENAME);
            {
                let conn = Connection::open(&db_path).expect("create unsupported db");
                conn.execute_batch(
                    "PRAGMA journal_mode = DELETE;
                     CREATE TABLE future_schema_data (id INTEGER PRIMARY KEY, note TEXT NOT NULL);
                     INSERT INTO future_schema_data (note) VALUES ('preserve-me');",
                )
                .expect("seed future schema db");
                conn.pragma_update(None, "user_version", raw_version)
                    .expect("set unsupported user_version");
            }

            let before_bytes = fs::read(&db_path).expect("read db bytes before");
            assert!(
                before_bytes.len() >= 20 && &before_bytes[18..20] == b"\x01\x01",
                "expected DELETE-mode SQLite header bytes [1, 1] for user_version={raw_version}"
            );

            let sink = SqliteUsageSink::new(tmp.path()).expect("sink init");
            let write_err = sink
                .record_checked(&sample_event("2026-11-22T22:00:00Z", "future-write"))
                .expect_err("unsupported schema version must fail write");
            assert!(
                matches!(write_err, CoreError::UsageDatabase { .. }),
                "expected UsageDatabase error, got {write_err:?}"
            );
            assert!(
                write_err.to_string().contains(expected_err_substr),
                "unexpected error text for user_version={raw_version}: {write_err}"
            );

            let read_err = sink
                .read_all_events()
                .expect_err("unsupported schema version must fail read");
            assert!(
                matches!(read_err, CoreError::UsageDatabase { .. }),
                "expected UsageDatabase error on read, got {read_err:?}"
            );

            let after_bytes = fs::read(&db_path).expect("read db bytes after");
            assert_eq!(
                after_bytes, before_bytes,
                "unsupported user_version={raw_version} database must remain byte-for-byte unchanged"
            );
            assert_eq!(&after_bytes[18..20], b"\x01\x01");
            assert!(
                !tmp.path().join("usage.db-wal").exists(),
                "no -wal sidecar should be created for unsupported user_version={raw_version}"
            );
            assert!(
                !tmp.path().join("usage.db-shm").exists(),
                "no -shm sidecar should be created for unsupported user_version={raw_version}"
            );

            let verify_conn = Connection::open_with_flags(
                &db_path,
                OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
            )
            .expect("open db read-only for verification");
            let after_jm: String = verify_conn
                .pragma_query_value(None, "journal_mode", |row| row.get(0))
                .expect("read after journal_mode");
            assert_eq!(after_jm.to_ascii_lowercase(), "delete");
        }
    }

    #[test]
    fn sqlite_sink_clamps_u64_max_handles_extreme_timestamps_and_duplicate_request_id() {
        let tmp = TempDir::new().expect("temp dir");
        let sink = SqliteUsageSink::new(tmp.path()).expect("sink init");

        let max_event = UsageEvent {
            timestamp_utc: "9999-12-31T23:59:59Z".to_string(),
            request_id: "max-u64".to_string(),
            provider: "gemini".to_string(),
            model: "gemini-4-hydrogen".to_string(),
            input_tokens: u64::MAX,
            output_tokens: u64::MAX,
            cached_tokens: u64::MAX,
            reasoning_tokens: u64::MAX,
            latency_ms: u64::MAX,
            source: "proxy".to_string(),
        };
        sink.record_checked(&max_event)
            .expect("u64::MAX clamped safely");

        for (ts, req_id) in [
            ("0000-01-01T00:00:00+05:00", "year-underflow"),
            ("1969-12-31T23:59:59Z", "pre-epoch"),
            ("../../etc/passwd", "weird-ts"),
        ] {
            let path = sink
                .record_checked(&sample_event(ts, req_id))
                .expect("extreme timestamp stored as opaque text");
            assert_eq!(path, tmp.path().join(USAGE_DB_FILENAME));
        }

        // Duplicate request_id must error in record_checked without panicking in record().
        let dup_err = sink
            .record_checked(&sample_event("2026-11-22T22:00:00Z", "max-u64"))
            .expect_err("duplicate request_id must fail");
        assert!(matches!(dup_err, CoreError::UsageDatabase { .. }));
        sink.record(sample_event("2026-11-22T22:00:00Z", "max-u64"));

        // Healthy connection remains usable after a constraint violation.
        sink.record_checked(&sample_event("2026-11-22T22:01:00Z", "after-dup"))
            .expect("subsequent unique write succeeds");

        let events = sink.read_all_events().expect("read events");
        assert_eq!(events.len(), 5);
        assert_eq!(events[0].input_tokens, i64::MAX as u64);
        assert_eq!(events[0].output_tokens, i64::MAX as u64);
        assert_eq!(events[0].cached_tokens, i64::MAX as u64);
        assert_eq!(events[0].reasoning_tokens, i64::MAX as u64);
        assert_eq!(events[0].latency_ms, i64::MAX as u64);
        assert_eq!(events[4].request_id, "after-dup");
    }

    #[test]
    fn sqlite_sink_handles_io_failure_without_panicking() {
        assert!(matches!(
            SqliteUsageSink::new("../escape"),
            Err(CoreError::Fs(FsError::ParentDirTraversal))
        ));

        let tmp = TempDir::new().expect("temp dir");
        let blocker_file = tmp.path().join("not-a-dir");
        fs::write(&blocker_file, "occupied").expect("write blocker");

        let sink = SqliteUsageSink::new(&blocker_file).expect("valid path");
        let event = sample_event("2026-11-22T22:15:00Z", "io-err");
        assert!(sink.record_checked(&event).is_err());

        // UsageSink::record must never panic on I/O errors
        sink.record(event);
    }

    #[test]
    fn sqlite_sink_leaves_legacy_jsonl_files_untouched() {
        let tmp = TempDir::new().expect("temp dir");
        let legacy_path = tmp.path().join("2026-10.jsonl");
        let legacy_content = "{\"legacy\":true}\n";
        fs::write(&legacy_path, legacy_content).expect("write legacy jsonl");

        let sink = SqliteUsageSink::new(tmp.path()).expect("sink init");
        sink.record_checked(&sample_event("2026-11-23T08:00:00Z", "coexist-1"))
            .expect("sqlite write succeeds alongside legacy jsonl");

        assert_eq!(
            fs::read_to_string(&legacy_path).expect("read legacy"),
            legacy_content
        );
        let events = sink.read_all_events().expect("read events");
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].request_id, "coexist-1");
    }

    #[test]
    fn sqlite_sink_invalidates_cached_connection_on_readonly_error_and_recovers() {
        let tmp = TempDir::new().expect("temp dir");
        let sink = SqliteUsageSink::new(tmp.path()).expect("sink init");

        sink.record_checked(&sample_event("2026-11-23T08:00:00Z", "before-err"))
            .expect("initial write");

        // Force a SQLITE_READONLY error on the cached connection to exercise invalidation.
        {
            let guard = sink.conn.lock().expect("lock conn");
            let conn = guard.as_ref().expect("cached conn present");
            conn.pragma_update(None, "query_only", true)
                .expect("set query_only");
        }

        let err = sink
            .record_checked(&sample_event("2026-11-23T08:01:00Z", "during-err"))
            .expect_err("readonly connection must fail");
        assert!(matches!(err, CoreError::UsageDatabase { .. }));
        assert!(
            sink.conn.lock().expect("lock conn").is_none(),
            "cached connection must be invalidated after fatal/readonly SQLite error"
        );

        // Next write must reopen, re-validate paths, and succeed cleanly.
        sink.record_checked(&sample_event("2026-11-23T08:02:00Z", "after-recover"))
            .expect("recovery write succeeds");
        let events = sink.read_all_events().expect("read recovered events");
        assert_eq!(events.len(), 2);
        assert_eq!(events[0].request_id, "before-err");
        assert_eq!(events[1].request_id, "after-recover");

        // Force a query error on the cached connection and verify read_all_events
        // invalidates the broken handle so a subsequent read recovers cleanly.
        {
            let mut guard = sink.conn.lock().expect("lock conn");
            *guard = Some(Connection::open_in_memory().expect("open empty in-memory db"));
        }
        let read_err = sink
            .read_all_events()
            .expect_err("query against broken cached connection must fail");
        assert!(matches!(read_err, CoreError::UsageDatabase { .. }));
        assert!(
            sink.conn.lock().expect("lock conn").is_none(),
            "cached connection must be invalidated after read_all_events error"
        );
        let recovered_events = sink
            .read_all_events()
            .expect("read recovers against on-disk usage.db after invalidation");
        assert_eq!(recovered_events.len(), 2);
    }

    #[cfg(unix)]
    #[test]
    fn sqlite_sink_rejects_symlink_db_and_sidecars_without_modifying_victim() {
        use std::os::unix::fs::{PermissionsExt, symlink};

        for file_name in [
            "usage.db",
            "usage.db-wal",
            "usage.db-shm",
            "usage.db-journal",
        ] {
            let tmp = TempDir::new().expect("temp dir");
            let usage_dir = tmp.path().join("usage");
            fs::create_dir(&usage_dir).expect("mkdir usage");

            let victim = tmp.path().join("victim.txt");
            fs::write(&victim, "DO_NOT_TOUCH\n").expect("write victim");
            fs::set_permissions(&victim, fs::Permissions::from_mode(0o644)).expect("chmod 0644");

            let link_path = usage_dir.join(file_name);
            symlink(&victim, &link_path).expect("symlink sqlite file");

            let sink = SqliteUsageSink::new(&usage_dir).expect("sink init");
            let write_err = sink
                .record_checked(&sample_event("2026-11-23T08:00:00Z", "sym-db"))
                .expect_err("symlinked sqlite file must fail write");
            assert!(
                matches!(write_err, CoreError::Fs(FsError::SymlinkRejected { .. })),
                "expected SymlinkRejected for symlinked {file_name}, got {write_err:?}"
            );

            let read_err = sink
                .read_all_events()
                .expect_err("symlinked sqlite file must fail read");
            assert!(
                matches!(read_err, CoreError::Fs(FsError::SymlinkRejected { .. })),
                "expected SymlinkRejected on read for symlinked {file_name}, got {read_err:?}"
            );

            assert_eq!(
                fs::read_to_string(&victim).expect("read victim"),
                "DO_NOT_TOUCH\n",
                "victim file contents must remain untouched for {file_name}"
            );
            let mode = fs::metadata(&victim)
                .expect("victim meta")
                .permissions()
                .mode()
                & 0o777;
            assert_eq!(
                mode, 0o644,
                "victim file permissions must remain 0644 for {file_name}"
            );
        }
    }

    #[cfg(unix)]
    #[test]
    fn sqlite_sink_rejects_symlink_usage_dir_and_fifos_without_blocking() {
        use std::os::unix::fs::symlink;

        let tmp = TempDir::new().expect("temp dir");

        // 1. Symlinked usage_dir must be rejected both on construction and on post-construction swap.
        let target_dir = tmp.path().join("target_dir");
        fs::create_dir(&target_dir).expect("mkdir target_dir");
        let sym_usage_dir = tmp.path().join("sym_usage");
        symlink(&target_dir, &sym_usage_dir).expect("symlink usage_dir");

        assert!(matches!(
            SqliteUsageSink::new(&sym_usage_dir),
            Err(CoreError::Fs(FsError::SymlinkRejected { .. }))
        ));

        let swapped_dir = tmp.path().join("swapped_usage");
        let swapped_sink = SqliteUsageSink::new(&swapped_dir).expect("missing usage_dir allowed");
        symlink(&target_dir, &swapped_dir).expect("swap usage_dir to symlink");
        let swap_err = swapped_sink
            .record_checked(&sample_event("2026-11-23T08:00:00Z", "swapped-dir"))
            .expect_err("post-construction symlinked usage_dir must fail");
        assert!(matches!(
            swap_err,
            CoreError::Fs(FsError::SymlinkRejected { .. })
        ));
        assert!(matches!(
            swapped_sink.read_all_events(),
            Err(CoreError::Fs(FsError::SymlinkRejected { .. }))
        ));
        assert!(
            fs::read_dir(&target_dir)
                .expect("read target_dir")
                .next()
                .is_none(),
            "target_dir must remain completely empty"
        );

        // 2. FIFO at usage.db and usage.db-wal must fail fast (< 1s) without blocking.
        for fifo_name in ["usage.db", "usage.db-wal"] {
            let fifo_usage_dir = tmp.path().join(format!("fifo_{fifo_name}"));
            fs::create_dir(&fifo_usage_dir).expect("mkdir fifo_usage");
            let fifo_path = fifo_usage_dir.join(fifo_name);
            let status = std::process::Command::new("mkfifo")
                .arg(&fifo_path)
                .status()
                .expect("run mkfifo");
            assert!(status.success(), "mkfifo failed for {fifo_name}");

            let fifo_sink = SqliteUsageSink::new(&fifo_usage_dir).expect("sink init");
            let (tx, rx) = std::sync::mpsc::channel();
            let handle = std::thread::spawn(move || {
                let res =
                    fifo_sink.record_checked(&sample_event("2026-11-23T08:00:00Z", "fifo-req"));
                let _ = tx.send(res);
            });
            let res = rx
                .recv_timeout(std::time::Duration::from_secs(1))
                .expect("record_checked hung on FIFO");
            handle.join().expect("thread join");
            let err = res.expect_err("FIFO sqlite path must fail");
            assert!(
                matches!(err, CoreError::Fs(FsError::NotRegularFile { .. })),
                "expected NotRegularFile for FIFO {fifo_name}, got {err:?}"
            );
        }
    }
}
