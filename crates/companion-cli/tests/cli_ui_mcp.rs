#![forbid(unsafe_code)]

mod common;

use std::fs;
use std::io::{BufRead, BufReader, Write};
use std::process::{Command, Stdio};
use std::time::Duration;

use companion_core::{ApiKey, Config};
use serde_json::{Value, json};
use tempfile::TempDir;

use common::{
    BIN, config_with_port, free_loopback_port, isolated_cli_command, run_ui_mcp_bounded,
    run_ui_mcp_bounded_with_calls,
};

#[test]
fn ui_mcp_stdio_initialize_emits_strict_json_rpc_only_on_stdout() {
    let temp = TempDir::new().expect("tempdir");
    let cfg_path = temp.path().join("config.json");
    let data_dir = temp.path().join("plugin-data");
    fs::create_dir(&data_dir).expect("mkdir data_dir");

    let mut cfg = Config::default();
    cfg.providers.gemini.api_key = Some(ApiKey::parse_strict("AIzaSyUiMcpCfgSecret5555").unwrap());
    cfg.save_secure(&cfg_path).expect("save_secure");

    let mut child = Command::new(BIN)
        .arg("ui-mcp")
        .arg(format!("--data-dir={}", data_dir.display()))
        .arg("--config")
        .arg(&cfg_path)
        .env("RUST_LOG", "debug")
        .env("GEMINI_API_KEY", "AIzaSyUiMcpEnvSecret6666")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn ui-mcp");

    let mut stdin = child.stdin.take().expect("stdin");
    let stdout = child.stdout.take().expect("stdout");
    let stderr = child.stderr.take().expect("stderr");

    let init_req = json!({
        "jsonrpc": "2.0",
        "id": 1,
        "method": "initialize",
        "params": {
            "protocolVersion": "2025-03-26",
            "capabilities": {},
            "clientInfo": {
                "name": "companion-cli-test",
                "version": "0.1.0"
            }
        }
    });
    let initialized_notif = json!({
        "jsonrpc": "2.0",
        "method": "notifications/initialized"
    });
    let list_tools_req = json!({
        "jsonrpc": "2.0",
        "id": 2,
        "method": "tools/list",
        "params": {}
    });

    writeln!(stdin, "{init_req}").expect("write init");
    stdin.flush().expect("flush init");

    let mut reader = BufReader::new(stdout);
    let mut first_line = String::new();
    reader
        .read_line(&mut first_line)
        .expect("read initialize response");
    let init_resp: Value =
        serde_json::from_str(&first_line).expect("first stdout line must be strict JSON-RPC 2.0");
    assert_eq!(init_resp["jsonrpc"], "2.0");
    assert_eq!(init_resp["id"], 1);
    assert_eq!(init_resp["result"]["serverInfo"]["name"], "agy-companion");
    assert_eq!(
        init_resp["result"]["serverInfo"]["title"],
        "Antigravity Companion"
    );
    assert!(
        init_resp["result"]["serverInfo"]["icons"][0]["src"]
            .as_str()
            .is_some_and(|s| s.starts_with("data:image/svg+xml;base64,"))
    );
    assert_eq!(
        init_resp["result"]["serverInfo"]["version"],
        env!("CARGO_PKG_VERSION")
    );

    writeln!(stdin, "{initialized_notif}").expect("write initialized");
    writeln!(stdin, "{list_tools_req}").expect("write tools/list");
    stdin.flush().expect("flush tools/list");
    drop(stdin);

    let mut remaining_lines = Vec::new();
    for line in reader.lines() {
        let line = line.expect("read stdout line");
        if !line.is_empty() {
            remaining_lines.push(line);
        }
    }

    let mut stderr_str = String::new();
    std::io::Read::read_to_string(&mut BufReader::new(stderr), &mut stderr_str)
        .expect("read stderr");
    let status = child.wait().expect("wait ui-mcp");
    assert_eq!(
        status.code(),
        Some(0),
        "ui-mcp should exit 0 on stdin EOF; stderr={stderr_str}"
    );

    assert!(
        !remaining_lines.is_empty(),
        "expected tools/list response on stdout"
    );
    let mut saw_tools_list = false;
    for line in &remaining_lines {
        let parsed: Value =
            serde_json::from_str(line).expect("every stdout line must be strict JSON-RPC 2.0");
        assert_eq!(parsed["jsonrpc"], "2.0");
        if parsed["id"] == 2 {
            let tools = parsed["result"]["tools"].as_array().expect("tools array");
            assert_eq!(tools.len(), 6);
            assert!(
                tools
                    .iter()
                    .any(|t| t.get("name") == Some(&json!("set_launcher_integration")))
            );
            assert!(
                tools
                    .iter()
                    .any(|t| t.get("name") == Some(&json!("get_usage")))
            );
            saw_tools_list = true;
        }
    }
    assert!(saw_tools_list, "missing tools/list response");

    assert!(
        !stderr_str.contains("\x1b["),
        "ui-mcp stderr must not contain ANSI escapes"
    );
    let combined = format!("{first_line}\n{}\n{stderr_str}", remaining_lines.join("\n"));
    assert!(!combined.contains("AIzaSyUiMcpCfgSecret5555"));
    assert!(!combined.contains("AIzaSyUiMcpEnvSecret6666"));
}

#[test]
fn ui_mcp_save_settings_never_leaks_api_key_under_trace_or_rmcp_trace_logging() {
    let canary = "AIzaSyRedactionCanary9999";

    for rust_log in ["debug", "trace", "rmcp=trace", "rmcp::service=trace"] {
        let temp = TempDir::new().expect("tempdir");
        let cfg_path = temp.path().join("config.json");
        let data_dir = temp.path().join("plugin-data");
        fs::create_dir(&data_dir).expect("mkdir data_dir");

        let mut child = Command::new(BIN)
            .arg("ui-mcp")
            .arg(format!("--data-dir={}", data_dir.display()))
            .arg("--config")
            .arg(&cfg_path)
            .env("RUST_LOG", rust_log)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("spawn ui-mcp");

        let mut stdin = child.stdin.take().expect("stdin");
        let stdout = child.stdout.take().expect("stdout");
        let stderr = child.stderr.take().expect("stderr");

        // Drain stderr concurrently to prevent pipe buffer deadlocks under trace logging.
        let stderr_thread = std::thread::spawn(move || {
            let mut buf = String::new();
            std::io::Read::read_to_string(&mut BufReader::new(stderr), &mut buf)
                .expect("read stderr");
            buf
        });

        let init_req = json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "initialize",
            "params": {
                "protocolVersion": "2025-03-26",
                "capabilities": {},
                "clientInfo": {
                    "name": "companion-cli-test",
                    "version": "0.1.0"
                }
            }
        });
        let initialized_notif = json!({
            "jsonrpc": "2.0",
            "method": "notifications/initialized"
        });
        let closed_port = free_loopback_port();
        let save_settings_req = json!({
            "jsonrpc": "2.0",
            "id": 2,
            "method": "tools/call",
            "params": {
                "name": "save_settings",
                "arguments": {
                    "proxy": { "port": closed_port },
                    "providers": {
                        "gemini": {
                            "api_key": { "set": canary }
                        }
                    }
                }
            }
        });

        writeln!(stdin, "{init_req}").expect("write init");
        stdin.flush().expect("flush init");

        let mut reader = BufReader::new(stdout);
        let mut first_line = String::new();
        reader
            .read_line(&mut first_line)
            .expect("read initialize response");
        let init_resp: Value = serde_json::from_str(&first_line)
            .expect("initialize response must be strict JSON-RPC 2.0");
        assert_eq!(init_resp["jsonrpc"], "2.0");
        assert_eq!(init_resp["id"], 1);

        writeln!(stdin, "{initialized_notif}").expect("write initialized");
        writeln!(stdin, "{save_settings_req}").expect("write save_settings");
        stdin.flush().expect("flush save_settings");
        drop(stdin);

        let mut remaining_lines = Vec::new();
        for line in reader.lines() {
            let line = line.expect("read stdout line");
            if !line.is_empty() {
                remaining_lines.push(line);
            }
        }

        let status = child.wait().expect("wait ui-mcp");
        let stderr_str = stderr_thread.join().expect("join stderr reader");
        assert_eq!(
            status.code(),
            Some(0),
            "ui-mcp save_settings must exit 0 under RUST_LOG={rust_log}; stderr={stderr_str}"
        );

        let mut saw_save_result = false;
        for line in &remaining_lines {
            let parsed: Value = serde_json::from_str(line)
                .expect("every ui-mcp stdout line must be strict JSON-RPC 2.0");
            assert_eq!(parsed["jsonrpc"], "2.0");
            if parsed["id"] == 2 {
                assert_ne!(
                    parsed["result"]["isError"], true,
                    "save_settings should succeed under RUST_LOG={rust_log}: {parsed}"
                );
                assert_eq!(
                    parsed["result"]["structuredContent"]["config"]["providers"]["gemini"]["api_key_configured"],
                    true
                );
                assert_eq!(
                    parsed["result"]["structuredContent"]["config"]["providers"]["gemini"]["api_key_hint"],
                    "AIza...9999"
                );
                saw_save_result = true;
            }
        }
        assert!(
            saw_save_result,
            "missing save_settings response under RUST_LOG={rust_log}"
        );

        // Verify persisted config on disk has 0600 permissions and saved API key.
        let saved = Config::load_or_default(&cfg_path).expect("load saved config");
        assert_eq!(
            saved.providers.gemini.api_key.as_ref().map(ApiKey::expose),
            Some(canary)
        );
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = fs::metadata(&cfg_path)
                .expect("metadata saved config")
                .permissions()
                .mode()
                & 0o777;
            assert_eq!(mode, 0o600, "saved config must have 0600 permissions");
        }

        assert!(
            !stderr_str.contains("\x1b["),
            "stderr must not contain ANSI escapes under RUST_LOG={rust_log}"
        );

        let stdout_all = format!("{first_line}\n{}", remaining_lines.join("\n"));
        assert!(
            !stdout_all.contains(canary) && !stdout_all.contains("RedactionCanary"),
            "stdout leaked api_key under RUST_LOG={rust_log}: {stdout_all}"
        );
        assert!(
            !stderr_str.contains(canary) && !stderr_str.contains("RedactionCanary"),
            "stderr leaked api_key under RUST_LOG={rust_log}: {stderr_str}"
        );
    }
}

#[test]
fn ui_mcp_eagerly_rejects_fifo_symlink_dir_and_oversized_config() {
    let temp = TempDir::new().expect("tempdir");
    let valid_data_dir = temp.path().join("valid-data-dir");
    fs::create_dir(&valid_data_dir).expect("mkdir valid_data_dir");

    let valid_cfg = temp.path().join("real_config.json");
    Config::default()
        .save_secure(&valid_cfg)
        .expect("save real_config");

    // (a) Directory passed as --config
    let cfg_dir = temp.path().join("cfg_dir");
    fs::create_dir(&cfg_dir).expect("mkdir cfg_dir");
    let (code, stdout, stderr, timed_out) =
        run_ui_mcp_bounded(&cfg_dir, &valid_data_dir, Duration::from_secs(10));
    assert!(!timed_out, "ui-mcp --config <dir> must not hang");
    assert_eq!(code, Some(1), "ui-mcp --config <dir> must exit 1");
    assert!(
        stdout.is_empty(),
        "ui-mcp --config <dir> must emit 0 stdout bytes, got: {stdout}"
    );
    assert!(
        !stderr.is_empty(),
        "ui-mcp --config <dir> must emit error on stderr"
    );

    // (b) Oversized (> 1 MiB) file passed as --config
    let oversized_cfg = temp.path().join("oversized.json");
    let mut oversized_bytes = br#"{"schema_version":1}"#.to_vec();
    oversized_bytes.resize(1024 * 1024 + 1, b' ');
    fs::write(&oversized_cfg, oversized_bytes).expect("write oversized");
    let (code, stdout, stderr, timed_out) =
        run_ui_mcp_bounded(&oversized_cfg, &valid_data_dir, Duration::from_secs(10));
    assert!(!timed_out, "ui-mcp --config <oversized> must not hang");
    assert_eq!(code, Some(1), "ui-mcp --config <oversized> must exit 1");
    assert!(
        stdout.is_empty(),
        "ui-mcp --config <oversized> must emit 0 stdout bytes, got: {stdout}"
    );
    assert!(!stderr.is_empty());

    // (c) Regular file passed as --data-dir
    let data_file = temp.path().join("data_as_file");
    fs::write(&data_file, "not a dir").expect("write data_file");
    let (code, stdout, stderr, timed_out) =
        run_ui_mcp_bounded(&valid_cfg, &data_file, Duration::from_secs(10));
    assert!(!timed_out, "ui-mcp --data-dir <file> must not hang");
    assert_eq!(code, Some(1), "ui-mcp --data-dir <file> must exit 1");
    assert!(
        stdout.is_empty(),
        "ui-mcp --data-dir <file> must emit 0 stdout bytes, got: {stdout}"
    );
    assert!(!stderr.is_empty());

    #[cfg(unix)]
    {
        use std::os::unix::fs::symlink;

        // (d) FIFO passed as --config (must NOT hang!)
        let fifo_cfg = temp.path().join("fifo_config.json");
        let mkfifo_status = Command::new("mkfifo")
            .arg(&fifo_cfg)
            .status()
            .expect("run mkfifo");
        assert!(mkfifo_status.success(), "mkfifo failed");
        let (code, stdout, stderr, timed_out) =
            run_ui_mcp_bounded(&fifo_cfg, &valid_data_dir, Duration::from_secs(10));
        assert!(!timed_out, "ui-mcp --config <FIFO> must not hang");
        assert_eq!(code, Some(1), "ui-mcp --config <FIFO> must exit 1");
        assert!(
            stdout.is_empty(),
            "ui-mcp --config <FIFO> must emit 0 stdout bytes, got: {stdout}"
        );
        assert!(!stderr.is_empty());

        // (e) Symlink to valid file passed as --config
        let symlink_cfg = temp.path().join("symlink_config.json");
        symlink(&valid_cfg, &symlink_cfg).expect("symlink config");
        let (code, stdout, stderr, timed_out) =
            run_ui_mcp_bounded(&symlink_cfg, &valid_data_dir, Duration::from_secs(10));
        assert!(!timed_out, "ui-mcp --config <symlink> must not hang");
        assert_eq!(code, Some(1), "ui-mcp --config <symlink> must exit 1");
        assert!(
            stdout.is_empty(),
            "ui-mcp --config <symlink> must emit 0 stdout bytes, got: {stdout}"
        );
        assert!(!stderr.is_empty());

        // (f) Dangling symlink passed as --config
        let dangling_cfg = temp.path().join("dangling_config.json");
        symlink(temp.path().join("does_not_exist.json"), &dangling_cfg)
            .expect("dangling symlink config");
        let (code, stdout, stderr, timed_out) =
            run_ui_mcp_bounded(&dangling_cfg, &valid_data_dir, Duration::from_secs(10));
        assert!(!timed_out, "ui-mcp --config <dangling> must not hang");
        assert_eq!(code, Some(1), "ui-mcp --config <dangling> must exit 1");
        assert!(
            stdout.is_empty(),
            "ui-mcp --config <dangling> must emit 0 stdout bytes, got: {stdout}"
        );
        assert!(!stderr.is_empty());

        // (g) Symlink to valid directory passed as --data-dir (both without and with trailing slash)
        let symlink_data_dir = temp.path().join("symlink_data_dir");
        symlink(&valid_data_dir, &symlink_data_dir).expect("symlink data_dir");
        let (code, stdout, stderr, timed_out) =
            run_ui_mcp_bounded(&valid_cfg, &symlink_data_dir, Duration::from_secs(10));
        assert!(!timed_out, "ui-mcp --data-dir <symlink> must not hang");
        assert_eq!(code, Some(1), "ui-mcp --data-dir <symlink> must exit 1");
        assert!(
            stdout.is_empty(),
            "ui-mcp --data-dir <symlink> must emit 0 stdout bytes, got: {stdout}"
        );
        assert!(!stderr.is_empty());

        let symlink_data_dir_slash =
            std::path::PathBuf::from(format!("{}/", symlink_data_dir.display()));
        let (code, stdout, stderr, timed_out) =
            run_ui_mcp_bounded(&valid_cfg, &symlink_data_dir_slash, Duration::from_secs(10));
        assert!(!timed_out, "ui-mcp --data-dir <symlink/> must not hang");
        assert_eq!(code, Some(1), "ui-mcp --data-dir <symlink/> must exit 1");
        assert!(
            stdout.is_empty(),
            "ui-mcp --data-dir <symlink/> must emit 0 stdout bytes, got: {stdout}"
        );
        assert!(!stderr.is_empty());
    }

    // (h) First-run UX: nonexistent --config and nonexistent --data-dir initialize cleanly
    // without probing ambient host port 4141.
    let nonexistent_cfg = temp.path().join("first_run_config.json");
    let nonexistent_data_dir = temp.path().join("first_run_data_dir");
    let closed_port = free_loopback_port();
    let (code, stdout, stderr, timed_out) = run_ui_mcp_bounded_with_calls(
        &nonexistent_cfg,
        &nonexistent_data_dir,
        vec![json!({
            "jsonrpc": "2.0",
            "id": 2,
            "method": "tools/call",
            "params": {
                "name": "save_settings",
                "arguments": { "proxy": { "port": closed_port } }
            }
        })],
        Duration::from_secs(15),
    );
    assert!(!timed_out, "first-run ui-mcp must not hang");
    assert_eq!(
        code,
        Some(0),
        "first-run ui-mcp must exit 0 cleanly; stderr={stderr}"
    );
    let lines: Vec<&str> = stdout.lines().filter(|l| !l.is_empty()).collect();
    assert!(
        lines.len() >= 2,
        "expected initialize + save_settings responses, got: {stdout}"
    );
    for line in &lines {
        let parsed: Value =
            serde_json::from_str(line).expect("first-run stdout must be strict JSON-RPC 2.0");
        assert_eq!(parsed["jsonrpc"], "2.0");
    }
}

#[cfg(unix)]
#[test]
fn ui_mcp_degrades_gracefully_and_preserves_rc_on_unsafe_leaf_env_sh() {
    use std::os::unix::fs::symlink;

    for kind in ["symlink", "directory", "fifo"] {
        let temp = TempDir::new().expect("tempdir");
        let home = temp.path().join("home");
        let xdg_config = temp.path().join("xdg_config");
        let xdg_data = temp.path().join("xdg_data");
        let data_dir = xdg_data.join("agy-companion");
        fs::create_dir_all(&home).expect("mkdir home");
        fs::create_dir_all(&data_dir).expect("mkdir data_dir");

        let cfg_path = xdg_config.join("agy-companion").join("config.json");
        let cfg = config_with_port(free_loopback_port());
        cfg.save_secure(&cfg_path).expect("save config");

        let env_path = data_dir.join(companion_core::integration::ENV_SCRIPT_FILENAME);
        let victim_env = temp.path().join("victim_ui_mcp_env.sh");
        let sentinel = "# VICTIM_UI_MCP_LEAF\n";
        match kind {
            "symlink" => {
                fs::write(&victim_env, sentinel).expect("write victim");
                symlink(&victim_env, &env_path).expect("symlink env.sh");
            }
            "directory" => {
                fs::create_dir_all(&env_path).expect("mkdir env.sh");
            }
            "fifo" => {
                assert!(
                    Command::new("mkfifo")
                        .arg(&env_path)
                        .status()
                        .expect("mkfifo")
                        .success()
                );
            }
            _ => unreachable!(),
        }

        let zlogin_path = home.join(".zlogin");
        let zlogin_before = format!(
            "{}\n[ -r \"${{XDG_DATA_HOME:-$HOME/.local/share}}/agy-companion/env.sh\" ] && . \"${{XDG_DATA_HOME:-$HOME/.local/share}}/agy-companion/env.sh\"\n{}\n",
            companion_core::HOOK_BEGIN_MARKER,
            companion_core::HOOK_END_MARKER
        );
        fs::write(&zlogin_path, &zlogin_before).expect("write zlogin");

        let mut child = isolated_cli_command(&home)
            .env("XDG_CONFIG_HOME", &xdg_config)
            .env("XDG_DATA_HOME", &xdg_data)
            .env("SHELL", "/bin/zsh")
            .env("ZDOTDIR", &home)
            .args(["ui-mcp", "--config"])
            .arg(&cfg_path)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("spawn ui-mcp");

        let mut stdin = child.stdin.take().expect("stdin");
        let stdout = child.stdout.take().expect("stdout");
        let stderr = child.stderr.take().expect("stderr");

        let init_req = json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "initialize",
            "params": {
                "protocolVersion": "2025-03-26",
                "capabilities": {},
                "clientInfo": { "name": "leaf-env-test", "version": "0.1.0" }
            }
        });
        let initialized_notif = json!({
            "jsonrpc": "2.0",
            "method": "notifications/initialized"
        });
        let get_settings_req = json!({
            "jsonrpc": "2.0",
            "id": 2,
            "method": "tools/call",
            "params": { "name": "get_settings", "arguments": {} }
        });
        let open_settings_req = json!({
            "jsonrpc": "2.0",
            "id": 3,
            "method": "tools/call",
            "params": { "name": "open_settings", "arguments": {} }
        });
        let disable_req = json!({
            "jsonrpc": "2.0",
            "id": 4,
            "method": "tools/call",
            "params": { "name": "set_launcher_integration", "arguments": { "enabled": false } }
        });

        writeln!(stdin, "{init_req}").expect("write init");
        stdin.flush().expect("flush init");

        let mut reader = BufReader::new(stdout);
        let mut init_line = String::new();
        reader.read_line(&mut init_line).expect("read init resp");
        let init_resp: Value = serde_json::from_str(&init_line).expect("parse init resp");
        assert_eq!(init_resp["id"], 1);

        writeln!(stdin, "{initialized_notif}").expect("write initialized");
        writeln!(stdin, "{get_settings_req}").expect("write get_settings");
        writeln!(stdin, "{open_settings_req}").expect("write open_settings");
        writeln!(stdin, "{disable_req}").expect("write disable");
        stdin.flush().expect("flush requests");
        drop(stdin);

        let mut responses: Vec<Value> = Vec::new();
        for line in reader.lines() {
            let line = line.expect("read line");
            if !line.is_empty() {
                responses.push(serde_json::from_str(&line).expect("parse jsonrpc"));
            }
        }
        let mut stderr_str = String::new();
        std::io::Read::read_to_string(&mut BufReader::new(stderr), &mut stderr_str)
            .expect("read stderr");
        let status = child.wait().expect("wait ui-mcp");
        assert_eq!(
            status.code(),
            Some(0),
            "{kind}: ui-mcp should exit 0 cleanly; stderr={stderr_str}"
        );

        for (req_id, name) in [(2, "get_settings"), (3, "open_settings")] {
            let resp = responses
                .iter()
                .find(|r| r["id"] == req_id)
                .unwrap_or_else(|| panic!("{kind}: missing response for {name}"));
            assert_eq!(
                resp["result"]["isError"], false,
                "{kind}: {name} must degrade cleanly with isError=false, got: {resp}"
            );
            let li = &resp["result"]["structuredContent"]["launcher_integration"];
            assert_eq!(
                li["state"], "disabled",
                "{kind}: {name} state must be disabled"
            );
            assert_eq!(li["enabled"], false, "{kind}: {name} enabled must be false");
            let msg = li["message"].as_str().unwrap_or("");
            assert!(
                !msg.is_empty() && msg.contains("env.sh"),
                "{kind}: {name} must surface informative env.sh error message, got: {li}"
            );
        }

        let dis_resp = responses
            .iter()
            .find(|r| r["id"] == 4)
            .unwrap_or_else(|| panic!("{kind}: missing response for set_launcher_integration"));
        assert_eq!(
            dis_resp["result"]["isError"], true,
            "{kind}: set_launcher_integration(false) must fail closed with isError=true, got: {dis_resp}"
        );

        assert!(
            zlogin_path.is_file(),
            "{kind}: .zlogin must NOT be deleted when set_launcher_integration(false) fails on leaf env.sh"
        );
        assert_eq!(
            fs::read_to_string(&zlogin_path).expect("read zlogin"),
            zlogin_before,
            "{kind}: .zlogin must stay byte-for-byte intact"
        );
        if kind == "symlink" {
            assert_eq!(
                fs::read_to_string(&victim_env).expect("read victim"),
                sentinel,
                "{kind}: victim_ui_mcp_env.sh must stay untouched"
            );
        }
    }
}

#[cfg(unix)]
#[test]
fn ui_mcp_auto_enable_launcher_and_get_usage_sqlite_end_to_end() {
    use companion_core::{GeminiUsageMetadata, SqliteUsageSink, UsageEvent};

    let temp = TempDir::new().expect("tempdir");
    let home = temp.path().join("home");
    let xdg_config = home.join("xdg-config");
    let xdg_data = home.join("xdg-data");
    let usage_dir = xdg_data.join("agy-companion").join("usage");
    fs::create_dir_all(&home).expect("mkdir home");
    fs::create_dir_all(&xdg_config).expect("mkdir xdg_config");
    fs::create_dir_all(&xdg_data).expect("mkdir xdg_data");

    let cfg_path = xdg_config.join("agy-companion").join("config.json");
    let sink = SqliteUsageSink::new(&usage_dir).expect("sink");
    sink.record_checked(&UsageEvent::from_gemini(
        "gemini-4-carbon",
        &GeminiUsageMetadata {
            prompt_token_count: 1200,
            candidates_token_count: 250,
            cached_content_token_count: 600,
            thoughts_token_count: 40,
        },
        300,
    ))
    .expect("seed gemini event");
    sink.record_checked(&UsageEvent::from_antigravity_builtin(
        "gemini-3.8-pro",
        &GeminiUsageMetadata {
            prompt_token_count: 800,
            candidates_token_count: 150,
            cached_content_token_count: 0,
            thoughts_token_count: 0,
        },
        150,
    ))
    .expect("seed builtin event");
    drop(sink);

    let mut child = isolated_cli_command(&home)
        .args([
            "ui-mcp",
            "--config",
            cfg_path.to_str().unwrap(),
            "--auto-enable-launcher",
        ])
        .env("SHELL", "/bin/zsh")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn ui-mcp");

    let mut stdin = child.stdin.take().expect("stdin");
    let mut stdout = BufReader::new(child.stdout.take().expect("stdout"));

    let init_req = json!({
        "jsonrpc": "2.0",
        "id": 1,
        "method": "initialize",
        "params": {
            "protocolVersion": "2025-03-26",
            "capabilities": {},
            "clientInfo": { "name": "cli-e2e", "version": "0.1.0" }
        }
    });
    writeln!(stdin, "{init_req}").expect("write init");
    stdin.flush().expect("flush init");
    let mut init_line = String::new();
    stdout.read_line(&mut init_line).expect("read init");

    let notif = json!({ "jsonrpc": "2.0", "method": "notifications/initialized" });
    writeln!(stdin, "{notif}").expect("write notif");

    let open_req = json!({
        "jsonrpc": "2.0",
        "id": 2,
        "method": "tools/call",
        "params": { "name": "open_settings", "arguments": {} }
    });
    writeln!(stdin, "{open_req}").expect("write open_settings");

    let usage_req = json!({
        "jsonrpc": "2.0",
        "id": 3,
        "method": "tools/call",
        "params": { "name": "get_usage", "arguments": { "range": "7d" } }
    });
    writeln!(stdin, "{usage_req}").expect("write get_usage");
    stdin.flush().expect("flush requests");
    drop(stdin);

    let mut responses = Vec::new();
    let mut line = String::new();
    while stdout.read_line(&mut line).expect("read line") > 0 {
        if let Ok(val) = serde_json::from_str::<Value>(&line) {
            responses.push(val);
        }
        line.clear();
    }
    let status = child.wait().expect("wait ui-mcp");
    assert!(status.success(), "ui-mcp should exit 0 cleanly");

    let open_resp = responses
        .iter()
        .find(|r| r["id"] == 2)
        .expect("open_settings response");
    assert_eq!(open_resp["result"]["isError"], false);
    assert_eq!(
        open_resp["result"]["structuredContent"]["config"]["launcher_auto_start"],
        true
    );
    assert_eq!(
        open_resp["result"]["structuredContent"]["launcher_integration"]["enabled"],
        true
    );
    assert!(home.join(".zlogin").is_file());

    let usage_resp = responses
        .iter()
        .find(|r| r["id"] == 3)
        .expect("get_usage response");
    assert_eq!(usage_resp["result"]["isError"], false);
    let usage_sc = &usage_resp["result"]["structuredContent"];
    assert_eq!(usage_sc["range"], "7d");
    assert_eq!(usage_sc["totals"]["requests"], 2);
    assert_eq!(usage_sc["totals"]["input_tokens"], 2000);
    assert_eq!(usage_sc["totals"]["cached_tokens"], 600);
    assert_eq!(usage_sc["by_model"].as_array().unwrap().len(), 2);
    assert_eq!(usage_sc["by_provider"].as_array().unwrap().len(), 2);
}
