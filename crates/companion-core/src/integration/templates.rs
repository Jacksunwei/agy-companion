use std::path::Path;

use crate::config::{CoreError, default_data_dir};
use crate::fs_safe::{self, AtomicWriteOptions, ParentDirPolicy, TargetFilePolicy};
use crate::validation::PROXY_PLACEHOLDER_API_KEY;

use super::fs_inspect::validate_env_script_parent_dir;
use super::{
    DEFAULT_RESTORE_PREFIX, ENV_SCRIPT_FILENAME, HOOK_BEGIN_MARKER, HOOK_END_MARKER,
    PORTABLE_ENV_SCRIPT_EXPR, ShellKind,
};

pub(super) fn validate_shell_safe_path(path: &Path) -> Result<&str, CoreError> {
    fs_safe::validate_path(path)?;
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

pub(crate) fn render_env_sh(binary_path: &Path) -> Result<String, CoreError> {
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

pub(crate) fn render_fish_dropin(binary_path: &Path) -> Result<String, CoreError> {
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

pub(super) fn wrap_with_markers(body: &str) -> String {
    let trimmed = body.trim_end_matches(['\r', '\n']);
    format!("{HOOK_BEGIN_MARKER}\n{trimmed}\n{HOOK_END_MARKER}")
}

pub(super) fn render_posix_rc_snippet_internal(
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

pub(crate) fn write_env_script(
    env_script_path: &Path,
    binary_path: &Path,
) -> Result<(), CoreError> {
    validate_shell_safe_path(env_script_path)?;
    let home = fs_safe::default_home_dir();
    validate_env_script_parent_dir(env_script_path, home.as_deref())?;
    let content = render_env_sh(binary_path)?;
    fs_safe::write_atomic(
        env_script_path,
        content.as_bytes(),
        AtomicWriteOptions {
            file_mode: 0o600,
            dir_mode: 0o700,
            parent_policy: ParentDirPolicy::RejectSymlinkAndTighten,
            target_policy: TargetFilePolicy::RejectSymlinkAndNonRegular,
        },
    )?;
    Ok(())
}
