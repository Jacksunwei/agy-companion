use std::io::ErrorKind;
use std::num::NonZeroU16;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use companion_core::{AuthStore, FileAuthStore, SqliteUsageSink, UsageSink};
use companion_providers::{CodexAuth, CodexAuthManager, CodexAuthOptions, LoopbackProxyClient};
use companion_proxy::{ProxyError, ProxyState, ProxyStateOptions};

use crate::daemon::{
    DEFAULT_IDLE_SHUTDOWN_SECS, DetachedDaemonOptions, MAX_PROXY_LOG_BYTES, PROXY_LOG_FILENAME,
    PROXY_LOG_ROTATED_FILENAME, PROXY_PID_FILENAME, ProxyPidInfo, RotatingFileWriter,
    ServeLogTarget, read_proxy_pid, reject_non_regular_target, resolve_runtime_data_dir,
    spawn_detached_proxy_daemon_with_exe_configured, write_proxy_pid,
};
use crate::plugin_upgrade::{
    BinaryVersionProbeCache, PluginManifestRead, collect_candidate_binaries,
    read_plugin_manifest_version, resolve_desired_version, resolve_plugin_dir,
    resolve_plugin_poll_interval,
};
use crate::process::GracefulStopOutcome;
use crate::{CliError, load_config_checked, precheck_existing_data_dir, resolve_cli_config_path};

pub const ADOPT_RUNNING_POLL_INTERVAL: Duration = Duration::from_millis(500);
pub const WATCH_PID_POLL_INTERVAL: Duration = Duration::from_millis(500);
const REPLACE_STALE_BIND_POLL_INTERVAL: Duration = Duration::from_millis(10);
const REPLACE_STALE_BIND_TIMEOUT: Duration = Duration::from_secs(3);
const REPLACE_STALE_HEALTH_RECHECK_INTERVAL: Duration = Duration::from_millis(150);
const REPLACE_STALE_PROBE_TIMEOUT: Duration = Duration::from_millis(250);
const ADOPT_REPLACE_COOLDOWN: Duration = Duration::from_secs(10);

pub const CODEX_OAUTH_ISSUER_ENV: &str = "AGY_COMPANION_CODEX_OAUTH_ISSUER";
pub const CODEX_CALLBACK_PORT_ENV: &str = "AGY_COMPANION_CODEX_CALLBACK_PORT";
#[cfg(debug_assertions)]
pub const TEST_HEALTH_VERSION_ENV: &str = "AGY_COMPANION_TEST_HEALTH_VERSION";

#[cfg(debug_assertions)]
fn resolve_test_health_version_override() -> Option<String> {
    let raw = std::env::var(TEST_HEALTH_VERSION_ENV).ok()?;
    let trimmed = raw.trim();
    if trimmed.is_empty() || trimmed.len() > 32 || !trimmed.bytes().all(|b| b.is_ascii_graphic()) {
        return None;
    }
    Some(trimmed.to_string())
}

fn resolve_proxy_role(options: &ServeOptions) -> &'static str {
    if options.adopt_running || std::env::var_os(crate::sidecar_ui::SIDECAR_WEB_PORT_ENV).is_some()
    {
        "sidecar"
    } else if options.watch_pid.is_some()
        || options.replace_stale
        || options.idle_shutdown_secs.is_some_and(|secs| secs > 0)
    {
        "bootstrap"
    } else {
        "standalone"
    }
}

fn codex_auth_options_from_env() -> Result<CodexAuthOptions, CliError> {
    let mut opts = CodexAuthOptions::default();
    if let Ok(raw_issuer) = std::env::var(CODEX_OAUTH_ISSUER_ENV) {
        let trimmed = raw_issuer.trim();
        if !trimmed.is_empty() {
            companion_core::validate_upstream_url(trimmed)?;
            opts.oauth_issuer = trimmed.to_string();
        }
    }
    if let Ok(raw_port) = std::env::var(CODEX_CALLBACK_PORT_ENV) {
        let trimmed = raw_port.trim();
        if !trimmed.is_empty() {
            let port: u16 = trimmed.parse().map_err(|_| {
                CliError::Install(format!(
                    "invalid {CODEX_CALLBACK_PORT_ENV} value '{trimmed}': expected integer 0..=65535"
                ))
            })?;
            opts.callback_bind_port = port;
        }
    }
    Ok(opts)
}

#[cfg(unix)]
fn exec_self_upgrade(target_exe: &std::path::Path) -> std::io::Error {
    use std::os::unix::process::CommandExt;

    let mut cmd = std::process::Command::new(target_exe);
    cmd.args(std::env::args_os().skip(1));
    #[cfg(debug_assertions)]
    cmd.env_remove(TEST_HEALTH_VERSION_ENV);
    cmd.exec()
}

struct AdoptUpgradeController {
    config_path: PathBuf,
    listen_port: u16,
    running_version: String,
    plugin_dir: Option<PathBuf>,
    startup_exe: Option<PathBuf>,
    plugin_poll_interval: Duration,
    cached_ancestor_pid: Option<Option<u32>>,
    last_plugin_poll: Option<Instant>,
    cached_manifest: PluginManifestRead,
    has_seen_valid_manifest: bool,
    probe_cache: Arc<Mutex<BinaryVersionProbeCache>>,
    last_replace_attempt: Option<Instant>,
    replacement_children: Vec<std::process::Child>,
    #[cfg(unix)]
    pending_self_exec: Option<(String, PathBuf)>,
}

impl AdoptUpgradeController {
    fn new(
        config_path: PathBuf,
        listen_port: u16,
        running_version: String,
        startup_exe: Option<PathBuf>,
    ) -> Self {
        Self {
            config_path,
            listen_port,
            running_version,
            plugin_dir: resolve_plugin_dir(),
            startup_exe,
            plugin_poll_interval: resolve_plugin_poll_interval(),
            cached_ancestor_pid: None,
            last_plugin_poll: None,
            cached_manifest: PluginManifestRead::Absent,
            has_seen_valid_manifest: false,
            probe_cache: Arc::new(Mutex::new(BinaryVersionProbeCache::new())),
            last_replace_attempt: None,
            replacement_children: Vec::new(),
            #[cfg(unix)]
            pending_self_exec: None,
        }
    }

    fn reap_children(&mut self) {
        self.replacement_children
            .retain_mut(|child| matches!(child.try_wait(), Ok(None)));
    }

    async fn resolve_watch_pid(&mut self) -> Option<u32> {
        match self.cached_ancestor_pid {
            Some(pid) => pid,
            None => {
                let found =
                    tokio::task::spawn_blocking(crate::process::find_antigravity_ancestor_pid)
                        .await
                        .ok()
                        .flatten();
                self.cached_ancestor_pid = Some(found);
                found
            }
        }
    }

    async fn find_matching_exe(&self, desired_version: &str) -> Option<PathBuf> {
        let plugin_dir = self.plugin_dir.clone();
        let startup_exe = self.startup_exe.clone();
        let desired_for_probe = desired_version.to_string();
        let probe_cache = Arc::clone(&self.probe_cache);
        tokio::task::spawn_blocking(move || {
            let candidates =
                collect_candidate_binaries(plugin_dir.as_deref(), startup_exe.as_deref());
            probe_cache
                .lock()
                .ok()?
                .find_matching_binary(&candidates, &desired_for_probe)
        })
        .await
        .ok()
        .flatten()
    }

    async fn tick(&mut self, active_health_version: &str) -> Option<PathBuf> {
        self.reap_children();

        let plugin_poll_due = self
            .last_plugin_poll
            .is_none_or(|last| last.elapsed() >= self.plugin_poll_interval);
        if plugin_poll_due {
            self.last_plugin_poll = Some(Instant::now());
            if let Some(dir) = self.plugin_dir.clone() {
                let manifest =
                    tokio::task::spawn_blocking(move || read_plugin_manifest_version(&dir))
                        .await
                        .unwrap_or(PluginManifestRead::Invalid);
                match &manifest {
                    PluginManifestRead::Valid(_) => {
                        self.has_seen_valid_manifest = true;
                    }
                    PluginManifestRead::Invalid => {
                        tracing::warn!(
                            plugin_dir = ?self.plugin_dir,
                            active_version = %active_health_version,
                            "plugin.json failed S6 or schema validation; failing closed without spawning replacement"
                        );
                    }
                    PluginManifestRead::Absent if self.has_seen_valid_manifest => {
                        tracing::debug!(
                            plugin_dir = ?self.plugin_dir,
                            active_version = %active_health_version,
                            "plugin.json is transiently absent after previously valid manifest; failing closed"
                        );
                    }
                    PluginManifestRead::Absent => {}
                }
                self.cached_manifest = manifest;
            }
        }

        let desired_version = resolve_desired_version(
            &self.cached_manifest,
            self.has_seen_valid_manifest,
            &self.running_version,
        )
        .map(str::to_string)?;

        if active_health_version == desired_version {
            #[cfg(unix)]
            if desired_version != self.running_version {
                if let Some((ref pending_ver, ref pending_exe)) = self.pending_self_exec
                    && pending_ver == &desired_version
                {
                    return Some(pending_exe.clone());
                }
                if plugin_poll_due
                    && let Some(target_exe) = self.find_matching_exe(&desired_version).await
                {
                    return Some(target_exe);
                }
            }
            return None;
        }
        if self
            .last_replace_attempt
            .is_some_and(|last| last.elapsed() < ADOPT_REPLACE_COOLDOWN)
        {
            return None;
        }

        let target_exe = if desired_version == self.running_version {
            self.startup_exe
                .clone()
                .or_else(|| std::env::current_exe().ok())
        } else {
            if !plugin_poll_due {
                return None;
            }
            self.find_matching_exe(&desired_version).await
        }?;

        let watch_pid = self.resolve_watch_pid().await;
        match spawn_detached_proxy_daemon_with_exe_configured(
            &target_exe,
            &self.config_path,
            DetachedDaemonOptions {
                port: self.listen_port,
                idle_shutdown_secs: DEFAULT_IDLE_SHUTDOWN_SECS,
                watch_pid,
                replace_stale: true,
            },
        ) {
            Ok(child) => {
                tracing::info!(
                    active_version = %active_health_version,
                    desired_version = %desired_version,
                    replacement_exe = %target_exe.display(),
                    replacement_pid = child.id(),
                    ?watch_pid,
                    "detected proxy version mismatch; spawned detached replacement daemon"
                );
                #[cfg(unix)]
                if desired_version != self.running_version {
                    self.pending_self_exec = Some((desired_version, target_exe));
                }
                self.replacement_children.push(child);
                self.last_replace_attempt = Some(Instant::now());
            }
            Err(err) => {
                tracing::warn!(
                    error = %err,
                    active_version = %active_health_version,
                    desired_version = %desired_version,
                    replacement_exe = %target_exe.display(),
                    "failed to spawn detached replacement daemon on version mismatch; continuing to monitor"
                );
                self.last_replace_attempt = Some(Instant::now());
            }
        }
        None
    }
}

#[derive(Default)]
pub struct ServeOptions {
    pub port_override: Option<u16>,
    pub config_override: Option<PathBuf>,
    pub adopt_running: bool,
    pub auto_enable_launcher: bool,
    pub idle_shutdown_secs: Option<u64>,
    pub watch_pid: Option<u32>,
    pub replace_stale: bool,
    pub log_target: Option<ServeLogTarget>,
}

pub async fn run_serve(
    port_override: Option<u16>,
    config_override: Option<PathBuf>,
) -> Result<(), CliError> {
    run_serve_with_options(port_override, config_override, false).await
}

pub async fn run_serve_with_options(
    port_override: Option<u16>,
    config_override: Option<PathBuf>,
    adopt_running: bool,
) -> Result<(), CliError> {
    run_serve_configured(ServeOptions {
        port_override,
        config_override,
        adopt_running,
        auto_enable_launcher: false,
        idle_shutdown_secs: None,
        watch_pid: None,
        replace_stale: false,
        log_target: None,
    })
    .await
}

pub async fn run_serve_configured(options: ServeOptions) -> Result<(), CliError> {
    if options.port_override == Some(0) {
        return Err(CliError::InvalidPort(0));
    }
    if options.adopt_running && options.replace_stale {
        return Err(CliError::Install(
            "--adopt-running and --replace-stale cannot be used together".to_string(),
        ));
    }
    if let Some(pid) = options.watch_pid
        && (pid <= 1 || pid > i32::MAX as u32)
    {
        return Err(CliError::Install(format!(
            "invalid --watch-pid {pid}: expected integer between 2 and {}",
            i32::MAX
        )));
    }

    let config_path = resolve_cli_config_path(options.config_override.as_deref())?;
    let mut config = load_config_checked(&config_path)?;
    if let Some(port) = options.port_override {
        config.proxy.port = NonZeroU16::new(port).ok_or(CliError::InvalidPort(0))?;
    }
    config.validate()?;
    crate::precheck_existing_auth_dir(&config_path)?;

    let usage_dir = config.effective_usage_dir()?;
    precheck_existing_data_dir(&usage_dir)?;
    let usage_db = usage_dir.join(companion_core::USAGE_DB_FILENAME);
    for suffix in ["", "-wal", "-shm", "-journal"] {
        let candidate = if suffix.is_empty() {
            usage_db.clone()
        } else {
            companion_core::fs_safe::append_os_suffix(&usage_db, suffix)
        };
        companion_core::fs_safe::inspect_regular_file_if_exists(&candidate, None)?;
    }
    let runtime_data_dir = resolve_runtime_data_dir(&config)?;
    if precheck_existing_data_dir(&runtime_data_dir)? {
        reject_non_regular_target(
            &runtime_data_dir.join(PROXY_PID_FILENAME),
            "proxy PID files",
        )?;
        reject_non_regular_target(
            &runtime_data_dir.join(PROXY_LOG_FILENAME),
            "proxy log files",
        )?;
        reject_non_regular_target(
            &runtime_data_dir.join(PROXY_LOG_ROTATED_FILENAME),
            "rotated proxy log files",
        )?;
    }

    if options.auto_enable_launcher {
        let auto_cfg_path = config_path.clone();
        match tokio::task::spawn_blocking(move || {
            companion_core::integration::ensure_first_run_launcher_enabled(
                &auto_cfg_path,
                &companion_core::integration::IntegrationOptions::default(),
            )
        })
        .await
        {
            Ok(Ok(outcome)) => {
                tracing::info!(?outcome, "first-run launcher auto-enable completed");
            }
            Ok(Err(err)) => {
                tracing::warn!(
                    error = %err,
                    "first-run launcher auto-enable failed; continuing proxy startup"
                );
            }
            Err(err) => {
                tracing::warn!(
                    error = %err,
                    "first-run launcher auto-enable task panicked; continuing proxy startup"
                );
            }
        }
    }

    let sqlite_sink = Arc::new(SqliteUsageSink::new(usage_dir)?);
    let usage_sink: Arc<dyn UsageSink> = Arc::clone(&sqlite_sink) as Arc<dyn UsageSink>;
    let auth_store: Arc<dyn AuthStore> = Arc::new(FileAuthStore::for_config_path(&config_path)?);
    let codex_auth_options = codex_auth_options_from_env()?;
    let codex_auth = Arc::new(
        CodexAuthManager::with_options(Arc::clone(&auth_store), codex_auth_options)
            .map_err(|err| CliError::Proxy(ProxyError::Provider(err)))?,
    );

    // Pre-register SIGTERM and SIGHUP before binding so host-restart signals are never missed.
    #[cfg(unix)]
    let mut sigterm =
        tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()).ok();
    #[cfg(unix)]
    let mut sighup = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::hangup()).ok();

    let mut signal_shutdown_fut: std::pin::Pin<
        Box<dyn std::future::Future<Output = ()> + Send + 'static>,
    > = Box::pin(async move {
        let ctrl_c = async {
            let _ = tokio::signal::ctrl_c().await;
        };

        #[cfg(unix)]
        let terminate = async {
            if let Some(ref mut stream) = sigterm {
                let _ = stream.recv().await;
            } else {
                std::future::pending::<()>().await;
            }
        };

        #[cfg(not(unix))]
        let terminate = std::future::pending::<()>();

        #[cfg(unix)]
        let hangup = async {
            if let Some(ref mut stream) = sighup {
                let _ = stream.recv().await;
            } else {
                std::future::pending::<()>().await;
            }
        };

        #[cfg(not(unix))]
        let hangup = std::future::pending::<()>();

        tokio::select! {
            _ = ctrl_c => {
                tracing::info!("received Ctrl-C, shutting down proxy");
            }
            _ = terminate => {
                tracing::info!("received SIGTERM, shutting down proxy");
            }
            _ = hangup => {
                tracing::info!("received SIGHUP, shutting down proxy");
            }
        }
    });

    let (sidecar_shutdown_tx, mut sidecar_shutdown_rx) = tokio::sync::watch::channel(false);
    struct SidecarShutdownGuard(tokio::sync::watch::Sender<bool>);
    impl Drop for SidecarShutdownGuard {
        fn drop(&mut self) {
            let _ = self.0.send(true);
        }
    }
    let sidecar_shutdown_guard = SidecarShutdownGuard(sidecar_shutdown_tx);
    let plugin_upgrade_shutdown_rx = sidecar_shutdown_rx.clone();
    let ls_collector = crate::ls_collector::spawn_ls_collector_from_env(
        Arc::clone(&sqlite_sink),
        sidecar_shutdown_rx.clone(),
    );
    #[allow(unused_mut, unused_variables)]
    let mut sidecar_ui_handle =
        if let Some(sidecar_cfg) = crate::sidecar_ui::SidecarUiConfig::from_env() {
            match crate::sidecar_ui::bind_and_spawn_sidecar_ui_with_auth_and_ls(
                config_path.clone(),
                sidecar_cfg,
                Some(Arc::clone(&auth_store)),
                Some(Arc::clone(&codex_auth)),
                ls_collector.clone(),
                async move {
                    let _ = sidecar_shutdown_rx.wait_for(|&stopped| stopped).await;
                },
            )
            .await
            {
                Ok((_, handle)) => Some(handle),
                Err(err) => {
                    tracing::warn!(
                        error = %err,
                        "failed to start sidecar Web UI server; continuing proxy startup"
                    );
                    None
                }
            }
        } else {
            None
        };

    #[cfg(debug_assertions)]
    let health_version_override = resolve_test_health_version_override();
    #[cfg(debug_assertions)]
    let expected_version = health_version_override
        .clone()
        .unwrap_or_else(|| env!("CARGO_PKG_VERSION").to_string());
    #[cfg(not(debug_assertions))]
    let expected_version = env!("CARGO_PKG_VERSION").to_string();

    let proxy_role = resolve_proxy_role(&options);
    let listen_port = config.proxy.port.get();
    let bind_config = config.clone();

    let build_proxy_state = || -> Result<ProxyState, CliError> {
        #[allow(unused_mut)]
        let mut state_opts = ProxyStateOptions {
            config_path: Some(config_path.clone()),
            auth_store: Some(Arc::clone(&auth_store)),
            role: Some(proxy_role.to_string()),
            ..Default::default()
        };
        #[cfg(debug_assertions)]
        {
            state_opts.health_version_override = health_version_override.clone();
        }
        Ok(ProxyState::from_config_with_codex_auth(
            config.clone(),
            Arc::clone(&usage_sink),
            state_opts,
            Arc::clone(&codex_auth) as Arc<dyn CodexAuth>,
        )?)
    };

    // Pre-initialize ProxyState before any `--replace-stale` SIGTERM so TLS roots and
    // HTTP clients are warm before the old daemon drops its listener (~10-20ms handoff).
    let mut prebuilt_state = if !options.adopt_running {
        Some(build_proxy_state()?)
    } else {
        None
    };

    let probe_client = if options.adopt_running || options.replace_stale {
        Some(LoopbackProxyClient::with_default_timeouts()?)
    } else {
        None
    };
    let startup_exe = std::env::current_exe().ok();
    let mut logged_adopt = false;
    let mut takeover_retry_used = false;
    let mut adopt_controller = if options.adopt_running {
        Some(AdoptUpgradeController::new(
            config_path.clone(),
            listen_port,
            expected_version.clone(),
            startup_exe,
        ))
    } else {
        None
    };

    let listener = 'bind_loop: loop {
        if let Some(controller) = adopt_controller.as_mut() {
            controller.reap_children();
        }

        match companion_proxy::bind_loopback_listener(&bind_config).await {
            Ok(listener) => break 'bind_loop listener,
            Err(ProxyError::Bind { addr, source })
                if options.adopt_running && source.kind() == ErrorKind::AddrInUse =>
            {
                let probe = probe_client
                    .as_ref()
                    .expect("adopt_running guarantees LoopbackProxyClient");
                if let Some(health) = probe.probe_healthy_companion(listen_port, None).await {
                    takeover_retry_used = false;
                    if !logged_adopt {
                        tracing::info!(
                            addr = %addr,
                            running_version = %health.version,
                            "detected running agy-companion proxy; monitoring until shutdown or takeover"
                        );
                        logged_adopt = true;
                    }
                    if let Some(controller) = adopt_controller.as_mut()
                        && let Some(target_exe) = controller.tick(&health.version).await
                    {
                        #[cfg(unix)]
                        {
                            tracing::info!(
                                running_version = %expected_version,
                                active_version = %health.version,
                                target_exe = %target_exe.display(),
                                "bootstrap converged to upgraded version; self-execing upgraded sidecar binary"
                            );
                            drop(sidecar_shutdown_guard);
                            if let Some(handle) = sidecar_ui_handle.take() {
                                let _ = tokio::time::timeout(Duration::from_secs(2), handle).await;
                            }
                            let err = exec_self_upgrade(&target_exe);
                            tracing::warn!(
                                error = %err,
                                target_exe = %target_exe.display(),
                                "self-exec of upgraded sidecar binary failed"
                            );
                            return Err(CliError::Io(err));
                        }
                        #[cfg(not(unix))]
                        let _ = target_exe;
                    }
                    tokio::select! {
                        _ = &mut signal_shutdown_fut => {
                            return Ok(());
                        }
                        _ = tokio::time::sleep(ADOPT_RUNNING_POLL_INTERVAL) => {}
                    }
                    continue;
                }
                if logged_adopt && !takeover_retry_used {
                    takeover_retry_used = true;
                    tokio::time::sleep(Duration::from_millis(50)).await;
                    continue;
                }
                return Err(CliError::Proxy(ProxyError::Bind { addr, source }));
            }
            Err(ProxyError::Bind { addr, source })
                if options.replace_stale && source.kind() == ErrorKind::AddrInUse =>
            {
                let probe = probe_client
                    .as_ref()
                    .expect("replace_stale guarantees LoopbackProxyClient");
                match probe
                    .probe_healthy_companion(listen_port, Some(REPLACE_STALE_PROBE_TIMEOUT))
                    .await
                {
                    Some(health) if health.version == expected_version => {
                        tracing::info!(
                            addr = %addr,
                            version = %health.version,
                            "matching-version agy-companion proxy already occupies port; exiting cleanly"
                        );
                        return Ok(());
                    }
                    Some(stale_health) => {
                        if let Some(ref state) = prebuilt_state
                            && let Ok(stale_stats) = probe
                                .probe_stats(listen_port, Some(REPLACE_STALE_PROBE_TIMEOUT))
                                .await
                            && let Some(ts) = stale_stats.cloudcode.last_catalog_refresh_at_ms
                        {
                            state.stats().record_catalog_refresh(
                                ts,
                                stale_stats
                                    .cloudcode
                                    .last_catalog_included_proxy_models
                                    .unwrap_or(false),
                            );
                        }

                        let pid_path = runtime_data_dir.join(PROXY_PID_FILENAME);
                        let mut signal_needed = true;
                        let mut immediate_health_recheck = false;
                        let mut transient_pid_race = false;

                        let pid_info_opt = match read_proxy_pid(&pid_path) {
                            Ok(Some(info)) if info.port == listen_port => {
                                if !info.version.is_empty() && info.version == expected_version {
                                    tracing::info!(
                                        pid = info.pid,
                                        version = %info.version,
                                        "proxy.pid already belongs to matching-version replacement; skipping SIGTERM and waiting for convergence"
                                    );
                                    signal_needed = false;
                                    immediate_health_recheck = true;
                                }
                                Some(info)
                            }
                            Ok(Some(info)) => {
                                tracing::warn!(
                                    pid = info.pid,
                                    pid_port = info.port,
                                    listen_port,
                                    "refusing --replace-stale signal: proxy.pid port does not match target listen port"
                                );
                                transient_pid_race = true;
                                None
                            }
                            Ok(None) => {
                                tracing::warn!(
                                    path = %pid_path.display(),
                                    "refusing --replace-stale signal: proxy.pid is missing"
                                );
                                transient_pid_race = true;
                                None
                            }
                            Err(err) => {
                                tracing::warn!(
                                    error = %err,
                                    path = %pid_path.display(),
                                    "refusing --replace-stale signal: failed to read proxy.pid"
                                );
                                transient_pid_race = true;
                                None
                            }
                        };

                        if signal_needed && let Some(pid_info) = pid_info_opt {
                            let target_pid = pid_info.pid;
                            match tokio::task::spawn_blocking(move || {
                                crate::process::signal_verified_companion_graceful_stop(&pid_info)
                            })
                            .await
                            {
                                Ok(Ok(GracefulStopOutcome::Signaled)) => {
                                    tracing::info!(
                                        stale_pid = target_pid,
                                        stale_version = %stale_health.version,
                                        expected_version = %expected_version,
                                        "signaled stale agy-companion proxy for graceful handoff"
                                    );
                                }
                                Ok(Ok(GracefulStopOutcome::AlreadyDead)) => {
                                    tracing::debug!(
                                        stale_pid = target_pid,
                                        "stale agy-companion proxy already exited before signal"
                                    );
                                }
                                Ok(Ok(GracefulStopOutcome::UnrelatedProcess { reason })) => {
                                    tracing::warn!(
                                        pid = target_pid,
                                        reason = %reason,
                                        "refusing --replace-stale signal: PID verification rejected target"
                                    );
                                    transient_pid_race = true;
                                }
                                Ok(Err(err)) => {
                                    tracing::warn!(
                                        error = %err,
                                        pid = target_pid,
                                        "failed to signal stale agy-companion proxy"
                                    );
                                    return Err(CliError::Proxy(ProxyError::Bind { addr, source }));
                                }
                                Err(err) => {
                                    tracing::warn!(
                                        error = %err,
                                        pid = target_pid,
                                        "blocking graceful stop task panicked"
                                    );
                                    return Err(CliError::Proxy(ProxyError::Bind { addr, source }));
                                }
                            }
                        }

                        if transient_pid_race {
                            // Under concurrent `--replace-stale` races, a peer may have just
                            // replaced the listener or removed/rewritten proxy.pid while the
                            // stale daemon exited. Wait briefly and re-probe `/health` (plus one
                            // bind retry if `/health` is transiently absent) before failing closed.
                            tokio::select! {
                                _ = &mut signal_shutdown_fut => {
                                    return Ok(());
                                }
                                _ = tokio::time::sleep(Duration::from_millis(50)) => {}
                            }
                            match probe
                                .probe_healthy_companion(
                                    listen_port,
                                    Some(REPLACE_STALE_PROBE_TIMEOUT),
                                )
                                .await
                            {
                                Some(health) if health.version == expected_version => {
                                    tracing::info!(
                                        addr = %addr,
                                        version = %health.version,
                                        "concurrent replacement already bound matching version after transient PID race; exiting cleanly"
                                    );
                                    return Ok(());
                                }
                                None => {
                                    match companion_proxy::bind_loopback_listener(&bind_config)
                                        .await
                                    {
                                        Ok(listener) => break 'bind_loop listener,
                                        Err(ProxyError::Bind {
                                            source: retry_source,
                                            ..
                                        }) if retry_source.kind() == ErrorKind::AddrInUse => {
                                            if let Some(health) = probe
                                                .probe_healthy_companion(
                                                    listen_port,
                                                    Some(REPLACE_STALE_PROBE_TIMEOUT),
                                                )
                                                .await
                                                && health.version == expected_version
                                            {
                                                return Ok(());
                                            }
                                            return Err(CliError::Proxy(ProxyError::Bind {
                                                addr,
                                                source,
                                            }));
                                        }
                                        Err(err) => return Err(CliError::Proxy(err)),
                                    }
                                }
                                Some(_) => {
                                    return Err(CliError::Proxy(ProxyError::Bind { addr, source }));
                                }
                            }
                        }

                        let deadline = Instant::now() + REPLACE_STALE_BIND_TIMEOUT;
                        let mut last_health_recheck = if immediate_health_recheck {
                            None
                        } else {
                            Some(Instant::now())
                        };
                        loop {
                            match companion_proxy::bind_loopback_listener(&bind_config).await {
                                Ok(listener) => break 'bind_loop listener,
                                Err(ProxyError::Bind {
                                    source: retry_source,
                                    ..
                                }) if retry_source.kind() == ErrorKind::AddrInUse => {
                                    let recheck_due = last_health_recheck.is_none_or(|last| {
                                        last.elapsed() >= REPLACE_STALE_HEALTH_RECHECK_INTERVAL
                                    });
                                    if recheck_due {
                                        last_health_recheck = Some(Instant::now());
                                        if let Some(health) = probe
                                            .probe_healthy_companion(
                                                listen_port,
                                                Some(REPLACE_STALE_PROBE_TIMEOUT),
                                            )
                                            .await
                                            && health.version == expected_version
                                        {
                                            tracing::info!(
                                                addr = %addr,
                                                version = %health.version,
                                                "concurrent replacement already bound matching version; exiting cleanly"
                                            );
                                            return Ok(());
                                        }
                                    }
                                    if Instant::now() >= deadline {
                                        return Err(CliError::Proxy(ProxyError::Bind {
                                            addr,
                                            source,
                                        }));
                                    }
                                    tokio::select! {
                                        _ = &mut signal_shutdown_fut => {
                                            return Ok(());
                                        }
                                        _ = tokio::time::sleep(REPLACE_STALE_BIND_POLL_INTERVAL) => {}
                                    }
                                }
                                Err(err) => return Err(CliError::Proxy(err)),
                            }
                        }
                    }
                    None => {
                        // Brief retry handles a concurrent handoff gap (~5-15ms) where the old
                        // listener dropped and a peer is binding right now.
                        tokio::select! {
                            _ = &mut signal_shutdown_fut => {
                                return Ok(());
                            }
                            _ = tokio::time::sleep(Duration::from_millis(50)) => {}
                        }
                        match companion_proxy::bind_loopback_listener(&bind_config).await {
                            Ok(listener) => break 'bind_loop listener,
                            Err(ProxyError::Bind {
                                source: retry_source,
                                ..
                            }) if retry_source.kind() == ErrorKind::AddrInUse => {
                                if let Some(health) = probe
                                    .probe_healthy_companion(
                                        listen_port,
                                        Some(REPLACE_STALE_PROBE_TIMEOUT),
                                    )
                                    .await
                                    && health.version == expected_version
                                {
                                    return Ok(());
                                }
                                return Err(CliError::Proxy(ProxyError::Bind { addr, source }));
                            }
                            Err(err) => return Err(CliError::Proxy(err)),
                        }
                    }
                }
            }
            Err(err) => return Err(CliError::Proxy(err)),
        }
    };

    let bound_addr = listener.local_addr().map_err(ProxyError::Serve)?;
    let log_writer = RotatingFileWriter::open(
        runtime_data_dir.join(PROXY_LOG_FILENAME),
        MAX_PROXY_LOG_BYTES,
    )?;
    let _log_guard = options
        .log_target
        .as_ref()
        .map(|target| target.attach(log_writer));
    let pid_guard = write_proxy_pid(
        &runtime_data_dir.join(PROXY_PID_FILENAME),
        &ProxyPidInfo::for_current_process_with_version(
            bound_addr.port(),
            expected_version.clone(),
        ),
    )?;
    let state = match prebuilt_state.take() {
        Some(state) => state,
        None => build_proxy_state()?,
    };

    tracing::info!(
        addr = %bound_addr,
        version = %expected_version,
        role = proxy_role,
        watch_pid = ?options.watch_pid,
        "agy-companion proxy listening"
    );

    let idle_timeout = options
        .idle_shutdown_secs
        .filter(|&secs| secs > 0)
        .map(Duration::from_secs);
    let initial_watched_identity = match options.watch_pid {
        Some(pid) => {
            tokio::task::spawn_blocking(move || crate::process::snapshot_watched_process(pid))
                .await
                .ok()
                .flatten()
        }
        None => None,
    };

    let idle_state = state.clone();
    let watch_state = state.clone();
    let watched_pid_opt = options.watch_pid;
    let graceful_shutdown = async move {
        let idle_fut = async move {
            match idle_timeout {
                None => std::future::pending::<()>().await,
                Some(timeout) => {
                    let poll_interval =
                        (timeout / 40).clamp(Duration::from_millis(25), Duration::from_secs(1));
                    loop {
                        if idle_state.in_flight_requests() == 0
                            && idle_state.idle_duration() >= timeout
                        {
                            tracing::info!(
                                idle_secs = timeout.as_secs(),
                                "proxy idle timeout reached with zero in-flight requests; shutting down"
                            );
                            return;
                        }
                        tokio::time::sleep(poll_interval).await;
                    }
                }
            }
        };

        let watch_fut = async move {
            let Some(watched_pid) = watched_pid_opt else {
                std::future::pending::<()>().await;
                return;
            };
            if let Some(identity) = initial_watched_identity {
                loop {
                    tokio::time::sleep(WATCH_PID_POLL_INTERVAL).await;
                    let id_clone = identity.clone();
                    let alive = tokio::task::spawn_blocking(move || {
                        crate::process::is_watched_process_alive(&id_clone)
                    })
                    .await
                    .unwrap_or(false);
                    if !alive {
                        break;
                    }
                }
            }
            while watch_state.in_flight_requests() > 0 {
                tokio::time::sleep(Duration::from_millis(25)).await;
            }
            tracing::info!(
                watch_pid = watched_pid,
                "watched parent PID exited and in-flight requests drained; shutting down proxy"
            );
        };

        tokio::select! {
            _ = signal_shutdown_fut => {}
            _ = idle_fut => {}
            _ = watch_fut => {}
        }
    };

    #[cfg(unix)]
    let pending_active_exec: Arc<Mutex<Option<(String, PathBuf)>>> = Arc::new(Mutex::new(None));
    let plugin_upgrade_watcher = if let Some(mut controller) = adopt_controller {
        let mut shutdown_rx = plugin_upgrade_shutdown_rx;
        #[cfg(unix)]
        let pending_exec_slot = Arc::clone(&pending_active_exec);
        Some(tokio::spawn(async move {
            let interval = controller.plugin_poll_interval;
            loop {
                let active_version = controller.running_version.clone();
                let _ = controller.tick(&active_version).await;
                #[cfg(unix)]
                if let Some(pending) = controller.pending_self_exec.clone()
                    && let Ok(mut guard) = pending_exec_slot.lock()
                {
                    *guard = Some(pending);
                }
                tokio::select! {
                    _ = shutdown_rx.wait_for(|&stopped| stopped) => break,
                    _ = tokio::time::sleep(interval) => {}
                }
            }
        }))
    } else {
        None
    };

    companion_proxy::serve_on_listener(listener, state, graceful_shutdown).await?;
    drop(pid_guard);
    if let Some(watcher) = plugin_upgrade_watcher {
        watcher.abort();
    }

    #[cfg(unix)]
    if let Some((desired_version, target_exe)) =
        pending_active_exec.lock().ok().and_then(|mut g| g.take())
        && let Some(probe) = probe_client.as_ref()
    {
        let deadline = Instant::now() + Duration::from_secs(1);
        let mut converged = false;
        while Instant::now() < deadline {
            if let Some(health) = probe
                .probe_healthy_companion(listen_port, Some(REPLACE_STALE_PROBE_TIMEOUT))
                .await
                && health.version == desired_version
            {
                converged = true;
                break;
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
        if converged {
            tracing::info!(
                running_version = %expected_version,
                active_version = %desired_version,
                target_exe = %target_exe.display(),
                "active-listener handoff converged; self-execing upgraded sidecar binary"
            );
            drop(sidecar_shutdown_guard);
            if let Some(handle) = sidecar_ui_handle.take() {
                let _ = tokio::time::timeout(Duration::from_secs(2), handle).await;
            }
            let err = exec_self_upgrade(&target_exe);
            tracing::warn!(
                error = %err,
                target_exe = %target_exe.display(),
                "self-exec of upgraded sidecar binary failed"
            );
            return Err(CliError::Io(err));
        }
    }

    #[cfg(not(unix))]
    drop(sidecar_shutdown_guard);

    Ok(())
}
