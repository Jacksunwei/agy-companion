#![forbid(unsafe_code)]

mod activity;
pub mod cloudcode;
mod hot_reload;
pub mod routes;
pub mod sse;
pub mod stats;
#[cfg(test)]
mod tests;

use std::future::Future;
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use companion_core::{
    CloudCodeUpstream, Config, CoreError, DEFAULT_CLOUDCODE_UPSTREAM_URL, DEFAULT_GEMINI_MODEL,
    DEFAULT_TIMEOUT_SECS, FsError, UsageSink,
};
use companion_providers::{GeminiProvider, ProviderAdapter, ProviderError};
use thiserror::Error;
use tokio::net::TcpListener;

use activity::ActivityTracker;
pub(crate) use activity::InFlightGuard;
use hot_reload::HotReloadProvider;

pub use cloudcode::{
    CloudCodeClient, CloudCodeError, MAX_CLOUDCODE_CATALOG_BYTES, is_cloudcode_path,
    merge_available_models, validate_cloudcode_path,
};
pub use companion_core::protocol::{HealthResponse, PROTOCOL_VERSION, StatsSnapshot};
pub use routes::{DEFAULT_MAX_JSON_TEE_BYTES, MAX_PROXY_BODY_BYTES, build_router};
pub use sse::{DEFAULT_MAX_SSE_EVENT_BYTES, SseUsageTracker};
pub use stats::ProxyStats;

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

enum ProviderSource {
    Static(ActiveProvider),
    HotReload(Box<HotReloadProvider>),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProxyStateOptions {
    pub config_path: Option<PathBuf>,
    pub env_fallback: bool,
}

impl Default for ProxyStateOptions {
    fn default() -> Self {
        Self {
            config_path: None,
            env_fallback: true,
        }
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
    ) -> Result<Self, ProxyError> {
        let cloudcode = Arc::new(CloudCodeClient::new(
            DEFAULT_CLOUDCODE_UPSTREAM_URL,
            DEFAULT_TIMEOUT_SECS,
        )?);
        Ok(Self::with_cloudcode(
            provider,
            cloudcode,
            usage_sink,
            has_configured_api_key,
        ))
    }

    pub fn with_cloudcode(
        provider: Arc<dyn ProviderAdapter>,
        cloudcode: Arc<CloudCodeClient>,
        usage_sink: Arc<dyn UsageSink>,
        has_configured_api_key: bool,
    ) -> Self {
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

    pub fn from_config(config: Config, usage_sink: Arc<dyn UsageSink>) -> Result<Self, ProxyError> {
        Self::from_config_with_options(config, usage_sink, ProxyStateOptions::default())
    }

    pub fn from_config_with_options(
        config: Config,
        usage_sink: Arc<dyn UsageSink>,
        options: ProxyStateOptions,
    ) -> Result<Self, ProxyError> {
        config.validate()?;
        let active = build_active_provider(&config, options.env_fallback, None)?;

        let source = match options.config_path {
            Some(path) => ProviderSource::HotReload(Box::new(HotReloadProvider::new(
                path,
                active,
                options.env_fallback,
            )?)),
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

    pub(crate) async fn active_provider(&self) -> ActiveProvider {
        match &self.inner.source {
            ProviderSource::Static(active) => active.clone(),
            ProviderSource::HotReload(hot_reload) => hot_reload.active_provider().await,
        }
    }
}

pub(crate) fn build_active_provider(
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
    let state = ProxyState::from_config_with_options(
        config,
        usage_sink,
        ProxyStateOptions {
            config_path,
            ..Default::default()
        },
    )?;
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
