use std::fmt;
use std::fs::{self, File};
use std::io::{ErrorKind, Read};
use std::net::Ipv4Addr;
use std::path::{Path, PathBuf};
use std::str::FromStr;

use serde::{Deserialize, Serialize};
use url::{Host, Url};

use crate::config::{
    CoreError, PROXY_PLACEHOLDER_API_KEY, TargetPlatform, XDG_CONFIG_HOME_ENV_VAR,
    default_data_dir, read_optional_env, resolve_xdg_base, validate_path,
    write_file_atomic_with_mode,
};

pub const HOOK_BEGIN_MARKER: &str =
    "# >>> agy-companion (Antigravity Dock/Launcher integration) >>>";
pub const HOOK_END_MARKER: &str = "# <<< agy-companion <<<";
pub const MAX_RC_FILE_BYTES: u64 = 1024 * 1024;
pub const ENV_SCRIPT_FILENAME: &str = "env.sh";
pub const FISH_DROPIN_FILENAME: &str = "agy-companion.fish";
pub const DEFAULT_RESTORE_PREFIX: &str = "AGY_COMPANION_ORIG_";
pub const PORTABLE_ENV_SCRIPT_EXPR: &str =
    "${XDG_DATA_HOME:-$HOME/.local/share}/agy-companion/env.sh";

const ZDOTDIR_ENV_VAR: &str = "ZDOTDIR";
const SHELL_ENV_VAR: &str = "SHELL";
const DEFAULT_RC_FILE_MODE: u32 = 0o644;
const DEFAULT_RC_DIR_MODE: u32 = 0o755;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ShellKind {
    Zsh,
    Bash,
    Fish,
}

impl fmt::Display for ShellKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Zsh => f.write_str("zsh"),
            Self::Bash => f.write_str("bash"),
            Self::Fish => f.write_str("fish"),
        }
    }
}

impl FromStr for ShellKind {
    type Err = CoreError;

    fn from_str(raw: &str) -> Result<Self, Self::Err> {
        match raw.trim().to_ascii_lowercase().as_str() {
            "zsh" => Ok(Self::Zsh),
            "bash" => Ok(Self::Bash),
            "fish" => Ok(Self::Fish),
            _ => Err(CoreError::InvalidShell {
                shell: raw.to_string(),
            }),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum IntegrationState {
    Active,
    Enabled,
    ManualRequired,
    Disabled,
    Unsupported,
}

#[derive(Debug, Default, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct IntegrationOptions {
    pub shell: Option<ShellKind>,
    pub rc_file: Option<PathBuf>,
    pub binary_path: Option<PathBuf>,
    pub data_dir: Option<PathBuf>,
    pub follow_rc_symlink: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct IntegrationStatus {
    pub supported: bool,
    pub enabled: bool,
    pub manual_required: bool,
    pub shell: Option<ShellKind>,
    pub rc_path: Option<PathBuf>,
    pub env_script_path: Option<PathBuf>,
    pub binary_path: Option<PathBuf>,
    pub symlink_target: Option<PathBuf>,
    pub manual_snippet: Option<String>,
    pub hybrid_active: bool,
    pub message: Option<String>,
}

impl IntegrationStatus {
    pub fn state(&self) -> IntegrationState {
        if !self.supported {
            IntegrationState::Unsupported
        } else if self.manual_required {
            IntegrationState::ManualRequired
        } else if self.enabled && self.hybrid_active {
            IntegrationState::Active
        } else if self.enabled {
            IntegrationState::Enabled
        } else {
            IntegrationState::Disabled
        }
    }
}

#[derive(Debug, Default, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StripReport {
    pub modified_files: Vec<PathBuf>,
    pub deleted_files: Vec<PathBuf>,
    pub skipped_manual_files: Vec<PathBuf>,
}

/// Validates that `prefix` is a safe POSIX shell identifier prefix (`^[A-Za-z_][A-Za-z0-9_]*$`).
pub fn is_valid_restore_prefix(prefix: &str) -> bool {
    let mut bytes = prefix.bytes();
    let Some(first) = bytes.next() else {
        return false;
    };
    if !(first.is_ascii_alphabetic() || first == b'_') {
        return false;
    }
    bytes.all(|b| b.is_ascii_alphanumeric() || b == b'_')
}

pub fn detect_shell(shell_env: Option<&str>) -> ShellKind {
    if let Some(raw) = shell_env {
        let base = raw
            .trim()
            .rsplit(['/', '\\'])
            .find(|seg| !seg.is_empty())
            .unwrap_or("");
        match base.to_ascii_lowercase().as_str() {
            "zsh" => return ShellKind::Zsh,
            "bash" => return ShellKind::Bash,
            "fish" => return ShellKind::Fish,
            _ => {}
        }
    }

    #[cfg(target_os = "macos")]
    {
        ShellKind::Zsh
    }
    #[cfg(not(target_os = "macos"))]
    {
        ShellKind::Bash
    }
}

pub fn is_loopback_hybrid_url(raw: &str) -> bool {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return false;
    }
    let Ok(parsed) = Url::parse(trimmed) else {
        return false;
    };
    if parsed.scheme() != "http" {
        return false;
    }
    if !parsed.username().is_empty() || parsed.password().is_some() {
        return false;
    }
    let host_is_loopback = matches!(
        parsed.host(),
        Some(Host::Ipv4(Ipv4Addr::LOCALHOST)) | Some(Host::Domain("localhost"))
    );
    host_is_loopback && parsed.port().is_some_and(|port| port > 0)
}

fn detect_hybrid_active_from(cloud_code_url: Option<&str>, hook_active: Option<&str>) -> bool {
    if let Some(raw) = cloud_code_url
        && !raw.trim().is_empty()
    {
        return is_loopback_hybrid_url(raw);
    }
    hook_active.map(str::trim) == Some("1")
}

pub fn detect_hybrid_active() -> bool {
    let cloud_code_url = std::env::var("CLOUD_CODE_URL").ok();
    let hook_active = std::env::var("AGY_COMPANION_HOOK_ACTIVE").ok();
    detect_hybrid_active_from(cloud_code_url.as_deref(), hook_active.as_deref())
}

fn validate_shell_safe_path(path: &Path) -> Result<&str, CoreError> {
    validate_path(path)?;
    let raw = path.to_str().ok_or_else(|| CoreError::InvalidPath {
        reason: "path must be valid UTF-8".to_string(),
    })?;
    if raw.contains('\n') || raw.contains('\r') {
        return Err(CoreError::InvalidPath {
            reason: "path must not contain newline characters".to_string(),
        });
    }
    Ok(raw)
}

fn posix_single_quote(raw: &str) -> String {
    format!("'{}'", raw.replace('\'', "'\"'\"'"))
}

fn fish_single_quote(raw: &str) -> String {
    let escaped = raw.replace('\\', "\\\\").replace('\'', "\\'");
    format!("'{escaped}'")
}

fn is_executable_regular_file(path: &Path) -> bool {
    let Ok(meta) = fs::metadata(path) else {
        return false;
    };
    if !meta.is_file() {
        return false;
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        meta.permissions().mode() & 0o111 != 0
    }
    #[cfg(not(unix))]
    {
        true
    }
}

pub fn resolve_stable_binary_path(
    override_bin: Option<&Path>,
    data_dir: Option<&Path>,
) -> Result<PathBuf, CoreError> {
    resolve_stable_binary_path_with(override_bin, data_dir, std::env::current_exe)
}

fn resolve_stable_binary_path_with<F>(
    override_bin: Option<&Path>,
    data_dir: Option<&Path>,
    current_exe: F,
) -> Result<PathBuf, CoreError>
where
    F: FnOnce() -> std::io::Result<PathBuf>,
{
    if let Some(bin) = override_bin {
        validate_shell_safe_path(bin)?;
        return Ok(bin.to_path_buf());
    }

    let resolved_data = match data_dir {
        Some(dir) => {
            validate_path(dir)?;
            Some(dir.to_path_buf())
        }
        None => default_data_dir().ok(),
    };

    if let Some(dir) = resolved_data {
        let bin_name = if cfg!(windows) {
            "agy-companion.exe"
        } else {
            "agy-companion"
        };
        let stable_bin = dir.join("bin").join(bin_name);
        if is_executable_regular_file(&stable_bin) {
            validate_shell_safe_path(&stable_bin)?;
            return Ok(stable_bin);
        }
    }

    let exe = current_exe().map_err(|source| CoreError::Io {
        path: PathBuf::from("current_exe"),
        source,
    })?;
    let resolved = fs::canonicalize(&exe).unwrap_or(exe);
    validate_shell_safe_path(&resolved)?;
    Ok(resolved)
}

fn resolve_optional_absolute_dir(candidate: Option<&Path>) -> Result<Option<PathBuf>, CoreError> {
    let Some(dir) = candidate else {
        return Ok(None);
    };
    if dir.as_os_str().is_empty() {
        return Ok(None);
    }
    if let Some(raw) = dir.to_str() {
        return resolve_xdg_base(Some(raw), TargetPlatform::current());
    }
    validate_path(dir)?;
    if dir.is_absolute() {
        Ok(Some(dir.to_path_buf()))
    } else {
        Ok(None)
    }
}

pub fn resolve_rc_path(
    shell: ShellKind,
    home: &Path,
    zdotdir: Option<&Path>,
    xdg_config_home: Option<&Path>,
) -> Result<PathBuf, CoreError> {
    validate_path(home)?;
    match shell {
        ShellKind::Zsh => {
            let base =
                resolve_optional_absolute_dir(zdotdir)?.unwrap_or_else(|| home.to_path_buf());
            Ok(base.join(".zlogin"))
        }
        ShellKind::Bash => {
            for candidate in [".bash_profile", ".bash_login", ".profile"] {
                let path = home.join(candidate);
                let check_path: PathBuf = path.components().collect();
                if fs::symlink_metadata(&check_path).is_ok() {
                    return Ok(path);
                }
            }
            Ok(home.join(".profile"))
        }
        ShellKind::Fish => {
            let base = resolve_optional_absolute_dir(xdg_config_home)?
                .unwrap_or_else(|| home.join(".config"));
            Ok(base.join("fish").join("conf.d").join(FISH_DROPIN_FILENAME))
        }
    }
}

pub fn render_env_sh(binary_path: &Path) -> Result<String, CoreError> {
    let bin_str = validate_shell_safe_path(binary_path)?;
    let quoted_bin = posix_single_quote(bin_str);
    let placeholder = PROXY_PLACEHOLDER_API_KEY;
    let default_prefix = DEFAULT_RESTORE_PREFIX;
    let tab = '\t';

    Ok(format!(
        r#"# Managed by Antigravity Companion (agy-companion). Do not edit manually.
# Re-generated automatically by `agy-companion integration enable` / `agy-companion install`.
if [ -n "${{SSH_CONNECTION:-}}${{SSH_CLIENT:-}}" ] && [ "${{AGY_COMPANION_ALLOW_REMOTE_SSH:-}}" != "1" ]; then
  return 0 2>/dev/null || exit 0
fi

if [ "${{ANTIGRAVITY_RESOLVING_ENVIRONMENT:-}}" = "1" ] && [ "${{AGY_COMPANION_HOOK_ACTIVE:-}}" != "1" ]; then
  _agy_bin={quoted_bin}
  if [ -x "$_agy_bin" ]; then
    _agy_port="$("$_agy_bin" hook-env --ensure-running --print-port 2>/dev/null || true)"
    case "$_agy_port" in
      ''|*[!0123456789]*|0*|??????*) _agy_port='' ;;
      *)
        if [ "$_agy_port" -lt 1 ] || [ "$_agy_port" -gt 65535 ]; then
          _agy_port=''
        fi
        ;;
    esac
    if [ -n "$_agy_port" ]; then
      if [ -n "${{JETSKI_MCP_ENV_RESTORE_PREFIX+x}}" ]; then
        export AGY_COMPANION_ORIG_RESTORE_PREFIX="$JETSKI_MCP_ENV_RESTORE_PREFIX"
      else
        unset AGY_COMPANION_ORIG_RESTORE_PREFIX
      fi
      if [ -n "${{JETSKI_MCP_ENV_RESTORE_VARS+x}}" ]; then
        export AGY_COMPANION_ORIG_RESTORE_VARS="$JETSKI_MCP_ENV_RESTORE_VARS"
      else
        unset AGY_COMPANION_ORIG_RESTORE_VARS
      fi

      _agy_prefix="${{JETSKI_MCP_ENV_RESTORE_PREFIX:-}}"
      case "$_agy_prefix" in
        [abcdefghijklmnopqrstuvwxyzABCDEFGHIJKLMNOPQRSTUVWXYZ_]*)
          case "$_agy_prefix" in
            *[!abcdefghijklmnopqrstuvwxyzABCDEFGHIJKLMNOPQRSTUVWXYZ0123456789_]*) _agy_prefix='{default_prefix}' ;;
          esac
          ;;
        *) _agy_prefix='{default_prefix}' ;;
      esac

      _agy_vars="${{JETSKI_MCP_ENV_RESTORE_VARS:-}}"
      _agy_orig_vars_csv=",${{_agy_vars}},"
      while :; do
        case "$_agy_orig_vars_csv" in
          *' '*) _agy_orig_vars_csv="${{_agy_orig_vars_csv%% *}}${{_agy_orig_vars_csv#* }}" ;;
          *'{tab}'*) _agy_orig_vars_csv="${{_agy_orig_vars_csv%%{tab}*}}${{_agy_orig_vars_csv#*{tab}}}" ;;
          *) break ;;
        esac
      done

      if [ -n "${{CLOUD_CODE_URL+x}}" ]; then
        export AGY_COMPANION_ORIG_CLOUD_CODE_URL="$CLOUD_CODE_URL"
      else
        unset AGY_COMPANION_ORIG_CLOUD_CODE_URL
      fi
      case "$_agy_orig_vars_csv" in
        *,CLOUD_CODE_URL,*) ;;
        *)
          if [ -n "${{CLOUD_CODE_URL+x}}" ]; then
            export "${{_agy_prefix}}CLOUD_CODE_URL=$CLOUD_CODE_URL"
          else
            unset "${{_agy_prefix}}CLOUD_CODE_URL"
          fi
          if [ "$_agy_prefix" != "{default_prefix}" ]; then
            export AGY_COMPANION_ADDED_PREFIX_CLOUD_CODE=1
          fi
          case "$_agy_vars" in
            '') _agy_vars='CLOUD_CODE_URL' ;;
            *,) _agy_vars="${{_agy_vars}}CLOUD_CODE_URL" ;;
            *) _agy_vars="${{_agy_vars}},CLOUD_CODE_URL" ;;
          esac
          ;;
      esac

      if [ -n "${{GOOGLE_GEMINI_BASE_URL+x}}" ]; then
        export AGY_COMPANION_ORIG_GOOGLE_GEMINI_BASE_URL="$GOOGLE_GEMINI_BASE_URL"
      else
        unset AGY_COMPANION_ORIG_GOOGLE_GEMINI_BASE_URL
      fi
      case "$_agy_orig_vars_csv" in
        *,GOOGLE_GEMINI_BASE_URL,*) ;;
        *)
          if [ -n "${{GOOGLE_GEMINI_BASE_URL+x}}" ]; then
            export "${{_agy_prefix}}GOOGLE_GEMINI_BASE_URL=$GOOGLE_GEMINI_BASE_URL"
          else
            unset "${{_agy_prefix}}GOOGLE_GEMINI_BASE_URL"
          fi
          if [ "$_agy_prefix" != "{default_prefix}" ]; then
            export AGY_COMPANION_ADDED_PREFIX_GEMINI_BASE=1
          fi
          case "$_agy_vars" in
            '') _agy_vars='GOOGLE_GEMINI_BASE_URL' ;;
            *,) _agy_vars="${{_agy_vars}}GOOGLE_GEMINI_BASE_URL" ;;
            *) _agy_vars="${{_agy_vars}},GOOGLE_GEMINI_BASE_URL" ;;
          esac
          ;;
      esac

      if [ -z "${{GEMINI_API_KEY:-}}" ] || [ "${{GEMINI_API_KEY:-}}" = "{placeholder}" ]; then
        export GEMINI_API_KEY='{placeholder}'
        case "$_agy_orig_vars_csv" in
          *,GEMINI_API_KEY,*) ;;
          *)
            unset "${{_agy_prefix}}GEMINI_API_KEY"
            case "$_agy_vars" in
              '') _agy_vars='GEMINI_API_KEY' ;;
              *,) _agy_vars="${{_agy_vars}}GEMINI_API_KEY" ;;
              *) _agy_vars="${{_agy_vars}},GEMINI_API_KEY" ;;
            esac
            ;;
        esac
      fi

      _agy_url="http://127.0.0.1:${{_agy_port}}"
      export CLOUD_CODE_URL="$_agy_url"
      export GOOGLE_GEMINI_BASE_URL="$_agy_url"
      export AGY_COMPANION_INJECTED_URL="$_agy_url"
      export AGY_COMPANION_HOOK_ACTIVE=1
      export JETSKI_MCP_ENV_RESTORE_PREFIX="$_agy_prefix"
      export JETSKI_MCP_ENV_RESTORE_VARS="$_agy_vars"
    fi
  fi
  unset _agy_bin _agy_port _agy_prefix _agy_vars _agy_orig_vars_csv _agy_url
elif [ "${{ANTIGRAVITY_RESOLVING_ENVIRONMENT:-}}" != "1" ] && [ "${{AGY_COMPANION_HOOK_ACTIVE:-}}" = "1" ]; then
  _agy_should_scrub_cc=0
  if [ -n "${{AGY_COMPANION_INJECTED_URL:-}}" ]; then
    if [ "${{CLOUD_CODE_URL:-}}" = "$AGY_COMPANION_INJECTED_URL" ]; then
      _agy_should_scrub_cc=1
    fi
  else
    case "${{CLOUD_CODE_URL:-}}" in
      http://127.0.0.1:[0123456789]*|http://localhost:[0123456789]*) _agy_should_scrub_cc=1 ;;
    esac
  fi
  if [ "$_agy_should_scrub_cc" = "1" ]; then
    if [ -n "${{AGY_COMPANION_ORIG_CLOUD_CODE_URL+x}}" ]; then
      export CLOUD_CODE_URL="$AGY_COMPANION_ORIG_CLOUD_CODE_URL"
    else
      unset CLOUD_CODE_URL
    fi
  fi

  _agy_should_scrub_gb=0
  if [ -n "${{AGY_COMPANION_INJECTED_URL:-}}" ]; then
    if [ "${{GOOGLE_GEMINI_BASE_URL:-}}" = "$AGY_COMPANION_INJECTED_URL" ]; then
      _agy_should_scrub_gb=1
    fi
  else
    case "${{GOOGLE_GEMINI_BASE_URL:-}}" in
      http://127.0.0.1:[0123456789]*|http://localhost:[0123456789]*) _agy_should_scrub_gb=1 ;;
    esac
  fi
  if [ "$_agy_should_scrub_gb" = "1" ]; then
    if [ -n "${{AGY_COMPANION_ORIG_GOOGLE_GEMINI_BASE_URL+x}}" ]; then
      export GOOGLE_GEMINI_BASE_URL="$AGY_COMPANION_ORIG_GOOGLE_GEMINI_BASE_URL"
    else
      unset GOOGLE_GEMINI_BASE_URL
    fi
  fi

  if [ "${{GEMINI_API_KEY:-}}" = "{placeholder}" ]; then
    unset GEMINI_API_KEY
  fi

  _agy_scrub_prefix="${{JETSKI_MCP_ENV_RESTORE_PREFIX:-}}"
  case "$_agy_scrub_prefix" in
    [abcdefghijklmnopqrstuvwxyzABCDEFGHIJKLMNOPQRSTUVWXYZ_]*)
      case "$_agy_scrub_prefix" in
        *[!abcdefghijklmnopqrstuvwxyzABCDEFGHIJKLMNOPQRSTUVWXYZ0123456789_]*) _agy_scrub_prefix='' ;;
      esac
      ;;
    *) _agy_scrub_prefix='' ;;
  esac
  if [ -n "$_agy_scrub_prefix" ] && [ "$_agy_scrub_prefix" != "{default_prefix}" ]; then
    if [ "${{AGY_COMPANION_ADDED_PREFIX_CLOUD_CODE:-}}" = "1" ]; then
      unset "${{_agy_scrub_prefix}}CLOUD_CODE_URL"
    fi
    if [ "${{AGY_COMPANION_ADDED_PREFIX_GEMINI_BASE:-}}" = "1" ]; then
      unset "${{_agy_scrub_prefix}}GOOGLE_GEMINI_BASE_URL"
    fi
  fi

  if [ -n "${{AGY_COMPANION_ORIG_RESTORE_PREFIX+x}}" ]; then
    export JETSKI_MCP_ENV_RESTORE_PREFIX="$AGY_COMPANION_ORIG_RESTORE_PREFIX"
  else
    unset JETSKI_MCP_ENV_RESTORE_PREFIX
  fi
  if [ -n "${{AGY_COMPANION_ORIG_RESTORE_VARS+x}}" ]; then
    export JETSKI_MCP_ENV_RESTORE_VARS="$AGY_COMPANION_ORIG_RESTORE_VARS"
  else
    unset JETSKI_MCP_ENV_RESTORE_VARS
  fi

  unset AGY_COMPANION_HOOK_ACTIVE AGY_COMPANION_INJECTED_URL \
    AGY_COMPANION_ORIG_CLOUD_CODE_URL AGY_COMPANION_ORIG_GOOGLE_GEMINI_BASE_URL \
    AGY_COMPANION_ORIG_RESTORE_PREFIX AGY_COMPANION_ORIG_RESTORE_VARS \
    AGY_COMPANION_ADDED_PREFIX_CLOUD_CODE AGY_COMPANION_ADDED_PREFIX_GEMINI_BASE \
    _agy_should_scrub_cc _agy_should_scrub_gb _agy_scrub_prefix
fi
"#
    ))
}

pub fn render_fish_dropin(binary_path: &Path) -> Result<String, CoreError> {
    let bin_str = validate_shell_safe_path(binary_path)?;
    let quoted_bin = fish_single_quote(bin_str);
    let placeholder = PROXY_PLACEHOLDER_API_KEY;
    let default_prefix = DEFAULT_RESTORE_PREFIX;

    Ok(format!(
        r#"# Managed by Antigravity Companion (agy-companion). Do not edit manually.
# Re-generated automatically by `agy-companion integration enable` / `agy-companion install`.
if test "$AGY_COMPANION_ALLOW_REMOTE_SSH" = "1"; or begin; test -z "$SSH_CONNECTION"; and test -z "$SSH_CLIENT"; end
    if test "$ANTIGRAVITY_RESOLVING_ENVIRONMENT" = "1"; and test "$AGY_COMPANION_HOOK_ACTIVE" != "1"
        set -l _agy_bin {quoted_bin}
        if test -x "$_agy_bin"
            set -l _agy_port ("$_agy_bin" hook-env --ensure-running --print-port 2>/dev/null | string collect | string trim)
            if string match -qr '\A[1-9][0-9]{{0,4}}\z' -- "$_agy_port"
                if test "$_agy_port" -ge 1 -a "$_agy_port" -le 65535
                    if set -q JETSKI_MCP_ENV_RESTORE_PREFIX
                        set -gx AGY_COMPANION_ORIG_RESTORE_PREFIX "$JETSKI_MCP_ENV_RESTORE_PREFIX"
                    else
                        set -e AGY_COMPANION_ORIG_RESTORE_PREFIX
                    end
                    if set -q JETSKI_MCP_ENV_RESTORE_VARS
                        set -gx AGY_COMPANION_ORIG_RESTORE_VARS "$JETSKI_MCP_ENV_RESTORE_VARS"
                    else
                        set -e AGY_COMPANION_ORIG_RESTORE_VARS
                    end

                    set -l _agy_prefix '{default_prefix}'
                    if set -q JETSKI_MCP_ENV_RESTORE_PREFIX; and string match -qr '\A[A-Za-z_][A-Za-z0-9_]*\z' -- "$JETSKI_MCP_ENV_RESTORE_PREFIX"
                        set _agy_prefix "$JETSKI_MCP_ENV_RESTORE_PREFIX"
                    end

                    set -l _agy_vars ""
                    if set -q JETSKI_MCP_ENV_RESTORE_VARS
                        set _agy_vars "$JETSKI_MCP_ENV_RESTORE_VARS"
                    end
                    set -l _agy_orig_csv ","(string replace -ra '[ \t]+' '' -- "$_agy_vars")","

                    if set -q CLOUD_CODE_URL
                        set -gx AGY_COMPANION_ORIG_CLOUD_CODE_URL "$CLOUD_CODE_URL"
                    else
                        set -e AGY_COMPANION_ORIG_CLOUD_CODE_URL
                    end
                    if not string match -q '*,CLOUD_CODE_URL,*' -- "$_agy_orig_csv"
                        if set -q CLOUD_CODE_URL
                            set -gx "$_agy_prefix"CLOUD_CODE_URL "$CLOUD_CODE_URL"
                        else
                            set -e "$_agy_prefix"CLOUD_CODE_URL
                        end
                        if test "$_agy_prefix" != "{default_prefix}"
                            set -gx AGY_COMPANION_ADDED_PREFIX_CLOUD_CODE 1
                        end
                        if test -z "$_agy_vars"
                            set _agy_vars 'CLOUD_CODE_URL'
                        else if string match -q '*,' -- "$_agy_vars"
                            set _agy_vars "$_agy_vars"CLOUD_CODE_URL
                        else
                            set _agy_vars "$_agy_vars,CLOUD_CODE_URL"
                        end
                    end

                    if set -q GOOGLE_GEMINI_BASE_URL
                        set -gx AGY_COMPANION_ORIG_GOOGLE_GEMINI_BASE_URL "$GOOGLE_GEMINI_BASE_URL"
                    else
                        set -e AGY_COMPANION_ORIG_GOOGLE_GEMINI_BASE_URL
                    end
                    if not string match -q '*,GOOGLE_GEMINI_BASE_URL,*' -- "$_agy_orig_csv"
                        if set -q GOOGLE_GEMINI_BASE_URL
                            set -gx "$_agy_prefix"GOOGLE_GEMINI_BASE_URL "$GOOGLE_GEMINI_BASE_URL"
                        else
                            set -e "$_agy_prefix"GOOGLE_GEMINI_BASE_URL
                        end
                        if test "$_agy_prefix" != "{default_prefix}"
                            set -gx AGY_COMPANION_ADDED_PREFIX_GEMINI_BASE 1
                        end
                        if test -z "$_agy_vars"
                            set _agy_vars 'GOOGLE_GEMINI_BASE_URL'
                        else if string match -q '*,' -- "$_agy_vars"
                            set _agy_vars "$_agy_vars"GOOGLE_GEMINI_BASE_URL
                        else
                            set _agy_vars "$_agy_vars,GOOGLE_GEMINI_BASE_URL"
                        end
                    end

                    if test -z "$GEMINI_API_KEY"; or test "$GEMINI_API_KEY" = "{placeholder}"
                        set -gx GEMINI_API_KEY '{placeholder}'
                        if not string match -q '*,GEMINI_API_KEY,*' -- "$_agy_orig_csv"
                            set -e "$_agy_prefix"GEMINI_API_KEY
                            if test -z "$_agy_vars"
                                set _agy_vars 'GEMINI_API_KEY'
                            else if string match -q '*,' -- "$_agy_vars"
                                set _agy_vars "$_agy_vars"GEMINI_API_KEY
                            else
                                set _agy_vars "$_agy_vars,GEMINI_API_KEY"
                            end
                        end
                    end

                    set -l _agy_url "http://127.0.0.1:$_agy_port"
                    set -gx CLOUD_CODE_URL "$_agy_url"
                    set -gx GOOGLE_GEMINI_BASE_URL "$_agy_url"
                    set -gx AGY_COMPANION_INJECTED_URL "$_agy_url"
                    set -gx AGY_COMPANION_HOOK_ACTIVE 1
                    set -gx JETSKI_MCP_ENV_RESTORE_PREFIX "$_agy_prefix"
                    set -gx JETSKI_MCP_ENV_RESTORE_VARS "$_agy_vars"
                end
            end
        end
    else if test "$ANTIGRAVITY_RESOLVING_ENVIRONMENT" != "1"; and test "$AGY_COMPANION_HOOK_ACTIVE" = "1"
        set -l _agy_should_scrub_cc 0
        if test -n "$AGY_COMPANION_INJECTED_URL"
            if test "$CLOUD_CODE_URL" = "$AGY_COMPANION_INJECTED_URL"
                set _agy_should_scrub_cc 1
            end
        else if string match -qr '\Ahttp://(127\.0\.0\.1|localhost):[0-9]+\z' -- "$CLOUD_CODE_URL"
            set _agy_should_scrub_cc 1
        end
        if test "$_agy_should_scrub_cc" = "1"
            if set -q AGY_COMPANION_ORIG_CLOUD_CODE_URL
                set -gx CLOUD_CODE_URL "$AGY_COMPANION_ORIG_CLOUD_CODE_URL"
            else
                set -e CLOUD_CODE_URL
            end
        end

        set -l _agy_should_scrub_gb 0
        if test -n "$AGY_COMPANION_INJECTED_URL"
            if test "$GOOGLE_GEMINI_BASE_URL" = "$AGY_COMPANION_INJECTED_URL"
                set _agy_should_scrub_gb 1
            end
        else if string match -qr '\Ahttp://(127\.0\.0\.1|localhost):[0-9]+\z' -- "$GOOGLE_GEMINI_BASE_URL"
            set _agy_should_scrub_gb 1
        end
        if test "$_agy_should_scrub_gb" = "1"
            if set -q AGY_COMPANION_ORIG_GOOGLE_GEMINI_BASE_URL
                set -gx GOOGLE_GEMINI_BASE_URL "$AGY_COMPANION_ORIG_GOOGLE_GEMINI_BASE_URL"
            else
                set -e GOOGLE_GEMINI_BASE_URL
            end
        end

        if test "$GEMINI_API_KEY" = "{placeholder}"
            set -e GEMINI_API_KEY
        end

        if set -q JETSKI_MCP_ENV_RESTORE_PREFIX; and string match -qr '\A[A-Za-z_][A-Za-z0-9_]*\z' -- "$JETSKI_MCP_ENV_RESTORE_PREFIX"
            if test "$JETSKI_MCP_ENV_RESTORE_PREFIX" != "{default_prefix}"
                if test "$AGY_COMPANION_ADDED_PREFIX_CLOUD_CODE" = "1"
                    set -e "$JETSKI_MCP_ENV_RESTORE_PREFIX"CLOUD_CODE_URL
                end
                if test "$AGY_COMPANION_ADDED_PREFIX_GEMINI_BASE" = "1"
                    set -e "$JETSKI_MCP_ENV_RESTORE_PREFIX"GOOGLE_GEMINI_BASE_URL
                end
            end
        end

        if set -q AGY_COMPANION_ORIG_RESTORE_PREFIX
            set -gx JETSKI_MCP_ENV_RESTORE_PREFIX "$AGY_COMPANION_ORIG_RESTORE_PREFIX"
        else
            set -e JETSKI_MCP_ENV_RESTORE_PREFIX
        end
        if set -q AGY_COMPANION_ORIG_RESTORE_VARS
            set -gx JETSKI_MCP_ENV_RESTORE_VARS "$AGY_COMPANION_ORIG_RESTORE_VARS"
        else
            set -e JETSKI_MCP_ENV_RESTORE_VARS
        end

        set -e AGY_COMPANION_HOOK_ACTIVE AGY_COMPANION_INJECTED_URL \
            AGY_COMPANION_ORIG_CLOUD_CODE_URL AGY_COMPANION_ORIG_GOOGLE_GEMINI_BASE_URL \
            AGY_COMPANION_ORIG_RESTORE_PREFIX AGY_COMPANION_ORIG_RESTORE_VARS \
            AGY_COMPANION_ADDED_PREFIX_CLOUD_CODE AGY_COMPANION_ADDED_PREFIX_GEMINI_BASE
    end
end
"#
    ))
}

fn wrap_with_markers(body: &str) -> String {
    let trimmed = body.trim_end_matches(['\r', '\n']);
    format!("{HOOK_BEGIN_MARKER}\n{trimmed}\n{HOOK_END_MARKER}")
}

fn render_posix_rc_snippet_internal(
    env_script_path: &Path,
    use_portable: bool,
) -> Result<String, CoreError> {
    let line = if use_portable {
        format!("[ -r \"{PORTABLE_ENV_SCRIPT_EXPR}\" ] && . \"{PORTABLE_ENV_SCRIPT_EXPR}\"")
    } else {
        let raw = validate_shell_safe_path(env_script_path)?;
        let quoted = posix_single_quote(raw);
        format!("[ -r {quoted} ] && . {quoted}")
    };
    Ok(wrap_with_markers(&line))
}

/// Renders the marker-wrapped shell snippet for `shell`.
///
/// - For POSIX shells (`ShellKind::Zsh` and `ShellKind::Bash`), `target_path` is the path to
///   `env.sh` (or `PORTABLE_ENV_SCRIPT_EXPR` to emit the portable `${XDG_DATA_HOME:-$HOME/.local/share}` hook).
/// - For `ShellKind::Fish`, which does not use an `env.sh` helper script, `target_path` is the
///   path to the `agy-companion` executable passed to [`render_fish_dropin`].
pub fn render_rc_snippet(shell: ShellKind, target_path: &Path) -> Result<String, CoreError> {
    match shell {
        ShellKind::Zsh | ShellKind::Bash => {
            let matches_portable = target_path == Path::new(PORTABLE_ENV_SCRIPT_EXPR)
                || default_data_dir()
                    .ok()
                    .map(|dir| dir.join(ENV_SCRIPT_FILENAME))
                    .as_deref()
                    == Some(target_path);
            render_posix_rc_snippet_internal(target_path, matches_portable)
        }
        ShellKind::Fish => {
            let body = render_fish_dropin(target_path)?;
            Ok(wrap_with_markers(&body))
        }
    }
}

pub fn write_env_script(env_script_path: &Path, binary_path: &Path) -> Result<(), CoreError> {
    validate_shell_safe_path(env_script_path)?;
    let home = dirs::home_dir();
    validate_env_script_parent_dir(env_script_path, home.as_deref())?;
    let content = render_env_sh(binary_path)?;
    write_file_atomic_with_mode(
        env_script_path,
        content.as_bytes(),
        0o600,
        0o700,
        true,
        false,
        true,
    )
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum MarkerState {
    Absent,
    Valid { start: usize, end: usize },
}

fn split_lines_preserving_eol(content: &str) -> (Vec<&str>, &'static str, bool) {
    let eol = if content.contains("\r\n") {
        "\r\n"
    } else {
        "\n"
    };
    let had_trailing_newline = content.ends_with('\n');
    if content.is_empty() {
        return (Vec::new(), eol, false);
    }
    let body = content
        .strip_suffix("\r\n")
        .or_else(|| content.strip_suffix('\n'))
        .unwrap_or(content);
    let lines = body
        .split('\n')
        .map(|line| line.strip_suffix('\r').unwrap_or(line))
        .collect();
    (lines, eol, had_trailing_newline)
}

fn is_begin_marker(trimmed: &str) -> bool {
    trimmed.starts_with("# >>> agy-companion")
}

fn is_end_marker(trimmed: &str) -> bool {
    trimmed.starts_with("# <<< agy-companion")
}

fn parse_markers(path: &Path, lines: &[&str]) -> Result<MarkerState, CoreError> {
    let mut begins = Vec::new();
    let mut ends = Vec::new();

    for (idx, line) in lines.iter().enumerate() {
        let trimmed = line.trim();
        if is_begin_marker(trimmed) {
            begins.push(idx + 1);
        } else if is_end_marker(trimmed) {
            ends.push(idx + 1);
        }
    }

    match (begins.as_slice(), ends.as_slice()) {
        ([], []) => Ok(MarkerState::Absent),
        ([begin], [end]) if begin < end => Ok(MarkerState::Valid {
            start: begin - 1,
            end: end - 1,
        }),
        ([begin], []) => Err(CoreError::CorruptedMarkers {
            path: path.to_path_buf(),
            reason: format!("unclosed begin marker at line {begin} (missing end marker)"),
        }),
        ([], [end]) => Err(CoreError::CorruptedMarkers {
            path: path.to_path_buf(),
            reason: format!("orphan end marker at line {end} (missing begin marker)"),
        }),
        ([begin], [end]) => Err(CoreError::CorruptedMarkers {
            path: path.to_path_buf(),
            reason: format!("end marker at line {end} appears before begin marker at line {begin}"),
        }),
        _ => Err(CoreError::CorruptedMarkers {
            path: path.to_path_buf(),
            reason: format!(
                "duplicate or unbalanced markers (begin lines: {begins:?}, end lines: {ends:?})"
            ),
        }),
    }
}

fn join_lines_preserving_eol(lines: &[&str], eol: &str, trailing_newline: bool) -> String {
    if lines.is_empty() {
        return String::new();
    }
    let mut out = lines.join(eol);
    if trailing_newline {
        out.push_str(eol);
    }
    out
}

fn upsert_marker_block(path: &Path, content: &str, block: &str) -> Result<String, CoreError> {
    let (lines, eol, had_trailing_newline) = split_lines_preserving_eol(content);
    let block_lines: Vec<&str> = block
        .split('\n')
        .map(|l| l.strip_suffix('\r').unwrap_or(l))
        .collect();
    let state = parse_markers(path, &lines)?;

    let mut result_lines: Vec<&str> = Vec::with_capacity(lines.len() + block_lines.len());
    match state {
        MarkerState::Absent => {
            result_lines.extend_from_slice(&lines);
            result_lines.extend_from_slice(&block_lines);
        }
        MarkerState::Valid { start, end } => {
            result_lines.extend_from_slice(&lines[..start]);
            result_lines.extend_from_slice(&block_lines);
            result_lines.extend_from_slice(&lines[end + 1..]);
        }
    }

    let trailing = if lines.is_empty() {
        true
    } else {
        had_trailing_newline
    };
    Ok(join_lines_preserving_eol(&result_lines, eol, trailing))
}

fn strip_marker_block(path: &Path, content: &str) -> Result<(String, bool), CoreError> {
    let (lines, eol, had_trailing_newline) = split_lines_preserving_eol(content);
    let state = parse_markers(path, &lines)?;
    match state {
        MarkerState::Absent => Ok((content.to_string(), false)),
        MarkerState::Valid { start, end } => {
            let mut result_lines: Vec<&str> = Vec::with_capacity(lines.len());
            result_lines.extend_from_slice(&lines[..start]);
            result_lines.extend_from_slice(&lines[end + 1..]);
            Ok((
                join_lines_preserving_eol(&result_lines, eol, had_trailing_newline),
                true,
            ))
        }
    }
}

fn is_deletable_when_empty(path: &Path) -> bool {
    matches!(
        path.file_name().and_then(|n| n.to_str()),
        Some(".zlogin") | Some(FISH_DROPIN_FILENAME)
    )
}

#[derive(Debug)]
struct LoadedRegularFile {
    content: String,
    unix_mode: u32,
    readonly: bool,
}

fn read_regular_file_checked(path: &Path) -> Result<Option<LoadedRegularFile>, CoreError> {
    validate_path(path)?;
    let check_path: PathBuf = path.components().collect();
    let lstat_meta = match fs::symlink_metadata(&check_path) {
        Ok(meta) => meta,
        Err(err) if err.kind() == ErrorKind::NotFound => return Ok(None),
        Err(source) => {
            return Err(CoreError::Io {
                path: path.to_path_buf(),
                source,
            });
        }
    };

    if lstat_meta.file_type().is_symlink() {
        return Err(CoreError::InvalidPath {
            reason: format!("symbolic links are not allowed for '{}'", path.display()),
        });
    }
    if !lstat_meta.file_type().is_file() {
        return Err(CoreError::InvalidPath {
            reason: format!("path '{}' must be a regular file", path.display()),
        });
    }
    if lstat_meta.len() > MAX_RC_FILE_BYTES {
        return Err(CoreError::InvalidPath {
            reason: format!(
                "file '{}' exceeds maximum allowed size of {MAX_RC_FILE_BYTES} bytes",
                path.display()
            ),
        });
    }

    let file = File::open(&check_path).map_err(|source| CoreError::Io {
        path: path.to_path_buf(),
        source,
    })?;
    let fstat_meta = file.metadata().map_err(|source| CoreError::Io {
        path: path.to_path_buf(),
        source,
    })?;
    if !fstat_meta.file_type().is_file() {
        return Err(CoreError::InvalidPath {
            reason: format!("path '{}' must be a regular file", path.display()),
        });
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        if lstat_meta.dev() != fstat_meta.dev() || lstat_meta.ino() != fstat_meta.ino() {
            return Err(CoreError::InvalidPath {
                reason: format!(
                    "path '{}' changed during open (TOCTOU symlink/file swap detected)",
                    path.display()
                ),
            });
        }
    }
    if fstat_meta.len() > MAX_RC_FILE_BYTES {
        return Err(CoreError::InvalidPath {
            reason: format!(
                "file '{}' exceeds maximum allowed size of {MAX_RC_FILE_BYTES} bytes",
                path.display()
            ),
        });
    }

    let mut bytes = Vec::new();
    file.take(MAX_RC_FILE_BYTES + 1)
        .read_to_end(&mut bytes)
        .map_err(|source| CoreError::Io {
            path: path.to_path_buf(),
            source,
        })?;
    if bytes.len() as u64 > MAX_RC_FILE_BYTES {
        return Err(CoreError::InvalidPath {
            reason: format!(
                "file '{}' exceeds maximum allowed size of {MAX_RC_FILE_BYTES} bytes",
                path.display()
            ),
        });
    }

    let content = String::from_utf8(bytes).map_err(|_| CoreError::InvalidPath {
        reason: format!("file '{}' is not valid UTF-8", path.display()),
    })?;

    #[cfg(unix)]
    let unix_mode = {
        use std::os::unix::fs::PermissionsExt;
        fstat_meta.permissions().mode() & 0o777
    };
    #[cfg(not(unix))]
    let unix_mode = DEFAULT_RC_FILE_MODE;

    let readonly = fstat_meta.permissions().readonly();
    Ok(Some(LoadedRegularFile {
        content,
        unix_mode,
        readonly,
    }))
}

#[derive(Debug)]
enum SymlinkTargetKind {
    Regular {
        canonical: PathBuf,
        file: LoadedRegularFile,
        markers: MarkerState,
    },
    Dangling,
    NonRegular,
}

#[derive(Debug)]
enum RcInspection {
    Missing,
    Regular {
        file: LoadedRegularFile,
        markers: MarkerState,
    },
    Symlink {
        link_target: Option<PathBuf>,
        target: SymlinkTargetKind,
    },
}

#[derive(Debug)]
enum DirChainInspection {
    SafeDir,
    Missing,
    Symlink {
        symlink_dir: PathBuf,
        link_target: Option<PathBuf>,
    },
    NotDirectory {
        path: PathBuf,
    },
}

#[derive(Debug)]
enum DirTargetState {
    Ready,
    Missing,
    NotDirectory { path: PathBuf },
}

fn default_fish_dir_chain(
    home: &Path,
    xdg_config_home: Option<&Path>,
) -> Result<(PathBuf, &'static [&'static str]), CoreError> {
    if let Some(xdg) = resolve_optional_absolute_dir(xdg_config_home)? {
        let default_dot_config = home.join(".config");
        let norm_xdg: PathBuf = xdg.components().collect();
        let norm_default: PathBuf = default_dot_config.components().collect();
        if norm_xdg != norm_default {
            return Ok((xdg, &["fish", "conf.d"]));
        }
    }
    Ok((home.to_path_buf(), &[".config", "fish", "conf.d"]))
}

fn inspect_dir_chain(root: &Path, rel_dirs: &[&str]) -> Result<DirChainInspection, CoreError> {
    let mut current = root.to_path_buf();
    for segment in rel_dirs {
        current.push(segment);
        let check_path: PathBuf = current.components().collect();
        match fs::symlink_metadata(&check_path) {
            Ok(meta) => {
                if meta.file_type().is_symlink() {
                    let link_target = fs::read_link(&check_path).ok();
                    return Ok(DirChainInspection::Symlink {
                        symlink_dir: current,
                        link_target,
                    });
                }
                if !meta.file_type().is_dir() {
                    return Ok(DirChainInspection::NotDirectory { path: current });
                }
            }
            Err(err) if err.kind() == ErrorKind::NotFound => {
                return Ok(DirChainInspection::Missing);
            }
            Err(source) => {
                return Err(CoreError::Io {
                    path: current,
                    source,
                });
            }
        }
    }
    Ok(DirChainInspection::SafeDir)
}

fn validate_env_script_parent_dir(env_path: &Path, home: Option<&Path>) -> Result<(), CoreError> {
    let data_dir = env_path
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .ok_or_else(|| CoreError::InvalidPath {
            reason: format!(
                "env script path '{}' must include a parent directory",
                env_path.display()
            ),
        })?;
    let norm_data_dir: PathBuf = data_dir.components().collect();
    if let Some(home_dir) = home {
        let default_data_dir: PathBuf = home_dir
            .join(".local")
            .join("share")
            .join("agy-companion")
            .components()
            .collect();
        if norm_data_dir == default_data_dir {
            match inspect_dir_chain(home_dir, &[".local", "share", "agy-companion"])? {
                DirChainInspection::SafeDir | DirChainInspection::Missing => return Ok(()),
                DirChainInspection::Symlink { symlink_dir, .. } => {
                    return Err(CoreError::InvalidPath {
                        reason: format!(
                            "symbolic links are not allowed for env script data directory '{}'",
                            symlink_dir.display()
                        ),
                    });
                }
                DirChainInspection::NotDirectory { path } => {
                    return Err(CoreError::InvalidPath {
                        reason: format!(
                            "intermediate directory '{}' for '{}' is not a directory",
                            path.display(),
                            env_path.display()
                        ),
                    });
                }
            }
        }
    }
    match fs::symlink_metadata(&norm_data_dir) {
        Ok(meta) => {
            if meta.file_type().is_symlink() {
                return Err(CoreError::InvalidPath {
                    reason: format!(
                        "symbolic links are not allowed for env script data directory '{}'",
                        data_dir.display()
                    ),
                });
            }
            if !meta.file_type().is_dir() {
                return Err(CoreError::InvalidPath {
                    reason: format!(
                        "env script parent directory '{}' must be a directory",
                        data_dir.display()
                    ),
                });
            }
            Ok(())
        }
        Err(err) if err.kind() == ErrorKind::NotFound => Ok(()),
        Err(source) => Err(CoreError::Io {
            path: data_dir.to_path_buf(),
            source,
        }),
    }
}

fn validate_env_script_dir_chain(ctx: &ResolvedContext) -> Result<(), CoreError> {
    let Some(env_path) = ctx.env_script_path.as_deref() else {
        return Ok(());
    };
    validate_env_script_parent_dir(env_path, Some(&ctx.home))?;
    let check_path: PathBuf = env_path.components().collect();
    match fs::symlink_metadata(&check_path) {
        Ok(meta) if meta.file_type().is_symlink() => Err(CoreError::InvalidPath {
            reason: format!(
                "symbolic links are not allowed for env script '{}'",
                env_path.display()
            ),
        }),
        Ok(meta) if !meta.file_type().is_file() => Err(CoreError::InvalidPath {
            reason: format!("env script '{}' must be a regular file", env_path.display()),
        }),
        Ok(_) => Ok(()),
        Err(err) if err.kind() == ErrorKind::NotFound => Ok(()),
        Err(source) => Err(CoreError::Io {
            path: env_path.to_path_buf(),
            source,
        }),
    }
}

fn inspect_symlinked_dir_chain_target(
    root: &Path,
    rel_dirs: &[&str],
) -> Result<DirTargetState, CoreError> {
    let mut current = root.to_path_buf();
    for segment in rel_dirs {
        current.push(segment);
        let check_path: PathBuf = current.components().collect();
        match fs::metadata(&check_path) {
            Ok(meta) if meta.is_dir() => {}
            Ok(_) => return Ok(DirTargetState::NotDirectory { path: current }),
            Err(err) if err.kind() == ErrorKind::NotFound => return Ok(DirTargetState::Missing),
            Err(source) => {
                return Err(CoreError::Io {
                    path: current,
                    source,
                });
            }
        }
    }
    Ok(DirTargetState::Ready)
}

fn inspect_rc_file(path: &Path) -> Result<RcInspection, CoreError> {
    validate_path(path)?;
    let check_path: PathBuf = path.components().collect();
    let lstat_meta = match fs::symlink_metadata(&check_path) {
        Ok(meta) => meta,
        Err(err) if err.kind() == ErrorKind::NotFound => return Ok(RcInspection::Missing),
        Err(source) => {
            return Err(CoreError::Io {
                path: path.to_path_buf(),
                source,
            });
        }
    };

    if lstat_meta.file_type().is_symlink() {
        let link_target = fs::read_link(&check_path).ok();
        match fs::metadata(&check_path) {
            Ok(target_meta) => {
                if !target_meta.file_type().is_file() {
                    return Ok(RcInspection::Symlink {
                        link_target,
                        target: SymlinkTargetKind::NonRegular,
                    });
                }
                let canonical = fs::canonicalize(&check_path).map_err(|source| CoreError::Io {
                    path: path.to_path_buf(),
                    source,
                })?;
                let loaded = read_regular_file_checked(&canonical)?;
                let target = match loaded {
                    Some(file) => {
                        let (lines, _, _) = split_lines_preserving_eol(&file.content);
                        let markers = parse_markers(&canonical, &lines)?;
                        SymlinkTargetKind::Regular {
                            canonical,
                            file,
                            markers,
                        }
                    }
                    None => SymlinkTargetKind::Dangling,
                };
                Ok(RcInspection::Symlink {
                    link_target,
                    target,
                })
            }
            Err(err) if err.kind() == ErrorKind::NotFound => Ok(RcInspection::Symlink {
                link_target,
                target: SymlinkTargetKind::Dangling,
            }),
            Err(source) => Err(CoreError::Io {
                path: path.to_path_buf(),
                source,
            }),
        }
    } else {
        let Some(loaded) = read_regular_file_checked(path)? else {
            return Ok(RcInspection::Missing);
        };
        let (lines, _, _) = split_lines_preserving_eol(&loaded.content);
        let markers = parse_markers(path, &lines)?;
        Ok(RcInspection::Regular {
            file: loaded,
            markers,
        })
    }
}

fn is_regular_file_existing(path: &Path) -> bool {
    let check_path: PathBuf = path.components().collect();
    fs::symlink_metadata(&check_path).is_ok_and(|meta| meta.file_type().is_file())
}

fn build_unsupported_status(hybrid_active: bool) -> IntegrationStatus {
    IntegrationStatus {
        supported: false,
        enabled: false,
        manual_required: false,
        shell: None,
        rc_path: None,
        env_script_path: None,
        binary_path: None,
        symlink_target: None,
        manual_snippet: None,
        hybrid_active,
        message: Some(
            "Login-shell RC integration is only used on macOS and Linux. Use `agy-companion launch` or the Windows Start Menu shortcut instead."
                .to_string(),
        ),
    }
}

struct ResolvedContext {
    shell: ShellKind,
    rc_path: PathBuf,
    env_script_path: Option<PathBuf>,
    binary_path: PathBuf,
    use_portable_env_expr: bool,
    home: PathBuf,
    zdotdir: Option<PathBuf>,
    xdg_config_home: Option<PathBuf>,
}

#[allow(clippy::too_many_arguments)]
fn resolve_context(
    opts: &IntegrationOptions,
    home: Option<&Path>,
    zdotdir: Option<&Path>,
    xdg_config_home: Option<&Path>,
    shell_env: Option<&str>,
    default_data_override: Option<&Path>,
) -> Result<ResolvedContext, CoreError> {
    let shell = opts.shell.unwrap_or_else(|| detect_shell(shell_env));
    let home_dir = home
        .map(Path::to_path_buf)
        .ok_or(CoreError::HomeDirUnavailable)?;
    validate_path(&home_dir)?;

    let (data_dir, use_portable_env_expr) = match opts.data_dir.as_deref() {
        Some(custom) => {
            validate_path(custom)?;
            (custom.to_path_buf(), false)
        }
        None => match default_data_override {
            Some(dir) => {
                validate_path(dir)?;
                (dir.to_path_buf(), true)
            }
            None => (default_data_dir()?, true),
        },
    };

    let rc_path = match opts.rc_file.as_deref() {
        Some(custom_rc) => {
            validate_shell_safe_path(custom_rc)?;
            custom_rc.to_path_buf()
        }
        None => resolve_rc_path(shell, &home_dir, zdotdir, xdg_config_home)?,
    };

    let binary_path = resolve_stable_binary_path(opts.binary_path.as_deref(), Some(&data_dir))?;
    let env_script_path = match shell {
        ShellKind::Zsh | ShellKind::Bash => Some(data_dir.join(ENV_SCRIPT_FILENAME)),
        ShellKind::Fish => None,
    };

    Ok(ResolvedContext {
        shell,
        rc_path,
        env_script_path,
        binary_path,
        use_portable_env_expr,
        home: home_dir,
        zdotdir: zdotdir.map(Path::to_path_buf),
        xdg_config_home: xdg_config_home.map(Path::to_path_buf),
    })
}

fn render_block_for_context(ctx: &ResolvedContext) -> Result<String, CoreError> {
    match ctx.shell {
        ShellKind::Zsh | ShellKind::Bash => {
            let env_path = ctx
                .env_script_path
                .as_deref()
                .expect("posix shell always has env_script_path");
            render_posix_rc_snippet_internal(env_path, ctx.use_portable_env_expr)
        }
        ShellKind::Fish => {
            let body = render_fish_dropin(&ctx.binary_path)?;
            Ok(wrap_with_markers(&body))
        }
    }
}

fn env_script_still_referenced_elsewhere(
    ctx: &ResolvedContext,
    excluded_rc: &Path,
) -> Result<bool, CoreError> {
    if !ctx.use_portable_env_expr {
        return Ok(false);
    }
    let candidates = collect_known_rc_paths(
        &ctx.home,
        ctx.zdotdir.as_deref(),
        ctx.xdg_config_home.as_deref(),
        false,
    )?;
    for path in candidates {
        if path == excluded_rc {
            continue;
        }
        match inspect_rc_file(&path) {
            Ok(RcInspection::Regular {
                markers: MarkerState::Valid { .. },
                ..
            })
            | Ok(RcInspection::Symlink {
                target:
                    SymlinkTargetKind::Regular {
                        markers: MarkerState::Valid { .. },
                        ..
                    },
                ..
            }) => return Ok(true),
            Ok(_) => {}
            Err(_) => {
                // Do not let an unrelated corrupt/unreadable shell RC prevent disabling this shell.
            }
        }
    }
    Ok(false)
}

fn remove_env_script_if_unused(ctx: &ResolvedContext) -> Result<(), CoreError> {
    let Some(env_path) = ctx.env_script_path.as_deref() else {
        return Ok(());
    };
    validate_env_script_dir_chain(ctx)?;
    if env_script_still_referenced_elsewhere(ctx, &ctx.rc_path)? {
        return Ok(());
    }
    let check_path: PathBuf = env_path.components().collect();
    match fs::symlink_metadata(&check_path) {
        Ok(meta) if meta.file_type().is_symlink() => Err(CoreError::InvalidPath {
            reason: format!(
                "symbolic links are not allowed for env script '{}'",
                env_path.display()
            ),
        }),
        Ok(meta) if !meta.file_type().is_file() => Err(CoreError::InvalidPath {
            reason: format!("env script '{}' must be a regular file", env_path.display()),
        }),
        Ok(_) => fs::remove_file(&check_path).map_err(|source| CoreError::Io {
            path: env_path.to_path_buf(),
            source,
        }),
        Err(err) if err.kind() == ErrorKind::NotFound => Ok(()),
        Err(source) => Err(CoreError::Io {
            path: env_path.to_path_buf(),
            source,
        }),
    }
}

struct ProcessEnvInputs {
    home: Option<PathBuf>,
    zdotdir: Option<PathBuf>,
    xdg_config_home: Option<PathBuf>,
    shell_env: Option<String>,
}

fn read_process_env_inputs() -> Result<ProcessEnvInputs, CoreError> {
    let home = dirs::home_dir();
    let zdotdir = read_optional_env(ZDOTDIR_ENV_VAR)?.map(PathBuf::from);
    let xdg_config_home = read_optional_env(XDG_CONFIG_HOME_ENV_VAR)?.map(PathBuf::from);
    let shell_env = std::env::var(SHELL_ENV_VAR).ok();
    Ok(ProcessEnvInputs {
        home,
        zdotdir,
        xdg_config_home,
        shell_env,
    })
}

const fn is_platform_supported() -> bool {
    !cfg!(windows)
}

pub fn status(opts: &IntegrationOptions) -> Result<IntegrationStatus, CoreError> {
    let env = read_process_env_inputs()?;
    status_with_env(
        opts,
        env.home.as_deref(),
        env.zdotdir.as_deref(),
        env.xdg_config_home.as_deref(),
        env.shell_env.as_deref(),
        None,
        detect_hybrid_active(),
        is_platform_supported(),
    )
}

pub fn enable(opts: &IntegrationOptions) -> Result<IntegrationStatus, CoreError> {
    let env = read_process_env_inputs()?;
    enable_with_env(
        opts,
        env.home.as_deref(),
        env.zdotdir.as_deref(),
        env.xdg_config_home.as_deref(),
        env.shell_env.as_deref(),
        None,
        detect_hybrid_active(),
        is_platform_supported(),
    )
}

pub fn disable(opts: &IntegrationOptions) -> Result<IntegrationStatus, CoreError> {
    let env = read_process_env_inputs()?;
    disable_with_env(
        opts,
        env.home.as_deref(),
        env.zdotdir.as_deref(),
        env.xdg_config_home.as_deref(),
        env.shell_env.as_deref(),
        None,
        detect_hybrid_active(),
        is_platform_supported(),
    )
}

#[allow(clippy::too_many_arguments)]
fn status_with_env(
    opts: &IntegrationOptions,
    home: Option<&Path>,
    zdotdir: Option<&Path>,
    xdg_config_home: Option<&Path>,
    shell_env: Option<&str>,
    default_data_override: Option<&Path>,
    hybrid_active: bool,
    platform_supported: bool,
) -> Result<IntegrationStatus, CoreError> {
    if !platform_supported {
        return Ok(build_unsupported_status(hybrid_active));
    }

    let ctx = resolve_context(
        opts,
        home,
        zdotdir,
        xdg_config_home,
        shell_env,
        default_data_override,
    )?;
    validate_env_script_dir_chain(&ctx)?;
    let snippet = render_block_for_context(&ctx)?;
    let env_exists = ctx
        .env_script_path
        .as_deref()
        .is_some_and(is_regular_file_existing);

    if ctx.shell == ShellKind::Fish && opts.rc_file.is_none() {
        let (fish_root, fish_rel_dirs) =
            default_fish_dir_chain(&ctx.home, ctx.xdg_config_home.as_deref())?;
        match inspect_dir_chain(&fish_root, fish_rel_dirs)? {
            DirChainInspection::SafeDir | DirChainInspection::Missing => {}
            DirChainInspection::NotDirectory { path } => {
                return Err(CoreError::InvalidPath {
                    reason: format!(
                        "intermediate directory '{}' for '{}' is not a directory",
                        path.display(),
                        ctx.rc_path.display()
                    ),
                });
            }
            DirChainInspection::Symlink {
                symlink_dir,
                link_target,
            } => {
                let has_markers =
                    match inspect_symlinked_dir_chain_target(&fish_root, fish_rel_dirs)? {
                        DirTargetState::Ready => matches!(
                            inspect_rc_file(&ctx.rc_path)?,
                            RcInspection::Regular {
                                markers: MarkerState::Valid { .. },
                                ..
                            } | RcInspection::Symlink {
                                target: SymlinkTargetKind::Regular {
                                    markers: MarkerState::Valid { .. },
                                    ..
                                },
                                ..
                            }
                        ),
                        DirTargetState::Missing | DirTargetState::NotDirectory { .. } => false,
                    };
                let enabled = has_markers;
                let manual_required = !has_markers;
                let symlink_target = link_target.or_else(|| fs::canonicalize(&symlink_dir).ok());
                return Ok(IntegrationStatus {
                    supported: true,
                    enabled,
                    manual_required,
                    shell: Some(ctx.shell),
                    rc_path: Some(ctx.rc_path),
                    env_script_path: ctx.env_script_path,
                    binary_path: Some(ctx.binary_path),
                    symlink_target,
                    manual_snippet: manual_required.then_some(snippet),
                    hybrid_active,
                    message: manual_required.then(|| {
                        "Fish configuration directory is a symlink (likely managed by Nix, Stow, or Chezmoi); add the snippet manually."
                            .to_string()
                    }),
                });
            }
        }
    }

    match inspect_rc_file(&ctx.rc_path)? {
        RcInspection::Missing => Ok(IntegrationStatus {
            supported: true,
            enabled: false,
            manual_required: false,
            shell: Some(ctx.shell),
            rc_path: Some(ctx.rc_path),
            env_script_path: ctx.env_script_path,
            binary_path: Some(ctx.binary_path),
            symlink_target: None,
            manual_snippet: None,
            hybrid_active,
            message: None,
        }),
        RcInspection::Regular { file, markers } => {
            let has_markers = matches!(markers, MarkerState::Valid { .. });
            let enabled = match ctx.shell {
                ShellKind::Zsh | ShellKind::Bash => has_markers && env_exists,
                ShellKind::Fish => has_markers,
            };
            let manual_required = file.readonly && !has_markers && env_exists;
            Ok(IntegrationStatus {
                supported: true,
                enabled,
                manual_required,
                shell: Some(ctx.shell),
                rc_path: Some(ctx.rc_path),
                env_script_path: ctx.env_script_path,
                binary_path: Some(ctx.binary_path),
                symlink_target: None,
                manual_snippet: manual_required.then_some(snippet),
                hybrid_active,
                message: manual_required.then(|| {
                    "RC file is read-only; add the snippet manually to finish enabling auto-start."
                        .to_string()
                }),
            })
        }
        RcInspection::Symlink {
            link_target,
            target,
        } => {
            let (canonical_target, has_markers) = match target {
                SymlinkTargetKind::Regular {
                    canonical, markers, ..
                } => (
                    Some(canonical),
                    matches!(markers, MarkerState::Valid { .. }),
                ),
                SymlinkTargetKind::Dangling | SymlinkTargetKind::NonRegular => (None, false),
            };
            let enabled = match ctx.shell {
                ShellKind::Zsh | ShellKind::Bash => has_markers && env_exists,
                ShellKind::Fish => has_markers,
            };
            let manual_required = match ctx.shell {
                ShellKind::Zsh | ShellKind::Bash => !has_markers && env_exists,
                ShellKind::Fish => !has_markers,
            };
            let symlink_target = link_target.or(canonical_target);
            Ok(IntegrationStatus {
                supported: true,
                enabled,
                manual_required,
                shell: Some(ctx.shell),
                rc_path: Some(ctx.rc_path),
                env_script_path: ctx.env_script_path,
                binary_path: Some(ctx.binary_path),
                symlink_target,
                manual_snippet: manual_required.then_some(snippet),
                hybrid_active,
                message: manual_required.then(|| {
                    "RC file is a symlink (likely managed by Nix, Stow, or Chezmoi); add the snippet manually."
                        .to_string()
                }),
            })
        }
    }
}

fn ensure_env_script(ctx: &ResolvedContext) -> Result<(), CoreError> {
    validate_env_script_dir_chain(ctx)?;
    if let Some(env_path) = ctx.env_script_path.as_deref() {
        write_env_script(env_path, &ctx.binary_path)?;
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn enable_with_env(
    opts: &IntegrationOptions,
    home: Option<&Path>,
    zdotdir: Option<&Path>,
    xdg_config_home: Option<&Path>,
    shell_env: Option<&str>,
    default_data_override: Option<&Path>,
    hybrid_active: bool,
    platform_supported: bool,
) -> Result<IntegrationStatus, CoreError> {
    if !platform_supported {
        return Ok(build_unsupported_status(hybrid_active));
    }

    let ctx = resolve_context(
        opts,
        home,
        zdotdir,
        xdg_config_home,
        shell_env,
        default_data_override,
    )?;
    validate_env_script_dir_chain(&ctx)?;

    // Inspect RC and render snippet BEFORE writing env.sh so fail-closed errors
    // (corrupted markers, FIFO/oversized/non-UTF-8 RC, unfollowable symlink) never
    // leave an orphan env.sh on disk.
    let snippet = render_block_for_context(&ctx)?;
    let mut followed_dir_symlink_target: Option<PathBuf> = None;
    if ctx.shell == ShellKind::Fish && opts.rc_file.is_none() {
        let (fish_root, fish_rel_dirs) =
            default_fish_dir_chain(&ctx.home, ctx.xdg_config_home.as_deref())?;
        match inspect_dir_chain(&fish_root, fish_rel_dirs)? {
            DirChainInspection::SafeDir | DirChainInspection::Missing => {}
            DirChainInspection::NotDirectory { path } => {
                return Err(CoreError::InvalidPath {
                    reason: format!(
                        "intermediate directory '{}' for '{}' is not a directory",
                        path.display(),
                        ctx.rc_path.display()
                    ),
                });
            }
            DirChainInspection::Symlink {
                symlink_dir,
                link_target,
            } => {
                let target_display = link_target.or_else(|| fs::canonicalize(&symlink_dir).ok());
                if !opts.follow_rc_symlink {
                    let (existing_content, has_valid_markers) =
                        match inspect_symlinked_dir_chain_target(&fish_root, fish_rel_dirs)? {
                            DirTargetState::Ready => match inspect_rc_file(&ctx.rc_path)? {
                                RcInspection::Regular {
                                    file,
                                    markers: MarkerState::Valid { .. },
                                } => (Some(file.content), true),
                                RcInspection::Symlink {
                                    target:
                                        SymlinkTargetKind::Regular {
                                            file,
                                            markers: MarkerState::Valid { .. },
                                            ..
                                        },
                                    ..
                                } => (Some(file.content), true),
                                _ => (None, false),
                            },
                            DirTargetState::Missing | DirTargetState::NotDirectory { .. } => {
                                (None, false)
                            }
                        };
                    if has_valid_markers {
                        let fish_needs_update = existing_content
                            .as_deref()
                            .is_some_and(|content| !content.contains(&snippet));
                        return Ok(IntegrationStatus {
                            supported: true,
                            enabled: true,
                            manual_required: fish_needs_update,
                            shell: Some(ctx.shell),
                            rc_path: Some(ctx.rc_path),
                            env_script_path: ctx.env_script_path,
                            binary_path: Some(ctx.binary_path),
                            symlink_target: target_display,
                            manual_snippet: fish_needs_update.then_some(snippet),
                            hybrid_active,
                            message: fish_needs_update.then(|| {
                                "Fish configuration directory is a symlink; existing hook could not be updated automatically."
                                    .to_string()
                            }),
                        });
                    }
                    return Ok(IntegrationStatus {
                        supported: true,
                        enabled: false,
                        manual_required: true,
                        shell: Some(ctx.shell),
                        rc_path: Some(ctx.rc_path),
                        env_script_path: ctx.env_script_path,
                        binary_path: Some(ctx.binary_path),
                        symlink_target: target_display,
                        manual_snippet: Some(snippet),
                        hybrid_active,
                        message: Some(
                            "Fish configuration directory is a symlink; left untouched. Add the snippet manually or re-run with --follow-rc-symlink."
                                .to_string(),
                        ),
                    });
                }

                let sym_check: PathBuf = symlink_dir.components().collect();
                match fs::metadata(&sym_check) {
                    Ok(meta) if meta.is_dir() => {}
                    Ok(_) => {
                        return Err(CoreError::InvalidPath {
                            reason: format!(
                                "Fish directory symlink '{}' does not point to a directory",
                                symlink_dir.display()
                            ),
                        });
                    }
                    Err(err) if err.kind() == ErrorKind::NotFound => {
                        return Err(CoreError::InvalidPath {
                            reason: format!(
                                "cannot follow dangling Fish directory symlink '{}'",
                                symlink_dir.display()
                            ),
                        });
                    }
                    Err(source) => {
                        return Err(CoreError::Io {
                            path: symlink_dir,
                            source,
                        });
                    }
                }
                if let DirTargetState::NotDirectory { path } =
                    inspect_symlinked_dir_chain_target(&fish_root, fish_rel_dirs)?
                {
                    return Err(CoreError::InvalidPath {
                        reason: format!(
                            "intermediate directory '{}' for '{}' is not a directory",
                            path.display(),
                            ctx.rc_path.display()
                        ),
                    });
                }
                followed_dir_symlink_target = fs::canonicalize(&sym_check).ok().or(target_display);
            }
        }
    }
    let inspection = inspect_rc_file(&ctx.rc_path)?;

    match inspection {
        RcInspection::Symlink {
            link_target,
            target,
        } => {
            if !opts.follow_rc_symlink {
                ensure_env_script(&ctx)?;
                let (canonical_target, file, markers) = match target {
                    SymlinkTargetKind::Regular {
                        canonical,
                        file,
                        markers,
                    } => (Some(canonical), Some(file), markers),
                    SymlinkTargetKind::Dangling | SymlinkTargetKind::NonRegular => {
                        (None, None, MarkerState::Absent)
                    }
                };
                let target_display = link_target.or(canonical_target);
                if matches!(markers, MarkerState::Valid { .. }) {
                    let fish_needs_update = ctx.shell == ShellKind::Fish
                        && file
                            .as_ref()
                            .is_some_and(|loaded| !loaded.content.contains(&snippet));
                    return Ok(IntegrationStatus {
                        supported: true,
                        enabled: true,
                        manual_required: fish_needs_update,
                        shell: Some(ctx.shell),
                        rc_path: Some(ctx.rc_path),
                        env_script_path: ctx.env_script_path,
                        binary_path: Some(ctx.binary_path),
                        symlink_target: target_display,
                        manual_snippet: fish_needs_update.then_some(snippet),
                        hybrid_active,
                        message: fish_needs_update.then(|| {
                            "Fish drop-in symlink already contains a hook but could not be updated automatically."
                                .to_string()
                        }),
                    });
                }
                return Ok(IntegrationStatus {
                    supported: true,
                    enabled: false,
                    manual_required: true,
                    shell: Some(ctx.shell),
                    rc_path: Some(ctx.rc_path),
                    env_script_path: ctx.env_script_path,
                    binary_path: Some(ctx.binary_path),
                    symlink_target: target_display,
                    manual_snippet: Some(snippet),
                    hybrid_active,
                    message: Some(
                        "RC file is a symlink; left untouched. Add the snippet manually or re-run with --follow-rc-symlink."
                            .to_string(),
                    ),
                });
            }

            let (canonical, loaded, markers) = match target {
                SymlinkTargetKind::Regular {
                    canonical,
                    file,
                    markers,
                } => (canonical, file, markers),
                SymlinkTargetKind::Dangling => {
                    return Err(CoreError::InvalidPath {
                        reason: format!(
                            "cannot follow dangling RC symlink '{}'",
                            ctx.rc_path.display()
                        ),
                    });
                }
                SymlinkTargetKind::NonRegular => {
                    return Err(CoreError::InvalidPath {
                        reason: format!(
                            "RC symlink '{}' does not point to a regular file",
                            ctx.rc_path.display()
                        ),
                    });
                }
            };
            validate_path(&canonical)?;
            if loaded.readonly {
                ensure_env_script(&ctx)?;
                return Ok(IntegrationStatus {
                    supported: true,
                    enabled: matches!(markers, MarkerState::Valid { .. }),
                    manual_required: true,
                    shell: Some(ctx.shell),
                    rc_path: Some(ctx.rc_path),
                    env_script_path: ctx.env_script_path,
                    binary_path: Some(ctx.binary_path),
                    symlink_target: Some(canonical),
                    manual_snippet: Some(snippet),
                    hybrid_active,
                    message: Some(
                        "Canonical RC symlink target is read-only; left untouched. Add the snippet manually."
                            .to_string(),
                    ),
                });
            }

            let updated = upsert_marker_block(&canonical, &loaded.content, &snippet)?;
            ensure_env_script(&ctx)?;
            match write_file_atomic_with_mode(
                &canonical,
                updated.as_bytes(),
                loaded.unix_mode,
                DEFAULT_RC_DIR_MODE,
                false,
                true,
                true,
            ) {
                Ok(()) => Ok(IntegrationStatus {
                    supported: true,
                    enabled: true,
                    manual_required: false,
                    shell: Some(ctx.shell),
                    rc_path: Some(ctx.rc_path),
                    env_script_path: ctx.env_script_path,
                    binary_path: Some(ctx.binary_path),
                    symlink_target: Some(canonical),
                    manual_snippet: None,
                    hybrid_active,
                    message: None,
                }),
                Err(CoreError::Io { source, .. })
                    if source.kind() == ErrorKind::PermissionDenied =>
                {
                    Ok(IntegrationStatus {
                        supported: true,
                        enabled: matches!(markers, MarkerState::Valid { .. }),
                        manual_required: true,
                        shell: Some(ctx.shell),
                        rc_path: Some(ctx.rc_path),
                        env_script_path: ctx.env_script_path,
                        binary_path: Some(ctx.binary_path),
                        symlink_target: Some(canonical),
                        manual_snippet: Some(snippet),
                        hybrid_active,
                        message: Some(
                            "Canonical RC symlink target is not writable; left untouched. Add the snippet manually."
                                .to_string(),
                        ),
                    })
                }
                Err(err) => Err(err),
            }
        }
        RcInspection::Regular { file, markers } => {
            if file.readonly {
                ensure_env_script(&ctx)?;
                let already_present = matches!(markers, MarkerState::Valid { .. });
                let manual_required = !already_present
                    || (ctx.shell == ShellKind::Fish && !file.content.contains(&snippet));
                return Ok(IntegrationStatus {
                    supported: true,
                    enabled: already_present,
                    manual_required,
                    shell: Some(ctx.shell),
                    rc_path: Some(ctx.rc_path),
                    env_script_path: ctx.env_script_path,
                    binary_path: Some(ctx.binary_path),
                    symlink_target: followed_dir_symlink_target,
                    manual_snippet: manual_required.then_some(snippet),
                    hybrid_active,
                    message: manual_required.then(|| {
                        "RC file is read-only; left untouched. Add the snippet manually."
                            .to_string()
                    }),
                });
            }

            let updated = upsert_marker_block(&ctx.rc_path, &file.content, &snippet)?;
            ensure_env_script(&ctx)?;
            match write_file_atomic_with_mode(
                &ctx.rc_path,
                updated.as_bytes(),
                file.unix_mode,
                DEFAULT_RC_DIR_MODE,
                false,
                true,
                true,
            ) {
                Ok(()) => Ok(IntegrationStatus {
                    supported: true,
                    enabled: true,
                    manual_required: false,
                    shell: Some(ctx.shell),
                    rc_path: Some(ctx.rc_path),
                    env_script_path: ctx.env_script_path,
                    binary_path: Some(ctx.binary_path),
                    symlink_target: followed_dir_symlink_target,
                    manual_snippet: None,
                    hybrid_active,
                    message: None,
                }),
                Err(CoreError::Io { source, .. })
                    if source.kind() == ErrorKind::PermissionDenied =>
                {
                    Ok(IntegrationStatus {
                        supported: true,
                        enabled: matches!(markers, MarkerState::Valid { .. }),
                        manual_required: true,
                        shell: Some(ctx.shell),
                        rc_path: Some(ctx.rc_path),
                        env_script_path: ctx.env_script_path,
                        binary_path: Some(ctx.binary_path),
                        symlink_target: followed_dir_symlink_target,
                        manual_snippet: Some(snippet),
                        hybrid_active,
                        message: Some(
                            "RC file directory is not writable; left untouched. Add the snippet manually."
                                .to_string(),
                        ),
                    })
                }
                Err(err) => Err(err),
            }
        }
        RcInspection::Missing => {
            let updated = upsert_marker_block(&ctx.rc_path, "", &snippet)?;
            ensure_env_script(&ctx)?;
            match write_file_atomic_with_mode(
                &ctx.rc_path,
                updated.as_bytes(),
                DEFAULT_RC_FILE_MODE,
                DEFAULT_RC_DIR_MODE,
                false,
                true,
                true,
            ) {
                Ok(()) => Ok(IntegrationStatus {
                    supported: true,
                    enabled: true,
                    manual_required: false,
                    shell: Some(ctx.shell),
                    rc_path: Some(ctx.rc_path),
                    env_script_path: ctx.env_script_path,
                    binary_path: Some(ctx.binary_path),
                    symlink_target: followed_dir_symlink_target,
                    manual_snippet: None,
                    hybrid_active,
                    message: None,
                }),
                Err(CoreError::Io { source, .. })
                    if source.kind() == ErrorKind::PermissionDenied =>
                {
                    Ok(IntegrationStatus {
                        supported: true,
                        enabled: false,
                        manual_required: true,
                        shell: Some(ctx.shell),
                        rc_path: Some(ctx.rc_path),
                        env_script_path: ctx.env_script_path,
                        binary_path: Some(ctx.binary_path),
                        symlink_target: followed_dir_symlink_target,
                        manual_snippet: Some(snippet),
                        hybrid_active,
                        message: Some(
                            "RC file parent directory is not writable; add the snippet manually."
                                .to_string(),
                        ),
                    })
                }
                Err(err) => Err(err),
            }
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn disable_with_env(
    opts: &IntegrationOptions,
    home: Option<&Path>,
    zdotdir: Option<&Path>,
    xdg_config_home: Option<&Path>,
    shell_env: Option<&str>,
    default_data_override: Option<&Path>,
    hybrid_active: bool,
    platform_supported: bool,
) -> Result<IntegrationStatus, CoreError> {
    if !platform_supported {
        return Ok(build_unsupported_status(hybrid_active));
    }

    let ctx = resolve_context(
        opts,
        home,
        zdotdir,
        xdg_config_home,
        shell_env,
        default_data_override,
    )?;
    validate_env_script_dir_chain(&ctx)?;

    let mut followed_dir_symlink_target: Option<PathBuf> = None;
    if ctx.shell == ShellKind::Fish && opts.rc_file.is_none() {
        let (fish_root, fish_rel_dirs) =
            default_fish_dir_chain(&ctx.home, ctx.xdg_config_home.as_deref())?;
        match inspect_dir_chain(&fish_root, fish_rel_dirs)? {
            DirChainInspection::SafeDir | DirChainInspection::Missing => {}
            DirChainInspection::NotDirectory { path } => {
                return Err(CoreError::InvalidPath {
                    reason: format!(
                        "intermediate directory '{}' for '{}' is not a directory",
                        path.display(),
                        ctx.rc_path.display()
                    ),
                });
            }
            DirChainInspection::Symlink {
                symlink_dir,
                link_target,
            } => {
                let target_display = link_target.or_else(|| fs::canonicalize(&symlink_dir).ok());
                match inspect_symlinked_dir_chain_target(&fish_root, fish_rel_dirs)? {
                    DirTargetState::Missing | DirTargetState::NotDirectory { .. } => {
                        return Ok(IntegrationStatus {
                            supported: true,
                            enabled: false,
                            manual_required: false,
                            shell: Some(ctx.shell),
                            rc_path: Some(ctx.rc_path),
                            env_script_path: ctx.env_script_path,
                            binary_path: Some(ctx.binary_path),
                            symlink_target: target_display,
                            manual_snippet: None,
                            hybrid_active,
                            message: None,
                        });
                    }
                    DirTargetState::Ready => {
                        if !opts.follow_rc_symlink {
                            let has_valid_markers = matches!(
                                inspect_rc_file(&ctx.rc_path)?,
                                RcInspection::Regular {
                                    markers: MarkerState::Valid { .. },
                                    ..
                                } | RcInspection::Symlink {
                                    target: SymlinkTargetKind::Regular {
                                        markers: MarkerState::Valid { .. },
                                        ..
                                    },
                                    ..
                                }
                            );
                            if !has_valid_markers {
                                return Ok(IntegrationStatus {
                                    supported: true,
                                    enabled: false,
                                    manual_required: false,
                                    shell: Some(ctx.shell),
                                    rc_path: Some(ctx.rc_path),
                                    env_script_path: ctx.env_script_path,
                                    binary_path: Some(ctx.binary_path),
                                    symlink_target: target_display,
                                    manual_snippet: None,
                                    hybrid_active,
                                    message: None,
                                });
                            }
                            return Ok(IntegrationStatus {
                                supported: true,
                                enabled: true,
                                manual_required: true,
                                shell: Some(ctx.shell),
                                rc_path: Some(ctx.rc_path),
                                env_script_path: ctx.env_script_path,
                                binary_path: Some(ctx.binary_path),
                                symlink_target: target_display,
                                manual_snippet: None,
                                hybrid_active,
                                message: Some(
                                    "Fish configuration directory is a symlink and was left untouched. Remove the hook manually or re-run with --follow-rc-symlink."
                                        .to_string(),
                                ),
                            });
                        }
                        let sym_check: PathBuf = symlink_dir.components().collect();
                        followed_dir_symlink_target =
                            fs::canonicalize(&sym_check).ok().or(target_display);
                    }
                }
            }
        }
    }

    match inspect_rc_file(&ctx.rc_path)? {
        RcInspection::Missing => {
            remove_env_script_if_unused(&ctx)?;
            Ok(IntegrationStatus {
                supported: true,
                enabled: false,
                manual_required: false,
                shell: Some(ctx.shell),
                rc_path: Some(ctx.rc_path),
                env_script_path: ctx.env_script_path,
                binary_path: Some(ctx.binary_path),
                symlink_target: followed_dir_symlink_target,
                manual_snippet: None,
                hybrid_active,
                message: None,
            })
        }
        RcInspection::Symlink {
            link_target,
            target,
        } => match target {
            SymlinkTargetKind::Dangling | SymlinkTargetKind::NonRegular => {
                remove_env_script_if_unused(&ctx)?;
                Ok(IntegrationStatus {
                    supported: true,
                    enabled: false,
                    manual_required: false,
                    shell: Some(ctx.shell),
                    rc_path: Some(ctx.rc_path),
                    env_script_path: ctx.env_script_path,
                    binary_path: Some(ctx.binary_path),
                    symlink_target: link_target,
                    manual_snippet: None,
                    hybrid_active,
                    message: None,
                })
            }
            SymlinkTargetKind::Regular {
                canonical,
                file: loaded,
                markers,
            } => {
                let target_display = link_target.or_else(|| Some(canonical.clone()));
                if !matches!(markers, MarkerState::Valid { .. }) {
                    remove_env_script_if_unused(&ctx)?;
                    return Ok(IntegrationStatus {
                        supported: true,
                        enabled: false,
                        manual_required: false,
                        shell: Some(ctx.shell),
                        rc_path: Some(ctx.rc_path),
                        env_script_path: ctx.env_script_path,
                        binary_path: Some(ctx.binary_path),
                        symlink_target: target_display,
                        manual_snippet: None,
                        hybrid_active,
                        message: None,
                    });
                }

                if !opts.follow_rc_symlink {
                    remove_env_script_if_unused(&ctx)?;
                    let still_enabled = ctx.shell == ShellKind::Fish;
                    return Ok(IntegrationStatus {
                        supported: true,
                        enabled: still_enabled,
                        manual_required: true,
                        shell: Some(ctx.shell),
                        rc_path: Some(ctx.rc_path),
                        env_script_path: ctx.env_script_path,
                        binary_path: Some(ctx.binary_path),
                        symlink_target: target_display,
                        manual_snippet: None,
                        hybrid_active,
                        message: Some(if ctx.shell == ShellKind::Fish {
                            "Fish drop-in is a symlink and was left untouched. Remove the hook manually or re-run with --follow-rc-symlink."
                                .to_string()
                        } else {
                            "Removed env.sh (neutralizing the hook), but left the symlinked RC file untouched. Remove the managed lines manually or re-run with --follow-rc-symlink."
                                .to_string()
                        }),
                    });
                }

                validate_path(&canonical)?;
                if loaded.readonly {
                    remove_env_script_if_unused(&ctx)?;
                    return Ok(IntegrationStatus {
                        supported: true,
                        enabled: ctx.shell == ShellKind::Fish,
                        manual_required: true,
                        shell: Some(ctx.shell),
                        rc_path: Some(ctx.rc_path),
                        env_script_path: ctx.env_script_path,
                        binary_path: Some(ctx.binary_path),
                        symlink_target: Some(canonical),
                        manual_snippet: None,
                        hybrid_active,
                        message: Some(
                            "Canonical RC symlink target is read-only; left untouched.".to_string(),
                        ),
                    });
                }

                let (stripped, _) = strip_marker_block(&canonical, &loaded.content)?;
                write_file_atomic_with_mode(
                    &canonical,
                    stripped.as_bytes(),
                    loaded.unix_mode,
                    DEFAULT_RC_DIR_MODE,
                    false,
                    true,
                    true,
                )?;
                remove_env_script_if_unused(&ctx)?;
                Ok(IntegrationStatus {
                    supported: true,
                    enabled: false,
                    manual_required: false,
                    shell: Some(ctx.shell),
                    rc_path: Some(ctx.rc_path),
                    env_script_path: ctx.env_script_path,
                    binary_path: Some(ctx.binary_path),
                    symlink_target: Some(canonical),
                    manual_snippet: None,
                    hybrid_active,
                    message: None,
                })
            }
        },
        RcInspection::Regular { file, markers } => {
            if !matches!(markers, MarkerState::Valid { .. }) {
                remove_env_script_if_unused(&ctx)?;
                return Ok(IntegrationStatus {
                    supported: true,
                    enabled: false,
                    manual_required: false,
                    shell: Some(ctx.shell),
                    rc_path: Some(ctx.rc_path),
                    env_script_path: ctx.env_script_path,
                    binary_path: Some(ctx.binary_path),
                    symlink_target: followed_dir_symlink_target,
                    manual_snippet: None,
                    hybrid_active,
                    message: None,
                });
            }

            if file.readonly {
                remove_env_script_if_unused(&ctx)?;
                return Ok(IntegrationStatus {
                    supported: true,
                    enabled: ctx.shell == ShellKind::Fish,
                    manual_required: true,
                    shell: Some(ctx.shell),
                    rc_path: Some(ctx.rc_path),
                    env_script_path: ctx.env_script_path,
                    binary_path: Some(ctx.binary_path),
                    symlink_target: followed_dir_symlink_target,
                    manual_snippet: None,
                    hybrid_active,
                    message: Some(
                        "RC file is read-only; left untouched. Remove the managed lines manually."
                            .to_string(),
                    ),
                });
            }

            let (stripped, _) = strip_marker_block(&ctx.rc_path, &file.content)?;
            if stripped.trim().is_empty() && is_deletable_when_empty(&ctx.rc_path) {
                match fs::remove_file(&ctx.rc_path) {
                    Ok(()) => {}
                    Err(err) if err.kind() == ErrorKind::NotFound => {}
                    Err(err) if err.kind() == ErrorKind::PermissionDenied => {
                        remove_env_script_if_unused(&ctx)?;
                        return Ok(IntegrationStatus {
                            supported: true,
                            enabled: ctx.shell == ShellKind::Fish,
                            manual_required: true,
                            shell: Some(ctx.shell),
                            rc_path: Some(ctx.rc_path),
                            env_script_path: ctx.env_script_path,
                            binary_path: Some(ctx.binary_path),
                            symlink_target: followed_dir_symlink_target,
                            manual_snippet: None,
                            hybrid_active,
                            message: Some(
                                "RC file could not be removed due to permissions; remove it manually."
                                    .to_string(),
                            ),
                        });
                    }
                    Err(source) => {
                        return Err(CoreError::Io {
                            path: ctx.rc_path,
                            source,
                        });
                    }
                }
            } else {
                match write_file_atomic_with_mode(
                    &ctx.rc_path,
                    stripped.as_bytes(),
                    file.unix_mode,
                    DEFAULT_RC_DIR_MODE,
                    false,
                    true,
                    true,
                ) {
                    Ok(()) => {}
                    Err(CoreError::Io { source, .. })
                        if source.kind() == ErrorKind::PermissionDenied =>
                    {
                        remove_env_script_if_unused(&ctx)?;
                        return Ok(IntegrationStatus {
                            supported: true,
                            enabled: ctx.shell == ShellKind::Fish,
                            manual_required: true,
                            shell: Some(ctx.shell),
                            rc_path: Some(ctx.rc_path),
                            env_script_path: ctx.env_script_path,
                            binary_path: Some(ctx.binary_path),
                            symlink_target: followed_dir_symlink_target,
                            manual_snippet: None,
                            hybrid_active,
                            message: Some(
                                "RC file directory is not writable; remove the managed lines manually."
                                    .to_string(),
                            ),
                        });
                    }
                    Err(err) => return Err(err),
                }
            }

            remove_env_script_if_unused(&ctx)?;
            Ok(IntegrationStatus {
                supported: true,
                enabled: false,
                manual_required: false,
                shell: Some(ctx.shell),
                rc_path: Some(ctx.rc_path),
                env_script_path: ctx.env_script_path,
                binary_path: Some(ctx.binary_path),
                symlink_target: followed_dir_symlink_target,
                manual_snippet: None,
                hybrid_active,
                message: None,
            })
        }
    }
}

fn collect_known_rc_paths(
    home: &Path,
    zdotdir: Option<&Path>,
    xdg_config_home: Option<&Path>,
    include_fish: bool,
) -> Result<Vec<PathBuf>, CoreError> {
    validate_path(home)?;
    let mut paths = Vec::new();
    let mut push_unique = |candidate: PathBuf| {
        if !paths.contains(&candidate) {
            paths.push(candidate);
        }
    };

    if let Some(zdir) = resolve_optional_absolute_dir(zdotdir)? {
        for name in [".zlogin", ".zprofile", ".zshrc"] {
            push_unique(zdir.join(name));
        }
    }

    for name in [
        ".zlogin",
        ".zprofile",
        ".zshrc",
        ".bash_profile",
        ".bash_login",
        ".profile",
    ] {
        push_unique(home.join(name));
    }

    if include_fish {
        let fish_base =
            resolve_optional_absolute_dir(xdg_config_home)?.unwrap_or_else(|| home.join(".config"));
        push_unique(
            fish_base
                .join("fish")
                .join("conf.d")
                .join(FISH_DROPIN_FILENAME),
        );
    }

    Ok(paths)
}

pub fn strip_all_known_rc_files(
    home: &Path,
    zdotdir: Option<&Path>,
    xdg_config_home: Option<&Path>,
    dry_run: bool,
) -> Result<StripReport, CoreError> {
    let candidates = collect_known_rc_paths(home, zdotdir, xdg_config_home, true)?;
    let (fish_root, fish_rel_dirs) = default_fish_dir_chain(home, xdg_config_home)?;
    let mut fish_dropin_path = fish_root.clone();
    for seg in fish_rel_dirs {
        fish_dropin_path.push(seg);
    }
    fish_dropin_path.push(FISH_DROPIN_FILENAME);
    let norm_fish_dropin: PathBuf = fish_dropin_path.components().collect();
    let mut report = StripReport::default();

    for path in candidates {
        if path.components().collect::<PathBuf>() == norm_fish_dropin {
            match inspect_dir_chain(&fish_root, fish_rel_dirs) {
                Ok(DirChainInspection::SafeDir) => {}
                Ok(DirChainInspection::Missing)
                | Ok(DirChainInspection::NotDirectory { .. })
                | Err(_) => {
                    continue;
                }
                Ok(DirChainInspection::Symlink { .. }) => {
                    match inspect_symlinked_dir_chain_target(&fish_root, fish_rel_dirs) {
                        Ok(DirTargetState::Ready) => match inspect_rc_file(&path) {
                            Ok(RcInspection::Regular {
                                markers: MarkerState::Valid { .. },
                                ..
                            })
                            | Ok(RcInspection::Symlink {
                                target:
                                    SymlinkTargetKind::Regular {
                                        markers: MarkerState::Valid { .. },
                                        ..
                                    },
                                ..
                            }) => {
                                report.skipped_manual_files.push(path);
                            }
                            Ok(_) => {}
                            Err(_) => {
                                let check_path: PathBuf = path.components().collect();
                                if fs::symlink_metadata(&check_path).is_ok() {
                                    report.skipped_manual_files.push(path);
                                }
                            }
                        },
                        Ok(DirTargetState::Missing)
                        | Ok(DirTargetState::NotDirectory { .. })
                        | Err(_) => {}
                    }
                    continue;
                }
            }
        }

        let inspection = match inspect_rc_file(&path) {
            Ok(inspection) => inspection,
            Err(_) => {
                // Do not let an unrelated corrupt, oversized, non-UTF-8, FIFO, or
                // unreadable RC file abort cleaning the remaining known RC files.
                let check_path: PathBuf = path.components().collect();
                if fs::symlink_metadata(&check_path).is_ok() {
                    report.skipped_manual_files.push(path);
                }
                continue;
            }
        };
        match inspection {
            RcInspection::Missing => {}
            RcInspection::Symlink { target, .. } => {
                if matches!(
                    target,
                    SymlinkTargetKind::Regular {
                        markers: MarkerState::Valid { .. },
                        ..
                    }
                ) {
                    report.skipped_manual_files.push(path);
                }
            }
            RcInspection::Regular { file, markers } => {
                if !matches!(markers, MarkerState::Valid { .. }) {
                    continue;
                }
                if file.readonly {
                    report.skipped_manual_files.push(path);
                    continue;
                }
                let (stripped, changed) = strip_marker_block(&path, &file.content)?;
                if !changed {
                    continue;
                }
                let should_delete = stripped.trim().is_empty() && is_deletable_when_empty(&path);
                if dry_run {
                    if should_delete {
                        report.deleted_files.push(path);
                    } else {
                        report.modified_files.push(path);
                    }
                    continue;
                }

                if should_delete {
                    match fs::remove_file(&path) {
                        Ok(()) => report.deleted_files.push(path),
                        Err(err) if err.kind() == ErrorKind::PermissionDenied => {
                            report.skipped_manual_files.push(path);
                        }
                        Err(source) => return Err(CoreError::Io { path, source }),
                    }
                } else {
                    match write_file_atomic_with_mode(
                        &path,
                        stripped.as_bytes(),
                        file.unix_mode,
                        DEFAULT_RC_DIR_MODE,
                        false,
                        true,
                        true,
                    ) {
                        Ok(()) => report.modified_files.push(path),
                        Err(CoreError::Io { source, .. })
                            if source.kind() == ErrorKind::PermissionDenied =>
                        {
                            report.skipped_manual_files.push(path);
                        }
                        Err(err) => return Err(err),
                    }
                }
            }
        }
    }

    Ok(report)
}

#[cfg(test)]
mod tests {
    use super::*;
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
            Err(CoreError::InvalidPath { .. })
        ));
        assert!(matches!(
            resolve_rc_path(
                ShellKind::Zsh,
                &home,
                Some(Path::new("/tmp/bad\0zdot")),
                None
            ),
            Err(CoreError::InvalidPath { .. })
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
            Err(CoreError::InvalidPath { .. })
        ));
        assert!(matches!(
            resolve_rc_path(ShellKind::Fish, &home, None, Some(Path::new("/tmp/a\0b"))),
            Err(CoreError::InvalidPath { .. })
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

        for bad in ["/tmp/a\nb", "/tmp/a\rb", "/tmp/../escape", "/tmp/a\0b"] {
            assert!(
                matches!(
                    render_env_sh(Path::new(bad)),
                    Err(CoreError::InvalidPath { .. })
                ),
                "expected InvalidPath for {bad:?}"
            );
        }
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
            Err(CoreError::InvalidPath { .. })
        ));
        assert_eq!(fs::read_to_string(&real_target).unwrap(), "keep");

        // Symlink parent dir rejected
        let sym_parent = tmp.path().join("sym-parent");
        symlink(&data_dir, &sym_parent).unwrap();
        assert!(matches!(
            write_env_script(&sym_parent.join("env.sh"), &bin),
            Err(CoreError::InvalidPath { .. })
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
        assert!(matches!(res, Err(CoreError::InvalidPath { .. })));
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
            Some(&home),
            None,
            None,
            Some("/bin/zsh"),
            None,
            false,
            true,
        )
        .unwrap();
        assert!(st1.enabled);
        assert!(!st1.manual_required);
        let zlogin = home.join(".zlogin");
        assert!(zlogin.is_file());
        assert!(data.join("env.sh").is_file());
        let first_content = fs::read_to_string(&zlogin).unwrap();

        let st2 = enable_with_env(
            &opts,
            Some(&home),
            None,
            None,
            Some("/bin/zsh"),
            None,
            false,
            true,
        )
        .unwrap();
        assert!(st2.enabled);
        assert_eq!(fs::read_to_string(&zlogin).unwrap(), first_content);

        // Empty .zlogin deleted on disable + env.sh removed
        let st3 = disable_with_env(
            &opts,
            Some(&home),
            None,
            None,
            Some("/bin/zsh"),
            None,
            false,
            true,
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
            Some(&home),
            None,
            None,
            Some("/bin/zsh"),
            None,
            false,
            true,
        )
        .unwrap();
        assert!(
            fs::read_to_string(&zlogin)
                .unwrap()
                .contains(HOOK_BEGIN_MARKER)
        );
        disable_with_env(
            &opts,
            Some(&home),
            None,
            None,
            Some("/bin/zsh"),
            None,
            false,
            true,
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
            Some(&home),
            None,
            None,
            Some("/bin/zsh"),
            None,
            false,
            true,
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
            Some(&home),
            None,
            None,
            Some("/bin/zsh"),
            None,
            false,
            true,
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
            Some(&home),
            None,
            None,
            Some("/bin/zsh"),
            None,
            false,
            true,
        )
        .unwrap();
        let content = fs::read_to_string(&zlogin).unwrap();
        assert!(content.starts_with("export NO_NL=1\n"));
        assert!(!content.ends_with('\n'));
        assert!(content.ends_with(HOOK_END_MARKER));

        disable_with_env(
            &opts,
            Some(&home),
            None,
            None,
            Some("/bin/zsh"),
            None,
            false,
            true,
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
            Some(&home),
            None,
            None,
            Some("/bin/bash"),
            None,
            false,
            true,
        )
        .unwrap();
        assert_eq!(st.rc_path.as_deref(), Some(home.join(".profile").as_path()));
        assert!(!home.join(".bash_profile").exists());
        assert!(home.join(".profile").is_file());

        disable_with_env(
            &opts,
            Some(&home),
            None,
            None,
            Some("/bin/bash"),
            None,
            false,
            true,
        )
        .unwrap();
        // .profile is NOT deleted when empty (only .zlogin and agy-companion.fish are deleted)
        assert!(home.join(".profile").is_file());
        assert_eq!(fs::read_to_string(home.join(".profile")).unwrap(), "");

        // When .bash_profile exists, it takes precedence
        fs::write(home.join(".bash_profile"), "# existing\n").unwrap();
        let st2 = enable_with_env(
            &opts,
            Some(&home),
            None,
            None,
            Some("/bin/bash"),
            None,
            false,
            true,
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
            Some(&home),
            None,
            Some(&xdg),
            Some("/usr/bin/fish"),
            None,
            false,
            true,
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
            Some(&home),
            None,
            Some(&xdg),
            Some("/usr/bin/fish"),
            None,
            false,
            true,
        )
        .unwrap();
        assert!(!dropin.exists());

        // Extra custom line preserved on disable
        fs::create_dir_all(dropin.parent().unwrap()).unwrap();
        fs::write(&dropin, "# custom fish note\n").unwrap();
        enable_with_env(
            &opts,
            Some(&home),
            None,
            Some(&xdg),
            Some("/usr/bin/fish"),
            None,
            false,
            true,
        )
        .unwrap();
        disable_with_env(
            &opts,
            Some(&home),
            None,
            Some(&xdg),
            Some("/usr/bin/fish"),
            None,
            false,
            true,
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
            Some(&home),
            None,
            None,
            Some("/bin/zsh"),
            None,
            false,
            true,
        )
        .unwrap();
        assert!(!st.enabled);
        assert!(st.manual_required);
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

        // Subsequent status() preserves manual_required == true
        let polled = status_with_env(
            &opts,
            Some(&home),
            None,
            None,
            Some("/bin/zsh"),
            None,
            false,
            true,
        )
        .unwrap();
        assert!(polled.manual_required);
        assert!(polled.manual_snippet.is_some());

        // Dangling symlink also non-mutating manual_required
        fs::remove_file(&dotfiles_target).unwrap();
        let dangling_st = enable_with_env(
            &opts,
            Some(&home),
            None,
            None,
            Some("/bin/zsh"),
            None,
            false,
            true,
        )
        .unwrap();
        assert!(dangling_st.manual_required);
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
            Some(&home),
            None,
            None,
            Some("/bin/zsh"),
            None,
            false,
            true,
        )
        .unwrap();
        assert!(st.enabled);
        assert!(!st.manual_required);
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
                Some(&home),
                None,
                None,
                Some("/bin/zsh"),
                None,
                false,
                true
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
                Some(&home),
                None,
                None,
                Some("/bin/zsh"),
                None,
                false,
                true
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
            Some(&home),
            None,
            None,
            Some("/bin/zsh"),
            None,
            false,
            true,
        )
        .unwrap();
        assert!(!st.enabled);
        assert!(st.manual_required);
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
                Some(&home),
                None,
                None,
                Some("/bin/zsh"),
                None,
                false,
                true,
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
                Some(&home_clone),
                None,
                None,
                Some("/bin/zsh"),
                None,
                false,
                true,
            );
            let _ = tx.send(res);
        });
        let res = rx
            .recv_timeout(Duration::from_secs(1))
            .expect("FIFO RC must fail fast");
        assert!(matches!(res, Err(CoreError::InvalidPath { .. })));
        fs::remove_file(&fifo_rc).unwrap();

        // > 1 MiB RC file fails closed
        let huge = vec![b'a'; (MAX_RC_FILE_BYTES as usize) + 1];
        fs::write(&fifo_rc, huge).unwrap();
        let err = status_with_env(
            &opts,
            Some(&home),
            None,
            None,
            Some("/bin/zsh"),
            None,
            false,
            true,
        )
        .unwrap_err();
        assert!(matches!(err, CoreError::InvalidPath { .. }));
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
        let st = status_with_env(&opts, None, None, None, None, None, false, false).unwrap();
        assert!(!st.supported);
        assert!(!st.enabled);
        assert!(!st.manual_required);
        assert_eq!(st.state(), IntegrationState::Unsupported);

        let val = serde_json::to_value(&st).unwrap();
        let obj = val.as_object().unwrap();
        for key in [
            "supported",
            "enabled",
            "manual_required",
            "shell",
            "rc_path",
            "env_script_path",
            "binary_path",
            "symlink_target",
            "manual_snippet",
            "hybrid_active",
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
            None
        ));
        assert!(!detect_hybrid_active_from(
            Some("https://cloudcode-pa.googleapis.com"),
            Some("1")
        ));
        assert!(detect_hybrid_active_from(None, Some("1")));
        assert!(!detect_hybrid_active_from(None, None));
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
            resolve_stable_binary_path_with(Some(&custom), Some(&data_dir), || Ok(
                fallback_exe.clone()
            ))
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
        let base = IntegrationStatus {
            supported: true,
            enabled: false,
            manual_required: false,
            shell: Some(ShellKind::Zsh),
            rc_path: None,
            env_script_path: None,
            binary_path: None,
            symlink_target: None,
            manual_snippet: None,
            hybrid_active: false,
            message: None,
        };
        assert_eq!(base.state(), IntegrationState::Disabled);

        let mut active = base.clone();
        active.enabled = true;
        active.hybrid_active = true;
        assert_eq!(active.state(), IntegrationState::Active);

        let mut pending = base.clone();
        pending.enabled = true;
        assert_eq!(pending.state(), IntegrationState::Enabled);

        let mut manual = base.clone();
        manual.manual_required = true;
        assert_eq!(manual.state(), IntegrationState::ManualRequired);

        let mut unsupported = base;
        unsupported.supported = false;
        assert_eq!(unsupported.state(), IntegrationState::Unsupported);

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
            Some(&home),
            None,
            None,
            Some("/bin/zsh"),
            None,
            false,
            true,
        )
        .unwrap();
        assert!(!st_before.enabled);
        assert!(!st_before.manual_required);
        assert_eq!(st_before.symlink_target.as_deref(), Some(fifo.as_path()));

        // default enable writes 0600 env.sh and returns manual_required without opening FIFO
        let st_en = enable_with_env(
            &opts_default,
            Some(&home),
            None,
            None,
            Some("/bin/zsh"),
            None,
            false,
            true,
        )
        .unwrap();
        assert!(!st_en.enabled);
        assert!(st_en.manual_required);
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

        // status after enable returns manual_required: true without crashing
        let st_after = status_with_env(
            &opts_default,
            Some(&home),
            None,
            None,
            Some("/bin/zsh"),
            None,
            false,
            true,
        )
        .unwrap();
        assert!(!st_after.enabled);
        assert!(st_after.manual_required);
        assert!(st_after.manual_snippet.is_some());
        assert_eq!(st_after.symlink_target.as_deref(), Some(fifo.as_path()));

        // Fish symlink-to-FIFO status returns manual_required: true without crashing
        let fish_dropin = xdg.join("fish/conf.d/agy-companion.fish");
        fs::create_dir_all(fish_dropin.parent().unwrap()).unwrap();
        symlink(&fifo, &fish_dropin).unwrap();
        let fish_opts = make_opts(ShellKind::Fish, None, data.clone(), bin.clone(), false);
        let fish_st = status_with_env(
            &fish_opts,
            Some(&home),
            None,
            Some(&xdg),
            Some("/usr/bin/fish"),
            None,
            false,
            true,
        )
        .unwrap();
        assert!(!fish_st.enabled);
        assert!(fish_st.manual_required);
        assert_eq!(fish_st.symlink_target.as_deref(), Some(fifo.as_path()));

        // disable neutralizes env.sh cleanly without opening or mutating symlink target
        let st_dis = disable_with_env(
            &opts_default,
            Some(&home),
            None,
            None,
            Some("/bin/zsh"),
            None,
            false,
            true,
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
            Some(&home),
            None,
            None,
            Some("/bin/zsh"),
            None,
            false,
            true,
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
                Some(&home),
                None,
                None,
                Some("/bin/zsh"),
                None,
                false,
                true,
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
                Some(&home),
                None,
                None,
                Some("/bin/zsh"),
                None,
                false,
                true,
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
                Some(&home),
                None,
                None,
                Some("/bin/zsh"),
                None,
                false,
                true,
            )
            .unwrap_err();
            assert!(matches!(err, CoreError::InvalidPath { .. }));
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
                Some(&home),
                None,
                None,
                Some("/bin/zsh"),
                None,
                false,
                true,
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
            Some(&home),
            None,
            None,
            Some("/bin/zsh"),
            None,
            false,
            true,
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
            Some(&home),
            None,
            None,
            Some("/bin/zsh"),
            None,
            false,
            true,
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
            Some(&home),
            None,
            Some(&xdg),
            Some("/bin/zsh"),
            None,
            false,
            true,
        )
        .unwrap();
        let fish_opts = make_opts(ShellKind::Fish, None, data, bin, false);
        enable_with_env(
            &fish_opts,
            Some(&home),
            None,
            Some(&xdg),
            Some("/usr/bin/fish"),
            None,
            false,
            true,
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
    fn fish_intermediate_symlink_dirs_require_manual_without_touching_target_and_allow_opt_in_follow()
     {
        use std::os::unix::fs::symlink;

        // Helper exercising status/enable/disable on a symlinked Fish intermediate directory.
        let check_symlink_case = |label: &str,
                                  home: &Path,
                                  xdg: Option<&Path>,
                                  victim_dir: &Path,
                                  victim_dropin: &Path| {
            let data = home.parent().unwrap().join(format!("data_{label}"));
            let bin = home.parent().unwrap().join("agy-companion");
            let opts = make_opts(ShellKind::Fish, None, data.clone(), bin.clone(), false);

            // 1. Absent drop-in inside symlinked dir: status & enable report manual_required, touch nothing.
            let st = status_with_env(
                &opts,
                Some(home),
                None,
                xdg,
                Some("/usr/local/bin/fish"),
                None,
                false,
                true,
            )
            .unwrap();
            assert!(!st.enabled, "{label}: status must not report enabled");
            assert!(
                st.manual_required,
                "{label}: status must report manual_required for symlinked Fish dir"
            );
            assert!(
                st.symlink_target.is_some(),
                "{label}: status must report symlink_target"
            );
            assert!(
                st.manual_snippet.is_some(),
                "{label}: status must include manual_snippet"
            );

            let en = enable_with_env(
                &opts,
                Some(home),
                None,
                xdg,
                Some("/usr/local/bin/fish"),
                None,
                false,
                true,
            )
            .unwrap();
            assert!(!en.enabled, "{label}: enable must not report enabled");
            assert!(
                en.manual_required,
                "{label}: enable must report manual_required without --follow-rc-symlink"
            );
            assert!(
                en.symlink_target.is_some(),
                "{label}: enable must report symlink_target"
            );
            assert!(
                en.manual_snippet.is_some(),
                "{label}: enable must include manual_snippet"
            );
            assert!(
                !victim_dropin.exists(),
                "{label}: enable must NOT create agy-companion.fish inside symlink target"
            );

            // 2. Pre-existing drop-in with markers inside symlink target: disable (no follow) leaves untouched.
            fs::create_dir_all(victim_dropin.parent().unwrap()).unwrap();
            let existing =
                format!("# stow user note\n{HOOK_BEGIN_MARKER}\n# old hook\n{HOOK_END_MARKER}\n");
            fs::write(victim_dropin, &existing).unwrap();

            let st_existing = status_with_env(
                &opts,
                Some(home),
                None,
                xdg,
                Some("/usr/local/bin/fish"),
                None,
                false,
                true,
            )
            .unwrap();
            assert!(
                st_existing.enabled,
                "{label}: status should reflect existing valid markers read-only"
            );
            assert!(!st_existing.manual_required);

            let en_stale = enable_with_env(
                &opts,
                Some(home),
                None,
                xdg,
                Some("/usr/local/bin/fish"),
                None,
                false,
                true,
            )
            .unwrap();
            assert!(en_stale.enabled);
            assert!(
                en_stale.manual_required,
                "{label}: stale fish hook behind symlinked dir must require manual update"
            );
            assert_eq!(
                fs::read_to_string(victim_dropin).unwrap(),
                existing,
                "{label}: enable must NOT mutate existing hook inside symlink target"
            );

            let dis = disable_with_env(
                &opts,
                Some(home),
                None,
                xdg,
                Some("/usr/local/bin/fish"),
                None,
                false,
                true,
            )
            .unwrap();
            assert!(dis.enabled, "{label}: fish hook still active until removed");
            assert!(
                dis.manual_required,
                "{label}: disable must report manual_required without --follow-rc-symlink"
            );
            assert_eq!(
                fs::read_to_string(victim_dropin).unwrap(),
                existing,
                "{label}: disable must NOT mutate or delete file inside symlink target"
            );

            // 3. Explicit opt-in follow_rc_symlink = true allows updating and disabling.
            let follow_opts = make_opts(ShellKind::Fish, None, data, bin, true);
            let en_follow = enable_with_env(
                &follow_opts,
                Some(home),
                None,
                xdg,
                Some("/usr/local/bin/fish"),
                None,
                false,
                true,
            )
            .unwrap();
            assert!(en_follow.enabled, "{label}: follow enable must succeed");
            assert!(!en_follow.manual_required);
            let updated = fs::read_to_string(victim_dropin).unwrap();
            assert!(updated.contains("# stow user note"));
            assert!(updated.contains(HOOK_BEGIN_MARKER));
            assert!(updated.contains("hook-env --ensure-running --print-port"));

            let dis_follow = disable_with_env(
                &follow_opts,
                Some(home),
                None,
                xdg,
                Some("/usr/local/bin/fish"),
                None,
                false,
                true,
            )
            .unwrap();
            assert!(!dis_follow.enabled, "{label}: follow disable must succeed");
            assert!(!dis_follow.manual_required);
            assert_eq!(
                fs::read_to_string(victim_dropin).unwrap(),
                "# stow user note\n"
            );
            let _ = victim_dir;
        };

        // Case (a): XDG_CONFIG_HOME unset, $HOME/.config -> victim_cfg
        {
            let tmp = TempDir::new().unwrap();
            let home = tmp.path().join("home");
            let victim = tmp.path().join("victim_cfg");
            fs::create_dir_all(&home).unwrap();
            fs::create_dir_all(&victim).unwrap();
            symlink(&victim, home.join(".config")).unwrap();
            check_symlink_case(
                "dot_config",
                &home,
                None,
                &victim,
                &victim.join("fish/conf.d/agy-companion.fish"),
            );
        }

        // Case (b): XDG_CONFIG_HOME unset, $HOME/.config/fish -> victim_fish (Stow tree-folding)
        {
            let tmp = TempDir::new().unwrap();
            let home = tmp.path().join("home");
            let victim = tmp.path().join("victim_fish");
            fs::create_dir_all(home.join(".config")).unwrap();
            fs::create_dir_all(&victim).unwrap();
            symlink(&victim, home.join(".config/fish")).unwrap();
            check_symlink_case(
                "dot_config_fish",
                &home,
                None,
                &victim,
                &victim.join("conf.d/agy-companion.fish"),
            );
        }

        // Case (c): XDG_CONFIG_HOME unset, $HOME/.config/fish/conf.d -> victim_confd (Stow)
        {
            let tmp = TempDir::new().unwrap();
            let home = tmp.path().join("home");
            let victim = tmp.path().join("victim_confd");
            fs::create_dir_all(home.join(".config/fish")).unwrap();
            fs::create_dir_all(&victim).unwrap();
            symlink(&victim, home.join(".config/fish/conf.d")).unwrap();
            check_symlink_case(
                "dot_config_fish_confd",
                &home,
                None,
                &victim,
                &victim.join("agy-companion.fish"),
            );
        }

        // Case (d): XDG_CONFIG_HOME set, $XDG_CONFIG_HOME/fish -> victim_xdg_fish
        {
            let tmp = TempDir::new().unwrap();
            let home = tmp.path().join("home");
            let xdg = tmp.path().join("xdg");
            let victim = tmp.path().join("victim_xdg_fish");
            fs::create_dir_all(&home).unwrap();
            fs::create_dir_all(&xdg).unwrap();
            fs::create_dir_all(&victim).unwrap();
            symlink(&victim, xdg.join("fish")).unwrap();
            check_symlink_case(
                "xdg_fish",
                &home,
                Some(&xdg),
                &victim,
                &victim.join("conf.d/agy-companion.fish"),
            );
        }

        // Case (e): XDG_CONFIG_HOME set, $XDG_CONFIG_HOME/fish/conf.d -> victim_xdg_confd
        {
            let tmp = TempDir::new().unwrap();
            let home = tmp.path().join("home");
            let xdg = tmp.path().join("xdg");
            let victim = tmp.path().join("victim_xdg_confd");
            fs::create_dir_all(&home).unwrap();
            fs::create_dir_all(xdg.join("fish")).unwrap();
            fs::create_dir_all(&victim).unwrap();
            symlink(&victim, xdg.join("fish/conf.d")).unwrap();
            check_symlink_case(
                "xdg_fish_confd",
                &home,
                Some(&xdg),
                &victim,
                &victim.join("agy-companion.fish"),
            );
        }

        // Case (f): Non-directory intermediate regular file fails closed with CoreError::InvalidPath.
        {
            let tmp = TempDir::new().unwrap();
            let home = tmp.path().join("home");
            let data = tmp.path().join("data");
            let bin = tmp.path().join("agy-companion");
            fs::create_dir_all(home.join(".config")).unwrap();
            fs::write(home.join(".config/fish"), "not a dir\n").unwrap();
            let opts = make_opts(ShellKind::Fish, None, data, bin, false);
            assert!(matches!(
                status_with_env(
                    &opts,
                    Some(&home),
                    None,
                    None,
                    Some("/usr/bin/fish"),
                    None,
                    false,
                    true
                ),
                Err(CoreError::InvalidPath { .. })
            ));
            assert!(matches!(
                enable_with_env(
                    &opts,
                    Some(&home),
                    None,
                    None,
                    Some("/usr/bin/fish"),
                    None,
                    false,
                    true
                ),
                Err(CoreError::InvalidPath { .. })
            ));
            assert!(matches!(
                disable_with_env(
                    &opts,
                    Some(&home),
                    None,
                    None,
                    Some("/usr/bin/fish"),
                    None,
                    false,
                    true
                ),
                Err(CoreError::InvalidPath { .. })
            ));
        }

        // Case (g): Dangling Fish directory symlink ($HOME/.config/fish -> /nonexistent):
        // manual_required when follow=false, InvalidPath when follow=true, no false-positive strip.
        {
            let tmp = TempDir::new().unwrap();
            let home = tmp.path().join("home");
            let data = tmp.path().join("data");
            let bin = tmp.path().join("agy-companion");
            let missing_target = tmp.path().join("does_not_exist_fish");
            fs::create_dir_all(home.join(".config")).unwrap();
            symlink(&missing_target, home.join(".config/fish")).unwrap();

            let opts = make_opts(ShellKind::Fish, None, data.clone(), bin.clone(), false);
            let st = status_with_env(
                &opts,
                Some(&home),
                None,
                None,
                Some("/usr/local/bin/fish"),
                None,
                false,
                true,
            )
            .unwrap();
            assert!(!st.enabled);
            assert!(st.manual_required);
            assert!(st.manual_snippet.is_some());

            let en = enable_with_env(
                &opts,
                Some(&home),
                None,
                None,
                Some("/usr/local/bin/fish"),
                None,
                false,
                true,
            )
            .unwrap();
            assert!(!en.enabled);
            assert!(en.manual_required);
            assert!(!missing_target.exists());

            let dis = disable_with_env(
                &opts,
                Some(&home),
                None,
                None,
                Some("/usr/local/bin/fish"),
                None,
                false,
                true,
            )
            .unwrap();
            assert!(!dis.enabled);
            assert!(!dis.manual_required);

            let follow_opts = make_opts(ShellKind::Fish, None, data, bin, true);
            assert!(matches!(
                enable_with_env(
                    &follow_opts,
                    Some(&home),
                    None,
                    None,
                    Some("/usr/local/bin/fish"),
                    None,
                    false,
                    true,
                ),
                Err(CoreError::InvalidPath { .. })
            ));

            let strip_rep = strip_all_known_rc_files(&home, None, None, false).unwrap();
            assert!(strip_rep.skipped_manual_files.is_empty());
        }

        // Case (h): Fresh absent ~/.config/fish/conf.d still succeeds cleanly on enable.
        {
            let tmp = TempDir::new().unwrap();
            let home = tmp.path().join("home");
            let data = tmp.path().join("data");
            let bin = tmp.path().join("agy-companion");
            fs::create_dir_all(&home).unwrap();
            let opts = make_opts(ShellKind::Fish, None, data, bin, false);
            let en = enable_with_env(
                &opts,
                Some(&home),
                None,
                None,
                Some("/usr/local/bin/fish"),
                None,
                false,
                true,
            )
            .unwrap();
            assert!(en.enabled);
            assert!(!en.manual_required);
            assert!(
                home.join(".config/fish/conf.d/agy-companion.fish")
                    .is_file()
            );
        }
    }

    #[cfg(unix)]
    #[test]
    fn strip_all_known_rc_files_refuses_symlinked_fish_intermediate_dirs() {
        use std::os::unix::fs::symlink;

        // Subcase (a): $HOME/.config -> victim_cfg with markers-only agy-companion.fish
        {
            let tmp = TempDir::new().unwrap();
            let home = tmp.path().join("home");
            let victim = tmp.path().join("victim_cfg");
            let dropin = victim.join("fish/conf.d/agy-companion.fish");
            fs::create_dir_all(&home).unwrap();
            fs::create_dir_all(dropin.parent().unwrap()).unwrap();
            let content = format!("{HOOK_BEGIN_MARKER}\n# managed\n{HOOK_END_MARKER}\n");
            fs::write(&dropin, &content).unwrap();
            symlink(&victim, home.join(".config")).unwrap();

            let expected_rc = home.join(".config/fish/conf.d/agy-companion.fish");
            for dry_run in [true, false] {
                let rep = strip_all_known_rc_files(&home, None, None, dry_run).unwrap();
                assert!(
                    rep.deleted_files.is_empty() && rep.modified_files.is_empty(),
                    "dry_run={dry_run}: must not delete or modify through symlinked ~/.config: {rep:?}"
                );
                assert_eq!(
                    rep.skipped_manual_files,
                    vec![expected_rc.clone()],
                    "dry_run={dry_run}: must record fish drop-in in skipped_manual_files"
                );
                assert_eq!(
                    fs::read_to_string(&dropin).unwrap(),
                    content,
                    "dry_run={dry_run}: victim fish drop-in must survive byte-for-byte"
                );
            }
        }

        // Subcase (b): $HOME/.config/fish -> victim_fish with user comment + markers
        {
            let tmp = TempDir::new().unwrap();
            let home = tmp.path().join("home");
            let victim = tmp.path().join("victim_fish");
            let dropin = victim.join("conf.d/agy-companion.fish");
            fs::create_dir_all(home.join(".config")).unwrap();
            fs::create_dir_all(dropin.parent().unwrap()).unwrap();
            let content =
                format!("# user note\n{HOOK_BEGIN_MARKER}\n# managed\n{HOOK_END_MARKER}\n");
            fs::write(&dropin, &content).unwrap();
            symlink(&victim, home.join(".config/fish")).unwrap();

            let expected_rc = home.join(".config/fish/conf.d/agy-companion.fish");
            for dry_run in [true, false] {
                let rep = strip_all_known_rc_files(&home, None, None, dry_run).unwrap();
                assert!(rep.deleted_files.is_empty() && rep.modified_files.is_empty());
                assert_eq!(rep.skipped_manual_files, vec![expected_rc.clone()]);
                assert_eq!(fs::read_to_string(&dropin).unwrap(), content);
            }
        }

        // Subcase (c): $HOME/.config/fish/conf.d -> victim_confd with markers
        {
            let tmp = TempDir::new().unwrap();
            let home = tmp.path().join("home");
            let victim = tmp.path().join("victim_confd");
            let dropin = victim.join("agy-companion.fish");
            fs::create_dir_all(home.join(".config/fish")).unwrap();
            fs::create_dir_all(&victim).unwrap();
            let content = format!("{HOOK_BEGIN_MARKER}\n# managed\n{HOOK_END_MARKER}\n");
            fs::write(&dropin, &content).unwrap();
            symlink(&victim, home.join(".config/fish/conf.d")).unwrap();

            let expected_rc = home.join(".config/fish/conf.d/agy-companion.fish");
            for dry_run in [true, false] {
                let rep = strip_all_known_rc_files(&home, None, None, dry_run).unwrap();
                assert!(rep.deleted_files.is_empty() && rep.modified_files.is_empty());
                assert_eq!(rep.skipped_manual_files, vec![expected_rc.clone()]);
                assert_eq!(fs::read_to_string(&dropin).unwrap(), content);
            }
        }

        // Subcase (d): $HOME/.config -> victim_empty with NO agy-companion.fish inside (no false positive)
        {
            let tmp = TempDir::new().unwrap();
            let home = tmp.path().join("home");
            let victim = tmp.path().join("victim_empty");
            fs::create_dir_all(&home).unwrap();
            fs::create_dir_all(&victim).unwrap();
            symlink(&victim, home.join(".config")).unwrap();

            for dry_run in [true, false] {
                let rep = strip_all_known_rc_files(&home, None, None, dry_run).unwrap();
                assert!(rep.deleted_files.is_empty());
                assert!(rep.modified_files.is_empty());
                assert!(
                    rep.skipped_manual_files.is_empty(),
                    "dry_run={dry_run}: absent fish drop-in must not emit false-positive skipped_manual_files"
                );
            }
        }

        // Subcase (e): Non-directory intermediate file at $XDG_CONFIG_HOME/fish skips safely
        {
            let tmp = TempDir::new().unwrap();
            let home = tmp.path().join("home");
            let xdg = tmp.path().join("xdg");
            fs::create_dir_all(&home).unwrap();
            fs::create_dir_all(&xdg).unwrap();
            fs::write(xdg.join("fish"), "regular file\n").unwrap();

            let rep = strip_all_known_rc_files(&home, None, Some(&xdg), false).unwrap();
            assert!(rep.deleted_files.is_empty());
            assert!(rep.modified_files.is_empty());
            assert!(rep.skipped_manual_files.is_empty());
            assert_eq!(
                fs::read_to_string(xdg.join("fish")).unwrap(),
                "regular file\n"
            );
        }
    }

    #[cfg(unix)]
    #[test]
    fn zsh_and_bash_integration_refuse_symlinked_or_non_dir_env_data_dir_chain() {
        use std::os::unix::fs::{PermissionsExt, symlink};

        let check_symlink_rejected = |label: &str,
                                      shell: ShellKind,
                                      home: &Path,
                                      opts_data_dir: Option<PathBuf>,
                                      default_data_override: Option<&Path>,
                                      victim_dir: &Path,
                                      victim_env: &Path| {
            let shell_env = match shell {
                ShellKind::Zsh => Some("/bin/zsh"),
                ShellKind::Bash => Some("/bin/bash"),
                ShellKind::Fish => unreachable!(),
            };
            let rc_path = match shell {
                ShellKind::Zsh => home.join(".zlogin"),
                ShellKind::Bash => home.join(".profile"),
                ShellKind::Fish => unreachable!(),
            };
            let bin = home.parent().unwrap().join("agy-companion");
            let opts = IntegrationOptions {
                shell: Some(shell),
                rc_file: None,
                binary_path: Some(bin),
                data_dir: opts_data_dir,
                follow_rc_symlink: false,
            };

            // 1. Fresh state: enable must fail closed, never create env.sh, never chmod victim_dir,
            //    and never create or mutate RC file.
            fs::set_permissions(victim_dir, fs::Permissions::from_mode(0o755)).unwrap();
            assert!(
                !victim_env.exists(),
                "{label} ({shell}): precondition no victim env"
            );
            assert!(
                matches!(
                    enable_with_env(
                        &opts,
                        Some(home),
                        None,
                        None,
                        shell_env,
                        default_data_override,
                        false,
                        true,
                    ),
                    Err(CoreError::InvalidPath { .. })
                ),
                "{label} ({shell}): enable must fail closed with InvalidPath"
            );
            assert!(
                !victim_env.exists(),
                "{label} ({shell}): enable must NOT create env.sh inside symlink target"
            );
            assert_eq!(
                fs::metadata(victim_dir).unwrap().permissions().mode() & 0o777,
                0o755,
                "{label} ({shell}): enable must NOT chmod symlink target dir from 0755 to 0700"
            );
            assert!(
                !rc_path.exists(),
                "{label} ({shell}): enable must NOT create RC file when data dir is unsafe"
            );

            // 2. Populated state (pre-existing victim env.sh + RC file with valid markers):
            //    status, enable, and disable must ALL fail closed, preserving victim env.sh,
            //    victim_dir 0o755 permissions, and RC content byte-for-byte.
            let sentinel_env = "# VICTIM_ENV_SENTINEL\n";
            fs::write(victim_env, sentinel_env).unwrap();
            let rc_before = format!(
                "# user line\n{HOOK_BEGIN_MARKER}\n[ -r \"{PORTABLE_ENV_SCRIPT_EXPR}\" ] && . \"{PORTABLE_ENV_SCRIPT_EXPR}\"\n{HOOK_END_MARKER}\n"
            );
            fs::write(&rc_path, &rc_before).unwrap();

            assert!(
                matches!(
                    status_with_env(
                        &opts,
                        Some(home),
                        None,
                        None,
                        shell_env,
                        default_data_override,
                        false,
                        true,
                    ),
                    Err(CoreError::InvalidPath { .. })
                ),
                "{label} ({shell}): status must fail closed with InvalidPath"
            );
            assert!(
                matches!(
                    enable_with_env(
                        &opts,
                        Some(home),
                        None,
                        None,
                        shell_env,
                        default_data_override,
                        false,
                        true,
                    ),
                    Err(CoreError::InvalidPath { .. })
                ),
                "{label} ({shell}): enable with existing files must fail closed with InvalidPath"
            );
            assert!(
                matches!(
                    disable_with_env(
                        &opts,
                        Some(home),
                        None,
                        None,
                        shell_env,
                        default_data_override,
                        false,
                        true,
                    ),
                    Err(CoreError::InvalidPath { .. })
                ),
                "{label} ({shell}): disable must fail closed with InvalidPath"
            );
            assert_eq!(
                fs::read_to_string(victim_env).unwrap(),
                sentinel_env,
                "{label} ({shell}): disable must NOT delete or modify victim env.sh"
            );
            assert_eq!(
                fs::metadata(victim_dir).unwrap().permissions().mode() & 0o777,
                0o755,
                "{label} ({shell}): victim dir mode must remain 0o755"
            );
            assert_eq!(
                fs::read_to_string(&rc_path).unwrap(),
                rc_before,
                "{label} ({shell}): RC file must remain untouched on fail-closed disable/enable"
            );
        };

        for shell in [ShellKind::Zsh, ShellKind::Bash] {
            // Case (a): XDG_DATA_HOME unset, $HOME/.local -> victim_local
            {
                let tmp = TempDir::new().unwrap();
                let home = tmp.path().join("home");
                let victim_local = tmp.path().join("victim_local");
                let victim_data = victim_local.join("share/agy-companion");
                fs::create_dir_all(&home).unwrap();
                fs::create_dir_all(&victim_data).unwrap();
                symlink(&victim_local, home.join(".local")).unwrap();
                let default_data = home.join(".local/share/agy-companion");
                check_symlink_rejected(
                    "dot_local",
                    shell,
                    &home,
                    None,
                    Some(&default_data),
                    &victim_data,
                    &victim_data.join("env.sh"),
                );
            }

            // Case (b): XDG_DATA_HOME unset, $HOME/.local/share -> victim_share
            {
                let tmp = TempDir::new().unwrap();
                let home = tmp.path().join("home");
                let victim_share = tmp.path().join("victim_share");
                let victim_data = victim_share.join("agy-companion");
                fs::create_dir_all(home.join(".local")).unwrap();
                fs::create_dir_all(&victim_data).unwrap();
                symlink(&victim_share, home.join(".local/share")).unwrap();
                let default_data = home.join(".local/share/agy-companion");
                check_symlink_rejected(
                    "dot_local_share",
                    shell,
                    &home,
                    None,
                    Some(&default_data),
                    &victim_data,
                    &victim_data.join("env.sh"),
                );
            }

            // Case (c): XDG_DATA_HOME unset, $HOME/.local/share/agy-companion -> victim_data
            {
                let tmp = TempDir::new().unwrap();
                let home = tmp.path().join("home");
                let victim_data = tmp.path().join("victim_data");
                fs::create_dir_all(home.join(".local/share")).unwrap();
                fs::create_dir_all(&victim_data).unwrap();
                symlink(&victim_data, home.join(".local/share/agy-companion")).unwrap();
                let default_data = home.join(".local/share/agy-companion");
                check_symlink_rejected(
                    "dot_local_share_app",
                    shell,
                    &home,
                    None,
                    Some(&default_data),
                    &victim_data,
                    &victim_data.join("env.sh"),
                );
            }

            // Case (d): Explicit XDG_DATA_HOME=$HOME/.local/share (with trailing slash) while $HOME/.local -> victim_local
            {
                let tmp = TempDir::new().unwrap();
                let home = tmp.path().join("home");
                let victim_local = tmp.path().join("victim_local");
                let victim_data = victim_local.join("share/agy-companion");
                fs::create_dir_all(&home).unwrap();
                fs::create_dir_all(&victim_data).unwrap();
                symlink(&victim_local, home.join(".local")).unwrap();
                let explicit_default_with_slash = PathBuf::from(format!(
                    "{}/",
                    home.join(".local/share/agy-companion").display()
                ));
                check_symlink_rejected(
                    "explicit_xdg_eq_default",
                    shell,
                    &home,
                    Some(explicit_default_with_slash),
                    None,
                    &victim_data,
                    &victim_data.join("env.sh"),
                );
            }

            // Case (e): Custom XDG_DATA_HOME/agy-companion -> victim_data
            {
                let tmp = TempDir::new().unwrap();
                let home = tmp.path().join("home");
                let custom_xdg = tmp.path().join("custom_xdg");
                let victim_data = tmp.path().join("victim_custom");
                fs::create_dir_all(&home).unwrap();
                fs::create_dir_all(&custom_xdg).unwrap();
                fs::create_dir_all(&victim_data).unwrap();
                let custom_data_link = custom_xdg.join("agy-companion");
                symlink(&victim_data, &custom_data_link).unwrap();
                check_symlink_rejected(
                    "custom_xdg_data",
                    shell,
                    &home,
                    Some(custom_data_link),
                    None,
                    &victim_data,
                    &victim_data.join("env.sh"),
                );
            }

            // Case (f): Non-directory regular file at $HOME/.local, $HOME/.local/share,
            //           $HOME/.local/share/agy-companion, and custom data_dir all fail closed.
            for non_dir_kind in [
                "dot_local",
                "dot_local_share",
                "dot_local_share_app",
                "custom",
            ] {
                let tmp = TempDir::new().unwrap();
                let home = tmp.path().join("home");
                let bin = tmp.path().join("agy-companion");
                fs::create_dir_all(&home).unwrap();
                let (opts_data_dir, default_override) = match non_dir_kind {
                    "dot_local" => {
                        fs::write(home.join(".local"), "not a dir\n").unwrap();
                        (None, Some(home.join(".local/share/agy-companion")))
                    }
                    "dot_local_share" => {
                        fs::create_dir_all(home.join(".local")).unwrap();
                        fs::write(home.join(".local/share"), "not a dir\n").unwrap();
                        (None, Some(home.join(".local/share/agy-companion")))
                    }
                    "dot_local_share_app" => {
                        fs::create_dir_all(home.join(".local/share")).unwrap();
                        fs::write(home.join(".local/share/agy-companion"), "not a dir\n").unwrap();
                        (None, Some(home.join(".local/share/agy-companion")))
                    }
                    "custom" => {
                        let custom_file = tmp.path().join("custom_file_as_data_dir");
                        fs::write(&custom_file, "not a dir\n").unwrap();
                        (Some(custom_file), None)
                    }
                    _ => unreachable!(),
                };
                let shell_env = match shell {
                    ShellKind::Zsh => Some("/bin/zsh"),
                    ShellKind::Bash => Some("/bin/bash"),
                    ShellKind::Fish => unreachable!(),
                };
                let opts = IntegrationOptions {
                    shell: Some(shell),
                    rc_file: None,
                    binary_path: Some(bin),
                    data_dir: opts_data_dir,
                    follow_rc_symlink: false,
                };
                assert!(
                    matches!(
                        enable_with_env(
                            &opts,
                            Some(&home),
                            None,
                            None,
                            shell_env,
                            default_override.as_deref(),
                            false,
                            true,
                        ),
                        Err(CoreError::InvalidPath { .. })
                    ),
                    "non_dir={non_dir_kind} ({shell}): enable must fail with InvalidPath"
                );
                assert!(
                    matches!(
                        status_with_env(
                            &opts,
                            Some(&home),
                            None,
                            None,
                            shell_env,
                            default_override.as_deref(),
                            false,
                            true,
                        ),
                        Err(CoreError::InvalidPath { .. })
                    ),
                    "non_dir={non_dir_kind} ({shell}): status must fail with InvalidPath"
                );
                assert!(
                    matches!(
                        disable_with_env(
                            &opts,
                            Some(&home),
                            None,
                            None,
                            shell_env,
                            default_override.as_deref(),
                            false,
                            true,
                        ),
                        Err(CoreError::InvalidPath { .. })
                    ),
                    "non_dir={non_dir_kind} ({shell}): disable must fail with InvalidPath"
                );
            }

            // Case (g): Even when .zlogin/.profile is ALSO a symlink, symlinked $HOME/.local
            //           must fail closed BEFORE creating env.sh inside victim_local.
            {
                let tmp = TempDir::new().unwrap();
                let home = tmp.path().join("home");
                let victim_local = tmp.path().join("victim_local");
                let victim_rc = tmp.path().join("victim_rc");
                let bin = tmp.path().join("agy-companion");
                fs::create_dir_all(&home).unwrap();
                fs::create_dir_all(&victim_local).unwrap();
                fs::write(&victim_rc, "# existing rc\n").unwrap();
                symlink(&victim_local, home.join(".local")).unwrap();
                let rc_link = match shell {
                    ShellKind::Zsh => home.join(".zlogin"),
                    ShellKind::Bash => home.join(".profile"),
                    ShellKind::Fish => unreachable!(),
                };
                symlink(&victim_rc, &rc_link).unwrap();
                let default_data = home.join(".local/share/agy-companion");
                let opts = IntegrationOptions {
                    shell: Some(shell),
                    rc_file: None,
                    binary_path: Some(bin),
                    data_dir: None,
                    follow_rc_symlink: false,
                };
                assert!(matches!(
                    enable_with_env(
                        &opts,
                        Some(&home),
                        None,
                        None,
                        Some("/bin/zsh"),
                        Some(&default_data),
                        false,
                        true,
                    ),
                    Err(CoreError::InvalidPath { .. })
                ));
                assert!(
                    !victim_local.join("share/agy-companion/env.sh").exists(),
                    "both_sym ({shell}): must not create env.sh in victim_local"
                );
                assert_eq!(fs::read_to_string(&victim_rc).unwrap(), "# existing rc\n");
            }

            // Case (h): Fresh absent ~/.local/share/agy-companion succeeds cleanly on enable -> status -> disable.
            {
                let tmp = TempDir::new().unwrap();
                let home = tmp.path().join("home");
                let bin = tmp.path().join("agy-companion");
                fs::create_dir_all(&home).unwrap();
                let default_data = home.join(".local/share/agy-companion");
                let opts = IntegrationOptions {
                    shell: Some(shell),
                    rc_file: None,
                    binary_path: Some(bin),
                    data_dir: None,
                    follow_rc_symlink: false,
                };
                let shell_env = match shell {
                    ShellKind::Zsh => Some("/bin/zsh"),
                    ShellKind::Bash => Some("/bin/bash"),
                    ShellKind::Fish => unreachable!(),
                };
                let en = enable_with_env(
                    &opts,
                    Some(&home),
                    None,
                    None,
                    shell_env,
                    Some(&default_data),
                    false,
                    true,
                )
                .unwrap();
                assert!(en.enabled, "fresh ({shell}): enable should succeed");
                assert!(default_data.join("env.sh").is_file());

                let st = status_with_env(
                    &opts,
                    Some(&home),
                    None,
                    None,
                    shell_env,
                    Some(&default_data),
                    false,
                    true,
                )
                .unwrap();
                assert!(st.enabled, "fresh ({shell}): status should report enabled");

                let dis = disable_with_env(
                    &opts,
                    Some(&home),
                    None,
                    None,
                    shell_env,
                    Some(&default_data),
                    false,
                    true,
                )
                .unwrap();
                assert!(!dis.enabled, "fresh ({shell}): disable should succeed");
                assert!(!default_data.join("env.sh").exists());
            }
        }

        // Case (i): Fish does NOT use env.sh and must NOT be blocked when $HOME/.local is a symlink.
        {
            let tmp = TempDir::new().unwrap();
            let home = tmp.path().join("home");
            let victim_local = tmp.path().join("victim_local");
            let bin = tmp.path().join("agy-companion");
            fs::create_dir_all(&home).unwrap();
            fs::create_dir_all(&victim_local).unwrap();
            symlink(&victim_local, home.join(".local")).unwrap();
            let default_data = home.join(".local/share/agy-companion");
            let opts = IntegrationOptions {
                shell: Some(ShellKind::Fish),
                rc_file: None,
                binary_path: Some(bin),
                data_dir: None,
                follow_rc_symlink: false,
            };
            let en = enable_with_env(
                &opts,
                Some(&home),
                None,
                None,
                Some("/usr/local/bin/fish"),
                Some(&default_data),
                false,
                true,
            )
            .unwrap();
            assert!(en.enabled);
            let st = status_with_env(
                &opts,
                Some(&home),
                None,
                None,
                Some("/usr/local/bin/fish"),
                Some(&default_data),
                false,
                true,
            )
            .unwrap();
            assert!(st.enabled);
            let dis = disable_with_env(
                &opts,
                Some(&home),
                None,
                None,
                Some("/usr/local/bin/fish"),
                Some(&default_data),
                false,
                true,
            )
            .unwrap();
            assert!(!dis.enabled);
            assert!(
                !victim_local.join("share/agy-companion/env.sh").exists(),
                "fish must never touch env.sh inside symlinked ~/.local"
            );
        }
    }

    #[cfg(unix)]
    #[test]
    fn leaf_env_script_symlink_dir_and_fifo_fail_closed_before_rc_mutation_and_fish_unaffected() {
        use std::os::unix::fs::symlink;
        use std::sync::mpsc;
        use std::time::Duration;

        for shell in [ShellKind::Zsh, ShellKind::Bash] {
            let shell_env = match shell {
                ShellKind::Zsh => Some("/bin/zsh"),
                ShellKind::Bash => Some("/bin/bash"),
                ShellKind::Fish => unreachable!(),
            };
            let rc_filename = match shell {
                ShellKind::Zsh => ".zlogin",
                ShellKind::Bash => ".profile",
                ShellKind::Fish => unreachable!(),
            };

            for kind in ["symlink", "directory", "fifo"] {
                let tmp = TempDir::new().unwrap();
                let home = tmp.path().join("home");
                let data_dir = tmp.path().join("data");
                let bin = tmp.path().join("agy-companion");
                fs::create_dir_all(&home).unwrap();
                fs::create_dir_all(&data_dir).unwrap();

                let env_path = data_dir.join(ENV_SCRIPT_FILENAME);
                let victim_env = tmp.path().join("victim_env.sh");
                let sentinel = "# VICTIM_LEAF_ENV\n";
                match kind {
                    "symlink" => {
                        fs::write(&victim_env, sentinel).unwrap();
                        symlink(&victim_env, &env_path).unwrap();
                    }
                    "directory" => {
                        fs::create_dir_all(&env_path).unwrap();
                    }
                    "fifo" => {
                        assert!(
                            Command::new("mkfifo")
                                .arg(&env_path)
                                .status()
                                .unwrap()
                                .success()
                        );
                    }
                    _ => unreachable!(),
                }

                // Marker-only RC file: Zsh .zlogin would be deleted if disable mutated RC before
                // validating leaf env.sh; Bash .profile would be stripped to empty.
                let rc_path = home.join(rc_filename);
                let snippet = render_rc_snippet(shell, &env_path).unwrap();
                let rc_before = format!("{snippet}\n");
                fs::write(&rc_path, &rc_before).unwrap();

                let opts = make_opts(shell, None, data_dir.clone(), bin.clone(), false);

                // Run on a worker thread with a bounded wait so a FIFO regression fails fast
                // instead of hanging the test runner.
                let (tx, rx) = mpsc::channel();
                let home_c = home.clone();
                let opts_c = opts.clone();
                std::thread::spawn(move || {
                    let st = status_with_env(
                        &opts_c,
                        Some(&home_c),
                        None,
                        None,
                        shell_env,
                        None,
                        false,
                        true,
                    );
                    let en = enable_with_env(
                        &opts_c,
                        Some(&home_c),
                        None,
                        None,
                        shell_env,
                        None,
                        false,
                        true,
                    );
                    let dis = disable_with_env(
                        &opts_c,
                        Some(&home_c),
                        None,
                        None,
                        shell_env,
                        None,
                        false,
                        true,
                    );
                    let _ = tx.send((st, en, dis));
                });

                let (st_res, en_res, dis_res) = rx
                    .recv_timeout(Duration::from_secs(2))
                    .unwrap_or_else(|_| panic!("{kind} ({shell}): timed out (FIFO hang)"));

                assert!(
                    matches!(st_res, Err(CoreError::InvalidPath { .. })),
                    "{kind} ({shell}): status must fail closed with InvalidPath, got: {st_res:?}"
                );
                assert!(
                    matches!(en_res, Err(CoreError::InvalidPath { .. })),
                    "{kind} ({shell}): enable must fail closed with InvalidPath, got: {en_res:?}"
                );
                assert!(
                    matches!(dis_res, Err(CoreError::InvalidPath { .. })),
                    "{kind} ({shell}): disable must fail closed with InvalidPath, got: {dis_res:?}"
                );
                assert!(
                    rc_path.is_file(),
                    "{kind} ({shell}): RC file must NOT be deleted when disable fails on leaf env.sh"
                );
                assert_eq!(
                    fs::read_to_string(&rc_path).unwrap(),
                    rc_before,
                    "{kind} ({shell}): RC file must remain byte-for-byte intact on failed enable/disable"
                );
                if kind == "symlink" {
                    assert_eq!(
                        fs::read_to_string(&victim_env).unwrap(),
                        sentinel,
                        "{kind} ({shell}): symlink victim must remain untouched"
                    );
                }
            }
        }

        // Fish does not use env.sh and must remain completely unaffected by symlink/dir/FIFO env.sh.
        for kind in ["symlink", "directory", "fifo"] {
            let tmp = TempDir::new().unwrap();
            let home = tmp.path().join("home");
            let data_dir = tmp.path().join("data");
            let bin = tmp.path().join("agy-companion");
            fs::create_dir_all(&home).unwrap();
            fs::create_dir_all(&data_dir).unwrap();

            let env_path = data_dir.join(ENV_SCRIPT_FILENAME);
            let victim_env = tmp.path().join("victim_fish_env.sh");
            let sentinel = "# FISH_VICTIM_ENV\n";
            match kind {
                "symlink" => {
                    fs::write(&victim_env, sentinel).unwrap();
                    symlink(&victim_env, &env_path).unwrap();
                }
                "directory" => {
                    fs::create_dir_all(&env_path).unwrap();
                }
                "fifo" => {
                    assert!(
                        Command::new("mkfifo")
                            .arg(&env_path)
                            .status()
                            .unwrap()
                            .success()
                    );
                }
                _ => unreachable!(),
            }

            let opts = make_opts(ShellKind::Fish, None, data_dir.clone(), bin.clone(), false);
            let en = enable_with_env(
                &opts,
                Some(&home),
                None,
                None,
                Some("/usr/local/bin/fish"),
                None,
                false,
                true,
            )
            .unwrap();
            assert!(en.enabled, "fish ({kind}): enable must succeed");
            let st = status_with_env(
                &opts,
                Some(&home),
                None,
                None,
                Some("/usr/local/bin/fish"),
                None,
                false,
                true,
            )
            .unwrap();
            assert!(st.enabled, "fish ({kind}): status must report enabled");
            let dis = disable_with_env(
                &opts,
                Some(&home),
                None,
                None,
                Some("/usr/local/bin/fish"),
                None,
                false,
                true,
            )
            .unwrap();
            assert!(!dis.enabled, "fish ({kind}): disable must succeed");
            if kind == "symlink" {
                assert_eq!(fs::read_to_string(&victim_env).unwrap(), sentinel);
            }
        }
    }
}
