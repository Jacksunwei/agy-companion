use std::sync::Arc;
use std::time::Duration;
#[cfg(unix)]
use std::time::Instant;

use companion_cli::daemon::read_proxy_pid;
use companion_core::{ApiKey, AuthStore, FileAuthStore, GeminiCredentials, HealthResponse};
use tempfile::TempDir;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;
use tokio::sync::Notify;

#[cfg(unix)]
use crate::common::{ChildGuard, DetachedPidFileGuard};
use crate::common::{
    config_with_port, free_loopback_port, isolated_cli_command, loopback_http_client,
    spawn_serve_and_wait_healthy, spawn_with_captured_files,
};

async fn spawn_notify_gated_sse_upstream()
-> (u16, Arc<Notify>, Arc<Notify>, tokio::task::JoinHandle<()>) {
    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind mock sse upstream");
    let port = listener.local_addr().expect("upstream addr").port();
    let chunk1_sent = Arc::new(Notify::new());
    let release_chunk2 = Arc::new(Notify::new());
    let chunk1_sent_bg = Arc::clone(&chunk1_sent);
    let release_chunk2_bg = Arc::clone(&release_chunk2);

    let handle = tokio::spawn(async move {
        if let Ok((mut stream, _)) = listener.accept().await {
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

            chunk1_sent_bg.notify_one();
            release_chunk2_bg.notified().await;

            let chunk2 = b"data: {\"candidates\":[{\"content\":{\"parts\":[{\"text\":\"part2\"}]}}],\"usageMetadata\":{\"promptTokenCount\":10,\"candidatesTokenCount\":5}}\n\n";
            let _ = stream
                .write_all(format!("{:x}\r\n", chunk2.len()).as_bytes())
                .await;
            let _ = stream.write_all(chunk2).await;
            let _ = stream.write_all(b"\r\n0\r\n\r\n").await;
            let _ = stream.flush().await;
        }
    });
    tokio::task::yield_now().await;
    (port, chunk1_sent, release_chunk2, handle)
}

fn write_test_config_with_upstream(
    temp: &TempDir,
    upstream_port: Option<u16>,
) -> std::path::PathBuf {
    let cfg_path = temp.path().join("config.json");
    let usage_dir = temp.path().join("usage");

    let mut cfg = config_with_port(free_loopback_port());
    if let Some(port) = upstream_port {
        cfg.providers.gemini.base_url = format!("http://127.0.0.1:{port}");
    }
    cfg.usage_dir = Some(usage_dir);
    cfg.save_secure(&cfg_path).expect("save_secure");
    FileAuthStore::for_config_path(&cfg_path)
        .expect("auth store")
        .save_gemini(&GeminiCredentials::new(
            ApiKey::parse_strict("AIzaSyLifecycleTestKey1").unwrap(),
        ))
        .expect("save gemini auth");
    cfg_path
}

async fn poll_health(client: &reqwest::Client, port: u16) -> Option<HealthResponse> {
    let resp = client
        .get(format!("http://127.0.0.1:{port}/health"))
        .timeout(Duration::from_millis(250))
        .send()
        .await
        .ok()?;
    if resp.status() != reqwest::StatusCode::OK {
        return None;
    }
    resp.json::<HealthResponse>().await.ok()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn serve_watch_pid_waits_for_in_flight_stream_then_exits_promptly() {
    let (upstream_port, chunk1_sent, release_chunk2, upstream_handle) =
        spawn_notify_gated_sse_upstream().await;

    // Sacrificial watched process (cross-platform: another isolated serve instance on its own port).
    let watched_temp = TempDir::new().expect("watched tempdir");
    let watched_cfg = write_test_config_with_upstream(&watched_temp, None);
    let client = loopback_http_client();
    let (watched_guard, _watched_port, _) =
        spawn_serve_and_wait_healthy(client, &watched_cfg, |port| {
            let mut cmd = isolated_cli_command(watched_temp.path());
            cmd.args(["serve", "--port", &port.to_string(), "--config"])
                .arg(&watched_cfg);
            cmd
        })
        .await;
    let watched_pid = watched_guard.id();

    let temp = TempDir::new().expect("target tempdir");
    let cfg_path = write_test_config_with_upstream(&temp, Some(upstream_port));

    let (mut target_guard, port, initial_resp) =
        spawn_serve_and_wait_healthy(client, &cfg_path, |port| {
            let mut cmd = isolated_cli_command(temp.path());
            cmd.args(["serve", "--port", &port.to_string(), "--config"])
                .arg(&cfg_path)
                .args([
                    "--watch-pid",
                    &watched_pid.to_string(),
                    "--idle-shutdown-secs",
                    "1800",
                ]);
            cmd
        })
        .await;
    let initial_health: HealthResponse = initial_resp.json().await.expect("parse initial health");
    assert_eq!(initial_health.role.as_deref(), Some("bootstrap"));

    let pid_path = temp.path().join("proxy.pid");
    assert!(pid_path.is_file());

    // Start an in-flight SSE stream and wait until chunk 1 has been sent by upstream.
    let stream_client = client.clone();
    let stream_task = tokio::spawn(async move {
        let resp = stream_client
            .post(format!(
                "http://127.0.0.1:{port}/v1beta/models/gemini-4-hydrogen:streamGenerateContent?alt=sse"
            ))
            .header("content-type", "application/json")
            .body(r#"{"contents":[{"parts":[{"text":"hi"}]}]}"#)
            .send()
            .await
            .expect("stream request");
        assert_eq!(resp.status(), reqwest::StatusCode::OK);
        resp.text().await.expect("read full stream")
    });

    tokio::time::timeout(Duration::from_secs(3), chunk1_sent.notified())
        .await
        .expect("upstream should send chunk 1");

    // Terminate the watched parent PID while the SSE stream is still in flight.
    watched_guard.kill_and_wait().expect("kill watched process");

    // Wait longer than WATCH_PID_POLL_INTERVAL (500ms); proxy MUST remain alive while streaming.
    tokio::time::sleep(Duration::from_millis(750)).await;
    assert!(
        target_guard.try_wait().expect("try_wait").is_none(),
        "proxy must wait for in-flight SSE stream before exiting after watched PID dies"
    );

    // Release chunk 2 + EOF so the SSE stream completes cleanly.
    release_chunk2.notify_one();
    let stream_body = tokio::time::timeout(Duration::from_secs(3), stream_task)
        .await
        .expect("stream task should finish")
        .expect("join stream task");
    assert!(stream_body.contains("part1"));
    assert!(stream_body.contains("part2"));

    // Now that in_flight_requests == 0, proxy must exit 0 promptly (far below 1800s idle fallback).
    let output = tokio::time::timeout(
        Duration::from_secs(4),
        tokio::task::spawn_blocking(move || target_guard.wait_with_output()),
    )
    .await
    .expect("serve did not exit promptly after watched PID died and stream drained")
    .expect("join wait")
    .expect("wait_with_output");
    assert_eq!(
        output.status.code(),
        Some(0),
        "--watch-pid shutdown must exit 0; stderr={}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        !pid_path.exists(),
        "proxy.pid must be removed on clean --watch-pid shutdown"
    );

    upstream_handle.abort();
}

#[cfg(unix)]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn serve_replace_stale_hands_off_port_without_dropping_active_sse_or_clobbering_pid() {
    let (upstream_port, chunk1_sent, release_chunk2, upstream_handle) =
        spawn_notify_gated_sse_upstream().await;

    let temp = TempDir::new().expect("tempdir");
    let cfg_path = write_test_config_with_upstream(&temp, Some(upstream_port));
    let pid_path = temp.path().join("proxy.pid");
    let client = loopback_http_client();

    // 1. Spawn stale-version daemon using debug test health version override.
    let (old_guard, port, old_resp) = spawn_serve_and_wait_healthy(client, &cfg_path, |port| {
        let mut cmd = isolated_cli_command(temp.path());
        cmd.env("AGY_COMPANION_TEST_HEALTH_VERSION", "0.0.1-stale")
            .args(["serve", "--port", &port.to_string(), "--config"])
            .arg(&cfg_path);
        cmd
    })
    .await;
    let old_pid = old_guard.id();
    let old_health: HealthResponse = old_resp.json().await.expect("parse old health");
    assert_eq!(old_health.version, "0.0.1-stale");
    assert_eq!(
        read_proxy_pid(&pid_path)
            .expect("read old pid")
            .expect("some")
            .pid,
        old_pid
    );

    // 2. Open an active SSE stream on the stale daemon and wait until chunk 1 is sent.
    let stream_client = client.clone();
    let stream_task = tokio::spawn(async move {
        let resp = stream_client
            .post(format!(
                "http://127.0.0.1:{port}/v1beta/models/gemini-4-hydrogen:streamGenerateContent?alt=sse"
            ))
            .header("content-type", "application/json")
            .body(r#"{"contents":[{"parts":[{"text":"hi"}]}]}"#)
            .send()
            .await
            .expect("stream request");
        assert_eq!(resp.status(), reqwest::StatusCode::OK);
        resp.text().await.expect("read full stream")
    });
    tokio::time::timeout(Duration::from_secs(3), chunk1_sent.notified())
        .await
        .expect("upstream should send chunk 1");

    // 3. Spawn replacement daemon with `--replace-stale` while the SSE stream is still open.
    let mut new_cmd = isolated_cli_command(temp.path());
    new_cmd
        .args(["serve", "--port", &port.to_string(), "--config"])
        .arg(&cfg_path)
        .arg("--replace-stale");
    let new_guard = ChildGuard::new(new_cmd.spawn().expect("spawn --replace-stale"));
    let new_pid = new_guard.id();

    // 4. New daemon must reclaim `:port` and report current version WHILE old SSE stream is open.
    let handoff_deadline = Instant::now() + Duration::from_secs(3);
    let mut upgraded_health = None;
    while Instant::now() < handoff_deadline {
        if let Some(health) = poll_health(client, port).await
            && health.version == env!("CARGO_PKG_VERSION")
        {
            upgraded_health = Some(health);
            break;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    let upgraded_health = upgraded_health.expect("new daemon must take over port before SSE ends");
    assert_eq!(upgraded_health.role.as_deref(), Some("bootstrap"));

    let mid_pid_info = read_proxy_pid(&pid_path)
        .expect("read new pid")
        .expect("proxy.pid should exist");
    assert_eq!(mid_pid_info.pid, new_pid);
    assert_ne!(new_pid, old_pid);

    // 5. Release chunk 2 so the old daemon finishes draining the active SSE stream cleanly.
    release_chunk2.notify_one();
    let stream_body = tokio::time::timeout(Duration::from_secs(3), stream_task)
        .await
        .expect("active SSE stream should complete without drop")
        .expect("join stream task");
    assert!(stream_body.contains("part1"));
    assert!(stream_body.contains("part2"));

    // 6. Old daemon must exit 0 and MUST NOT delete the new daemon's proxy.pid.
    let old_output = tokio::time::timeout(
        Duration::from_secs(4),
        tokio::task::spawn_blocking(move || old_guard.wait_with_output()),
    )
    .await
    .expect("old daemon should exit after draining SSE")
    .expect("join old wait")
    .expect("old wait_with_output");
    assert_eq!(
        old_output.status.code(),
        Some(0),
        "old daemon must exit 0 after graceful drain; stderr={}",
        String::from_utf8_lossy(&old_output.stderr)
    );

    let final_pid_info = read_proxy_pid(&pid_path)
        .expect("read final pid")
        .expect("old daemon exit must not delete new daemon proxy.pid");
    assert_eq!(final_pid_info.pid, new_pid);

    let final_health = poll_health(client, port)
        .await
        .expect("new daemon must stay healthy after old daemon exits");
    assert_eq!(final_health.version, env!("CARGO_PKG_VERSION"));

    drop(new_guard);
    upstream_handle.abort();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn serve_replace_stale_against_matching_version_exits_cleanly_without_killing_owner() {
    let temp = TempDir::new().expect("tempdir");
    let cfg_path = write_test_config_with_upstream(&temp, None);
    let pid_path = temp.path().join("proxy.pid");
    let client = loopback_http_client();

    let (mut owner_guard, port, owner_resp) =
        spawn_serve_and_wait_healthy(client, &cfg_path, |port| {
            let mut cmd = isolated_cli_command(temp.path());
            cmd.args(["serve", "--port", &port.to_string(), "--config"])
                .arg(&cfg_path);
            cmd
        })
        .await;
    let owner_pid = owner_guard.id();
    let owner_health: HealthResponse = owner_resp.json().await.expect("parse owner health");
    assert_eq!(owner_health.version, env!("CARGO_PKG_VERSION"));
    assert_eq!(
        read_proxy_pid(&pid_path)
            .expect("read owner pid")
            .expect("owner pid")
            .pid,
        owner_pid
    );

    // Launch two concurrent `--replace-stale` invocations against the matching-version owner.
    let mut cmd1 = isolated_cli_command(temp.path());
    cmd1.args(["serve", "--port", &port.to_string(), "--config"])
        .arg(&cfg_path)
        .arg("--replace-stale");
    let mut cmd2 = isolated_cli_command(temp.path());
    cmd2.args(["serve", "--port", &port.to_string(), "--config"])
        .arg(&cfg_path)
        .arg("--replace-stale");

    let child1 =
        spawn_with_captured_files(&mut cmd1, temp.path(), "replace1").expect("spawn replace1");
    let child2 =
        spawn_with_captured_files(&mut cmd2, temp.path(), "replace2").expect("spawn replace2");

    let (out1, out2) = tokio::time::timeout(
        Duration::from_secs(5),
        tokio::task::spawn_blocking(move || (child1.wait_with_output(), child2.wait_with_output())),
    )
    .await
    .expect("concurrent --replace-stale processes should exit promptly")
    .expect("join concurrent replace");

    let out1 = out1.expect("out1");
    let out2 = out2.expect("out2");
    assert_eq!(
        out1.status.code(),
        Some(0),
        "replace1 must exit 0; stderr={}",
        String::from_utf8_lossy(&out1.stderr)
    );
    assert_eq!(
        out2.status.code(),
        Some(0),
        "replace2 must exit 0; stderr={}",
        String::from_utf8_lossy(&out2.stderr)
    );

    // Owner must remain untouched and healthy.
    assert!(
        owner_guard.try_wait().expect("try_wait owner").is_none(),
        "matching-version owner must not be killed by --replace-stale"
    );
    assert_eq!(
        read_proxy_pid(&pid_path)
            .expect("read owner pid after")
            .expect("owner pid after")
            .pid,
        owner_pid
    );
    let health_after = poll_health(client, port)
        .await
        .expect("owner must remain healthy");
    assert_eq!(health_after.version, env!("CARGO_PKG_VERSION"));
}

#[cfg(unix)]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn sidecar_adopt_running_spawns_detached_replacement_on_version_skew_with_scrubbed_sidecar_env()
 {
    let temp = TempDir::new().expect("tempdir");
    let cfg_path = write_test_config_with_upstream(&temp, None);
    let pid_path = temp.path().join("proxy.pid");
    let client = loopback_http_client();

    // RAII guard ensures any detached replacement daemon written to proxy.pid is cleaned up.
    let _detached_cleanup = DetachedPidFileGuard::new(pid_path.clone());

    // 1. Spawn stale-version daemon.
    let (old_guard, port, old_resp) = spawn_serve_and_wait_healthy(client, &cfg_path, |port| {
        let mut cmd = isolated_cli_command(temp.path());
        cmd.env("AGY_COMPANION_TEST_HEALTH_VERSION", "0.0.1-stale")
            .args(["serve", "--port", &port.to_string(), "--config"])
            .arg(&cfg_path);
        cmd
    })
    .await;
    let old_pid = old_guard.id();
    let old_health: HealthResponse = old_resp.json().await.expect("parse old health");
    assert_eq!(old_health.version, "0.0.1-stale");

    // 2. Spawn sidecar with `--adopt-running` and sidecar Web UI env vars.
    let sidecar_web_port = free_loopback_port();
    let mut sidecar_cmd = isolated_cli_command(temp.path());
    sidecar_cmd
        .env("ANTIGRAVITY_SIDECAR_WEB_PORT", sidecar_web_port.to_string())
        .env("ANTIGRAVITY_SIDECAR_UI_TOKEN", "test-sidecar-token")
        .args(["serve", "--port", &port.to_string(), "--config"])
        .arg(&cfg_path)
        .arg("--adopt-running");
    let sidecar_guard = ChildGuard::new(sidecar_cmd.spawn().expect("spawn sidecar"));
    let sidecar_pid = sidecar_guard.id();

    // 3. Wait for sidecar to detect version skew and spawn detached replacement bootstrap daemon.
    let upgrade_deadline = Instant::now() + Duration::from_secs(6);
    let mut upgraded_health = None;
    while Instant::now() < upgrade_deadline {
        if let Some(health) = poll_health(client, port).await
            && health.version == env!("CARGO_PKG_VERSION")
        {
            upgraded_health = Some(health);
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    let upgraded_health =
        upgraded_health.expect("sidecar --adopt-running should trigger version-skew upgrade");
    assert_eq!(upgraded_health.role.as_deref(), Some("bootstrap"));

    let replacement_pid = read_proxy_pid(&pid_path)
        .expect("read replacement pid")
        .expect("replacement pid present")
        .pid;
    assert_ne!(replacement_pid, old_pid);
    assert_ne!(
        replacement_pid, sidecar_pid,
        "replacement must be a detached bootstrap daemon, not the sidecar itself"
    );

    // Confirm sidecar Web UI is serving on sidecar_web_port.
    let sidecar_ui_resp = client
        .get(format!("http://127.0.0.1:{sidecar_web_port}/"))
        .timeout(Duration::from_secs(2))
        .send()
        .await
        .expect("sidecar UI should respond");
    assert_eq!(sidecar_ui_resp.status(), reqwest::StatusCode::OK);

    // 4. Terminate the sidecar; sidecar web port must close while detached replacement survives.
    sidecar_guard.kill_and_wait().expect("terminate sidecar");

    let web_close_deadline = Instant::now() + Duration::from_secs(3);
    let mut web_port_closed = false;
    while Instant::now() < web_close_deadline {
        if client
            .get(format!("http://127.0.0.1:{sidecar_web_port}/"))
            .timeout(Duration::from_millis(200))
            .send()
            .await
            .is_err()
        {
            web_port_closed = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert!(
        web_port_closed,
        "sidecar web port must close after sidecar termination (detached replacement must scrub ANTIGRAVITY_SIDECAR_WEB_PORT)"
    );

    let surviving_health = poll_health(client, port)
        .await
        .expect("detached replacement daemon must survive sidecar termination");
    assert_eq!(surviving_health.version, env!("CARGO_PKG_VERSION"));
    assert_eq!(
        read_proxy_pid(&pid_path)
            .expect("read surviving pid")
            .expect("surviving pid")
            .pid,
        replacement_pid
    );

    drop(old_guard);
}

#[cfg(unix)]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn sidecar_plugin_manifest_version_change_triggers_binary_poll_handoff_and_stays_alive() {
    let temp = TempDir::new().expect("tempdir");
    let cfg_path = write_test_config_with_upstream(&temp, None);
    let pid_path = temp.path().join("proxy.pid");
    let plugin_dir = temp.path().join("plugin");
    std::fs::create_dir_all(&plugin_dir).expect("mkdir plugin");
    let manifest_path = plugin_dir.join("plugin.json");
    std::fs::write(
        &manifest_path,
        r#"{"name":"agy-companion","version":"0.0.2-old"}"#,
    )
    .expect("write initial manifest");

    let client = loopback_http_client();
    let _detached_cleanup = DetachedPidFileGuard::new(pid_path.clone());

    // 1. Start vOld bootstrap daemon on :port.
    let (old_guard, port, old_resp) = spawn_serve_and_wait_healthy(client, &cfg_path, |port| {
        let mut cmd = isolated_cli_command(temp.path());
        cmd.env("AGY_COMPANION_TEST_HEALTH_VERSION", "0.0.2-old")
            .args(["serve", "--port", &port.to_string(), "--config"])
            .arg(&cfg_path);
        cmd
    })
    .await;
    let old_pid = old_guard.id();
    let old_health: HealthResponse = old_resp.json().await.expect("parse old health");
    assert_eq!(old_health.version, "0.0.2-old");

    // 2. Start vOld sidecar (--adopt-running) watching plugin_dir.
    let mut sidecar_cmd = isolated_cli_command(temp.path());
    sidecar_cmd
        .env("AGY_COMPANION_TEST_HEALTH_VERSION", "0.0.2-old")
        .env("AGY_COMPANION_PLUGIN_DIR", &plugin_dir)
        .env("AGY_COMPANION_TEST_PLUGIN_POLL_MS", "100")
        .args(["serve", "--port", &port.to_string(), "--config"])
        .arg(&cfg_path)
        .arg("--adopt-running");
    let mut sidecar_guard = ChildGuard::new(sidecar_cmd.spawn().expect("spawn sidecar"));
    let sidecar_pid = sidecar_guard.id();

    // Phase 1 (Converged): plugin.json == running bootstrap (0.0.2-old) -> no spawn.
    tokio::time::sleep(Duration::from_millis(350)).await;
    assert_eq!(
        read_proxy_pid(&pid_path)
            .expect("read pid phase1")
            .expect("pid phase1")
            .pid,
        old_pid
    );

    // Phase 2 (Mismatch without matching binary, e.g. branch switch without rebuild):
    // plugin.json -> 0.0.99-unbuilt; no local binary matches -> must NOT spawn or kill bootstrap.
    std::fs::write(
        &manifest_path,
        r#"{"name":"agy-companion","version":"0.0.99-unbuilt"}"#,
    )
    .expect("write unbuilt manifest");
    tokio::time::sleep(Duration::from_millis(400)).await;
    assert_eq!(
        read_proxy_pid(&pid_path)
            .expect("read pid phase2")
            .expect("pid phase2")
            .pid,
        old_pid,
        "mismatch without matching binary must not replace running bootstrap"
    );
    assert_eq!(
        poll_health(client, port)
            .await
            .expect("health phase2")
            .version,
        "0.0.2-old"
    );

    // Phase 3 (Plugin version matches compiled binary): advance plugin.json to CARGO_PKG_VERSION.
    std::fs::write(
        &manifest_path,
        format!(
            r#"{{"name":"agy-companion","version":"{}"}}"#,
            env!("CARGO_PKG_VERSION")
        ),
    )
    .expect("write target manifest");

    let upgrade_deadline = Instant::now() + Duration::from_secs(6);
    let mut upgraded_health = None;
    while Instant::now() < upgrade_deadline {
        if let Some(health) = poll_health(client, port).await
            && health.version == env!("CARGO_PKG_VERSION")
        {
            upgraded_health = Some(health);
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    let upgraded_health = upgraded_health
        .expect("vOld sidecar should poll matching binary and upgrade bootstrap daemon");
    assert_eq!(upgraded_health.role.as_deref(), Some("bootstrap"));

    let new_pid = read_proxy_pid(&pid_path)
        .expect("read new pid")
        .expect("new pid present")
        .pid;
    assert_ne!(new_pid, old_pid);
    assert_ne!(new_pid, sidecar_pid);

    // Phase 4 (Q2=B stay-alive + no downgrade loop): wait past ADOPT_RUNNING_POLL_INTERVAL;
    // vOld sidecar must remain alive and MUST NOT downgrade vNext bootstrap back to 0.0.2-old.
    tokio::time::sleep(Duration::from_millis(750)).await;
    assert!(
        sidecar_guard
            .try_wait()
            .expect("try_wait sidecar")
            .is_none(),
        "vOld sidecar must stay alive after bootstrap upgrade (Q2=B)"
    );
    assert_eq!(
        read_proxy_pid(&pid_path)
            .expect("read steady pid")
            .expect("steady pid")
            .pid,
        new_pid,
        "vOld sidecar must not respawn or downgrade converged bootstrap"
    );
    assert_eq!(
        poll_health(client, port)
            .await
            .expect("steady health")
            .version,
        env!("CARGO_PKG_VERSION")
    );

    drop(sidecar_guard);
    drop(old_guard);
}

#[cfg(unix)]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn sidecar_owning_listener_hands_off_to_upgraded_bootstrap_when_plugin_version_advances() {
    let temp = TempDir::new().expect("tempdir");
    let cfg_path = write_test_config_with_upstream(&temp, None);
    let pid_path = temp.path().join("proxy.pid");
    let plugin_dir = temp.path().join("plugin");
    std::fs::create_dir_all(&plugin_dir).expect("mkdir plugin");
    let manifest_path = plugin_dir.join("plugin.json");
    std::fs::write(
        &manifest_path,
        r#"{"name":"agy-companion","version":"0.0.2-old"}"#,
    )
    .expect("write initial manifest");

    let client = loopback_http_client();
    let _detached_cleanup = DetachedPidFileGuard::new(pid_path.clone());

    // Start vOld sidecar with no existing bootstrap, so sidecar itself owns :port (serve_on_listener).
    let (sidecar_guard, port, initial_resp) =
        spawn_serve_and_wait_healthy(client, &cfg_path, |port| {
            let mut cmd = isolated_cli_command(temp.path());
            cmd.env("AGY_COMPANION_TEST_HEALTH_VERSION", "0.0.2-old")
                .env("AGY_COMPANION_PLUGIN_DIR", &plugin_dir)
                .env("AGY_COMPANION_TEST_PLUGIN_POLL_MS", "100")
                .args(["serve", "--port", &port.to_string(), "--config"])
                .arg(&cfg_path)
                .arg("--adopt-running");
            cmd
        })
        .await;
    let sidecar_pid = sidecar_guard.id();
    let initial_health: HealthResponse = initial_resp.json().await.expect("parse initial health");
    assert_eq!(initial_health.version, "0.0.2-old");
    assert_eq!(initial_health.role.as_deref(), Some("sidecar"));

    // Advance plugin.json to CARGO_PKG_VERSION -> active-listener watcher spawns upgraded bootstrap.
    std::fs::write(
        &manifest_path,
        format!(
            r#"{{"name":"agy-companion","version":"{}"}}"#,
            env!("CARGO_PKG_VERSION")
        ),
    )
    .expect("write upgraded manifest");

    let upgrade_deadline = Instant::now() + Duration::from_secs(6);
    let mut upgraded_health = None;
    while Instant::now() < upgrade_deadline {
        if let Some(health) = poll_health(client, port).await
            && health.version == env!("CARGO_PKG_VERSION")
        {
            upgraded_health = Some(health);
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    let upgraded_health = upgraded_health
        .expect("active-listener sidecar watcher should hand off to upgraded bootstrap");
    assert_eq!(upgraded_health.role.as_deref(), Some("bootstrap"));

    let new_pid = read_proxy_pid(&pid_path)
        .expect("read new pid")
        .expect("new pid present")
        .pid;
    assert_ne!(new_pid, sidecar_pid);

    drop(sidecar_guard);
}

#[cfg(unix)]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn sidecar_fails_closed_on_invalid_or_attacked_plugin_manifest_without_downgrading_bootstrap()
{
    use std::os::unix::fs::symlink;

    let temp = TempDir::new().expect("tempdir");
    let cfg_path = write_test_config_with_upstream(&temp, None);
    let pid_path = temp.path().join("proxy.pid");
    let plugin_dir = temp.path().join("plugin");
    std::fs::create_dir_all(&plugin_dir).expect("mkdir plugin");
    let manifest_path = plugin_dir.join("plugin.json");
    std::fs::write(
        &manifest_path,
        r#"{"name":"agy-companion","version":"0.0.4"}"#,
    )
    .expect("write valid vNext manifest");

    let client = loopback_http_client();
    let _detached_cleanup = DetachedPidFileGuard::new(pid_path.clone());

    // 1. Start vNext bootstrap daemon (0.0.4).
    let (mut bootstrap_guard, port, bootstrap_resp) =
        spawn_serve_and_wait_healthy(client, &cfg_path, |port| {
            let mut cmd = isolated_cli_command(temp.path());
            cmd.env("AGY_COMPANION_TEST_HEALTH_VERSION", "0.0.4")
                .args(["serve", "--port", &port.to_string(), "--config"])
                .arg(&cfg_path);
            cmd
        })
        .await;
    let bootstrap_pid = bootstrap_guard.id();
    let bootstrap_health: HealthResponse =
        bootstrap_resp.json().await.expect("parse bootstrap health");
    assert_eq!(bootstrap_health.version, "0.0.4");

    // 2. Start vOld sidecar (0.0.3 / CARGO_PKG_VERSION) kept alive per Q2=B.
    let mut sidecar_cmd = isolated_cli_command(temp.path());
    sidecar_cmd
        .env("AGY_COMPANION_PLUGIN_DIR", &plugin_dir)
        .env("AGY_COMPANION_TEST_PLUGIN_POLL_MS", "75")
        .args(["serve", "--port", &port.to_string(), "--config"])
        .arg(&cfg_path)
        .arg("--adopt-running");
    let mut sidecar_guard = ChildGuard::new(sidecar_cmd.spawn().expect("spawn sidecar"));

    // Allow sidecar to observe the initial valid manifest (`has_seen_valid_manifest = true`).
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert_eq!(
        read_proxy_pid(&pid_path)
            .expect("read pid baseline")
            .expect("pid baseline")
            .pid,
        bootstrap_pid
    );

    // 3. Apply S6 / schema / transient-absence attacks to plugin.json.
    //    Sidecar MUST fail closed and NEVER spawn a replacement or downgrade vNext bootstrap.
    let target_json = temp.path().join("symlink_target.json");
    std::fs::write(
        &target_json,
        r#"{"name":"agy-companion","version":"0.0.4"}"#,
    )
    .expect("write symlink target");
    let oversized_json = format!(
        r#"{{"name":"agy-companion","version":"0.0.4","pad":"{}"}}"#,
        "x".repeat(65 * 1024)
    );

    let attacks: [(&str, Option<&str>); 5] = [
        ("symlink", None),
        ("oversized", Some(oversized_json.as_str())),
        (
            "wrong_name",
            Some(r#"{"name":"other-plugin","version":"0.0.4"}"#),
        ),
        (
            "path_traversal_ver",
            Some(r#"{"name":"agy-companion","version":"../0.0.4"}"#),
        ),
        ("transient_absent", None),
    ];

    for (label, content) in attacks {
        let _ = std::fs::remove_file(&manifest_path);
        match (label, content) {
            ("symlink", _) => symlink(&target_json, &manifest_path).expect("create symlink"),
            ("transient_absent", _) => {}
            (_, Some(body)) => std::fs::write(&manifest_path, body).expect("write attack manifest"),
            _ => unreachable!(),
        }

        // Wait across multiple plugin poll intervals.
        tokio::time::sleep(Duration::from_millis(260)).await;

        assert!(
            bootstrap_guard
                .try_wait()
                .expect("try_wait bootstrap")
                .is_none(),
            "attack '{label}' must not terminate vNext bootstrap"
        );
        assert_eq!(
            read_proxy_pid(&pid_path)
                .expect("read pid after attack")
                .expect("pid present after attack")
                .pid,
            bootstrap_pid,
            "attack '{label}' must not change bootstrap PID"
        );
        let health = poll_health(client, port)
            .await
            .expect("bootstrap must remain healthy after attack");
        assert_eq!(
            health.version, "0.0.4",
            "attack '{label}' must not downgrade vNext bootstrap"
        );
        assert!(
            sidecar_guard
                .try_wait()
                .expect("try_wait sidecar")
                .is_none(),
            "sidecar must stay alive during attack '{label}'"
        );
    }

    drop(sidecar_guard);
    drop(bootstrap_guard);
}

#[cfg(unix)]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn concurrent_replace_stale_racers_converge_to_one_winner_and_zero_nonzero_exits() {
    let temp = TempDir::new().expect("tempdir");
    let cfg_path = write_test_config_with_upstream(&temp, None);
    let pid_path = temp.path().join("proxy.pid");
    let client = loopback_http_client();
    let _detached_cleanup = DetachedPidFileGuard::new(pid_path.clone());

    // 1. Spawn stale-version daemon.
    let (stale_guard, port, stale_resp) = spawn_serve_and_wait_healthy(client, &cfg_path, |port| {
        let mut cmd = isolated_cli_command(temp.path());
        cmd.env("AGY_COMPANION_TEST_HEALTH_VERSION", "0.0.1-stale")
            .args(["serve", "--port", &port.to_string(), "--config"])
            .arg(&cfg_path);
        cmd
    })
    .await;
    let stale_pid = stale_guard.id();
    let stale_health: HealthResponse = stale_resp.json().await.expect("parse stale health");
    assert_eq!(stale_health.version, "0.0.1-stale");

    // 2. Spawn 6 concurrent `--replace-stale` instances racing to replace the stale daemon.
    const RACER_COUNT: usize = 6;
    let mut racers = Vec::with_capacity(RACER_COUNT);
    for _ in 0..RACER_COUNT {
        let mut cmd = isolated_cli_command(temp.path());
        cmd.args(["serve", "--port", &port.to_string(), "--config"])
            .arg(&cfg_path)
            .arg("--replace-stale")
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::piped());
        let child = ChildGuard::new(cmd.spawn().expect("spawn racer"));
        racers.push(child);
    }

    // 3. Wait for the port to converge to CARGO_PKG_VERSION.
    let converge_deadline = Instant::now() + Duration::from_secs(6);
    let mut converged = false;
    while Instant::now() < converge_deadline {
        if let Some(health) = poll_health(client, port).await
            && health.version == env!("CARGO_PKG_VERSION")
        {
            converged = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    assert!(
        converged,
        "concurrent --replace-stale racers must converge to current version"
    );

    // 4. Wait until all 5 losers exit and exactly 1 winner remains alive.
    let settle_deadline = Instant::now() + Duration::from_secs(5);
    let mut exited_codes: Vec<(u32, Option<i32>, String)> = Vec::new();
    let mut alive_racers = racers;
    while Instant::now() < settle_deadline {
        let mut still_alive = Vec::new();
        for mut racer in alive_racers {
            if let Some(status) = racer.try_wait().expect("try_wait racer") {
                let pid = racer.id();
                let out = racer.wait_with_output().expect("read racer output");
                exited_codes.push((
                    pid,
                    status.code(),
                    String::from_utf8_lossy(&out.stderr).into_owned(),
                ));
            } else {
                still_alive.push(racer);
            }
        }
        alive_racers = still_alive;
        if alive_racers.len() == 1 && exited_codes.len() == RACER_COUNT - 1 {
            break;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }

    assert_eq!(
        alive_racers.len(),
        1,
        "expected exactly 1 surviving winner, got {}; exited={exited_codes:?}",
        alive_racers.len()
    );
    assert_eq!(
        exited_codes.len(),
        RACER_COUNT - 1,
        "expected {} losers to exit cleanly; exited={exited_codes:?}",
        RACER_COUNT - 1
    );
    for (pid, code, stderr) in &exited_codes {
        assert_eq!(
            *code,
            Some(0),
            "concurrent --replace-stale loser pid={pid} must exit 0; stderr={stderr}"
        );
    }

    let winner_pid = alive_racers[0].id();
    assert_ne!(winner_pid, stale_pid);
    let pid_info = read_proxy_pid(&pid_path)
        .expect("read winner proxy.pid")
        .expect("winner proxy.pid present");
    assert_eq!(
        pid_info.pid, winner_pid,
        "proxy.pid must match the surviving winner"
    );
    assert_eq!(pid_info.version, env!("CARGO_PKG_VERSION"));

    // Clean up winner and stale daemon.
    for racer in alive_racers {
        let _ = racer.kill_and_wait();
    }
    drop(stale_guard);
}
