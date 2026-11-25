#![forbid(unsafe_code)]

mod common;

use std::fs;
use std::process::Stdio;
use std::time::{Duration, Instant};

use axum::Router;
use axum::routing::get;
use serde_json::Value;
use tempfile::TempDir;
use tokio::net::TcpListener;

use common::{
    DetachedPidFileGuard, config_with_port, free_loopback_port, isolated_cli_command,
    spawn_serve_and_wait_healthy,
};

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
fn cli_integration_enable_disable_persist_launcher_auto_start_and_validate_config_flag() {
    use std::os::unix::fs::symlink;

    let temp = TempDir::new().expect("tempdir");
    let home = temp.path().join("home");
    let xdg_data = temp.path().join("xdg_data");
    fs::create_dir_all(&home).expect("mkdir home");
    let cfg_path = temp.path().join("custom/config.json");

    // 1. Invalid --config traversal fails closed on enable, disable, and status without mutating RC.
    for subcmd in ["enable", "disable", "status"] {
        let out = isolated_cli_command(&home)
            .env("XDG_DATA_HOME", &xdg_data)
            .env("ZDOTDIR", &home)
            .env("SHELL", "/bin/zsh")
            .args([
                "integration",
                subcmd,
                "--config",
                "/tmp/../escape.json",
                "--json",
            ])
            .output()
            .expect("run invalid --config");
        assert_ne!(
            out.status.code(),
            Some(0),
            "integration {subcmd} must reject traversal --config"
        );
    }
    assert!(!home.join(".zlogin").exists());

    // 2. Symlinked --config fails closed before mutating RC.
    let target_cfg = temp.path().join("target_cfg.json");
    let sym_cfg = temp.path().join("sym_cfg.json");
    fs::write(&target_cfg, "{}").expect("write target");
    symlink(&target_cfg, &sym_cfg).expect("symlink cfg");
    let sym_out = isolated_cli_command(&home)
        .env("XDG_DATA_HOME", &xdg_data)
        .env("ZDOTDIR", &home)
        .env("SHELL", "/bin/zsh")
        .args([
            "integration",
            "enable",
            "--config",
            sym_cfg.to_str().unwrap(),
            "--json",
        ])
        .output()
        .expect("run symlink --config");
    assert_ne!(sym_out.status.code(), Some(0));
    assert!(!home.join(".zlogin").exists());

    // 3. `integration enable --config` persists launcher_auto_start = true.
    let en_out = isolated_cli_command(&home)
        .env("XDG_DATA_HOME", &xdg_data)
        .env("ZDOTDIR", &home)
        .env("SHELL", "/bin/zsh")
        .args([
            "integration",
            "enable",
            "--config",
            cfg_path.to_str().unwrap(),
            "--json",
        ])
        .output()
        .expect("run integration enable");
    assert_eq!(
        en_out.status.code(),
        Some(0),
        "stderr={}",
        String::from_utf8_lossy(&en_out.stderr)
    );
    assert!(home.join(".zlogin").is_file());
    assert_eq!(
        companion_core::Config::load_or_default(&cfg_path)
            .unwrap()
            .launcher_auto_start,
        Some(true)
    );

    // 4. `integration disable --config` persists launcher_auto_start = false.
    let dis_out = isolated_cli_command(&home)
        .env("XDG_DATA_HOME", &xdg_data)
        .env("ZDOTDIR", &home)
        .env("SHELL", "/bin/zsh")
        .args([
            "integration",
            "disable",
            "--config",
            cfg_path.to_str().unwrap(),
            "--json",
        ])
        .output()
        .expect("run integration disable");
    assert_eq!(
        dis_out.status.code(),
        Some(0),
        "stderr={}",
        String::from_utf8_lossy(&dis_out.stderr)
    );
    assert_eq!(
        companion_core::Config::load_or_default(&cfg_path)
            .unwrap()
            .launcher_auto_start,
        Some(false)
    );
}

#[cfg(unix)]
#[tokio::test]
async fn cli_serve_auto_enable_launcher_runs_once_and_standalone_serve_never_mutates_rc() {
    let temp = TempDir::new().expect("tempdir");
    let home = temp.path().join("home");
    let xdg_data = temp.path().join("xdg_data");
    fs::create_dir_all(&home).expect("mkdir home");
    let cfg_path = temp.path().join("config.json");
    let mut cfg = config_with_port(free_loopback_port());
    cfg.usage_dir = Some(xdg_data.join("agy-companion").join("usage"));
    cfg.save_secure(&cfg_path).expect("save config");

    let client = reqwest::Client::builder()
        .no_proxy()
        .timeout(Duration::from_secs(2))
        .build()
        .expect("build client");

    // 1. Standalone `serve` (WITHOUT --auto-enable-launcher) NEVER mutates RC or launcher_auto_start.
    {
        let (_guard, _port, _resp) = spawn_serve_and_wait_healthy(&client, &cfg_path, |port| {
            let mut cmd = isolated_cli_command(&home);
            cmd.env("XDG_DATA_HOME", &xdg_data)
                .env("ZDOTDIR", &home)
                .env("SHELL", "/bin/zsh")
                .args([
                    "serve",
                    "--port",
                    &port.to_string(),
                    "--config",
                    cfg_path.to_str().unwrap(),
                ]);
            cmd
        })
        .await;
        assert!(
            !home.join(".zlogin").exists(),
            "standalone serve must NEVER create .zlogin"
        );
        assert_eq!(
            companion_core::Config::load_or_default(&cfg_path)
                .unwrap()
                .launcher_auto_start,
            None,
            "standalone serve must leave launcher_auto_start as None"
        );
    }

    // 2. `serve --auto-enable-launcher` on None creates .zlogin + env.sh and persists Some(true).
    {
        let (_guard, _port, _resp) = spawn_serve_and_wait_healthy(&client, &cfg_path, |port| {
            let mut cmd = isolated_cli_command(&home);
            cmd.env("XDG_DATA_HOME", &xdg_data)
                .env("ZDOTDIR", &home)
                .env("SHELL", "/bin/zsh")
                .args([
                    "serve",
                    "--port",
                    &port.to_string(),
                    "--config",
                    cfg_path.to_str().unwrap(),
                    "--auto-enable-launcher",
                ]);
            cmd
        })
        .await;
        assert!(
            home.join(".zlogin").is_file(),
            "serve --auto-enable-launcher must create .zlogin on first run"
        );
        assert!(
            xdg_data.join("agy-companion/env.sh").is_file(),
            "serve --auto-enable-launcher must create env.sh on first run"
        );
        assert_eq!(
            companion_core::Config::load_or_default(&cfg_path)
                .unwrap()
                .launcher_auto_start,
            Some(true)
        );
    }

    // 3. After `integration disable` sets Some(false), `serve --auto-enable-launcher` does NOT re-enable!
    let dis_out = isolated_cli_command(&home)
        .env("XDG_DATA_HOME", &xdg_data)
        .env("ZDOTDIR", &home)
        .env("SHELL", "/bin/zsh")
        .args([
            "integration",
            "disable",
            "--config",
            cfg_path.to_str().unwrap(),
            "--json",
        ])
        .output()
        .expect("run integration disable");
    assert_eq!(dis_out.status.code(), Some(0));
    assert!(!home.join(".zlogin").exists());
    assert_eq!(
        companion_core::Config::load_or_default(&cfg_path)
            .unwrap()
            .launcher_auto_start,
        Some(false)
    );

    {
        let (_guard, _port, _resp) = spawn_serve_and_wait_healthy(&client, &cfg_path, |port| {
            let mut cmd = isolated_cli_command(&home);
            cmd.env("XDG_DATA_HOME", &xdg_data)
                .env("ZDOTDIR", &home)
                .env("SHELL", "/bin/zsh")
                .args([
                    "serve",
                    "--port",
                    &port.to_string(),
                    "--config",
                    cfg_path.to_str().unwrap(),
                    "--auto-enable-launcher",
                ]);
            cmd
        })
        .await;
        assert!(
            !home.join(".zlogin").exists(),
            "serve --auto-enable-launcher must NOT re-enable after explicit opt-out"
        );
        assert_eq!(
            companion_core::Config::load_or_default(&cfg_path)
                .unwrap()
                .launcher_auto_start,
            Some(false)
        );
    }
}
