use std::collections::{HashMap, HashSet};
use std::fs;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant, SystemTime};

use companion_core::fs_safe::{FsError, SymlinkPolicy, read_regular_file_bounded};

use crate::BINARY_NAME;

pub const PLUGIN_DIR_ENV: &str = "AGY_COMPANION_PLUGIN_DIR";
pub const PLUGINS_DIR_ENV: &str = "AGY_PLUGINS_DIR";
pub const BIN_OVERRIDE_ENV: &str = "AGY_COMPANION_BIN";
pub const EXECUTABLE_DATA_DIR_ENV: &str = "ANTIGRAVITY_EXECUTABLE_DATA_DIR";
pub const MAX_PLUGIN_MANIFEST_BYTES: u64 = 64 * 1024;
pub const DEFAULT_PLUGIN_POLL_INTERVAL: Duration = Duration::from_secs(2);

#[cfg(debug_assertions)]
pub const TEST_PLUGIN_POLL_MS_ENV: &str = "AGY_COMPANION_TEST_PLUGIN_POLL_MS";

const PLUGIN_NAME: &str = "agy-companion";
const VERSION_PROBE_TIMEOUT: Duration = Duration::from_secs(2);
const VERSION_PROBE_POLL_STEP: Duration = Duration::from_millis(10);
const MAX_VERSION_STDOUT_BYTES: u64 = 256;
const MAX_PROBE_CACHE_ENTRIES: usize = 64;

#[cfg(windows)]
const BIN_FILENAME: &str = "agy-companion.exe";
#[cfg(not(windows))]
const BIN_FILENAME: &str = "agy-companion";

/// Returns the effective plugin-manifest polling interval (`2s` in release builds;
/// optionally overridable via `AGY_COMPANION_TEST_PLUGIN_POLL_MS` in debug builds).
pub fn resolve_plugin_poll_interval() -> Duration {
    #[cfg(debug_assertions)]
    {
        if let Ok(raw) = std::env::var(TEST_PLUGIN_POLL_MS_ENV)
            && let Ok(ms) = raw.trim().parse::<u64>()
        {
            return Duration::from_millis(ms.clamp(50, 5_000));
        }
    }
    DEFAULT_PLUGIN_POLL_INTERVAL
}

/// Validates a plugin or binary version string (ASCII, non-empty, `<= 64` chars,
/// starts with a digit, allowed `[0-9A-Za-z._+-]`, no `".."` traversal segments).
pub fn is_valid_version_string(raw: &str) -> bool {
    if raw.is_empty() || raw.len() > 64 || raw.contains("..") {
        return false;
    }
    let bytes = raw.as_bytes();
    if !bytes[0].is_ascii_digit() {
        return false;
    }
    bytes
        .iter()
        .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'-' | b'_' | b'+'))
}

fn validate_env_path(raw: &str) -> Option<PathBuf> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return None;
    }
    let path = PathBuf::from(trimmed);
    companion_core::validate_path(&path).ok()?;
    Some(path)
}

/// Pure, testable plugin directory resolver with S6 path validation.
///
/// Precedence:
/// 1. `AGY_COMPANION_PLUGIN_DIR` (if set & non-empty; invalid value fails closed to `None`)
/// 2. `AGY_PLUGINS_DIR/agy-companion` (if set & non-empty; invalid value fails closed to `None`)
/// 3. `default_plugins_dir/agy-companion`
pub(crate) fn resolve_plugin_dir_from(
    plugin_dir_env: Option<&str>,
    plugins_dir_env: Option<&str>,
    default_plugins_dir: Option<PathBuf>,
) -> Option<PathBuf> {
    if let Some(raw) = plugin_dir_env
        && !raw.trim().is_empty()
    {
        return validate_env_path(raw);
    }
    if let Some(raw) = plugins_dir_env
        && !raw.trim().is_empty()
    {
        return validate_env_path(raw).map(|dir| dir.join(PLUGIN_NAME));
    }
    default_plugins_dir.map(|dir| dir.join(PLUGIN_NAME))
}

/// Resolves the active `agy-companion` plugin directory using S6-validated precedence:
/// `AGY_COMPANION_PLUGIN_DIR` -> `AGY_PLUGINS_DIR/agy-companion` ->
/// `companion_core::default_plugins_dir()/agy-companion`.
pub fn resolve_plugin_dir() -> Option<PathBuf> {
    let plugin_dir_env = std::env::var(PLUGIN_DIR_ENV).ok();
    let plugins_dir_env = std::env::var(PLUGINS_DIR_ENV).ok();
    let default_plugins_dir = if plugin_dir_env
        .as_deref()
        .is_none_or(|v| v.trim().is_empty())
        && plugins_dir_env
            .as_deref()
            .is_none_or(|v| v.trim().is_empty())
    {
        companion_core::default_plugins_dir().ok()
    } else {
        None
    };
    resolve_plugin_dir_from(
        plugin_dir_env.as_deref(),
        plugins_dir_env.as_deref(),
        default_plugins_dir,
    )
}

/// Three-state outcome for reading `<plugin_dir>/plugin.json` so callers can
/// fail closed on S6/validation errors without falling back to `running_version`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PluginManifestRead {
    Valid(String),
    Absent,
    Invalid,
}

/// Reads and validates `<plugin_dir>/plugin.json` fail-closed per S6.
///
/// Permits a symlinked `plugin_dir` parent (e.g. `scripts/dev-install.sh`), but rejects
/// symlinked/FIFO/non-regular or oversized (`> 64 KiB`) leaf `plugin.json`, non-matching
/// `"name"`, or invalid `"version"` as `PluginManifestRead::Invalid`.
pub fn read_plugin_manifest_version(plugin_dir: &Path) -> PluginManifestRead {
    let manifest_path = plugin_dir.join("plugin.json");
    let bytes = match read_regular_file_bounded(
        &manifest_path,
        MAX_PLUGIN_MANIFEST_BYTES,
        SymlinkPolicy::Reject,
    ) {
        Ok(Some(res)) => res.bytes,
        Ok(None) => return PluginManifestRead::Absent,
        Err(FsError::Io { source, .. }) if source.kind() == std::io::ErrorKind::NotFound => {
            return PluginManifestRead::Absent;
        }
        Err(_) => return PluginManifestRead::Invalid,
    };

    let Ok(parsed) = serde_json::from_slice::<serde_json::Value>(&bytes) else {
        return PluginManifestRead::Invalid;
    };
    if parsed.get("name").and_then(|v| v.as_str()) != Some(PLUGIN_NAME) {
        return PluginManifestRead::Invalid;
    }
    let Some(version) = parsed
        .get("version")
        .and_then(|v| v.as_str())
        .map(str::trim)
    else {
        return PluginManifestRead::Invalid;
    };
    if !is_valid_version_string(version) {
        return PluginManifestRead::Invalid;
    }
    PluginManifestRead::Valid(version.to_string())
}

/// Unified version-convergence target:
/// - `Valid(v)` -> `Some(v)`
/// - `Invalid` -> `None` (fail closed; never downgrade or spawn)
/// - `Absent` when a valid manifest was previously observed -> `None` (fail closed across non-atomic rewrites)
/// - `Absent` with no previously valid manifest -> `Some(running_version)` (M1 direct-skew fallback)
pub fn resolve_desired_version<'a>(
    manifest: &'a PluginManifestRead,
    has_seen_valid_manifest: bool,
    running_version: &'a str,
) -> Option<&'a str> {
    match manifest {
        PluginManifestRead::Valid(ver) => Some(ver.as_str()),
        PluginManifestRead::Invalid => None,
        PluginManifestRead::Absent => {
            if has_seen_valid_manifest {
                None
            } else {
                Some(running_version)
            }
        }
    }
}

fn is_executable_regular_file(path: &Path) -> bool {
    let Ok(meta) = fs::metadata(path) else {
        return false;
    };
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        meta.is_file() && (meta.permissions().mode() & 0o111 != 0)
    }
    #[cfg(not(unix))]
    {
        meta.is_file()
    }
}

fn dev_checkout_candidates(plugin_dir: &Path) -> Vec<PathBuf> {
    let resolved = fs::canonicalize(plugin_dir).unwrap_or_else(|_| plugin_dir.to_path_buf());
    let Some(repo_root) = resolved.parent() else {
        return Vec::new();
    };
    let debug_bin = repo_root.join("target").join("debug").join(BIN_FILENAME);
    let release_bin = repo_root.join("target").join("release").join(BIN_FILENAME);
    let debug_mtime = fs::metadata(&debug_bin)
        .ok()
        .filter(|m| m.is_file())
        .and_then(|m| m.modified().ok());
    let release_mtime = fs::metadata(&release_bin)
        .ok()
        .filter(|m| m.is_file())
        .and_then(|m| m.modified().ok());

    match (debug_mtime, release_mtime) {
        (Some(dm), Some(rm)) if rm > dm => vec![release_bin, debug_bin],
        _ => vec![debug_bin, release_bin],
    }
}

pub(crate) struct CandidateDiscoveryInputs<'a> {
    pub bin_override_env: Option<&'a str>,
    pub plugin_dir: Option<&'a Path>,
    pub startup_exe: Option<&'a Path>,
    pub executable_data_dir_env: Option<&'a str>,
    pub default_data_dir: Option<&'a Path>,
    pub path_env: Option<&'a std::ffi::OsStr>,
}

/// Collects deduplicated local candidate binary paths in trust & parity order with
/// `run-companion.sh`:
/// 1. `AGY_COMPANION_BIN`
/// 2. Dev sibling `<repo>/target/{debug,release}/agy-companion` (newer mtime first)
/// 3. Startup-captured `current_exe()`
/// 4. `${ANTIGRAVITY_EXECUTABLE_DATA_DIR}/bin/agy-companion`
/// 5. `companion_core::default_data_dir()/bin/agy-companion`
/// 6. `$PATH`
pub(crate) fn collect_candidate_binaries_from(
    inputs: CandidateDiscoveryInputs<'_>,
) -> Vec<PathBuf> {
    let mut candidates = Vec::new();
    let mut seen = HashSet::new();
    let mut push_unique = |path: PathBuf| {
        if seen.insert(path.clone()) {
            candidates.push(path);
        }
    };

    if let Some(raw) = inputs.bin_override_env
        && let Some(path) = validate_env_path(raw)
    {
        push_unique(path);
    }

    if let Some(plugin_dir) = inputs.plugin_dir {
        for dev_bin in dev_checkout_candidates(plugin_dir) {
            push_unique(dev_bin);
        }
    }

    if let Some(exe) = inputs.startup_exe {
        push_unique(exe.to_path_buf());
    }

    if let Some(raw) = inputs.executable_data_dir_env
        && let Some(dir) = validate_env_path(raw)
    {
        push_unique(dir.join("bin").join(BIN_FILENAME));
    }

    if let Some(data_dir) = inputs.default_data_dir {
        push_unique(data_dir.join("bin").join(BIN_FILENAME));
    }

    if let Some(path_var) = inputs.path_env {
        for dir in std::env::split_paths(path_var) {
            if !dir.as_os_str().is_empty() {
                push_unique(dir.join(BIN_FILENAME));
            }
        }
    }

    candidates
}

/// Collects local candidate binary paths from the current process environment.
pub fn collect_candidate_binaries(
    plugin_dir: Option<&Path>,
    startup_exe: Option<&Path>,
) -> Vec<PathBuf> {
    let bin_override_env = std::env::var(BIN_OVERRIDE_ENV).ok();
    let executable_data_dir_env = std::env::var(EXECUTABLE_DATA_DIR_ENV).ok();
    let default_data_dir = companion_core::default_data_dir().ok();
    let path_env = std::env::var_os("PATH");
    collect_candidate_binaries_from(CandidateDiscoveryInputs {
        bin_override_env: bin_override_env.as_deref(),
        plugin_dir,
        startup_exe,
        executable_data_dir_env: executable_data_dir_env.as_deref(),
        default_data_dir: default_data_dir.as_deref(),
        path_env: path_env.as_deref(),
    })
}

/// Parses trimmed `--version` stdout (`"agy-companion <version>"`).
pub(crate) fn parse_cli_version_output(stdout: &str) -> Option<String> {
    let trimmed = stdout.trim();
    let prefix = format!("{BINARY_NAME} ");
    let version = trimmed.strip_prefix(&prefix)?.trim();
    if !is_valid_version_string(version) {
        return None;
    }
    Some(version.to_string())
}

fn probe_binary_version_with_timeout(candidate: &Path, timeout: Duration) -> Option<String> {
    let mut cmd = Command::new(candidate);
    cmd.arg("--version")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null());

    // Scrub proxy, sidecar, LS, plugin, and debug test env vars before probing.
    cmd.env_remove("CLOUD_CODE_URL")
        .env_remove("GOOGLE_GEMINI_BASE_URL")
        .env_remove("AGY_COMPANION_HOOK_ACTIVE")
        .env_remove("AGY_COMPANION_INJECTED_URL")
        .env_remove("JETSKI_MCP_ENV_RESTORE_PREFIX")
        .env_remove("JETSKI_MCP_ENV_RESTORE_VARS")
        .env_remove(crate::sidecar_ui::SIDECAR_WEB_PORT_ENV)
        .env_remove(crate::sidecar_ui::SIDECAR_UI_TOKEN_ENV)
        .env_remove(companion_providers::ANTIGRAVITY_LS_ADDRESS_ENV_VAR)
        .env_remove(companion_providers::ANTIGRAVITY_CSRF_TOKEN_ENV_VAR)
        .env_remove(PLUGIN_DIR_ENV);
    #[cfg(debug_assertions)]
    {
        cmd.env_remove(crate::serve::TEST_HEALTH_VERSION_ENV)
            .env_remove(TEST_PLUGIN_POLL_MS_ENV);
    }

    let mut child = cmd.spawn().ok()?;
    let mut stdout_pipe = child.stdout.take();
    let deadline = Instant::now() + timeout;
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) => {
                if Instant::now() >= deadline {
                    let _ = child.kill();
                    let _ = child.wait();
                    return None;
                }
                std::thread::sleep(VERSION_PROBE_POLL_STEP);
            }
            Err(_) => {
                let _ = child.kill();
                let _ = child.wait();
                return None;
            }
        }
    };

    if !status.success() {
        return None;
    }

    let mut buf = Vec::new();
    if let Some(pipe) = stdout_pipe.as_mut() {
        let _ = pipe.take(MAX_VERSION_STDOUT_BYTES).read_to_end(&mut buf);
    }
    let stdout_str = String::from_utf8(buf).ok()?;
    parse_cli_version_output(&stdout_str)
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct CachedBinaryProbe {
    len: u64,
    modified: Option<SystemTime>,
    detected_version: Option<String>,
}

/// Stat-gated cache for candidate binary `--version` probes so unchanged files are not
/// re-executed on every poll tick.
#[derive(Debug, Default)]
pub struct BinaryVersionProbeCache {
    entries: HashMap<PathBuf, CachedBinaryProbe>,
    #[cfg(all(test, unix))]
    probe_invocations: usize,
}

impl BinaryVersionProbeCache {
    pub fn new() -> Self {
        Self::default()
    }

    /// Probes `candidates` in order and returns the first regular executable whose
    /// `--version` output equals `format!("agy-companion {target_version}")`.
    pub fn find_matching_binary(
        &mut self,
        candidates: &[PathBuf],
        target_version: &str,
    ) -> Option<PathBuf> {
        self.find_matching_binary_with_timeout(candidates, target_version, VERSION_PROBE_TIMEOUT)
    }

    fn find_matching_binary_with_timeout(
        &mut self,
        candidates: &[PathBuf],
        target_version: &str,
        timeout: Duration,
    ) -> Option<PathBuf> {
        for candidate in candidates {
            if !is_executable_regular_file(candidate) {
                self.entries.remove(candidate);
                continue;
            }
            let Ok(meta) = fs::metadata(candidate) else {
                self.entries.remove(candidate);
                continue;
            };
            let len = meta.len();
            let modified = meta.modified().ok();

            if let Some(cached) = self.entries.get(candidate)
                && cached.len == len
                && cached.modified == modified
            {
                if cached.detected_version.as_deref() == Some(target_version) {
                    return Some(candidate.clone());
                }
                continue;
            }

            #[cfg(all(test, unix))]
            {
                self.probe_invocations += 1;
            }
            let detected_version = probe_binary_version_with_timeout(candidate, timeout);
            if self.entries.len() >= MAX_PROBE_CACHE_ENTRIES
                && !self.entries.contains_key(candidate)
            {
                self.entries.clear();
            }
            let matched = detected_version.as_deref() == Some(target_version);
            self.entries.insert(
                candidate.clone(),
                CachedBinaryProbe {
                    len,
                    modified,
                    detected_version,
                },
            );
            if matched {
                return Some(candidate.clone());
            }
        }
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn valid_and_invalid_version_strings() {
        for ok in [
            "0.0.3",
            "0.0.4",
            "0.1.0-dev.0",
            "0.1.0-dev.1",
            "0.2.0-dev.0",
            "0.2.0-dev.1",
            "1.2.3-alpha.1",
            "1.2.3+build.42",
            "0.0.1-stale",
        ] {
            assert!(is_valid_version_string(ok), "expected valid: {ok}");
        }
        for bad in [
            "",
            "   ",
            "v0.0.3",
            "0.0..3",
            "../0.0.3",
            "0.0.3\n",
            "0.0.3;rm -rf /",
            &"1".repeat(65),
        ] {
            assert!(!is_valid_version_string(bad), "expected invalid: {bad}");
        }
    }

    #[test]
    fn parse_cli_version_output_requires_exact_prefix_and_valid_version() {
        assert_eq!(
            parse_cli_version_output("agy-companion 0.0.4\n"),
            Some("0.0.4".to_string())
        );
        assert_eq!(
            parse_cli_version_output("agy-companion 0.0.4-rc.1"),
            Some("0.0.4-rc.1".to_string())
        );
        assert_eq!(
            parse_cli_version_output("agy-companion 0.1.0-dev.0\n"),
            Some("0.1.0-dev.0".to_string())
        );
        assert_eq!(
            parse_cli_version_output("agy-companion 0.1.0-dev.1"),
            Some("0.1.0-dev.1".to_string())
        );
        assert_eq!(
            parse_cli_version_output("agy-companion 0.2.0-dev.0\n"),
            Some("0.2.0-dev.0".to_string())
        );
        assert_eq!(
            parse_cli_version_output("agy-companion 0.2.0-dev.1"),
            Some("0.2.0-dev.1".to_string())
        );
        assert_eq!(parse_cli_version_output("other-bin 0.0.4\n"), None);
        assert_eq!(parse_cli_version_output("agy-companion ../0.0.4\n"), None);
        assert_eq!(parse_cli_version_output(""), None);
    }

    #[test]
    fn resolve_plugin_dir_from_precedence_and_s6_fail_closed() {
        let default_dir = PathBuf::from("/default/plugins");
        assert_eq!(
            resolve_plugin_dir_from(
                Some("/explicit/plugin"),
                Some("/ignored/plugins"),
                Some(default_dir.clone())
            ),
            Some(PathBuf::from("/explicit/plugin"))
        );
        assert_eq!(
            resolve_plugin_dir_from(None, Some("/custom/plugins"), Some(default_dir.clone())),
            Some(PathBuf::from("/custom/plugins/agy-companion"))
        );
        assert_eq!(
            resolve_plugin_dir_from(None, None, Some(default_dir.clone())),
            Some(PathBuf::from("/default/plugins/agy-companion"))
        );
        // S6 fail-closed: invalid traversal in explicit env must NOT fall back to default.
        assert_eq!(
            resolve_plugin_dir_from(Some("/bad/../plugin"), None, Some(default_dir.clone())),
            None
        );
        assert_eq!(
            resolve_plugin_dir_from(None, Some("/bad/../plugins"), Some(default_dir)),
            None
        );
    }

    #[test]
    fn read_plugin_manifest_version_enforces_s6_and_name_validation() {
        let temp = TempDir::new().expect("tempdir");

        // Missing plugin.json -> Absent
        assert_eq!(
            read_plugin_manifest_version(temp.path()),
            PluginManifestRead::Absent
        );

        // Valid manifest -> Valid("0.0.4")
        let manifest_path = temp.path().join("plugin.json");
        fs::write(
            &manifest_path,
            r#"{"name":"agy-companion","version":"0.0.4"}"#,
        )
        .expect("write valid manifest");
        assert_eq!(
            read_plugin_manifest_version(temp.path()),
            PluginManifestRead::Valid("0.0.4".to_string())
        );

        // Wrong plugin name -> Invalid
        fs::write(&manifest_path, r#"{"name":"other","version":"0.0.4"}"#)
            .expect("write wrong name");
        assert_eq!(
            read_plugin_manifest_version(temp.path()),
            PluginManifestRead::Invalid
        );

        // Path traversal / empty / whitespace version / malformed JSON -> Invalid
        for bad_json in [
            r#"{"name":"agy-companion","version":"../0.0.4"}"#,
            r#"{"name":"agy-companion","version":""}"#,
            r#"{"name":"agy-companion","version":"   "}"#,
            "{not-json",
        ] {
            fs::write(&manifest_path, bad_json).expect("write bad manifest");
            assert_eq!(
                read_plugin_manifest_version(temp.path()),
                PluginManifestRead::Invalid,
                "expected Invalid for {bad_json}"
            );
        }

        // Oversized (> 64 KiB) -> Invalid
        let oversized = format!(
            r#"{{"name":"agy-companion","version":"0.0.4","pad":"{}"}}"#,
            "a".repeat(MAX_PLUGIN_MANIFEST_BYTES as usize + 64)
        );
        fs::write(&manifest_path, oversized).expect("write oversized");
        assert_eq!(
            read_plugin_manifest_version(temp.path()),
            PluginManifestRead::Invalid
        );

        // Symlinked leaf plugin.json -> rejected fail-closed as Invalid (S6)
        #[cfg(unix)]
        {
            use std::os::unix::fs::symlink;
            let target = temp.path().join("real_manifest.json");
            fs::write(&target, r#"{"name":"agy-companion","version":"0.0.5"}"#)
                .expect("write target");
            fs::remove_file(&manifest_path).expect("remove old manifest");
            symlink(&target, &manifest_path).expect("symlink manifest");
            assert_eq!(
                read_plugin_manifest_version(temp.path()),
                PluginManifestRead::Invalid
            );

            // Symlinked plugin_dir parent with regular leaf plugin.json -> allowed (dev-install)
            let real_dir = temp.path().join("real_plugin_dir");
            fs::create_dir(&real_dir).expect("create real_dir");
            fs::write(
                real_dir.join("plugin.json"),
                r#"{"name":"agy-companion","version":"0.0.6"}"#,
            )
            .expect("write real_dir manifest");
            let linked_dir = temp.path().join("linked_plugin_dir");
            symlink(&real_dir, &linked_dir).expect("symlink dir");
            assert_eq!(
                read_plugin_manifest_version(&linked_dir),
                PluginManifestRead::Valid("0.0.6".to_string())
            );
        }
    }

    #[test]
    fn resolve_desired_version_fails_closed_on_invalid_or_transiently_absent_manifest() {
        let valid = PluginManifestRead::Valid("0.0.4".to_string());
        assert_eq!(
            resolve_desired_version(&valid, false, "0.0.3"),
            Some("0.0.4")
        );
        assert_eq!(
            resolve_desired_version(&valid, true, "0.0.3"),
            Some("0.0.4")
        );

        // Invalid manifest ALWAYS fails closed (never falls back to running_version).
        assert_eq!(
            resolve_desired_version(&PluginManifestRead::Invalid, false, "0.0.3"),
            None
        );
        assert_eq!(
            resolve_desired_version(&PluginManifestRead::Invalid, true, "0.0.3"),
            None
        );

        // Absent falls back to running_version only when no valid manifest was ever observed.
        assert_eq!(
            resolve_desired_version(&PluginManifestRead::Absent, false, "0.0.3"),
            Some("0.0.3")
        );
        assert_eq!(
            resolve_desired_version(&PluginManifestRead::Absent, true, "0.0.3"),
            None
        );
    }

    #[test]
    fn collect_candidate_binaries_from_orders_and_deduplicates() {
        let temp = TempDir::new().expect("tempdir");
        let repo = temp.path().join("repo");
        let plugin_dir = repo.join("plugin");
        let debug_dir = repo.join("target").join("debug");
        let release_dir = repo.join("target").join("release");
        fs::create_dir_all(&plugin_dir).expect("mkdir plugin");
        fs::create_dir_all(&debug_dir).expect("mkdir debug");
        fs::create_dir_all(&release_dir).expect("mkdir release");

        let canonical_repo = fs::canonicalize(&repo).unwrap_or_else(|_| repo.clone());
        let expected_debug = canonical_repo
            .join("target")
            .join("debug")
            .join(BIN_FILENAME);
        let expected_release = canonical_repo
            .join("target")
            .join("release")
            .join(BIN_FILENAME);

        let override_bin = temp.path().join("override-bin");
        let startup_exe = temp.path().join("startup-bin");
        let exec_data_dir = temp.path().join("exec-data");
        let default_data_dir = temp.path().join("xdg-data");
        let path_dir = temp.path().join("path-bin");

        let path_os = std::env::join_paths([&path_dir]).expect("join paths");
        let candidates = collect_candidate_binaries_from(CandidateDiscoveryInputs {
            bin_override_env: Some(override_bin.to_str().unwrap()),
            plugin_dir: Some(&plugin_dir),
            startup_exe: Some(&startup_exe),
            executable_data_dir_env: Some(exec_data_dir.to_str().unwrap()),
            default_data_dir: Some(&default_data_dir),
            path_env: Some(&path_os),
        });

        assert_eq!(candidates[0], override_bin);
        assert_eq!(candidates[1], expected_debug);
        assert_eq!(candidates[2], expected_release);
        assert_eq!(candidates[3], startup_exe);
        assert_eq!(candidates[4], exec_data_dir.join("bin").join(BIN_FILENAME));
        assert_eq!(
            candidates[5],
            default_data_dir.join("bin").join(BIN_FILENAME)
        );
        assert_eq!(candidates[6], path_dir.join(BIN_FILENAME));

        #[cfg(unix)]
        {
            use std::os::unix::fs::symlink;
            let plugins_root = temp.path().join("plugins");
            fs::create_dir_all(&plugins_root).expect("mkdir plugins");
            let linked_plugin = plugins_root.join("agy-companion");
            symlink(&plugin_dir, &linked_plugin).expect("symlink plugin dir");
            let linked_dev = dev_checkout_candidates(&linked_plugin);
            assert_eq!(linked_dev, vec![expected_debug, expected_release]);
        }
    }

    #[cfg(unix)]
    #[test]
    fn probe_cache_avoids_reexec_when_unchanged_and_invalidates_on_update() {
        use std::os::unix::fs::PermissionsExt;

        let temp = TempDir::new().expect("tempdir");
        let fake_bin = temp.path().join("agy-companion");
        fs::write(&fake_bin, "#!/bin/sh\necho 'agy-companion 0.0.3'\n").expect("write v3");
        fs::set_permissions(&fake_bin, fs::Permissions::from_mode(0o755)).expect("chmod");

        let mut cache = BinaryVersionProbeCache::new();
        let candidates = vec![fake_bin.clone()];

        // First probe: executes script (1 invocation), sees 0.0.3 != 0.0.4.
        assert_eq!(cache.find_matching_binary(&candidates, "0.0.4"), None);
        assert_eq!(cache.probe_invocations, 1);

        // Second probe with unchanged file: cache hit (still 1 invocation).
        assert_eq!(cache.find_matching_binary(&candidates, "0.0.4"), None);
        assert_eq!(cache.probe_invocations, 1);

        // Update file with different length (and newer mtime) -> invalidates cache -> matches!
        fs::write(
            &fake_bin,
            "#!/bin/sh\n# upgraded\necho 'agy-companion 0.0.4'\n",
        )
        .expect("write v4");
        fs::set_permissions(&fake_bin, fs::Permissions::from_mode(0o755)).expect("chmod");

        assert_eq!(
            cache.find_matching_binary(&candidates, "0.0.4"),
            Some(fake_bin.clone())
        );
        assert_eq!(cache.probe_invocations, 2);

        // Subsequent check with same updated file hits cache (still 2 invocations).
        assert_eq!(
            cache.find_matching_binary(&candidates, "0.0.4"),
            Some(fake_bin)
        );
        assert_eq!(cache.probe_invocations, 2);
    }

    #[test]
    fn dev_prerelease_manifest_and_desired_version_transitions() {
        let temp = TempDir::new().expect("tempdir");
        let manifest_path = temp.path().join("plugin.json");

        for ver in ["0.1.0-dev.0", "0.1.0-dev.1", "0.2.0-dev.0", "0.2.0-dev.1"] {
            fs::write(
                &manifest_path,
                format!(r#"{{"name":"agy-companion","version":"{ver}"}}"#),
            )
            .expect("write dev manifest");
            assert_eq!(
                read_plugin_manifest_version(temp.path()),
                PluginManifestRead::Valid(ver.to_string())
            );
        }

        // Dev-to-dev and dev-to-final desired version transitions
        assert_eq!(
            resolve_desired_version(
                &PluginManifestRead::Valid("0.1.0-dev.1".to_string()),
                true,
                "0.1.0-dev.0",
            ),
            Some("0.1.0-dev.1")
        );
        assert_eq!(
            resolve_desired_version(
                &PluginManifestRead::Valid("0.2.0-dev.1".to_string()),
                true,
                "0.2.0-dev.0",
            ),
            Some("0.2.0-dev.1")
        );
        assert_eq!(
            resolve_desired_version(
                &PluginManifestRead::Valid("0.2.0".to_string()),
                true,
                "0.2.0-dev.1",
            ),
            Some("0.2.0")
        );
        // Fail closed once a valid manifest was observed and then becomes invalid or absent
        assert_eq!(
            resolve_desired_version(&PluginManifestRead::Invalid, true, "0.2.0-dev.0"),
            None
        );
        assert_eq!(
            resolve_desired_version(&PluginManifestRead::Absent, true, "0.2.0-dev.0"),
            None
        );
    }

    #[cfg(unix)]
    #[test]
    fn probe_cache_tracks_dev_prerelease_transitions() {
        use std::os::unix::fs::PermissionsExt;

        let temp = TempDir::new().expect("tempdir");
        let fake_bin = temp.path().join("agy-companion");
        fs::write(&fake_bin, "#!/bin/sh\necho 'agy-companion 0.2.0-dev.0'\n").expect("write dev.0");
        fs::set_permissions(&fake_bin, fs::Permissions::from_mode(0o755)).expect("chmod");

        let mut cache = BinaryVersionProbeCache::new();
        let candidates = vec![fake_bin.clone()];

        // First probe executes script (1 invocation): 0.2.0-dev.0 != 0.2.0-dev.1.
        assert_eq!(cache.find_matching_binary(&candidates, "0.2.0-dev.1"), None);
        assert_eq!(cache.probe_invocations, 1);

        // Cache hit against final 0.2.0 and exact match 0.2.0-dev.0 (still 1 invocation).
        assert_eq!(cache.find_matching_binary(&candidates, "0.2.0"), None);
        assert_eq!(
            cache.find_matching_binary(&candidates, "0.2.0-dev.0"),
            Some(fake_bin.clone())
        );
        assert_eq!(cache.probe_invocations, 1);

        // Rewrite with different byte length so cache invalidates even within the same 1s mtime.
        fs::write(
            &fake_bin,
            "#!/bin/sh\n# upgraded to 0.2.0-dev.1\necho 'agy-companion 0.2.0-dev.1'\n",
        )
        .expect("write dev.1");
        fs::set_permissions(&fake_bin, fs::Permissions::from_mode(0o755)).expect("chmod");

        assert_eq!(
            cache.find_matching_binary(&candidates, "0.2.0-dev.1"),
            Some(fake_bin.clone())
        );
        assert_eq!(cache.probe_invocations, 2);
        assert_eq!(
            cache.find_matching_binary(&candidates, "0.2.0-dev.1"),
            Some(fake_bin.clone())
        );
        assert_eq!(cache.probe_invocations, 2);

        // Promote to final 0.2.0 (distinct byte length) -> invalidates cache and matches final.
        fs::write(
            &fake_bin,
            "#!/bin/sh\n# promoted to final release 0.2.0\necho 'agy-companion 0.2.0'\n",
        )
        .expect("write 0.2.0");
        fs::set_permissions(&fake_bin, fs::Permissions::from_mode(0o755)).expect("chmod");

        assert_eq!(cache.find_matching_binary(&candidates, "0.2.0-dev.1"), None);
        assert_eq!(
            cache.find_matching_binary(&candidates, "0.2.0"),
            Some(fake_bin)
        );
        assert_eq!(cache.probe_invocations, 3);

        // Also cover 0.1.0-dev.0 -> 0.1.0-dev.1 transition on a second candidate binary.
        let fake_bin_01 = temp.path().join("agy-companion-01");
        fs::write(
            &fake_bin_01,
            "#!/bin/sh\necho 'agy-companion 0.1.0-dev.0'\n",
        )
        .expect("write 0.1.0-dev.0");
        fs::set_permissions(&fake_bin_01, fs::Permissions::from_mode(0o755)).expect("chmod");
        let candidates_01 = vec![fake_bin_01.clone()];

        assert_eq!(
            cache.find_matching_binary(&candidates_01, "0.1.0-dev.1"),
            None
        );
        assert_eq!(
            cache.find_matching_binary(&candidates_01, "0.1.0-dev.0"),
            Some(fake_bin_01.clone())
        );
        assert_eq!(cache.probe_invocations, 4);

        fs::write(
            &fake_bin_01,
            "#!/bin/sh\n# upgraded to 0.1.0-dev.1\necho 'agy-companion 0.1.0-dev.1'\n",
        )
        .expect("write 0.1.0-dev.1");
        fs::set_permissions(&fake_bin_01, fs::Permissions::from_mode(0o755)).expect("chmod");

        assert_eq!(
            cache.find_matching_binary(&candidates_01, "0.1.0-dev.1"),
            Some(fake_bin_01)
        );
        assert_eq!(cache.probe_invocations, 5);
    }
}
