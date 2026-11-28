use std::fmt;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::Duration;

use rusqlite::{Connection, ErrorCode, OpenFlags, Row, Statement, TransactionBehavior, params};

use super::{
    LsSyncWatermark, ObservedModel, TelemetrySource, USAGE_DB_FILENAME, USAGE_DB_SCHEMA_VERSION,
    UsageEvent, UsageRange, UsageSink, UsageSummary, normalize_utc_timestamp,
    sanitize_conversation_id, sanitize_model_display_name, sanitize_response_id,
};
use crate::config::CoreError;
use crate::fs_safe::{self, FsError, OpenRegularMode};
use crate::models::lookup_static_placeholder;

pub(super) const SQLITE_BUSY_TIMEOUT_MS: u64 = 5_000;
const SQLITE_INIT_BUSY_RETRIES: u32 = 16;
const SIDECAR_TIGHTEN_MAX_ATTEMPTS: usize = 3;
const SIDECAR_TIGHTEN_RETRY_DELAY: Duration = Duration::from_millis(5);

const SCHEMA_V3_DDL: &str = "
CREATE TABLE IF NOT EXISTS usage_events (
    id                      INTEGER PRIMARY KEY AUTOINCREMENT,
    timestamp_utc           TEXT    NOT NULL,
    request_id              TEXT    NOT NULL UNIQUE,
    provider                TEXT    NOT NULL,
    model                   TEXT    NOT NULL,
    input_tokens            INTEGER NOT NULL CHECK (input_tokens >= 0),
    output_tokens           INTEGER NOT NULL CHECK (output_tokens >= 0),
    cached_tokens           INTEGER NOT NULL CHECK (cached_tokens >= 0),
    reasoning_tokens        INTEGER NOT NULL CHECK (reasoning_tokens >= 0),
    latency_ms              INTEGER NOT NULL CHECK (latency_ms >= 0),
    source                  TEXT    NOT NULL,
    response_id             TEXT,
    conversation_id         TEXT,
    root_conversation_id    TEXT,
    parent_conversation_id  TEXT,
    cache_write_tokens      INTEGER NOT NULL DEFAULT 0 CHECK (cache_write_tokens >= 0),
    ttft_ms                 INTEGER CHECK (ttft_ms IS NULL OR ttft_ms >= 0),
    model_display_name      TEXT,
    telemetry_source        TEXT    NOT NULL DEFAULT 'proxy'
                            CHECK (telemetry_source IN ('proxy', 'ls', 'reconciled')),
    cascade_step_index      INTEGER CHECK (cascade_step_index IS NULL OR cascade_step_index >= 0)
);

CREATE INDEX IF NOT EXISTS idx_usage_events_timestamp_utc
    ON usage_events (timestamp_utc);

CREATE INDEX IF NOT EXISTS idx_usage_events_provider_model
    ON usage_events (provider, model);

CREATE UNIQUE INDEX IF NOT EXISTS idx_usage_events_response_id
    ON usage_events (response_id)
    WHERE response_id IS NOT NULL;

CREATE INDEX IF NOT EXISTS idx_usage_events_conversation_ts
    ON usage_events (conversation_id, timestamp_utc DESC, id DESC)
    WHERE conversation_id IS NOT NULL;

CREATE INDEX IF NOT EXISTS idx_usage_events_root_conv_ts
    ON usage_events (root_conversation_id, timestamp_utc DESC, id DESC)
    WHERE root_conversation_id IS NOT NULL;

CREATE TABLE IF NOT EXISTS ls_sync_watermarks (
    cascade_id        TEXT    PRIMARY KEY,
    trajectory_id     TEXT    NOT NULL,
    step_offset       INTEGER NOT NULL CHECK (step_offset >= 0),
    last_modified_utc TEXT    NOT NULL,
    updated_at_utc    TEXT    NOT NULL
);

CREATE TABLE IF NOT EXISTS observed_models (
    provider                TEXT    NOT NULL,
    model                   TEXT    NOT NULL,
    compaction_limit_tokens INTEGER CHECK (compaction_limit_tokens IS NULL OR compaction_limit_tokens > 0),
    updated_at_utc          TEXT    NOT NULL,
    PRIMARY KEY (provider, model)
);

PRAGMA user_version = 3;
";

const MIGRATE_V1_TO_V2_DDL: &str = "
ALTER TABLE usage_events ADD COLUMN response_id TEXT;
ALTER TABLE usage_events ADD COLUMN conversation_id TEXT;
ALTER TABLE usage_events ADD COLUMN root_conversation_id TEXT;
ALTER TABLE usage_events ADD COLUMN parent_conversation_id TEXT;
ALTER TABLE usage_events ADD COLUMN cache_write_tokens INTEGER NOT NULL DEFAULT 0 CHECK (cache_write_tokens >= 0);
ALTER TABLE usage_events ADD COLUMN ttft_ms INTEGER CHECK (ttft_ms IS NULL OR ttft_ms >= 0);
ALTER TABLE usage_events ADD COLUMN model_display_name TEXT;
ALTER TABLE usage_events ADD COLUMN telemetry_source TEXT NOT NULL DEFAULT 'proxy' CHECK (telemetry_source IN ('proxy', 'ls', 'reconciled'));
ALTER TABLE usage_events ADD COLUMN cascade_step_index INTEGER CHECK (cascade_step_index IS NULL OR cascade_step_index >= 0);

CREATE UNIQUE INDEX IF NOT EXISTS idx_usage_events_response_id
    ON usage_events (response_id)
    WHERE response_id IS NOT NULL;

CREATE INDEX IF NOT EXISTS idx_usage_events_conversation_ts
    ON usage_events (conversation_id, timestamp_utc DESC, id DESC)
    WHERE conversation_id IS NOT NULL;

CREATE INDEX IF NOT EXISTS idx_usage_events_root_conv_ts
    ON usage_events (root_conversation_id, timestamp_utc DESC, id DESC)
    WHERE root_conversation_id IS NOT NULL;

CREATE TABLE IF NOT EXISTS ls_sync_watermarks (
    cascade_id        TEXT    PRIMARY KEY,
    trajectory_id     TEXT    NOT NULL,
    step_offset       INTEGER NOT NULL CHECK (step_offset >= 0),
    last_modified_utc TEXT    NOT NULL,
    updated_at_utc    TEXT    NOT NULL
);

PRAGMA user_version = 2;
";

const MIGRATE_V2_TO_V3_DDL: &str = "
CREATE TABLE IF NOT EXISTS observed_models (
    provider                TEXT    NOT NULL,
    model                   TEXT    NOT NULL,
    compaction_limit_tokens INTEGER CHECK (compaction_limit_tokens IS NULL OR compaction_limit_tokens > 0),
    updated_at_utc          TEXT    NOT NULL,
    PRIMARY KEY (provider, model)
);

PRAGMA user_version = 3;
";

const SCHEMA_MIGRATIONS: &[(u32, &str)] = &[(1, MIGRATE_V1_TO_V2_DDL), (2, MIGRATE_V2_TO_V3_DDL)];

const UPSERT_OBSERVED_MODEL_SQL: &str = "
INSERT INTO observed_models (
    provider, model, compaction_limit_tokens, updated_at_utc
) VALUES (?1, ?2, ?3, ?4)
ON CONFLICT(provider, model) DO UPDATE SET
    compaction_limit_tokens = excluded.compaction_limit_tokens,
    updated_at_utc = excluded.updated_at_utc
WHERE observed_models.compaction_limit_tokens IS NOT excluded.compaction_limit_tokens
";

const UPSERT_USAGE_EVENT_SQL: &str = "
INSERT INTO usage_events (
    timestamp_utc, request_id, provider, model,
    input_tokens, output_tokens, cached_tokens, reasoning_tokens,
    latency_ms, source,
    response_id, conversation_id, root_conversation_id, parent_conversation_id,
    cache_write_tokens, ttft_ms, model_display_name, telemetry_source, cascade_step_index
) VALUES (
    ?1, ?2, ?3, ?4,
    ?5, ?6, ?7, ?8,
    ?9, ?10,
    ?11, ?12, ?13, ?14,
    ?15, ?16, ?17, ?18, ?19
)
ON CONFLICT(request_id) DO NOTHING
ON CONFLICT(response_id) WHERE response_id IS NOT NULL DO UPDATE SET
    conversation_id = CASE
        WHEN usage_events.telemetry_source = 'proxy' AND excluded.telemetry_source = 'ls'
            THEN COALESCE(excluded.conversation_id, usage_events.conversation_id)
        ELSE COALESCE(usage_events.conversation_id, excluded.conversation_id)
    END,
    root_conversation_id = CASE
        WHEN usage_events.telemetry_source = 'proxy' AND excluded.telemetry_source = 'ls'
            THEN COALESCE(excluded.root_conversation_id, usage_events.root_conversation_id)
        WHEN usage_events.telemetry_source IN ('ls', 'reconciled')
            AND excluded.telemetry_source = 'ls'
            AND usage_events.conversation_id IS NOT NULL
            THEN usage_events.root_conversation_id
        ELSE COALESCE(usage_events.root_conversation_id, excluded.root_conversation_id)
    END,
    parent_conversation_id = CASE
        WHEN usage_events.telemetry_source = 'proxy' AND excluded.telemetry_source = 'ls'
            THEN COALESCE(excluded.parent_conversation_id, usage_events.parent_conversation_id)
        WHEN usage_events.telemetry_source IN ('ls', 'reconciled')
            AND excluded.telemetry_source = 'ls'
            AND usage_events.conversation_id IS NOT NULL
            THEN usage_events.parent_conversation_id
        ELSE COALESCE(usage_events.parent_conversation_id, excluded.parent_conversation_id)
    END,
    cascade_step_index = CASE
        WHEN usage_events.telemetry_source = 'proxy' AND excluded.telemetry_source = 'ls'
            THEN COALESCE(excluded.cascade_step_index, usage_events.cascade_step_index)
        WHEN usage_events.telemetry_source IN ('ls', 'reconciled')
            AND excluded.telemetry_source = 'ls'
            AND usage_events.conversation_id IS NOT NULL
            THEN usage_events.cascade_step_index
        ELSE COALESCE(usage_events.cascade_step_index, excluded.cascade_step_index)
    END,
    source = CASE
        WHEN usage_events.telemetry_source = 'ls' AND excluded.telemetry_source = 'proxy'
            THEN excluded.source
        ELSE usage_events.source
    END,
    provider = CASE
        WHEN usage_events.telemetry_source = 'ls' AND excluded.telemetry_source = 'proxy'
            AND excluded.source = 'proxy'
            THEN excluded.provider
        WHEN usage_events.telemetry_source = 'proxy' AND excluded.telemetry_source = 'ls'
            AND usage_events.source = 'builtin' AND excluded.provider != 'antigravity'
            THEN excluded.provider
        ELSE usage_events.provider
    END,
    latency_ms = CASE
        WHEN usage_events.telemetry_source = 'ls' AND excluded.telemetry_source = 'proxy' AND excluded.latency_ms > 0
            THEN excluded.latency_ms
        WHEN usage_events.latency_ms = 0 AND excluded.latency_ms > 0
            THEN excluded.latency_ms
        ELSE usage_events.latency_ms
    END,
    ttft_ms = COALESCE(usage_events.ttft_ms, excluded.ttft_ms),
    model_display_name = COALESCE(usage_events.model_display_name, excluded.model_display_name),
    cache_write_tokens = MAX(usage_events.cache_write_tokens, excluded.cache_write_tokens),
    telemetry_source = CASE
        WHEN usage_events.telemetry_source != excluded.telemetry_source
            THEN 'reconciled'
        ELSE usage_events.telemetry_source
    END
WHERE
    NOT (usage_events.telemetry_source IN ('ls', 'reconciled') AND excluded.telemetry_source = 'ls')
    OR usage_events.conversation_id IS NULL
    OR usage_events.ttft_ms IS NULL
    OR usage_events.model_display_name IS NULL
";

const UPSERT_WATERMARK_SQL: &str = "
INSERT INTO ls_sync_watermarks (
    cascade_id, trajectory_id, step_offset, last_modified_utc, updated_at_utc
) VALUES (?1, ?2, ?3, ?4, ?5)
ON CONFLICT(cascade_id) DO UPDATE SET
    step_offset = CASE
        WHEN ls_sync_watermarks.trajectory_id = excluded.trajectory_id
            THEN MAX(ls_sync_watermarks.step_offset, excluded.step_offset)
        ELSE excluded.step_offset
    END,
    trajectory_id = excluded.trajectory_id,
    last_modified_utc = MAX(ls_sync_watermarks.last_modified_utc, excluded.last_modified_utc),
    updated_at_utc = excluded.updated_at_utc
";

const SELECT_USAGE_EVENTS_V2_COLS: &str = "timestamp_utc, request_id, provider, model, \
     input_tokens, output_tokens, cached_tokens, reasoning_tokens, \
     latency_ms, source, \
     response_id, conversation_id, root_conversation_id, parent_conversation_id, \
     cache_write_tokens, ttft_ms, model_display_name, telemetry_source, cascade_step_index";

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

pub(super) fn sqlite_i64_to_u64(value: i64) -> u64 {
    u64::try_from(value).unwrap_or(0)
}

pub(super) fn db_error(db_path: &Path, err: impl fmt::Display) -> CoreError {
    CoreError::UsageDatabase {
        path: db_path.to_path_buf(),
        reason: err.to_string(),
    }
}

pub(super) fn sqlite_related_paths(db_path: &Path) -> [PathBuf; 4] {
    [
        db_path.to_path_buf(),
        fs_safe::append_os_suffix(db_path, "-wal"),
        fs_safe::append_os_suffix(db_path, "-shm"),
        fs_safe::append_os_suffix(db_path, "-journal"),
    ]
}

pub(super) fn validate_sqlite_paths(db_path: &Path) -> Result<Option<fs::Metadata>, CoreError> {
    let paths = sqlite_related_paths(db_path);
    let db_meta = fs_safe::inspect_regular_file_if_exists(&paths[0], None)?;
    for sidecar in &paths[1..] {
        let _ = fs_safe::inspect_regular_file_if_exists(sidecar, None)?;
    }
    Ok(db_meta)
}

pub(super) fn check_schema_version(db_path: &Path, version: u32) -> Result<(), CoreError> {
    if version > USAGE_DB_SCHEMA_VERSION {
        return Err(CoreError::UsageDatabase {
            path: db_path.to_path_buf(),
            reason: format!(
                "unsupported usage database schema version {version} (supported: {USAGE_DB_SCHEMA_VERSION})"
            ),
        });
    }
    Ok(())
}

pub(super) fn read_user_version_with_busy_retry(
    db_path: &Path,
    conn: &Connection,
) -> Result<u32, CoreError> {
    let mut last_busy = None;
    for attempt in 0..SQLITE_INIT_BUSY_RETRIES {
        match conn.pragma_query_value(None, "user_version", |row| row.get::<_, i64>(0)) {
            Ok(raw) => {
                return u32::try_from(raw).map_err(|_| {
                    db_error(
                        db_path,
                        format!("invalid negative or out-of-range user_version {raw}"),
                    )
                });
            }
            Err(err) if is_busy_or_locked(&err) => {
                last_busy = Some(err);
                std::thread::sleep(Duration::from_millis(5 * u64::from(attempt + 1)));
            }
            Err(err) => return Err(db_error(db_path, err)),
        }
    }
    Err(db_error(
        db_path,
        last_busy.expect("SQLITE_INIT_BUSY_RETRIES > 0 guarantees a busy error"),
    ))
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

fn tighten_sidecar_with_retry<F, S>(
    path: &Path,
    mut tighten_fn: F,
    mut sleep_fn: S,
    tolerate_exhausted_concurrent_mod: bool,
) -> Result<(), CoreError>
where
    F: FnMut(&Path) -> Result<(), FsError>,
    S: FnMut(Duration),
{
    for attempt in 1..=SIDECAR_TIGHTEN_MAX_ATTEMPTS {
        match tighten_fn(path) {
            Ok(()) => return Ok(()),
            Err(FsError::ConcurrentModification { .. })
                if attempt < SIDECAR_TIGHTEN_MAX_ATTEMPTS =>
            {
                sleep_fn(SIDECAR_TIGHTEN_RETRY_DELAY);
            }
            Err(FsError::ConcurrentModification { .. }) if tolerate_exhausted_concurrent_mod => {
                tracing::debug!(
                    path = %path.display(),
                    attempts = SIDECAR_TIGHTEN_MAX_ATTEMPTS,
                    "ephemeral SQLite sidecar rotated concurrently during post-commit permission tighten"
                );
                return Ok(());
            }
            Err(err) => return Err(err.into()),
        }
    }
    unreachable!("SIDECAR_TIGHTEN_MAX_ATTEMPTS >= 1 always returns inside loop")
}

fn usage_event_from_v2_row(row: &Row<'_>) -> rusqlite::Result<UsageEvent> {
    let raw_telemetry_source: String = row.get(17)?;
    let telemetry_source =
        TelemetrySource::parse(&raw_telemetry_source).unwrap_or(TelemetrySource::Proxy);
    let ttft_ms = row.get::<_, Option<i64>>(15)?.map(sqlite_i64_to_u64);
    let cascade_step_index = row
        .get::<_, Option<i64>>(18)?
        .and_then(|v| u32::try_from(v).ok());
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
        response_id: row.get(10)?,
        conversation_id: row.get(11)?,
        root_conversation_id: row.get(12)?,
        parent_conversation_id: row.get(13)?,
        cache_write_tokens: sqlite_i64_to_u64(row.get(14)?),
        ttft_ms,
        model_display_name: row.get(16)?,
        telemetry_source,
        cascade_step_index,
    })
}

fn execute_upsert_usage_event(
    stmt: &mut Statement<'_>,
    event: &UsageEvent,
) -> rusqlite::Result<usize> {
    let timestamp_utc = normalize_utc_timestamp(&event.timestamp_utc)
        .unwrap_or_else(|| event.timestamp_utc.clone());
    let response_id = event.response_id.as_deref().and_then(sanitize_response_id);
    let conversation_id = event
        .conversation_id
        .as_deref()
        .and_then(sanitize_conversation_id);
    let root_conversation_id = event
        .root_conversation_id
        .as_deref()
        .and_then(sanitize_conversation_id);
    let parent_conversation_id = event
        .parent_conversation_id
        .as_deref()
        .and_then(sanitize_conversation_id);
    let model_display_name = event
        .model_display_name
        .as_deref()
        .and_then(sanitize_model_display_name);
    let ttft_ms = event.ttft_ms.map(u64_to_sqlite_i64);
    let cascade_step_index = event.cascade_step_index.map(i64::from);

    stmt.execute(params![
        timestamp_utc,
        &event.request_id,
        &event.provider,
        &event.model,
        u64_to_sqlite_i64(event.input_tokens),
        u64_to_sqlite_i64(event.output_tokens),
        u64_to_sqlite_i64(event.cached_tokens),
        u64_to_sqlite_i64(event.reasoning_tokens),
        u64_to_sqlite_i64(event.latency_ms),
        &event.source,
        response_id,
        conversation_id,
        root_conversation_id,
        parent_conversation_id,
        u64_to_sqlite_i64(event.cache_write_tokens),
        ttft_ms,
        model_display_name,
        event.telemetry_source.as_str(),
        cascade_step_index,
    ])
}

fn execute_upsert_watermark(
    conn: &Connection,
    watermark: &LsSyncWatermark,
) -> rusqlite::Result<usize> {
    let last_modified_utc = normalize_utc_timestamp(&watermark.last_modified_utc)
        .unwrap_or_else(|| watermark.last_modified_utc.clone());
    let updated_at_utc = normalize_utc_timestamp(&watermark.updated_at_utc)
        .unwrap_or_else(|| watermark.updated_at_utc.clone());
    conn.execute(
        UPSERT_WATERMARK_SQL,
        params![
            &watermark.cascade_id,
            &watermark.trajectory_id,
            i64::from(watermark.step_offset),
            last_modified_utc,
            updated_at_utc,
        ],
    )
}

fn strip_tiered_historical_model_suffix(raw: &str) -> Option<String> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return None;
    }
    let lower = trimmed.to_ascii_lowercase();
    let without_effort = if lower.ends_with("-medium") {
        &trimmed[..trimmed.len() - 7]
    } else if lower.ends_with("-xhigh") {
        &trimmed[..trimmed.len() - 6]
    } else if lower.ends_with("-high") {
        &trimmed[..trimmed.len() - 5]
    } else if lower.ends_with("-low") || lower.ends_with("-max") {
        &trimmed[..trimmed.len() - 4]
    } else {
        trimmed
    };
    let lower_effort = without_effort.to_ascii_lowercase();
    let stripped = lower_effort
        .ends_with("-tiered")
        .then(|| without_effort[..without_effort.len() - 7].trim())
        .filter(|s| !s.is_empty())?;
    (stripped != trimmed).then(|| stripped.to_string())
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
        db_error(&self.db_path, err)
    }

    fn sqlite_related_paths(&self) -> [PathBuf; 4] {
        sqlite_related_paths(&self.db_path)
    }

    fn validate_sqlite_paths(&self) -> Result<Option<fs::Metadata>, CoreError> {
        validate_sqlite_paths(&self.db_path)
    }

    fn tighten_sqlite_file_permissions(&self, post_commit: bool) -> Result<(), CoreError> {
        let paths = self.sqlite_related_paths();
        // Keep `usage.db` (`paths[0]`) strict 1-shot fail-closed on any error.
        fs_safe::tighten_regular_file_mode_0600(&paths[0])?;
        // Ephemeral sidecars (`-wal`, `-shm`, `-journal`) can be unlinked/recreated by SQLite
        // across concurrent connections; retry up to 3 attempts ONLY on ConcurrentModification,
        // still failing closed immediately on SymlinkRejected / NotRegularFile / IO errors.
        for sidecar in &paths[1..] {
            tighten_sidecar_with_retry(
                sidecar,
                fs_safe::tighten_regular_file_mode_0600,
                std::thread::sleep,
                post_commit,
            )?;
        }
        Ok(())
    }

    fn check_schema_version(&self, version: u32) -> Result<(), CoreError> {
        check_schema_version(&self.db_path, version)
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
        read_user_version_with_busy_retry(&self.db_path, conn)
    }

    fn configure_and_migrate(&self, conn: &mut Connection) -> Result<usize, CoreError> {
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
        if version < USAGE_DB_SCHEMA_VERSION {
            let tx = conn
                .transaction_with_behavior(TransactionBehavior::Immediate)
                .map_err(|err| self.db_error(err))?;
            let mut tx_version = self.read_user_version(&tx)?;
            self.check_schema_version(tx_version)?;
            if tx_version == 0 {
                tx.execute_batch(SCHEMA_V3_DDL)
                    .map_err(|err| self.db_error(err))?;
            } else {
                while tx_version < USAGE_DB_SCHEMA_VERSION {
                    let Some((_, ddl)) = SCHEMA_MIGRATIONS
                        .iter()
                        .find(|(from_ver, _)| *from_ver == tx_version)
                    else {
                        return Err(self.db_error(format!(
                            "missing migration step for schema version {tx_version}"
                        )));
                    };
                    tx.execute_batch(ddl).map_err(|err| self.db_error(err))?;
                    let next_version = self.read_user_version(&tx)?;
                    self.check_schema_version(next_version)?;
                    if next_version <= tx_version {
                        return Err(self.db_error(format!(
                            "migration from schema version {tx_version} did not advance user_version (got {next_version})"
                        )));
                    }
                    tx_version = next_version;
                }
            }
            tx.commit().map_err(|err| self.db_error(err))?;
        }

        self.normalize_historical_rows_on_connection(conn)
    }

    fn normalize_historical_rows_on_connection(
        &self,
        conn: &mut Connection,
    ) -> Result<usize, CoreError> {
        let needs_work: bool = conn
            .query_row(
                "SELECT EXISTS(
                    SELECT 1 FROM usage_events
                    WHERE (
                        telemetry_source IN ('ls', 'reconciled')
                        AND provider = 'gemini'
                        AND (
                            LOWER(model) LIKE 'gpt-%'
                            OR LOWER(model) LIKE 'codex-%'
                            OR LOWER(model) LIKE 'companion/gpt-%'
                            OR LOWER(model) LIKE 'companion/codex-%'
                        )
                    )
                    OR model LIKE 'MODEL_PLACEHOLDER_M%'
                    OR (model != '' AND model NOT GLOB '*[^0-9]*')
                    OR LOWER(model) LIKE '%-tiered'
                    OR LOWER(model) LIKE '%-tiered-low'
                    OR LOWER(model) LIKE '%-tiered-medium'
                    OR LOWER(model) LIKE '%-tiered-high'
                    OR LOWER(model) LIKE '%-tiered-xhigh'
                    OR LOWER(model) LIKE '%-tiered-max'
                )",
                [],
                |row| row.get::<_, i64>(0),
            )
            .map(|v| v != 0)
            .map_err(|err| self.db_error(err))?;

        if !needs_work {
            return Ok(0);
        }

        let tx = conn
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(|err| self.db_error(err))?;

        let mut updated = tx
            .execute(
                "UPDATE usage_events
                 SET provider = 'openai-codex'
                 WHERE telemetry_source IN ('ls', 'reconciled')
                   AND provider = 'gemini'
                   AND (
                       LOWER(model) LIKE 'gpt-%'
                       OR LOWER(model) LIKE 'codex-%'
                       OR LOWER(model) LIKE 'companion/gpt-%'
                       OR LOWER(model) LIKE 'companion/codex-%'
                   )",
                [],
            )
            .map_err(|err| self.db_error(err))?;

        let placeholder_models: Vec<String> = {
            let mut stmt = tx
                .prepare(
                    "SELECT DISTINCT model FROM usage_events
                     WHERE model LIKE 'MODEL_PLACEHOLDER_M%'
                        OR (model != '' AND model NOT GLOB '*[^0-9]*')",
                )
                .map_err(|err| self.db_error(err))?;
            let rows = stmt
                .query_map([], |row| row.get::<_, String>(0))
                .map_err(|err| self.db_error(err))?;
            let mut list = Vec::new();
            for row in rows {
                list.push(row.map_err(|err| self.db_error(err))?);
            }
            list
        };

        if !placeholder_models.is_empty() {
            let mut update_stmt = tx
                .prepare(
                    "UPDATE usage_events
                     SET model = ?1,
                         model_display_name = COALESCE(model_display_name, ?2),
                         provider = CASE
                             WHEN provider = 'antigravity' AND ?3 != 'antigravity' THEN ?3
                             ELSE provider
                         END
                     WHERE model = ?4",
                )
                .map_err(|err| self.db_error(err))?;
            for raw_model in placeholder_models {
                if let Some(mapped) = lookup_static_placeholder(&raw_model) {
                    let count = update_stmt
                        .execute(params![
                            mapped.model_id,
                            mapped.display_name,
                            mapped.provider,
                            raw_model
                        ])
                        .map_err(|err| self.db_error(err))?;
                    updated = updated.saturating_add(count);
                }
            }
        }

        let tiered_models: Vec<String> = {
            let mut stmt = tx
                .prepare(
                    "SELECT DISTINCT model FROM usage_events
                     WHERE LOWER(model) LIKE '%-tiered'
                        OR LOWER(model) LIKE '%-tiered-low'
                        OR LOWER(model) LIKE '%-tiered-medium'
                        OR LOWER(model) LIKE '%-tiered-high'
                        OR LOWER(model) LIKE '%-tiered-xhigh'
                        OR LOWER(model) LIKE '%-tiered-max'",
                )
                .map_err(|err| self.db_error(err))?;
            let rows = stmt
                .query_map([], |row| row.get::<_, String>(0))
                .map_err(|err| self.db_error(err))?;
            let mut list = Vec::new();
            for row in rows {
                list.push(row.map_err(|err| self.db_error(err))?);
            }
            list
        };

        if !tiered_models.is_empty() {
            let mut update_tiered_stmt = tx
                .prepare("UPDATE usage_events SET model = ?1 WHERE model = ?2")
                .map_err(|err| self.db_error(err))?;
            for raw_model in tiered_models {
                if let Some(canonical) = strip_tiered_historical_model_suffix(&raw_model) {
                    let count = update_tiered_stmt
                        .execute(params![canonical, raw_model])
                        .map_err(|err| self.db_error(err))?;
                    updated = updated.saturating_add(count);
                }
            }
        }

        tx.commit().map_err(|err| self.db_error(err))?;
        Ok(updated)
    }

    pub fn normalize_historical_rows(&self) -> Result<usize, CoreError> {
        let mut conn_guard = self
            .conn
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());

        if let Some(ref mut conn) = *conn_guard {
            let updated = self.normalize_historical_rows_on_connection(conn)?;
            if updated > 0 {
                self.tighten_sqlite_file_permissions(true)?;
            }
            return Ok(updated);
        }

        if fs_safe::inspect_existing_dir(&self.usage_dir)?.is_none() {
            return Ok(0);
        }
        if self.validate_sqlite_paths()?.is_none() {
            return Ok(0);
        }

        let (conn, updated) = self.open_rw_connection_with_normalize_count()?;
        *conn_guard = Some(conn);
        if updated > 0 {
            self.tighten_sqlite_file_permissions(true)?;
        }
        Ok(updated)
    }

    fn open_rw_connection(&self) -> Result<Connection, CoreError> {
        self.open_rw_connection_with_normalize_count()
            .map(|(conn, _)| conn)
    }

    fn open_rw_connection_with_normalize_count(&self) -> Result<(Connection, usize), CoreError> {
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
        let updated = self.configure_and_migrate(&mut conn)?;
        self.tighten_sqlite_file_permissions(false)?;
        Ok((conn, updated))
    }

    fn query_all_from_connection_v1(
        &self,
        conn: &Connection,
    ) -> Result<Vec<UsageEvent>, CoreError> {
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
                    response_id: None,
                    conversation_id: None,
                    root_conversation_id: None,
                    parent_conversation_id: None,
                    cache_write_tokens: 0,
                    ttft_ms: None,
                    model_display_name: None,
                    telemetry_source: TelemetrySource::Proxy,
                    cascade_step_index: None,
                })
            })
            .map_err(|err| self.db_error(err))?;

        let mut events = Vec::new();
        for row in rows {
            events.push(row.map_err(|err| self.db_error(err))?);
        }
        Ok(events)
    }

    fn query_all_from_connection(&self, conn: &Connection) -> Result<Vec<UsageEvent>, CoreError> {
        let sql = format!("SELECT {SELECT_USAGE_EVENTS_V2_COLS} FROM usage_events ORDER BY id ASC");
        let mut stmt = conn.prepare(&sql).map_err(|err| self.db_error(err))?;
        let rows = stmt
            .query_map([], usage_event_from_v2_row)
            .map_err(|err| self.db_error(err))?;

        let mut events = Vec::new();
        for row in rows {
            events.push(row.map_err(|err| self.db_error(err))?);
        }
        Ok(events)
    }

    fn query_observed_models_from_connection(
        &self,
        conn: &Connection,
    ) -> Result<Vec<ObservedModel>, CoreError> {
        let mut stmt = conn
            .prepare(
                "SELECT provider, model, compaction_limit_tokens, updated_at_utc \
                 FROM observed_models \
                 ORDER BY provider ASC, model ASC",
            )
            .map_err(|err| self.db_error(err))?;
        let rows = stmt
            .query_map([], |row| {
                let raw_limit: Option<i64> = row.get(2)?;
                Ok(ObservedModel {
                    provider: row.get(0)?,
                    model: row.get(1)?,
                    compaction_limit_tokens: raw_limit.and_then(|v| u64::try_from(v).ok()),
                    updated_at_utc: row.get(3)?,
                })
            })
            .map_err(|err| self.db_error(err))?;

        let mut models = Vec::new();
        for row in rows {
            models.push(row.map_err(|err| self.db_error(err))?);
        }
        Ok(models)
    }

    fn query_watermarks_from_connection(
        &self,
        conn: &Connection,
    ) -> Result<Vec<LsSyncWatermark>, CoreError> {
        let mut stmt = conn
            .prepare(
                "SELECT cascade_id, trajectory_id, step_offset, last_modified_utc, updated_at_utc \
                 FROM ls_sync_watermarks \
                 ORDER BY cascade_id ASC",
            )
            .map_err(|err| self.db_error(err))?;
        let rows = stmt
            .query_map([], |row| {
                let raw_offset: i64 = row.get(2)?;
                Ok(LsSyncWatermark {
                    cascade_id: row.get(0)?,
                    trajectory_id: row.get(1)?,
                    step_offset: u32::try_from(raw_offset).unwrap_or(0),
                    last_modified_utc: row.get(3)?,
                    updated_at_utc: row.get(4)?,
                })
            })
            .map_err(|err| self.db_error(err))?;

        let mut watermarks = Vec::new();
        for row in rows {
            watermarks.push(row.map_err(|err| self.db_error(err))?);
        }
        Ok(watermarks)
    }

    fn query_conversation_events_from_connection(
        &self,
        conn: &Connection,
        conversation_id: &str,
        limit: usize,
    ) -> Result<Vec<UsageEvent>, CoreError> {
        let sql = format!(
            "SELECT {SELECT_USAGE_EVENTS_V2_COLS} \
             FROM usage_events \
             WHERE root_conversation_id = ?1 OR conversation_id = ?1 \
             ORDER BY timestamp_utc DESC, id DESC \
             LIMIT ?2"
        );
        let mut stmt = conn.prepare(&sql).map_err(|err| self.db_error(err))?;
        let limit_i64 = i64::try_from(limit).unwrap_or(i64::MAX);
        let rows = stmt
            .query_map(params![conversation_id, limit_i64], usage_event_from_v2_row)
            .map_err(|err| self.db_error(err))?;

        let mut events = Vec::new();
        for row in rows {
            events.push(row.map_err(|err| self.db_error(err))?);
        }
        // Return in chronological order (oldest -> newest) for deterministic session folding.
        events.reverse();
        Ok(events)
    }

    /// Upserts a batch of usage events and an optional LS sync watermark inside a single
    /// `BEGIN IMMEDIATE` transaction, returning the total number of event rows inserted or updated.
    pub fn record_batch_checked(
        &self,
        events: &[UsageEvent],
        watermark: Option<&LsSyncWatermark>,
    ) -> Result<usize, CoreError> {
        if events.is_empty() && watermark.is_none() {
            return Ok(0);
        }

        let mut conn_guard = self
            .conn
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());

        if conn_guard.is_none() {
            *conn_guard = Some(self.open_rw_connection()?);
        }

        let tx_res: rusqlite::Result<usize> = {
            let conn = conn_guard
                .as_mut()
                .expect("connection initialized immediately above");
            (|| {
                let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
                let mut affected = 0usize;
                if !events.is_empty() {
                    let mut stmt = tx.prepare(UPSERT_USAGE_EVENT_SQL)?;
                    for event in events {
                        affected =
                            affected.saturating_add(execute_upsert_usage_event(&mut stmt, event)?);
                    }
                }
                if let Some(wm) = watermark {
                    let _ = execute_upsert_watermark(&tx, wm)?;
                }
                tx.commit()?;
                Ok(affected)
            })()
        };

        let affected = match tx_res {
            Ok(count) => count,
            Err(err) => {
                if should_invalidate_connection(&err) {
                    *conn_guard = None;
                }
                return Err(self.db_error(err));
            }
        };

        if let Err(err) = self.tighten_sqlite_file_permissions(true) {
            *conn_guard = None;
            return Err(err);
        }

        Ok(affected)
    }

    pub fn record_checked(&self, event: &UsageEvent) -> Result<PathBuf, CoreError> {
        let _ = self.record_batch_checked(std::slice::from_ref(event), None)?;
        Ok(self.db_path.clone())
    }

    pub fn upsert_watermark(&self, watermark: &LsSyncWatermark) -> Result<(), CoreError> {
        let _ = self.record_batch_checked(&[], Some(watermark))?;
        Ok(())
    }

    pub fn load_all_watermarks(&self) -> Result<Vec<LsSyncWatermark>, CoreError> {
        let mut conn_guard = self
            .conn
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());

        if let Some(ref conn) = *conn_guard {
            return match self.query_watermarks_from_connection(conn) {
                Ok(watermarks) => Ok(watermarks),
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

        let version = self.read_user_version_with_busy_retry(&conn)?;
        self.check_schema_version(version)?;
        if version < 2 {
            return Ok(Vec::new());
        }
        let watermarks = self.query_watermarks_from_connection(&conn)?;
        drop(conn);
        self.tighten_sqlite_file_permissions(true)?;
        Ok(watermarks)
    }

    /// Loads all persisted `observed_models` rows ordered by `(provider ASC, model ASC)`.
    ///
    /// Lazy read-only: returns `Ok(Vec::new())` without creating files when `usage_dir` or
    /// `usage.db` does not exist or when `PRAGMA user_version < 3`. Fails closed on
    /// `user_version > USAGE_DB_SCHEMA_VERSION`, negative `user_version`, or symlink/FIFO paths.
    pub fn load_all_observed_models(&self) -> Result<Vec<ObservedModel>, CoreError> {
        let mut conn_guard = self
            .conn
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());

        if let Some(ref conn) = *conn_guard {
            return match self.query_observed_models_from_connection(conn) {
                Ok(models) => Ok(models),
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

        let version = self.read_user_version_with_busy_retry(&conn)?;
        self.check_schema_version(version)?;
        if version < 3 {
            return Ok(Vec::new());
        }
        let models = self.query_observed_models_from_connection(&conn)?;
        drop(conn);
        self.tighten_sqlite_file_permissions(true)?;
        Ok(models)
    }

    /// Inserts or updates an `observed_models` row only when the `(provider, model)` entry is
    /// new or its `compaction_limit_tokens` value changed (NULL-safe `IS NOT` comparison).
    ///
    /// Returns `Ok(true)` if a row was inserted or updated, `Ok(false)` if unchanged (no-op).
    /// Rejects `compaction_limit_tokens == Some(0)` via SQLite CHECK constraint without
    /// invalidating the cached connection.
    pub fn upsert_observed_model(&self, observed: &ObservedModel) -> Result<bool, CoreError> {
        let updated_at = normalize_utc_timestamp(&observed.updated_at_utc)
            .unwrap_or_else(|| observed.updated_at_utc.clone());
        let compaction_limit = observed.compaction_limit_tokens.map(u64_to_sqlite_i64);

        let mut conn_guard = self
            .conn
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());

        if conn_guard.is_none() {
            *conn_guard = Some(self.open_rw_connection()?);
        }

        let tx_res: rusqlite::Result<usize> = {
            let conn = conn_guard
                .as_mut()
                .expect("connection initialized immediately above");
            (|| {
                let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
                let affected = tx.execute(
                    UPSERT_OBSERVED_MODEL_SQL,
                    params![
                        observed.provider,
                        observed.model,
                        compaction_limit,
                        updated_at,
                    ],
                )?;
                tx.commit()?;
                Ok(affected)
            })()
        };

        let affected = match tx_res {
            Ok(count) => count,
            Err(err) => {
                if should_invalidate_connection(&err) {
                    *conn_guard = None;
                }
                return Err(self.db_error(err));
            }
        };

        if let Err(err) = self.tighten_sqlite_file_permissions(true) {
            *conn_guard = None;
            return Err(err);
        }

        Ok(affected > 0)
    }

    pub fn prune_stale_watermarks(&self, cutoff_utc: &str) -> Result<usize, CoreError> {
        let cutoff = normalize_utc_timestamp(cutoff_utc).unwrap_or_else(|| cutoff_utc.to_string());

        let mut conn_guard = self
            .conn
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());

        if conn_guard.is_none() {
            if fs_safe::inspect_existing_dir(&self.usage_dir)?.is_none() {
                return Ok(0);
            }
            if self.validate_sqlite_paths()?.is_none() {
                return Ok(0);
            }
            *conn_guard = Some(self.open_rw_connection()?);
        }

        let delete_res: rusqlite::Result<usize> = {
            let conn = conn_guard
                .as_mut()
                .expect("connection initialized immediately above");
            (|| {
                let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
                let deleted = tx.execute(
                    "DELETE FROM ls_sync_watermarks WHERE updated_at_utc < ?1",
                    params![cutoff],
                )?;
                tx.commit()?;
                Ok(deleted)
            })()
        };

        let deleted = match delete_res {
            Ok(n) => n,
            Err(err) => {
                if should_invalidate_connection(&err) {
                    *conn_guard = None;
                }
                return Err(self.db_error(err));
            }
        };

        if let Err(err) = self.tighten_sqlite_file_permissions(true) {
            *conn_guard = None;
            return Err(err);
        }

        Ok(deleted)
    }

    /// Returns up to `limit` most recent usage events belonging to `conversation_id` (matching
    /// either `root_conversation_id` or `conversation_id`), ordered chronologically (oldest first).
    pub fn query_events_for_root_or_conversation(
        &self,
        conversation_id: &str,
        limit: usize,
    ) -> Result<Vec<UsageEvent>, CoreError> {
        let Some(sanitized_id) = sanitize_conversation_id(conversation_id) else {
            return Ok(Vec::new());
        };
        if limit == 0 {
            return Ok(Vec::new());
        }

        let mut conn_guard = self
            .conn
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());

        if let Some(ref conn) = *conn_guard {
            return match self.query_conversation_events_from_connection(conn, &sanitized_id, limit)
            {
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

        let version = self.read_user_version_with_busy_retry(&conn)?;
        self.check_schema_version(version)?;
        if version < 2 {
            return Ok(Vec::new());
        }
        let events = self.query_conversation_events_from_connection(&conn, &sanitized_id, limit)?;
        drop(conn);
        self.tighten_sqlite_file_permissions(true)?;
        Ok(events)
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

        let version = self.read_user_version_with_busy_retry(&conn)?;
        self.check_schema_version(version)?;
        let events = if version == 0 {
            Vec::new()
        } else if version == 1 {
            self.query_all_from_connection_v1(&conn)?
        } else {
            self.query_all_from_connection(&conn)?
        };
        drop(conn);
        self.tighten_sqlite_file_permissions(true)?;
        Ok(events)
    }

    pub fn query_usage_summary(&self, range: UsageRange) -> Result<UsageSummary, CoreError> {
        super::query::query_usage_summary(&self.usage_dir, range)
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
        assert!(!contains_bytes(&raw_bytes, b"systemPrompt"));
        assert!(!contains_bytes(&raw_bytes, b"api_key"));

        // Verify PRAGMA user_version, WAL mode, and v3 schema columns directly.
        let conn = Connection::open(sink.db_path()).expect("open db");
        let version: u32 = conn
            .pragma_query_value(None, "user_version", |row| row.get(0))
            .expect("user_version");
        assert_eq!(version, USAGE_DB_SCHEMA_VERSION);
        assert_eq!(version, 3);
        let journal_mode: String = conn
            .pragma_query_value(None, "journal_mode", |row| row.get(0))
            .expect("journal_mode");
        assert_eq!(journal_mode.to_ascii_lowercase(), "wal");

        let mut observed_stmt = conn
            .prepare("PRAGMA table_info(observed_models)")
            .expect("observed_models table_info");
        let observed_cols: Vec<String> = observed_stmt
            .query_map([], |row| row.get(1))
            .expect("query observed_models columns")
            .map(|r| r.expect("col name"))
            .collect();
        assert_eq!(
            observed_cols,
            vec![
                "provider",
                "model",
                "compaction_limit_tokens",
                "updated_at_utc",
            ]
        );

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
                "response_id",
                "conversation_id",
                "root_conversation_id",
                "parent_conversation_id",
                "cache_write_tokens",
                "ttft_ms",
                "model_display_name",
                "telemetry_source",
                "cascade_step_index",
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
            (4_i64, "schema version 4"),
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

            let observed_err = sink
                .load_all_observed_models()
                .expect_err("unsupported schema version must fail observed_models read");
            assert!(
                matches!(observed_err, CoreError::UsageDatabase { .. }),
                "expected UsageDatabase error on observed_models read, got {observed_err:?}"
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
            response_id: None,
            conversation_id: None,
            root_conversation_id: None,
            parent_conversation_id: None,
            cache_write_tokens: u64::MAX,
            ttft_ms: Some(u64::MAX),
            model_display_name: None,
            telemetry_source: TelemetrySource::Proxy,
            cascade_step_index: Some(42),
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

        // Duplicate request_id is absorbed idempotently via ON CONFLICT(request_id) DO NOTHING.
        let dup_changes = sink
            .record_batch_checked(&[sample_event("2026-11-22T22:00:00Z", "max-u64")], None)
            .expect("duplicate request_id absorbed cleanly");
        assert_eq!(dup_changes, 0);
        sink.record(sample_event("2026-11-22T22:00:00Z", "max-u64"));

        // Healthy connection remains usable after an absorbed duplicate.
        sink.record_checked(&sample_event("2026-11-22T22:01:00Z", "after-dup"))
            .expect("subsequent unique write succeeds");

        let events = sink.read_all_events().expect("read events");
        assert_eq!(events.len(), 5);
        assert_eq!(events[0].input_tokens, i64::MAX as u64);
        assert_eq!(events[0].output_tokens, i64::MAX as u64);
        assert_eq!(events[0].cached_tokens, i64::MAX as u64);
        assert_eq!(events[0].reasoning_tokens, i64::MAX as u64);
        assert_eq!(events[0].latency_ms, i64::MAX as u64);
        assert_eq!(events[0].cache_write_tokens, i64::MAX as u64);
        assert_eq!(events[0].ttft_ms, Some(i64::MAX as u64));
        assert_eq!(events[0].cascade_step_index, Some(42));
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

    #[test]
    fn sqlite_v1_to_v2_in_place_migration_preserves_rows_and_adds_v2_schema() {
        let tmp = TempDir::new().expect("temp dir");
        let db_path = tmp.path().join(USAGE_DB_FILENAME);

        // Seed an authentic schema v1 database on disk.
        {
            let conn = Connection::open(&db_path).expect("open v1 db");
            conn.execute_batch(
                "CREATE TABLE usage_events (
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
                CREATE INDEX idx_usage_events_timestamp_utc ON usage_events (timestamp_utc);
                CREATE INDEX idx_usage_events_provider_model ON usage_events (provider, model);
                INSERT INTO usage_events (
                    timestamp_utc, request_id, provider, model,
                    input_tokens, output_tokens, cached_tokens, reasoning_tokens,
                    latency_ms, source
                ) VALUES (
                    '2026-11-20T10:00:00Z', 'legacy-v1-req', 'gemini', 'gemini-4-carbon',
                    500, 120, 300, 40, 250, 'proxy'
                );
                PRAGMA user_version = 1;",
            )
            .expect("seed v1 schema");
        }

        let sink = SqliteUsageSink::new(tmp.path()).expect("sink init");

        // Read-only access against an unmigrated v1 DB works cleanly without writing.
        let before_events = sink.read_all_events().expect("read v1 events");
        assert_eq!(before_events.len(), 1);
        assert_eq!(before_events[0].request_id, "legacy-v1-req");
        assert_eq!(before_events[0].telemetry_source, TelemetrySource::Proxy);
        assert_eq!(before_events[0].cache_write_tokens, 0);
        assert_eq!(before_events[0].response_id, None);
        assert!(
            sink.load_all_watermarks()
                .expect("watermarks on v1")
                .is_empty()
        );
        assert!(
            sink.load_all_observed_models()
                .expect("observed_models on v1")
                .is_empty()
        );

        // First write triggers atomic v1 -> v2 -> v3 sequential migration in-place.
        let mut v2_ev = sample_event("2026-11-21T12:00:00.500Z", "ls:conv_abc123:1:0");
        v2_ev.response_id = Some("resp-v2-001".to_string());
        v2_ev.conversation_id = Some("conv_abc123".to_string());
        v2_ev.root_conversation_id = Some("conv_abc123".to_string());
        v2_ev.cache_write_tokens = 64;
        v2_ev.ttft_ms = Some(180);
        v2_ev.model_display_name = Some("Gemini 4 Hydrogen".to_string());
        v2_ev.telemetry_source = TelemetrySource::Ls;
        v2_ev.cascade_step_index = Some(1);
        sink.record_checked(&v2_ev).expect("migrate and write v3");

        let conn = Connection::open(&db_path).expect("open migrated db");
        let version: u32 = conn
            .pragma_query_value(None, "user_version", |r| r.get(0))
            .expect("user_version");
        assert_eq!(version, 3);

        let all = sink.read_all_events().expect("read migrated events");
        assert_eq!(all.len(), 2);
        assert_eq!(all[0].request_id, "legacy-v1-req");
        assert_eq!(all[0].input_tokens, 500);
        assert_eq!(all[0].telemetry_source, TelemetrySource::Proxy);
        assert_eq!(all[0].cache_write_tokens, 0);
        assert_eq!(all[0].response_id, None);

        assert_eq!(all[1].request_id, "ls:conv_abc123:1:0");
        assert_eq!(all[1].timestamp_utc, "2026-11-21T12:00:00Z");
        assert_eq!(all[1].response_id.as_deref(), Some("resp-v2-001"));
        assert_eq!(all[1].conversation_id.as_deref(), Some("conv_abc123"));
        assert_eq!(all[1].cache_write_tokens, 64);
        assert_eq!(all[1].ttft_ms, Some(180));
        assert_eq!(
            all[1].model_display_name.as_deref(),
            Some("Gemini 4 Hydrogen")
        );
        assert_eq!(all[1].telemetry_source, TelemetrySource::Ls);
        assert_eq!(all[1].cascade_step_index, Some(1));
    }

    #[test]
    fn sqlite_multi_clause_reconciliation_handles_all_four_cases_and_side_question_immunity() {
        let tmp = TempDir::new().expect("temp dir");
        let sink = SqliteUsageSink::new(tmp.path()).expect("sink init");

        // Case 1: SIDE_QUESTION clone must NEVER overwrite original LS conversation_id/root/parent
        let mut orig_ls = sample_event("2026-11-22T10:00:00Z", "ls:a25ce165:10:0");
        orig_ls.provider = "antigravity".to_string();
        orig_ls.source = "builtin".to_string();
        orig_ls.response_id = Some("AJ8Ia-sideq-1".to_string());
        orig_ls.conversation_id = Some("a25ce165".to_string());
        orig_ls.root_conversation_id = Some("a25ce165".to_string());
        orig_ls.parent_conversation_id = None;
        orig_ls.ttft_ms = None; // Exercise WHERE branch even when ttft_ms was initially NULL
        orig_ls.model_display_name = Some("Gemini 3.8 Pro".to_string());
        orig_ls.telemetry_source = TelemetrySource::Ls;
        orig_ls.cascade_step_index = Some(10);

        let mut side_q = orig_ls.clone();
        side_q.request_id = "ls:4fd3c59a:10:0".to_string();
        side_q.conversation_id = Some("4fd3c59a".to_string());
        side_q.root_conversation_id = Some("4fd3c59a".to_string());
        side_q.parent_conversation_id = Some("a25ce165".to_string());
        side_q.ttft_ms = Some(410);
        side_q.cascade_step_index = Some(99);

        sink.record_checked(&orig_ls).expect("insert orig_ls");
        sink.record_checked(&side_q).expect("side_question upsert");

        // Second SIDE_QUESTION after ttft_ms & model_display_name are populated must be a 0-row no-op.
        let mut side_q2 = side_q.clone();
        side_q2.request_id = "ls:9999aaaa:10:0".to_string();
        side_q2.conversation_id = Some("9999aaaa".to_string());
        let changes_noop = sink
            .record_batch_checked(&[side_q2], None)
            .expect("side_question noop");
        assert_eq!(changes_noop, 0);

        let events_c1 = sink.read_all_events().expect("read c1");
        assert_eq!(events_c1.len(), 1);
        assert_eq!(events_c1[0].conversation_id.as_deref(), Some("a25ce165"));
        assert_eq!(
            events_c1[0].root_conversation_id.as_deref(),
            Some("a25ce165")
        );
        assert_eq!(events_c1[0].parent_conversation_id, None);
        assert_eq!(events_c1[0].cascade_step_index, Some(10));
        assert_eq!(events_c1[0].telemetry_source, TelemetrySource::Ls);

        // Case 2A (Proxy first -> LS second) vs Case 2B (LS first -> Proxy second): commutative!
        let make_proxy_and_ls = |resp_id: &str, conv: &str| {
            let mut proxy_ev = sample_event("2026-11-22T11:00:00Z", &format!("proxy:{resp_id}"));
            proxy_ev.provider = "gemini".to_string();
            proxy_ev.source = "proxy".to_string();
            proxy_ev.latency_ms = 2796;
            proxy_ev.response_id = Some(resp_id.to_string());
            proxy_ev.conversation_id = None;
            proxy_ev.cache_write_tokens = 10;
            proxy_ev.telemetry_source = TelemetrySource::Proxy;

            let mut ls_ev = sample_event("2026-11-22T11:00:01Z", &format!("ls:{conv}:4:0"));
            ls_ev.provider = "antigravity".to_string();
            ls_ev.source = "builtin".to_string();
            ls_ev.latency_ms = 2900;
            ls_ev.response_id = Some(resp_id.to_string());
            ls_ev.conversation_id = Some(conv.to_string());
            ls_ev.root_conversation_id = Some("root_conv_001".to_string());
            ls_ev.parent_conversation_id = Some("root_conv_001".to_string());
            ls_ev.cache_write_tokens = 250;
            ls_ev.ttft_ms = Some(410);
            ls_ev.model_display_name = Some("Gemini 4 Hydrogen".to_string());
            ls_ev.telemetry_source = TelemetrySource::Ls;
            ls_ev.cascade_step_index = Some(4);
            (proxy_ev, ls_ev)
        };

        let (p_2a, ls_2a) = make_proxy_and_ls("G4EIa-comm-2a", "sub_conv_2a");
        sink.record_checked(&p_2a).expect("2a proxy");
        sink.record_checked(&ls_2a).expect("2a ls");

        let (p_2b, ls_2b) = make_proxy_and_ls("G4EIa-comm-2b", "sub_conv_2b");
        sink.record_checked(&ls_2b).expect("2b ls");
        sink.record_checked(&p_2b).expect("2b proxy");

        // Case 2C (Proxy->LS) vs Case 2D (LS->Proxy): CloudCode builtin teed by proxy
        // (`from_antigravity_builtin`) reconciled with LS Vertex Claude attribution must preserve
        // `source = "builtin"` and `provider = "vertex_claude"` commutatively in both orders.
        let make_builtin_proxy_and_vertex_ls = |resp_id: &str, conv: &str| {
            let builtin_usage = super::super::GeminiUsageMetadata {
                prompt_token_count: 1500,
                candidates_token_count: 420,
                cached_content_token_count: 800,
                thoughts_token_count: 90,
            };
            let mut proxy_builtin =
                UsageEvent::from_antigravity_builtin("claude-sonnet-4-6", &builtin_usage, 1850)
                    .with_response_id(Some(resp_id.to_string()));
            proxy_builtin.timestamp_utc = "2026-11-22T11:30:00Z".to_string();
            proxy_builtin.request_id = format!("proxy:builtin-{resp_id}");

            let mut ls_vertex = sample_event("2026-11-22T11:30:01Z", &format!("ls:{conv}:6:0"));
            ls_vertex.provider = "vertex_claude".to_string();
            ls_vertex.source = "builtin".to_string();
            ls_vertex.model = "claude-sonnet-4-6".to_string();
            ls_vertex.latency_ms = 2100;
            ls_vertex.response_id = Some(resp_id.to_string());
            ls_vertex.conversation_id = Some(conv.to_string());
            ls_vertex.root_conversation_id = Some("root_conv_vertex".to_string());
            ls_vertex.parent_conversation_id = Some("root_conv_vertex".to_string());
            ls_vertex.cache_write_tokens = 320;
            ls_vertex.ttft_ms = Some(290);
            ls_vertex.model_display_name = Some("Claude Sonnet 4.6".to_string());
            ls_vertex.telemetry_source = TelemetrySource::Ls;
            ls_vertex.cascade_step_index = Some(6);
            (proxy_builtin, ls_vertex)
        };

        let (bp_2c, bls_2c) = make_builtin_proxy_and_vertex_ls("V4CL-builtin-2c", "sub_conv_2c");
        sink.record_checked(&bp_2c).expect("2c builtin proxy");
        sink.record_checked(&bls_2c).expect("2c vertex ls");

        let (bp_2d, bls_2d) = make_builtin_proxy_and_vertex_ls("V4CL-builtin-2d", "sub_conv_2d");
        sink.record_checked(&bls_2d).expect("2d vertex ls");
        sink.record_checked(&bp_2d).expect("2d builtin proxy");

        // Case 3: NULL / empty response_id events never collide
        let mut null_a = sample_event("2026-11-22T12:00:00Z", "proxy:null-a");
        null_a.response_id = Some("   ".to_string());
        let mut null_b = sample_event("2026-11-22T12:00:01Z", "ls:conv_null:1:0");
        null_b.response_id = None;
        null_b.telemetry_source = TelemetrySource::Ls;
        sink.record_batch_checked(&[null_a, null_b], None)
            .expect("null response_id batch");

        let all = sink.read_all_events().expect("read all");
        assert_eq!(all.len(), 7);

        let r2a = all
            .iter()
            .find(|e| e.response_id.as_deref() == Some("G4EIa-comm-2a"))
            .expect("2a row");
        let r2b = all
            .iter()
            .find(|e| e.response_id.as_deref() == Some("G4EIa-comm-2b"))
            .expect("2b row");

        for (row, expected_conv) in [(r2a, "sub_conv_2a"), (r2b, "sub_conv_2b")] {
            assert_eq!(row.telemetry_source, TelemetrySource::Reconciled);
            assert_eq!(row.source, "proxy");
            assert_eq!(row.provider, "gemini");
            assert_eq!(row.latency_ms, 2796);
            assert_eq!(row.conversation_id.as_deref(), Some(expected_conv));
            assert_eq!(row.root_conversation_id.as_deref(), Some("root_conv_001"));
            assert_eq!(row.parent_conversation_id.as_deref(), Some("root_conv_001"));
            assert_eq!(row.cascade_step_index, Some(4));
            assert_eq!(row.ttft_ms, Some(410));
            assert_eq!(row.model_display_name.as_deref(), Some("Gemini 4 Hydrogen"));
            assert_eq!(row.cache_write_tokens, 250);
        }

        let r2c = all
            .iter()
            .find(|e| e.response_id.as_deref() == Some("V4CL-builtin-2c"))
            .expect("2c builtin row");
        let r2d = all
            .iter()
            .find(|e| e.response_id.as_deref() == Some("V4CL-builtin-2d"))
            .expect("2d builtin row");

        for (row, expected_conv) in [(r2c, "sub_conv_2c"), (r2d, "sub_conv_2d")] {
            assert_eq!(row.telemetry_source, TelemetrySource::Reconciled);
            assert_eq!(row.source, "builtin");
            assert_eq!(row.provider, "vertex_claude");
            assert_eq!(row.latency_ms, 1850);
            assert_eq!(row.conversation_id.as_deref(), Some(expected_conv));
            assert_eq!(
                row.root_conversation_id.as_deref(),
                Some("root_conv_vertex")
            );
            assert_eq!(
                row.parent_conversation_id.as_deref(),
                Some("root_conv_vertex")
            );
            assert_eq!(row.cascade_step_index, Some(6));
            assert_eq!(row.ttft_ms, Some(290));
            assert_eq!(row.model_display_name.as_deref(), Some("Claude Sonnet 4.6"));
            assert_eq!(row.cache_write_tokens, 320);
        }

        // Case 4: 4 concurrent multi-instance writers racing on overlapping request_id & response_id
        let race_dir = tmp.path().join("race_reconcile");
        let mut handles = Vec::new();
        for w in 0..4u32 {
            let dir = race_dir.clone();
            handles.push(std::thread::spawn(move || {
                let writer = SqliteUsageSink::new(&dir).expect("race writer");
                let mut batch = Vec::new();
                for i in 0..8u32 {
                    let mut ev =
                        sample_event("2026-11-22T13:00:00Z", &format!("ls:race_conv:{i}:0"));
                    ev.response_id = Some(format!("race-resp-{i}"));
                    ev.conversation_id = Some("race_conv".to_string());
                    ev.telemetry_source = TelemetrySource::Ls;
                    ev.cascade_step_index = Some(i);
                    batch.push(ev);
                }
                // Also race a proxy row for every even response_id
                let mut proxy_ev =
                    sample_event("2026-11-22T13:00:00Z", &format!("proxy:w{w}-even-0"));
                proxy_ev.response_id = Some("race-resp-0".to_string());
                proxy_ev.latency_ms = 555;
                proxy_ev.telemetry_source = TelemetrySource::Proxy;
                batch.push(proxy_ev);

                writer
                    .record_batch_checked(&batch, None)
                    .expect("concurrent batch upsert must never fail with UNIQUE");
            }));
        }
        for handle in handles {
            handle.join().expect("join race thread");
        }

        let race_reader = SqliteUsageSink::new(&race_dir).expect("race reader");
        let race_events = race_reader.read_all_events().expect("read race events");
        assert_eq!(race_events.len(), 8);
        let reconciled_zero = race_events
            .iter()
            .find(|e| e.response_id.as_deref() == Some("race-resp-0"))
            .expect("race-resp-0");
        assert_eq!(
            reconciled_zero.telemetry_source,
            TelemetrySource::Reconciled
        );
        assert_eq!(reconciled_zero.source, "proxy");
        assert_eq!(reconciled_zero.latency_ms, 555);
    }

    #[test]
    fn sqlite_sidecar_tighten_retries_concurrent_modification_and_rejects_symlinks_immediately() {
        let dummy_path = Path::new("/tmp/usage.db-wal");

        // 1. Succeeds on attempt 1 -> zero sleeps.
        let mut attempts = 0usize;
        let mut sleeps = Vec::new();
        tighten_sidecar_with_retry(
            dummy_path,
            |_| {
                attempts += 1;
                Ok(())
            },
            |d| sleeps.push(d),
            false,
        )
        .expect("immediate ok");
        assert_eq!(attempts, 1);
        assert!(sleeps.is_empty());

        // 2. Recovers after 2 ConcurrentModification errors (3rd attempt succeeds).
        let mut attempts = 0usize;
        let mut sleeps = Vec::new();
        tighten_sidecar_with_retry(
            dummy_path,
            |p| {
                attempts += 1;
                if attempts < 3 {
                    Err(FsError::ConcurrentModification {
                        path: p.to_path_buf(),
                    })
                } else {
                    Ok(())
                }
            },
            |d| sleeps.push(d),
            false,
        )
        .expect("recovers on 3rd attempt");
        assert_eq!(attempts, 3);
        assert_eq!(
            sleeps,
            vec![SIDECAR_TIGHTEN_RETRY_DELAY, SIDECAR_TIGHTEN_RETRY_DELAY]
        );

        // 3. Exhausting 3 ConcurrentModification attempts fails when tolerate=false,
        //    and succeeds (with debug log) when post_commit tolerate=true.
        for tolerate in [false, true] {
            let mut attempts = 0usize;
            let mut sleeps = Vec::new();
            let res = tighten_sidecar_with_retry(
                dummy_path,
                |p| {
                    attempts += 1;
                    Err(FsError::ConcurrentModification {
                        path: p.to_path_buf(),
                    })
                },
                |d| sleeps.push(d),
                tolerate,
            );
            assert_eq!(attempts, SIDECAR_TIGHTEN_MAX_ATTEMPTS);
            assert_eq!(sleeps.len(), SIDECAR_TIGHTEN_MAX_ATTEMPTS - 1);
            if tolerate {
                assert!(res.is_ok());
            } else {
                assert!(matches!(
                    res,
                    Err(CoreError::Fs(FsError::ConcurrentModification { .. }))
                ));
            }
        }

        // 4. SymlinkRejected and NotRegularFile fail closed on attempt 1 with ZERO retries,
        //    even when post_commit tolerate=true!
        for fatal in [
            FsError::SymlinkRejected {
                path: dummy_path.to_path_buf(),
            },
            FsError::NotRegularFile {
                path: dummy_path.to_path_buf(),
            },
        ] {
            let mut attempts = 0usize;
            let mut sleeps = Vec::new();
            let err = tighten_sidecar_with_retry(
                dummy_path,
                |_| {
                    attempts += 1;
                    match &fatal {
                        FsError::SymlinkRejected { path } => {
                            Err(FsError::SymlinkRejected { path: path.clone() })
                        }
                        FsError::NotRegularFile { path } => {
                            Err(FsError::NotRegularFile { path: path.clone() })
                        }
                        _ => unreachable!(),
                    }
                },
                |d| sleeps.push(d),
                true,
            )
            .expect_err("symlink / FIFO must never be retried or tolerated");
            assert_eq!(attempts, 1);
            assert!(sleeps.is_empty());
            assert!(matches!(
                err,
                CoreError::Fs(FsError::SymlinkRejected { .. } | FsError::NotRegularFile { .. })
            ));
        }
    }

    #[test]
    fn sqlite_watermarks_monotonic_upsert_revert_reset_prune_and_conversation_query() {
        let tmp = TempDir::new().expect("temp dir");
        let sink = SqliteUsageSink::new(tmp.path()).expect("sink init");

        // Lazy-init reads on missing DB do not create usage.db.
        assert!(sink.load_all_watermarks().expect("empty wm").is_empty());
        assert_eq!(
            sink.prune_stale_watermarks("2026-11-01T00:00:00Z")
                .expect("prune missing"),
            0
        );
        assert!(
            sink.query_events_for_root_or_conversation("root_conv_1", 10)
                .expect("query missing")
                .is_empty()
        );
        assert!(!sink.db_path().exists());

        // Atomic batch + watermark write.
        let wm_initial = LsSyncWatermark {
            cascade_id: "casc_001".to_string(),
            trajectory_id: "traj_v1".to_string(),
            step_offset: 10,
            last_modified_utc: "2026-11-22T10:00:00.123Z".to_string(),
            updated_at_utc: "2026-11-22T10:00:05Z".to_string(),
        };
        let mut root_ev = sample_event("2026-11-22T10:00:01Z", "ls:root_conv_1:1:0");
        root_ev.conversation_id = Some("root_conv_1".to_string());
        root_ev.root_conversation_id = Some("root_conv_1".to_string());
        root_ev.telemetry_source = TelemetrySource::Ls;

        let mut sub_ev = sample_event("2026-11-22T10:00:02Z", "ls:sub_conv_1:1:0");
        sub_ev.conversation_id = Some("sub_conv_1".to_string());
        sub_ev.root_conversation_id = Some("root_conv_1".to_string());
        sub_ev.parent_conversation_id = Some("root_conv_1".to_string());
        sub_ev.telemetry_source = TelemetrySource::Ls;

        let mut other_ev = sample_event("2026-11-22T10:00:03Z", "ls:other_conv:1:0");
        other_ev.conversation_id = Some("other_conv".to_string());
        other_ev.root_conversation_id = Some("other_conv".to_string());
        other_ev.telemetry_source = TelemetrySource::Ls;

        let affected = sink
            .record_batch_checked(&[root_ev, sub_ev, other_ev], Some(&wm_initial))
            .expect("batch with watermark");
        assert_eq!(affected, 3);

        // Same trajectory_id with lower step_offset stays monotonic at 10.
        sink.upsert_watermark(&LsSyncWatermark {
            cascade_id: "casc_001".to_string(),
            trajectory_id: "traj_v1".to_string(),
            step_offset: 7,
            last_modified_utc: "2026-11-22T09:59:00Z".to_string(),
            updated_at_utc: "2026-11-22T10:01:00Z".to_string(),
        })
        .expect("monotonic watermark");
        let wms = sink.load_all_watermarks().expect("load wm");
        assert_eq!(wms.len(), 1);
        assert_eq!(wms[0].step_offset, 10);
        assert_eq!(wms[0].last_modified_utc, "2026-11-22T10:00:00Z");

        // RevertToStep: trajectory_id changes -> accepts reset to lower step_offset 3.
        sink.upsert_watermark(&LsSyncWatermark {
            cascade_id: "casc_001".to_string(),
            trajectory_id: "traj_v2".to_string(),
            step_offset: 3,
            last_modified_utc: "2026-11-22T10:05:00Z".to_string(),
            updated_at_utc: "2026-11-22T10:05:00Z".to_string(),
        })
        .expect("revert reset watermark");
        let wms = sink.load_all_watermarks().expect("load wm after revert");
        assert_eq!(wms[0].trajectory_id, "traj_v2");
        assert_eq!(wms[0].step_offset, 3);

        // Add an older watermark and prune only rows older than cutoff.
        sink.upsert_watermark(&LsSyncWatermark {
            cascade_id: "casc_stale".to_string(),
            trajectory_id: "traj_old".to_string(),
            step_offset: 99,
            last_modified_utc: "2026-08-01T00:00:00Z".to_string(),
            updated_at_utc: "2026-08-01T00:00:00Z".to_string(),
        })
        .expect("insert stale wm");
        let pruned = sink
            .prune_stale_watermarks("2026-09-01T00:00:00Z")
            .expect("prune stale");
        assert_eq!(pruned, 1);
        let remaining = sink.load_all_watermarks().expect("remaining wms");
        assert_eq!(remaining.len(), 1);
        assert_eq!(remaining[0].cascade_id, "casc_001");

        // Conversation query returns root + subagent in chronological order, excluding other_conv.
        let conv_events = sink
            .query_events_for_root_or_conversation("root_conv_1", 10)
            .expect("query root_conv_1");
        assert_eq!(conv_events.len(), 2);
        assert_eq!(conv_events[0].request_id, "ls:root_conv_1:1:0");
        assert_eq!(conv_events[1].request_id, "ls:sub_conv_1:1:0");

        // Limit=1 returns the newest event only.
        let limited = sink
            .query_events_for_root_or_conversation("root_conv_1", 1)
            .expect("query limited");
        assert_eq!(limited.len(), 1);
        assert_eq!(limited[0].request_id, "ls:sub_conv_1:1:0");

        // Invalid conversation_id returns empty safely.
        assert!(
            sink.query_events_for_root_or_conversation("../bad", 10)
                .expect("invalid conv id")
                .is_empty()
        );
    }

    #[test]
    fn sqlite_v2_to_v3_in_place_migration_preserves_events_and_watermarks_with_readonly_compat() {
        let tmp = TempDir::new().expect("temp dir");
        let db_path = tmp.path().join(USAGE_DB_FILENAME);

        // Seed an authentic schema v2 database on disk with usage_events and ls_sync_watermarks.
        {
            let conn = Connection::open(&db_path).expect("open v2 db");
            conn.execute_batch(
                "CREATE TABLE usage_events (
                    id                      INTEGER PRIMARY KEY AUTOINCREMENT,
                    timestamp_utc           TEXT    NOT NULL,
                    request_id              TEXT    NOT NULL UNIQUE,
                    provider                TEXT    NOT NULL,
                    model                   TEXT    NOT NULL,
                    input_tokens            INTEGER NOT NULL CHECK (input_tokens >= 0),
                    output_tokens           INTEGER NOT NULL CHECK (output_tokens >= 0),
                    cached_tokens           INTEGER NOT NULL CHECK (cached_tokens >= 0),
                    reasoning_tokens        INTEGER NOT NULL CHECK (reasoning_tokens >= 0),
                    latency_ms              INTEGER NOT NULL CHECK (latency_ms >= 0),
                    source                  TEXT    NOT NULL,
                    response_id             TEXT,
                    conversation_id         TEXT,
                    root_conversation_id    TEXT,
                    parent_conversation_id  TEXT,
                    cache_write_tokens      INTEGER NOT NULL DEFAULT 0 CHECK (cache_write_tokens >= 0),
                    ttft_ms                 INTEGER CHECK (ttft_ms IS NULL OR ttft_ms >= 0),
                    model_display_name      TEXT,
                    telemetry_source        TEXT    NOT NULL DEFAULT 'proxy'
                                            CHECK (telemetry_source IN ('proxy', 'ls', 'reconciled')),
                    cascade_step_index      INTEGER CHECK (cascade_step_index IS NULL OR cascade_step_index >= 0)
                );
                CREATE INDEX idx_usage_events_timestamp_utc ON usage_events (timestamp_utc);
                CREATE INDEX idx_usage_events_provider_model ON usage_events (provider, model);
                CREATE UNIQUE INDEX idx_usage_events_response_id
                    ON usage_events (response_id) WHERE response_id IS NOT NULL;
                CREATE INDEX idx_usage_events_conversation_ts
                    ON usage_events (conversation_id, timestamp_utc DESC, id DESC)
                    WHERE conversation_id IS NOT NULL;
                CREATE INDEX idx_usage_events_root_conv_ts
                    ON usage_events (root_conversation_id, timestamp_utc DESC, id DESC)
                    WHERE root_conversation_id IS NOT NULL;
                CREATE TABLE ls_sync_watermarks (
                    cascade_id        TEXT    PRIMARY KEY,
                    trajectory_id     TEXT    NOT NULL,
                    step_offset       INTEGER NOT NULL CHECK (step_offset >= 0),
                    last_modified_utc TEXT    NOT NULL,
                    updated_at_utc    TEXT    NOT NULL
                );
                INSERT INTO usage_events (
                    timestamp_utc, request_id, provider, model,
                    input_tokens, output_tokens, cached_tokens, reasoning_tokens,
                    latency_ms, source, conversation_id, root_conversation_id,
                    telemetry_source, cascade_step_index
                ) VALUES (
                    '2026-11-22T10:00:00Z', 'ls:conv_v2_001:4:0', 'antigravity', 'gemini-4-neon',
                    1200, 300, 600, 50, 400, 'builtin', 'conv_v2_001', 'conv_v2_001',
                    'ls', 4
                );
                INSERT INTO ls_sync_watermarks (
                    cascade_id, trajectory_id, step_offset, last_modified_utc, updated_at_utc
                ) VALUES (
                    'conv_v2_001', 'traj_v2_001', 5, '2026-11-22T10:00:00Z', '2026-11-22T10:00:00Z'
                );
                PRAGMA user_version = 2;",
            )
            .expect("seed v2 schema");
        }

        let before_bytes = fs::read(&db_path).expect("read v2 bytes before");
        let sink = SqliteUsageSink::new(tmp.path()).expect("sink init");

        // Unmigrated v2 DB remains fully readable read-only without mutating disk.
        let watermarks_before = sink.load_all_watermarks().expect("load watermarks on v2");
        assert_eq!(watermarks_before.len(), 1);
        assert_eq!(watermarks_before[0].cascade_id, "conv_v2_001");
        assert_eq!(watermarks_before[0].step_offset, 5);

        let conv_before = sink
            .query_events_for_root_or_conversation("conv_v2_001", 10)
            .expect("query conversation on v2");
        assert_eq!(conv_before.len(), 1);
        assert_eq!(conv_before[0].request_id, "ls:conv_v2_001:4:0");

        let observed_before = sink
            .load_all_observed_models()
            .expect("load observed_models on v2 returns empty");
        assert!(observed_before.is_empty());

        let summary_before = crate::usage::query_usage_summary_at(
            tmp.path(),
            UsageRange::Days7,
            chrono::DateTime::parse_from_rfc3339("2026-11-23T00:00:00Z")
                .expect("ts")
                .with_timezone(&chrono::Utc),
        )
        .expect("query summary on v2");
        assert_eq!(summary_before.totals.requests, 1);

        let after_ro_bytes = fs::read(&db_path).expect("read v2 bytes after ro");
        assert_eq!(
            after_ro_bytes, before_bytes,
            "read-only queries on v2 must not mutate disk"
        );

        // First write triggers atomic v2 -> v3 migration in-place.
        let changed = sink
            .upsert_observed_model(&ObservedModel {
                provider: "antigravity".to_string(),
                model: "gemini-4-neon".to_string(),
                compaction_limit_tokens: Some(512_000),
                updated_at_utc: "2026-11-22T10:05:00.250Z".to_string(),
            })
            .expect("migrate v2->v3 and upsert observed_model");
        assert!(changed);

        let conn = Connection::open(&db_path).expect("open migrated v3 db");
        let version: u32 = conn
            .pragma_query_value(None, "user_version", |r| r.get(0))
            .expect("user_version");
        assert_eq!(version, 3);

        // Existing v2 usage_events and ls_sync_watermarks are intact.
        let events_after = sink.read_all_events().expect("read events after v3");
        assert_eq!(events_after.len(), 1);
        assert_eq!(events_after[0].request_id, "ls:conv_v2_001:4:0");

        let wms_after = sink
            .load_all_watermarks()
            .expect("read watermarks after v3");
        assert_eq!(wms_after, watermarks_before);

        let observed_after = sink
            .load_all_observed_models()
            .expect("read observed_models after v3");
        assert_eq!(
            observed_after,
            vec![ObservedModel {
                provider: "antigravity".to_string(),
                model: "gemini-4-neon".to_string(),
                compaction_limit_tokens: Some(512_000),
                updated_at_utc: "2026-11-22T10:05:00Z".to_string(),
            }]
        );
    }

    #[test]
    fn sqlite_observed_models_lazy_read_upsert_on_change_and_check_constraint() {
        let tmp = TempDir::new().expect("temp dir");
        let usage_dir = tmp.path().join("lazy_observed");
        let sink = SqliteUsageSink::new(&usage_dir).expect("sink init");

        // Missing dir/db returns empty without creating files.
        assert!(
            sink.load_all_observed_models()
                .expect("lazy read missing")
                .is_empty()
        );
        assert!(!usage_dir.exists());

        // Insert new row -> Ok(true).
        assert!(
            sink.upsert_observed_model(&ObservedModel {
                provider: "antigravity".to_string(),
                model: "gemini-4-neon".to_string(),
                compaction_limit_tokens: None,
                updated_at_utc: "2026-11-22T10:00:00Z".to_string(),
            })
            .expect("insert None")
        );

        // Identical NULL re-upsert -> Ok(false) (NULL-safe IS NOT), updated_at_utc unchanged.
        assert!(
            !sink
                .upsert_observed_model(&ObservedModel {
                    provider: "antigravity".to_string(),
                    model: "gemini-4-neon".to_string(),
                    compaction_limit_tokens: None,
                    updated_at_utc: "2026-11-22T10:01:00Z".to_string(),
                })
                .expect("no-op NULL re-upsert")
        );
        let rows = sink
            .load_all_observed_models()
            .expect("load after NULL no-op");
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].updated_at_utc, "2026-11-22T10:00:00Z");

        // None -> Some(512_000) -> Ok(true), updated_at_utc updated.
        assert!(
            sink.upsert_observed_model(&ObservedModel {
                provider: "antigravity".to_string(),
                model: "gemini-4-neon".to_string(),
                compaction_limit_tokens: Some(512_000),
                updated_at_utc: "2026-11-22T10:02:00Z".to_string(),
            })
            .expect("update None -> 512k")
        );

        // Identical Some(512_000) re-upsert -> Ok(false), updated_at_utc unchanged.
        assert!(
            !sink
                .upsert_observed_model(&ObservedModel {
                    provider: "antigravity".to_string(),
                    model: "gemini-4-neon".to_string(),
                    compaction_limit_tokens: Some(512_000),
                    updated_at_utc: "2026-11-22T10:03:00Z".to_string(),
                })
                .expect("no-op 512k re-upsert")
        );
        let rows = sink
            .load_all_observed_models()
            .expect("load after 512k no-op");
        assert_eq!(rows[0].compaction_limit_tokens, Some(512_000));
        assert_eq!(rows[0].updated_at_utc, "2026-11-22T10:02:00Z");

        // Changed value 512_000 -> 1_000_000 -> Ok(true).
        assert!(
            sink.upsert_observed_model(&ObservedModel {
                provider: "antigravity".to_string(),
                model: "gemini-4-neon".to_string(),
                compaction_limit_tokens: Some(1_000_000),
                updated_at_utc: "2026-11-22T10:04:00Z".to_string(),
            })
            .expect("update 512k -> 1m")
        );

        // Some(0) violates CHECK constraint without invalidating cached connection.
        let check_err = sink
            .upsert_observed_model(&ObservedModel {
                provider: "openai-codex".to_string(),
                model: "gpt-6.2-astra".to_string(),
                compaction_limit_tokens: Some(0),
                updated_at_utc: "2026-11-22T10:05:00Z".to_string(),
            })
            .expect_err("compaction_limit_tokens = 0 must fail CHECK");
        assert!(matches!(check_err, CoreError::UsageDatabase { .. }));

        // Subsequent valid write on same sink succeeds cleanly.
        assert!(
            sink.upsert_observed_model(&ObservedModel {
                provider: "openai-codex".to_string(),
                model: "gpt-6.2-astra".to_string(),
                compaction_limit_tokens: Some(400_000),
                updated_at_utc: "2026-11-22T10:06:00Z".to_string(),
            })
            .expect("valid write after CHECK error")
        );

        // Verify deterministic ordering by (provider ASC, model ASC).
        let final_rows = sink.load_all_observed_models().expect("final load");
        assert_eq!(
            final_rows,
            vec![
                ObservedModel {
                    provider: "antigravity".to_string(),
                    model: "gemini-4-neon".to_string(),
                    compaction_limit_tokens: Some(1_000_000),
                    updated_at_utc: "2026-11-22T10:04:00Z".to_string(),
                },
                ObservedModel {
                    provider: "openai-codex".to_string(),
                    model: "gpt-6.2-astra".to_string(),
                    compaction_limit_tokens: Some(400_000),
                    updated_at_utc: "2026-11-22T10:06:00Z".to_string(),
                },
            ]
        );
    }

    #[test]
    fn historical_db_normalization_fixes_placeholders_and_codex_ls_rows_idempotently() {
        let tmp = TempDir::new().expect("temp dir");
        let db_dir = tmp.path().join("usage");
        fs::create_dir_all(&db_dir).expect("mkdir");
        let db_path = db_dir.join(USAGE_DB_FILENAME);

        // Seed a v3 usage.db with historical unmapped placeholders and mis-classified Codex LS rows.
        {
            let conn = Connection::open(&db_path).expect("open raw db");
            conn.execute_batch(SCHEMA_V3_DDL).expect("schema v3");
            conn.execute(
                "INSERT INTO usage_events (
                    timestamp_utc, request_id, provider, model,
                    input_tokens, output_tokens, cached_tokens, reasoning_tokens,
                    latency_ms, source, response_id, model_display_name, telemetry_source
                ) VALUES
                ('2026-11-26T10:00:00Z', 'ls:c1:1:0', 'antigravity', 'MODEL_PLACEHOLDER_M530', 1000, 100, 800, 10, 200, 'builtin', 'r-m530', NULL, 'ls'),
                ('2026-11-26T10:01:00Z', 'ls:c1:2:0', 'antigravity', 'MODEL_PLACEHOLDER_M400', 500, 50, 200, 0, 150, 'builtin', 'r-m400', NULL, 'ls'),
                ('2026-11-26T10:02:00Z', 'ls:c1:3:0', 'gemini', 'gpt-6.2-sol', 700, 70, 300, 5, 180, 'proxy', 'r-codex-ls', 'GPT-6.2 Sol (Codex) (Medium)', 'ls'),
                ('2026-11-26T10:03:00Z', 'ls:c1:4:0', 'gemini', 'codex-next', 300, 30, 0, 0, 120, 'proxy', 'r-codex-next', NULL, 'ls'),
                ('2026-11-26T10:04:00Z', 'ls:c1:5:0', 'vertex_openai', 'gpt-5.2', 400, 40, 100, 0, 100, 'builtin', 'r-vertex-openai', NULL, 'ls'),
                ('2026-11-26T10:05:00Z', 'ls:c1:6:0', 'gemini', 'gemini-4-neon', 600, 60, 400, 0, 90, 'proxy', 'r-gemini', NULL, 'ls')",
                [],
            )
            .expect("seed historical rows");
        }

        let sink = SqliteUsageSink::new(&db_dir).expect("sink init");
        // Opening/normalizing fixes historical rows without altering token totals.
        let updated = sink.normalize_historical_rows().expect("normalize");
        assert!(updated >= 4);

        let events = sink.read_all_events().expect("read all");
        assert_eq!(events.len(), 6);
        let by_id: std::collections::HashMap<&str, &UsageEvent> =
            events.iter().map(|e| (e.request_id.as_str(), e)).collect();

        assert_eq!(by_id["ls:c1:1:0"].model, "gemini-4-hydrogen");
        assert_eq!(by_id["ls:c1:1:0"].provider, "antigravity");
        assert_eq!(
            by_id["ls:c1:1:0"].model_display_name.as_deref(),
            Some("Gemini 4 Hydrogen")
        );
        assert_eq!(by_id["ls:c1:1:0"].input_tokens, 1000);

        assert_eq!(by_id["ls:c1:2:0"].model, "claude-opus-5-5");
        assert_eq!(by_id["ls:c1:2:0"].provider, "vertex_claude");
        assert_eq!(
            by_id["ls:c1:2:0"].model_display_name.as_deref(),
            Some("Claude Opus 5.5")
        );

        assert_eq!(by_id["ls:c1:3:0"].provider, "openai-codex");
        assert_eq!(by_id["ls:c1:3:0"].model, "gpt-6.2-sol");

        assert_eq!(by_id["ls:c1:4:0"].provider, "openai-codex");
        assert_eq!(by_id["ls:c1:5:0"].provider, "vertex_openai");
        assert_eq!(by_id["ls:c1:6:0"].provider, "gemini");

        // Second run is a zero-row no-op (idempotent).
        assert_eq!(sink.normalize_historical_rows().expect("idempotent"), 0);
    }

    #[test]
    fn historical_db_normalization_strips_tiered_and_resolves_bare_numeric_placeholders_with_exact_token_sums()
     {
        let tmp = TempDir::new().expect("temp dir");
        let db_dir = tmp.path().join("usage");
        fs::create_dir_all(&db_dir).expect("mkdir");
        let db_path = db_dir.join(USAGE_DB_FILENAME);

        {
            let conn = Connection::open(&db_path).expect("open raw db");
            conn.execute_batch(SCHEMA_V3_DDL).expect("schema v3");
            conn.execute(
                "INSERT INTO usage_events (
                    timestamp_utc, request_id, provider, model,
                    input_tokens, output_tokens, cached_tokens, reasoning_tokens,
                    latency_ms, source, response_id, model_display_name, telemetry_source
                ) VALUES
                ('2026-11-26T10:00:00Z', 'ls:t1:1:0', 'antigravity', 'gemini-4-hydrogen-tiered', 1200, 120, 900, 15, 210, 'builtin', 'r-t1', 'Gemini 4 Hydrogen', 'ls'),
                ('2026-11-26T10:01:00Z', 'ls:t1:2:0', 'antigravity', 'gemini-4-hydrogen-tiered-high', 800, 80, 600, 5, 190, 'builtin', 'r-t2', 'Gemini 4 Hydrogen (High)', 'ls'),
                ('2026-11-26T10:02:00Z', 'ls:t1:3:0', 'antigravity', '530', 500, 50, 200, 0, 150, 'builtin', 'r-530', NULL, 'ls'),
                ('2026-11-26T10:03:00Z', 'ls:t1:4:0', 'antigravity', '99999', 100, 10, 0, 0, 50, 'builtin', 'r-unk', NULL, 'ls'),
                ('2026-11-26T10:04:00Z', 'ls:t1:5:0', 'gemini', 'gemini-4-neon-high', 300, 30, 100, 0, 90, 'proxy', 'r-non-tiered', NULL, 'ls')",
                [],
            )
            .expect("seed tiered and numeric rows");
        }

        let sink = SqliteUsageSink::new(&db_dir).expect("sink init");
        assert_eq!(sink.normalize_historical_rows().expect("normalize"), 3);
        assert_eq!(sink.normalize_historical_rows().expect("idempotent"), 0);

        let events = sink.read_all_events().expect("read all");
        assert_eq!(events.len(), 5);
        let by_id: std::collections::HashMap<&str, &UsageEvent> =
            events.iter().map(|e| (e.request_id.as_str(), e)).collect();

        assert_eq!(by_id["ls:t1:1:0"].model, "gemini-4-hydrogen");
        assert_eq!(by_id["ls:t1:2:0"].model, "gemini-4-hydrogen");
        assert_eq!(by_id["ls:t1:3:0"].model, "gemini-4-hydrogen");
        assert_eq!(
            by_id["ls:t1:3:0"].model_display_name.as_deref(),
            Some("Gemini 4 Hydrogen")
        );
        // Unmapped all-digit placeholder and non-tiered effort suffix remain untouched.
        assert_eq!(by_id["ls:t1:4:0"].model, "99999");
        assert_eq!(by_id["ls:t1:5:0"].model, "gemini-4-neon-high");

        // Exact token sums preserved across all rows.
        assert_eq!(
            events.iter().map(|e| e.input_tokens).sum::<u64>(),
            1200 + 800 + 500 + 100 + 300
        );
        assert_eq!(
            events.iter().map(|e| e.output_tokens).sum::<u64>(),
            120 + 80 + 50 + 10 + 30
        );
        assert_eq!(
            events.iter().map(|e| e.cached_tokens).sum::<u64>(),
            900 + 600 + 200 + 100
        );
        assert_eq!(events.iter().map(|e| e.reasoning_tokens).sum::<u64>(), 20);

        // Verify Usage -> By Model merges all three gemini-4-hydrogen rows into one row.
        let now = chrono::DateTime::parse_from_rfc3339("2026-11-26T12:00:00Z")
            .expect("parse now")
            .with_timezone(&chrono::Utc);
        let summary =
            crate::usage::query_usage_summary_at(&db_dir, crate::usage::UsageRange::Today, now)
                .expect("query summary");
        let hydrogen_rows: Vec<_> = summary
            .by_model
            .iter()
            .filter(|r| r.key == "gemini-4-hydrogen")
            .collect();
        assert_eq!(hydrogen_rows.len(), 1);
        assert_eq!(hydrogen_rows[0].requests, 3);
        assert_eq!(hydrogen_rows[0].input_tokens, 2500);
        assert_eq!(hydrogen_rows[0].output_tokens, 250);
        assert_eq!(hydrogen_rows[0].cached_tokens, 1700);
        assert_eq!(hydrogen_rows[0].reasoning_tokens, 20);
    }
}
