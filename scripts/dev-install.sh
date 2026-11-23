#!/usr/bin/env bash
set -euo pipefail
unset CDPATH

REPO_ROOT="$(cd -P -- "$(dirname -- "${BASH_SOURCE[0]}")/.." >/dev/null 2>&1 && pwd -P)"
PLUGIN_SRC="${REPO_ROOT}/plugin"

NO_BUILD=0
RELEASE=0
DRY_RUN=0
UNINSTALL=0
PLUGINS_DIR_ARG=""
PLUGINS_DIR_SET=0

usage() {
  cat <<'EOF'
Usage: scripts/dev-install.sh [OPTIONS]

Build agy-companion locally and link ./plugin into the Antigravity plugins
directory without modifying user Antigravity settings.

Options:
  --no-build            Skip UI and Cargo builds; link ./plugin only
  --release             Build release binary (cargo build --release -p companion-cli)
  --dry-run             Validate target safety and print planned actions only
  --uninstall           Safely remove the agy-companion symlink if it points here
  --plugins-dir <dir>   Override target plugins directory
                        (default: $AGY_PLUGINS_DIR or ~/.gemini/config/plugins)
  -h, --help            Show this help message and exit
EOF
}

die() {
  printf 'dev-install.sh: error: %s\n' "$1" >&2
  exit 1
}

is_blank() {
  local val="$1"
  [[ -z "${val//[[:space:]]/}" ]]
}

resolve_symlink_target_dir() {
  local link_path="$1"
  local raw_target=""
  raw_target="$(readlink -- "${link_path}")" || return 1

  if [[ "${raw_target}" == "${PLUGIN_SRC}" ]]; then
    printf '%s\n' "${PLUGIN_SRC}"
    return 0
  fi

  local candidate=""
  case "${raw_target}" in
    /*)
      candidate="${raw_target}"
      ;;
    *)
      local link_parent=""
      link_parent="$(cd -P -- "$(dirname -- "${link_path}")" >/dev/null 2>&1 && pwd -P)" || return 1
      candidate="${link_parent}/${raw_target}"
      ;;
  esac

  if [[ -d "${candidate}" ]]; then
    (cd -P -- "${candidate}" >/dev/null 2>&1 && pwd -P) || return 1
    return 0
  fi

  printf '%s\n' "${raw_target}"
  return 0
}

parse_args() {
  while [[ $# -gt 0 ]]; do
    case "$1" in
      --no-build)
        NO_BUILD=1
        shift
        ;;
      --release)
        RELEASE=1
        shift
        ;;
      --dry-run)
        DRY_RUN=1
        shift
        ;;
      --uninstall)
        UNINSTALL=1
        shift
        ;;
      --plugins-dir)
        if [[ $# -lt 2 ]]; then
          die "missing value for --plugins-dir"
        fi
        if is_blank "$2" || [[ "$2" == -* ]]; then
          die "invalid value for --plugins-dir: '$2'"
        fi
        PLUGINS_DIR_ARG="$2"
        PLUGINS_DIR_SET=1
        shift 2
        ;;
      --plugins-dir=*)
        local val="${1#--plugins-dir=}"
        if is_blank "${val}"; then
          die "missing value for --plugins-dir="
        fi
        PLUGINS_DIR_ARG="${val}"
        PLUGINS_DIR_SET=1
        shift
        ;;
      -h|--help)
        usage
        exit 0
        ;;
      *)
        printf 'dev-install.sh: error: unknown argument: %s\n\n' "$1" >&2
        usage >&2
        exit 1
        ;;
    esac
  done
}

parse_args ${1+"$@"}

if [[ ! -d "${PLUGIN_SRC}" || ! -f "${PLUGIN_SRC}/plugin.json" ]]; then
  die "plugin bundle not found at ${PLUGIN_SRC}"
fi

PLUGINS_DIR=""
if [[ ${PLUGINS_DIR_SET} -eq 1 ]]; then
  PLUGINS_DIR="${PLUGINS_DIR_ARG}"
elif [[ -n "${AGY_PLUGINS_DIR+x}" ]]; then
  if is_blank "${AGY_PLUGINS_DIR}"; then
    die "AGY_PLUGINS_DIR is set but empty"
  fi
  PLUGINS_DIR="${AGY_PLUGINS_DIR}"
else
  if [[ -z "${HOME:-}" ]]; then
    die "HOME is not set; pass --plugins-dir <dir> explicitly"
  fi
  PLUGINS_DIR="${HOME%/}/.gemini/config/plugins"
fi

if is_blank "${PLUGINS_DIR}"; then
  die "plugins directory must not be empty"
fi

while [[ "${PLUGINS_DIR}" != "/" && "${PLUGINS_DIR}" == */ ]]; do
  PLUGINS_DIR="${PLUGINS_DIR%/}"
done

case "${PLUGINS_DIR}" in
  -*)
    PLUGINS_DIR="./${PLUGINS_DIR}"
    ;;
esac

if [[ ( -e "${PLUGINS_DIR}" || -L "${PLUGINS_DIR}" ) && ! -d "${PLUGINS_DIR}" ]]; then
  die "plugins directory path exists and is not a directory: ${PLUGINS_DIR}"
fi

if [[ "${PLUGINS_DIR}" == "/" ]]; then
  LINK_PATH="/agy-companion"
else
  LINK_PATH="${PLUGINS_DIR}/agy-companion"
fi

if [[ ${UNINSTALL} -eq 1 ]]; then
  if [[ ! -e "${LINK_PATH}" && ! -L "${LINK_PATH}" ]]; then
    printf 'Nothing to uninstall: %s does not exist.\n' "${LINK_PATH}"
    exit 0
  fi

  if [[ ! -L "${LINK_PATH}" ]]; then
    die "refusing to remove non-symlink path: ${LINK_PATH}"
  fi

  raw_target="$(readlink -- "${LINK_PATH}")"
  resolved_target="$(resolve_symlink_target_dir "${LINK_PATH}" || true)"
  if [[ "${raw_target}" != "${PLUGIN_SRC}" && "${resolved_target}" != "${PLUGIN_SRC}" ]]; then
    die "refusing to remove symlink pointing elsewhere: ${raw_target} (expected ${PLUGIN_SRC})"
  fi

  if [[ ${DRY_RUN} -eq 1 ]]; then
    printf '[dry-run] rm -- %s\n' "${LINK_PATH}"
    exit 0
  fi

  rm -- "${LINK_PATH}"
  printf 'Removed dev plugin symlink: %s\n' "${LINK_PATH}"
  exit 0
fi

# Install precondition check: run BEFORE any build and during --dry-run.
if [[ -e "${LINK_PATH}" && ! -L "${LINK_PATH}" ]]; then
  die "refusing to overwrite existing non-symlink path: ${LINK_PATH}; move or uninstall it first"
fi

if [[ ${NO_BUILD} -eq 0 ]]; then
  if [[ ${DRY_RUN} -eq 0 ]]; then
    if ! command -v npm >/dev/null 2>&1; then
      die "npm not found on PATH (re-run with --no-build to skip builds)"
    fi
    if ! command -v cargo >/dev/null 2>&1; then
      die "cargo not found on PATH (re-run with --no-build to skip builds)"
    fi
  fi

  if [[ ! -d "${REPO_ROOT}/ui/node_modules" ]]; then
    if [[ ${DRY_RUN} -eq 1 ]]; then
      printf '[dry-run] npm --prefix %s ci\n' "${REPO_ROOT}/ui"
    else
      npm --prefix "${REPO_ROOT}/ui" ci
    fi
  fi

  if [[ ${DRY_RUN} -eq 1 ]]; then
    printf '[dry-run] npm --prefix %s run build\n' "${REPO_ROOT}/ui"
  else
    npm --prefix "${REPO_ROOT}/ui" run build
  fi

  if [[ ${RELEASE} -eq 1 ]]; then
    if [[ ${DRY_RUN} -eq 1 ]]; then
      printf '[dry-run] cargo build --manifest-path %s --release -p companion-cli\n' "${REPO_ROOT}/Cargo.toml"
    else
      cargo build --manifest-path "${REPO_ROOT}/Cargo.toml" --release -p companion-cli
    fi
  else
    if [[ ${DRY_RUN} -eq 1 ]]; then
      printf '[dry-run] cargo build --manifest-path %s -p companion-cli\n' "${REPO_ROOT}/Cargo.toml"
    else
      cargo build --manifest-path "${REPO_ROOT}/Cargo.toml" -p companion-cli
    fi
  fi
fi

if [[ ${DRY_RUN} -eq 1 ]]; then
  if [[ ! -d "${PLUGINS_DIR}" ]]; then
    printf '[dry-run] mkdir -p -- %s\n' "${PLUGINS_DIR}"
  fi
  if [[ -L "${LINK_PATH}" ]]; then
    printf '[dry-run] ln -sfn -- %s %s (update existing symlink)\n' "${PLUGIN_SRC}" "${LINK_PATH}"
  else
    printf '[dry-run] ln -s -- %s %s\n' "${PLUGIN_SRC}" "${LINK_PATH}"
  fi
  exit 0
fi

mkdir -p -- "${PLUGINS_DIR}"
ln -sfn -- "${PLUGIN_SRC}" "${LINK_PATH}"

if [[ ${RELEASE} -eq 1 ]]; then
  EXPECTED_BIN="${REPO_ROOT}/target/release/agy-companion"
else
  EXPECTED_BIN="${REPO_ROOT}/target/debug/agy-companion"
fi

printf 'Linked dev plugin:\n'
printf '  Plugin symlink : %s -> %s\n' "${LINK_PATH}" "${PLUGIN_SRC}"
printf '  Local binary   : %s (auto-detected by plugin/scripts/run-companion.sh)\n' "${EXPECTED_BIN}"
printf '  MCP live reload: Settings > MCP Servers > agy-companion_companion > Restart\n'
printf '                   (or RefreshMcpServers RPC with {"serverName": "agy-companion_companion"})\n'
printf '  UI live reload : export AGY_COMPANION_UI_HTML="%s/ui/dist/index.html"\n' "${REPO_ROOT}"
printf '  Sidecar logs   : ~/.gemini/antigravity/sidecar_data/agy-companion/proxy/logs/\n'
