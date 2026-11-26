use std::fs::{File, TryLockError};
use std::path::{Path, PathBuf};

use crate::config::CoreError;
use crate::fs_safe::{self, OpenRegularMode};

use super::fs_inspect::validate_data_dir_chain;

pub const INTEGRATION_LOCK_FILENAME: &str = "integration.lock";

/// Exclusive cross-process advisory lock over `<data_dir>/integration.lock` (`0600`).
///
/// `Drop` explicitly unlocks the open file description before closing the file descriptor
/// so concurrently spawning child processes cannot keep the `flock(2)` lock held across
/// `fork()`/`exec()`. The lock file is intentionally never unlinked on drop to avoid
/// classic flock-unlink inode races.
#[derive(Debug)]
pub struct IntegrationLock {
    file: File,
    path: PathBuf,
}

impl IntegrationLock {
    pub fn acquire(data_dir: &Path) -> Result<Self, CoreError> {
        let home = fs_safe::default_home_dir();
        Self::acquire_with_home(data_dir, home.as_deref())
    }

    pub fn try_acquire(data_dir: &Path) -> Result<Option<Self>, CoreError> {
        let home = fs_safe::default_home_dir();
        Self::try_acquire_with_home(data_dir, home.as_deref())
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    #[cfg(all(test, unix))]
    pub(super) fn try_clone_file(&self) -> std::io::Result<File> {
        self.file.try_clone()
    }

    pub(super) fn acquire_with_home(
        data_dir: &Path,
        home: Option<&Path>,
    ) -> Result<Self, CoreError> {
        let (file, lock_path) = open_lock_file(data_dir, home)?;
        file.lock().map_err(|source| CoreError::Io {
            path: lock_path.clone(),
            source,
        })?;
        Ok(Self {
            file,
            path: lock_path,
        })
    }

    pub(super) fn try_acquire_with_home(
        data_dir: &Path,
        home: Option<&Path>,
    ) -> Result<Option<Self>, CoreError> {
        let (file, lock_path) = open_lock_file(data_dir, home)?;
        match file.try_lock() {
            Ok(()) => Ok(Some(Self {
                file,
                path: lock_path,
            })),
            Err(TryLockError::WouldBlock) => Ok(None),
            Err(TryLockError::Error(source)) => Err(CoreError::Io {
                path: lock_path,
                source,
            }),
        }
    }
}

impl Drop for IntegrationLock {
    fn drop(&mut self) {
        // Explicitly unlock the open file description before closing the fd so a
        // concurrently spawning child process inheriting the fd during fork/exec
        // cannot keep the flock(2) lock held past Drop. Never unlink `self.path`.
        let _ = self.file.unlock();
    }
}

fn open_lock_file(data_dir: &Path, home: Option<&Path>) -> Result<(File, PathBuf), CoreError> {
    fs_safe::validate_path(data_dir)?;
    validate_data_dir_chain(data_dir, home)?;
    fs_safe::ensure_dir_no_symlink(data_dir, 0o700)?;

    let lock_path = data_dir.join(INTEGRATION_LOCK_FILENAME);
    let (file, _meta) =
        fs_safe::open_verified_regular_file_0600(&lock_path, OpenRegularMode::ReadWriteCreate)?;
    Ok((file, lock_path))
}
