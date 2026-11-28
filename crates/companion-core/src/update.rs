use std::cmp::Ordering;
use std::fmt;
use std::path::{Path, PathBuf};
use std::str::FromStr;

use serde::{Deserialize, Serialize};

use crate::config::CoreError;
use crate::fs_safe::{self, SymlinkPolicy};

pub const UPDATE_STATE_SCHEMA_VERSION: u32 = 1;
pub const UPDATE_STATE_FILENAME: &str = "update-state.json";
pub const MAX_UPDATE_STATE_BYTES: u64 = 64 * 1024;
pub const INSTALL_CHANNEL_ENV_VAR: &str = "AGY_COMPANION_INSTALL_CHANNEL";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "kebab-case")]
pub enum InstallChannel {
    #[default]
    Npx,
    Uvx,
}

impl InstallChannel {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Npx => "npx",
            Self::Uvx => "uvx",
        }
    }

    pub const fn fallback_command(self) -> &'static str {
        match self {
            Self::Npx => "npx -y agy-companion@latest install",
            Self::Uvx => "uvx --refresh agy-companion install",
        }
    }
}

impl fmt::Display for InstallChannel {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl FromStr for InstallChannel {
    type Err = CoreError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let trimmed = s.trim();
        if trimmed.eq_ignore_ascii_case("npx") {
            Ok(Self::Npx)
        } else if trimmed.eq_ignore_ascii_case("uvx") {
            Ok(Self::Uvx)
        } else {
            Err(CoreError::InvalidField {
                field: "install_channel".to_string(),
                reason: format!("unsupported install channel '{s}' (expected npx or uvx)"),
            })
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PrereleaseIdent {
    Numeric(u64),
    AlphaNumeric(String),
}

impl Ord for PrereleaseIdent {
    fn cmp(&self, other: &Self) -> Ordering {
        match (self, other) {
            (Self::Numeric(a), Self::Numeric(b)) => a.cmp(b),
            (Self::Numeric(_), Self::AlphaNumeric(_)) => Ordering::Less,
            (Self::AlphaNumeric(_), Self::Numeric(_)) => Ordering::Greater,
            (Self::AlphaNumeric(a), Self::AlphaNumeric(b)) => a.cmp(b),
        }
    }
}

impl PartialOrd for PrereleaseIdent {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl fmt::Display for PrereleaseIdent {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Numeric(n) => write!(f, "{n}"),
            Self::AlphaNumeric(s) => f.write_str(s),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SemVer {
    pub major: u64,
    pub minor: u64,
    pub patch: u64,
    prerelease: Vec<PrereleaseIdent>,
}

impl SemVer {
    pub fn parse(raw: &str) -> Result<Self, CoreError> {
        let invalid = || CoreError::InvalidField {
            field: "version".to_string(),
            reason: format!(
                "invalid SemVer version '{raw}' (expected MAJOR.MINOR.PATCH or MAJOR.MINOR.PATCH-prerelease)"
            ),
        };

        if raw.is_empty() || !raw.is_ascii() {
            return Err(invalid());
        }

        let (core, pre_raw) = match raw.split_once('-') {
            Some((core, pre)) => (core, Some(pre)),
            None => (raw, None),
        };

        let mut core_parts = core.split('.');
        let major_s = core_parts.next().ok_or_else(invalid)?;
        let minor_s = core_parts.next().ok_or_else(invalid)?;
        let patch_s = core_parts.next().ok_or_else(invalid)?;
        if core_parts.next().is_some() {
            return Err(invalid());
        }

        let major = parse_strict_numeric_ident(major_s).ok_or_else(invalid)?;
        let minor = parse_strict_numeric_ident(minor_s).ok_or_else(invalid)?;
        let patch = parse_strict_numeric_ident(patch_s).ok_or_else(invalid)?;

        let mut prerelease = Vec::new();
        if let Some(pre) = pre_raw {
            if pre.is_empty() {
                return Err(invalid());
            }
            for ident in pre.split('.') {
                if ident.is_empty()
                    || !ident
                        .bytes()
                        .all(|b| b.is_ascii_alphanumeric() || b == b'-')
                {
                    return Err(invalid());
                }
                if ident.bytes().all(|b| b.is_ascii_digit()) {
                    let num = parse_strict_numeric_ident(ident).ok_or_else(invalid)?;
                    prerelease.push(PrereleaseIdent::Numeric(num));
                } else {
                    prerelease.push(PrereleaseIdent::AlphaNumeric(ident.to_string()));
                }
            }
        }

        Ok(Self {
            major,
            minor,
            patch,
            prerelease,
        })
    }

    pub fn is_prerelease(&self) -> bool {
        !self.prerelease.is_empty()
    }

    pub fn prerelease(&self) -> &[PrereleaseIdent] {
        &self.prerelease
    }
}

fn parse_strict_numeric_ident(s: &str) -> Option<u64> {
    if s.is_empty() || !s.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    if s.len() > 1 && s.starts_with('0') {
        return None;
    }
    s.parse::<u64>().ok()
}

impl Ord for SemVer {
    fn cmp(&self, other: &Self) -> Ordering {
        (self.major, self.minor, self.patch)
            .cmp(&(other.major, other.minor, other.patch))
            .then_with(|| {
                // Per SemVer 2.0.0: a normal version has higher precedence than a pre-release
                // version with the same MAJOR.MINOR.PATCH (false < true).
                self.prerelease.is_empty().cmp(&other.prerelease.is_empty())
            })
            .then_with(|| self.prerelease.cmp(&other.prerelease))
    }
}

impl PartialOrd for SemVer {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl FromStr for SemVer {
    type Err = CoreError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Self::parse(s)
    }
}

impl fmt::Display for SemVer {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}.{}.{}", self.major, self.minor, self.patch)?;
        if !self.prerelease.is_empty() {
            f.write_str("-")?;
            for (idx, ident) in self.prerelease.iter().enumerate() {
                if idx > 0 {
                    f.write_str(".")?;
                }
                write!(f, "{ident}")?;
            }
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum UpgradeOutcome {
    Upgrading,
    Succeeded,
    Failed,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LastUpgradeRecord {
    pub target_version: String,
    pub started_at_ms: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub finished_at_ms: Option<u64>,
    pub outcome: UpgradeOutcome,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct UpdateState {
    pub schema_version: u32,
    pub install_channel: InstallChannel,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub checked_at_ms: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub latest_version: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_upgrade: Option<LastUpgradeRecord>,
}

impl Default for UpdateState {
    fn default() -> Self {
        Self::new(InstallChannel::default())
    }
}

impl UpdateState {
    pub fn new(install_channel: InstallChannel) -> Self {
        Self {
            schema_version: UPDATE_STATE_SCHEMA_VERSION,
            install_channel,
            checked_at_ms: None,
            latest_version: None,
            last_upgrade: None,
        }
    }

    pub fn validate(&self) -> Result<(), CoreError> {
        if self.schema_version != UPDATE_STATE_SCHEMA_VERSION {
            return Err(CoreError::InvalidField {
                field: "schema_version".to_string(),
                reason: format!(
                    "unsupported update-state schema version {} (supported: {UPDATE_STATE_SCHEMA_VERSION})",
                    self.schema_version
                ),
            });
        }
        Ok(())
    }
}

pub fn update_state_path(data_dir: &Path) -> PathBuf {
    data_dir.join(UPDATE_STATE_FILENAME)
}

pub fn load_update_state(path: &Path) -> Result<Option<UpdateState>, CoreError> {
    let Some((raw, _meta)) = fs_safe::read_regular_file_utf8_bounded(
        path,
        MAX_UPDATE_STATE_BYTES,
        SymlinkPolicy::Reject,
    )?
    else {
        return Ok(None);
    };

    let state: UpdateState = serde_json::from_str(&raw).map_err(|err| CoreError::ParseConfig {
        path: path.to_path_buf(),
        line: err.line(),
        column: err.column(),
    })?;
    state.validate()?;
    Ok(Some(state))
}

pub fn save_update_state(path: &Path, state: &UpdateState) -> Result<(), CoreError> {
    state.validate()?;
    let mut payload =
        serde_json::to_string_pretty(state).map_err(|err| CoreError::SerializeConfig {
            reason: err.to_string(),
        })?;
    payload.push('\n');
    fs_safe::write_atomic_secure(path, payload.as_bytes())?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::fs;

    use tempfile::TempDir;

    use super::*;
    use crate::fs_safe::FsError;

    #[test]
    fn install_channel_helpers_serde_and_from_str() {
        assert_eq!(InstallChannel::default(), InstallChannel::Npx);
        assert_eq!(InstallChannel::Npx.as_str(), "npx");
        assert_eq!(InstallChannel::Uvx.as_str(), "uvx");
        assert_eq!(
            InstallChannel::Npx.fallback_command(),
            "npx -y agy-companion@latest install"
        );
        assert_eq!(
            InstallChannel::Uvx.fallback_command(),
            "uvx --refresh agy-companion install"
        );

        assert_eq!(
            "npx".parse::<InstallChannel>().unwrap(),
            InstallChannel::Npx
        );
        assert_eq!(
            "NPX".parse::<InstallChannel>().unwrap(),
            InstallChannel::Npx
        );
        assert_eq!(
            "  uvx  ".parse::<InstallChannel>().unwrap(),
            InstallChannel::Uvx
        );
        assert_eq!(
            "UvX".parse::<InstallChannel>().unwrap(),
            InstallChannel::Uvx
        );
        for bad in ["", "   ", "brew", "cargo", "pip", "npm"] {
            assert!(
                bad.parse::<InstallChannel>().is_err(),
                "expected error for '{bad}'"
            );
        }

        assert_eq!(
            serde_json::to_string(&InstallChannel::Npx).unwrap(),
            "\"npx\""
        );
        assert_eq!(
            serde_json::to_string(&InstallChannel::Uvx).unwrap(),
            "\"uvx\""
        );
        assert_eq!(
            serde_json::from_str::<InstallChannel>("\"npx\"").unwrap(),
            InstallChannel::Npx
        );
        assert_eq!(
            serde_json::from_str::<InstallChannel>("\"uvx\"").unwrap(),
            InstallChannel::Uvx
        );
        assert!(serde_json::from_str::<InstallChannel>("\"brew\"").is_err());
    }

    #[test]
    fn semver_ordering_and_prerelease_semantics() {
        let v_0_1_2: SemVer = "0.1.2".parse().unwrap();
        let v_0_1_3_dev_0: SemVer = "0.1.3-dev.0".parse().unwrap();
        let v_0_1_3_dev_2: SemVer = "0.1.3-dev.2".parse().unwrap();
        let v_0_1_3_dev_10: SemVer = "0.1.3-dev.10".parse().unwrap();
        let v_0_1_3: SemVer = "0.1.3".parse().unwrap();
        let v_0_2_0: SemVer = "0.2.0".parse().unwrap();
        let v_1_0_0_alpha: SemVer = "1.0.0-alpha".parse().unwrap();
        let v_1_0_0_alpha_1: SemVer = "1.0.0-alpha.1".parse().unwrap();
        let v_1_0_0_alpha_beta: SemVer = "1.0.0-alpha.beta".parse().unwrap();
        let v_1_0_0_beta: SemVer = "1.0.0-beta".parse().unwrap();
        let v_1_0_0_1: SemVer = "1.0.0-1".parse().unwrap();
        let v_1_0_0: SemVer = "1.0.0".parse().unwrap();

        assert!(!v_0_1_2.is_prerelease());
        assert!(v_0_1_3_dev_2.is_prerelease());
        assert!(!v_0_1_3.is_prerelease());

        // Core dev-build reminder invariants
        assert!(v_0_1_2 < v_0_1_3_dev_2);
        assert!(v_0_1_3_dev_2 < v_0_1_3);
        assert!(v_0_1_3_dev_0 < v_0_1_3_dev_2);
        assert!(v_0_1_3_dev_2 < v_0_1_3_dev_10);
        assert!(v_0_1_3_dev_10 < v_0_1_3);
        assert!(v_0_1_3 < v_0_2_0);

        // General SemVer 2.0.0 prerelease precedence
        assert!(v_1_0_0_1 < v_1_0_0_alpha);
        assert!(v_1_0_0_alpha < v_1_0_0_alpha_1);
        assert!(v_1_0_0_alpha_1 < v_1_0_0_alpha_beta);
        assert!(v_1_0_0_alpha_beta < v_1_0_0_beta);
        assert!(v_1_0_0_beta < v_1_0_0);

        // Display round-trip
        assert_eq!(v_0_1_3.to_string(), "0.1.3");
        assert_eq!(v_0_1_3_dev_2.to_string(), "0.1.3-dev.2");
        assert_eq!(v_1_0_0_alpha_1.to_string(), "1.0.0-alpha.1");
    }

    #[test]
    fn semver_rejects_invalid_inputs() {
        for bad in [
            "",
            "1",
            "1.2",
            "1.2.3.4",
            "v1.2.3",
            "01.2.3",
            "1.02.3",
            "1.2.03",
            "1.2.3-",
            "1.2.3-dev.",
            "1.2.3-.dev",
            "1.2.3-dev..1",
            "1.2.3-dev.01",
            "1.2.3+build.1",
            "1.2.3 ",
            " 1.2.3",
            "1.2.3\n",
            "1.2.3-dev_1",
            "a.b.c",
        ] {
            assert!(
                SemVer::parse(bad).is_err(),
                "expected SemVer::parse to reject '{bad}'"
            );
        }
    }

    #[test]
    fn update_state_round_trip_permissions_deny_unknown_fields_and_symlink_rejection() {
        let temp = TempDir::new().expect("tempdir");
        let data_dir = temp.path().join("data");
        let path = update_state_path(&data_dir);
        assert_eq!(path, data_dir.join("update-state.json"));

        // Missing file returns Ok(None)
        assert_eq!(load_update_state(&path).expect("load missing"), None);

        let state = UpdateState {
            schema_version: UPDATE_STATE_SCHEMA_VERSION,
            install_channel: InstallChannel::Uvx,
            checked_at_ms: Some(1_760_000_000_000),
            latest_version: Some("0.2.0".to_string()),
            last_upgrade: Some(LastUpgradeRecord {
                target_version: "0.2.0".to_string(),
                started_at_ms: 1_760_000_000_100,
                finished_at_ms: Some(1_760_000_005_000),
                outcome: UpgradeOutcome::Succeeded,
                error: None,
            }),
        };
        save_update_state(&path, &state).expect("save update state");
        let loaded = load_update_state(&path)
            .expect("load saved")
            .expect("some state");
        assert_eq!(loaded, state);

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let file_mode = fs::metadata(&path).unwrap().permissions().mode() & 0o777;
            assert_eq!(file_mode, 0o600);
            let dir_mode = fs::metadata(&data_dir).unwrap().permissions().mode() & 0o777;
            assert_eq!(dir_mode, 0o700);
        }

        // Reject unknown fields on both UpdateState and LastUpgradeRecord
        fs::write(
            &path,
            r#"{"schema_version":1,"install_channel":"npx","unexpected":true}"#,
        )
        .unwrap();
        assert!(matches!(
            load_update_state(&path),
            Err(CoreError::ParseConfig { .. })
        ));

        fs::write(
            &path,
            r#"{"schema_version":1,"install_channel":"npx","last_upgrade":{"target_version":"0.2.0","started_at_ms":1,"outcome":"failed","extra":1}}"#,
        )
        .unwrap();
        assert!(matches!(
            load_update_state(&path),
            Err(CoreError::ParseConfig { .. })
        ));

        // Reject unsupported schema_version
        fs::write(&path, r#"{"schema_version":2,"install_channel":"npx"}"#).unwrap();
        assert!(matches!(
            load_update_state(&path),
            Err(CoreError::InvalidField { .. })
        ));

        // Reject oversized file (> 64 KiB)
        let big = vec![b' '; (MAX_UPDATE_STATE_BYTES as usize) + 1];
        fs::write(&path, big).unwrap();
        assert!(matches!(
            load_update_state(&path),
            Err(CoreError::Fs(FsError::SizeLimitExceeded { .. }))
        ));

        // Reject symlinked update-state.json on load and save
        #[cfg(unix)]
        {
            use std::os::unix::fs::symlink;
            let real = temp.path().join("real.json");
            fs::write(&real, r#"{"schema_version":1,"install_channel":"npx"}"#).unwrap();
            let link = data_dir.join("symlink-state.json");
            symlink(&real, &link).unwrap();
            assert!(matches!(
                load_update_state(&link),
                Err(CoreError::Fs(FsError::SymlinkRejected { .. }))
            ));
            assert!(matches!(
                save_update_state(&link, &UpdateState::default()),
                Err(CoreError::Fs(FsError::SymlinkRejected { .. }))
            ));
        }
    }
}
