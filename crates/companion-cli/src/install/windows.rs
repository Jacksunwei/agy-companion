#[cfg(windows)]
use std::fs;
#[cfg(any(windows, test))]
use std::path::{Path, PathBuf};
#[cfg(windows)]
use std::process::Command;

#[cfg(windows)]
use companion_core::fs_safe::{
    FsError, default_platform_config_dir, inspect_regular_file_if_exists,
};

#[cfg(any(windows, test))]
use super::bundle::EMBEDDED_PLUGIN_FILES;
#[cfg(windows)]
use super::safety::{ExistingDirSafety, ensure_non_symlink_dir, inspect_non_symlink_rel_chain};
#[cfg(any(windows, test))]
use crate::CliError;
#[cfg(windows)]
use crate::process::CREATE_NO_WINDOW;

pub const WINDOWS_SHORTCUT_NAME: &str = "Antigravity (Companion).lnk";

#[cfg(any(windows, test))]
fn embedded_plugin_content(rel_path: &str) -> Result<&'static str, CliError> {
    EMBEDDED_PLUGIN_FILES
        .iter()
        .find(|entry| entry.rel_path == rel_path)
        .map(|entry| entry.content)
        .ok_or_else(|| {
            CliError::Install(format!(
                "embedded plugin bundle is missing required manifest '{rel_path}'"
            ))
        })
}

#[cfg(any(windows, test))]
fn strip_run_companion_wrapper_arg(
    args: Option<&mut serde_json::Value>,
    manifest_label: &str,
) -> Result<(), CliError> {
    let arr = args.and_then(|v| v.as_array_mut()).ok_or_else(|| {
        CliError::Install(format!("embedded {manifest_label} missing 'args' array"))
    })?;
    let is_wrapper = arr.first().and_then(|v| v.as_str()).is_some_and(|first| {
        first == "run-companion.sh"
            || first.ends_with("/run-companion.sh")
            || first.ends_with("\\run-companion.sh")
    });
    if !is_wrapper {
        return Err(CliError::Install(format!(
            "embedded {manifest_label} first arg is not a run-companion.sh wrapper"
        )));
    }
    arr.remove(0);
    Ok(())
}

#[cfg(any(windows, test))]
pub(crate) fn patch_windows_mcp_config_from_str(
    embedded_json: &str,
    binary_path: &Path,
) -> Result<String, CliError> {
    let mut payload: serde_json::Value =
        serde_json::from_str(embedded_json).map_err(|e| CliError::Serialization(e.to_string()))?;
    let hub = payload
        .get_mut("mcpServers")
        .and_then(|v| v.get_mut("hub"))
        .and_then(|v| v.as_object_mut())
        .ok_or_else(|| {
            CliError::Install(
                "embedded mcp_config.json missing 'mcpServers.hub' object".to_string(),
            )
        })?;
    hub.insert(
        "command".to_string(),
        serde_json::Value::String(binary_path.to_string_lossy().into_owned()),
    );
    strip_run_companion_wrapper_arg(hub.get_mut("args"), "mcp_config.json")?;
    let mut rendered = serde_json::to_string_pretty(&payload)
        .map_err(|e| CliError::Serialization(e.to_string()))?;
    rendered.push('\n');
    Ok(rendered)
}

#[cfg(any(windows, test))]
pub(crate) fn render_windows_mcp_config(binary_path: &Path) -> Result<String, CliError> {
    patch_windows_mcp_config_from_str(embedded_plugin_content("mcp_config.json")?, binary_path)
}

#[cfg(any(windows, test))]
pub(crate) fn patch_windows_sidecar_config_from_str(
    embedded_json: &str,
    binary_path: &Path,
) -> Result<String, CliError> {
    let mut payload: serde_json::Value =
        serde_json::from_str(embedded_json).map_err(|e| CliError::Serialization(e.to_string()))?;
    let obj = payload.as_object_mut().ok_or_else(|| {
        CliError::Install("embedded sidecars/proxy/sidecar.json is not a JSON object".to_string())
    })?;
    obj.insert(
        "command".to_string(),
        serde_json::Value::String(binary_path.to_string_lossy().into_owned()),
    );
    strip_run_companion_wrapper_arg(obj.get_mut("args"), "sidecars/proxy/sidecar.json")?;
    let mut rendered = serde_json::to_string_pretty(&payload)
        .map_err(|e| CliError::Serialization(e.to_string()))?;
    rendered.push('\n');
    Ok(rendered)
}

#[cfg(any(windows, test))]
pub(crate) fn render_windows_sidecar_config(binary_path: &Path) -> Result<String, CliError> {
    patch_windows_sidecar_config_from_str(
        embedded_plugin_content("sidecars/proxy/sidecar.json")?,
        binary_path,
    )
}

#[cfg(any(windows, test))]
pub(crate) fn resolve_windows_shortcut_path(app_data: &Path) -> PathBuf {
    app_data
        .join("Microsoft")
        .join("Windows")
        .join("Start Menu")
        .join("Programs")
        .join(WINDOWS_SHORTCUT_NAME)
}

#[cfg(any(windows, test))]
pub(crate) fn ps_single_quote(raw: &str) -> String {
    // PowerShell 5.1 treats both ASCII apostrophe and Unicode single-quote variants
    // (U+2018, U+2019, U+201A, U+201B) as single-quote string delimiters.
    let mut escaped = String::with_capacity(raw.len() + 2);
    escaped.push('\'');
    for ch in raw.chars() {
        if matches!(ch, '\'' | '\u{2018}' | '\u{2019}' | '\u{201A}' | '\u{201B}') {
            escaped.push(ch);
        }
        escaped.push(ch);
    }
    escaped.push('\'');
    escaped
}

#[cfg(any(windows, test))]
pub(crate) fn reject_ps_path_newlines(path: &Path) -> Result<(), CliError> {
    let text = path.to_string_lossy();
    if text.contains('\r') || text.contains('\n') {
        return Err(CliError::Install(format!(
            "refusing to embed path containing newline characters in PowerShell shortcut script: '{}'",
            path.display()
        )));
    }
    Ok(())
}

#[cfg(any(windows, test))]
pub(crate) fn render_windows_shortcut_ps_script(
    shortcut_path: &Path,
    binary_path: &Path,
    icon_path: Option<&Path>,
) -> String {
    let shortcut_lit = ps_single_quote(&shortcut_path.to_string_lossy());
    let target_lit = ps_single_quote(&binary_path.to_string_lossy());
    let working_dir = binary_path
        .parent()
        .map(|p| p.to_string_lossy().into_owned())
        .unwrap_or_default();
    let working_dir_lit = ps_single_quote(&working_dir);

    let mut script = format!(
        "$ErrorActionPreference = 'Stop'; \
$ws = New-Object -ComObject WScript.Shell; \
$s = $ws.CreateShortcut({shortcut_lit}); \
$s.TargetPath = {target_lit}; \
$s.Arguments = 'launch'; \
$s.WorkingDirectory = {working_dir_lit}; \
$s.WindowStyle = 7; \
$s.Description = 'Launch Antigravity with Antigravity Companion proxy'; "
    );
    if let Some(icon) = icon_path {
        let icon_lit = ps_single_quote(&format!("{},0", icon.to_string_lossy()));
        script.push_str(&format!("$s.IconLocation = {icon_lit}; "));
    }
    script.push_str("$s.Save();");
    script
}

#[cfg(windows)]
pub(crate) fn create_windows_start_menu_shortcut(binary_path: &Path) -> Result<PathBuf, CliError> {
    use std::os::windows::process::CommandExt;

    let app_data = std::env::var_os("APPDATA")
        .filter(|v| !v.to_string_lossy().trim().is_empty())
        .map(PathBuf::from)
        .or_else(default_platform_config_dir)
        .ok_or_else(|| {
            CliError::Install(
                "cannot resolve %APPDATA% for Windows Start Menu shortcut".to_string(),
            )
        })?;
    companion_core::validate_path(&app_data)?;
    let mut shortcut_warnings = Vec::new();
    if inspect_non_symlink_rel_chain(
        &app_data,
        &["Microsoft", "Windows", "Start Menu", "Programs"],
        "Start Menu Programs directory",
        &mut shortcut_warnings,
    )? == ExistingDirSafety::Unsafe
    {
        return Err(CliError::Install(
            shortcut_warnings.into_iter().next().unwrap_or_else(|| {
                "refusing to traverse symlinked Start Menu directory".to_string()
            }),
        ));
    }
    let shortcut_path = resolve_windows_shortcut_path(&app_data);
    if let Some(parent) = shortcut_path.parent() {
        ensure_non_symlink_dir(parent, 0o755)?;
    }
    match inspect_regular_file_if_exists(&shortcut_path, None) {
        Ok(_) => {}
        Err(FsError::SymlinkRejected { .. } | FsError::NotRegularFile { .. }) => {
            return Err(CliError::Install(format!(
                "refusing to overwrite symlinked/non-regular Start Menu shortcut '{}'",
                shortcut_path.display()
            )));
        }
        Err(FsError::Io { source, .. }) => return Err(CliError::Io(source)),
        Err(err) => return Err(CliError::from(err)),
    }

    let discovered_app = crate::launch::default_antigravity_app_path();
    let icon_path = discovered_app.is_file().then_some(discovered_app.as_path());
    reject_ps_path_newlines(&shortcut_path)?;
    reject_ps_path_newlines(binary_path)?;
    if let Some(icon) = icon_path {
        reject_ps_path_newlines(icon)?;
    }
    let script = render_windows_shortcut_ps_script(&shortcut_path, binary_path, icon_path);

    let output = Command::new("powershell.exe")
        .stdin(std::process::Stdio::null())
        .args([
            "-NoProfile",
            "-NonInteractive",
            "-InputFormat",
            "None",
            "-WindowStyle",
            "Hidden",
            "-Command",
            &script,
        ])
        .creation_flags(CREATE_NO_WINDOW)
        .output()?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(CliError::Install(format!(
            "failed to create Start Menu shortcut '{}': {}",
            shortcut_path.display(),
            stderr.trim()
        )));
    }
    Ok(shortcut_path)
}

#[cfg(windows)]
pub(crate) fn remove_windows_start_menu_shortcut(
    dry_run: bool,
    warnings: &mut Vec<String>,
) -> Result<bool, CliError> {
    let Some(app_data) = std::env::var_os("APPDATA")
        .filter(|v| !v.to_string_lossy().trim().is_empty())
        .map(PathBuf::from)
        .or_else(default_platform_config_dir)
    else {
        return Ok(false);
    };
    if companion_core::validate_path(&app_data).is_err() {
        return Ok(false);
    }
    if inspect_non_symlink_rel_chain(
        &app_data,
        &["Microsoft", "Windows", "Start Menu", "Programs"],
        "Start Menu Programs directory",
        warnings,
    )? != ExistingDirSafety::SafeDir
    {
        return Ok(false);
    }
    let shortcut_path = resolve_windows_shortcut_path(&app_data);
    match inspect_regular_file_if_exists(&shortcut_path, None) {
        Ok(Some(_)) => {
            if !dry_run {
                fs::remove_file(&shortcut_path)?;
            }
            Ok(true)
        }
        Ok(None) | Err(FsError::SymlinkRejected { .. } | FsError::NotRegularFile { .. }) => {
            Ok(false)
        }
        Err(FsError::Io { source, .. }) => Err(CliError::Io(source)),
        Err(err) => Err(CliError::from(err)),
    }
}
