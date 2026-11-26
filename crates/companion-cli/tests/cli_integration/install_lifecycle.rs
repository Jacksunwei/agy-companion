use std::fs;
#[cfg(unix)]
use std::process::Command;

use serde_json::Value;
use tempfile::TempDir;

use crate::common::{config_with_port, free_loopback_port, isolated_cli_command};

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

    // Pre-write an isolated config with an ephemeral free port so we never touch default 24141.
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
    config_with_port(free_loopback_port())
        .save_secure(&default_cfg_path)
        .expect("save default xdg config");

    // Custom config in a non-`agy-companion` directory with a distinct port.
    let custom_dir = temp.path().join("custom_cfg_dir");
    fs::create_dir_all(&custom_dir).expect("mkdir custom dir");
    let keep_sibling = custom_dir.join("keep.txt");
    fs::write(&keep_sibling, "keep").expect("write sibling");
    let custom_cfg = custom_dir.join("my-companion.json");
    let custom_port = free_loopback_port();
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
        config_with_port(free_loopback_port())
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

#[cfg(unix)]
#[test]
fn install_persists_launcher_auto_start_and_uninstall_purge_removes_integration_lock() {
    let temp = TempDir::new().expect("tempdir");
    let home = temp.path().join("home");
    let xdg_config = temp.path().join("xdg_config");
    let xdg_data = temp.path().join("xdg_data");
    let plugins_dir = temp.path().join("plugins");
    fs::create_dir_all(&home).expect("mkdir home");

    // 1. `install --no-launcher-hook` persists launcher_auto_start = Some(false) and leaves RC untouched.
    // Pre-write an ephemeral-port config (launcher_auto_start = None) so install never probes default 24141.
    let cfg_opt_out = temp.path().join("opt_out/config.json");
    config_with_port(free_loopback_port())
        .save_secure(&cfg_opt_out)
        .expect("save opt_out config");
    let out_opt_out = isolated_cli_command(&home)
        .env("XDG_CONFIG_HOME", &xdg_config)
        .env("XDG_DATA_HOME", &xdg_data)
        .env("ZDOTDIR", &home)
        .env("SHELL", "/bin/zsh")
        .args([
            "install",
            "--config",
            cfg_opt_out.to_str().expect("utf8 cfg"),
            "--plugins-dir",
            plugins_dir.to_str().expect("utf8 plugins"),
            "--no-launcher-hook",
            "--no-start-proxy",
            "--json",
        ])
        .output()
        .expect("run install --no-launcher-hook");
    assert_eq!(
        out_opt_out.status.code(),
        Some(0),
        "stderr={}",
        String::from_utf8_lossy(&out_opt_out.stderr)
    );
    assert!(!home.join(".zlogin").exists());
    let loaded_opt_out = companion_core::Config::load_or_default(&cfg_opt_out).unwrap();
    assert_eq!(loaded_opt_out.launcher_auto_start, Some(false));
    let lock_path = xdg_data
        .join("agy-companion")
        .join(companion_core::INTEGRATION_LOCK_FILENAME);
    assert!(lock_path.is_file(), "integration.lock must exist");

    // 2. Default Unix `install` enables launcher hook and persists launcher_auto_start = Some(true).
    let cfg_default = temp.path().join("default_install/config.json");
    config_with_port(free_loopback_port())
        .save_secure(&cfg_default)
        .expect("save default_install config");
    let out_default = isolated_cli_command(&home)
        .env("XDG_CONFIG_HOME", &xdg_config)
        .env("XDG_DATA_HOME", &xdg_data)
        .env("ZDOTDIR", &home)
        .env("SHELL", "/bin/zsh")
        .args([
            "install",
            "--config",
            cfg_default.to_str().expect("utf8 cfg"),
            "--plugins-dir",
            plugins_dir.to_str().expect("utf8 plugins"),
            "--no-start-proxy",
            "--json",
        ])
        .output()
        .expect("run default install");
    assert_eq!(
        out_default.status.code(),
        Some(0),
        "stderr={}",
        String::from_utf8_lossy(&out_default.stderr)
    );
    assert!(home.join(".zlogin").is_file());
    let loaded_default = companion_core::Config::load_or_default(&cfg_default).unwrap();
    assert_eq!(loaded_default.launcher_auto_start, Some(true));

    // 3. `uninstall --purge` deletes integration.lock and cleans up empty data_dir.
    let out_purge = isolated_cli_command(&home)
        .env("XDG_CONFIG_HOME", &xdg_config)
        .env("XDG_DATA_HOME", &xdg_data)
        .env("ZDOTDIR", &home)
        .env("SHELL", "/bin/zsh")
        .args([
            "uninstall",
            "--config",
            cfg_default.to_str().expect("utf8 cfg"),
            "--plugins-dir",
            plugins_dir.to_str().expect("utf8 plugins"),
            "--purge",
            "--json",
        ])
        .output()
        .expect("run uninstall --purge");
    assert_eq!(
        out_purge.status.code(),
        Some(0),
        "stderr={}",
        String::from_utf8_lossy(&out_purge.stderr)
    );
    assert!(
        !lock_path.exists(),
        "uninstall --purge must remove integration.lock"
    );
    assert!(
        !xdg_data.join("agy-companion").exists(),
        "empty data_dir must be removed cleanly after integration.lock is purged"
    );
}
