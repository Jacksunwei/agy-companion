use std::collections::BTreeMap;
use std::fs;
use std::net::{Ipv4Addr, TcpListener};
use std::path::{Path, PathBuf};

use companion_core::fs_safe::TempPathGuard;
use companion_providers::LoopbackProxyClient;
use tempfile::TempDir;

use super::bundle::{
    EMBEDDED_PLUGIN_FILES, PLUGIN_DIR_NAME, STABLE_BIN_NAME, files_have_identical_bytes,
    is_companion_plugin_dir, select_fresh_install_port, swap_plugin_dir_two_step,
    write_staged_plugin_dir,
};
#[cfg(unix)]
use super::proxy::verify_same_version_replacement_owner;
use super::proxy::{
    InstallProxyContext, ProxyReplaceReason, classify_proxy_replacement,
    ensure_installed_proxy_running,
};
#[cfg(unix)]
use super::safety::inspect_non_symlink_rel_chain;
use super::safety::{ExistingDirSafety, inspect_existing_dir, usage_dir_under_dir};
use super::windows::{
    WINDOWS_SHORTCUT_NAME, patch_windows_sidecar_config_from_str, ps_single_quote,
    reject_ps_path_newlines, render_windows_shortcut_ps_script, render_windows_sidecar_config,
    resolve_windows_shortcut_path,
};
use crate::CliError;
use crate::daemon::{PROXY_PID_FILENAME, ProxyPidInfo, read_proxy_pid};
use crate::process::{
    CREATE_NO_WINDOW, PidVerifyResult, is_companion_serve_process, remove_pid_file_if_matches,
    stop_proxy_from_pid_files, verify_proxy_pid_process,
};

fn collect_repo_files(root: &Path, dir: &Path, out: &mut BTreeMap<String, PathBuf>) {
    let mut entries: Vec<_> = fs::read_dir(dir)
        .expect("read_dir")
        .map(|r| r.expect("dir entry"))
        .collect();
    entries.sort_by_key(|e| e.file_name());
    for entry in entries {
        let path = entry.path();
        let meta = fs::symlink_metadata(&path).expect("symlink_metadata");
        assert!(
            !meta.file_type().is_symlink(),
            "plugin directory must not contain symlinks: {}",
            path.display()
        );
        if meta.file_type().is_dir() {
            collect_repo_files(root, &path, out);
        } else {
            assert!(
                meta.file_type().is_file(),
                "plugin entry must be a regular file: {}",
                path.display()
            );
            let rel = path
                .strip_prefix(root)
                .expect("strip_prefix")
                .to_string_lossy()
                .replace('\\', "/");
            out.insert(rel, path);
        }
    }
}

#[test]
fn embedded_plugin_bundle_matches_repo_plugin_dir_byte_for_byte() {
    let repo_plugin_dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../plugin");
    let mut disk_files = BTreeMap::new();
    collect_repo_files(&repo_plugin_dir, &repo_plugin_dir, &mut disk_files);

    assert_eq!(
        EMBEDDED_PLUGIN_FILES.len(),
        6,
        "expected exactly 6 embedded plugin files"
    );
    assert_eq!(
        EMBEDDED_PLUGIN_FILES.last().expect("non-empty").rel_path,
        "plugin.json",
        "plugin.json must be written last during staging"
    );
    assert_eq!(
        disk_files.len(),
        EMBEDDED_PLUGIN_FILES.len(),
        "repo plugin/ file count drifted from EMBEDDED_PLUGIN_FILES: {:?}",
        disk_files.keys().collect::<Vec<_>>()
    );

    for entry in EMBEDDED_PLUGIN_FILES {
        let disk_path = disk_files
            .get(entry.rel_path)
            .unwrap_or_else(|| panic!("missing repo plugin file '{}'", entry.rel_path));
        let disk_bytes = fs::read(disk_path).expect("read disk plugin file");
        assert_eq!(
            disk_bytes,
            entry.content.as_bytes(),
            "embedded content drifted from disk for '{}'",
            entry.rel_path
        );
        #[cfg(unix)]
        if entry.executable {
            use std::os::unix::fs::PermissionsExt;
            let mode = fs::metadata(disk_path)
                .expect("metadata")
                .permissions()
                .mode();
            assert_ne!(
                mode & 0o111,
                0,
                "expected executable bit on '{}'",
                entry.rel_path
            );
        }
    }
}

#[test]
fn embedded_plugin_docs_enforce_open_source_copy_hygiene_and_ux_v2_contracts() {
    let find_embedded = |rel_path: &str| -> &'static str {
        EMBEDDED_PLUGIN_FILES
            .iter()
            .find(|f| f.rel_path == rel_path)
            .unwrap_or_else(|| panic!("missing embedded plugin file '{rel_path}'"))
            .content
    };

    // 1. Open-source copy hygiene: zero internal codenames, raw *-pa endpoints, or stale labels
    // across all user-facing plugin docs and JSON manifests.
    let forbidden_ci = [
        "cloudcode",
        "jetski",
        "-cloudcode-pa.googleapis.com",
        "-pa.googleapis.com",
        "restart required",
        "agy companion",
        "coming soon",
    ];
    for rel_path in [
        "README.md",
        "skills/companion/SKILL.md",
        "plugin.json",
        "sidecars/proxy/sidecar.json",
    ] {
        let lower = find_embedded(rel_path).to_lowercase();
        for token in forbidden_ci {
            assert!(
                !lower.contains(token),
                "forbidden open-source hygiene token `{token}` found in embedded plugin/{rel_path}"
            );
        }
    }

    // 2. SKILL.md structural & UX v2 contract (mirrors agy plugin validate + E2E R25/F10).
    let skill = find_embedded("skills/companion/SKILL.md");
    assert!(
        skill.len() <= 24_000,
        "embedded SKILL.md exceeds 24,000-byte budget ({} bytes)",
        skill.len()
    );
    assert!(
        skill.starts_with("---\n") || skill.starts_with("---\r\n"),
        "embedded SKILL.md must start with YAML frontmatter"
    );
    let mut parts = skill.splitn(3, "---");
    let _ = parts.next();
    let frontmatter = parts.next().expect("SKILL.md frontmatter block");
    assert!(
        frontmatter.lines().any(|l| l.trim() == "name: companion"),
        "SKILL.md frontmatter must declare `name: companion`"
    );
    assert!(
        frontmatter
            .lines()
            .any(|l| l.trim_start().starts_with("description:")),
        "SKILL.md frontmatter must declare `description:`"
    );
    for endpoint_name in [
        "sidecar://agy-companion/proxy/",
        "/_api/get_settings",
        "/_api/save_settings",
        "/_api/test_connection",
        "/_api/set_launcher_integration",
        "/_api/get_usage",
    ] {
        assert!(
            skill.contains(endpoint_name),
            "embedded SKILL.md must document Sidecar Web UI endpoint `{endpoint_name}`"
        );
    }
    let skill_lower = skill.to_lowercase();
    for ux_term in [
        "monitor",
        "usage",
        "settings",
        "enabled models",
        "antigravity plan",
    ] {
        assert!(
            skill_lower.contains(ux_term),
            "embedded SKILL.md missing UX v2 concept `{ux_term}`"
        );
    }

    // 3. plugin/README.md UX v2 & install essentials.
    let readme = find_embedded("README.md");
    assert!(
        readme.starts_with("# Antigravity Companion"),
        "embedded plugin/README.md must start with `# Antigravity Companion`"
    );
    let readme_lower = readme.to_lowercase();
    for ux_term in [
        "monitor",
        "usage",
        "settings",
        "enabled models",
        "antigravity plan",
        "npx -y agy-companion@latest install",
        "uninstall",
    ] {
        assert!(
            readme_lower.contains(ux_term),
            "embedded plugin/README.md missing UX v2 / install term `{ux_term}`"
        );
    }

    // 4. Runtime protocol guard: run-companion.sh MUST retain MCP env-restore scrubbing.
    let launcher = find_embedded("scripts/run-companion.sh");
    assert!(
        launcher.contains("JETSKI_MCP_ENV_RESTORE_PREFIX")
            && launcher.contains("JETSKI_MCP_ENV_RESTORE_VARS"),
        "plugin/scripts/run-companion.sh must preserve runtime MCP env-restore scrubbing"
    );
}

#[test]
fn windows_manifest_generators_emit_direct_exe_json_without_bash() {
    let bin_path = Path::new(r"C:\Users\alice\AppData\Local\agy-companion\bin\agy-companion.exe");

    let sidecar_raw =
        render_windows_sidecar_config(bin_path).expect("render windows sidecar config");
    assert!(
        !sidecar_raw.contains("bash") && !sidecar_raw.contains("run-companion.sh"),
        "Windows sidecar.json must not reference bash: {sidecar_raw}"
    );
    let sidecar_json: serde_json::Value =
        serde_json::from_str(&sidecar_raw).expect("parse sidecar json");
    assert_eq!(
        sidecar_json.as_object().expect("sidecar object").len(),
        7,
        "Windows sidecar.json must contain only the 7 strict-valid schema keys: {sidecar_raw}"
    );
    assert_eq!(sidecar_json["has_web_ui"], serde_json::json!(true));
    assert_eq!(
        sidecar_json["ui_config"]["views"][0]["entrypoint"],
        "SIDECAR_UI_ENTRYPOINT_AUX_PANE"
    );
    assert!(
        sidecar_json.get("name").is_none(),
        "unknown field 'name' fails agy plugin validate --strict"
    );
    assert!(
        sidecar_json.get("health_check").is_none(),
        "unknown field 'health_check' fails agy plugin validate --strict"
    );
    assert_eq!(sidecar_json["command"], bin_path.to_string_lossy().as_ref());
    assert_eq!(
        sidecar_json["args"],
        serde_json::json!(["serve", "--adopt-running", "--auto-enable-launcher"])
    );
    assert_eq!(sidecar_json["restart_policy"], "on-failure");
    assert_eq!(sidecar_json["display_name"], "Antigravity Companion");
    assert_eq!(
        sidecar_json["description"],
        "Local loopback Gemini pass-through proxy with live session monitoring, usage analytics, and BYOM settings."
    );
}

#[test]
fn windows_manifest_generators_derive_from_embedded_json_and_preserve_future_fields() {
    let bin_path = Path::new(r"C:\Users\alice\AppData\Local\agy-companion\bin\agy-companion.exe");

    // 1. Live embedded manifest: every non-command field and post-wrapper arg must match verbatim.
    let embedded_sidecar: serde_json::Value = serde_json::from_str(
        EMBEDDED_PLUGIN_FILES
            .iter()
            .find(|f| f.rel_path == "sidecars/proxy/sidecar.json")
            .expect("embedded sidecar.json")
            .content,
    )
    .expect("parse embedded sidecar.json");
    let rendered_sidecar: serde_json::Value =
        serde_json::from_str(&render_windows_sidecar_config(bin_path).expect("render sidecar"))
            .expect("parse rendered sidecar");
    let expected_sidecar_args: Vec<serde_json::Value> = embedded_sidecar["args"]
        .as_array()
        .expect("embedded sidecar args")
        .iter()
        .skip(1)
        .cloned()
        .collect();
    assert_eq!(
        rendered_sidecar["args"],
        serde_json::Value::Array(expected_sidecar_args)
    );
    for (key, val) in embedded_sidecar
        .as_object()
        .expect("embedded sidecar object")
    {
        if key != "command" && key != "args" {
            assert_eq!(
                rendered_sidecar.get(key),
                Some(val),
                "Windows sidecar.json drifted on field '{key}'"
            );
        }
    }

    // 2. Synthetic future fields and extra args MUST flow through automatically without drift.
    let synthetic_sidecar = r#"{
      "command": "bash",
      "args": ["../../scripts/run-companion.sh", "serve", "--adopt-running", "--idle-shutdown-secs=900"],
      "restart_policy": "always",
      "display_name": "Updated Proxy Title",
      "description": "Updated description.",
      "timeout_secs": 45
    }"#;
    let patched_sidecar: serde_json::Value = serde_json::from_str(
        &patch_windows_sidecar_config_from_str(synthetic_sidecar, bin_path)
            .expect("patch synthetic sidecar"),
    )
    .expect("parse patched synthetic sidecar");
    assert_eq!(
        patched_sidecar["command"],
        bin_path.to_string_lossy().as_ref()
    );
    assert_eq!(
        patched_sidecar["args"],
        serde_json::json!(["serve", "--adopt-running", "--idle-shutdown-secs=900"])
    );
    assert_eq!(patched_sidecar["restart_policy"], "always");
    assert_eq!(patched_sidecar["display_name"], "Updated Proxy Title");
    assert_eq!(patched_sidecar["description"], "Updated description.");
    assert_eq!(patched_sidecar["timeout_secs"], 45);

    // 3. Malformed or non-wrapper manifests fail closed.
    let missing_wrapper = r#"{"command": "bash", "args": ["serve", "--adopt-running"]}"#;
    assert!(
        patch_windows_sidecar_config_from_str(missing_wrapper, bin_path).is_err(),
        "sidecar manifest without leading run-companion.sh wrapper must fail closed"
    );
}

#[test]
fn windows_shortcut_path_and_ps_script_escape_quotes_and_hide_console() {
    let app_data = Path::new(r"C:\Users\O'Brien\AppData\Roaming");
    let shortcut = resolve_windows_shortcut_path(app_data);
    assert!(
        shortcut.to_string_lossy().ends_with(WINDOWS_SHORTCUT_NAME),
        "shortcut path should end with {WINDOWS_SHORTCUT_NAME}: {}",
        shortcut.display()
    );

    let bin = Path::new(r"C:\Users\O'Brien\AppData\Local\agy-companion\bin\agy-companion.exe");
    let icon = Path::new(r"C:\Users\O'Brien\AppData\Local\Programs\Antigravity\Antigravity.exe");
    let script = render_windows_shortcut_ps_script(&shortcut, bin, Some(icon));
    assert!(script.contains("O''Brien"), "single quotes must be doubled");
    assert!(script.contains("$s.Arguments = 'launch'"));
    assert!(script.contains("$s.WindowStyle = 7"));
    assert!(script.contains("IconLocation"));
    assert_eq!(CREATE_NO_WINDOW, 0x0800_0000);

    // PowerShell 5.1 treats Unicode single-quote variants as string delimiters too.
    for q in ['\'', '\u{2018}', '\u{2019}', '\u{201A}', '\u{201B}'] {
        let input = format!("a{q}b");
        let expected = format!("'a{q}{q}b'");
        assert_eq!(
            ps_single_quote(&input),
            expected,
            "PowerShell 5.1 quote U+{:04X} must be doubled",
            q as u32
        );
    }

    // Newline characters must be rejected before embedding paths in PowerShell scripts.
    assert!(reject_ps_path_newlines(bin).is_ok());
    assert!(reject_ps_path_newlines(Path::new("C:\\Users\\alice\nbad\\app.exe")).is_err());
    assert!(reject_ps_path_newlines(Path::new("C:\\Users\\alice\rbad\\app.exe")).is_err());
}

#[test]
fn is_companion_serve_process_handles_quoted_windows_cmdline_with_serve_in_dir_name() {
    assert!(!is_companion_serve_process(
        r"C:\Users\My serve dir\agy-companion.exe",
        r#""C:\Users\My serve dir\agy-companion.exe" doctor"#,
        ""
    ));
    assert!(is_companion_serve_process(
        r"C:\Users\My serve dir\agy-companion.exe",
        r#""C:\Users\My serve dir\agy-companion.exe" serve --adopt-running"#,
        ""
    ));
}

#[test]
fn is_companion_serve_process_requires_serve_as_first_arg_and_handles_quoted_nul_and_mixed_case_exe()
 {
    // 1. doctor with `serve` inside --config value (quoted Windows & unquoted Unix) -> false
    assert!(!is_companion_serve_process(
        r"C:\Users\Alice\agy-companion.exe",
        r#""C:\Users\Alice\agy-companion.exe" doctor --config="C:\Users\my serve dir\config.json""#,
        ""
    ));
    assert!(!is_companion_serve_process(
        "/Users/alice/agy-companion",
        "/Users/alice/agy-companion doctor --config=/tmp/my serve dir/config.json",
        ""
    ));

    // 2. doctor with flag value literally named `serve` -> false
    assert!(!is_companion_serve_process(
        "/Users/alice/agy-companion",
        "/Users/alice/agy-companion doctor --config serve",
        ""
    ));

    // 3. Windows case-mismatched quoted CommandLine -> false for doctor, true for serve
    assert!(!is_companion_serve_process(
        r"C:\Users\My serve dir\agy-companion.exe",
        r#""c:\users\my serve dir\agy-companion.exe" doctor"#,
        ""
    ));
    assert!(is_companion_serve_process(
        r"C:\Users\My serve dir\agy-companion.exe",
        r#""c:\users\my serve dir\agy-companion.exe" serve"#,
        ""
    ));

    // 4. Linux NUL-delimited symlinked argv[0] containing ` serve ` -> false for doctor, true for serve
    assert!(!is_companion_serve_process(
        "/private/tmp/my serve dir/agy-companion",
        "/tmp/my serve dir/agy-companion\0doctor\0",
        ""
    ));
    assert!(!is_companion_serve_process(
        "/private/tmp/my serve dir/agy-companion",
        "/tmp/my serve dir/agy-companion\0doctor\0--config\0serve\0",
        ""
    ));
    assert!(is_companion_serve_process(
        "/private/tmp/my serve dir/agy-companion",
        "/tmp/my serve dir/agy-companion\0serve\0",
        ""
    ));

    // 5. Mixed-case .Exe suffix -> true for serve
    assert!(is_companion_serve_process(
        r"C:\Users\Alice\agy-companion.Exe",
        r#""C:\Users\Alice\agy-companion.Exe" serve"#,
        ""
    ));

    // Non-ASCII UTF-8 basename must never panic during suffix check
    assert!(!is_companion_serve_process(
        "/tmp/😀a",
        "/tmp/😀a serve",
        ""
    ));

    // 6. Unquoted mismatched path containing ` serve ` and 1-arg Linux NUL cmdline -> false;
    //    8.3 Windows short path with companion basename -> true.
    assert!(!is_companion_serve_process(
        "/real/bin/agy-companion",
        "/tmp/my serve dir/agy-companion",
        ""
    ));
    assert!(!is_companion_serve_process(
        "/real/bin/agy-companion",
        "/tmp/my serve dir/agy-companion\0",
        ""
    ));
    assert!(!is_companion_serve_process(
        r"C:\real\agy-companion.exe",
        r"C:\Users\my serve dir\agy-companion.exe doctor",
        ""
    ));
    assert!(is_companion_serve_process(
        r"C:\Users\Jane Doe\AppData\Local\agy-companion\bin\agy-companion.exe",
        r"C:\Users\JANEDO~1\AppData\Local\agy-companion\bin\agy-companion.exe serve --port 4141",
        ""
    ));
}

#[test]
fn is_companion_serve_process_accepts_companion_and_rejects_unrelated_processes() {
    assert!(is_companion_serve_process(
        "/Users/alice/.local/share/agy-companion/bin/agy-companion",
        "/Users/alice/.local/share/agy-companion/bin/agy-companion serve --adopt-running --port 4141",
        "/Users/alice/.local/share/agy-companion/bin/agy-companion"
    ));
    assert!(is_companion_serve_process(
        "/Users/alice/My Folder/agy-companion",
        "/Users/alice/My Folder/agy-companion serve --port 4142",
        ""
    ));
    assert!(is_companion_serve_process(
        r"C:\Users\alice\AppData\Local\agy-companion\bin\agy-companion.exe",
        r"C:\Users\alice\AppData\Local\agy-companion\bin\agy-companion.exe serve --port 4141",
        ""
    ));
    assert!(is_companion_serve_process(
        r"C:\Users\alice\AppData\Local\agy-companion\bin\.agy-companion.old.1234.exe",
        r"C:\Users\alice\AppData\Local\agy-companion\bin\.agy-companion.old.1234.exe serve",
        ""
    ));

    // Unrelated processes mentioning agy-companion / serve in arguments MUST be rejected.
    assert!(!is_companion_serve_process(
        "/usr/bin/python3",
        "/usr/bin/python3 -c 'import time; # agy-companion serve; time.sleep(60)'",
        "/Users/alice/.local/share/agy-companion/bin/agy-companion"
    ));
    // Non-serve subcommand of agy-companion MUST be rejected.
    assert!(!is_companion_serve_process(
        "/Users/alice/.local/share/agy-companion/bin/agy-companion",
        "/Users/alice/.local/share/agy-companion/bin/agy-companion doctor",
        ""
    ));
    // PID sanity checks reject PID 0, PID 1, self PID, and overflow PIDs.
    for bad_pid in [0u32, 1u32, std::process::id(), (i32::MAX as u32) + 1] {
        let info = ProxyPidInfo {
            pid: bad_pid,
            port: 4141,
            version: "0.1.0".to_string(),
            started_at_ms: 0,
            binary_path: String::new(),
        };
        assert!(
            matches!(
                verify_proxy_pid_process(&info).expect("verify"),
                PidVerifyResult::UnrelatedProcess { .. }
            ),
            "bad PID {bad_pid} must be rejected as UnrelatedProcess"
        );
    }
}

#[tokio::test]
async fn select_fresh_install_port_falls_back_when_first_port_is_occupied_and_fails_when_exhausted()
{
    // Use two disjoint non-ephemeral loopback port ranges outside macOS (49152..=65535) and
    // Linux (32768..=60999) ephemeral pools, with zero drop-and-rebind windows (which can
    // otherwise race on macOS when a sibling test spawns a subprocess between socket() and
    // FIOCLEX and transiently holds the dropped listener fd).
    let mut exhausted: Option<(u16, TcpListener, TcpListener, TcpListener)> = None;
    for candidate in (24_200u16..26_000).step_by(3) {
        if let (Ok(e1), Ok(e2), Ok(e3)) = (
            TcpListener::bind((Ipv4Addr::LOCALHOST, candidate)),
            TcpListener::bind((Ipv4Addr::LOCALHOST, candidate + 1)),
            TcpListener::bind((Ipv4Addr::LOCALHOST, candidate + 2)),
        ) {
            exhausted = Some((candidate, e1, e2, e3));
            break;
        }
    }
    let (ex_base, e1, e2, e3) =
        exhausted.expect("contiguous non-ephemeral loopback range for exhaustion");

    // All 3 ports held -> exhausted range must fail closed.
    let probe = LoopbackProxyClient::with_default_timeouts().expect("probe client");
    assert!(
        select_fresh_install_port(&probe, ex_base, ex_base + 2, false)
            .await
            .is_err(),
        "exhausted port range must fail closed"
    );
    drop((e1, e2, e3));

    // Separate range for fallback: hold fb_base and fb_base+2 while never binding fb_base+1
    // in the test process, ensuring deterministic fallback to fb_base+1 with zero re-bind.
    let mut fallback: Option<(u16, TcpListener, TcpListener)> = None;
    for candidate in (26_000u16..29_000).step_by(3) {
        if let (Ok(f1), Ok(f3)) = (
            TcpListener::bind((Ipv4Addr::LOCALHOST, candidate)),
            TcpListener::bind((Ipv4Addr::LOCALHOST, candidate + 2)),
        ) && std::net::TcpStream::connect((Ipv4Addr::LOCALHOST, candidate + 1)).is_err()
        {
            fallback = Some((candidate, f1, f3));
            break;
        }
    }
    let (fb_base, f1, f3) = fallback.expect("contiguous non-ephemeral loopback range for fallback");

    let (chosen, auto_selected) = select_fresh_install_port(&probe, fb_base, fb_base + 2, false)
        .await
        .expect("should select fallback port");
    assert_eq!((chosen, auto_selected), (fb_base + 1, true));
    drop((f1, f3));
}

#[test]
fn two_step_plugin_swap_writes_all_files_replaces_existing_and_leaves_zero_temp_dirs() {
    let tmp = TempDir::new().expect("tempdir");
    let plugins_root = tmp.path().join("plugins");
    fs::create_dir_all(&plugins_root).expect("mkdir plugins");
    let plugin_target = plugins_root.join(PLUGIN_DIR_NAME);
    let stable_bin = tmp.path().join("bin").join(STABLE_BIN_NAME);

    // Fresh swap
    let (stage1, guard1) = write_staged_plugin_dir(&plugins_root, &stable_bin).expect("stage1");
    let mut warnings = Vec::new();
    swap_plugin_dir_two_step(
        &plugins_root,
        &plugin_target,
        &stage1,
        guard1,
        &mut warnings,
    )
    .expect("fresh swap");
    assert!(warnings.is_empty(), "unexpected warnings: {warnings:?}");
    assert!(is_companion_plugin_dir(&plugin_target).expect("valid plugin"));

    // Upgrade swap over existing dir with extra old file -> old file gone, zero .stage./.backup leftovers
    let old_marker = plugin_target.join("stale_marker.txt");
    fs::write(&old_marker, "old").expect("write stale");
    let (stage2, guard2) = write_staged_plugin_dir(&plugins_root, &stable_bin).expect("stage2");
    swap_plugin_dir_two_step(
        &plugins_root,
        &plugin_target,
        &stage2,
        guard2,
        &mut warnings,
    )
    .expect("upgrade swap");
    assert!(!old_marker.exists(), "old plugin files must be replaced");
    assert!(is_companion_plugin_dir(&plugin_target).expect("still valid plugin"));

    let entries: Vec<String> = fs::read_dir(&plugins_root)
        .expect("read_dir")
        .map(|e| e.expect("entry").file_name().to_string_lossy().into_owned())
        .collect();
    assert_eq!(
        entries,
        vec![PLUGIN_DIR_NAME.to_string()],
        "no .stage. or .backup leftovers permitted"
    );
}

#[test]
fn is_companion_plugin_dir_rejects_foreign_and_symlink_manifests() {
    let tmp = TempDir::new().expect("tempdir");
    let foreign = tmp.path().join("foreign");
    fs::create_dir_all(&foreign).expect("mkdir");
    fs::write(
        foreign.join("plugin.json"),
        r#"{"name":"other-plugin","version":"1.0.0"}"#,
    )
    .expect("write");
    assert!(!is_companion_plugin_dir(&foreign).expect("foreign"));

    let missing = tmp.path().join("missing");
    fs::create_dir_all(&missing).expect("mkdir");
    assert!(!is_companion_plugin_dir(&missing).expect("missing"));

    #[cfg(unix)]
    {
        let sym_dir = tmp.path().join("sym_manifest");
        fs::create_dir_all(&sym_dir).expect("mkdir");
        let real_manifest = tmp.path().join("real.json");
        fs::write(
            &real_manifest,
            r#"{"name":"agy-companion","version":"0.1.0"}"#,
        )
        .expect("write real");
        std::os::unix::fs::symlink(&real_manifest, sym_dir.join("plugin.json")).expect("symlink");
        assert!(!is_companion_plugin_dir(&sym_dir).expect("symlink manifest rejected"));
    }
}

#[tokio::test]
async fn stop_proxy_from_pid_files_refuses_mismatched_port_and_preserves_pid_file() {
    let tmp = TempDir::new().expect("tempdir");
    let pid_path = tmp.path().join(PROXY_PID_FILENAME);
    let pid_info = ProxyPidInfo {
        pid: 99_999,
        port: 4142,
        version: "0.0.9".to_string(),
        started_at_ms: 1_700_000_000_000,
        binary_path: "/usr/local/bin/agy-companion".to_string(),
    };
    let encoded = serde_json::to_string_pretty(&pid_info).expect("serialize pid");
    fs::write(&pid_path, encoded).expect("write pid file");

    let probe = LoopbackProxyClient::with_default_timeouts().expect("probe client");
    let mut warnings = Vec::new();
    let (stopped, pid) = stop_proxy_from_pid_files(
        &probe,
        std::slice::from_ref(&pid_path),
        Some(4141),
        false,
        &mut warnings,
    )
    .await;
    assert!(!stopped, "mismatched port PID must not be stopped");
    assert_eq!(pid, None);
    assert!(
        pid_path.exists(),
        "mismatched-port proxy.pid must NOT be deleted"
    );
    assert!(
        warnings
            .iter()
            .any(|w| w.contains("proxy.pid port 4142 does not match target listen_port 4141")),
        "expected port mismatch warning, got: {warnings:?}"
    );
}

#[test]
fn remove_pid_file_if_matches_respects_expected_pid_and_preserves_symlink() {
    let tmp = TempDir::new().expect("tempdir");
    let pid_path = tmp.path().join(PROXY_PID_FILENAME);

    let write_pid = |path: &Path, pid: u32| {
        let info = ProxyPidInfo {
            pid,
            port: 4141,
            version: "0.1.0".to_string(),
            started_at_ms: 1_700_000_000_000,
            binary_path: "/usr/local/bin/agy-companion".to_string(),
        };
        fs::write(path, serde_json::to_string_pretty(&info).expect("json")).expect("write pid");
    };

    // 1. Matching PID -> deletes proxy.pid.
    write_pid(&pid_path, 42_001);
    remove_pid_file_if_matches(&pid_path, 42_001);
    assert!(!pid_path.exists(), "matching proxy.pid should be deleted");

    // 2. Different PID (simulating standby takeover) -> preserves proxy.pid.
    write_pid(&pid_path, 42_002);
    remove_pid_file_if_matches(&pid_path, 42_001);
    assert!(
        pid_path.is_file(),
        "standby proxy.pid with newer PID must be preserved"
    );
    assert_eq!(
        read_proxy_pid(&pid_path).expect("read").expect("some").pid,
        42_002
    );

    // 3. Symlinked proxy.pid pointing to a matching target -> neither symlink nor target unlinked.
    #[cfg(unix)]
    {
        fs::remove_file(&pid_path).expect("clean regular pid");
        let real_target = tmp.path().join("real_target.pid");
        write_pid(&real_target, 42_001);
        std::os::unix::fs::symlink(&real_target, &pid_path).expect("symlink pid");

        remove_pid_file_if_matches(&pid_path, 42_001);
        assert!(
            fs::symlink_metadata(&pid_path)
                .expect("symlink metadata")
                .file_type()
                .is_symlink(),
            "symlinked proxy.pid must not be unlinked"
        );
        assert!(
            real_target.is_file(),
            "symlink target must not be followed or removed"
        );
    }
}

#[test]
fn swap_plugin_dir_two_step_rolls_back_on_directory_and_symlink_failure_and_rejects_non_dir() {
    let tmp = TempDir::new().expect("tempdir");

    // (a) Directory-upgrade rollback: missing stage_dir causes second rename to fail;
    //     backup_dir must be restored to plugin_target with zero .backup leftovers.
    {
        let plugins_root = tmp.path().join("dir_rollback");
        fs::create_dir_all(&plugins_root).expect("mkdir");
        let plugin_target = plugins_root.join(PLUGIN_DIR_NAME);
        fs::create_dir_all(&plugin_target).expect("mkdir target");
        let sentinel = plugin_target.join("original.txt");
        fs::write(&sentinel, "KEEP_ORIGINAL\n").expect("write sentinel");

        let missing_stage = plugins_root.join(".agy-companion.stage.missing");
        let mut warnings = Vec::new();
        let res = swap_plugin_dir_two_step(
            &plugins_root,
            &plugin_target,
            &missing_stage,
            TempPathGuard::dir(missing_stage.clone()),
            &mut warnings,
        );
        assert!(res.is_err(), "swap with missing stage_dir must fail");
        assert_eq!(
            fs::read_to_string(&sentinel).expect("restored sentinel"),
            "KEEP_ORIGINAL\n"
        );
        let entries: Vec<String> = fs::read_dir(&plugins_root)
            .expect("read_dir")
            .map(|e| e.expect("entry").file_name().to_string_lossy().into_owned())
            .collect();
        assert_eq!(
            entries,
            vec![PLUGIN_DIR_NAME.to_string()],
            "rollback must leave zero .backup leftovers"
        );
    }

    // (b) Unix symlink-swap rollback: unlink_symlink succeeds, missing stage_dir fails rename;
    //     original symlink pointing to dev_target must be restored intact.
    #[cfg(unix)]
    {
        let plugins_root = tmp.path().join("sym_rollback");
        let dev_target = tmp.path().join("dev_target");
        fs::create_dir_all(&plugins_root).expect("mkdir");
        fs::create_dir_all(&dev_target).expect("mkdir dev");
        fs::write(dev_target.join("dev.txt"), "DEV_OK\n").expect("write dev");
        let plugin_target = plugins_root.join(PLUGIN_DIR_NAME);
        std::os::unix::fs::symlink(&dev_target, &plugin_target).expect("symlink");

        let missing_stage = plugins_root.join(".agy-companion.stage.missing");
        let mut warnings = Vec::new();
        let res = swap_plugin_dir_two_step(
            &plugins_root,
            &plugin_target,
            &missing_stage,
            TempPathGuard::dir(missing_stage.clone()),
            &mut warnings,
        );
        assert!(
            res.is_err(),
            "symlink swap with missing stage_dir must fail"
        );
        assert!(
            fs::symlink_metadata(&plugin_target)
                .expect("restored symlink meta")
                .file_type()
                .is_symlink(),
            "original symlink must be restored on rollback"
        );
        assert_eq!(
            fs::read_link(&plugin_target).expect("read_link"),
            dev_target
        );
        assert_eq!(
            fs::read_to_string(dev_target.join("dev.txt")).expect("dev file"),
            "DEV_OK\n"
        );
    }

    // (c) Non-directory plugin_target rejection: regular file at plugin_target must be
    //     rejected with CliError::Install, untouched, and stage_dir cleaned up.
    {
        let plugins_root = tmp.path().join("non_dir");
        fs::create_dir_all(&plugins_root).expect("mkdir");
        let plugin_target = plugins_root.join(PLUGIN_DIR_NAME);
        fs::write(&plugin_target, "NOT_A_DIR\n").expect("write file");

        let stable_bin = tmp.path().join("bin").join(STABLE_BIN_NAME);
        let (stage_dir, stage_guard) =
            write_staged_plugin_dir(&plugins_root, &stable_bin).expect("stage");
        let mut warnings = Vec::new();
        let err = swap_plugin_dir_two_step(
            &plugins_root,
            &plugin_target,
            &stage_dir,
            stage_guard,
            &mut warnings,
        )
        .expect_err("non-directory plugin_target must be rejected");
        assert!(
            matches!(err, CliError::Install(_)),
            "expected CliError::Install, got: {err:?}"
        );
        assert_eq!(
            fs::read_to_string(&plugin_target).expect("file untouched"),
            "NOT_A_DIR\n"
        );
        assert!(
            !stage_dir.exists(),
            "stage_guard must clean up stage_dir on error"
        );
    }
}

#[test]
fn dir_safety_helpers_reject_symlinks_non_dirs_and_match_normalized_usage_dir_ancestry() {
    let tmp = TempDir::new().expect("tempdir");
    let real_dir = tmp.path().join("real_dir");
    fs::create_dir_all(&real_dir).expect("mkdir real_dir");
    let reg_file = tmp.path().join("not_a_dir");
    fs::write(&reg_file, "file\n").expect("write file");
    let missing_dir = tmp.path().join("does_not_exist");

    let mut warnings = Vec::new();
    assert_eq!(
        inspect_existing_dir(&real_dir, "bin directory", &mut warnings).expect("real"),
        ExistingDirSafety::SafeDir
    );
    assert_eq!(
        inspect_existing_dir(&missing_dir, "bin directory", &mut warnings).expect("missing"),
        ExistingDirSafety::NotFound
    );
    assert!(warnings.is_empty());

    assert_eq!(
        inspect_existing_dir(&reg_file, "bin directory", &mut warnings).expect("file"),
        ExistingDirSafety::Unsafe
    );
    assert_eq!(warnings.len(), 1);
    assert!(warnings[0].contains("refusing to traverse symlinked/non-directory bin directory"));

    // usage_dir_under_dir normalizes trailing slashes and `.` segments.
    let base = tmp.path().join("sym_parent");
    assert!(usage_dir_under_dir(&base, &base));
    assert!(usage_dir_under_dir(
        &PathBuf::from(format!("{}/", base.display())),
        &base
    ));
    assert!(usage_dir_under_dir(&base.join("usage"), &base));
    assert!(usage_dir_under_dir(
        &PathBuf::from(format!("{}/usage/.", base.display())),
        &base
    ));
    assert!(!usage_dir_under_dir(
        &tmp.path().join("other").join("usage"),
        &base
    ));

    #[cfg(unix)]
    {
        let target_parent = tmp.path().join("target_parent");
        fs::create_dir_all(target_parent.join("child")).expect("mkdir target");
        let chain_root = tmp.path().join("chain_root");
        fs::create_dir_all(chain_root.join("a")).expect("mkdir a");
        std::os::unix::fs::symlink(&target_parent, chain_root.join("a").join("b"))
            .expect("symlink b");

        let mut chain_warnings = Vec::new();
        assert_eq!(
            inspect_non_symlink_rel_chain(
                &chain_root,
                &["a", "b", "child"],
                "sidecar data directory",
                &mut chain_warnings
            )
            .expect("chain check"),
            ExistingDirSafety::Unsafe
        );
        assert_eq!(chain_warnings.len(), 1);
        assert!(
            chain_warnings[0]
                .contains("refusing to traverse symlinked/non-directory sidecar data directory")
        );
    }
}

#[test]
fn files_have_identical_bytes_distinguishes_matching_differing_missing_symlink_and_fifo() {
    let tmp = TempDir::new().expect("tempdir");
    let file_a = tmp.path().join("bin_a");
    let file_b = tmp.path().join("bin_b");
    let file_c = tmp.path().join("bin_c");
    let missing = tmp.path().join("missing");

    // Multi-chunk (> 64 KiB) identical content
    let mut payload = vec![0x5Au8; 160 * 1024];
    payload[0] = 0x01;
    payload[160 * 1024 - 1] = 0xFE;
    fs::write(&file_a, &payload).expect("write a");
    fs::write(&file_b, &payload).expect("write b");
    assert!(files_have_identical_bytes(&file_a, &file_b));
    assert!(files_have_identical_bytes(&file_a, &file_a));

    // Same length, last byte differs (forces reading final chunk)
    let mut modified = payload.clone();
    modified[160 * 1024 - 1] = 0xFF;
    fs::write(&file_c, &modified).expect("write c");
    assert!(!files_have_identical_bytes(&file_a, &file_c));

    // Different length
    fs::write(&file_c, b"short").expect("write short");
    assert!(!files_have_identical_bytes(&file_a, &file_c));

    // Missing file
    assert!(!files_have_identical_bytes(&file_a, &missing));
    assert!(!files_have_identical_bytes(&missing, &file_a));

    #[cfg(unix)]
    {
        use std::os::unix::fs::symlink;
        use std::process::Command;

        // Symlinked destination pointing directly to `file_a` must return false (never followed).
        let link_to_a = tmp.path().join("link_to_a");
        symlink(&file_a, &link_to_a).expect("symlink to a");
        assert!(!files_have_identical_bytes(&file_a, &link_to_a));

        // Named pipe (FIFO) at either operand must return false immediately without blocking.
        let fifo_path = tmp.path().join("bin_fifo");
        assert!(
            Command::new("mkfifo")
                .arg(&fifo_path)
                .status()
                .expect("mkfifo")
                .success()
        );
        assert!(!files_have_identical_bytes(&file_a, &fifo_path));
        assert!(!files_have_identical_bytes(&fifo_path, &file_a));
    }
}

#[test]
fn classify_proxy_replacement_and_owner_verification_cover_same_version_paths() {
    let tmp = TempDir::new().expect("tempdir");
    let stable_bin = tmp.path().join("stable_bin");
    let other_bin = tmp.path().join("other_bin");
    fs::write(&stable_bin, b"bin").expect("write stable_bin");
    fs::write(&other_bin, b"other").expect("write other_bin");

    let pid_path = tmp.path().join(PROXY_PID_FILENAME);
    let pid_candidates = vec![pid_path.clone()];
    let port = 45_123;

    // 1. Version mismatch always triggers replacement.
    assert_eq!(
        classify_proxy_replacement("0.1.0", "0.1.1", false, &stable_bin, port, &pid_candidates),
        Some(ProxyReplaceReason::VersionMismatch)
    );

    // 2. Binary bytes changed triggers replacement at same version.
    assert_eq!(
        classify_proxy_replacement("0.1.1", "0.1.1", true, &stable_bin, port, &pid_candidates),
        Some(ProxyReplaceReason::BinaryUpdated)
    );

    // 3. No proxy.pid at same version without binary_changed -> keep running.
    assert_eq!(
        classify_proxy_replacement("0.1.1", "0.1.1", false, &stable_bin, port, &pid_candidates),
        None
    );

    let write_pid_json = |info: &ProxyPidInfo| {
        fs::write(&pid_path, serde_json::to_vec(info).expect("serialize pid"))
            .expect("write pid file");
    };

    // 4. Matching binary_path + fresh started_at_ms -> keep running.
    let fresh_pid = ProxyPidInfo {
        pid: std::process::id(),
        port,
        version: "0.1.1".to_string(),
        started_at_ms: u64::MAX,
        binary_path: stable_bin.display().to_string(),
    };
    write_pid_json(&fresh_pid);
    assert_eq!(
        classify_proxy_replacement("0.1.1", "0.1.1", false, &stable_bin, port, &pid_candidates),
        None
    );

    // 5. Legacy/relocated binary_path in proxy.pid triggers replacement.
    let relocated_pid = ProxyPidInfo {
        binary_path: other_bin.display().to_string(),
        ..fresh_pid.clone()
    };
    write_pid_json(&relocated_pid);
    assert_eq!(
        classify_proxy_replacement("0.1.1", "0.1.1", false, &stable_bin, port, &pid_candidates),
        Some(ProxyReplaceReason::BinaryPathChanged)
    );

    // 6. Proxy started strictly before stable_bin mtime triggers replacement (e.g. after --no-start-proxy).
    let older_pid = ProxyPidInfo {
        started_at_ms: 1,
        ..fresh_pid.clone()
    };
    write_pid_json(&older_pid);
    assert_eq!(
        classify_proxy_replacement("0.1.1", "0.1.1", false, &stable_bin, port, &pid_candidates),
        Some(ProxyReplaceReason::BinaryNewerThanProxy)
    );

    // 7. Missing/zero started_at_ms (legacy PID format) does NOT falsely trigger mtime restart.
    let zero_ts_pid = ProxyPidInfo {
        started_at_ms: 0,
        ..fresh_pid
    };
    write_pid_json(&zero_ts_pid);
    assert_eq!(
        classify_proxy_replacement("0.1.1", "0.1.1", false, &stable_bin, port, &pid_candidates),
        None
    );

    // 8. Same-version replacement owner verification rejects port-hijacking PID or exited child.
    #[cfg(unix)]
    {
        use std::process::{Child, Command};

        struct KillOnDrop(Child);
        impl Drop for KillOnDrop {
            fn drop(&mut self) {
                let _ = self.0.kill();
                let _ = self.0.wait();
            }
        }

        let mut child = KillOnDrop(
            Command::new("/bin/sleep")
                .arg("30")
                .spawn()
                .expect("spawn sleep"),
        );
        let child_pid = child.0.id();

        let hijacked = ProxyPidInfo {
            pid: child_pid.saturating_add(1),
            port,
            version: "0.1.1".to_string(),
            started_at_ms: u64::MAX,
            binary_path: stable_bin.display().to_string(),
        };
        write_pid_json(&hijacked);
        assert!(
            !verify_same_version_replacement_owner(&mut child.0, &pid_path, port),
            "different PID in proxy.pid must fail ownership check"
        );

        let owned = ProxyPidInfo {
            pid: child_pid,
            ..hijacked
        };
        write_pid_json(&owned);
        assert!(
            verify_same_version_replacement_owner(&mut child.0, &pid_path, port),
            "live child matching proxy.pid must pass ownership check"
        );

        let _ = child.0.kill();
        let _ = child.0.wait();
        assert!(
            !verify_same_version_replacement_owner(&mut child.0, &pid_path, port),
            "exited child must fail ownership check"
        );
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn ensure_installed_proxy_running_skips_up_to_date_and_warns_accurately_on_same_version_restart()
 {
    use axum::Router;
    use axum::routing::get;
    use companion_proxy::HealthResponse;

    let version = env!("CARGO_PKG_VERSION");
    let listener = tokio::net::TcpListener::bind((Ipv4Addr::LOCALHOST, 0))
        .await
        .expect("bind mock health server");
    let port = listener.local_addr().expect("local addr").port();
    let app = Router::new().route(
        "/health",
        get(move || async move { axum::Json(HealthResponse::ok(version).with_role("bootstrap")) }),
    );
    let server = tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });
    tokio::task::yield_now().await;

    let tmp = TempDir::new().expect("tempdir");
    let stable_bin = tmp.path().join(STABLE_BIN_NAME);
    let legacy_bin = tmp.path().join("legacy_bin");
    let cfg_path = tmp.path().join("config.json");
    let data_dir = tmp.path().join("data");
    fs::create_dir_all(&data_dir).expect("mkdir data");
    fs::write(&stable_bin, b"bin").expect("write stable_bin");
    fs::write(&legacy_bin, b"legacy").expect("write legacy_bin");
    let pid_path = data_dir.join(PROXY_PID_FILENAME);

    let probe = LoopbackProxyClient::new(
        std::time::Duration::from_secs(2),
        std::time::Duration::from_secs(2),
    )
    .expect("probe client");

    let write_pid_json = |info: &ProxyPidInfo| {
        fs::write(&pid_path, serde_json::to_vec(info).expect("serialize pid"))
            .expect("write pid file");
    };

    // (a) Up-to-date binary_path + fresh started_at_ms -> skips restart cleanly.
    write_pid_json(&ProxyPidInfo {
        pid: u32::MAX,
        port,
        version: version.to_string(),
        started_at_ms: u64::MAX,
        binary_path: stable_bin.display().to_string(),
    });
    let mut warnings = Vec::new();
    let outcome = ensure_installed_proxy_running(
        &InstallProxyContext {
            probe: &probe,
            stable_bin: &stable_bin,
            config_path: &cfg_path,
            data_dir: &data_dir,
            runtime_data_dir: &data_dir,
            listen_port: port,
            version,
            binary_changed: false,
        },
        &mut warnings,
    )
    .await
    .expect("ensure running up-to-date");
    assert!(!outcome.started && !outcome.replaced);
    assert_eq!(outcome.version.as_deref(), Some(version));
    assert!(warnings.is_empty(), "unexpected warnings: {warnings:?}");

    // (b) binary_changed == true with unverified PID -> enters stop path and emits accurate same-version warning.
    let mut warnings_updated = Vec::new();
    let outcome_updated = ensure_installed_proxy_running(
        &InstallProxyContext {
            probe: &probe,
            stable_bin: &stable_bin,
            config_path: &cfg_path,
            data_dir: &data_dir,
            runtime_data_dir: &data_dir,
            listen_port: port,
            version,
            binary_changed: true,
        },
        &mut warnings_updated,
    )
    .await
    .expect("ensure running updated");
    assert!(!outcome_updated.started && !outcome_updated.replaced);
    assert!(
        warnings_updated
            .iter()
            .any(|w| w.contains("updated binary at version") && !w.contains("has version")),
        "expected accurate updated-binary warning, got {warnings_updated:?}"
    );

    // (c) Relocated binary_path in proxy.pid (binary_changed == false) -> enters stop path with relocated warning.
    write_pid_json(&ProxyPidInfo {
        pid: u32::MAX,
        port,
        version: version.to_string(),
        started_at_ms: u64::MAX,
        binary_path: legacy_bin.display().to_string(),
    });
    let mut warnings_relocated = Vec::new();
    let _ = ensure_installed_proxy_running(
        &InstallProxyContext {
            probe: &probe,
            stable_bin: &stable_bin,
            config_path: &cfg_path,
            data_dir: &data_dir,
            runtime_data_dir: &data_dir,
            listen_port: port,
            version,
            binary_changed: false,
        },
        &mut warnings_relocated,
    )
    .await
    .expect("ensure running relocated");
    assert!(
        warnings_relocated
            .iter()
            .any(|w| w.contains("relocated binary path at version") && !w.contains("has version")),
        "expected accurate relocated-binary warning, got {warnings_relocated:?}"
    );

    server.abort();
}

#[test]
fn resolve_install_channel_follows_five_level_precedence_including_windows_uvx_paths() {
    use companion_core::{InstallChannel, LastUpgradeRecord, UpdateState, UpgradeOutcome};

    use super::{next_update_state, resolve_install_channel};

    let persisted_uvx = UpdateState::new(InstallChannel::Uvx);

    // 1. Explicit CLI flag wins over everything.
    assert_eq!(
        resolve_install_channel(
            Some(InstallChannel::Npx),
            Some("uvx"),
            Some(Path::new(
                "/home/u/.cache/uv/archive-v0/abc/bin/agy-companion"
            )),
            Some("/home/u/.cache/uv/cache/archive-v0/abc"),
            Some(&persisted_uvx),
        ),
        InstallChannel::Npx
    );

    // 2. Valid env var (case-insensitive) wins over uvx path detection and persisted state.
    assert_eq!(
        resolve_install_channel(
            None,
            Some("NPX"),
            Some(Path::new(
                r"C:\Users\u\AppData\Local\uv\cache\archive-v0\abc\agy-companion.exe"
            )),
            None,
            Some(&persisted_uvx),
        ),
        InstallChannel::Npx
    );
    assert_eq!(
        resolve_install_channel(None, Some("UvX"), None, None, None),
        InstallChannel::Uvx
    );

    // Invalid/empty env var is ignored and falls through to uvx path detection.
    assert_eq!(
        resolve_install_channel(
            None,
            Some("invalid"),
            Some(Path::new(
                "/Users/u/.cache/uv/archive-v0/xyz/bin/agy-companion"
            )),
            None,
            None,
        ),
        InstallChannel::Uvx
    );
    assert_eq!(
        resolve_install_channel(
            None,
            Some("  "),
            None,
            Some(r"C:\Users\u\AppData\Local\uv\cache\archive-v0\xyz"),
            None,
        ),
        InstallChannel::Uvx
    );

    // 4. Persisted state wins over fallback.
    assert_eq!(
        resolve_install_channel(
            None,
            None,
            Some(Path::new("/usr/local/bin/agy-companion")),
            None,
            Some(&persisted_uvx),
        ),
        InstallChannel::Uvx
    );

    // 5. Fallback is Npx.
    assert_eq!(
        resolve_install_channel(
            None,
            None,
            Some(Path::new("/usr/local/bin/agy-companion")),
            None,
            None,
        ),
        InstallChannel::Npx
    );

    // next_update_state preserves cache on same channel, clears cache (keeping last_upgrade) on channel switch.
    let seeded = UpdateState {
        schema_version: 1,
        install_channel: InstallChannel::Uvx,
        checked_at_ms: Some(12345),
        latest_version: Some("0.2.0".to_string()),
        last_upgrade: Some(LastUpgradeRecord {
            target_version: "0.1.2".to_string(),
            started_at_ms: 100,
            finished_at_ms: Some(200),
            outcome: UpgradeOutcome::Succeeded,
            error: None,
        }),
    };
    assert_eq!(
        next_update_state(Some(seeded.clone()), InstallChannel::Uvx),
        seeded
    );
    let switched = next_update_state(Some(seeded.clone()), InstallChannel::Npx);
    assert_eq!(switched.install_channel, InstallChannel::Npx);
    assert_eq!(switched.checked_at_ms, None);
    assert_eq!(switched.latest_version, None);
    assert_eq!(switched.last_upgrade, seeded.last_upgrade);
}

#[cfg(unix)]
#[test]
fn run_companion_sh_preserves_parent_hybrid_active_across_env_scrub() {
    use std::os::unix::fs::PermissionsExt;
    use std::process::Command;

    let tmp = TempDir::new().expect("tempdir");
    let fake_bin = tmp.path().join("fake-agy-companion");
    fs::write(
        &fake_bin,
        "#!/usr/bin/env bash\n\
         printf 'CLOUD_CODE_URL=%s\\n' \"${CLOUD_CODE_URL-<UNSET>}\"\n\
         printf 'AGY_COMPANION_HOOK_ACTIVE=%s\\n' \"${AGY_COMPANION_HOOK_ACTIVE-<UNSET>}\"\n\
         printf 'AGY_COMPANION_PARENT_HYBRID_ACTIVE=%s\\n' \"${AGY_COMPANION_PARENT_HYBRID_ACTIVE-<UNSET>}\"\n",
    )
    .expect("write fake bin");
    fs::set_permissions(&fake_bin, fs::Permissions::from_mode(0o755)).expect("chmod fake bin");

    let launcher_script =
        Path::new(env!("CARGO_MANIFEST_DIR")).join("../../plugin/scripts/run-companion.sh");
    assert!(
        launcher_script.is_file(),
        "launcher script missing at {}",
        launcher_script.display()
    );

    let run_case = |envs: &[(&str, &str)]| -> String {
        let mut cmd = Command::new("bash");
        cmd.arg(&launcher_script)
            .env_remove("CLOUD_CODE_URL")
            .env_remove("GOOGLE_GEMINI_BASE_URL")
            .env_remove("AGY_COMPANION_HOOK_ACTIVE")
            .env_remove("AGY_COMPANION_PARENT_HYBRID_ACTIVE")
            .env_remove("AGY_COMPANION_INJECTED_URL")
            .env_remove("JETSKI_MCP_ENV_RESTORE_PREFIX")
            .env_remove("JETSKI_MCP_ENV_RESTORE_VARS")
            .env("AGY_COMPANION_BIN", &fake_bin);
        for (k, v) in envs {
            cmd.env(k, v);
        }
        let out = cmd.output().expect("run launcher");
        assert!(
            out.status.success(),
            "launcher failed: stderr={}",
            String::from_utf8_lossy(&out.stderr)
        );
        String::from_utf8(out.stdout).expect("utf8 stdout")
    };

    // 1. Loopback CLOUD_CODE_URL -> scrubbed, AGY_COMPANION_PARENT_HYBRID_ACTIVE=1
    for loopback in [
        "http://127.0.0.1:4141",
        "  http://localhost:4242/  ",
        "HTTP://LOCALHOST:65535/v1",
    ] {
        let out = run_case(&[("CLOUD_CODE_URL", loopback)]);
        assert!(
            out.contains("CLOUD_CODE_URL=<UNSET>\n"),
            "CLOUD_CODE_URL must be scrubbed for {loopback}: {out}"
        );
        assert!(
            out.contains("AGY_COMPANION_PARENT_HYBRID_ACTIVE=1\n"),
            "expected PARENT_HYBRID_ACTIVE=1 for {loopback}: {out}"
        );
    }

    // 2. HOOK_ACTIVE=1 without CLOUD_CODE_URL -> scrubbed, PARENT_HYBRID_ACTIVE=1
    let out_hook = run_case(&[("AGY_COMPANION_HOOK_ACTIVE", "1")]);
    assert!(out_hook.contains("AGY_COMPANION_HOOK_ACTIVE=<UNSET>\n"));
    assert!(out_hook.contains("AGY_COMPANION_PARENT_HYBRID_ACTIVE=1\n"));

    // 3. Existing PARENT_HYBRID_ACTIVE=1 without CLOUD_CODE_URL -> preserved
    let out_parent = run_case(&[("AGY_COMPANION_PARENT_HYBRID_ACTIVE", "1")]);
    assert!(out_parent.contains("AGY_COMPANION_PARENT_HYBRID_ACTIVE=1\n"));

    // 4. Day-0 unset -> PARENT_HYBRID_ACTIVE remains unset
    let out_day0 = run_case(&[]);
    assert!(out_day0.contains("AGY_COMPANION_PARENT_HYBRID_ACTIVE=<UNSET>\n"));

    // 5. Non-loopback or invalid CLOUD_CODE_URL overrides HOOK_ACTIVE=1 and stale PARENT_HYBRID_ACTIVE=1
    for non_loopback in [
        "https://cloudcode-pa.googleapis.com",
        "https://127.0.0.1:4141",
        "http://127.0.0.1:0",
        "http://127.0.0.1:65536",
        "http://user:pass@127.0.0.1:4141",
        "http://127.0.0.1:4141@evil.example",
    ] {
        let out = run_case(&[
            ("CLOUD_CODE_URL", non_loopback),
            ("AGY_COMPANION_HOOK_ACTIVE", "1"),
            ("AGY_COMPANION_PARENT_HYBRID_ACTIVE", "1"),
        ]);
        assert!(
            out.contains("CLOUD_CODE_URL=<UNSET>\n"),
            "CLOUD_CODE_URL must be scrubbed for {non_loopback}: {out}"
        );
        assert!(
            out.contains("AGY_COMPANION_PARENT_HYBRID_ACTIVE=<UNSET>\n"),
            "non-loopback {non_loopback} must unset stale PARENT_HYBRID_ACTIVE: {out}"
        );
    }
}
