use std::fs::{self, File};
use std::io::Write;
use std::net::{Ipv4Addr, TcpListener};
use std::path::{Path, PathBuf};

#[cfg(unix)]
use companion_core::fs_safe::{DirChainStatus, ensure_dir_no_symlink, inspect_dir_chain};
use companion_core::fs_safe::{
    FsError, PathLeafKind, SymlinkPolicy, TempPathGuard, inspect_leaf_kind,
    read_regular_file_bounded, rename_with_retry, unique_temp_suffix, unlink_symlink,
};
use companion_providers::LoopbackProxyClient;

#[cfg(windows)]
use super::windows::{render_windows_mcp_config, render_windows_sidecar_config};
use crate::process::FRESH_PORT_PROBE_TIMEOUT;
use crate::{BINARY_NAME, CliError, normalized_metadata_path};

pub const PLUGIN_DIR_NAME: &str = "agy-companion";
const MAX_PLUGIN_MANIFEST_BYTES: u64 = 1024 * 1024;

#[cfg(windows)]
pub(crate) const STABLE_BIN_NAME: &str = "agy-companion.exe";
#[cfg(not(windows))]
pub(crate) const STABLE_BIN_NAME: &str = "agy-companion";

pub struct EmbeddedPluginFile {
    pub rel_path: &'static str,
    pub content: &'static str,
    pub executable: bool,
}

/// All 7 bundled plugin files, ordered with `plugin.json` LAST so staging writes the
/// manifest only after scripts, assets, docs, and sidecar/MCP configs are already on disk.
pub const EMBEDDED_PLUGIN_FILES: &[EmbeddedPluginFile] = &[
    EmbeddedPluginFile {
        rel_path: "README.md",
        content: include_str!("../../../../plugin/README.md"),
        executable: false,
    },
    EmbeddedPluginFile {
        rel_path: "assets/logo.svg",
        content: include_str!("../../../../plugin/assets/logo.svg"),
        executable: false,
    },
    EmbeddedPluginFile {
        rel_path: "mcp_config.json",
        content: include_str!("../../../../plugin/mcp_config.json"),
        executable: false,
    },
    EmbeddedPluginFile {
        rel_path: "sidecars/proxy/sidecar.json",
        content: include_str!("../../../../plugin/sidecars/proxy/sidecar.json"),
        executable: false,
    },
    EmbeddedPluginFile {
        rel_path: "skills/companion/SKILL.md",
        content: include_str!("../../../../plugin/skills/companion/SKILL.md"),
        executable: false,
    },
    EmbeddedPluginFile {
        rel_path: "scripts/run-companion.sh",
        content: include_str!("../../../../plugin/scripts/run-companion.sh"),
        executable: true,
    },
    EmbeddedPluginFile {
        rel_path: "plugin.json",
        content: include_str!("../../../../plugin/plugin.json"),
        executable: false,
    },
];

/// Checks whether `plugin_dir/plugin.json` is a regular non-symlink JSON file declaring
/// `"name": "agy-companion"`.
pub(crate) fn is_companion_plugin_dir(plugin_dir: &Path) -> Result<bool, CliError> {
    let manifest_path = plugin_dir.join("plugin.json");
    let bytes = match read_regular_file_bounded(
        &manifest_path,
        MAX_PLUGIN_MANIFEST_BYTES,
        SymlinkPolicy::Reject,
    ) {
        Ok(Some(res)) => res.bytes,
        Ok(None)
        | Err(
            FsError::SymlinkRejected { .. }
            | FsError::NotRegularFile { .. }
            | FsError::SizeLimitExceeded { .. }
            | FsError::ConcurrentModification { .. },
        ) => return Ok(false),
        Err(FsError::Io { source, .. }) => return Err(CliError::Io(source)),
        Err(err) => return Err(CliError::from(err)),
    };

    let parsed: serde_json::Value = match serde_json::from_slice(&bytes) {
        Ok(value) => value,
        Err(_) => return Ok(false),
    };
    Ok(parsed.get("name").and_then(|v| v.as_str()) == Some(PLUGIN_DIR_NAME))
}

/// Probes `start_port..=end_port` on `127.0.0.1` for a fresh install when no `config.json` exists.
///
/// If `check_companion_on_first` is true and `start_port` already hosts a healthy `agy-companion`
/// proxy, `start_port` is selected so `install` can reuse or upgrade it. Otherwise the first
/// bindable port in the inclusive range is returned along with `port_auto_selected`.
pub(crate) async fn select_fresh_install_port(
    probe: &LoopbackProxyClient,
    start_port: u16,
    end_port: u16,
    check_companion_on_first: bool,
) -> Result<(u16, bool), CliError> {
    if check_companion_on_first
        && probe
            .probe_healthy_companion(start_port, Some(FRESH_PORT_PROBE_TIMEOUT))
            .await
            .is_some()
    {
        return Ok((start_port, false));
    }

    for port in start_port..=end_port {
        if let Ok(listener) = TcpListener::bind((Ipv4Addr::LOCALHOST, port)) {
            drop(listener);
            return Ok((port, port != start_port));
        }
    }

    Err(CliError::Install(format!(
        "no free loopback port available in {start_port}..={end_port}; free a port or preconfigure listen_port in config.json"
    )))
}

pub(crate) fn stage_binary_copy(
    src_exe: &Path,
    bin_dir: &Path,
) -> Result<(PathBuf, TempPathGuard), CliError> {
    let staged_path = bin_dir.join(format!(".{BINARY_NAME}.new.{}", unique_temp_suffix()));
    let guard = TempPathGuard::file(staged_path.clone());

    let mut reader = File::open(src_exe)?;
    let mut open_opts = fs::OpenOptions::new();
    open_opts.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        open_opts.mode(0o755);
    }
    let mut writer = open_opts.open(&staged_path)?;
    let mut copy_buf = vec![0u8; 256 * 1024];
    loop {
        let n = std::io::Read::read(&mut reader, &mut copy_buf)?;
        if n == 0 {
            break;
        }
        writer.write_all(&copy_buf[..n])?;
    }
    writer.sync_all()?;
    drop(writer);

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&staged_path, fs::Permissions::from_mode(0o755))?;
    }

    Ok((staged_path, guard))
}

pub(crate) fn stage_version_file(
    bin_dir: &Path,
    version: &str,
) -> Result<(PathBuf, TempPathGuard), CliError> {
    let staged_path = bin_dir.join(format!(".version.new.{}", unique_temp_suffix()));
    let guard = TempPathGuard::file(staged_path.clone());

    let mut open_opts = fs::OpenOptions::new();
    open_opts.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        open_opts.mode(0o644);
    }
    let mut writer = open_opts.open(&staged_path)?;
    writer.write_all(format!("{version}\n").as_bytes())?;
    writer.sync_all()?;
    drop(writer);

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&staged_path, fs::Permissions::from_mode(0o644))?;
    }

    Ok((staged_path, guard))
}

pub(crate) fn write_staged_plugin_dir(
    plugins_root: &Path,
    #[allow(unused_variables)] stable_bin: &Path,
) -> Result<(PathBuf, TempPathGuard), CliError> {
    let stage_dir = plugins_root.join(format!(".{PLUGIN_DIR_NAME}.stage.{}", unique_temp_suffix()));
    fs::create_dir(&stage_dir)?;
    let guard = TempPathGuard::dir(stage_dir.clone());

    for entry in EMBEDDED_PLUGIN_FILES {
        let dest = stage_dir.join(entry.rel_path);
        if let Some(parent) = dest.parent() {
            fs::create_dir_all(parent)?;
        }

        #[cfg(windows)]
        let rendered_override: Option<String> = match entry.rel_path {
            "mcp_config.json" => Some(render_windows_mcp_config(stable_bin)?),
            "sidecars/proxy/sidecar.json" => Some(render_windows_sidecar_config(stable_bin)?),
            _ => None,
        };
        #[cfg(not(windows))]
        let rendered_override: Option<String> = None;

        let bytes: &[u8] = match rendered_override.as_deref() {
            Some(content) => content.as_bytes(),
            None => entry.content.as_bytes(),
        };

        let mut open_opts = fs::OpenOptions::new();
        open_opts.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            open_opts.mode(if entry.executable { 0o755 } else { 0o644 });
        }
        let mut file = open_opts.open(&dest)?;
        file.write_all(bytes)?;
        file.sync_all()?;
        drop(file);

        #[cfg(unix)]
        if entry.executable {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&dest, fs::Permissions::from_mode(0o755))?;
        }
    }

    Ok((stage_dir, guard))
}

#[cfg(unix)]
pub(crate) fn symlink_points_to_path(link_path: &Path, target_path: &Path) -> bool {
    let Ok(raw_target) = fs::read_link(link_path) else {
        return false;
    };
    let resolved_link = if raw_target.is_absolute() {
        raw_target
    } else {
        link_path
            .parent()
            .unwrap_or_else(|| Path::new("."))
            .join(raw_target)
    };
    if normalized_metadata_path(&resolved_link) == normalized_metadata_path(target_path) {
        return true;
    }
    match (
        fs::canonicalize(&resolved_link),
        fs::canonicalize(target_path),
    ) {
        (Ok(a), Ok(b)) => a == b,
        _ => false,
    }
}

#[cfg(unix)]
pub(crate) fn install_local_bin_symlink(
    home: &Path,
    stable_bin: &Path,
    warnings: &mut Vec<String>,
) -> Option<PathBuf> {
    use std::os::unix::fs::symlink;

    let dot_local_dir = home.join(".local");
    let local_bin_dir = dot_local_dir.join("bin");
    let local_bin_link = local_bin_dir.join(BINARY_NAME);

    // Ensure ~/.local and ~/.local/bin are non-symlink directories (or create them).
    match inspect_dir_chain(home, &[".local", "bin"]) {
        Ok(DirChainStatus::SafeDir) => {}
        Ok(DirChainStatus::Missing) => {
            for dir in [&dot_local_dir, &local_bin_dir] {
                match ensure_dir_no_symlink(dir, 0o755) {
                    Ok(()) => {}
                    Err(FsError::SymlinkRejected { path } | FsError::NotDirectory { path }) => {
                        warnings.push(format!(
                            "skipping {} symlink because '{}' is not a regular directory",
                            local_bin_link.display(),
                            path.display()
                        ));
                        return None;
                    }
                    Err(FsError::Io { source, .. }) => {
                        warnings.push(format!("failed to create '{}': {source}", dir.display()));
                        return None;
                    }
                    Err(err) => {
                        warnings.push(format!("failed to create '{}': {err}", dir.display()));
                        return None;
                    }
                }
            }
        }
        Ok(DirChainStatus::Symlink { path, .. } | DirChainStatus::NotDirectory { path }) => {
            warnings.push(format!(
                "skipping {} symlink because '{}' is not a regular directory",
                local_bin_link.display(),
                path.display()
            ));
            return None;
        }
        Err(err) => {
            warnings.push(format!(
                "failed to inspect '{}': {err}",
                local_bin_dir.display()
            ));
            return None;
        }
    }

    match inspect_leaf_kind(&local_bin_link) {
        Ok(PathLeafKind::Missing) => {
            if let Err(sym_err) = symlink(stable_bin, &local_bin_link) {
                warnings.push(format!(
                    "failed to create symlink '{}': {sym_err}",
                    local_bin_link.display()
                ));
                return None;
            }
            Some(local_bin_link)
        }
        Ok(PathLeafKind::Symlink) => {
            if !symlink_points_to_path(&local_bin_link, stable_bin) {
                warnings.push(format!(
                    "leaving existing foreign symlink '{}' untouched",
                    local_bin_link.display()
                ));
                return None;
            }
            let tmp_link =
                local_bin_dir.join(format!(".{BINARY_NAME}.link.{}", unique_temp_suffix()));
            let mut tmp_guard = TempPathGuard::file(tmp_link.clone());
            if let Err(sym_err) = symlink(stable_bin, &tmp_link) {
                warnings.push(format!(
                    "failed to refresh symlink '{}': {sym_err}",
                    local_bin_link.display()
                ));
                return Some(local_bin_link);
            }
            if let Err(ren_err) = rename_with_retry(&tmp_link, &local_bin_link) {
                warnings.push(format!(
                    "failed to replace symlink '{}': {ren_err}",
                    local_bin_link.display()
                ));
                return Some(local_bin_link);
            }
            tmp_guard.disarm();
            Some(local_bin_link)
        }
        Ok(PathLeafKind::RegularFile | PathLeafKind::Directory | PathLeafKind::Other) => {
            warnings.push(format!(
                "leaving existing non-symlink '{}' untouched",
                local_bin_link.display()
            ));
            None
        }
        Err(err) => {
            warnings.push(format!(
                "failed to inspect '{}': {err}",
                local_bin_link.display()
            ));
            None
        }
    }
}

pub(crate) fn swap_staged_binary_and_version(
    staged_bin: Option<(PathBuf, TempPathGuard)>,
    stable_bin: &Path,
    (staged_ver, mut ver_guard): (PathBuf, TempPathGuard),
    version_file: &Path,
) -> Result<(), CliError> {
    if let Some((tmp_bin, mut bin_guard)) = staged_bin {
        #[cfg(not(windows))]
        {
            rename_with_retry(&tmp_bin, stable_bin)?;
            bin_guard.disarm();
        }
        #[cfg(windows)]
        {
            match rename_with_retry(&tmp_bin, stable_bin) {
                Ok(()) => bin_guard.disarm(),
                Err(_) => {
                    let old_bin = stable_bin
                        .parent()
                        .unwrap_or_else(|| Path::new("."))
                        .join(format!(".{BINARY_NAME}.old.{}.exe", unique_temp_suffix()));
                    if stable_bin.exists() {
                        rename_with_retry(stable_bin, &old_bin)?;
                    }
                    if let Err(err) = rename_with_retry(&tmp_bin, stable_bin) {
                        let _ = rename_with_retry(&old_bin, stable_bin);
                        return Err(CliError::from(err));
                    }
                    bin_guard.disarm();
                    let _ = fs::remove_file(&old_bin);
                }
            }
        }
    }

    rename_with_retry(&staged_ver, version_file)?;
    ver_guard.disarm();
    Ok(())
}

pub(crate) fn swap_plugin_dir_two_step(
    plugins_root: &Path,
    plugin_target: &Path,
    stage_dir: &Path,
    mut stage_guard: TempPathGuard,
    warnings: &mut Vec<String>,
) -> Result<(), CliError> {
    match inspect_leaf_kind(plugin_target) {
        Ok(PathLeafKind::Missing) => {
            rename_with_retry(stage_dir, plugin_target)?;
            stage_guard.disarm();
            Ok(())
        }
        Ok(PathLeafKind::Symlink) => {
            #[cfg(unix)]
            let orig_target = fs::read_link(plugin_target).ok();
            unlink_symlink(plugin_target)?;
            if let Err(rename_err) = rename_with_retry(stage_dir, plugin_target) {
                #[cfg(unix)]
                if let Some(target) = orig_target {
                    let _ = std::os::unix::fs::symlink(target, plugin_target);
                }
                return Err(CliError::from(rename_err));
            }
            stage_guard.disarm();
            Ok(())
        }
        Ok(PathLeafKind::Directory) => {
            let backup_dir = plugins_root.join(format!(
                ".{PLUGIN_DIR_NAME}.backup.{}",
                unique_temp_suffix()
            ));
            rename_with_retry(plugin_target, &backup_dir)?;
            if let Err(rename_err) = rename_with_retry(stage_dir, plugin_target) {
                let _ = rename_with_retry(&backup_dir, plugin_target);
                return Err(CliError::from(rename_err));
            }
            stage_guard.disarm();
            if let Err(rm_err) = fs::remove_dir_all(&backup_dir) {
                warnings.push(format!(
                    "failed to clean up previous plugin backup '{}': {rm_err}",
                    backup_dir.display()
                ));
            }
            Ok(())
        }
        Ok(PathLeafKind::RegularFile | PathLeafKind::Other) => Err(CliError::Install(format!(
            "cannot install plugin at '{}': path exists and is not a directory",
            plugin_target.display()
        ))),
        Err(FsError::Io { source, .. }) => Err(CliError::Io(source)),
        Err(err) => Err(CliError::from(err)),
    }
}

pub(crate) fn is_same_file_path(a: &Path, b: &Path) -> bool {
    if normalized_metadata_path(a) == normalized_metadata_path(b) {
        return true;
    }
    match (fs::canonicalize(a), fs::canonicalize(b)) {
        (Ok(ca), Ok(cb)) => ca == cb,
        _ => false,
    }
}
