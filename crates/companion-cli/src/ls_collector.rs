use std::collections::{HashMap, HashSet, VecDeque};
use std::fs::{File, TryLockError};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use chrono::{DateTime, SecondsFormat, Utc};
use companion_core::fs_safe::{self, OpenRegularMode};
use companion_core::{
    LsSyncWatermark, MAX_RECENT_CALLS, MAX_SESSION_MODELS, ModelSessionStats, ObservedModel,
    RecentCallSnapshot, SqliteUsageSink, StaticPlaceholderMapping, StatsSnapshot, TelemetrySource,
    UpstreamModelCatalog, UsageEvent, UsageStatsSnapshot, lookup_static_placeholder,
    normalize_placeholder_enum_key, normalize_utc_timestamp, placeholder_numeric_alias,
    sanitize_conversation_id, sanitize_model_display_name, sanitize_response_id,
};
use companion_providers::{
    LsClient, LsError, LsGeneratorMetadata, LsModelConfig, LsModelUsage, LsStepMetadata,
    LsSummaryDelta, LsTrajectoryStep, LsTrajectorySummary,
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
const MAX_GEN_META_TAIL_SEEK_RPCS: usize = 24;
const GEN_META_EXPANSION_MIN_STEP: u32 = 64;
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

fn is_unresolved_placeholder_id(raw: &str) -> bool {
    let trimmed = raw.trim();
    trimmed.is_empty()
        || trimmed.starts_with("MODEL_PLACEHOLDER_")
        || trimmed.bytes().all(|b| b.is_ascii_digit())
}

fn insert_catalog_entry_with_aliases(
    map: &mut HashMap<String, CatalogEntry>,
    raw_key: &str,
    entry: CatalogEntry,
    overwrite: bool,
) {
    let trimmed = raw_key.trim();
    if trimmed.is_empty() {
        return;
    }
    let mut keys = vec![trimmed.to_string()];
    if let Some(enum_key) = normalize_placeholder_enum_key(trimmed) {
        if enum_key != trimmed {
            keys.push(enum_key.clone());
        }
        if let Some(num_key) = placeholder_numeric_alias(&enum_key)
            && num_key != trimmed
        {
            keys.push(num_key);
        }
    }
    for k in keys {
        if overwrite {
            let merged = match map.get(&k) {
                Some(existing) => merge_catalog_entry(Some(entry.clone()), existing),
                None => entry.clone(),
            };
            map.insert(k, merged);
        } else {
            map.entry(k).or_insert_with(|| entry.clone());
        }
    }
}

fn lookup_map_with_aliases<'a>(
    map: &'a HashMap<String, CatalogEntry>,
    raw_key: &str,
) -> Option<&'a CatalogEntry> {
    let trimmed = raw_key.trim();
    if trimmed.is_empty() {
        return None;
    }
    if let Some(hit) = map.get(trimmed) {
        return Some(hit);
    }
    if let Some(enum_key) = normalize_placeholder_enum_key(trimmed) {
        if let Some(hit) = map.get(&enum_key) {
            return Some(hit);
        }
        if let Some(num_key) = placeholder_numeric_alias(&enum_key)
            && let Some(hit) = map.get(&num_key)
        {
            return Some(hit);
        }
    }
    None
}

fn merge_catalog_entry(base: Option<CatalogEntry>, fallback: &CatalogEntry) -> CatalogEntry {
    match base {
        None => fallback.clone(),
        Some(mut acc) => {
            let acc_has_real_id = acc
                .model_id
                .as_deref()
                .is_some_and(|s| !is_unresolved_placeholder_id(s));
            if !acc_has_real_id
                && let Some(ref fb_id) = fallback.model_id
                && !is_unresolved_placeholder_id(fb_id)
            {
                acc.model_id = Some(fb_id.clone());
            } else if acc.model_id.is_none() {
                acc.model_id = fallback.model_id.clone();
            }
            if acc.label.as_deref().is_none_or(|s| s.trim().is_empty()) {
                acc.label = fallback.label.clone();
            }
            if acc.provider.as_deref().is_none_or(|s| s.trim().is_empty()) {
                acc.provider = fallback.provider.clone();
            }
            if acc.model_url.as_deref().is_none_or(|s| s.trim().is_empty()) {
                acc.model_url = fallback.model_url.clone();
            }
            acc
        }
    }
}

#[derive(Debug, Default)]
pub struct ModelCatalog {
    entries: HashMap<String, CatalogEntry>,
    upstream_entries: HashMap<String, CatalogEntry>,
    learned_entries: HashMap<String, CatalogEntry>,
    upstream_catalog: Option<Arc<UpstreamModelCatalog>>,
    upstream_version: u64,
    last_refreshed: Option<Instant>,
}

impl ModelCatalog {
    pub fn with_upstream_catalog(upstream_catalog: Option<Arc<UpstreamModelCatalog>>) -> Self {
        let mut catalog = Self {
            upstream_catalog,
            ..Default::default()
        };
        catalog.sync_from_upstream();
        catalog
    }

    pub fn sync_from_upstream(&mut self) {
        let Some(ref uc) = self.upstream_catalog else {
            return;
        };
        let ver = uc.version();
        if ver == self.upstream_version {
            return;
        }
        let snap = uc.snapshot();
        let mut next = HashMap::with_capacity(snap.len() * 2);
        for (key, item) in snap {
            let entry = CatalogEntry {
                model_id: item.model_id,
                label: item.display_name,
                provider: item.model_provider.or(item.api_provider),
                model_url: item.model_url,
            };
            insert_catalog_entry_with_aliases(&mut next, &key, entry, true);
        }
        self.upstream_entries = next;
        self.upstream_version = ver;
    }

    pub fn update(&mut self, configs: Vec<LsModelConfig>, now: Instant) {
        // Merge/upsert into self.entries (preserving previously seen session models even if
        // removed from a subsequent GetCascadeModelConfigData response).
        // Iterate in reverse so earlier configs (clientModelConfigs) take precedence over
        // later configs (cached/battleMode) within the same refresh batch.
        for cfg in configs.into_iter().rev() {
            let entry = CatalogEntry {
                model_id: cfg.model_id,
                label: cfg.label,
                provider: cfg.provider,
                model_url: cfg.model_url,
            };
            if let Some(moa) = cfg.model_or_alias {
                if let Some(model) = moa.model
                    && !model.trim().is_empty()
                {
                    insert_catalog_entry_with_aliases(
                        &mut self.entries,
                        &model,
                        entry.clone(),
                        true,
                    );
                }
                if let Some(alias) = moa.alias
                    && !alias.trim().is_empty()
                {
                    insert_catalog_entry_with_aliases(&mut self.entries, &alias, entry, false);
                }
            }
        }
        self.last_refreshed = Some(now);
    }

    pub fn learn_from_generator_metadata(&mut self, metadata: &[LsGeneratorMetadata]) -> bool {
        let mut learned_any = false;
        for item in metadata {
            let Some(ref chat_model) = item.chat_model else {
                continue;
            };
            let Some(raw_enum) = chat_model
                .model
                .as_deref()
                .map(str::trim)
                .filter(|s| !s.is_empty())
            else {
                continue;
            };
            let response_model = chat_model
                .response_model
                .as_deref()
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(str::to_string);
            let display_name = chat_model
                .model_display_name
                .as_deref()
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(str::to_string);
            if response_model.is_none() && display_name.is_none() {
                continue;
            }
            let entry = CatalogEntry {
                model_id: response_model,
                label: display_name,
                provider: None,
                model_url: None,
            };
            insert_catalog_entry_with_aliases(&mut self.learned_entries, raw_enum, entry, true);
            learned_any = true;
        }
        learned_any
    }

    pub fn lookup(&self, key: &str) -> Option<&CatalogEntry> {
        lookup_map_with_aliases(&self.entries, key)
            .or_else(|| lookup_map_with_aliases(&self.upstream_entries, key))
            .or_else(|| lookup_map_with_aliases(&self.learned_entries, key))
    }

    pub fn lookup_merged(&self, key: &str) -> Option<CatalogEntry> {
        let mut acc: Option<CatalogEntry> = None;
        if let Some(e) = lookup_map_with_aliases(&self.entries, key) {
            acc = Some(merge_catalog_entry(acc, e));
        }
        if let Some(e) = lookup_map_with_aliases(&self.upstream_entries, key) {
            acc = Some(merge_catalog_entry(acc, e));
        }
        if let Some(e) = lookup_map_with_aliases(&self.learned_entries, key) {
            acc = Some(merge_catalog_entry(acc, e));
        }
        acc
    }

    pub fn has_resolved_entry(&self, key: &str) -> bool {
        self.lookup_merged(key).is_some_and(|entry| {
            entry
                .model_id
                .as_deref()
                .is_some_and(|id| !is_unresolved_placeholder_id(id))
                || entry
                    .model_url
                    .as_deref()
                    .and_then(|u| u.strip_prefix("gemini-api://"))
                    .is_some_and(|s| !s.trim().is_empty())
        })
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
        if !is_unresolved_placeholder_id(key) {
            return false;
        }
        !self.has_resolved_entry(key) && self.can_refresh(min_interval, now)
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

fn sanitize_display_as_stats_model(raw: &str) -> Option<String> {
    let cleaned: String = sanitize_model_display_name(raw)?
        .chars()
        .filter(|c| *c != '"' && *c != '\\')
        .take(MAX_STATS_MODEL_LEN)
        .collect();
    let trimmed = cleaned.trim();
    if trimmed.is_empty() {
        None
    } else {
        Some(trimmed.to_string())
    }
}

pub fn canonicalize_model_id(raw: &str) -> String {
    let trimmed = raw.trim();
    let without_scheme = trimmed.strip_prefix("gemini-api://").unwrap_or(trimmed);
    let without_prefix = without_scheme
        .strip_prefix("companion/")
        .unwrap_or(without_scheme);
    let lower = without_prefix.to_ascii_lowercase();
    let without_effort = if lower.ends_with("-medium") {
        &without_prefix[..without_prefix.len() - 7]
    } else if lower.ends_with("-xhigh") {
        &without_prefix[..without_prefix.len() - 6]
    } else if lower.ends_with("-high") {
        &without_prefix[..without_prefix.len() - 5]
    } else if lower.ends_with("-low") || lower.ends_with("-max") {
        &without_prefix[..without_prefix.len() - 4]
    } else {
        without_prefix
    };
    let lower_effort = without_effort.to_ascii_lowercase();
    let stripped = if lower_effort.ends_with("-tiered") {
        &without_effort[..without_effort.len() - 7]
    } else {
        without_effort
    };
    sanitize_stats_model(stripped)
}

fn non_placeholder_canonical(raw: &str) -> Option<String> {
    let trimmed = raw.trim();
    if is_unresolved_placeholder_id(trimmed) {
        return None;
    }
    let canon = canonicalize_model_id(trimmed);
    if canon == "unknown" || is_unresolved_placeholder_id(&canon) {
        None
    } else {
        Some(canon)
    }
}

fn is_codex_model_candidate(raw_model_id: Option<&str>, display_name: Option<&str>) -> bool {
    if let Some(mid) = raw_model_id {
        let bare = canonicalize_model_id(mid);
        if companion_core::is_codex_model(&bare) {
            return true;
        }
        let lower = bare.to_ascii_lowercase();
        if lower.starts_with("gpt-") || lower.starts_with("codex-") {
            return true;
        }
    }
    display_name.is_some_and(|d| d.to_ascii_lowercase().contains("(codex)"))
}

fn is_internal_byom_enum(model_enum: Option<&str>) -> bool {
    matches!(
        model_enum,
        Some("MODEL_GOOGLE_GEMINI_INTERNAL_BYOM" | "326")
    )
}

#[allow(clippy::too_many_arguments)]
fn map_provider_from_api_provider(
    api_provider: Option<&str>,
    source: &str,
    model_enum: Option<&str>,
    catalog_provider: Option<&str>,
    info_model_provider: Option<&str>,
    raw_model_id: Option<&str>,
    display_name: Option<&str>,
    static_entry: Option<StaticPlaceholderMapping>,
) -> String {
    if let Some(ap) = api_provider {
        match ap {
            "API_PROVIDER_ANTHROPIC_VERTEX" | "26" => return "vertex_claude".to_string(),
            "API_PROVIDER_OPENAI_VERTEX" | "31" | "API_PROVIDER_OPENAI_RESPONSES_VERTEX" | "32" => {
                return "vertex_openai".to_string();
            }
            "API_PROVIDER_GOOGLE_GEMINI" | "24" => {
                return if source != "builtin" {
                    if is_codex_model_candidate(raw_model_id, display_name) {
                        "openai-codex".to_string()
                    } else {
                        "gemini".to_string()
                    }
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
                    if is_codex_model_candidate(raw_model_id, display_name) {
                        "openai-codex".to_string()
                    } else {
                        "gemini".to_string()
                    }
                } else {
                    "antigravity".to_string()
                };
            }
            _ => {}
        }
    }

    if source == "proxy" || source == "direct" {
        if source == "proxy" && is_codex_model_candidate(raw_model_id, display_name) {
            return "openai-codex".to_string();
        }
        return "gemini".to_string();
    }

    for cp in catalog_provider.into_iter().chain(info_model_provider) {
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

    if let Some(mapped) = static_entry {
        return mapped.provider.to_string();
    }

    "antigravity".to_string()
}

/// Deterministic 3-tier attribution (`GetCascadeModelConfigData` / `fetchAvailableModels` /
/// `GeneratorMetadata` + `responseHeader` 3-way wire fingerprint + `apiProvider`/`modelInfo` +
/// static placeholder fallback).
pub fn resolve_attribution(
    meta: &LsStepMetadata,
    usage: &LsModelUsage,
    catalog: &ModelCatalog,
) -> AttributedModel {
    let model_enum = meta.generator_model.as_deref().or(usage.model.as_deref());
    let catalog_entry = model_enum.and_then(|key| catalog.lookup_merged(key));
    let static_entry = model_enum.and_then(lookup_static_placeholder);

    let resp_hdr = usage.response_header.as_ref();
    let has_builtin_session = resp_hdr.is_some_and(|h| h.has_builtin_session_id());
    let has_direct_session = resp_hdr.is_some_and(|h| h.has_direct_session_id());
    let has_hdr_response_id = resp_hdr.is_some_and(|h| h.has_response_id());

    let catalog_model_id = catalog_entry
        .as_ref()
        .and_then(|c| c.model_id.as_deref())
        .filter(|s| !s.trim().is_empty());
    let catalog_model_url = catalog_entry
        .as_ref()
        .and_then(|c| c.model_url.as_deref())
        .filter(|s| !s.trim().is_empty());
    let info_model_url = meta
        .model_info
        .as_ref()
        .and_then(|i| i.model_url.as_deref())
        .filter(|s| !s.trim().is_empty());

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

    let dynamic_model = if is_companion_catalog {
        catalog_model_url
            .and_then(|u| u.strip_prefix("gemini-api://"))
            .and_then(non_placeholder_canonical)
            .or_else(|| catalog_model_id.and_then(non_placeholder_canonical))
    } else {
        catalog_model_id.and_then(non_placeholder_canonical)
    }
    .or_else(|| {
        meta.model_info.as_ref().and_then(|info| {
            info.model_url
                .as_deref()
                .and_then(|u| u.strip_prefix("gemini-api://"))
                .and_then(non_placeholder_canonical)
                .or_else(|| {
                    info.vertex_model_id
                        .as_deref()
                        .and_then(non_placeholder_canonical)
                })
                .or_else(|| {
                    info.model_name
                        .as_deref()
                        .and_then(non_placeholder_canonical)
                })
                .or_else(|| info.model_id.as_deref().and_then(non_placeholder_canonical))
        })
    });

    let used_static_model = dynamic_model.is_none() && static_entry.is_some();

    let model_display_name = catalog_entry
        .as_ref()
        .and_then(|c| c.label.as_deref())
        .or_else(|| {
            meta.model_info
                .as_ref()
                .and_then(|i| i.display_name.as_deref())
        })
        .and_then(sanitize_model_display_name)
        .or_else(|| {
            if used_static_model {
                static_entry.map(|s| s.display_name.to_string())
            } else {
                None
            }
        });

    let model = dynamic_model
        .or_else(|| static_entry.map(|s| s.model_id.to_string()))
        .or_else(|| {
            model_display_name
                .as_deref()
                .and_then(sanitize_display_as_stats_model)
        })
        .unwrap_or_else(|| sanitize_stats_model(model_enum.unwrap_or("unknown")));

    let api_provider = usage.api_provider.as_deref().or_else(|| {
        meta.model_info
            .as_ref()
            .and_then(|i| i.api_provider.as_deref())
    });
    let info_model_provider = meta
        .model_info
        .as_ref()
        .and_then(|i| i.model_provider.as_deref());
    let catalog_provider = catalog_entry.as_ref().and_then(|c| c.provider.as_deref());

    let provider = if is_companion_catalog {
        if is_codex_model_candidate(Some(&model), model_display_name.as_deref()) {
            "openai-codex".to_string()
        } else if companion_core::is_supported_gemini_model(&model)
            || model.to_ascii_lowercase().starts_with("gemini-")
        {
            "gemini".to_string()
        } else {
            map_provider_from_api_provider(
                api_provider,
                &source,
                model_enum,
                catalog_provider,
                info_model_provider,
                Some(&model),
                model_display_name.as_deref(),
                static_entry,
            )
        }
    } else {
        map_provider_from_api_provider(
            api_provider,
            &source,
            model_enum,
            catalog_provider,
            info_model_provider,
            Some(&model),
            model_display_name.as_deref(),
            static_entry,
        )
    };

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

fn generator_metadata_model_enum(entry: &LsGeneratorMetadata) -> Option<&str> {
    let chat = entry.chat_model.as_ref()?;
    chat.model
        .as_deref()
        .filter(|s| !s.trim().is_empty())
        .or_else(|| {
            chat.usage
                .as_ref()
                .and_then(|u| u.model.as_deref())
                .filter(|s| !s.trim().is_empty())
        })
}

fn resolve_generator_metadata_model(
    entry: &LsGeneratorMetadata,
    catalog: &ModelCatalog,
) -> Option<(String, String)> {
    let chat = entry.chat_model.as_ref()?;
    let chat_model = chat.model.as_deref().filter(|s| !s.trim().is_empty());
    let usage_model = chat
        .usage
        .as_ref()
        .and_then(|u| u.model.as_deref())
        .filter(|s| !s.trim().is_empty());
    let response_model = chat
        .response_model
        .as_deref()
        .filter(|s| !s.trim().is_empty());
    if chat_model.is_none() && usage_model.is_none() && response_model.is_none() {
        return None;
    }
    let generator_model = chat_model
        .or_else(|| {
            if usage_model.is_none() {
                response_model
            } else {
                None
            }
        })
        .map(str::to_string);
    let meta = LsStepMetadata {
        generator_model,
        ..Default::default()
    };
    let default_usage = LsModelUsage::default();
    let usage = chat.usage.as_ref().unwrap_or(&default_usage);
    let attr = resolve_attribution(&meta, usage, catalog);
    if attr.model == "unknown" {
        None
    } else {
        Some((attr.provider, attr.model))
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
    pub unknown_model_enums: Vec<String>,
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
    let mut unknown_model_enums: Vec<String> = Vec::new();
    let mut seen_unknown_keys: HashSet<String> = HashSet::new();

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

        let candidate_enum = meta
            .generator_model
            .as_deref()
            .or_else(|| meta.model_usage.as_ref().and_then(|u| u.model.as_deref()));
        if let Some(key) = candidate_enum
            && is_unresolved_placeholder_id(key)
            && !catalog.has_resolved_entry(key)
        {
            if unknown_model_enum.is_none() {
                unknown_model_enum = Some(key.to_string());
            }
            let norm_key =
                normalize_placeholder_enum_key(key).unwrap_or_else(|| key.trim().to_string());
            if seen_unknown_keys.insert(norm_key.clone()) {
                unknown_model_enums.push(norm_key);
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
        unknown_model_enums,
    }
}

#[derive(Debug, Default, Clone)]
pub struct CascadeFoldState {
    pub trajectory_id: String,
    pub step_offset: u32,
    pub generator_metadata_offset: u32,
    generator_metadata_probed_step: Option<u32>,
    pub max_context_tokens: Option<u64>,
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
            if entry.model_display_name.is_none() {
                entry.model_display_name = ev.model_display_name.clone();
            }
        } else {
            self.by_model.insert(
                model_key,
                ModelSessionStats {
                    provider: ev.provider.clone(),
                    source: ev.source.clone(),
                    model: ev.model.clone(),
                    model_display_name: ev.model_display_name.clone(),
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
                if acc.model_display_name.is_none() {
                    acc.model_display_name = stats.model_display_name.clone();
                }
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
        max_context_tokens: folds.first().and_then(|f| f.max_context_tokens),
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
    observed_compaction_limits: HashMap<(String, String), u64>,
    boot_baselines: HashMap<String, BootBaseline>,
    summaries: HashMap<String, LsTrajectorySummary>,
    children_by_root: HashMap<String, HashSet<String>>,
    dirty_cascades: HashMap<String, DirtyCascade>,
    last_fetched: HashMap<String, Instant>,
    last_gen_meta_lookup: HashMap<(String, String), Instant>,
    monitor_cache: HashMap<String, CascadeFoldState>,
}

impl CollectorState {
    fn new(
        collector_boot_utc: DateTime<Utc>,
        watermarks: Vec<LsSyncWatermark>,
        observed_models: Vec<ObservedModel>,
        upstream_catalog: Option<Arc<UpstreamModelCatalog>>,
    ) -> Self {
        let mut persisted_watermarks = HashMap::with_capacity(watermarks.len());
        for wm in watermarks {
            persisted_watermarks.insert(wm.cascade_id.clone(), wm);
        }
        let mut observed_compaction_limits = HashMap::with_capacity(observed_models.len());
        for observed in observed_models {
            if let Some(tokens) = observed.compaction_limit_tokens
                && tokens > 0
            {
                observed_compaction_limits.insert((observed.provider, observed.model), tokens);
            }
        }
        Self {
            collector_boot_utc,
            boot_settled: false,
            unauthenticated: false,
            warned_unauthenticated: false,
            circuit_open_until: None,
            catalog: ModelCatalog::with_upstream_catalog(upstream_catalog),
            persisted_watermarks,
            observed_compaction_limits,
            boot_baselines: HashMap::new(),
            summaries: HashMap::new(),
            children_by_root: HashMap::new(),
            dirty_cascades: HashMap::new(),
            last_fetched: HashMap::new(),
            last_gen_meta_lookup: HashMap::new(),
            monitor_cache: HashMap::new(),
        }
    }

    fn can_query_generator_metadata(
        &self,
        cid: &str,
        normalized_enum: &str,
        min_interval: Duration,
        now: Instant,
    ) -> bool {
        self.last_gen_meta_lookup
            .iter()
            .find(|((c, e), _)| c == cid && e == normalized_enum)
            .is_none_or(|(_, last)| now.saturating_duration_since(*last) >= min_interval)
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
        self.last_gen_meta_lookup.retain(|(c, _), _| c != cid);
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
    spawn_ls_collector_from_env_with_catalog(sqlite_sink, None, shutdown_rx)
}

pub fn spawn_ls_collector_from_env_with_catalog(
    sqlite_sink: Arc<SqliteUsageSink>,
    upstream_catalog: Option<Arc<UpstreamModelCatalog>>,
    shutdown_rx: watch::Receiver<bool>,
) -> Option<LsCollectorHandle> {
    match LsClient::from_env() {
        Ok(Some(client)) => Some(spawn_ls_collector_with_catalog(
            client,
            sqlite_sink,
            LsCollectorConfig::default(),
            upstream_catalog,
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
    spawn_ls_collector_with_catalog(client, sqlite_sink, config, None, shutdown_rx)
}

pub fn spawn_ls_collector_with_catalog(
    client: LsClient,
    sqlite_sink: Arc<SqliteUsageSink>,
    config: LsCollectorConfig,
    upstream_catalog: Option<Arc<UpstreamModelCatalog>>,
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

    let initial_observed_models = match sqlite_sink.load_all_observed_models() {
        Ok(models) => models,
        Err(err) => {
            tracing::warn!(error = %err, "failed to load persisted observed models at startup");
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
        state: Mutex::new(CollectorState::new(
            Utc::now(),
            initial_watermarks,
            initial_observed_models,
            upstream_catalog,
        )),
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
            let mut snap = merge_cascade_folds(&folds);
            snap.max_context_tokens = None;
            return Some(snap);
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

        for (idx, cid) in target_cids.iter().enumerate() {
            if let Err(LsError::Unauthenticated { .. }) =
                hydrate_cascade_for_monitor(&self.inner, cid, idx == 0).await
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
        let mut snap = merge_cascade_folds(&folds);
        snap.max_context_tokens = guard.monitor_cache.get(&focused_cid).and_then(|fold| {
            fold.max_context_tokens.or_else(|| {
                fold.recent_calls.back().and_then(|call| {
                    guard
                        .observed_compaction_limits
                        .get(&(call.provider.clone(), call.model.clone()))
                        .copied()
                })
            })
        });
        Some(snap)
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
            guard.catalog.sync_from_upstream();
            guard.catalog.update(configs, Instant::now());
            true
        }
        Err(err) => {
            handle_catalog_or_discovery_error(inner, &err).await;
            false
        }
    }
}

async fn fetch_generator_metadata_tail_seeking(
    client: &LsClient,
    cascade_id: &str,
    req_offset: u32,
    summary_steps: u32,
) -> Result<Option<(u32, Vec<LsGeneratorMetadata>)>, LsError> {
    let mut rpcs = 0usize;
    let mut phase_a: Option<(u32, u32)> = None;
    let mut phase_b: Option<(Option<u32>, u32)> = None;

    if req_offset > 0 || summary_steps <= LARGE_CASCADE_HYDRATION_THRESHOLD {
        rpcs += 1;
        match client
            .get_cascade_trajectory_generator_metadata(cascade_id, req_offset)
            .await
        {
            Ok(entries) => return Ok(Some((req_offset, entries))),
            Err(LsError::ResponseTooLarge { .. }) => {
                let initial_step = summary_steps
                    .saturating_sub(req_offset)
                    .max(GEN_META_EXPANSION_MIN_STEP);
                phase_a = Some((req_offset, initial_step));
            }
            Err(err) => return Err(err),
        }
    } else {
        let probe = summary_steps / 2;
        rpcs += 1;
        match client
            .get_cascade_trajectory_generator_metadata(cascade_id, probe)
            .await
        {
            Ok(entries) if !entries.is_empty() => return Ok(Some((probe, entries))),
            Ok(_) => {
                phase_b = Some((None, probe));
            }
            Err(LsError::ResponseTooLarge { .. }) => {
                let initial_step = summary_steps
                    .saturating_sub(probe)
                    .max(GEN_META_EXPANSION_MIN_STEP);
                phase_a = Some((probe, initial_step));
            }
            Err(err) => return Err(err),
        }
    }

    // Phase A: exponential bracket expansion from a known ResponseTooLarge lower bound.
    if let Some((mut lo, mut step)) = phase_a {
        while rpcs < MAX_GEN_META_TAIL_SEEK_RPCS {
            let probe = lo.saturating_add(step);
            if probe == lo {
                return Ok(None);
            }
            rpcs += 1;
            match client
                .get_cascade_trajectory_generator_metadata(cascade_id, probe)
                .await
            {
                Ok(entries) if !entries.is_empty() => return Ok(Some((probe, entries))),
                Ok(_) => {
                    phase_b = Some((Some(lo), probe));
                    break;
                }
                Err(LsError::ResponseTooLarge { .. }) => {
                    lo = probe;
                    step = step.saturating_mul(2);
                }
                Err(err) => return Err(err),
            }
        }
    }

    // Phase B: binary search into the trailing window that fits within MAX_LS_RESPONSE_BYTES.
    if let Some((mut too_large_lo, mut hi)) = phase_b {
        while too_large_lo.is_none() && hi > 0 && rpcs < MAX_GEN_META_TAIL_SEEK_RPCS {
            let mid = hi / 2;
            rpcs += 1;
            match client
                .get_cascade_trajectory_generator_metadata(cascade_id, mid)
                .await
            {
                Ok(entries) if !entries.is_empty() => return Ok(Some((mid, entries))),
                Ok(_) => hi = mid,
                Err(LsError::ResponseTooLarge { .. }) => too_large_lo = Some(mid),
                Err(err) => return Err(err),
            }
        }
        if too_large_lo.is_none() && hi == 0 {
            return Ok(Some((0, Vec::new())));
        }

        if let Some(mut lo) = too_large_lo {
            while lo.saturating_add(1) < hi && rpcs < MAX_GEN_META_TAIL_SEEK_RPCS {
                let mid = lo + (hi - lo) / 2;
                rpcs += 1;
                match client
                    .get_cascade_trajectory_generator_metadata(cascade_id, mid)
                    .await
                {
                    Ok(entries) if !entries.is_empty() => return Ok(Some((mid, entries))),
                    Ok(_) => hi = mid,
                    Err(LsError::ResponseTooLarge { .. }) => lo = mid,
                    Err(err) => return Err(err),
                }
            }
        }
    }

    Ok(None)
}

fn normalize_gen_meta_lookup_key(raw: &str) -> String {
    normalize_placeholder_enum_key(raw).unwrap_or_else(|| raw.trim().to_string())
}

async fn resolve_unknown_model_enum_via_generator_metadata(
    inner: &LsCollectorInner,
    cascade_id: &str,
    unknown_enums: &[String],
) -> Option<Vec<LsGeneratorMetadata>> {
    if unknown_enums.is_empty() {
        return None;
    }
    let should_query = {
        let mut guard = inner.state.lock().await;
        guard.catalog.sync_from_upstream();
        let unresolved: Vec<String> = unknown_enums
            .iter()
            .filter(|k| !guard.catalog.has_resolved_entry(k))
            .map(|k| normalize_gen_meta_lookup_key(k))
            .collect();
        if unresolved.is_empty() {
            return None;
        }
        let now = Instant::now();
        let any_eligible = unresolved.iter().any(|norm_enum| {
            guard.can_query_generator_metadata(
                cascade_id,
                norm_enum,
                inner.config.catalog_refresh_min_interval,
                now,
            )
        });
        if !any_eligible {
            return None;
        }
        for norm_enum in unresolved {
            guard
                .last_gen_meta_lookup
                .insert((cascade_id.to_string(), norm_enum), now);
        }
        true
    };
    if !should_query {
        return None;
    }
    match inner
        .client
        .get_cascade_trajectory_generator_metadata(cascade_id, 0)
        .await
    {
        Ok(entries) => {
            let mut guard = inner.state.lock().await;
            guard.catalog.learn_from_generator_metadata(&entries);
            Some(entries)
        }
        Err(err) => {
            tracing::debug!(
                cascade_id,
                error = %err,
                "on-demand LS generator metadata lookup for unknown model enum skipped"
            );
            None
        }
    }
}

fn upsert_monitor_fold<'a>(
    monitor_cache: &'a mut HashMap<String, CascadeFoldState>,
    cascade_id: &str,
    traj_id: String,
    start_offset: u32,
    reset_fold: bool,
) -> &'a mut CascadeFoldState {
    if reset_fold {
        monitor_cache.insert(
            cascade_id.to_string(),
            CascadeFoldState::new(traj_id, start_offset),
        );
        monitor_cache
            .get_mut(cascade_id)
            .expect("inserted immediately above")
    } else {
        monitor_cache
            .entry(cascade_id.to_string())
            .or_insert_with(|| CascadeFoldState::new(traj_id, start_offset))
    }
}

async fn hydrate_cascade_for_monitor(
    inner: &LsCollectorInner,
    cascade_id: &str,
    fetch_generator_metadata: bool,
) -> Result<(), LsError> {
    let step_plan = {
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
                    Some((0, true, summary_opt, summary_traj))
                } else if summary_opt.is_some() && summary_steps <= existing.step_offset {
                    None
                } else {
                    Some((existing.step_offset, false, summary_opt, summary_traj))
                }
            }
            None => {
                let initial_offset = if summary_steps > LARGE_CASCADE_HYDRATION_THRESHOLD {
                    summary_steps.saturating_sub(LARGE_CASCADE_TAIL_WINDOW)
                } else {
                    0
                };
                Some((initial_offset, true, summary_opt, summary_traj))
            }
        }
    };

    let mut prefetched_gen_entries: Option<Vec<LsGeneratorMetadata>> = None;

    if let Some((start_offset, reset_fold, summary_opt, traj_id)) = step_plan {
        let steps = {
            let _permit = match inner.step_semaphore.clone().acquire_owned().await {
                Ok(p) => p,
                Err(_) => return Ok(()),
            };
            match inner
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
            }
        };

        let mut batch = {
            let mut guard = inner.state.lock().await;
            guard.catalog.sync_from_upstream();
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

        if !batch.unknown_model_enums.is_empty() {
            let refreshed = refresh_catalog(inner, true).await;
            let learned_entries = resolve_unknown_model_enum_via_generator_metadata(
                inner,
                cascade_id,
                &batch.unknown_model_enums,
            )
            .await;
            let learned_any = learned_entries.is_some();
            prefetched_gen_entries = learned_entries;
            if refreshed || learned_any {
                let mut guard = inner.state.lock().await;
                guard.catalog.sync_from_upstream();
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
        }

        let mut guard = inner.state.lock().await;
        let fold = upsert_monitor_fold(
            &mut guard.monitor_cache,
            cascade_id,
            traj_id,
            start_offset,
            reset_fold,
        );
        fold.apply_batch(&batch);
    }

    if fetch_generator_metadata {
        let gen_plan = {
            let guard = inner.state.lock().await;
            let summary_steps = guard
                .summaries
                .get(cascade_id)
                .map(|s| s.step_count)
                .unwrap_or(0);
            guard.monitor_cache.get(cascade_id).and_then(|fold| {
                let should_probe = fold.generator_metadata_probed_step != Some(fold.step_offset);
                if should_probe {
                    Some((
                        fold.generator_metadata_offset,
                        fold.step_offset,
                        summary_steps,
                    ))
                } else {
                    None
                }
            })
        };

        if let Some((req_offset, probed_step, summary_steps)) = gen_plan {
            let entries_res = if req_offset == 0
                && let Some(entries) = prefetched_gen_entries.take()
            {
                Ok(Some((0, entries)))
            } else {
                let _permit = match inner.step_semaphore.clone().acquire_owned().await {
                    Ok(p) => p,
                    Err(_) => return Ok(()),
                };
                fetch_generator_metadata_tail_seeking(
                    &inner.client,
                    cascade_id,
                    req_offset,
                    summary_steps,
                )
                .await
            };

            match entries_res {
                Ok(Some((effective_offset, entries))) => {
                    let has_unknown_placeholder = {
                        let guard = inner.state.lock().await;
                        entries.iter().any(|entry| {
                            entry.max_context_tokens().is_some()
                                && generator_metadata_model_enum(entry).is_some_and(|key| {
                                    key.starts_with("MODEL_PLACEHOLDER_")
                                        && guard.catalog.lookup(key).is_none()
                                })
                        })
                    };
                    if has_unknown_placeholder {
                        let _ = refresh_catalog(inner, true).await;
                    }

                    let mut dirty_observed: HashMap<(String, String), u64> = HashMap::new();
                    {
                        let mut guard = inner.state.lock().await;
                        guard.catalog.learn_from_generator_metadata(&entries);
                        let mut latest_tokens = None;
                        let mut observed_updates: Vec<(String, String, u64)> = Vec::new();
                        for entry in &entries {
                            if let Some(tokens) = entry.max_context_tokens() {
                                latest_tokens = Some(tokens);
                                if let Some((provider, model)) =
                                    resolve_generator_metadata_model(entry, &guard.catalog)
                                {
                                    observed_updates.push((provider, model, tokens));
                                }
                            }
                        }

                        if let Some(fold) = guard.monitor_cache.get_mut(cascade_id) {
                            fold.generator_metadata_offset = fold
                                .generator_metadata_offset
                                .max(effective_offset.saturating_add(entries.len() as u32));
                            fold.generator_metadata_probed_step = Some(probed_step);
                            if let Some(tokens) = latest_tokens {
                                fold.max_context_tokens = Some(tokens);
                            }
                        }

                        for (provider, model, tokens) in observed_updates {
                            let key = (provider.clone(), model.clone());
                            if guard.observed_compaction_limits.get(&key).copied() != Some(tokens) {
                                guard.observed_compaction_limits.insert(key.clone(), tokens);
                                dirty_observed.insert(key, tokens);
                            }
                        }
                    }

                    if !dirty_observed.is_empty() {
                        let sink = Arc::clone(&inner.sqlite_sink);
                        let updated_at_utc = Utc::now().to_rfc3339_opts(SecondsFormat::Secs, true);
                        let rows: Vec<ObservedModel> = dirty_observed
                            .into_iter()
                            .map(|((provider, model), tokens)| ObservedModel {
                                provider,
                                model,
                                compaction_limit_tokens: Some(tokens),
                                updated_at_utc: updated_at_utc.clone(),
                            })
                            .collect();
                        let res = tokio::task::spawn_blocking(move || {
                            for row in &rows {
                                if let Err(err) = sink.upsert_observed_model(row) {
                                    tracing::warn!(
                                        provider = %row.provider,
                                        model = %row.model,
                                        error = %err,
                                        "failed to persist observed model compaction limit"
                                    );
                                }
                            }
                        })
                        .await;
                        if let Err(err) = res {
                            tracing::warn!(
                                error = %err,
                                "observed model persistence task failed"
                            );
                        }
                    }
                }
                Ok(None) => {
                    let mut guard = inner.state.lock().await;
                    if let Some(fold) = guard.monitor_cache.get_mut(cascade_id) {
                        fold.generator_metadata_probed_step = Some(probed_step);
                    }
                    tracing::debug!(
                        cascade_id,
                        req_offset,
                        summary_steps,
                        "generator metadata tail-seeking exhausted budget or trailing entry exceeded cap; falling back to observed cache"
                    );
                }
                Err(err) => {
                    let mut guard = inner.state.lock().await;
                    if let Some(fold) = guard.monitor_cache.get_mut(cascade_id) {
                        fold.generator_metadata_probed_step = Some(probed_step);
                    }
                    // Strictly fail-open: never fail Monitor hydration, pause collector,
                    // or trip the circuit breaker on generator metadata errors.
                    tracing::debug!(
                        cascade_id,
                        error = %err,
                        "on-demand LS generator metadata hydration skipped"
                    );
                }
            }
        }
    }

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
        let mut guard = inner.state.lock().await;
        guard.catalog.sync_from_upstream();
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

    if !batch.unknown_model_enums.is_empty() {
        let refreshed = refresh_catalog(&inner, true).await;
        let learned = resolve_unknown_model_enum_via_generator_metadata(
            &inner,
            &cascade_id,
            &batch.unknown_model_enums,
        )
        .await
        .is_some();
        if refreshed || learned {
            let mut guard = inner.state.lock().await;
            guard.catalog.sync_from_upstream();
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
            Vec::new(),
            None,
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

    #[test]
    fn merge_cascade_folds_uses_focused_fold_max_context_tokens_only() {
        let mut focused = CascadeFoldState::new("traj-root".to_string(), 1);
        focused.max_context_tokens = Some(512_000);
        let mut descendant = CascadeFoldState::new("traj-sub".to_string(), 1);
        descendant.max_context_tokens = Some(200_000);

        let merged = merge_cascade_folds(&[&focused, &descendant]);
        assert_eq!(merged.max_context_tokens, Some(512_000));

        focused.max_context_tokens = None;
        let merged_none = merge_cascade_folds(&[&focused, &descendant]);
        assert_eq!(merged_none.max_context_tokens, None);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn focused_cascade_generator_metadata_incremental_offset_model_switch_and_subagent_isolation()
     {
        use std::sync::Mutex as StdMutex;

        #[derive(Default)]
        struct MockState {
            root_steps: Vec<serde_json::Value>,
            sub_steps: Vec<serde_json::Value>,
            root_gen_meta: Vec<serde_json::Value>,
            gen_meta_calls: Vec<(String, u32)>,
        }

        let mk_step = |tokens: u64, sid: &str| {
            serde_json::json!({
                "type": "CORTEX_STEP_TYPE_PLANNER_RESPONSE",
                "status": "CORTEX_STEP_STATUS_DONE",
                "metadata": {
                    "createdAt": "2026-11-26T10:00:00Z",
                    "finishedGeneratingAt": "2026-11-26T10:00:01Z",
                    "generatorModel": "MODEL_PLACEHOLDER_M501",
                    "modelUsage": {
                        "inputTokens": tokens.to_string(),
                        "cacheReadTokens": "0",
                        "outputTokens": "100",
                        "responseOutputTokens": "100",
                        "apiProvider": "API_PROVIDER_ANTHROPIC_VERTEX",
                        "responseHeader": { "sessionID": sid }
                    }
                }
            })
        };

        let mk_gen = |max_ctx: Option<u64>| match max_ctx {
            Some(n) => serde_json::json!({
                "chatModel": {
                    "chatStartMetadata": {
                        "contextWindowMetadata": { "maxContextTokens": n.to_string() }
                    }
                }
            }),
            None => serde_json::json!({ "chatModel": {} }),
        };

        let state = Arc::new(StdMutex::new(MockState {
            root_steps: vec![mk_step(1000, "s1")],
            sub_steps: vec![mk_step(500, "sub1")],
            root_gen_meta: vec![mk_gen(Some(512_000))],
            gen_meta_calls: Vec::new(),
        }));
        let (delta_tx, delta_rx) = tokio::sync::mpsc::unbounded_channel::<Vec<u8>>();
        let delta_rx = Arc::new(tokio::sync::Mutex::new(Some(delta_rx)));

        let send_summaries = |tx: &tokio::sync::mpsc::UnboundedSender<Vec<u8>>, root_count: u32| {
            let payload = serde_json::json!({
                "updates": {
                    "root-conv": {
                        "trajectoryId": "traj-root",
                        "stepCount": root_count,
                        "status": "CASCADE_RUN_STATUS_IDLE"
                    },
                    "sub-conv": {
                        "trajectoryId": "traj-sub",
                        "stepCount": 1,
                        "status": "CASCADE_RUN_STATUS_IDLE",
                        "trajectoryMetadata": {
                            "parentConversationId": "root-conv"
                        }
                    }
                }
            });
            let _ = tx.send(encode_connect_frame(0x00, payload.to_string().as_bytes()));
        };
        send_summaries(&delta_tx, 1);

        let st_steps = Arc::clone(&state);
        let st_gen = Arc::clone(&state);
        let router = Router::new()
            .route(
                "/exa.language_server_pb.LanguageServerService/GetCascadeModelConfigData",
                post(|| async {
                    axum::Json(serde_json::json!({
                        "clientModelConfigs": [{
                            "label": "Gemini 4 Neon",
                            "modelOrAlias": { "model": "MODEL_PLACEHOLDER_M501" },
                            "provider": "MODEL_PROVIDER_GOOGLE",
                            "modelId": "gemini-4-neon"
                        }]
                    }))
                }),
            )
            .route(
                "/exa.language_server_pb.LanguageServerService/GetCascadeTrajectorySteps",
                post(move |axum::Json(req): axum::Json<serde_json::Value>| {
                    let st = Arc::clone(&st_steps);
                    async move {
                        let cid = req["cascadeId"].as_str().unwrap_or("");
                        let off = req["stepOffset"].as_u64().unwrap_or(0) as usize;
                        let guard = st.lock().unwrap();
                        let src = if cid == "root-conv" {
                            &guard.root_steps
                        } else {
                            &guard.sub_steps
                        };
                        let slice = if off < src.len() { &src[off..] } else { &[] };
                        axum::Json(serde_json::json!({ "steps": slice }))
                    }
                }),
            )
            .route(
                "/exa.language_server_pb.LanguageServerService/GetCascadeTrajectoryGeneratorMetadata",
                post(move |axum::Json(req): axum::Json<serde_json::Value>| {
                    let st = Arc::clone(&st_gen);
                    async move {
                        let cid = req["cascadeId"].as_str().unwrap_or("").to_string();
                        let off = req["generatorMetadataOffset"].as_u64().unwrap_or(0) as u32;
                        let mut guard = st.lock().unwrap();
                        guard.gen_meta_calls.push((cid.clone(), off));
                        let off_usize = off as usize;
                        let slice = if cid == "root-conv" && off_usize < guard.root_gen_meta.len() {
                            guard.root_gen_meta[off_usize..].to_vec()
                        } else {
                            Vec::new()
                        };
                        axum::Json(serde_json::json!({ "generatorMetadata": slice }))
                    }
                }),
            )
            .route(
                "/exa.language_server_pb.LanguageServerService/JetboxSubscribeToSummaries",
                post(move || {
                    let rx_slot = Arc::clone(&delta_rx);
                    async move {
                        let rx = rx_slot.lock().await.take().expect("single stream");
                        let stream = futures_util::stream::unfold(rx, |mut rx| async move {
                            rx.recv().await.map(|frame| {
                                (Ok::<_, std::io::Error>(axum::body::Bytes::from(frame)), rx)
                            })
                        });
                        let mut resp = Response::new(Body::from_stream(stream));
                        *resp.status_mut() = StatusCode::OK;
                        resp.headers_mut().insert(
                            header::CONTENT_TYPE,
                            HeaderValue::from_static("application/connect+json"),
                        );
                        resp
                    }
                }),
            );

        let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
        let port = listener.local_addr().expect("addr").port();
        let srv = tokio::spawn(async move {
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
        let (tx, rx) = watch::channel(false);
        let handle = spawn_ls_collector(client, sink, LsCollectorConfig::default(), rx);

        // Wait until initial summary frame is processed so sub-conv is known as a descendant.
        let deadline = Instant::now() + Duration::from_secs(2);
        loop {
            let snap = handle
                .session_stats(Some("root-conv"))
                .await
                .expect("stats");
            if snap.usage.requests == 2 {
                assert_eq!(snap.max_context_tokens, Some(512_000));
                break;
            }
            assert!(Instant::now() < deadline, "timed out waiting for rollup");
            tokio::time::sleep(Duration::from_millis(15)).await;
        }

        // Only root-conv was queried for generator metadata (never sub-conv), at offset 0.
        assert_eq!(
            state.lock().unwrap().gen_meta_calls,
            vec![("root-conv".to_string(), 0)]
        );

        // Idle poll issues zero extra generator metadata RPCs.
        let idle_snap = handle
            .session_stats(Some("root-conv"))
            .await
            .expect("idle stats");
        assert_eq!(idle_snap.max_context_tokens, Some(512_000));
        assert_eq!(state.lock().unwrap().gen_meta_calls.len(), 1);

        // Global session_stats(None) must return None for max_context_tokens.
        let global_snap = handle.session_stats(None).await.expect("global stats");
        assert_eq!(global_snap.max_context_tokens, None);

        // Turn 2: model switch to 200K + a trailing entry with missing/zero maxContextTokens (must not clobber).
        {
            let mut guard = state.lock().unwrap();
            guard.root_steps.push(mk_step(2000, "s2"));
            guard.root_gen_meta.push(mk_gen(Some(200_000)));
            guard.root_gen_meta.push(mk_gen(Some(0)));
            guard.root_gen_meta.push(mk_gen(None));
        }
        send_summaries(&delta_tx, 2);
        let deadline = Instant::now() + Duration::from_secs(2);
        loop {
            let snap = handle
                .session_stats(Some("root-conv"))
                .await
                .expect("turn2 stats");
            if snap.max_context_tokens == Some(200_000) {
                break;
            }
            assert!(Instant::now() < deadline, "timed out waiting for 200K");
            tokio::time::sleep(Duration::from_millis(15)).await;
        }
        assert_eq!(
            state.lock().unwrap().gen_meta_calls,
            vec![("root-conv".to_string(), 0), ("root-conv".to_string(), 1)]
        );

        // RevertToStep: step_count drops from 2 -> 1; offset resets to 0 and window reverts to 512K.
        {
            let mut guard = state.lock().unwrap();
            guard.root_steps.truncate(1);
            guard.root_gen_meta.truncate(1);
        }
        send_summaries(&delta_tx, 1);
        let deadline = Instant::now() + Duration::from_secs(2);
        loop {
            let snap = handle
                .session_stats(Some("root-conv"))
                .await
                .expect("reverted stats");
            if snap.max_context_tokens == Some(512_000) {
                break;
            }
            assert!(
                Instant::now() < deadline,
                "timed out waiting for revert to 512K"
            );
            tokio::time::sleep(Duration::from_millis(15)).await;
        }
        assert_eq!(
            state.lock().unwrap().gen_meta_calls.last(),
            Some(&("root-conv".to_string(), 0))
        );

        drop(delta_tx);
        let _ = tx.send(true);
        srv.abort();
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn generator_metadata_error_fails_open_without_tripping_circuit_breaker() {
        for status in [StatusCode::NOT_FOUND, StatusCode::INTERNAL_SERVER_ERROR] {
            let router = Router::new()
                .route(
                    "/exa.language_server_pb.LanguageServerService/GetCascadeModelConfigData",
                    post(|| async {
                        axum::Json(serde_json::json!({
                            "clientModelConfigs": [{
                                "label": "Gemini 4 Neon",
                                "modelOrAlias": { "model": "MODEL_PLACEHOLDER_M501" },
                                "provider": "MODEL_PROVIDER_GOOGLE",
                                "modelId": "gemini-4-neon"
                            }]
                        }))
                    }),
                )
                .route(
                    "/exa.language_server_pb.LanguageServerService/GetCascadeTrajectorySteps",
                    post(|| async {
                        axum::Json(serde_json::json!({
                            "steps": [{
                                "type": "CORTEX_STEP_TYPE_PLANNER_RESPONSE",
                                "status": "CORTEX_STEP_STATUS_DONE",
                                "metadata": {
                                    "createdAt": "2026-11-26T10:00:00Z",
                                    "finishedGeneratingAt": "2026-11-26T10:00:01Z",
                                    "generatorModel": "MODEL_PLACEHOLDER_M501",
                                    "modelUsage": {
                                        "inputTokens": "1000",
                                        "cacheReadTokens": "500",
                                        "outputTokens": "100",
                                        "responseOutputTokens": "100",
                                        "apiProvider": "API_PROVIDER_ANTHROPIC_VERTEX",
                                        "responseHeader": { "sessionID": "s-err" }
                                    }
                                }
                            }]
                        }))
                    }),
                )
                .route(
                    "/exa.language_server_pb.LanguageServerService/GetCascadeTrajectoryGeneratorMetadata",
                    post(move || async move { status }),
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

            let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
            let port = listener.local_addr().expect("addr").port();
            let srv = tokio::spawn(async move {
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
            let (tx, rx) = watch::channel(false);
            let handle = spawn_ls_collector(client, sink, LsCollectorConfig::default(), rx);

            let snap = handle
                .session_stats(Some("conv-fail-open"))
                .await
                .expect("stats must still succeed on generator metadata error");
            assert_eq!(snap.usage.requests, 1);
            assert_eq!(snap.max_context_tokens, None);
            assert!(
                handle.is_active().await,
                "generator metadata {status} must NOT trip circuit breaker"
            );

            let _ = tx.send(true);
            srv.abort();
        }
    }

    #[test]
    fn consistent_provider_model_normalization_across_builtin_gemini_proxy_and_codex_xhigh() {
        assert_eq!(
            canonicalize_model_id("companion/gpt-6.2-astra-xhigh"),
            "gpt-6.2-astra"
        );
        assert_eq!(
            canonicalize_model_id("companion/gpt-6.2-astra-high"),
            "gpt-6.2-astra"
        );
        assert_eq!(
            canonicalize_model_id("companion/gemini-4-hydrogen-xhigh"),
            "gemini-4-hydrogen"
        );

        let mut catalog = ModelCatalog::default();
        catalog.update(
            vec![
                LsModelConfig {
                    label: Some("Gemini 4 Neon".to_string()),
                    model_or_alias: Some(companion_providers::LsModelOrAlias {
                        model: Some("MODEL_PLACEHOLDER_M501".to_string()),
                        alias: None,
                    }),
                    model_id: Some("gemini-4-neon".to_string()),
                    provider: Some("MODEL_PROVIDER_GOOGLE".to_string()),
                    model_url: None,
                    ..Default::default()
                },
                LsModelConfig {
                    label: Some("Gemini 4 Hydrogen (High)".to_string()),
                    model_or_alias: Some(companion_providers::LsModelOrAlias {
                        model: Some("MODEL_PLACEHOLDER_M601".to_string()),
                        alias: None,
                    }),
                    model_id: Some("companion/gemini-4-hydrogen-high".to_string()),
                    provider: Some("MODEL_PROVIDER_GOOGLE".to_string()),
                    model_url: Some("gemini-api://gemini-4-hydrogen".to_string()),
                    ..Default::default()
                },
                LsModelConfig {
                    label: Some("GPT 6.2 Astra (Extra High)".to_string()),
                    model_or_alias: Some(companion_providers::LsModelOrAlias {
                        model: Some("MODEL_PLACEHOLDER_M701".to_string()),
                        alias: None,
                    }),
                    model_id: Some("companion/gpt-6.2-astra-xhigh".to_string()),
                    provider: Some("MODEL_PROVIDER_GOOGLE".to_string()),
                    model_url: Some("gemini-api://gpt-6.2-astra".to_string()),
                    ..Default::default()
                },
                LsModelConfig {
                    label: Some("GPT 5.2 (Vertex)".to_string()),
                    model_or_alias: Some(companion_providers::LsModelOrAlias {
                        model: Some("MODEL_PLACEHOLDER_M801".to_string()),
                        alias: None,
                    }),
                    model_id: Some("gpt-5.2".to_string()),
                    provider: Some("MODEL_PROVIDER_OPENAI".to_string()),
                    model_url: None,
                    ..Default::default()
                },
            ],
            Instant::now(),
        );

        // 1. Built-in Gemini
        let builtin_meta = LsStepMetadata {
            generator_model: Some("MODEL_PLACEHOLDER_M501".to_string()),
            ..Default::default()
        };
        let builtin_usage = LsModelUsage {
            api_provider: Some("API_PROVIDER_GOOGLE_GEMINI".to_string()),
            ..Default::default()
        };
        let builtin_attr = resolve_attribution(&builtin_meta, &builtin_usage, &catalog);
        assert_eq!(
            (builtin_attr.provider.as_str(), builtin_attr.model.as_str()),
            ("antigravity", "gemini-4-neon")
        );

        // 2. Gemini Proxy
        let gemini_proxy_meta = LsStepMetadata {
            generator_model: Some("MODEL_PLACEHOLDER_M601".to_string()),
            ..Default::default()
        };
        let gemini_proxy_usage = LsModelUsage {
            api_provider: Some("API_PROVIDER_GOOGLE_GEMINI".to_string()),
            ..Default::default()
        };
        let gemini_proxy_attr =
            resolve_attribution(&gemini_proxy_meta, &gemini_proxy_usage, &catalog);
        assert_eq!(
            (
                gemini_proxy_attr.provider.as_str(),
                gemini_proxy_attr.model.as_str()
            ),
            ("gemini", "gemini-4-hydrogen")
        );

        // 3. Injected OpenAI Codex Proxy (modelUrl starts with gemini-api://, modelId ends with -xhigh)
        let codex_meta = LsStepMetadata {
            generator_model: Some("MODEL_PLACEHOLDER_M701".to_string()),
            ..Default::default()
        };
        let codex_usage = LsModelUsage {
            api_provider: Some("API_PROVIDER_GOOGLE_GEMINI".to_string()),
            ..Default::default()
        };
        let codex_attr = resolve_attribution(&codex_meta, &codex_usage, &catalog);
        assert_eq!(
            (
                codex_attr.provider.as_str(),
                codex_attr.source.as_str(),
                codex_attr.model.as_str()
            ),
            ("openai-codex", "proxy", "gpt-6.2-astra")
        );

        // Generator metadata entry resolves to the exact same (provider, model) key!
        let codex_gen_entry = LsGeneratorMetadata {
            chat_model: Some(companion_providers::LsChatModelMetadata {
                model: Some("MODEL_PLACEHOLDER_M701".to_string()),
                usage: Some(codex_usage),
                response_model: None,
                model_display_name: None,
                chat_start_metadata: Some(companion_providers::LsChatStartMetadata {
                    context_window_metadata: Some(companion_providers::LsContextWindowMetadata {
                        max_context_tokens: 400_000,
                    }),
                }),
            }),
            step_indices: Vec::new(),
        };
        assert_eq!(
            resolve_generator_metadata_model(&codex_gen_entry, &catalog),
            Some(("openai-codex".to_string(), "gpt-6.2-astra".to_string()))
        );

        // 4. Built-in Vertex OpenAI remains vertex_openai (not reclassified to openai-codex).
        let vertex_openai_meta = LsStepMetadata {
            generator_model: Some("MODEL_PLACEHOLDER_M801".to_string()),
            ..Default::default()
        };
        let vertex_openai_usage = LsModelUsage {
            api_provider: Some("API_PROVIDER_OPENAI_VERTEX".to_string()),
            ..Default::default()
        };
        let vertex_openai_attr =
            resolve_attribution(&vertex_openai_meta, &vertex_openai_usage, &catalog);
        assert_eq!(
            (
                vertex_openai_attr.provider.as_str(),
                vertex_openai_attr.source.as_str(),
                vertex_openai_attr.model.as_str()
            ),
            ("vertex_openai", "builtin", "gpt-5.2")
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn observed_models_persistence_startup_preload_update_on_change_and_global_none() {
        use std::sync::Mutex as StdMutex;

        #[derive(Default)]
        struct MockState {
            steps: Vec<serde_json::Value>,
            gen_meta_status: Option<StatusCode>,
            gen_meta: Vec<serde_json::Value>,
        }

        let mk_step = |resp_id: &str| {
            serde_json::json!({
                "type": "CORTEX_STEP_TYPE_PLANNER_RESPONSE",
                "status": "CORTEX_STEP_STATUS_DONE",
                "metadata": {
                    "createdAt": "2026-11-26T10:00:00Z",
                    "finishedGeneratingAt": "2026-11-26T10:00:01Z",
                    "generatorModel": "MODEL_PLACEHOLDER_M501",
                    "modelUsage": {
                        "inputTokens": "1000",
                        "cacheReadTokens": "500",
                        "outputTokens": "100",
                        "responseOutputTokens": "100",
                        "apiProvider": "API_PROVIDER_GOOGLE_GEMINI",
                        "responseId": resp_id,
                        "responseHeader": { "sessionID": "builtin-sess-1" }
                    }
                }
            })
        };
        let mk_gen = |limit: u64| {
            serde_json::json!({
                "chatModel": {
                    "model": "MODEL_PLACEHOLDER_M501",
                    "usage": {
                        "apiProvider": "API_PROVIDER_GOOGLE_GEMINI",
                        "responseHeader": { "sessionID": "builtin-sess-1" }
                    },
                    "chatStartMetadata": {
                        "contextWindowMetadata": { "maxContextTokens": limit.to_string() }
                    }
                }
            })
        };

        let mock_state = Arc::new(StdMutex::new(MockState {
            steps: vec![mk_step("r1")],
            gen_meta_status: None,
            gen_meta: vec![mk_gen(512_000)],
        }));

        let build_router = |st: Arc<StdMutex<MockState>>| {
            let st_steps = Arc::clone(&st);
            let st_gen = Arc::clone(&st);
            Router::new()
                .route(
                    "/exa.language_server_pb.LanguageServerService/GetCascadeModelConfigData",
                    post(|| async {
                        axum::Json(serde_json::json!({
                            "clientModelConfigs": [{
                                "label": "Gemini 4 Neon",
                                "modelOrAlias": { "model": "MODEL_PLACEHOLDER_M501" },
                                "provider": "MODEL_PROVIDER_GOOGLE",
                                "modelId": "gemini-4-neon"
                            }]
                        }))
                    }),
                )
                .route(
                    "/exa.language_server_pb.LanguageServerService/GetCascadeTrajectorySteps",
                    post(move |axum::Json(req): axum::Json<serde_json::Value>| {
                        let st = Arc::clone(&st_steps);
                        async move {
                            let offset =
                                req.get("stepOffset").and_then(|v| v.as_u64()).unwrap_or(0)
                                    as usize;
                            let guard = st.lock().unwrap();
                            let slice = if offset < guard.steps.len() {
                                guard.steps[offset..].to_vec()
                            } else {
                                Vec::new()
                            };
                            axum::Json(serde_json::json!({ "steps": slice }))
                        }
                    }),
                )
                .route(
                    "/exa.language_server_pb.LanguageServerService/GetCascadeTrajectoryGeneratorMetadata",
                    post(move |axum::Json(req): axum::Json<serde_json::Value>| {
                        let st = Arc::clone(&st_gen);
                        async move {
                            let offset = req
                                .get("generatorMetadataOffset")
                                .and_then(|v| v.as_u64())
                                .unwrap_or(0) as usize;
                            let guard = st.lock().unwrap();
                            if let Some(code) = guard.gen_meta_status {
                                return (code, axum::Json(serde_json::json!({})));
                            }
                            let slice = if offset < guard.gen_meta.len() {
                                guard.gen_meta[offset..].to_vec()
                            } else {
                                Vec::new()
                            };
                            (
                                StatusCode::OK,
                                axum::Json(serde_json::json!({ "generatorMetadata": slice })),
                            )
                        }
                    }),
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
                )
        };

        let tmp = TempDir::new().expect("tmp");
        let usage_dir = tmp.path().join("usage");

        // --- Collector 1: observes 512_000 and persists to observed_models ---
        let listener1 = TcpListener::bind("127.0.0.1:0").await.expect("bind 1");
        let port1 = listener1.local_addr().expect("addr 1").port();
        let router1 = build_router(Arc::clone(&mock_state));
        let srv1 = tokio::spawn(async move {
            let _ = axum::serve(listener1, router1).await;
        });
        tokio::task::yield_now().await;

        let sink1 = Arc::new(SqliteUsageSink::new(&usage_dir).expect("sink1"));
        let client1 = LsClient::new(
            LsEndpoint::parse(&format!("127.0.0.1:{port1}")).expect("ep1"),
            CsrfToken::parse(TEST_CSRF).expect("csrf"),
        )
        .expect("client1");
        let (tx1, rx1) = watch::channel(false);
        let handle1 = spawn_ls_collector(
            client1,
            Arc::clone(&sink1),
            LsCollectorConfig::default(),
            rx1,
        );

        let snap1 = handle1
            .session_stats(Some("conv-obs-1"))
            .await
            .expect("snap1");
        assert_eq!(snap1.max_context_tokens, Some(512_000));

        // Global view (None) MUST remain None.
        let global1 = handle1.session_stats(None).await.expect("global1");
        assert_eq!(global1.max_context_tokens, None);

        // Verify persisted in usage.db.
        let persisted = sink1.load_all_observed_models().expect("load persisted");
        assert_eq!(persisted.len(), 1);
        assert_eq!(persisted[0].provider, "antigravity");
        assert_eq!(persisted[0].model, "gemini-4-neon");
        assert_eq!(persisted[0].compaction_limit_tokens, Some(512_000));

        let _ = tx1.send(true);
        srv1.abort();

        // --- Collector 2: starts against same usage.db with LS generator metadata failing (500) ---
        mock_state.lock().unwrap().gen_meta_status = Some(StatusCode::INTERNAL_SERVER_ERROR);

        let listener2 = TcpListener::bind("127.0.0.1:0").await.expect("bind 2");
        let port2 = listener2.local_addr().expect("addr 2").port();
        let router2 = build_router(Arc::clone(&mock_state));
        let srv2 = tokio::spawn(async move {
            let _ = axum::serve(listener2, router2).await;
        });
        tokio::task::yield_now().await;

        let sink2 = Arc::new(SqliteUsageSink::new(&usage_dir).expect("sink2"));
        let client2 = LsClient::new(
            LsEndpoint::parse(&format!("127.0.0.1:{port2}")).expect("ep2"),
            CsrfToken::parse(TEST_CSRF).expect("csrf"),
        )
        .expect("client2");
        let (tx2, rx2) = watch::channel(false);
        let handle2 = spawn_ls_collector(
            client2,
            Arc::clone(&sink2),
            LsCollectorConfig::default(),
            rx2,
        );

        // Even though generator metadata returns 500, focused session resolves 512_000 from preloaded observed_models.
        let snap2 = handle2
            .session_stats(Some("conv-obs-2"))
            .await
            .expect("snap2");
        assert_eq!(snap2.max_context_tokens, Some(512_000));
        assert_eq!(
            handle2
                .session_stats(None)
                .await
                .expect("global2")
                .max_context_tokens,
            None
        );

        // Now LS recovers and reports a new compaction limit (256_000) on a new step.
        {
            let mut g = mock_state.lock().unwrap();
            g.gen_meta_status = None;
            g.steps.push(mk_step("r2"));
            g.gen_meta.push(mk_gen(256_000));
        }
        let snap2_updated = handle2
            .session_stats(Some("conv-obs-2"))
            .await
            .expect("snap2 updated");
        assert_eq!(snap2_updated.max_context_tokens, Some(256_000));

        let persisted_after = sink2
            .load_all_observed_models()
            .expect("load persisted after update");
        assert_eq!(persisted_after.len(), 1);
        assert_eq!(persisted_after[0].compaction_limit_tokens, Some(256_000));

        let _ = tx2.send(true);
        srv2.abort();
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn generator_metadata_response_too_large_tail_seeking_recovery_and_pathological_fallback()
    {
        use std::sync::Mutex as StdMutex;

        #[derive(Default)]
        struct TailSeekMock {
            /// Total virtual generator metadata entries.
            total_entries: usize,
            /// Any request with `offset < too_large_below` returns > MAX_LS_RESPONSE_BYTES.
            too_large_below: usize,
            /// Compaction limit for entries `[0..total_entries - 1]` vs latest entry.
            latest_limit: u64,
            /// Recorded offsets requested from GetCascadeTrajectoryGeneratorMetadata.
            requested_offsets: Vec<u32>,
            /// Steps returned for GetCascadeTrajectorySteps.
            steps: Vec<serde_json::Value>,
        }

        let oversized_body = axum::body::Bytes::from(vec![
            b' ';
            companion_providers::MAX_LS_RESPONSE_BYTES
                + 1024
        ]);
        let mk_step = |resp_id: &str| {
            serde_json::json!({
                "type": "CORTEX_STEP_TYPE_PLANNER_RESPONSE",
                "status": "CORTEX_STEP_STATUS_DONE",
                "metadata": {
                    "createdAt": "2026-11-26T10:00:00Z",
                    "finishedGeneratingAt": "2026-11-26T10:00:01Z",
                    "generatorModel": "MODEL_PLACEHOLDER_M501",
                    "modelUsage": {
                        "inputTokens": "1000",
                        "cacheReadTokens": "500",
                        "outputTokens": "100",
                        "responseOutputTokens": "100",
                        "apiProvider": "API_PROVIDER_GOOGLE_GEMINI",
                        "responseId": resp_id,
                        "responseHeader": { "sessionID": "builtin-tail-1" }
                    }
                }
            })
        };

        let mock = Arc::new(StdMutex::new(TailSeekMock {
            total_entries: 1600,
            too_large_below: 1000,
            latest_limit: 512_000,
            requested_offsets: Vec::new(),
            steps: vec![mk_step("tail-s1")],
        }));

        let (delta_tx, delta_rx) = tokio::sync::mpsc::channel::<Vec<u8>>(16);
        let delta_rx_slot = Arc::new(tokio::sync::Mutex::new(Some(delta_rx)));

        let send_summary = |tx: &tokio::sync::mpsc::Sender<Vec<u8>>, cid: &str, step_count: u32| {
            let payload = serde_json::json!({
                "updates": {
                    cid: {
                        "trajectoryId": format!("traj-{cid}"),
                        "stepCount": step_count,
                        "lastModifiedTime": "2026-11-26T10:00:05Z",
                        "status": "CASCADE_RUN_STATUS_IDLE"
                    }
                }
            });
            tx.try_send(encode_connect_frame(0x00, payload.to_string().as_bytes()))
                .expect("send summary");
        };

        let mock_steps = Arc::clone(&mock);
        let mock_gen = Arc::clone(&mock);
        let oversized_clone = oversized_body.clone();
        let router = Router::new()
            .route(
                "/exa.language_server_pb.LanguageServerService/GetCascadeModelConfigData",
                post(|| async {
                    axum::Json(serde_json::json!({
                        "clientModelConfigs": [{
                            "label": "Gemini 4 Neon",
                            "modelOrAlias": { "model": "MODEL_PLACEHOLDER_M501" },
                            "provider": "MODEL_PROVIDER_GOOGLE",
                            "modelId": "gemini-4-neon"
                        }]
                    }))
                }),
            )
            .route(
                "/exa.language_server_pb.LanguageServerService/GetCascadeTrajectorySteps",
                post(move |axum::Json(req): axum::Json<serde_json::Value>| {
                    let st = Arc::clone(&mock_steps);
                    async move {
                        let offset = req["stepOffset"].as_u64().unwrap_or(0) as usize;
                        let prefix: usize = if req["cascadeId"] == "conv-large-4600" { 4599 } else { 0 };
                        let guard = st.lock().unwrap();
                        let mut steps = vec![serde_json::json!({"status": "CORTEX_STEP_STATUS_DONE"}); prefix.saturating_sub(offset)];
                        steps.extend(guard.steps.iter().skip(offset.saturating_sub(prefix)).cloned());
                        axum::Json(serde_json::json!({ "steps": steps }))
                    }
                }),
            )
            .route(
                "/exa.language_server_pb.LanguageServerService/GetCascadeTrajectoryGeneratorMetadata",
                post(move |axum::Json(req): axum::Json<serde_json::Value>| {
                    let st = Arc::clone(&mock_gen);
                    let big = oversized_clone.clone();
                    async move {
                        let offset = req
                            .get("generatorMetadataOffset")
                            .and_then(|v| v.as_u64())
                            .unwrap_or(0) as usize;
                        let mut guard = st.lock().unwrap();
                        guard.requested_offsets.push(offset as u32);
                        if offset >= guard.total_entries {
                            return Response::builder()
                                .status(StatusCode::OK)
                                .header(header::CONTENT_TYPE, "application/json")
                                .body(Body::from(r#"{"generatorMetadata":[]}"#))
                                .unwrap();
                        }
                        if offset < guard.too_large_below {
                            return Response::builder()
                                .status(StatusCode::OK)
                                .header(header::CONTENT_TYPE, "application/json")
                                .body(Body::from(big))
                                .unwrap();
                        }
                        let count = guard.total_entries - offset;
                        let limit = guard.latest_limit;
                        let mut entries = Vec::with_capacity(count);
                        for i in 0..count {
                            let tok = if i + 1 == count { limit } else { 512_000 };
                            entries.push(serde_json::json!({
                                "chatModel": {
                                    "model": "MODEL_PLACEHOLDER_M501",
                                    "usage": {
                                        "apiProvider": "API_PROVIDER_GOOGLE_GEMINI",
                                        "responseHeader": { "sessionID": "builtin-tail-1" }
                                    },
                                    "chatStartMetadata": {
                                        "contextWindowMetadata": { "maxContextTokens": tok.to_string() }
                                    }
                                }
                            }));
                        }
                        let body = serde_json::json!({ "generatorMetadata": entries }).to_string();
                        Response::builder()
                            .status(StatusCode::OK)
                            .header(header::CONTENT_TYPE, "application/json")
                            .body(Body::from(body))
                            .unwrap()
                    }
                }),
            )
            .route(
                "/exa.language_server_pb.LanguageServerService/JetboxSubscribeToSummaries",
                post(move || {
                    let slot = Arc::clone(&delta_rx_slot);
                    async move {
                        let rx = slot.lock().await.take().expect("single stream");
                        let stream = futures_util::stream::unfold(rx, |mut rx| async move {
                            rx.recv().await.map(|frame| {
                                (Ok::<_, std::io::Error>(axum::body::Bytes::from(frame)), rx)
                            })
                        });
                        let mut resp = Response::new(Body::from_stream(stream));
                        *resp.status_mut() = StatusCode::OK;
                        resp.headers_mut().insert(
                            header::CONTENT_TYPE,
                            HeaderValue::from_static("application/connect+json"),
                        );
                        resp
                    }
                }),
            );

        let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
        let port = listener.local_addr().expect("addr").port();
        let srv = tokio::spawn(async move {
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
        let (tx, rx) = watch::channel(false);
        let handle = spawn_ls_collector(
            client,
            Arc::clone(&sink),
            LsCollectorConfig {
                snapshot_settle_idle: Duration::from_millis(20),
                snapshot_settle_max: Duration::from_millis(80),
                cascade_debounce: Duration::from_millis(20),
                ..LsCollectorConfig::default()
            },
            rx,
        );

        // --- Case A: Large cascade (4600 steps, 1600 entries, <1000 is >4 MiB) ---
        send_summary(&delta_tx, "conv-large-4600", 4600);
        let deadline = Instant::now() + Duration::from_secs(2);
        loop {
            let has_summary = {
                let state = handle.inner.state.lock().await;
                state.boot_settled && state.summaries.contains_key("conv-large-4600")
            };
            if has_summary {
                break;
            }
            assert!(Instant::now() < deadline, "timed out waiting for summary");
            tokio::time::sleep(Duration::from_millis(10)).await;
        }

        let snap_a = handle
            .session_stats(Some("conv-large-4600"))
            .await
            .expect("snap_a");
        assert_eq!(snap_a.max_context_tokens, Some(512_000));
        let offsets_a = mock.lock().unwrap().requested_offsets.clone();
        assert!(
            offsets_a.len() <= MAX_GEN_META_TAIL_SEEK_RPCS && offsets_a.len() <= 3,
            "expected proactive large-cascade tail-seek in <=3 RPCs, got {offsets_a:?}"
        );
        assert_eq!(offsets_a[0], 2300);

        // Turn 2 on the same large cascade: detects mid-session switch to 256_000 at offset 1600 in 1 RPC!
        {
            let mut g = mock.lock().unwrap();
            g.requested_offsets.clear();
            g.total_entries = 1601;
            g.latest_limit = 256_000;
            g.steps.push(mk_step("tail-s2"));
        }
        send_summary(&delta_tx, "conv-large-4600", 4601);
        let deadline = Instant::now() + Duration::from_secs(2);
        while !handle
            .inner
            .state
            .lock()
            .await
            .persisted_watermarks
            .contains_key("conv-large-4600")
        {
            assert!(Instant::now() < deadline, "turn 2 was not ingested");
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        loop {
            let snap = handle
                .session_stats(Some("conv-large-4600"))
                .await
                .expect("snap_a_turn2");
            if snap.max_context_tokens == Some(256_000) {
                break;
            }
            assert!(
                Instant::now() < deadline,
                "timed out waiting for turn2 256K switch"
            );
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        assert_eq!(mock.lock().unwrap().requested_offsets, vec![1600]);

        // --- Case B: Unknown/small step_count hitting ResponseTooLarge at offset 0 -> exponential expansion + binary search ---
        {
            let mut g = mock.lock().unwrap();
            g.requested_offsets.clear();
            g.total_entries = 900;
            g.too_large_below = 700;
            g.latest_limit = 384_000;
        }
        let snap_b = handle
            .session_stats(Some("conv-small-unknown"))
            .await
            .expect("snap_b");
        assert_eq!(snap_b.max_context_tokens, Some(384_000));
        let offsets_b = mock.lock().unwrap().requested_offsets.clone();
        assert_eq!(offsets_b[0], 0);
        assert!(
            offsets_b.len() <= MAX_GEN_META_TAIL_SEEK_RPCS,
            "RPC count exceeded cap: {offsets_b:?}"
        );

        // --- Case C: Pathological >4 MiB single trailing entry -> falls back to observed_models cache & no idle re-probe ---
        {
            let mut g = mock.lock().unwrap();
            g.requested_offsets.clear();
            g.total_entries = 10;
            g.too_large_below = 10; // even the last single entry at offset 9 is > 4 MiB
        }
        send_summary(&delta_tx, "conv-pathological", 2);
        let deadline = Instant::now() + Duration::from_secs(2);
        loop {
            if handle
                .inner
                .state
                .lock()
                .await
                .summaries
                .contains_key("conv-pathological")
            {
                break;
            }
            assert!(
                Instant::now() < deadline,
                "timed out waiting for conv-pathological summary"
            );
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        let snap_c = handle
            .session_stats(Some("conv-pathological"))
            .await
            .expect("snap_c");
        // Falls back to the latest observed_models limit (384_000 from Case B) for ("antigravity", "gemini-4-neon").
        assert_eq!(snap_c.max_context_tokens, Some(384_000));
        let probe_count = mock.lock().unwrap().requested_offsets.len();
        assert!(
            probe_count > 0 && probe_count <= MAX_GEN_META_TAIL_SEEK_RPCS,
            "expected bounded probe count, got {probe_count}"
        );

        // Idle re-poll at same step_offset MUST NOT re-run tail-seek!
        mock.lock().unwrap().requested_offsets.clear();
        let snap_c_idle = handle
            .session_stats(Some("conv-pathological"))
            .await
            .expect("snap_c_idle");
        assert_eq!(snap_c_idle.max_context_tokens, Some(384_000));
        assert!(
            mock.lock().unwrap().requested_offsets.is_empty(),
            "idle poll must not repeat generator metadata tail-seek"
        );

        drop(delta_tx);
        let _ = tx.send(true);
        srv.abort();
    }

    #[test]
    fn resolve_generator_metadata_model_ignores_empty_chat_model_and_uses_usage_model() {
        let catalog = make_catalog();
        let entry = LsGeneratorMetadata {
            chat_model: Some(companion_providers::LsChatModelMetadata {
                model: Some("   ".to_string()),
                usage: Some(LsModelUsage {
                    model: Some("MODEL_PLACEHOLDER_M467".to_string()),
                    api_provider: Some("24".to_string()),
                    response_header: Some(LsResponseHeaderFingerprint {
                        session_id_upper: Some("builtin-sess".to_string()),
                        ..Default::default()
                    }),
                    ..Default::default()
                }),
                response_model: Some("".to_string()),
                model_display_name: None,
                chat_start_metadata: None,
            }),
            step_indices: Vec::new(),
        };
        assert_eq!(
            generator_metadata_model_enum(&entry),
            Some("MODEL_PLACEHOLDER_M467")
        );
        assert_eq!(
            resolve_generator_metadata_model(&entry, &catalog),
            Some(("antigravity".to_string(), "gemini-4-neon".to_string()))
        );

        let all_empty = LsGeneratorMetadata {
            chat_model: Some(companion_providers::LsChatModelMetadata {
                model: Some("".to_string()),
                usage: Some(LsModelUsage {
                    model: Some("  ".to_string()),
                    ..Default::default()
                }),
                response_model: Some("".to_string()),
                model_display_name: None,
                chat_start_metadata: None,
            }),
            step_indices: Vec::new(),
        };
        assert_eq!(generator_metadata_model_enum(&all_empty), None);
        assert_eq!(resolve_generator_metadata_model(&all_empty, &catalog), None);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn idle_poll_with_inflight_step_or_no_summary_skips_reprobe_and_revert_reprobes() {
        use std::sync::Mutex as StdMutex;

        struct MockState {
            steps: Vec<serde_json::Value>,
            pathological: bool,
            total_entries: usize,
            latest_limit: u64,
            requested_offsets: Vec<u32>,
        }

        let mk_step = |status: &str, resp_id: &str| {
            serde_json::json!({
                "type": "CORTEX_STEP_TYPE_PLANNER_RESPONSE",
                "status": status,
                "metadata": {
                    "createdAt": "2026-11-26T10:00:00Z",
                    "finishedGeneratingAt": "2026-11-26T10:00:01Z",
                    "generatorModel": "MODEL_PLACEHOLDER_M501",
                    "modelUsage": {
                        "inputTokens": "1000",
                        "cacheReadTokens": "500",
                        "outputTokens": "100",
                        "responseOutputTokens": "100",
                        "apiProvider": "API_PROVIDER_GOOGLE_GEMINI",
                        "responseId": resp_id,
                        "responseHeader": { "sessionID": "builtin-inflight-1" }
                    }
                }
            })
        };

        let oversized_body = axum::body::Bytes::from(vec![
            b' ';
            companion_providers::MAX_LS_RESPONSE_BYTES
                + 1024
        ]);

        let mock = Arc::new(StdMutex::new(MockState {
            steps: vec![
                mk_step("CORTEX_STEP_STATUS_DONE", "inflight-r1"),
                mk_step("CORTEX_STEP_STATUS_GENERATING", "inflight-r2"),
            ],
            pathological: true,
            total_entries: 10,
            latest_limit: 512_000,
            requested_offsets: Vec::new(),
        }));

        let (delta_tx, delta_rx) = tokio::sync::mpsc::channel::<Vec<u8>>(16);
        let delta_rx_slot = Arc::new(tokio::sync::Mutex::new(Some(delta_rx)));

        let send_summary =
            |tx: &tokio::sync::mpsc::Sender<Vec<u8>>, cid: &str, traj_id: &str, step_count: u32| {
                let payload = serde_json::json!({
                    "updates": {
                        cid: {
                            "trajectoryId": traj_id,
                            "stepCount": step_count,
                            "lastModifiedTime": "2026-11-26T10:00:05Z",
                            "status": "CASCADE_RUN_STATUS_RUNNING"
                        }
                    }
                });
                tx.try_send(encode_connect_frame(0x00, payload.to_string().as_bytes()))
                    .expect("send summary");
            };

        let mock_steps = Arc::clone(&mock);
        let mock_gen = Arc::clone(&mock);
        let big_clone = oversized_body.clone();
        let router = Router::new()
            .route(
                "/exa.language_server_pb.LanguageServerService/GetCascadeModelConfigData",
                post(|| async {
                    axum::Json(serde_json::json!({
                        "clientModelConfigs": [{
                            "label": "Gemini 4 Neon",
                            "modelOrAlias": { "model": "MODEL_PLACEHOLDER_M501" },
                            "provider": "MODEL_PROVIDER_GOOGLE",
                            "modelId": "gemini-4-neon"
                        }]
                    }))
                }),
            )
            .route(
                "/exa.language_server_pb.LanguageServerService/GetCascadeTrajectorySteps",
                post(move |axum::Json(req): axum::Json<serde_json::Value>| {
                    let st = Arc::clone(&mock_steps);
                    async move {
                        let offset = req
                            .get("stepOffset")
                            .and_then(|v| v.as_u64())
                            .unwrap_or(0) as usize;
                        let guard = st.lock().unwrap();
                        let slice = if offset < guard.steps.len() {
                            guard.steps[offset..].to_vec()
                        } else {
                            Vec::new()
                        };
                        axum::Json(serde_json::json!({ "steps": slice }))
                    }
                }),
            )
            .route(
                "/exa.language_server_pb.LanguageServerService/GetCascadeTrajectoryGeneratorMetadata",
                post(move |axum::Json(req): axum::Json<serde_json::Value>| {
                    let st = Arc::clone(&mock_gen);
                    let big = big_clone.clone();
                    async move {
                        let offset = req
                            .get("generatorMetadataOffset")
                            .and_then(|v| v.as_u64())
                            .unwrap_or(0) as usize;
                        let mut guard = st.lock().unwrap();
                        guard.requested_offsets.push(offset as u32);
                        if offset >= guard.total_entries {
                            return Response::builder()
                                .status(StatusCode::OK)
                                .header(header::CONTENT_TYPE, "application/json")
                                .body(Body::from(r#"{"generatorMetadata":[]}"#))
                                .unwrap();
                        }
                        if guard.pathological {
                            return Response::builder()
                                .status(StatusCode::OK)
                                .header(header::CONTENT_TYPE, "application/json")
                                .body(Body::from(big))
                                .unwrap();
                        }
                        let limit = guard.latest_limit;
                        let body = serde_json::json!({
                            "generatorMetadata": [{
                                "chatModel": {
                                    "model": "MODEL_PLACEHOLDER_M501",
                                    "usage": {
                                        "apiProvider": "API_PROVIDER_GOOGLE_GEMINI",
                                        "responseHeader": { "sessionID": "builtin-inflight-1" }
                                    },
                                    "chatStartMetadata": {
                                        "contextWindowMetadata": { "maxContextTokens": limit.to_string() }
                                    }
                                }
                            }]
                        })
                        .to_string();
                        Response::builder()
                            .status(StatusCode::OK)
                            .header(header::CONTENT_TYPE, "application/json")
                            .body(Body::from(body))
                            .unwrap()
                    }
                }),
            )
            .route(
                "/exa.language_server_pb.LanguageServerService/JetboxSubscribeToSummaries",
                post(move || {
                    let slot = Arc::clone(&delta_rx_slot);
                    async move {
                        let rx = slot.lock().await.take().expect("single stream");
                        let stream = futures_util::stream::unfold(rx, |mut rx| async move {
                            rx.recv().await.map(|frame| {
                                (Ok::<_, std::io::Error>(axum::body::Bytes::from(frame)), rx)
                            })
                        });
                        let mut resp = Response::new(Body::from_stream(stream));
                        *resp.status_mut() = StatusCode::OK;
                        resp.headers_mut().insert(
                            header::CONTENT_TYPE,
                            HeaderValue::from_static("application/connect+json"),
                        );
                        resp
                    }
                }),
            );

        let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
        let port = listener.local_addr().expect("addr").port();
        let srv = tokio::spawn(async move {
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
        let (tx, rx) = watch::channel(false);
        let handle = spawn_ls_collector(
            client,
            Arc::clone(&sink),
            LsCollectorConfig {
                snapshot_settle_idle: Duration::from_millis(20),
                snapshot_settle_max: Duration::from_millis(80),
                cascade_debounce: Duration::from_millis(20),
                ..LsCollectorConfig::default()
            },
            rx,
        );

        // Subcase A: In-flight GENERATING step at index 1 keeps fold.step_offset == 1 < summary_steps (2).
        send_summary(&delta_tx, "conv-inflight", "traj-v1", 2);
        let deadline = Instant::now() + Duration::from_secs(2);
        loop {
            if handle
                .inner
                .state
                .lock()
                .await
                .summaries
                .contains_key("conv-inflight")
            {
                break;
            }
            assert!(Instant::now() < deadline, "timed out waiting for summary");
            tokio::time::sleep(Duration::from_millis(10)).await;
        }

        let snap1 = handle
            .session_stats(Some("conv-inflight"))
            .await
            .expect("snap1");
        assert_eq!(snap1.max_context_tokens, None);
        {
            let guard = handle.inner.state.lock().await;
            let fold = guard.monitor_cache.get("conv-inflight").expect("fold");
            assert_eq!(fold.step_offset, 1);
            assert_eq!(fold.generator_metadata_probed_step, Some(1));
        }
        assert!(!mock.lock().unwrap().requested_offsets.is_empty());

        // Idle poll while step 1 is still GENERATING must issue 0 generator-metadata RPCs.
        mock.lock().unwrap().requested_offsets.clear();
        let snap2 = handle
            .session_stats(Some("conv-inflight"))
            .await
            .expect("snap2");
        assert_eq!(snap2.max_context_tokens, None);
        assert!(
            mock.lock().unwrap().requested_offsets.is_empty(),
            "in-flight GENERATING idle poll must issue 0 gen_meta RPCs"
        );

        // Subcase B: Monitor-only window (no summary cached for conv-no-summary) must also skip re-probe.
        mock.lock().unwrap().requested_offsets.clear();
        let _ = handle
            .session_stats(Some("conv-no-summary"))
            .await
            .expect("no-summary snap1");
        assert!(!mock.lock().unwrap().requested_offsets.is_empty());

        mock.lock().unwrap().requested_offsets.clear();
        let _ = handle
            .session_stats(Some("conv-no-summary"))
            .await
            .expect("no-summary snap2");
        assert!(
            mock.lock().unwrap().requested_offsets.is_empty(),
            "Monitor-only (summary_opt == None) idle poll must issue 0 gen_meta RPCs"
        );

        // Subcase C: RevertToStep (new trajectoryId) resets fold and properly re-probes.
        {
            let mut g = mock.lock().unwrap();
            g.pathological = false;
            g.total_entries = 1;
            g.latest_limit = 256_000;
            g.steps = vec![mk_step("CORTEX_STEP_STATUS_DONE", "reverted-r1")];
            g.requested_offsets.clear();
        }
        send_summary(&delta_tx, "conv-inflight", "traj-v2", 1);
        let deadline = Instant::now() + Duration::from_secs(2);
        loop {
            let updated = handle
                .inner
                .state
                .lock()
                .await
                .summaries
                .get("conv-inflight")
                .and_then(|s| s.trajectory_id.as_deref())
                == Some("traj-v2");
            if updated {
                break;
            }
            assert!(
                Instant::now() < deadline,
                "timed out waiting for revert summary"
            );
            tokio::time::sleep(Duration::from_millis(10)).await;
        }

        let snap_reverted = handle
            .session_stats(Some("conv-inflight"))
            .await
            .expect("snap_reverted");
        assert_eq!(snap_reverted.max_context_tokens, Some(256_000));
        assert_eq!(mock.lock().unwrap().requested_offsets, vec![0]);

        drop(delta_tx);
        let _ = tx.send(true);
        srv.abort();
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn background_ingested_step_still_probes_generator_metadata_for_mid_session_switch() {
        use std::sync::Mutex as StdMutex;

        struct SwitchMock {
            steps: Vec<serde_json::Value>,
            gen_meta: Vec<serde_json::Value>,
            requested_offsets: Vec<u32>,
        }

        let mk_step = |model_enum: &str, api_provider: &str, resp_id: &str| {
            serde_json::json!({
                "type": "CORTEX_STEP_TYPE_PLANNER_RESPONSE",
                "status": "CORTEX_STEP_STATUS_DONE",
                "metadata": {
                    "createdAt": "2026-11-26T10:00:00Z",
                    "finishedGeneratingAt": "2026-11-26T10:00:01Z",
                    "generatorModel": model_enum,
                    "modelUsage": {
                        "inputTokens": "1000",
                        "cacheReadTokens": "500",
                        "outputTokens": "100",
                        "responseOutputTokens": "100",
                        "apiProvider": api_provider,
                        "responseId": resp_id,
                        "responseHeader": { "sessionID": "builtin-switch-1" }
                    }
                }
            })
        };
        let mk_gen = |model_enum: &str, api_provider: &str, limit: u64| {
            serde_json::json!({
                "chatModel": {
                    "model": model_enum,
                    "usage": {
                        "apiProvider": api_provider,
                        "responseHeader": { "sessionID": "builtin-switch-1" }
                    },
                    "chatStartMetadata": {
                        "contextWindowMetadata": { "maxContextTokens": limit.to_string() }
                    }
                }
            })
        };

        let mock = Arc::new(StdMutex::new(SwitchMock {
            steps: vec![mk_step(
                "MODEL_PLACEHOLDER_M501",
                "API_PROVIDER_GOOGLE_GEMINI",
                "sw-r1",
            )],
            gen_meta: vec![mk_gen(
                "MODEL_PLACEHOLDER_M501",
                "API_PROVIDER_GOOGLE_GEMINI",
                512_000,
            )],
            requested_offsets: Vec::new(),
        }));

        let (delta_tx, delta_rx) = tokio::sync::mpsc::channel::<Vec<u8>>(16);
        let delta_rx_slot = Arc::new(tokio::sync::Mutex::new(Some(delta_rx)));

        let send_summary = |tx: &tokio::sync::mpsc::Sender<Vec<u8>>, cid: &str, step_count: u32| {
            let payload = serde_json::json!({
                "updates": {
                    cid: {
                        "trajectoryId": format!("traj-{cid}"),
                        "stepCount": step_count,
                        "lastModifiedTime": "2026-11-26T10:00:05Z",
                        "status": "CASCADE_RUN_STATUS_IDLE"
                    }
                }
            });
            tx.try_send(encode_connect_frame(0x00, payload.to_string().as_bytes()))
                .expect("send summary");
        };

        let mock_steps = Arc::clone(&mock);
        let mock_gen = Arc::clone(&mock);
        let router = Router::new()
            .route(
                "/exa.language_server_pb.LanguageServerService/GetCascadeModelConfigData",
                post(|| async {
                    axum::Json(serde_json::json!({
                        "clientModelConfigs": [
                            {
                                "label": "Gemini 4 Neon",
                                "modelOrAlias": { "model": "MODEL_PLACEHOLDER_M501" },
                                "provider": "MODEL_PROVIDER_GOOGLE",
                                "modelId": "gemini-4-neon"
                            },
                            {
                                "label": "Claude Opus 5.5",
                                "modelOrAlias": { "model": "MODEL_PLACEHOLDER_M601" },
                                "provider": "MODEL_PROVIDER_ANTHROPIC",
                                "modelId": "claude-opus-5-5"
                            }
                        ]
                    }))
                }),
            )
            .route(
                "/exa.language_server_pb.LanguageServerService/GetCascadeTrajectorySteps",
                post(move |axum::Json(req): axum::Json<serde_json::Value>| {
                    let st = Arc::clone(&mock_steps);
                    async move {
                        let offset = req
                            .get("stepOffset")
                            .and_then(|v| v.as_u64())
                            .unwrap_or(0) as usize;
                        let guard = st.lock().unwrap();
                        let slice = if offset < guard.steps.len() {
                            guard.steps[offset..].to_vec()
                        } else {
                            Vec::new()
                        };
                        axum::Json(serde_json::json!({ "steps": slice }))
                    }
                }),
            )
            .route(
                "/exa.language_server_pb.LanguageServerService/GetCascadeTrajectoryGeneratorMetadata",
                post(move |axum::Json(req): axum::Json<serde_json::Value>| {
                    let st = Arc::clone(&mock_gen);
                    async move {
                        let offset = req
                            .get("generatorMetadataOffset")
                            .and_then(|v| v.as_u64())
                            .unwrap_or(0) as usize;
                        let mut guard = st.lock().unwrap();
                        guard.requested_offsets.push(offset as u32);
                        let slice = if offset < guard.gen_meta.len() {
                            guard.gen_meta[offset..].to_vec()
                        } else {
                            Vec::new()
                        };
                        axum::Json(serde_json::json!({ "generatorMetadata": slice }))
                    }
                }),
            )
            .route(
                "/exa.language_server_pb.LanguageServerService/JetboxSubscribeToSummaries",
                post(move || {
                    let slot = Arc::clone(&delta_rx_slot);
                    async move {
                        let rx = slot.lock().await.take().expect("single stream");
                        let stream = futures_util::stream::unfold(rx, |mut rx| async move {
                            rx.recv().await.map(|frame| {
                                (Ok::<_, std::io::Error>(axum::body::Bytes::from(frame)), rx)
                            })
                        });
                        let mut resp = Response::new(Body::from_stream(stream));
                        *resp.status_mut() = StatusCode::OK;
                        resp.headers_mut().insert(
                            header::CONTENT_TYPE,
                            HeaderValue::from_static("application/connect+json"),
                        );
                        resp
                    }
                }),
            );

        let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
        let port = listener.local_addr().expect("addr").port();
        let srv = tokio::spawn(async move {
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
        let (tx, rx) = watch::channel(false);
        let handle = spawn_ls_collector(
            client,
            Arc::clone(&sink),
            LsCollectorConfig {
                snapshot_settle_idle: Duration::from_millis(20),
                snapshot_settle_max: Duration::from_millis(80),
                cascade_debounce: Duration::from_millis(20),
                ..LsCollectorConfig::default()
            },
            rx,
        );

        send_summary(&delta_tx, "conv-bg-switch", 1);
        let deadline = Instant::now() + Duration::from_secs(2);
        loop {
            let ready = {
                let guard = handle.inner.state.lock().await;
                guard.boot_settled && guard.summaries.contains_key("conv-bg-switch")
            };
            if ready {
                break;
            }
            assert!(
                Instant::now() < deadline,
                "timed out waiting for initial summary and boot settle"
            );
            tokio::time::sleep(Duration::from_millis(10)).await;
        }

        // Initial hydration observes 512_000 for ("antigravity", "gemini-4-neon").
        let snap1 = handle
            .session_stats(Some("conv-bg-switch"))
            .await
            .expect("snap1");
        assert_eq!(snap1.max_context_tokens, Some(512_000));

        // Mid-session switch to Claude Opus 5.5 (400_000): push step 1 and let background
        // ingest_dirty_cascade advance monitor_cache.step_offset to 2 BEFORE session_stats runs.
        {
            let mut g = mock.lock().unwrap();
            g.steps.push(mk_step(
                "MODEL_PLACEHOLDER_M601",
                "API_PROVIDER_ANTHROPIC_VERTEX",
                "sw-r2",
            ));
            g.gen_meta.push(mk_gen(
                "MODEL_PLACEHOLDER_M601",
                "API_PROVIDER_ANTHROPIC_VERTEX",
                400_000,
            ));
            g.requested_offsets.clear();
        }
        send_summary(&delta_tx, "conv-bg-switch", 2);

        let deadline = Instant::now() + Duration::from_secs(2);
        loop {
            let offset = handle
                .inner
                .state
                .lock()
                .await
                .monitor_cache
                .get("conv-bg-switch")
                .map(|f| f.step_offset);
            if offset == Some(2) {
                break;
            }
            assert!(
                Instant::now() < deadline,
                "timed out waiting for background ingest_dirty_cascade to advance step_offset to 2"
            );
            tokio::time::sleep(Duration::from_millis(10)).await;
        }

        // Confirm precondition: fold already at step_offset == 2 with stale max_context_tokens == Some(512_000).
        {
            let guard = handle.inner.state.lock().await;
            let fold = guard.monitor_cache.get("conv-bg-switch").expect("fold");
            assert_eq!(fold.step_offset, 2);
            assert_eq!(fold.max_context_tokens, Some(512_000));
            assert_eq!(fold.generator_metadata_probed_step, Some(1));
        }
        assert!(mock.lock().unwrap().requested_offsets.is_empty());

        // session_stats must STILL probe generator metadata and update both snapshot and observed_models.
        let snap2 = handle
            .session_stats(Some("conv-bg-switch"))
            .await
            .expect("snap2");
        assert_eq!(snap2.max_context_tokens, Some(400_000));
        assert_eq!(mock.lock().unwrap().requested_offsets, vec![1]);

        let observed = sink.load_all_observed_models().expect("load observed");
        assert!(
            observed.iter().any(|m| {
                m.provider == "antigravity"
                    && m.model == "gemini-4-neon"
                    && m.compaction_limit_tokens == Some(512_000)
            }),
            "expected initial model in observed_models: {observed:?}"
        );
        assert!(
            observed.iter().any(|m| {
                m.provider == "vertex_claude"
                    && m.model == "claude-opus-5-5"
                    && m.compaction_limit_tokens == Some(400_000)
            }),
            "expected switched model in observed_models: {observed:?}"
        );

        drop(delta_tx);
        let _ = tx.send(true);
        srv.abort();
    }

    #[test]
    fn canonicalize_model_id_strips_xhigh_and_tiered_suffixes() {
        assert_eq!(
            canonicalize_model_id("gemini-4-hydrogen-tiered-high"),
            "gemini-4-hydrogen"
        );
        assert_eq!(
            canonicalize_model_id("gemini-4-hydrogen-tiered-xhigh"),
            "gemini-4-hydrogen"
        );
        assert_eq!(
            canonicalize_model_id("gemini-4-hydrogen-tiered"),
            "gemini-4-hydrogen"
        );
        assert_eq!(
            canonicalize_model_id("companion/gpt-6.2-sol-xhigh"),
            "gpt-6.2-sol"
        );
        assert_eq!(
            canonicalize_model_id("gemini-api://gpt-6.2-astra-medium"),
            "gpt-6.2-astra"
        );
    }

    #[test]
    fn resolve_attribution_classifies_codex_companion_and_uncatalogued_codex_as_openai_codex() {
        let mut catalog = make_catalog();
        catalog.update(
            vec![LsModelConfig {
                label: Some("GPT-6.2 Sol (Codex) (Medium)".to_string()),
                model_or_alias: Some(LsModelOrAlias {
                    model: Some("MODEL_PLACEHOLDER_M356".to_string()),
                    alias: None,
                }),
                provider: Some("MODEL_PROVIDER_GOOGLE".to_string()),
                model_url: Some("gemini-api://gpt-6.2-sol".to_string()),
                model_id: Some("companion/gpt-6.2-sol-medium".to_string()),
                context_window_options: Vec::new(),
            }],
            Instant::now(),
        );

        // 1. Companion-injected Codex model with API_PROVIDER_GOOGLE_GEMINI must be openai-codex, NOT gemini.
        let codex_cat = resolve_attribution(
            &LsStepMetadata {
                generator_model: Some("MODEL_PLACEHOLDER_M356".to_string()),
                ..Default::default()
            },
            &LsModelUsage {
                api_provider: Some("API_PROVIDER_GOOGLE_GEMINI".to_string()),
                response_header: Some(LsResponseHeaderFingerprint {
                    response_id: Some("resp_codex_cat".to_string()),
                    ..Default::default()
                }),
                ..Default::default()
            },
            &catalog,
        );
        assert_eq!(codex_cat.provider, "openai-codex");
        assert_eq!(codex_cat.source, "proxy");
        assert_eq!(codex_cat.model, "gpt-6.2-sol");

        // 2. Uncatalogued Codex model with only model_info.model_url = gemini-api://gpt-6.2-astra
        let codex_uncat = resolve_attribution(
            &LsStepMetadata {
                generator_model: Some("MODEL_PLACEHOLDER_M352".to_string()),
                model_info: Some(LsModelInfo {
                    model_url: Some("gemini-api://gpt-6.2-astra".to_string()),
                    ..Default::default()
                }),
                ..Default::default()
            },
            &LsModelUsage {
                api_provider: Some("API_PROVIDER_GOOGLE_GEMINI".to_string()),
                response_header: Some(LsResponseHeaderFingerprint {
                    response_id: Some("resp_codex_uncat".to_string()),
                    ..Default::default()
                }),
                ..Default::default()
            },
            &catalog,
        );
        assert_eq!(codex_uncat.provider, "openai-codex");
        assert_eq!(codex_uncat.source, "proxy");
        assert_eq!(codex_uncat.model, "gpt-6.2-astra");

        // 3. Built-in Vertex OpenAI (gpt-5.2) remains vertex_openai, NOT openai-codex.
        let vertex_openai = resolve_attribution(
            &LsStepMetadata {
                generator_model: Some("MODEL_PLACEHOLDER_M601".to_string()),
                ..Default::default()
            },
            &LsModelUsage {
                api_provider: Some("32".to_string()),
                response_header: Some(LsResponseHeaderFingerprint {
                    session_id_upper: Some("sess-builtin".to_string()),
                    ..Default::default()
                }),
                ..Default::default()
            },
            &catalog,
        );
        assert_eq!(vertex_openai.provider, "vertex_openai");
        assert_eq!(vertex_openai.source, "builtin");
        assert_eq!(vertex_openai.model, "gpt-5.2");
    }

    #[test]
    fn model_catalog_normalizes_numeric_preserves_learned_across_updates_and_merges_upstream() {
        let upstream = Arc::new(companion_core::UpstreamModelCatalog::new());
        upstream.update_from_fetch_available_models(
            br#"{
                "models": {
                    "gemini-4-neon-tiered-high": {
                        "model": "MODEL_PLACEHOLDER_M532",
                        "displayName": "Gemini 4 Neon (High)",
                        "modelProvider": "MODEL_PROVIDER_GOOGLE",
                        "apiProvider": "API_PROVIDER_GOOGLE_GEMINI"
                    }
                }
            }"#,
        );

        let mut catalog = ModelCatalog::with_upstream_catalog(Some(Arc::clone(&upstream)));
        catalog.update(
            vec![LsModelConfig {
                label: Some("Initial Hydrogen".to_string()),
                model_or_alias: Some(LsModelOrAlias {
                    model: Some("525".to_string()),
                    alias: None,
                }),
                provider: Some("MODEL_PROVIDER_GOOGLE".to_string()),
                model_url: None,
                model_id: Some("gemini-4-hydrogen-high".to_string()),
                context_window_options: Vec::new(),
            }],
            Instant::now(),
        );
        // Numeric "525" resolves "MODEL_PLACEHOLDER_M525" and vice versa.
        assert_eq!(
            catalog
                .lookup("MODEL_PLACEHOLDER_M525")
                .and_then(|e| e.model_id.as_deref()),
            Some("gemini-4-hydrogen-high")
        );

        // Learn M530 from GeneratorMetadata.
        catalog.learn_from_generator_metadata(&[companion_providers::LsGeneratorMetadata {
            step_indices: vec![26],
            chat_model: Some(companion_providers::LsChatModelMetadata {
                model: Some("MODEL_PLACEHOLDER_M530".to_string()),
                usage: None,
                response_model: Some("gemini-4-hydrogen-tiered-high".to_string()),
                model_display_name: Some("Gemini 4 Hydrogen (High)".to_string()),
                chat_start_metadata: None,
            }),
        }]);

        // Subsequent update with a different config must NOT wipe M525 or learned M530.
        catalog.update(
            vec![LsModelConfig {
                label: Some("Gemini 4 Carbon".to_string()),
                model_or_alias: Some(LsModelOrAlias {
                    model: Some("MODEL_PLACEHOLDER_M427".to_string()),
                    alias: None,
                }),
                provider: Some("MODEL_PROVIDER_GOOGLE".to_string()),
                model_url: None,
                model_id: Some("gemini-4-carbon-high".to_string()),
                context_window_options: Vec::new(),
            }],
            Instant::now(),
        );

        assert_eq!(
            catalog
                .lookup("MODEL_PLACEHOLDER_M525")
                .and_then(|e| e.model_id.as_deref()),
            Some("gemini-4-hydrogen-high")
        );
        assert_eq!(
            catalog.lookup("530").and_then(|e| e.model_id.as_deref()),
            Some("gemini-4-hydrogen-tiered-high")
        );
        // Upstream fetchAvailableModels entry is visible too.
        assert_eq!(
            catalog
                .lookup("MODEL_PLACEHOLDER_M532")
                .and_then(|e| e.model_id.as_deref()),
            Some("gemini-4-neon-tiered-high")
        );
    }

    #[test]
    fn resolve_attribution_prioritizes_dynamic_sources_then_static_fallback_then_display_name() {
        let mut catalog = ModelCatalog::default();

        // 1. Static fallback resolves M530 & M400 & M537 even with empty catalog and null modelInfo.
        let m530_static = resolve_attribution(
            &LsStepMetadata {
                generator_model: Some("MODEL_PLACEHOLDER_M530".to_string()),
                ..Default::default()
            },
            &LsModelUsage {
                api_provider: Some("API_PROVIDER_GOOGLE_GEMINI".to_string()),
                response_header: Some(LsResponseHeaderFingerprint {
                    session_id_upper: Some("builtin-sess".to_string()),
                    ..Default::default()
                }),
                ..Default::default()
            },
            &catalog,
        );
        assert_eq!(m530_static.provider, "antigravity");
        assert_eq!(m530_static.source, "builtin");
        assert_eq!(m530_static.model, "gemini-4-hydrogen");
        assert_eq!(
            m530_static.model_display_name.as_deref(),
            Some("Gemini 4 Hydrogen")
        );

        let m400_static = resolve_attribution(
            &LsStepMetadata {
                generator_model: Some("MODEL_PLACEHOLDER_M400".to_string()),
                ..Default::default()
            },
            &LsModelUsage::default(),
            &catalog,
        );
        assert_eq!(m400_static.provider, "vertex_claude");
        assert_eq!(m400_static.model, "claude-opus-5-5");

        // 2. Dynamic catalog overrides static fallback if M530 points elsewhere dynamically.
        catalog.update(
            vec![LsModelConfig {
                label: Some("Custom Dynamic Model".to_string()),
                model_or_alias: Some(LsModelOrAlias {
                    model: Some("MODEL_PLACEHOLDER_M530".to_string()),
                    alias: None,
                }),
                provider: Some("MODEL_PROVIDER_GOOGLE".to_string()),
                model_url: None,
                model_id: Some("gemini-4-neon-tiered-high".to_string()),
                context_window_options: Vec::new(),
            }],
            Instant::now(),
        );
        let m530_dynamic = resolve_attribution(
            &LsStepMetadata {
                generator_model: Some("MODEL_PLACEHOLDER_M530".to_string()),
                ..Default::default()
            },
            &LsModelUsage {
                response_header: Some(LsResponseHeaderFingerprint {
                    session_id_upper: Some("builtin-sess".to_string()),
                    ..Default::default()
                }),
                ..Default::default()
            },
            &catalog,
        );
        assert_eq!(m530_dynamic.model, "gemini-4-neon");
        assert_eq!(
            m530_dynamic.model_display_name.as_deref(),
            Some("Custom Dynamic Model")
        );

        // 3. Unknown future placeholder with label present & model_id missing/placeholder falls back to label.
        catalog.update(
            vec![LsModelConfig {
                label: Some("Gemini 5 Nova (High)".to_string()),
                model_or_alias: Some(LsModelOrAlias {
                    model: Some("MODEL_PLACEHOLDER_M999".to_string()),
                    alias: None,
                }),
                provider: Some("MODEL_PROVIDER_GOOGLE".to_string()),
                model_url: None,
                model_id: Some("MODEL_PLACEHOLDER_M999".to_string()),
                context_window_options: Vec::new(),
            }],
            Instant::now(),
        );
        let m999 = resolve_attribution(
            &LsStepMetadata {
                generator_model: Some("MODEL_PLACEHOLDER_M999".to_string()),
                ..Default::default()
            },
            &LsModelUsage::default(),
            &catalog,
        );
        assert_eq!(m999.model, "Gemini 5 Nova (High)");
        assert_eq!(
            m999.model_display_name.as_deref(),
            Some("Gemini 5 Nova (High)")
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn collector_learns_unresolved_placeholder_via_generator_metadata_with_rate_limit() {
        let gen_meta_hits = Arc::new(AtomicUsize::new(0));
        let gen_meta_hits_srv = Arc::clone(&gen_meta_hits);

        let router = Router::new()
            .route(
                "/exa.language_server_pb.LanguageServerService/GetCascadeModelConfigData",
                post(|| async {
                    // Omit M998 completely from GetCascadeModelConfigData
                    axum::Json(serde_json::json!({ "clientModelConfigs": [] }))
                }),
            )
            .route(
                "/exa.language_server_pb.LanguageServerService/GetCascadeTrajectorySteps",
                post(|| async {
                    axum::Json(serde_json::json!({
                        "steps": [{
                            "type": "CORTEX_STEP_TYPE_PLANNER_RESPONSE",
                            "status": "CORTEX_STEP_STATUS_DONE",
                            "metadata": {
                                "createdAt": "2026-11-26T10:00:00Z",
                                "finishedGeneratingAt": "2026-11-26T10:00:01Z",
                                "generatorModel": "MODEL_PLACEHOLDER_M998",
                                "modelUsage": {
                                    "model": "MODEL_PLACEHOLDER_M998",
                                    "inputTokens": "1000",
                                    "cacheReadTokens": "500",
                                    "outputTokens": "100",
                                    "responseOutputTokens": "100",
                                    "apiProvider": "API_PROVIDER_GOOGLE_GEMINI",
                                    "responseHeader": { "sessionID": "s-m998" }
                                }
                            }
                        }]
                    }))
                }),
            )
            .route(
                "/exa.language_server_pb.LanguageServerService/GetCascadeTrajectoryGeneratorMetadata",
                post(move || {
                    let hits = Arc::clone(&gen_meta_hits_srv);
                    async move {
                        hits.fetch_add(1, Ordering::SeqCst);
                        axum::Json(serde_json::json!({
                            "generatorMetadata": [{
                                "stepIndices": [0],
                                "chatModel": {
                                    "model": "MODEL_PLACEHOLDER_M998",
                                    "responseModel": "gemini-5-nova-tiered-high",
                                    "modelDisplayName": "Gemini 5 Nova (High)",
                                    "chatStartMetadata": {
                                        "contextWindowMetadata": { "maxContextTokens": 512000 }
                                    }
                                }
                            }]
                        }))
                    }
                }),
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

        let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
        let port = listener.local_addr().expect("addr").port();
        let srv = tokio::spawn(async move {
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
        let (tx, rx) = watch::channel(false);
        let handle = spawn_ls_collector(client, sink, LsCollectorConfig::default(), rx);

        let snap = handle
            .session_stats(Some("conv-m998"))
            .await
            .expect("session stats");
        assert_eq!(snap.by_model.len(), 1);
        assert_eq!(snap.by_model[0].model, "gemini-5-nova");
        assert_eq!(
            snap.by_model[0].model_display_name.as_deref(),
            Some("Gemini 5 Nova (High)")
        );
        assert_eq!(snap.by_model[0].provider, "antigravity");
        assert_eq!(snap.max_context_tokens, Some(512_000));
        assert_eq!(
            gen_meta_hits.load(Ordering::SeqCst),
            1,
            "should reuse GeneratorMetadata result in the same hydration cycle without duplicate RPC"
        );

        let _ = tx.send(true);
        srv.abort();
    }

    #[test]
    fn model_catalog_update_preserves_existing_fields_when_subsequent_entry_has_none() {
        let mut catalog = ModelCatalog::default();
        catalog.update(
            vec![LsModelConfig {
                label: Some("Gemini 5 Alpha (High)".to_string()),
                model_or_alias: Some(LsModelOrAlias {
                    model: Some("MODEL_PLACEHOLDER_M901".to_string()),
                    alias: None,
                }),
                provider: Some("MODEL_PROVIDER_GOOGLE".to_string()),
                model_url: Some("https://example.invalid/m901".to_string()),
                model_id: Some("gemini-5-alpha-high".to_string()),
                context_window_options: Vec::new(),
            }],
            Instant::now(),
        );

        // Subsequent partial config omits model_id/provider/model_url (and even label) -> must not regress.
        catalog.update(
            vec![LsModelConfig {
                label: None,
                model_or_alias: Some(LsModelOrAlias {
                    model: Some("MODEL_PLACEHOLDER_M901".to_string()),
                    alias: None,
                }),
                provider: None,
                model_url: None,
                model_id: None,
                context_window_options: Vec::new(),
            }],
            Instant::now(),
        );

        let entry = catalog
            .lookup("MODEL_PLACEHOLDER_M901")
            .expect("entry should remain present");
        assert_eq!(entry.model_id.as_deref(), Some("gemini-5-alpha-high"));
        assert_eq!(entry.provider.as_deref(), Some("MODEL_PROVIDER_GOOGLE"));
        assert_eq!(entry.label.as_deref(), Some("Gemini 5 Alpha (High)"));
        assert_eq!(
            entry.model_url.as_deref(),
            Some("https://example.invalid/m901")
        );

        // Partial update with a new label and None model_id updates label while keeping model_id/provider.
        catalog.update(
            vec![LsModelConfig {
                label: Some("Gemini 5 Alpha (Updated)".to_string()),
                model_or_alias: Some(LsModelOrAlias {
                    model: Some("MODEL_PLACEHOLDER_M901".to_string()),
                    alias: None,
                }),
                provider: None,
                model_url: None,
                model_id: None,
                context_window_options: Vec::new(),
            }],
            Instant::now(),
        );
        let updated = catalog
            .lookup("MODEL_PLACEHOLDER_M901")
            .expect("entry after partial update");
        assert_eq!(updated.model_id.as_deref(), Some("gemini-5-alpha-high"));
        assert_eq!(updated.provider.as_deref(), Some("MODEL_PROVIDER_GOOGLE"));
        assert_eq!(updated.label.as_deref(), Some("Gemini 5 Alpha (Updated)"));
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn collector_resolves_sequential_uncatalogued_placeholders_within_rate_limit_for_sqlite_and_monitor()
     {
        let (delta_tx, delta_rx) = tokio::sync::mpsc::unbounded_channel::<serde_json::Value>();
        let delta_rx = Arc::new(tokio::sync::Mutex::new(Some(delta_rx)));
        let gen_meta_calls = Arc::new(AtomicUsize::new(0));
        let gen_meta_calls_srv = Arc::clone(&gen_meta_calls);
        let turn_phase = Arc::new(AtomicUsize::new(1));
        let turn_phase_steps = Arc::clone(&turn_phase);
        let turn_phase_gen = Arc::clone(&turn_phase);

        let now_rfc3339 = Utc::now().to_rfc3339_opts(SecondsFormat::Secs, true);
        let created_at_steps = now_rfc3339.clone();

        #[derive(serde::Deserialize)]
        #[serde(rename_all = "camelCase")]
        struct StepsReq {
            #[serde(default)]
            step_offset: u32,
        }

        #[derive(serde::Deserialize)]
        #[serde(rename_all = "camelCase")]
        struct GenMetaReq {
            #[serde(default)]
            generator_metadata_offset: u32,
        }

        let router = Router::new()
            .route(
                "/exa.language_server_pb.LanguageServerService/GetCascadeModelConfigData",
                post(|| async { axum::Json(serde_json::json!({ "clientModelConfigs": [] })) }),
            )
            .route(
                "/exa.language_server_pb.LanguageServerService/GetCascadeTrajectorySteps",
                post(move |axum::Json(req): axum::Json<StepsReq>| {
                    let phase = turn_phase_steps.load(Ordering::SeqCst);
                    let ts = created_at_steps.clone();
                    async move {
                        let step0 = serde_json::json!({
                            "type": "CORTEX_STEP_TYPE_PLANNER_RESPONSE",
                            "status": "CORTEX_STEP_STATUS_DONE",
                            "metadata": {
                                "createdAt": ts,
                                "finishedGeneratingAt": ts,
                                "generatorModel": "MODEL_PLACEHOLDER_M997",
                                "modelUsage": {
                                    "model": "MODEL_PLACEHOLDER_M997",
                                    "inputTokens": "1000",
                                    "cacheReadTokens": "500",
                                    "outputTokens": "100",
                                    "responseOutputTokens": "100",
                                    "apiProvider": "API_PROVIDER_GOOGLE_GEMINI",
                                    "responseHeader": { "sessionID": "s-turn1" }
                                }
                            }
                        });
                        let step1 = serde_json::json!({
                            "type": "CORTEX_STEP_TYPE_PLANNER_RESPONSE",
                            "status": "CORTEX_STEP_STATUS_DONE",
                            "metadata": {
                                "createdAt": ts,
                                "finishedGeneratingAt": ts,
                                "generatorModel": "MODEL_PLACEHOLDER_M999",
                                "modelUsage": {
                                    "model": "MODEL_PLACEHOLDER_M999",
                                    "inputTokens": "2000",
                                    "cacheReadTokens": "1000",
                                    "outputTokens": "200",
                                    "responseOutputTokens": "200",
                                    "apiProvider": "API_PROVIDER_GOOGLE_GEMINI",
                                    "responseHeader": { "sessionID": "s-turn2" }
                                }
                            }
                        });
                        let all_steps = if phase >= 2 {
                            vec![step0, step1]
                        } else {
                            vec![step0]
                        };
                        let sliced: Vec<_> =
                            all_steps.into_iter().skip(req.step_offset as usize).collect();
                        axum::Json(serde_json::json!({ "steps": sliced }))
                    }
                }),
            )
            .route(
                "/exa.language_server_pb.LanguageServerService/GetCascadeTrajectoryGeneratorMetadata",
                post(move |axum::Json(req): axum::Json<GenMetaReq>| {
                    let hits = Arc::clone(&gen_meta_calls_srv);
                    let phase = turn_phase_gen.load(Ordering::SeqCst);
                    async move {
                        hits.fetch_add(1, Ordering::SeqCst);
                        let gm0 = serde_json::json!({
                            "stepIndices": [0],
                            "chatModel": {
                                "model": "MODEL_PLACEHOLDER_M997",
                                "responseModel": "gemini-5-alpha-tiered-high",
                                "modelDisplayName": "Gemini 5 Alpha (High)",
                                "chatStartMetadata": {
                                    "contextWindowMetadata": { "maxContextTokens": 512000 }
                                }
                            }
                        });
                        let gm1 = serde_json::json!({
                            "stepIndices": [1],
                            "chatModel": {
                                "model": "MODEL_PLACEHOLDER_M999",
                                "responseModel": "gemini-5-beta-tiered-high",
                                "modelDisplayName": "Gemini 5 Beta (High)",
                                "chatStartMetadata": {
                                    "contextWindowMetadata": { "maxContextTokens": 512000 }
                                }
                            }
                        });
                        let all_gm = if phase >= 2 {
                            vec![gm0, gm1]
                        } else {
                            vec![gm0]
                        };
                        let sliced: Vec<_> = all_gm
                            .into_iter()
                            .skip(req.generator_metadata_offset as usize)
                            .collect();
                        axum::Json(serde_json::json!({ "generatorMetadata": sliced }))
                    }
                }),
            )
            .route(
                "/exa.language_server_pb.LanguageServerService/JetboxSubscribeToSummaries",
                post(move || {
                    let rx_holder = Arc::clone(&delta_rx);
                    async move {
                        let (tx, rx) =
                            tokio::sync::mpsc::channel::<Result<Vec<u8>, std::io::Error>>(16);
                        let mut maybe_rx = rx_holder.lock().await.take();
                        tokio::spawn(async move {
                            let boot_frame = encode_connect_frame(0x00, b"{}");
                            if tx.send(Ok(boot_frame)).await.is_err() {
                                return;
                            }
                            if let Some(ref mut ext_rx) = maybe_rx {
                                while let Some(payload) = ext_rx.recv().await {
                                    let frame = encode_connect_frame(
                                        0x00,
                                        &serde_json::to_vec(&payload).unwrap(),
                                    );
                                    if tx.send(Ok(frame)).await.is_err() {
                                        break;
                                    }
                                }
                            }
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
                    }
                }),
            );

        let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
        let port = listener.local_addr().expect("addr").port();
        let srv = tokio::spawn(async move {
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
            snapshot_settle_idle: Duration::from_millis(40),
            snapshot_settle_max: Duration::from_millis(200),
            cascade_debounce: Duration::from_millis(10),
            catalog_refresh_min_interval: Duration::from_secs(60),
            ..Default::default()
        };
        let handle = spawn_ls_collector(client, Arc::clone(&sink), cfg, shutdown_rx);

        // Wait for boot settle.
        let deadline = Instant::now() + Duration::from_secs(3);
        while !handle.inner.state.lock().await.boot_settled {
            assert!(Instant::now() < deadline, "collector failed to settle");
            tokio::time::sleep(Duration::from_millis(10)).await;
        }

        // Turn 1: M997
        delta_tx
            .send(serde_json::json!({
                "updates": {
                    "conv-1c": {
                        "trajectoryId": "traj-1c",
                        "stepCount": 1,
                        "createdTime": now_rfc3339,
                        "lastModifiedTime": now_rfc3339
                    }
                }
            }))
            .expect("send turn 1 delta");

        let deadline = Instant::now() + Duration::from_secs(3);
        loop {
            if !sink.read_all_events().expect("read").is_empty() {
                break;
            }
            assert!(Instant::now() < deadline, "turn 1 event not ingested");
            tokio::time::sleep(Duration::from_millis(15)).await;
        }

        let snap1 = handle
            .session_stats(Some("conv-1c"))
            .await
            .expect("monitor snap 1");
        assert_eq!(snap1.by_model.len(), 1);
        assert_eq!(snap1.by_model[0].model, "gemini-5-alpha");

        // Turn 2 (< 60s later): switches to M999 in the same cascade.
        turn_phase.store(2, Ordering::SeqCst);
        delta_tx
            .send(serde_json::json!({
                "updates": {
                    "conv-1c": {
                        "trajectoryId": "traj-1c",
                        "stepCount": 2,
                        "createdTime": now_rfc3339,
                        "lastModifiedTime": now_rfc3339
                    }
                }
            }))
            .expect("send turn 2 delta");

        let deadline = Instant::now() + Duration::from_secs(3);
        loop {
            if sink.read_all_events().expect("read").len() >= 2 {
                break;
            }
            assert!(Instant::now() < deadline, "turn 2 event not ingested");
            tokio::time::sleep(Duration::from_millis(15)).await;
        }

        let events = sink.read_all_events().expect("read all events");
        assert_eq!(events.len(), 2);
        assert_eq!(events[0].model, "gemini-5-alpha");
        assert_eq!(
            events[0].model_display_name.as_deref(),
            Some("Gemini 5 Alpha (High)")
        );
        assert_eq!(events[1].model, "gemini-5-beta");
        assert_eq!(
            events[1].model_display_name.as_deref(),
            Some("Gemini 5 Beta (High)")
        );

        let snap2 = handle
            .session_stats(Some("conv-1c"))
            .await
            .expect("monitor snap 2");
        let monitor_models: Vec<&str> = snap2.by_model.iter().map(|m| m.model.as_str()).collect();
        assert!(
            monitor_models.contains(&"gemini-5-alpha") && monitor_models.contains(&"gemini-5-beta"),
            "Monitor must resolve both models instead of caching MODEL_PLACEHOLDER_M999: {monitor_models:?}"
        );

        // Repeated lookup for the SAME uncatalogued placeholder that fails to resolve remains rate-limited to 60s.
        let before_repeat = gen_meta_calls.load(Ordering::SeqCst);
        let first_unmapped = resolve_unknown_model_enum_via_generator_metadata(
            &handle.inner,
            "conv-1c",
            &["MODEL_PLACEHOLDER_M994".to_string()],
        )
        .await;
        assert!(first_unmapped.is_some());
        assert_eq!(gen_meta_calls.load(Ordering::SeqCst), before_repeat + 1);

        let second_unmapped = resolve_unknown_model_enum_via_generator_metadata(
            &handle.inner,
            "conv-1c",
            &["MODEL_PLACEHOLDER_M994".to_string()],
        )
        .await;
        assert!(
            second_unmapped.is_none(),
            "same (cascade_id, unknown_enum) must remain rate-limited within 60s"
        );
        assert_eq!(gen_meta_calls.load(Ordering::SeqCst), before_repeat + 1);

        drop(delta_tx);
        let _ = shutdown_tx.send(true);
        srv.abort();
    }
}
