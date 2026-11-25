use std::sync::Arc;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::time::{Duration, Instant};

pub(crate) struct ActivityTracker {
    in_flight: AtomicUsize,
    started_at: Instant,
    last_activity_ms: AtomicU64,
}

impl ActivityTracker {
    pub(crate) fn new() -> Self {
        Self {
            in_flight: AtomicUsize::new(0),
            started_at: Instant::now(),
            last_activity_ms: AtomicU64::new(0),
        }
    }

    fn elapsed_ms(&self) -> u64 {
        u64::try_from(self.started_at.elapsed().as_millis()).unwrap_or(u64::MAX)
    }

    fn touch(&self) {
        let now_ms = self.elapsed_ms();
        self.last_activity_ms.fetch_max(now_ms, Ordering::Release);
    }

    pub(crate) fn enter(self: &Arc<Self>) -> InFlightGuard {
        self.in_flight.fetch_add(1, Ordering::AcqRel);
        self.touch();
        InFlightGuard {
            tracker: Arc::clone(self),
        }
    }

    pub(crate) fn in_flight_requests(&self) -> usize {
        self.in_flight.load(Ordering::Acquire)
    }

    pub(crate) fn idle_duration(&self) -> Duration {
        if self.in_flight_requests() > 0 {
            return Duration::ZERO;
        }
        let now_ms = self.elapsed_ms();
        let last_ms = self.last_activity_ms.load(Ordering::Acquire);
        Duration::from_millis(now_ms.saturating_sub(last_ms))
    }
}

pub(crate) struct InFlightGuard {
    tracker: Arc<ActivityTracker>,
}

impl InFlightGuard {
    pub(crate) fn touch(&self) {
        self.tracker.touch();
    }
}

impl Drop for InFlightGuard {
    fn drop(&mut self) {
        self.tracker.touch();
        self.tracker.in_flight.fetch_sub(1, Ordering::AcqRel);
    }
}
