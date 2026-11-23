#![forbid(unsafe_code)]

pub mod routes;
pub mod sse;
pub mod stats;

use std::fs;
use std::future::Future;
use std::io::{ErrorKind, Read};
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::path::{Path, PathBuf};
use std::sync::{Arc, RwLock};
use std::time::{Duration, Instant, SystemTime};

use companion_core::{Config, CoreError, MAX_CONFIG_FILE_BYTES, UsageSink, validate_path};
use companion_providers::{GeminiProvider, ProviderAdapter, ProviderError};
use thiserror::Error;
use tokio::net::TcpListener;

pub use routes::{DEFAULT_MAX_JSON_TEE_BYTES, HealthResponse, MAX_PROXY_BODY_BYTES, build_router};
pub use sse::{DEFAULT_MAX_SSE_EVENT_BYTES, SseUsageTracker};
pub use stats::{ProxyStats, StatsSnapshot};

pub const PROTOCOL_VERSION: u32 = 1;

const RACY_CLEAN_WINDOW: Duration = Duration::from_secs(1);

#[derive(Debug, Error)]
pub enum ProxyError {
    #[error("configuration error: {0}")]
    Core(#[from] CoreError),
    #[error("provider error: {0}")]
    Provider(#[from] ProviderError),
    #[error("failed to bind loopback listener on {addr}: {source}")]
    Bind {
        addr: SocketAddr,
        #[source]
        source: std::io::Error,
    },
    #[error("proxy server error: {0}")]
    Serve(#[source] std::io::Error),
}

#[derive(Clone)]
pub(crate) struct ActiveProvider {
    pub(crate) provider: Arc<dyn ProviderAdapter>,
    pub(crate) has_configured_api_key: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RejectedReason {
    Symlink,
    NotRegularFile,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum FileStamp {
    Missing,
    Rejected(RejectedReason),
    Unreadable(ErrorKind),
    Present {
        modified: Option<SystemTime>,
        len: u64,
        #[cfg(unix)]
        dev: u64,
        #[cfg(unix)]
        ino: u64,
        #[cfg(unix)]
        ctime: i64,
        #[cfg(unix)]
        ctime_nsec: i64,
    },
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

struct ProxyStateInner {
    source: ProviderSource,
    stats: Arc<ProxyStats>,
    usage_sink: Arc<dyn UsageSink>,
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
        Self {
            inner: Arc::new(ProxyStateInner {
                source: ProviderSource::Static(ActiveProvider {
                    provider,
                    has_configured_api_key,
                }),
                stats: Arc::new(ProxyStats::new()),
                usage_sink,
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
        let active = build_active_provider(&config, env_fallback)?;

        let source = match config_path {
            Some(path) => {
                validate_path(&path)?;
                let stamp = read_file_stamp(&path);
                if let FileStamp::Rejected(reason) = stamp {
                    let msg = match reason {
                        RejectedReason::Symlink => {
                            "symbolic links are not allowed for config files"
                        }
                        RejectedReason::NotRegularFile => "config path must be a regular file",
                    };
                    return Err(ProxyError::Core(CoreError::InvalidPath {
                        reason: msg.to_string(),
                    }));
                }
                let raw_bytes = read_bounded_bytes(&path, &stamp);
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
            }),
        })
    }

    pub fn stats(&self) -> &Arc<ProxyStats> {
        &self.inner.stats
    }

    pub fn usage_sink(&self) -> &Arc<dyn UsageSink> {
        &self.inner.usage_sink
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
) -> Result<ActiveProvider, ProxyError> {
    let provider = GeminiProvider::from_config(&config.gemini)?.with_env_fallback(env_fallback);
    let has_configured_api_key = provider.api_key_hint().is_some();
    Ok(ActiveProvider {
        provider: Arc::new(provider),
        has_configured_api_key,
    })
}

fn read_file_stamp(path: &Path) -> FileStamp {
    // Strip trailing separators/dot components so POSIX lstat(2) inspects the entry itself
    // without dereferencing symlinks or opening FIFOs/devices.
    let check_path: PathBuf = path.components().collect();
    match fs::symlink_metadata(&check_path) {
        Ok(meta) => {
            let file_type = meta.file_type();
            if file_type.is_symlink() {
                return FileStamp::Rejected(RejectedReason::Symlink);
            }
            if !file_type.is_file() {
                return FileStamp::Rejected(RejectedReason::NotRegularFile);
            }
            let modified = meta.modified().ok();
            let len = meta.len();
            #[cfg(unix)]
            {
                use std::os::unix::fs::MetadataExt;
                FileStamp::Present {
                    modified,
                    len,
                    dev: meta.dev(),
                    ino: meta.ino(),
                    ctime: meta.ctime(),
                    ctime_nsec: meta.ctime_nsec(),
                }
            }
            #[cfg(not(unix))]
            {
                FileStamp::Present { modified, len }
            }
        }
        Err(err) if err.kind() == ErrorKind::NotFound => FileStamp::Missing,
        Err(err) => FileStamp::Unreadable(err.kind()),
    }
}

enum BoundedReadError {
    NotFound,
    Oversized,
    Rejected(RejectedReason),
    ChangedConcurrently,
    Io(ErrorKind),
}

fn read_verified_config_bytes(path: &Path, stamp: &FileStamp) -> Result<Vec<u8>, BoundedReadError> {
    let FileStamp::Present {
        len,
        #[cfg(unix)]
        dev,
        #[cfg(unix)]
        ino,
        ..
    } = stamp
    else {
        return Err(match stamp {
            FileStamp::Missing => BoundedReadError::NotFound,
            FileStamp::Rejected(reason) => BoundedReadError::Rejected(*reason),
            FileStamp::Unreadable(kind) => BoundedReadError::Io(*kind),
            FileStamp::Present { .. } => unreachable!(),
        });
    };

    if *len > MAX_CONFIG_FILE_BYTES {
        return Err(BoundedReadError::Oversized);
    }

    let file = fs::File::open(path).map_err(|err| {
        if err.kind() == ErrorKind::NotFound {
            BoundedReadError::NotFound
        } else {
            BoundedReadError::Io(err.kind())
        }
    })?;

    let open_meta = file
        .metadata()
        .map_err(|err| BoundedReadError::Io(err.kind()))?;
    if !open_meta.file_type().is_file() {
        return Err(BoundedReadError::Rejected(RejectedReason::NotRegularFile));
    }

    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        if open_meta.dev() != *dev || open_meta.ino() != *ino {
            return Err(BoundedReadError::ChangedConcurrently);
        }
    }

    let mut bytes = Vec::new();
    file.take(MAX_CONFIG_FILE_BYTES + 1)
        .read_to_end(&mut bytes)
        .map_err(|err| BoundedReadError::Io(err.kind()))?;

    if (bytes.len() as u64) > MAX_CONFIG_FILE_BYTES {
        return Err(BoundedReadError::Oversized);
    }

    Ok(bytes)
}

fn read_bounded_bytes(path: &Path, stamp: &FileStamp) -> Option<Vec<u8>> {
    read_verified_config_bytes(path, stamp).ok()
}

impl HotReloadProvider {
    fn active_provider(&self) -> ActiveProvider {
        let current_stamp = read_file_stamp(&self.path);

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
                if let Some(bytes) = read_bounded_bytes(&self.path, &current_stamp)
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
                if let Some(bytes) = read_bounded_bytes(&self.path, &current_stamp)
                    && guard.last_failed_bytes.as_deref() == Some(bytes.as_slice())
                {
                    return guard.active.clone();
                }
            }
        }

        let mut guard = self.inner.write().unwrap_or_else(|e| e.into_inner());
        let current_stamp = read_file_stamp(&self.path);
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
                let bytes = match read_verified_config_bytes(&self.path, &current_stamp) {
                    Ok(bytes) => bytes,
                    Err(BoundedReadError::NotFound) => {
                        self.apply_missing_config(&mut guard);
                        return guard.active.clone();
                    }
                    Err(BoundedReadError::Oversized) => {
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
                    Err(BoundedReadError::Rejected(reason)) => {
                        let rejected_stamp = FileStamp::Rejected(reason);
                        if guard.last_failed_stamp.as_ref() != Some(&rejected_stamp) {
                            tracing::warn!(
                                reason = ?reason,
                                "ignoring rejected config path during hot-reload; retaining previous configuration"
                            );
                        }
                        guard.last_failed_stamp = Some(rejected_stamp);
                        guard.last_failed_bytes = None;
                        guard.checked_at = Instant::now();
                        return guard.active.clone();
                    }
                    Err(BoundedReadError::ChangedConcurrently) => {
                        tracing::warn!(
                            "config file changed concurrently during hot-reload read; retaining previous configuration"
                        );
                        guard.checked_at = Instant::now();
                        return guard.active.clone();
                    }
                    Err(BoundedReadError::Io(kind)) => {
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

                let parsed: Config = match serde_json::from_slice(&bytes) {
                    Ok(cfg) => cfg,
                    Err(err) => {
                        tracing::warn!(
                            line = err.line(),
                            column = err.column(),
                            "failed to parse config JSON during hot-reload; retaining previous configuration"
                        );
                        guard.last_failed_stamp = Some(current_stamp);
                        guard.last_failed_bytes = Some(bytes);
                        guard.checked_at = Instant::now();
                        return guard.active.clone();
                    }
                };

                if parsed.validate().is_err() {
                    tracing::warn!(
                        "config validation failed during hot-reload; retaining previous configuration"
                    );
                    guard.last_failed_stamp = Some(current_stamp);
                    guard.last_failed_bytes = Some(bytes);
                    guard.checked_at = Instant::now();
                    return guard.active.clone();
                }

                let new_active = match build_active_provider(&parsed, self.env_fallback) {
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
        if let Ok(default_active) = build_active_provider(&Config::default(), self.env_fallback) {
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
    Ok(SocketAddr::from((Ipv4Addr::LOCALHOST, config.listen_port)))
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
    use std::sync::Mutex;

    use axum::Router;
    use axum::body::Body;
    use axum::response::Response;
    use axum::routing::post;
    use companion_core::{JsonlUsageSink, NoopUsageSink, UsageEvent};
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
        let cfg_any = Config {
            listen_host: "0.0.0.0".to_string(),
            ..Config::default()
        };
        assert!(matches!(
            loopback_socket_addr(&cfg_any),
            Err(ProxyError::Core(CoreError::InvalidListenHost { .. }))
        ));
        assert!(matches!(
            serve_with_shutdown(cfg_any, None, Arc::new(NoopUsageSink), async {}).await,
            Err(ProxyError::Core(CoreError::InvalidListenHost { .. }))
        ));

        let cfg_v6 = Config {
            listen_host: "::".to_string(),
            ..Config::default()
        };
        assert!(matches!(
            loopback_socket_addr(&cfg_v6),
            Err(ProxyError::Core(CoreError::InvalidListenHost { .. }))
        ));

        let cfg_port0 = Config {
            listen_port: 0,
            ..Config::default()
        };
        assert!(matches!(
            loopback_socket_addr(&cfg_port0),
            Err(ProxyError::Core(CoreError::InvalidListenPort))
        ));

        // "localhost" must still bind strictly to 127.0.0.1:<port>.
        let cfg_localhost = Config {
            listen_host: "localhost".to_string(),
            listen_port: 4141,
            ..Config::default()
        };
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
        let sink = Arc::new(JsonlUsageSink::new(&usage_dir).unwrap());

        let mut cfg = Config::default();
        cfg.gemini.base_url = format!("http://{upstream_addr}");
        cfg.gemini.api_key = Some("AIzaLiveLoopbackKey12345".to_string());

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
                "http://{proxy_addr}/v1beta/models/gemini-2.5-pro:streamGenerateContent?alt=sse"
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
        assert_eq!(snap.requests, 1);
        assert_eq!(snap.prompt_tokens, 40);
        assert_eq!(snap.cached_tokens, 20);
        assert_eq!(snap.output_tokens, 10);
        assert!((snap.cache_hit_ratio - 0.5).abs() < 1e-12);

        // 4. JsonlUsageSink persisted 1 event with zero secrets and 0600 permissions.
        let mut jsonl_files: Vec<PathBuf> = fs::read_dir(&usage_dir)
            .unwrap()
            .filter_map(|e| e.ok().map(|e| e.path()))
            .collect();
        assert_eq!(jsonl_files.len(), 1);
        let jsonl_path = jsonl_files.pop().unwrap();
        let jsonl_contents = fs::read_to_string(&jsonl_path).unwrap();
        assert!(!jsonl_contents.contains("AIzaLiveLoopbackKey12345"));
        let lines: Vec<&str> = jsonl_contents.lines().collect();
        assert_eq!(lines.len(), 1);
        let event: UsageEvent = serde_json::from_str(lines[0]).unwrap();
        assert_eq!(event.model, "gemini-2.5-pro");
        assert_eq!(event.input_tokens, 40);
        assert_eq!(event.cached_tokens, 20);

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = fs::metadata(&jsonl_path).unwrap().permissions().mode() & 0o777;
            assert_eq!(mode, 0o600);
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
        cfg.gemini.base_url = format!("http://{upstream1_addr}");
        cfg.gemini.api_key = Some("AIzaInitialKey11111111".to_string());
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
            "http://{proxy_addr}/v1beta/models/gemini-2.5-pro:streamGenerateContent?alt=sse"
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
        cfg.gemini.api_key = Some("AIzaRotatedKey22222222".to_string());
        cfg.save_secure(&config_path).unwrap();
        let r2 = client.post(&url).body("{}").send().await.unwrap();
        assert_eq!(r2.status(), StatusCode::OK);
        let _ = r2.text().await.unwrap();
        assert_eq!(
            upstream1_keys.lock().unwrap().last().cloned().flatten(),
            Some("AIzaRotatedKey22222222".to_string())
        );

        // 3. Base URL switch to upstream 2.
        cfg.gemini.base_url = format!("http://{upstream2_addr}");
        cfg.save_secure(&config_path).unwrap();
        let r3 = client.post(&url).body("{}").send().await.unwrap();
        assert_eq!(r3.status(), StatusCode::OK);
        assert!(r3.text().await.unwrap().contains("upstream-2"));
        assert_eq!(
            upstream2_keys.lock().unwrap().last().cloned().flatten(),
            Some("AIzaRotatedKey22222222".to_string())
        );

        // 4. Revoke API key (clear_api_key -> None) -> 401 Unauthorized immediately.
        cfg.gemini.api_key = None;
        cfg.save_secure(&config_path).unwrap();
        let r4 = client.post(&url).body("{}").send().await.unwrap();
        assert_eq!(r4.status(), StatusCode::UNAUTHORIZED);

        // 5. Re-add key, confirm 200, then delete config file -> immediately revokes key -> 401.
        cfg.gemini.api_key = Some("AIzaReaddedKey33333333".to_string());
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
        cfg.gemini.base_url = format!("http://{upstream_addr}");
        cfg.gemini.api_key = Some("AIzaValidBeforeCorrupt1".to_string());
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
            "http://{proxy_addr}/v1beta/models/gemini-2.5-pro:streamGenerateContent?alt=sse"
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
            "listen_host": "0.0.0.0",
            "listen_port": 4141,
            "gemini": {
                "base_url": format!("http://{upstream_addr}"),
                "api_key": "AIzaINVALIDHOSTSECRET",
                "default_model": "gemini-2.5-pro",
                "timeout_secs": 900
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
        cfg.gemini.api_key = Some("AIzaRecoveredKey9999999".to_string());
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
        good_cfg.gemini.base_url = format!("http://{upstream1_addr}");
        good_cfg.gemini.api_key = Some("AIzaGoodOriginalKey1111".to_string());
        good_cfg.save_secure(&config_path).unwrap();

        let mut evil_cfg = Config::default();
        evil_cfg.gemini.base_url = format!("http://{upstream2_addr}");
        evil_cfg.gemini.api_key = Some("AIzaEvilHijackKey22222".to_string());
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
            Err(ProxyError::Core(CoreError::InvalidPath { .. }))
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
            "http://{proxy_addr}/v1beta/models/gemini-2.5-pro:streamGenerateContent?alt=sse"
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
        good_cfg.gemini.api_key = Some("AIzaRecoveredAfterSwap33".to_string());
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
}
