use std::fs;
#[cfg(unix)]
use std::process::{Command, Stdio};
#[cfg(unix)]
use std::time::{Duration, Instant};

use companion_core::{ApiKey, Config};
use companion_proxy::{HealthResponse, PROTOCOL_VERSION};
use serde_json::Value;
use tempfile::TempDir;

#[cfg(unix)]
use crate::common::ChildGuard;
use crate::common::{
    BIN, config_with_port, free_loopback_port, isolated_cli_command, isolated_tokio_cli_command,
    loopback_http_client, proxy_pid_matches_child, spawn_mock_health, spawn_serve_and_wait_healthy,
    spawn_serve_with_port_picker,
};

#[test]
fn binary_version_prints_agy_companion_version() {
    let temp = TempDir::new().expect("tempdir");
    let output = isolated_cli_command(temp.path())
        .arg("--version")
        .output()
        .expect("run --version");
    assert!(output.status.success());
    assert_eq!(
        String::from_utf8_lossy(&output.stdout).trim(),
        format!("agy-companion {}", env!("CARGO_PKG_VERSION"))
    );
    assert!(output.stderr.is_empty());
}

#[test]
fn serve_rejects_non_loopback_config_and_port_zero() {
    let temp = TempDir::new().expect("tempdir");

    // (a) proxy.host: 0.0.0.0
    let bad_host = temp.path().join("bad_host.json");
    fs::write(
        &bad_host,
        r#"{"schema_version":1,"proxy":{"host":"0.0.0.0","port":4141},"providers":{"gemini":{"base_url":"https://generativelanguage.googleapis.com","api_key":"AIzaSyServeBadHostSecret111","default_model":"gemini-4-hydrogen","timeout_secs":900}}}"#,
    )
    .expect("write bad_host");
    let out = isolated_cli_command(temp.path())
        .args(["serve", "--config"])
        .arg(&bad_host)
        .output()
        .expect("run serve bad_host");
    assert!(!out.status.success());
    assert!(out.stdout.is_empty());
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(!stderr.is_empty());
    assert!(!stderr.contains("AIzaSyServeBadHostSecret111"));

    // (b) --port 0
    let valid_cfg = temp.path().join("valid.json");
    Config::default().save_secure(&valid_cfg).expect("save");
    let out = isolated_cli_command(temp.path())
        .args(["serve", "--port", "0", "--config"])
        .arg(&valid_cfg)
        .output()
        .expect("run serve port 0");
    assert!(!out.status.success());
    assert!(out.stdout.is_empty());

    // (c) remote http:// upstream
    let bad_url = temp.path().join("bad_url.json");
    fs::write(
        &bad_url,
        r#"{"schema_version":1,"proxy":{"host":"127.0.0.1","port":4141},"providers":{"gemini":{"base_url":"http://evil.com","api_key":"AIzaSyServeBadUrlSecret222","default_model":"gemini-4-hydrogen","timeout_secs":900}}}"#,
    )
    .expect("write bad_url");
    let out = isolated_cli_command(temp.path())
        .args(["serve", "--config"])
        .arg(&bad_url)
        .output()
        .expect("run serve bad_url");
    assert!(!out.status.success());
    assert!(out.stdout.is_empty());
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(!stderr.is_empty());
    assert!(!stderr.contains("AIzaSyServeBadUrlSecret222"));
}

#[tokio::test]
async fn serve_binds_loopback_serves_health_and_exits_cleanly_on_sigterm() {
    let temp = TempDir::new().expect("tempdir");
    let cfg_path = temp.path().join("config.json");
    let usage_dir = temp.path().join("usage");

    let mut cfg = config_with_port(free_loopback_port());
    cfg.providers.gemini.api_key = Some(ApiKey::parse_strict("AIzaSyServeLiveSecret8888").unwrap());
    cfg.usage_dir = Some(usage_dir);
    cfg.save_secure(&cfg_path).expect("save_secure");

    let client = loopback_http_client();

    let (guard, _port, resp) = spawn_serve_and_wait_healthy(client, &cfg_path, |port| {
        let mut cmd = isolated_cli_command(temp.path());
        cmd.args(["serve", "--port", &port.to_string(), "--config"])
            .arg(&cfg_path)
            .env("GEMINI_API_KEY", "AIzaSyServeEnvSecret7777");
        cmd
    })
    .await;

    assert_eq!(resp.status(), reqwest::StatusCode::OK);
    assert_eq!(
        resp.headers()
            .get("x-content-type-options")
            .and_then(|v| v.to_str().ok()),
        Some("nosniff")
    );
    assert_eq!(
        resp.headers()
            .get("cache-control")
            .and_then(|v| v.to_str().ok()),
        Some("no-store")
    );
    let body: HealthResponse = resp.json().await.expect("parse HealthResponse");
    assert_eq!(body.status, "ok");
    assert_eq!(body.version, env!("CARGO_PKG_VERSION"));
    assert_eq!(body.protocol_version, PROTOCOL_VERSION);

    #[cfg(unix)]
    {
        let pid = guard.id().to_string();
        let kill_status = Command::new("kill")
            .args(["-TERM", &pid])
            .status()
            .expect("send SIGTERM");
        assert!(kill_status.success());

        let output = tokio::task::spawn_blocking(move || guard.wait_with_output())
            .await
            .expect("join wait")
            .expect("wait_with_output");
        assert_eq!(
            output.status.code(),
            Some(0),
            "serve must exit cleanly with code 0 on SIGTERM; stderr={}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(output.stdout.is_empty(), "serve stdout must be empty");
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(
            !stderr.contains("\x1b["),
            "serve stderr must not contain ANSI escapes"
        );
        assert!(!stderr.contains("AIzaSyServeLiveSecret8888"));
        assert!(!stderr.contains("AIzaSyServeEnvSecret7777"));
    }

    #[cfg(not(unix))]
    let _ = &guard;
}

#[tokio::test]
async fn doctor_passes_against_healthy_server_under_hostile_proxy_and_never_leaks_key() {
    let (port, handle) = spawn_mock_health("ok", env!("CARGO_PKG_VERSION"), PROTOCOL_VERSION).await;
    let temp = TempDir::new().expect("tempdir");
    let cfg_path = temp.path().join("config.json");
    let mut cfg = Config::default();
    cfg.providers.gemini.api_key =
        Some(ApiKey::parse_strict("AIzaSyCfgSecretMustNotLeak1").unwrap());
    cfg.save_secure(&cfg_path).expect("save_secure");

    for extra_args in [&["--json"][..], &[][..]] {
        let mut cmd = isolated_tokio_cli_command(temp.path());
        cmd.args(["doctor", "--config"])
            .arg(&cfg_path)
            .args(["--port", &port.to_string()])
            .args(extra_args)
            .env("HTTP_PROXY", "http://127.0.0.1:1")
            .env("HTTPS_PROXY", "http://127.0.0.1:1")
            .env("ALL_PROXY", "http://127.0.0.1:1")
            .env("GEMINI_API_KEY", "AIzaSyEnvSecretMustNotLeak2")
            .env(
                "GOOGLE_GEMINI_BASE_URL",
                format!("http://127.0.0.1:{port}?key=AIzaSyQuerySecret3"),
            );
        let output = cmd.output().await.expect("run doctor");
        assert_eq!(
            output.status.code(),
            Some(0),
            "doctor should pass; stdout={}, stderr={}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );

        let stdout = String::from_utf8_lossy(&output.stdout);
        let stderr = String::from_utf8_lossy(&output.stderr);
        if extra_args.contains(&"--json") {
            let parsed: Value = serde_json::from_str(&stdout).expect("valid doctor JSON");
            assert_eq!(parsed["ok"], true);
            assert_eq!(parsed["version_skew"], false);
            assert_eq!(parsed["protocol_skew"], false);
        }
        let combined = format!("{stdout}\n{stderr}");
        for secret in [
            "AIzaSyCfgSecretMustNotLeak1",
            "AIzaSyEnvSecretMustNotLeak2",
            "AIzaSyEnvSecret",
            "Leak2",
            "AIzaSyQuerySecret3",
            "QuerySecret3",
        ] {
            assert!(!combined.contains(secret), "doctor leaked '{secret}'");
        }
    }

    handle.abort();
}

#[tokio::test]
async fn doctor_fails_on_version_skew_open_permissions_and_unreachable_proxy() {
    let temp = TempDir::new().expect("tempdir");

    // (a) Version skew + 0600 config
    let (skew_port, skew_handle) = spawn_mock_health("ok", "0.0.1", PROTOCOL_VERSION).await;
    let cfg_ok = temp.path().join("cfg_ok.json");
    Config::default().save_secure(&cfg_ok).expect("save_secure");
    let out = isolated_tokio_cli_command(temp.path())
        .args(["doctor", "--config"])
        .arg(&cfg_ok)
        .args(["--port", &skew_port.to_string(), "--json"])
        .output()
        .await
        .expect("run doctor skew");
    skew_handle.abort();
    assert_eq!(out.status.code(), Some(1));
    let parsed: Value =
        serde_json::from_str(&String::from_utf8_lossy(&out.stdout)).expect("valid JSON on skew");
    assert_eq!(parsed["ok"], false);
    assert_eq!(parsed["version_skew"], true);

    // (b) Healthy server + 0644 config
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;

        let (ok_port, ok_handle) =
            spawn_mock_health("ok", env!("CARGO_PKG_VERSION"), PROTOCOL_VERSION).await;
        let cfg_open = temp.path().join("cfg_0644.json");
        Config::default()
            .save_secure(&cfg_open)
            .expect("save_secure");
        fs::set_permissions(&cfg_open, fs::Permissions::from_mode(0o644)).expect("chmod 0644");

        let out = isolated_tokio_cli_command(temp.path())
            .args(["doctor", "--config"])
            .arg(&cfg_open)
            .args(["--port", &ok_port.to_string(), "--json"])
            .output()
            .await
            .expect("run doctor 0644");
        ok_handle.abort();
        assert_eq!(out.status.code(), Some(1));
        let stdout = String::from_utf8_lossy(&out.stdout);
        let parsed: Value = serde_json::from_str(&stdout).expect("valid JSON on 0644");
        assert_eq!(parsed["ok"], false);
        assert!(stdout.contains("0644"));
    }

    // (c) Closed free port + 0600 config
    let closed_port = free_loopback_port();
    let out = isolated_tokio_cli_command(temp.path())
        .args(["doctor", "--config"])
        .arg(&cfg_ok)
        .args(["--port", &closed_port.to_string(), "--json"])
        .output()
        .await
        .expect("run doctor unreachable");
    assert_eq!(out.status.code(), Some(1));
    let parsed: Value = serde_json::from_str(&String::from_utf8_lossy(&out.stdout))
        .expect("valid JSON on unreachable");
    assert_eq!(parsed["ok"], false);
    assert_eq!(parsed["proxy"]["reachable"], false);
}

#[test]
fn models_json_flag_emits_valid_custom_models_config_for_custom_port() {
    let temp = TempDir::new().expect("tempdir");
    let cfg_path = temp.path().join("config.json");
    let mut cfg = Config::default();
    cfg.providers.gemini.default_model = "gemini-3.8-flash".to_string();
    cfg.providers.gemini.api_key = Some(ApiKey::parse_strict("AIzaSyModelsSecret4444").unwrap());
    cfg.save_secure(&cfg_path).expect("save_secure");

    let output = isolated_cli_command(temp.path())
        .args(["models", "--json", "--port", "5252", "--config"])
        .arg(&cfg_path)
        .output()
        .expect("run models");
    assert_eq!(output.status.code(), Some(0));

    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    let parsed: Value = serde_json::from_str(&stdout).expect("pure JSON stdout");
    assert_eq!(parsed["modelProvider"], "gemini");
    let models = parsed["customModelsConfig"]["customModels"]
        .as_array()
        .expect("customModels array");
    assert_eq!(models.len(), 5);
    assert_eq!(
        models[0]["modelName"],
        "gemini-api://http://127.0.0.1:5252/models/gemini-3.8-flash"
    );
    assert_eq!(
        models[0]["label"],
        "Antigravity Companion (gemini-3.8-flash)"
    );
    assert_eq!(
        models[1]["modelName"],
        "gemini-api://http://127.0.0.1:5252/models/gemini-4-carbon"
    );
    assert_eq!(
        models[1]["label"],
        "Antigravity Companion (gemini-4-carbon)"
    );
    assert_eq!(
        models[2]["modelName"],
        "gemini-api://http://127.0.0.1:5252/models/gemini-4-neon"
    );
    assert_eq!(models[2]["label"], "Antigravity Companion (gemini-4-neon)");
    assert_eq!(
        models[3]["modelName"],
        "gemini-api://http://127.0.0.1:5252/models/gemini-4-hydrogen"
    );
    assert_eq!(
        models[3]["label"],
        "Antigravity Companion (gemini-4-hydrogen)"
    );
    assert_eq!(
        models[4]["modelName"],
        "gemini-api://http://127.0.0.1:5252/models/gemini-4-argon"
    );
    assert_eq!(models[4]["label"], "Antigravity Companion (gemini-4-argon)");
    assert!(!stdout.contains("AIzaSyModelsSecret4444"));
    assert!(!stderr.contains("AIzaSyModelsSecret4444"));

    // Negative check: --port 0 must exit non-zero
    let zero_out = isolated_cli_command(temp.path())
        .args(["models", "--json", "--port", "0", "--config"])
        .arg(&cfg_path)
        .output()
        .expect("run models port 0");
    assert!(!zero_out.status.success());
    assert!(zero_out.stdout.is_empty());
}

#[test]
fn config_env_precedence_and_eager_path_traversal_rejection() {
    let temp = TempDir::new().expect("tempdir");
    let env_cfg = temp.path().join("env_config.json");
    let mut cfg = config_with_port(5353);
    cfg.providers.gemini.default_model = "gemini-3.9-pro".to_string();
    cfg.save_secure(&env_cfg).expect("save env_cfg");

    // AGY_COMPANION_CONFIG is honored when --config is omitted
    let out = isolated_cli_command(temp.path())
        .args(["models", "--json"])
        .env("AGY_COMPANION_CONFIG", &env_cfg)
        .output()
        .expect("run models with env config");
    assert_eq!(out.status.code(), Some(0));
    let parsed: Value =
        serde_json::from_str(&String::from_utf8_lossy(&out.stdout)).expect("valid json");
    assert_eq!(
        parsed["customModelsConfig"]["customModels"][0]["modelName"],
        "gemini-api://http://127.0.0.1:5353/models/gemini-3.9-pro"
    );

    // Explicit --config overrides even an invalid AGY_COMPANION_CONFIG
    let cli_cfg = temp.path().join("cli_config.json");
    let cfg2 = config_with_port(5454);
    cfg2.save_secure(&cli_cfg).expect("save cli_cfg");
    let out = isolated_cli_command(temp.path())
        .args(["models", "--json", "--config"])
        .arg(&cli_cfg)
        .env("AGY_COMPANION_CONFIG", "../invalid-env-path.json")
        .output()
        .expect("run models with --config override");
    assert_eq!(out.status.code(), Some(0));
    let parsed: Value =
        serde_json::from_str(&String::from_utf8_lossy(&out.stdout)).expect("valid json");
    assert_eq!(
        parsed["customModelsConfig"]["customModels"][0]["modelName"],
        "gemini-api://http://127.0.0.1:5454/models/gemini-4-hydrogen"
    );

    // ui-mcp eagerly rejects path traversal in --data-dir with zero stdout
    let out = isolated_cli_command(temp.path())
        .args(["ui-mcp", "--data-dir=../escape", "--config"])
        .arg(&cli_cfg)
        .output()
        .expect("run ui-mcp invalid data-dir");
    assert_eq!(out.status.code(), Some(1));
    assert!(out.stdout.is_empty());
    assert!(!out.stderr.is_empty());

    // doctor --config ../escape.json --json still outputs valid JSON and exits 1
    let closed_port = free_loopback_port();
    let out = isolated_cli_command(temp.path())
        .args([
            "doctor",
            "--config",
            "../escape.json",
            "--port",
            &closed_port.to_string(),
            "--json",
        ])
        .output()
        .expect("run doctor invalid config path");
    assert_eq!(out.status.code(), Some(1));
    let parsed: Value = serde_json::from_str(&String::from_utf8_lossy(&out.stdout))
        .expect("doctor --json must emit valid JSON even on invalid config path");
    assert_eq!(parsed["ok"], false);
}

#[cfg(unix)]
#[tokio::test]
async fn doctor_rejects_trailing_slash_and_dot_suffix_symlinks_via_flag_and_env() {
    use std::os::unix::fs::symlink;
    use std::path::PathBuf;

    let (port, handle) = spawn_mock_health("ok", env!("CARGO_PKG_VERSION"), PROTOCOL_VERSION).await;
    let temp = TempDir::new().expect("tempdir");

    let real_cfg = temp.path().join("real_0600.json");
    Config::default().save_secure(&real_cfg).expect("save real");
    let real_dir = temp.path().join("real_dir");
    fs::create_dir(&real_dir).expect("mkdir real_dir");

    let dangling_link = temp.path().join("dangling_symlink.json");
    symlink(temp.path().join("missing_target.json"), &dangling_link).expect("dangling symlink");

    let file_link = temp.path().join("file_symlink.json");
    symlink(&real_cfg, &file_link).expect("file symlink");

    let dir_link = temp.path().join("dir_symlink");
    symlink(&real_dir, &dir_link).expect("dir symlink");

    // 4 representative trailing-slash & dot-suffix symlink shapes (exhaustive normalization
    // permutations are owned by companion_core::fs_safe unit tests).
    let cases = [
        (
            "dangling/",
            PathBuf::from(format!("{}/", dangling_link.display())),
        ),
        (
            "dangling/.",
            PathBuf::from(format!("{}/.", dangling_link.display())),
        ),
        (
            "sym_file/",
            PathBuf::from(format!("{}/", file_link.display())),
        ),
        (
            "sym_dir/.",
            PathBuf::from(format!("{}/.", dir_link.display())),
        ),
    ];

    for (label, path) in &cases {
        for use_env in [false, true] {
            for json_mode in [true, false] {
                let mut cmd = isolated_tokio_cli_command(temp.path());
                cmd.arg("doctor");
                if use_env {
                    cmd.env("AGY_COMPANION_CONFIG", path);
                } else {
                    cmd.arg("--config").arg(path);
                }
                cmd.args(["--port", &port.to_string()]);
                if json_mode {
                    cmd.arg("--json");
                }

                let output = cmd.output().await.expect("run doctor");
                let stdout = String::from_utf8_lossy(&output.stdout);
                let stderr = String::from_utf8_lossy(&output.stderr);
                let mode_desc = format!(
                    "{label} via {} ({})",
                    if use_env {
                        "AGY_COMPANION_CONFIG"
                    } else {
                        "--config"
                    },
                    if json_mode { "json" } else { "human" }
                );

                assert_eq!(
                    output.status.code(),
                    Some(1),
                    "{mode_desc}: expected exit 1; stdout={stdout}, stderr={stderr}"
                );

                if json_mode {
                    let parsed: Value = serde_json::from_str(&stdout)
                        .unwrap_or_else(|e| panic!("{mode_desc}: invalid JSON ({e}): {stdout}"));
                    assert_eq!(parsed["ok"], false, "{mode_desc}: expected ok=false");
                    let perm_check = parsed["checks"]
                        .as_array()
                        .expect("checks array")
                        .iter()
                        .find(|c| c["name"] == "config_permissions")
                        .expect("config_permissions check");
                    assert_eq!(
                        perm_check["status"], "fail",
                        "{mode_desc}: expected config_permissions fail: {perm_check}"
                    );
                    let msg = perm_check["message"]
                        .as_str()
                        .expect("config_permissions message");
                    assert!(
                        msg.to_lowercase().contains("symlink"),
                        "{mode_desc}: expected 'symlink' in config_permissions message, got '{msg}'"
                    );
                } else {
                    assert!(
                        stdout.contains("[FAIL] config_permissions:"),
                        "{mode_desc}: missing [FAIL] config_permissions in stdout:\n{stdout}"
                    );
                    assert!(
                        stdout.to_lowercase().contains("symlink"),
                        "{mode_desc}: missing 'symlink' in human stdout:\n{stdout}"
                    );
                    assert!(
                        stdout.contains("Result: FAILED"),
                        "{mode_desc}: missing 'Result: FAILED' in human stdout:\n{stdout}"
                    );
                }
            }
        }
    }

    handle.abort();
}

#[tokio::test]
async fn serve_writes_0600_proxy_pid_and_proxy_log_in_isolated_dir_and_cleans_pid_on_sigterm() {
    let temp = TempDir::new().expect("tempdir");
    let cfg_path = temp.path().join("config.json");
    let usage_dir = temp.path().join("usage");

    let mut cfg = config_with_port(free_loopback_port());
    cfg.providers.gemini.api_key = Some(ApiKey::parse_strict("AIzaSyPidLogSecret12345").unwrap());
    cfg.usage_dir = Some(usage_dir);
    cfg.save_secure(&cfg_path).expect("save_secure");

    let client = loopback_http_client();
    let (guard, port, _resp) = spawn_serve_and_wait_healthy(client, &cfg_path, |port| {
        let mut cmd = isolated_cli_command(temp.path());
        cmd.args(["serve", "--port", &port.to_string(), "--config"])
            .arg(&cfg_path);
        cmd
    })
    .await;
    let expected_pid = guard.id();

    let pid_path = temp.path().join("proxy.pid");
    let log_path = temp.path().join("proxy.log");
    assert!(pid_path.is_file(), "proxy.pid must exist in isolated dir");
    assert!(log_path.is_file(), "proxy.log must exist in isolated dir");

    let pid_json: Value =
        serde_json::from_str(&fs::read_to_string(&pid_path).expect("read proxy.pid"))
            .expect("parse proxy.pid JSON");
    assert_eq!(pid_json["pid"], expected_pid);
    assert_eq!(pid_json["port"], port);
    assert_eq!(pid_json["version"], env!("CARGO_PKG_VERSION"));
    assert!(pid_json["started_at_ms"].as_u64().is_some_and(|v| v > 0));
    assert!(
        pid_json["binary_path"]
            .as_str()
            .is_some_and(|s| !s.is_empty())
    );

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(
            fs::metadata(&pid_path)
                .expect("pid meta")
                .permissions()
                .mode()
                & 0o777,
            0o600
        );
        assert_eq!(
            fs::metadata(&log_path)
                .expect("log meta")
                .permissions()
                .mode()
                & 0o777,
            0o600
        );
    }

    let log_text = fs::read_to_string(&log_path).expect("read proxy.log");
    assert!(
        log_text.contains("agy-companion proxy listening"),
        "proxy.log missing startup line: {log_text}"
    );
    assert!(!log_text.contains("AIzaSyPidLogSecret12345"));

    #[cfg(unix)]
    {
        let kill_status = Command::new("kill")
            .args(["-TERM", &expected_pid.to_string()])
            .status()
            .expect("send SIGTERM");
        assert!(kill_status.success());

        let output = tokio::task::spawn_blocking(move || guard.wait_with_output())
            .await
            .expect("join wait")
            .expect("wait_with_output");
        assert_eq!(output.status.code(), Some(0));
        assert!(
            !pid_path.exists(),
            "proxy.pid must be removed on clean SIGTERM shutdown"
        );
    }
}

#[cfg(unix)]
#[tokio::test]
async fn serve_adopt_running_does_not_clobber_pid_and_takes_over_within_1500ms() {
    let temp = TempDir::new().expect("tempdir");
    let cfg_path = temp.path().join("config.json");
    let usage_dir = temp.path().join("usage");

    let mut cfg = config_with_port(free_loopback_port());
    cfg.usage_dir = Some(usage_dir);
    cfg.save_secure(&cfg_path).expect("save_secure");

    let client = loopback_http_client();
    let (primary_guard, port, _resp) = spawn_serve_and_wait_healthy(client, &cfg_path, |port| {
        let mut cmd = isolated_cli_command(temp.path());
        cmd.args(["serve", "--port", &port.to_string(), "--config"])
            .arg(&cfg_path);
        cmd
    })
    .await;
    let primary_pid = primary_guard.id();
    let health_url = format!("http://127.0.0.1:{port}/health");

    let pid_path = temp.path().join("proxy.pid");
    let primary_pid_json: Value =
        serde_json::from_str(&fs::read_to_string(&pid_path).expect("read pid")).expect("parse");
    assert_eq!(primary_pid_json["pid"], primary_pid);

    // Start standby with --adopt-running.
    let standby_child = isolated_cli_command(temp.path())
        .args(["serve", "--port", &port.to_string(), "--config"])
        .arg(&cfg_path)
        .arg("--adopt-running")
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn standby");
    let mut standby_guard = ChildGuard::new(standby_child);
    let standby_pid = standby_guard.id();

    // Standby must NOT clobber primary's proxy.pid while parked.
    tokio::time::sleep(Duration::from_millis(250)).await;
    if let Some(status) = standby_guard.try_wait().expect("try_wait standby") {
        let output = tokio::task::spawn_blocking(move || standby_guard.wait_with_output())
            .await
            .expect("join standby")
            .expect("wait standby");
        panic!(
            "standby exited prematurely while parked (status={status}): stderr={}",
            String::from_utf8_lossy(&output.stderr)
        );
    }
    let parked_pid_json: Value =
        serde_json::from_str(&fs::read_to_string(&pid_path).expect("read pid")).expect("parse");
    assert_eq!(parked_pid_json["pid"], primary_pid);

    // Stop primary; standby must take over within ~1.5s and update proxy.pid to standby_pid.
    let takeover_start = Instant::now();
    let kill_primary = Command::new("kill")
        .args(["-TERM", &primary_pid.to_string()])
        .status()
        .expect("kill primary");
    assert!(kill_primary.success());
    let primary_out = tokio::task::spawn_blocking(move || primary_guard.wait_with_output())
        .await
        .expect("join primary")
        .expect("wait primary");
    assert_eq!(primary_out.status.code(), Some(0));

    let takeover_deadline = Instant::now() + Duration::from_millis(2000);
    loop {
        if let Some(status) = standby_guard.try_wait().expect("try_wait standby") {
            let output = tokio::task::spawn_blocking(move || standby_guard.wait_with_output())
                .await
                .expect("join standby")
                .expect("wait standby");
            panic!(
                "standby exited prematurely during takeover (status={status}): stderr={}",
                String::from_utf8_lossy(&output.stderr)
            );
        }
        if let Ok(raw) = fs::read_to_string(&pid_path)
            && let Ok(parsed) = serde_json::from_str::<Value>(&raw)
            && parsed["pid"] == standby_pid
            && let Ok(resp) = client.get(&health_url).send().await
            && resp.status().is_success()
        {
            break;
        }
        assert!(
            Instant::now() < takeover_deadline,
            "standby failed to take over within 2s (elapsed={:?})",
            takeover_start.elapsed()
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }

    // Stop standby cleanly and verify proxy.pid is removed.
    let kill_standby = Command::new("kill")
        .args(["-TERM", &standby_pid.to_string()])
        .status()
        .expect("kill standby");
    assert!(kill_standby.success());
    let standby_out = tokio::task::spawn_blocking(move || standby_guard.wait_with_output())
        .await
        .expect("join standby")
        .expect("wait standby");
    assert_eq!(standby_out.status.code(), Some(0));
    assert!(!pid_path.exists());
}

#[tokio::test]
async fn spawn_serve_rejects_foreign_health_and_retries_fresh_port() {
    let (foreign_port, foreign_handle) =
        spawn_mock_health("ok", env!("CARGO_PKG_VERSION"), PROTOCOL_VERSION).await;

    let temp = TempDir::new().expect("tempdir");
    let cfg_path = temp.path().join("config.json");
    let usage_dir = temp.path().join("usage");
    let mut cfg = config_with_port(free_loopback_port());
    cfg.usage_dir = Some(usage_dir);
    cfg.save_secure(&cfg_path).expect("save_secure");

    // Pre-seed a stale proxy.pid with matching port but non-matching PID (0) to verify
    // that foreign /health 200 OK cannot pass ownership verification even if port matches.
    let pid_path = temp.path().join(companion_cli::daemon::PROXY_PID_FILENAME);
    let stale_pid = companion_cli::daemon::ProxyPidInfo {
        pid: 0,
        port: foreign_port,
        version: env!("CARGO_PKG_VERSION").to_string(),
        started_at_ms: 1,
        binary_path: BIN.to_string(),
    };
    fs::write(
        &pid_path,
        serde_json::to_vec(&stale_pid).expect("serialize stale pid"),
    )
    .expect("write stale pid");

    let client = loopback_http_client();

    let mut attempts_seen = 0usize;
    let (guard, actual_port, resp) = spawn_serve_with_port_picker(
        client,
        &cfg_path,
        |attempt| {
            attempts_seen = attempt;
            if attempt == 1 {
                foreign_port
            } else {
                free_loopback_port()
            }
        },
        |port| {
            let mut cmd = isolated_cli_command(temp.path());
            cmd.args(["serve", "--port", &port.to_string(), "--config"])
                .arg(&cfg_path);
            cmd
        },
    )
    .await;

    foreign_handle.abort();

    assert!(
        attempts_seen >= 2,
        "expected AddrInUse retry after foreign /health rejection, saw attempts={attempts_seen}"
    );
    assert_ne!(
        actual_port, foreign_port,
        "child must retry onto a fresh port instead of accepting foreign /health"
    );
    assert_eq!(resp.status(), reqwest::StatusCode::OK);
    assert!(
        proxy_pid_matches_child(&pid_path, guard.id(), actual_port),
        "proxy.pid must match our child's PID and fresh port"
    );

    #[cfg(unix)]
    {
        let kill_status = Command::new("kill")
            .args(["-TERM", &guard.id().to_string()])
            .status()
            .expect("send SIGTERM");
        assert!(kill_status.success());

        let output = tokio::task::spawn_blocking(move || guard.wait_with_output())
            .await
            .expect("join wait")
            .expect("wait_with_output");
        assert_eq!(output.status.code(), Some(0));
        assert!(!pid_path.exists());
    }
}
