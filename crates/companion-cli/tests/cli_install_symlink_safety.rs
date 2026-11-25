#![forbid(unsafe_code)]

mod common;

use std::fs;
use std::process::Command;

use companion_core::Config;
use serde_json::Value;
use tempfile::TempDir;

use common::{
    BIN, DetachedPidFileGuard, config_with_port, free_loopback_port, isolated_cli_command,
};

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
