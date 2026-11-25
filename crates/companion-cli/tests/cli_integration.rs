#![forbid(unsafe_code)]

use std::fs;
use std::io::{BufRead, BufReader, Write};
use std::net::{Ipv4Addr, TcpListener as StdTcpListener};
use std::num::NonZeroU16;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use axum::Router;
use axum::routing::get;
use companion_core::{ApiKey, Config};
use companion_proxy::{HealthResponse, PROTOCOL_VERSION};
use serde_json::{Value, json};
use tempfile::TempDir;
use tokio::net::TcpListener;

const BIN: &str = env!("CARGO_BIN_EXE_agy-companion");

fn config_with_port(port: u16) -> Config {
    let mut cfg = Config::default();
    cfg.proxy.port = NonZeroU16::new(port).expect("non-zero port");
    cfg
}

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
        if cfg.proxy.port.get() != port {
            cfg.proxy.port = NonZeroU16::new(port).expect("non-zero port");
            cfg.save_secure(cfg_path)
                .expect("update temp config proxy.port");
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

    // (a) proxy.host: 0.0.0.0
    let bad_host = temp.path().join("bad_host.json");
    fs::write(
        &bad_host,
        r#"{"schema_version":1,"proxy":{"host":"0.0.0.0","port":4141},"providers":{"gemini":{"base_url":"https://generativelanguage.googleapis.com","api_key":"AIzaSyServeBadHostSecret111","default_model":"gemini-4-hydrogen","timeout_secs":900}}}"#,
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
        r#"{"schema_version":1,"proxy":{"host":"127.0.0.1","port":4141},"providers":{"gemini":{"base_url":"http://evil.com","api_key":"AIzaSyServeBadUrlSecret222","default_model":"gemini-4-hydrogen","timeout_secs":900}}}"#,
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
    cfg.providers.gemini.api_key = Some(ApiKey::parse_strict("AIzaSyServeLiveSecret8888").unwrap());
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
    cfg.providers.gemini.api_key =
        Some(ApiKey::parse_strict("AIzaSyCfgSecretMustNotLeak1").unwrap());
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
    cfg.providers.gemini.default_model = "gemini-3.8-flash".to_string();
    cfg.providers.gemini.api_key = Some(ApiKey::parse_strict("AIzaSyModelsSecret4444").unwrap());
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
    let mut cfg = config_with_port(5353);
    cfg.providers.gemini.default_model = "gemini-3.9-pro".to_string();
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
        "gemini-api://http://127.0.0.1:5353/models/gemini-3.9-pro"
    );

    // Explicit --config overrides even an invalid AGY_COMPANION_CONFIG
    let cli_cfg = temp.path().join("cli_config.json");
    let cfg2 = config_with_port(5454);
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
    cfg.providers.gemini.api_key = Some(ApiKey::parse_strict("AIzaSyPidLogSecret12345").unwrap());
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
    cfg.providers.gemini.base_url = format!("http://127.0.0.1:{upstream_port}");
    cfg.providers.gemini.api_key = Some(ApiKey::parse_strict("AIzaSyIdleStreamKey1234").unwrap());
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
    let mut cfg = config_with_port(port);
    cfg.usage_dir = Some(usage_dir);
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
        assert_eq!(en_json["state"], "unsupported");
        return;
    }
    assert_eq!(en_json["state"], "enabled");
    assert_eq!(en_json["enabled"], true);
    assert!(en_json.get("supported").is_none());
    assert!(en_json.get("manual_required").is_none());
    assert!(home.join(".zlogin").is_file());
    let env_sh = home.join("xdg-data/agy-companion/env.sh");
    assert!(env_sh.is_file());

    let st = isolated_cli_command(&home)
        .args(["integration", "status", "--shell", "zsh", "--json"])
        .output()
        .expect("status zsh");
    assert_eq!(st.status.code(), Some(0));
    let st_json: Value = serde_json::from_slice(&st.stdout).expect("json");
    assert_eq!(st_json["state"], "enabled");
    assert_eq!(st_json["enabled"], true);

    let dis = isolated_cli_command(&home)
        .args(["integration", "disable", "--shell", "zsh", "--json"])
        .output()
        .expect("disable zsh");
    assert_eq!(dis.status.code(), Some(0));
    let dis_json: Value = serde_json::from_slice(&dis.stdout).expect("json");
    assert_eq!(dis_json["state"], "disabled");
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
        assert_eq!(m_json["state"], "manual_required");
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
        assert_eq!(f_json["state"], "enabled");
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
    let mut cfg = config_with_port(port);
    cfg.usage_dir = Some(usage_dir);
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

#[cfg(unix)]
#[test]
fn install_and_uninstall_full_isolated_lifecycle_respects_symlink_guard_dry_run_and_purge() {
    use std::os::unix::fs::symlink;

    let temp = TempDir::new().expect("tempdir");
    let home = temp.path().join("home");
    let xdg_config = temp.path().join("xdg_config");
    let xdg_data = temp.path().join("xdg_data");
    let plugins_dir = temp.path().join("plugins");
    fs::create_dir_all(&home).expect("mkdir home");
    fs::create_dir_all(&plugins_dir).expect("mkdir plugins");

    // Pre-write an isolated config with an ephemeral free port so we never collide with host 4141.
    let cfg_path = xdg_config.join("agy-companion").join("config.json");
    let usage_dir = xdg_data.join("agy-companion").join("usage");
    fs::create_dir_all(&usage_dir).expect("mkdir usage");
    fs::write(usage_dir.join("usage.db"), b"SQLite format 3\0").expect("write usage db");
    fs::write(usage_dir.join("usage.db-wal"), b"wal-canary").expect("write usage wal");
    fs::write(usage_dir.join("2026-11.jsonl"), "{}\n").expect("write legacy usage jsonl");
    let cfg = config_with_port(free_loopback_port());
    cfg.save_secure(&cfg_path).expect("save config");

    // Protect user's ~/.gemini/config/config.json — must NEVER be modified even with --purge.
    let agy_cfg = home.join(".gemini").join("config").join("config.json");
    fs::create_dir_all(agy_cfg.parent().unwrap()).expect("mkdir agy cfg");
    fs::write(&agy_cfg, "{\"keep\":true}\n").expect("write agy cfg");

    // 1. Dev-symlink guard blocks install without --force and succeeds with --force without touching target.
    let dev_target = temp.path().join("dev_plugin");
    fs::create_dir_all(&dev_target).expect("mkdir dev target");
    fs::write(dev_target.join("sentinel.txt"), "DEV_TARGET\n").expect("write sentinel");
    let plugin_link = plugins_dir.join("agy-companion");
    symlink(&dev_target, &plugin_link).expect("symlink");

    let blocked = isolated_cli_command(&home)
        .env("XDG_CONFIG_HOME", &xdg_config)
        .env("XDG_DATA_HOME", &xdg_data)
        .env("SHELL", "/bin/zsh")
        .env("ZDOTDIR", &home)
        .args([
            "install",
            "--plugins-dir",
            plugins_dir.to_str().unwrap(),
            "--no-start-proxy",
            "--json",
        ])
        .output()
        .expect("run blocked install");
    assert_ne!(
        blocked.status.code(),
        Some(0),
        "install must fail on dev symlink without --force"
    );
    assert!(
        fs::symlink_metadata(&plugin_link)
            .expect("link meta")
            .file_type()
            .is_symlink()
    );

    let forced = isolated_cli_command(&home)
        .env("XDG_CONFIG_HOME", &xdg_config)
        .env("XDG_DATA_HOME", &xdg_data)
        .env("SHELL", "/bin/zsh")
        .env("ZDOTDIR", &home)
        .args([
            "install",
            "--plugins-dir",
            plugins_dir.to_str().unwrap(),
            "--no-start-proxy",
            "--force",
            "--json",
        ])
        .output()
        .expect("run forced install");
    assert_eq!(
        forced.status.code(),
        Some(0),
        "stderr={}",
        String::from_utf8_lossy(&forced.stderr)
    );
    let install_json: Value =
        serde_json::from_slice(&forced.stdout).expect("parse install JSON report");
    assert_eq!(install_json["ok"], true);
    assert!(dev_target.join("sentinel.txt").is_file());
    assert!(plugin_link.join("plugin.json").is_file());
    assert!(xdg_data.join("agy-companion/bin/agy-companion").is_file());
    assert!(xdg_data.join("agy-companion/env.sh").is_file());
    assert!(home.join(".zlogin").is_file());

    // 2. --dry-run uninstall mutates nothing.
    let dry = isolated_cli_command(&home)
        .env("XDG_CONFIG_HOME", &xdg_config)
        .env("XDG_DATA_HOME", &xdg_data)
        .env("ZDOTDIR", &home)
        .args([
            "uninstall",
            "--plugins-dir",
            plugins_dir.to_str().unwrap(),
            "--dry-run",
            "--json",
        ])
        .output()
        .expect("run dry uninstall");
    assert_eq!(dry.status.code(), Some(0));
    let dry_json: Value = serde_json::from_slice(&dry.stdout).expect("parse dry JSON");
    assert_eq!(dry_json["ok"], true);
    assert_eq!(dry_json["dry_run"], true);
    assert!(plugin_link.join("plugin.json").is_file());
    assert!(home.join(".zlogin").is_file());

    // 3. Standard uninstall removes plugin, binary, env.sh, empty .zlogin, and preserves config + usage.
    let uninst = isolated_cli_command(&home)
        .env("XDG_CONFIG_HOME", &xdg_config)
        .env("XDG_DATA_HOME", &xdg_data)
        .env("ZDOTDIR", &home)
        .args([
            "uninstall",
            "--plugins-dir",
            plugins_dir.to_str().unwrap(),
            "--json",
        ])
        .output()
        .expect("run uninstall");
    assert_eq!(uninst.status.code(), Some(0));
    assert!(!plugin_link.exists());
    assert!(!xdg_data.join("agy-companion/bin/agy-companion").exists());
    assert!(!xdg_data.join("agy-companion/env.sh").exists());
    assert!(!home.join(".zlogin").exists());
    assert!(cfg_path.is_file(), "config.json preserved without --purge");
    assert!(usage_dir.is_dir(), "usage/ preserved without --purge");
    assert_eq!(fs::read_to_string(&agy_cfg).unwrap(), "{\"keep\":true}\n");

    // 4. --purge removes config.json and usage/, still leaves ~/.gemini/config/config.json untouched.
    let purged = isolated_cli_command(&home)
        .env("XDG_CONFIG_HOME", &xdg_config)
        .env("XDG_DATA_HOME", &xdg_data)
        .env("ZDOTDIR", &home)
        .args([
            "uninstall",
            "--plugins-dir",
            plugins_dir.to_str().unwrap(),
            "--purge",
            "--json",
        ])
        .output()
        .expect("run purge uninstall");
    assert_eq!(purged.status.code(), Some(0));
    assert!(!cfg_path.exists());
    assert!(!usage_dir.exists());
    assert_eq!(fs::read_to_string(&agy_cfg).unwrap(), "{\"keep\":true}\n");
}

#[cfg(unix)]
#[test]
fn uninstall_refuses_to_traverse_symlinked_plugins_root_and_preserves_target() {
    use std::os::unix::fs::symlink;

    let temp = TempDir::new().expect("tempdir");
    let home = temp.path().join("home");
    let xdg_config = temp.path().join("xdg_config");
    let xdg_data = temp.path().join("xdg_data");
    let target_root = temp.path().join("real_plugins_target");
    let target_plugin = target_root.join("agy-companion");
    fs::create_dir_all(&home).expect("mkdir home");
    fs::create_dir_all(&target_plugin).expect("mkdir target plugin");
    fs::write(
        target_plugin.join("plugin.json"),
        r#"{"name":"agy-companion","version":"0.1.0"}"#,
    )
    .expect("write plugin.json");
    let sentinel = target_plugin.join("precious.txt");
    fs::write(&sentinel, "DO_NOT_DELETE\n").expect("write sentinel");

    let symlink_plugins_root = temp.path().join("symlink_plugins_root");
    symlink(&target_root, &symlink_plugins_root).expect("symlink plugins root");

    // Also place an owned env.sh so we verify other cleanup still succeeds safely.
    let env_sh = xdg_data.join("agy-companion").join("env.sh");
    fs::create_dir_all(env_sh.parent().unwrap()).expect("mkdir data dir");
    fs::write(&env_sh, "# Managed by Antigravity Companion\n").expect("write env.sh");

    for dry_run in [true, false] {
        let mut args = vec![
            "uninstall",
            "--plugins-dir",
            symlink_plugins_root.to_str().unwrap(),
            "--json",
        ];
        if dry_run {
            args.push("--dry-run");
        }
        let out = isolated_cli_command(&home)
            .env("XDG_CONFIG_HOME", &xdg_config)
            .env("XDG_DATA_HOME", &xdg_data)
            .env("ZDOTDIR", &home)
            .args(&args)
            .output()
            .expect("run uninstall");
        assert_eq!(
            out.status.code(),
            Some(0),
            "dry_run={dry_run} stderr={}",
            String::from_utf8_lossy(&out.stderr)
        );
        let rep: Value = serde_json::from_slice(&out.stdout).expect("parse uninstall JSON");
        assert_eq!(rep["ok"], true);
        assert_eq!(
            rep["plugin_dir_removed"], false,
            "dry_run={dry_run} must not remove plugin dir through symlinked plugins root"
        );
        let warnings = rep["warnings"].as_array().expect("warnings array");
        assert!(
            warnings.iter().any(|w| {
                w.as_str().is_some_and(|s| {
                    s.contains("refusing to traverse symlinked/non-directory plugins root")
                })
            }),
            "dry_run={dry_run} expected symlinked plugins root warning, got: {warnings:?}"
        );
        assert!(
            sentinel.is_file(),
            "dry_run={dry_run}: target plugin sentinel must remain intact"
        );
    }
    assert!(
        !env_sh.exists(),
        "non-plugin cleanup (env.sh) must still proceed safely"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn concurrent_cold_start_hook_env_leaves_no_zombie_standby_after_uninstall_purge() {
    let temp = TempDir::new().expect("tempdir");
    let home = temp.path().join("home");
    let xdg_config = temp.path().join("xdg_config");
    let xdg_data = temp.path().join("xdg_data");
    let plugins_dir = temp.path().join("plugins");
    let plugin_root = plugins_dir.join("agy-companion");
    fs::create_dir_all(&home).expect("mkdir home");
    fs::create_dir_all(&plugin_root).expect("mkdir plugin");
    fs::write(
        plugin_root.join("plugin.json"),
        r#"{"name":"agy-companion","version":"0.1.0"}"#,
    )
    .expect("write plugin.json");

    let port = free_loopback_port();
    let cfg_path = xdg_config.join("agy-companion").join("config.json");
    let cfg = config_with_port(port);
    cfg.save_secure(&cfg_path).expect("save config");

    let data_dir = xdg_data.join("agy-companion");
    let pid_path = data_dir.join(companion_cli::daemon::PROXY_PID_FILENAME);
    let log_path = data_dir.join(companion_cli::daemon::PROXY_LOG_FILENAME);
    let _pid_guard = DetachedPidFileGuard::new(pid_path.clone());

    // Launch 3 concurrent cold-start hook-env calls simultaneously.
    let mut children = Vec::with_capacity(3);
    for _ in 0..3 {
        let child = isolated_cli_command(&home)
            .env("XDG_CONFIG_HOME", &xdg_config)
            .env("XDG_DATA_HOME", &xdg_data)
            .env("ZDOTDIR", &home)
            .args([
                "hook-env",
                "--ensure-running",
                "--print-port",
                "--plugins-dir",
                plugins_dir.to_str().unwrap(),
            ])
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("spawn hook-env");
        children.push(child);
    }

    for (idx, child) in children.into_iter().enumerate() {
        let out = child.wait_with_output().expect("wait hook-env");
        assert_eq!(
            out.status.code(),
            Some(0),
            "hook-env #{idx} stderr={}",
            String::from_utf8_lossy(&out.stderr)
        );
        assert_eq!(
            String::from_utf8(out.stdout).expect("utf8"),
            format!("{port}\n"),
            "hook-env #{idx} must print validated listen port"
        );
    }

    // Allow bind-race losers to exit cleanly on AddrInUse.
    tokio::time::sleep(Duration::from_millis(300)).await;

    let uninst = isolated_cli_command(&home)
        .env("XDG_CONFIG_HOME", &xdg_config)
        .env("XDG_DATA_HOME", &xdg_data)
        .env("ZDOTDIR", &home)
        .args([
            "uninstall",
            "--plugins-dir",
            plugins_dir.to_str().unwrap(),
            "--purge",
            "--json",
        ])
        .output()
        .expect("run uninstall --purge");
    assert_eq!(
        uninst.status.code(),
        Some(0),
        "stderr={}",
        String::from_utf8_lossy(&uninst.stderr)
    );
    let rep: Value = serde_json::from_slice(&uninst.stdout).expect("parse uninstall JSON");
    assert_eq!(rep["ok"], true);
    assert_eq!(rep["proxy_stopped"], true);

    // Wait 1.0s — if any losing daemon stayed parked via --adopt-running, it would
    // reclaim the port within 500ms and resurrect proxy.pid/proxy.log.
    tokio::time::sleep(Duration::from_secs(1)).await;

    assert!(
        companion_cli::launch::probe_running_companion_proxy_with_timeout(
            port,
            Duration::from_millis(250),
        )
        .await
        .is_none(),
        "GET /health must fail (connection refused) after uninstall --purge"
    );
    assert!(
        std::net::TcpStream::connect((std::net::Ipv4Addr::LOCALHOST, port)).is_err(),
        "port {port} must not accept connections after uninstall --purge"
    );
    assert!(
        !pid_path.exists(),
        "proxy.pid must NOT resurrect in purged data dir"
    );
    assert!(
        !log_path.exists(),
        "proxy.log must NOT resurrect in purged data dir"
    );
}

#[cfg(unix)]
#[test]
fn uninstall_refuses_to_traverse_symlinked_bin_dir_and_preserves_target() {
    use std::os::unix::fs::symlink;

    let temp = TempDir::new().expect("tempdir");
    let home = temp.path().join("home");
    let xdg_config = temp.path().join("xdg_config");
    let xdg_data = temp.path().join("xdg_data");
    let plugins_dir = temp.path().join("plugins");
    fs::create_dir_all(&home).expect("mkdir home");
    fs::create_dir_all(&plugins_dir).expect("mkdir plugins");

    let victim_bin_dir = temp.path().join("victim_bin_dir");
    fs::create_dir_all(&victim_bin_dir).expect("mkdir victim bin");
    let victim_bin = victim_bin_dir.join("agy-companion");
    let victim_ver = victim_bin_dir.join(".version");
    fs::write(&victim_bin, "VICTIM_BINARY\n").expect("write victim bin");
    fs::write(&victim_ver, "0.1.0\n").expect("write victim version");

    let data_dir = xdg_data.join("agy-companion");
    fs::create_dir_all(&data_dir).expect("mkdir data dir");
    let bin_symlink = data_dir.join("bin");
    symlink(&victim_bin_dir, &bin_symlink).expect("symlink bin dir");

    // Also create ~/.local/bin/agy-companion pointing to victim_bin so canonicalization
    // through the symlinked bin_dir cannot trick uninstall into deleting it.
    let local_bin_dir = home.join(".local").join("bin");
    fs::create_dir_all(&local_bin_dir).expect("mkdir local bin");
    let local_bin_link = local_bin_dir.join("agy-companion");
    symlink(&victim_bin, &local_bin_link).expect("symlink local bin");

    for dry_run in [true, false] {
        let mut args = vec![
            "uninstall",
            "--plugins-dir",
            plugins_dir.to_str().unwrap(),
            "--json",
        ];
        if dry_run {
            args.push("--dry-run");
        }
        let out = isolated_cli_command(&home)
            .env("XDG_CONFIG_HOME", &xdg_config)
            .env("XDG_DATA_HOME", &xdg_data)
            .env("ZDOTDIR", &home)
            .args(&args)
            .output()
            .expect("run uninstall");
        assert_eq!(
            out.status.code(),
            Some(0),
            "dry_run={dry_run} stderr={}",
            String::from_utf8_lossy(&out.stderr)
        );
        let rep: Value = serde_json::from_slice(&out.stdout).expect("parse uninstall JSON");
        assert_eq!(rep["ok"], true);
        assert_eq!(
            rep["binary_removed"], false,
            "dry_run={dry_run}: must not remove binary through symlinked bin_dir"
        );
        assert_eq!(
            rep["local_bin_symlink_removed"], false,
            "dry_run={dry_run}: must not remove local_bin symlink when bin_dir is a symlink"
        );
        let warnings = rep["warnings"].as_array().expect("warnings array");
        assert!(
            warnings.iter().any(|w| {
                w.as_str().is_some_and(|s| {
                    s.contains("refusing to traverse symlinked/non-directory bin directory")
                })
            }),
            "dry_run={dry_run}: expected symlinked bin directory warning, got: {warnings:?}"
        );
        assert!(
            victim_bin.is_file(),
            "dry_run={dry_run}: victim binary must remain intact"
        );
        assert!(
            victim_ver.is_file(),
            "dry_run={dry_run}: victim .version must remain intact"
        );
        assert!(
            fs::symlink_metadata(&bin_symlink)
                .expect("bin symlink metadata")
                .file_type()
                .is_symlink(),
            "dry_run={dry_run}: bin symlink must remain intact"
        );
        assert!(
            fs::symlink_metadata(&local_bin_link)
                .expect("local_bin_link metadata")
                .file_type()
                .is_symlink(),
            "dry_run={dry_run}: local_bin symlink must remain intact"
        );
    }
}

#[cfg(unix)]
#[test]
fn uninstall_refuses_to_traverse_symlinked_runtime_data_dir_and_usage_dir() {
    use std::os::unix::fs::symlink;

    // Case (a): config.usage_dir points directly to a symlinked directory (sym_custom_usage -> victim).
    {
        let temp = TempDir::new().expect("tempdir");
        let home = temp.path().join("home");
        let xdg_config = temp.path().join("xdg_config");
        let xdg_data = temp.path().join("xdg_data");
        let plugins_dir = temp.path().join("plugins");
        fs::create_dir_all(&home).expect("mkdir home");
        fs::create_dir_all(&plugins_dir).expect("mkdir plugins");

        let victim_dir = temp.path().join("victim_direct");
        fs::create_dir_all(&victim_dir).expect("mkdir victim");
        let victim_pid = victim_dir.join(companion_cli::daemon::PROXY_PID_FILENAME);
        let victim_log = victim_dir.join(companion_cli::daemon::PROXY_LOG_FILENAME);
        let victim_log1 = victim_dir.join(companion_cli::daemon::PROXY_LOG_ROTATED_FILENAME);
        let victim_db = victim_dir.join("usage.db");
        let dead_pid = companion_cli::daemon::ProxyPidInfo {
            pid: 999_999,
            port: free_loopback_port(),
            version: env!("CARGO_PKG_VERSION").to_string(),
            started_at_ms: 1,
            binary_path: BIN.to_string(),
        };
        fs::write(&victim_pid, serde_json::to_vec(&dead_pid).unwrap()).expect("write pid");
        fs::write(&victim_log, "VICTIM_LOG\n").expect("write log");
        fs::write(&victim_log1, "VICTIM_LOG1\n").expect("write log1");
        fs::write(&victim_db, b"SQLite format 3\0").expect("write usage db");

        let sym_custom_usage = temp.path().join("sym_custom_usage");
        symlink(&victim_dir, &sym_custom_usage).expect("symlink custom usage");

        let cfg_path = xdg_config.join("agy-companion").join("config.json");
        let mut cfg = config_with_port(free_loopback_port());
        cfg.usage_dir = Some(sym_custom_usage.clone());
        cfg.save_secure(&cfg_path).expect("save config");

        for dry_run in [true, false] {
            let mut args = vec![
                "uninstall",
                "--plugins-dir",
                plugins_dir.to_str().unwrap(),
                "--purge",
                "--json",
            ];
            if dry_run {
                args.push("--dry-run");
            }
            let out = isolated_cli_command(&home)
                .env("XDG_CONFIG_HOME", &xdg_config)
                .env("XDG_DATA_HOME", &xdg_data)
                .env("ZDOTDIR", &home)
                .args(&args)
                .output()
                .expect("run uninstall");
            assert_eq!(
                out.status.code(),
                Some(0),
                "case(a) dry_run={dry_run} stderr={}",
                String::from_utf8_lossy(&out.stderr)
            );
            let rep: Value = serde_json::from_slice(&out.stdout).expect("parse uninstall JSON");
            assert_eq!(rep["ok"], true);
            let warnings = rep["warnings"].as_array().expect("warnings array");
            assert!(
                warnings.iter().any(|w| {
                    w.as_str().is_some_and(|s| {
                        s.contains(
                            "refusing to traverse symlinked/non-directory runtime data directory",
                        )
                    })
                }),
                "case(a) dry_run={dry_run}: expected runtime data dir warning, got: {warnings:?}"
            );
            let purged = rep["purged_paths"].as_array().expect("purged_paths");
            assert!(
                !purged.iter().any(|p| {
                    p.as_str().is_some_and(|s| {
                        s.contains("sym_custom_usage") || s.contains("victim_direct")
                    })
                }),
                "case(a) dry_run={dry_run}: symlinked runtime paths must not be purged: {purged:?}"
            );
            assert!(
                victim_pid.is_file(),
                "case(a) dry_run={dry_run}: victim proxy.pid must remain"
            );
            assert!(
                victim_log.is_file(),
                "case(a) dry_run={dry_run}: victim proxy.log must remain"
            );
            assert!(
                victim_log1.is_file(),
                "case(a) dry_run={dry_run}: victim proxy.log.1 must remain"
            );
            assert!(
                victim_db.is_file(),
                "case(a) dry_run={dry_run}: victim usage.db must remain"
            );
            assert!(
                fs::symlink_metadata(&sym_custom_usage)
                    .expect("sym meta")
                    .file_type()
                    .is_symlink(),
                "case(a) dry_run={dry_run}: symlink itself must remain"
            );
        }
    }

    // Case (b): config.usage_dir is <sym_parent>/usage where sym_parent -> victim_parent.
    {
        let temp = TempDir::new().expect("tempdir");
        let home = temp.path().join("home");
        let xdg_config = temp.path().join("xdg_config");
        let xdg_data = temp.path().join("xdg_data");
        let plugins_dir = temp.path().join("plugins");
        fs::create_dir_all(&home).expect("mkdir home");
        fs::create_dir_all(&plugins_dir).expect("mkdir plugins");

        let victim_parent = temp.path().join("victim_parent");
        let victim_usage = victim_parent.join("usage");
        fs::create_dir_all(&victim_usage).expect("mkdir victim usage");
        let victim_pid = victim_parent.join(companion_cli::daemon::PROXY_PID_FILENAME);
        let victim_log = victim_parent.join(companion_cli::daemon::PROXY_LOG_FILENAME);
        let victim_log1 = victim_parent.join(companion_cli::daemon::PROXY_LOG_ROTATED_FILENAME);
        let victim_db = victim_usage.join("usage.db");
        let dead_pid = companion_cli::daemon::ProxyPidInfo {
            pid: 999_998,
            port: free_loopback_port(),
            version: env!("CARGO_PKG_VERSION").to_string(),
            started_at_ms: 1,
            binary_path: BIN.to_string(),
        };
        fs::write(&victim_pid, serde_json::to_vec(&dead_pid).unwrap()).expect("write pid");
        fs::write(&victim_log, "VICTIM_LOG\n").expect("write log");
        fs::write(&victim_log1, "VICTIM_LOG1\n").expect("write log1");
        fs::write(&victim_db, b"SQLite format 3\0").expect("write usage db");

        let sym_parent = temp.path().join("sym_parent");
        symlink(&victim_parent, &sym_parent).expect("symlink parent");

        let cfg_path = xdg_config.join("agy-companion").join("config.json");
        let mut cfg = config_with_port(free_loopback_port());
        cfg.usage_dir = Some(sym_parent.join("usage"));
        cfg.save_secure(&cfg_path).expect("save config");

        for dry_run in [true, false] {
            let mut args = vec![
                "uninstall",
                "--plugins-dir",
                plugins_dir.to_str().unwrap(),
                "--purge",
                "--json",
            ];
            if dry_run {
                args.push("--dry-run");
            }
            let out = isolated_cli_command(&home)
                .env("XDG_CONFIG_HOME", &xdg_config)
                .env("XDG_DATA_HOME", &xdg_data)
                .env("ZDOTDIR", &home)
                .args(&args)
                .output()
                .expect("run uninstall");
            assert_eq!(
                out.status.code(),
                Some(0),
                "case(b) dry_run={dry_run} stderr={}",
                String::from_utf8_lossy(&out.stderr)
            );
            let rep: Value = serde_json::from_slice(&out.stdout).expect("parse uninstall JSON");
            assert_eq!(rep["ok"], true);
            let warnings = rep["warnings"].as_array().expect("warnings array");
            assert!(
                warnings.iter().any(|w| {
                    w.as_str().is_some_and(|s| {
                        s.contains(
                            "refusing to traverse symlinked/non-directory runtime data directory",
                        )
                    })
                }),
                "case(b) dry_run={dry_run}: expected runtime data dir warning, got: {warnings:?}"
            );
            let purged = rep["purged_paths"].as_array().expect("purged_paths");
            assert!(
                !purged.iter().any(|p| {
                    p.as_str()
                        .is_some_and(|s| s.contains("sym_parent") || s.contains("victim_parent"))
                }),
                "case(b) dry_run={dry_run}: symlinked parent paths must not be purged: {purged:?}"
            );
            assert!(
                victim_pid.is_file(),
                "case(b) dry_run={dry_run}: victim proxy.pid must remain"
            );
            assert!(
                victim_log.is_file(),
                "case(b) dry_run={dry_run}: victim proxy.log must remain"
            );
            assert!(
                victim_log1.is_file(),
                "case(b) dry_run={dry_run}: victim proxy.log.1 must remain"
            );
            assert!(
                victim_db.is_file(),
                "case(b) dry_run={dry_run}: victim usage/usage.db must remain"
            );
            assert!(
                fs::symlink_metadata(&sym_parent)
                    .expect("sym meta")
                    .file_type()
                    .is_symlink(),
                "case(b) dry_run={dry_run}: sym_parent symlink must remain"
            );
        }
    }
}

#[cfg(unix)]
#[test]
fn uninstall_refuses_to_traverse_symlinked_config_sidecar_and_local_bin_dirs() {
    use std::os::unix::fs::symlink;

    let temp = TempDir::new().expect("tempdir");
    let home = temp.path().join("home");
    let xdg_config = temp.path().join("xdg_config");
    let xdg_data = temp.path().join("xdg_data");
    let plugins_dir = temp.path().join("plugins");
    fs::create_dir_all(&home).expect("mkdir home");
    fs::create_dir_all(&xdg_config).expect("mkdir xdg_config");
    fs::create_dir_all(&plugins_dir).expect("mkdir plugins");

    // 1. Symlinked config directory ($XDG_CONFIG_HOME/agy-companion -> victim_cfg_dir)
    let victim_cfg_dir = temp.path().join("victim_cfg_dir");
    fs::create_dir_all(&victim_cfg_dir).expect("mkdir victim cfg");
    let victim_cfg = victim_cfg_dir.join("config.json");
    Config::default()
        .save_secure(&victim_cfg)
        .expect("save victim cfg");
    let sym_cfg_dir = xdg_config.join("agy-companion");
    symlink(&victim_cfg_dir, &sym_cfg_dir).expect("symlink cfg dir");

    // 2. Symlinked sidecar_data parent (~/.gemini/antigravity/sidecar_data -> victim_sidecar_root)
    let victim_sidecar_root = temp.path().join("victim_sidecar_root");
    let victim_sidecar_app = victim_sidecar_root.join("agy-companion");
    fs::create_dir_all(&victim_sidecar_app).expect("mkdir victim sidecar");
    let victim_sidecar_file = victim_sidecar_app.join("precious.log");
    fs::write(&victim_sidecar_file, "KEEP_SIDECAR\n").expect("write sidecar");
    let agy_dir = home.join(".gemini").join("antigravity");
    fs::create_dir_all(&agy_dir).expect("mkdir antigravity dir");
    symlink(&victim_sidecar_root, agy_dir.join("sidecar_data")).expect("symlink sidecar_data");

    // 3. Symlinked ~/.local/bin (~/.local/bin -> victim_local_bin)
    let victim_local_bin = temp.path().join("victim_local_bin");
    fs::create_dir_all(&victim_local_bin).expect("mkdir victim local bin");
    let data_bin_dir = xdg_data.join("agy-companion").join("bin");
    fs::create_dir_all(&data_bin_dir).expect("mkdir data bin");
    let stable_bin = data_bin_dir.join("agy-companion");
    fs::write(&stable_bin, "REAL_BIN\n").expect("write real bin");
    let victim_local_link = victim_local_bin.join("agy-companion");
    symlink(&stable_bin, &victim_local_link).expect("symlink inside victim local bin");
    let dot_local = home.join(".local");
    fs::create_dir_all(&dot_local).expect("mkdir .local");
    symlink(&victim_local_bin, dot_local.join("bin")).expect("symlink .local/bin");

    for dry_run in [true, false] {
        let mut args = vec![
            "uninstall",
            "--plugins-dir",
            plugins_dir.to_str().unwrap(),
            "--purge",
            "--json",
        ];
        if dry_run {
            args.push("--dry-run");
        }
        let out = isolated_cli_command(&home)
            .env("XDG_CONFIG_HOME", &xdg_config)
            .env("XDG_DATA_HOME", &xdg_data)
            .env("ZDOTDIR", &home)
            .args(&args)
            .output()
            .expect("run uninstall");
        assert_eq!(
            out.status.code(),
            Some(0),
            "dry_run={dry_run} stderr={}",
            String::from_utf8_lossy(&out.stderr)
        );
        let rep: Value = serde_json::from_slice(&out.stdout).expect("parse uninstall JSON");
        assert_eq!(rep["ok"], true);
        assert_eq!(rep["local_bin_symlink_removed"], false);
        let warnings = rep["warnings"].as_array().expect("warnings array");
        for expected in [
            "refusing to traverse symlinked/non-directory config directory",
            "refusing to traverse symlinked/non-directory sidecar data directory",
            "refusing to traverse symlinked/non-directory local bin directory",
        ] {
            assert!(
                warnings
                    .iter()
                    .any(|w| w.as_str().is_some_and(|s| s.contains(expected))),
                "dry_run={dry_run}: expected warning '{expected}', got: {warnings:?}"
            );
        }
        assert!(
            victim_cfg.is_file(),
            "dry_run={dry_run}: victim config.json must remain"
        );
        assert!(
            victim_sidecar_file.is_file(),
            "dry_run={dry_run}: victim sidecar file must remain"
        );
        assert!(
            fs::symlink_metadata(&victim_local_link)
                .expect("victim local link")
                .file_type()
                .is_symlink(),
            "dry_run={dry_run}: victim local bin symlink must remain"
        );
    }
}

#[cfg(unix)]
#[test]
fn install_refuses_to_traverse_symlinked_default_plugins_ancestors() {
    use std::os::unix::fs::symlink;

    // Subcase (a): $HOME/.gemini -> victim_gemini
    {
        let temp = TempDir::new().expect("tempdir");
        let home = temp.path().join("home");
        let xdg_config = temp.path().join("xdg_config");
        let xdg_data = temp.path().join("xdg_data");
        fs::create_dir_all(&home).expect("mkdir home");

        let victim_gemini = temp.path().join("victim_gemini");
        let victim_plugin = victim_gemini
            .join("config")
            .join("plugins")
            .join("agy-companion");
        fs::create_dir_all(&victim_plugin).expect("mkdir victim plugin");
        let precious = victim_plugin.join("precious.txt");
        fs::write(&precious, "KEEP_GEMINI_TARGET\n").expect("write precious");

        symlink(&victim_gemini, home.join(".gemini")).expect("symlink .gemini");

        for force in [false, true] {
            let mut args = vec![
                "install",
                "--no-launcher-hook",
                "--no-start-proxy",
                "--json",
            ];
            if force {
                args.push("--force");
            }
            let out = isolated_cli_command(&home)
                .env("XDG_CONFIG_HOME", &xdg_config)
                .env("XDG_DATA_HOME", &xdg_data)
                .env("ZDOTDIR", &home)
                .args(&args)
                .output()
                .expect("run install");
            assert_ne!(
                out.status.code(),
                Some(0),
                "symlinked ~/.gemini (force={force}) must fail closed; stdout={}",
                String::from_utf8_lossy(&out.stdout)
            );
            let stderr = String::from_utf8_lossy(&out.stderr);
            assert!(
                stderr.contains("refusing to traverse symlinked/non-directory plugins root"),
                "symlinked ~/.gemini (force={force}) expected plugins root refusal, got stderr={stderr}"
            );
            assert_eq!(
                fs::read_to_string(&precious).expect("read precious"),
                "KEEP_GEMINI_TARGET\n",
                "symlinked ~/.gemini (force={force}): precious.txt must remain intact"
            );
            assert!(
                !victim_plugin.join("plugin.json").exists(),
                "symlinked ~/.gemini (force={force}): plugin.json must not be created in symlink target"
            );
        }
    }

    // Subcase (b): $HOME/.gemini/config -> victim_config
    {
        let temp = TempDir::new().expect("tempdir");
        let home = temp.path().join("home");
        let xdg_config = temp.path().join("xdg_config");
        let xdg_data = temp.path().join("xdg_data");
        fs::create_dir_all(home.join(".gemini")).expect("mkdir .gemini");

        let victim_config = temp.path().join("victim_config");
        let victim_plugin = victim_config.join("plugins").join("agy-companion");
        fs::create_dir_all(&victim_plugin).expect("mkdir victim plugin");
        let precious = victim_plugin.join("precious.txt");
        fs::write(&precious, "KEEP_CONFIG_TARGET\n").expect("write precious");

        symlink(&victim_config, home.join(".gemini").join("config"))
            .expect("symlink .gemini/config");

        for force in [false, true] {
            let mut args = vec![
                "install",
                "--no-launcher-hook",
                "--no-start-proxy",
                "--json",
            ];
            if force {
                args.push("--force");
            }
            let out = isolated_cli_command(&home)
                .env("XDG_CONFIG_HOME", &xdg_config)
                .env("XDG_DATA_HOME", &xdg_data)
                .env("ZDOTDIR", &home)
                .args(&args)
                .output()
                .expect("run install");
            assert_ne!(
                out.status.code(),
                Some(0),
                "symlinked ~/.gemini/config (force={force}) must fail closed; stdout={}",
                String::from_utf8_lossy(&out.stdout)
            );
            let stderr = String::from_utf8_lossy(&out.stderr);
            assert!(
                stderr.contains("refusing to traverse symlinked/non-directory plugins root"),
                "symlinked ~/.gemini/config (force={force}) expected plugins root refusal, got stderr={stderr}"
            );
            assert_eq!(
                fs::read_to_string(&precious).expect("read precious"),
                "KEEP_CONFIG_TARGET\n",
                "symlinked ~/.gemini/config (force={force}): precious.txt must remain intact"
            );
            assert!(
                !victim_plugin.join("plugin.json").exists(),
                "symlinked ~/.gemini/config (force={force}): plugin.json must not be created in symlink target"
            );
        }
    }
}

#[cfg(unix)]
#[test]
fn install_and_uninstall_refuse_symlinked_default_xdg_home_ancestors() {
    use std::os::unix::fs::symlink;

    // Subcase (a): XDG_DATA_HOME & XDG_CONFIG_HOME unset, $HOME/.local -> victim_local
    {
        let temp = TempDir::new().expect("tempdir");
        let home = temp.path().join("home");
        let plugins_dir = temp.path().join("plugins");
        fs::create_dir_all(&home).expect("mkdir home");
        fs::create_dir_all(&plugins_dir).expect("mkdir plugins");

        let victim_local = temp.path().join("victim_local");
        let victim_app = victim_local.join("share").join("agy-companion");
        let victim_usage = victim_app.join("usage");
        let victim_bin_dir = victim_app.join("bin");
        let victim_local_bin = victim_local.join("bin");
        fs::create_dir_all(&victim_usage).expect("mkdir victim usage");
        fs::create_dir_all(&victim_bin_dir).expect("mkdir victim bin");
        fs::create_dir_all(&victim_local_bin).expect("mkdir victim local/bin");
        let victim_db = victim_usage.join("usage.db");
        let victim_env = victim_app.join("env.sh");
        let victim_bin = victim_bin_dir.join("agy-companion");
        let victim_log = victim_app.join(companion_cli::daemon::PROXY_LOG_FILENAME);
        fs::write(&victim_db, b"SQLite format 3\0").expect("write usage.db");
        fs::write(&victim_env, "# KEEP_ENV\n").expect("write env.sh");
        fs::write(&victim_bin, "KEEP_BIN\n").expect("write bin");
        fs::write(&victim_log, "KEEP_LOG\n").expect("write log");
        let victim_local_link = victim_local_bin.join("agy-companion");
        symlink(&victim_bin, &victim_local_link).expect("symlink local/bin/agy-companion");

        symlink(&victim_local, home.join(".local")).expect("symlink .local");

        // install must fail closed without touching victim_local
        let inst = isolated_cli_command(&home)
            .env_remove("XDG_DATA_HOME")
            .env_remove("XDG_CONFIG_HOME")
            .env("ZDOTDIR", &home)
            .args([
                "install",
                "--plugins-dir",
                plugins_dir.to_str().unwrap(),
                "--no-launcher-hook",
                "--no-start-proxy",
                "--json",
            ])
            .output()
            .expect("run install");
        assert_ne!(
            inst.status.code(),
            Some(0),
            "install with symlinked ~/.local must fail closed; stdout={}",
            String::from_utf8_lossy(&inst.stdout)
        );
        let inst_stderr = String::from_utf8_lossy(&inst.stderr);
        assert!(
            inst_stderr.contains("refusing to traverse symlinked/non-directory data directory"),
            "expected data directory refusal on install, got stderr={inst_stderr}"
        );
        assert!(
            !victim_bin_dir.join(".version").exists(),
            "install must not write .version into symlinked ~/.local target"
        );

        // uninstall --purge (both --dry-run and real) must warn and leave all victim files intact
        for dry_run in [true, false] {
            let mut args = vec![
                "uninstall",
                "--plugins-dir",
                plugins_dir.to_str().unwrap(),
                "--purge",
                "--json",
            ];
            if dry_run {
                args.push("--dry-run");
            }
            let out = isolated_cli_command(&home)
                .env_remove("XDG_DATA_HOME")
                .env_remove("XDG_CONFIG_HOME")
                .env("ZDOTDIR", &home)
                .args(&args)
                .output()
                .expect("run uninstall");
            assert_eq!(
                out.status.code(),
                Some(0),
                "~/.local dry_run={dry_run} stderr={}",
                String::from_utf8_lossy(&out.stderr)
            );
            let rep: Value = serde_json::from_slice(&out.stdout).expect("parse uninstall JSON");
            assert_eq!(rep["ok"], true);
            assert_eq!(rep["env_script_removed"], false);
            assert_eq!(rep["binary_removed"], false);
            assert_eq!(rep["local_bin_symlink_removed"], false);
            let warnings = rep["warnings"].as_array().expect("warnings");
            assert!(
                warnings.iter().any(|w| {
                    w.as_str().is_some_and(|s| {
                        s.contains("refusing to traverse symlinked/non-directory data directory")
                    })
                }),
                "~/.local dry_run={dry_run}: expected data directory warning, got {warnings:?}"
            );
            let purged = rep["purged_paths"].as_array().expect("purged_paths");
            assert!(
                purged.is_empty(),
                "~/.local dry_run={dry_run}: nothing should be purged through symlinked ~/.local, got {purged:?}"
            );
            assert!(victim_db.is_file(), "usage.db must survive");
            assert!(victim_env.is_file(), "env.sh must survive");
            assert!(victim_bin.is_file(), "bin/agy-companion must survive");
            assert!(victim_log.is_file(), "proxy.log must survive");
            assert!(
                fs::symlink_metadata(&victim_local_link)
                    .expect("local link meta")
                    .file_type()
                    .is_symlink(),
                "victim_local/bin/agy-companion must survive"
            );
        }
    }

    // Subcase (b): XDG_DATA_HOME & XDG_CONFIG_HOME unset, $HOME/.local/share -> victim_share
    {
        let temp = TempDir::new().expect("tempdir");
        let home = temp.path().join("home");
        let plugins_dir = temp.path().join("plugins");
        fs::create_dir_all(home.join(".local")).expect("mkdir .local");
        fs::create_dir_all(&plugins_dir).expect("mkdir plugins");

        let victim_share = temp.path().join("victim_share");
        let victim_app = victim_share.join("agy-companion");
        let victim_usage = victim_app.join("usage");
        fs::create_dir_all(&victim_usage).expect("mkdir victim usage");
        let victim_db = victim_usage.join("usage.db");
        let victim_env = victim_app.join("env.sh");
        fs::write(&victim_db, b"SQLite format 3\0").expect("write usage.db");
        fs::write(&victim_env, "# KEEP_ENV\n").expect("write env.sh");

        symlink(&victim_share, home.join(".local").join("share")).expect("symlink .local/share");

        let inst = isolated_cli_command(&home)
            .env_remove("XDG_DATA_HOME")
            .env_remove("XDG_CONFIG_HOME")
            .env("ZDOTDIR", &home)
            .args([
                "install",
                "--plugins-dir",
                plugins_dir.to_str().unwrap(),
                "--no-launcher-hook",
                "--no-start-proxy",
                "--json",
            ])
            .output()
            .expect("run install");
        assert_ne!(
            inst.status.code(),
            Some(0),
            "install with symlinked ~/.local/share must fail closed; stdout={}",
            String::from_utf8_lossy(&inst.stdout)
        );
        let inst_stderr = String::from_utf8_lossy(&inst.stderr);
        assert!(
            inst_stderr.contains("refusing to traverse symlinked/non-directory data directory"),
            "expected data directory refusal on install, got stderr={inst_stderr}"
        );

        for dry_run in [true, false] {
            let mut args = vec![
                "uninstall",
                "--plugins-dir",
                plugins_dir.to_str().unwrap(),
                "--purge",
                "--json",
            ];
            if dry_run {
                args.push("--dry-run");
            }
            let out = isolated_cli_command(&home)
                .env_remove("XDG_DATA_HOME")
                .env_remove("XDG_CONFIG_HOME")
                .env("ZDOTDIR", &home)
                .args(&args)
                .output()
                .expect("run uninstall");
            assert_eq!(
                out.status.code(),
                Some(0),
                "~/.local/share dry_run={dry_run} stderr={}",
                String::from_utf8_lossy(&out.stderr)
            );
            let rep: Value = serde_json::from_slice(&out.stdout).expect("parse uninstall JSON");
            assert_eq!(rep["ok"], true);
            assert_eq!(rep["env_script_removed"], false);
            let warnings = rep["warnings"].as_array().expect("warnings");
            assert!(
                warnings.iter().any(|w| {
                    w.as_str().is_some_and(|s| {
                        s.contains("refusing to traverse symlinked/non-directory data directory")
                    })
                }),
                "~/.local/share dry_run={dry_run}: expected data directory warning, got {warnings:?}"
            );
            assert!(victim_db.is_file(), "usage.db must survive");
            assert!(victim_env.is_file(), "env.sh must survive");
        }
    }

    // Subcase (c): XDG_DATA_HOME & XDG_CONFIG_HOME unset, $HOME/.config -> victim_cfg
    {
        let temp = TempDir::new().expect("tempdir");
        let home = temp.path().join("home");
        let plugins_dir = temp.path().join("plugins");
        fs::create_dir_all(&home).expect("mkdir home");
        fs::create_dir_all(&plugins_dir).expect("mkdir plugins");

        let victim_cfg = temp.path().join("victim_cfg");
        let victim_cfg_file = victim_cfg.join("agy-companion").join("config.json");
        config_with_port(free_loopback_port())
            .save_secure(&victim_cfg_file)
            .expect("save victim config");

        symlink(&victim_cfg, home.join(".config")).expect("symlink .config");

        let inst = isolated_cli_command(&home)
            .env_remove("XDG_DATA_HOME")
            .env_remove("XDG_CONFIG_HOME")
            .env("ZDOTDIR", &home)
            .args([
                "install",
                "--plugins-dir",
                plugins_dir.to_str().unwrap(),
                "--no-launcher-hook",
                "--no-start-proxy",
                "--json",
            ])
            .output()
            .expect("run install");
        assert_ne!(
            inst.status.code(),
            Some(0),
            "install with symlinked ~/.config must fail closed; stdout={}",
            String::from_utf8_lossy(&inst.stdout)
        );
        let inst_stderr = String::from_utf8_lossy(&inst.stderr);
        assert!(
            inst_stderr.contains("refusing to traverse symlinked/non-directory config directory"),
            "expected config directory refusal on install, got stderr={inst_stderr}"
        );

        for dry_run in [true, false] {
            let mut args = vec![
                "uninstall",
                "--plugins-dir",
                plugins_dir.to_str().unwrap(),
                "--purge",
                "--json",
            ];
            if dry_run {
                args.push("--dry-run");
            }
            let out = isolated_cli_command(&home)
                .env_remove("XDG_DATA_HOME")
                .env_remove("XDG_CONFIG_HOME")
                .env("ZDOTDIR", &home)
                .args(&args)
                .output()
                .expect("run uninstall");
            assert_eq!(
                out.status.code(),
                Some(0),
                "~/.config dry_run={dry_run} stderr={}",
                String::from_utf8_lossy(&out.stderr)
            );
            let rep: Value = serde_json::from_slice(&out.stdout).expect("parse uninstall JSON");
            assert_eq!(rep["ok"], true);
            let warnings = rep["warnings"].as_array().expect("warnings");
            assert!(
                warnings.iter().any(|w| {
                    w.as_str().is_some_and(|s| {
                        s.contains("refusing to traverse symlinked/non-directory config directory")
                    })
                }),
                "~/.config dry_run={dry_run}: expected config directory warning, got {warnings:?}"
            );
            let purged = rep["purged_paths"].as_array().expect("purged_paths");
            assert!(
                purged.is_empty(),
                "~/.config dry_run={dry_run}: config.json must not be purged through symlinked ~/.config, got {purged:?}"
            );
            assert!(victim_cfg_file.is_file(), "victim config.json must survive");
        }
    }
}

#[cfg(unix)]
#[test]
fn install_refuses_to_traverse_symlinked_config_dir_and_starts_no_proxy() {
    use std::os::unix::fs::symlink;

    let temp = TempDir::new().expect("tempdir");
    let home = temp.path().join("home");
    let xdg_config = temp.path().join("xdg_config");
    let xdg_data = temp.path().join("xdg_data");
    let plugins_dir = temp.path().join("plugins");
    fs::create_dir_all(&home).expect("mkdir home");
    fs::create_dir_all(&xdg_config).expect("mkdir xdg_config");
    fs::create_dir_all(&plugins_dir).expect("mkdir plugins");

    let port = free_loopback_port();
    let victim_cfg_dir = temp.path().join("victim_cfg_dir");
    let victim_cfg_file = victim_cfg_dir.join("config.json");
    config_with_port(port)
        .save_secure(&victim_cfg_file)
        .expect("save victim config");
    let before = fs::read_to_string(&victim_cfg_file).expect("read before");

    symlink(&victim_cfg_dir, xdg_config.join("agy-companion")).expect("symlink config dir");
    let _pid_guard = DetachedPidFileGuard::new(
        xdg_data
            .join("agy-companion")
            .join(companion_cli::daemon::PROXY_PID_FILENAME),
    );

    let out = isolated_cli_command(&home)
        .env("XDG_CONFIG_HOME", &xdg_config)
        .env("XDG_DATA_HOME", &xdg_data)
        .env("ZDOTDIR", &home)
        .args([
            "install",
            "--plugins-dir",
            plugins_dir.to_str().unwrap(),
            "--no-launcher-hook",
            "--json",
        ])
        .output()
        .expect("run install");
    assert_ne!(
        out.status.code(),
        Some(0),
        "install with symlinked XDG config dir must fail closed; stdout={}",
        String::from_utf8_lossy(&out.stdout)
    );
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("refusing to traverse symlinked/non-directory config directory"),
        "expected config directory refusal, got stderr={stderr}"
    );
    assert_eq!(
        fs::read_to_string(&victim_cfg_file).expect("read after"),
        before,
        "victim config.json must remain untouched"
    );
    assert!(
        !plugins_dir.join("agy-companion").exists(),
        "plugin must not be installed when config dir is a symlink"
    );
    assert!(
        std::net::TcpStream::connect((std::net::Ipv4Addr::LOCALHOST, port)).is_err(),
        "no proxy may be started on port {port} from symlinked config dir"
    );
}

#[cfg(unix)]
#[test]
fn uninstall_and_install_refuse_symlinked_fish_intermediate_dirs() {
    use std::os::unix::fs::symlink;

    // 1. Probes A3a & A3-zsh: uninstall --purge (with SHELL=/bin/zsh, XDG_CONFIG_HOME unset,
    //    $HOME/.config -> victim_cfg) preserves agy-companion.fish untouched (both markers-only
    //    and user-comment + markers) and records it in rc_manual_remaining.
    for (case_name, fish_content) in [
        (
            "markers_only",
            format!(
                "{}\n# managed fish hook\n{}\n",
                companion_core::HOOK_BEGIN_MARKER,
                companion_core::HOOK_END_MARKER
            ),
        ),
        (
            "user_comment_plus_markers",
            format!(
                "# user fish setting\n{}\n# managed fish hook\n{}\n",
                companion_core::HOOK_BEGIN_MARKER,
                companion_core::HOOK_END_MARKER
            ),
        ),
    ] {
        let temp = TempDir::new().expect("tempdir");
        let home = temp.path().join("home");
        let plugins_dir = temp.path().join("plugins");
        let victim_cfg = temp.path().join("victim_cfg");
        let victim_fish_dropin = victim_cfg.join("fish/conf.d/agy-companion.fish");
        fs::create_dir_all(&home).expect("mkdir home");
        fs::create_dir_all(&plugins_dir).expect("mkdir plugins");
        fs::create_dir_all(victim_fish_dropin.parent().unwrap()).expect("mkdir victim fish");
        fs::write(&victim_fish_dropin, &fish_content).expect("write victim fish drop-in");
        symlink(&victim_cfg, home.join(".config")).expect("symlink .config");

        let expected_rc = home.join(".config/fish/conf.d/agy-companion.fish");
        for dry_run in [true, false] {
            let mut args = vec![
                "uninstall",
                "--plugins-dir",
                plugins_dir.to_str().unwrap(),
                "--purge",
                "--json",
            ];
            if dry_run {
                args.push("--dry-run");
            }
            let out = isolated_cli_command(&home)
                .env("SHELL", "/bin/zsh")
                .env_remove("XDG_CONFIG_HOME")
                .env_remove("XDG_DATA_HOME")
                .env("ZDOTDIR", &home)
                .args(&args)
                .output()
                .expect("run uninstall");
            assert_eq!(
                out.status.code(),
                Some(0),
                "{case_name} dry_run={dry_run} stderr={}",
                String::from_utf8_lossy(&out.stderr)
            );
            let rep: Value = serde_json::from_slice(&out.stdout).expect("parse uninstall JSON");
            assert_eq!(rep["ok"], true);
            assert!(
                rep["rc_deleted"].as_array().unwrap().is_empty(),
                "{case_name} dry_run={dry_run}: must not delete through symlinked ~/.config: {rep}"
            );
            assert!(
                rep["rc_modified"].as_array().unwrap().is_empty(),
                "{case_name} dry_run={dry_run}: must not modify through symlinked ~/.config: {rep}"
            );
            let manual = rep["rc_manual_remaining"].as_array().unwrap();
            assert!(
                manual
                    .iter()
                    .any(|p| p.as_str() == Some(expected_rc.to_str().unwrap())),
                "{case_name} dry_run={dry_run}: expected {expected_rc:?} in rc_manual_remaining, got {manual:?}"
            );
            assert_eq!(
                fs::read_to_string(&victim_fish_dropin).expect("read victim fish drop-in"),
                fish_content,
                "{case_name} dry_run={dry_run}: victim agy-companion.fish must survive untouched"
            );
        }
    }

    // 2. Probes A3b & A3-stow-fish: Stow-style symlinked ~/.config/fish and ~/.config/fish/conf.d
    //    with SHELL=/usr/local/bin/fish: install reports manual_required=true and creates zero
    //    files in victim target; uninstall preserves pre-existing drop-in and reports rc_manual_remaining.
    for stow_kind in ["fish_dir", "confd_dir"] {
        let temp = TempDir::new().expect("tempdir");
        let home = temp.path().join("home");
        let plugins_dir = temp.path().join("plugins");
        let xdg_data = temp.path().join("xdg_data");
        let victim = temp.path().join(format!("victim_{stow_kind}"));
        fs::create_dir_all(&plugins_dir).expect("mkdir plugins");
        fs::create_dir_all(&victim).expect("mkdir victim");

        let victim_dropin = if stow_kind == "fish_dir" {
            fs::create_dir_all(home.join(".config")).expect("mkdir .config");
            symlink(&victim, home.join(".config/fish")).expect("symlink .config/fish");
            victim.join("conf.d/agy-companion.fish")
        } else {
            fs::create_dir_all(home.join(".config/fish")).expect("mkdir .config/fish");
            symlink(&victim, home.join(".config/fish/conf.d"))
                .expect("symlink .config/fish/conf.d");
            victim.join("agy-companion.fish")
        };

        // Pre-write isolated config with an ephemeral free port so we never collide with 4141.
        let cfg_path = home.join(".config/agy-companion/config.json");
        config_with_port(free_loopback_port())
            .save_secure(&cfg_path)
            .expect("save isolated config");

        let inst = isolated_cli_command(&home)
            .env("SHELL", "/usr/local/bin/fish")
            .env_remove("XDG_CONFIG_HOME")
            .env("XDG_DATA_HOME", &xdg_data)
            .env("ZDOTDIR", &home)
            .args([
                "install",
                "--plugins-dir",
                plugins_dir.to_str().unwrap(),
                "--no-start-proxy",
                "--json",
            ])
            .output()
            .expect("run fish install");
        assert_eq!(
            inst.status.code(),
            Some(0),
            "{stow_kind}: install stderr={}",
            String::from_utf8_lossy(&inst.stderr)
        );
        let inst_json: Value = serde_json::from_slice(&inst.stdout).expect("parse install JSON");
        assert_eq!(inst_json["ok"], true);
        assert_eq!(
            inst_json["launcher_integration"]["state"], "manual_required",
            "{stow_kind}: Stow-symlinked fish dir must set state=manual_required: {inst_json}"
        );
        assert_eq!(
            inst_json["launcher_integration"]["enabled"], false,
            "{stow_kind}: Stow-symlinked fish dir must not claim enabled=true: {inst_json}"
        );
        assert!(
            !victim_dropin.exists(),
            "{stow_kind}: install must NOT create agy-companion.fish inside victim target"
        );

        // Now populate a pre-existing drop-in inside victim target and run uninstall --purge.
        fs::create_dir_all(victim_dropin.parent().unwrap()).expect("mkdir victim parent");
        let existing_fish = format!(
            "{}\n# stow managed\n{}\n",
            companion_core::HOOK_BEGIN_MARKER,
            companion_core::HOOK_END_MARKER
        );
        fs::write(&victim_dropin, &existing_fish).expect("write victim drop-in");

        let uninst = isolated_cli_command(&home)
            .env("SHELL", "/usr/local/bin/fish")
            .env_remove("XDG_CONFIG_HOME")
            .env("XDG_DATA_HOME", &xdg_data)
            .env("ZDOTDIR", &home)
            .args([
                "uninstall",
                "--plugins-dir",
                plugins_dir.to_str().unwrap(),
                "--purge",
                "--json",
            ])
            .output()
            .expect("run fish uninstall");
        assert_eq!(
            uninst.status.code(),
            Some(0),
            "{stow_kind}: uninstall stderr={}",
            String::from_utf8_lossy(&uninst.stderr)
        );
        let uninst_json: Value =
            serde_json::from_slice(&uninst.stdout).expect("parse uninstall JSON");
        let expected_rc = home.join(".config/fish/conf.d/agy-companion.fish");
        assert!(
            uninst_json["rc_deleted"].as_array().unwrap().is_empty(),
            "{stow_kind}: uninstall must not delete inside Stow symlink target"
        );
        assert!(
            uninst_json["rc_manual_remaining"]
                .as_array()
                .unwrap()
                .iter()
                .any(|p| p.as_str() == Some(expected_rc.to_str().unwrap())),
            "{stow_kind}: uninstall must record fish drop-in in rc_manual_remaining: {uninst_json}"
        );
        assert_eq!(
            fs::read_to_string(&victim_dropin).expect("read victim drop-in"),
            existing_fish,
            "{stow_kind}: victim agy-companion.fish must survive uninstall untouched"
        );
    }
}

#[cfg(unix)]
#[test]
fn install_step1_failure_does_not_create_default_gemini_plugins_dir() {
    use std::os::unix::fs::symlink;

    let temp = TempDir::new().expect("tempdir");
    let home = temp.path().join("home");
    let xdg_config = temp.path().join("xdg_config");
    let xdg_data = temp.path().join("xdg_data");
    let victim_cfg_dir = temp.path().join("victim_cfg_dir");
    fs::create_dir_all(&home).expect("mkdir home");
    fs::create_dir_all(&xdg_config).expect("mkdir xdg_config");
    fs::create_dir_all(&victim_cfg_dir).expect("mkdir victim_cfg_dir");
    symlink(&victim_cfg_dir, xdg_config.join("agy-companion")).expect("symlink config dir");

    assert!(
        !home.join(".gemini").exists(),
        "precondition: ~/.gemini must not exist before install"
    );

    let out = isolated_cli_command(&home)
        .env("XDG_CONFIG_HOME", &xdg_config)
        .env("XDG_DATA_HOME", &xdg_data)
        .env("ZDOTDIR", &home)
        .args([
            "install",
            "--no-launcher-hook",
            "--no-start-proxy",
            "--json",
        ])
        .output()
        .expect("run install");
    assert_ne!(
        out.status.code(),
        Some(0),
        "install must fail closed in Step 1 on symlinked XDG config dir"
    );
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("refusing to traverse symlinked/non-directory config directory"),
        "expected config directory refusal, got stderr={stderr}"
    );
    assert!(
        !home.join(".gemini").exists(),
        "Step 1 failure must NOT create ~/.gemini/config/plugins prematurely on disk"
    );
}

#[cfg(unix)]
#[test]
fn cli_integration_refuses_symlinked_env_data_dir_on_zsh_and_bash() {
    use std::os::unix::fs::{PermissionsExt, symlink};

    for shell in ["zsh", "bash"] {
        let rc_filename = if shell == "zsh" {
            ".zlogin"
        } else {
            ".profile"
        };

        // Case (a): $XDG_DATA_HOME/agy-companion -> victim_data
        {
            let temp = TempDir::new().expect("tempdir");
            let home = temp.path().join("home");
            let xdg_data = temp.path().join("xdg_data");
            let victim_data = temp.path().join("victim_data");
            fs::create_dir_all(&home).expect("mkdir home");
            fs::create_dir_all(&xdg_data).expect("mkdir xdg_data");
            fs::create_dir_all(&victim_data).expect("mkdir victim_data");
            fs::set_permissions(&victim_data, fs::Permissions::from_mode(0o755))
                .expect("chmod 0755");
            symlink(&victim_data, xdg_data.join("agy-companion")).expect("symlink data dir");

            let victim_env = victim_data.join("env.sh");
            let sentinel_env = "# VICTIM_XDG_ENV\n";
            fs::write(&victim_env, sentinel_env).expect("write victim env");
            let rc_path = home.join(rc_filename);
            let rc_before = format!(
                "# keep\n{}\n[ -r \"${{XDG_DATA_HOME:-$HOME/.local/share}}/agy-companion/env.sh\" ] && . \"${{XDG_DATA_HOME:-$HOME/.local/share}}/agy-companion/env.sh\"\n{}\n",
                companion_core::HOOK_BEGIN_MARKER,
                companion_core::HOOK_END_MARKER
            );
            fs::write(&rc_path, &rc_before).expect("write rc");

            for subcmd in ["enable", "status", "disable"] {
                let out = isolated_cli_command(&home)
                    .env("XDG_DATA_HOME", &xdg_data)
                    .env("ZDOTDIR", &home)
                    .args(["integration", subcmd, "--shell", shell, "--json"])
                    .output()
                    .expect("run integration");
                assert_ne!(
                    out.status.code(),
                    Some(0),
                    "xdg_data {shell} {subcmd} must fail closed on symlinked data dir; stdout={}",
                    String::from_utf8_lossy(&out.stdout)
                );
            }
            assert_eq!(
                fs::read_to_string(&victim_env).expect("read victim env"),
                sentinel_env,
                "xdg_data {shell}: victim env.sh must survive untouched"
            );
            assert_eq!(
                fs::metadata(&victim_data)
                    .expect("stat victim")
                    .permissions()
                    .mode()
                    & 0o777,
                0o755,
                "xdg_data {shell}: victim dir mode must stay 0o755"
            );
            assert_eq!(
                fs::read_to_string(&rc_path).expect("read rc"),
                rc_before,
                "xdg_data {shell}: RC file must remain untouched"
            );
        }

        // Case (b): XDG_DATA_HOME unset, $HOME/.local -> victim_local (plus symlinked RC file)
        {
            let temp = TempDir::new().expect("tempdir");
            let home = temp.path().join("home");
            let victim_local = temp.path().join("victim_local");
            let victim_data = victim_local.join("share/agy-companion");
            let victim_rc = temp.path().join("victim_rc");
            fs::create_dir_all(&home).expect("mkdir home");
            fs::create_dir_all(&victim_data).expect("mkdir victim_data");
            fs::set_permissions(&victim_data, fs::Permissions::from_mode(0o755))
                .expect("chmod 0755");
            symlink(&victim_local, home.join(".local")).expect("symlink .local");
            fs::write(&victim_rc, "# sym rc\n").expect("write victim_rc");
            symlink(&victim_rc, home.join(rc_filename)).expect("symlink rc");

            let victim_env = victim_data.join("env.sh");
            let enable_out = isolated_cli_command(&home)
                .env_remove("XDG_DATA_HOME")
                .env("ZDOTDIR", &home)
                .args(["integration", "enable", "--shell", shell, "--json"])
                .output()
                .expect("run enable");
            assert_ne!(
                enable_out.status.code(),
                Some(0),
                "dot_local {shell} enable must fail closed even when RC is also a symlink"
            );
            assert!(
                !victim_env.exists(),
                "dot_local {shell}: must not create env.sh in victim_local"
            );
            assert_eq!(
                fs::metadata(&victim_data)
                    .expect("stat victim")
                    .permissions()
                    .mode()
                    & 0o777,
                0o755
            );

            fs::write(&victim_env, "# KEEP_DOT_LOCAL\n").expect("write victim env");
            for subcmd in ["status", "disable"] {
                let out = isolated_cli_command(&home)
                    .env_remove("XDG_DATA_HOME")
                    .env("ZDOTDIR", &home)
                    .args(["integration", subcmd, "--shell", shell, "--json"])
                    .output()
                    .expect("run integration");
                assert_ne!(
                    out.status.code(),
                    Some(0),
                    "dot_local {shell} {subcmd} must fail closed"
                );
            }
            assert_eq!(
                fs::read_to_string(&victim_env).expect("read victim env"),
                "# KEEP_DOT_LOCAL\n"
            );
            assert_eq!(
                fs::read_to_string(&victim_rc).expect("read victim rc"),
                "# sym rc\n"
            );
        }

        // Case (c): XDG_DATA_HOME unset, $HOME/.local/share -> victim_share
        {
            let temp = TempDir::new().expect("tempdir");
            let home = temp.path().join("home");
            let victim_share = temp.path().join("victim_share");
            let victim_data = victim_share.join("agy-companion");
            fs::create_dir_all(home.join(".local")).expect("mkdir .local");
            fs::create_dir_all(&victim_data).expect("mkdir victim_data");
            fs::set_permissions(&victim_data, fs::Permissions::from_mode(0o755))
                .expect("chmod 0755");
            symlink(&victim_share, home.join(".local/share")).expect("symlink .local/share");

            let victim_env = victim_data.join("env.sh");
            let enable_out = isolated_cli_command(&home)
                .env_remove("XDG_DATA_HOME")
                .env("ZDOTDIR", &home)
                .args(["integration", "enable", "--shell", shell, "--json"])
                .output()
                .expect("run enable");
            assert_ne!(enable_out.status.code(), Some(0));
            assert!(!victim_env.exists());
            assert_eq!(
                fs::metadata(&victim_data)
                    .expect("stat victim")
                    .permissions()
                    .mode()
                    & 0o777,
                0o755
            );

            fs::write(&victim_env, "# KEEP_SHARE\n").expect("write victim env");
            for subcmd in ["status", "disable"] {
                let out = isolated_cli_command(&home)
                    .env_remove("XDG_DATA_HOME")
                    .env("ZDOTDIR", &home)
                    .args(["integration", subcmd, "--shell", shell, "--json"])
                    .output()
                    .expect("run integration");
                assert_ne!(out.status.code(), Some(0));
            }
            assert_eq!(
                fs::read_to_string(&victim_env).expect("read victim env"),
                "# KEEP_SHARE\n"
            );
        }

        // Case (d): Explicit XDG_DATA_HOME=$HOME/.local/share/ (with trailing slash) while $HOME/.local -> victim_local
        {
            let temp = TempDir::new().expect("tempdir");
            let home = temp.path().join("home");
            let victim_local = temp.path().join("victim_local");
            let victim_data = victim_local.join("share/agy-companion");
            fs::create_dir_all(&home).expect("mkdir home");
            fs::create_dir_all(&victim_data).expect("mkdir victim_data");
            symlink(&victim_local, home.join(".local")).expect("symlink .local");
            let xdg_eq_default = format!("{}/", home.join(".local/share").display());

            let out = isolated_cli_command(&home)
                .env("XDG_DATA_HOME", &xdg_eq_default)
                .env("ZDOTDIR", &home)
                .args(["integration", "enable", "--shell", shell, "--json"])
                .output()
                .expect("run enable");
            assert_ne!(
                out.status.code(),
                Some(0),
                "explicit XDG_DATA_HOME=$HOME/.local/share/ ({shell}) must fail closed"
            );
            assert!(!victim_data.join("env.sh").exists());
        }
    }
}

#[cfg(unix)]
#[test]
fn install_step1_symlinked_bin_dir_does_not_create_default_gemini_plugins_dir() {
    use std::os::unix::fs::symlink;

    for kind in ["symlink", "regular_file"] {
        let temp = TempDir::new().expect("tempdir");
        let home = temp.path().join("home");
        let xdg_config = temp.path().join("xdg_config");
        let xdg_data = temp.path().join("xdg_data");
        let data_dir = xdg_data.join("agy-companion");
        fs::create_dir_all(&home).expect("mkdir home");
        fs::create_dir_all(&data_dir).expect("mkdir data_dir");

        let victim_bin = temp.path().join("victim_bin");
        if kind == "symlink" {
            fs::create_dir_all(&victim_bin).expect("mkdir victim_bin");
            symlink(&victim_bin, data_dir.join("bin")).expect("symlink bin dir");
        } else {
            fs::write(data_dir.join("bin"), "not a dir\n").expect("write regular bin file");
        }

        assert!(
            !home.join(".gemini").exists(),
            "{kind}: precondition ~/.gemini must not exist before install"
        );

        let out = isolated_cli_command(&home)
            .env("XDG_CONFIG_HOME", &xdg_config)
            .env("XDG_DATA_HOME", &xdg_data)
            .env("ZDOTDIR", &home)
            .args([
                "install",
                "--no-launcher-hook",
                "--no-start-proxy",
                "--json",
            ])
            .output()
            .expect("run install");
        assert_ne!(
            out.status.code(),
            Some(0),
            "{kind}: install must fail closed in Step 1 on unsafe data_dir/bin"
        );
        let stderr = String::from_utf8_lossy(&out.stderr);
        assert!(
            stderr.contains("refusing to traverse symlinked/non-directory bin directory"),
            "{kind}: expected bin directory refusal in Step 1, got stderr={stderr}"
        );
        assert!(
            !home.join(".gemini").exists(),
            "{kind}: Step 1 failure on unsafe data_dir/bin must NOT create ~/.gemini/config/plugins"
        );
        if kind == "symlink" {
            assert!(
                fs::read_dir(&victim_bin)
                    .expect("read victim_bin")
                    .next()
                    .is_none(),
                "victim_bin must remain completely untouched"
            );
        }
    }
}

#[cfg(unix)]
#[test]
fn cli_integration_refuses_symlink_dir_and_fifo_leaf_env_sh_without_mutating_rc() {
    use std::os::unix::fs::symlink;

    for shell in ["zsh", "bash"] {
        let rc_filename = if shell == "zsh" {
            ".zlogin"
        } else {
            ".profile"
        };

        for kind in ["symlink", "directory", "fifo"] {
            let temp = TempDir::new().expect("tempdir");
            let home = temp.path().join("home");
            let xdg_data = temp.path().join("xdg_data");
            let data_dir = xdg_data.join("agy-companion");
            fs::create_dir_all(&home).expect("mkdir home");
            fs::create_dir_all(&data_dir).expect("mkdir data_dir");

            let env_path = data_dir.join(companion_core::integration::ENV_SCRIPT_FILENAME);
            let victim_env = temp.path().join("victim_env.sh");
            let sentinel = "# VICTIM_CLI_LEAF\n";
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

            let rc_path = home.join(rc_filename);
            let rc_before = format!(
                "{}\n[ -r \"${{XDG_DATA_HOME:-$HOME/.local/share}}/agy-companion/env.sh\" ] && . \"${{XDG_DATA_HOME:-$HOME/.local/share}}/agy-companion/env.sh\"\n{}\n",
                companion_core::HOOK_BEGIN_MARKER,
                companion_core::HOOK_END_MARKER
            );
            fs::write(&rc_path, &rc_before).expect("write marker-only rc");

            for subcmd in ["status", "enable", "disable"] {
                let out = isolated_cli_command(&home)
                    .env("XDG_DATA_HOME", &xdg_data)
                    .env("ZDOTDIR", &home)
                    .args(["integration", subcmd, "--shell", shell, "--json"])
                    .output()
                    .expect("run integration");
                assert_ne!(
                    out.status.code(),
                    Some(0),
                    "{kind} ({shell}) {subcmd} must fail closed on unsafe leaf env.sh; stdout={}",
                    String::from_utf8_lossy(&out.stdout)
                );
            }

            assert!(
                rc_path.is_file(),
                "{kind} ({shell}): marker-only RC file must NOT be deleted on failed disable"
            );
            assert_eq!(
                fs::read_to_string(&rc_path).expect("read rc"),
                rc_before,
                "{kind} ({shell}): RC file must remain byte-for-byte intact"
            );
            if kind == "symlink" {
                assert_eq!(
                    fs::read_to_string(&victim_env).expect("read victim"),
                    sentinel,
                    "{kind} ({shell}): victim_env.sh must remain untouched"
                );
            }
        }
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
fn install_step1_unsafe_leaf_env_sh_fails_before_creating_dirs_unless_fish_or_no_hook() {
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
        let victim_env = temp.path().join("victim_install_env.sh");
        let sentinel = "# VICTIM_INSTALL_LEAF\n";
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

        assert!(!home.join(".gemini").exists());
        assert!(!data_dir.join("bin").exists());

        let out = isolated_cli_command(&home)
            .env("XDG_CONFIG_HOME", &xdg_config)
            .env("XDG_DATA_HOME", &xdg_data)
            .env("SHELL", "/bin/zsh")
            .env("ZDOTDIR", &home)
            .args(["install", "--no-start-proxy", "--json"])
            .output()
            .expect("run install");

        assert_ne!(
            out.status.code(),
            Some(0),
            "{kind}: install must fail closed in Step 1 on unsafe data_dir/env.sh"
        );
        let stderr = String::from_utf8_lossy(&out.stderr);
        let expected_err = if kind == "symlink" {
            "refusing to use symlinked env script"
        } else {
            "refusing to use non-regular env script"
        };
        assert!(
            stderr.contains(expected_err),
            "{kind}: expected Step 1 error '{expected_err}', got stderr={stderr}"
        );
        assert!(
            !home.join(".gemini").exists(),
            "{kind}: Step 1 failure on unsafe data_dir/env.sh must NOT create ~/.gemini"
        );
        assert!(
            !data_dir.join("bin").exists(),
            "{kind}: Step 1 failure on unsafe data_dir/env.sh must NOT create data_dir/bin"
        );
        if kind == "symlink" {
            assert_eq!(
                fs::read_to_string(&victim_env).expect("read victim"),
                sentinel,
                "{kind}: victim_install_env.sh must stay untouched"
            );
        }
    }

    // Verify SHELL=/usr/local/bin/fish and --no-launcher-hook are NOT erroneously blocked by env.sh.
    for mode in ["fish", "no_launcher_hook"] {
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
        let victim_env = temp.path().join("victim_unblocked_env.sh");
        let sentinel = "# UNBLOCKED_VICTIM\n";
        fs::write(&victim_env, sentinel).expect("write victim");
        symlink(&victim_env, &env_path).expect("symlink env.sh");

        let mut cmd = isolated_cli_command(&home);
        cmd.env("XDG_CONFIG_HOME", &xdg_config)
            .env("XDG_DATA_HOME", &xdg_data)
            .env("ZDOTDIR", &home);
        if mode == "fish" {
            cmd.env("SHELL", "/usr/local/bin/fish")
                .args(["install", "--no-start-proxy", "--json"]);
        } else {
            cmd.env("SHELL", "/bin/zsh").args([
                "install",
                "--no-launcher-hook",
                "--no-start-proxy",
                "--json",
            ]);
        }

        let out = cmd.output().expect("run unblocked install");
        assert_eq!(
            out.status.code(),
            Some(0),
            "{mode}: install must NOT be blocked by symlinked env.sh; stderr={}",
            String::from_utf8_lossy(&out.stderr)
        );
        let rep: Value = serde_json::from_slice(&out.stdout).expect("parse install json");
        assert_eq!(rep["ok"], true);
        assert!(
            home.join(".gemini/config/plugins/agy-companion/plugin.json")
                .is_file()
        );
        assert!(data_dir.join("bin/agy-companion").is_file());
        assert_eq!(
            fs::read_to_string(&victim_env).expect("read victim"),
            sentinel,
            "{mode}: victim_unblocked_env.sh must remain untouched"
        );
    }
}

#[test]
fn install_and_uninstall_respect_explicit_config_flag_and_fail_closed_on_invalid_or_symlink() {
    let temp = TempDir::new().expect("tempdir");
    let home = temp.path().join("home");
    let xdg_config = temp.path().join("xdg_config");
    let xdg_data = temp.path().join("xdg_data");
    let plugins_dir = temp.path().join("plugins");
    fs::create_dir_all(&home).expect("mkdir home");

    // Seed default XDG config with a different port so we prove --config overrides XDG.
    let default_cfg_path = xdg_config.join("agy-companion").join("config.json");
    config_with_port(24191)
        .save_secure(&default_cfg_path)
        .expect("save default xdg config");

    // Custom config in a non-`agy-companion` directory with a distinct port.
    let custom_dir = temp.path().join("custom_cfg_dir");
    fs::create_dir_all(&custom_dir).expect("mkdir custom dir");
    let keep_sibling = custom_dir.join("keep.txt");
    fs::write(&keep_sibling, "keep").expect("write sibling");
    let custom_cfg = custom_dir.join("my-companion.json");
    let custom_port = 24149u16;
    config_with_port(custom_port)
        .save_secure(&custom_cfg)
        .expect("save custom config");

    // 1. `install --config <custom_cfg>` honors custom port and ignores XDG config.
    let install_out = isolated_cli_command(&home)
        .env("XDG_CONFIG_HOME", &xdg_config)
        .env("XDG_DATA_HOME", &xdg_data)
        .env("ZDOTDIR", &home)
        .args([
            "install",
            "--config",
            custom_cfg.to_str().expect("utf8 cfg"),
            "--plugins-dir",
            plugins_dir.to_str().expect("utf8 plugins"),
            "--no-launcher-hook",
            "--no-start-proxy",
            "--json",
        ])
        .output()
        .expect("run install --config");
    assert_eq!(
        install_out.status.code(),
        Some(0),
        "install --config failed: stderr={}",
        String::from_utf8_lossy(&install_out.stderr)
    );
    let install_rep: Value =
        serde_json::from_slice(&install_out.stdout).expect("parse install json");
    assert_eq!(install_rep["ok"], true);
    assert_eq!(install_rep["listen_port"], custom_port);

    // 2. `install --config ../escape.json` fails closed before creating plugin dir.
    let traversal_plugins = temp.path().join("traversal_plugins");
    let bad_install = isolated_cli_command(&home)
        .env("XDG_CONFIG_HOME", &xdg_config)
        .env("XDG_DATA_HOME", &xdg_data)
        .env("ZDOTDIR", &home)
        .args([
            "install",
            "--config",
            "../escape.json",
            "--plugins-dir",
            traversal_plugins.to_str().expect("utf8 plugins"),
            "--no-launcher-hook",
            "--no-start-proxy",
            "--json",
        ])
        .output()
        .expect("run traversal install");
    assert_ne!(bad_install.status.code(), Some(0));
    assert!(
        !traversal_plugins.exists(),
        "invalid --config path must fail closed before creating directories"
    );

    // 3. `uninstall --config <custom_cfg>` (no --purge) preserves custom_cfg in preserved_paths.
    let uninstall_preserve = isolated_cli_command(&home)
        .env("XDG_CONFIG_HOME", &xdg_config)
        .env("XDG_DATA_HOME", &xdg_data)
        .env("ZDOTDIR", &home)
        .args([
            "uninstall",
            "--config",
            custom_cfg.to_str().expect("utf8 cfg"),
            "--plugins-dir",
            plugins_dir.to_str().expect("utf8 plugins"),
            "--json",
        ])
        .output()
        .expect("run uninstall preserve");
    assert_eq!(uninstall_preserve.status.code(), Some(0));
    let preserve_rep: Value =
        serde_json::from_slice(&uninstall_preserve.stdout).expect("parse preserve json");
    assert!(
        custom_cfg.is_file(),
        "non-purge uninstall must preserve config"
    );
    let preserved = preserve_rep["preserved_paths"]
        .as_array()
        .expect("preserved_paths");
    assert!(
        preserved
            .iter()
            .any(|p| p.as_str() == Some(custom_cfg.to_str().expect("utf8")))
    );

    // 4. `uninstall --config <custom_cfg> --purge` removes custom_cfg, keeps default XDG config,
    //    and NEVER removes the non-`agy-companion` custom parent directory.
    let uninstall_purge = isolated_cli_command(&home)
        .env("XDG_CONFIG_HOME", &xdg_config)
        .env("XDG_DATA_HOME", &xdg_data)
        .env("ZDOTDIR", &home)
        .args([
            "uninstall",
            "--config",
            custom_cfg.to_str().expect("utf8 cfg"),
            "--plugins-dir",
            plugins_dir.to_str().expect("utf8 plugins"),
            "--purge",
            "--json",
        ])
        .output()
        .expect("run uninstall purge");
    assert_eq!(uninstall_purge.status.code(), Some(0));
    let purge_rep: Value =
        serde_json::from_slice(&uninstall_purge.stdout).expect("parse purge json");
    assert!(
        !custom_cfg.exists(),
        "--purge must delete explicit --config file"
    );
    assert!(
        default_cfg_path.is_file(),
        "default XDG config must remain untouched when explicit --config is purged"
    );
    assert!(
        custom_dir.is_dir() && keep_sibling.is_file(),
        "non-agy-companion parent directory must never be deleted"
    );
    let purged = purge_rep["purged_paths"].as_array().expect("purged_paths");
    assert!(
        purged
            .iter()
            .any(|p| p.as_str() == Some(custom_cfg.to_str().expect("utf8")))
    );

    // 5. Symlinked parent directory of `--config` fails closed on install and warns/untouched on uninstall --purge.
    #[cfg(unix)]
    {
        use std::os::unix::fs::symlink;

        let victim_dir = temp.path().join("victim_cfg_dir");
        fs::create_dir_all(&victim_dir).expect("mkdir victim");
        let victim_cfg = victim_dir.join("config.json");
        config_with_port(24155)
            .save_secure(&victim_cfg)
            .expect("save victim cfg");
        let sym_parent = temp.path().join("sym_cfg_parent");
        symlink(&victim_dir, &sym_parent).expect("symlink cfg parent");
        let sym_cfg = sym_parent.join("config.json");

        let sym_install = isolated_cli_command(&home)
            .env("XDG_CONFIG_HOME", &xdg_config)
            .env("XDG_DATA_HOME", &xdg_data)
            .env("ZDOTDIR", &home)
            .args([
                "install",
                "--config",
                sym_cfg.to_str().expect("utf8 sym cfg"),
                "--plugins-dir",
                plugins_dir.to_str().expect("utf8 plugins"),
                "--no-launcher-hook",
                "--no-start-proxy",
                "--json",
            ])
            .output()
            .expect("run symlink config install");
        assert_ne!(
            sym_install.status.code(),
            Some(0),
            "install must reject symlinked --config parent dir"
        );

        let sym_uninstall = isolated_cli_command(&home)
            .env("XDG_CONFIG_HOME", &xdg_config)
            .env("XDG_DATA_HOME", &xdg_data)
            .env("ZDOTDIR", &home)
            .args([
                "uninstall",
                "--config",
                sym_cfg.to_str().expect("utf8 sym cfg"),
                "--plugins-dir",
                plugins_dir.to_str().expect("utf8 plugins"),
                "--purge",
                "--json",
            ])
            .output()
            .expect("run symlink config uninstall");
        assert_eq!(sym_uninstall.status.code(), Some(0));
        assert!(
            victim_cfg.is_file(),
            "uninstall --purge must NOT delete config through symlinked --config parent dir"
        );
        let sym_rep: Value =
            serde_json::from_slice(&sym_uninstall.stdout).expect("parse sym uninstall json");
        let warnings = sym_rep["warnings"].as_array().expect("warnings");
        assert!(
            warnings.iter().any(|w| {
                w.as_str()
                    .is_some_and(|s| s.contains("refusing to traverse symlinked/non-directory"))
            }),
            "expected symlink refusal warning, got {warnings:?}"
        );
    }
}
