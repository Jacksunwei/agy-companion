use std::fmt;
use std::fs::{self, File, TryLockError};
use std::io::ErrorKind;
use std::path::{Path, PathBuf};
use std::sync::RwLock;

use serde::{Deserialize, Serialize};

use crate::config::CoreError;
use crate::fs_safe::{
    self, AtomicWriteOptions, FileStamp, FsError, OpenRegularMode, ParentDirPolicy, RejectedReason,
    SymlinkPolicy, TargetFilePolicy,
};
use crate::redaction::mask_account_id;
use crate::validation::ApiKey;

pub const AUTH_SCHEMA_VERSION: u32 = 1;
pub const MAX_AUTH_FILE_BYTES: u64 = 64 * 1024;
pub const AUTH_DIR_NAME: &str = "auth";
pub const GEMINI_AUTH_FILENAME: &str = "gemini.json";
pub const CODEX_AUTH_FILENAME: &str = "codex.json";
pub const CODEX_REFRESH_LOCK_FILENAME: &str = ".codex-refresh.lock";
const MAX_ACCOUNT_ID_LEN: usize = 128;
const MAX_PLAN_TYPE_LEN: usize = 64;
const MAX_EMAIL_HINT_LEN: usize = 256;
const MAX_LAST_REFRESH_LEN: usize = 64;

const AUTH_WRITE_OPTIONS: AtomicWriteOptions = AtomicWriteOptions {
    file_mode: 0o600,
    dir_mode: 0o700,
    parent_policy: ParentDirPolicy::RejectSymlinkAndTighten,
    target_policy: TargetFilePolicy::RejectSymlinkAndNonRegular,
};

fn default_auth_schema_version() -> u32 {
    AUTH_SCHEMA_VERSION
}

/// Derives the sibling `auth/` directory path for `config_path`.
///
/// Validates `config_path` via `fs_safe::validate_path` *before* calling `.parent()` so
/// empty/whitespace paths, `..` traversal components, and NUL bytes fail closed.
pub fn auth_dir_for_config_path(config_path: &Path) -> Result<PathBuf, CoreError> {
    fs_safe::validate_path(config_path)?;
    let parent = config_path
        .parent()
        .filter(|dir| !dir.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    Ok(parent.join(AUTH_DIR_NAME))
}

/// Redacted secret string wrapper for provider tokens/credentials.
///
/// - `Debug` always emits `"SecretString([REDACTED])"`.
/// - Intentionally omits `Display`, `Deref`, and `AsRef<str>` to prevent accidental leaks.
/// - Callers must explicitly invoke `.expose()` when constructing outbound auth headers.
#[derive(Clone, PartialEq, Eq)]
pub struct SecretString(String);

impl SecretString {
    pub fn new(value: impl Into<String>) -> Self {
        Self(value.into())
    }

    pub fn expose(&self) -> &str {
        &self.0
    }

    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }
}

impl fmt::Debug for SecretString {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("SecretString([REDACTED])")
    }
}

impl Serialize for SecretString {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(&self.0)
    }
}

impl<'de> Deserialize<'de> for SecretString {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        String::deserialize(deserializer).map(Self)
    }
}

#[derive(Deserialize)]
struct RawGeminiCredentials {
    #[serde(default = "default_auth_schema_version")]
    schema_version: u32,
    #[serde(default)]
    api_key: Option<String>,
}

/// Stored Google AI Studio / Gemini API credentials persisted in `<auth_dir>/gemini.json`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GeminiCredentials {
    pub schema_version: u32,
    pub api_key: ApiKey,
}

impl GeminiCredentials {
    pub fn new(api_key: ApiKey) -> Self {
        Self {
            schema_version: AUTH_SCHEMA_VERSION,
            api_key,
        }
    }

    pub fn validate(&self) -> Result<(), CoreError> {
        if self.schema_version != AUTH_SCHEMA_VERSION {
            return Err(CoreError::UnsupportedAuthSchemaVersion {
                found: self.schema_version,
                supported: AUTH_SCHEMA_VERSION,
            });
        }
        Ok(())
    }

    /// Dual-layer disk parser preserving exact non-leaking `CoreError` variants:
    /// - Malformed JSON -> `CoreError::ParseAuth { path, line, column }` (zero secret echo)
    /// - Unsupported `schema_version` (`!= 1`) -> `CoreError::UnsupportedAuthSchemaVersion`
    /// - Control chars / non-ASCII / internal spaces -> `CoreError::InvalidApiKey`
    /// - Missing / `null` / empty / ASCII spaces / `PROXY_PLACEHOLDER_API_KEY` -> `Ok(None)`
    /// - Valid key -> `Ok(Some(GeminiCredentials))`
    pub fn from_json_str(raw: &str, path: &Path) -> Result<Option<Self>, CoreError> {
        let raw_creds: RawGeminiCredentials =
            serde_json::from_str(raw).map_err(|err| CoreError::ParseAuth {
                path: path.to_path_buf(),
                line: err.line(),
                column: err.column(),
            })?;

        if raw_creds.schema_version != AUTH_SCHEMA_VERSION {
            return Err(CoreError::UnsupportedAuthSchemaVersion {
                found: raw_creds.schema_version,
                supported: AUTH_SCHEMA_VERSION,
            });
        }

        let Some(ref raw_key) = raw_creds.api_key else {
            return Ok(None);
        };
        Ok(ApiKey::parse_from_disk(raw_key)?.map(Self::new))
    }

    pub fn from_json_bytes(bytes: &[u8], path: &Path) -> Result<Option<Self>, CoreError> {
        let raw = std::str::from_utf8(bytes).map_err(|_| FsError::InvalidUtf8 {
            path: path.to_path_buf(),
        })?;
        Self::from_json_str(raw, path)
    }
}

const MAX_JWT_PAYLOAD_B64_BYTES: usize = 32 * 1024;

fn decode_base64url_no_pad(encoded: &str) -> Option<Vec<u8>> {
    let bytes = encoded.as_bytes();
    if bytes.is_empty() || bytes.len() > MAX_JWT_PAYLOAD_B64_BYTES || bytes.len() % 4 == 1 {
        return None;
    }
    let mut out = Vec::with_capacity((bytes.len() / 4) * 3 + 2);
    let mut buf = 0u32;
    let mut bits = 0u8;
    for &b in bytes {
        let val = match b {
            b'A'..=b'Z' => b - b'A',
            b'a'..=b'z' => b - b'a' + 26,
            b'0'..=b'9' => b - b'0' + 52,
            b'-' => 62,
            b'_' => 63,
            b'=' => break,
            _ => return None,
        };
        buf = (buf << 6) | u32::from(val);
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push((buf >> bits) as u8);
        }
    }
    Some(out)
}

fn extract_jwt_email_claim(jwt: &str) -> Option<String> {
    let mut parts = jwt.split('.');
    let _header = parts.next()?;
    let payload_b64 = parts.next()?;
    let _sig = parts.next()?;
    if parts.next().is_some() {
        return None;
    }
    let payload_bytes = decode_base64url_no_pad(payload_b64)?;
    let parsed: serde_json::Value = serde_json::from_slice(&payload_bytes).ok()?;
    let obj = parsed.as_object()?;
    let raw = obj
        .get("email")
        .and_then(serde_json::Value::as_str)
        .or_else(|| {
            obj.get("https://api.openai.com/profile")
                .and_then(serde_json::Value::as_object)
                .and_then(|ns| ns.get("email"))
                .and_then(serde_json::Value::as_str)
        })?;
    let trimmed = raw.trim();
    if trimmed.is_empty()
        || trimmed.len() > MAX_EMAIL_HINT_LEN
        || trimmed.chars().any(|c| c.is_control() || c.is_whitespace())
    {
        return None;
    }
    let (local, domain) = trimmed.split_once('@')?;
    if local.is_empty() || domain.is_empty() || domain.contains('@') {
        return None;
    }
    Some(trimmed.to_string())
}

/// Authentication mode persisted inside `<auth_dir>/codex.json`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CodexAuthMode {
    #[serde(rename = "chatgpt_oauth")]
    ChatGptOAuth,
}

#[derive(Deserialize)]
struct RawCodexCredentials {
    #[serde(default = "default_auth_schema_version")]
    schema_version: u32,
    auth_mode: CodexAuthMode,
    access_token: SecretString,
    refresh_token: SecretString,
    #[serde(default)]
    id_token: Option<SecretString>,
    account_id: String,
    expires_at_ms: u64,
    #[serde(default)]
    plan_type: Option<String>,
    #[serde(default)]
    email_hint: Option<String>,
    last_refresh_utc: String,
}

fn is_header_safe_ascii(value: &str) -> bool {
    !value.is_empty() && value.bytes().all(|b| (0x21..=0x7E).contains(&b))
}

fn is_printable_ascii_non_empty(value: &str, max_len: usize) -> bool {
    !value.is_empty() && value.len() <= max_len && value.bytes().all(|b| (0x20..=0x7E).contains(&b))
}

/// Stored OpenAI Codex OAuth credentials persisted in `<auth_dir>/codex.json`.
///
/// `Debug` is safe to derive because `SecretString` always formats as
/// `"SecretString([REDACTED])"`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CodexCredentials {
    pub schema_version: u32,
    pub auth_mode: CodexAuthMode,
    pub access_token: SecretString,
    pub refresh_token: SecretString,
    pub id_token: Option<SecretString>,
    pub account_id: String,
    pub expires_at_ms: u64,
    pub plan_type: Option<String>,
    pub email_hint: Option<String>,
    pub last_refresh_utc: String,
}

impl CodexCredentials {
    /// Validates schema version and field safety without echoing any secret values in errors.
    pub fn validate(&self) -> Result<(), CoreError> {
        if self.schema_version != AUTH_SCHEMA_VERSION {
            return Err(CoreError::UnsupportedAuthSchemaVersion {
                found: self.schema_version,
                supported: AUTH_SCHEMA_VERSION,
            });
        }
        if !is_header_safe_ascii(self.access_token.expose()) {
            return Err(CoreError::InvalidAuthCredentials {
                reason: "codex access_token must be non-empty header-safe ASCII".to_string(),
            });
        }
        if !is_header_safe_ascii(self.refresh_token.expose()) {
            return Err(CoreError::InvalidAuthCredentials {
                reason: "codex refresh_token must be non-empty header-safe ASCII".to_string(),
            });
        }
        if let Some(ref id_token) = self.id_token
            && !is_header_safe_ascii(id_token.expose())
        {
            return Err(CoreError::InvalidAuthCredentials {
                reason: "codex id_token must be non-empty header-safe ASCII".to_string(),
            });
        }
        if !is_header_safe_ascii(&self.account_id) || self.account_id.len() > MAX_ACCOUNT_ID_LEN {
            return Err(CoreError::InvalidAuthCredentials {
                reason: format!(
                    "codex account_id must be non-empty header-safe ASCII (<= {MAX_ACCOUNT_ID_LEN} bytes)"
                ),
            });
        }
        if !is_printable_ascii_non_empty(&self.last_refresh_utc, MAX_LAST_REFRESH_LEN) {
            return Err(CoreError::InvalidAuthCredentials {
                reason: "codex last_refresh_utc must be non-empty printable ASCII".to_string(),
            });
        }
        if let Some(ref plan_type) = self.plan_type
            && (plan_type.is_empty()
                || plan_type.len() > MAX_PLAN_TYPE_LEN
                || plan_type.chars().any(char::is_control))
        {
            return Err(CoreError::InvalidAuthCredentials {
                reason: "codex plan_type must be non-empty and free of control characters"
                    .to_string(),
            });
        }
        if let Some(ref email_hint) = self.email_hint
            && (email_hint.is_empty()
                || email_hint.len() > MAX_EMAIL_HINT_LEN
                || email_hint.chars().any(char::is_control))
        {
            return Err(CoreError::InvalidAuthCredentials {
                reason: "codex email_hint must be non-empty and free of control characters"
                    .to_string(),
            });
        }
        Ok(())
    }

    /// Dual-layer disk parser preserving exact non-leaking `CoreError` variants:
    /// - Malformed JSON -> `CoreError::ParseAuth { path, line, column }` (zero secret echo)
    /// - Unsupported `schema_version` (`!= 1`) -> `CoreError::UnsupportedAuthSchemaVersion`
    /// - Invalid token/account/metadata syntax -> `CoreError::InvalidAuthCredentials` (zero secret echo)
    pub fn from_json_str(raw: &str, path: &Path) -> Result<Self, CoreError> {
        // Check `schema_version` cleanly even if future schema versions add/change fields.
        let version_probe: serde_json::Value =
            serde_json::from_str(raw).map_err(|err| CoreError::ParseAuth {
                path: path.to_path_buf(),
                line: err.line(),
                column: err.column(),
            })?;
        if let Some(found) = version_probe
            .get("schema_version")
            .and_then(serde_json::Value::as_u64)
            && found != u64::from(AUTH_SCHEMA_VERSION)
        {
            return Err(CoreError::UnsupportedAuthSchemaVersion {
                found: u32::try_from(found).unwrap_or(u32::MAX),
                supported: AUTH_SCHEMA_VERSION,
            });
        }

        let raw_creds: RawCodexCredentials =
            serde_json::from_str(raw).map_err(|err| CoreError::ParseAuth {
                path: path.to_path_buf(),
                line: err.line(),
                column: err.column(),
            })?;

        if raw_creds.schema_version != AUTH_SCHEMA_VERSION {
            return Err(CoreError::UnsupportedAuthSchemaVersion {
                found: raw_creds.schema_version,
                supported: AUTH_SCHEMA_VERSION,
            });
        }

        let creds = Self {
            schema_version: raw_creds.schema_version,
            auth_mode: raw_creds.auth_mode,
            access_token: raw_creds.access_token,
            refresh_token: raw_creds.refresh_token,
            id_token: raw_creds.id_token,
            account_id: raw_creds.account_id,
            expires_at_ms: raw_creds.expires_at_ms,
            plan_type: raw_creds.plan_type,
            email_hint: raw_creds.email_hint,
            last_refresh_utc: raw_creds.last_refresh_utc,
        };
        creds.validate()?;
        Ok(creds)
    }

    pub fn from_json_bytes(bytes: &[u8], path: &Path) -> Result<Self, CoreError> {
        let raw = std::str::from_utf8(bytes).map_err(|_| FsError::InvalidUtf8 {
            path: path.to_path_buf(),
        })?;
        Self::from_json_str(raw, path)
    }

    /// Returns the UI display label for the signed-in Codex account: full email from `email_hint`
    /// (upgrading legacy masked `***` hints from the stored `id_token`/`access_token` JWT `email`
    /// claim when present), otherwise a masked suffix of `account_id` (`...wxyz` or `"****"`).
    /// Never exposes raw account IDs or tokens.
    pub fn account_hint(&self) -> Option<String> {
        let stored = self
            .email_hint
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty());
        if let Some(hint) = stored
            && !hint.contains("***")
        {
            return Some(hint.to_string());
        }
        if let Some(jwt_email) = self
            .id_token
            .as_ref()
            .and_then(|tok| extract_jwt_email_claim(tok.expose()))
            .or_else(|| extract_jwt_email_claim(self.access_token.expose()))
        {
            return Some(jwt_email);
        }
        if let Some(hint) = stored {
            return Some(hint.to_string());
        }
        let masked = mask_account_id(&self.account_id);
        if masked.is_empty() {
            None
        } else {
            Some(masked)
        }
    }

    /// Formats `expires_at_ms` as an RFC 3339 UTC timestamp string (seconds precision),
    /// returning `None` if `expires_at_ms` is out of `chrono::DateTime` range.
    pub fn expires_at_rfc3339(&self) -> Option<String> {
        let millis = i64::try_from(self.expires_at_ms).ok()?;
        let dt = chrono::DateTime::from_timestamp_millis(millis)?;
        Some(dt.to_rfc3339_opts(chrono::SecondsFormat::Secs, true))
    }
}

/// Exclusive cross-process advisory lock over `<auth_dir>/.codex-refresh.lock` (`0600`)
/// serializing rotating OpenAI Codex refresh-token grants across `serve`, `companion-ui`, and `doctor`.
///
/// `Drop` explicitly unlocks the open file description before closing the descriptor so
/// concurrently spawning child processes cannot keep the `flock(2)` lock held across
/// `fork()`/`exec()`. The lock file is intentionally never unlinked on drop to avoid
/// classic flock-unlink inode races.
#[derive(Debug)]
pub struct CodexRefreshLock {
    file: File,
    path: PathBuf,
}

impl CodexRefreshLock {
    pub fn acquire(auth_dir: &Path) -> Result<Self, CoreError> {
        let (file, lock_path) = open_codex_refresh_lock_file(auth_dir)?;
        file.lock().map_err(|source| CoreError::Io {
            path: lock_path.clone(),
            source,
        })?;
        Ok(Self {
            file,
            path: lock_path,
        })
    }

    pub fn try_acquire(auth_dir: &Path) -> Result<Option<Self>, CoreError> {
        let (file, lock_path) = open_codex_refresh_lock_file(auth_dir)?;
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

    pub fn path(&self) -> &Path {
        &self.path
    }
}

impl Drop for CodexRefreshLock {
    fn drop(&mut self) {
        let _ = self.file.unlock();
    }
}

fn open_codex_refresh_lock_file(auth_dir: &Path) -> Result<(File, PathBuf), CoreError> {
    fs_safe::validate_path(auth_dir)?;
    fs_safe::inspect_existing_dir(auth_dir)?;
    fs_safe::ensure_dir_no_symlink(auth_dir, 0o700)?;

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;

        let normalized = fs_safe::normalize_path(auth_dir);
        fs::set_permissions(&normalized, fs::Permissions::from_mode(0o700)).map_err(|source| {
            CoreError::Io {
                path: normalized,
                source,
            }
        })?;
    }

    let lock_path = auth_dir.join(CODEX_REFRESH_LOCK_FILENAME);
    let (file, _meta) =
        fs_safe::open_verified_regular_file_0600(&lock_path, OpenRegularMode::ReadWriteCreate)?;
    Ok((file, lock_path))
}

/// Snapshots the metadata stamp of `<auth_dir>/<filename>`, verifying `auth_dir` via
/// `fs_safe::inspect_existing_dir` *before* inspecting the leaf file so symlinked or
/// non-directory `auth/` directories fail closed instead of being traversed.
pub fn inspect_auth_file_stamp(auth_dir: &Path, filename: &str) -> FileStamp {
    match fs_safe::inspect_existing_dir(auth_dir) {
        Ok(None) => FileStamp::Missing,
        Ok(Some(_)) => fs_safe::inspect_file_stamp(&auth_dir.join(filename)),
        Err(FsError::SymlinkRejected { .. }) => FileStamp::Rejected(RejectedReason::Symlink),
        Err(FsError::NotDirectory { .. }) => FileStamp::Rejected(RejectedReason::NotRegularFile),
        Err(FsError::Io { source, .. }) => FileStamp::Unreadable(source.kind()),
        Err(_) => FileStamp::Unreadable(ErrorKind::InvalidInput),
    }
}

/// Reads verified bytes for `<auth_dir>/<filename>` matching `stamp`, checking `auth_dir`
/// via `fs_safe::inspect_existing_dir` *before* opening the file.
pub fn read_verified_auth_bytes_for_stamp(
    auth_dir: &Path,
    filename: &str,
    stamp: &FileStamp,
) -> Result<Option<Vec<u8>>, FsError> {
    if fs_safe::inspect_existing_dir(auth_dir)?.is_none() {
        return Ok(None);
    }
    fs_safe::read_verified_bytes_for_stamp(&auth_dir.join(filename), stamp, MAX_AUTH_FILE_BYTES)
}

/// Unified credential storage abstraction shared by all providers.
pub trait AuthStore: Send + Sync {
    fn auth_dir(&self) -> Option<&Path>;
    fn load_gemini(&self) -> Result<Option<GeminiCredentials>, CoreError>;
    fn save_gemini(&self, creds: &GeminiCredentials) -> Result<(), CoreError>;
    fn clear_gemini(&self) -> Result<(), CoreError>;
    fn load_codex(&self) -> Result<Option<CodexCredentials>, CoreError>;
    fn save_codex(&self, creds: &CodexCredentials) -> Result<(), CoreError>;
    fn clear_codex(&self) -> Result<(), CoreError>;
}

/// Filesystem-backed `AuthStore` persisting provider secrets under `<config_dir>/auth/`
/// with `0700` directory and `0600` atomic regular files.
#[derive(Debug, Clone)]
pub struct FileAuthStore {
    auth_dir: PathBuf,
}

impl FileAuthStore {
    pub fn new(auth_dir: PathBuf) -> Result<Self, CoreError> {
        fs_safe::validate_path(&auth_dir)?;
        Ok(Self { auth_dir })
    }

    pub fn for_config_path(config_path: &Path) -> Result<Self, CoreError> {
        let auth_dir = auth_dir_for_config_path(config_path)?;
        Self::new(auth_dir)
    }

    pub fn gemini_path(&self) -> PathBuf {
        self.auth_dir.join(GEMINI_AUTH_FILENAME)
    }

    pub fn codex_path(&self) -> PathBuf {
        self.auth_dir.join(CODEX_AUTH_FILENAME)
    }

    pub fn codex_refresh_lock_path(&self) -> PathBuf {
        self.auth_dir.join(CODEX_REFRESH_LOCK_FILENAME)
    }

    pub fn inspect_gemini_stamp(&self) -> FileStamp {
        inspect_auth_file_stamp(&self.auth_dir, GEMINI_AUTH_FILENAME)
    }

    pub fn inspect_codex_stamp(&self) -> FileStamp {
        inspect_auth_file_stamp(&self.auth_dir, CODEX_AUTH_FILENAME)
    }

    pub fn load_gemini_with_stamp(
        &self,
    ) -> Result<(FileStamp, Option<GeminiCredentials>), CoreError> {
        if fs_safe::inspect_existing_dir(&self.auth_dir)?.is_none() {
            return Ok((FileStamp::Missing, None));
        }
        let path = self.gemini_path();
        let stamp = fs_safe::inspect_file_stamp(&path);
        let Some(bytes) =
            fs_safe::read_verified_bytes_for_stamp(&path, &stamp, MAX_AUTH_FILE_BYTES)?
        else {
            return Ok((FileStamp::Missing, None));
        };
        let creds = GeminiCredentials::from_json_bytes(&bytes, &path)?;
        Ok((stamp, creds))
    }

    pub fn load_codex_with_stamp(
        &self,
    ) -> Result<(FileStamp, Option<CodexCredentials>), CoreError> {
        if fs_safe::inspect_existing_dir(&self.auth_dir)?.is_none() {
            return Ok((FileStamp::Missing, None));
        }
        let path = self.codex_path();
        let stamp = fs_safe::inspect_file_stamp(&path);
        let Some(bytes) =
            fs_safe::read_verified_bytes_for_stamp(&path, &stamp, MAX_AUTH_FILE_BYTES)?
        else {
            return Ok((FileStamp::Missing, None));
        };
        let creds = CodexCredentials::from_json_bytes(&bytes, &path)?;
        Ok((stamp, Some(creds)))
    }
}

impl AuthStore for FileAuthStore {
    fn auth_dir(&self) -> Option<&Path> {
        Some(&self.auth_dir)
    }

    fn load_gemini(&self) -> Result<Option<GeminiCredentials>, CoreError> {
        if fs_safe::inspect_existing_dir(&self.auth_dir)?.is_none() {
            return Ok(None);
        }
        let path = self.gemini_path();
        let Some((raw, _meta)) = fs_safe::read_regular_file_utf8_bounded(
            &path,
            MAX_AUTH_FILE_BYTES,
            SymlinkPolicy::Reject,
        )?
        else {
            return Ok(None);
        };
        GeminiCredentials::from_json_str(&raw, &path)
    }

    fn save_gemini(&self, creds: &GeminiCredentials) -> Result<(), CoreError> {
        creds.validate()?;
        fs_safe::inspect_existing_dir(&self.auth_dir)?;

        let mut payload =
            serde_json::to_string_pretty(creds).map_err(|err| CoreError::SerializeAuth {
                reason: err.to_string(),
            })?;
        payload.push('\n');

        fs_safe::write_atomic(&self.gemini_path(), payload.as_bytes(), AUTH_WRITE_OPTIONS)?;
        Ok(())
    }

    fn clear_gemini(&self) -> Result<(), CoreError> {
        if fs_safe::inspect_existing_dir(&self.auth_dir)?.is_none() {
            return Ok(());
        }
        let path = self.gemini_path();
        if fs_safe::inspect_regular_file_if_exists(&path, None)?.is_none() {
            return Ok(());
        }
        match fs::remove_file(&path) {
            Ok(()) => Ok(()),
            Err(err) if err.kind() == ErrorKind::NotFound => Ok(()),
            Err(source) => Err(CoreError::Io { path, source }),
        }
    }

    fn load_codex(&self) -> Result<Option<CodexCredentials>, CoreError> {
        if fs_safe::inspect_existing_dir(&self.auth_dir)?.is_none() {
            return Ok(None);
        }
        let path = self.codex_path();
        let Some((raw, _meta)) = fs_safe::read_regular_file_utf8_bounded(
            &path,
            MAX_AUTH_FILE_BYTES,
            SymlinkPolicy::Reject,
        )?
        else {
            return Ok(None);
        };
        CodexCredentials::from_json_str(&raw, &path).map(Some)
    }

    fn save_codex(&self, creds: &CodexCredentials) -> Result<(), CoreError> {
        creds.validate()?;
        fs_safe::inspect_existing_dir(&self.auth_dir)?;

        let mut payload =
            serde_json::to_string_pretty(creds).map_err(|err| CoreError::SerializeAuth {
                reason: err.to_string(),
            })?;
        payload.push('\n');

        fs_safe::write_atomic(&self.codex_path(), payload.as_bytes(), AUTH_WRITE_OPTIONS)?;
        Ok(())
    }

    fn clear_codex(&self) -> Result<(), CoreError> {
        if fs_safe::inspect_existing_dir(&self.auth_dir)?.is_none() {
            return Ok(());
        }
        let path = self.codex_path();
        if fs_safe::inspect_regular_file_if_exists(&path, None)?.is_none() {
            return Ok(());
        }
        match fs::remove_file(&path) {
            Ok(()) => Ok(()),
            Err(err) if err.kind() == ErrorKind::NotFound => Ok(()),
            Err(source) => Err(CoreError::Io { path, source }),
        }
    }
}

/// In-memory `AuthStore` for fast hermetic unit tests.
#[derive(Debug, Default)]
pub struct MemoryAuthStore {
    gemini: RwLock<Option<GeminiCredentials>>,
    codex: RwLock<Option<CodexCredentials>>,
}

impl MemoryAuthStore {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn with_gemini_key(api_key: ApiKey) -> Self {
        Self {
            gemini: RwLock::new(Some(GeminiCredentials::new(api_key))),
            codex: RwLock::new(None),
        }
    }

    pub fn with_codex_credentials(creds: CodexCredentials) -> Self {
        Self {
            gemini: RwLock::new(None),
            codex: RwLock::new(Some(creds)),
        }
    }
}

impl AuthStore for MemoryAuthStore {
    fn auth_dir(&self) -> Option<&Path> {
        None
    }

    fn load_gemini(&self) -> Result<Option<GeminiCredentials>, CoreError> {
        let guard = self.gemini.read().unwrap_or_else(|e| e.into_inner());
        Ok(guard.clone())
    }

    fn save_gemini(&self, creds: &GeminiCredentials) -> Result<(), CoreError> {
        creds.validate()?;
        let mut guard = self.gemini.write().unwrap_or_else(|e| e.into_inner());
        *guard = Some(creds.clone());
        Ok(())
    }

    fn clear_gemini(&self) -> Result<(), CoreError> {
        let mut guard = self.gemini.write().unwrap_or_else(|e| e.into_inner());
        *guard = None;
        Ok(())
    }

    fn load_codex(&self) -> Result<Option<CodexCredentials>, CoreError> {
        let guard = self.codex.read().unwrap_or_else(|e| e.into_inner());
        Ok(guard.clone())
    }

    fn save_codex(&self, creds: &CodexCredentials) -> Result<(), CoreError> {
        creds.validate()?;
        let mut guard = self.codex.write().unwrap_or_else(|e| e.into_inner());
        *guard = Some(creds.clone());
        Ok(())
    }

    fn clear_codex(&self) -> Result<(), CoreError> {
        let mut guard = self.codex.write().unwrap_or_else(|e| e.into_inner());
        *guard = None;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::path::{Path, PathBuf};

    use tempfile::TempDir;

    use super::*;
    use crate::validation::PROXY_PLACEHOLDER_API_KEY;

    #[test]
    fn auth_dir_for_config_path_derives_sibling_and_rejects_traversal_nul_empty() {
        assert_eq!(
            auth_dir_for_config_path(Path::new("/tmp/agy-companion/config.json")).unwrap(),
            PathBuf::from("/tmp/agy-companion/auth")
        );
        assert_eq!(
            auth_dir_for_config_path(Path::new("config.json")).unwrap(),
            PathBuf::from("./auth")
        );
        assert_eq!(
            auth_dir_for_config_path(Path::new("/config.json")).unwrap(),
            PathBuf::from("/auth")
        );

        for (bad, expect_traversal, expect_nul, expect_empty) in [
            ("", false, false, true),
            ("   ", false, false, true),
            ("..", true, false, false),
            ("../config.json", true, false, false),
            ("/tmp/a/..", true, false, false),
            ("/tmp/a/../config.json", true, false, false),
            ("/tmp/bad\0cfg", false, true, false),
        ] {
            let err = auth_dir_for_config_path(Path::new(bad))
                .expect_err("expected invalid config path rejection");
            if expect_traversal {
                assert!(matches!(err, CoreError::Fs(FsError::ParentDirTraversal)));
            } else if expect_nul {
                assert!(matches!(err, CoreError::Fs(FsError::ContainsNul)));
            } else if expect_empty {
                assert!(matches!(err, CoreError::Fs(FsError::EmptyPath)));
            }
            assert!(FileAuthStore::for_config_path(Path::new(bad)).is_err());
        }
    }

    #[test]
    fn secret_string_redacts_debug_and_roundtrips_json() {
        let secret = SecretString::new("top-secret-token-xyz");
        assert!(!secret.is_empty());
        assert_eq!(secret.expose(), "top-secret-token-xyz");
        assert_eq!(format!("{secret:?}"), "SecretString([REDACTED])");
        assert!(!format!("{secret:?}").contains("top-secret-token-xyz"));

        let empty = SecretString::new("");
        assert!(empty.is_empty());

        let json = serde_json::to_string(&secret).expect("serialize");
        assert_eq!(json, "\"top-secret-token-xyz\"");
        let parsed: SecretString = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(parsed, secret);
    }

    #[test]
    fn gemini_credentials_disk_load_validation_schema_and_placeholder_quarantine() {
        let path = Path::new("gemini.json");

        // Valid v1 credentials (including padded ASCII spaces)
        let valid = GeminiCredentials::from_json_str(
            r#"{"schema_version": 1, "api_key": "  AIzaSyValidDiskKey01  "}"#,
            path,
        )
        .expect("valid creds")
        .expect("some creds");
        assert_eq!(valid.schema_version, AUTH_SCHEMA_VERSION);
        assert_eq!(valid.api_key.expose(), "AIzaSyValidDiskKey01");
        assert!(!format!("{valid:?}").contains("SyValidDiskKey01"));

        // Missing/null/blank/placeholder normalize to Ok(None)
        for none_json in [
            r#"{}"#,
            r#"{"schema_version": 1}"#,
            r#"{"schema_version": 1, "api_key": null}"#,
            r#"{"schema_version": 1, "api_key": ""}"#,
            r#"{"schema_version": 1, "api_key": "   "}"#,
            &format!(r#"{{"schema_version": 1, "api_key": "{PROXY_PLACEHOLDER_API_KEY}"}}"#),
            &format!(r#"{{"schema_version": 1, "api_key": "  {PROXY_PLACEHOLDER_API_KEY}  "}}"#),
        ] {
            let loaded = GeminiCredentials::from_json_str(none_json, path)
                .unwrap_or_else(|err| panic!("expected Ok(None) for {none_json}: {err}"));
            assert!(
                loaded.is_none(),
                "expected None for {none_json}, got {loaded:?}"
            );
        }

        // Unsupported schema_version fails closed
        for bad_version in [0u32, 2, 99] {
            let json = format!(
                r#"{{"schema_version": {bad_version}, "api_key": "AIzaSyValidDiskKey01"}}"#
            );
            let err = GeminiCredentials::from_json_str(&json, path)
                .expect_err("unsupported schema must fail");
            assert!(matches!(
                err,
                CoreError::UnsupportedAuthSchemaVersion {
                    found,
                    supported: 1
                } if found == bad_version
            ));
        }

        // Control chars / internal spaces / non-ASCII fail with InvalidApiKey and zero secret echo
        for bad_key in [
            r"AIzaSyBadSecret\r\nKey",
            r"AIzaSyBadSecret\tKey",
            "AIzaSyBadSecret Key",
            "AIzaSyBadSecret\u{00A0}Key",
            r"\tagy-companion-local\n",
        ] {
            let json = format!(r#"{{"schema_version": 1, "api_key": "{bad_key}"}}"#);
            let err = GeminiCredentials::from_json_str(&json, path)
                .expect_err("expected non-header-safe key rejection");
            assert!(matches!(err, CoreError::InvalidApiKey));
            assert!(!err.to_string().contains("BadSecret"));
            assert!(!format!("{err:?}").contains("BadSecret"));
        }

        // Corrupt JSON fails with ParseAuth and zero secret echo
        let corrupt = r#"{"schema_version": 1, "api_key": "AIzaSyNeverLeakCorrupt", broken"#;
        let err = GeminiCredentials::from_json_str(corrupt, path).expect_err("corrupt json");
        assert!(matches!(err, CoreError::ParseAuth { .. }));
        assert!(!err.to_string().contains("NeverLeakCorrupt"));
        assert!(!format!("{err:?}").contains("NeverLeakCorrupt"));

        // Invalid UTF-8 bytes fail with FsError::InvalidUtf8
        assert!(matches!(
            GeminiCredentials::from_json_bytes(&[0xff, 0xfe], path),
            Err(CoreError::Fs(FsError::InvalidUtf8 { .. }))
        ));
    }

    #[test]
    fn file_auth_store_roundtrip_permissions_and_idempotent_clear() {
        let tmp = TempDir::new().expect("temp dir");
        let cfg_path = tmp.path().join("agy-companion").join("config.json");
        let store = FileAuthStore::for_config_path(&cfg_path).expect("store");
        let auth_dir = store.auth_dir().expect("auth_dir").to_path_buf();

        // Day-0 missing auth/ returns Ok(None) and does NOT create auth/
        assert_eq!(store.load_gemini().expect("day-0 load"), None);
        assert_eq!(store.inspect_gemini_stamp(), FileStamp::Missing);
        assert_eq!(
            store.load_gemini_with_stamp().expect("day-0 load_stamp"),
            (FileStamp::Missing, None)
        );
        assert!(!auth_dir.exists(), "load must not create auth/ on Day-0");
        store.clear_gemini().expect("idempotent clear on missing");
        assert!(!auth_dir.exists());

        // Save creates auth/ (0700) and gemini.json (0600) with zero leftover temp files
        let creds = GeminiCredentials::new(ApiKey::parse_strict("AIzaSyStoreRoundTrip01").unwrap());
        store.save_gemini(&creds).expect("save gemini");
        assert_eq!(store.load_gemini().expect("reload"), Some(creds.clone()));

        let (stamp, stamped_creds) = store.load_gemini_with_stamp().expect("load with stamp");
        assert!(matches!(stamp, FileStamp::Present { .. }));
        assert_eq!(stamped_creds, Some(creds.clone()));

        let leftover_temps: Vec<_> = fs::read_dir(&auth_dir)
            .expect("read auth_dir")
            .filter_map(|e| e.ok())
            .filter(|e| e.file_name().to_string_lossy().contains(".tmp."))
            .collect();
        assert!(leftover_temps.is_empty());

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;

            let dir_mode = fs::metadata(&auth_dir).unwrap().permissions().mode() & 0o777;
            let file_mode = fs::metadata(store.gemini_path())
                .unwrap()
                .permissions()
                .mode()
                & 0o777;
            assert_eq!(dir_mode, 0o700);
            assert_eq!(file_mode, 0o600);

            // Re-saving tightens loosened 0755 dir and 0644 file back to 0700 / 0600
            fs::set_permissions(&auth_dir, fs::Permissions::from_mode(0o755)).unwrap();
            fs::set_permissions(store.gemini_path(), fs::Permissions::from_mode(0o644)).unwrap();
            let rotated =
                GeminiCredentials::new(ApiKey::parse_strict("AIzaSyStoreRoundTrip02").unwrap());
            store.save_gemini(&rotated).expect("resave tightens");
            assert_eq!(
                fs::metadata(&auth_dir).unwrap().permissions().mode() & 0o777,
                0o700
            );
            assert_eq!(
                fs::metadata(store.gemini_path())
                    .unwrap()
                    .permissions()
                    .mode()
                    & 0o777,
                0o600
            );
            assert_eq!(store.load_gemini().unwrap(), Some(rotated));
        }

        // Invalid schema_version on save fails without mutating disk
        let bad_creds = GeminiCredentials {
            schema_version: 99,
            api_key: ApiKey::parse_strict("AIzaSyBadVersionKey01").unwrap(),
        };
        assert!(matches!(
            store.save_gemini(&bad_creds),
            Err(CoreError::UnsupportedAuthSchemaVersion {
                found: 99,
                supported: 1
            })
        ));

        // Clear removes gemini.json and is idempotent
        store.clear_gemini().expect("clear");
        assert!(!store.gemini_path().exists());
        assert_eq!(store.load_gemini().expect("after clear"), None);
        store.clear_gemini().expect("second clear idempotent");
    }

    #[test]
    fn file_auth_store_rejects_oversized_non_regular_and_symlinks_fail_closed() {
        let tmp = TempDir::new().expect("temp dir");

        // 1. Oversized (> 64 KiB) gemini.json rejected
        let oversized_store =
            FileAuthStore::for_config_path(&tmp.path().join("oversized").join("config.json"))
                .unwrap();
        let oversized_dir = oversized_store.auth_dir().unwrap().to_path_buf();
        fs::create_dir_all(&oversized_dir).unwrap();
        fs::write(
            oversized_store.gemini_path(),
            vec![b' '; (MAX_AUTH_FILE_BYTES + 1) as usize],
        )
        .unwrap();
        assert!(matches!(
            oversized_store.load_gemini(),
            Err(CoreError::Fs(FsError::SizeLimitExceeded {
                max_bytes: MAX_AUTH_FILE_BYTES,
                ..
            }))
        ));
        assert!(matches!(
            oversized_store.load_gemini_with_stamp(),
            Err(CoreError::Fs(FsError::SizeLimitExceeded {
                max_bytes: MAX_AUTH_FILE_BYTES,
                ..
            }))
        ));

        // 2. Directory at gemini.json path rejected on load, save, clear, and stamp
        let dir_as_file_store =
            FileAuthStore::for_config_path(&tmp.path().join("dir_as_file").join("config.json"))
                .unwrap();
        fs::create_dir_all(dir_as_file_store.gemini_path()).unwrap();
        assert_eq!(
            dir_as_file_store.inspect_gemini_stamp(),
            FileStamp::Rejected(RejectedReason::NotRegularFile)
        );
        assert!(matches!(
            dir_as_file_store.load_gemini(),
            Err(CoreError::Fs(FsError::NotRegularFile { .. }))
        ));
        let dummy = GeminiCredentials::new(ApiKey::parse_strict("AIzaSyDummyCredKey01").unwrap());
        assert!(matches!(
            dir_as_file_store.save_gemini(&dummy),
            Err(CoreError::Fs(FsError::NotRegularFile { .. }))
        ));
        assert!(matches!(
            dir_as_file_store.clear_gemini(),
            Err(CoreError::Fs(FsError::NotRegularFile { .. }))
        ));

        // 3. Regular file at auth/ path rejected on load, save, clear, and stamp
        let file_as_dir_root = tmp.path().join("file_as_dir");
        fs::create_dir_all(&file_as_dir_root).unwrap();
        fs::write(file_as_dir_root.join("auth"), "not-a-dir").unwrap();
        let file_as_dir_store =
            FileAuthStore::for_config_path(&file_as_dir_root.join("config.json")).unwrap();
        assert_eq!(
            file_as_dir_store.inspect_gemini_stamp(),
            FileStamp::Rejected(RejectedReason::NotRegularFile)
        );
        assert!(matches!(
            file_as_dir_store.load_gemini(),
            Err(CoreError::Fs(FsError::NotDirectory { .. }))
        ));
        assert!(matches!(
            file_as_dir_store.save_gemini(&dummy),
            Err(CoreError::Fs(FsError::NotDirectory { .. }))
        ));
        assert!(matches!(
            file_as_dir_store.clear_gemini(),
            Err(CoreError::Fs(FsError::NotDirectory { .. }))
        ));

        #[cfg(unix)]
        {
            use std::os::unix::fs::symlink;

            // 4. Symlinked gemini.json (pointing to victim file) rejected without mutating victim
            let victim = tmp.path().join("victim.json");
            let victim_sentinel = r#"{"schema_version":1,"api_key":"AIzaSyVictimKeep0001"}"#;
            fs::write(&victim, victim_sentinel).unwrap();

            let sym_file_store =
                FileAuthStore::for_config_path(&tmp.path().join("sym_file").join("config.json"))
                    .unwrap();
            fs::create_dir_all(sym_file_store.auth_dir().unwrap()).unwrap();
            symlink(&victim, sym_file_store.gemini_path()).unwrap();

            assert_eq!(
                sym_file_store.inspect_gemini_stamp(),
                FileStamp::Rejected(RejectedReason::Symlink)
            );
            assert!(matches!(
                sym_file_store.load_gemini(),
                Err(CoreError::Fs(FsError::SymlinkRejected { .. }))
            ));
            assert!(matches!(
                sym_file_store.save_gemini(&dummy),
                Err(CoreError::Fs(FsError::SymlinkRejected { .. }))
            ));
            assert!(matches!(
                sym_file_store.clear_gemini(),
                Err(CoreError::Fs(FsError::SymlinkRejected { .. }))
            ));
            assert_eq!(fs::read_to_string(&victim).unwrap(), victim_sentinel);

            // 5. Dangling symlink at gemini.json rejected
            let dangling_store =
                FileAuthStore::for_config_path(&tmp.path().join("dangling").join("config.json"))
                    .unwrap();
            fs::create_dir_all(dangling_store.auth_dir().unwrap()).unwrap();
            symlink(
                tmp.path().join("nonexistent-target"),
                dangling_store.gemini_path(),
            )
            .unwrap();
            assert_eq!(
                dangling_store.inspect_gemini_stamp(),
                FileStamp::Rejected(RejectedReason::Symlink)
            );
            assert!(matches!(
                dangling_store.load_gemini(),
                Err(CoreError::Fs(FsError::SymlinkRejected { .. }))
            ));
            assert!(matches!(
                dangling_store.save_gemini(&dummy),
                Err(CoreError::Fs(FsError::SymlinkRejected { .. }))
            ));
            assert!(matches!(
                dangling_store.clear_gemini(),
                Err(CoreError::Fs(FsError::SymlinkRejected { .. }))
            ));

            // 6. Symlinked auth/ directory rejected on ALL operations (victim dir untouched)
            let victim_dir = tmp.path().join("victim_auth_dir");
            fs::create_dir_all(&victim_dir).unwrap();
            let victim_in_dir = victim_dir.join(GEMINI_AUTH_FILENAME);
            fs::write(&victim_in_dir, victim_sentinel).unwrap();

            let sym_dir_root = tmp.path().join("sym_auth_dir");
            fs::create_dir_all(&sym_dir_root).unwrap();
            symlink(&victim_dir, sym_dir_root.join(AUTH_DIR_NAME)).unwrap();
            let sym_dir_store =
                FileAuthStore::for_config_path(&sym_dir_root.join("config.json")).unwrap();

            assert_eq!(
                sym_dir_store.inspect_gemini_stamp(),
                FileStamp::Rejected(RejectedReason::Symlink)
            );
            assert!(matches!(
                sym_dir_store.load_gemini(),
                Err(CoreError::Fs(FsError::SymlinkRejected { .. }))
            ));
            assert!(matches!(
                sym_dir_store.load_gemini_with_stamp(),
                Err(CoreError::Fs(FsError::SymlinkRejected { .. }))
            ));
            assert!(matches!(
                sym_dir_store.save_gemini(&dummy),
                Err(CoreError::Fs(FsError::SymlinkRejected { .. }))
            ));
            assert!(matches!(
                sym_dir_store.clear_gemini(),
                Err(CoreError::Fs(FsError::SymlinkRejected { .. }))
            ));
            assert!(matches!(
                read_verified_auth_bytes_for_stamp(
                    sym_dir_store.auth_dir().unwrap(),
                    GEMINI_AUTH_FILENAME,
                    &FileStamp::Missing,
                ),
                Err(FsError::SymlinkRejected { .. })
            ));
            assert_eq!(fs::read_to_string(&victim_in_dir).unwrap(), victim_sentinel);

            // 7. FIFO at gemini.json rejected without blocking
            let fifo_store =
                FileAuthStore::for_config_path(&tmp.path().join("fifo_case").join("config.json"))
                    .unwrap();
            fs::create_dir_all(fifo_store.auth_dir().unwrap()).unwrap();
            let fifo_path = fifo_store.gemini_path();
            let status = std::process::Command::new("mkfifo")
                .arg(&fifo_path)
                .status()
                .expect("mkfifo");
            assert!(status.success());

            let (tx, rx) = std::sync::mpsc::channel();
            let store_clone = fifo_store.clone();
            let handle = std::thread::spawn(move || {
                let _ = tx.send(store_clone.load_gemini());
            });
            let res = rx
                .recv_timeout(std::time::Duration::from_secs(1))
                .expect("load_gemini must not hang on FIFO");
            handle.join().expect("join");
            assert!(matches!(
                res,
                Err(CoreError::Fs(FsError::NotRegularFile { .. }))
            ));
            assert_eq!(
                fifo_store.inspect_gemini_stamp(),
                FileStamp::Rejected(RejectedReason::NotRegularFile)
            );
        }
    }

    #[test]
    fn memory_auth_store_roundtrip_and_validation() {
        let store = MemoryAuthStore::new();
        assert_eq!(store.auth_dir(), None);
        assert_eq!(store.load_gemini().unwrap(), None);

        let creds = GeminiCredentials::new(ApiKey::parse_strict("AIzaSyMemKey0001").unwrap());
        store.save_gemini(&creds).unwrap();
        assert_eq!(store.load_gemini().unwrap(), Some(creds.clone()));

        let bad = GeminiCredentials {
            schema_version: 2,
            api_key: ApiKey::parse_strict("AIzaSyMemKey0002").unwrap(),
        };
        assert!(matches!(
            store.save_gemini(&bad),
            Err(CoreError::UnsupportedAuthSchemaVersion {
                found: 2,
                supported: 1
            })
        ));
        assert_eq!(store.load_gemini().unwrap(), Some(creds));

        store.clear_gemini().unwrap();
        assert_eq!(store.load_gemini().unwrap(), None);

        let seeded =
            MemoryAuthStore::with_gemini_key(ApiKey::parse_strict("AIzaSyMemSeeded01").unwrap());
        assert_eq!(
            seeded.load_gemini().unwrap().unwrap().api_key.expose(),
            "AIzaSyMemSeeded01"
        );
    }

    fn sample_codex_credentials() -> CodexCredentials {
        CodexCredentials {
            schema_version: AUTH_SCHEMA_VERSION,
            auth_mode: CodexAuthMode::ChatGptOAuth,
            access_token: SecretString::new("eyJhbGciOiJSUzI1NiJ9.codex_access_secret.sig"),
            refresh_token: SecretString::new("rt_codex_refresh_secret"),
            id_token: Some(SecretString::new(
                "eyJhbGciOiJSUzI1NiJ9.codex_id_secret.sig",
            )),
            account_id: "acct_1234567890wxyz".to_string(),
            expires_at_ms: 1_900_000_000_000,
            plan_type: Some("pro".to_string()),
            email_hint: Some("é***@example.com".to_string()),
            last_refresh_utc: "2026-11-26T22:00:00Z".to_string(),
        }
    }

    #[test]
    fn codex_credentials_json_roundtrip_wire_mode_redacted_debug_and_validation() {
        let path = Path::new("codex.json");
        let creds = sample_codex_credentials();

        let debug_str = format!("{creds:?}");
        for secret in [
            "codex_access_secret",
            "rt_codex_refresh_secret",
            "codex_id_secret",
        ] {
            assert!(
                !debug_str.contains(secret),
                "CodexCredentials Debug leaked '{secret}': {debug_str}"
            );
        }

        let json = serde_json::to_string_pretty(&creds).expect("serialize codex creds");
        let val: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert_eq!(val["schema_version"], 1);
        assert_eq!(val["auth_mode"], "chatgpt_oauth");
        assert_eq!(val["account_id"], "acct_1234567890wxyz");
        assert_eq!(val["plan_type"], "pro");
        assert_eq!(val["email_hint"], "é***@example.com");

        let parsed = CodexCredentials::from_json_str(&json, path).expect("roundtrip parse");
        assert_eq!(parsed, creds);
        assert_eq!(parsed.account_hint().as_deref(), Some("é***@example.com"));

        let mut no_email = creds.clone();
        no_email.email_hint = None;
        assert_eq!(no_email.account_hint().as_deref(), Some("...wxyz"));

        // Legacy masked email_hint upgrades automatically to full email from id_token / access_token JWT
        let mut legacy_masked = creds.clone();
        legacy_masked.email_hint = Some("j***@example.com".to_string());
        // {"email":"jack@example.com"} -> eyJlbWFpbCI6ImphY2tAZXhhbXBsZS5jb20ifQ
        legacy_masked.id_token = Some(SecretString::new(
            "eyJhbGciOiJSUzI1NiJ9.eyJlbWFpbCI6ImphY2tAZXhhbXBsZS5jb20ifQ.sig",
        ));
        assert_eq!(
            legacy_masked.account_hint().as_deref(),
            Some("jack@example.com")
        );

        let mut full_email = creds.clone();
        full_email.email_hint = Some("alice@example.com".to_string());
        assert_eq!(
            full_email.account_hint().as_deref(),
            Some("alice@example.com")
        );

        // Unsupported schema_version fails closed even if payload omits future fields
        for bad_ver in [0u32, 2, 99] {
            let err = CodexCredentials::from_json_str(
                &format!(r#"{{"schema_version": {bad_ver}, "future_field": true}}"#),
                path,
            )
            .expect_err("unsupported version must fail");
            assert!(matches!(
                err,
                CoreError::UnsupportedAuthSchemaVersion {
                    found,
                    supported: 1
                } if found == bad_ver
            ));
        }

        // Corrupt JSON fails with ParseAuth and zero secret echo
        let corrupt = r#"{"schema_version": 1, "access_token": "tok_never_echo_corrupt", broken"#;
        let err = CodexCredentials::from_json_str(corrupt, path).expect_err("corrupt json");
        assert!(matches!(err, CoreError::ParseAuth { .. }));
        assert!(!err.to_string().contains("tok_never_echo_corrupt"));
        assert!(!format!("{err:?}").contains("tok_never_echo_corrupt"));

        // Invalid UTF-8 bytes fail with FsError::InvalidUtf8
        assert!(matches!(
            CodexCredentials::from_json_bytes(&[0xff, 0xfe], path),
            Err(CoreError::Fs(FsError::InvalidUtf8 { .. }))
        ));

        // Non-header-safe tokens/account_id fail closed without echoing secret values
        let bad_cases: [fn(&mut CodexCredentials); 11] = [
            |c| c.access_token = SecretString::new(""),
            |c| c.access_token = SecretString::new("tok_secret\r\nX-Inject: 1"),
            |c| c.access_token = SecretString::new("tok_secret with space"),
            |c| c.refresh_token = SecretString::new(""),
            |c| c.refresh_token = SecretString::new("rt_secret\0nul"),
            |c| c.id_token = Some(SecretString::new("id_secret\n")),
            |c| c.account_id = "acct\r\nheader".to_string(),
            |c| c.account_id = "a".repeat(MAX_ACCOUNT_ID_LEN + 1),
            |c| c.last_refresh_utc = String::new(),
            |c| c.plan_type = Some("pro\n".to_string()),
            |c| c.email_hint = Some("j***@example.com\r".to_string()),
        ];
        for mutate in bad_cases {
            let mut bad = sample_codex_credentials();
            mutate(&mut bad);
            let err = bad.validate().expect_err("invalid creds must fail");
            assert!(matches!(err, CoreError::InvalidAuthCredentials { .. }));
            assert!(!err.to_string().contains("tok_secret"));
            assert!(!err.to_string().contains("rt_secret"));
            assert!(!err.to_string().contains("id_secret"));
        }
    }

    #[test]
    fn file_auth_store_codex_roundtrip_permissions_and_fail_closed_guards() {
        let tmp = TempDir::new().expect("temp dir");
        let cfg_path = tmp.path().join("agy-companion").join("config.json");
        let store = FileAuthStore::for_config_path(&cfg_path).expect("store");
        let auth_dir = store.auth_dir().expect("auth_dir").to_path_buf();

        // Day-0 missing auth/ returns Ok(None) and does NOT create auth/
        assert_eq!(store.load_codex().expect("day-0 load"), None);
        assert_eq!(store.inspect_codex_stamp(), FileStamp::Missing);
        assert_eq!(
            store.load_codex_with_stamp().expect("day-0 stamp"),
            (FileStamp::Missing, None)
        );
        assert!(!auth_dir.exists());
        store.clear_codex().expect("idempotent clear on missing");
        assert!(!auth_dir.exists());

        // Save creates auth/ (0700) and codex.json (0600)
        let creds = sample_codex_credentials();
        store.save_codex(&creds).expect("save codex");
        assert_eq!(store.load_codex().expect("reload"), Some(creds.clone()));

        let (stamp, stamped) = store.load_codex_with_stamp().expect("load_with_stamp");
        assert!(matches!(stamp, FileStamp::Present { .. }));
        assert_eq!(stamped, Some(creds.clone()));

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;

            assert_eq!(
                fs::metadata(&auth_dir).unwrap().permissions().mode() & 0o777,
                0o700
            );
            assert_eq!(
                fs::metadata(store.codex_path())
                    .unwrap()
                    .permissions()
                    .mode()
                    & 0o777,
                0o600
            );

            // Loosened permissions are tightened back to 0700 / 0600 on save
            fs::set_permissions(&auth_dir, fs::Permissions::from_mode(0o755)).unwrap();
            fs::set_permissions(store.codex_path(), fs::Permissions::from_mode(0o644)).unwrap();
            store.save_codex(&creds).expect("resave tightens");
            assert_eq!(
                fs::metadata(&auth_dir).unwrap().permissions().mode() & 0o777,
                0o700
            );
            assert_eq!(
                fs::metadata(store.codex_path())
                    .unwrap()
                    .permissions()
                    .mode()
                    & 0o777,
                0o600
            );
        }

        // Clear removes codex.json idempotently
        store.clear_codex().expect("clear codex");
        assert!(!store.codex_path().exists());
        assert_eq!(store.load_codex().expect("after clear"), None);
        store.clear_codex().expect("second clear");

        // Oversized (> 64 KiB) codex.json rejected
        fs::write(
            store.codex_path(),
            vec![b' '; (MAX_AUTH_FILE_BYTES + 1) as usize],
        )
        .unwrap();
        assert!(matches!(
            store.load_codex(),
            Err(CoreError::Fs(FsError::SizeLimitExceeded {
                max_bytes: MAX_AUTH_FILE_BYTES,
                ..
            }))
        ));
        fs::remove_file(store.codex_path()).unwrap();

        // Directory at codex.json rejected
        fs::create_dir(store.codex_path()).unwrap();
        assert_eq!(
            store.inspect_codex_stamp(),
            FileStamp::Rejected(RejectedReason::NotRegularFile)
        );
        assert!(matches!(
            store.load_codex(),
            Err(CoreError::Fs(FsError::NotRegularFile { .. }))
        ));
        assert!(matches!(
            store.save_codex(&creds),
            Err(CoreError::Fs(FsError::NotRegularFile { .. }))
        ));
        assert!(matches!(
            store.clear_codex(),
            Err(CoreError::Fs(FsError::NotRegularFile { .. }))
        ));
        fs::remove_dir(store.codex_path()).unwrap();

        #[cfg(unix)]
        {
            use std::os::unix::fs::symlink;

            // Symlinked codex.json rejected without touching victim
            let victim = tmp.path().join("victim-codex.json");
            fs::write(&victim, "keep-victim-intact").unwrap();
            symlink(&victim, store.codex_path()).unwrap();
            assert_eq!(
                store.inspect_codex_stamp(),
                FileStamp::Rejected(RejectedReason::Symlink)
            );
            assert!(matches!(
                store.load_codex(),
                Err(CoreError::Fs(FsError::SymlinkRejected { .. }))
            ));
            assert!(matches!(
                store.save_codex(&creds),
                Err(CoreError::Fs(FsError::SymlinkRejected { .. }))
            ));
            assert!(matches!(
                store.clear_codex(),
                Err(CoreError::Fs(FsError::SymlinkRejected { .. }))
            ));
            assert_eq!(fs::read_to_string(&victim).unwrap(), "keep-victim-intact");

            // Symlinked auth/ rejected
            let sym_root = tmp.path().join("sym_codex_auth");
            fs::create_dir_all(&sym_root).unwrap();
            let real_dir = tmp.path().join("real_codex_auth");
            fs::create_dir_all(&real_dir).unwrap();
            symlink(&real_dir, sym_root.join(AUTH_DIR_NAME)).unwrap();
            let sym_store = FileAuthStore::for_config_path(&sym_root.join("config.json")).unwrap();
            assert_eq!(
                sym_store.inspect_codex_stamp(),
                FileStamp::Rejected(RejectedReason::Symlink)
            );
            assert!(matches!(
                sym_store.load_codex(),
                Err(CoreError::Fs(FsError::SymlinkRejected { .. }))
            ));
            assert!(matches!(
                sym_store.save_codex(&creds),
                Err(CoreError::Fs(FsError::SymlinkRejected { .. }))
            ));

            // FIFO at codex.json rejected without hanging
            let fifo_store =
                FileAuthStore::for_config_path(&tmp.path().join("codex_fifo").join("config.json"))
                    .unwrap();
            fs::create_dir_all(fifo_store.auth_dir().unwrap()).unwrap();
            let fifo_path = fifo_store.codex_path();
            assert!(
                std::process::Command::new("mkfifo")
                    .arg(&fifo_path)
                    .status()
                    .expect("mkfifo")
                    .success()
            );
            let (tx, rx) = std::sync::mpsc::channel();
            let cloned = fifo_store.clone();
            let handle = std::thread::spawn(move || {
                let _ = tx.send(cloned.load_codex());
            });
            let res = rx
                .recv_timeout(std::time::Duration::from_secs(1))
                .expect("load_codex must not hang on FIFO");
            handle.join().expect("join");
            assert!(matches!(
                res,
                Err(CoreError::Fs(FsError::NotRegularFile { .. }))
            ));
        }
    }

    #[test]
    fn memory_auth_store_codex_roundtrip_and_validation() {
        let store = MemoryAuthStore::new();
        assert_eq!(store.load_codex().unwrap(), None);

        let creds = sample_codex_credentials();
        store.save_codex(&creds).unwrap();
        assert_eq!(store.load_codex().unwrap(), Some(creds.clone()));

        let mut bad = creds.clone();
        bad.access_token = SecretString::new("bad\ntoken");
        assert!(matches!(
            store.save_codex(&bad),
            Err(CoreError::InvalidAuthCredentials { .. })
        ));
        assert_eq!(store.load_codex().unwrap(), Some(creds.clone()));

        store.clear_codex().unwrap();
        assert_eq!(store.load_codex().unwrap(), None);

        let seeded = MemoryAuthStore::with_codex_credentials(creds.clone());
        assert_eq!(seeded.load_codex().unwrap(), Some(creds));
    }

    #[test]
    fn codex_refresh_lock_serializes_retains_lockfile_and_rejects_symlinks() {
        let tmp = TempDir::new().expect("temp dir");
        let auth_dir = tmp.path().join("auth");

        let lock = CodexRefreshLock::acquire(&auth_dir).expect("acquire lock");
        assert_eq!(lock.path(), auth_dir.join(CODEX_REFRESH_LOCK_FILENAME));
        assert!(lock.path().exists());

        // Second non-blocking acquisition contends while first is held
        assert!(
            CodexRefreshLock::try_acquire(&auth_dir)
                .expect("try_acquire")
                .is_none()
        );

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;

            assert_eq!(
                fs::metadata(&auth_dir).unwrap().permissions().mode() & 0o777,
                0o700
            );
            assert_eq!(
                fs::metadata(lock.path()).unwrap().permissions().mode() & 0o777,
                0o600
            );
        }

        let lock_path = lock.path().to_path_buf();
        drop(lock);
        // Lockfile is intentionally retained on Drop (no flock-unlink race)
        assert!(lock_path.exists());

        // Re-acquisition succeeds immediately after Drop
        let reacquired = CodexRefreshLock::try_acquire(&auth_dir)
            .expect("reacquire")
            .expect("lock must be free after drop");
        drop(reacquired);

        #[cfg(unix)]
        {
            use std::os::unix::fs::symlink;

            // Symlinked auth_dir rejected
            let victim_dir = tmp.path().join("victim_dir");
            fs::create_dir_all(&victim_dir).unwrap();
            let sym_auth = tmp.path().join("sym_auth_lock");
            symlink(&victim_dir, &sym_auth).unwrap();
            assert!(matches!(
                CodexRefreshLock::acquire(&sym_auth),
                Err(CoreError::Fs(FsError::SymlinkRejected { .. }))
            ));

            // Symlinked .codex-refresh.lock rejected without touching victim
            let victim_file = tmp.path().join("victim_lock_target");
            fs::write(&victim_file, "keep-lock-victim").unwrap();
            let sym_lock_dir = tmp.path().join("sym_lock_file_dir");
            fs::create_dir_all(&sym_lock_dir).unwrap();
            symlink(&victim_file, sym_lock_dir.join(CODEX_REFRESH_LOCK_FILENAME)).unwrap();
            assert!(matches!(
                CodexRefreshLock::acquire(&sym_lock_dir),
                Err(CoreError::Fs(FsError::SymlinkRejected { .. }))
            ));
            assert_eq!(
                fs::read_to_string(&victim_file).unwrap(),
                "keep-lock-victim"
            );
        }
    }
}
