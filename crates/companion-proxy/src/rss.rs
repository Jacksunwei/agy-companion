use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

pub const DEFAULT_RSS_CACHE_TTL: Duration = Duration::from_secs(15);

#[derive(Debug, Clone, Copy)]
struct CachedRssSample {
    captured_at: Instant,
    bytes: Option<u64>,
}

#[derive(Debug)]
pub(crate) struct RssSampler {
    ttl: Duration,
    cache: Arc<Mutex<Option<CachedRssSample>>>,
    refresh_gate: tokio::sync::Mutex<()>,
}

impl RssSampler {
    pub(crate) fn new() -> Self {
        Self::with_ttl(DEFAULT_RSS_CACHE_TTL)
    }

    pub(crate) fn with_ttl(ttl: Duration) -> Self {
        Self {
            ttl,
            cache: Arc::new(Mutex::new(None)),
            refresh_gate: tokio::sync::Mutex::new(()),
        }
    }

    pub(crate) async fn sample_bytes(&self) -> Option<u64> {
        if let Some(entry) = *self.cache.lock().unwrap_or_else(|e| e.into_inner())
            && entry.captured_at.elapsed() < self.ttl
        {
            return entry.bytes;
        }

        // Coalesce concurrent cache misses so macOS never forks multiple `/bin/ps` processes.
        let _gate = self.refresh_gate.lock().await;
        if let Some(entry) = *self.cache.lock().unwrap_or_else(|e| e.into_inner())
            && entry.captured_at.elapsed() < self.ttl
        {
            return entry.bytes;
        }

        let measured = tokio::task::spawn_blocking(read_current_process_rss_bytes)
            .await
            .ok()
            .flatten();

        *self.cache.lock().unwrap_or_else(|e| e.into_inner()) = Some(CachedRssSample {
            captured_at: Instant::now(),
            bytes: measured,
        });
        measured
    }
}

#[allow(dead_code)]
pub(crate) fn parse_proc_status_vmrss_bytes(status: &str) -> Option<u64> {
    for line in status.lines() {
        if let Some(rest) = line.strip_prefix("VmRSS:") {
            let kib = rest.split_whitespace().next()?.parse::<u64>().ok()?;
            return kib.checked_mul(1024);
        }
    }
    None
}

#[allow(dead_code)]
pub(crate) fn parse_proc_statm_rss_bytes(statm: &str, page_size: u64) -> Option<u64> {
    let pages = statm.split_whitespace().nth(1)?.parse::<u64>().ok()?;
    pages.checked_mul(page_size)
}

#[allow(dead_code)]
pub(crate) fn parse_ps_rss_kib_bytes(stdout: &str) -> Option<u64> {
    let kib = stdout.trim().parse::<u64>().ok()?;
    kib.checked_mul(1024)
}

fn read_current_process_rss_bytes() -> Option<u64> {
    #[cfg(target_os = "linux")]
    {
        if let Ok(status) = std::fs::read_to_string("/proc/self/status")
            && let Some(bytes) = parse_proc_status_vmrss_bytes(&status)
        {
            return Some(bytes);
        }
        std::fs::read_to_string("/proc/self/statm")
            .ok()
            .and_then(|statm| parse_proc_statm_rss_bytes(&statm, 4096))
    }

    #[cfg(target_os = "macos")]
    {
        use std::process::{Command, Stdio};

        let output = Command::new("/bin/ps")
            .args(["-o", "rss=", "-p", &std::process::id().to_string()])
            .stdin(Stdio::null())
            .stderr(Stdio::null())
            .output()
            .ok()?;
        if !output.status.success() {
            return None;
        }
        std::str::from_utf8(&output.stdout)
            .ok()
            .and_then(parse_ps_rss_kib_bytes)
    }

    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
    {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_proc_status_vmrss_kib_and_rejects_malformed_or_overflow() {
        let sample =
            "Name:\tagy-companion\nVmSize:\t  123456 kB\nVmRSS:\t   18432 kB\nThreads:\t4\n";
        assert_eq!(parse_proc_status_vmrss_bytes(sample), Some(18_874_368));
        assert_eq!(parse_proc_status_vmrss_bytes("Name:\tfoo\n"), None);
        assert_eq!(parse_proc_status_vmrss_bytes("VmRSS:\tabc kB\n"), None);
        assert_eq!(
            parse_proc_status_vmrss_bytes(&format!("VmRSS:\t{} kB\n", u64::MAX)),
            None
        );
    }

    #[test]
    fn parses_proc_statm_and_ps_rss_safely() {
        assert_eq!(
            parse_proc_statm_rss_bytes("1000 512 200 1 0 300 0\n", 4096),
            Some(2_097_152)
        );
        assert_eq!(parse_proc_statm_rss_bytes("1000", 4096), None);
        assert_eq!(
            parse_proc_statm_rss_bytes(&format!("1000 {}", u64::MAX), 4096),
            None
        );

        assert_eq!(parse_ps_rss_kib_bytes("  2720\n"), Some(2_785_280));
        assert_eq!(parse_ps_rss_kib_bytes(""), None);
        assert_eq!(parse_ps_rss_kib_bytes("-5"), None);
        assert_eq!(parse_ps_rss_kib_bytes(&u64::MAX.to_string()), None);
    }

    #[tokio::test]
    async fn rss_sampler_caches_measurement_within_ttl() {
        let sampler = RssSampler::with_ttl(Duration::from_secs(60));
        let first = sampler.sample_bytes().await;
        #[cfg(any(target_os = "macos", target_os = "linux"))]
        assert!(
            first.is_some_and(|bytes| bytes > 0),
            "expected positive RSS on macOS/Linux, got {first:?}"
        );

        // Cached second call returns the exact same cached measurement immediately.
        let second = sampler.sample_bytes().await;
        assert_eq!(second, first);
    }
}
