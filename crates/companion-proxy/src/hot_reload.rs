use std::path::PathBuf;
use std::sync::RwLock;
use std::time::{Duration, Instant};

use companion_core::fs_safe::{inspect_file_stamp, read_verified_bytes_for_stamp};
use companion_core::{
    Config, CoreError, FileStamp, FsError, MAX_CONFIG_FILE_BYTES, RejectedReason, validate_path,
};

use crate::{ActiveProvider, ProxyError, build_active_provider};

const RACY_CLEAN_WINDOW: Duration = Duration::from_secs(1);

struct HotReloadInner {
    active: ActiveProvider,
    stamp: FileStamp,
    raw_bytes: Option<Vec<u8>>,
    checked_at: Instant,
    last_failed_stamp: Option<FileStamp>,
    last_failed_bytes: Option<Vec<u8>>,
}

pub(crate) struct HotReloadProvider {
    path: PathBuf,
    env_fallback: bool,
    inner: RwLock<HotReloadInner>,
    reload_lock: tokio::sync::Mutex<()>,
}

enum FastCheckDecision {
    ReturnCached(ActiveProvider),
    VerifySameStampBytes { match_failed_bytes: bool },
    NeedsReload,
}

struct ReloadBaseline {
    active: ActiveProvider,
    stamp: FileStamp,
    raw_bytes: Option<Vec<u8>>,
    last_failed_stamp: Option<FileStamp>,
    last_failed_bytes: Option<Vec<u8>>,
}

enum ReloadOutcome {
    Unchanged {
        touch_checked_at: bool,
    },
    Missing {
        new_active: Option<ActiveProvider>,
        already_missing: bool,
    },
    SameBytesNewStamp {
        stamp: FileStamp,
    },
    Updated {
        active: ActiveProvider,
        stamp: FileStamp,
        raw_bytes: Vec<u8>,
    },
    Failed {
        failed_stamp: FileStamp,
        failed_bytes: Option<Vec<u8>>,
    },
}

impl HotReloadProvider {
    pub(crate) fn new(
        path: PathBuf,
        active: ActiveProvider,
        env_fallback: bool,
    ) -> Result<Self, ProxyError> {
        validate_path(&path)?;
        let stamp = inspect_file_stamp(&path);
        if let FileStamp::Rejected(reason) = stamp {
            return Err(match reason {
                RejectedReason::Symlink => FsError::SymlinkRejected { path }.into(),
                RejectedReason::NotRegularFile => FsError::NotRegularFile { path }.into(),
            });
        }
        let raw_bytes = read_verified_bytes_for_stamp(&path, &stamp, MAX_CONFIG_FILE_BYTES)
            .ok()
            .flatten();
        Ok(Self {
            path,
            env_fallback,
            inner: RwLock::new(HotReloadInner {
                active,
                stamp,
                raw_bytes,
                checked_at: Instant::now(),
                last_failed_stamp: None,
                last_failed_bytes: None,
            }),
            reload_lock: tokio::sync::Mutex::new(()),
        })
    }

    pub(crate) async fn active_provider(&self) -> ActiveProvider {
        // Immediate off-thread stat on every request (zero debounce) so config changes
        // are observed instantaneously without blocking the Tokio runtime or holding locks.
        let stat_path = self.path.clone();
        let current_stamp =
            match tokio::task::spawn_blocking(move || inspect_file_stamp(&stat_path)).await {
                Ok(stamp) => stamp,
                Err(err) => {
                    tracing::warn!(
                        error = %err,
                        "hot-reload stat task failed; retaining previous configuration"
                    );
                    return self.current_active();
                }
            };

        match self.fast_check(&current_stamp) {
            FastCheckDecision::ReturnCached(active) => return active,
            FastCheckDecision::VerifySameStampBytes { match_failed_bytes } => {
                let verify_path = self.path.clone();
                let verify_stamp = current_stamp.clone();
                match tokio::task::spawn_blocking(move || {
                    read_verified_bytes_for_stamp(
                        &verify_path,
                        &verify_stamp,
                        MAX_CONFIG_FILE_BYTES,
                    )
                })
                .await
                {
                    Ok(Ok(Some(bytes))) => {
                        if let Some(active) =
                            self.cached_if_bytes_match(&current_stamp, &bytes, match_failed_bytes)
                        {
                            return active;
                        }
                    }
                    Ok(Ok(None)) | Ok(Err(_)) => {}
                    Err(err) => {
                        tracing::warn!(
                            error = %err,
                            "hot-reload byte-verification task failed; retaining previous configuration"
                        );
                        return self.current_active();
                    }
                }
            }
            FastCheckDecision::NeedsReload => {}
        }

        let _reload_guard = self.reload_lock.lock().await;
        let baseline = self.snapshot_baseline();
        let reload_path = self.path.clone();
        let env_fallback = self.env_fallback;
        let outcome = match tokio::task::spawn_blocking(move || {
            evaluate_reload(reload_path, env_fallback, baseline)
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

    fn fast_check(&self, current_stamp: &FileStamp) -> FastCheckDecision {
        let guard = self.inner.read().unwrap_or_else(|e| e.into_inner());
        let outside_racy_window = guard.checked_at.elapsed() >= RACY_CLEAN_WINDOW;
        if *current_stamp == guard.stamp {
            if outside_racy_window
                || matches!(
                    current_stamp,
                    FileStamp::Missing | FileStamp::Rejected(_) | FileStamp::Unreadable(_)
                )
            {
                return FastCheckDecision::ReturnCached(guard.active.clone());
            }
            return FastCheckDecision::VerifySameStampBytes {
                match_failed_bytes: false,
            };
        }

        if guard.last_failed_stamp.as_ref() == Some(current_stamp) {
            if outside_racy_window
                || matches!(
                    current_stamp,
                    FileStamp::Rejected(_) | FileStamp::Unreadable(_)
                )
            {
                return FastCheckDecision::ReturnCached(guard.active.clone());
            }
            return FastCheckDecision::VerifySameStampBytes {
                match_failed_bytes: true,
            };
        }

        FastCheckDecision::NeedsReload
    }

    fn cached_if_bytes_match(
        &self,
        current_stamp: &FileStamp,
        bytes: &[u8],
        match_failed_bytes: bool,
    ) -> Option<ActiveProvider> {
        let guard = self.inner.read().unwrap_or_else(|e| e.into_inner());
        let matched = if match_failed_bytes {
            guard.last_failed_stamp.as_ref() == Some(current_stamp)
                && guard.last_failed_bytes.as_deref() == Some(bytes)
        } else {
            guard.stamp == *current_stamp && guard.raw_bytes.as_deref() == Some(bytes)
        };
        matched.then(|| guard.active.clone())
    }

    fn snapshot_baseline(&self) -> ReloadBaseline {
        let guard = self.inner.read().unwrap_or_else(|e| e.into_inner());
        ReloadBaseline {
            active: guard.active.clone(),
            stamp: guard.stamp.clone(),
            raw_bytes: guard.raw_bytes.clone(),
            last_failed_stamp: guard.last_failed_stamp.clone(),
            last_failed_bytes: guard.last_failed_bytes.clone(),
        }
    }

    fn apply_outcome(&self, outcome: ReloadOutcome) -> ActiveProvider {
        let mut guard = self.inner.write().unwrap_or_else(|e| e.into_inner());
        match outcome {
            ReloadOutcome::Unchanged { touch_checked_at } => {
                if touch_checked_at {
                    guard.checked_at = Instant::now();
                }
            }
            ReloadOutcome::Missing {
                new_active,
                already_missing,
            } => {
                if already_missing {
                    guard.last_failed_stamp = None;
                    guard.last_failed_bytes = None;
                    guard.checked_at = Instant::now();
                } else if let Some(default_active) = new_active {
                    guard.active = default_active;
                    guard.stamp = FileStamp::Missing;
                    guard.raw_bytes = None;
                    guard.last_failed_stamp = None;
                    guard.last_failed_bytes = None;
                    guard.checked_at = Instant::now();
                }
            }
            ReloadOutcome::SameBytesNewStamp { stamp } => {
                guard.stamp = stamp;
                guard.last_failed_stamp = None;
                guard.last_failed_bytes = None;
                guard.checked_at = Instant::now();
            }
            ReloadOutcome::Updated {
                active,
                stamp,
                raw_bytes,
            } => {
                guard.active = active;
                guard.stamp = stamp;
                guard.raw_bytes = Some(raw_bytes);
                guard.last_failed_stamp = None;
                guard.last_failed_bytes = None;
                guard.checked_at = Instant::now();
            }
            ReloadOutcome::Failed {
                failed_stamp,
                failed_bytes,
            } => {
                guard.last_failed_stamp = Some(failed_stamp);
                guard.last_failed_bytes = failed_bytes;
                guard.checked_at = Instant::now();
            }
        }
        guard.active.clone()
    }
}

fn evaluate_missing(env_fallback: bool, baseline: &ReloadBaseline) -> ReloadOutcome {
    if baseline.stamp == FileStamp::Missing {
        return ReloadOutcome::Missing {
            new_active: None,
            already_missing: true,
        };
    }
    let new_active =
        build_active_provider(&Config::default(), env_fallback, Some(&baseline.active)).ok();
    ReloadOutcome::Missing {
        new_active,
        already_missing: false,
    }
}

fn failed_with_warn(
    baseline: &ReloadBaseline,
    failed_stamp: FileStamp,
    warn: impl FnOnce(),
) -> ReloadOutcome {
    if baseline.last_failed_stamp.as_ref() != Some(&failed_stamp) {
        warn();
    }
    ReloadOutcome::Failed {
        failed_stamp,
        failed_bytes: None,
    }
}

fn failed_rejected(baseline: &ReloadBaseline, reason: RejectedReason) -> ReloadOutcome {
    failed_with_warn(baseline, FileStamp::Rejected(reason), || {
        tracing::warn!(
            reason = ?reason,
            "ignoring rejected config path during hot-reload; retaining previous configuration"
        );
    })
}

fn evaluate_reload(path: PathBuf, env_fallback: bool, baseline: ReloadBaseline) -> ReloadOutcome {
    let current_stamp = inspect_file_stamp(&path);
    match current_stamp {
        FileStamp::Missing => evaluate_missing(env_fallback, &baseline),
        FileStamp::Rejected(reason) => failed_rejected(&baseline, reason),
        FileStamp::Unreadable(kind) => failed_with_warn(&baseline, current_stamp, || {
            tracing::warn!(
                error_kind = ?kind,
                "failed to stat config file during hot-reload; retaining previous configuration"
            );
        }),
        FileStamp::Present { .. } => {
            let bytes = match read_verified_bytes_for_stamp(
                &path,
                &current_stamp,
                MAX_CONFIG_FILE_BYTES,
            ) {
                Ok(Some(bytes)) => bytes,
                Ok(None) => return evaluate_missing(env_fallback, &baseline),
                Err(FsError::SizeLimitExceeded { .. }) => {
                    return failed_with_warn(&baseline, current_stamp, || {
                        tracing::warn!(
                            max_bytes = MAX_CONFIG_FILE_BYTES,
                            "ignoring oversized config file during hot-reload; retaining previous configuration"
                        );
                    });
                }
                Err(FsError::SymlinkRejected { .. }) => {
                    return failed_rejected(&baseline, RejectedReason::Symlink);
                }
                Err(FsError::NotRegularFile { .. }) => {
                    return failed_rejected(&baseline, RejectedReason::NotRegularFile);
                }
                Err(FsError::ConcurrentModification { .. }) => {
                    tracing::warn!(
                        "config file changed concurrently during hot-reload read; retaining previous configuration"
                    );
                    return ReloadOutcome::Unchanged {
                        touch_checked_at: true,
                    };
                }
                Err(err) => {
                    let kind = match &err {
                        FsError::Io { source, .. } => source.kind(),
                        _ => std::io::ErrorKind::InvalidInput,
                    };
                    return failed_with_warn(&baseline, current_stamp, || {
                        tracing::warn!(
                            error_kind = ?kind,
                            "failed to read config file during hot-reload; retaining previous configuration"
                        );
                    });
                }
            };

            if baseline.raw_bytes.as_deref() == Some(bytes.as_slice()) {
                return ReloadOutcome::SameBytesNewStamp {
                    stamp: current_stamp,
                };
            }

            if baseline.last_failed_stamp.as_ref() == Some(&current_stamp)
                && baseline.last_failed_bytes.as_deref() == Some(bytes.as_slice())
            {
                return ReloadOutcome::Unchanged {
                    touch_checked_at: true,
                };
            }

            let parsed = match Config::from_json_bytes(&bytes, &path) {
                Ok(cfg) => cfg,
                Err(CoreError::ParseConfig { line, column, .. }) => {
                    tracing::warn!(
                        line,
                        column,
                        "failed to parse config JSON during hot-reload; retaining previous configuration"
                    );
                    return ReloadOutcome::Failed {
                        failed_stamp: current_stamp,
                        failed_bytes: Some(bytes),
                    };
                }
                Err(_) => {
                    tracing::warn!(
                        "config validation failed during hot-reload; retaining previous configuration"
                    );
                    return ReloadOutcome::Failed {
                        failed_stamp: current_stamp,
                        failed_bytes: Some(bytes),
                    };
                }
            };

            match build_active_provider(&parsed, env_fallback, Some(&baseline.active)) {
                Ok(active) => ReloadOutcome::Updated {
                    active,
                    stamp: current_stamp,
                    raw_bytes: bytes,
                },
                Err(_) => {
                    tracing::warn!(
                        "provider reinitialization failed during hot-reload; retaining previous configuration"
                    );
                    ReloadOutcome::Failed {
                        failed_stamp: current_stamp,
                        failed_bytes: Some(bytes),
                    }
                }
            }
        }
    }
}
