use std::path::{Path, PathBuf};
use std::sync::{Arc, RwLock};
use std::time::{Duration, Instant};

use companion_core::fs_safe::{inspect_file_stamp, read_verified_bytes_for_stamp};
use companion_core::{
    AuthStore, CODEX_AUTH_FILENAME, CodexCredentials, Config, CoreError, FileStamp, FsError,
    GEMINI_AUTH_FILENAME, GeminiCredentials, MAX_AUTH_FILE_BYTES, MAX_CONFIG_FILE_BYTES,
    RejectedReason, inspect_auth_file_stamp, read_verified_auth_bytes_for_stamp, validate_path,
};
use companion_providers::CodexAuth;

use crate::{ActiveProvider, ProxyError, build_active_provider};

const RACY_CLEAN_WINDOW: Duration = Duration::from_secs(1);

struct WatchedFileState<T> {
    value: T,
    stamp: FileStamp,
    raw_bytes: Option<Vec<u8>>,
    checked_at: Instant,
    last_failed_stamp: Option<FileStamp>,
    last_failed_bytes: Option<Vec<u8>>,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum FileFastDecision {
    Cached,
    VerifySameStampBytes { match_failed_bytes: bool },
    NeedsReload,
}

struct WatchedFileBaseline<T> {
    value: T,
    stamp: FileStamp,
    raw_bytes: Option<Vec<u8>>,
    last_failed_stamp: Option<FileStamp>,
    last_failed_bytes: Option<Vec<u8>>,
}

enum FileEvalOutcome<T> {
    Unchanged {
        touch_checked_at: bool,
    },
    Missing {
        already_missing: bool,
        default_value: T,
    },
    SameBytesNewStamp {
        stamp: FileStamp,
    },
    Updated {
        value: T,
        stamp: FileStamp,
        raw_bytes: Vec<u8>,
    },
    Failed {
        failed_stamp: FileStamp,
        failed_bytes: Option<Vec<u8>>,
    },
}

impl<T> FileEvalOutcome<T> {
    fn changed_value(&self) -> bool {
        matches!(
            self,
            Self::Updated { .. }
                | Self::Missing {
                    already_missing: false,
                    ..
                }
        )
    }

    fn effective_value<'a>(&'a self, baseline: &'a T) -> &'a T {
        match self {
            Self::Updated { value, .. } => value,
            Self::Missing {
                already_missing: false,
                default_value,
            } => default_value,
            _ => baseline,
        }
    }

    fn into_failed_after_rebuild_error(self) -> Self {
        match self {
            Self::Updated {
                stamp, raw_bytes, ..
            } => Self::Failed {
                failed_stamp: stamp,
                failed_bytes: Some(raw_bytes),
            },
            Self::Missing {
                already_missing: false,
                ..
            } => Self::Unchanged {
                touch_checked_at: true,
            },
            other => other,
        }
    }
}

impl<T: Clone> WatchedFileState<T> {
    fn new(value: T, stamp: FileStamp, raw_bytes: Option<Vec<u8>>) -> Self {
        Self {
            value,
            stamp,
            raw_bytes,
            checked_at: Instant::now(),
            last_failed_stamp: None,
            last_failed_bytes: None,
        }
    }

    fn fast_check(&self, current_stamp: &FileStamp) -> FileFastDecision {
        let outside_racy_window = self.checked_at.elapsed() >= RACY_CLEAN_WINDOW;
        if *current_stamp == self.stamp {
            if outside_racy_window
                || matches!(
                    current_stamp,
                    FileStamp::Missing | FileStamp::Rejected(_) | FileStamp::Unreadable(_)
                )
            {
                return FileFastDecision::Cached;
            }
            return FileFastDecision::VerifySameStampBytes {
                match_failed_bytes: false,
            };
        }

        if self.last_failed_stamp.as_ref() == Some(current_stamp) {
            if outside_racy_window
                || matches!(
                    current_stamp,
                    FileStamp::Rejected(_) | FileStamp::Unreadable(_)
                )
            {
                return FileFastDecision::Cached;
            }
            return FileFastDecision::VerifySameStampBytes {
                match_failed_bytes: true,
            };
        }

        FileFastDecision::NeedsReload
    }

    fn matches_cached_bytes(
        &self,
        current_stamp: &FileStamp,
        bytes: &[u8],
        match_failed_bytes: bool,
    ) -> bool {
        if match_failed_bytes {
            self.last_failed_stamp.as_ref() == Some(current_stamp)
                && self.last_failed_bytes.as_deref() == Some(bytes)
        } else {
            self.stamp == *current_stamp && self.raw_bytes.as_deref() == Some(bytes)
        }
    }

    fn snapshot_baseline(&self) -> WatchedFileBaseline<T> {
        WatchedFileBaseline {
            value: self.value.clone(),
            stamp: self.stamp.clone(),
            raw_bytes: self.raw_bytes.clone(),
            last_failed_stamp: self.last_failed_stamp.clone(),
            last_failed_bytes: self.last_failed_bytes.clone(),
        }
    }

    fn apply_outcome(&mut self, outcome: FileEvalOutcome<T>) {
        match outcome {
            FileEvalOutcome::Unchanged { touch_checked_at } => {
                if touch_checked_at {
                    self.checked_at = Instant::now();
                }
            }
            FileEvalOutcome::Missing {
                already_missing,
                default_value,
            } => {
                if already_missing {
                    self.last_failed_stamp = None;
                    self.last_failed_bytes = None;
                    self.checked_at = Instant::now();
                } else {
                    self.value = default_value;
                    self.stamp = FileStamp::Missing;
                    self.raw_bytes = None;
                    self.last_failed_stamp = None;
                    self.last_failed_bytes = None;
                    self.checked_at = Instant::now();
                }
            }
            FileEvalOutcome::SameBytesNewStamp { stamp } => {
                self.stamp = stamp;
                self.last_failed_stamp = None;
                self.last_failed_bytes = None;
                self.checked_at = Instant::now();
            }
            FileEvalOutcome::Updated {
                value,
                stamp,
                raw_bytes,
            } => {
                self.value = value;
                self.stamp = stamp;
                self.raw_bytes = Some(raw_bytes);
                self.last_failed_stamp = None;
                self.last_failed_bytes = None;
                self.checked_at = Instant::now();
            }
            FileEvalOutcome::Failed {
                failed_stamp,
                failed_bytes,
            } => {
                self.last_failed_stamp = Some(failed_stamp);
                self.last_failed_bytes = failed_bytes;
                self.checked_at = Instant::now();
            }
        }
    }
}

struct HotReloadInner {
    active: ActiveProvider,
    config_state: WatchedFileState<Config>,
    auth_state: Option<WatchedFileState<Option<GeminiCredentials>>>,
    codex_auth_state: Option<WatchedFileState<Option<CodexCredentials>>>,
}

pub(crate) struct HotReloadProvider {
    config_path: PathBuf,
    auth_store: Arc<dyn AuthStore>,
    codex_auth: Arc<dyn CodexAuth>,
    auth_dir: Option<PathBuf>,
    env_fallback: bool,
    inner: RwLock<HotReloadInner>,
    reload_lock: tokio::sync::Mutex<()>,
}

struct ReloadBaseline {
    active: ActiveProvider,
    config: WatchedFileBaseline<Config>,
    auth: Option<WatchedFileBaseline<Option<GeminiCredentials>>>,
    codex_auth: Option<WatchedFileBaseline<Option<CodexCredentials>>>,
}

struct CombinedReloadOutcome {
    config_outcome: FileEvalOutcome<Config>,
    auth_outcome: Option<FileEvalOutcome<Option<GeminiCredentials>>>,
    codex_auth_outcome: Option<FileEvalOutcome<Option<CodexCredentials>>>,
    new_active: Option<ActiveProvider>,
}

impl HotReloadProvider {
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn new(
        config_path: PathBuf,
        auth_store: Arc<dyn AuthStore>,
        codex_auth: Arc<dyn CodexAuth>,
        initial_config: Config,
        initial_gemini_creds: Option<GeminiCredentials>,
        initial_codex_creds: Option<CodexCredentials>,
        active: ActiveProvider,
        env_fallback: bool,
    ) -> Result<Self, ProxyError> {
        validate_path(&config_path)?;
        let config_stamp = inspect_file_stamp(&config_path);
        if let FileStamp::Rejected(reason) = config_stamp {
            return Err(match reason {
                RejectedReason::Symlink => FsError::SymlinkRejected { path: config_path }.into(),
                RejectedReason::NotRegularFile => {
                    FsError::NotRegularFile { path: config_path }.into()
                }
            });
        }
        let config_raw_bytes =
            read_verified_bytes_for_stamp(&config_path, &config_stamp, MAX_CONFIG_FILE_BYTES)
                .ok()
                .flatten();

        let auth_dir = auth_store.auth_dir().map(Path::to_path_buf);
        let (auth_state, codex_auth_state) = if let Some(ref dir) = auth_dir {
            validate_path(dir)?;
            let auth_stamp = inspect_auth_file_stamp(dir, GEMINI_AUTH_FILENAME);
            if let FileStamp::Rejected(reason) = auth_stamp {
                let auth_path = dir.join(GEMINI_AUTH_FILENAME);
                return Err(match reason {
                    RejectedReason::Symlink => FsError::SymlinkRejected { path: auth_path }.into(),
                    RejectedReason::NotRegularFile => {
                        FsError::NotRegularFile { path: auth_path }.into()
                    }
                });
            }
            let auth_raw_bytes =
                read_verified_auth_bytes_for_stamp(dir, GEMINI_AUTH_FILENAME, &auth_stamp)
                    .ok()
                    .flatten();

            let codex_stamp = inspect_auth_file_stamp(dir, CODEX_AUTH_FILENAME);
            if let FileStamp::Rejected(reason) = codex_stamp {
                let codex_path = dir.join(CODEX_AUTH_FILENAME);
                return Err(match reason {
                    RejectedReason::Symlink => FsError::SymlinkRejected { path: codex_path }.into(),
                    RejectedReason::NotRegularFile => {
                        FsError::NotRegularFile { path: codex_path }.into()
                    }
                });
            }
            let codex_raw_bytes =
                read_verified_auth_bytes_for_stamp(dir, CODEX_AUTH_FILENAME, &codex_stamp)
                    .ok()
                    .flatten();

            (
                Some(WatchedFileState::new(
                    initial_gemini_creds,
                    auth_stamp,
                    auth_raw_bytes,
                )),
                Some(WatchedFileState::new(
                    initial_codex_creds,
                    codex_stamp,
                    codex_raw_bytes,
                )),
            )
        } else {
            (None, None)
        };

        Ok(Self {
            config_path,
            auth_store,
            codex_auth,
            auth_dir,
            env_fallback,
            inner: RwLock::new(HotReloadInner {
                active,
                config_state: WatchedFileState::new(initial_config, config_stamp, config_raw_bytes),
                auth_state,
                codex_auth_state,
            }),
            reload_lock: tokio::sync::Mutex::new(()),
        })
    }

    pub(crate) async fn active_provider(&self) -> ActiveProvider {
        // Immediate off-thread stat of config.json + sibling auth/{gemini,codex}.json in one
        // blocking call so config or credential changes are observed instantaneously without
        // holding locks.
        let stat_config_path = self.config_path.clone();
        let stat_auth_dir = self.auth_dir.clone();
        let (cfg_stamp, auth_stamp, codex_stamp) = match tokio::task::spawn_blocking(move || {
            let cfg = inspect_file_stamp(&stat_config_path);
            let auth = stat_auth_dir
                .as_deref()
                .map(|dir| inspect_auth_file_stamp(dir, GEMINI_AUTH_FILENAME));
            let codex = stat_auth_dir
                .as_deref()
                .map(|dir| inspect_auth_file_stamp(dir, CODEX_AUTH_FILENAME));
            (cfg, auth, codex)
        })
        .await
        {
            Ok(stamps) => stamps,
            Err(err) => {
                tracing::warn!(
                    error = %err,
                    "hot-reload stat task failed; retaining previous configuration"
                );
                return self.current_active();
            }
        };

        let (cfg_dec, auth_dec, codex_dec) = {
            let guard = self.inner.read().unwrap_or_else(|e| e.into_inner());
            let cfg_dec = guard.config_state.fast_check(&cfg_stamp);
            let auth_dec = match (&guard.auth_state, &auth_stamp) {
                (Some(st), Some(stamp)) => st.fast_check(stamp),
                _ => FileFastDecision::Cached,
            };
            let codex_dec = match (&guard.codex_auth_state, &codex_stamp) {
                (Some(st), Some(stamp)) => st.fast_check(stamp),
                _ => FileFastDecision::Cached,
            };
            if cfg_dec == FileFastDecision::Cached
                && auth_dec == FileFastDecision::Cached
                && codex_dec == FileFastDecision::Cached
            {
                return guard.active.clone();
            }
            (cfg_dec, auth_dec, codex_dec)
        };

        if cfg_dec != FileFastDecision::NeedsReload
            && auth_dec != FileFastDecision::NeedsReload
            && codex_dec != FileFastDecision::NeedsReload
        {
            let verify_cfg = matches!(cfg_dec, FileFastDecision::VerifySameStampBytes { .. })
                .then(|| (self.config_path.clone(), cfg_stamp.clone()));
            let verify_auth = matches!(auth_dec, FileFastDecision::VerifySameStampBytes { .. })
                .then(|| self.auth_dir.clone().zip(auth_stamp.clone()))
                .flatten();
            let verify_codex = matches!(codex_dec, FileFastDecision::VerifySameStampBytes { .. })
                .then(|| self.auth_dir.clone().zip(codex_stamp.clone()))
                .flatten();

            let verify_res = tokio::task::spawn_blocking(move || {
                let cfg_bytes = verify_cfg.map(|(path, stamp)| {
                    read_verified_bytes_for_stamp(&path, &stamp, MAX_CONFIG_FILE_BYTES)
                });
                let auth_bytes = verify_auth.map(|(dir, stamp)| {
                    read_verified_auth_bytes_for_stamp(&dir, GEMINI_AUTH_FILENAME, &stamp)
                });
                let codex_bytes = verify_codex.map(|(dir, stamp)| {
                    read_verified_auth_bytes_for_stamp(&dir, CODEX_AUTH_FILENAME, &stamp)
                });
                (cfg_bytes, auth_bytes, codex_bytes)
            })
            .await;

            match verify_res {
                Ok((cfg_bytes_opt, auth_bytes_opt, codex_bytes_opt)) => {
                    let guard = self.inner.read().unwrap_or_else(|e| e.into_inner());
                    let cfg_ok = match (cfg_dec, cfg_bytes_opt) {
                        (FileFastDecision::Cached, _) => true,
                        (
                            FileFastDecision::VerifySameStampBytes { match_failed_bytes },
                            Some(Ok(Some(bytes))),
                        ) => guard.config_state.matches_cached_bytes(
                            &cfg_stamp,
                            &bytes,
                            match_failed_bytes,
                        ),
                        _ => false,
                    };
                    let auth_ok = match (auth_dec, auth_bytes_opt, &guard.auth_state, &auth_stamp) {
                        (FileFastDecision::Cached, _, _, _) => true,
                        (
                            FileFastDecision::VerifySameStampBytes { match_failed_bytes },
                            Some(Ok(Some(bytes))),
                            Some(st),
                            Some(stamp),
                        ) => st.matches_cached_bytes(stamp, &bytes, match_failed_bytes),
                        _ => false,
                    };
                    let codex_ok = match (
                        codex_dec,
                        codex_bytes_opt,
                        &guard.codex_auth_state,
                        &codex_stamp,
                    ) {
                        (FileFastDecision::Cached, _, _, _) => true,
                        (
                            FileFastDecision::VerifySameStampBytes { match_failed_bytes },
                            Some(Ok(Some(bytes))),
                            Some(st),
                            Some(stamp),
                        ) => st.matches_cached_bytes(stamp, &bytes, match_failed_bytes),
                        _ => false,
                    };
                    if cfg_ok && auth_ok && codex_ok {
                        return guard.active.clone();
                    }
                }
                Err(err) => {
                    tracing::warn!(
                        error = %err,
                        "hot-reload byte-verification task failed; retaining previous configuration"
                    );
                    return self.current_active();
                }
            }
        }

        let _reload_guard = self.reload_lock.lock().await;
        let baseline = self.snapshot_baseline();
        let reload_config_path = self.config_path.clone();
        let reload_auth_dir = self.auth_dir.clone();
        let reload_auth_store = Arc::clone(&self.auth_store);
        let reload_codex_auth = Arc::clone(&self.codex_auth);
        let env_fallback = self.env_fallback;
        let outcome = match tokio::task::spawn_blocking(move || {
            evaluate_reload(
                reload_config_path,
                reload_auth_dir,
                reload_auth_store,
                reload_codex_auth,
                env_fallback,
                baseline,
            )
        })
        .await
        {
            Ok(outcome) => outcome,
            Err(err) => {
                tracing::warn!(
                    error = %err,
                    "hot-reload evaluation task failed; retaining previous configuration"
                );
                return self.current_active();
            }
        };

        self.apply_outcome(outcome)
    }

    fn current_active(&self) -> ActiveProvider {
        let guard = self.inner.read().unwrap_or_else(|e| e.into_inner());
        guard.active.clone()
    }

    fn snapshot_baseline(&self) -> ReloadBaseline {
        let guard = self.inner.read().unwrap_or_else(|e| e.into_inner());
        ReloadBaseline {
            active: guard.active.clone(),
            config: guard.config_state.snapshot_baseline(),
            auth: guard
                .auth_state
                .as_ref()
                .map(WatchedFileState::snapshot_baseline),
            codex_auth: guard
                .codex_auth_state
                .as_ref()
                .map(WatchedFileState::snapshot_baseline),
        }
    }

    fn apply_outcome(&self, outcome: CombinedReloadOutcome) -> ActiveProvider {
        let mut guard = self.inner.write().unwrap_or_else(|e| e.into_inner());
        guard.config_state.apply_outcome(outcome.config_outcome);
        if let (Some(auth_state), Some(auth_outcome)) =
            (&mut guard.auth_state, outcome.auth_outcome)
        {
            auth_state.apply_outcome(auth_outcome);
        }
        if let (Some(codex_state), Some(codex_outcome)) =
            (&mut guard.codex_auth_state, outcome.codex_auth_outcome)
        {
            codex_state.apply_outcome(codex_outcome);
        }
        if let Some(new_active) = outcome.new_active {
            guard.active = new_active;
        }
        guard.active.clone()
    }
}

fn failed_with_warn<T>(
    baseline: &WatchedFileBaseline<T>,
    failed_stamp: FileStamp,
    warn: impl FnOnce(),
) -> FileEvalOutcome<T> {
    if baseline.last_failed_stamp.as_ref() != Some(&failed_stamp) {
        warn();
    }
    FileEvalOutcome::Failed {
        failed_stamp,
        failed_bytes: None,
    }
}

fn failed_rejected<T>(
    baseline: &WatchedFileBaseline<T>,
    label: &str,
    reason: RejectedReason,
) -> FileEvalOutcome<T> {
    failed_with_warn(baseline, FileStamp::Rejected(reason), || {
        tracing::warn!(
            file = label,
            reason = ?reason,
            "ignoring rejected path during hot-reload; retaining previous state"
        );
    })
}

fn evaluate_file<T>(
    label: &str,
    max_bytes: u64,
    baseline: &WatchedFileBaseline<T>,
    current_stamp: FileStamp,
    read_bytes: impl FnOnce(&FileStamp) -> Result<Option<Vec<u8>>, FsError>,
    default_on_missing: impl FnOnce() -> T,
    parse_bytes: impl FnOnce(&[u8]) -> Result<T, CoreError>,
) -> FileEvalOutcome<T> {
    match current_stamp {
        FileStamp::Missing => {
            if baseline.stamp == FileStamp::Missing {
                FileEvalOutcome::Missing {
                    already_missing: true,
                    default_value: default_on_missing(),
                }
            } else {
                FileEvalOutcome::Missing {
                    already_missing: false,
                    default_value: default_on_missing(),
                }
            }
        }
        FileStamp::Rejected(reason) => failed_rejected(baseline, label, reason),
        FileStamp::Unreadable(kind) => failed_with_warn(baseline, current_stamp, || {
            tracing::warn!(
                file = label,
                error_kind = ?kind,
                "failed to stat file during hot-reload; retaining previous state"
            );
        }),
        FileStamp::Present { .. } => {
            let bytes = match read_bytes(&current_stamp) {
                Ok(Some(bytes)) => bytes,
                Ok(None) => {
                    return FileEvalOutcome::Missing {
                        already_missing: baseline.stamp == FileStamp::Missing,
                        default_value: default_on_missing(),
                    };
                }
                Err(FsError::SizeLimitExceeded { .. }) => {
                    return failed_with_warn(baseline, current_stamp, || {
                        tracing::warn!(
                            file = label,
                            max_bytes,
                            "ignoring oversized file during hot-reload; retaining previous state"
                        );
                    });
                }
                Err(FsError::SymlinkRejected { .. }) => {
                    return failed_rejected(baseline, label, RejectedReason::Symlink);
                }
                Err(FsError::NotRegularFile { .. } | FsError::NotDirectory { .. }) => {
                    return failed_rejected(baseline, label, RejectedReason::NotRegularFile);
                }
                Err(FsError::ConcurrentModification { .. }) => {
                    tracing::warn!(
                        file = label,
                        "file changed concurrently during hot-reload read; retaining previous state"
                    );
                    return FileEvalOutcome::Unchanged {
                        touch_checked_at: true,
                    };
                }
                Err(err) => {
                    let kind = match &err {
                        FsError::Io { source, .. } => source.kind(),
                        _ => std::io::ErrorKind::InvalidInput,
                    };
                    return failed_with_warn(baseline, current_stamp, || {
                        tracing::warn!(
                            file = label,
                            error_kind = ?kind,
                            "failed to read file during hot-reload; retaining previous state"
                        );
                    });
                }
            };

            if baseline.raw_bytes.as_deref() == Some(bytes.as_slice()) {
                return FileEvalOutcome::SameBytesNewStamp {
                    stamp: current_stamp,
                };
            }

            if baseline.last_failed_stamp.as_ref() == Some(&current_stamp)
                && baseline.last_failed_bytes.as_deref() == Some(bytes.as_slice())
            {
                return FileEvalOutcome::Unchanged {
                    touch_checked_at: true,
                };
            }

            match parse_bytes(&bytes) {
                Ok(value) => FileEvalOutcome::Updated {
                    value,
                    stamp: current_stamp,
                    raw_bytes: bytes,
                },
                Err(
                    CoreError::ParseConfig { line, column, .. }
                    | CoreError::ParseAuth { line, column, .. },
                ) => {
                    tracing::warn!(
                        file = label,
                        line,
                        column,
                        "failed to parse JSON during hot-reload; retaining previous state"
                    );
                    FileEvalOutcome::Failed {
                        failed_stamp: current_stamp,
                        failed_bytes: Some(bytes),
                    }
                }
                Err(_) => {
                    tracing::warn!(
                        file = label,
                        "validation failed during hot-reload; retaining previous state"
                    );
                    FileEvalOutcome::Failed {
                        failed_stamp: current_stamp,
                        failed_bytes: Some(bytes),
                    }
                }
            }
        }
    }
}

fn evaluate_reload(
    config_path: PathBuf,
    auth_dir: Option<PathBuf>,
    auth_store: Arc<dyn AuthStore>,
    codex_auth: Arc<dyn CodexAuth>,
    env_fallback: bool,
    baseline: ReloadBaseline,
) -> CombinedReloadOutcome {
    let cfg_stamp = inspect_file_stamp(&config_path);
    let mut config_outcome = evaluate_file(
        "config.json",
        MAX_CONFIG_FILE_BYTES,
        &baseline.config,
        cfg_stamp,
        |stamp| read_verified_bytes_for_stamp(&config_path, stamp, MAX_CONFIG_FILE_BYTES),
        Config::default,
        |bytes| Config::from_json_bytes(bytes, &config_path),
    );

    let mut auth_outcome = match (auth_dir.as_deref(), baseline.auth.as_ref()) {
        (Some(dir), Some(auth_baseline)) => {
            let auth_path = dir.join(GEMINI_AUTH_FILENAME);
            let auth_stamp = inspect_auth_file_stamp(dir, GEMINI_AUTH_FILENAME);
            Some(evaluate_file(
                "auth/gemini.json",
                MAX_AUTH_FILE_BYTES,
                auth_baseline,
                auth_stamp,
                |stamp| read_verified_auth_bytes_for_stamp(dir, GEMINI_AUTH_FILENAME, stamp),
                || None,
                |bytes| GeminiCredentials::from_json_bytes(bytes, &auth_path),
            ))
        }
        _ => None,
    };

    let mut codex_auth_outcome = match (auth_dir.as_deref(), baseline.codex_auth.as_ref()) {
        (Some(dir), Some(codex_baseline)) => {
            let codex_path = dir.join(CODEX_AUTH_FILENAME);
            let codex_stamp = inspect_auth_file_stamp(dir, CODEX_AUTH_FILENAME);
            Some(evaluate_file(
                "auth/codex.json",
                MAX_AUTH_FILE_BYTES,
                codex_baseline,
                codex_stamp,
                |stamp| read_verified_auth_bytes_for_stamp(dir, CODEX_AUTH_FILENAME, stamp),
                || None,
                |bytes| CodexCredentials::from_json_bytes(bytes, &codex_path).map(Some),
            ))
        }
        _ => None,
    };

    let cfg_changed = config_outcome.changed_value();
    let auth_changed = auth_outcome
        .as_ref()
        .is_some_and(FileEvalOutcome::changed_value);
    let codex_changed = codex_auth_outcome
        .as_ref()
        .is_some_and(FileEvalOutcome::changed_value);

    let new_active = if cfg_changed || auth_changed || codex_changed {
        let effective_config = config_outcome.effective_value(&baseline.config.value);
        let fallback_memory_creds;
        let effective_creds: &Option<GeminiCredentials> =
            match (auth_outcome.as_ref(), baseline.auth.as_ref()) {
                (Some(outcome), Some(auth_baseline)) => {
                    outcome.effective_value(&auth_baseline.value)
                }
                _ => {
                    fallback_memory_creds = auth_store.load_gemini().ok().flatten();
                    &fallback_memory_creds
                }
            };
        let fallback_memory_codex;
        let effective_codex_creds: &Option<CodexCredentials> =
            match (codex_auth_outcome.as_ref(), baseline.codex_auth.as_ref()) {
                (Some(outcome), Some(codex_baseline)) => {
                    outcome.effective_value(&codex_baseline.value)
                }
                _ => {
                    fallback_memory_codex = auth_store.load_codex().ok().flatten();
                    &fallback_memory_codex
                }
            };

        match build_active_provider(
            effective_config,
            effective_creds.as_ref().map(|c| &c.api_key),
            effective_codex_creds.is_some(),
            &codex_auth,
            env_fallback,
            Some(&baseline.active),
        ) {
            Ok(active) => Some(active),
            Err(_) => {
                tracing::warn!(
                    "provider reinitialization failed during hot-reload; retaining previous configuration"
                );
                if cfg_changed {
                    config_outcome = config_outcome.into_failed_after_rebuild_error();
                }
                if auth_changed {
                    auth_outcome =
                        auth_outcome.map(FileEvalOutcome::into_failed_after_rebuild_error);
                }
                if codex_changed {
                    codex_auth_outcome =
                        codex_auth_outcome.map(FileEvalOutcome::into_failed_after_rebuild_error);
                }
                None
            }
        }
    } else {
        None
    };

    CombinedReloadOutcome {
        config_outcome,
        auth_outcome,
        codex_auth_outcome,
        new_active,
    }
}
