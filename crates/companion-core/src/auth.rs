use std::fmt;
use std::fs;
use std::io::ErrorKind;
use std::path::{Path, PathBuf};
use std::sync::RwLock;

use serde::{Deserialize, Serialize};

use crate::config::CoreError;
use crate::fs_safe::{
    self, AtomicWriteOptions, FileStamp, FsError, ParentDirPolicy, RejectedReason, SymlinkPolicy,
    TargetFilePolicy,
};
use crate::validation::ApiKey;

pub const AUTH_SCHEMA_VERSION: u32 = 1;
pub const MAX_AUTH_FILE_BYTES: u64 = 64 * 1024;
pub const AUTH_DIR_NAME: &str = "auth";
pub const GEMINI_AUTH_FILENAME: &str = "gemini.json";

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

    pub fn inspect_gemini_stamp(&self) -> FileStamp {
        inspect_auth_file_stamp(&self.auth_dir, GEMINI_AUTH_FILENAME)
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
}

/// In-memory `AuthStore` for fast hermetic unit tests.
#[derive(Debug, Default)]
pub struct MemoryAuthStore {
    gemini: RwLock<Option<GeminiCredentials>>,
}

impl MemoryAuthStore {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn with_gemini_key(api_key: ApiKey) -> Self {
        Self {
            gemini: RwLock::new(Some(GeminiCredentials::new(api_key))),
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
}
