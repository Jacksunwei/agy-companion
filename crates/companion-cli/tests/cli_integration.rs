#![forbid(unsafe_code)]

use std::fs;
use std::io::{BufRead, BufReader, Write};
use std::net::{Ipv4Addr, TcpListener as StdTcpListener};
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

    fn try_wait(&mut self) -> std::io::Result<Option<std::process::ExitStatus>> {
        self.child.as_mut().expect("child present").try_wait()
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

struct DetachedPidFileGuard {
    pid_path: std::path::PathBuf,
}

impl DetachedPidFileGuard {
    fn new(pid_path: std::path::PathBuf) -> Self {
        Self { pid_path }
    }
}

impl Drop for DetachedPidFileGuard {
    fn drop(&mut self) {
        let Ok(Some(info)) = companion_cli::daemon::read_proxy_pid(&self.pid_path) else {
            return;
        };
        if info.pid <= 1 || info.pid == std::process::id() {
            return;
        }
        #[cfg(unix)]
        {
            let pid_str = info.pid.to_string();
            let ps_out = Command::new("ps")
                .args(["-p", &pid_str, "-o", "command="])
                .output();
            let is_companion = ps_out
                .ok()
                .filter(|o| o.status.success())
                .map(|o| String::from_utf8_lossy(&o.stdout).to_string())
                .is_some_and(|cmdline| cmdline.contains("agy-companion"));
            if !is_companion {
                return;
            }
            let _ = Command::new("kill").args(["-TERM", &pid_str]).status();
            let deadline = Instant::now() + Duration::from_secs(2);
            while Instant::now() < deadline {
                if !self.pid_path.exists() {
                    return;
                }
                let alive = Command::new("kill")
                    .args(["-0", &pid_str])
                    .stdout(Stdio::null())
                    .stderr(Stdio::null())
                    .status()
                    .is_ok_and(|s| s.success());
                if !alive {
                    return;
                }
                std::thread::sleep(Duration::from_millis(50));
            }
            let _ = Command::new("kill").args(["-KILL", &pid_str]).status();
        }
        #[cfg(windows)]
        {
            let _ = Command::new("taskkill")
                .args(["/PID", &info.pid.to_string(), "/T", "/F"])
                .status();
        }
    }
}

fn isolated_cli_command(home: &std::path::Path) -> Command {
    let mut cmd = Command::new(BIN);
    for var in [
        "CLOUD_CODE_URL",
        "GOOGLE_GEMINI_BASE_URL",
        "GEMINI_API_KEY",
        "GOOGLE_API_KEY",
        "AGY_COMPANION_CONFIG",
        "AGY_COMPANION_HOOK_ACTIVE",
        "AGY_COMPANION_INJECTED_URL",
        "AGY_COMPANION_ALLOW_REMOTE_SSH",
        "JETSKI_MCP_ENV_RESTORE_PREFIX",
        "JETSKI_MCP_ENV_RESTORE_VARS",
        "ANTIGRAVITY_RESOLVING_ENVIRONMENT",
        "SSH_CONNECTION",
        "SSH_CLIENT",
        "ZDOTDIR",
        "SHELL",
    ] {
        cmd.env_remove(var);
    }
    cmd.env("HOME", home)
        .env("XDG_CONFIG_HOME", home.join("xdg-config"))
        .env("XDG_DATA_HOME", home.join("xdg-data"));
    cmd
}

const SERVE_START_MAX_ATTEMPTS: usize = 5;

fn free_loopback_port() -> u16 {
    use std::collections::HashSet;
    use std::sync::{Mutex, OnceLock};

    // Allocate outside both macOS/Windows (49152..=65535) and Linux (32768..=60999)
    // OS ephemeral port ranges so concurrent `127.0.0.1:0` listeners and outbound
    // client sockets in the same test binary never collide with ports reserved for
    // `agy-companion serve` subprocesses. Assign each PID a disjoint 50-port block
    // (240 * 50 = 12_000 <= RANGE_SPAN) so consecutive parallel test processes do
    // not overlap across their ~12 allocated ports.
    const RANGE_START: u16 = 20_000;
    const RANGE_END: u16 = 32_000;
    const RANGE_SPAN: u16 = RANGE_END - RANGE_START + 1;
    const BLOCK_STRIDE: u32 = 50;

    struct PortAllocator {
        cursor: u16,
        claimed: HashSet<u16>,
    }

    static ALLOCATOR: OnceLock<Mutex<PortAllocator>> = OnceLock::new();
    let allocator = ALLOCATOR.get_or_init(|| {
        let initial_offset = u16::try_from((std::process::id() % 240) * BLOCK_STRIDE).unwrap_or(0);
        Mutex::new(PortAllocator {
            cursor: initial_offset,
            claimed: HashSet::new(),
        })
    });

    let mut state = allocator.lock().expect("lock port allocator");
    for _ in 0..RANGE_SPAN {
        let candidate = RANGE_START + state.cursor;
        state.cursor = (state.cursor + 1) % RANGE_SPAN;
        if state.claimed.contains(&candidate) {
            continue;
        }
        if let Ok(listener) = StdTcpListener::bind((Ipv4Addr::LOCALHOST, candidate)) {
            drop(listener);
            state.claimed.insert(candidate);
            return candidate;
        }
    }

    panic!(
        "unable to find an available loopback port in {RANGE_START}..={RANGE_END} (claimed={})",
        state.claimed.len()
    );
}

fn is_addr_in_use_stderr(stderr: &str) -> bool {
    let lower = stderr.to_ascii_lowercase();
    lower.contains("address already in use")
        || lower.contains("addrinuse")
        || lower.contains("only one usage of each socket address")
}

fn proxy_pid_matches_child(
    pid_path: &std::path::Path,
    expected_pid: u32,
    expected_port: u16,
) -> bool {
    let Ok(raw) = fs::read_to_string(pid_path) else {
        return false;
    };
    let Ok(info) = serde_json::from_str::<companion_cli::daemon::ProxyPidInfo>(&raw) else {
        return false;
    };
    info.pid == expected_pid && info.port == expected_port
}

async fn spawn_serve_with_port_picker<P, F>(
    client: &reqwest::Client,
    cfg_path: &std::path::Path,
    mut pick_port: P,
    mut build_cmd: F,
) -> (ChildGuard, u16, reqwest::Response)
where
    P: FnMut(usize) -> u16,
    F: FnMut(u16) -> Command,
{
    let mut last_addr_in_use = String::new();
    for attempt in 1..=SERVE_START_MAX_ATTEMPTS {
        let port = pick_port(attempt);
        let mut cfg = Config::load_or_default(cfg_path).expect("load temp config");
        if cfg.listen_port != port {
            cfg.listen_port = port;
            cfg.save_secure(cfg_path)
                .expect("update temp config listen_port");
        }
        let pid_path = companion_cli::daemon::resolve_runtime_data_dir(&cfg)
            .expect("resolve runtime data dir")
            .join(companion_cli::daemon::PROXY_PID_FILENAME);

        let child = build_cmd(port)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("spawn serve");
        let mut guard = ChildGuard::new(child);

        let health_url = format!("http://127.0.0.1:{port}/health");
        let deadline = Instant::now() + Duration::from_secs(15);

        loop {
            if let Some(status) = guard.try_wait().expect("try_wait serve") {
                let output = tokio::task::spawn_blocking(move || guard.wait_with_output())
                    .await
                    .expect("join wait_with_output")
                    .expect("wait_with_output");
                let stderr = String::from_utf8_lossy(&output.stderr).into_owned();
                let stdout = String::from_utf8_lossy(&output.stdout).into_owned();
                if is_addr_in_use_stderr(&stderr) && attempt < SERVE_START_MAX_ATTEMPTS {
                    last_addr_in_use = stderr;
                    break;
                }
                panic!(
                    "serve exited prematurely on port {port} (attempt {attempt}/{SERVE_START_MAX_ATTEMPTS}, status={status}): stdout={stdout:?}, stderr={stderr}"
                );
            }

            let last_err = match client.get(&health_url).send().await {
                Ok(resp) if resp.status().is_success() => {
                    if proxy_pid_matches_child(&pid_path, guard.id(), port) {
                        if let Some(status) = guard.try_wait().expect("try_wait serve after health")
                        {
                            let output =
                                tokio::task::spawn_blocking(move || guard.wait_with_output())
                                    .await
                                    .expect("join wait_with_output")
                                    .expect("wait_with_output");
                            let stderr = String::from_utf8_lossy(&output.stderr).into_owned();
                            if is_addr_in_use_stderr(&stderr) && attempt < SERVE_START_MAX_ATTEMPTS
                            {
                                last_addr_in_use = stderr;
                                break;
                            }
                            panic!(
                                "serve exited prematurely on port {port} (attempt {attempt}/{SERVE_START_MAX_ATTEMPTS}, status={status}): stderr={stderr}"
                            );
                        }
                        return (guard, port, resp);
                    }
                    if let Some(status) = guard
                        .try_wait()
                        .expect("try_wait serve after foreign health")
                    {
                        let output = tokio::task::spawn_blocking(move || guard.wait_with_output())
                            .await
                            .expect("join wait_with_output")
                            .expect("wait_with_output");
                        let stderr = String::from_utf8_lossy(&output.stderr).into_owned();
                        if is_addr_in_use_stderr(&stderr) && attempt < SERVE_START_MAX_ATTEMPTS {
                            last_addr_in_use = stderr;
                            break;
                        }
                        panic!(
                            "serve exited prematurely on port {port} (attempt {attempt}/{SERVE_START_MAX_ATTEMPTS}, status={status}): stderr={stderr}"
                        );
                    }
                    format!(
                        "health on port {port} not yet owned by child pid {} in {}",
                        guard.id(),
                        pid_path.display()
                    )
                }
                Ok(resp) => format!("unexpected status {}", resp.status()),
                Err(err) => err.to_string(),
            };

            if Instant::now() >= deadline {
                if let Some(status) = guard.try_wait().expect("try_wait serve at deadline") {
                    let output = tokio::task::spawn_blocking(move || guard.wait_with_output())
                        .await
                        .expect("join wait_with_output")
                        .expect("wait_with_output");
                    let stderr = String::from_utf8_lossy(&output.stderr).into_owned();
                    if is_addr_in_use_stderr(&stderr) && attempt < SERVE_START_MAX_ATTEMPTS {
                        last_addr_in_use = stderr;
                        break;
                    }
                    panic!(
                        "serve exited before becoming healthy on port {port} (attempt {attempt}/{SERVE_START_MAX_ATTEMPTS}, status={status}): stderr={stderr}"
                    );
                }
                panic!(
                    "proxy never became healthy on port {port} (attempt {attempt}/{SERVE_START_MAX_ATTEMPTS}, last_err={last_err})"
                );
            }

            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    }

    panic!(
        "serve failed to bind after {SERVE_START_MAX_ATTEMPTS} attempts due to AddrInUse; last stderr={last_addr_in_use}"
    );
}

async fn spawn_serve_and_wait_healthy<F>(
    client: &reqwest::Client,
    cfg_path: &std::path::Path,
    build_cmd: F,
) -> (ChildGuard, u16, reqwest::Response)
where
    F: FnMut(u16) -> Command,
{
    spawn_serve_with_port_picker(client, cfg_path, |_| free_loopback_port(), build_cmd).await
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

    let client = reqwest::Client::builder()
        .no_proxy()
        .timeout(Duration::from_secs(2))
        .build()
        .expect("client");

    let (guard, _port, resp) = spawn_serve_and_wait_healthy(&client, &cfg_path, |port| {
        let mut cmd = Command::new(BIN);
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
            assert_eq!(tools.len(), 5);
            assert!(
                tools
                    .iter()
                    .any(|t| t.get("name") == Some(&json!("set_launcher_integration")))
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

#[tokio::test]
async fn serve_writes_0600_proxy_pid_and_proxy_log_in_isolated_dir_and_cleans_pid_on_sigterm() {
    let temp = TempDir::new().expect("tempdir");
    let cfg_path = temp.path().join("config.json");
    let usage_dir = temp.path().join("usage");

    let mut cfg = Config::default();
    cfg.gemini.api_key = Some("AIzaSyPidLogSecret12345".to_string());
    cfg.usage_dir = Some(usage_dir);
    cfg.save_secure(&cfg_path).expect("save_secure");

    let client = reqwest::Client::builder()
        .no_proxy()
        .timeout(Duration::from_secs(2))
        .build()
        .expect("client");
    let (guard, port, _resp) = spawn_serve_and_wait_healthy(&client, &cfg_path, |port| {
        let mut cmd = Command::new(BIN);
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

#[tokio::test]
async fn serve_idle_shutdown_waits_for_in_flight_stream_then_exits_zero() {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    // Mock upstream SSE server that holds the stream open for 1.6s (> 1s idle timeout).
    let upstream_listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind mock upstream");
    let upstream_port = upstream_listener.local_addr().expect("addr").port();
    let upstream_handle = tokio::spawn(async move {
        if let Ok((mut stream, _)) = upstream_listener.accept().await {
            let mut req_buf = [0u8; 4096];
            let _ = stream.read(&mut req_buf).await;
            let _ = stream
                .write_all(
                    b"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nTransfer-Encoding: chunked\r\n\r\n",
                )
                .await;
            let chunk1 =
                b"data: {\"candidates\":[{\"content\":{\"parts\":[{\"text\":\"part1\"}]}}]}\n\n";
            let _ = stream
                .write_all(format!("{:x}\r\n", chunk1.len()).as_bytes())
                .await;
            let _ = stream.write_all(chunk1).await;
            let _ = stream.write_all(b"\r\n").await;
            let _ = stream.flush().await;

            tokio::time::sleep(Duration::from_millis(1600)).await;

            let chunk2 = b"data: {\"candidates\":[{\"content\":{\"parts\":[{\"text\":\"part2\"}]}}],\"usageMetadata\":{\"promptTokenCount\":10,\"candidatesTokenCount\":5}}\n\n";
            let _ = stream
                .write_all(format!("{:x}\r\n", chunk2.len()).as_bytes())
                .await;
            let _ = stream.write_all(chunk2).await;
            let _ = stream.write_all(b"\r\n0\r\n\r\n").await;
            let _ = stream.flush().await;
        }
    });

    let temp = TempDir::new().expect("tempdir");
    let cfg_path = temp.path().join("config.json");
    let usage_dir = temp.path().join("usage");

    let mut cfg = Config::default();
    cfg.gemini.base_url = format!("http://127.0.0.1:{upstream_port}");
    cfg.gemini.api_key = Some("AIzaSyIdleStreamKey1234".to_string());
    cfg.usage_dir = Some(usage_dir);
    cfg.save_secure(&cfg_path).expect("save_secure");

    let client = reqwest::Client::builder()
        .no_proxy()
        .timeout(Duration::from_secs(10))
        .build()
        .expect("client");
    let (guard, port, _resp) = spawn_serve_and_wait_healthy(&client, &cfg_path, |port| {
        let mut cmd = Command::new(BIN);
        cmd.args(["serve", "--port", &port.to_string(), "--config"])
            .arg(&cfg_path)
            .args(["--idle-shutdown-secs", "1"]);
        cmd
    })
    .await;

    let pid_path = temp.path().join("proxy.pid");
    assert!(pid_path.is_file());

    // Start slow SSE stream; proxy must stay alive through the entire 1.6s stream despite 1s idle timeout.
    let stream_started = Instant::now();
    let stream_resp = client
        .post(format!(
            "http://127.0.0.1:{port}/v1beta/models/gemini-4-hydrogen:streamGenerateContent?alt=sse"
        ))
        .header("content-type", "application/json")
        .body(r#"{"contents":[{"parts":[{"text":"hi"}]}]}"#)
        .send()
        .await
        .expect("stream request");
    assert_eq!(stream_resp.status(), reqwest::StatusCode::OK);
    let stream_body = stream_resp.text().await.expect("read full stream");
    let stream_elapsed = stream_started.elapsed();
    assert!(
        stream_elapsed >= Duration::from_millis(1400),
        "stream ended too early ({stream_elapsed:?}); idle shutdown must not interrupt active stream"
    );
    assert!(stream_body.contains("part1"));
    assert!(stream_body.contains("part2"));

    // Now that zero requests are in flight, serve must exit cleanly with code 0 after ~1s idle.
    let output = tokio::time::timeout(
        Duration::from_secs(5),
        tokio::task::spawn_blocking(move || guard.wait_with_output()),
    )
    .await
    .expect("serve did not exit after idle shutdown timeout")
    .expect("join wait")
    .expect("wait_with_output");
    assert_eq!(
        output.status.code(),
        Some(0),
        "idle shutdown must exit 0; stderr={}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        !pid_path.exists(),
        "proxy.pid must be removed on clean idle shutdown"
    );

    upstream_handle.abort();
}

#[cfg(unix)]
#[tokio::test]
async fn serve_adopt_running_does_not_clobber_pid_and_takes_over_within_1500ms() {
    let temp = TempDir::new().expect("tempdir");
    let cfg_path = temp.path().join("config.json");
    let usage_dir = temp.path().join("usage");

    let cfg = Config {
        usage_dir: Some(usage_dir),
        ..Default::default()
    };
    cfg.save_secure(&cfg_path).expect("save_secure");

    let client = reqwest::Client::builder()
        .no_proxy()
        .timeout(Duration::from_secs(2))
        .build()
        .expect("client");
    let (primary_guard, port, _resp) = spawn_serve_and_wait_healthy(&client, &cfg_path, |port| {
        let mut cmd = Command::new(BIN);
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
    let standby_child = Command::new(BIN)
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
    tokio::time::sleep(Duration::from_millis(400)).await;
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
        tokio::time::sleep(Duration::from_millis(50)).await;
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
async fn doctor_cli_quarantines_placeholder_gemini_api_key() {
    let (port, handle) = spawn_mock_health("ok", env!("CARGO_PKG_VERSION"), PROTOCOL_VERSION).await;
    let temp = TempDir::new().expect("tempdir");
    let cfg_path = temp.path().join("config.json");
    Config::default().save_secure(&cfg_path).expect("save");

    let output = tokio::process::Command::new(BIN)
        .args(["doctor", "--config"])
        .arg(&cfg_path)
        .args(["--port", &port.to_string(), "--json"])
        .env("GEMINI_API_KEY", companion_core::PROXY_PLACEHOLDER_API_KEY)
        .output()
        .await
        .expect("run doctor");
    handle.abort();

    assert_eq!(output.status.code(), Some(0));
    let parsed: Value =
        serde_json::from_str(&String::from_utf8_lossy(&output.stdout)).expect("parse doctor JSON");
    assert_eq!(parsed["ok"], true);
    assert_eq!(parsed["env"]["gemini_api_key_set"], false);
    let key_check = parsed["checks"]
        .as_array()
        .expect("checks")
        .iter()
        .find(|c| c["name"] == "env_gemini_api_key")
        .expect("env_gemini_api_key check");
    assert_eq!(key_check["status"], "warn");
}

#[tokio::test]
async fn spawn_serve_rejects_foreign_health_and_retries_fresh_port() {
    let (foreign_port, foreign_handle) =
        spawn_mock_health("ok", env!("CARGO_PKG_VERSION"), PROTOCOL_VERSION).await;

    let temp = TempDir::new().expect("tempdir");
    let cfg_path = temp.path().join("config.json");
    let usage_dir = temp.path().join("usage");
    let cfg = Config {
        usage_dir: Some(usage_dir),
        ..Default::default()
    };
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

    let client = reqwest::Client::builder()
        .no_proxy()
        .timeout(Duration::from_secs(2))
        .build()
        .expect("client");

    let mut attempts_seen = 0usize;
    let (guard, actual_port, resp) = spawn_serve_with_port_picker(
        &client,
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
            let mut cmd = Command::new(BIN);
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

#[tokio::test]
async fn hook_env_cold_starts_detached_daemon_and_fails_open_when_plugin_missing_or_port_occupied()
{
    let temp = TempDir::new().expect("tempdir");
    let home = temp.path().join("home");
    fs::create_dir_all(&home).expect("mkdir home");

    // 1. Missing plugin -> fail-open 0 stdout, exit 0
    let empty_plugins = temp.path().join("empty-plugins");
    fs::create_dir_all(&empty_plugins).expect("mkdir");
    let missing_out = isolated_cli_command(&home)
        .args([
            "hook-env",
            "--ensure-running",
            "--print-port",
            "--plugins-dir",
        ])
        .arg(&empty_plugins)
        .output()
        .expect("run hook-env missing plugin");
    assert_eq!(missing_out.status.code(), Some(0));
    assert!(missing_out.stdout.is_empty());

    // 2. Cold-start detached proxy daemon + warm hit reuses same PID
    let plugins_dir = temp.path().join("plugins");
    let plugin_root = plugins_dir.join("agy-companion");
    fs::create_dir_all(&plugin_root).expect("mkdir plugin");
    fs::write(
        plugin_root.join("plugin.json"),
        r#"{"name":"agy-companion"}"#,
    )
    .expect("plugin.json");

    let port = free_loopback_port();
    let cfg_path = temp.path().join("config.json");
    let usage_dir = temp.path().join("runtime").join("usage");
    let cfg = Config {
        listen_port: port,
        usage_dir: Some(usage_dir),
        ..Default::default()
    };
    cfg.save_secure(&cfg_path).expect("save config");

    let pid_path = temp.path().join("runtime").join("proxy.pid");
    let log_path = temp.path().join("runtime").join("proxy.log");
    let _pid_guard = DetachedPidFileGuard::new(pid_path.clone());

    let cold = isolated_cli_command(&home)
        .args([
            "hook-env",
            "--ensure-running",
            "--print-port",
            "--port",
            &port.to_string(),
            "--config",
        ])
        .arg(&cfg_path)
        .arg("--plugins-dir")
        .arg(&plugins_dir)
        .output()
        .expect("run cold hook-env");
    assert_eq!(cold.status.code(), Some(0));
    assert_eq!(
        String::from_utf8(cold.stdout).expect("utf8"),
        format!("{port}\n"),
        "stderr={}",
        String::from_utf8_lossy(&cold.stderr)
    );

    let pid_info = companion_cli::daemon::read_proxy_pid(&pid_path)
        .expect("read pid")
        .expect("pid file present");
    assert_eq!(pid_info.port, port);
    assert!(log_path.is_file());
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(
            fs::metadata(&pid_path).unwrap().permissions().mode() & 0o777,
            0o600
        );
        assert_eq!(
            fs::metadata(&log_path).unwrap().permissions().mode() & 0o777,
            0o600
        );
    }

    // Warm re-run prints same port and keeps the exact same daemon PID
    let warm = isolated_cli_command(&home)
        .args([
            "hook-env",
            "--ensure-running",
            "--print-port",
            "--port",
            &port.to_string(),
            "--config",
        ])
        .arg(&cfg_path)
        .arg("--plugins-dir")
        .arg(&plugins_dir)
        .output()
        .expect("run warm hook-env");
    assert_eq!(warm.status.code(), Some(0));
    assert_eq!(
        String::from_utf8(warm.stdout).expect("utf8"),
        format!("{port}\n")
    );
    let pid_info_after = companion_cli::daemon::read_proxy_pid(&pid_path)
        .expect("read pid")
        .expect("pid still present");
    assert_eq!(pid_info_after.pid, pid_info.pid);

    // 3. Non-companion occupant on another port -> fail-open 0 stdout, exit 0
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let foreign_port = listener.local_addr().expect("addr").port();
    let foreign_handle = tokio::spawn(async move {
        let router = Router::new().route(
            "/health",
            get(|| async { axum::http::StatusCode::NOT_FOUND }),
        );
        let _ = axum::serve(listener, router).await;
    });
    let occupied = isolated_cli_command(&home)
        .args([
            "hook-env",
            "--ensure-running",
            "--print-port",
            "--port",
            &foreign_port.to_string(),
            "--config",
        ])
        .arg(&cfg_path)
        .arg("--plugins-dir")
        .arg(&plugins_dir)
        .output()
        .expect("run occupied hook-env");
    foreign_handle.abort();
    assert_eq!(occupied.status.code(), Some(0));
    assert!(occupied.stdout.is_empty());
}

#[test]
fn integration_cli_lifecycle_handles_shells_symlinks_and_corrupted_markers() {
    let temp = TempDir::new().expect("tempdir");
    let home = temp.path().join("home");
    fs::create_dir_all(&home).expect("mkdir home");

    // 1. Zsh enable -> status -> disable
    let en = isolated_cli_command(&home)
        .args(["integration", "enable", "--shell", "zsh", "--json"])
        .output()
        .expect("enable zsh");
    assert_eq!(en.status.code(), Some(0));
    let en_json: Value = serde_json::from_slice(&en.stdout).expect("json");
    if cfg!(windows) {
        assert_eq!(en_json["supported"], false);
        return;
    }
    assert_eq!(en_json["enabled"], true);
    assert_eq!(en_json["manual_required"], false);
    assert!(home.join(".zlogin").is_file());
    let env_sh = home.join("xdg-data/agy-companion/env.sh");
    assert!(env_sh.is_file());

    let st = isolated_cli_command(&home)
        .args(["integration", "status", "--shell", "zsh", "--json"])
        .output()
        .expect("status zsh");
    assert_eq!(st.status.code(), Some(0));
    let st_json: Value = serde_json::from_slice(&st.stdout).expect("json");
    assert_eq!(st_json["enabled"], true);

    let dis = isolated_cli_command(&home)
        .args(["integration", "disable", "--shell", "zsh", "--json"])
        .output()
        .expect("disable zsh");
    assert_eq!(dis.status.code(), Some(0));
    let dis_json: Value = serde_json::from_slice(&dis.stdout).expect("json");
    assert_eq!(dis_json["enabled"], false);
    assert!(!home.join(".zlogin").exists());
    assert!(!env_sh.exists());

    // 2. Bash & Fish targets
    let bash_en = isolated_cli_command(&home)
        .args(["integration", "enable", "--shell", "bash", "--json"])
        .output()
        .expect("enable bash");
    assert_eq!(bash_en.status.code(), Some(0));
    let bash_json: Value = serde_json::from_slice(&bash_en.stdout).expect("json");
    assert_eq!(
        bash_json["rc_path"],
        home.join(".profile").to_str().unwrap()
    );

    let fish_en = isolated_cli_command(&home)
        .args(["integration", "enable", "--shell", "fish", "--json"])
        .output()
        .expect("enable fish");
    assert_eq!(fish_en.status.code(), Some(0));
    let fish_json: Value = serde_json::from_slice(&fish_en.stdout).expect("json");
    assert_eq!(
        fish_json["rc_path"],
        home.join("xdg-config/fish/conf.d/agy-companion.fish")
            .to_str()
            .unwrap()
    );

    // 3. Symlinked RC without vs with --follow-rc-symlink
    #[cfg(unix)]
    {
        use std::os::unix::fs::symlink;
        let target = temp.path().join("managed-zlogin");
        fs::write(&target, "# stow\n").expect("write target");
        let sym = home.join(".zlogin");
        symlink(&target, &sym).expect("symlink");

        let manual = isolated_cli_command(&home)
            .args(["integration", "enable", "--shell", "zsh", "--json"])
            .output()
            .expect("enable symlink");
        assert_eq!(manual.status.code(), Some(0));
        let m_json: Value = serde_json::from_slice(&manual.stdout).expect("json");
        assert_eq!(m_json["manual_required"], true);
        assert_eq!(m_json["enabled"], false);
        assert_eq!(fs::read_to_string(&target).unwrap(), "# stow\n");

        let follow = isolated_cli_command(&home)
            .args([
                "integration",
                "enable",
                "--shell",
                "zsh",
                "--follow-rc-symlink",
                "--json",
            ])
            .output()
            .expect("enable follow");
        assert_eq!(follow.status.code(), Some(0));
        let f_json: Value = serde_json::from_slice(&follow.stdout).expect("json");
        assert_eq!(f_json["enabled"], true);
        assert!(
            fs::read_to_string(&target)
                .unwrap()
                .contains(companion_core::HOOK_BEGIN_MARKER)
        );
    }

    // 4. Unbalanced marker fails closed with exit 1, 0 stdout, line number on stderr
    let broken_rc = temp.path().join("broken.rc");
    fs::write(
        &broken_rc,
        format!("line1\n{}\nline3\n", companion_core::HOOK_BEGIN_MARKER),
    )
    .expect("write broken");
    let broken = isolated_cli_command(&home)
        .args(["integration", "enable", "--rc-file"])
        .arg(&broken_rc)
        .output()
        .expect("run broken");
    assert_eq!(broken.status.code(), Some(1));
    assert!(broken.stdout.is_empty());
    let stderr_str = String::from_utf8_lossy(&broken.stderr);
    assert!(stderr_str.contains("line 2"), "stderr={stderr_str}");

    // 5. Path traversal fails with exit 1, 0 stdout
    let trav = isolated_cli_command(&home)
        .args(["integration", "enable", "--rc-file", "/tmp/../escape"])
        .output()
        .expect("run traversal");
    assert_eq!(trav.status.code(), Some(1));
    assert!(trav.stdout.is_empty());
}

#[cfg(unix)]
#[tokio::test]
async fn launch_subcommand_spawns_detached_daemon_and_sets_restore_env_preserving_real_keys() {
    use std::os::unix::fs::PermissionsExt;

    let temp = TempDir::new().expect("tempdir");
    let home = temp.path().join("home");
    fs::create_dir_all(&home).expect("mkdir home");

    // 1. --print-env with real GEMINI_API_KEY + GOOGLE_API_KEY + custom restore prefix/vars
    let print_out = isolated_cli_command(&home)
        .args([
            "launch",
            "--port",
            "4321",
            "--print-env",
            "--app",
            "/opt/agy",
        ])
        .env("GEMINI_API_KEY", "real-user-gemini-key")
        .env("GOOGLE_API_KEY", "real-user-google-key")
        .env("JETSKI_MCP_ENV_RESTORE_PREFIX", "MY_")
        .env("JETSKI_MCP_ENV_RESTORE_VARS", "EXISTING")
        .output()
        .expect("launch --print-env");
    assert_eq!(print_out.status.code(), Some(0));
    let printed = String::from_utf8(print_out.stdout).expect("utf8");
    assert!(printed.contains("JETSKI_MCP_ENV_RESTORE_PREFIX=\"MY_\""));
    assert!(printed.contains(
        "JETSKI_MCP_ENV_RESTORE_VARS=\"EXISTING,CLOUD_CODE_URL,GOOGLE_GEMINI_BASE_URL\""
    ));
    assert!(!printed.contains("GEMINI_API_KEY"));
    assert!(!printed.contains("real-user-gemini-key"));
    assert!(!printed.contains("real-user-google-key"));

    // 2. Live launch with dummy app dumping env to file in TempDir
    let port = free_loopback_port();
    let cfg_path = temp.path().join("config.json");
    let usage_dir = temp.path().join("runtime").join("usage");
    let cfg = Config {
        listen_port: port,
        usage_dir: Some(usage_dir),
        ..Default::default()
    };
    cfg.save_secure(&cfg_path).expect("save config");
    let pid_path = temp.path().join("runtime").join("proxy.pid");
    let _pid_guard = DetachedPidFileGuard::new(pid_path.clone());

    let env_dump = temp.path().join("app-env.txt");
    let env_dump_tmp = temp.path().join("app-env.txt.tmp");
    let app_script = temp.path().join("fake-antigravity.sh");
    fs::write(
        &app_script,
        format!(
            "#!/bin/sh\nenv > '{}' && mv '{}' '{}'\n",
            env_dump_tmp.display(),
            env_dump_tmp.display(),
            env_dump.display()
        ),
    )
    .expect("write script");
    fs::set_permissions(&app_script, fs::Permissions::from_mode(0o755)).expect("chmod");

    let launched = isolated_cli_command(&home)
        .args(["launch", "--port", &port.to_string(), "--config"])
        .arg(&cfg_path)
        .arg("--app")
        .arg(&app_script)
        .env("GOOGLE_API_KEY", "keep-google-secret")
        .output()
        .expect("run launch");
    assert_eq!(
        launched.status.code(),
        Some(0),
        "stderr={}",
        String::from_utf8_lossy(&launched.stderr)
    );
    assert!(pid_path.is_file());

    // Wait briefly for the spawned script to finish writing app-env.txt
    let deadline = Instant::now() + Duration::from_secs(3);
    while Instant::now() < deadline && !env_dump.is_file() {
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    let dumped = fs::read_to_string(&env_dump).expect("read dumped env");
    assert!(dumped.contains(&format!("CLOUD_CODE_URL=http://127.0.0.1:{port}")));
    assert!(dumped.contains(&format!("GOOGLE_GEMINI_BASE_URL=http://127.0.0.1:{port}")));
    assert!(dumped.contains("GEMINI_API_KEY=agy-companion-local"));
    assert!(dumped.contains("GOOGLE_API_KEY=keep-google-secret"));
    assert!(dumped.contains("JETSKI_MCP_ENV_RESTORE_PREFIX=AGY_COMPANION_ORIG_"));
    assert!(dumped.contains(
        "JETSKI_MCP_ENV_RESTORE_VARS=CLOUD_CODE_URL,GOOGLE_GEMINI_BASE_URL,GEMINI_API_KEY"
    ));
}
