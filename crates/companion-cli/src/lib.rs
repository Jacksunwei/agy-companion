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
pub mod sidecar_ui;

use std::path::{Path, PathBuf};

use companion_core::fs_safe;
use companion_core::{Config, CoreError, FsError};
use companion_providers::ProbeError;
use companion_proxy::ProxyError;
use companion_ui::UiError;
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
    Ui(#[from] UiError),
    #[error("invalid port {0}: port must be between 1 and 65535")]
    InvalidPort(u16),
    #[error("invalid config file '{path}': {reason}")]
    InvalidConfigFile { path: PathBuf, reason: String },
    #[error("invalid auth path '{path}': {reason}")]
    InvalidAuthPath { path: PathBuf, reason: String },
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

pub fn precheck_existing_auth_dir(config_path: &Path) -> Result<(), CliError> {
    let auth_dir = companion_core::auth_dir_for_config_path(config_path)?;
    match fs_safe::inspect_existing_dir(&auth_dir) {
        Ok(None) => return Ok(()),
        Ok(Some(_)) => {}
        Err(FsError::SymlinkRejected { .. }) => {
            return Err(CliError::InvalidAuthPath {
                path: auth_dir,
                reason: "symbolic links are not allowed for auth directories".to_string(),
            });
        }
        Err(FsError::NotDirectory { .. }) => {
            return Err(CliError::InvalidAuthPath {
                path: auth_dir,
                reason: "auth path must be a directory".to_string(),
            });
        }
        Err(FsError::Io { source, .. }) => return Err(CliError::Io(source)),
        Err(err) => return Err(CliError::from(err)),
    }

    for filename in [
        companion_core::GEMINI_AUTH_FILENAME,
        companion_core::CODEX_AUTH_FILENAME,
        companion_core::CODEX_REFRESH_LOCK_FILENAME,
    ] {
        let candidate = auth_dir.join(filename);
        match fs_safe::inspect_regular_file_if_exists(
            &candidate,
            Some(companion_core::MAX_AUTH_FILE_BYTES),
        ) {
            Ok(None) | Ok(Some(_)) => {}
            Err(FsError::SymlinkRejected { .. }) => {
                return Err(CliError::InvalidAuthPath {
                    path: candidate,
                    reason: "symbolic links are not allowed for auth files".to_string(),
                });
            }
            Err(FsError::NotRegularFile { .. }) => {
                return Err(CliError::InvalidAuthPath {
                    path: candidate,
                    reason: "auth file path must be a regular file".to_string(),
                });
            }
            Err(FsError::SizeLimitExceeded { max_bytes, .. }) => {
                return Err(CliError::InvalidAuthPath {
                    path: candidate,
                    reason: format!("auth file exceeds maximum allowed size of {max_bytes} bytes"),
                });
            }
            Err(FsError::Io { source, .. }) => return Err(CliError::Io(source)),
            Err(err) => return Err(CliError::from(err)),
        }
    }
    Ok(())
}

pub fn load_config_checked(path: &Path) -> Result<Config, CliError> {
    companion_core::validate_path(path)?;
    precheck_existing_config_file(path)?;
    Ok(Config::load_or_default(path)?)
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::path::Path;
    #[cfg(unix)]
    use std::path::PathBuf;

    use companion_core::{CoreError, FsError};
    use tempfile::TempDir;

    use crate::{CliError, precheck_existing_data_dir, resolve_cli_config_path};

    #[test]
    fn cli_rejects_path_traversal_in_config() {
        let cfg_err = resolve_cli_config_path(Some(Path::new("../escape.json")))
            .expect_err("traversal in --config must fail");
        assert!(matches!(
            cfg_err,
            CoreError::Fs(FsError::ParentDirTraversal)
        ));
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
