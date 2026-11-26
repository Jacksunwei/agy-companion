use std::time::{Duration, Instant};

use companion_core::{ApiKey, AuthStore, FileAuthStore, GeminiCredentials};
use tempfile::TempDir;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

use crate::common::{
    config_with_port, free_loopback_port, isolated_cli_command, loopback_http_client,
    spawn_serve_and_wait_healthy,
};

#[tokio::test]
async fn serve_idle_shutdown_waits_for_in_flight_stream_then_exits_zero() {
    // Mock upstream SSE server that holds the stream open for 1.1s (> 1s idle timeout).
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

            tokio::time::sleep(Duration::from_millis(1100)).await;

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

    let temp = TempDir::new().expect("tempdir");
    let cfg_path = temp.path().join("config.json");
    let usage_dir = temp.path().join("usage");

    let mut cfg = config_with_port(free_loopback_port());
    cfg.providers.gemini.base_url = format!("http://127.0.0.1:{upstream_port}");
    cfg.usage_dir = Some(usage_dir);
    cfg.save_secure(&cfg_path).expect("save_secure");
    FileAuthStore::for_config_path(&cfg_path)
        .expect("auth store")
        .save_gemini(&GeminiCredentials::new(
            ApiKey::parse_strict("AIzaSyIdleStreamKey1234").unwrap(),
        ))
        .expect("save gemini auth");

    let client = loopback_http_client();
    let (guard, port, _resp) = spawn_serve_and_wait_healthy(client, &cfg_path, |port| {
        let mut cmd = isolated_cli_command(temp.path());
        cmd.args(["serve", "--port", &port.to_string(), "--config"])
            .arg(&cfg_path)
            .args(["--idle-shutdown-secs", "1"]);
        cmd
    })
    .await;

    let pid_path = temp.path().join("proxy.pid");
    assert!(pid_path.is_file());

    // Start slow SSE stream; proxy must stay alive through the entire 1.1s stream despite 1s idle timeout.
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
        stream_elapsed >= Duration::from_millis(1020),
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
