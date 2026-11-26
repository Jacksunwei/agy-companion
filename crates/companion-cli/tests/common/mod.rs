#![allow(dead_code)]

use std::ffi::{OsStr, OsString};
use std::fs;
use std::io::{BufReader, Write};
use std::net::{Ipv4Addr, TcpListener as StdTcpListener};
use std::num::NonZeroU16;
use std::path::Path;
use std::process::{Child, ChildStderr, ChildStdin, ChildStdout, Command, ExitStatus, Stdio};
use std::time::{Duration, Instant};

use axum::Router;
use axum::routing::get;
use companion_core::Config;
use companion_proxy::HealthResponse;
use serde_json::{Value, json};
use tokio::net::TcpListener;

pub const BIN: &str = env!("CARGO_BIN_EXE_agy-companion");

const ISOLATED_PASSTHROUGH_ENV_VARS: &[&str] = &[
    "PATH",
    "LANG",
    "LC_ALL",
    "SSL_CERT_FILE",
    "SSL_CERT_DIR",
    "SystemRoot",
    "SYSTEMROOT",
    "COMSPEC",
    "ComSpec",
    "PATHEXT",
    "WINDIR",
];

pub fn config_with_port(port: u16) -> Config {
    let mut cfg = Config::default();
    cfg.proxy.port = NonZeroU16::new(port).expect("non-zero port");
    cfg
}

pub fn loopback_http_client() -> &'static reqwest::Client {
    static CLIENT: std::sync::OnceLock<reqwest::Client> = std::sync::OnceLock::new();
    CLIENT.get_or_init(|| {
        reqwest::Client::builder()
            .no_proxy()
            .timeout(Duration::from_secs(10))
            .build()
            .expect("build loopback http client")
    })
}

pub struct ChildGuard {
    child: Option<Child>,
}

impl ChildGuard {
    pub fn new(child: Child) -> Self {
        Self { child: Some(child) }
    }

    pub fn id(&self) -> u32 {
        self.child.as_ref().expect("child present").id()
    }

    pub fn take_stdin(&mut self) -> Option<ChildStdin> {
        self.child.as_mut().expect("child present").stdin.take()
    }

    pub fn take_stdout(&mut self) -> Option<ChildStdout> {
        self.child.as_mut().expect("child present").stdout.take()
    }

    pub fn take_stderr(&mut self) -> Option<ChildStderr> {
        self.child.as_mut().expect("child present").stderr.take()
    }

    pub fn try_wait(&mut self) -> std::io::Result<Option<ExitStatus>> {
        self.child.as_mut().expect("child present").try_wait()
    }

    pub fn wait(mut self) -> std::io::Result<ExitStatus> {
        self.child.take().expect("child present").wait()
    }

    pub fn kill_and_wait(mut self) -> std::io::Result<ExitStatus> {
        let mut child = self.child.take().expect("child present");
        let _ = child.kill();
        child.wait()
    }

    pub fn wait_with_output(mut self) -> std::io::Result<std::process::Output> {
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

pub struct DetachedPidFileGuard {
    pid_path: std::path::PathBuf,
}

impl DetachedPidFileGuard {
    pub fn new(pid_path: std::path::PathBuf) -> Self {
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
                std::thread::sleep(Duration::from_millis(20));
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

fn isolated_env_pairs(home: &Path) -> Vec<(OsString, OsString)> {
    let tmp_dir = home.join(".test-tmp");
    fs::create_dir_all(&tmp_dir).expect("create isolated TMPDIR");

    let mut pairs = Vec::with_capacity(ISOLATED_PASSTHROUGH_ENV_VARS.len() + 10);
    for var in ISOLATED_PASSTHROUGH_ENV_VARS {
        if let Some(val) = std::env::var_os(var) {
            pairs.push((OsString::from(var), val));
        }
    }

    let xdg_config = home.join("xdg-config");
    let xdg_data = home.join("xdg-data");
    let xdg_cache = home.join("xdg-cache");
    pairs.push((OsString::from("HOME"), home.as_os_str().to_os_string()));
    pairs.push((
        OsString::from("XDG_CONFIG_HOME"),
        xdg_config.clone().into_os_string(),
    ));
    pairs.push((
        OsString::from("XDG_DATA_HOME"),
        xdg_data.clone().into_os_string(),
    ));
    pairs.push((OsString::from("XDG_CACHE_HOME"), xdg_cache.into_os_string()));
    pairs.push((OsString::from("TMPDIR"), tmp_dir.clone().into_os_string()));
    pairs.push((OsString::from("TEMP"), tmp_dir.clone().into_os_string()));
    pairs.push((OsString::from("TMP"), tmp_dir.into_os_string()));
    pairs.push((
        OsString::from("USERPROFILE"),
        home.as_os_str().to_os_string(),
    ));
    pairs.push((OsString::from("APPDATA"), xdg_config.into_os_string()));
    pairs.push((OsString::from("LOCALAPPDATA"), xdg_data.into_os_string()));
    pairs
}

pub fn isolated_cli_command(home: &Path) -> Command {
    let mut cmd = Command::new(BIN);
    cmd.env_clear();
    cmd.envs(isolated_env_pairs(home));
    cmd
}

pub fn isolated_tokio_cli_command(home: &Path) -> tokio::process::Command {
    let mut cmd = tokio::process::Command::new(BIN);
    cmd.env_clear();
    cmd.envs(isolated_env_pairs(home));
    cmd
}

const SERVE_START_MAX_ATTEMPTS: usize = 5;

pub fn free_loopback_port() -> u16 {
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
        if (24_140..=24_160).contains(&candidate) || state.claimed.contains(&candidate) {
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

pub fn proxy_pid_matches_child(
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

pub async fn spawn_serve_with_port_picker<P, F>(
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
        assert!(
            cfg.usage_dir.is_some(),
            "spawn_serve_with_port_picker requires an isolated cfg.usage_dir"
        );
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

            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    }

    panic!(
        "serve failed to bind after {SERVE_START_MAX_ATTEMPTS} attempts due to AddrInUse; last stderr={last_addr_in_use}"
    );
}

pub async fn spawn_serve_and_wait_healthy<F>(
    client: &reqwest::Client,
    cfg_path: &std::path::Path,
    build_cmd: F,
) -> (ChildGuard, u16, reqwest::Response)
where
    F: FnMut(u16) -> Command,
{
    let initial_port = Config::load_or_default(cfg_path)
        .ok()
        .map(|cfg| cfg.proxy.port.get())
        .filter(|port| !(24_140..=24_160).contains(port));
    spawn_serve_with_port_picker(
        client,
        cfg_path,
        move |attempt| {
            if attempt == 1
                && let Some(port) = initial_port
            {
                return port;
            }
            free_loopback_port()
        },
        build_cmd,
    )
    .await
}

pub async fn spawn_mock_health(
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
    tokio::task::yield_now().await;
    (port, handle)
}

pub fn run_ui_mcp_bounded_with_env(
    home: &Path,
    config_path: &Path,
    data_dir: &Path,
    extra_env: &[(&str, &OsStr)],
    calls: Vec<Value>,
    timeout: Duration,
) -> (Option<i32>, String, String, bool) {
    let mut cmd = isolated_cli_command(home);
    cmd.arg("ui-mcp")
        .arg(format!("--data-dir={}", data_dir.display()))
        .arg("--config")
        .arg(config_path)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    for (k, v) in extra_env {
        cmd.env(k, v);
    }
    let mut guard = ChildGuard::new(cmd.spawn().expect("spawn ui-mcp"));

    let stdin = guard.take_stdin().expect("stdin");
    let stdout = guard.take_stdout().expect("stdout");
    let stderr = guard.take_stderr().expect("stderr");

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
        match guard.try_wait().expect("try_wait ui-mcp") {
            Some(status) => break status.code(),
            None if Instant::now() < deadline => {
                std::thread::sleep(Duration::from_millis(20));
            }
            None => {
                timed_out = true;
                let status = guard.kill_and_wait().expect("wait killed ui-mcp");
                break status.code();
            }
        }
    };

    let _ = writer_thread.join();
    let stdout_str = stdout_thread.join().expect("join stdout");
    let stderr_str = stderr_thread.join().expect("join stderr");
    (exit_code, stdout_str, stderr_str, timed_out)
}

pub fn run_ui_mcp_bounded_with_calls(
    home: &Path,
    config_path: &Path,
    data_dir: &Path,
    calls: Vec<Value>,
    timeout: Duration,
) -> (Option<i32>, String, String, bool) {
    run_ui_mcp_bounded_with_env(home, config_path, data_dir, &[], calls, timeout)
}

pub fn run_ui_mcp_bounded(
    home: &Path,
    config_path: &Path,
    data_dir: &Path,
    timeout: Duration,
) -> (Option<i32>, String, String, bool) {
    run_ui_mcp_bounded_with_calls(
        home,
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
