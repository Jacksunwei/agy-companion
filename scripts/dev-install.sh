#!/usr/bin/env bash
set -euo pipefail
unset CDPATH

SCRIPT_DIR="$(cd -P -- "$(dirname -- "${BASH_SOURCE[0]}")" >/dev/null 2>&1 && pwd -P)"
SCRIPT_SELF="${SCRIPT_DIR}/$(basename -- "${BASH_SOURCE[0]}")"
REPO_ROOT="$(cd -P -- "${SCRIPT_DIR}/.." >/dev/null 2>&1 && pwd -P)"
PLUGIN_SRC="${REPO_ROOT}/plugin"

NO_BUILD=0
SKIP_UI=0
RELEASE=0
DRY_RUN=0
UNINSTALL=0
NO_LAUNCHER_HOOK=0
NO_START_PROXY=0
FORCE=0
PURGE=0
JSON_OUT=0
CONFIG_ARG=""
CONFIG_SET=0
PLUGINS_DIR_ARG=""
PLUGINS_DIR_EXPLICIT=0

usage() {
  cat <<'EOF'
Usage: scripts/dev-install.sh [OPTIONS]

Build agy-companion locally (UI + Rust binary) and deploy the compiled binary
and embedded plugin bundle via `agy-companion install` (real files, no repo
symlinks) without modifying user Antigravity settings.

Options:
  --no-build            Skip UI and Cargo builds; deploy existing compiled binary
  --skip-ui             Skip npm UI build; still run cargo build unless --no-build
  --release             Build/deploy release binary (cargo build --release -p companion-cli)
  --dry-run             Print planned build and install commands (or delegate uninstall --dry-run)
  --uninstall           Uninstall via `agy-companion uninstall`
  --config <path>       Pass --config <path> to agy-companion install/uninstall
  --plugins-dir <dir>   Override target plugins directory
                        (default: $AGY_PLUGINS_DIR or ~/.gemini/config/plugins)
  --no-launcher-hook    Skip configuring the Dock/Launcher shell hook
  --no-start-proxy      Skip starting or restarting the background local proxy
  --force               Pass --force to `agy-companion install`
  --purge               Pass --purge to `agy-companion uninstall` (with --uninstall)
  --json                Emit machine-readable JSON report from install/uninstall
  -h, --help            Show this help message and exit

Environment:
  AGY_COMPANION_BIN     Override binary used with --no-build or --uninstall (fails if invalid)
  AGY_PLUGINS_DIR       Default plugins directory override (forwarded via --plugins-dir)
  CARGO_TARGET_DIR      Cargo target directory override (default: <repo>/target)
EOF
}

die() {
  printf 'dev-install.sh: error: %s\n' "$1" >&2
  exit "${2:-1}"
}

is_blank() { [[ -z "${1//[[:space:]]/}" ]]; }

is_usable_bin() {
  local cand="$1"
  [[ -f "${cand}" && -x "${cand}" && ! "${cand}" -ef "${SCRIPT_SELF}" ]]
}

strip_trailing_slashes() {
  local val="$1"
  while [[ "${val}" != "/" && "${val}" == */ ]]; do
    val="${val%/}"
  done
  printf '%s\n' "${val}"
}

is_legacy_repo_symlink() {
  local link_path="$1"
  [[ -L "${link_path}" ]] || return 1
  local raw_target="" candidate="" link_parent="" resolved=""
  raw_target="$(readlink -- "${link_path}" 2>/dev/null || true)"
  [[ -n "${raw_target}" ]] || return 1
  [[ "${raw_target}" == "${PLUGIN_SRC}" ]] && return 0
  case "${raw_target}" in
    /*) candidate="${raw_target}" ;;
    *)
      link_parent="$(cd -P -- "$(dirname -- "${link_path}")" >/dev/null 2>&1 && pwd -P)" || return 1
      candidate="${link_parent}/${raw_target}"
      ;;
  esac
  if [[ -d "${candidate}" ]]; then
    resolved="$(cd -P -- "${candidate}" >/dev/null 2>&1 && pwd -P)" || return 1
    [[ "${resolved}" == "${PLUGIN_SRC}" ]]
    return $?
  fi
  return 1
}

parse_args() {
  while [[ $# -gt 0 ]]; do
    case "$1" in
      --no-build) NO_BUILD=1; shift ;;
      --skip-ui) SKIP_UI=1; shift ;;
      --release) RELEASE=1; shift ;;
      --dry-run) DRY_RUN=1; shift ;;
      --uninstall) UNINSTALL=1; shift ;;
      --no-launcher-hook) NO_LAUNCHER_HOOK=1; shift ;;
      --no-start-proxy) NO_START_PROXY=1; shift ;;
      --force) FORCE=1; shift ;;
      --purge) PURGE=1; shift ;;
      --json) JSON_OUT=1; shift ;;
      --config)
        [[ $# -ge 2 ]] || die "missing value for --config"
        if is_blank "$2" || [[ "$2" == -* ]]; then die "invalid value for --config: '$2'"; fi
        CONFIG_ARG="$2"; CONFIG_SET=1; shift 2
        ;;
      --config=*)
        local cval="${1#--config=}"
        if is_blank "${cval}" || [[ "${cval}" == -* ]]; then die "invalid value for --config: '${cval}'"; fi
        CONFIG_ARG="${cval}"; CONFIG_SET=1; shift
        ;;
      --plugins-dir)
        [[ $# -ge 2 ]] || die "missing value for --plugins-dir"
        if is_blank "$2" || [[ "$2" == -* ]]; then die "invalid value for --plugins-dir: '$2'"; fi
        PLUGINS_DIR_ARG="$2"; PLUGINS_DIR_EXPLICIT=1; shift 2
        ;;
      --plugins-dir=*)
        local pval="${1#--plugins-dir=}"
        if is_blank "${pval}"; then die "missing value for --plugins-dir="; fi
        PLUGINS_DIR_ARG="${pval}"; PLUGINS_DIR_EXPLICIT=1; shift
        ;;
      -h|--help) usage; exit 0 ;;
      *)
        printf 'dev-install.sh: error: unknown argument: %s\n\n' "$1" >&2
        usage >&2
        exit 1
        ;;
    esac
  done
}

run_build_cmd() {
  if [[ ${DRY_RUN} -eq 1 ]]; then
    printf '[dry-run]'
    printf ' %q' "$@"
    printf '\n'
  elif [[ ${JSON_OUT} -eq 1 ]]; then
    "$@" >&2
  else
    "$@"
  fi
}

parse_args ${1+"$@"}

if [[ ${UNINSTALL} -eq 1 ]]; then
  if [[ ${NO_LAUNCHER_HOOK} -eq 1 || ${NO_START_PROXY} -eq 1 || ${FORCE} -eq 1 || ${SKIP_UI} -eq 1 ]]; then
    die "--no-launcher-hook, --no-start-proxy, --force, and --skip-ui cannot be combined with --uninstall"
  fi
else
  [[ ${PURGE} -eq 0 ]] || die "--purge can only be used with --uninstall"
  if [[ ${DRY_RUN} -eq 1 && ${JSON_OUT} -eq 1 ]]; then
    die "--dry-run cannot be combined with --json on install (agy-companion install has no --dry-run mode)" 2
  fi
fi

[[ -d "${PLUGIN_SRC}" && -f "${PLUGIN_SRC}/plugin.json" ]] || die "plugin bundle not found at ${PLUGIN_SRC}"

if [[ ${PLUGINS_DIR_EXPLICIT} -eq 1 ]]; then
  PLUGINS_DIR="${PLUGINS_DIR_ARG}"
elif [[ -n "${AGY_PLUGINS_DIR+x}" ]]; then
  is_blank "${AGY_PLUGINS_DIR}" && die "AGY_PLUGINS_DIR is set but empty"
  PLUGINS_DIR="${AGY_PLUGINS_DIR}"
  PLUGINS_DIR_EXPLICIT=1
else
  [[ -n "${HOME:-}" ]] || die "HOME is not set; pass --plugins-dir <dir> explicitly"
  PLUGINS_DIR="${HOME%/}/.gemini/config/plugins"
fi

is_blank "${PLUGINS_DIR}" && die "plugins directory must not be empty"
PLUGINS_DIR="$(strip_trailing_slashes "${PLUGINS_DIR}")"
case "${PLUGINS_DIR}" in -*) PLUGINS_DIR="./${PLUGINS_DIR}" ;; esac
if [[ ${UNINSTALL} -eq 0 && -L "${PLUGINS_DIR}" ]]; then
  die "refusing to traverse symlinked plugins directory: ${PLUGINS_DIR}"
fi
if [[ -e "${PLUGINS_DIR}" && ! -d "${PLUGINS_DIR}" ]]; then
  die "plugins directory path exists and is not a directory: ${PLUGINS_DIR}"
fi
PLUGIN_TARGET="${PLUGINS_DIR%/}/agy-companion"

if [[ -n "${CARGO_TARGET_DIR+x}" ]]; then
  is_blank "${CARGO_TARGET_DIR}" && die "CARGO_TARGET_DIR is set but empty"
  TARGET_DIR="$(strip_trailing_slashes "${CARGO_TARGET_DIR}")"
else
  TARGET_DIR="${REPO_ROOT}/target"
fi
case "${TARGET_DIR}" in -*) TARGET_DIR="./${TARGET_DIR}" ;; esac
if [[ ${RELEASE} -eq 1 ]]; then
  BUILT_BIN="${TARGET_DIR}/release/agy-companion"
  ALT_BUILT_BIN="${TARGET_DIR}/debug/agy-companion"
else
  BUILT_BIN="${TARGET_DIR}/debug/agy-companion"
  ALT_BUILT_BIN="${TARGET_DIR}/release/agy-companion"
fi

OVERRIDE_BIN=""
if [[ -n "${AGY_COMPANION_BIN+x}" ]]; then
  is_blank "${AGY_COMPANION_BIN}" && die "AGY_COMPANION_BIN is set but empty"
  OVERRIDE_BIN="${AGY_COMPANION_BIN}"
  case "${OVERRIDE_BIN}" in */*) ;; *) OVERRIDE_BIN="./${OVERRIDE_BIN}" ;; esac
  is_usable_bin "${OVERRIDE_BIN}" || die "AGY_COMPANION_BIN is not a usable executable binary: ${AGY_COMPANION_BIN}"
  if [[ ${UNINSTALL} -eq 1 || ${NO_BUILD} -eq 1 ]]; then
    BUILT_BIN="${OVERRIDE_BIN}"
  fi
fi

if [[ -n "${XDG_DATA_HOME:-}" ]]; then
  case "/${XDG_DATA_HOME}/" in
    */../*) die "XDG_DATA_HOME must not contain '..' traversal: ${XDG_DATA_HOME}" ;;
  esac
fi

resolve_uninstall_bin() {
  local cand="" xdg_bin="" path_bin=""
  if [[ -n "${OVERRIDE_BIN}" ]]; then
    printf '%s\n' "${OVERRIDE_BIN}"
    return 0
  fi
  for cand in "${BUILT_BIN}" "${ALT_BUILT_BIN}"; do
    if is_usable_bin "${cand}"; then
      printf '%s\n' "${cand}"
      return 0
    fi
  done
  if [[ -n "${XDG_DATA_HOME:-}" && "${XDG_DATA_HOME}" == /* ]]; then
    xdg_bin="${XDG_DATA_HOME%/}/agy-companion/bin/agy-companion"
  elif [[ -n "${HOME:-}" ]]; then
    xdg_bin="${HOME%/}/.local/share/agy-companion/bin/agy-companion"
  fi
  if [[ -n "${xdg_bin}" ]] && is_usable_bin "${xdg_bin}"; then
    printf '%s\n' "${xdg_bin}"
    return 0
  fi
  path_bin="$(command -v agy-companion 2>/dev/null || true)"
  if [[ -n "${path_bin}" ]] && is_usable_bin "${path_bin}"; then
    printf '%s\n' "${path_bin}"
    return 0
  fi
  return 1
}

if [[ ${UNINSTALL} -eq 1 ]]; then
  UNINSTALL_ARGS=(uninstall)
  [[ ${CONFIG_SET} -eq 1 ]] && UNINSTALL_ARGS+=(--config "${CONFIG_ARG}")
  [[ ${PLUGINS_DIR_EXPLICIT} -eq 1 ]] && UNINSTALL_ARGS+=(--plugins-dir "${PLUGINS_DIR}")
  [[ ${PURGE} -eq 1 ]] && UNINSTALL_ARGS+=(--purge)
  [[ ${DRY_RUN} -eq 1 ]] && UNINSTALL_ARGS+=(--dry-run)
  [[ ${JSON_OUT} -eq 1 ]] && UNINSTALL_ARGS+=(--json)

  UNINSTALL_BIN=""
  if UNINSTALL_BIN="$(resolve_uninstall_bin)"; then
    exec -- "${UNINSTALL_BIN}" "${UNINSTALL_ARGS[@]}"
  fi
  if [[ ${PURGE} -eq 0 && ${JSON_OUT} -eq 0 && ! -L "${PLUGINS_DIR}" ]] && is_legacy_repo_symlink "${PLUGIN_TARGET}"; then
    if [[ ${DRY_RUN} -eq 1 ]]; then
      printf '[dry-run] rm -- %q\n' "${PLUGIN_TARGET}"
      exit 0
    fi
    rm -- "${PLUGIN_TARGET}"
    printf 'dev-install.sh: removed legacy repo symlink %s\n' "${PLUGIN_TARGET}" >&2
    exit 0
  fi
  die "no agy-companion binary found to run uninstall; build companion-cli first (cargo build -p companion-cli)"
fi

# Fail fast before builds (and in --dry-run) on non-dir files, foreign symlinks, or non-plugin dirs.
if [[ -L "${PLUGIN_TARGET}" ]]; then
  if [[ ${FORCE} -eq 0 ]] && ! is_legacy_repo_symlink "${PLUGIN_TARGET}"; then
    die "plugin path '${PLUGIN_TARGET}' is a symlink to another target; re-run with --force to replace it"
  fi
elif [[ -e "${PLUGIN_TARGET}" ]]; then
  [[ -d "${PLUGIN_TARGET}" ]] || die "plugin path '${PLUGIN_TARGET}' exists and is not a directory"
  if [[ ${FORCE} -eq 0 ]]; then
    if [[ -L "${PLUGIN_TARGET}/plugin.json" || ! -f "${PLUGIN_TARGET}/plugin.json" || ! -r "${PLUGIN_TARGET}/plugin.json" ]] || \
       ! grep -Eq '"name"[[:space:]]*:[[:space:]]*"agy-companion"' "${PLUGIN_TARGET}/plugin.json"; then
      die "refusing to overwrite '${PLUGIN_TARGET}' without a valid agy-companion plugin.json; re-run with --force to override"
    fi
  fi
fi

if [[ ${NO_BUILD} -eq 0 ]]; then
  if [[ ${SKIP_UI} -eq 0 ]]; then
    [[ ${DRY_RUN} -eq 1 ]] || command -v npm >/dev/null 2>&1 || die "npm not found on PATH (re-run with --no-build or --skip-ui)"
    if [[ ! -d "${REPO_ROOT}/ui/node_modules" ]]; then
      run_build_cmd npm --prefix "${REPO_ROOT}/ui" ci
    fi
    run_build_cmd npm --prefix "${REPO_ROOT}/ui" run build
  fi
  [[ ${DRY_RUN} -eq 1 ]] || command -v cargo >/dev/null 2>&1 || die "cargo not found on PATH (re-run with --no-build)"
  if [[ ${RELEASE} -eq 1 ]]; then
    run_build_cmd cargo build --manifest-path "${REPO_ROOT}/Cargo.toml" --release -p companion-cli
  else
    run_build_cmd cargo build --manifest-path "${REPO_ROOT}/Cargo.toml" -p companion-cli
  fi
fi

# Re-verify legacy repo symlink immediately before passing --force (no build-duration TOCTOU).
AUTO_MIGRATE_SYMLINK=0
if is_legacy_repo_symlink "${PLUGIN_TARGET}"; then
  AUTO_MIGRATE_SYMLINK=1
fi

INSTALL_ARGS=(install)
[[ ${CONFIG_SET} -eq 1 ]] && INSTALL_ARGS+=(--config "${CONFIG_ARG}")
[[ ${PLUGINS_DIR_EXPLICIT} -eq 1 ]] && INSTALL_ARGS+=(--plugins-dir "${PLUGINS_DIR}")
[[ ${NO_LAUNCHER_HOOK} -eq 1 ]] && INSTALL_ARGS+=(--no-launcher-hook)
[[ ${NO_START_PROXY} -eq 1 ]] && INSTALL_ARGS+=(--no-start-proxy)
if [[ ${FORCE} -eq 1 || ${AUTO_MIGRATE_SYMLINK} -eq 1 ]]; then
  INSTALL_ARGS+=(--force)
fi
[[ ${JSON_OUT} -eq 1 ]] && INSTALL_ARGS+=(--json)

if [[ ${DRY_RUN} -eq 1 ]]; then
  if [[ ${AUTO_MIGRATE_SYMLINK} -eq 1 ]]; then
    printf '[dry-run] migrating legacy repo symlink %s -> %s to deployed plugin directory\n' "${PLUGIN_TARGET}" "${PLUGIN_SRC}"
  fi
  printf '[dry-run] %q' "${BUILT_BIN}"
  printf ' %q' "${INSTALL_ARGS[@]}"
  printf '\n'
  exit 0
fi

is_usable_bin "${BUILT_BIN}" || die "compiled binary not found or not executable at ${BUILT_BIN}; run without --no-build or build companion-cli first"

if [[ ${AUTO_MIGRATE_SYMLINK} -eq 1 ]]; then
  printf 'dev-install.sh: migrating legacy repo symlink %s to deployed plugin directory\n' "${PLUGIN_TARGET}" >&2
fi

exec -- "${BUILT_BIN}" "${INSTALL_ARGS[@]}"
