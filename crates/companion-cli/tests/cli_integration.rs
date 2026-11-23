#![forbid(unsafe_code)]

use std::fs;
use std::io::{BufRead, BufReader, Write};
use std::net::TcpListener as StdTcpListener;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use axum::Router;
use axum::routing::get;
use companion_core::Config;
use companion_proxy::{HealthResponse, PROTOCOL_VERSION};
use serde_json::{Value, json};
use tempfile::TempDir;
use tokio::net::TcpListener;

const BIN: &str = env!("CARGO_BIN_EXE_agy-companion");

struct ChildGuard {
    child: Option<Child>,
}

impl ChildGuard {
    fn new(child: Child) -> Self {
        Self { child: Some(child) }
    }

    fn id(&self) -> u32 {
        self.child.as_ref().expect("child present").id()
    }

    fn wait_with_output(mut self) -> std::io::Result<std::process::Output> {
        self.child.take().expect("child present").wait_with_output()
    }
}

impl Drop for ChildGuard {
    fn drop(&mut self) {
        if let Some(mut child) = self.child.take() {
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}

fn free_loopback_port() -> u16 {
    let listener = StdTcpListener::bind("127.0.0.1:0").expect("bind ephemeral port");
    listener.local_addr().expect("local_addr").port()
}

async fn spawn_mock_health(
    status: &'static str,
    version: &'static str,
    protocol_version: u32,
) -> (u16, tokio::task::JoinHandle<()>) {
    let router = Router::new().route(
        "/health",
        get(move || async move {
            axum::Json(HealthResponse {
                status: status.to_string(),
                version: version.to_string(),
                protocol_version,
            })
        }),
    );
    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind mock health");
    let port = listener.local_addr().expect("local_addr").port();
    let handle = tokio::spawn(async move {
        let _ = axum::serve(listener, router).await;
    });
    (port, handle)
}

#[test]
fn binary_version_prints_agy_companion_version() {
    let output = Command::new(BIN)
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

    // (a) listen_host: 0.0.0.0
    let bad_host = temp.path().join("bad_host.json");
    fs::write(
        &bad_host,
        r#"{"schema_version":1,"listen_host":"0.0.0.0","listen_port":4141,"gemini":{"base_url":"https://generativelanguage.googleapis.com","api_key":"AIzaSyServeBadHostSecret111","default_model":"gemini-4-hydrogen","timeout_secs":900}}"#,
    )
    .expect("write bad_host");
    let out = Command::new(BIN)
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
    let out = Command::new(BIN)
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
        r#"{"schema_version":1,"listen_host":"127.0.0.1","listen_port":4141,"gemini":{"base_url":"http://evil.com","api_key":"AIzaSyServeBadUrlSecret222","default_model":"gemini-4-hydrogen","timeout_secs":900}}"#,
    )
    .expect("write bad_url");
    let out = Command::new(BIN)
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

    let mut cfg = Config::default();
    cfg.gemini.api_key = Some("AIzaSyServeLiveSecret8888".to_string());
    cfg.usage_dir = Some(usage_dir);
    cfg.save_secure(&cfg_path).expect("save_secure");

    let port = free_loopback_port();
    let child = Command::new(BIN)
        .args(["serve", "--port", &port.to_string(), "--config"])
        .arg(&cfg_path)
        .env("GEMINI_API_KEY", "AIzaSyServeEnvSecret7777")
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn serve");
    let guard = ChildGuard::new(child);

    let client = reqwest::Client::builder()
        .no_proxy()
        .timeout(Duration::from_secs(2))
        .build()
        .expect("client");
    let health_url = format!("http://127.0.0.1:{port}/health");

    let deadline = Instant::now() + Duration::from_secs(15);
    let resp = loop {
        match client.get(&health_url).send().await {
            Ok(resp) if resp.status().is_success() => break resp,
            _ if Instant::now() < deadline => {
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
            Err(err) => panic!("proxy never became healthy: {err}"),
            Ok(resp) => panic!("proxy returned unexpected status: {}", resp.status()),
        }
    };

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
}

#[tokio::test]
async fn doctor_passes_against_healthy_server_under_hostile_proxy_and_never_leaks_key() {
    let (port, handle) = spawn_mock_health("ok", env!("CARGO_PKG_VERSION"), PROTOCOL_VERSION).await;
    let temp = TempDir::new().expect("tempdir");
    let cfg_path = temp.path().join("config.json");
    let mut cfg = Config::default();
    cfg.gemini.api_key = Some("AIzaSyCfgSecretMustNotLeak1".to_string());
    cfg.save_secure(&cfg_path).expect("save_secure");

    for extra_args in [&["--json"][..], &[][..]] {
        let mut cmd = tokio::process::Command::new(BIN);
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
    let out = tokio::process::Command::new(BIN)
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

        let out = tokio::process::Command::new(BIN)
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
    let out = tokio::process::Command::new(BIN)
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
fn ui_mcp_stdio_initialize_emits_strict_json_rpc_only_on_stdout() {
    let temp = TempDir::new().expect("tempdir");
    let cfg_path = temp.path().join("config.json");
    let data_dir = temp.path().join("plugin-data");
    fs::create_dir(&data_dir).expect("mkdir data_dir");

    let mut cfg = Config::default();
    cfg.gemini.api_key = Some("AIzaSyUiMcpCfgSecret5555".to_string());
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
            assert_eq!(tools.len(), 4);
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
fn models_json_flag_emits_valid_custom_models_config_for_custom_port() {
    let temp = TempDir::new().expect("tempdir");
    let cfg_path = temp.path().join("config.json");
    let mut cfg = Config::default();
    cfg.gemini.default_model = "gemini-3.8-flash".to_string();
    cfg.gemini.api_key = Some("AIzaSyModelsSecret4444".to_string());
    cfg.save_secure(&cfg_path).expect("save_secure");

    let output = Command::new(BIN)
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
        "gemini-api://http://127.0.0.1:5252/models/gemini-4-hydrogen"
    );
    assert_eq!(
        models[1]["label"],
        "Antigravity Companion (gemini-4-hydrogen)"
    );
    assert_eq!(
        models[2]["modelName"],
        "gemini-api://http://127.0.0.1:5252/models/gemini-4-argon"
    );
    assert_eq!(models[2]["label"], "Antigravity Companion (gemini-4-argon)");
    assert_eq!(
        models[3]["modelName"],
        "gemini-api://http://127.0.0.1:5252/models/gemini-4-neon"
    );
    assert_eq!(models[3]["label"], "Antigravity Companion (gemini-4-neon)");
    assert_eq!(
        models[4]["modelName"],
        "gemini-api://http://127.0.0.1:5252/models/gemini-4-carbon"
    );
    assert_eq!(
        models[4]["label"],
        "Antigravity Companion (gemini-4-carbon)"
    );
    assert!(!stdout.contains("AIzaSyModelsSecret4444"));
    assert!(!stderr.contains("AIzaSyModelsSecret4444"));

    // Negative check: --port 0 must exit non-zero
    let zero_out = Command::new(BIN)
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
    let mut cfg = Config {
        listen_port: 5353,
        ..Config::default()
    };
    cfg.gemini.default_model = "gemini-3.0-pro".to_string();
    cfg.save_secure(&env_cfg).expect("save env_cfg");

    // AGY_COMPANION_CONFIG is honored when --config is omitted
    let out = Command::new(BIN)
        .args(["models", "--json"])
        .env("AGY_COMPANION_CONFIG", &env_cfg)
        .output()
        .expect("run models with env config");
    assert_eq!(out.status.code(), Some(0));
    let parsed: Value =
        serde_json::from_str(&String::from_utf8_lossy(&out.stdout)).expect("valid json");
    assert_eq!(
        parsed["customModelsConfig"]["customModels"][0]["modelName"],
        "gemini-api://http://127.0.0.1:5353/models/gemini-3.0-pro"
    );

    // Explicit --config overrides even an invalid AGY_COMPANION_CONFIG
    let cli_cfg = temp.path().join("cli_config.json");
    let cfg2 = Config {
        listen_port: 5454,
        ..Config::default()
    };
    cfg2.save_secure(&cli_cfg).expect("save cli_cfg");
    let out = Command::new(BIN)
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
    let out = Command::new(BIN)
        .args(["ui-mcp", "--data-dir=../escape", "--config"])
        .arg(&cli_cfg)
        .output()
        .expect("run ui-mcp invalid data-dir");
    assert_eq!(out.status.code(), Some(1));
    assert!(out.stdout.is_empty());
    assert!(!out.stderr.is_empty());

    // doctor --config ../escape.json --json still outputs valid JSON and exits 1
    let closed_port = free_loopback_port();
    let out = Command::new(BIN)
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
                    "api_key": canary,
                    "listen_port": closed_port
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
                    parsed["result"]["structuredContent"]["config"]["api_key_configured"],
                    true
                );
                assert_eq!(
                    parsed["result"]["structuredContent"]["config"]["api_key_hint"],
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
        assert_eq!(saved.gemini.api_key.as_deref(), Some(canary));
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

fn run_ui_mcp_bounded_with_calls(
    config_path: &std::path::Path,
    data_dir: &std::path::Path,
    calls: Vec<Value>,
    timeout: Duration,
) -> (Option<i32>, String, String, bool) {
    let mut child = Command::new(BIN)
        .arg("ui-mcp")
        .arg(format!("--data-dir={}", data_dir.display()))
        .arg("--config")
        .arg(config_path)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn ui-mcp");

    let stdin = child.stdin.take().expect("stdin");
    let stdout = child.stdout.take().expect("stdout");
    let stderr = child.stderr.take().expect("stderr");

    // Send initialize + requests on a helper thread so a hung FIFO open cannot block the test.
    let writer_thread = std::thread::spawn(move || {
        let mut stdin = stdin;
        let init_req = json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "initialize",
            "params": {
                "protocolVersion": "2025-03-26",
                "capabilities": {},
                "clientInfo": { "name": "companion-cli-test", "version": "0.1.0" }
            }
        });
        let initialized_notif = json!({
            "jsonrpc": "2.0",
            "method": "notifications/initialized"
        });
        let _ = writeln!(stdin, "{init_req}");
        let _ = writeln!(stdin, "{initialized_notif}");
        for req in calls {
            let _ = writeln!(stdin, "{req}");
        }
        let _ = stdin.flush();
    });

    let stdout_thread = std::thread::spawn(move || {
        let mut buf = String::new();
        let _ = std::io::Read::read_to_string(&mut BufReader::new(stdout), &mut buf);
        buf
    });
    let stderr_thread = std::thread::spawn(move || {
        let mut buf = String::new();
        let _ = std::io::Read::read_to_string(&mut BufReader::new(stderr), &mut buf);
        buf
    });

    let deadline = Instant::now() + timeout;
    let mut timed_out = false;
    let exit_code = loop {
        match child.try_wait().expect("try_wait ui-mcp") {
            Some(status) => break status.code(),
            None if Instant::now() < deadline => {
                std::thread::sleep(Duration::from_millis(20));
            }
            None => {
                timed_out = true;
                let _ = child.kill();
                let status = child.wait().expect("wait killed ui-mcp");
                break status.code();
            }
        }
    };

    let _ = writer_thread.join();
    let stdout_str = stdout_thread.join().expect("join stdout");
    let stderr_str = stderr_thread.join().expect("join stderr");
    (exit_code, stdout_str, stderr_str, timed_out)
}

fn run_ui_mcp_bounded(
    config_path: &std::path::Path,
    data_dir: &std::path::Path,
    timeout: Duration,
) -> (Option<i32>, String, String, bool) {
    run_ui_mcp_bounded_with_calls(
        config_path,
        data_dir,
        vec![json!({
            "jsonrpc": "2.0",
            "id": 2,
            "method": "tools/call",
            "params": { "name": "get_settings", "arguments": {} }
        })],
        timeout,
    )
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
                "arguments": { "listen_port": closed_port }
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

    let cases = [
        (
            "dangling/",
            PathBuf::from(format!("{}/", dangling_link.display())),
        ),
        (
            "dangling//",
            PathBuf::from(format!("{}//", dangling_link.display())),
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
            "sym_file/.",
            PathBuf::from(format!("{}/.", file_link.display())),
        ),
        (
            "sym_dir/",
            PathBuf::from(format!("{}/", dir_link.display())),
        ),
        (
            "sym_dir/.",
            PathBuf::from(format!("{}/.", dir_link.display())),
        ),
    ];

    for (label, path) in &cases {
        for use_env in [false, true] {
            for json_mode in [true, false] {
                let mut cmd = tokio::process::Command::new(BIN);
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
