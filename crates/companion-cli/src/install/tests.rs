use std::collections::BTreeMap;
use std::fs;
use std::net::{Ipv4Addr, TcpListener};
use std::path::{Path, PathBuf};

use companion_core::fs_safe::TempPathGuard;
use companion_providers::LoopbackProxyClient;
use tempfile::TempDir;

use super::bundle::{
    EMBEDDED_PLUGIN_FILES, PLUGIN_DIR_NAME, STABLE_BIN_NAME, is_companion_plugin_dir,
    select_fresh_install_port, swap_plugin_dir_two_step, write_staged_plugin_dir,
};
#[cfg(unix)]
use super::safety::inspect_non_symlink_rel_chain;
use super::safety::{ExistingDirSafety, inspect_existing_dir, usage_dir_under_dir};
use super::windows::{
    WINDOWS_SHORTCUT_NAME, patch_windows_mcp_config_from_str,
    patch_windows_sidecar_config_from_str, ps_single_quote, reject_ps_path_newlines,
    render_windows_mcp_config, render_windows_shortcut_ps_script, render_windows_sidecar_config,
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
        7,
        "expected exactly 7 embedded plugin files"
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
        "mcp_config.json",
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
    for tool_name in [
        "open_settings",
        "get_settings",
        "save_settings",
        "test_connection",
        "set_launcher_integration",
        "get_usage",
    ] {
        assert!(
            skill.contains(tool_name),
            "embedded SKILL.md must document MCP tool `{tool_name}`"
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

    let mcp_raw = render_windows_mcp_config(bin_path).expect("render windows mcp config");
    assert!(
        !mcp_raw.contains("bash") && !mcp_raw.contains("run-companion.sh"),
        "Windows mcp_config.json must not reference bash: {mcp_raw}"
    );
    let mcp_json: serde_json::Value = serde_json::from_str(&mcp_raw).expect("parse mcp json");
    assert_eq!(
        mcp_json["mcpServers"]["hub"]["command"],
        bin_path.to_string_lossy().as_ref()
    );
    assert_eq!(
        mcp_json["mcpServers"]["hub"]["args"],
        serde_json::json!([
            "ui-mcp",
            "--data-dir=${PLUGIN_DATA}",
            "--auto-enable-launcher"
        ])
    );

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
        5,
        "Windows sidecar.json must contain only the 5 strict-valid schema keys: {sidecar_raw}"
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
    assert_eq!(sidecar_json["display_name"], "Antigravity Companion Proxy");
    assert_eq!(
        sidecar_json["description"],
        "Local loopback Gemini pass-through proxy with SSE usageMetadata telemetry and config hot-reload."
    );
}

#[test]
fn windows_manifest_generators_derive_from_embedded_json_and_preserve_future_fields() {
    let bin_path = Path::new(r"C:\Users\alice\AppData\Local\agy-companion\bin\agy-companion.exe");

    // 1. Live embedded manifests: every non-command field and post-wrapper arg must match verbatim.
    let embedded_mcp: serde_json::Value = serde_json::from_str(
        EMBEDDED_PLUGIN_FILES
            .iter()
            .find(|f| f.rel_path == "mcp_config.json")
            .expect("embedded mcp_config.json")
            .content,
    )
    .expect("parse embedded mcp_config.json");
    let rendered_mcp: serde_json::Value =
        serde_json::from_str(&render_windows_mcp_config(bin_path).expect("render mcp"))
            .expect("parse rendered mcp");
    let expected_mcp_args: Vec<serde_json::Value> = embedded_mcp["mcpServers"]["hub"]["args"]
        .as_array()
        .expect("embedded mcp args")
        .iter()
        .skip(1)
        .cloned()
        .collect();
    assert_eq!(
        rendered_mcp["mcpServers"]["hub"]["args"],
        serde_json::Value::Array(expected_mcp_args)
    );
    for (key, val) in embedded_mcp["mcpServers"]["hub"]
        .as_object()
        .expect("embedded hub object")
    {
        if key != "command" && key != "args" {
            assert_eq!(
                rendered_mcp["mcpServers"]["hub"].get(key),
                Some(val),
                "Windows mcp_config.json drifted on field '{key}'"
            );
        }
    }

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
    let synthetic_mcp = r#"{
      "mcpServers": {
        "hub": {
          "command": "bash",
          "args": ["${PLUGIN_ROOT}/scripts/run-companion.sh", "ui-mcp", "--data-dir=${PLUGIN_DATA}", "--extra-flag"],
          "env": { "FOO": "bar" },
          "timeout_secs": 30
        }
      },
      "futureTopLevel": true
    }"#;
    let patched_mcp: serde_json::Value = serde_json::from_str(
        &patch_windows_mcp_config_from_str(synthetic_mcp, bin_path).expect("patch synthetic mcp"),
    )
    .expect("parse patched synthetic mcp");
    assert_eq!(
        patched_mcp["mcpServers"]["hub"]["command"],
        bin_path.to_string_lossy().as_ref()
    );
    assert_eq!(
        patched_mcp["mcpServers"]["hub"]["args"],
        serde_json::json!(["ui-mcp", "--data-dir=${PLUGIN_DATA}", "--extra-flag"])
    );
    assert_eq!(
        patched_mcp["mcpServers"]["hub"]["env"],
        serde_json::json!({ "FOO": "bar" })
    );
    assert_eq!(patched_mcp["mcpServers"]["hub"]["timeout_secs"], 30);
    assert_eq!(patched_mcp["futureTopLevel"], true);

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
    let missing_hub = r#"{"mcpServers": {}}"#;
    assert!(
        patch_windows_mcp_config_from_str(missing_hub, bin_path).is_err(),
        "mcp manifest without mcpServers.hub must fail closed"
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
        r#""C:\Users\My serve dir\agy-companion.exe" ui-mcp"#,
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
    // 1. ui-mcp with `serve` inside --data-dir value (quoted Windows & unquoted Unix) -> false
    assert!(!is_companion_serve_process(
        r"C:\Users\Alice\agy-companion.exe",
        r#""C:\Users\Alice\agy-companion.exe" ui-mcp --data-dir="C:\Users\my serve dir\data""#,
        ""
    ));
    assert!(!is_companion_serve_process(
        "/Users/alice/agy-companion",
        "/Users/alice/agy-companion ui-mcp --data-dir=/tmp/my serve dir/data",
        ""
    ));

    // 2. ui-mcp with flag value literally named `serve` -> false
    assert!(!is_companion_serve_process(
        "/Users/alice/agy-companion",
        "/Users/alice/agy-companion ui-mcp --config serve",
        ""
    ));

    // 3. Windows case-mismatched quoted CommandLine -> false for ui-mcp, true for serve
    assert!(!is_companion_serve_process(
        r"C:\Users\My serve dir\agy-companion.exe",
        r#""c:\users\my serve dir\agy-companion.exe" ui-mcp"#,
        ""
    ));
    assert!(is_companion_serve_process(
        r"C:\Users\My serve dir\agy-companion.exe",
        r#""c:\users\my serve dir\agy-companion.exe" serve"#,
        ""
    ));

    // 4. Linux NUL-delimited symlinked argv[0] containing ` serve ` -> false for ui-mcp, true for serve
    assert!(!is_companion_serve_process(
        "/private/tmp/my serve dir/agy-companion",
        "/tmp/my serve dir/agy-companion\0ui-mcp\0",
        ""
    ));
    assert!(!is_companion_serve_process(
        "/private/tmp/my serve dir/agy-companion",
        "/tmp/my serve dir/agy-companion\0ui-mcp\0--config\0serve\0",
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
        r"C:\Users\my serve dir\agy-companion.exe ui-mcp",
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
        "/Users/alice/.local/share/agy-companion/bin/agy-companion ui-mcp",
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
