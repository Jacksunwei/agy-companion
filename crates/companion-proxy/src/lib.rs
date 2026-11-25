#![forbid(unsafe_code)]

pub mod cloudcode;
pub mod routes;
pub mod sse;
pub mod stats;

use std::future::Future;
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, RwLock};
use std::time::{Duration, Instant};

use companion_core::fs_safe::{inspect_file_stamp, read_verified_bytes_for_stamp};
use companion_core::{
    CloudCodeUpstream, Config, CoreError, DEFAULT_CLOUDCODE_UPSTREAM_URL, DEFAULT_GEMINI_MODEL,
    DEFAULT_TIMEOUT_SECS, FileStamp, FsError, MAX_CONFIG_FILE_BYTES, RejectedReason, UsageSink,
    validate_path,
};
use companion_providers::{GeminiProvider, ProviderAdapter, ProviderError};
use thiserror::Error;
use tokio::net::TcpListener;

pub use cloudcode::{
    CloudCodeClient, CloudCodeError, MAX_CLOUDCODE_CATALOG_BYTES, is_cloudcode_path,
    merge_available_models, validate_cloudcode_path,
};
pub use companion_core::protocol::{HealthResponse, PROTOCOL_VERSION, StatsSnapshot};
pub use routes::{DEFAULT_MAX_JSON_TEE_BYTES, MAX_PROXY_BODY_BYTES, build_router};
pub use sse::{DEFAULT_MAX_SSE_EVENT_BYTES, SseUsageTracker};
pub use stats::ProxyStats;

const RACY_CLEAN_WINDOW: Duration = Duration::from_secs(1);

#[derive(Debug, Error)]
pub enum ProxyError {
    #[error("configuration error: {0}")]
    Core(#[from] CoreError),
    #[error("provider error: {0}")]
    Provider(#[from] ProviderError),
    #[error("cloudcode proxy error: {0}")]
    CloudCode(#[from] CloudCodeError),
    #[error("failed to bind loopback listener on {addr}: {source}")]
    Bind {
        addr: SocketAddr,
        #[source]
        source: std::io::Error,
    },
    #[error("proxy server error: {0}")]
    Serve(#[source] std::io::Error),
}

impl From<FsError> for ProxyError {
    fn from(err: FsError) -> Self {
        Self::Core(CoreError::Fs(err))
    }
}

#[derive(Clone)]
pub(crate) struct ActiveProvider {
    pub(crate) provider: Arc<dyn ProviderAdapter>,
    pub(crate) cloudcode: Arc<CloudCodeClient>,
    pub(crate) cloudcode_upstream: CloudCodeUpstream,
    pub(crate) cloudcode_timeout_secs: u64,
    pub(crate) has_configured_api_key: bool,
    pub(crate) default_model: String,
}

struct HotReloadInner {
    active: ActiveProvider,
    stamp: FileStamp,
    raw_bytes: Option<Vec<u8>>,
    checked_at: Instant,
    last_failed_stamp: Option<FileStamp>,
    last_failed_bytes: Option<Vec<u8>>,
}

struct HotReloadProvider {
    path: PathBuf,
    env_fallback: bool,
    inner: RwLock<HotReloadInner>,
}

enum ProviderSource {
    Static(ActiveProvider),
    HotReload(Box<HotReloadProvider>),
}

struct ActivityTracker {
    in_flight: AtomicUsize,
    started_at: Instant,
    last_activity_ms: AtomicU64,
}

impl ActivityTracker {
    fn new() -> Self {
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

    fn enter(self: &Arc<Self>) -> InFlightGuard {
        self.in_flight.fetch_add(1, Ordering::AcqRel);
        self.touch();
        InFlightGuard {
            tracker: Arc::clone(self),
        }
    }

    fn in_flight_requests(&self) -> usize {
        self.in_flight.load(Ordering::Acquire)
    }

    fn idle_duration(&self) -> Duration {
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

struct ProxyStateInner {
    source: ProviderSource,
    stats: Arc<ProxyStats>,
    usage_sink: Arc<dyn UsageSink>,
    activity: Arc<ActivityTracker>,
}

#[derive(Clone)]
pub struct ProxyState {
    inner: Arc<ProxyStateInner>,
}

impl ProxyState {
    pub fn new(
        provider: Arc<dyn ProviderAdapter>,
        usage_sink: Arc<dyn UsageSink>,
        has_configured_api_key: bool,
    ) -> Self {
        let cloudcode = Arc::new(
            CloudCodeClient::new(DEFAULT_CLOUDCODE_UPSTREAM_URL, DEFAULT_TIMEOUT_SECS)
                .expect("default cloudcode upstream URL must be valid"),
        );
        Self {
            inner: Arc::new(ProxyStateInner {
                source: ProviderSource::Static(ActiveProvider {
                    provider,
                    cloudcode,
                    cloudcode_upstream: CloudCodeUpstream::Auto,
                    cloudcode_timeout_secs: DEFAULT_TIMEOUT_SECS,
                    has_configured_api_key,
                    default_model: DEFAULT_GEMINI_MODEL.to_string(),
                }),
                stats: Arc::new(ProxyStats::new()),
                usage_sink,
                activity: Arc::new(ActivityTracker::new()),
            }),
        }
    }

    pub fn from_config(
        config: Config,
        config_path: Option<PathBuf>,
        usage_sink: Arc<dyn UsageSink>,
    ) -> Result<Self, ProxyError> {
        Self::from_config_with_env_fallback(config, config_path, usage_sink, true)
    }

    pub fn from_config_with_env_fallback(
        config: Config,
        config_path: Option<PathBuf>,
        usage_sink: Arc<dyn UsageSink>,
        env_fallback: bool,
    ) -> Result<Self, ProxyError> {
        config.validate()?;
        let active = build_active_provider(&config, env_fallback, None)?;

        let source = match config_path {
            Some(path) => {
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
                ProviderSource::HotReload(Box::new(HotReloadProvider {
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
                }))
            }
            None => ProviderSource::Static(active),
        };

        Ok(Self {
            inner: Arc::new(ProxyStateInner {
                source,
                stats: Arc::new(ProxyStats::new()),
                usage_sink,
                activity: Arc::new(ActivityTracker::new()),
            }),
        })
    }

    pub fn stats(&self) -> &Arc<ProxyStats> {
        &self.inner.stats
    }

    pub fn usage_sink(&self) -> &Arc<dyn UsageSink> {
        &self.inner.usage_sink
    }

    pub fn in_flight_requests(&self) -> usize {
        self.inner.activity.in_flight_requests()
    }

    pub fn idle_duration(&self) -> Duration {
        self.inner.activity.idle_duration()
    }

    pub(crate) fn enter_request(&self) -> InFlightGuard {
        self.inner.activity.enter()
    }

    pub(crate) fn active_provider(&self) -> ActiveProvider {
        match &self.inner.source {
            ProviderSource::Static(active) => active.clone(),
            ProviderSource::HotReload(hot_reload) => hot_reload.active_provider(),
        }
    }
}

fn build_active_provider(
    config: &Config,
    env_fallback: bool,
    previous_active: Option<&ActiveProvider>,
) -> Result<ActiveProvider, ProxyError> {
    let provider =
        GeminiProvider::from_config(&config.providers.gemini)?.with_env_fallback(env_fallback);
    let cloudcode = match previous_active {
        Some(prev)
            if prev.cloudcode_upstream == config.cloudcode.upstream
                && prev.cloudcode_timeout_secs == config.providers.gemini.timeout_secs =>
        {
            Arc::clone(&prev.cloudcode)
        }
        _ => Arc::new(CloudCodeClient::new(
            config.cloudcode.upstream.as_str(),
            config.providers.gemini.timeout_secs,
        )?),
    };
    let has_configured_api_key = provider.api_key_hint().is_some();
    Ok(ActiveProvider {
        provider: Arc::new(provider),
        cloudcode,
        cloudcode_upstream: config.cloudcode.upstream.clone(),
        cloudcode_timeout_secs: config.providers.gemini.timeout_secs,
        has_configured_api_key,
        default_model: config.providers.gemini.default_model.clone(),
    })
}

impl HotReloadProvider {
    fn active_provider(&self) -> ActiveProvider {
        let current_stamp = inspect_file_stamp(&self.path);

        {
            let guard = self.inner.read().unwrap_or_else(|e| e.into_inner());
            let outside_racy_window = guard.checked_at.elapsed() >= RACY_CLEAN_WINDOW;
            if current_stamp == guard.stamp {
                if outside_racy_window
                    || matches!(
                        current_stamp,
                        FileStamp::Missing | FileStamp::Rejected(_) | FileStamp::Unreadable(_)
                    )
                {
                    return guard.active.clone();
                }
                if let Some(bytes) =
                    read_verified_bytes_for_stamp(&self.path, &current_stamp, MAX_CONFIG_FILE_BYTES)
                        .ok()
                        .flatten()
                    && guard.raw_bytes.as_deref() == Some(bytes.as_slice())
                {
                    return guard.active.clone();
                }
            } else if guard.last_failed_stamp.as_ref() == Some(&current_stamp) {
                if outside_racy_window
                    || matches!(
                        current_stamp,
                        FileStamp::Rejected(_) | FileStamp::Unreadable(_)
                    )
                {
                    return guard.active.clone();
                }
                if let Some(bytes) =
                    read_verified_bytes_for_stamp(&self.path, &current_stamp, MAX_CONFIG_FILE_BYTES)
                        .ok()
                        .flatten()
                    && guard.last_failed_bytes.as_deref() == Some(bytes.as_slice())
                {
                    return guard.active.clone();
                }
            }
        }

        let mut guard = self.inner.write().unwrap_or_else(|e| e.into_inner());
        let current_stamp = inspect_file_stamp(&self.path);
        match current_stamp {
            FileStamp::Missing => {
                self.apply_missing_config(&mut guard);
                guard.active.clone()
            }
            FileStamp::Rejected(reason) => {
                if guard.last_failed_stamp.as_ref() != Some(&current_stamp) {
                    tracing::warn!(
                        reason = ?reason,
                        "ignoring rejected config path during hot-reload; retaining previous configuration"
                    );
                }
                guard.last_failed_stamp = Some(current_stamp);
                guard.last_failed_bytes = None;
                guard.checked_at = Instant::now();
                guard.active.clone()
            }
            FileStamp::Unreadable(kind) => {
                if guard.last_failed_stamp.as_ref() != Some(&current_stamp) {
                    tracing::warn!(
                        error_kind = ?kind,
                        "failed to stat config file during hot-reload; retaining previous configuration"
                    );
                }
                guard.last_failed_stamp = Some(current_stamp);
                guard.last_failed_bytes = None;
                guard.checked_at = Instant::now();
                guard.active.clone()
            }
            FileStamp::Present { .. } => {
                let bytes = match read_verified_bytes_for_stamp(
                    &self.path,
                    &current_stamp,
                    MAX_CONFIG_FILE_BYTES,
                ) {
                    Ok(Some(bytes)) => bytes,
                    Ok(None) => {
                        self.apply_missing_config(&mut guard);
                        return guard.active.clone();
                    }
                    Err(FsError::SizeLimitExceeded { .. }) => {
                        if guard.last_failed_stamp.as_ref() != Some(&current_stamp) {
                            tracing::warn!(
                                max_bytes = MAX_CONFIG_FILE_BYTES,
                                "ignoring oversized config file during hot-reload; retaining previous configuration"
                            );
                        }
                        guard.last_failed_stamp = Some(current_stamp);
                        guard.last_failed_bytes = None;
                        guard.checked_at = Instant::now();
                        return guard.active.clone();
                    }
                    Err(FsError::SymlinkRejected { .. }) => {
                        let rejected_stamp = FileStamp::Rejected(RejectedReason::Symlink);
                        if guard.last_failed_stamp.as_ref() != Some(&rejected_stamp) {
                            tracing::warn!(
                                reason = ?RejectedReason::Symlink,
                                "ignoring rejected config path during hot-reload; retaining previous configuration"
                            );
                        }
                        guard.last_failed_stamp = Some(rejected_stamp);
                        guard.last_failed_bytes = None;
                        guard.checked_at = Instant::now();
                        return guard.active.clone();
                    }
                    Err(FsError::NotRegularFile { .. }) => {
                        let rejected_stamp = FileStamp::Rejected(RejectedReason::NotRegularFile);
                        if guard.last_failed_stamp.as_ref() != Some(&rejected_stamp) {
                            tracing::warn!(
                                reason = ?RejectedReason::NotRegularFile,
                                "ignoring rejected config path during hot-reload; retaining previous configuration"
                            );
                        }
                        guard.last_failed_stamp = Some(rejected_stamp);
                        guard.last_failed_bytes = None;
                        guard.checked_at = Instant::now();
                        return guard.active.clone();
                    }
                    Err(FsError::ConcurrentModification { .. }) => {
                        tracing::warn!(
                            "config file changed concurrently during hot-reload read; retaining previous configuration"
                        );
                        guard.checked_at = Instant::now();
                        return guard.active.clone();
                    }
                    Err(err) => {
                        let kind = match &err {
                            FsError::Io { source, .. } => source.kind(),
                            _ => std::io::ErrorKind::InvalidInput,
                        };
                        if guard.last_failed_stamp.as_ref() != Some(&current_stamp) {
                            tracing::warn!(
                                error_kind = ?kind,
                                "failed to read config file during hot-reload; retaining previous configuration"
                            );
                        }
                        guard.last_failed_stamp = Some(current_stamp);
                        guard.last_failed_bytes = None;
                        guard.checked_at = Instant::now();
                        return guard.active.clone();
                    }
                };

                if guard.raw_bytes.as_deref() == Some(bytes.as_slice()) {
                    guard.stamp = current_stamp;
                    guard.last_failed_stamp = None;
                    guard.last_failed_bytes = None;
                    guard.checked_at = Instant::now();
                    return guard.active.clone();
                }

                if guard.last_failed_stamp.as_ref() == Some(&current_stamp)
                    && guard.last_failed_bytes.as_deref() == Some(bytes.as_slice())
                {
                    guard.checked_at = Instant::now();
                    return guard.active.clone();
                }

                let parsed = match Config::from_json_bytes(&bytes, &self.path) {
                    Ok(cfg) => cfg,
                    Err(CoreError::ParseConfig { line, column, .. }) => {
                        tracing::warn!(
                            line,
                            column,
                            "failed to parse config JSON during hot-reload; retaining previous configuration"
                        );
                        guard.last_failed_stamp = Some(current_stamp);
                        guard.last_failed_bytes = Some(bytes);
                        guard.checked_at = Instant::now();
                        return guard.active.clone();
                    }
                    Err(_) => {
                        tracing::warn!(
                            "config validation failed during hot-reload; retaining previous configuration"
                        );
                        guard.last_failed_stamp = Some(current_stamp);
                        guard.last_failed_bytes = Some(bytes);
                        guard.checked_at = Instant::now();
                        return guard.active.clone();
                    }
                };

                let new_active = match build_active_provider(
                    &parsed,
                    self.env_fallback,
                    Some(&guard.active),
                ) {
                    Ok(active) => active,
                    Err(_) => {
                        tracing::warn!(
                            "provider reinitialization failed during hot-reload; retaining previous configuration"
                        );
                        guard.last_failed_stamp = Some(current_stamp);
                        guard.last_failed_bytes = Some(bytes);
                        guard.checked_at = Instant::now();
                        return guard.active.clone();
                    }
                };

                guard.active = new_active;
                guard.stamp = current_stamp;
                guard.raw_bytes = Some(bytes);
                guard.last_failed_stamp = None;
                guard.last_failed_bytes = None;
                guard.checked_at = Instant::now();
                guard.active.clone()
            }
        }
    }

    fn apply_missing_config(&self, guard: &mut HotReloadInner) {
        if guard.stamp == FileStamp::Missing {
            guard.last_failed_stamp = None;
            guard.last_failed_bytes = None;
            guard.checked_at = Instant::now();
            return;
        }
        if let Ok(default_active) =
            build_active_provider(&Config::default(), self.env_fallback, Some(&guard.active))
        {
            guard.active = default_active;
            guard.stamp = FileStamp::Missing;
            guard.raw_bytes = None;
            guard.last_failed_stamp = None;
            guard.last_failed_bytes = None;
            guard.checked_at = Instant::now();
        }
    }
}

pub fn loopback_socket_addr(config: &Config) -> Result<SocketAddr, ProxyError> {
    config.validate()?;
    Ok(SocketAddr::from((
        Ipv4Addr::LOCALHOST,
        config.proxy.port.get(),
    )))
}

pub async fn bind_loopback_listener(config: &Config) -> Result<TcpListener, ProxyError> {
    let addr = loopback_socket_addr(config)?;
    TcpListener::bind(addr)
        .await
        .map_err(|source| ProxyError::Bind { addr, source })
}

pub async fn serve_on_listener<F>(
    listener: TcpListener,
    state: ProxyState,
    shutdown: F,
) -> Result<(), ProxyError>
where
    F: Future<Output = ()> + Send + 'static,
{
    let local_addr = listener.local_addr().map_err(ProxyError::Serve)?;
    if local_addr.ip() != IpAddr::V4(Ipv4Addr::LOCALHOST) {
        return Err(ProxyError::Core(CoreError::InvalidListenHost {
            host: local_addr.ip().to_string(),
        }));
    }

    let router = build_router(state);
    axum::serve(listener, router)
        .with_graceful_shutdown(shutdown)
        .await
        .map_err(ProxyError::Serve)
}

pub async fn serve_with_shutdown<F>(
    config: Config,
    config_path: Option<PathBuf>,
    usage_sink: Arc<dyn UsageSink>,
    shutdown: F,
) -> Result<(), ProxyError>
where
    F: Future<Output = ()> + Send + 'static,
{
    let listener = bind_loopback_listener(&config).await?;
    let state = ProxyState::from_config(config, config_path, usage_sink)?;
    serve_on_listener(listener, state, shutdown).await
}

pub async fn serve(
    config: Config,
    config_path: Option<PathBuf>,
    usage_sink: Arc<dyn UsageSink>,
) -> Result<(), ProxyError> {
    serve_with_shutdown(config, config_path, usage_sink, shutdown_signal()).await
}

async fn shutdown_signal() {
    let ctrl_c = async {
        let _ = tokio::signal::ctrl_c().await;
    };

    #[cfg(unix)]
    let terminate = async {
        if let Ok(mut sigterm) =
            tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
        {
            sigterm.recv().await;
        } else {
            std::future::pending::<()>().await;
        }
    };

    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();

    tokio::select! {
        _ = ctrl_c => {}
        _ = terminate => {}
    }
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::num::NonZeroU16;
    use std::path::Path;
    use std::sync::Mutex;

    use axum::Router;
    use axum::body::Body;
    use axum::response::Response;
    use axum::routing::post;
    use companion_core::{ApiKey, LoopbackHost, NoopUsageSink, SqliteUsageSink, USAGE_DB_FILENAME};
    use http::{HeaderMap, StatusCode, header};
    use tempfile::TempDir;
    use tokio::sync::oneshot;

    use super::*;

    fn test_http_client() -> reqwest::Client {
        reqwest::Client::builder()
            .no_proxy()
            .build()
            .expect("build loopback client")
    }

    async fn spawn_mock_upstream(
        tag: &'static str,
    ) -> (
        SocketAddr,
        Arc<Mutex<Vec<Option<String>>>>,
        tokio::task::JoinHandle<()>,
    ) {
        let keys = Arc::new(Mutex::new(Vec::new()));
        let keys_clone = Arc::clone(&keys);

        let app = Router::new().route(
            "/v1beta/models/{*rest}",
            post(move |headers: HeaderMap| {
                let keys_clone = Arc::clone(&keys_clone);
                async move {
                    let key = headers
                        .get("x-goog-api-key")
                        .and_then(|v| v.to_str().ok())
                        .map(str::to_string);
                    keys_clone.lock().unwrap().push(key);
                    let body = format!(
                        "data: {{\"candidates\":[{{\"text\":\"{tag}\"}}],\"usageMetadata\":{{\"promptTokenCount\":40,\"candidatesTokenCount\":10,\"cachedContentTokenCount\":20}}}}\n\n"
                    );
                    Response::builder()
                        .status(StatusCode::OK)
                        .header(header::CONTENT_TYPE, "text/event-stream")
                        .body(Body::from(body))
                        .unwrap()
                }
            }),
        );

        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let handle = tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        (addr, keys, handle)
    }

    #[tokio::test]
    async fn rejects_non_loopback_hosts_ports_and_listeners() {
        let path = Path::new("config.json");
        assert!(matches!(
            Config::from_json_str(r#"{"schema_version":1,"proxy":{"host":"0.0.0.0"}}"#, path),
            Err(CoreError::InvalidListenHost { .. })
        ));
        assert!(matches!(
            Config::from_json_str(r#"{"schema_version":1,"proxy":{"host":"::"}}"#, path),
            Err(CoreError::InvalidListenHost { .. })
        ));
        assert!(matches!(
            Config::from_json_str(r#"{"schema_version":1,"proxy":{"port":0}}"#, path),
            Err(CoreError::InvalidListenPort)
        ));

        // "localhost" must still bind strictly to 127.0.0.1:<port>.
        let mut cfg_localhost = Config::default();
        cfg_localhost.proxy.host = LoopbackHost::Localhost;
        cfg_localhost.proxy.port = NonZeroU16::new(4141).unwrap();
        assert_eq!(
            loopback_socket_addr(&cfg_localhost).unwrap(),
            SocketAddr::from((Ipv4Addr::LOCALHOST, 4141))
        );

        // Defense-in-depth: serve_on_listener rejects a listener bound to 0.0.0.0.
        let non_loopback_listener = TcpListener::bind("0.0.0.0:0").await.unwrap();
        let state = ProxyState::from_config_with_env_fallback(
            Config::default(),
            None,
            Arc::new(NoopUsageSink),
            false,
        )
        .unwrap();
        let err = serve_on_listener(non_loopback_listener, state, async {})
            .await
            .unwrap_err();
        assert!(matches!(
            err,
            ProxyError::Core(CoreError::InvalidListenHost { .. })
        ));
    }

    #[tokio::test]
    async fn serve_on_listener_end_to_end_over_real_tcp_with_graceful_shutdown() {
        let (upstream_addr, upstream_keys, upstream_handle) = spawn_mock_upstream("live-e2e").await;

        let tmp = TempDir::new().unwrap();
        let usage_dir = tmp.path().join("usage");
        let sink = Arc::new(SqliteUsageSink::new(&usage_dir).unwrap());
        let sink_reader = Arc::clone(&sink);

        let mut cfg = Config::default();
        cfg.providers.gemini.base_url = format!("http://{upstream_addr}");
        cfg.providers.gemini.api_key =
            Some(ApiKey::parse_strict("AIzaLiveLoopbackKey12345").unwrap());

        let state = ProxyState::from_config_with_env_fallback(cfg, None, sink, false).unwrap();
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let proxy_addr = listener.local_addr().unwrap();

        let (tx_shutdown, rx_shutdown) = oneshot::channel::<()>();
        let server = tokio::spawn(async move {
            serve_on_listener(listener, state, async move {
                let _ = rx_shutdown.await;
            })
            .await
            .unwrap();
        });

        let client = test_http_client();

        // 1. GET /health
        let health_resp = client
            .get(format!("http://{proxy_addr}/health"))
            .send()
            .await
            .unwrap();
        assert_eq!(health_resp.status(), StatusCode::OK);
        assert_eq!(
            health_resp
                .headers()
                .get("x-content-type-options")
                .and_then(|v| v.to_str().ok()),
            Some("nosniff")
        );
        assert_eq!(
            health_resp
                .headers()
                .get("cache-control")
                .and_then(|v| v.to_str().ok()),
            Some("no-store")
        );
        let health: HealthResponse = health_resp.json().await.unwrap();
        assert_eq!(health.status, "ok");
        assert_eq!(health.protocol_version, PROTOCOL_VERSION);

        // 2. POST SSE pass-through
        let proxy_resp = client
            .post(format!(
                "http://{proxy_addr}/v1beta/models/gemini-4-hydrogen:streamGenerateContent?alt=sse"
            ))
            .header("content-type", "application/json")
            .body(r#"{"contents":[]}"#)
            .send()
            .await
            .unwrap();
        assert_eq!(proxy_resp.status(), StatusCode::OK);
        let sse_text = proxy_resp.text().await.unwrap();
        assert!(sse_text.contains("live-e2e"));
        assert_eq!(
            upstream_keys.lock().unwrap().as_slice(),
            &[Some("AIzaLiveLoopbackKey12345".to_string())]
        );

        // 3. GET /api/stats reflects recorded usage immediately.
        let snap: StatsSnapshot = client
            .get(format!("http://{proxy_addr}/api/stats"))
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        assert_eq!(snap.usage.requests, 1);
        assert_eq!(snap.usage.prompt_tokens, 40);
        assert_eq!(snap.usage.cached_tokens, 20);
        assert_eq!(snap.usage.output_tokens, 10);
        assert!((snap.usage.cache_hit_ratio - 0.5).abs() < 1e-12);

        // 4. SqliteUsageSink persisted 1 event with zero secrets and 0600 permissions.
        let db_path = usage_dir.join(USAGE_DB_FILENAME);
        assert!(
            db_path.is_file(),
            "expected usage.db at {}",
            db_path.display()
        );

        let mut raw_bytes = Vec::new();
        for name in [
            "usage.db",
            "usage.db-wal",
            "usage.db-shm",
            "usage.db-journal",
        ] {
            let path = usage_dir.join(name);
            if path.is_file() {
                raw_bytes.extend_from_slice(&fs::read(&path).unwrap());
            }
        }
        let secret = b"AIzaLiveLoopbackKey12345";
        assert!(
            !raw_bytes.windows(secret.len()).any(|w| w == secret),
            "API key must never appear in SQLite storage bytes"
        );

        let events = sink_reader.read_all_events().unwrap();
        assert_eq!(events.len(), 1);
        let event = &events[0];
        assert_eq!(event.model, "gemini-4-hydrogen");
        assert_eq!(event.input_tokens, 40);
        assert_eq!(event.cached_tokens, 20);

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            for name in ["usage.db", "usage.db-wal", "usage.db-shm"] {
                let path = usage_dir.join(name);
                if path.exists() {
                    let mode = fs::metadata(&path).unwrap().permissions().mode() & 0o777;
                    assert_eq!(mode, 0o600, "expected 0600 on {name}, got {mode:o}");
                }
            }
        }

        let _ = tx_shutdown.send(());
        server.await.unwrap();
        upstream_handle.abort();
    }

    #[tokio::test]
    async fn hot_reloads_key_rotation_revocation_base_url_and_file_deletion() {
        let (upstream1_addr, upstream1_keys, upstream1_handle) =
            spawn_mock_upstream("upstream-1").await;
        let (upstream2_addr, upstream2_keys, upstream2_handle) =
            spawn_mock_upstream("upstream-2").await;

        let tmp = TempDir::new().unwrap();
        let config_path = tmp.path().join("config.json");

        let mut cfg = Config::default();
        cfg.providers.gemini.base_url = format!("http://{upstream1_addr}");
        cfg.providers.gemini.api_key =
            Some(ApiKey::parse_strict("AIzaInitialKey11111111").unwrap());
        cfg.save_secure(&config_path).unwrap();

        // env_fallback = false keeps revocation deterministic even when host exports GEMINI_API_KEY.
        let state = ProxyState::from_config_with_env_fallback(
            cfg.clone(),
            Some(config_path.clone()),
            Arc::new(NoopUsageSink),
            false,
        )
        .unwrap();

        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let proxy_addr = listener.local_addr().unwrap();
        let (tx_shutdown, rx_shutdown) = oneshot::channel::<()>();
        let server = tokio::spawn(async move {
            serve_on_listener(listener, state, async move {
                let _ = rx_shutdown.await;
            })
            .await
            .unwrap();
        });

        let client = test_http_client();
        let url = format!(
            "http://{proxy_addr}/v1beta/models/gemini-4-hydrogen:streamGenerateContent?alt=sse"
        );

        // 1. Initial key hits upstream 1.
        let r1 = client.post(&url).body("{}").send().await.unwrap();
        assert_eq!(r1.status(), StatusCode::OK);
        assert!(r1.text().await.unwrap().contains("upstream-1"));
        assert_eq!(
            upstream1_keys.lock().unwrap().last().cloned().flatten(),
            Some("AIzaInitialKey11111111".to_string())
        );

        // 2. Immediate same-millisecond key rotation via Config::save_secure.
        cfg.providers.gemini.api_key =
            Some(ApiKey::parse_strict("AIzaRotatedKey22222222").unwrap());
        cfg.save_secure(&config_path).unwrap();
        let r2 = client.post(&url).body("{}").send().await.unwrap();
        assert_eq!(r2.status(), StatusCode::OK);
        let _ = r2.text().await.unwrap();
        assert_eq!(
            upstream1_keys.lock().unwrap().last().cloned().flatten(),
            Some("AIzaRotatedKey22222222".to_string())
        );

        // 3. Base URL switch to upstream 2.
        cfg.providers.gemini.base_url = format!("http://{upstream2_addr}");
        cfg.save_secure(&config_path).unwrap();
        let r3 = client.post(&url).body("{}").send().await.unwrap();
        assert_eq!(r3.status(), StatusCode::OK);
        assert!(r3.text().await.unwrap().contains("upstream-2"));
        assert_eq!(
            upstream2_keys.lock().unwrap().last().cloned().flatten(),
            Some("AIzaRotatedKey22222222".to_string())
        );

        // 4. Revoke API key (clear_api_key -> None) -> 401 Unauthorized immediately.
        cfg.providers.gemini.api_key = None;
        cfg.save_secure(&config_path).unwrap();
        let r4 = client.post(&url).body("{}").send().await.unwrap();
        assert_eq!(r4.status(), StatusCode::UNAUTHORIZED);

        // 5. Re-add key, confirm 200, then delete config file -> immediately revokes key -> 401.
        cfg.providers.gemini.api_key =
            Some(ApiKey::parse_strict("AIzaReaddedKey33333333").unwrap());
        cfg.save_secure(&config_path).unwrap();
        let r5 = client.post(&url).body("{}").send().await.unwrap();
        assert_eq!(r5.status(), StatusCode::OK);
        let _ = r5.text().await.unwrap();

        fs::remove_file(&config_path).unwrap();
        let r6 = client.post(&url).body("{}").send().await.unwrap();
        assert_eq!(r6.status(), StatusCode::UNAUTHORIZED);

        let _ = tx_shutdown.send(());
        server.await.unwrap();
        upstream1_handle.abort();
        upstream2_handle.abort();
    }

    #[tokio::test]
    async fn hot_reload_survives_corrupt_and_invalid_config_and_recovers_without_leaking_secrets() {
        let (upstream_addr, upstream_keys, upstream_handle) =
            spawn_mock_upstream("resilient").await;

        let tmp = TempDir::new().unwrap();
        let config_path = tmp.path().join("config.json");

        let mut cfg = Config::default();
        cfg.providers.gemini.base_url = format!("http://{upstream_addr}");
        cfg.providers.gemini.api_key =
            Some(ApiKey::parse_strict("AIzaValidBeforeCorrupt1").unwrap());
        cfg.save_secure(&config_path).unwrap();

        let state = ProxyState::from_config_with_env_fallback(
            cfg.clone(),
            Some(config_path.clone()),
            Arc::new(NoopUsageSink),
            false,
        )
        .unwrap();

        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let proxy_addr = listener.local_addr().unwrap();
        let (tx_shutdown, rx_shutdown) = oneshot::channel::<()>();
        let server = tokio::spawn(async move {
            serve_on_listener(listener, state, async move {
                let _ = rx_shutdown.await;
            })
            .await
            .unwrap();
        });

        let client = test_http_client();
        let url = format!(
            "http://{proxy_addr}/v1beta/models/gemini-4-hydrogen:streamGenerateContent?alt=sse"
        );

        // 1. Write corrupt JSON containing a secret -> proxy keeps last valid provider cleanly.
        fs::write(&config_path, "{ \"api_key\": \"AIzaCORRUPTSECRET\", broken").unwrap();
        let r1 = client.post(&url).body("{}").send().await.unwrap();
        assert_eq!(r1.status(), StatusCode::OK);
        let body1 = r1.text().await.unwrap();
        assert!(body1.contains("resilient"));
        assert!(!body1.contains("AIzaCORRUPTSECRET"));
        assert_eq!(
            upstream_keys.lock().unwrap().last().cloned().flatten(),
            Some("AIzaValidBeforeCorrupt1".to_string())
        );

        // 2. Write schema-invalid config (0.0.0.0) -> proxy still retains last valid provider.
        let bad_json = serde_json::json!({
            "schema_version": 1,
            "proxy": {
                "host": "0.0.0.0",
                "port": 4141
            },
            "providers": {
                "gemini": {
                    "base_url": format!("http://{upstream_addr}"),
                    "api_key": "AIzaINVALIDHOSTSECRET",
                    "default_model": "gemini-4-hydrogen",
                    "timeout_secs": 900
                }
            }
        });
        fs::write(&config_path, bad_json.to_string()).unwrap();
        let r2 = client.post(&url).body("{}").send().await.unwrap();
        assert_eq!(r2.status(), StatusCode::OK);
        let _ = r2.text().await.unwrap();
        assert_eq!(
            upstream_keys.lock().unwrap().last().cloned().flatten(),
            Some("AIzaValidBeforeCorrupt1".to_string())
        );

        // 3. Write valid updated config -> proxy immediately recovers and adopts new key.
        cfg.providers.gemini.api_key =
            Some(ApiKey::parse_strict("AIzaRecoveredKey9999999").unwrap());
        cfg.save_secure(&config_path).unwrap();
        let r3 = client.post(&url).body("{}").send().await.unwrap();
        assert_eq!(r3.status(), StatusCode::OK);
        let _ = r3.text().await.unwrap();
        assert_eq!(
            upstream_keys.lock().unwrap().last().cloned().flatten(),
            Some("AIzaRecoveredKey9999999".to_string())
        );

        let _ = tx_shutdown.send(());
        server.await.unwrap();
        upstream_handle.abort();
    }

    #[cfg(unix)]
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn hot_reload_rejects_post_startup_symlink_dangling_symlink_and_fifo_without_blocking() {
        use std::os::unix::fs::symlink;

        let (upstream1_addr, upstream1_keys, upstream1_handle) =
            spawn_mock_upstream("good-upstream").await;
        let (upstream2_addr, upstream2_keys, upstream2_handle) =
            spawn_mock_upstream("evil-upstream").await;

        let tmp = TempDir::new().unwrap();
        let config_path = tmp.path().join("config.json");
        let evil_path = tmp.path().join("evil.json");

        let mut good_cfg = Config::default();
        good_cfg.providers.gemini.base_url = format!("http://{upstream1_addr}");
        good_cfg.providers.gemini.api_key =
            Some(ApiKey::parse_strict("AIzaGoodOriginalKey1111").unwrap());
        good_cfg.save_secure(&config_path).unwrap();

        let mut evil_cfg = Config::default();
        evil_cfg.providers.gemini.base_url = format!("http://{upstream2_addr}");
        evil_cfg.providers.gemini.api_key =
            Some(ApiKey::parse_strict("AIzaEvilHijackKey22222").unwrap());
        evil_cfg.save_secure(&evil_path).unwrap();

        // Startup must also reject an initial symlink or FIFO config_path cleanly.
        let initial_sym = tmp.path().join("initial_sym.json");
        symlink(&evil_path, &initial_sym).unwrap();
        assert!(matches!(
            ProxyState::from_config_with_env_fallback(
                good_cfg.clone(),
                Some(initial_sym),
                Arc::new(NoopUsageSink),
                false,
            ),
            Err(ProxyError::Core(CoreError::Fs(
                FsError::SymlinkRejected { .. }
            )))
        ));

        let state = ProxyState::from_config_with_env_fallback(
            good_cfg.clone(),
            Some(config_path.clone()),
            Arc::new(NoopUsageSink),
            false,
        )
        .unwrap();

        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let proxy_addr = listener.local_addr().unwrap();
        let (tx_shutdown, rx_shutdown) = oneshot::channel::<()>();
        let server = tokio::spawn(async move {
            serve_on_listener(listener, state, async move {
                let _ = rx_shutdown.await;
            })
            .await
            .unwrap();
        });

        let client = reqwest::Client::builder()
            .no_proxy()
            .timeout(Duration::from_secs(2))
            .build()
            .expect("build loopback client");
        let url = format!(
            "http://{proxy_addr}/v1beta/models/gemini-4-hydrogen:streamGenerateContent?alt=sse"
        );

        // Baseline: hits upstream 1 with original key.
        let r0 = client.post(&url).body("{}").send().await.unwrap();
        assert_eq!(r0.status(), StatusCode::OK);
        assert!(r0.text().await.unwrap().contains("good-upstream"));

        // (a) Post-startup swap to symlink pointing to evil.json -> must retain original upstream & key.
        fs::remove_file(&config_path).unwrap();
        symlink(&evil_path, &config_path).unwrap();
        let r1 = client.post(&url).body("{}").send().await.unwrap();
        assert_eq!(r1.status(), StatusCode::OK);
        let body1 = r1.text().await.unwrap();
        assert!(body1.contains("good-upstream"), "body1={body1}");
        assert!(!body1.contains("evil-upstream"));
        assert!(
            upstream2_keys.lock().unwrap().is_empty(),
            "evil upstream must receive 0 hits"
        );
        assert_eq!(
            upstream1_keys.lock().unwrap().last().cloned().flatten(),
            Some("AIzaGoodOriginalKey1111".to_string())
        );

        // (b) Post-startup swap to dangling symlink -> must retain original config (NOT 401 wipe!).
        fs::remove_file(&config_path).unwrap();
        symlink(tmp.path().join("does-not-exist.json"), &config_path).unwrap();
        let r2 = client.post(&url).body("{}").send().await.unwrap();
        assert_eq!(
            r2.status(),
            StatusCode::OK,
            "dangling symlink must retain active config instead of wiping to 401"
        );
        assert!(r2.text().await.unwrap().contains("good-upstream"));

        // (c) Post-startup swap to FIFO -> must NOT hang and must retain active config.
        fs::remove_file(&config_path).unwrap();
        let mkfifo_status = std::process::Command::new("mkfifo")
            .arg(&config_path)
            .status()
            .expect("run mkfifo");
        assert!(mkfifo_status.success(), "mkfifo failed");
        let r3 = tokio::time::timeout(Duration::from_secs(1), client.post(&url).body("{}").send())
            .await
            .expect("proxy deadlocked on FIFO config swap")
            .expect("request succeeded");
        assert_eq!(r3.status(), StatusCode::OK);
        assert!(r3.text().await.unwrap().contains("good-upstream"));

        // (d) Clean recovery when replaced back with regular file via save_secure.
        fs::remove_file(&config_path).unwrap();
        good_cfg.providers.gemini.api_key =
            Some(ApiKey::parse_strict("AIzaRecoveredAfterSwap33").unwrap());
        good_cfg.save_secure(&config_path).unwrap();
        let r4 = client.post(&url).body("{}").send().await.unwrap();
        assert_eq!(r4.status(), StatusCode::OK);
        let _ = r4.text().await.unwrap();
        assert_eq!(
            upstream1_keys.lock().unwrap().last().cloned().flatten(),
            Some("AIzaRecoveredAfterSwap33".to_string())
        );

        let _ = tx_shutdown.send(());
        server.await.unwrap();
        upstream1_handle.abort();
        upstream2_handle.abort();
    }

    #[tokio::test]
    async fn hot_reload_preserves_cloudcode_routing_cache_when_upstream_and_timeout_unchanged_and_rebuilds_on_change()
     {
        let tmp = TempDir::new().unwrap();
        let config_path = tmp.path().join("config.json");

        let mut cfg = Config::default();
        cfg.cloudcode.upstream = CloudCodeUpstream::Auto;
        cfg.providers.gemini.timeout_secs = 900;
        cfg.save_secure(&config_path).unwrap();

        let state = ProxyState::from_config_with_env_fallback(
            cfg.clone(),
            Some(config_path.clone()),
            Arc::new(NoopUsageSink),
            false,
        )
        .unwrap();

        let initial_cloudcode = Arc::clone(&state.active_provider().cloudcode);
        assert_eq!(initial_cloudcode.mode_label(), "auto");

        // Saving a new API key without changing cloudcode.upstream or timeout_secs
        // must preserve the exact same Arc<CloudCodeClient> (and its routing LRU cache).
        cfg.providers.gemini.api_key =
            Some(ApiKey::parse_strict("AIzaSyReloadPreservesLru1").unwrap());
        cfg.save_secure(&config_path).unwrap();
        let after_key_save = state.active_provider();
        assert!(after_key_save.has_configured_api_key);
        assert!(
            Arc::ptr_eq(&initial_cloudcode, &after_key_save.cloudcode),
            "CloudCodeClient Arc must be preserved across API key hot-reload"
        );

        // Changing timeout_secs must rebuild CloudCodeClient.
        cfg.providers.gemini.timeout_secs = 600;
        cfg.save_secure(&config_path).unwrap();
        let after_timeout_change = state.active_provider();
        assert!(
            !Arc::ptr_eq(&initial_cloudcode, &after_timeout_change.cloudcode),
            "CloudCodeClient Arc must rebuild when timeout_secs changes"
        );

        // Changing cloudcode.upstream must also rebuild CloudCodeClient.
        let prev = Arc::clone(&after_timeout_change.cloudcode);
        cfg.cloudcode.upstream =
            CloudCodeUpstream::Fixed("https://cloudcode-pa.googleapis.com".to_string());
        cfg.save_secure(&config_path).unwrap();
        let after_url_change = state.active_provider();
        assert!(
            !Arc::ptr_eq(&prev, &after_url_change.cloudcode),
            "CloudCodeClient Arc must rebuild when cloudcode_upstream_url changes"
        );
        assert_eq!(after_url_change.cloudcode.mode_label(), "fixed");
    }

    #[tokio::test]
    async fn activity_tracker_keeps_in_flight_active_across_long_sse_stream_and_resets_idle_after_eof_and_disconnect()
     {
        use axum::body::Body;
        use axum::http::Request;
        use bytes::Bytes;
        use http_body_util::BodyExt;
        use tokio::sync::{Notify, mpsc};
        use tower::ServiceExt;

        let chunk1_sent = Arc::new(Notify::new());
        let release_chunk2 = Arc::new(Notify::new());
        let c1 = Arc::clone(&chunk1_sent);
        let r2 = Arc::clone(&release_chunk2);

        let stream_handler = move || {
            let c1 = Arc::clone(&c1);
            let r2 = Arc::clone(&r2);
            async move {
                let (tx, rx) = mpsc::channel::<Result<Bytes, std::io::Error>>(4);
                tokio::spawn(async move {
                    let _ = tx
                        .send(Ok(Bytes::from_static(b"data: {\"chunk\":1}\n\n")))
                        .await;
                    c1.notify_one();
                    r2.notified().await;
                    let _ = tx
                        .send(Ok(Bytes::from_static(b"data: {\"chunk\":2}\n\n")))
                        .await;
                });
                let body_stream = futures_util::stream::unfold(rx, |mut rx| async move {
                    rx.recv().await.map(|item| (item, rx))
                });
                (
                    StatusCode::OK,
                    [(header::CONTENT_TYPE, "text/event-stream")],
                    Body::from_stream(body_stream),
                )
            }
        };

        let upstream = Router::new()
            .route(
                "/v1beta/models/gemini-4-hydrogen:streamGenerateContent",
                post(stream_handler.clone()),
            )
            .route("/v1internal:streamGenerateContent", post(stream_handler));

        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let upstream_addr = listener.local_addr().unwrap();
        let upstream_srv = tokio::spawn(async move {
            let _ = axum::serve(listener, upstream).await;
        });

        let mut cfg = Config::default();
        cfg.providers.gemini.base_url = format!("http://{upstream_addr}");
        cfg.providers.gemini.api_key =
            Some(ApiKey::parse_strict("AIzaSyActivityTestKey123").unwrap());
        cfg.cloudcode.upstream = CloudCodeUpstream::Fixed(format!("http://{upstream_addr}"));

        let state =
            ProxyState::from_config_with_env_fallback(cfg, None, Arc::new(NoopUsageSink), false)
                .unwrap();
        assert_eq!(state.in_flight_requests(), 0);
        let app = build_router(state.clone());

        // 1. /v1beta/models/* SSE stream: stays in_flight == 1 while paused mid-stream, returns to 0 after EOF.
        let resp = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/v1beta/models/gemini-4-hydrogen:streamGenerateContent?alt=sse")
                    .header(header::CONTENT_TYPE, "application/json")
                    .body(Body::from("{}"))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let mut body = resp.into_body();
        let first_frame = body.frame().await.expect("chunk 1").unwrap();
        assert!(!first_frame.into_data().unwrap().is_empty());
        chunk1_sent.notified().await;

        assert_eq!(state.in_flight_requests(), 1);
        tokio::time::sleep(Duration::from_millis(30)).await;
        assert_eq!(
            state.idle_duration(),
            Duration::ZERO,
            "idle_duration must stay ZERO while SSE stream is active"
        );

        release_chunk2.notify_one();
        while body.frame().await.is_some() {}
        drop(body);
        assert_eq!(state.in_flight_requests(), 0);
        tokio::time::sleep(Duration::from_millis(25)).await;
        assert!(
            state.idle_duration() >= Duration::from_millis(15),
            "idle_duration must advance once stream completes"
        );

        // 2. /v1internal:streamGenerateContent early client disconnect releases in_flight back to 0.
        let cc_resp = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/v1internal:streamGenerateContent?alt=sse")
                    .header(header::CONTENT_TYPE, "application/json")
                    .body(Body::from("{}"))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(cc_resp.status(), StatusCode::OK);
        let mut cc_body = cc_resp.into_body();
        let _ = cc_body.frame().await.expect("cc chunk 1").unwrap();
        chunk1_sent.notified().await;
        assert_eq!(state.in_flight_requests(), 1);
        assert_eq!(state.idle_duration(), Duration::ZERO);

        // Drop response body early without waiting for chunk 2!
        drop(cc_body);
        assert_eq!(
            state.in_flight_requests(),
            0,
            "early client disconnect must decrement in_flight back to 0"
        );
        release_chunk2.notify_one();

        upstream_srv.abort();
    }

    #[tokio::test]
    async fn passive_health_and_stats_probes_do_not_reset_idle_duration() {
        use axum::http::Request;
        use tower::ServiceExt;

        let state = ProxyState::from_config_with_env_fallback(
            Config::default(),
            None,
            Arc::new(NoopUsageSink),
            false,
        )
        .unwrap();
        let app = build_router(state.clone());

        tokio::time::sleep(Duration::from_millis(60)).await;
        let idle_before = state.idle_duration();
        assert!(
            idle_before >= Duration::from_millis(45),
            "expected idle_before >= 45ms, got {idle_before:?}"
        );

        let health_resp = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("GET")
                    .uri("/health")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(health_resp.status(), StatusCode::OK);

        let stats_resp = app
            .oneshot(
                Request::builder()
                    .method("GET")
                    .uri("/api/stats")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(stats_resp.status(), StatusCode::OK);

        assert_eq!(state.in_flight_requests(), 0);
        let idle_after = state.idle_duration();
        assert!(
            idle_after >= idle_before,
            "passive /health and /api/stats probes must not reset idle_duration (before={idle_before:?}, after={idle_after:?})"
        );
    }
}
