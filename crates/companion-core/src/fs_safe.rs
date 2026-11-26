use std::ffi::OsString;
use std::fs::{self, File};
use std::io::{self, ErrorKind, Read, Write};
use std::path::{Component, Path, PathBuf};
use std::time::{Duration, SystemTime};

use thiserror::Error;
use uuid::Uuid;

pub const WINDOWS_RENAME_MAX_ATTEMPTS: usize = 3;

pub const WINDOWS_RENAME_BACKOFFS: [Duration; WINDOWS_RENAME_MAX_ATTEMPTS - 1] =
    [Duration::from_millis(25), Duration::from_millis(50)];

#[derive(Debug, Error)]
pub enum FsError {
    #[error("invalid path: path must not be empty")]
    EmptyPath,
    #[error("invalid path: path must not contain NUL bytes")]
    ContainsNul,
    #[error("invalid path: path must not contain parent directory ('..') traversal")]
    ParentDirTraversal,
    #[error("symbolic links are not allowed for '{path}'")]
    SymlinkRejected { path: PathBuf },
    #[error("path '{path}' must be a regular file")]
    NotRegularFile { path: PathBuf },
    #[error("path '{path}' must be a directory")]
    NotDirectory { path: PathBuf },
    #[error("file '{path}' exceeds maximum allowed size of {max_bytes} bytes")]
    SizeLimitExceeded { path: PathBuf, max_bytes: u64 },
    #[error("path '{path}' changed concurrently during open (TOCTOU detected)")]
    ConcurrentModification { path: PathBuf },
    #[error("file '{path}' is not valid UTF-8")]
    InvalidUtf8 { path: PathBuf },
    #[error("target path '{path}' must include a valid UTF-8 file name")]
    InvalidFileName { path: PathBuf },
    #[error("I/O error at '{path}': {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: io::Error,
    },
}

/// Strips trailing separators and redundant `.` components so POSIX `lstat(2)` inspects
/// the entry itself without dereferencing a trailing-slash symlink path.
pub fn normalize_path(path: &Path) -> PathBuf {
    path.components().collect()
}

/// Rejects empty/whitespace paths, embedded NUL bytes, and `..` parent directory traversal.
pub fn validate_path(path: &Path) -> Result<(), FsError> {
    if path.as_os_str().is_empty() || path.to_str().is_some_and(|s| s.trim().is_empty()) {
        return Err(FsError::EmptyPath);
    }

    #[cfg(unix)]
    {
        use std::os::unix::ffi::OsStrExt;
        if path.as_os_str().as_bytes().contains(&0) {
            return Err(FsError::ContainsNul);
        }
    }
    #[cfg(not(unix))]
    {
        if path.to_string_lossy().contains('\0') {
            return Err(FsError::ContainsNul);
        }
    }

    if path
        .components()
        .any(|component| matches!(component, Component::ParentDir))
    {
        return Err(FsError::ParentDirTraversal);
    }

    Ok(())
}

/// Appends `suffix` directly onto the underlying `OsString` (e.g. `usage.db` -> `usage.db-wal`).
pub fn append_os_suffix(path: &Path, suffix: &str) -> PathBuf {
    let mut os = OsString::from(path.as_os_str());
    os.push(suffix);
    PathBuf::from(os)
}

/// Generates a random collision-resistant suffix for sibling temporary files or staging directories.
pub fn unique_temp_suffix() -> String {
    Uuid::new_v4().to_string()
}

pub fn default_home_dir() -> Option<PathBuf> {
    dirs::home_dir()
}

pub fn default_platform_config_dir() -> Option<PathBuf> {
    dirs::config_dir()
}

pub fn default_platform_data_local_dir() -> Option<PathBuf> {
    dirs::data_local_dir()
}

/// RAII cleanup guard for temporary files or staging directories.
#[derive(Debug)]
pub struct TempPathGuard {
    path: Option<PathBuf>,
    is_dir: bool,
}

impl TempPathGuard {
    pub fn file(path: PathBuf) -> Self {
        Self {
            path: Some(path),
            is_dir: false,
        }
    }

    pub fn dir(path: PathBuf) -> Self {
        Self {
            path: Some(path),
            is_dir: true,
        }
    }

    pub fn path(&self) -> Option<&Path> {
        self.path.as_deref()
    }

    pub fn disarm(&mut self) {
        self.path = None;
    }
}

impl Drop for TempPathGuard {
    fn drop(&mut self) {
        if let Some(ref path) = self.path {
            if self.is_dir {
                let _ = fs::remove_dir_all(path);
            } else {
                let _ = fs::remove_file(path);
            }
        }
    }
}

/// Retries a rename operation on transient `ErrorKind::PermissionDenied` using bounded backoffs.
pub fn retry_rename_with_backoff<F, S>(
    backoffs: &[Duration],
    mut rename_op: F,
    mut sleep_op: S,
) -> io::Result<()>
where
    F: FnMut() -> io::Result<()>,
    S: FnMut(Duration),
{
    let mut attempt = 0;
    loop {
        match rename_op() {
            Ok(()) => return Ok(()),
            Err(err) if err.kind() == ErrorKind::PermissionDenied && attempt < backoffs.len() => {
                sleep_op(backoffs[attempt]);
                attempt += 1;
            }
            Err(err) => return Err(err),
        }
    }
}

/// Renames `from` -> `to`, applying bounded Windows `PermissionDenied` retry backoff on Windows.
pub fn rename_with_retry(from: &Path, to: &Path) -> Result<(), FsError> {
    validate_path(from)?;
    validate_path(to)?;
    #[cfg(windows)]
    let res = retry_rename_with_backoff(
        &WINDOWS_RENAME_BACKOFFS,
        || fs::rename(from, to),
        std::thread::sleep,
    );
    #[cfg(not(windows))]
    let res = fs::rename(from, to);

    res.map_err(|source| FsError::Io {
        path: to.to_path_buf(),
        source,
    })
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ParentDirPolicy {
    /// Allow existing symlink parent dir (e.g. `/var` -> `/private/var`), create missing with `dir_mode`.
    AllowSymlink,
    /// Reject symlink parent dir, create missing with `dir_mode`, do not chmod existing parent.
    RejectSymlink,
    /// Reject symlink parent dir, create missing with `dir_mode`, and tighten parent mode to `dir_mode` on Unix.
    RejectSymlinkAndTighten,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TargetFilePolicy {
    /// Reject symlink targets (including trailing-slash symlinks); non-symlink targets use `fs::rename` semantics.
    RejectSymlink,
    /// Reject symlink targets AND existing non-regular entries (directories, FIFOs, sockets, devices).
    RejectSymlinkAndNonRegular,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AtomicWriteOptions {
    pub file_mode: u32,
    pub dir_mode: u32,
    pub parent_policy: ParentDirPolicy,
    pub target_policy: TargetFilePolicy,
}

impl AtomicWriteOptions {
    pub const fn secure() -> Self {
        Self {
            file_mode: 0o600,
            dir_mode: 0o700,
            parent_policy: ParentDirPolicy::AllowSymlink,
            target_policy: TargetFilePolicy::RejectSymlink,
        }
    }
}

/// Atomically writes `payload` to `path` using a sibling temporary file, `sync_all()`,
/// explicit Unix permissions, and atomic rename with RAII cleanup on any failure.
pub fn write_atomic(path: &Path, payload: &[u8], opts: AtomicWriteOptions) -> Result<(), FsError> {
    #[cfg(not(unix))]
    let _ = (opts.file_mode, opts.dir_mode);

    validate_path(path)?;

    let check_path = normalize_path(path);
    match fs::symlink_metadata(&check_path) {
        Ok(meta) => {
            if meta.file_type().is_symlink() {
                return Err(FsError::SymlinkRejected {
                    path: path.to_path_buf(),
                });
            }
            if opts.target_policy == TargetFilePolicy::RejectSymlinkAndNonRegular
                && !meta.file_type().is_file()
            {
                return Err(FsError::NotRegularFile {
                    path: path.to_path_buf(),
                });
            }
        }
        Err(err) if err.kind() == ErrorKind::NotFound => {}
        Err(source) => {
            return Err(FsError::Io {
                path: path.to_path_buf(),
                source,
            });
        }
    }

    let file_name = path
        .file_name()
        .and_then(|name| name.to_str())
        .ok_or_else(|| FsError::InvalidFileName {
            path: path.to_path_buf(),
        })?;

    let parent = path
        .parent()
        .filter(|dir| !dir.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));

    let parent_check = normalize_path(parent);
    let allow_symlink_parent = opts.parent_policy == ParentDirPolicy::AllowSymlink;

    match fs::symlink_metadata(&parent_check) {
        Ok(meta) => {
            if !allow_symlink_parent && meta.file_type().is_symlink() {
                return Err(FsError::SymlinkRejected {
                    path: parent.to_path_buf(),
                });
            }
            if !(meta.file_type().is_dir()
                || (allow_symlink_parent && meta.file_type().is_symlink()))
            {
                return Err(FsError::NotDirectory {
                    path: parent.to_path_buf(),
                });
            }
            #[cfg(unix)]
            if opts.parent_policy == ParentDirPolicy::RejectSymlinkAndTighten {
                use std::os::unix::fs::PermissionsExt;
                fs::set_permissions(&parent_check, fs::Permissions::from_mode(opts.dir_mode))
                    .map_err(|source| FsError::Io {
                        path: parent.to_path_buf(),
                        source,
                    })?;
            }
        }
        Err(err) if err.kind() == ErrorKind::NotFound => {
            let mut dir_builder = fs::DirBuilder::new();
            dir_builder.recursive(true);
            #[cfg(unix)]
            {
                use std::os::unix::fs::DirBuilderExt;
                dir_builder.mode(opts.dir_mode);
            }
            dir_builder.create(parent).map_err(|source| FsError::Io {
                path: parent.to_path_buf(),
                source,
            })?;

            if !allow_symlink_parent {
                let created_meta =
                    fs::symlink_metadata(&parent_check).map_err(|source| FsError::Io {
                        path: parent.to_path_buf(),
                        source,
                    })?;
                if created_meta.file_type().is_symlink() {
                    return Err(FsError::SymlinkRejected {
                        path: parent.to_path_buf(),
                    });
                }
                if !created_meta.file_type().is_dir() {
                    return Err(FsError::NotDirectory {
                        path: parent.to_path_buf(),
                    });
                }
            }

            #[cfg(unix)]
            if opts.parent_policy == ParentDirPolicy::RejectSymlinkAndTighten {
                use std::os::unix::fs::PermissionsExt;
                fs::set_permissions(&parent_check, fs::Permissions::from_mode(opts.dir_mode))
                    .map_err(|source| FsError::Io {
                        path: parent.to_path_buf(),
                        source,
                    })?;
            }
        }
        Err(source) => {
            return Err(FsError::Io {
                path: parent.to_path_buf(),
                source,
            });
        }
    }

    let tmp_path = parent.join(format!(".{file_name}.tmp.{}", unique_temp_suffix()));

    let mut open_opts = fs::OpenOptions::new();
    open_opts.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        open_opts.mode(opts.file_mode);
    }

    let mut file = open_opts.open(&tmp_path).map_err(|source| FsError::Io {
        path: tmp_path.clone(),
        source,
    })?;
    let mut tmp_guard = TempPathGuard::file(tmp_path.clone());

    let write_result = (|| -> io::Result<()> {
        file.write_all(payload)?;
        file.sync_all()?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            file.set_permissions(fs::Permissions::from_mode(opts.file_mode))?;
        }
        Ok(())
    })();
    drop(file);

    if let Err(source) = write_result {
        return Err(FsError::Io {
            path: tmp_path,
            source,
        });
    }

    rename_with_retry(&tmp_path, path)?;
    tmp_guard.disarm();
    Ok(())
}

/// Atomically writes `payload` to `path` with mode `0600` (and `0700` parent dir on Unix),
/// rejecting path traversal, NUL bytes, and symlink targets fail-closed.
pub fn write_atomic_secure(path: &Path, payload: &[u8]) -> Result<(), FsError> {
    write_atomic(path, payload, AtomicWriteOptions::secure())
}

/// Inspects an existing entry via `lstat(2)` on a normalized path, rejecting symlinks,
/// non-regular files, and optionally files exceeding `max_bytes`. Returns `Ok(None)` if missing.
pub fn inspect_regular_file_if_exists(
    path: &Path,
    max_bytes: Option<u64>,
) -> Result<Option<fs::Metadata>, FsError> {
    validate_path(path)?;
    let check_path = normalize_path(path);
    match fs::symlink_metadata(&check_path) {
        Ok(meta) => {
            let file_type = meta.file_type();
            if file_type.is_symlink() {
                return Err(FsError::SymlinkRejected {
                    path: path.to_path_buf(),
                });
            }
            if !file_type.is_file() {
                return Err(FsError::NotRegularFile {
                    path: path.to_path_buf(),
                });
            }
            if let Some(limit) = max_bytes
                && meta.len() > limit
            {
                return Err(FsError::SizeLimitExceeded {
                    path: path.to_path_buf(),
                    max_bytes: limit,
                });
            }
            Ok(Some(meta))
        }
        Err(err) if err.kind() == ErrorKind::NotFound => Ok(None),
        Err(source) => Err(FsError::Io {
            path: path.to_path_buf(),
            source,
        }),
    }
}

/// Inspects an existing directory via `lstat(2)` on a normalized path, rejecting symlinks
/// and non-directories. Returns `Ok(None)` if missing.
pub fn inspect_existing_dir(dir: &Path) -> Result<Option<fs::Metadata>, FsError> {
    validate_path(dir)?;
    let check_dir = normalize_path(dir);
    match fs::symlink_metadata(&check_dir) {
        Ok(meta) => {
            if meta.file_type().is_symlink() {
                return Err(FsError::SymlinkRejected {
                    path: dir.to_path_buf(),
                });
            }
            if !meta.file_type().is_dir() {
                return Err(FsError::NotDirectory {
                    path: dir.to_path_buf(),
                });
            }
            Ok(Some(meta))
        }
        Err(err) if err.kind() == ErrorKind::NotFound => Ok(None),
        Err(source) => Err(FsError::Io {
            path: dir.to_path_buf(),
            source,
        }),
    }
}

/// Classification of the final path component returned by [`inspect_leaf_kind`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PathLeafKind {
    Missing,
    RegularFile,
    Directory,
    Symlink,
    Other,
}

/// Classifies the final path component via `lstat(2)` on a normalized path without
/// dereferencing symlinks. Enforces `validate_path(path)` fail-closed.
pub fn inspect_leaf_kind(path: &Path) -> Result<PathLeafKind, FsError> {
    validate_path(path)?;
    let check_path = normalize_path(path);
    match fs::symlink_metadata(&check_path) {
        Ok(meta) => {
            let file_type = meta.file_type();
            if file_type.is_symlink() {
                Ok(PathLeafKind::Symlink)
            } else if file_type.is_file() {
                Ok(PathLeafKind::RegularFile)
            } else if file_type.is_dir() {
                Ok(PathLeafKind::Directory)
            } else {
                Ok(PathLeafKind::Other)
            }
        }
        Err(err) if err.kind() == ErrorKind::NotFound => Ok(PathLeafKind::Missing),
        Err(source) => Err(FsError::Io {
            path: path.to_path_buf(),
            source,
        }),
    }
}

/// Ensures `dir` exists as a non-symlink directory, creating it recursively with `unix_mode` on Unix
/// and post-create `lstat` verification.
pub fn ensure_dir_no_symlink(dir: &Path, unix_mode: u32) -> Result<(), FsError> {
    #[cfg(not(unix))]
    let _ = unix_mode;

    validate_path(dir)?;
    if inspect_existing_dir(dir)?.is_some() {
        return Ok(());
    }

    let mut dir_builder = fs::DirBuilder::new();
    dir_builder.recursive(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        dir_builder.mode(unix_mode);
    }
    dir_builder.create(dir).map_err(|source| FsError::Io {
        path: dir.to_path_buf(),
        source,
    })?;

    let check_dir = normalize_path(dir);
    let created_meta = fs::symlink_metadata(&check_dir).map_err(|source| FsError::Io {
        path: dir.to_path_buf(),
        source,
    })?;
    if created_meta.file_type().is_symlink() {
        return Err(FsError::SymlinkRejected {
            path: dir.to_path_buf(),
        });
    }
    if !created_meta.file_type().is_dir() {
        return Err(FsError::NotDirectory {
            path: dir.to_path_buf(),
        });
    }
    Ok(())
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DirChainStatus {
    SafeDir,
    Missing,
    Symlink {
        path: PathBuf,
        link_target: Option<PathBuf>,
    },
    NotDirectory {
        path: PathBuf,
    },
}

/// Walks `root/rel_dirs[0]/rel_dirs[1]/...` using `lstat(2)` on each segment.
pub fn inspect_dir_chain(root: &Path, rel_dirs: &[&str]) -> Result<DirChainStatus, FsError> {
    validate_path(root)?;
    for segment in rel_dirs {
        let seg_path = Path::new(segment);
        validate_path(seg_path)?;
        let mut comps = seg_path.components();
        let is_single_normal = matches!(
            (comps.next(), comps.next()),
            (Some(Component::Normal(_)), None)
        );
        if !is_single_normal || segment.contains('/') || segment.contains('\\') {
            return Err(FsError::InvalidFileName {
                path: PathBuf::from(segment),
            });
        }
    }

    let mut current = root.to_path_buf();
    for segment in rel_dirs {
        current.push(segment);
        let check_path = normalize_path(&current);
        match fs::symlink_metadata(&check_path) {
            Ok(meta) => {
                if meta.file_type().is_symlink() {
                    let link_target = fs::read_link(&check_path).ok();
                    return Ok(DirChainStatus::Symlink {
                        path: current,
                        link_target,
                    });
                }
                if !meta.file_type().is_dir() {
                    return Ok(DirChainStatus::NotDirectory { path: current });
                }
            }
            Err(err) if err.kind() == ErrorKind::NotFound => {
                return Ok(DirChainStatus::Missing);
            }
            Err(source) => {
                return Err(FsError::Io {
                    path: current,
                    source,
                });
            }
        }
    }
    Ok(DirChainStatus::SafeDir)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SymlinkPolicy {
    Reject,
    Follow,
}

#[derive(Debug)]
pub struct BoundedReadResult {
    pub bytes: Vec<u8>,
    pub metadata: fs::Metadata,
}

/// Reads a regular file up to `max_bytes` with pre-open stat, post-open `fstat` `is_file()` +
/// Unix `(dev, ino)` TOCTOU verification, and bounded `take(max_bytes + 1)` reads.
///
/// Opens the original `path` (not the normalized path) so trailing slashes on a regular file
/// still fail safely with `ENOTDIR`.
pub fn read_regular_file_bounded(
    path: &Path,
    max_bytes: u64,
    symlinks: SymlinkPolicy,
) -> Result<Option<BoundedReadResult>, FsError> {
    validate_path(path)?;
    let check_path = normalize_path(path);

    let pre_meta = match symlinks {
        SymlinkPolicy::Reject => {
            let Some(meta) = inspect_regular_file_if_exists(path, Some(max_bytes))? else {
                return Ok(None);
            };
            meta
        }
        SymlinkPolicy::Follow => match fs::metadata(&check_path) {
            Ok(meta) => {
                if !meta.file_type().is_file() {
                    return Err(FsError::NotRegularFile {
                        path: path.to_path_buf(),
                    });
                }
                if meta.len() > max_bytes {
                    return Err(FsError::SizeLimitExceeded {
                        path: path.to_path_buf(),
                        max_bytes,
                    });
                }
                meta
            }
            Err(err) if err.kind() == ErrorKind::NotFound => return Ok(None),
            Err(source) => {
                return Err(FsError::Io {
                    path: path.to_path_buf(),
                    source,
                });
            }
        },
    };

    // Open the original path so trailing slashes on a regular file still fail safely (ENOTDIR),
    // while symlinks, FIFOs, directories, and oversized files were already rejected above.
    let file = match File::open(path) {
        Ok(file) => file,
        Err(err) if err.kind() == ErrorKind::NotFound => return Ok(None),
        Err(source) => {
            return Err(FsError::Io {
                path: path.to_path_buf(),
                source,
            });
        }
    };

    let open_meta = file.metadata().map_err(|source| FsError::Io {
        path: path.to_path_buf(),
        source,
    })?;
    if !open_meta.file_type().is_file() {
        return Err(FsError::NotRegularFile {
            path: path.to_path_buf(),
        });
    }

    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        if open_meta.dev() != pre_meta.dev() || open_meta.ino() != pre_meta.ino() {
            return Err(FsError::ConcurrentModification {
                path: path.to_path_buf(),
            });
        }
    }
    #[cfg(not(unix))]
    let _ = pre_meta;

    if open_meta.len() > max_bytes {
        return Err(FsError::SizeLimitExceeded {
            path: path.to_path_buf(),
            max_bytes,
        });
    }

    let mut bytes = Vec::new();
    file.take(max_bytes.saturating_add(1))
        .read_to_end(&mut bytes)
        .map_err(|source| FsError::Io {
            path: path.to_path_buf(),
            source,
        })?;

    if (bytes.len() as u64) > max_bytes {
        return Err(FsError::SizeLimitExceeded {
            path: path.to_path_buf(),
            max_bytes,
        });
    }

    Ok(Some(BoundedReadResult {
        bytes,
        metadata: open_meta,
    }))
}

/// Bounded TOCTOU-checked UTF-8 file reader.
pub fn read_regular_file_utf8_bounded(
    path: &Path,
    max_bytes: u64,
    symlinks: SymlinkPolicy,
) -> Result<Option<(String, fs::Metadata)>, FsError> {
    let Some(res) = read_regular_file_bounded(path, max_bytes, symlinks)? else {
        return Ok(None);
    };
    let text = String::from_utf8(res.bytes).map_err(|_| FsError::InvalidUtf8 {
        path: path.to_path_buf(),
    })?;
    Ok(Some((text, res.metadata)))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RejectedReason {
    Symlink,
    NotRegularFile,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FileStamp {
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

/// Snapshots the metadata stamp of `path` via `lstat(2)` without opening or dereferencing it.
pub fn inspect_file_stamp(path: &Path) -> FileStamp {
    if validate_path(path).is_err() {
        return FileStamp::Unreadable(ErrorKind::InvalidInput);
    }
    let check_path = normalize_path(path);
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

/// Reads bounded bytes for a previously captured `FileStamp`, verifying post-open `fstat`
/// regular-file type and Unix `(dev, ino)` identity against `stamp`.
pub fn read_verified_bytes_for_stamp(
    path: &Path,
    stamp: &FileStamp,
    max_bytes: u64,
) -> Result<Option<Vec<u8>>, FsError> {
    validate_path(path)?;
    let FileStamp::Present {
        len,
        #[cfg(unix)]
        dev,
        #[cfg(unix)]
        ino,
        ..
    } = stamp
    else {
        return match stamp {
            FileStamp::Missing => Ok(None),
            FileStamp::Rejected(RejectedReason::Symlink) => Err(FsError::SymlinkRejected {
                path: path.to_path_buf(),
            }),
            FileStamp::Rejected(RejectedReason::NotRegularFile) => Err(FsError::NotRegularFile {
                path: path.to_path_buf(),
            }),
            FileStamp::Unreadable(kind) => Err(FsError::Io {
                path: path.to_path_buf(),
                source: io::Error::from(*kind),
            }),
            FileStamp::Present { .. } => unreachable!(),
        };
    };

    if *len > max_bytes {
        return Err(FsError::SizeLimitExceeded {
            path: path.to_path_buf(),
            max_bytes,
        });
    }

    let file = match File::open(path) {
        Ok(file) => file,
        Err(err) if err.kind() == ErrorKind::NotFound => return Ok(None),
        Err(source) => {
            return Err(FsError::Io {
                path: path.to_path_buf(),
                source,
            });
        }
    };

    let open_meta = file.metadata().map_err(|source| FsError::Io {
        path: path.to_path_buf(),
        source,
    })?;
    if !open_meta.file_type().is_file() {
        return Err(FsError::NotRegularFile {
            path: path.to_path_buf(),
        });
    }

    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        if open_meta.dev() != *dev || open_meta.ino() != *ino {
            return Err(FsError::ConcurrentModification {
                path: path.to_path_buf(),
            });
        }
    }

    if open_meta.len() > max_bytes {
        return Err(FsError::SizeLimitExceeded {
            path: path.to_path_buf(),
            max_bytes,
        });
    }

    let mut bytes = Vec::new();
    file.take(max_bytes.saturating_add(1))
        .read_to_end(&mut bytes)
        .map_err(|source| FsError::Io {
            path: path.to_path_buf(),
            source,
        })?;

    if (bytes.len() as u64) > max_bytes {
        return Err(FsError::SizeLimitExceeded {
            path: path.to_path_buf(),
            max_bytes,
        });
    }

    Ok(Some(bytes))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OpenRegularMode {
    ReadWriteCreate,
    AppendCreate,
}

/// Opens or creates a regular file with mode `0600` on Unix, rejecting symlinks/non-regular entries
/// pre-open, verifying `fstat` `is_file()` + Unix `(dev, ino)` post-open, and applying fd-level `0600`.
pub fn open_verified_regular_file_0600(
    path: &Path,
    mode: OpenRegularMode,
) -> Result<(File, fs::Metadata), FsError> {
    validate_path(path)?;
    let pre_open_meta = inspect_regular_file_if_exists(path, None)?;

    let mut open_opts = fs::OpenOptions::new();
    match mode {
        OpenRegularMode::ReadWriteCreate => {
            open_opts.read(true).write(true).create(true);
        }
        OpenRegularMode::AppendCreate => {
            open_opts.create(true).append(true);
        }
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        open_opts.mode(0o600);
    }

    let file = open_opts.open(path).map_err(|source| FsError::Io {
        path: path.to_path_buf(),
        source,
    })?;

    let open_meta = file.metadata().map_err(|source| FsError::Io {
        path: path.to_path_buf(),
        source,
    })?;
    if !open_meta.file_type().is_file() {
        return Err(FsError::NotRegularFile {
            path: path.to_path_buf(),
        });
    }

    #[cfg(unix)]
    {
        use std::os::unix::fs::{MetadataExt, PermissionsExt};
        if let Some(lstat_meta) = pre_open_meta {
            if open_meta.dev() != lstat_meta.dev() || open_meta.ino() != lstat_meta.ino() {
                return Err(FsError::ConcurrentModification {
                    path: path.to_path_buf(),
                });
            }
        } else {
            let check_path = normalize_path(path);
            let post_lstat = fs::symlink_metadata(&check_path).map_err(|source| FsError::Io {
                path: path.to_path_buf(),
                source,
            })?;
            if post_lstat.file_type().is_symlink()
                || !post_lstat.file_type().is_file()
                || post_lstat.dev() != open_meta.dev()
                || post_lstat.ino() != open_meta.ino()
            {
                return Err(FsError::ConcurrentModification {
                    path: path.to_path_buf(),
                });
            }
        }
        file.set_permissions(fs::Permissions::from_mode(0o600))
            .map_err(|source| FsError::Io {
                path: path.to_path_buf(),
                source,
            })?;
    }
    #[cfg(not(unix))]
    let _ = pre_open_meta;

    Ok((file, open_meta))
}

/// Tightens an existing regular file's permissions to `0600` on Unix using `lstat` +
/// `File::open` + `fstat` `(dev, ino)` verification + `fchmod`. No-op on missing files and non-Unix.
pub fn tighten_regular_file_mode_0600(path: &Path) -> Result<(), FsError> {
    validate_path(path)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::{MetadataExt, PermissionsExt};

        let Some(lstat_meta) = inspect_regular_file_if_exists(path, None)? else {
            return Ok(());
        };
        if path.as_os_str() == normalize_path(path).as_os_str()
            && (lstat_meta.permissions().mode() & 0o777) == 0o600
        {
            return Ok(());
        }

        let file = match File::open(path) {
            Ok(file) => file,
            Err(err) if err.kind() == ErrorKind::NotFound => return Ok(()),
            Err(source) => {
                return Err(FsError::Io {
                    path: path.to_path_buf(),
                    source,
                });
            }
        };
        let open_meta = file.metadata().map_err(|source| FsError::Io {
            path: path.to_path_buf(),
            source,
        })?;
        if !open_meta.file_type().is_file()
            || open_meta.dev() != lstat_meta.dev()
            || open_meta.ino() != lstat_meta.ino()
        {
            return Err(FsError::ConcurrentModification {
                path: path.to_path_buf(),
            });
        }
        if (open_meta.permissions().mode() & 0o777) != 0o600 {
            file.set_permissions(fs::Permissions::from_mode(0o600))
                .map_err(|source| FsError::Io {
                    path: path.to_path_buf(),
                    source,
                })?;
        }
        Ok(())
    }
    #[cfg(not(unix))]
    {
        let _ = path;
        Ok(())
    }
}

/// Unlinks a symlink entry across platforms (on Windows directory symlinks require `remove_dir`).
pub fn unlink_symlink(path: &Path) -> io::Result<()> {
    if let Err(err) = validate_path(path) {
        return Err(io::Error::new(ErrorKind::InvalidInput, err.to_string()));
    }
    #[cfg(not(windows))]
    {
        fs::remove_file(path)
    }
    #[cfg(windows)]
    {
        fs::remove_file(path).or_else(|_| fs::remove_dir(path))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn validate_path_rejects_empty_nul_and_parent_traversal() {
        assert!(matches!(
            validate_path(Path::new("")),
            Err(FsError::EmptyPath)
        ));
        assert!(matches!(
            validate_path(Path::new("   ")),
            Err(FsError::EmptyPath)
        ));
        assert!(matches!(
            validate_path(Path::new("/tmp/a\0b")),
            Err(FsError::ContainsNul)
        ));
        assert!(matches!(
            validate_path(Path::new("../escape")),
            Err(FsError::ParentDirTraversal)
        ));
        assert!(matches!(
            validate_path(Path::new("/tmp/a/../b")),
            Err(FsError::ParentDirTraversal)
        ));

        assert!(validate_path(Path::new("my..config.json")).is_ok());
        assert!(validate_path(Path::new("./local.json")).is_ok());
    }

    #[test]
    fn normalize_path_strips_trailing_slashes_and_dot_segments() {
        assert_eq!(
            normalize_path(Path::new("/tmp/./foo/bar/")),
            PathBuf::from("/tmp/foo/bar")
        );
        assert_eq!(
            append_os_suffix(Path::new("/tmp/usage.db"), "-wal"),
            PathBuf::from("/tmp/usage.db-wal")
        );
    }

    #[test]
    fn retry_rename_with_backoff_succeeds_immediately_without_sleeping() {
        let mut attempts = 0;
        let mut sleeps = Vec::new();

        let res = retry_rename_with_backoff(
            &WINDOWS_RENAME_BACKOFFS,
            || {
                attempts += 1;
                Ok(())
            },
            |delay| sleeps.push(delay),
        );

        assert!(res.is_ok());
        assert_eq!(attempts, 1);
        assert!(sleeps.is_empty());
    }

    #[test]
    fn retry_rename_with_backoff_recovers_after_one_or_two_permission_denied() {
        for fail_count in [1_usize, 2] {
            let mut attempts = 0;
            let mut sleeps = Vec::new();
            let res = retry_rename_with_backoff(
                &WINDOWS_RENAME_BACKOFFS,
                || {
                    attempts += 1;
                    if attempts <= fail_count {
                        Err(io::Error::new(
                            ErrorKind::PermissionDenied,
                            "transient lock",
                        ))
                    } else {
                        Ok(())
                    }
                },
                |delay| sleeps.push(delay),
            );
            assert!(res.is_ok(), "failed for fail_count={fail_count}");
            assert_eq!(attempts, fail_count + 1);
            assert_eq!(sleeps, WINDOWS_RENAME_BACKOFFS[..fail_count]);
        }
    }

    #[test]
    fn retry_rename_with_backoff_exhausts_on_third_permission_denied() {
        let mut attempts = 0;
        let mut sleeps = Vec::new();
        let err = retry_rename_with_backoff(
            &WINDOWS_RENAME_BACKOFFS,
            || {
                attempts += 1;
                Err(io::Error::new(
                    ErrorKind::PermissionDenied,
                    "persistent lock",
                ))
            },
            |delay| sleeps.push(delay),
        )
        .expect_err("must fail after exhausting retries");

        assert_eq!(err.kind(), ErrorKind::PermissionDenied);
        assert_eq!(attempts, WINDOWS_RENAME_MAX_ATTEMPTS);
        assert_eq!(sleeps, WINDOWS_RENAME_BACKOFFS);
    }

    #[test]
    fn retry_rename_with_backoff_aborts_immediately_on_non_permission_denied() {
        let mut attempts = 0;
        let mut sleeps = Vec::new();
        let err = retry_rename_with_backoff(
            &WINDOWS_RENAME_BACKOFFS,
            || {
                attempts += 1;
                Err(io::Error::new(ErrorKind::NotFound, "missing"))
            },
            |delay| sleeps.push(delay),
        )
        .expect_err("must abort immediately");
        assert_eq!(err.kind(), ErrorKind::NotFound);
        assert_eq!(attempts, 1);
        assert!(sleeps.is_empty());

        // Mid-sequence non-PermissionDenied abort
        let mut attempts2 = 0;
        let mut sleeps2 = Vec::new();
        let err2 = retry_rename_with_backoff(
            &WINDOWS_RENAME_BACKOFFS,
            || {
                attempts2 += 1;
                if attempts2 == 1 {
                    Err(io::Error::new(ErrorKind::PermissionDenied, "lock"))
                } else {
                    Err(io::Error::new(ErrorKind::AlreadyExists, "conflict"))
                }
            },
            |delay| sleeps2.push(delay),
        )
        .expect_err("must abort on second attempt");
        assert_eq!(err2.kind(), ErrorKind::AlreadyExists);
        assert_eq!(attempts2, 2);
        assert_eq!(sleeps2, vec![WINDOWS_RENAME_BACKOFFS[0]]);
    }

    #[test]
    fn rename_with_retry_real_fs_smoke_test() {
        let tmp = TempDir::new().expect("temp dir");
        let src = tmp.path().join("src.txt");
        let dst = tmp.path().join("dst.txt");
        fs::write(&src, b"hello").expect("write src");
        rename_with_retry(&src, &dst).expect("rename succeeds");
        assert!(!src.exists());
        assert_eq!(fs::read(&dst).expect("read dst"), b"hello");
    }

    #[test]
    fn temp_path_guard_cleans_file_and_dir_and_disarms() {
        let tmp = TempDir::new().expect("temp dir");
        let file_a = tmp.path().join("a.tmp");
        let file_b = tmp.path().join("b.tmp");
        let dir_a = tmp.path().join("dir_a");
        let dir_b = tmp.path().join("dir_b");

        fs::write(&file_a, b"x").expect("write a");
        fs::write(&file_b, b"y").expect("write b");
        fs::create_dir(&dir_a).expect("mkdir a");
        fs::write(dir_a.join("nested"), b"z").expect("write nested");
        fs::create_dir(&dir_b).expect("mkdir b");

        {
            let _gf = TempPathGuard::file(file_a.clone());
            let mut gf_disarm = TempPathGuard::file(file_b.clone());
            assert_eq!(gf_disarm.path(), Some(file_b.as_path()));
            gf_disarm.disarm();
            assert_eq!(gf_disarm.path(), None);

            let _gd = TempPathGuard::dir(dir_a.clone());
            let mut gd_disarm = TempPathGuard::dir(dir_b.clone());
            gd_disarm.disarm();
        }

        assert!(!file_a.exists());
        assert!(file_b.exists());
        assert!(!dir_a.exists());
        assert!(dir_b.exists());
    }

    #[test]
    fn write_atomic_secure_modes_temp_cleanup_and_policies() {
        let tmp = TempDir::new().expect("temp dir");
        let target = tmp.path().join("sub").join("config.json");

        write_atomic_secure(&target, b"{\"ok\":true}\n").expect("write atomic secure");
        assert_eq!(fs::read(&target).expect("read"), b"{\"ok\":true}\n");

        // Zero .tmp. leftovers
        let entries: Vec<_> = fs::read_dir(target.parent().unwrap())
            .expect("read_dir")
            .filter_map(|e| e.ok())
            .filter(|e| e.file_name().to_string_lossy().contains(".tmp."))
            .collect();
        assert!(entries.is_empty());

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let file_mode = fs::metadata(&target).unwrap().permissions().mode() & 0o777;
            let dir_mode = fs::metadata(target.parent().unwrap())
                .unwrap()
                .permissions()
                .mode()
                & 0o777;
            assert_eq!(file_mode, 0o600);
            assert_eq!(dir_mode, 0o700);
        }

        // TargetFilePolicy::RejectSymlink renaming over an existing directory fails with FsError::Io
        // and TempPathGuard removes the temporary file.
        let dir_target = tmp.path().join("existing_dir");
        fs::create_dir(&dir_target).expect("mkdir existing_dir");
        let err = write_atomic_secure(&dir_target, b"payload")
            .expect_err("rename over directory must fail");
        assert!(matches!(err, FsError::Io { .. }), "got {err:?}");
        let leftover_tmps: Vec<_> = fs::read_dir(tmp.path())
            .unwrap()
            .filter_map(|e| e.ok())
            .filter(|e| e.file_name().to_string_lossy().contains(".tmp."))
            .collect();
        assert!(leftover_tmps.is_empty());

        // TargetFilePolicy::RejectSymlinkAndNonRegular rejects directory pre-open with NotRegularFile.
        let strict_opts = AtomicWriteOptions {
            target_policy: TargetFilePolicy::RejectSymlinkAndNonRegular,
            ..AtomicWriteOptions::secure()
        };
        let err = write_atomic(&dir_target, b"payload", strict_opts)
            .expect_err("non-regular target must fail");
        assert!(matches!(err, FsError::NotRegularFile { .. }), "got {err:?}");
    }

    #[cfg(unix)]
    #[test]
    fn write_atomic_rejects_symlinks_and_tightens_parent() {
        use std::os::unix::fs::{PermissionsExt, symlink};

        let tmp = TempDir::new().expect("temp dir");
        let victim = tmp.path().join("victim.txt");
        fs::write(&victim, b"UNTOUCHED").expect("write victim");

        let live_link = tmp.path().join("live_link");
        symlink(&victim, &live_link).expect("symlink live");
        let err = write_atomic_secure(&live_link, b"pwned").expect_err("live symlink rejected");
        assert!(matches!(err, FsError::SymlinkRejected { .. }));
        assert!(err.to_string().contains("symbolic link"));
        assert_eq!(fs::read(&victim).unwrap(), b"UNTOUCHED");

        let dangling_link = tmp.path().join("dangling_link");
        symlink(tmp.path().join("missing.txt"), &dangling_link).expect("symlink dangling");
        assert!(matches!(
            write_atomic_secure(&dangling_link, b"pwned"),
            Err(FsError::SymlinkRejected { .. })
        ));

        let trailing = PathBuf::from(format!("{}/", live_link.display()));
        assert!(matches!(
            write_atomic_secure(&trailing, b"pwned"),
            Err(FsError::SymlinkRejected { .. })
        ));

        // ParentDirPolicy::RejectSymlinkAndTighten
        let real_parent = tmp.path().join("real_parent");
        fs::create_dir(&real_parent).expect("mkdir");
        fs::set_permissions(&real_parent, fs::Permissions::from_mode(0o755)).expect("chmod 0755");
        let sym_parent = tmp.path().join("sym_parent");
        symlink(&real_parent, &sym_parent).expect("symlink parent");

        let tighten_opts = AtomicWriteOptions {
            file_mode: 0o600,
            dir_mode: 0o700,
            parent_policy: ParentDirPolicy::RejectSymlinkAndTighten,
            target_policy: TargetFilePolicy::RejectSymlinkAndNonRegular,
        };
        assert!(matches!(
            write_atomic(&sym_parent.join("env.sh"), b"data", tighten_opts),
            Err(FsError::SymlinkRejected { .. })
        ));

        write_atomic(&real_parent.join("env.sh"), b"data", tighten_opts).expect("write succeeds");
        let tightened_mode = fs::metadata(&real_parent).unwrap().permissions().mode() & 0o777;
        assert_eq!(tightened_mode, 0o700);
    }

    #[test]
    fn read_regular_file_bounded_handles_missing_oversized_utf8_and_trailing_slash() {
        let tmp = TempDir::new().expect("temp dir");
        assert!(
            read_regular_file_bounded(&tmp.path().join("missing"), 64, SymlinkPolicy::Reject)
                .expect("missing is Ok(None)")
                .is_none()
        );

        let small = tmp.path().join("small.txt");
        fs::write(&small, "hello").expect("write small");
        let (text, _meta) = read_regular_file_utf8_bounded(&small, 5, SymlinkPolicy::Reject)
            .expect("read ok")
            .expect("some");
        assert_eq!(text, "hello");

        let err = read_regular_file_bounded(&small, 4, SymlinkPolicy::Reject)
            .expect_err("oversized must fail");
        assert!(matches!(
            err,
            FsError::SizeLimitExceeded { max_bytes: 4, .. }
        ));

        let bad_utf8 = tmp.path().join("bad.bin");
        fs::write(&bad_utf8, [0xff, 0xfe]).expect("write bad utf8");
        assert!(matches!(
            read_regular_file_utf8_bounded(&bad_utf8, 64, SymlinkPolicy::Reject),
            Err(FsError::InvalidUtf8 { .. })
        ));

        let dir = tmp.path().join("adir");
        fs::create_dir(&dir).expect("mkdir");
        assert!(matches!(
            read_regular_file_bounded(&dir, 64, SymlinkPolicy::Reject),
            Err(FsError::NotRegularFile { .. })
        ));

        // Trailing slash on regular file must fail with FsError::Io (ENOTDIR on Unix)
        #[cfg(unix)]
        {
            let trailing = PathBuf::from(format!("{}/", small.display()));
            let err = read_regular_file_bounded(&trailing, 64, SymlinkPolicy::Reject)
                .expect_err("trailing slash on regular file must fail");
            assert!(matches!(err, FsError::Io { .. }), "got {err:?}");
        }
    }

    #[cfg(unix)]
    #[test]
    fn read_regular_file_bounded_symlink_policies_and_fifo_no_block() {
        use std::os::unix::fs::symlink;

        let tmp = TempDir::new().expect("temp dir");
        let target = tmp.path().join("target.txt");
        fs::write(&target, "secret").expect("write target");
        let link = tmp.path().join("link.txt");
        symlink(&target, &link).expect("symlink");

        assert!(matches!(
            read_regular_file_bounded(&link, 64, SymlinkPolicy::Reject),
            Err(FsError::SymlinkRejected { .. })
        ));
        let (content, _) = read_regular_file_utf8_bounded(&link, 64, SymlinkPolicy::Follow)
            .expect("follow succeeds")
            .expect("present");
        assert_eq!(content, "secret");

        // FIFO fails fast without blocking
        let fifo = tmp.path().join("pipe.fifo");
        let status = std::process::Command::new("mkfifo")
            .arg(&fifo)
            .status()
            .expect("mkfifo");
        assert!(status.success());

        let (tx, rx) = std::sync::mpsc::channel();
        let fifo_clone = fifo.clone();
        std::thread::spawn(move || {
            let _ = tx.send(read_regular_file_bounded(
                &fifo_clone,
                64,
                SymlinkPolicy::Reject,
            ));
        });
        let res = rx
            .recv_timeout(Duration::from_secs(1))
            .expect("must not hang on FIFO");
        assert!(matches!(res, Err(FsError::NotRegularFile { .. })));
    }

    #[test]
    fn file_stamp_and_verified_bytes_detect_missing_rejected_oversized_and_swap() {
        let tmp = TempDir::new().expect("temp dir");
        let missing = tmp.path().join("missing.json");
        assert_eq!(inspect_file_stamp(&missing), FileStamp::Missing);
        assert_eq!(
            read_verified_bytes_for_stamp(&missing, &FileStamp::Missing, 64).expect("missing"),
            None
        );

        let dir = tmp.path().join("dir");
        fs::create_dir(&dir).expect("mkdir");
        let dir_stamp = inspect_file_stamp(&dir);
        assert_eq!(
            dir_stamp,
            FileStamp::Rejected(RejectedReason::NotRegularFile)
        );
        assert!(matches!(
            read_verified_bytes_for_stamp(&dir, &dir_stamp, 64),
            Err(FsError::NotRegularFile { .. })
        ));

        let file = tmp.path().join("cfg.json");
        fs::write(&file, b"{\"a\":1}").expect("write");
        let stamp = inspect_file_stamp(&file);
        assert!(matches!(stamp, FileStamp::Present { .. }));
        assert_eq!(
            read_verified_bytes_for_stamp(&file, &stamp, 64).expect("read"),
            Some(b"{\"a\":1}".to_vec())
        );
        assert!(matches!(
            read_verified_bytes_for_stamp(&file, &stamp, 2),
            Err(FsError::SizeLimitExceeded { max_bytes: 2, .. })
        ));

        // Replace file via atomic rename (new inode) -> ConcurrentModification on Unix
        #[cfg(unix)]
        {
            let replacement = tmp.path().join("cfg.new");
            fs::write(&replacement, b"{\"a\":2}").expect("write replacement");
            fs::rename(&replacement, &file).expect("swap file");
            assert!(matches!(
                read_verified_bytes_for_stamp(&file, &stamp, 64),
                Err(FsError::ConcurrentModification { .. })
            ));
        }
    }

    #[cfg(unix)]
    #[test]
    fn open_verified_0600_tighten_0600_inspect_dir_chain_and_unlink_symlink() {
        use std::os::unix::fs::{PermissionsExt, symlink};

        let tmp = TempDir::new().expect("temp dir");

        // open_verified_regular_file_0600 (ReadWriteCreate & AppendCreate)
        let db = tmp.path().join("usage.db");
        {
            let (_f, _meta) =
                open_verified_regular_file_0600(&db, OpenRegularMode::ReadWriteCreate)
                    .expect("create rw 0600");
        }
        assert_eq!(
            fs::metadata(&db).unwrap().permissions().mode() & 0o777,
            0o600
        );

        let log = tmp.path().join("proxy.log");
        fs::write(&log, b"line1\n").expect("write log");
        fs::set_permissions(&log, fs::Permissions::from_mode(0o644)).expect("chmod 0644");
        {
            let (mut f, _meta) =
                open_verified_regular_file_0600(&log, OpenRegularMode::AppendCreate)
                    .expect("open append 0600");
            f.write_all(b"line2\n").expect("append");
        }
        assert_eq!(fs::read(&log).unwrap(), b"line1\nline2\n");
        assert_eq!(
            fs::metadata(&log).unwrap().permissions().mode() & 0o777,
            0o600
        );

        let new_append_log = tmp.path().join("new_proxy.log");
        {
            let (mut f, meta) =
                open_verified_regular_file_0600(&new_append_log, OpenRegularMode::AppendCreate)
                    .expect("create append 0600");
            assert_eq!(meta.len(), 0);
            f.write_all(b"first\n").expect("append new");
        }
        assert_eq!(fs::read(&new_append_log).unwrap(), b"first\n");
        assert_eq!(
            fs::metadata(&new_append_log).unwrap().permissions().mode() & 0o777,
            0o600
        );

        // tighten_regular_file_mode_0600
        let loose = tmp.path().join("loose.txt");
        fs::write(&loose, b"x").expect("write loose");
        fs::set_permissions(&loose, fs::Permissions::from_mode(0o644)).expect("chmod 0644");
        tighten_regular_file_mode_0600(&loose).expect("tighten");
        assert_eq!(
            fs::metadata(&loose).unwrap().permissions().mode() & 0o777,
            0o600
        );
        // Already 0600 short-circuit & missing file
        tighten_regular_file_mode_0600(&loose).expect("already 0600");
        tighten_regular_file_mode_0600(&tmp.path().join("missing")).expect("missing ok");

        // Symlink rejected by tighten & open_verified without modifying target
        fs::set_permissions(&loose, fs::Permissions::from_mode(0o644)).expect("chmod 0644");
        let link = tmp.path().join("loose_link");
        symlink(&loose, &link).expect("symlink");
        assert!(matches!(
            tighten_regular_file_mode_0600(&link),
            Err(FsError::SymlinkRejected { .. })
        ));
        assert!(matches!(
            open_verified_regular_file_0600(&link, OpenRegularMode::ReadWriteCreate),
            Err(FsError::SymlinkRejected { .. })
        ));
        assert!(matches!(
            open_verified_regular_file_0600(&link, OpenRegularMode::AppendCreate),
            Err(FsError::SymlinkRejected { .. })
        ));
        assert_eq!(
            fs::metadata(&loose).unwrap().permissions().mode() & 0o777,
            0o644
        );

        // unlink_symlink removes symlink without touching target
        unlink_symlink(&link).expect("unlink symlink");
        assert!(!link.exists());
        assert!(loose.exists());

        // inspect_dir_chain
        let root = tmp.path().join("chain");
        fs::create_dir(&root).expect("mkdir root");
        assert_eq!(
            inspect_dir_chain(&root, &["a", "b"]).expect("missing"),
            DirChainStatus::Missing
        );
        fs::create_dir_all(root.join("a").join("b")).expect("mkdir a/b");
        assert_eq!(
            inspect_dir_chain(&root, &["a", "b"]).expect("safe"),
            DirChainStatus::SafeDir
        );

        let file_blocker = root.join("blocker");
        fs::write(&file_blocker, b"x").expect("write blocker");
        assert_eq!(
            inspect_dir_chain(&root, &["blocker", "sub"]).expect("not dir"),
            DirChainStatus::NotDirectory {
                path: file_blocker.clone(),
            }
        );

        let sym_dir = root.join("sym");
        symlink(root.join("a"), &sym_dir).expect("symlink dir");
        assert_eq!(
            inspect_dir_chain(&root, &["sym", "b"]).expect("sym"),
            DirChainStatus::Symlink {
                path: sym_dir,
                link_target: Some(root.join("a")),
            }
        );
    }

    #[test]
    fn write_atomic_secure_rejects_traversal_nul_empty_and_strict_fifo_without_blocking() {
        let tmp = TempDir::new().expect("temp dir");

        assert!(matches!(
            write_atomic_secure(&tmp.path().join("../escape.txt"), b"x"),
            Err(FsError::ParentDirTraversal)
        ));
        assert!(matches!(
            write_atomic_secure(Path::new("/tmp/bad\0file"), b"x"),
            Err(FsError::ContainsNul)
        ));
        assert!(matches!(
            write_atomic_secure(Path::new(""), b"x"),
            Err(FsError::EmptyPath)
        ));
        assert!(matches!(
            write_atomic_secure(Path::new("   "), b"x"),
            Err(FsError::EmptyPath)
        ));

        #[cfg(unix)]
        {
            let fifo = tmp.path().join("target.fifo");
            let status = std::process::Command::new("mkfifo")
                .arg(&fifo)
                .status()
                .expect("mkfifo");
            assert!(status.success());

            let strict_opts = AtomicWriteOptions {
                target_policy: TargetFilePolicy::RejectSymlinkAndNonRegular,
                ..AtomicWriteOptions::secure()
            };
            let (tx, rx) = std::sync::mpsc::channel();
            let fifo_clone = fifo.clone();
            let handle = std::thread::spawn(move || {
                let _ = tx.send(write_atomic(&fifo_clone, b"payload", strict_opts));
            });
            let res = rx
                .recv_timeout(Duration::from_secs(1))
                .expect("write_atomic(RejectSymlinkAndNonRegular) hung on FIFO");
            handle.join().expect("join");
            assert!(
                matches!(res, Err(FsError::NotRegularFile { .. })),
                "expected NotRegularFile for FIFO, got {res:?}"
            );
        }
    }

    #[test]
    fn all_public_fs_safe_apis_enforce_fail_closed_path_and_segment_validation() {
        let tmp = TempDir::new().expect("temp dir");
        let sub = tmp.path().join("sub");
        fs::create_dir(&sub).expect("mkdir sub");
        let victim = tmp.path().join("victim.txt");
        fs::write(&victim, b"SECRET_VICTIM").expect("write victim");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&victim, fs::Permissions::from_mode(0o644)).expect("chmod 0644");
        }

        let traversal_victim = sub.join("..").join("victim.txt");
        let traversal_dir = sub.join("..");
        let nul_path = tmp.path().join("a\0b");
        let victim_stamp = inspect_file_stamp(&victim);
        assert!(matches!(victim_stamp, FileStamp::Present { .. }));

        // 1. tighten_regular_file_mode_0600 fail-closed validation
        assert!(matches!(
            tighten_regular_file_mode_0600(&traversal_victim),
            Err(FsError::ParentDirTraversal)
        ));
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                fs::metadata(&victim).unwrap().permissions().mode() & 0o777,
                0o644,
                "victim mode must not be mutated through .."
            );
        }
        assert!(matches!(
            tighten_regular_file_mode_0600(&nul_path),
            Err(FsError::ContainsNul)
        ));
        assert!(matches!(
            tighten_regular_file_mode_0600(Path::new("")),
            Err(FsError::EmptyPath)
        ));
        assert!(matches!(
            tighten_regular_file_mode_0600(Path::new("   ")),
            Err(FsError::EmptyPath)
        ));

        // Challenge 4: trailing-slash consistency on Unix for both 0600 and 0644 files
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let f600 = tmp.path().join("f600.txt");
            let f644 = tmp.path().join("f644.txt");
            fs::write(&f600, b"a").expect("write f600");
            fs::write(&f644, b"b").expect("write f644");
            fs::set_permissions(&f600, fs::Permissions::from_mode(0o600)).expect("chmod 0600");
            fs::set_permissions(&f644, fs::Permissions::from_mode(0o644)).expect("chmod 0644");

            let err_600 =
                tighten_regular_file_mode_0600(&PathBuf::from(format!("{}/", f600.display())))
                    .expect_err("trailing slash on 0600 regular file must fail");
            let err_644 =
                tighten_regular_file_mode_0600(&PathBuf::from(format!("{}/", f644.display())))
                    .expect_err("trailing slash on 0644 regular file must fail");
            assert!(
                matches!(&err_600, FsError::Io { source, .. } if source.kind() == ErrorKind::NotADirectory),
                "expected ENOTDIR for f600/, got {err_600:?}"
            );
            assert!(
                matches!(&err_644, FsError::Io { source, .. } if source.kind() == ErrorKind::NotADirectory),
                "expected ENOTDIR for f644/, got {err_644:?}"
            );
        }

        // 2. inspect_dir_chain fail-closed root & single-normal-component segment validation
        assert!(matches!(
            inspect_dir_chain(Path::new(""), &[]),
            Err(FsError::EmptyPath)
        ));
        assert!(matches!(
            inspect_dir_chain(Path::new("   "), &["a"]),
            Err(FsError::EmptyPath)
        ));
        assert!(matches!(
            inspect_dir_chain(&traversal_dir, &["sub"]),
            Err(FsError::ParentDirTraversal)
        ));
        assert!(matches!(
            inspect_dir_chain(&sub, &[".."]),
            Err(FsError::ParentDirTraversal)
        ));
        assert!(matches!(
            inspect_dir_chain(&sub, &["missing", ".."]),
            Err(FsError::ParentDirTraversal)
        ));
        assert!(matches!(
            inspect_dir_chain(&nul_path, &["a"]),
            Err(FsError::ContainsNul)
        ));
        assert!(matches!(
            inspect_dir_chain(&sub, &["a\0b"]),
            Err(FsError::ContainsNul)
        ));
        assert!(matches!(
            inspect_dir_chain(&sub, &[""]),
            Err(FsError::EmptyPath)
        ));
        assert!(matches!(
            inspect_dir_chain(&sub, &["   "]),
            Err(FsError::EmptyPath)
        ));

        let abs_segment = tmp.path().to_string_lossy().into_owned();
        for bad_seg in [
            abs_segment.as_str(),
            "/abs",
            "/",
            ".",
            "a/b",
            "a/",
            "a/.",
            r"a\b",
        ] {
            assert!(
                matches!(
                    inspect_dir_chain(&sub, &[bad_seg]),
                    Err(FsError::InvalidFileName { .. })
                ),
                "expected InvalidFileName for segment '{bad_seg}'"
            );
        }

        // 3. inspect_regular_file_if_exists fail-closed validation
        assert!(matches!(
            inspect_regular_file_if_exists(&traversal_victim, None),
            Err(FsError::ParentDirTraversal)
        ));
        assert!(matches!(
            inspect_regular_file_if_exists(&nul_path, None),
            Err(FsError::ContainsNul)
        ));
        assert!(matches!(
            inspect_regular_file_if_exists(Path::new(""), None),
            Err(FsError::EmptyPath)
        ));

        // 4. inspect_existing_dir fail-closed validation
        assert!(matches!(
            inspect_existing_dir(&traversal_dir),
            Err(FsError::ParentDirTraversal)
        ));
        assert!(matches!(
            inspect_existing_dir(&nul_path),
            Err(FsError::ContainsNul)
        ));
        assert!(matches!(
            inspect_existing_dir(Path::new("   ")),
            Err(FsError::EmptyPath)
        ));

        // 5. read_verified_bytes_for_stamp & inspect_file_stamp fail-closed validation
        assert!(matches!(
            read_verified_bytes_for_stamp(&traversal_victim, &victim_stamp, 1024),
            Err(FsError::ParentDirTraversal)
        ));
        assert!(matches!(
            read_verified_bytes_for_stamp(&nul_path, &victim_stamp, 1024),
            Err(FsError::ContainsNul)
        ));
        assert!(matches!(
            read_verified_bytes_for_stamp(Path::new(""), &FileStamp::Missing, 1024),
            Err(FsError::EmptyPath)
        ));
        assert_eq!(
            inspect_file_stamp(&traversal_victim),
            FileStamp::Unreadable(ErrorKind::InvalidInput)
        );
        assert_eq!(
            inspect_file_stamp(&nul_path),
            FileStamp::Unreadable(ErrorKind::InvalidInput)
        );

        // 6. rename_with_retry & unlink_symlink fail-closed validation
        assert!(matches!(
            rename_with_retry(&traversal_victim, &tmp.path().join("dst.txt")),
            Err(FsError::ParentDirTraversal)
        ));
        assert!(matches!(
            rename_with_retry(&victim, &traversal_victim),
            Err(FsError::ParentDirTraversal)
        ));
        assert_eq!(
            unlink_symlink(&traversal_victim)
                .expect_err("unlink_symlink rejects ..")
                .kind(),
            ErrorKind::InvalidInput
        );
        assert!(victim.exists());
    }

    #[test]
    fn inspect_leaf_kind_classifies_entries_and_rejects_invalid_paths() {
        let tmp = TempDir::new().expect("temp dir");
        let missing = tmp.path().join("does-not-exist");
        let file = tmp.path().join("regular.txt");
        let dir = tmp.path().join("dir");
        fs::write(&file, b"hello").expect("write file");
        fs::create_dir(&dir).expect("mkdir");

        assert_eq!(inspect_leaf_kind(&missing).unwrap(), PathLeafKind::Missing);
        assert_eq!(inspect_leaf_kind(&file).unwrap(), PathLeafKind::RegularFile);
        assert_eq!(inspect_leaf_kind(&dir).unwrap(), PathLeafKind::Directory);

        // Invalid paths fail closed before any lstat.
        assert!(matches!(
            inspect_leaf_kind(Path::new("")),
            Err(FsError::EmptyPath)
        ));
        assert!(matches!(
            inspect_leaf_kind(Path::new("   ")),
            Err(FsError::EmptyPath)
        ));
        assert!(matches!(
            inspect_leaf_kind(Path::new("/tmp/a\0b")),
            Err(FsError::ContainsNul)
        ));
        assert!(matches!(
            inspect_leaf_kind(&dir.join("..").join("regular.txt")),
            Err(FsError::ParentDirTraversal)
        ));

        #[cfg(unix)]
        {
            use std::os::unix::fs::symlink;
            use std::process::Command;

            let file_link = tmp.path().join("file_link");
            let dir_link = tmp.path().join("dir_link");
            let dangling_link = tmp.path().join("dangling_link");
            symlink(&file, &file_link).expect("file symlink");
            symlink(&dir, &dir_link).expect("dir symlink");
            symlink(tmp.path().join("nowhere"), &dangling_link).expect("dangling symlink");

            assert_eq!(
                inspect_leaf_kind(&file_link).unwrap(),
                PathLeafKind::Symlink
            );
            assert_eq!(inspect_leaf_kind(&dir_link).unwrap(), PathLeafKind::Symlink);
            // Trailing slash on a directory symlink must normalize and still classify as Symlink.
            let trailing_dir_link = PathBuf::from(format!("{}/", dir_link.display()));
            assert_eq!(
                inspect_leaf_kind(&trailing_dir_link).unwrap(),
                PathLeafKind::Symlink
            );
            assert_eq!(
                inspect_leaf_kind(&dangling_link).unwrap(),
                PathLeafKind::Symlink
            );

            // FIFO classifies as Other without blocking.
            let fifo = tmp.path().join("pipe.fifo");
            assert!(
                Command::new("mkfifo")
                    .arg(&fifo)
                    .status()
                    .expect("mkfifo")
                    .success()
            );
            assert_eq!(inspect_leaf_kind(&fifo).unwrap(), PathLeafKind::Other);

            // Child under regular file surfaces Io error (ENOTDIR), not Missing.
            assert!(matches!(
                inspect_leaf_kind(&file.join("child")),
                Err(FsError::Io { .. })
            ));
        }
    }
}
