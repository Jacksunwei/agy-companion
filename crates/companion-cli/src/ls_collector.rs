use std::collections::{HashMap, HashSet, VecDeque};
use std::fs::{File, TryLockError};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use chrono::{DateTime, SecondsFormat, Utc};
use companion_core::fs_safe::{self, OpenRegularMode};
use companion_core::{
    LsSyncWatermark, MAX_RECENT_CALLS, MAX_SESSION_MODELS, ModelSessionStats, RecentCallSnapshot,
    SqliteUsageSink, StatsSnapshot, TelemetrySource, UsageEvent, UsageStatsSnapshot,
    normalize_utc_timestamp, sanitize_conversation_id, sanitize_model_display_name,
    sanitize_response_id,
};
use companion_providers::{
    LsClient, LsError, LsModelConfig, LsModelUsage, LsStepMetadata, LsSummaryDelta,
    LsTrajectoryStep, LsTrajectorySummary,
};
use tokio::sync::{Mutex, Notify, Semaphore, watch};

pub const DEFAULT_SNAPSHOT_SETTLE_IDLE: Duration = Duration::from_millis(300);
pub const DEFAULT_SNAPSHOT_SETTLE_MAX: Duration = Duration::from_secs(5);
pub const DEFAULT_FALLBACK_POLL_INTERVAL: Duration = Duration::from_secs(30);
pub const DEFAULT_CATALOG_REFRESH_MIN_INTERVAL: Duration = Duration::from_secs(60);
pub const DEFAULT_CASCADE_DEBOUNCE: Duration = Duration::from_secs(2);
pub const DEFAULT_CIRCUIT_BREAKER_COOLDOWN: Duration = Duration::from_secs(300);
pub const DEFAULT_RECONNECT_INITIAL_BACKOFF: Duration = Duration::from_secs(1);
pub const DEFAULT_RECONNECT_MAX_BACKOFF: Duration = Duration::from_secs(30);
pub const DEFAULT_MAX_IN_FLIGHT_STEPS: usize = 2;
pub const DEFAULT_MAX_CASCADES_PER_TICK: usize = 8;
pub const DEFAULT_WATERMARK_RETENTION_DAYS: i64 = 90;
pub const DEFAULT_MAX_MONITOR_DESCENDANTS: usize = 32;
pub const DEFAULT_BOOT_NEW_CASCADE_GRACE_SECS: i64 = 5;

const MAX_TRACKED_VARIANTS: usize = 128;
const MAX_CACHED_CALLS_PER_CASCADE: usize = 64;
const MAX_STATS_MODEL_LEN: usize = 128;
const LARGE_CASCADE_HYDRATION_THRESHOLD: u32 = 500;
const LARGE_CASCADE_TAIL_WINDOW: u32 = 64;
const SCHEDULER_TICK_INTERVAL: Duration = Duration::from_millis(50);

const STEP_TYPE_PLANNER_RESPONSE: &str = "CORTEX_STEP_TYPE_PLANNER_RESPONSE";
const STEP_TYPE_CHECKPOINT: &str = "CORTEX_STEP_TYPE_CHECKPOINT";
const TRAJECTORY_TYPE_SIDE_QUESTION: &str = "CORTEX_TRAJECTORY_TYPE_SIDE_QUESTION";

#[derive(Debug, Clone)]
pub struct LsCollectorConfig {
    pub snapshot_settle_idle: Duration,
    pub snapshot_settle_max: Duration,
    pub fallback_poll_interval: Duration,
    pub catalog_refresh_min_interval: Duration,
    pub cascade_debounce: Duration,
    pub circuit_breaker_cooldown: Duration,
    pub reconnect_initial_backoff: Duration,
    pub reconnect_max_backoff: Duration,
    pub max_in_flight_steps: usize,
    pub max_cascades_per_tick: usize,
    pub watermark_retention_days: i64,
    pub max_monitor_descendants: usize,
    pub boot_new_cascade_grace_secs: i64,
}

impl Default for LsCollectorConfig {
    fn default() -> Self {
        Self {
            snapshot_settle_idle: DEFAULT_SNAPSHOT_SETTLE_IDLE,
            snapshot_settle_max: DEFAULT_SNAPSHOT_SETTLE_MAX,
            fallback_poll_interval: DEFAULT_FALLBACK_POLL_INTERVAL,
            catalog_refresh_min_interval: DEFAULT_CATALOG_REFRESH_MIN_INTERVAL,
            cascade_debounce: DEFAULT_CASCADE_DEBOUNCE,
            circuit_breaker_cooldown: DEFAULT_CIRCUIT_BREAKER_COOLDOWN,
            reconnect_initial_backoff: DEFAULT_RECONNECT_INITIAL_BACKOFF,
            reconnect_max_backoff: DEFAULT_RECONNECT_MAX_BACKOFF,
            max_in_flight_steps: DEFAULT_MAX_IN_FLIGHT_STEPS,
            max_cascades_per_tick: DEFAULT_MAX_CASCADES_PER_TICK,
            watermark_retention_days: DEFAULT_WATERMARK_RETENTION_DAYS,
            max_monitor_descendants: DEFAULT_MAX_MONITOR_DESCENDANTS,
            boot_new_cascade_grace_secs: DEFAULT_BOOT_NEW_CASCADE_GRACE_SECS,
        }
    }
}

/// Non-blocking exclusive per-port advisory lock over `<usage_dir>/ls-collector-<port>.lock`.
///
/// Only the holder runs the background `JetboxSubscribeToSummaries` SQLite writer loop;
/// sibling sidecar windows sharing the same LS port still keep `LsCollectorHandle` active
/// for on-demand Monitor hydration.
#[derive(Debug)]
pub struct PerPortCollectorLock {
    file: File,
    path: PathBuf,
}

impl PerPortCollectorLock {
    pub fn try_acquire(usage_dir: &Path, ls_port: u16) -> Result<Option<Self>, std::io::Error> {
        fs_safe::validate_path(usage_dir)
            .map_err(|err| std::io::Error::new(std::io::ErrorKind::InvalidInput, err))?;
        fs_safe::ensure_dir_no_symlink(usage_dir, 0o700)
            .map_err(|err| std::io::Error::other(err.to_string()))?;
        let lock_path = usage_dir.join(format!("ls-collector-{ls_port}.lock"));
        let (file, _meta) =
            fs_safe::open_verified_regular_file_0600(&lock_path, OpenRegularMode::ReadWriteCreate)
                .map_err(|err| std::io::Error::other(err.to_string()))?;
        match file.try_lock() {
            Ok(()) => Ok(Some(Self {
                file,
                path: lock_path,
            })),
            Err(TryLockError::WouldBlock) => Ok(None),
            Err(TryLockError::Error(err)) => Err(err),
        }
    }

    pub fn path(&self) -> &Path {
        &self.path
    }
}

impl Drop for PerPortCollectorLock {
    fn drop(&mut self) {
        let _ = self.file.unlock();
    }
}

// ---------------------------------------------------------------------------
// Catalog & 3-Tier Attribution
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Default)]
pub struct CatalogEntry {
    pub model_id: Option<String>,
    pub label: Option<String>,
    pub provider: Option<String>,
    pub model_url: Option<String>,
}

#[derive(Debug, Default)]
pub struct ModelCatalog {
    entries: HashMap<String, CatalogEntry>,
    last_refreshed: Option<Instant>,
}

impl ModelCatalog {
    pub fn update(&mut self, configs: Vec<LsModelConfig>, now: Instant) {
        self.entries.clear();
        for cfg in configs {
            let entry = CatalogEntry {
                model_id: cfg.model_id,
                label: cfg.label,
                provider: cfg.provider,
                model_url: cfg.model_url,
            };
            if let Some(moa) = cfg.model_or_alias {
                if let Some(model) = moa.model
                    && !model.is_empty()
                {
                    self.entries.insert(model, entry.clone());
                }
                if let Some(alias) = moa.alias
                    && !alias.is_empty()
                {
                    self.entries.entry(alias).or_insert(entry);
                }
            }
        }
        self.last_refreshed = Some(now);
    }

    pub fn lookup(&self, key: &str) -> Option<&CatalogEntry> {
        self.entries.get(key)
    }

    pub fn has_loaded(&self) -> bool {
        self.last_refreshed.is_some()
    }

    pub fn can_refresh(&self, min_interval: Duration, now: Instant) -> bool {
        self.last_refreshed
            .is_none_or(|last| now.saturating_duration_since(last) >= min_interval)
    }

    pub fn needs_refresh_for_enum(
        &self,
        model_enum: Option<&str>,
        min_interval: Duration,
        now: Instant,
    ) -> bool {
        let Some(key) = model_enum else {
            return false;
        };
        if !key.starts_with("MODEL_PLACEHOLDER_") {
            return false;
        }
        !self.entries.contains_key(key) && self.can_refresh(min_interval, now)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AttributedModel {
    pub provider: String,
    pub source: String,
    pub model: String,
    pub model_display_name: Option<String>,
    pub variant_key: String,
}

fn sanitize_stats_model(raw: &str) -> String {
    let cleaned: String = raw
        .chars()
        .filter(|c| c.is_ascii_graphic() && *c != '"' && *c != '\\')
        .take(MAX_STATS_MODEL_LEN)
        .collect();
    if cleaned.is_empty() {
        "unknown".to_string()
    } else {
        cleaned
    }
}

pub fn canonicalize_model_id(raw: &str) -> String {
    let trimmed = raw.trim();
    let without_scheme = trimmed.strip_prefix("gemini-api://").unwrap_or(trimmed);
    let without_prefix = without_scheme
        .strip_prefix("companion/")
        .unwrap_or(without_scheme);
    let lower = without_prefix.to_ascii_lowercase();
    let stripped = if lower.ends_with("-medium") {
        &without_prefix[..without_prefix.len() - 7]
    } else if lower.ends_with("-high") {
        &without_prefix[..without_prefix.len() - 5]
    } else if lower.ends_with("-low") || lower.ends_with("-max") {
        &without_prefix[..without_prefix.len() - 4]
    } else {
        without_prefix
    };
    sanitize_stats_model(stripped)
}

fn is_internal_byom_enum(model_enum: Option<&str>) -> bool {
    matches!(
        model_enum,
        Some("MODEL_GOOGLE_GEMINI_INTERNAL_BYOM" | "326")
    )
}

fn map_provider_from_api_provider(
    api_provider: Option<&str>,
    source: &str,
    model_enum: Option<&str>,
    catalog_provider: Option<&str>,
    raw_model_id: Option<&str>,
) -> String {
    if let Some(ap) = api_provider {
        match ap {
            "API_PROVIDER_ANTHROPIC_VERTEX" | "26" => return "vertex_claude".to_string(),
            "API_PROVIDER_OPENAI_VERTEX" | "31" | "API_PROVIDER_OPENAI_RESPONSES_VERTEX" | "32" => {
                return "vertex_openai".to_string();
            }
            "API_PROVIDER_GOOGLE_GEMINI" | "24" => {
                return if source != "builtin" {
                    "gemini".to_string()
                } else {
                    "antigravity".to_string()
                };
            }
            "API_PROVIDER_GOOGLE_VERTEX"
            | "3"
            | "API_PROVIDER_GOOGLE_EVERGREEN"
            | "30"
            | "API_PROVIDER_INTERNAL"
            | "1" => {
                return if source == "proxy" && is_internal_byom_enum(model_enum) {
                    "gemini".to_string()
                } else {
                    "antigravity".to_string()
                };
            }
            _ => {}
        }
    }

    if source == "proxy" || source == "direct" {
        return "gemini".to_string();
    }

    if let Some(cp) = catalog_provider {
        let upper = cp.to_ascii_uppercase();
        if upper.contains("ANTHROPIC") || upper.contains("CLAUDE") {
            return "vertex_claude".to_string();
        }
        if upper.contains("OPENAI") {
            return "vertex_openai".to_string();
        }
    }

    if let Some(mid) = raw_model_id {
        let lower = mid.trim().to_ascii_lowercase();
        let bare = lower.strip_prefix("companion/").unwrap_or(&lower);
        if bare.starts_with("claude") {
            return "vertex_claude".to_string();
        }
        if bare.starts_with("gpt")
            || bare.starts_with("o3")
            || bare.starts_with("o4")
            || bare.starts_with("codex")
        {
            return "vertex_openai".to_string();
        }
    }

    "antigravity".to_string()
}

/// Deterministic 3-tier attribution (`GetCascadeModelConfigData` + `responseHeader` 3-way
/// wire fingerprint + `apiProvider`/`modelInfo` fallback).
pub fn resolve_attribution(
    meta: &LsStepMetadata,
    usage: &LsModelUsage,
    catalog: &ModelCatalog,
) -> AttributedModel {
    let model_enum = meta.generator_model.as_deref().or(usage.model.as_deref());
    let catalog_entry = model_enum.and_then(|key| catalog.lookup(key));

    let resp_hdr = usage.response_header.as_ref();
    let has_builtin_session = resp_hdr.is_some_and(|h| h.has_builtin_session_id());
    let has_direct_session = resp_hdr.is_some_and(|h| h.has_direct_session_id());
    let has_hdr_response_id = resp_hdr.is_some_and(|h| h.has_response_id());

    let catalog_model_id = catalog_entry.and_then(|c| c.model_id.as_deref());
    let catalog_model_url = catalog_entry.and_then(|c| c.model_url.as_deref());
    let info_model_url = meta
        .model_info
        .as_ref()
        .and_then(|i| i.model_url.as_deref());

    let is_companion_catalog = catalog_model_id.is_some_and(|id| id.starts_with("companion/"))
        || (catalog_model_url.is_some_and(|u| u.starts_with("gemini-api://"))
            && !has_builtin_session
            && !has_direct_session);

    let source = if is_companion_catalog {
        "proxy".to_string()
    } else if has_builtin_session {
        "builtin".to_string()
    } else if has_hdr_response_id && !has_direct_session {
        "proxy".to_string()
    } else if has_hdr_response_id && has_direct_session {
        "direct".to_string()
    } else if info_model_url.is_some_and(|u| u.starts_with("gemini-api://")) {
        "proxy".to_string()
    } else {
        "builtin".to_string()
    };

    let api_provider = usage.api_provider.as_deref().or_else(|| {
        meta.model_info
            .as_ref()
            .and_then(|i| i.api_provider.as_deref())
    });
    let provider = if is_companion_catalog {
        "gemini".to_string()
    } else {
        map_provider_from_api_provider(
            api_provider,
            &source,
            model_enum,
            catalog_entry.and_then(|c| c.provider.as_deref()),
            catalog_model_id.or_else(|| {
                meta.model_info.as_ref().and_then(|i| {
                    i.vertex_model_id
                        .as_deref()
                        .or(i.model_name.as_deref())
                        .or(i.model_id.as_deref())
                })
            }),
        )
    };

    let model = if is_companion_catalog {
        if let Some(url_model) = catalog_model_url.and_then(|u| u.strip_prefix("gemini-api://"))
            && !url_model.trim().is_empty()
        {
            canonicalize_model_id(url_model)
        } else if let Some(mid) = catalog_model_id {
            canonicalize_model_id(mid)
        } else {
            sanitize_stats_model(model_enum.unwrap_or("unknown"))
        }
    } else if let Some(mid) = catalog_model_id.filter(|s| !s.trim().is_empty()) {
        canonicalize_model_id(mid)
    } else if let Some(info) = meta.model_info.as_ref() {
        let candidate = info
            .model_url
            .as_deref()
            .and_then(|u| u.strip_prefix("gemini-api://"))
            .filter(|s| !s.trim().is_empty())
            .or_else(|| {
                info.vertex_model_id
                    .as_deref()
                    .filter(|s| !s.trim().is_empty())
            })
            .or_else(|| info.model_name.as_deref().filter(|s| !s.trim().is_empty()))
            .or_else(|| info.model_id.as_deref().filter(|s| !s.trim().is_empty()));
        candidate
            .map(canonicalize_model_id)
            .unwrap_or_else(|| sanitize_stats_model(model_enum.unwrap_or("unknown")))
    } else {
        sanitize_stats_model(model_enum.unwrap_or("unknown"))
    };

    let model_display_name = catalog_entry
        .and_then(|c| c.label.as_deref())
        .or_else(|| {
            meta.model_info
                .as_ref()
                .and_then(|i| i.display_name.as_deref())
        })
        .and_then(sanitize_model_display_name);

    let variant_key = model_enum
        .filter(|s| !s.trim().is_empty())
        .or_else(|| catalog_model_id.filter(|s| !s.trim().is_empty()))
        .map(sanitize_stats_model)
        .unwrap_or_else(|| model.clone());

    AttributedModel {
        provider,
        source,
        model,
        model_display_name,
        variant_key,
    }
}

// ---------------------------------------------------------------------------
// Step Normalization, In-Flight Guard & Per-Cascade Chronological Fold
// ---------------------------------------------------------------------------

pub fn is_side_question_summary(summary: &LsTrajectorySummary) -> bool {
    matches!(
        summary.trajectory_type.as_deref(),
        Some(TRAJECTORY_TYPE_SIDE_QUESTION)
    )
}

fn is_planner_response_step(step: &LsTrajectoryStep) -> bool {
    matches!(step.step_type.as_deref(), Some(STEP_TYPE_PLANNER_RESPONSE))
}

fn is_checkpoint_step(step: &LsTrajectoryStep) -> bool {
    matches!(step.step_type.as_deref(), Some(STEP_TYPE_CHECKPOINT))
}

fn is_terminal_status(status: Option<&str>) -> bool {
    matches!(
        status,
        Some(
            "CORTEX_STEP_STATUS_DONE"
                | "3"
                | "CORTEX_STEP_STATUS_ERROR"
                | "4"
                | "CORTEX_STEP_STATUS_CANCELED"
                | "5"
                | "CORTEX_STEP_STATUS_CANCELLED"
        )
    )
}

fn is_done_status(status: Option<&str>) -> bool {
    matches!(status, Some("CORTEX_STEP_STATUS_DONE" | "3"))
}

/// Returns `false` for any in-flight/non-terminal step so `step_offset` never advances past it.
pub fn is_step_terminal(step: &LsTrajectoryStep) -> bool {
    if !is_terminal_status(step.status.as_deref()) {
        return false;
    }
    if is_planner_response_step(step) && is_done_status(step.status.as_deref()) {
        let Some(meta) = step.metadata.as_ref() else {
            return false;
        };
        if meta.completed_at.is_none()
            && meta.finished_generating_at.is_none()
            && meta.model_usage.is_none()
            && meta.retry_infos.is_empty()
        {
            return false;
        }
    }
    true
}

fn has_nonzero_tokens(usage: &LsModelUsage) -> bool {
    usage.input_tokens > 0
        || usage.cache_read_tokens > 0
        || usage.cache_write_tokens > 0
        || usage.output_tokens > 0
        || usage.response_output_tokens > 0
        || usage.thinking_output_tokens > 0
}

fn parse_rfc3339_utc(raw: Option<&str>) -> Option<DateTime<Utc>> {
    let trimmed = raw?.trim();
    if trimmed.is_empty() {
        return None;
    }
    DateTime::parse_from_rfc3339(trimmed)
        .ok()
        .map(|dt| dt.with_timezone(&Utc))
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NormalizedCall {
    pub event: UsageEvent,
    pub timestamp_ms: u64,
    pub variant_key: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum NormalizedStepEntry {
    Checkpoint {
        step_index: u32,
        calls: Vec<NormalizedCall>,
    },
    Planner {
        step_index: u32,
        calls: Vec<NormalizedCall>,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProcessedStepsBatch {
    pub new_step_offset: u32,
    pub hit_non_terminal: bool,
    pub entries: Vec<NormalizedStepEntry>,
    pub unknown_model_enum: Option<String>,
}

struct StepCallContext<'a> {
    cascade_id: &'a str,
    summary: Option<&'a LsTrajectorySummary>,
    step_index: u32,
    catalog: &'a ModelCatalog,
    fallback_now: DateTime<Utc>,
}

fn build_normalized_call(
    ctx: &StepCallContext<'_>,
    retry_index: usize,
    meta: &LsStepMetadata,
    usage: &LsModelUsage,
) -> NormalizedCall {
    let attributed = resolve_attribution(meta, usage, ctx.catalog);

    let cached_tokens = usage.cache_read_tokens;
    let cache_write_tokens = usage.cache_write_tokens;
    let input_tokens = usage
        .input_tokens
        .saturating_add(cached_tokens)
        .saturating_add(cache_write_tokens);
    let reasoning_tokens = usage.thinking_output_tokens;
    let output_tokens = if usage.response_output_tokens > 0 {
        usage.response_output_tokens
    } else {
        usage
            .output_tokens
            .saturating_sub(usage.thinking_output_tokens)
    };

    let created_dt = parse_rfc3339_utc(meta.created_at.as_deref());
    let viewable_dt = parse_rfc3339_utc(meta.viewable_at.as_deref());
    let finished_dt = parse_rfc3339_utc(meta.finished_generating_at.as_deref());
    let completed_dt = parse_rfc3339_utc(meta.completed_at.as_deref());

    let ttft_ms = match (created_dt, viewable_dt) {
        (Some(start), Some(first)) => {
            Some(first.signed_duration_since(start).num_milliseconds().max(0) as u64)
        }
        _ => None,
    };

    let latency_ms = match (created_dt, finished_dt.or(completed_dt)) {
        (Some(start), Some(end)) => {
            end.signed_duration_since(start).num_milliseconds().max(0) as u64
        }
        _ => 0,
    };

    let event_dt = finished_dt
        .or(completed_dt)
        .or(created_dt)
        .unwrap_or(ctx.fallback_now);
    let raw_utc = event_dt.to_rfc3339_opts(SecondsFormat::Secs, true);
    let timestamp_utc = normalize_utc_timestamp(&raw_utc)
        .unwrap_or_else(|| ctx.fallback_now.to_rfc3339_opts(SecondsFormat::Secs, true));
    let timestamp_ms = event_dt.timestamp_millis().max(0) as u64;

    let response_id = usage
        .response_id
        .as_deref()
        .and_then(sanitize_response_id)
        .or_else(|| {
            usage
                .response_header
                .as_ref()
                .and_then(|h| h.response_id.as_deref())
                .and_then(sanitize_response_id)
        });

    let conversation_id = sanitize_conversation_id(ctx.cascade_id);
    let parent_conversation_id = ctx
        .summary
        .and_then(|s| s.trajectory_metadata.as_ref())
        .and_then(|m| m.parent_conversation_id.as_deref())
        .and_then(sanitize_conversation_id);
    let root_conversation_id = ctx
        .summary
        .and_then(|s| s.trajectory_metadata.as_ref())
        .and_then(|m| m.root_conversation_id.as_deref())
        .and_then(sanitize_conversation_id)
        .or_else(|| parent_conversation_id.clone());

    let step_instance_key = created_dt
        .or(finished_dt)
        .or(completed_dt)
        .map(|dt| dt.timestamp_micros().to_string())
        .or_else(|| {
            ctx.summary
                .and_then(|s| s.trajectory_id.as_deref())
                .and_then(sanitize_conversation_id)
        });
    let request_id = match step_instance_key {
        Some(key) => format!(
            "ls:{}:{}:{retry_index}:{key}",
            ctx.cascade_id, ctx.step_index
        ),
        None => format!("ls:{}:{}:{retry_index}", ctx.cascade_id, ctx.step_index),
    };

    let event = UsageEvent {
        timestamp_utc,
        request_id,
        provider: attributed.provider,
        model: attributed.model,
        input_tokens,
        output_tokens,
        cached_tokens,
        reasoning_tokens,
        latency_ms,
        source: attributed.source,
        response_id,
        conversation_id,
        root_conversation_id,
        parent_conversation_id,
        cache_write_tokens,
        ttft_ms,
        model_display_name: attributed.model_display_name,
        telemetry_source: TelemetrySource::Ls,
        cascade_step_index: Some(ctx.step_index),
    };

    NormalizedCall {
        event,
        timestamp_ms,
        variant_key: attributed.variant_key,
    }
}

fn extract_step_calls(
    cascade_id: &str,
    summary: Option<&LsTrajectorySummary>,
    step_index: u32,
    meta: &LsStepMetadata,
    catalog: &ModelCatalog,
    fallback_now: DateTime<Utc>,
) -> Vec<NormalizedCall> {
    let ctx = StepCallContext {
        cascade_id,
        summary,
        step_index,
        catalog,
        fallback_now,
    };
    let nonzero_retries: Vec<(usize, &LsModelUsage)> = meta
        .retry_infos
        .iter()
        .enumerate()
        .filter_map(|(retry_idx, info)| {
            info.usage
                .as_ref()
                .filter(|u| has_nonzero_tokens(u))
                .map(|u| (retry_idx, u))
        })
        .collect();

    if !nonzero_retries.is_empty() {
        return nonzero_retries
            .into_iter()
            .map(|(retry_idx, usage)| build_normalized_call(&ctx, retry_idx, meta, usage))
            .collect();
    }

    if let Some(usage) = meta.model_usage.as_ref().filter(|u| has_nonzero_tokens(u)) {
        return vec![build_normalized_call(&ctx, 0, meta, usage)];
    }

    Vec::new()
}

/// Processes a slice of `LsTrajectoryStep`s starting at `step_offset`.
///
/// Stops advancing `new_step_offset` at the lowest index of any non-terminal step.
pub fn process_steps_slice(
    cascade_id: &str,
    summary: Option<&LsTrajectorySummary>,
    step_offset: u32,
    steps: &[LsTrajectoryStep],
    catalog: &ModelCatalog,
    min_step_created_at: Option<DateTime<Utc>>,
    fallback_now: DateTime<Utc>,
) -> ProcessedStepsBatch {
    let mut new_step_offset = step_offset;
    let mut hit_non_terminal = false;
    let mut entries = Vec::new();
    let mut unknown_model_enum: Option<String> = None;

    for (rel_idx, step) in steps.iter().enumerate() {
        let abs_idx = step_offset.saturating_add(rel_idx as u32);
        if !is_step_terminal(step) {
            hit_non_terminal = true;
            break;
        }
        new_step_offset = abs_idx.saturating_add(1);

        let is_checkpoint = is_checkpoint_step(step);
        let is_planner = is_planner_response_step(step);
        if !is_checkpoint && !is_planner {
            continue;
        }

        let Some(meta) = step.metadata.as_ref() else {
            if is_checkpoint {
                entries.push(NormalizedStepEntry::Checkpoint {
                    step_index: abs_idx,
                    calls: Vec::new(),
                });
            }
            continue;
        };

        if let Some(cutoff) = min_step_created_at
            && let Some(step_created) = parse_rfc3339_utc(meta.created_at.as_deref())
            && step_created < cutoff
        {
            continue;
        }

        if unknown_model_enum.is_none() {
            let candidate_enum = meta
                .generator_model
                .as_deref()
                .or_else(|| meta.model_usage.as_ref().and_then(|u| u.model.as_deref()));
            if let Some(key) = candidate_enum
                && key.starts_with("MODEL_PLACEHOLDER_")
                && catalog.lookup(key).is_none()
            {
                unknown_model_enum = Some(key.to_string());
            }
        }

        let calls = extract_step_calls(cascade_id, summary, abs_idx, meta, catalog, fallback_now);
        if is_checkpoint {
            entries.push(NormalizedStepEntry::Checkpoint {
                step_index: abs_idx,
                calls,
            });
        } else if !calls.is_empty() {
            entries.push(NormalizedStepEntry::Planner {
                step_index: abs_idx,
                calls,
            });
        }
    }

    ProcessedStepsBatch {
        new_step_offset,
        hit_non_terminal,
        entries,
        unknown_model_enum,
    }
}

#[derive(Debug, Default, Clone)]
pub struct CascadeFoldState {
    pub trajectory_id: String,
    pub step_offset: u32,
    seen_variants: HashSet<(String, String, String)>,
    saw_checkpoint_since_last_planner: bool,
    pub usage: UsageStatsSnapshot,
    pub by_model: HashMap<(String, String, String), ModelSessionStats>,
    pub recent_calls: VecDeque<RecentCallSnapshot>,
}

impl CascadeFoldState {
    pub fn new(trajectory_id: String, initial_step_offset: u32) -> Self {
        Self {
            trajectory_id,
            step_offset: initial_step_offset,
            ..Default::default()
        }
    }

    fn record_call_snapshot(&mut self, call: &NormalizedCall, cold_start: bool, cache_break: bool) {
        let ev = &call.event;
        self.usage.requests = self.usage.requests.saturating_add(1);
        self.usage.prompt_tokens = self.usage.prompt_tokens.saturating_add(ev.input_tokens);
        self.usage.cached_tokens = self.usage.cached_tokens.saturating_add(ev.cached_tokens);
        self.usage.output_tokens = self.usage.output_tokens.saturating_add(ev.output_tokens);
        self.usage.reasoning_tokens = self
            .usage
            .reasoning_tokens
            .saturating_add(ev.reasoning_tokens);
        self.usage.cache_hit_ratio = if self.usage.prompt_tokens > 0 {
            self.usage.cached_tokens as f64 / self.usage.prompt_tokens as f64
        } else {
            0.0
        };

        if self.recent_calls.len() >= MAX_CACHED_CALLS_PER_CASCADE {
            self.recent_calls.pop_front();
        }
        self.recent_calls.push_back(RecentCallSnapshot {
            timestamp_ms: call.timestamp_ms,
            provider: ev.provider.clone(),
            source: ev.source.clone(),
            model: ev.model.clone(),
            input_tokens: ev.input_tokens,
            cached_tokens: ev.cached_tokens,
            output_tokens: ev.output_tokens,
            reasoning_tokens: ev.reasoning_tokens,
            latency_ms: ev.latency_ms,
            cold_start,
            cache_break,
        });

        let model_key = (ev.provider.clone(), ev.source.clone(), ev.model.clone());
        if let Some(entry) = self.by_model.get_mut(&model_key) {
            entry.requests = entry.requests.saturating_add(1);
            entry.input_tokens = entry.input_tokens.saturating_add(ev.input_tokens);
            entry.cached_tokens = entry.cached_tokens.saturating_add(ev.cached_tokens);
            entry.output_tokens = entry.output_tokens.saturating_add(ev.output_tokens);
            entry.reasoning_tokens = entry.reasoning_tokens.saturating_add(ev.reasoning_tokens);
            entry.total_latency_ms = entry.total_latency_ms.saturating_add(ev.latency_ms);
        } else {
            self.by_model.insert(
                model_key,
                ModelSessionStats {
                    provider: ev.provider.clone(),
                    source: ev.source.clone(),
                    model: ev.model.clone(),
                    requests: 1,
                    input_tokens: ev.input_tokens,
                    cached_tokens: ev.cached_tokens,
                    output_tokens: ev.output_tokens,
                    reasoning_tokens: ev.reasoning_tokens,
                    total_latency_ms: ev.latency_ms,
                },
            );
        }
    }

    pub fn apply_entry(&mut self, entry: &NormalizedStepEntry) {
        match entry {
            NormalizedStepEntry::Checkpoint { calls, .. } => {
                self.saw_checkpoint_since_last_planner = true;
                for call in calls {
                    self.record_call_snapshot(call, false, false);
                }
            }
            NormalizedStepEntry::Planner { calls, .. } => {
                for call in calls {
                    let variant = (
                        call.event.provider.clone(),
                        call.event.source.clone(),
                        call.variant_key.clone(),
                    );
                    let has_prior = self.seen_variants.contains(&variant);
                    let (cold_start, cache_break) = if !has_prior {
                        if self.seen_variants.len() < MAX_TRACKED_VARIANTS {
                            self.seen_variants.insert(variant);
                        }
                        (call.event.cached_tokens == 0, false)
                    } else if self.saw_checkpoint_since_last_planner {
                        (call.event.cached_tokens == 0, false)
                    } else {
                        (
                            false,
                            call.event.input_tokens > 0 && call.event.cached_tokens == 0,
                        )
                    };
                    self.saw_checkpoint_since_last_planner = false;
                    self.record_call_snapshot(call, cold_start, cache_break);
                }
            }
        }
    }

    /// Applies only entries at `step_index >= self.step_offset` and advances `self.step_offset`.
    pub fn apply_batch(&mut self, batch: &ProcessedStepsBatch) {
        for entry in &batch.entries {
            let step_index = match entry {
                NormalizedStepEntry::Checkpoint { step_index, .. }
                | NormalizedStepEntry::Planner { step_index, .. } => *step_index,
            };
            if step_index >= self.step_offset {
                self.apply_entry(entry);
            }
        }
        if batch.new_step_offset > self.step_offset {
            self.step_offset = batch.new_step_offset;
        }
    }
}

/// Merges per-cascade fold states into a single `StatsSnapshot` (preserving independent
/// per-cascade `cold_start`/`cache_break` flags, sorting `recent_calls` chronologically
/// ascending and capping at `MAX_RECENT_CALLS` / `MAX_SESSION_MODELS`).
pub fn merge_cascade_folds(folds: &[&CascadeFoldState]) -> StatsSnapshot {
    let mut usage = UsageStatsSnapshot::default();
    let mut by_model_map: HashMap<(String, String, String), ModelSessionStats> = HashMap::new();
    let mut all_recent: Vec<RecentCallSnapshot> = Vec::new();

    for fold in folds {
        usage.requests = usage.requests.saturating_add(fold.usage.requests);
        usage.prompt_tokens = usage.prompt_tokens.saturating_add(fold.usage.prompt_tokens);
        usage.cached_tokens = usage.cached_tokens.saturating_add(fold.usage.cached_tokens);
        usage.output_tokens = usage.output_tokens.saturating_add(fold.usage.output_tokens);
        usage.reasoning_tokens = usage
            .reasoning_tokens
            .saturating_add(fold.usage.reasoning_tokens);

        for (key, stats) in &fold.by_model {
            if let Some(acc) = by_model_map.get_mut(key) {
                acc.requests = acc.requests.saturating_add(stats.requests);
                acc.input_tokens = acc.input_tokens.saturating_add(stats.input_tokens);
                acc.cached_tokens = acc.cached_tokens.saturating_add(stats.cached_tokens);
                acc.output_tokens = acc.output_tokens.saturating_add(stats.output_tokens);
                acc.reasoning_tokens = acc.reasoning_tokens.saturating_add(stats.reasoning_tokens);
                acc.total_latency_ms = acc.total_latency_ms.saturating_add(stats.total_latency_ms);
            } else {
                by_model_map.insert(key.clone(), stats.clone());
            }
        }

        all_recent.extend(fold.recent_calls.iter().cloned());
    }

    usage.cache_hit_ratio = if usage.prompt_tokens > 0 {
        usage.cached_tokens as f64 / usage.prompt_tokens as f64
    } else {
        0.0
    };

    all_recent.sort_by_key(|c| c.timestamp_ms);
    if all_recent.len() > MAX_RECENT_CALLS {
        let drop_count = all_recent.len() - MAX_RECENT_CALLS;
        all_recent.drain(..drop_count);
    }

    let mut by_model: Vec<ModelSessionStats> = by_model_map.into_values().collect();
    by_model.sort_by(|a, b| {
        b.input_tokens
            .cmp(&a.input_tokens)
            .then_with(|| b.requests.cmp(&a.requests))
            .then_with(|| a.model.cmp(&b.model))
            .then_with(|| a.provider.cmp(&b.provider))
            .then_with(|| a.source.cmp(&b.source))
    });
    by_model.truncate(MAX_SESSION_MODELS);

    StatsSnapshot {
        usage,
        cloudcode: Default::default(),
        proxy_rss_bytes: None,
        recent_calls: all_recent,
        by_model,
    }
}

// ---------------------------------------------------------------------------
// Collector State Machine (Discovery, Boot Baselines, Dirty Queue, Circuit Breaker)
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Eq)]
struct BootBaseline {
    trajectory_id: String,
    step_count: u32,
}

#[derive(Debug, Clone)]
struct DirtyCascade {
    start_offset: u32,
    reset_fold: bool,
    from_fallback_poll: bool,
}

#[derive(Debug)]
struct CollectorState {
    collector_boot_utc: DateTime<Utc>,
    boot_settled: bool,
    unauthenticated: bool,
    warned_unauthenticated: bool,
    circuit_open_until: Option<Instant>,
    catalog: ModelCatalog,
    persisted_watermarks: HashMap<String, LsSyncWatermark>,
    boot_baselines: HashMap<String, BootBaseline>,
    summaries: HashMap<String, LsTrajectorySummary>,
    children_by_root: HashMap<String, HashSet<String>>,
    dirty_cascades: HashMap<String, DirtyCascade>,
    last_fetched: HashMap<String, Instant>,
    monitor_cache: HashMap<String, CascadeFoldState>,
}

impl CollectorState {
    fn new(collector_boot_utc: DateTime<Utc>, watermarks: Vec<LsSyncWatermark>) -> Self {
        let mut persisted_watermarks = HashMap::with_capacity(watermarks.len());
        for wm in watermarks {
            persisted_watermarks.insert(wm.cascade_id.clone(), wm);
        }
        Self {
            collector_boot_utc,
            boot_settled: false,
            unauthenticated: false,
            warned_unauthenticated: false,
            circuit_open_until: None,
            catalog: ModelCatalog::default(),
            persisted_watermarks,
            boot_baselines: HashMap::new(),
            summaries: HashMap::new(),
            children_by_root: HashMap::new(),
            dirty_cascades: HashMap::new(),
            last_fetched: HashMap::new(),
            monitor_cache: HashMap::new(),
        }
    }

    fn is_circuit_open(&self, now: Instant) -> bool {
        self.circuit_open_until.is_some_and(|until| now < until)
    }

    fn is_active(&self, now: Instant) -> bool {
        !self.unauthenticated && !self.is_circuit_open(now)
    }

    fn trip_circuit_breaker(&mut self, cooldown: Duration, now: Instant) {
        self.circuit_open_until = Some(now + cooldown);
    }

    fn mark_unauthenticated(&mut self) -> bool {
        self.unauthenticated = true;
        if !self.warned_unauthenticated {
            self.warned_unauthenticated = true;
            true
        } else {
            false
        }
    }

    fn update_summary_index(&mut self, cid: &str, summary: &LsTrajectorySummary) {
        if let Some(meta) = summary.trajectory_metadata.as_ref() {
            let parent = meta
                .parent_conversation_id
                .as_deref()
                .and_then(sanitize_conversation_id);
            let root = meta
                .root_conversation_id
                .as_deref()
                .and_then(sanitize_conversation_id)
                .or(parent);
            if let Some(root_id) = root
                && root_id != cid
            {
                self.children_by_root
                    .entry(root_id)
                    .or_default()
                    .insert(cid.to_string());
            }
        }
        self.summaries.insert(cid.to_string(), summary.clone());
    }

    fn remove_cascade(&mut self, cid: &str) {
        self.summaries.remove(cid);
        self.boot_baselines.remove(cid);
        self.dirty_cascades.remove(cid);
        self.monitor_cache.remove(cid);
        self.children_by_root.remove(cid);
        for children in self.children_by_root.values_mut() {
            children.remove(cid);
        }
    }

    /// Seeds initial snapshot frame into `boot_baselines` with ZERO `GetCascadeTrajectorySteps` calls.
    fn apply_boot_snapshot_frame(&mut self, delta: LsSummaryDelta) {
        for deleted in delta.deletes {
            self.remove_cascade(&deleted);
        }
        for (cid, summary) in delta.updates {
            if is_side_question_summary(&summary) {
                continue;
            }
            self.update_summary_index(&cid, &summary);
            let trajectory_id = summary.trajectory_id.unwrap_or_default();
            self.boot_baselines.insert(
                cid,
                BootBaseline {
                    trajectory_id,
                    step_count: summary.step_count,
                },
            );
        }
    }

    /// Processes a post-settle live delta, enqueuing ONLY mutated, reverted, or brand-new
    /// cascades (`created_time >= collector_boot_utc - grace`).
    fn apply_live_delta(
        &mut self,
        delta: LsSummaryDelta,
        grace_secs: i64,
        from_fallback_poll: bool,
    ) -> usize {
        for deleted in delta.deletes {
            self.remove_cascade(&deleted);
        }

        let mut enqueued = 0usize;
        let grace_cutoff = self.collector_boot_utc - chrono::Duration::seconds(grace_secs);

        for (cid, summary) in delta.updates {
            if is_side_question_summary(&summary) {
                continue;
            }
            self.update_summary_index(&cid, &summary);
            let traj_id = summary.trajectory_id.clone().unwrap_or_default();
            let step_count = summary.step_count;

            if let Some(wm) = self.persisted_watermarks.get(&cid).cloned() {
                let reverted = (!traj_id.is_empty()
                    && !wm.trajectory_id.is_empty()
                    && traj_id != wm.trajectory_id)
                    || step_count < wm.step_offset;
                if reverted {
                    self.monitor_cache.remove(&cid);
                    self.persisted_watermarks.insert(
                        cid.clone(),
                        LsSyncWatermark {
                            cascade_id: cid.clone(),
                            trajectory_id: traj_id,
                            step_offset: 0,
                            last_modified_utc: summary
                                .last_modified_time
                                .clone()
                                .unwrap_or_else(|| wm.last_modified_utc.clone()),
                            updated_at_utc: Utc::now().to_rfc3339_opts(SecondsFormat::Secs, true),
                        },
                    );
                    self.enqueue_dirty(cid, 0, true, from_fallback_poll);
                    enqueued += 1;
                } else if step_count > wm.step_offset {
                    self.enqueue_dirty(cid, wm.step_offset, false, from_fallback_poll);
                    enqueued += 1;
                }
            } else if let Some(baseline) = self.boot_baselines.get(&cid).cloned() {
                let reverted = (!traj_id.is_empty()
                    && !baseline.trajectory_id.is_empty()
                    && traj_id != baseline.trajectory_id)
                    || step_count < baseline.step_count;
                if reverted {
                    self.monitor_cache.remove(&cid);
                    self.boot_baselines.insert(
                        cid.clone(),
                        BootBaseline {
                            trajectory_id: traj_id,
                            step_count: 0,
                        },
                    );
                    self.enqueue_dirty(cid, 0, true, from_fallback_poll);
                    enqueued += 1;
                } else if step_count > baseline.step_count {
                    self.enqueue_dirty(cid, baseline.step_count, false, from_fallback_poll);
                    enqueued += 1;
                }
            } else {
                // Brand-new cascade not seen during boot settle.
                let created_dt = parse_rfc3339_utc(summary.created_time.as_deref());
                let is_fresh_post_boot = match created_dt {
                    Some(dt) => dt >= grace_cutoff,
                    None => step_count <= 20,
                };
                if is_fresh_post_boot {
                    self.boot_baselines.insert(
                        cid.clone(),
                        BootBaseline {
                            trajectory_id: traj_id,
                            step_count: 0,
                        },
                    );
                    if step_count > 0 {
                        self.enqueue_dirty(cid, 0, false, from_fallback_poll);
                        enqueued += 1;
                    }
                } else {
                    // Historical cascade arriving late: seed baseline without cold backfill.
                    self.boot_baselines.insert(
                        cid,
                        BootBaseline {
                            trajectory_id: traj_id,
                            step_count,
                        },
                    );
                }
            }
        }

        enqueued
    }

    fn enqueue_dirty(
        &mut self,
        cid: String,
        start_offset: u32,
        reset_fold: bool,
        from_fallback_poll: bool,
    ) {
        self.dirty_cascades
            .entry(cid)
            .and_modify(|existing| {
                if reset_fold {
                    existing.start_offset = 0;
                    existing.reset_fold = true;
                } else {
                    existing.start_offset = existing.start_offset.min(start_offset);
                }
                existing.from_fallback_poll |= from_fallback_poll;
            })
            .or_insert(DirtyCascade {
                start_offset,
                reset_fold,
                from_fallback_poll,
            });
    }

    fn take_eligible_dirty(
        &mut self,
        now: Instant,
        debounce: Duration,
        max_per_tick: usize,
    ) -> Vec<(String, DirtyCascade)> {
        let mut eligible: Vec<String> = self
            .dirty_cascades
            .keys()
            .filter(|cid| {
                self.last_fetched
                    .get(*cid)
                    .is_none_or(|last| now.saturating_duration_since(*last) >= debounce)
            })
            .cloned()
            .collect();
        eligible.sort();
        eligible.truncate(max_per_tick);

        let mut taken = Vec::with_capacity(eligible.len());
        for cid in eligible {
            if let Some(entry) = self.dirty_cascades.remove(&cid) {
                self.last_fetched.insert(cid.clone(), now);
                taken.push((cid, entry));
            }
        }
        taken
    }

    fn select_descendants(&self, root_cid: &str, max_descendants: usize) -> Vec<String> {
        let Some(children) = self.children_by_root.get(root_cid) else {
            return Vec::new();
        };
        let mut sorted: Vec<(String, String)> = children
            .iter()
            .filter_map(|child_id| {
                let summary = self.summaries.get(child_id);
                if summary.is_some_and(is_side_question_summary) {
                    return None;
                }
                let modified = summary
                    .and_then(|s| s.last_modified_time.clone())
                    .unwrap_or_default();
                Some((modified, child_id.clone()))
            })
            .collect();
        // Sort most-recently-modified first, tie-break deterministically by cascade_id.
        sorted.sort_by(|a, b| b.0.cmp(&a.0).then_with(|| a.1.cmp(&b.1)));
        sorted.truncate(max_descendants);
        sorted.into_iter().map(|(_, cid)| cid).collect()
    }
}

// ---------------------------------------------------------------------------
// Public Handle & Background Worker
// ---------------------------------------------------------------------------

struct LsCollectorInner {
    client: LsClient,
    sqlite_sink: Arc<SqliteUsageSink>,
    config: LsCollectorConfig,
    state: Mutex<CollectorState>,
    step_semaphore: Arc<Semaphore>,
    dirty_notify: Notify,
}

#[derive(Clone)]
pub struct LsCollectorHandle {
    inner: Arc<LsCollectorInner>,
    holds_writer_lock: bool,
}

impl std::fmt::Debug for LsCollectorHandle {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("LsCollectorHandle")
            .field("endpoint", self.inner.client.endpoint())
            .field("holds_writer_lock", &self.holds_writer_lock)
            .finish_non_exhaustive()
    }
}

/// Spawns `LsCollector` from `ANTIGRAVITY_LS_ADDRESS` and `ANTIGRAVITY_CSRF_TOKEN`.
///
/// Returns `None` cleanly when LS env vars are absent or invalid (standalone `agy` CLI
/// or detached daemon), never logging any secret.
pub fn spawn_ls_collector_from_env(
    sqlite_sink: Arc<SqliteUsageSink>,
    shutdown_rx: watch::Receiver<bool>,
) -> Option<LsCollectorHandle> {
    match LsClient::from_env() {
        Ok(Some(client)) => Some(spawn_ls_collector(
            client,
            sqlite_sink,
            LsCollectorConfig::default(),
            shutdown_rx,
        )),
        Ok(None) => None,
        Err(err) => {
            tracing::warn!(
                error = %err,
                "Antigravity LS telemetry disabled due to invalid LS environment configuration"
            );
            None
        }
    }
}

/// Spawns a per-window `LsCollector` background task and returns its shared `LsCollectorHandle`.
pub fn spawn_ls_collector(
    client: LsClient,
    sqlite_sink: Arc<SqliteUsageSink>,
    config: LsCollectorConfig,
    shutdown_rx: watch::Receiver<bool>,
) -> LsCollectorHandle {
    let ls_port = client.endpoint().port();
    let writer_lock = match PerPortCollectorLock::try_acquire(sqlite_sink.usage_dir(), ls_port) {
        Ok(Some(lock)) => Some(lock),
        Ok(None) => {
            tracing::info!(
                ls_port,
                "another agy-companion sidecar holds LS writer lock; running in Monitor-only hydration mode"
            );
            None
        }
        Err(err) => {
            tracing::warn!(
                ls_port,
                error = %err,
                "failed to acquire per-port LS collector lock; running in Monitor-only hydration mode"
            );
            None
        }
    };
    let holds_writer_lock = writer_lock.is_some();

    let initial_watermarks = match sqlite_sink.load_all_watermarks() {
        Ok(wms) => wms,
        Err(err) => {
            tracing::warn!(error = %err, "failed to load persisted LS watermarks at startup");
            Vec::new()
        }
    };

    let cutoff = (Utc::now() - chrono::Duration::days(config.watermark_retention_days))
        .to_rfc3339_opts(SecondsFormat::Secs, true);
    if let Err(err) = sqlite_sink.prune_stale_watermarks(&cutoff) {
        tracing::debug!(error = %err, "failed to prune stale LS watermarks at startup");
    }

    let inner = Arc::new(LsCollectorInner {
        client,
        sqlite_sink,
        step_semaphore: Arc::new(Semaphore::new(config.max_in_flight_steps.max(1))),
        config,
        state: Mutex::new(CollectorState::new(Utc::now(), initial_watermarks)),
        dirty_notify: Notify::new(),
    });

    let handle = LsCollectorHandle {
        inner: Arc::clone(&inner),
        holds_writer_lock,
    };

    if holds_writer_lock {
        let bg_inner = Arc::clone(&inner);
        tokio::spawn(async move {
            let _lock_guard = writer_lock;
            run_background_collector(bg_inner, shutdown_rx).await;
        });
    } else {
        // Non-lock-holder still warms the model catalog once for on-demand Monitor hydration.
        let warm_inner = Arc::clone(&inner);
        tokio::spawn(async move {
            let _ = refresh_catalog(&warm_inner, false).await;
        });
    }

    handle
}

impl LsCollectorHandle {
    pub fn holds_writer_lock(&self) -> bool {
        self.holds_writer_lock
    }

    pub async fn is_active(&self) -> bool {
        let guard = self.inner.state.lock().await;
        guard.is_active(Instant::now())
    }

    /// Builds a unified `StatsSnapshot` for the focused `conversation_id` (plus up to 32
    /// most-recent descendants when focused on a root cascade), running `cold_start`/
    /// `cache_break` folding independently per `cascade_id`.
    ///
    /// Returns `None` when the collector is unauthenticated or circuit-broken so callers
    /// fail open cleanly to `:24141/api/stats`.
    pub async fn session_stats(&self, conversation_id: Option<&str>) -> Option<StatsSnapshot> {
        {
            let guard = self.inner.state.lock().await;
            if !guard.is_active(Instant::now()) {
                return None;
            }
        }

        // Ensure model catalog has been fetched at least once (also surfaces discovery/catalog auth errors).
        if !self.inner.state.lock().await.catalog.has_loaded() {
            let _ = refresh_catalog(&self.inner, false).await;
            let guard = self.inner.state.lock().await;
            if !guard.is_active(Instant::now()) {
                return None;
            }
        }

        let focused = conversation_id.and_then(sanitize_conversation_id);
        let Some(focused_cid) = focused else {
            let guard = self.inner.state.lock().await;
            if !guard.is_active(Instant::now()) {
                return None;
            }
            let folds: Vec<&CascadeFoldState> = guard.monitor_cache.values().collect();
            return Some(merge_cascade_folds(&folds));
        };

        let target_cids =
            {
                let guard = self.inner.state.lock().await;
                if !guard.is_active(Instant::now()) {
                    return None;
                }
                let mut targets = vec![focused_cid.clone()];
                let is_subagent = guard
                    .summaries
                    .get(&focused_cid)
                    .and_then(|s| s.trajectory_metadata.as_ref())
                    .and_then(|m| m.parent_conversation_id.as_deref())
                    .and_then(sanitize_conversation_id)
                    .is_some();
                if !is_subagent {
                    targets.extend(guard.select_descendants(
                        &focused_cid,
                        self.inner.config.max_monitor_descendants,
                    ));
                }
                targets
            };

        for cid in &target_cids {
            if let Err(LsError::Unauthenticated { .. }) =
                hydrate_cascade_for_monitor(&self.inner, cid).await
            {
                return None;
            }
        }

        let guard = self.inner.state.lock().await;
        if !guard.is_active(Instant::now()) {
            return None;
        }
        let folds: Vec<&CascadeFoldState> = target_cids
            .iter()
            .filter_map(|cid| guard.monitor_cache.get(cid))
            .collect();
        Some(merge_cascade_folds(&folds))
    }
}

// ---------------------------------------------------------------------------
// Internal Async Helpers
// ---------------------------------------------------------------------------

async fn handle_catalog_or_discovery_error(inner: &LsCollectorInner, err: &LsError) {
    match err {
        LsError::Unauthenticated { rpc } => {
            let should_warn = inner.state.lock().await.mark_unauthenticated();
            if should_warn {
                tracing::warn!(
                    rpc,
                    "Antigravity LS returned 401 Unauthenticated; pausing LS telemetry collector and failing open"
                );
            }
        }
        LsError::PermissionDenied { rpc } | LsError::Unimplemented { rpc } => {
            inner
                .state
                .lock()
                .await
                .trip_circuit_breaker(inner.config.circuit_breaker_cooldown, Instant::now());
            tracing::warn!(
                rpc,
                cooldown_secs = inner.config.circuit_breaker_cooldown.as_secs(),
                "Antigravity LS catalog/discovery RPC rejected; opening circuit breaker and failing open"
            );
        }
        other => {
            tracing::debug!(error = %other, "transient Antigravity LS catalog/discovery error");
        }
    }
}

async fn refresh_catalog(inner: &LsCollectorInner, force_rate_limit: bool) -> bool {
    if force_rate_limit {
        let guard = inner.state.lock().await;
        if !guard
            .catalog
            .can_refresh(inner.config.catalog_refresh_min_interval, Instant::now())
        {
            return false;
        }
    }
    match inner.client.get_cascade_model_config_data().await {
        Ok(configs) => {
            let mut guard = inner.state.lock().await;
            guard.catalog.update(configs, Instant::now());
            true
        }
        Err(err) => {
            handle_catalog_or_discovery_error(inner, &err).await;
            false
        }
    }
}

async fn hydrate_cascade_for_monitor(
    inner: &LsCollectorInner,
    cascade_id: &str,
) -> Result<(), LsError> {
    let (start_offset, reset_fold, summary_opt, traj_id) = {
        let guard = inner.state.lock().await;
        let summary_opt = guard.summaries.get(cascade_id).cloned();
        let summary_traj = summary_opt
            .as_ref()
            .and_then(|s| s.trajectory_id.clone())
            .unwrap_or_default();
        let summary_steps = summary_opt.as_ref().map(|s| s.step_count).unwrap_or(0);

        match guard.monitor_cache.get(cascade_id) {
            Some(existing) => {
                let reverted = (!summary_traj.is_empty()
                    && !existing.trajectory_id.is_empty()
                    && summary_traj != existing.trajectory_id)
                    || (summary_opt.is_some() && summary_steps < existing.step_offset);
                if reverted {
                    (0, true, summary_opt, summary_traj)
                } else if summary_opt.is_some() && summary_steps <= existing.step_offset {
                    return Ok(());
                } else {
                    (existing.step_offset, false, summary_opt, summary_traj)
                }
            }
            None => {
                let initial_offset = if summary_steps > LARGE_CASCADE_HYDRATION_THRESHOLD {
                    summary_steps.saturating_sub(LARGE_CASCADE_TAIL_WINDOW)
                } else {
                    0
                };
                (initial_offset, true, summary_opt, summary_traj)
            }
        }
    };

    let _permit = match inner.step_semaphore.clone().acquire_owned().await {
        Ok(p) => p,
        Err(_) => return Ok(()),
    };

    let steps = match inner
        .client
        .get_cascade_trajectory_steps(cascade_id, start_offset)
        .await
    {
        Ok(steps) => steps,
        Err(LsError::Unauthenticated { rpc }) => {
            let should_warn = inner.state.lock().await.mark_unauthenticated();
            if should_warn {
                tracing::warn!(
                    rpc,
                    "Antigravity LS returned 401 Unauthenticated during Monitor hydration; pausing LS collector"
                );
            }
            return Err(LsError::Unauthenticated { rpc });
        }
        Err(err) => {
            // Single-cascade 404/Unimplemented or transient error MUST NOT trip the circuit breaker.
            tracing::debug!(
                cascade_id,
                error = %err,
                "on-demand LS step hydration skipped"
            );
            return Ok(());
        }
    };

    let mut batch = {
        let guard = inner.state.lock().await;
        process_steps_slice(
            cascade_id,
            summary_opt.as_ref(),
            start_offset,
            &steps,
            &guard.catalog,
            None,
            Utc::now(),
        )
    };

    if batch.unknown_model_enum.is_some() && refresh_catalog(inner, true).await {
        let guard = inner.state.lock().await;
        batch = process_steps_slice(
            cascade_id,
            summary_opt.as_ref(),
            start_offset,
            &steps,
            &guard.catalog,
            None,
            Utc::now(),
        );
    }

    let mut guard = inner.state.lock().await;
    let fold = if reset_fold {
        guard.monitor_cache.insert(
            cascade_id.to_string(),
            CascadeFoldState::new(traj_id, start_offset),
        );
        guard
            .monitor_cache
            .get_mut(cascade_id)
            .expect("inserted immediately above")
    } else {
        guard
            .monitor_cache
            .entry(cascade_id.to_string())
            .or_insert_with(|| CascadeFoldState::new(traj_id.clone(), start_offset))
    };
    fold.apply_batch(&batch);
    Ok(())
}

async fn ingest_dirty_cascade(
    inner: Arc<LsCollectorInner>,
    cascade_id: String,
    dirty: DirtyCascade,
) {
    let _permit = match inner.step_semaphore.clone().acquire_owned().await {
        Ok(p) => p,
        Err(_) => return,
    };

    let summary_opt = inner.state.lock().await.summaries.get(&cascade_id).cloned();
    if summary_opt.as_ref().is_some_and(is_side_question_summary) {
        return;
    }

    let steps = match inner
        .client
        .get_cascade_trajectory_steps(&cascade_id, dirty.start_offset)
        .await
    {
        Ok(steps) => steps,
        Err(LsError::Unauthenticated { rpc }) => {
            let should_warn = inner.state.lock().await.mark_unauthenticated();
            if should_warn {
                tracing::warn!(
                    rpc,
                    "Antigravity LS returned 401 Unauthenticated during step ingestion; pausing LS collector"
                );
            }
            return;
        }
        Err(err) => {
            // Single-cascade 404/Unimplemented or transient error MUST NOT trip the circuit breaker.
            tracing::debug!(
                cascade_id = %cascade_id,
                error = %err,
                "LS GetCascadeTrajectorySteps failed for cascade; skipping without tripping circuit breaker"
            );
            return;
        }
    };

    let min_step_created_at = if dirty.from_fallback_poll {
        summary_opt
            .as_ref()
            .and_then(|s| parse_rfc3339_utc(s.created_time.as_deref()))
    } else {
        None
    };

    let now_utc = Utc::now();
    let mut batch = {
        let guard = inner.state.lock().await;
        process_steps_slice(
            &cascade_id,
            summary_opt.as_ref(),
            dirty.start_offset,
            &steps,
            &guard.catalog,
            min_step_created_at,
            now_utc,
        )
    };

    if batch.unknown_model_enum.is_some() && refresh_catalog(&inner, true).await {
        let guard = inner.state.lock().await;
        batch = process_steps_slice(
            &cascade_id,
            summary_opt.as_ref(),
            dirty.start_offset,
            &steps,
            &guard.catalog,
            min_step_created_at,
            now_utc,
        );
    }

    let events: Vec<UsageEvent> = batch
        .entries
        .iter()
        .flat_map(|entry| match entry {
            NormalizedStepEntry::Checkpoint { calls, .. }
            | NormalizedStepEntry::Planner { calls, .. } => calls.iter().map(|c| c.event.clone()),
        })
        .collect();

    let traj_id = summary_opt
        .as_ref()
        .and_then(|s| s.trajectory_id.clone())
        .unwrap_or_default();
    let now_str = now_utc.to_rfc3339_opts(SecondsFormat::Secs, true);
    let last_modified_utc = summary_opt
        .as_ref()
        .and_then(|s| s.last_modified_time.as_deref())
        .and_then(normalize_utc_timestamp)
        .unwrap_or_else(|| now_str.clone());

    let watermark = LsSyncWatermark {
        cascade_id: cascade_id.clone(),
        trajectory_id: traj_id.clone(),
        step_offset: batch.new_step_offset,
        last_modified_utc,
        updated_at_utc: now_str,
    };

    let sink = Arc::clone(&inner.sqlite_sink);
    let wm_for_db = watermark.clone();
    let persist_res =
        tokio::task::spawn_blocking(move || sink.record_batch_checked(&events, Some(&wm_for_db)))
            .await;

    match persist_res {
        Ok(Ok(_affected)) => {}
        Ok(Err(err)) => {
            tracing::warn!(
                cascade_id = %cascade_id,
                error = %err,
                "failed to persist LS usage batch to SQLite"
            );
            return;
        }
        Err(err) => {
            tracing::warn!(
                cascade_id = %cascade_id,
                error = %err,
                "SQLite persistence task panicked"
            );
            return;
        }
    }

    let mut guard = inner.state.lock().await;
    guard
        .persisted_watermarks
        .insert(cascade_id.clone(), watermark);

    if dirty.reset_fold {
        guard.monitor_cache.insert(
            cascade_id.clone(),
            CascadeFoldState::new(traj_id, dirty.start_offset),
        );
    }
    if let Some(fold) = guard.monitor_cache.get_mut(&cascade_id) {
        fold.apply_batch(&batch);
    }

    // If an in-flight GENERATING step held the watermark back, re-enqueue for the next debounced check.
    if batch.hit_non_terminal {
        guard.enqueue_dirty(
            cascade_id,
            batch.new_step_offset,
            false,
            dirty.from_fallback_poll,
        );
    }
}

fn spawn_dirty_flush(inner: &Arc<LsCollectorInner>, items: Vec<(String, DirtyCascade)>) {
    for (cid, dirty) in items {
        let task_inner = Arc::clone(inner);
        tokio::spawn(async move {
            ingest_dirty_cascade(task_inner, cid, dirty).await;
        });
    }
}

fn should_fallback_from_summaries_error(err: &LsError) -> bool {
    match err {
        LsError::Unimplemented { .. } => true,
        LsError::ConnectError { code, .. } => matches!(
            code.as_str(),
            "unimplemented" | "failed_precondition" | "unavailable" | "internal"
        ),
        _ => false,
    }
}

async fn sleep_or_shutdown(duration: Duration, shutdown_rx: &mut watch::Receiver<bool>) -> bool {
    if *shutdown_rx.borrow() {
        return true;
    }
    tokio::select! {
        _ = tokio::time::sleep(duration) => false,
        res = shutdown_rx.changed() => res.is_err() || *shutdown_rx.borrow(),
    }
}

async fn run_background_collector(
    inner: Arc<LsCollectorInner>,
    mut shutdown_rx: watch::Receiver<bool>,
) {
    if *shutdown_rx.borrow() {
        return;
    }

    // Initial catalog load at boot.
    let _ = refresh_catalog(&inner, false).await;

    let mut use_fallback_polling = false;
    let mut backoff = inner.config.reconnect_initial_backoff;

    loop {
        if *shutdown_rx.borrow() {
            return;
        }

        // Respect unauthenticated pause or circuit breaker cooldown.
        let wait_cooldown = {
            let guard = inner.state.lock().await;
            if guard.unauthenticated {
                return;
            }
            guard
                .circuit_open_until
                .and_then(|until| until.checked_duration_since(Instant::now()))
        };
        if let Some(wait) = wait_cooldown {
            if sleep_or_shutdown(wait, &mut shutdown_rx).await {
                return;
            }
            let _ = refresh_catalog(&inner, false).await;
            continue;
        }

        if use_fallback_polling {
            match run_fallback_poll_cycle(&inner, &mut shutdown_rx).await {
                FallbackOutcome::Shutdown => return,
                FallbackOutcome::CircuitTripped | FallbackOutcome::Unauthenticated => continue,
            }
        }

        match inner.client.subscribe_to_summaries().await {
            Ok(mut stream) => {
                backoff = inner.config.reconnect_initial_backoff;
                let stream_err = run_summary_stream(&inner, &mut stream, &mut shutdown_rx).await;
                if *shutdown_rx.borrow() {
                    return;
                }
                if let Some(err) = stream_err {
                    if matches!(err, LsError::Unauthenticated { .. }) {
                        handle_catalog_or_discovery_error(&inner, &err).await;
                        return;
                    }
                    if matches!(err, LsError::PermissionDenied { .. }) {
                        handle_catalog_or_discovery_error(&inner, &err).await;
                        continue;
                    }
                    if should_fallback_from_summaries_error(&err) {
                        tracing::info!(
                            error = %err,
                            "JetboxSubscribeToSummaries unavailable; degrading to GetAllCascadeTrajectories polling"
                        );
                        use_fallback_polling = true;
                        continue;
                    }
                    tracing::debug!(
                        error = %err,
                        backoff_ms = backoff.as_millis() as u64,
                        "JetboxSubscribeToSummaries stream interrupted; reconnecting"
                    );
                }
            }
            Err(err) => {
                if matches!(err, LsError::Unauthenticated { .. }) {
                    handle_catalog_or_discovery_error(&inner, &err).await;
                    return;
                }
                if matches!(err, LsError::PermissionDenied { .. }) {
                    handle_catalog_or_discovery_error(&inner, &err).await;
                    continue;
                }
                if should_fallback_from_summaries_error(&err) {
                    tracing::info!(
                        error = %err,
                        "JetboxSubscribeToSummaries unavailable; degrading to GetAllCascadeTrajectories polling"
                    );
                    use_fallback_polling = true;
                    continue;
                }
                tracing::debug!(
                    error = %err,
                    backoff_ms = backoff.as_millis() as u64,
                    "failed to open JetboxSubscribeToSummaries stream; retrying with backoff"
                );
            }
        }

        if sleep_or_shutdown(backoff, &mut shutdown_rx).await {
            return;
        }
        backoff = (backoff * 2).min(inner.config.reconnect_max_backoff);
    }
}

async fn run_summary_stream(
    inner: &Arc<LsCollectorInner>,
    stream: &mut companion_providers::LsSummaryStream,
    shutdown_rx: &mut watch::Receiver<bool>,
) -> Option<LsError> {
    // Phase 1: Multi-frame initial snapshot settle (300ms idle quiet, capped at 5s total).
    let already_settled = inner.state.lock().await.boot_settled;
    if !already_settled {
        let settle_deadline = Instant::now() + inner.config.snapshot_settle_max;
        loop {
            let now = Instant::now();
            let Some(remaining_cap) = settle_deadline.checked_duration_since(now) else {
                break;
            };
            let wait = inner.config.snapshot_settle_idle.min(remaining_cap);
            tokio::select! {
                res = shutdown_rx.changed() => {
                    if res.is_err() || *shutdown_rx.borrow() {
                        return None;
                    }
                }
                frame = tokio::time::timeout(wait, stream.next_delta()) => {
                    match frame {
                        Ok(Ok(Some(delta))) => {
                            inner.state.lock().await.apply_boot_snapshot_frame(delta);
                        }
                        Ok(Ok(None)) => {
                            inner.state.lock().await.boot_settled = true;
                            return None;
                        }
                        Ok(Err(err)) => {
                            return Some(err);
                        }
                        Err(_) => {
                            // Idle quiet window elapsed: all initial snapshot frames received.
                            break;
                        }
                    }
                }
            }
        }
        inner.state.lock().await.boot_settled = true;
    }

    // Phase 2: Live delta processing + debounced bounded step ingestion.
    let mut flush_tick = tokio::time::interval(SCHEDULER_TICK_INTERVAL);
    flush_tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

    loop {
        tokio::select! {
            res = shutdown_rx.changed() => {
                if res.is_err() || *shutdown_rx.borrow() {
                    return None;
                }
            }
            frame = stream.next_delta() => {
                match frame {
                    Ok(Some(delta)) => {
                        let enqueued = {
                            let mut guard = inner.state.lock().await;
                            guard.apply_live_delta(
                                delta,
                                inner.config.boot_new_cascade_grace_secs,
                                false,
                            )
                        };
                        if enqueued > 0 {
                            inner.dirty_notify.notify_one();
                        }
                    }
                    Ok(None) => return None,
                    Err(err) => return Some(err),
                }
            }
            _ = inner.dirty_notify.notified() => {
                let items = inner.state.lock().await.take_eligible_dirty(
                    Instant::now(),
                    inner.config.cascade_debounce,
                    inner.config.max_cascades_per_tick,
                );
                if !items.is_empty() {
                    spawn_dirty_flush(inner, items);
                }
            }
            _ = flush_tick.tick() => {
                let items = inner.state.lock().await.take_eligible_dirty(
                    Instant::now(),
                    inner.config.cascade_debounce,
                    inner.config.max_cascades_per_tick,
                );
                if !items.is_empty() {
                    spawn_dirty_flush(inner, items);
                }
            }
        }
    }
}

enum FallbackOutcome {
    Shutdown,
    CircuitTripped,
    Unauthenticated,
}

async fn run_fallback_poll_cycle(
    inner: &Arc<LsCollectorInner>,
    shutdown_rx: &mut watch::Receiver<bool>,
) -> FallbackOutcome {
    let mut flush_tick = tokio::time::interval(SCHEDULER_TICK_INTERVAL);
    flush_tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

    loop {
        if *shutdown_rx.borrow() {
            return FallbackOutcome::Shutdown;
        }

        match inner.client.get_all_cascade_trajectories(false).await {
            Ok(summaries) => {
                let delta = LsSummaryDelta {
                    updates: summaries,
                    deletes: Vec::new(),
                };
                let mut guard = inner.state.lock().await;
                if !guard.boot_settled {
                    guard.apply_boot_snapshot_frame(delta);
                    guard.boot_settled = true;
                } else {
                    let enqueued = guard.apply_live_delta(
                        delta,
                        inner.config.boot_new_cascade_grace_secs,
                        true,
                    );
                    if enqueued > 0 {
                        inner.dirty_notify.notify_one();
                    }
                }
            }
            Err(err) => {
                let is_unauth = matches!(err, LsError::Unauthenticated { .. });
                let is_circuit = matches!(
                    err,
                    LsError::PermissionDenied { .. } | LsError::Unimplemented { .. }
                );
                handle_catalog_or_discovery_error(inner, &err).await;
                if is_unauth {
                    return FallbackOutcome::Unauthenticated;
                }
                if is_circuit {
                    return FallbackOutcome::CircuitTripped;
                }
            }
        }

        let next_poll = Instant::now() + inner.config.fallback_poll_interval;
        while let Some(remaining) = next_poll.checked_duration_since(Instant::now()) {
            tokio::select! {
                res = shutdown_rx.changed() => {
                    if res.is_err() || *shutdown_rx.borrow() {
                        return FallbackOutcome::Shutdown;
                    }
                }
                _ = inner.dirty_notify.notified() => {
                    let items = inner.state.lock().await.take_eligible_dirty(
                        Instant::now(),
                        inner.config.cascade_debounce,
                        inner.config.max_cascades_per_tick,
                    );
                    if !items.is_empty() {
                        spawn_dirty_flush(inner, items);
                    }
                }
                _ = flush_tick.tick() => {
                    let items = inner.state.lock().await.take_eligible_dirty(
                        Instant::now(),
                        inner.config.cascade_debounce,
                        inner.config.max_cascades_per_tick,
                    );
                    if !items.is_empty() {
                        spawn_dirty_flush(inner, items);
                    }
                }
                _ = tokio::time::sleep(remaining) => {
                    break;
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicUsize, Ordering};

    use axum::Router;
    use axum::body::Body;
    use axum::http::{HeaderValue, StatusCode, header};
    use axum::response::Response;
    use axum::routing::post;
    use companion_core::{CsrfToken, LsEndpoint};
    use companion_providers::{
        LsModelInfo, LsModelOrAlias, LsResponseHeaderFingerprint, LsRetryInfo, LsTrajectoryMetadata,
    };
    use tempfile::TempDir;
    use tokio::net::TcpListener;

    use super::*;

    const TEST_CSRF: &str = "csrf-ls-collector-unit-test-token-1234";

    fn encode_connect_frame(flags: u8, payload: &[u8]) -> Vec<u8> {
        let mut frame = Vec::with_capacity(5 + payload.len());
        frame.push(flags);
        frame.extend_from_slice(&(payload.len() as u32).to_be_bytes());
        frame.extend_from_slice(payload);
        frame
    }

    fn make_catalog() -> ModelCatalog {
        let mut catalog = ModelCatalog::default();
        catalog.update(
            vec![
                LsModelConfig {
                    label: Some("Gemini 4 Hydrogen (Medium)".to_string()),
                    model_or_alias: Some(LsModelOrAlias {
                        model: Some("MODEL_PLACEHOLDER_M335".to_string()),
                        alias: None,
                    }),
                    provider: Some("MODEL_PROVIDER_GOOGLE".to_string()),
                    model_url: Some("gemini-api://gemini-4-hydrogen".to_string()),
                    model_id: Some("companion/gemini-4-hydrogen-medium".to_string()),
                    context_window_options: Vec::new(),
                },
                LsModelConfig {
                    label: Some("Gemini 4 Hydrogen (High)".to_string()),
                    model_or_alias: Some(LsModelOrAlias {
                        model: Some("MODEL_PLACEHOLDER_M336".to_string()),
                        alias: None,
                    }),
                    provider: Some("MODEL_PROVIDER_GOOGLE".to_string()),
                    model_url: Some("gemini-api://gemini-4-hydrogen".to_string()),
                    model_id: Some("companion/gemini-4-hydrogen-high".to_string()),
                    context_window_options: Vec::new(),
                },
                LsModelConfig {
                    label: Some("Gemini 4 Neon (High)".to_string()),
                    model_or_alias: Some(LsModelOrAlias {
                        model: Some("MODEL_PLACEHOLDER_M467".to_string()),
                        alias: None,
                    }),
                    provider: Some("MODEL_PROVIDER_GOOGLE".to_string()),
                    model_url: None,
                    model_id: Some("gemini-4-neon-high".to_string()),
                    context_window_options: Vec::new(),
                },
                LsModelConfig {
                    label: Some("Claude Opus 5.5 (High)".to_string()),
                    model_or_alias: Some(LsModelOrAlias {
                        model: Some("MODEL_PLACEHOLDER_M501".to_string()),
                        alias: None,
                    }),
                    provider: Some("MODEL_PROVIDER_ANTHROPIC".to_string()),
                    model_url: None,
                    model_id: Some("claude-opus-5-5-high".to_string()),
                    context_window_options: Vec::new(),
                },
                LsModelConfig {
                    label: Some("GPT-5.2 (High)".to_string()),
                    model_or_alias: Some(LsModelOrAlias {
                        model: Some("MODEL_PLACEHOLDER_M601".to_string()),
                        alias: None,
                    }),
                    provider: Some("MODEL_PROVIDER_OPENAI".to_string()),
                    model_url: None,
                    model_id: Some("gpt-5.2-high".to_string()),
                    context_window_options: Vec::new(),
                },
            ],
            Instant::now(),
        );
        catalog
    }

    #[allow(clippy::too_many_arguments)]
    fn make_planner_step(
        status: &str,
        generator_model: &str,
        usage: Option<LsModelUsage>,
        retry_infos: Vec<LsRetryInfo>,
        created_at: &str,
        viewable_at: Option<&str>,
        finished_at: Option<&str>,
        completed_at: Option<&str>,
    ) -> LsTrajectoryStep {
        LsTrajectoryStep {
            step_type: Some(STEP_TYPE_PLANNER_RESPONSE.to_string()),
            status: Some(status.to_string()),
            metadata: Some(LsStepMetadata {
                created_at: Some(created_at.to_string()),
                viewable_at: viewable_at.map(str::to_string),
                finished_generating_at: finished_at.map(str::to_string),
                completed_at: completed_at.map(str::to_string),
                started_at: None,
                generator_model: Some(generator_model.to_string()),
                model_usage: usage,
                retry_infos,
                model_info: None,
            }),
        }
    }

    #[test]
    fn token_and_latency_normalization_matches_golden_fixture() {
        let catalog = make_catalog();
        let usage = LsModelUsage {
            model: Some("MODEL_PLACEHOLDER_M335".to_string()),
            input_tokens: 2_177,
            output_tokens: 335,
            cache_read_tokens: 32_952,
            cache_write_tokens: 0,
            response_output_tokens: 283,
            thinking_output_tokens: 52,
            api_provider: Some("API_PROVIDER_GOOGLE_GEMINI".to_string()),
            response_id: Some("G4EIa_golden_1".to_string()),
            response_header: Some(LsResponseHeaderFingerprint {
                response_id: Some("G4EIa_golden_1".to_string()),
                ..Default::default()
            }),
            ..Default::default()
        };

        let step = make_planner_step(
            "CORTEX_STEP_STATUS_DONE",
            "MODEL_PLACEHOLDER_M335",
            Some(usage),
            Vec::new(),
            "2026-11-26T10:00:00.000000Z",
            Some("2026-11-26T10:00:00.410000Z"),
            Some("2026-11-26T10:00:02.811000-00:00"),
            Some("2026-11-26T10:00:02.820000Z"),
        );

        let batch = process_steps_slice(
            "a1262862-root",
            None,
            0,
            &[step],
            &catalog,
            None,
            Utc::now(),
        );
        assert_eq!(batch.new_step_offset, 1);
        assert!(!batch.hit_non_terminal);
        assert_eq!(batch.entries.len(), 1);
        let NormalizedStepEntry::Planner { calls, .. } = &batch.entries[0] else {
            panic!("expected planner entry");
        };
        assert_eq!(calls.len(), 1);
        let ev = &calls[0].event;
        assert_eq!(ev.input_tokens, 35_129);
        assert_eq!(ev.cached_tokens, 32_952);
        assert_eq!(ev.cache_write_tokens, 0);
        assert_eq!(ev.output_tokens, 283);
        assert_eq!(ev.reasoning_tokens, 52);
        assert_eq!(ev.ttft_ms, Some(410));
        assert_eq!(ev.latency_ms, 2_811);
        assert_eq!(ev.timestamp_utc, "2026-11-26T10:00:02Z");
        assert_eq!(ev.provider, "gemini");
        assert_eq!(ev.source, "proxy");
        assert_eq!(ev.model, "gemini-4-hydrogen");
        assert_eq!(ev.request_id, "ls:a1262862-root:0:0:1795687200000000");
        assert_eq!(ev.response_id.as_deref(), Some("G4EIa_golden_1"));
        assert_eq!(ev.telemetry_source, TelemetrySource::Ls);

        // Fallback when response_output_tokens == 0: output_tokens = output - thinking.
        let fallback_usage = LsModelUsage {
            input_tokens: 1_000,
            cache_read_tokens: 500,
            cache_write_tokens: 250,
            output_tokens: 335,
            response_output_tokens: 0,
            thinking_output_tokens: 52,
            ..Default::default()
        };
        let fallback_step = make_planner_step(
            "CORTEX_STEP_STATUS_DONE",
            "MODEL_PLACEHOLDER_M467",
            Some(fallback_usage),
            Vec::new(),
            "2026-11-26T10:01:00Z",
            None,
            None,
            Some("2026-11-26T10:01:01.500Z"),
        );
        let b2 = process_steps_slice(
            "a1262862-root",
            None,
            1,
            &[fallback_step],
            &catalog,
            None,
            Utc::now(),
        );
        let NormalizedStepEntry::Planner { calls, .. } = &b2.entries[0] else {
            panic!("expected planner");
        };
        assert_eq!(calls[0].event.input_tokens, 1_750);
        assert_eq!(calls[0].event.cache_write_tokens, 250);
        assert_eq!(calls[0].event.output_tokens, 283);
        assert_eq!(calls[0].event.latency_ms, 1_500);
        assert_eq!(calls[0].event.ttft_ms, None);
    }

    #[test]
    fn retry_infos_billing_emits_nonzero_retries_without_double_counting_model_usage() {
        let catalog = make_catalog();
        let final_usage = LsModelUsage {
            input_tokens: 1_000,
            output_tokens: 200,
            response_output_tokens: 200,
            response_id: Some("resp-retry-final".to_string()),
            ..Default::default()
        };
        let partial_failed_retry = LsModelUsage {
            input_tokens: 1_000,
            output_tokens: 40,
            response_output_tokens: 40,
            response_id: Some("resp-retry-partial".to_string()),
            ..Default::default()
        };
        let zero_retry = LsModelUsage::default();

        let step = make_planner_step(
            "CORTEX_STEP_STATUS_DONE",
            "MODEL_PLACEHOLDER_M467",
            Some(final_usage.clone()),
            vec![
                LsRetryInfo {
                    usage: Some(zero_retry),
                    retry_reason: Some("TIMEOUT".to_string()),
                },
                LsRetryInfo {
                    usage: Some(partial_failed_retry),
                    retry_reason: Some("MID_STREAM_RESET".to_string()),
                },
                LsRetryInfo {
                    usage: Some(final_usage),
                    retry_reason: None,
                },
            ],
            "2026-11-26T10:00:00Z",
            None,
            Some("2026-11-26T10:00:02Z"),
            None,
        );

        let batch = process_steps_slice(
            "conv-retries-1",
            None,
            4,
            &[step],
            &catalog,
            None,
            Utc::now(),
        );
        let NormalizedStepEntry::Planner { calls, .. } = &batch.entries[0] else {
            panic!("expected planner");
        };
        // Only the 2 non-zero retryInfos entries are emitted — NEVER summed with modelUsage.
        assert_eq!(calls.len(), 2);
        assert_eq!(
            calls[0].event.request_id,
            "ls:conv-retries-1:4:1:1795687200000000"
        );
        assert_eq!(calls[0].event.output_tokens, 40);
        assert_eq!(
            calls[1].event.request_id,
            "ls:conv-retries-1:4:2:1795687200000000"
        );
        assert_eq!(calls[1].event.output_tokens, 200);
    }

    #[test]
    fn three_tier_attribution_covers_catalog_response_header_and_providers() {
        let catalog = make_catalog();

        // 1. Companion BYOM via catalog
        let byom = resolve_attribution(
            &LsStepMetadata {
                generator_model: Some("MODEL_PLACEHOLDER_M335".to_string()),
                ..Default::default()
            },
            &LsModelUsage {
                api_provider: Some("API_PROVIDER_GOOGLE_GEMINI".to_string()),
                response_header: Some(LsResponseHeaderFingerprint {
                    response_id: Some("r1".to_string()),
                    ..Default::default()
                }),
                ..Default::default()
            },
            &catalog,
        );
        assert_eq!(byom.source, "proxy");
        assert_eq!(byom.provider, "gemini");
        assert_eq!(byom.model, "gemini-4-hydrogen");
        assert_eq!(byom.variant_key, "MODEL_PLACEHOLDER_M335");

        // 2. Built-in Vertex Claude (26) with capital sessionID
        let claude = resolve_attribution(
            &LsStepMetadata {
                generator_model: Some("MODEL_PLACEHOLDER_M501".to_string()),
                ..Default::default()
            },
            &LsModelUsage {
                api_provider: Some("API_PROVIDER_ANTHROPIC_VERTEX".to_string()),
                response_header: Some(LsResponseHeaderFingerprint {
                    session_id_upper: Some("sess-upper".to_string()),
                    ..Default::default()
                }),
                ..Default::default()
            },
            &catalog,
        );
        assert_eq!(claude.source, "builtin");
        assert_eq!(claude.provider, "vertex_claude");
        assert_eq!(claude.model, "claude-opus-5-5");

        // 3. Built-in Vertex OpenAI (31/32)
        let openai = resolve_attribution(
            &LsStepMetadata {
                generator_model: Some("MODEL_PLACEHOLDER_M601".to_string()),
                ..Default::default()
            },
            &LsModelUsage {
                api_provider: Some("32".to_string()),
                response_header: Some(LsResponseHeaderFingerprint {
                    session_id_upper: Some("sess-upper".to_string()),
                    ..Default::default()
                }),
                ..Default::default()
            },
            &catalog,
        );
        assert_eq!(openai.source, "builtin");
        assert_eq!(openai.provider, "vertex_openai");
        assert_eq!(openai.model, "gpt-5.2");

        // 4. Consumer AICode: API_PROVIDER_GOOGLE_GEMINI (24) + capital sessionID -> builtin/antigravity!
        let aicode = resolve_attribution(
            &LsStepMetadata {
                generator_model: Some("MODEL_PLACEHOLDER_M467".to_string()),
                ..Default::default()
            },
            &LsModelUsage {
                api_provider: Some("24".to_string()),
                response_header: Some(LsResponseHeaderFingerprint {
                    session_id_upper: Some("sess-aicode".to_string()),
                    ..Default::default()
                }),
                ..Default::default()
            },
            &catalog,
        );
        assert_eq!(aicode.source, "builtin");
        assert_eq!(aicode.provider, "antigravity");
        assert_eq!(aicode.model, "gemini-4-neon");

        // 5. Direct public AI Studio: responseId + lowercase sessionId -> direct/gemini
        let direct = resolve_attribution(
            &LsStepMetadata {
                model_info: Some(LsModelInfo {
                    model_name: Some("gemini-3.8-pro".to_string()),
                    ..Default::default()
                }),
                ..Default::default()
            },
            &LsModelUsage {
                api_provider: Some("API_PROVIDER_GOOGLE_GEMINI".to_string()),
                response_header: Some(LsResponseHeaderFingerprint {
                    response_id: Some("r-direct".to_string()),
                    session_id_lower: Some("lower-d".to_string()),
                    ..Default::default()
                }),
                ..Default::default()
            },
            &catalog,
        );
        assert_eq!(direct.source, "direct");
        assert_eq!(direct.provider, "gemini");
        assert_eq!(direct.model, "gemini-3.8-pro");

        // 6. Internal BYOM (326) with proxy fingerprint -> proxy/gemini
        let internal_byom = resolve_attribution(
            &LsStepMetadata {
                generator_model: Some("MODEL_GOOGLE_GEMINI_INTERNAL_BYOM".to_string()),
                model_info: Some(LsModelInfo {
                    model_url: Some("gemini-api://gemini-4-carbon".to_string()),
                    ..Default::default()
                }),
                ..Default::default()
            },
            &LsModelUsage {
                api_provider: Some("API_PROVIDER_INTERNAL".to_string()),
                response_header: Some(LsResponseHeaderFingerprint {
                    response_id: Some("r-byom".to_string()),
                    ..Default::default()
                }),
                ..Default::default()
            },
            &catalog,
        );
        assert_eq!(internal_byom.source, "proxy");
        assert_eq!(internal_byom.provider, "gemini");
        assert_eq!(internal_byom.model, "gemini-4-carbon");
    }

    #[test]
    fn status_generating_locks_watermark_until_terminal() {
        let catalog = make_catalog();
        let done_step = make_planner_step(
            "CORTEX_STEP_STATUS_DONE",
            "MODEL_PLACEHOLDER_M467",
            Some(LsModelUsage {
                input_tokens: 100,
                output_tokens: 20,
                response_output_tokens: 20,
                ..Default::default()
            }),
            Vec::new(),
            "2026-11-26T10:00:00Z",
            None,
            Some("2026-11-26T10:00:01Z"),
            None,
        );
        let generating_step = make_planner_step(
            "CORTEX_STEP_STATUS_GENERATING",
            "MODEL_PLACEHOLDER_M467",
            None,
            Vec::new(),
            "2026-11-26T10:00:02Z",
            None,
            None,
            None,
        );
        let done_without_usage_or_completed = make_planner_step(
            "CORTEX_STEP_STATUS_DONE",
            "MODEL_PLACEHOLDER_M467",
            None,
            Vec::new(),
            "2026-11-26T10:00:03Z",
            None,
            None,
            None,
        );

        let batch = process_steps_slice(
            "conv-inflight-1",
            None,
            5,
            &[done_step.clone(), generating_step],
            &catalog,
            None,
            Utc::now(),
        );
        assert_eq!(batch.new_step_offset, 6);
        assert!(batch.hit_non_terminal);
        assert_eq!(batch.entries.len(), 1);

        let batch2 = process_steps_slice(
            "conv-inflight-1",
            None,
            6,
            &[done_without_usage_or_completed],
            &catalog,
            None,
            Utc::now(),
        );
        assert_eq!(batch2.new_step_offset, 6);
        assert!(batch2.hit_non_terminal);
        assert!(batch2.entries.is_empty());

        // Once step 6 completes, watermark advances cleanly to 7.
        let batch3 = process_steps_slice(
            "conv-inflight-1",
            None,
            6,
            std::slice::from_ref(&done_step),
            &catalog,
            None,
            Utc::now(),
        );
        assert_eq!(batch3.new_step_offset, 7);
        assert!(!batch3.hit_non_terminal);
        assert_eq!(batch3.entries.len(), 1);

        // CANCELED and ERROR planner steps without completed_at / model_usage (or metadata)
        // must be immediately terminal so they NEVER pin step_offset in an infinite loop.
        let canceled_step = make_planner_step(
            "CORTEX_STEP_STATUS_CANCELED",
            "MODEL_PLACEHOLDER_M467",
            None,
            Vec::new(),
            "2026-11-26T10:00:04Z",
            None,
            None,
            None,
        );
        let error_step = make_planner_step(
            "CORTEX_STEP_STATUS_ERROR",
            "MODEL_PLACEHOLDER_M467",
            None,
            Vec::new(),
            "2026-11-26T10:00:05Z",
            None,
            None,
            None,
        );
        let numeric_canceled_no_meta = LsTrajectoryStep {
            step_type: Some(STEP_TYPE_PLANNER_RESPONSE.to_string()),
            status: Some("5".to_string()),
            metadata: None,
        };
        let numeric_error_no_meta = LsTrajectoryStep {
            step_type: Some(STEP_TYPE_PLANNER_RESPONSE.to_string()),
            status: Some("4".to_string()),
            metadata: None,
        };
        let done_with_finished_at = make_planner_step(
            "CORTEX_STEP_STATUS_DONE",
            "MODEL_PLACEHOLDER_M467",
            None,
            Vec::new(),
            "2026-11-26T10:00:06Z",
            None,
            Some("2026-11-26T10:00:07Z"),
            None,
        );

        let batch4 = process_steps_slice(
            "conv-inflight-1",
            None,
            7,
            &[
                canceled_step,
                error_step,
                numeric_canceled_no_meta,
                numeric_error_no_meta,
                done_with_finished_at,
                done_step,
            ],
            &catalog,
            None,
            Utc::now(),
        );
        assert_eq!(batch4.new_step_offset, 13);
        assert!(!batch4.hit_non_terminal);
        assert_eq!(batch4.entries.len(), 1);
    }

    #[test]
    fn checkpoint_compaction_and_effort_switch_and_subagent_fold_independently() {
        let catalog = make_catalog();
        let mk_usage = |uncached: u64, cached: u64, out: u64| {
            Some(LsModelUsage {
                input_tokens: uncached,
                cache_read_tokens: cached,
                output_tokens: out,
                response_output_tokens: out,
                api_provider: Some("API_PROVIDER_GOOGLE_GEMINI".to_string()),
                response_header: Some(LsResponseHeaderFingerprint {
                    response_id: Some(format!("resp-{uncached}-{cached}-{out}")),
                    ..Default::default()
                }),
                ..Default::default()
            })
        };

        let root_steps = vec![
            // 0: first turn M335 (Medium) -> cold_start=true, cache_break=false
            make_planner_step(
                "CORTEX_STEP_STATUS_DONE",
                "MODEL_PLACEHOLDER_M335",
                mk_usage(1000, 0, 100),
                Vec::new(),
                "2026-11-26T10:00:00Z",
                None,
                Some("2026-11-26T10:00:01Z"),
                None,
            ),
            // 1: second turn M335 cached=0 (no checkpoint!) -> cold_start=false, cache_break=true
            make_planner_step(
                "CORTEX_STEP_STATUS_DONE",
                "MODEL_PLACEHOLDER_M335",
                mk_usage(1200, 0, 100),
                Vec::new(),
                "2026-11-26T10:00:02Z",
                None,
                Some("2026-11-26T10:00:03Z"),
                None,
            ),
            // 2: CHECKPOINT compaction with its own summarizer usage
            LsTrajectoryStep {
                step_type: Some(STEP_TYPE_CHECKPOINT.to_string()),
                status: Some("CORTEX_STEP_STATUS_DONE".to_string()),
                metadata: Some(LsStepMetadata {
                    created_at: Some("2026-11-26T10:00:04Z".to_string()),
                    finished_generating_at: Some("2026-11-26T10:00:05Z".to_string()),
                    generator_model: Some("MODEL_PLACEHOLDER_M467".to_string()),
                    model_usage: Some(LsModelUsage {
                        input_tokens: 500,
                        output_tokens: 80,
                        response_output_tokens: 80,
                        api_provider: Some("API_PROVIDER_GOOGLE_VERTEX".to_string()),
                        response_header: Some(LsResponseHeaderFingerprint {
                            session_id_upper: Some("sess-cp".to_string()),
                            ..Default::default()
                        }),
                        ..Default::default()
                    }),
                    ..Default::default()
                }),
            },
            // 3: post-compaction M335 cached=0 -> cold_start=true, cache_break=false!
            make_planner_step(
                "CORTEX_STEP_STATUS_DONE",
                "MODEL_PLACEHOLDER_M335",
                mk_usage(800, 0, 90),
                Vec::new(),
                "2026-11-26T10:00:06Z",
                None,
                Some("2026-11-26T10:00:07Z"),
                None,
            ),
            // 4: effort-level switch M335 (Medium) -> M336 (High) cached=0 -> cold_start=true, cache_break=false!
            make_planner_step(
                "CORTEX_STEP_STATUS_DONE",
                "MODEL_PLACEHOLDER_M336",
                mk_usage(900, 0, 110),
                Vec::new(),
                "2026-11-26T10:00:08Z",
                None,
                Some("2026-11-26T10:00:09Z"),
                None,
            ),
        ];

        let root_batch = process_steps_slice(
            "conv-root-001",
            None,
            0,
            &root_steps,
            &catalog,
            None,
            Utc::now(),
        );
        let mut root_fold = CascadeFoldState::new("traj-root".to_string(), 0);
        root_fold.apply_batch(&root_batch);

        assert_eq!(root_fold.recent_calls.len(), 5);
        assert_eq!(
            (
                root_fold.recent_calls[0].cold_start,
                root_fold.recent_calls[0].cache_break
            ),
            (true, false)
        );
        assert_eq!(
            (
                root_fold.recent_calls[1].cold_start,
                root_fold.recent_calls[1].cache_break
            ),
            (false, true)
        );
        // Checkpoint call
        assert_eq!(
            (
                root_fold.recent_calls[2].cold_start,
                root_fold.recent_calls[2].cache_break
            ),
            (false, false)
        );
        // Post-compaction turn -> cold_start, NOT cache_break
        assert_eq!(
            (
                root_fold.recent_calls[3].cold_start,
                root_fold.recent_calls[3].cache_break
            ),
            (true, false)
        );
        // Effort-level switch -> cold_start, NOT cache_break
        assert_eq!(
            (
                root_fold.recent_calls[4].cold_start,
                root_fold.recent_calls[4].cache_break
            ),
            (true, false)
        );

        // Subagent starts fresh on M335 with cached=0 -> cold_start=true, never triggers parent cache_break!
        let sub_step = make_planner_step(
            "CORTEX_STEP_STATUS_DONE",
            "MODEL_PLACEHOLDER_M335",
            mk_usage(600, 0, 70),
            Vec::new(),
            "2026-11-26T10:00:03Z",
            None,
            Some("2026-11-26T10:00:04Z"),
            None,
        );
        let sub_batch = process_steps_slice(
            "conv-sub-001",
            Some(&LsTrajectorySummary {
                trajectory_metadata: Some(LsTrajectoryMetadata {
                    parent_conversation_id: Some("conv-root-001".to_string()),
                    root_conversation_id: Some("conv-root-001".to_string()),
                    nesting_depth: 1,
                }),
                ..Default::default()
            }),
            0,
            &[sub_step],
            &catalog,
            None,
            Utc::now(),
        );
        let mut sub_fold = CascadeFoldState::new("traj-sub".to_string(), 0);
        sub_fold.apply_batch(&sub_batch);
        assert!(sub_fold.recent_calls[0].cold_start);
        assert!(!sub_fold.recent_calls[0].cache_break);

        let merged = merge_cascade_folds(&[&root_fold, &sub_fold]);
        assert_eq!(merged.usage.requests, 6);
        assert_eq!(merged.recent_calls.len(), 6);
        // Chronological timestamp_ms ordering preserved across merged root + subagent.
        for pair in merged.recent_calls.windows(2) {
            assert!(pair[0].timestamp_ms <= pair[1].timestamp_ms);
        }
        // Effort variants M335 & M336 aggregate into canonical base model "gemini-4-hydrogen".
        let hydrogen = merged
            .by_model
            .iter()
            .find(|m| m.model == "gemini-4-hydrogen")
            .expect("hydrogen rollup");
        assert_eq!(hydrogen.requests, 5);
    }

    #[test]
    fn revert_to_step_resets_watermark_and_invalidates_monitor_fold() {
        let boot = Utc::now();
        let mut state = CollectorState::new(
            boot,
            vec![LsSyncWatermark {
                cascade_id: "conv-revert-1".to_string(),
                trajectory_id: "traj-v1".to_string(),
                step_offset: 12,
                last_modified_utc: boot.to_rfc3339_opts(SecondsFormat::Secs, true),
                updated_at_utc: boot.to_rfc3339_opts(SecondsFormat::Secs, true),
            }],
        );
        state.boot_settled = true;
        state.monitor_cache.insert(
            "conv-revert-1".to_string(),
            CascadeFoldState::new("traj-v1".to_string(), 12),
        );

        // Revert changes trajectory_id and shrinks step_count from 12 -> 6.
        let mut updates = HashMap::new();
        updates.insert(
            "conv-revert-1".to_string(),
            LsTrajectorySummary {
                trajectory_id: Some("traj-v2".to_string()),
                step_count: 6,
                ..Default::default()
            },
        );
        let enqueued = state.apply_live_delta(
            LsSummaryDelta {
                updates,
                deletes: Vec::new(),
            },
            5,
            false,
        );
        assert_eq!(enqueued, 1);
        assert!(
            !state.monitor_cache.contains_key("conv-revert-1"),
            "revert must invalidate monitor fold cache"
        );
        let dirty = state
            .dirty_cascades
            .get("conv-revert-1")
            .expect("enqueued dirty");
        assert_eq!(dirty.start_offset, 0);
        assert!(dirty.reset_fold);
    }

    #[test]
    fn revert_to_step_without_response_id_avoids_duplicates_and_persists_new_turn() {
        let catalog = make_catalog();
        let tmp = TempDir::new().expect("tmp");
        let sink = SqliteUsageSink::new(tmp.path().join("usage")).expect("sink");
        let cid = "conv-revert-no-respid-1";

        let make_vertex_claude_step = |created_at: &str, finished_at: &str, inp: u64, out: u64| {
            make_planner_step(
                "CORTEX_STEP_STATUS_DONE",
                "MODEL_PLACEHOLDER_M501",
                Some(LsModelUsage {
                    model: Some("MODEL_PLACEHOLDER_M501".to_string()),
                    input_tokens: inp,
                    output_tokens: out,
                    response_output_tokens: out,
                    api_provider: Some("API_PROVIDER_ANTHROPIC_VERTEX".to_string()),
                    // Intentionally omit response_id (e.g., Vertex Claude / CHECKPOINT with sessionID only).
                    response_id: None,
                    response_header: Some(LsResponseHeaderFingerprint {
                        session_id_upper: Some("sess-claude-vertex".to_string()),
                        ..Default::default()
                    }),
                    ..Default::default()
                }),
                Vec::new(),
                created_at,
                None,
                Some(finished_at),
                Some(finished_at),
            )
        };

        let collect_events = |batch: &ProcessedStepsBatch| -> Vec<UsageEvent> {
            batch
                .entries
                .iter()
                .flat_map(|entry| match entry {
                    NormalizedStepEntry::Checkpoint { calls, .. }
                    | NormalizedStepEntry::Planner { calls, .. } => {
                        calls.iter().map(|c| c.event.clone()).collect::<Vec<_>>()
                    }
                })
                .collect()
        };

        let pre_step0 =
            make_vertex_claude_step("2026-11-26T10:00:00Z", "2026-11-26T10:00:01Z", 1_000, 100);
        let pre_step1 =
            make_vertex_claude_step("2026-11-26T10:01:00Z", "2026-11-26T10:01:01Z", 1_500, 150);

        let summary_v1 = LsTrajectorySummary {
            trajectory_id: Some("traj-v1".to_string()),
            step_count: 2,
            ..Default::default()
        };
        let batch_v1 = process_steps_slice(
            cid,
            Some(&summary_v1),
            0,
            &[pre_step0.clone(), pre_step1],
            &catalog,
            None,
            Utc::now(),
        );
        let events_v1 = collect_events(&batch_v1);
        assert_eq!(events_v1.len(), 2);
        assert!(events_v1.iter().all(|e| e.response_id.is_none()));
        let affected_v1 = sink
            .record_batch_checked(&events_v1, None)
            .expect("persist v1");
        assert_eq!(affected_v1, 2);

        // Case 1: RevertToStep creates a new trajectory_id ("traj-v2") and replays unchanged pre_step0
        // alongside a brand-new post-revert step at reused index 1 (both lacking response_id).
        let post_step1_v2 =
            make_vertex_claude_step("2026-11-26T10:02:00Z", "2026-11-26T10:02:01Z", 2_000, 200);
        let summary_v2 = LsTrajectorySummary {
            trajectory_id: Some("traj-v2".to_string()),
            step_count: 2,
            ..Default::default()
        };
        let batch_v2 = process_steps_slice(
            cid,
            Some(&summary_v2),
            0,
            &[pre_step0.clone(), post_step1_v2],
            &catalog,
            None,
            Utc::now(),
        );
        let events_v2 = collect_events(&batch_v2);
        assert_eq!(events_v2[0].request_id, events_v1[0].request_id);
        assert_ne!(events_v2[1].request_id, events_v1[1].request_id);
        let affected_v2 = sink
            .record_batch_checked(&events_v2, None)
            .expect("persist v2");
        assert_eq!(
            affected_v2, 1,
            "unchanged pre-revert step 0 must not duplicate; only new step 1 must insert"
        );

        // Case 2: In-place revert keeps the SAME trajectory_id ("traj-v2") while replacing step 1
        // with a newer turn at the same step index.
        let post_step1_inplace =
            make_vertex_claude_step("2026-11-26T10:03:00Z", "2026-11-26T10:03:01Z", 2_500, 250);
        let batch_v3 = process_steps_slice(
            cid,
            Some(&summary_v2),
            0,
            &[pre_step0, post_step1_inplace],
            &catalog,
            None,
            Utc::now(),
        );
        let events_v3 = collect_events(&batch_v3);
        assert_eq!(events_v3[0].request_id, events_v1[0].request_id);
        assert_ne!(events_v3[1].request_id, events_v2[1].request_id);
        let affected_v3 = sink
            .record_batch_checked(&events_v3, None)
            .expect("persist v3");
        assert_eq!(
            affected_v3, 1,
            "in-place revert with unchanged trajectory_id must still persist new post-revert step"
        );

        let all_rows = sink.read_all_events().expect("read_all_events");
        assert_eq!(all_rows.len(), 4);
        let total_input: u64 = all_rows.iter().map(|e| e.input_tokens).sum();
        let total_output: u64 = all_rows.iter().map(|e| e.output_tokens).sum();
        assert_eq!(total_input, 7_000);
        assert_eq!(total_output, 700);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn multi_frame_initial_snapshot_settle_issues_zero_cold_backfill_step_calls() {
        let steps_calls = Arc::new(AtomicUsize::new(0));
        let steps_calls_srv = Arc::clone(&steps_calls);

        let router = Router::new()
            .route(
                "/exa.language_server_pb.LanguageServerService/GetCascadeModelConfigData",
                post(|| async { axum::Json(serde_json::json!({ "clientModelConfigs": [] })) }),
            )
            .route(
                "/exa.language_server_pb.LanguageServerService/GetCascadeTrajectorySteps",
                post(move || {
                    let count = Arc::clone(&steps_calls_srv);
                    async move {
                        count.fetch_add(1, Ordering::SeqCst);
                        axum::Json(serde_json::json!({ "steps": [] }))
                    }
                }),
            )
            .route(
                "/exa.language_server_pb.LanguageServerService/JetboxSubscribeToSummaries",
                post(|| async move {
                    let (tx, rx) = tokio::sync::mpsc::channel::<Result<Vec<u8>, std::io::Error>>(8);
                    tokio::spawn(async move {
                        // Send 3 separate multi-frame snapshot chunks (simulating 12x100 live LS frames).
                        for frame_idx in 0..3 {
                            let payload = serde_json::json!({
                                "updates": {
                                    format!("hist-conv-{frame_idx:04}"): {
                                        "trajectoryId": format!("traj-hist-{frame_idx}"),
                                        "stepCount": 40,
                                        "createdTime": "2026-01-01T00:00:00Z",
                                        "lastModifiedTime": "2026-01-01T00:05:00Z"
                                    }
                                }
                            });
                            let bytes =
                                encode_connect_frame(0x00, &serde_json::to_vec(&payload).unwrap());
                            if tx.send(Ok(bytes)).await.is_err() {
                                return;
                            }
                            tokio::time::sleep(Duration::from_millis(10)).await;
                        }
                        // Keep stream open quietly past the idle settle window.
                        tokio::time::sleep(Duration::from_millis(500)).await;
                    });

                    let stream = futures_util::stream::unfold(rx, |mut rx| async move {
                        rx.recv().await.map(|item| (item, rx))
                    });
                    let mut resp = Response::new(Body::from_stream(stream));
                    *resp.status_mut() = StatusCode::OK;
                    resp.headers_mut().insert(
                        header::CONTENT_TYPE,
                        HeaderValue::from_static("application/connect+json"),
                    );
                    resp
                }),
            );

        let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
        let port = listener.local_addr().expect("addr").port();
        let server = tokio::spawn(async move {
            let _ = axum::serve(listener, router).await;
        });
        tokio::task::yield_now().await;

        let tmp = TempDir::new().expect("tmp");
        let sink = Arc::new(SqliteUsageSink::new(tmp.path().join("usage")).expect("sink"));
        let client = LsClient::new(
            LsEndpoint::parse(&format!("127.0.0.1:{port}")).expect("ep"),
            CsrfToken::parse(TEST_CSRF).expect("csrf"),
        )
        .expect("client");

        let (shutdown_tx, shutdown_rx) = watch::channel(false);
        let cfg = LsCollectorConfig {
            snapshot_settle_idle: Duration::from_millis(60),
            snapshot_settle_max: Duration::from_millis(400),
            cascade_debounce: Duration::from_millis(20),
            ..Default::default()
        };
        let handle = spawn_ls_collector(client, sink, cfg, shutdown_rx);

        // Wait until settled (3 frames * 10ms + 60ms idle ~ 90ms).
        let deadline = Instant::now() + Duration::from_secs(2);
        loop {
            if handle.inner.state.lock().await.boot_settled {
                break;
            }
            assert!(Instant::now() < deadline, "collector failed to settle");
            tokio::time::sleep(Duration::from_millis(10)).await;
        }

        // All 3 historical cascades seeded in boot_baselines with ZERO Steps RPC calls!
        {
            let guard = handle.inner.state.lock().await;
            assert_eq!(guard.boot_baselines.len(), 3);
            assert!(guard.dirty_cascades.is_empty());
        }
        assert_eq!(steps_calls.load(Ordering::SeqCst), 0);

        let _ = shutdown_tx.send(true);
        server.abort();
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn circuit_breaker_ignores_single_steps_404_but_trips_on_discovery_permission_denied() {
        // Case 1: Single GetCascadeTrajectorySteps 404 MUST NOT trip circuit breaker.
        let router_steps_404 = Router::new()
            .route(
                "/exa.language_server_pb.LanguageServerService/GetCascadeModelConfigData",
                post(|| async { axum::Json(serde_json::json!({ "clientModelConfigs": [] })) }),
            )
            .route(
                "/exa.language_server_pb.LanguageServerService/GetCascadeTrajectorySteps",
                post(|| async { StatusCode::NOT_FOUND }),
            )
            .route(
                "/exa.language_server_pb.LanguageServerService/JetboxSubscribeToSummaries",
                post(|| async {
                    let mut resp = Response::new(Body::from(encode_connect_frame(0x00, b"{}")));
                    *resp.status_mut() = StatusCode::OK;
                    resp.headers_mut().insert(
                        header::CONTENT_TYPE,
                        HeaderValue::from_static("application/connect+json"),
                    );
                    resp
                }),
            );

        let l1 = TcpListener::bind("127.0.0.1:0").await.expect("bind");
        let p1 = l1.local_addr().expect("addr").port();
        let s1 = tokio::spawn(async move {
            let _ = axum::serve(l1, router_steps_404).await;
        });
        tokio::task::yield_now().await;

        let tmp1 = TempDir::new().expect("tmp1");
        let sink1 = Arc::new(SqliteUsageSink::new(tmp1.path().join("usage")).expect("sink1"));
        let c1 = LsClient::new(
            LsEndpoint::parse(&format!("127.0.0.1:{p1}")).expect("ep1"),
            CsrfToken::parse(TEST_CSRF).expect("csrf"),
        )
        .expect("c1");
        let (tx1, rx1) = watch::channel(false);
        let h1 = spawn_ls_collector(c1, sink1, LsCollectorConfig::default(), rx1);
        let snap1 = h1.session_stats(Some("deleted-cascade-999")).await;
        assert!(
            snap1.is_some(),
            "single Steps 404 must NOT trip circuit breaker"
        );
        assert!(h1.is_active().await);
        let _ = tx1.send(true);
        s1.abort();

        // Case 2: Discovery/Catalog PermissionDenied MUST trip circuit breaker and fail open.
        let router_denied = Router::new()
            .route(
                "/exa.language_server_pb.LanguageServerService/GetCascadeModelConfigData",
                post(|| async { StatusCode::FORBIDDEN }),
            )
            .route(
                "/exa.language_server_pb.LanguageServerService/JetboxSubscribeToSummaries",
                post(|| async { StatusCode::FORBIDDEN }),
            );

        let l2 = TcpListener::bind("127.0.0.1:0").await.expect("bind");
        let p2 = l2.local_addr().expect("addr").port();
        let s2 = tokio::spawn(async move {
            let _ = axum::serve(l2, router_denied).await;
        });
        tokio::task::yield_now().await;

        let tmp2 = TempDir::new().expect("tmp2");
        let sink2 = Arc::new(SqliteUsageSink::new(tmp2.path().join("usage")).expect("sink2"));
        let c2 = LsClient::new(
            LsEndpoint::parse(&format!("127.0.0.1:{p2}")).expect("ep2"),
            CsrfToken::parse(TEST_CSRF).expect("csrf"),
        )
        .expect("c2");
        let (tx2, rx2) = watch::channel(false);
        let h2 = spawn_ls_collector(c2, sink2, LsCollectorConfig::default(), rx2);
        let deadline = Instant::now() + Duration::from_secs(2);
        while h2.is_active().await {
            assert!(
                Instant::now() < deadline,
                "circuit breaker did not trip on PermissionDenied"
            );
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        assert!(
            h2.session_stats(Some("any-conv-123456")).await.is_none(),
            "session_stats must fail open (None) when circuit breaker is open"
        );
        let _ = tx2.send(true);
        s2.abort();
    }

    #[test]
    fn per_port_advisory_lock_blocks_second_holder_without_unlinking() {
        let tmp = TempDir::new().expect("tmp");
        let usage_dir = tmp.path().join("usage");
        let lock1 = PerPortCollectorLock::try_acquire(&usage_dir, 64482)
            .expect("lock1")
            .expect("first lock acquired");
        assert!(lock1.path().exists());

        let lock2 = PerPortCollectorLock::try_acquire(&usage_dir, 64482).expect("lock2");
        assert!(lock2.is_none(), "second holder for same port must get None");

        let lock_other_port = PerPortCollectorLock::try_acquire(&usage_dir, 64483)
            .expect("other port")
            .expect("distinct port lock acquired");
        drop(lock_other_port);

        let lock_path = lock1.path().to_path_buf();
        drop(lock1);
        assert!(lock_path.exists(), "lock file must not be unlinked on drop");

        let lock3 = PerPortCollectorLock::try_acquire(&usage_dir, 64482)
            .expect("lock3")
            .expect("re-acquired after drop");
        drop(lock3);
    }
}
