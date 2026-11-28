use std::fs;
use std::path::{Path, PathBuf};

use crate::config::CoreError;
use crate::fs_safe::FsError;

use super::disable::disable_with_env;
use super::enable::enable_with_env;
use super::lifecycle::{LifecycleEnv, status_with_env};
use super::markers::parse_markers;
#[cfg(unix)]
use super::templates::write_env_script;
use super::templates::{render_env_sh, render_fish_dropin, wrap_with_markers};
use super::*;
#[cfg(unix)]
use std::process::Command;
use tempfile::TempDir;

fn make_opts(
    shell: ShellKind,
    rc_file: Option<PathBuf>,
    data_dir: PathBuf,
    binary_path: PathBuf,
    follow_rc_symlink: bool,
) -> IntegrationOptions {
    IntegrationOptions {
        shell: Some(shell),
        rc_file,
        binary_path: Some(binary_path),
        data_dir: Some(data_dir),
        follow_rc_symlink,
    }
}

#[test]
fn detect_shell_parses_basenames_and_platform_default() {
    assert_eq!(detect_shell(Some("/bin/zsh")), ShellKind::Zsh);
    assert_eq!(detect_shell(Some("/usr/local/bin/BASH")), ShellKind::Bash);
    assert_eq!(
        detect_shell(Some("/opt/homebrew/bin/fish")),
        ShellKind::Fish
    );
    assert_eq!(
        detect_shell(Some("C:\\msys64\\usr\\bin\\zsh")),
        ShellKind::Zsh
    );

    let expected_default = if cfg!(target_os = "macos") {
        ShellKind::Zsh
    } else {
        ShellKind::Bash
    };
    assert_eq!(detect_shell(None), expected_default);
    assert_eq!(detect_shell(Some("")), expected_default);
    assert_eq!(detect_shell(Some("/bin/tcsh")), expected_default);

    assert_eq!("ZSH".parse::<ShellKind>().unwrap(), ShellKind::Zsh);
    assert_eq!("bash".parse::<ShellKind>().unwrap(), ShellKind::Bash);
    assert_eq!("Fish".parse::<ShellKind>().unwrap(), ShellKind::Fish);
    assert!(matches!(
        "tcsh".parse::<ShellKind>(),
        Err(CoreError::InvalidShell { .. })
    ));
}

#[test]
fn resolve_rc_path_zsh_honors_zdotdir_and_rejects_traversal_and_nul() {
    let tmp = TempDir::new().unwrap();
    let home = tmp.path().join("home");
    let zdot = tmp.path().join("zdot");

    assert_eq!(
        resolve_rc_path(ShellKind::Zsh, &home, None, None).unwrap(),
        home.join(".zlogin")
    );
    assert_eq!(
        resolve_rc_path(ShellKind::Zsh, &home, Some(&zdot), None).unwrap(),
        zdot.join(".zlogin")
    );
    assert_eq!(
        resolve_rc_path(
            ShellKind::Zsh,
            &home,
            Some(Path::new("relative/zdot")),
            None
        )
        .unwrap(),
        home.join(".zlogin")
    );
    assert!(matches!(
        resolve_rc_path(
            ShellKind::Zsh,
            &home,
            Some(Path::new("/tmp/../escape")),
            None
        ),
        Err(CoreError::Fs(FsError::ParentDirTraversal))
    ));
    assert!(matches!(
        resolve_rc_path(
            ShellKind::Zsh,
            &home,
            Some(Path::new("/tmp/bad\0zdot")),
            None
        ),
        Err(CoreError::Fs(FsError::ContainsNul))
    ));
}

#[test]
fn resolve_rc_path_bash_precedence_and_dangling_symlink_detection() {
    let tmp = TempDir::new().unwrap();
    let home = tmp.path().join("home");
    fs::create_dir_all(&home).unwrap();

    assert_eq!(
        resolve_rc_path(ShellKind::Bash, &home, None, None).unwrap(),
        home.join(".profile")
    );

    fs::write(home.join(".profile"), "# profile\n").unwrap();
    assert_eq!(
        resolve_rc_path(ShellKind::Bash, &home, None, None).unwrap(),
        home.join(".profile")
    );

    fs::write(home.join(".bash_login"), "# login\n").unwrap();
    assert_eq!(
        resolve_rc_path(ShellKind::Bash, &home, None, None).unwrap(),
        home.join(".bash_login")
    );

    fs::write(home.join(".bash_profile"), "# bash_profile\n").unwrap();
    assert_eq!(
        resolve_rc_path(ShellKind::Bash, &home, None, None).unwrap(),
        home.join(".bash_profile")
    );

    #[cfg(unix)]
    {
        use std::os::unix::fs::symlink;
        fs::remove_file(home.join(".bash_profile")).unwrap();
        symlink(home.join("does-not-exist"), home.join(".bash_profile")).unwrap();
        assert_eq!(
            resolve_rc_path(ShellKind::Bash, &home, None, None).unwrap(),
            home.join(".bash_profile")
        );
    }
}

#[test]
fn resolve_rc_path_fish_honors_xdg_config_home_and_rejects_traversal() {
    let tmp = TempDir::new().unwrap();
    let home = tmp.path().join("home");
    let xdg = tmp.path().join("xdg");

    assert_eq!(
        resolve_rc_path(ShellKind::Fish, &home, None, None).unwrap(),
        home.join(".config/fish/conf.d/agy-companion.fish")
    );
    assert_eq!(
        resolve_rc_path(ShellKind::Fish, &home, None, Some(&xdg)).unwrap(),
        xdg.join("fish/conf.d/agy-companion.fish")
    );
    assert_eq!(
        resolve_rc_path(ShellKind::Fish, &home, None, Some(Path::new("rel/cfg"))).unwrap(),
        home.join(".config/fish/conf.d/agy-companion.fish")
    );
    assert!(matches!(
        resolve_rc_path(ShellKind::Fish, &home, None, Some(Path::new("/tmp/../bad"))),
        Err(CoreError::Fs(FsError::ParentDirTraversal))
    ));
    assert!(matches!(
        resolve_rc_path(ShellKind::Fish, &home, None, Some(Path::new("/tmp/a\0b"))),
        Err(CoreError::Fs(FsError::ContainsNul))
    ));
}

#[test]
fn render_env_sh_contains_zero_eval_escapes_quotes_and_rejects_newlines() {
    let path_with_quotes = Path::new("/opt/my 'quoted' dir/agy-companion");
    let rendered = render_env_sh(path_with_quotes).unwrap();
    assert!(!rendered.contains("eval"));
    assert!(!rendered.contains("GOOGLE_API_KEY"));
    assert!(rendered.contains("'/opt/my '\"'\"'quoted'\"'\"' dir/agy-companion'"));
    assert!(rendered.contains("AGY_COMPANION_ALLOW_REMOTE_SSH"));
    assert!(rendered.contains("0*|??????*"));

    for bad in ["/tmp/a\nb", "/tmp/a\rb"] {
        assert!(
            matches!(
                render_env_sh(Path::new(bad)),
                Err(CoreError::InvalidPath { .. })
            ),
            "expected InvalidPath for {bad:?}"
        );
    }
    assert!(matches!(
        render_env_sh(Path::new("/tmp/../escape")),
        Err(CoreError::Fs(FsError::ParentDirTraversal))
    ));
    assert!(matches!(
        render_env_sh(Path::new("/tmp/a\0b")),
        Err(CoreError::Fs(FsError::ContainsNul))
    ));
}

#[test]
fn render_fish_dropin_contains_zero_eval_zero_pipe_source_and_escapes_backslashes() {
    let path_with_escapes = Path::new(r"/opt/a\b'c/agy-companion");
    let rendered = render_fish_dropin(path_with_escapes).unwrap();
    assert!(!rendered.contains("| source"));
    assert!(!rendered.contains("eval"));
    assert!(!rendered.contains("exit 0"));
    assert!(!rendered.contains("GOOGLE_API_KEY"));
    assert!(rendered.contains(r"'/opt/a\\b\'c/agy-companion'"));
    assert!(rendered.contains("string collect | string trim"));
    assert!(rendered.contains(r"\A[1-9][0-9]{0,4}\z"));
}

#[cfg(unix)]
#[test]
fn write_env_script_enforces_0700_dir_0600_file_and_rejects_symlink_and_fifo() {
    use std::os::unix::fs::{PermissionsExt, symlink};
    use std::sync::mpsc;
    use std::time::Duration;

    let tmp = TempDir::new().unwrap();
    let data_dir = tmp.path().join("data");
    fs::create_dir_all(&data_dir).unwrap();
    fs::set_permissions(&data_dir, fs::Permissions::from_mode(0o755)).unwrap();

    let env_path = data_dir.join("env.sh");
    let bin = tmp.path().join("agy-companion");
    write_env_script(&env_path, &bin).unwrap();

    assert_eq!(
        fs::metadata(&data_dir).unwrap().permissions().mode() & 0o777,
        0o700
    );
    assert_eq!(
        fs::metadata(&env_path).unwrap().permissions().mode() & 0o777,
        0o600
    );

    // Symlink target rejected
    let real_target = tmp.path().join("real.sh");
    fs::write(&real_target, "keep").unwrap();
    let sym_env = data_dir.join("sym-env.sh");
    symlink(&real_target, &sym_env).unwrap();
    assert!(matches!(
        write_env_script(&sym_env, &bin),
        Err(CoreError::Fs(FsError::SymlinkRejected { .. }))
    ));
    assert_eq!(fs::read_to_string(&real_target).unwrap(), "keep");

    // Symlink parent dir rejected
    let sym_parent = tmp.path().join("sym-parent");
    symlink(&data_dir, &sym_parent).unwrap();
    assert!(matches!(
        write_env_script(&sym_parent.join("env.sh"), &bin),
        Err(CoreError::Fs(FsError::SymlinkRejected { .. }))
    ));

    // FIFO target rejected without hanging (< 1s)
    let fifo_path = data_dir.join("fifo-env.sh");
    let status = Command::new("mkfifo").arg(&fifo_path).status().unwrap();
    assert!(status.success());
    let fifo_clone = fifo_path.clone();
    let bin_clone = bin.clone();
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        let res = write_env_script(&fifo_clone, &bin_clone);
        let _ = tx.send(res);
    });
    let res = rx
        .recv_timeout(Duration::from_secs(1))
        .expect("write_env_script must reject FIFO without blocking");
    assert!(matches!(
        res,
        Err(CoreError::Fs(FsError::NotRegularFile { .. }))
    ));
}

#[test]
fn enable_disable_zsh_creates_and_deletes_empty_zlogin_and_roundtrips_nonempty() {
    let tmp = TempDir::new().unwrap();
    let home = tmp.path().join("home");
    let data = tmp.path().join("data");
    let bin = tmp.path().join("agy-companion");
    fs::create_dir_all(&home).unwrap();
    let opts = make_opts(ShellKind::Zsh, None, data.clone(), bin, false);

    // Create new .zlogin on enable, then idempotent double-enable
    let st1 = enable_with_env(
        &opts,
        &LifecycleEnv {
            home: Some(&home),
            zdotdir: None,
            xdg_config_home: None,
            shell_env: Some("/bin/zsh"),
            default_data_override: None,
            hybrid_active: false,
            platform_supported: true,
        },
    )
    .unwrap();
    assert!(st1.enabled);
    assert_eq!(st1.state, IntegrationState::Enabled);
    let zlogin = home.join(".zlogin");
    assert!(zlogin.is_file());
    assert!(data.join("env.sh").is_file());
    let first_content = fs::read_to_string(&zlogin).unwrap();

    let st2 = enable_with_env(
        &opts,
        &LifecycleEnv {
            home: Some(&home),
            zdotdir: None,
            xdg_config_home: None,
            shell_env: Some("/bin/zsh"),
            default_data_override: None,
            hybrid_active: false,
            platform_supported: true,
        },
    )
    .unwrap();
    assert!(st2.enabled);
    assert_eq!(fs::read_to_string(&zlogin).unwrap(), first_content);

    // Empty .zlogin deleted on disable + env.sh removed
    let st3 = disable_with_env(
        &opts,
        &LifecycleEnv {
            home: Some(&home),
            zdotdir: None,
            xdg_config_home: None,
            shell_env: Some("/bin/zsh"),
            default_data_override: None,
            hybrid_active: false,
            platform_supported: true,
        },
    )
    .unwrap();
    assert!(!st3.enabled);
    assert!(!zlogin.exists());
    assert!(!data.join("env.sh").exists());

    // Non-empty .zlogin roundtrips byte-for-byte
    let original = "# user config\nexport FOO=bar\n";
    fs::write(&zlogin, original).unwrap();
    enable_with_env(
        &opts,
        &LifecycleEnv {
            home: Some(&home),
            zdotdir: None,
            xdg_config_home: None,
            shell_env: Some("/bin/zsh"),
            default_data_override: None,
            hybrid_active: false,
            platform_supported: true,
        },
    )
    .unwrap();
    assert!(
        fs::read_to_string(&zlogin)
            .unwrap()
            .contains(HOOK_BEGIN_MARKER)
    );
    disable_with_env(
        &opts,
        &LifecycleEnv {
            home: Some(&home),
            zdotdir: None,
            xdg_config_home: None,
            shell_env: Some("/bin/zsh"),
            default_data_override: None,
            hybrid_active: false,
            platform_supported: true,
        },
    )
    .unwrap();
    assert_eq!(fs::read_to_string(&zlogin).unwrap(), original);
}

#[test]
fn enable_disable_preserves_crlf_trailing_newline_and_unix_mode() {
    let tmp = TempDir::new().unwrap();
    let home = tmp.path().join("home");
    let data = tmp.path().join("data");
    let bin = tmp.path().join("agy-companion");
    fs::create_dir_all(&home).unwrap();
    let zlogin = home.join(".zlogin");
    let crlf_content = "export A=1\r\nexport B=2\r\n";
    fs::write(&zlogin, crlf_content).unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&zlogin, fs::Permissions::from_mode(0o644)).unwrap();
    }

    let opts = make_opts(ShellKind::Zsh, None, data, bin, false);
    enable_with_env(
        &opts,
        &LifecycleEnv {
            home: Some(&home),
            zdotdir: None,
            xdg_config_home: None,
            shell_env: Some("/bin/zsh"),
            default_data_override: None,
            hybrid_active: false,
            platform_supported: true,
        },
    )
    .unwrap();
    let enabled_content = fs::read_to_string(&zlogin).unwrap();
    assert!(enabled_content.contains("\r\n"));
    assert!(!enabled_content.replace("\r\n", "").contains('\n'));
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(
            fs::metadata(&zlogin).unwrap().permissions().mode() & 0o777,
            0o644
        );
    }

    disable_with_env(
        &opts,
        &LifecycleEnv {
            home: Some(&home),
            zdotdir: None,
            xdg_config_home: None,
            shell_env: Some("/bin/zsh"),
            default_data_override: None,
            hybrid_active: false,
            platform_supported: true,
        },
    )
    .unwrap();
    assert_eq!(fs::read_to_string(&zlogin).unwrap(), crlf_content);
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(
            fs::metadata(&zlogin).unwrap().permissions().mode() & 0o777,
            0o644
        );
    }

    // File without trailing newline separates the injected block and round-trips losslessly
    fs::write(&zlogin, "export NO_NL=1").unwrap();
    enable_with_env(
        &opts,
        &LifecycleEnv {
            home: Some(&home),
            zdotdir: None,
            xdg_config_home: None,
            shell_env: Some("/bin/zsh"),
            default_data_override: None,
            hybrid_active: false,
            platform_supported: true,
        },
    )
    .unwrap();
    let content = fs::read_to_string(&zlogin).unwrap();
    assert!(content.starts_with("export NO_NL=1\n"));
    assert!(!content.ends_with('\n'));
    assert!(content.ends_with(HOOK_END_MARKER));

    disable_with_env(
        &opts,
        &LifecycleEnv {
            home: Some(&home),
            zdotdir: None,
            xdg_config_home: None,
            shell_env: Some("/bin/zsh"),
            default_data_override: None,
            hybrid_active: false,
            platform_supported: true,
        },
    )
    .unwrap();
    assert_eq!(fs::read_to_string(&zlogin).unwrap(), "export NO_NL=1");
}

#[test]
fn enable_disable_bash_uses_profile_fallback_and_existing_bash_profile() {
    let tmp = TempDir::new().unwrap();
    let home = tmp.path().join("home");
    let data = tmp.path().join("data");
    let bin = tmp.path().join("agy-companion");
    fs::create_dir_all(&home).unwrap();
    let opts = make_opts(ShellKind::Bash, None, data, bin, false);

    let st = enable_with_env(
        &opts,
        &LifecycleEnv {
            home: Some(&home),
            zdotdir: None,
            xdg_config_home: None,
            shell_env: Some("/bin/bash"),
            default_data_override: None,
            hybrid_active: false,
            platform_supported: true,
        },
    )
    .unwrap();
    assert_eq!(st.rc_path.as_deref(), Some(home.join(".profile").as_path()));
    assert!(!home.join(".bash_profile").exists());
    assert!(home.join(".profile").is_file());

    disable_with_env(
        &opts,
        &LifecycleEnv {
            home: Some(&home),
            zdotdir: None,
            xdg_config_home: None,
            shell_env: Some("/bin/bash"),
            default_data_override: None,
            hybrid_active: false,
            platform_supported: true,
        },
    )
    .unwrap();
    // .profile is NOT deleted when empty (only .zlogin and agy-companion.fish are deleted)
    assert!(home.join(".profile").is_file());
    assert_eq!(fs::read_to_string(home.join(".profile")).unwrap(), "");

    // When .bash_profile exists, it takes precedence
    fs::write(home.join(".bash_profile"), "# existing\n").unwrap();
    let st2 = enable_with_env(
        &opts,
        &LifecycleEnv {
            home: Some(&home),
            zdotdir: None,
            xdg_config_home: None,
            shell_env: Some("/bin/bash"),
            default_data_override: None,
            hybrid_active: false,
            platform_supported: true,
        },
    )
    .unwrap();
    assert_eq!(
        st2.rc_path.as_deref(),
        Some(home.join(".bash_profile").as_path())
    );
    assert!(
        fs::read_to_string(home.join(".bash_profile"))
            .unwrap()
            .contains(HOOK_BEGIN_MARKER)
    );
}

#[test]
fn enable_disable_fish_writes_dropin_and_deletes_when_empty_preserving_custom_lines() {
    let tmp = TempDir::new().unwrap();
    let home = tmp.path().join("home");
    let xdg = tmp.path().join("xdg");
    let data = tmp.path().join("data");
    let bin = tmp.path().join("agy-companion");
    fs::create_dir_all(&home).unwrap();
    let opts = make_opts(ShellKind::Fish, None, data, bin, false);

    let st = enable_with_env(
        &opts,
        &LifecycleEnv {
            home: Some(&home),
            zdotdir: None,
            xdg_config_home: Some(&xdg),
            shell_env: Some("/usr/bin/fish"),
            default_data_override: None,
            hybrid_active: false,
            platform_supported: true,
        },
    )
    .unwrap();
    assert!(st.enabled);
    assert!(st.env_script_path.is_none());
    let dropin = xdg.join("fish/conf.d/agy-companion.fish");
    assert!(dropin.is_file());
    let dropin_content = fs::read_to_string(&dropin).unwrap();
    assert!(!dropin_content.contains("| source"));
    assert!(dropin_content.contains(HOOK_BEGIN_MARKER));

    disable_with_env(
        &opts,
        &LifecycleEnv {
            home: Some(&home),
            zdotdir: None,
            xdg_config_home: Some(&xdg),
            shell_env: Some("/usr/bin/fish"),
            default_data_override: None,
            hybrid_active: false,
            platform_supported: true,
        },
    )
    .unwrap();
    assert!(!dropin.exists());

    // Extra custom line preserved on disable
    fs::create_dir_all(dropin.parent().unwrap()).unwrap();
    fs::write(&dropin, "# custom fish note\n").unwrap();
    enable_with_env(
        &opts,
        &LifecycleEnv {
            home: Some(&home),
            zdotdir: None,
            xdg_config_home: Some(&xdg),
            shell_env: Some("/usr/bin/fish"),
            default_data_override: None,
            hybrid_active: false,
            platform_supported: true,
        },
    )
    .unwrap();
    disable_with_env(
        &opts,
        &LifecycleEnv {
            home: Some(&home),
            zdotdir: None,
            xdg_config_home: Some(&xdg),
            shell_env: Some("/usr/bin/fish"),
            default_data_override: None,
            hybrid_active: false,
            platform_supported: true,
        },
    )
    .unwrap();
    assert_eq!(fs::read_to_string(&dropin).unwrap(), "# custom fish note\n");
}

#[cfg(unix)]
#[test]
fn symlinked_rc_default_is_non_mutating_manual_required_and_writes_env_sh() {
    use std::os::unix::fs::{PermissionsExt, symlink};

    let tmp = TempDir::new().unwrap();
    let home = tmp.path().join("home");
    let data = tmp.path().join("data");
    let bin = tmp.path().join("agy-companion");
    fs::create_dir_all(&home).unwrap();

    let dotfiles_target = tmp.path().join("dotfiles-zlogin");
    fs::write(&dotfiles_target, "# nix managed\n").unwrap();
    let zlogin = home.join(".zlogin");
    symlink(&dotfiles_target, &zlogin).unwrap();

    let opts = make_opts(ShellKind::Zsh, None, data.clone(), bin, false);
    let st = enable_with_env(
        &opts,
        &LifecycleEnv {
            home: Some(&home),
            zdotdir: None,
            xdg_config_home: None,
            shell_env: Some("/bin/zsh"),
            default_data_override: None,
            hybrid_active: false,
            platform_supported: true,
        },
    )
    .unwrap();
    assert!(!st.enabled);
    assert_eq!(st.state, IntegrationState::ManualRequired);
    assert_eq!(
        st.symlink_target.as_deref(),
        Some(dotfiles_target.as_path())
    );
    assert!(
        st.manual_snippet
            .as_deref()
            .unwrap()
            .contains(HOOK_BEGIN_MARKER)
    );
    // Target untouched, symlink still symlink, env.sh written 0600
    assert!(
        fs::symlink_metadata(&zlogin)
            .unwrap()
            .file_type()
            .is_symlink()
    );
    assert_eq!(
        fs::read_to_string(&dotfiles_target).unwrap(),
        "# nix managed\n"
    );
    assert_eq!(
        fs::metadata(data.join("env.sh"))
            .unwrap()
            .permissions()
            .mode()
            & 0o777,
        0o600
    );

    // Subsequent status() preserves ManualRequired state
    let polled = status_with_env(
        &opts,
        &LifecycleEnv {
            home: Some(&home),
            zdotdir: None,
            xdg_config_home: None,
            shell_env: Some("/bin/zsh"),
            default_data_override: None,
            hybrid_active: false,
            platform_supported: true,
        },
    )
    .unwrap();
    assert_eq!(polled.state, IntegrationState::ManualRequired);
    assert!(polled.manual_snippet.is_some());

    // Dangling symlink also non-mutating ManualRequired
    fs::remove_file(&dotfiles_target).unwrap();
    let dangling_st = enable_with_env(
        &opts,
        &LifecycleEnv {
            home: Some(&home),
            zdotdir: None,
            xdg_config_home: None,
            shell_env: Some("/bin/zsh"),
            default_data_override: None,
            hybrid_active: false,
            platform_supported: true,
        },
    )
    .unwrap();
    assert_eq!(dangling_st.state, IntegrationState::ManualRequired);
    assert!(!dangling_st.enabled);
    assert!(
        fs::symlink_metadata(&zlogin)
            .unwrap()
            .file_type()
            .is_symlink()
    );
}

#[cfg(unix)]
#[test]
fn follow_rc_symlink_updates_regular_target_and_rejects_dangling_and_fifo() {
    use std::os::unix::fs::symlink;

    let tmp = TempDir::new().unwrap();
    let home = tmp.path().join("home");
    let data = tmp.path().join("data");
    let bin = tmp.path().join("agy-companion");
    fs::create_dir_all(&home).unwrap();

    let target = tmp.path().join("stow-zlogin");
    fs::write(&target, "# stow\n").unwrap();
    let zlogin = home.join(".zlogin");
    symlink(&target, &zlogin).unwrap();

    let opts = make_opts(ShellKind::Zsh, None, data, bin, true);
    let st = enable_with_env(
        &opts,
        &LifecycleEnv {
            home: Some(&home),
            zdotdir: None,
            xdg_config_home: None,
            shell_env: Some("/bin/zsh"),
            default_data_override: None,
            hybrid_active: false,
            platform_supported: true,
        },
    )
    .unwrap();
    assert!(st.enabled);
    assert_eq!(st.state, IntegrationState::Enabled);
    assert!(
        fs::symlink_metadata(&zlogin)
            .unwrap()
            .file_type()
            .is_symlink()
    );
    assert!(
        fs::read_to_string(&target)
            .unwrap()
            .contains(HOOK_BEGIN_MARKER)
    );

    // Dangling symlink fails closed with follow_rc_symlink = true
    fs::remove_file(&target).unwrap();
    assert!(matches!(
        enable_with_env(
            &opts,
            &LifecycleEnv {
                home: Some(&home),
                zdotdir: None,
                xdg_config_home: None,
                shell_env: Some("/bin/zsh"),
                default_data_override: None,
                hybrid_active: false,
                platform_supported: true,
            },
        ),
        Err(CoreError::InvalidPath { .. })
    ));

    // Symlink to FIFO fails closed without hanging
    let fifo = tmp.path().join("fifo-target");
    assert!(
        Command::new("mkfifo")
            .arg(&fifo)
            .status()
            .unwrap()
            .success()
    );
    fs::remove_file(&zlogin).unwrap();
    symlink(&fifo, &zlogin).unwrap();
    assert!(matches!(
        enable_with_env(
            &opts,
            &LifecycleEnv {
                home: Some(&home),
                zdotdir: None,
                xdg_config_home: None,
                shell_env: Some("/bin/zsh"),
                default_data_override: None,
                hybrid_active: false,
                platform_supported: true,
            },
        ),
        Err(CoreError::InvalidPath { .. })
    ));
}

#[cfg(unix)]
#[test]
fn readonly_rc_file_returns_manual_required_without_mutation() {
    use std::os::unix::fs::PermissionsExt;

    let tmp = TempDir::new().unwrap();
    let home = tmp.path().join("home");
    let data = tmp.path().join("data");
    let bin = tmp.path().join("agy-companion");
    fs::create_dir_all(&home).unwrap();
    let zlogin = home.join(".zlogin");
    fs::write(&zlogin, "# readonly\n").unwrap();
    fs::set_permissions(&zlogin, fs::Permissions::from_mode(0o444)).unwrap();

    let opts = make_opts(ShellKind::Zsh, None, data.clone(), bin, false);
    let st = enable_with_env(
        &opts,
        &LifecycleEnv {
            home: Some(&home),
            zdotdir: None,
            xdg_config_home: None,
            shell_env: Some("/bin/zsh"),
            default_data_override: None,
            hybrid_active: false,
            platform_supported: true,
        },
    )
    .unwrap();
    assert!(!st.enabled);
    assert_eq!(st.state, IntegrationState::ManualRequired);
    assert!(st.manual_snippet.is_some());
    assert_eq!(fs::read_to_string(&zlogin).unwrap(), "# readonly\n");
    assert_eq!(
        fs::metadata(data.join("env.sh"))
            .unwrap()
            .permissions()
            .mode()
            & 0o777,
        0o600
    );

    // Restore mode so TempDir cleans up cleanly everywhere
    fs::set_permissions(&zlogin, fs::Permissions::from_mode(0o644)).unwrap();
}

#[test]
fn marker_parser_fails_closed_with_exact_1_based_line_numbers() {
    let tmp = TempDir::new().unwrap();
    let home = tmp.path().join("home");
    let data = tmp.path().join("data");
    let bin = tmp.path().join("agy-companion");
    fs::create_dir_all(&home).unwrap();
    let zlogin = home.join(".zlogin");
    let opts = make_opts(ShellKind::Zsh, None, data, bin, false);

    let cases = [
        (format!("line1\n{HOOK_BEGIN_MARKER}\nline3\n"), "line 2"),
        (format!("line1\nline2\n{HOOK_END_MARKER}\n"), "line 3"),
        (
            format!("{HOOK_END_MARKER}\n{HOOK_BEGIN_MARKER}\n"),
            "line 1",
        ),
        (
            format!("{HOOK_BEGIN_MARKER}\n{HOOK_BEGIN_MARKER}\n{HOOK_END_MARKER}\n"),
            "[1, 2]",
        ),
        (
            format!("{HOOK_BEGIN_MARKER}\n{HOOK_END_MARKER}\n{HOOK_END_MARKER}\n"),
            "[2, 3]",
        ),
        (
            format!(
                "{HOOK_BEGIN_MARKER}\n{HOOK_END_MARKER}\n{HOOK_BEGIN_MARKER}\n{HOOK_END_MARKER}\n"
            ),
            "[1, 3]",
        ),
    ];

    for (bad_content, expected_substr) in cases {
        fs::write(&zlogin, &bad_content).unwrap();
        let err = enable_with_env(
            &opts,
            &LifecycleEnv {
                home: Some(&home),
                zdotdir: None,
                xdg_config_home: None,
                shell_env: Some("/bin/zsh"),
                default_data_override: None,
                hybrid_active: false,
                platform_supported: true,
            },
        )
        .unwrap_err();
        let msg = err.to_string();
        assert!(
            matches!(err, CoreError::CorruptedMarkers { .. }),
            "expected CorruptedMarkers, got {err:?}"
        );
        assert!(
            msg.contains(expected_substr),
            "expected {expected_substr:?} in error message: {msg}"
        );
        // Zero file mutation
        assert_eq!(fs::read_to_string(&zlogin).unwrap(), bad_content);
    }
}

#[cfg(unix)]
#[test]
fn rc_fifo_and_oversized_file_fail_closed_without_hanging() {
    use std::sync::mpsc;
    use std::time::Duration;

    let tmp = TempDir::new().unwrap();
    let home = tmp.path().join("home");
    let data = tmp.path().join("data");
    let bin = tmp.path().join("agy-companion");
    fs::create_dir_all(&home).unwrap();

    let fifo_rc = home.join(".zlogin");
    assert!(
        Command::new("mkfifo")
            .arg(&fifo_rc)
            .status()
            .unwrap()
            .success()
    );
    let opts = make_opts(ShellKind::Zsh, None, data.clone(), bin.clone(), false);
    let home_clone = home.clone();
    let opts_clone = opts.clone();
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        let res = status_with_env(
            &opts_clone,
            &LifecycleEnv {
                home: Some(&home_clone),
                zdotdir: None,
                xdg_config_home: None,
                shell_env: Some("/bin/zsh"),
                default_data_override: None,
                hybrid_active: false,
                platform_supported: true,
            },
        );
        let _ = tx.send(res);
    });
    let res = rx
        .recv_timeout(Duration::from_secs(1))
        .expect("FIFO RC must fail fast");
    assert!(matches!(
        res,
        Err(CoreError::Fs(FsError::NotRegularFile { .. }))
    ));
    fs::remove_file(&fifo_rc).unwrap();

    // > 1 MiB RC file fails closed
    let huge = vec![b'a'; (MAX_RC_FILE_BYTES as usize) + 1];
    fs::write(&fifo_rc, huge).unwrap();
    let err = status_with_env(
        &opts,
        &LifecycleEnv {
            home: Some(&home),
            zdotdir: None,
            xdg_config_home: None,
            shell_env: Some("/bin/zsh"),
            default_data_override: None,
            hybrid_active: false,
            platform_supported: true,
        },
    )
    .unwrap_err();
    assert!(matches!(
        err,
        CoreError::Fs(FsError::SizeLimitExceeded { .. })
    ));
}

#[test]
fn strip_all_known_rc_files_cleans_all_7_files_honors_dry_run_and_skips_symlinks() {
    let tmp = TempDir::new().unwrap();
    let home = tmp.path().join("home");
    let xdg = tmp.path().join("xdg");
    fs::create_dir_all(&home).unwrap();
    let fish_path = xdg.join("fish/conf.d/agy-companion.fish");
    fs::create_dir_all(fish_path.parent().unwrap()).unwrap();

    let block = wrap_with_markers("[ -r env.sh ] && . env.sh");
    fs::write(home.join(".zlogin"), format!("{block}\n")).unwrap();
    fs::write(home.join(".zprofile"), format!("# keep\n{block}\n")).unwrap();
    fs::write(home.join(".bash_profile"), format!("# keep\n{block}\n")).unwrap();
    fs::write(home.join(".bash_login"), format!("# keep\n{block}\n")).unwrap();
    fs::write(home.join(".profile"), format!("# keep\n{block}\n")).unwrap();
    fs::write(&fish_path, format!("{block}\n")).unwrap();

    #[cfg(unix)]
    {
        use std::os::unix::fs::symlink;
        let linked_zshrc = tmp.path().join("managed-zshrc");
        fs::write(&linked_zshrc, format!("# managed\n{block}\n")).unwrap();
        symlink(&linked_zshrc, home.join(".zshrc")).unwrap();
    }
    #[cfg(not(unix))]
    {
        fs::write(home.join(".zshrc"), format!("# keep\n{block}\n")).unwrap();
    }

    // Dry run mutates nothing
    let dry = strip_all_known_rc_files(&home, None, Some(&xdg), true).unwrap();
    assert_eq!(dry.deleted_files.len(), 2);
    assert!(home.join(".zlogin").exists());
    assert!(fish_path.exists());
    #[cfg(unix)]
    {
        assert_eq!(dry.modified_files.len(), 4);
        assert_eq!(dry.skipped_manual_files, vec![home.join(".zshrc")]);
    }

    // Live strip deletes empty .zlogin & agy-companion.fish, edits regular RCs, skips symlink
    let live = strip_all_known_rc_files(&home, None, Some(&xdg), false).unwrap();
    assert_eq!(live.deleted_files.len(), 2);
    assert!(!home.join(".zlogin").exists());
    assert!(!fish_path.exists());
    assert_eq!(
        fs::read_to_string(home.join(".zprofile")).unwrap(),
        "# keep\n"
    );
    assert_eq!(
        fs::read_to_string(home.join(".bash_profile")).unwrap(),
        "# keep\n"
    );
    assert_eq!(
        fs::read_to_string(home.join(".bash_login")).unwrap(),
        "# keep\n"
    );
    assert_eq!(
        fs::read_to_string(home.join(".profile")).unwrap(),
        "# keep\n"
    );
    #[cfg(unix)]
    {
        assert_eq!(live.modified_files.len(), 4);
        assert_eq!(live.skipped_manual_files, vec![home.join(".zshrc")]);
        assert!(
            fs::read_to_string(tmp.path().join("managed-zshrc"))
                .unwrap()
                .contains(HOOK_BEGIN_MARKER)
        );
    }
}

#[test]
fn windows_unsupported_status_and_json_roundtrip() {
    let opts = IntegrationOptions::default();
    let st = status_with_env(
        &opts,
        &LifecycleEnv {
            home: None,
            zdotdir: None,
            xdg_config_home: None,
            shell_env: None,
            default_data_override: None,
            hybrid_active: false,
            platform_supported: false,
        },
    )
    .unwrap();
    assert_eq!(st.state, IntegrationState::Unsupported);
    assert!(!st.enabled);
    assert_eq!(st.state(), IntegrationState::Unsupported);

    let val = serde_json::to_value(&st).unwrap();
    let obj = val.as_object().unwrap();
    assert!(!obj.contains_key("supported"));
    assert!(!obj.contains_key("manual_required"));
    assert_eq!(obj["state"], "unsupported");
    for key in [
        "state",
        "enabled",
        "hybrid_active",
        "shell",
        "rc_path",
        "env_script_path",
        "binary_path",
        "symlink_target",
        "manual_snippet",
        "message",
    ] {
        assert!(obj.contains_key(key), "missing IntegrationStatus key {key}");
    }
    let roundtrip: IntegrationStatus = serde_json::from_value(val).unwrap();
    assert_eq!(roundtrip, st);

    let report = StripReport {
        modified_files: vec![PathBuf::from("/tmp/a")],
        deleted_files: vec![PathBuf::from("/tmp/b")],
        skipped_manual_files: vec![PathBuf::from("/tmp/c")],
    };
    let report_rt: StripReport =
        serde_json::from_str(&serde_json::to_string(&report).unwrap()).unwrap();
    assert_eq!(report_rt, report);
}

#[test]
fn is_loopback_hybrid_url_validates_loopback_ports_only() {
    assert!(is_loopback_hybrid_url("http://127.0.0.1:4141"));
    assert!(is_loopback_hybrid_url("http://localhost:4242"));
    assert!(is_loopback_hybrid_url("  http://127.0.0.1:4141/  "));

    for bad in [
        "",
        "https://127.0.0.1:4141",
        "http://example.com:4141",
        "http://127.0.0.1:0",
        "http://127.0.0.1",
        "http://user:pass@127.0.0.1:4141",
        "http://[::1]:4141",
    ] {
        assert!(!is_loopback_hybrid_url(bad), "should reject {bad:?}");
    }

    assert!(detect_hybrid_active_from(
        Some("http://127.0.0.1:4141"),
        None,
        None,
    ));
    assert!(!detect_hybrid_active_from(
        Some("https://cloudcode-pa.googleapis.com"),
        Some("1"),
        None,
    ));
    assert!(!detect_hybrid_active_from(
        Some("https://cloudcode-pa.googleapis.com"),
        None,
        Some("1"),
    ));
    assert!(!detect_hybrid_active_from(
        Some("https://cloudcode-pa.googleapis.com"),
        Some("1"),
        Some("1"),
    ));
    assert!(detect_hybrid_active_from(None, Some("1"), None));
    assert!(detect_hybrid_active_from(None, None, Some("1")));
    assert!(detect_hybrid_active_from(Some("   "), None, Some(" 1 ")));
    assert!(!detect_hybrid_active_from(None, None, Some("0")));
    assert!(!detect_hybrid_active_from(None, None, Some("true")));
    assert!(!detect_hybrid_active_from(None, None, None));
}

#[test]
fn resolve_stable_binary_path_precedence() {
    let tmp = TempDir::new().unwrap();
    let data_dir = tmp.path().join("data");
    let bin_dir = data_dir.join("bin");
    fs::create_dir_all(&bin_dir).unwrap();
    let bin_name = if cfg!(windows) {
        "agy-companion.exe"
    } else {
        "agy-companion"
    };
    let stable = bin_dir.join(bin_name);
    let fallback_exe = tmp.path().join("fallback-exe");
    fs::write(&fallback_exe, "exe").unwrap();

    // Explicit override wins
    let custom = tmp.path().join("explicit-bin");
    assert_eq!(
        resolve_stable_binary_path_with(
            Some(&custom),
            Some(&data_dir),
            || Ok(fallback_exe.clone())
        )
        .unwrap(),
        custom
    );

    // Non-executable stable file is skipped on Unix -> falls back to current_exe
    fs::write(&stable, "#!/bin/sh\n").unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&stable, fs::Permissions::from_mode(0o644)).unwrap();
        let resolved =
            resolve_stable_binary_path_with(None, Some(&data_dir), || Ok(fallback_exe.clone()))
                .unwrap();
        assert_eq!(resolved, fs::canonicalize(&fallback_exe).unwrap());

        // Executable stable file beats current_exe
        fs::set_permissions(&stable, fs::Permissions::from_mode(0o755)).unwrap();
        let resolved_stable =
            resolve_stable_binary_path_with(None, Some(&data_dir), || Ok(fallback_exe.clone()))
                .unwrap();
        assert_eq!(resolved_stable, stable);
    }
}

#[cfg(unix)]
#[test]
fn env_sh_live_posix_shell_matrix() {
    use std::os::unix::fs::PermissionsExt;

    let tmp = TempDir::new().unwrap();
    let stub = tmp.path().join("fake-agy-companion");
    fs::write(&stub, "#!/bin/sh\nprintf '%s\\n' \"${FAKE_HOOK_PORT:-}\"\n").unwrap();
    fs::set_permissions(&stub, fs::Permissions::from_mode(0o755)).unwrap();

    let env_sh = tmp.path().join("env.sh");
    write_env_script(&env_sh, &stub).unwrap();

    let shells: Vec<&str> = ["/bin/sh", "/bin/bash", "/bin/zsh"]
        .into_iter()
        .filter(|s| Path::new(s).is_file())
        .collect();
    assert!(!shells.is_empty());

    for shell in shells {
        // Syntax check
        let syntax = Command::new(shell).arg("-n").arg(&env_sh).status().unwrap();
        assert!(syntax.success(), "{shell} -n failed on env.sh");

        // 1. Activation + double-source idempotency + S2 real GEMINI_API_KEY & GOOGLE_API_KEY untouched
        let out = Command::new(shell)
            .arg("-c")
            .arg(format!(
                "set -eu; . '{}'; . '{}'; printf 'CC=%s|GB=%s|GK=%s|GOOG=%s|PRE=%s|VARS=%s|ACT=%s\\n' \
                \"${{CLOUD_CODE_URL:-}}\" \"${{GOOGLE_GEMINI_BASE_URL:-}}\" \"${{GEMINI_API_KEY:-}}\" \
                \"${{GOOGLE_API_KEY:-}}\" \"${{JETSKI_MCP_ENV_RESTORE_PREFIX:-}}\" \
                \"${{JETSKI_MCP_ENV_RESTORE_VARS:-}}\" \"${{AGY_COMPANION_HOOK_ACTIVE:-}}\"",
                env_sh.display(),
                env_sh.display()
            ))
            .env_clear()
            .env("PATH", "/usr/bin:/bin")
            .env("ANTIGRAVITY_RESOLVING_ENVIRONMENT", "1")
            .env("FAKE_HOOK_PORT", "4321")
            .env("GEMINI_API_KEY", "my-real-secret-key")
            .env("GOOGLE_API_KEY", "my-google-secret-key")
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "{shell} activation failed: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        let stdout = String::from_utf8(out.stdout).unwrap();
        assert_eq!(
            stdout.trim(),
            "CC=http://127.0.0.1:4321|GB=http://127.0.0.1:4321|GK=my-real-secret-key|GOOG=my-google-secret-key|PRE=AGY_COMPANION_ORIG_|VARS=CLOUD_CODE_URL,GOOGLE_GEMINI_BASE_URL|ACT=1",
            "{shell} activation mismatch"
        );

        // 2. SSH guard blocks by default, AGY_COMPANION_ALLOW_REMOTE_SSH=1 overrides
        let ssh_blocked = Command::new(shell)
            .arg("-c")
            .arg(format!(
                "set -eu; . '{}'; printf '%s' \"${{CLOUD_CODE_URL:-}}\"",
                env_sh.display()
            ))
            .env_clear()
            .env("PATH", "/usr/bin:/bin")
            .env("ANTIGRAVITY_RESOLVING_ENVIRONMENT", "1")
            .env("FAKE_HOOK_PORT", "4321")
            .env("SSH_CONNECTION", "1.2.3.4 1234 5.6.7.8 22")
            .output()
            .unwrap();
        assert!(ssh_blocked.status.success());
        assert_eq!(String::from_utf8(ssh_blocked.stdout).unwrap(), "");

        let ssh_allowed = Command::new(shell)
            .arg("-c")
            .arg(format!(
                "set -eu; . '{}'; printf '%s' \"${{CLOUD_CODE_URL:-}}\"",
                env_sh.display()
            ))
            .env_clear()
            .env("PATH", "/usr/bin:/bin")
            .env("ANTIGRAVITY_RESOLVING_ENVIRONMENT", "1")
            .env("FAKE_HOOK_PORT", "4321")
            .env("SSH_CONNECTION", "1.2.3.4 1234 5.6.7.8 22")
            .env("AGY_COMPANION_ALLOW_REMOTE_SSH", "1")
            .output()
            .unwrap();
        assert!(ssh_allowed.status.success());
        assert_eq!(
            String::from_utf8(ssh_allowed.stdout).unwrap(),
            "http://127.0.0.1:4321"
        );

        // 3. Hostile/overflow/leading-zero port rejection
        for bad_port in [
            "",
            "0",
            "04141",
            "65536",
            "99999999999999999999",
            "$(id)",
            "4141;id",
        ] {
            let bad_out = Command::new(shell)
                .arg("-c")
                .arg(format!(
                    "set -eu; . '{}'; printf '%s' \"${{CLOUD_CODE_URL:-}}\"",
                    env_sh.display()
                ))
                .env_clear()
                .env("PATH", "/usr/bin:/bin")
                .env("ANTIGRAVITY_RESOLVING_ENVIRONMENT", "1")
                .env("FAKE_HOOK_PORT", bad_port)
                .output()
                .unwrap();
            assert!(
                bad_out.status.success(),
                "{shell} failed on port {bad_port:?}"
            );
            assert_eq!(
                String::from_utf8(bad_out.stdout).unwrap(),
                "",
                "{shell} accepted hostile port {bad_port:?}"
            );
        }

        // 4. Custom prefix + existing _VARS + 3-state "" vs unset + integrated terminal scrub + value-guard
        let scrub = Command::new(shell)
            .arg("-c")
            .arg(format!(
                "set -eu; . '{}'; \
                printf 'PRE=%s|VARS=%s|MC=%s|GK=%s\\n' \
                \"$JETSKI_MCP_ENV_RESTORE_PREFIX\" \"$JETSKI_MCP_ENV_RESTORE_VARS\" \
                \"${{MY_CLOUD_CODE_URL+SET:$MY_CLOUD_CODE_URL}}\" \"$GEMINI_API_KEY\"; \
                unset ANTIGRAVITY_RESOLVING_ENVIRONMENT; \
                export GOOGLE_GEMINI_BASE_URL='https://user-override.example'; \
                . '{}'; \
                printf 'AFTER_CC=%s|AFTER_GB=%s|AFTER_GK=%s|AFTER_PRE=%s|AFTER_VARS=%s|AFTER_MC=%s\\n' \
                \"${{CLOUD_CODE_URL+SET:$CLOUD_CODE_URL}}\" \"${{GOOGLE_GEMINI_BASE_URL:-}}\" \
                \"${{GEMINI_API_KEY+SET}}\" \"${{JETSKI_MCP_ENV_RESTORE_PREFIX:-}}\" \
                \"${{JETSKI_MCP_ENV_RESTORE_VARS:-}}\" \"${{MY_CLOUD_CODE_URL+SET}}\"",
                env_sh.display(),
                env_sh.display()
            ))
            .env_clear()
            .env("PATH", "/usr/bin:/bin")
            .env("ANTIGRAVITY_RESOLVING_ENVIRONMENT", "1")
            .env("FAKE_HOOK_PORT", "4321")
            .env("JETSKI_MCP_ENV_RESTORE_PREFIX", "MY_")
            .env("JETSKI_MCP_ENV_RESTORE_VARS", "FOO")
            .env("CLOUD_CODE_URL", "")
            .output()
            .unwrap();
        assert!(
            scrub.status.success(),
            "{shell} scrub failed: {}",
            String::from_utf8_lossy(&scrub.stderr)
        );
        let lines: Vec<String> = String::from_utf8(scrub.stdout)
            .unwrap()
            .lines()
            .map(str::to_string)
            .collect();
        assert_eq!(lines.len(), 2);
        assert_eq!(
            lines[0],
            "PRE=MY_|VARS=FOO,CLOUD_CODE_URL,GOOGLE_GEMINI_BASE_URL,GEMINI_API_KEY|MC=SET:|GK=agy-companion-local"
        );
        assert_eq!(
            lines[1],
            "AFTER_CC=SET:|AFTER_GB=https://user-override.example|AFTER_GK=|AFTER_PRE=MY_|AFTER_VARS=FOO|AFTER_MC="
        );
    }
}

#[test]
fn integration_status_state_derivation() {
    let paths = || IntegrationPaths {
        shell: ShellKind::Zsh,
        rc_path: PathBuf::from("/tmp/.zlogin"),
        env_script_path: Some(PathBuf::from("/tmp/env.sh")),
        binary_path: PathBuf::from("/tmp/agy-companion"),
    };

    let mut disabled = IntegrationStatus::disabled(paths(), None, false);
    assert_eq!(disabled.state(), IntegrationState::Disabled);
    assert!(!disabled.enabled);
    disabled.mark_hybrid_active();
    assert!(disabled.hybrid_active);
    assert_eq!(disabled.state(), IntegrationState::Disabled);

    let active = IntegrationStatus::enabled(paths(), None, true);
    assert_eq!(active.state(), IntegrationState::Active);
    assert!(active.enabled);
    assert!(active.hybrid_active);

    let mut pending = IntegrationStatus::enabled(paths(), None, false);
    assert_eq!(pending.state(), IntegrationState::Enabled);
    assert!(pending.enabled);
    assert!(!pending.hybrid_active);
    pending.mark_hybrid_active();
    assert!(pending.hybrid_active);
    assert_eq!(pending.state(), IntegrationState::Active);

    let mut manual =
        IntegrationStatus::manual_required(paths(), false, None, None, false, "manual");
    assert_eq!(manual.state(), IntegrationState::ManualRequired);
    manual.mark_hybrid_active();
    assert!(manual.hybrid_active);
    assert_eq!(manual.state(), IntegrationState::ManualRequired);

    let mut unsupported = IntegrationStatus::unsupported(false, "windows");
    assert_eq!(unsupported.state(), IntegrationState::Unsupported);
    assert!(!unsupported.enabled);
    unsupported.mark_hybrid_active();
    assert!(unsupported.hybrid_active);
    assert_eq!(unsupported.state(), IntegrationState::Unsupported);

    let fallback = IntegrationStatus::error_fallback(false, "probe failed");
    let expected_fallback_state = if is_platform_supported() {
        IntegrationState::Disabled
    } else {
        IntegrationState::Unsupported
    };
    assert_eq!(fallback.state(), expected_fallback_state);
    assert!(!fallback.enabled);

    assert!(is_valid_restore_prefix("AGY_COMPANION_ORIG_"));
    assert!(is_valid_restore_prefix("_A1"));
    assert!(!is_valid_restore_prefix(""));
    assert!(!is_valid_restore_prefix("1ABC"));
    assert!(!is_valid_restore_prefix("$(evil)"));
    assert!(!is_valid_restore_prefix("BAD-PREFIX"));
}

#[cfg(unix)]
#[test]
fn symlink_to_fifo_rc_non_mutating_default_and_fail_closed_when_followed() {
    use std::os::unix::fs::{PermissionsExt, symlink};

    let tmp = TempDir::new().unwrap();
    let home = tmp.path().join("home");
    let xdg = tmp.path().join("xdg");
    let data = tmp.path().join("data");
    let bin = tmp.path().join("agy-companion");
    fs::create_dir_all(&home).unwrap();

    let fifo = tmp.path().join("named-pipe");
    assert!(
        Command::new("mkfifo")
            .arg(&fifo)
            .status()
            .unwrap()
            .success()
    );
    let zlogin = home.join(".zlogin");
    symlink(&fifo, &zlogin).unwrap();

    let opts_default = make_opts(ShellKind::Zsh, None, data.clone(), bin.clone(), false);

    // status before enable must not crash or open FIFO
    let st_before = status_with_env(
        &opts_default,
        &LifecycleEnv {
            home: Some(&home),
            zdotdir: None,
            xdg_config_home: None,
            shell_env: Some("/bin/zsh"),
            default_data_override: None,
            hybrid_active: false,
            platform_supported: true,
        },
    )
    .unwrap();
    assert!(!st_before.enabled);
    assert_eq!(st_before.state, IntegrationState::Disabled);
    assert_eq!(st_before.symlink_target.as_deref(), Some(fifo.as_path()));

    // default enable writes 0600 env.sh and returns ManualRequired without opening FIFO
    let st_en = enable_with_env(
        &opts_default,
        &LifecycleEnv {
            home: Some(&home),
            zdotdir: None,
            xdg_config_home: None,
            shell_env: Some("/bin/zsh"),
            default_data_override: None,
            hybrid_active: false,
            platform_supported: true,
        },
    )
    .unwrap();
    assert!(!st_en.enabled);
    assert_eq!(st_en.state, IntegrationState::ManualRequired);
    assert!(st_en.manual_snippet.is_some());
    assert_eq!(st_en.symlink_target.as_deref(), Some(fifo.as_path()));
    assert_eq!(
        fs::metadata(data.join("env.sh"))
            .unwrap()
            .permissions()
            .mode()
            & 0o777,
        0o600
    );

    // status after enable returns ManualRequired without crashing
    let st_after = status_with_env(
        &opts_default,
        &LifecycleEnv {
            home: Some(&home),
            zdotdir: None,
            xdg_config_home: None,
            shell_env: Some("/bin/zsh"),
            default_data_override: None,
            hybrid_active: false,
            platform_supported: true,
        },
    )
    .unwrap();
    assert!(!st_after.enabled);
    assert_eq!(st_after.state, IntegrationState::ManualRequired);
    assert!(st_after.manual_snippet.is_some());
    assert_eq!(st_after.symlink_target.as_deref(), Some(fifo.as_path()));

    // Fish symlink-to-FIFO status returns ManualRequired without crashing
    let fish_dropin = xdg.join("fish/conf.d/agy-companion.fish");
    fs::create_dir_all(fish_dropin.parent().unwrap()).unwrap();
    symlink(&fifo, &fish_dropin).unwrap();
    let fish_opts = make_opts(ShellKind::Fish, None, data.clone(), bin.clone(), false);
    let fish_st = status_with_env(
        &fish_opts,
        &LifecycleEnv {
            home: Some(&home),
            zdotdir: None,
            xdg_config_home: Some(&xdg),
            shell_env: Some("/usr/bin/fish"),
            default_data_override: None,
            hybrid_active: false,
            platform_supported: true,
        },
    )
    .unwrap();
    assert!(!fish_st.enabled);
    assert_eq!(fish_st.state, IntegrationState::ManualRequired);
    assert_eq!(fish_st.symlink_target.as_deref(), Some(fifo.as_path()));

    // disable neutralizes env.sh cleanly without opening or mutating symlink target
    let st_dis = disable_with_env(
        &opts_default,
        &LifecycleEnv {
            home: Some(&home),
            zdotdir: None,
            xdg_config_home: None,
            shell_env: Some("/bin/zsh"),
            default_data_override: None,
            hybrid_active: false,
            platform_supported: true,
        },
    )
    .unwrap();
    assert!(!st_dis.enabled);
    assert!(!data.join("env.sh").exists());
    assert!(
        fs::symlink_metadata(&zlogin)
            .unwrap()
            .file_type()
            .is_symlink()
    );

    // --follow-rc-symlink fails closed BEFORE writing env.sh
    let fresh_data = tmp.path().join("fresh-data");
    let opts_follow = make_opts(ShellKind::Zsh, None, fresh_data.clone(), bin, true);
    let err = enable_with_env(
        &opts_follow,
        &LifecycleEnv {
            home: Some(&home),
            zdotdir: None,
            xdg_config_home: None,
            shell_env: Some("/bin/zsh"),
            default_data_override: None,
            hybrid_active: false,
            platform_supported: true,
        },
    )
    .unwrap_err();
    assert!(matches!(err, CoreError::InvalidPath { .. }));
    assert!(
        !fresh_data.join("env.sh").exists(),
        "orphan env.sh must not be created when follow_rc_symlink fails closed on FIFO symlink"
    );
}

#[cfg(unix)]
#[test]
fn enable_fail_closed_errors_never_leave_orphan_env_sh() {
    use std::os::unix::fs::symlink;

    let tmp = TempDir::new().unwrap();
    let bin = tmp.path().join("agy-companion");

    // 1. Corrupted markers on regular RC
    {
        let home = tmp.path().join("case_corrupt_reg/home");
        let data = tmp.path().join("case_corrupt_reg/data");
        fs::create_dir_all(&home).unwrap();
        fs::write(
            home.join(".zlogin"),
            format!("line1\n{HOOK_BEGIN_MARKER}\n"),
        )
        .unwrap();
        let opts = make_opts(ShellKind::Zsh, None, data.clone(), bin.clone(), false);
        let err = enable_with_env(
            &opts,
            &LifecycleEnv {
                home: Some(&home),
                zdotdir: None,
                xdg_config_home: None,
                shell_env: Some("/bin/zsh"),
                default_data_override: None,
                hybrid_active: false,
                platform_supported: true,
            },
        )
        .unwrap_err();
        assert!(matches!(err, CoreError::CorruptedMarkers { .. }));
        assert!(!data.join("env.sh").exists());
    }

    // 2. Corrupted markers on symlink-to-regular RC
    {
        let home = tmp.path().join("case_corrupt_sym/home");
        let data = tmp.path().join("case_corrupt_sym/data");
        fs::create_dir_all(&home).unwrap();
        let target = tmp.path().join("case_corrupt_sym/target_zlogin");
        fs::write(&target, format!("{HOOK_END_MARKER}\n")).unwrap();
        symlink(&target, home.join(".zlogin")).unwrap();
        let opts = make_opts(ShellKind::Zsh, None, data.clone(), bin.clone(), false);
        let err = enable_with_env(
            &opts,
            &LifecycleEnv {
                home: Some(&home),
                zdotdir: None,
                xdg_config_home: None,
                shell_env: Some("/bin/zsh"),
                default_data_override: None,
                hybrid_active: false,
                platform_supported: true,
            },
        )
        .unwrap_err();
        assert!(matches!(err, CoreError::CorruptedMarkers { .. }));
        assert!(!data.join("env.sh").exists());
    }

    // 3. Direct FIFO RC
    {
        let home = tmp.path().join("case_direct_fifo/home");
        let data = tmp.path().join("case_direct_fifo/data");
        fs::create_dir_all(&home).unwrap();
        let zlogin = home.join(".zlogin");
        assert!(
            Command::new("mkfifo")
                .arg(&zlogin)
                .status()
                .unwrap()
                .success()
        );
        let opts = make_opts(ShellKind::Zsh, None, data.clone(), bin.clone(), false);
        let err = enable_with_env(
            &opts,
            &LifecycleEnv {
                home: Some(&home),
                zdotdir: None,
                xdg_config_home: None,
                shell_env: Some("/bin/zsh"),
                default_data_override: None,
                hybrid_active: false,
                platform_supported: true,
            },
        )
        .unwrap_err();
        assert!(matches!(err, CoreError::Fs(FsError::NotRegularFile { .. })));
        assert!(!data.join("env.sh").exists());
    }

    // 4. Dangling symlink with follow_rc_symlink = true
    {
        let home = tmp.path().join("case_dangling_follow/home");
        let data = tmp.path().join("case_dangling_follow/data");
        fs::create_dir_all(&home).unwrap();
        let missing = tmp.path().join("case_dangling_follow/does-not-exist");
        symlink(&missing, home.join(".zlogin")).unwrap();
        let opts = make_opts(ShellKind::Zsh, None, data.clone(), bin.clone(), true);
        let err = enable_with_env(
            &opts,
            &LifecycleEnv {
                home: Some(&home),
                zdotdir: None,
                xdg_config_home: None,
                shell_env: Some("/bin/zsh"),
                default_data_override: None,
                hybrid_active: false,
                platform_supported: true,
            },
        )
        .unwrap_err();
        assert!(matches!(err, CoreError::InvalidPath { .. }));
        assert!(!data.join("env.sh").exists());
    }
}

#[cfg(unix)]
#[test]
fn crlf_without_trailing_newline_roundtrips_byte_for_byte_across_enable_disable() {
    use std::os::unix::fs::PermissionsExt;

    let tmp = TempDir::new().unwrap();
    let home = tmp.path().join("home");
    let data = tmp.path().join("data");
    let bin = tmp.path().join("agy-companion");
    fs::create_dir_all(&home).unwrap();

    let zlogin = home.join(".zlogin");
    let original = b"export FOO=1\r\nexport BAR=2";
    fs::write(&zlogin, original).unwrap();
    fs::set_permissions(&zlogin, fs::Permissions::from_mode(0o640)).unwrap();

    let opts = make_opts(ShellKind::Zsh, None, data, bin, false);
    enable_with_env(
        &opts,
        &LifecycleEnv {
            home: Some(&home),
            zdotdir: None,
            xdg_config_home: None,
            shell_env: Some("/bin/zsh"),
            default_data_override: None,
            hybrid_active: false,
            platform_supported: true,
        },
    )
    .unwrap();

    let after_enable = fs::read(&zlogin).unwrap();
    assert_eq!(
        fs::metadata(&zlogin).unwrap().permissions().mode() & 0o777,
        0o640
    );
    // Zero bare LF anywhere in the CRLF file
    for (idx, &b) in after_enable.iter().enumerate() {
        if b == b'\n' {
            assert!(
                idx > 0 && after_enable[idx - 1] == b'\r',
                "bare LF at byte offset {idx}"
            );
        }
    }

    disable_with_env(
        &opts,
        &LifecycleEnv {
            home: Some(&home),
            zdotdir: None,
            xdg_config_home: None,
            shell_env: Some("/bin/zsh"),
            default_data_override: None,
            hybrid_active: false,
            platform_supported: true,
        },
    )
    .unwrap();

    assert_eq!(
        fs::metadata(&zlogin).unwrap().permissions().mode() & 0o777,
        0o640
    );
    assert_eq!(fs::read(&zlogin).unwrap(), original);
}

#[cfg(unix)]
#[test]
fn strip_all_known_rc_files_continues_past_non_utf8_oversized_and_fifo_rc_files() {
    use std::os::unix::fs::symlink;

    let tmp = TempDir::new().unwrap();
    let home = tmp.path().join("home");
    let xdg = tmp.path().join("xdg");
    let data = tmp.path().join("data");
    let bin = tmp.path().join("agy-companion");
    fs::create_dir_all(&home).unwrap();

    // Enable managed Zsh .zlogin and Fish drop-in
    let zsh_opts = make_opts(ShellKind::Zsh, None, data.clone(), bin.clone(), false);
    enable_with_env(
        &zsh_opts,
        &LifecycleEnv {
            home: Some(&home),
            zdotdir: None,
            xdg_config_home: Some(&xdg),
            shell_env: Some("/bin/zsh"),
            default_data_override: None,
            hybrid_active: false,
            platform_supported: true,
        },
    )
    .unwrap();
    let fish_opts = make_opts(ShellKind::Fish, None, data, bin, false);
    enable_with_env(
        &fish_opts,
        &LifecycleEnv {
            home: Some(&home),
            zdotdir: None,
            xdg_config_home: Some(&xdg),
            shell_env: Some("/usr/bin/fish"),
            default_data_override: None,
            hybrid_active: false,
            platform_supported: true,
        },
    )
    .unwrap();

    let zlogin = home.join(".zlogin");
    let fish_dropin = xdg.join("fish/conf.d/agy-companion.fish");
    assert!(zlogin.is_file());
    assert!(fish_dropin.is_file());

    // Add hostile/unrelated RC files: non-UTF-8 .zshrc, >1 MiB .profile, symlink-to-FIFO .zprofile
    let zshrc = home.join(".zshrc");
    fs::write(&zshrc, b"\xff\xfeinvalid-utf8\n").unwrap();

    let profile = home.join(".profile");
    fs::write(&profile, vec![b'#'; (MAX_RC_FILE_BYTES as usize) + 64]).unwrap();

    let fifo = tmp.path().join("pipe");
    assert!(
        Command::new("mkfifo")
            .arg(&fifo)
            .status()
            .unwrap()
            .success()
    );
    let zprofile = home.join(".zprofile");
    symlink(&fifo, &zprofile).unwrap();

    let report = strip_all_known_rc_files(&home, None, Some(&xdg), false).unwrap();
    assert!(!zlogin.exists(), ".zlogin should have been deleted");
    assert!(
        !fish_dropin.exists(),
        "agy-companion.fish should have been deleted"
    );
    assert!(report.deleted_files.contains(&zlogin));
    assert!(report.deleted_files.contains(&fish_dropin));
    assert!(report.skipped_manual_files.contains(&zshrc));
    assert!(report.skipped_manual_files.contains(&profile));
}

#[test]
fn parse_markers_fails_closed_on_custom_labeled_or_malformed_marker_prefixes() {
    let path = Path::new("/tmp/test.zlogin");

    // Unclosed begin marker with custom label (e.g. legacy or edited marker text).
    let unclosed = vec![
        "# user header",
        "# >>> agy-companion (test) >>>",
        "export FOO=1",
    ];
    let err = parse_markers(path, &unclosed).expect_err("must fail closed on unclosed begin");
    let msg = err.to_string();
    assert!(msg.contains("line 2"), "expected line 2 in error: {msg}");

    // Orphan custom-labeled end marker.
    let orphan_end = vec!["export FOO=1", "# <<< agy-companion (legacy) <<<"];
    let err = parse_markers(path, &orphan_end).expect_err("must fail closed on orphan end");
    let msg = err.to_string();
    assert!(msg.contains("line 2"), "expected line 2 in error: {msg}");

    // Reversed custom-labeled markers.
    let reversed = vec![
        "# <<< agy-companion (legacy) <<<",
        "# >>> agy-companion (test) >>>",
    ];
    let err = parse_markers(path, &reversed).expect_err("must fail closed on reversed markers");
    let msg = err.to_string();
    assert!(
        msg.contains("line 1") && msg.contains("line 2"),
        "expected lines 1 and 2 in error: {msg}"
    );

    // Duplicate begin markers with mixed labels.
    let dup = vec![
        HOOK_BEGIN_MARKER,
        "# >>> agy-companion (test) >>>",
        HOOK_END_MARKER,
    ];
    let err = parse_markers(path, &dup).expect_err("must fail closed on duplicate begin");
    let msg = err.to_string();
    assert!(
        msg.contains("1") && msg.contains("2"),
        "expected line numbers 1 and 2 in error: {msg}"
    );
}

#[cfg(unix)]
#[test]
fn env_sh_and_fish_trim_whitespace_in_restore_vars_preserving_outer_backups() {
    use std::os::unix::fs::PermissionsExt;

    let tmp = TempDir::new().unwrap();
    let fake_bin = tmp.path().join("fake-agy-companion");
    fs::write(
        &fake_bin,
        "#!/bin/sh\nprintf '%s\\n' \"${FAKE_HOOK_PORT:-}\"\n",
    )
    .unwrap();
    fs::set_permissions(&fake_bin, fs::Permissions::from_mode(0o755)).unwrap();

    let env_sh = tmp.path().join("env.sh");
    let rendered = render_env_sh(&fake_bin).unwrap();
    fs::write(&env_sh, &rendered).unwrap();

    let restore_vars = "FOO, CLOUD_CODE_URL,\tGOOGLE_GEMINI_BASE_URL, GEMINI_API_KEY";

    for shell in ["/bin/sh", "/bin/bash", "/bin/zsh"] {
        if !Path::new(shell).exists() {
            continue;
        }

        // 1. Pre-existing set OUTER_CLOUD_CODE_URL must never be overwritten, duplicated, or unset.
        let set_case = Command::new(shell)
            .arg("-c")
            .arg(format!(
                "set -eu; . '{}'; \
                printf 'ACT_OUTER=%s|ACT_VARS=%s|ACT_ADDED=%s|ACT_CC=%s\\n' \
                \"${{OUTER_CLOUD_CODE_URL+SET:$OUTER_CLOUD_CODE_URL}}\" \
                \"${{JETSKI_MCP_ENV_RESTORE_VARS:-}}\" \
                \"${{AGY_COMPANION_ADDED_PREFIX_CLOUD_CODE:-}}\" \
                \"${{CLOUD_CODE_URL:-}}\"; \
                unset ANTIGRAVITY_RESOLVING_ENVIRONMENT; \
                . '{}'; \
                printf 'SCRUB_OUTER=%s|SCRUB_VARS=%s|SCRUB_CC=%s\\n' \
                \"${{OUTER_CLOUD_CODE_URL+SET:$OUTER_CLOUD_CODE_URL}}\" \
                \"${{JETSKI_MCP_ENV_RESTORE_VARS:-}}\" \
                \"${{CLOUD_CODE_URL:-}}\"",
                env_sh.display(),
                env_sh.display()
            ))
            .env_clear()
            .env("PATH", "/usr/bin:/bin")
            .env("ANTIGRAVITY_RESOLVING_ENVIRONMENT", "1")
            .env("FAKE_HOOK_PORT", "4321")
            .env("JETSKI_MCP_ENV_RESTORE_PREFIX", "OUTER_")
            .env("JETSKI_MCP_ENV_RESTORE_VARS", restore_vars)
            .env("OUTER_CLOUD_CODE_URL", "https://precious-orig.example")
            .env("CLOUD_CODE_URL", "https://outer-wrapper.example")
            .output()
            .unwrap();
        assert!(
            set_case.status.success(),
            "{shell} set_case failed: {}",
            String::from_utf8_lossy(&set_case.stderr)
        );
        let lines: Vec<String> = String::from_utf8(set_case.stdout)
            .unwrap()
            .lines()
            .map(str::to_string)
            .collect();
        assert_eq!(lines.len(), 2, "{shell} unexpected output: {lines:?}");
        assert_eq!(
            lines[0],
            format!(
                "ACT_OUTER=SET:https://precious-orig.example|ACT_VARS={restore_vars}|ACT_ADDED=|ACT_CC=http://127.0.0.1:4321"
            ),
            "{shell} activation clobbered or duplicated spaced JETSKI_MCP_ENV_RESTORE_VARS"
        );
        assert_eq!(
            lines[1],
            format!(
                "SCRUB_OUTER=SET:https://precious-orig.example|SCRUB_VARS={restore_vars}|SCRUB_CC=https://outer-wrapper.example"
            ),
            "{shell} scrub deleted or corrupted outer backup var"
        );

        // 2. 3-state originally-unset OUTER_CLOUD_CODE_URL must remain unset across activation & scrub.
        let unset_case = Command::new(shell)
            .arg("-c")
            .arg(format!(
                "set -eu; . '{}'; \
                printf 'ACT_OUTER=%s|ACT_VARS=%s|ACT_ADDED=%s\\n' \
                \"${{OUTER_CLOUD_CODE_URL+SET:$OUTER_CLOUD_CODE_URL}}\" \
                \"${{JETSKI_MCP_ENV_RESTORE_VARS:-}}\" \
                \"${{AGY_COMPANION_ADDED_PREFIX_CLOUD_CODE:-}}\"; \
                unset ANTIGRAVITY_RESOLVING_ENVIRONMENT; \
                . '{}'; \
                printf 'SCRUB_OUTER=%s|SCRUB_CC=%s\\n' \
                \"${{OUTER_CLOUD_CODE_URL+SET:$OUTER_CLOUD_CODE_URL}}\" \
                \"${{CLOUD_CODE_URL:-}}\"",
                env_sh.display(),
                env_sh.display()
            ))
            .env_clear()
            .env("PATH", "/usr/bin:/bin")
            .env("ANTIGRAVITY_RESOLVING_ENVIRONMENT", "1")
            .env("FAKE_HOOK_PORT", "4321")
            .env("JETSKI_MCP_ENV_RESTORE_PREFIX", "OUTER_")
            .env("JETSKI_MCP_ENV_RESTORE_VARS", restore_vars)
            .env("CLOUD_CODE_URL", "https://outer-wrapper.example")
            .output()
            .unwrap();
        assert!(
            unset_case.status.success(),
            "{shell} unset_case failed: {}",
            String::from_utf8_lossy(&unset_case.stderr)
        );
        let unset_lines: Vec<String> = String::from_utf8(unset_case.stdout)
            .unwrap()
            .lines()
            .map(str::to_string)
            .collect();
        assert_eq!(
            unset_lines.len(),
            2,
            "{shell} unexpected unset output: {unset_lines:?}"
        );
        assert_eq!(
            unset_lines[0],
            format!("ACT_OUTER=|ACT_VARS={restore_vars}|ACT_ADDED="),
            "{shell} activation overwrote 3-state originally-unset OUTER_CLOUD_CODE_URL"
        );
        assert_eq!(
            unset_lines[1], "SCRUB_OUTER=|SCRUB_CC=https://outer-wrapper.example",
            "{shell} scrub corrupted 3-state unset OUTER_CLOUD_CODE_URL"
        );
    }

    // 3. Fish drop-in normalizes spaces and tabs before checking membership.
    let fish = render_fish_dropin(&fake_bin).unwrap();
    assert!(
        fish.contains(
            r#"set -l _agy_orig_csv ","(string replace -ra '[ \t]+' '' -- "$_agy_vars")",""#
        ),
        "render_fish_dropin must strip spaces/tabs from _agy_vars before CSV matching:\n{fish}"
    );
}

#[cfg(unix)]
#[test]
fn integration_fs_safe_symlink_and_fifo_wiring_matrix() {
    use std::os::unix::fs::{PermissionsExt, symlink};
    use std::sync::mpsc;
    use std::time::Duration;

    fn make_env<'a>(
        home: &'a Path,
        xdg: Option<&'a Path>,
        shell_env: Option<&'a str>,
        default_data_override: Option<&'a Path>,
    ) -> LifecycleEnv<'a> {
        LifecycleEnv {
            home: Some(home),
            zdotdir: None,
            xdg_config_home: xdg,
            shell_env,
            default_data_override,
            hybrid_active: false,
            platform_supported: true,
        }
    }

    // Block A: Fish intermediate directory symlink, dangling symlink, and non-dir wiring.
    for case in ["dot_config_fish", "xdg_fish_confd"] {
        let tmp = TempDir::new().unwrap();
        let home = tmp.path().join("home");
        let xdg = tmp.path().join("xdg");
        let victim = tmp.path().join("victim");
        let data = tmp.path().join("data");
        let bin = tmp.path().join("agy-companion");
        fs::create_dir_all(&home).unwrap();
        let (xdg_opt, victim_dropin, expected_rc) = match case {
            "dot_config_fish" => {
                fs::create_dir_all(home.join(".config")).unwrap();
                fs::create_dir_all(victim.join("conf.d")).unwrap();
                symlink(&victim, home.join(".config/fish")).unwrap();
                (
                    None,
                    victim.join("conf.d").join(FISH_DROPIN_FILENAME),
                    home.join(".config/fish/conf.d").join(FISH_DROPIN_FILENAME),
                )
            }
            "xdg_fish_confd" => {
                fs::create_dir_all(xdg.join("fish")).unwrap();
                fs::create_dir_all(&victim).unwrap();
                symlink(&victim, xdg.join("fish/conf.d")).unwrap();
                (
                    Some(xdg.as_path()),
                    victim.join(FISH_DROPIN_FILENAME),
                    xdg.join("fish/conf.d").join(FISH_DROPIN_FILENAME),
                )
            }
            _ => unreachable!(),
        };
        let env = make_env(&home, xdg_opt, Some("/usr/local/bin/fish"), None);
        let opts_safe = make_opts(ShellKind::Fish, None, data.clone(), bin.clone(), false);

        // 1. Absent drop-in: status/enable report ManualRequired; strip emits no false positive.
        let st = status_with_env(&opts_safe, &env).unwrap();
        assert_eq!(st.state, IntegrationState::ManualRequired, "{case}");
        assert!(st.symlink_target.is_some() && st.manual_snippet.is_some());
        let en = enable_with_env(&opts_safe, &env).unwrap();
        assert_eq!(en.state, IntegrationState::ManualRequired, "{case}");
        assert!(!victim_dropin.exists(), "{case}: must not create drop-in");
        let rep = strip_all_known_rc_files(&home, None, xdg_opt, false).unwrap();
        assert!(
            rep.skipped_manual_files.is_empty(),
            "{case}: no false positive"
        );

        // 2. Populated drop-in: follow=false refuses mutation; strip skips without touching victim.
        let populated = format!("# DO_NOT_DELETE\n{HOOK_BEGIN_MARKER}\n# old\n{HOOK_END_MARKER}\n");
        fs::write(&victim_dropin, &populated).unwrap();
        assert_eq!(
            status_with_env(&opts_safe, &env).unwrap().state,
            IntegrationState::Enabled
        );
        assert_eq!(
            enable_with_env(&opts_safe, &env).unwrap().state,
            IntegrationState::ManualRequired
        );
        assert_eq!(
            disable_with_env(&opts_safe, &env).unwrap().state,
            IntegrationState::ManualRequired
        );
        for dry_run in [true, false] {
            let rep = strip_all_known_rc_files(&home, None, xdg_opt, dry_run).unwrap();
            assert!(rep.deleted_files.is_empty() && rep.modified_files.is_empty());
            assert_eq!(rep.skipped_manual_files, vec![expected_rc.clone()]);
            assert_eq!(fs::read_to_string(&victim_dropin).unwrap(), populated);
        }

        // 3. Opt-in follow_rc_symlink=true updates and strips while preserving user content.
        let opts_follow = make_opts(ShellKind::Fish, None, data, bin, true);
        assert_eq!(
            enable_with_env(&opts_follow, &env).unwrap().state,
            IntegrationState::Enabled
        );
        let updated = fs::read_to_string(&victim_dropin).unwrap();
        assert!(updated.contains("# DO_NOT_DELETE") && updated.contains(HOOK_BEGIN_MARKER));
        assert_eq!(
            disable_with_env(&opts_follow, &env).unwrap().state,
            IntegrationState::Disabled
        );
        let after_dis = fs::read_to_string(&victim_dropin).unwrap();
        assert!(after_dis.contains("# DO_NOT_DELETE") && !after_dis.contains(HOOK_BEGIN_MARKER));
    }

    // Block A (continued): Dangling symlink and non-directory blocker at ~/.config/fish.
    {
        let tmp = TempDir::new().unwrap();
        let home = tmp.path().join("home");
        fs::create_dir_all(home.join(".config")).unwrap();
        symlink(tmp.path().join("nonexistent"), home.join(".config/fish")).unwrap();
        let env = make_env(&home, None, Some("/usr/local/bin/fish"), None);
        let opts_safe = make_opts(
            ShellKind::Fish,
            None,
            tmp.path().join("d"),
            tmp.path().join("b"),
            false,
        );
        let opts_follow = make_opts(
            ShellKind::Fish,
            None,
            tmp.path().join("d"),
            tmp.path().join("b"),
            true,
        );
        assert_eq!(
            status_with_env(&opts_safe, &env).unwrap().state,
            IntegrationState::ManualRequired
        );
        assert_eq!(
            enable_with_env(&opts_safe, &env).unwrap().state,
            IntegrationState::ManualRequired
        );
        assert_eq!(
            disable_with_env(&opts_safe, &env).unwrap().state,
            IntegrationState::Disabled
        );
        assert!(
            strip_all_known_rc_files(&home, None, None, false)
                .unwrap()
                .skipped_manual_files
                .is_empty()
        );
        assert!(matches!(
            enable_with_env(&opts_follow, &env),
            Err(CoreError::InvalidPath { .. })
        ));

        let tmp2 = TempDir::new().unwrap();
        let home2 = tmp2.path().join("home");
        fs::create_dir_all(home2.join(".config")).unwrap();
        let blocker = home2.join(".config/fish");
        fs::write(&blocker, "DO_NOT_DELETE").unwrap();
        let env2 = make_env(&home2, None, Some("/usr/local/bin/fish"), None);
        let opts2 = make_opts(
            ShellKind::Fish,
            None,
            tmp2.path().join("d"),
            tmp2.path().join("b"),
            false,
        );
        assert!(matches!(
            status_with_env(&opts2, &env2),
            Err(CoreError::Fs(FsError::NotDirectory { .. }))
        ));
        assert!(matches!(
            enable_with_env(&opts2, &env2),
            Err(CoreError::Fs(FsError::NotDirectory { .. }))
        ));
        assert!(matches!(
            disable_with_env(&opts2, &env2),
            Err(CoreError::Fs(FsError::NotDirectory { .. }))
        ));
        assert!(
            strip_all_known_rc_files(&home2, None, None, false)
                .unwrap()
                .modified_files
                .is_empty()
        );
        assert_eq!(fs::read_to_string(&blocker).unwrap(), "DO_NOT_DELETE");
    }

    // Block B: Zsh & Bash env_data_dir chain symlink and non-dir wiring.
    for shell in [ShellKind::Zsh, ShellKind::Bash] {
        let (shell_env, rc_name) = match shell {
            ShellKind::Zsh => (Some("/bin/zsh"), ".zlogin"),
            ShellKind::Bash => (Some("/bin/bash"), ".profile"),
            ShellKind::Fish => unreachable!(),
        };
        for kind in [
            "default_dot_local_symlink",
            "custom_data_dir_symlink",
            "non_dir_file_data_dir",
        ] {
            let tmp = TempDir::new().unwrap();
            let home = tmp.path().join("home");
            let victim_dir = tmp.path().join("victim_dir");
            fs::create_dir_all(&home).unwrap();
            fs::create_dir_all(&victim_dir).unwrap();
            fs::set_permissions(&victim_dir, fs::Permissions::from_mode(0o755)).unwrap();

            let default_data = home.join(".local/share/agy-companion");
            let (opts_data_dir, default_override, victim_env, expect_symlink) = match kind {
                "default_dot_local_symlink" => {
                    symlink(&victim_dir, home.join(".local")).unwrap();
                    let v_env = victim_dir
                        .join("share/agy-companion")
                        .join(ENV_SCRIPT_FILENAME);
                    fs::create_dir_all(v_env.parent().unwrap()).unwrap();
                    (None, Some(default_data.as_path()), Some(v_env), true)
                }
                "custom_data_dir_symlink" => {
                    let link = tmp.path().join("custom_data_link");
                    symlink(&victim_dir, &link).unwrap();
                    (
                        Some(link),
                        None,
                        Some(victim_dir.join(ENV_SCRIPT_FILENAME)),
                        true,
                    )
                }
                "non_dir_file_data_dir" => {
                    let blocker = tmp.path().join("not_a_dir");
                    fs::write(&blocker, "DO_NOT_DELETE").unwrap();
                    (Some(blocker), None, None, false)
                }
                _ => unreachable!(),
            };
            let opts = IntegrationOptions {
                shell: Some(shell),
                rc_file: None,
                binary_path: Some(tmp.path().join("agy-companion")),
                data_dir: opts_data_dir,
                follow_rc_symlink: false,
            };
            let env = make_env(&home, None, shell_env, default_override);
            let check_err = |res: Result<IntegrationStatus, CoreError>, op: &str| {
                if expect_symlink {
                    assert!(
                        matches!(res, Err(CoreError::Fs(FsError::SymlinkRejected { .. }))),
                        "{shell}/{kind}/{op}: {res:?}"
                    );
                } else {
                    assert!(
                        matches!(res, Err(CoreError::Fs(FsError::NotDirectory { .. }))),
                        "{shell}/{kind}/{op}: {res:?}"
                    );
                }
            };

            // Fresh state: enable fails closed without creating env.sh or chmod'ing victim_dir.
            check_err(enable_with_env(&opts, &env), "enable-fresh");
            if let Some(ref v_env) = victim_env {
                assert!(!v_env.exists(), "{shell}/{kind}: must not create env.sh");
                assert_eq!(
                    fs::metadata(&victim_dir).unwrap().permissions().mode() & 0o777,
                    0o755
                );
            }

            // Populated state: status/enable/disable all fail closed preserving RC and victim.
            if let Some(ref v_env) = victim_env {
                fs::write(v_env, "# DO_NOT_DELETE\n").unwrap();
            }
            let rc_path = home.join(rc_name);
            let rc_before = format!("# user\n{HOOK_BEGIN_MARKER}\n# managed\n{HOOK_END_MARKER}\n");
            fs::write(&rc_path, &rc_before).unwrap();
            check_err(status_with_env(&opts, &env), "status");
            check_err(enable_with_env(&opts, &env), "enable");
            check_err(disable_with_env(&opts, &env), "disable");
            assert_eq!(fs::read_to_string(&rc_path).unwrap(), rc_before);
            if let Some(ref v_env) = victim_env {
                assert_eq!(fs::read_to_string(v_env).unwrap(), "# DO_NOT_DELETE\n");
                assert_eq!(
                    fs::metadata(&victim_dir).unwrap().permissions().mode() & 0o777,
                    0o755
                );
            }
        }
    }

    // Block C: Leaf env.sh symlink, directory, and FIFO fail closed before RC mutation.
    for shell in [ShellKind::Zsh, ShellKind::Bash] {
        let (shell_env, rc_name) = match shell {
            ShellKind::Zsh => (Some("/bin/zsh"), ".zlogin"),
            ShellKind::Bash => (Some("/bin/bash"), ".profile"),
            ShellKind::Fish => unreachable!(),
        };
        for kind in ["symlink", "directory", "fifo"] {
            let tmp = TempDir::new().unwrap();
            let home = tmp.path().join("home");
            let data_dir = tmp.path().join("data");
            fs::create_dir_all(&home).unwrap();
            fs::create_dir_all(&data_dir).unwrap();
            let env_path = data_dir.join(ENV_SCRIPT_FILENAME);
            let victim_env = tmp.path().join("victim_env.sh");
            match kind {
                "symlink" => {
                    fs::write(&victim_env, "# DO_NOT_DELETE\n").unwrap();
                    symlink(&victim_env, &env_path).unwrap();
                }
                "directory" => fs::create_dir_all(&env_path).unwrap(),
                "fifo" => assert!(
                    Command::new("mkfifo")
                        .arg(&env_path)
                        .status()
                        .unwrap()
                        .success()
                ),
                _ => unreachable!(),
            }
            let rc_path = home.join(rc_name);
            let rc_before = format!("{}\n", render_rc_snippet(shell, &env_path).unwrap());
            fs::write(&rc_path, &rc_before).unwrap();
            let opts = make_opts(
                shell,
                None,
                data_dir,
                tmp.path().join("agy-companion"),
                false,
            );

            let (tx, rx) = mpsc::channel();
            let home_c = home.clone();
            std::thread::spawn(move || {
                let env = LifecycleEnv {
                    home: Some(&home_c),
                    zdotdir: None,
                    xdg_config_home: None,
                    shell_env,
                    default_data_override: None,
                    hybrid_active: false,
                    platform_supported: true,
                };
                let _ = tx.send((
                    status_with_env(&opts, &env),
                    enable_with_env(&opts, &env),
                    disable_with_env(&opts, &env),
                ));
            });
            let (st, en, dis) = rx
                .recv_timeout(Duration::from_secs(2))
                .unwrap_or_else(|_| panic!("{shell}/{kind}: hung on leaf env.sh"));
            for (op, res) in [("status", st), ("enable", en), ("disable", dis)] {
                assert!(
                    matches!(
                        res,
                        Err(CoreError::Fs(
                            FsError::SymlinkRejected { .. } | FsError::NotRegularFile { .. }
                        ))
                    ),
                    "{shell}/{kind}/{op}: expected SymlinkRejected|NotRegularFile, got {res:?}"
                );
            }
            assert_eq!(fs::read_to_string(&rc_path).unwrap(), rc_before);
            if kind == "symlink" {
                assert_eq!(
                    fs::read_to_string(&victim_env).unwrap(),
                    "# DO_NOT_DELETE\n"
                );
            }
        }
    }

    // Block D: Fish does not use env.sh/data_dir and remains unaffected by symlinked ~/.local or leaf env.sh.
    for kind in ["symlink", "fifo"] {
        let tmp = TempDir::new().unwrap();
        let home = tmp.path().join("home");
        let data_dir = tmp.path().join("data");
        let victim_local = tmp.path().join("victim_local");
        let victim_env = tmp.path().join("victim_env.sh");
        fs::create_dir_all(&home).unwrap();
        fs::create_dir_all(&data_dir).unwrap();
        fs::create_dir_all(&victim_local).unwrap();
        symlink(&victim_local, home.join(".local")).unwrap();
        let env_path = data_dir.join(ENV_SCRIPT_FILENAME);
        if kind == "symlink" {
            fs::write(&victim_env, "# DO_NOT_DELETE\n").unwrap();
            symlink(&victim_env, &env_path).unwrap();
        } else {
            assert!(
                Command::new("mkfifo")
                    .arg(&env_path)
                    .status()
                    .unwrap()
                    .success()
            );
        }
        let opts = make_opts(
            ShellKind::Fish,
            None,
            data_dir,
            tmp.path().join("agy-companion"),
            false,
        );
        let env = make_env(&home, None, Some("/usr/local/bin/fish"), None);
        assert!(enable_with_env(&opts, &env).unwrap().enabled);
        assert!(status_with_env(&opts, &env).unwrap().enabled);
        assert!(!disable_with_env(&opts, &env).unwrap().enabled);
        if kind == "symlink" {
            assert_eq!(
                fs::read_to_string(&victim_env).unwrap(),
                "# DO_NOT_DELETE\n"
            );
        }
    }
}

#[test]
fn integration_lock_serializes_threads_and_rejects_unsafe_paths() {
    use std::sync::{Arc, Barrier};

    let tmp = TempDir::new().unwrap();
    let data_dir = tmp.path().join("data");

    // Acquiring creates 0700 data_dir and 0600 integration.lock; try_acquire sees contention.
    let guard = IntegrationLock::acquire(&data_dir).unwrap();
    let lock_path = guard.path().to_path_buf();
    assert_eq!(lock_path, data_dir.join(INTEGRATION_LOCK_FILENAME));
    assert!(lock_path.is_file());
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(
            fs::metadata(&lock_path).unwrap().permissions().mode() & 0o777,
            0o600
        );
        assert_eq!(
            fs::metadata(&data_dir).unwrap().permissions().mode() & 0o777,
            0o700
        );
    }
    assert!(
        IntegrationLock::try_acquire(&data_dir).unwrap().is_none(),
        "second try_acquire must return Ok(None) while lock is held"
    );
    drop(guard);
    assert!(
        lock_path.is_file(),
        "integration.lock must never be unlinked on Drop"
    );
    let reacquired = IntegrationLock::try_acquire(&data_dir).unwrap();
    assert!(reacquired.is_some());
    drop(reacquired);

    // Concurrent threads serialize cleanly through IntegrationLock::acquire.
    let counter_path = tmp.path().join("counter.txt");
    fs::write(&counter_path, "0").unwrap();
    let barrier = Arc::new(Barrier::new(4));
    let mut handles = Vec::new();
    for _ in 0..4 {
        let dir = data_dir.clone();
        let cp = counter_path.clone();
        let b = Arc::clone(&barrier);
        handles.push(std::thread::spawn(move || {
            b.wait();
            let _lock = IntegrationLock::acquire(&dir).unwrap();
            let cur: u32 = fs::read_to_string(&cp).unwrap().trim().parse().unwrap();
            std::thread::sleep(std::time::Duration::from_millis(10));
            fs::write(&cp, format!("{}", cur + 1)).unwrap();
        }));
    }
    for h in handles {
        h.join().unwrap();
    }
    assert_eq!(fs::read_to_string(&counter_path).unwrap().trim(), "4");

    // Path traversal & NUL rejected fail-closed.
    assert!(matches!(
        IntegrationLock::acquire(Path::new("/tmp/../escape")),
        Err(CoreError::Fs(FsError::ParentDirTraversal))
    ));
    assert!(matches!(
        IntegrationLock::acquire(Path::new("/tmp/bad\0dir")),
        Err(CoreError::Fs(FsError::ContainsNul))
    ));

    #[cfg(unix)]
    {
        use std::os::unix::fs::symlink;

        // Symlinked integration.lock rejected.
        let sym_dir = tmp.path().join("sym_lock_dir");
        fs::create_dir_all(&sym_dir).unwrap();
        let victim = tmp.path().join("victim_lock");
        fs::write(&victim, "keep").unwrap();
        symlink(&victim, sym_dir.join(INTEGRATION_LOCK_FILENAME)).unwrap();
        assert!(matches!(
            IntegrationLock::acquire(&sym_dir),
            Err(CoreError::Fs(FsError::SymlinkRejected { .. }))
        ));

        // Directory integration.lock rejected.
        let dir_lock_parent = tmp.path().join("dir_lock_parent");
        fs::create_dir_all(dir_lock_parent.join(INTEGRATION_LOCK_FILENAME)).unwrap();
        assert!(matches!(
            IntegrationLock::acquire(&dir_lock_parent),
            Err(CoreError::Fs(FsError::NotRegularFile { .. }))
        ));

        // FIFO integration.lock rejected without hanging.
        let fifo_dir = tmp.path().join("fifo_lock_dir");
        fs::create_dir_all(&fifo_dir).unwrap();
        let fifo_lock = fifo_dir.join(INTEGRATION_LOCK_FILENAME);
        assert!(
            Command::new("mkfifo")
                .arg(&fifo_lock)
                .status()
                .unwrap()
                .success()
        );
        assert!(matches!(
            IntegrationLock::acquire(&fifo_dir),
            Err(CoreError::Fs(FsError::NotRegularFile { .. }))
        ));

        // Symlinked data_dir rejected.
        let victim_dir = tmp.path().join("victim_data_dir");
        fs::create_dir_all(&victim_dir).unwrap();
        let sym_data = tmp.path().join("sym_data_dir");
        symlink(&victim_dir, &sym_data).unwrap();
        assert!(matches!(
            IntegrationLock::acquire(&sym_data),
            Err(CoreError::Fs(FsError::SymlinkRejected { .. }))
        ));
        assert!(!victim_dir.join(INTEGRATION_LOCK_FILENAME).exists());

        // Symlinked $HOME/.local rejected before touching victim.
        let fake_home = tmp.path().join("fake_home");
        let victim_local = tmp.path().join("victim_local");
        let victim_default_data = victim_local.join("share/agy-companion");
        fs::create_dir_all(&fake_home).unwrap();
        fs::create_dir_all(&victim_default_data).unwrap();
        symlink(&victim_local, fake_home.join(".local")).unwrap();
        let default_data = fake_home.join(".local/share/agy-companion");
        assert!(matches!(
            IntegrationLock::acquire_with_home(&default_data, Some(&fake_home)),
            Err(CoreError::Fs(FsError::SymlinkRejected { .. }))
        ));
        assert!(!victim_default_data.join(INTEGRATION_LOCK_FILENAME).exists());
    }
}

#[cfg(unix)]
#[test]
fn integration_lock_drop_unlocks_with_duplicated_fd_and_concurrent_child_spawns() {
    use std::sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    };

    let tmp = TempDir::new().unwrap();
    let data_dir = tmp.path().join("data");

    // 1. Deterministic open-file-description proof: holding a dup'd fd (identical to a
    //    child process inheriting the fd across fork/exec before O_CLOEXEC) MUST NOT
    //    block reacquisition once IntegrationLock::drop calls File::unlock().
    let guard = IntegrationLock::acquire(&data_dir).unwrap();
    let lock_path = guard.path().to_path_buf();
    let dup_fd = guard.try_clone_file().unwrap();
    drop(guard);
    assert!(
        lock_path.is_file(),
        "integration.lock must never be unlinked on Drop"
    );
    let reacquired = IntegrationLock::try_acquire(&data_dir).unwrap();
    assert!(
        reacquired.is_some(),
        "Drop must explicitly unlock the open file description even while a dup fd is open"
    );
    drop(reacquired);
    drop(dup_fd);

    // 2. Real-world concurrent fork/exec stress: repeatedly acquire/drop/reacquire while
    //    a sibling thread continuously spawns short-lived child processes.
    let stop = Arc::new(AtomicBool::new(false));
    let stop_worker = Arc::clone(&stop);
    let spawner = std::thread::spawn(move || {
        while !stop_worker.load(Ordering::Relaxed) {
            let status = Command::new("sh")
                .args(["-c", ":"])
                .status()
                .expect("spawn sh");
            assert!(status.success());
        }
    });

    for iter in 0..40 {
        let guard = IntegrationLock::acquire(&data_dir).unwrap();
        drop(guard);
        let reacquired = IntegrationLock::try_acquire(&data_dir).unwrap();
        assert!(
            reacquired.is_some(),
            "iteration {iter}: reacquire must succeed immediately after Drop during concurrent spawns"
        );
        drop(reacquired);
    }

    stop.store(true, Ordering::Relaxed);
    spawner.join().unwrap();
}

#[test]
fn ensure_first_run_launcher_enabled_state_machine_respects_opt_out_symlink_and_windows() {
    use super::auto_enable::ensure_first_run_launcher_enabled_with_env;
    use crate::config::Config;

    let tmp = TempDir::new().unwrap();
    let bin = tmp.path().join("agy-companion");
    fs::write(&bin, "#!/bin/sh\n").unwrap();

    // 1. Fresh None on Unix -> enables RC + env.sh, persists Some(true); second call is no-op.
    {
        let home = tmp.path().join("case1_home");
        let data = tmp.path().join("case1_data");
        let cfg_path = tmp.path().join("case1_cfg/config.json");
        fs::create_dir_all(&home).unwrap();
        let opts = make_opts(ShellKind::Zsh, None, data.clone(), bin.clone(), false);
        let env = LifecycleEnv {
            home: Some(&home),
            zdotdir: Some(&home),
            xdg_config_home: None,
            shell_env: Some("/bin/zsh"),
            default_data_override: Some(&data),
            hybrid_active: false,
            platform_supported: true,
        };
        let first = ensure_first_run_launcher_enabled_with_env(&cfg_path, &opts, &env).unwrap();
        assert!(matches!(
            first,
            AutoEnableOutcome::Attempted { ref status } if status.enabled && status.state == IntegrationState::Enabled
        ));
        assert!(home.join(".zlogin").is_file());
        assert!(data.join(ENV_SCRIPT_FILENAME).is_file());
        assert_eq!(
            Config::load_or_default(&cfg_path)
                .unwrap()
                .launcher_auto_start,
            Some(true)
        );

        let second = ensure_first_run_launcher_enabled_with_env(&cfg_path, &opts, &env).unwrap();
        assert_eq!(
            second,
            AutoEnableOutcome::AlreadyInitialized {
                launcher_auto_start: true
            }
        );
    }

    // 2. Explicit opt-out Some(false) is NEVER overridden.
    {
        let home = tmp.path().join("case2_home");
        let data = tmp.path().join("case2_data");
        let cfg_path = tmp.path().join("case2_cfg/config.json");
        fs::create_dir_all(&home).unwrap();
        let cfg = Config {
            launcher_auto_start: Some(false),
            ..Default::default()
        };
        cfg.save_secure(&cfg_path).unwrap();

        let opts = make_opts(ShellKind::Zsh, None, data.clone(), bin.clone(), false);
        let env = LifecycleEnv {
            home: Some(&home),
            zdotdir: Some(&home),
            xdg_config_home: None,
            shell_env: Some("/bin/zsh"),
            default_data_override: Some(&data),
            hybrid_active: false,
            platform_supported: true,
        };
        let outcome = ensure_first_run_launcher_enabled_with_env(&cfg_path, &opts, &env).unwrap();
        assert_eq!(
            outcome,
            AutoEnableOutcome::AlreadyInitialized {
                launcher_auto_start: false
            }
        );
        assert!(!home.join(".zlogin").exists());
        assert!(!data.join(ENV_SCRIPT_FILENAME).exists());
    }

    // 3. hybrid_active = true on first run -> AlreadyEnabled, persists Some(true) without writing RC.
    {
        let home = tmp.path().join("case3_home");
        let data = tmp.path().join("case3_data");
        let cfg_path = tmp.path().join("case3_cfg/config.json");
        fs::create_dir_all(&home).unwrap();
        let opts = make_opts(ShellKind::Zsh, None, data.clone(), bin.clone(), false);
        let env = LifecycleEnv {
            home: Some(&home),
            zdotdir: Some(&home),
            xdg_config_home: None,
            shell_env: Some("/bin/zsh"),
            default_data_override: Some(&data),
            hybrid_active: true,
            platform_supported: true,
        };
        let outcome = ensure_first_run_launcher_enabled_with_env(&cfg_path, &opts, &env).unwrap();
        assert!(matches!(outcome, AutoEnableOutcome::AlreadyEnabled { .. }));
        assert!(!home.join(".zlogin").exists());
        assert_eq!(
            Config::load_or_default(&cfg_path)
                .unwrap()
                .launcher_auto_start,
            Some(true)
        );
    }

    // 4. Simulated Windows (platform_supported = false) -> clean no-op, zero files touched.
    {
        let home = tmp.path().join("case4_home");
        let data = tmp.path().join("case4_data");
        let cfg_path = tmp.path().join("case4_cfg/config.json");
        fs::create_dir_all(&home).unwrap();
        let opts = make_opts(ShellKind::Zsh, None, data.clone(), bin.clone(), false);
        let env = LifecycleEnv {
            home: Some(&home),
            zdotdir: Some(&home),
            xdg_config_home: None,
            shell_env: Some("/bin/zsh"),
            default_data_override: Some(&data),
            hybrid_active: false,
            platform_supported: false,
        };
        let outcome = ensure_first_run_launcher_enabled_with_env(&cfg_path, &opts, &env).unwrap();
        assert_eq!(outcome, AutoEnableOutcome::UnsupportedPlatform);
        assert!(!home.join(".zlogin").exists());
        assert!(!cfg_path.exists());
        assert!(!data.join(INTEGRATION_LOCK_FILENAME).exists());
    }

    // 5. Symlinked RC file -> forces follow_rc_symlink=false even if caller passed true,
    //    returns ManualRequired, leaves symlink target untouched, persists Some(true) (no retry loop).
    #[cfg(unix)]
    {
        use std::os::unix::fs::symlink;

        let home = tmp.path().join("case5_home");
        let data = tmp.path().join("case5_data");
        let cfg_path = tmp.path().join("case5_cfg/config.json");
        fs::create_dir_all(&home).unwrap();
        let target_rc = tmp.path().join("case5_dotfile_zlogin");
        fs::write(&target_rc, "# managed by nix\n").unwrap();
        symlink(&target_rc, home.join(".zlogin")).unwrap();

        // Caller deliberately passes follow_rc_symlink = true; first-run auto-enable must override to false.
        let opts = make_opts(ShellKind::Zsh, None, data.clone(), bin.clone(), true);
        let env = LifecycleEnv {
            home: Some(&home),
            zdotdir: Some(&home),
            xdg_config_home: None,
            shell_env: Some("/bin/zsh"),
            default_data_override: Some(&data),
            hybrid_active: false,
            platform_supported: true,
        };
        let first = ensure_first_run_launcher_enabled_with_env(&cfg_path, &opts, &env).unwrap();
        assert!(matches!(
            first,
            AutoEnableOutcome::Attempted { ref status } if status.state == IntegrationState::ManualRequired
        ));
        assert_eq!(
            fs::read_to_string(&target_rc).unwrap(),
            "# managed by nix\n",
            "symlinked RC target must remain untouched"
        );
        assert_eq!(
            Config::load_or_default(&cfg_path)
                .unwrap()
                .launcher_auto_start,
            Some(true)
        );
        let second = ensure_first_run_launcher_enabled_with_env(&cfg_path, &opts, &env).unwrap();
        assert_eq!(
            second,
            AutoEnableOutcome::AlreadyInitialized {
                launcher_auto_start: true
            }
        );

        // 6. Corrupt / symlinked config_path fails closed before touching RC.
        let home6 = tmp.path().join("case6_home");
        let data6 = tmp.path().join("case6_data");
        let bad_cfg_target = tmp.path().join("case6_target.json");
        let bad_cfg_sym = tmp.path().join("case6_sym.json");
        fs::create_dir_all(&home6).unwrap();
        fs::write(&bad_cfg_target, "{}").unwrap();
        symlink(&bad_cfg_target, &bad_cfg_sym).unwrap();
        let opts6 = make_opts(ShellKind::Zsh, None, data6.clone(), bin, false);
        let env6 = LifecycleEnv {
            home: Some(&home6),
            zdotdir: Some(&home6),
            xdg_config_home: None,
            shell_env: Some("/bin/zsh"),
            default_data_override: Some(&data6),
            hybrid_active: false,
            platform_supported: true,
        };
        assert!(ensure_first_run_launcher_enabled_with_env(&bad_cfg_sym, &opts6, &env6).is_err());
        assert!(!home6.join(".zlogin").exists());
    }
}

#[test]
fn ensure_first_run_launcher_enabled_concurrent_race_executes_once_cleanly() {
    use std::sync::{Arc, Barrier};

    use super::auto_enable::ensure_first_run_launcher_enabled_with_env;
    use crate::config::Config;

    let tmp = TempDir::new().unwrap();
    let home = tmp.path().join("race_home");
    let data = tmp.path().join("race_data");
    let cfg_path = tmp.path().join("race_cfg/config.json");
    let bin = tmp.path().join("agy-companion");
    fs::create_dir_all(&home).unwrap();
    fs::write(&bin, "#!/bin/sh\n").unwrap();

    let barrier = Arc::new(Barrier::new(6));
    let mut handles = Vec::new();
    for _ in 0..6 {
        let home = home.clone();
        let data = data.clone();
        let cfg_path = cfg_path.clone();
        let bin = bin.clone();
        let b = Arc::clone(&barrier);
        handles.push(std::thread::spawn(move || {
            let opts = make_opts(ShellKind::Zsh, None, data.clone(), bin, false);
            let env = LifecycleEnv {
                home: Some(&home),
                zdotdir: Some(&home),
                xdg_config_home: None,
                shell_env: Some("/bin/zsh"),
                default_data_override: Some(&data),
                hybrid_active: false,
                platform_supported: true,
            };
            b.wait();
            ensure_first_run_launcher_enabled_with_env(&cfg_path, &opts, &env).unwrap()
        }));
    }

    let mut attempted = 0usize;
    let mut already_init = 0usize;
    for h in handles {
        match h.join().unwrap() {
            AutoEnableOutcome::Attempted { status } => {
                assert!(status.enabled);
                attempted += 1;
            }
            AutoEnableOutcome::AlreadyInitialized {
                launcher_auto_start: true,
            } => {
                already_init += 1;
            }
            other => panic!("unexpected concurrent outcome: {other:?}"),
        }
    }
    assert_eq!(attempted, 1, "exactly one thread must execute auto-enable");
    assert_eq!(already_init, 5);
    assert_eq!(
        Config::load_or_default(&cfg_path)
            .unwrap()
            .launcher_auto_start,
        Some(true)
    );
    let rc_content = fs::read_to_string(home.join(".zlogin")).unwrap();
    assert_eq!(rc_content.matches(HOOK_BEGIN_MARKER).count(), 1);
    assert_eq!(rc_content.matches(HOOK_END_MARKER).count(), 1);
}

#[test]
fn enable_and_disable_and_persist_validate_config_before_mutating_rc() {
    use super::auto_enable::{disable_and_persist_with_env, enable_and_persist_with_env};
    use crate::config::Config;

    let tmp = TempDir::new().unwrap();
    let home = tmp.path().join("persist_home");
    let data = tmp.path().join("persist_data");
    let cfg_path = tmp.path().join("persist_cfg/config.json");
    let bin = tmp.path().join("agy-companion");
    fs::create_dir_all(&home).unwrap();
    fs::write(&bin, "#!/bin/sh\n").unwrap();

    let opts = make_opts(ShellKind::Zsh, None, data.clone(), bin, false);
    let env = LifecycleEnv {
        home: Some(&home),
        zdotdir: Some(&home),
        xdg_config_home: None,
        shell_env: Some("/bin/zsh"),
        default_data_override: Some(&data),
        hybrid_active: false,
        platform_supported: true,
    };

    // Corrupt config must fail closed BEFORE mutating RC.
    fs::create_dir_all(cfg_path.parent().unwrap()).unwrap();
    fs::write(&cfg_path, "{ not valid json").unwrap();
    assert!(enable_and_persist_with_env(&cfg_path, &opts, &env).is_err());
    assert!(!home.join(".zlogin").exists());

    // Valid enable persists Some(true); disable persists Some(false).
    fs::remove_file(&cfg_path).unwrap();
    let en = enable_and_persist_with_env(&cfg_path, &opts, &env).unwrap();
    assert!(en.enabled);
    assert_eq!(
        Config::load_or_default(&cfg_path)
            .unwrap()
            .launcher_auto_start,
        Some(true)
    );
    let dis = disable_and_persist_with_env(&cfg_path, &opts, &env).unwrap();
    assert!(!dis.enabled);
    assert_eq!(
        Config::load_or_default(&cfg_path)
            .unwrap()
            .launcher_auto_start,
        Some(false)
    );
}
