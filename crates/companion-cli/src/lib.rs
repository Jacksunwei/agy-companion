#![forbid(unsafe_code)]

pub mod cli;
pub mod doctor;
pub mod launch;
pub mod models;

use std::fs;
use std::io::ErrorKind;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use companion_core::{Config, CoreError, JsonlUsageSink, UsageSink};
use companion_proxy::{ProxyError, ProxyState};
use companion_ui_mcp::UiMcpError;
use thiserror::Error;

pub const BINARY_NAME: &str = "agy-companion";
pub use companion_core::MAX_CONFIG_FILE_BYTES;

#[derive(Debug, Error)]
pub enum CliError {
    #[error("{0}")]
    Core(#[from] CoreError),
    #[error("{0}")]
    Proxy(#[from] ProxyError),
    #[error("{0}")]
    UiMcp(#[from] UiMcpError),
    #[error("invalid port {0}: port must be between 1 and 65535")]
    InvalidPort(u16),
    #[error("invalid config file '{path}': {reason}")]
    InvalidConfigFile { path: PathBuf, reason: String },
    #[error("invalid data directory '{path}': {reason}")]
    InvalidDataDir { path: PathBuf, reason: String },
    #[error("failed to serialize output: {0}")]
    Serialization(String),
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),
}

pub(crate) fn normalized_metadata_path(path: &Path) -> PathBuf {
    // Normalizing trailing slashes prevents POSIX lstat(2) from dereferencing a symlink path
    // passed with a trailing separator (e.g. `--data-dir=/tmp/link/`).
    path.components().collect()
}

pub fn resolve_cli_config_path(cli_config: Option<&Path>) -> Result<PathBuf, CoreError> {
    match cli_config {
        Some(path) => {
            companion_core::validate_path(path)?;
            Ok(path.to_path_buf())
        }
        None => companion_core::default_config_path(),
    }
}

pub fn precheck_existing_config_file(path: &Path) -> Result<bool, CliError> {
    let check_path = normalized_metadata_path(path);
    match fs::symlink_metadata(&check_path) {
        Ok(meta) => {
            if meta.file_type().is_symlink() {
                return Err(CliError::InvalidConfigFile {
                    path: path.to_path_buf(),
                    reason: "symbolic links are not allowed for config files".to_string(),
                });
            }
            if !meta.file_type().is_file() {
                return Err(CliError::InvalidConfigFile {
                    path: path.to_path_buf(),
                    reason: "config path must be a regular file".to_string(),
                });
            }
            if meta.len() > MAX_CONFIG_FILE_BYTES {
                return Err(CliError::InvalidConfigFile {
                    path: path.to_path_buf(),
                    reason: format!(
                        "config file exceeds maximum allowed size of {MAX_CONFIG_FILE_BYTES} bytes"
                    ),
                });
            }
            Ok(true)
        }
        Err(err) if err.kind() == ErrorKind::NotFound => Ok(false),
        Err(err) => Err(CliError::Io(err)),
    }
}

pub fn precheck_existing_data_dir(path: &Path) -> Result<bool, CliError> {
    let check_path = normalized_metadata_path(path);
    match fs::symlink_metadata(&check_path) {
        Ok(meta) => {
            if meta.file_type().is_symlink() {
                return Err(CliError::InvalidDataDir {
                    path: path.to_path_buf(),
                    reason: "symbolic links are not allowed for data directories".to_string(),
                });
            }
            if !meta.file_type().is_dir() {
                return Err(CliError::InvalidDataDir {
                    path: path.to_path_buf(),
                    reason: "data directory path must be a directory".to_string(),
                });
            }
            Ok(true)
        }
        Err(err) if err.kind() == ErrorKind::NotFound => Ok(false),
        Err(err) => Err(CliError::Io(err)),
    }
}

pub fn load_config_checked(path: &Path) -> Result<Config, CliError> {
    companion_core::validate_path(path)?;
    precheck_existing_config_file(path)?;
    Ok(Config::load_or_default(path)?)
}

pub fn validate_ui_mcp_paths(
    config_override: Option<&Path>,
    data_dir: Option<&Path>,
) -> Result<(PathBuf, Option<PathBuf>), CoreError> {
    let config_path = resolve_cli_config_path(config_override)?;
    let validated_data_dir = match data_dir {
        Some(dir) => {
            companion_core::validate_path(dir)?;
            Some(dir.to_path_buf())
        }
        None => None,
    };
    Ok((config_path, validated_data_dir))
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
    if port_override == Some(0) {
        return Err(CliError::InvalidPort(0));
    }

    let config_path = resolve_cli_config_path(config_override.as_deref())?;
    let mut config = load_config_checked(&config_path)?;
    if let Some(port) = port_override {
        config.listen_port = port;
    }
    config.validate()?;

    let usage_dir = config.effective_usage_dir()?;
    precheck_existing_data_dir(&usage_dir)?;
    let usage_sink: Arc<dyn UsageSink> = Arc::new(JsonlUsageSink::new(usage_dir)?);

    // Pre-register SIGTERM and SIGHUP before binding so host-restart signals are never missed.
    #[cfg(unix)]
    let mut sigterm =
        tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()).ok();
    #[cfg(unix)]
    let mut sighup = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::hangup()).ok();

    let mut shutdown_fut: std::pin::Pin<Box<dyn std::future::Future<Output = ()> + Send>> =
        Box::pin(async move {
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

    let mut logged_adopt = false;
    let listener = loop {
        match companion_proxy::bind_loopback_listener(&config).await {
            Ok(listener) => break listener,
            Err(ProxyError::Bind { addr, source })
                if adopt_running && source.kind() == ErrorKind::AddrInUse =>
            {
                if let Some(health) =
                    launch::probe_running_companion_proxy(config.listen_port).await
                {
                    if !logged_adopt {
                        tracing::info!(
                            addr = %addr,
                            running_version = %health.version,
                            "detected running agy-companion proxy; monitoring until shutdown or takeover"
                        );
                        logged_adopt = true;
                    }
                    tokio::select! {
                        _ = &mut shutdown_fut => {
                            return Ok(());
                        }
                        _ = tokio::time::sleep(std::time::Duration::from_secs(5)) => {}
                    }
                    continue;
                }
                return Err(CliError::Proxy(ProxyError::Bind { addr, source }));
            }
            Err(err) => return Err(CliError::Proxy(err)),
        }
    };

    let bound_addr = listener.local_addr().map_err(ProxyError::Serve)?;
    let state = ProxyState::from_config(config, Some(config_path), usage_sink)?;

    tracing::info!(
        addr = %bound_addr,
        version = env!("CARGO_PKG_VERSION"),
        "agy-companion proxy listening"
    );

    companion_proxy::serve_on_listener(listener, state, shutdown_fut).await?;
    Ok(())
}

pub async fn run_ui_mcp(
    data_dir: Option<PathBuf>,
    config_override: Option<PathBuf>,
) -> Result<(), CliError> {
    let (config_path, validated_data_dir) =
        validate_ui_mcp_paths(config_override.as_deref(), data_dir.as_deref())?;
    precheck_existing_config_file(&config_path)?;
    if let Some(ref dir) = validated_data_dir {
        precheck_existing_data_dir(dir)?;
    }
    companion_ui_mcp::serve_stdio(config_path, validated_data_dir).await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::path::{Path, PathBuf};

    use companion_core::CoreError;
    use tempfile::TempDir;

    use crate::{
        CliError, precheck_existing_data_dir, resolve_cli_config_path, validate_ui_mcp_paths,
    };

    #[test]
    fn cli_rejects_path_traversal_in_config_and_data_dir() {
        let cfg_err = resolve_cli_config_path(Some(Path::new("../escape.json")))
            .expect_err("traversal in --config must fail");
        assert!(matches!(cfg_err, CoreError::InvalidPath { .. }));

        let data_err = validate_ui_mcp_paths(
            Some(Path::new("/tmp/valid-config.json")),
            Some(Path::new("../escape")),
        )
        .expect_err("traversal in --data-dir must fail");
        assert!(matches!(data_err, CoreError::InvalidPath { .. }));

        let empty_data_err = validate_ui_mcp_paths(
            Some(Path::new("/tmp/valid-config.json")),
            Some(Path::new("")),
        )
        .expect_err("empty --data-dir must fail");
        assert!(matches!(empty_data_err, CoreError::InvalidPath { .. }));
    }

    #[test]
    fn precheck_existing_data_dir_allows_missing_and_real_dir_rejects_file_and_symlink() {
        let temp = TempDir::new().expect("tempdir");

        // Nonexistent path is allowed (first-run UX).
        let missing = temp.path().join("missing-dir");
        assert!(!precheck_existing_data_dir(&missing).expect("missing"));

        // Existing real directory is allowed.
        let real_dir = temp.path().join("real-dir");
        fs::create_dir(&real_dir).expect("mkdir");
        assert!(precheck_existing_data_dir(&real_dir).expect("real dir"));

        // Regular file is rejected.
        let file = temp.path().join("not-a-dir");
        fs::write(&file, "x").expect("write");
        assert!(matches!(
            precheck_existing_data_dir(&file),
            Err(CliError::InvalidDataDir { .. })
        ));

        #[cfg(unix)]
        {
            use std::os::unix::fs::symlink;

            let dir_link = temp.path().join("link-dir");
            symlink(&real_dir, &dir_link).expect("symlink dir");
            assert!(matches!(
                precheck_existing_data_dir(&dir_link),
                Err(CliError::InvalidDataDir { .. })
            ));

            let dir_link_slash = PathBuf::from(format!("{}/", dir_link.display()));
            assert!(matches!(
                precheck_existing_data_dir(&dir_link_slash),
                Err(CliError::InvalidDataDir { .. })
            ));

            let dangling_link = temp.path().join("dangling-dir");
            symlink(temp.path().join("no-such-target"), &dangling_link).expect("dangling symlink");
            assert!(matches!(
                precheck_existing_data_dir(&dangling_link),
                Err(CliError::InvalidDataDir { .. })
            ));
        }
    }
}
