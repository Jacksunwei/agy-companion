#![forbid(unsafe_code)]

pub mod cli;
pub mod daemon;
pub mod doctor;
pub mod hook_env;
pub mod install;
pub mod integration;
pub mod launch;
pub mod models;
pub mod process;
pub mod serve;

use std::path::{Path, PathBuf};

use companion_core::fs_safe;
use companion_core::{Config, CoreError, FsError};
use companion_providers::ProbeError;
use companion_proxy::ProxyError;
use companion_ui_mcp::UiMcpError;
use thiserror::Error;

pub const BINARY_NAME: &str = "agy-companion";
pub use companion_core::MAX_CONFIG_FILE_BYTES;
pub use serve::{
    ADOPT_RUNNING_POLL_INTERVAL, ServeOptions, run_serve, run_serve_configured,
    run_serve_with_options,
};

#[derive(Debug, Error)]
pub enum CliError {
    #[error("{0}")]
    Core(#[from] CoreError),
    #[error("{0}")]
    Probe(#[from] ProbeError),
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
    #[error("{0}")]
    Install(String),
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),
}

impl From<FsError> for CliError {
    fn from(err: FsError) -> Self {
        Self::Core(CoreError::Fs(err))
    }
}

pub(crate) fn normalized_metadata_path(path: &Path) -> PathBuf {
    fs_safe::normalize_path(path)
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
    match fs_safe::inspect_regular_file_if_exists(path, Some(MAX_CONFIG_FILE_BYTES)) {
        Ok(Some(_)) => Ok(true),
        Ok(None) => Ok(false),
        Err(FsError::SymlinkRejected { .. }) => Err(CliError::InvalidConfigFile {
            path: path.to_path_buf(),
            reason: "symbolic links are not allowed for config files".to_string(),
        }),
        Err(FsError::NotRegularFile { .. }) => Err(CliError::InvalidConfigFile {
            path: path.to_path_buf(),
            reason: "config path must be a regular file".to_string(),
        }),
        Err(FsError::SizeLimitExceeded { max_bytes, .. }) => Err(CliError::InvalidConfigFile {
            path: path.to_path_buf(),
            reason: format!("config file exceeds maximum allowed size of {max_bytes} bytes"),
        }),
        Err(FsError::Io { source, .. }) => Err(CliError::Io(source)),
        Err(err) => Err(CliError::from(err)),
    }
}

pub fn precheck_existing_data_dir(path: &Path) -> Result<bool, CliError> {
    match fs_safe::inspect_existing_dir(path) {
        Ok(Some(_)) => Ok(true),
        Ok(None) => Ok(false),
        Err(FsError::SymlinkRejected { .. }) => Err(CliError::InvalidDataDir {
            path: path.to_path_buf(),
            reason: "symbolic links are not allowed for data directories".to_string(),
        }),
        Err(FsError::NotDirectory { .. }) => Err(CliError::InvalidDataDir {
            path: path.to_path_buf(),
            reason: "data directory path must be a directory".to_string(),
        }),
        Err(FsError::Io { source, .. }) => Err(CliError::Io(source)),
        Err(err) => Err(CliError::from(err)),
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

pub async fn run_ui_mcp(
    data_dir: Option<PathBuf>,
    config_override: Option<PathBuf>,
) -> Result<(), CliError> {
    run_ui_mcp_with_options(data_dir, config_override, false).await
}

pub async fn run_ui_mcp_with_options(
    data_dir: Option<PathBuf>,
    config_override: Option<PathBuf>,
    auto_enable_launcher: bool,
) -> Result<(), CliError> {
    let (config_path, validated_data_dir) =
        validate_ui_mcp_paths(config_override.as_deref(), data_dir.as_deref())?;
    precheck_existing_config_file(&config_path)?;
    if let Some(ref dir) = validated_data_dir {
        precheck_existing_data_dir(dir)?;
    }
    let options = companion_ui_mcp::UiServerOptions {
        auto_enable_launcher,
        ..Default::default()
    };
    companion_ui_mcp::serve_stdio_with_options(config_path, options).await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::path::{Path, PathBuf};

    use companion_core::{CoreError, FsError};
    use tempfile::TempDir;

    use crate::{
        CliError, precheck_existing_data_dir, resolve_cli_config_path, validate_ui_mcp_paths,
    };

    #[test]
    fn cli_rejects_path_traversal_in_config_and_data_dir() {
        let cfg_err = resolve_cli_config_path(Some(Path::new("../escape.json")))
            .expect_err("traversal in --config must fail");
        assert!(matches!(
            cfg_err,
            CoreError::Fs(FsError::ParentDirTraversal)
        ));

        let data_err = validate_ui_mcp_paths(
            Some(Path::new("/tmp/valid-config.json")),
            Some(Path::new("../escape")),
        )
        .expect_err("traversal in --data-dir must fail");
        assert!(matches!(
            data_err,
            CoreError::Fs(FsError::ParentDirTraversal)
        ));

        let empty_data_err = validate_ui_mcp_paths(
            Some(Path::new("/tmp/valid-config.json")),
            Some(Path::new("")),
        )
        .expect_err("empty --data-dir must fail");
        assert!(matches!(empty_data_err, CoreError::Fs(FsError::EmptyPath)));
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
