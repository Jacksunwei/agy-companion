//! Minimum Symlink Wiring Matrix (WS4): 9 behavioral CLI wiring tests across all 7 required
//! surfaces (`install`, `uninstall`/`--purge`, `integration`, `serve`, `usage.db`, `ui-mcp`,
//! `doctor`) plus Stow Fish and default-XDG ancestors. Exhaustive path-chain & normalization
//! permutations are owned by `companion_core::fs_safe`.

#[cfg(unix)]
mod symlink_matrix {
    use std::fs;
    use std::net::{Ipv4Addr, TcpStream};
    use std::os::unix::fs::{PermissionsExt, symlink};
    use std::path::Path;
    use std::process::{Command, Stdio};
    use std::time::{Duration, Instant};

    use companion_core::integration::{ENV_SCRIPT_FILENAME, INTEGRATION_LOCK_FILENAME};
    use serde_json::{Value, json};
    use tempfile::TempDir;

    use crate::common::{
        ChildGuard, config_with_port, free_loopback_port, isolated_cli_command,
        run_ui_mcp_bounded_with_env,
    };

    const VICTIM_BYTES: &str = "DO_NOT_DELETE\n";

    fn run_serve_expect_fail_bounded(mut cmd: Command, label: &str) {
        cmd.stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        let mut guard = ChildGuard::new(cmd.spawn().expect("spawn serve"));
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            if guard.try_wait().expect("try_wait serve").is_some() {
                let out = guard.wait_with_output().expect("wait serve output");
                assert_ne!(
                    out.status.code(),
                    Some(0),
                    "{label}: serve must exit non-zero"
                );
                assert!(out.stdout.is_empty(), "{label}: stdout must be empty");
                assert!(!out.stderr.is_empty(), "{label}: stderr must be non-empty");
                return;
            }
            assert!(
                Instant::now() < deadline,
                "{label}: serve did not fail closed within 5s (started unexpectedly)"
            );
            std::thread::sleep(Duration::from_millis(15));
        }
    }

    fn write_victim(path: &Path) {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).expect("mkdir victim parent");
        }
        fs::write(path, VICTIM_BYTES).expect("write victim");
        fs::set_permissions(path, fs::Permissions::from_mode(0o644)).expect("chmod victim 0644");
    }

    fn assert_victim_intact(path: &Path, label: &str) {
        assert_eq!(
            fs::read_to_string(path).unwrap_or_else(|e| panic!("{label}: read victim failed: {e}")),
            VICTIM_BYTES,
            "{label}: victim contents modified"
        );
        assert_eq!(
            fs::metadata(path)
                .expect("victim meta")
                .permissions()
                .mode()
                & 0o777,
            0o644,
            "{label}: victim permissions modified"
        );
    }

    fn make_fifo(path: &Path) {
        assert!(
            Command::new("mkfifo")
                .arg(path)
                .status()
                .expect("run mkfifo")
                .success(),
            "mkfifo failed for {}",
            path.display()
        );
    }

    /// Surface 1 (`install`): rejects symlinked config dir, `data_dir/bin`, `~/.gemini`, and
    /// symlinked/FIFO `integration.lock` without creating files or touching victims; blocks
    /// deliberate dev-symlink without `--force` and replaces link cleanly with `--force`.
    #[test]
    fn matrix_install_rejects_unsafe_targets_and_preserves_dev_symlink() {
        for case in ["cfg_dir", "bin_dir", "gemini_dir", "lock_sym", "lock_fifo"] {
            let temp = TempDir::new().expect("tempdir");
            let home = temp.path().join("home");
            let xdg_config = temp.path().join("xdg_config");
            let xdg_data = temp.path().join("xdg_data");
            let data_dir = xdg_data.join("agy-companion");
            fs::create_dir_all(&home).expect("mkdir home");
            fs::create_dir_all(&xdg_config).expect("mkdir xdg_config");

            let victim_dir = temp.path().join("victim_dir");
            let victim_file = victim_dir.join("sentinel.txt");
            write_victim(&victim_file);

            match case {
                "cfg_dir" => symlink(&victim_dir, xdg_config.join("agy-companion")).expect("sym"),
                "bin_dir" => {
                    fs::create_dir_all(&data_dir).expect("mkdir data_dir");
                    symlink(&victim_dir, data_dir.join("bin")).expect("sym");
                }
                "gemini_dir" => symlink(&victim_dir, home.join(".gemini")).expect("sym"),
                "lock_sym" => {
                    fs::create_dir_all(&data_dir).expect("mkdir data_dir");
                    symlink(&victim_file, data_dir.join(INTEGRATION_LOCK_FILENAME)).expect("sym");
                }
                "lock_fifo" => {
                    fs::create_dir_all(&data_dir).expect("mkdir data_dir");
                    make_fifo(&data_dir.join(INTEGRATION_LOCK_FILENAME));
                }
                _ => unreachable!(),
            }

            let out = isolated_cli_command(&home)
                .env("XDG_CONFIG_HOME", &xdg_config)
                .env("XDG_DATA_HOME", &xdg_data)
                .env("ZDOTDIR", &home)
                .env("SHELL", "/bin/zsh")
                .args(["install", "--no-start-proxy", "--json"])
                .output()
                .expect("run install");
            assert_ne!(out.status.code(), Some(0), "{case}: install must fail");
            assert_victim_intact(&victim_file, case);
            assert!(!home.join(".zlogin").exists(), "{case}: RC must not exist");
            if case != "gemini_dir" {
                assert!(
                    !home.join(".gemini").exists(),
                    "{case}: ~/.gemini must not be created"
                );
            }
            if case != "bin_dir" {
                assert!(
                    !data_dir.join("bin").exists(),
                    "{case}: data_dir/bin must not be created"
                );
            }
        }

        // Deliberate dev-symlink preservation without --force, and safe replacement with --force.
        let temp = TempDir::new().expect("tempdir");
        let home = temp.path().join("home");
        let xdg_config = temp.path().join("xdg_config");
        let xdg_data = temp.path().join("xdg_data");
        let plugins_dir = temp.path().join("plugins");
        fs::create_dir_all(&home).expect("mkdir home");
        fs::create_dir_all(&plugins_dir).expect("mkdir plugins");

        let cfg_path = xdg_config.join("agy-companion").join("config.json");
        config_with_port(free_loopback_port())
            .save_secure(&cfg_path)
            .expect("save config");

        let dev_target = temp.path().join("dev_target");
        let dev_victim = dev_target.join("DO_NOT_DELETE");
        write_victim(&dev_victim);
        let plugin_link = plugins_dir.join("agy-companion");
        symlink(&dev_target, &plugin_link).expect("symlink dev plugin");

        let blocked = isolated_cli_command(&home)
            .env("XDG_CONFIG_HOME", &xdg_config)
            .env("XDG_DATA_HOME", &xdg_data)
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
            .expect("run blocked dev-symlink install");
        assert_ne!(blocked.status.code(), Some(0));
        assert!(
            fs::symlink_metadata(&plugin_link)
                .unwrap()
                .file_type()
                .is_symlink()
        );
        assert_victim_intact(&dev_victim, "dev-symlink blocked");

        let forced = isolated_cli_command(&home)
            .env("XDG_CONFIG_HOME", &xdg_config)
            .env("XDG_DATA_HOME", &xdg_data)
            .env("ZDOTDIR", &home)
            .args([
                "install",
                "--plugins-dir",
                plugins_dir.to_str().unwrap(),
                "--no-launcher-hook",
                "--no-start-proxy",
                "--force",
                "--json",
            ])
            .output()
            .expect("run forced dev-symlink install");
        assert_eq!(forced.status.code(), Some(0));
        let rep: Value = serde_json::from_slice(&forced.stdout).expect("parse install JSON");
        assert_eq!(rep["ok"], true);
        assert!(
            !fs::symlink_metadata(&plugin_link)
                .unwrap()
                .file_type()
                .is_symlink()
        );
        assert!(plugin_link.join("plugin.json").is_file());
        assert_victim_intact(&dev_victim, "dev-symlink forced");
    }

    /// Surface 2 (`uninstall` & `--purge`): preserves all victim files across symlinked plugin,
    /// bin, config, usage, sidecar, and `~/.local/bin` targets; removes owned `local_bin_link` on
    /// a clean install.
    #[test]
    fn matrix_uninstall_and_purge_preserve_victims_and_remove_owned_local_bin_link() {
        let temp = TempDir::new().expect("tempdir");
        let home = temp.path().join("home");
        let xdg_config = temp.path().join("xdg_config");
        let xdg_data = temp.path().join("xdg_data");
        let data_dir = xdg_data.join("agy-companion");
        fs::create_dir_all(&home).expect("mkdir home");
        fs::create_dir_all(&xdg_config).expect("mkdir xdg_config");
        fs::create_dir_all(&data_dir).expect("mkdir data_dir");

        let victims = [
            (
                "plugin",
                temp.path().join("v_plugin/agy-companion/precious.txt"),
            ),
            ("bin", temp.path().join("v_bin/agy-companion")),
            ("cfg", temp.path().join("v_cfg/config.json")),
            ("usage", temp.path().join("v_usage/usage.db")),
            (
                "sidecar",
                temp.path().join("v_sidecar/agy-companion/precious.log"),
            ),
            ("local_bin", temp.path().join("v_local_bin/agy-companion")),
        ];
        for (_, p) in &victims {
            write_victim(p);
        }
        let pid_victim = temp.path().join("v_pid_target");
        let log_victim = temp.path().join("v_log_target");
        write_victim(&pid_victim);
        write_victim(&log_victim);

        let sym_plugins = temp.path().join("sym_plugins");
        symlink(temp.path().join("v_plugin"), &sym_plugins).expect("sym plugins");
        symlink(temp.path().join("v_bin"), data_dir.join("bin")).expect("sym bin");
        symlink(temp.path().join("v_cfg"), xdg_config.join("agy-companion")).expect("sym cfg");
        symlink(temp.path().join("v_usage"), data_dir.join("usage")).expect("sym usage");
        symlink(&pid_victim, data_dir.join("proxy.pid")).expect("sym pid");
        symlink(&log_victim, data_dir.join("proxy.log")).expect("sym log");
        fs::create_dir_all(home.join(".gemini/antigravity")).expect("mkdir antigravity");
        symlink(
            temp.path().join("v_sidecar"),
            home.join(".gemini/antigravity/sidecar_data"),
        )
        .expect("sym sidecar");
        fs::create_dir_all(home.join(".local")).expect("mkdir .local");
        symlink(temp.path().join("v_local_bin"), home.join(".local/bin")).expect("sym local_bin");

        for dry_run in [true, false] {
            let mut args = vec![
                "uninstall",
                "--plugins-dir",
                sym_plugins.to_str().unwrap(),
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
            assert_eq!(out.status.code(), Some(0));
            let rep: Value = serde_json::from_slice(&out.stdout).expect("parse uninstall JSON");
            assert_eq!(rep["ok"], true);
            assert!(!rep["warnings"].as_array().unwrap().is_empty());
            assert_eq!(rep["plugin_dir_removed"], false);
            assert_eq!(rep["binary_removed"], false);
            assert_eq!(rep["local_bin_symlink_removed"], false);
            assert!(rep["purged_paths"].as_array().unwrap().is_empty());

            for (label, p) in &victims {
                assert_victim_intact(p, label);
            }
            assert_victim_intact(&pid_victim, "pid");
            assert_victim_intact(&log_victim, "log");
        }

        // Clean install removes its own ~/.local/bin/agy-companion symlink on uninstall.
        let clean_temp = TempDir::new().expect("tempdir");
        let clean_home = clean_temp.path().join("home");
        let clean_cfg_dir = clean_temp.path().join("xdg_config");
        let clean_data_dir = clean_temp.path().join("xdg_data");
        let clean_plugins = clean_temp.path().join("plugins");
        fs::create_dir_all(&clean_home).expect("mkdir clean home");
        config_with_port(free_loopback_port())
            .save_secure(&clean_cfg_dir.join("agy-companion/config.json"))
            .expect("save clean cfg");

        let inst = isolated_cli_command(&clean_home)
            .env("XDG_CONFIG_HOME", &clean_cfg_dir)
            .env("XDG_DATA_HOME", &clean_data_dir)
            .args([
                "install",
                "--plugins-dir",
                clean_plugins.to_str().unwrap(),
                "--no-launcher-hook",
                "--no-start-proxy",
                "--json",
            ])
            .output()
            .expect("clean install");
        assert_eq!(inst.status.code(), Some(0));
        assert!(
            fs::symlink_metadata(clean_home.join(".local/bin/agy-companion"))
                .unwrap()
                .file_type()
                .is_symlink()
        );

        let uninst = isolated_cli_command(&clean_home)
            .env("XDG_CONFIG_HOME", &clean_cfg_dir)
            .env("XDG_DATA_HOME", &clean_data_dir)
            .args([
                "uninstall",
                "--plugins-dir",
                clean_plugins.to_str().unwrap(),
                "--json",
            ])
            .output()
            .expect("clean uninstall");
        assert_eq!(uninst.status.code(), Some(0));
        let rep: Value = serde_json::from_slice(&uninst.stdout).expect("parse clean uninstall");
        assert_eq!(rep["local_bin_symlink_removed"], true);
        assert!(fs::symlink_metadata(clean_home.join(".local/bin/agy-companion")).is_err());
    }

    /// Surface 3 (`integration`): refuses symlinked data dir, symlink/FIFO leaf `env.sh`, and
    /// degrades symlinked RC to `manual_required` without modifying victims or deleting marker RC.
    #[test]
    fn matrix_integration_refuses_symlinked_rc_data_dir_and_leaf_env_sh() {
        // (a) Symlinked XDG data dir
        let temp = TempDir::new().expect("tempdir");
        let home = temp.path().join("home");
        let xdg_data = temp.path().join("xdg_data");
        let victim_dir = temp.path().join("victim_data");
        fs::create_dir_all(&home).expect("mkdir home");
        fs::create_dir_all(&xdg_data).expect("mkdir xdg_data");
        fs::create_dir_all(&victim_dir).expect("mkdir victim_dir");
        fs::set_permissions(&victim_dir, fs::Permissions::from_mode(0o755)).expect("chmod 0755");
        let victim_env = victim_dir.join(ENV_SCRIPT_FILENAME);
        write_victim(&victim_env);
        symlink(&victim_dir, xdg_data.join("agy-companion")).expect("symlink data_dir");

        let rc_path = home.join(".zlogin");
        let rc_before = format!(
            "{}\n# marker\n{}\n",
            companion_core::HOOK_BEGIN_MARKER,
            companion_core::HOOK_END_MARKER
        );
        fs::write(&rc_path, &rc_before).expect("write rc");

        for sub in ["enable", "status", "disable"] {
            let out = isolated_cli_command(&home)
                .env("XDG_DATA_HOME", &xdg_data)
                .env("ZDOTDIR", &home)
                .args(["integration", sub, "--shell", "zsh", "--json"])
                .output()
                .expect("run integration");
            assert_ne!(out.status.code(), Some(0), "integration {sub} must fail");
        }
        assert_victim_intact(&victim_env, "symlinked data_dir env.sh");
        assert_eq!(
            fs::metadata(&victim_dir).unwrap().permissions().mode() & 0o777,
            0o755
        );
        assert_eq!(fs::read_to_string(&rc_path).unwrap(), rc_before);

        // (b) Symlinked & FIFO leaf env.sh
        for kind in ["symlink", "fifo"] {
            let t = TempDir::new().expect("tempdir");
            let h = t.path().join("home");
            let xd = t.path().join("xdg_data");
            let dd = xd.join("agy-companion");
            fs::create_dir_all(&h).expect("mkdir h");
            fs::create_dir_all(&dd).expect("mkdir dd");
            let v_leaf = t.path().join("victim_leaf");
            write_victim(&v_leaf);
            let env_sh = dd.join(ENV_SCRIPT_FILENAME);
            if kind == "symlink" {
                symlink(&v_leaf, &env_sh).expect("sym env.sh");
            } else {
                make_fifo(&env_sh);
            }
            let zlogin = h.join(".zlogin");
            fs::write(&zlogin, &rc_before).expect("write zlogin");

            for sub in ["enable", "disable"] {
                let out = isolated_cli_command(&h)
                    .env("XDG_DATA_HOME", &xd)
                    .env("ZDOTDIR", &h)
                    .args(["integration", sub, "--shell", "zsh", "--json"])
                    .output()
                    .expect("run integration leaf");
                assert_ne!(out.status.code(), Some(0), "{kind}/{sub} must fail");
            }
            assert_eq!(fs::read_to_string(&zlogin).unwrap(), rc_before);
            assert_victim_intact(&v_leaf, kind);
        }

        // (c) Symlinked .zlogin -> manual_required
        let t = TempDir::new().expect("tempdir");
        let h = t.path().join("home");
        fs::create_dir_all(&h).expect("mkdir h");
        let victim_rc = t.path().join("victim_rc");
        write_victim(&victim_rc);
        symlink(&victim_rc, h.join(".zlogin")).expect("sym .zlogin");

        let out = isolated_cli_command(&h)
            .env("ZDOTDIR", &h)
            .args(["integration", "enable", "--shell", "zsh", "--json"])
            .output()
            .expect("run symlinked rc");
        assert_eq!(out.status.code(), Some(0));
        let rep: Value = serde_json::from_slice(&out.stdout).expect("parse JSON");
        assert_eq!(rep["state"], "manual_required");
        assert_eq!(rep["enabled"], false);
        assert_victim_intact(&victim_rc, "symlinked .zlogin");
    }

    /// Surface 4 (`serve`): rejects symlinked `config.json`, `usage_dir`, `proxy.pid`, `proxy.log`,
    /// and `proxy.log.1` before creating `proxy.log` or binding its port, leaving victims intact.
    #[test]
    fn matrix_serve_rejects_symlinked_config_usage_dir_pid_and_log() {
        for case in [
            "config",
            "usage_dir",
            "proxy_pid",
            "proxy_log",
            "proxy_log_rotated",
        ] {
            let temp = TempDir::new().expect("tempdir");
            let home = temp.path().join("home");
            fs::create_dir_all(&home).expect("mkdir home");
            let port = free_loopback_port();

            let runtime_dir = temp.path().join("runtime");
            let usage_dir = runtime_dir.join("usage");
            let victim = temp.path().join("victim_serve.txt");
            write_victim(&victim);
            let mut check_victim = victim.clone();

            let cfg_path = temp.path().join("config.json");
            if case == "config" {
                symlink(&victim, &cfg_path).expect("symlink config.json");
            } else {
                let mut cfg = config_with_port(port);
                cfg.usage_dir = Some(usage_dir.clone());
                cfg.save_secure(&cfg_path).expect("save config");
                fs::create_dir_all(&runtime_dir).expect("mkdir runtime");
                match case {
                    "usage_dir" => {
                        let victim_dir = temp.path().join("victim_usage_dir");
                        let victim_dir_sentinel = victim_dir.join("DO_NOT_DELETE");
                        write_victim(&victim_dir_sentinel);
                        check_victim = victim_dir_sentinel;
                        symlink(&victim_dir, &usage_dir).expect("symlink usage_dir");
                    }
                    "proxy_pid" => symlink(&victim, runtime_dir.join("proxy.pid")).expect("sym"),
                    "proxy_log" => symlink(&victim, runtime_dir.join("proxy.log")).expect("sym"),
                    "proxy_log_rotated" => {
                        symlink(&victim, runtime_dir.join("proxy.log.1")).expect("sym")
                    }
                    _ => unreachable!(),
                }
            }

            let mut cmd = isolated_cli_command(&home);
            cmd.args(["serve", "--config"])
                .arg(&cfg_path)
                .args(["--port", &port.to_string()]);
            run_serve_expect_fail_bounded(cmd, case);
            assert_victim_intact(&check_victim, case);
            if matches!(case, "proxy_pid" | "proxy_log_rotated") {
                assert!(
                    !runtime_dir.join("proxy.log").exists(),
                    "{case}: serve must fail closed before creating proxy.log"
                );
            }
            assert!(
                TcpStream::connect((Ipv4Addr::LOCALHOST, port)).is_err(),
                "{case}: serve must not bind port {port}"
            );
        }
    }

    /// Surface 5 (`usage.db`): rejects symlinked `usage.db` and symlinked `usage.db-wal` sidecar
    /// before binding and leaves victims intact.
    #[test]
    fn matrix_serve_rejects_symlinked_usage_db_and_sidecar() {
        for case in ["usage_db", "usage_db_wal"] {
            let temp = TempDir::new().expect("tempdir");
            let home = temp.path().join("home");
            fs::create_dir_all(&home).expect("mkdir home");
            let port = free_loopback_port();

            let usage_dir = temp.path().join("runtime/usage");
            fs::create_dir_all(&usage_dir).expect("mkdir usage_dir");
            let cfg_path = temp.path().join("config.json");
            let mut cfg = config_with_port(port);
            cfg.usage_dir = Some(usage_dir.clone());
            cfg.save_secure(&cfg_path).expect("save config");

            let victim = temp.path().join("victim_usage_db");
            write_victim(&victim);
            if case == "usage_db" {
                symlink(&victim, usage_dir.join("usage.db")).expect("sym usage.db");
            } else {
                fs::write(usage_dir.join("usage.db"), b"").expect("create usage.db");
                symlink(&victim, usage_dir.join("usage.db-wal")).expect("sym usage.db-wal");
            }

            let mut cmd = isolated_cli_command(&home);
            cmd.args(["serve", "--config"])
                .arg(&cfg_path)
                .args(["--port", &port.to_string()]);
            run_serve_expect_fail_bounded(cmd, case);
            assert_victim_intact(&victim, case);
            assert!(
                TcpStream::connect((Ipv4Addr::LOCALHOST, port)).is_err(),
                "{case}: port {port} must remain unbound"
            );
        }
    }

    /// Surface 6 (`ui-mcp`): rejects symlinked `config.json` eagerly and fails closed without
    /// hanging when `AGY_COMPANION_UI_HTML` points to a FIFO.
    #[test]
    fn matrix_ui_mcp_rejects_symlinked_config_and_fifo_ui_html_override() {
        let temp = TempDir::new().expect("tempdir");
        let home = temp.path().join("home");
        let data_dir = temp.path().join("data");
        fs::create_dir_all(&home).expect("mkdir home");
        fs::create_dir_all(&data_dir).expect("mkdir data");

        // (a) Symlinked config.json -> exit 1, 0 stdout, victim intact
        let victim = temp.path().join("victim_cfg.json");
        write_victim(&victim);
        let sym_cfg = temp.path().join("sym_cfg.json");
        symlink(&victim, &sym_cfg).expect("sym config");
        let (code, stdout, _stderr, timed_out) = run_ui_mcp_bounded_with_env(
            &home,
            &sym_cfg,
            &data_dir,
            &[],
            vec![],
            Duration::from_secs(5),
        );
        assert!(!timed_out);
        assert_eq!(code, Some(1));
        assert!(stdout.is_empty());
        assert_victim_intact(&victim, "ui-mcp symlinked config");

        // (b) FIFO AGY_COMPANION_UI_HTML -> bounded non-hanging JSON-RPC error on resources/read
        let valid_cfg = temp.path().join("valid_cfg.json");
        config_with_port(free_loopback_port())
            .save_secure(&valid_cfg)
            .expect("save valid cfg");
        let fifo_html = temp.path().join("override.html.fifo");
        make_fifo(&fifo_html);

        let (code, stdout, stderr, timed_out) = run_ui_mcp_bounded_with_env(
            &home,
            &valid_cfg,
            &data_dir,
            &[("AGY_COMPANION_UI_HTML", fifo_html.as_os_str())],
            vec![json!({
                "jsonrpc": "2.0",
                "id": 2,
                "method": "resources/read",
                "params": { "uri": "ui://companion/settings.html" }
            })],
            Duration::from_secs(5),
        );
        assert!(
            !timed_out,
            "ui-mcp must not hang on FIFO AGY_COMPANION_UI_HTML"
        );
        assert_eq!(code, Some(0), "stderr={stderr}");
        let resp = stdout
            .lines()
            .filter_map(|l| serde_json::from_str::<Value>(l).ok())
            .find(|v| v["id"] == 2)
            .unwrap_or_else(|| panic!("missing id=2 response: {stdout}"));
        assert!(
            resp.get("error").is_some(),
            "FIFO UI HTML override must return JSON-RPC error, got: {resp}"
        );
    }

    /// Surface 7 (`doctor`): fails closed on direct and dangling symlinked configs with structured
    /// JSON `ok == false` and `config_permissions == "fail"`, leaving victim intact.
    #[test]
    fn matrix_doctor_fails_closed_on_symlinked_config_with_structured_status() {
        let temp = TempDir::new().expect("tempdir");
        let home = temp.path().join("home");
        fs::create_dir_all(&home).expect("mkdir home");
        let port = free_loopback_port();

        let victim = temp.path().join("victim_doc.json");
        write_victim(&victim);
        let direct_sym = temp.path().join("direct_sym.json");
        let dangling_sym = temp.path().join("dangling_sym.json");
        symlink(&victim, &direct_sym).expect("direct sym");
        symlink(temp.path().join("missing_target.json"), &dangling_sym).expect("dangling sym");

        for sym in [&direct_sym, &dangling_sym] {
            let out = isolated_cli_command(&home)
                .args(["doctor", "--config"])
                .arg(sym)
                .args(["--port", &port.to_string(), "--json"])
                .output()
                .expect("run doctor");
            assert_eq!(out.status.code(), Some(1));
            let parsed: Value = serde_json::from_slice(&out.stdout).expect("valid doctor JSON");
            assert_eq!(parsed["ok"], false);
            let perm = parsed["checks"]
                .as_array()
                .unwrap()
                .iter()
                .find(|c| c["name"] == "config_permissions")
                .expect("config_permissions check");
            assert_eq!(perm["status"], "fail");
        }
        assert_victim_intact(&victim, "doctor symlinked config");
    }

    /// Cross-surface (Fish Stow): `uninstall --purge` reports Stow-symlinked `~/.config/fish`
    /// snippet under `rc_manual_remaining` without modifying or deleting the target file.
    #[test]
    fn matrix_uninstall_preserves_stow_symlinked_fish_rc_as_manual_remaining() {
        let temp = TempDir::new().expect("tempdir");
        let home = temp.path().join("home");
        let victim_fish = temp.path().join("victim_fish");
        let fish_snippet = victim_fish.join("conf.d/agy-companion.fish");
        fs::create_dir_all(home.join(".config")).expect("mkdir .config");
        fs::create_dir_all(fish_snippet.parent().unwrap()).expect("mkdir conf.d");
        let content = format!(
            "{VICTIM_BYTES}{}\n# hook\n{}\n",
            companion_core::HOOK_BEGIN_MARKER,
            companion_core::HOOK_END_MARKER
        );
        fs::write(&fish_snippet, &content).expect("write fish snippet");
        symlink(&victim_fish, home.join(".config/fish")).expect("symlink fish");

        let out = isolated_cli_command(&home)
            .env_remove("XDG_CONFIG_HOME")
            .args([
                "uninstall",
                "--plugins-dir",
                temp.path().join("plugins").to_str().unwrap(),
                "--purge",
                "--json",
            ])
            .output()
            .expect("run uninstall stow fish");
        assert_eq!(out.status.code(), Some(0));
        let rep: Value = serde_json::from_slice(&out.stdout).expect("parse JSON");
        assert!(rep["rc_deleted"].as_array().unwrap().is_empty());
        assert!(rep["rc_modified"].as_array().unwrap().is_empty());
        let expected_rc = home
            .join(".config/fish/conf.d/agy-companion.fish")
            .to_string_lossy()
            .into_owned();
        assert!(
            rep["rc_manual_remaining"]
                .as_array()
                .unwrap()
                .iter()
                .any(|p| p.as_str() == Some(expected_rc.as_str())),
            "expected {expected_rc} in rc_manual_remaining, got: {rep}"
        );
        assert_eq!(fs::read_to_string(&fish_snippet).unwrap(), content);
    }

    /// Cross-surface (default XDG unset ancestors): symlinked `~/.local` and `~/.config` are
    /// rejected by `install` and left untouched by `uninstall --purge` when XDG vars are unset.
    #[test]
    fn matrix_default_xdg_ancestors_refuse_symlinked_home_local_and_config() {
        let temp = TempDir::new().expect("tempdir");
        let home = temp.path().join("home");
        fs::create_dir_all(&home).expect("mkdir home");

        let victim_local = temp.path().join("victim_local");
        let victim_cfg = temp.path().join("victim_cfg");
        let v_local_file = victim_local.join("share/agy-companion/usage/usage.db");
        let v_cfg_file = victim_cfg.join("agy-companion/config.json");
        write_victim(&v_local_file);
        write_victim(&v_cfg_file);
        symlink(&victim_local, home.join(".local")).expect("sym .local");
        symlink(&victim_cfg, home.join(".config")).expect("sym .config");

        let inst = isolated_cli_command(&home)
            .env_remove("XDG_DATA_HOME")
            .env_remove("XDG_CONFIG_HOME")
            .args([
                "install",
                "--plugins-dir",
                temp.path().join("plugins").to_str().unwrap(),
                "--no-launcher-hook",
                "--no-start-proxy",
                "--json",
            ])
            .output()
            .expect("run install unset XDG");
        assert_ne!(inst.status.code(), Some(0));
        assert_victim_intact(&v_local_file, "install unset XDG .local");
        assert_victim_intact(&v_cfg_file, "install unset XDG .config");

        let uninst = isolated_cli_command(&home)
            .env_remove("XDG_DATA_HOME")
            .env_remove("XDG_CONFIG_HOME")
            .args([
                "uninstall",
                "--plugins-dir",
                temp.path().join("plugins").to_str().unwrap(),
                "--purge",
                "--json",
            ])
            .output()
            .expect("run uninstall unset XDG");
        assert_eq!(uninst.status.code(), Some(0));
        let rep: Value = serde_json::from_slice(&uninst.stdout).expect("parse JSON");
        assert!(rep["purged_paths"].as_array().unwrap().is_empty());
        assert_victim_intact(&v_local_file, "uninstall unset XDG .local");
        assert_victim_intact(&v_cfg_file, "uninstall unset XDG .config");
    }
}
