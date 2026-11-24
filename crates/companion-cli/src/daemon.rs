use std::fs::{self, File};
use std::io::{self, ErrorKind, Read, Write};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};

use companion_core::{Config, CoreError};
use serde::{Deserialize, Serialize};

use crate::{CliError, normalized_metadata_path};

pub const PROXY_PID_FILENAME: &str = "proxy.pid";
pub const PROXY_LOG_FILENAME: &str = "proxy.log";
pub const PROXY_LOG_ROTATED_FILENAME: &str = "proxy.log.1";
pub const MAX_PROXY_LOG_BYTES: u64 = 5 * 1024 * 1024;
pub const DEFAULT_IDLE_SHUTDOWN_SECS: u64 = 1800;
const MAX_PID_FILE_BYTES: u64 = 64 * 1024;

#[cfg(any(windows, test))]
const WINDOWS_RENAME_BACKOFFS: [std::time::Duration; 2] = [
    std::time::Duration::from_millis(25),
    std::time::Duration::from_millis(50),
];

/// Derives the runtime directory for `proxy.pid` and `proxy.log`.
///
/// When `config.usage_dir` is explicitly configured (for example in isolated tests
/// setting `<tmp>/usage`), runtime files stay isolated alongside `usage_dir` rather
/// than touching the user's real `$XDG_DATA_HOME/agy-companion` directory.
pub fn resolve_runtime_data_dir(config: &Config) -> Result<PathBuf, CoreError> {
    if let Some(ref usage_dir) = config.usage_dir {
        companion_core::validate_path(usage_dir)?;
        let normalized: PathBuf = usage_dir.components().collect();
        if normalized.file_name().and_then(|n| n.to_str()) == Some("usage")
            && let Some(parent) = normalized.parent()
            && parent.file_name().is_some()
        {
            return Ok(parent.to_path_buf());
        }
        return Ok(normalized);
    }
    companion_core::default_data_dir()
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProxyPidInfo {
    pub pid: u32,
    pub port: u16,
    pub version: String,
    pub started_at_ms: u64,
    pub binary_path: String,
}

impl ProxyPidInfo {
    pub fn for_current_process(port: u16) -> Self {
        let started_at_ms = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| u64::try_from(d.as_millis()).unwrap_or(u64::MAX))
            .unwrap_or(0);
        let binary_path = std::env::current_exe()
            .and_then(|p| fs::canonicalize(&p).or(Ok(p)))
            .map(|p| p.display().to_string())
            .unwrap_or_default();
        Self {
            pid: std::process::id(),
            port,
            version: env!("CARGO_PKG_VERSION").to_string(),
            started_at_ms,
            binary_path,
        }
    }
}

/// RAII guard that removes `proxy.pid` on drop only if the file still belongs to `expected_pid`.
pub struct ProxyPidGuard {
    path: PathBuf,
    expected_pid: u32,
}

impl ProxyPidGuard {
    pub fn path(&self) -> &Path {
        &self.path
    }
}

impl Drop for ProxyPidGuard {
    fn drop(&mut self) {
        if let Ok(Some(info)) = read_proxy_pid(&self.path)
            && info.pid == self.expected_pid
        {
            let _ = fs::remove_file(&self.path);
        }
    }
}

fn ensure_secure_dir(dir: &Path) -> Result<(), CliError> {
    companion_core::validate_path(dir)?;
    let check_dir = normalized_metadata_path(dir);
    match fs::symlink_metadata(&check_dir) {
        Ok(meta) => {
            if meta.file_type().is_symlink() {
                return Err(CliError::InvalidDataDir {
                    path: dir.to_path_buf(),
                    reason: "symbolic links are not allowed for data directories".to_string(),
                });
            }
            if !meta.file_type().is_dir() {
                return Err(CliError::InvalidDataDir {
                    path: dir.to_path_buf(),
                    reason: "data directory path must be a directory".to_string(),
                });
            }
            Ok(())
        }
        Err(err) if err.kind() == ErrorKind::NotFound => {
            let mut builder = fs::DirBuilder::new();
            builder.recursive(true);
            #[cfg(unix)]
            {
                use std::os::unix::fs::DirBuilderExt;
                builder.mode(0o700);
            }
            builder.create(dir)?;

            let created = fs::symlink_metadata(&check_dir)?;
            if created.file_type().is_symlink() || !created.file_type().is_dir() {
                return Err(CliError::InvalidDataDir {
                    path: dir.to_path_buf(),
                    reason: "data directory path must be a non-symlink directory".to_string(),
                });
            }
            Ok(())
        }
        Err(err) => Err(CliError::Io(err)),
    }
}

fn reject_non_regular_target(path: &Path, label: &str) -> Result<(), CliError> {
    let check_path = normalized_metadata_path(path);
    match fs::symlink_metadata(&check_path) {
        Ok(meta) => {
            if meta.file_type().is_symlink() {
                return Err(CliError::InvalidDataDir {
                    path: path.to_path_buf(),
                    reason: format!("symbolic links are not allowed for {label}"),
                });
            }
            if !meta.file_type().is_file() {
                return Err(CliError::InvalidDataDir {
                    path: path.to_path_buf(),
                    reason: format!("{label} path must be a regular file"),
                });
            }
            Ok(())
        }
        Err(err) if err.kind() == ErrorKind::NotFound => Ok(()),
        Err(err) => Err(CliError::Io(err)),
    }
}

fn check_regular_or_missing_io(path: &Path, label: &str) -> io::Result<Option<fs::Metadata>> {
    let check_path = normalized_metadata_path(path);
    match fs::symlink_metadata(&check_path) {
        Ok(meta) => {
            if meta.file_type().is_symlink() {
                return Err(io::Error::new(
                    ErrorKind::InvalidInput,
                    format!("symbolic links are not allowed for {label}"),
                ));
            }
            if !meta.file_type().is_file() {
                return Err(io::Error::new(
                    ErrorKind::InvalidInput,
                    format!("{label} path must be a regular file"),
                ));
            }
            Ok(Some(meta))
        }
        Err(err) if err.kind() == ErrorKind::NotFound => Ok(None),
        Err(err) => Err(err),
    }
}

pub fn write_proxy_pid(pid_path: &Path, info: &ProxyPidInfo) -> Result<ProxyPidGuard, CliError> {
    companion_core::validate_path(pid_path)?;
    let parent = pid_path
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    ensure_secure_dir(parent)?;
    reject_non_regular_target(pid_path, "proxy PID files")?;

    let mut payload =
        serde_json::to_string_pretty(info).map_err(|e| CliError::Serialization(e.to_string()))?;
    payload.push('\n');

    companion_core::write_file_atomic_secure(pid_path, payload.as_bytes())?;
    Ok(ProxyPidGuard {
        path: pid_path.to_path_buf(),
        expected_pid: info.pid,
    })
}

pub fn read_proxy_pid(pid_path: &Path) -> Result<Option<ProxyPidInfo>, CliError> {
    companion_core::validate_path(pid_path)?;
    let check_path = normalized_metadata_path(pid_path);
    let pre_meta = match fs::symlink_metadata(&check_path) {
        Ok(meta) => {
            if meta.file_type().is_symlink() {
                return Err(CliError::InvalidDataDir {
                    path: pid_path.to_path_buf(),
                    reason: "symbolic links are not allowed for proxy PID files".to_string(),
                });
            }
            if !meta.file_type().is_file() {
                return Err(CliError::InvalidDataDir {
                    path: pid_path.to_path_buf(),
                    reason: "proxy PID file path must be a regular file".to_string(),
                });
            }
            if meta.len() > MAX_PID_FILE_BYTES {
                return Err(CliError::InvalidDataDir {
                    path: pid_path.to_path_buf(),
                    reason: format!(
                        "proxy PID file exceeds maximum allowed size of {MAX_PID_FILE_BYTES} bytes"
                    ),
                });
            }
            meta
        }
        Err(err) if err.kind() == ErrorKind::NotFound => return Ok(None),
        Err(err) => return Err(CliError::Io(err)),
    };

    let file = match File::open(pid_path) {
        Ok(file) => file,
        Err(err) if err.kind() == ErrorKind::NotFound => return Ok(None),
        Err(err) => return Err(CliError::Io(err)),
    };

    let open_meta = file.metadata()?;
    if !open_meta.file_type().is_file() {
        return Err(CliError::InvalidDataDir {
            path: pid_path.to_path_buf(),
            reason: "proxy PID file path must be a regular file".to_string(),
        });
    }

    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        if open_meta.dev() != pre_meta.dev() || open_meta.ino() != pre_meta.ino() {
            return Err(CliError::InvalidDataDir {
                path: pid_path.to_path_buf(),
                reason: "proxy PID file changed concurrently during open".to_string(),
            });
        }
    }
    #[cfg(not(unix))]
    let _ = pre_meta;

    let mut buf = Vec::new();
    file.take(MAX_PID_FILE_BYTES + 1).read_to_end(&mut buf)?;
    if (buf.len() as u64) > MAX_PID_FILE_BYTES {
        return Err(CliError::InvalidDataDir {
            path: pid_path.to_path_buf(),
            reason: format!(
                "proxy PID file exceeds maximum allowed size of {MAX_PID_FILE_BYTES} bytes"
            ),
        });
    }

    let info: ProxyPidInfo =
        serde_json::from_slice(&buf).map_err(|e| CliError::Serialization(e.to_string()))?;
    Ok(Some(info))
}

fn open_verified_append_file(path: &Path) -> io::Result<(File, u64)> {
    let check_path = normalized_metadata_path(path);
    let pre_open_meta = check_regular_or_missing_io(path, "proxy log files")?;

    let mut open_opts = fs::OpenOptions::new();
    open_opts.create(true).append(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        open_opts.mode(0o600);
    }

    let file = open_opts.open(path)?;
    let open_meta = file.metadata()?;
    if !open_meta.file_type().is_file() {
        return Err(io::Error::new(
            ErrorKind::InvalidInput,
            "proxy log file path must be a regular file",
        ));
    }

    #[cfg(unix)]
    {
        use std::os::unix::fs::{MetadataExt, PermissionsExt};
        if let Some(lstat_meta) = pre_open_meta {
            if open_meta.dev() != lstat_meta.dev() || open_meta.ino() != lstat_meta.ino() {
                return Err(io::Error::new(
                    ErrorKind::InvalidInput,
                    "proxy log file changed concurrently during open",
                ));
            }
        } else {
            let post_lstat = fs::symlink_metadata(&check_path)?;
            if post_lstat.file_type().is_symlink()
                || !post_lstat.file_type().is_file()
                || post_lstat.dev() != open_meta.dev()
                || post_lstat.ino() != open_meta.ino()
            {
                return Err(io::Error::new(
                    ErrorKind::InvalidInput,
                    "proxy log file changed concurrently during open",
                ));
            }
        }
        file.set_permissions(fs::Permissions::from_mode(0o600))?;
    }
    #[cfg(not(unix))]
    let _ = (check_path, pre_open_meta);

    Ok((file, open_meta.len()))
}

#[cfg(any(windows, test))]
fn retry_rename_with_backoff<F, S>(
    backoffs: &[std::time::Duration],
    mut rename_op: F,
    mut sleep_op: S,
) -> io::Result<()>
where
    F: FnMut() -> io::Result<()>,
    S: FnMut(std::time::Duration),
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

#[cfg(windows)]
fn retry_rename_on_permission_denied(from: &Path, to: &Path) -> io::Result<()> {
    retry_rename_with_backoff(
        &WINDOWS_RENAME_BACKOFFS,
        || fs::rename(from, to),
        std::thread::sleep,
    )
}

struct RotatingFileInner {
    path: PathBuf,
    rotated_path: PathBuf,
    max_bytes: u64,
    current_size: u64,
    file: Option<File>,
}

#[derive(Clone)]
pub struct RotatingFileWriter {
    inner: Arc<Mutex<RotatingFileInner>>,
}

impl RotatingFileWriter {
    pub fn open(path: PathBuf, max_bytes: u64) -> Result<Self, CliError> {
        companion_core::validate_path(&path)?;
        let file_name =
            path.file_name()
                .and_then(|n| n.to_str())
                .ok_or_else(|| CliError::InvalidDataDir {
                    path: path.clone(),
                    reason: "proxy log path must include a valid UTF-8 file name".to_string(),
                })?;
        let parent = path
            .parent()
            .filter(|p| !p.as_os_str().is_empty())
            .unwrap_or_else(|| Path::new("."));
        ensure_secure_dir(parent)?;

        let rotated_path = parent.join(format!("{file_name}.1"));
        reject_non_regular_target(&path, "proxy log files")?;
        reject_non_regular_target(&rotated_path, "rotated proxy log files")?;

        let (file, current_size) = open_verified_append_file(&path)?;
        Ok(Self {
            inner: Arc::new(Mutex::new(RotatingFileInner {
                path,
                rotated_path,
                max_bytes: max_bytes.max(1),
                current_size,
                file: Some(file),
            })),
        })
    }

    pub fn write_bytes(&self, buf: &[u8]) -> io::Result<()> {
        if buf.is_empty() {
            return Ok(());
        }
        let mut inner = self
            .inner
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());

        // Guard with `current_size > 0` so a single line larger than `max_bytes` does not loop rotations.
        if inner.current_size > 0
            && inner.current_size.saturating_add(buf.len() as u64) > inner.max_bytes
        {
            if let Some(mut existing) = inner.file.take() {
                let _ = existing.flush();
                drop(existing);
            }
            let _ = check_regular_or_missing_io(&inner.path, "proxy log files")?;
            let _ = check_regular_or_missing_io(&inner.rotated_path, "rotated proxy log files")?;

            #[cfg(windows)]
            let rename_res = retry_rename_on_permission_denied(&inner.path, &inner.rotated_path);
            #[cfg(not(windows))]
            let rename_res = fs::rename(&inner.path, &inner.rotated_path);

            match rename_res {
                Ok(()) => {}
                Err(err) if err.kind() == ErrorKind::NotFound => {}
                Err(err) => return Err(err),
            }

            let (fresh_file, fresh_size) = open_verified_append_file(&inner.path)?;
            inner.file = Some(fresh_file);
            inner.current_size = fresh_size;
        }

        if inner.file.is_none() {
            let (fresh_file, fresh_size) = open_verified_append_file(&inner.path)?;
            inner.file = Some(fresh_file);
            inner.current_size = fresh_size;
        }

        if let Some(ref mut file) = inner.file {
            file.write_all(buf)?;
            inner.current_size = inner.current_size.saturating_add(buf.len() as u64);
        }
        Ok(())
    }

    pub fn flush(&self) -> io::Result<()> {
        let mut inner = self
            .inner
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if let Some(ref mut file) = inner.file {
            file.flush()?;
        }
        Ok(())
    }
}

/// Late-binding `MakeWriter` target for `agy-companion serve`.
///
/// Writes to `stderr` immediately and tees to `proxy.log` only after the proxy
/// binds its listener, so config validation errors and `--adopt-running` standbys
/// never contend on or pollute `proxy.log`.
#[derive(Clone, Default)]
pub struct ServeLogTarget {
    writer: Arc<Mutex<Option<RotatingFileWriter>>>,
}

impl ServeLogTarget {
    pub fn attach(&self, writer: RotatingFileWriter) -> ServeLogAttachGuard {
        let mut guard = self
            .writer
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        *guard = Some(writer);
        ServeLogAttachGuard {
            writer: Arc::clone(&self.writer),
        }
    }
}

pub struct ServeLogAttachGuard {
    writer: Arc<Mutex<Option<RotatingFileWriter>>>,
}

impl Drop for ServeLogAttachGuard {
    fn drop(&mut self) {
        let detached = self
            .writer
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .take();
        if let Some(writer) = detached {
            let _ = writer.flush();
        }
    }
}

pub struct ServeLogWriter {
    file: Option<RotatingFileWriter>,
}

impl Write for ServeLogWriter {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        // Ignore stderr errors (e.g. EPIPE in detached daemons) so file logging still succeeds.
        let _ = io::stderr().write_all(buf);
        if let Some(ref file) = self.file {
            let _ = file.write_bytes(buf);
        }
        Ok(buf.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        let _ = io::stderr().flush();
        if let Some(ref file) = self.file {
            let _ = file.flush();
        }
        Ok(())
    }
}

impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for ServeLogTarget {
    type Writer = ServeLogWriter;

    fn make_writer(&'a self) -> Self::Writer {
        let file = self
            .writer
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone();
        ServeLogWriter { file }
    }
}

/// Spawns a detached Managed On-Demand `agy-companion serve` daemon.
///
/// The child runs in its own process group (`process_group(0)` on Unix;
/// `CREATE_NEW_PROCESS_GROUP | CREATE_NO_WINDOW` on Windows) with `stdin/stdout/stderr`
/// redirected to `Stdio::null()`. `serve` itself opens and rotates `0600` `proxy.log`
/// (5 MiB cap) and writes `0600` `proxy.pid` inside `resolve_runtime_data_dir(&config)`
/// immediately upon binding its listener.
pub fn spawn_detached_proxy_daemon(
    port: u16,
    config_path: &Path,
    idle_shutdown_secs: u64,
) -> Result<std::process::Child, CliError> {
    let exe = std::env::current_exe()?;
    spawn_detached_proxy_daemon_with_exe(&exe, port, config_path, idle_shutdown_secs)
}

pub(crate) fn spawn_detached_proxy_daemon_with_exe(
    exe: &Path,
    port: u16,
    config_path: &Path,
    idle_shutdown_secs: u64,
) -> Result<std::process::Child, CliError> {
    let mut cmd = std::process::Command::new(exe);
    cmd.arg("serve")
        .arg("--adopt-running")
        .arg("--idle-shutdown-secs")
        .arg(idle_shutdown_secs.to_string())
        .arg("--port")
        .arg(port.to_string())
        .arg("--config")
        .arg(config_path)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null());

    // Defensively scrub injected proxy routing vars so the daemon inherits clean
    // HTTPS_PROXY / XDG_* / real GEMINI_API_KEY without echoing placeholder/loopback vars.
    cmd.env_remove("CLOUD_CODE_URL")
        .env_remove("GOOGLE_GEMINI_BASE_URL")
        .env_remove("AGY_COMPANION_HOOK_ACTIVE")
        .env_remove("AGY_COMPANION_INJECTED_URL")
        .env_remove("JETSKI_MCP_ENV_RESTORE_PREFIX")
        .env_remove("JETSKI_MCP_ENV_RESTORE_VARS");
    if std::env::var("GEMINI_API_KEY")
        .ok()
        .is_some_and(|v| companion_core::is_placeholder_api_key(&v))
    {
        cmd.env_remove("GEMINI_API_KEY");
    }

    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        cmd.process_group(0);
    }
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        const CREATE_NEW_PROCESS_GROUP: u32 = 0x0000_0200;
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        cmd.creation_flags(CREATE_NEW_PROCESS_GROUP | CREATE_NO_WINDOW);
    }

    Ok(cmd.spawn()?)
}

#[cfg(test)]
mod tests {
    use super::*;

    use tempfile::TempDir;

    #[test]
    fn resolve_runtime_data_dir_isolates_explicit_usage_dir_and_falls_back_to_default() {
        let temp = TempDir::new().expect("tempdir");

        // <tmp>/usage -> <tmp>
        let mut cfg = Config {
            usage_dir: Some(temp.path().join("usage")),
            ..Default::default()
        };
        assert_eq!(
            resolve_runtime_data_dir(&cfg).expect("usage parent"),
            temp.path()
        );

        // <tmp>/explicit_usage -> <tmp>/explicit_usage
        let explicit = temp.path().join("explicit_usage");
        cfg.usage_dir = Some(explicit.clone());
        assert_eq!(
            resolve_runtime_data_dir(&cfg).expect("explicit usage"),
            explicit
        );

        // Single-component relative "usage" has empty parent -> stays "usage"
        cfg.usage_dir = Some(PathBuf::from("usage"));
        assert_eq!(
            resolve_runtime_data_dir(&cfg).expect("relative usage"),
            PathBuf::from("usage")
        );

        // Root-level "/usage" has parent "/" (file_name() == None) -> stays "/usage"
        #[cfg(unix)]
        {
            cfg.usage_dir = Some(PathBuf::from("/usage"));
            assert_eq!(
                resolve_runtime_data_dir(&cfg).expect("root usage"),
                PathBuf::from("/usage")
            );
        }

        // Traversal rejected fail-closed
        cfg.usage_dir = Some(PathBuf::from("../escape/usage"));
        assert!(matches!(
            resolve_runtime_data_dir(&cfg),
            Err(CoreError::InvalidPath { .. })
        ));

        // None -> companion_core::default_data_dir()
        cfg.usage_dir = None;
        assert_eq!(
            resolve_runtime_data_dir(&cfg).expect("default data dir"),
            companion_core::default_data_dir().expect("core default")
        );
    }

    #[test]
    fn proxy_pid_writes_atomic_0600_json_and_guard_deletes_only_matching_pid() {
        let temp = TempDir::new().expect("tempdir");
        let runtime_dir = temp.path().join("runtime");
        let pid_path = runtime_dir.join(PROXY_PID_FILENAME);

        let info = ProxyPidInfo::for_current_process(4141);
        assert_eq!(info.pid, std::process::id());
        assert_eq!(info.port, 4141);
        assert_eq!(info.version, env!("CARGO_PKG_VERSION"));
        assert!(info.started_at_ms > 0);
        assert!(!info.binary_path.is_empty());

        let guard = write_proxy_pid(&pid_path, &info).expect("write proxy.pid");
        assert!(pid_path.is_file());

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let dir_mode = fs::metadata(&runtime_dir)
                .expect("dir meta")
                .permissions()
                .mode()
                & 0o777;
            let file_mode = fs::metadata(&pid_path)
                .expect("file meta")
                .permissions()
                .mode()
                & 0o777;
            assert_eq!(dir_mode, 0o700);
            assert_eq!(file_mode, 0o600);
        }

        let loaded = read_proxy_pid(&pid_path)
            .expect("read pid")
            .expect("pid present");
        assert_eq!(loaded, info);

        // Dropping guard with matching PID removes the file.
        drop(guard);
        assert!(!pid_path.exists());

        // Guard MUST NOT delete proxy.pid if another daemon overwrote it with a different PID.
        let guard_old = write_proxy_pid(&pid_path, &info).expect("write old pid");
        let replacement = ProxyPidInfo {
            pid: info.pid.saturating_add(1),
            port: 4141,
            version: env!("CARGO_PKG_VERSION").to_string(),
            started_at_ms: info.started_at_ms + 10,
            binary_path: info.binary_path.clone(),
        };
        let guard_new = write_proxy_pid(&pid_path, &replacement).expect("write new pid");
        drop(guard_old);
        assert!(
            pid_path.is_file(),
            "old guard must not unlink proxy.pid owned by replacement PID"
        );
        assert_eq!(
            read_proxy_pid(&pid_path).expect("read").expect("some").pid,
            replacement.pid
        );
        drop(guard_new);
        assert!(!pid_path.exists());
    }

    #[cfg(unix)]
    #[test]
    fn proxy_pid_rejects_symlink_and_fifo_fail_closed() {
        use std::os::unix::fs::symlink;
        use std::process::Command;

        let temp = TempDir::new().expect("tempdir");
        let real_target = temp.path().join("victim.txt");
        fs::write(&real_target, "do-not-overwrite").expect("write victim");

        let link_pid = temp.path().join("symlink.pid");
        symlink(&real_target, &link_pid).expect("symlink pid");

        let info = ProxyPidInfo::for_current_process(4141);
        assert!(write_proxy_pid(&link_pid, &info).is_err());
        assert!(read_proxy_pid(&link_pid).is_err());
        assert_eq!(
            fs::read_to_string(&real_target).expect("read victim"),
            "do-not-overwrite"
        );

        let fifo_pid = temp.path().join("fifo.pid");
        let status = Command::new("mkfifo")
            .arg(&fifo_pid)
            .status()
            .expect("mkfifo");
        assert!(status.success());
        assert!(write_proxy_pid(&fifo_pid, &info).is_err());
        assert!(read_proxy_pid(&fifo_pid).is_err());
    }

    #[test]
    fn rotating_file_writer_rotates_at_threshold_enforces_0600_and_rejects_symlink_or_fifo() {
        let temp = TempDir::new().expect("tempdir");
        let log_dir = temp.path().join("logs");
        let log_path = log_dir.join(PROXY_LOG_FILENAME);
        let rotated_path = log_dir.join(PROXY_LOG_ROTATED_FILENAME);

        let writer = RotatingFileWriter::open(log_path.clone(), 100).expect("open rotating log");
        writer.write_bytes(&[b'a'; 60]).expect("write 1");
        writer
            .write_bytes(&[b'b'; 60])
            .expect("write 2 triggers rotation");
        writer.flush().expect("flush");

        assert!(log_path.is_file());
        assert!(rotated_path.is_file());
        assert_eq!(fs::read(&rotated_path).expect("read .1"), vec![b'a'; 60]);
        assert_eq!(fs::read(&log_path).expect("read log"), vec![b'b'; 60]);

        // Single oversized line (> max_bytes) rotates previous contents once and writes cleanly.
        writer
            .write_bytes(&[b'c'; 250])
            .expect("write oversized single line");
        writer.flush().expect("flush oversized");
        assert_eq!(fs::read(&rotated_path).expect("read .1"), vec![b'b'; 60]);
        assert_eq!(fs::read(&log_path).expect("read log"), vec![b'c'; 250]);

        #[cfg(unix)]
        {
            use std::os::unix::fs::{PermissionsExt, symlink};
            use std::process::Command;

            assert_eq!(
                fs::metadata(&log_dir).expect("dir").permissions().mode() & 0o777,
                0o700
            );
            assert_eq!(
                fs::metadata(&log_path).expect("log").permissions().mode() & 0o777,
                0o600
            );
            assert_eq!(
                fs::metadata(&rotated_path)
                    .expect("rot")
                    .permissions()
                    .mode()
                    & 0o777,
                0o600
            );

            // Symlink at proxy.log rejected
            let sym_log = temp.path().join("sym.log");
            let victim = temp.path().join("victim.log");
            fs::write(&victim, "keep").expect("write victim");
            symlink(&victim, &sym_log).expect("symlink log");
            assert!(RotatingFileWriter::open(sym_log, 100).is_err());
            assert_eq!(fs::read_to_string(&victim).expect("read victim"), "keep");

            // Symlink at proxy.log.1 rejected
            let rot_dir = temp.path().join("rot_sym");
            fs::create_dir(&rot_dir).expect("mkdir");
            symlink(&victim, rot_dir.join(PROXY_LOG_ROTATED_FILENAME)).expect("symlink .1");
            assert!(RotatingFileWriter::open(rot_dir.join(PROXY_LOG_FILENAME), 100).is_err());

            // FIFO at proxy.log rejected without hanging
            let fifo_log = temp.path().join("fifo.log");
            let status = Command::new("mkfifo")
                .arg(&fifo_log)
                .status()
                .expect("mkfifo");
            assert!(status.success());
            assert!(RotatingFileWriter::open(fifo_log, 100).is_err());
        }
    }

    #[test]
    fn retry_rename_with_backoff_retries_transient_permission_denied() {
        let mut attempts = 0usize;
        let mut slept = Vec::new();
        retry_rename_with_backoff(
            &WINDOWS_RENAME_BACKOFFS,
            || {
                attempts += 1;
                if attempts < 3 {
                    Err(io::Error::new(ErrorKind::PermissionDenied, "locked"))
                } else {
                    Ok(())
                }
            },
            |d| slept.push(d),
        )
        .expect("should succeed on third attempt");
        assert_eq!(attempts, 3);
        assert_eq!(slept, WINDOWS_RENAME_BACKOFFS.to_vec());
    }
}
