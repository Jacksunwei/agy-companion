#!/usr/bin/env bash
set -euo pipefail
unset CDPATH

GITHUB_REPO="Jacksunwei/agy-companion"
BINARY_NAME="agy-companion"

TMP_DIR=""
STAGED_BIN=""
STAGED_VER=""

log_err() {
  printf 'agy-companion launcher error: %s\n' "$*" >&2
}

log_info() {
  printf 'agy-companion launcher: %s\n' "$*" >&2
}

cleanup() {
  if [[ -n "${STAGED_BIN:-}" && -e "${STAGED_BIN}" ]]; then
    rm -f -- "${STAGED_BIN}" >/dev/null 2>&1 || true
  fi
  if [[ -n "${STAGED_VER:-}" && -e "${STAGED_VER}" ]]; then
    rm -f -- "${STAGED_VER}" >/dev/null 2>&1 || true
  fi
  if [[ -n "${TMP_DIR:-}" && -d "${TMP_DIR}" ]]; then
    rm -rf -- "${TMP_DIR}" >/dev/null 2>&1 || true
  fi
  STAGED_BIN=""
  STAGED_VER=""
  TMP_DIR=""
}

is_executable_file() {
  local path="$1"
  [[ -n "${path}" && -f "${path}" && -x "${path}" ]]
}

is_usable_binary() {
  local candidate="$1"
  local self_path="$2"
  is_executable_file "${candidate}" && ! [[ "${candidate}" -ef "${self_path}" ]]
}

scrub_injected_proxy_env() {
  local prefix="${JETSKI_MCP_ENV_RESTORE_PREFIX:-}"
  case "${prefix}" in
    [abcdefghijklmnopqrstuvwxyzABCDEFGHIJKLMNOPQRSTUVWXYZ_]*)
      case "${prefix}" in
        *[!abcdefghijklmnopqrstuvwxyzABCDEFGHIJKLMNOPQRSTUVWXYZ0123456789_]*) prefix="" ;;
      esac
      ;;
    *) prefix="" ;;
  esac
  if [[ -n "${prefix}" ]]; then
    if [[ "${AGY_COMPANION_ADDED_PREFIX_CLOUD_CODE:-}" == "1" ]]; then
      unset "${prefix}CLOUD_CODE_URL"
    fi
    if [[ "${AGY_COMPANION_ADDED_PREFIX_GEMINI_BASE:-}" == "1" ]]; then
      unset "${prefix}GOOGLE_GEMINI_BASE_URL"
    fi
  fi

  unset CLOUD_CODE_URL GOOGLE_GEMINI_BASE_URL \
    AGY_COMPANION_HOOK_ACTIVE AGY_COMPANION_INJECTED_URL \
    AGY_COMPANION_ORIG_CLOUD_CODE_URL AGY_COMPANION_ORIG_GOOGLE_GEMINI_BASE_URL \
    AGY_COMPANION_ORIG_RESTORE_PREFIX AGY_COMPANION_ORIG_RESTORE_VARS \
    AGY_COMPANION_ADDED_PREFIX_CLOUD_CODE AGY_COMPANION_ADDED_PREFIX_GEMINI_BASE \
    JETSKI_MCP_ENV_RESTORE_PREFIX JETSKI_MCP_ENV_RESTORE_VARS

  # Scrub GEMINI_API_KEY only when it equals the local placeholder sentinel;
  # preserve any real user GEMINI_API_KEY and never touch GOOGLE_API_KEY.
  local trimmed_key="${GEMINI_API_KEY:-}"
  trimmed_key="${trimmed_key#"${trimmed_key%%[![:space:]]*}"}"
  trimmed_key="${trimmed_key%"${trimmed_key##*[![:space:]]}"}"
  if [[ "${trimmed_key}" == "agy-companion-local" ]]; then
    unset GEMINI_API_KEY
  fi
}

exec_clean() {
  local target_bin="$1"
  shift
  scrub_injected_proxy_env
  exec -- "${target_bin}" ${1+"$@"}
}

resolve_script_path() {
  local target="$1"
  local max_hops=40
  local hops=0
  local dir=""
  local link=""
  local base=""
  local resolved=""

  while [[ -L "${target}" ]]; do
    hops=$((hops + 1))
    if [[ ${hops} -gt ${max_hops} ]]; then
      log_err "symlink loop detected while resolving launcher path (exceeded ${max_hops} hops): $1"
      return 1
    fi
    dir="$(cd -P -- "$(dirname -- "${target}")" >/dev/null 2>&1 && pwd -P)" || {
      log_err "failed to resolve directory for launcher path: ${target}"
      return 1
    }
    link="$(readlink -- "${target}")" || {
      log_err "failed to read symlink target: ${target}"
      return 1
    }
    case "${link}" in
      /*) target="${link}" ;;
      *)  target="${dir}/${link}" ;;
    esac
  done

  dir="$(cd -P -- "$(dirname -- "${target}")" >/dev/null 2>&1 && pwd -P)" || {
    log_err "failed to resolve parent directory for launcher path: ${target}"
    return 1
  }
  base="$(basename -- "${target}")" || {
    log_err "failed to resolve basename for launcher path: ${target}"
    return 1
  }
  if [[ "${dir}" == "/" ]]; then
    resolved="/${base}"
  else
    resolved="${dir}/${base}"
  fi
  if [[ ! -f "${resolved}" ]]; then
    log_err "resolved launcher path does not exist: ${resolved}"
    return 1
  fi
  printf '%s\n' "${resolved}"
}

validate_version() {
  local version="$1"
  local version_re='^[0-9]+\.[0-9]+\.[0-9]+([.-][0-9A-Za-z._-]+)?$'
  if [[ -z "${version}" || ! "${version}" =~ ${version_re} || "${version}" == *".."* ]]; then
    log_err "invalid plugin version string: '${version}'"
    return 1
  fi
}

parse_plugin_version() {
  local manifest="$1"
  if [[ ! -f "${manifest}" || ! -r "${manifest}" ]]; then
    log_err "plugin manifest not found or unreadable: ${manifest}"
    return 1
  fi
  local version=""
  version="$(
    { tr '\r\n' '  ' < "${manifest}"; printf '\n'; } | awk '
      {
        if (match($0, /(^|[{,[:space:]])"version"[[:space:]]*:[[:space:]]*"[^"]*"/)) {
          seg = substr($0, RSTART, RLENGTH)
          sub(/^[{,[:space:]]*"version"[[:space:]]*:[[:space:]]*"/, "", seg)
          sub(/"$/, "", seg)
          print seg
          exit
        }
      }
    '
  )" || {
    log_err "failed to parse plugin manifest: ${manifest}"
    return 1
  }
  validate_version "${version}" || return 1
  printf '%s\n' "${version}"
}

detect_target() {
  local os arch
  os="$(uname -s)" || {
    log_err "failed to detect operating system via uname -s"
    return 1
  }
  arch="$(uname -m)" || {
    log_err "failed to detect CPU architecture via uname -m"
    return 1
  }
  case "${os}:${arch}" in
    Darwin:arm64)            printf 'darwin-arm64\n' ;;
    Darwin:x86_64)           printf 'darwin-x64\n' ;;
    Linux:x86_64)            printf 'linux-x64\n' ;;
    Linux:aarch64|Linux:arm64) printf 'linux-arm64\n' ;;
    *)
      log_err "unsupported platform ${os}/${arch} (supported: darwin-arm64, darwin-x64, linux-x64, linux-arm64)"
      return 1
      ;;
  esac
}

compute_sha256() {
  local file="$1"
  local file_dir file_base raw hash
  file_dir="$(dirname -- "${file}")" || {
    log_err "failed to determine directory for ${file}"
    return 1
  }
  file_base="$(basename -- "${file}")" || {
    log_err "failed to determine filename for ${file}"
    return 1
  }

  if command -v sha256sum >/dev/null 2>&1; then
    raw="$(cd -P -- "${file_dir}" >/dev/null && sha256sum -- "${file_base}")" || {
      log_err "failed to compute SHA-256 for ${file_base}"
      return 1
    }
  elif command -v shasum >/dev/null 2>&1; then
    raw="$(cd -P -- "${file_dir}" >/dev/null && shasum -a 256 -- "${file_base}")" || {
      log_err "failed to compute SHA-256 for ${file_base}"
      return 1
    }
  else
    log_err "neither sha256sum nor shasum is available; cannot verify download integrity"
    return 1
  fi

  hash="$(printf '%s\n' "${raw}" | awk '{print $1}' | tr 'A-F' 'a-f')"
  local hex_re='^[0-9a-f]{64}$'
  if [[ ! "${hash}" =~ ${hex_re} ]]; then
    log_err "unexpected SHA-256 output while hashing ${file_base}"
    return 1
  fi
  printf '%s\n' "${hash}"
}

lookup_expected_sha256() {
  local checksums_file="$1"
  local target_filename="$2"
  case "${checksums_file}" in
    -*) checksums_file="./${checksums_file}" ;;
  esac
  if [[ ! -f "${checksums_file}" || ! -r "${checksums_file}" ]]; then
    log_err "checksums file not found or unreadable: ${checksums_file}"
    return 1
  fi

  local matches=""
  matches="$(
    awk -v fname="${target_filename}" '
      {
        sub(/\r$/, "")
        if ($0 ~ /^[[:space:]]*$/ || $0 ~ /^[[:space:]]*#/) next
        if (match($0, /^[0-9A-Fa-f]+[[:space:]]+\*?/)) {
          prefix = substr($0, 1, RLENGTH)
          rest = substr($0, RLENGTH + 1)
          sub(/[[:space:]]+\*?$/, "", prefix)
          sub(/^\.\//, "", rest)
          if (length(prefix) == 64 && rest == fname) {
            print tolower(prefix)
          }
        }
      }
    ' < "${checksums_file}"
  )" || {
    log_err "failed to parse checksums file: ${checksums_file}"
    return 1
  }

  if [[ -z "${matches}" ]]; then
    log_err "checksum entry for '${target_filename}' not found in checksums.txt"
    return 1
  fi

  local line_count=""
  line_count="$(printf '%s\n' "${matches}" | wc -l | tr -d '[:space:]')"
  if [[ "${line_count}" != "1" ]]; then
    log_err "ambiguous duplicate checksum entries (${line_count}) for '${target_filename}' in checksums.txt"
    return 1
  fi

  local hex_re='^[0-9a-f]{64}$'
  if [[ ! "${matches}" =~ ${hex_re} ]]; then
    log_err "invalid SHA-256 format in checksums.txt for '${target_filename}'"
    return 1
  fi
  printf '%s\n' "${matches}"
}

verify_sha256() {
  local archive_path="$1"
  local checksums_file="$2"
  local archive_name="$3"

  local expected_hash actual_hash
  expected_hash="$(lookup_expected_sha256 "${checksums_file}" "${archive_name}")" || return 1
  actual_hash="$(compute_sha256 "${archive_path}")" || return 1

  if [[ "${actual_hash}" != "${expected_hash}" ]]; then
    log_err "SHA-256 mismatch for ${archive_name} (expected ${expected_hash}, got ${actual_hash})"
    return 1
  fi
}

extract_verified_binary() {
  local tarball="$1"
  local extract_dir="$2"

  if ! command -v tar >/dev/null 2>&1; then
    log_err "tar is required to extract release archives"
    return 1
  fi

  case "${tarball}" in
    -*) tarball="./${tarball}" ;;
  esac
  case "${extract_dir}" in
    -*) extract_dir="./${extract_dir}" ;;
  esac

  local listing=""
  listing="$(tar -tzf - < "${tarball}")" || {
    log_err "failed to inspect release archive: ${tarball}"
    return 1
  }

  local unsafe_entries=""
  unsafe_entries="$(
    printf '%s\n' "${listing}" | awk '
      {
        sub(/\r$/, "")
        if ($0 ~ /^\// || $0 ~ /(^|\/)\.\.(\/|$)/) {
          print $0
        }
      }
    '
  )" || {
    log_err "failed to validate paths in release archive: ${tarball}"
    return 1
  }
  if [[ -n "${unsafe_entries}" ]]; then
    log_err "release archive contains unsafe path entry (absolute path or parent traversal): ${tarball}"
    return 1
  fi

  local members=""
  members="$(
    printf '%s\n' "${listing}" | awk -v bin="${BINARY_NAME}" '
      {
        sub(/\r$/, "")
        if ($0 == bin || $0 == ("./" bin)) {
          print $0
        }
      }
    '
  )" || {
    log_err "failed to scan release archive entries: ${tarball}"
    return 1
  }

  if [[ -z "${members}" ]]; then
    log_err "release archive does not contain expected binary '${BINARY_NAME}'"
    return 1
  fi

  local member_count=""
  member_count="$(printf '%s\n' "${members}" | wc -l | tr -d '[:space:]')" || {
    log_err "failed to count binary entries in release archive: ${tarball}"
    return 1
  }
  if [[ "${member_count}" != "1" ]]; then
    log_err "release archive contains ambiguous duplicate '${BINARY_NAME}' entries (${member_count})"
    return 1
  fi

  mkdir -p -- "${extract_dir}" || {
    log_err "failed to create extraction directory: ${extract_dir}"
    return 1
  }
  chmod -- 0700 "${extract_dir}" || {
    log_err "failed to set permissions on extraction directory: ${extract_dir}"
    return 1
  }
  if ! tar -xzf - -C "${extract_dir}" --no-same-owner --no-same-permissions -- "${members}" < "${tarball}" >/dev/null; then
    log_err "failed to extract '${BINARY_NAME}' from release archive"
    return 1
  fi

  local extracted_bin="${extract_dir}/${BINARY_NAME}"
  if [[ -L "${extracted_bin}" || ! -f "${extracted_bin}" || ! -s "${extracted_bin}" ]]; then
    log_err "extracted '${BINARY_NAME}' is missing, empty, or not a regular file"
    return 1
  fi

  printf '%s\n' "${extracted_bin}"
}

install_binary_atomically() {
  local extracted_bin="$1"
  local cache_bin_dir="$2"
  local version="$3"

  case "${cache_bin_dir}" in
    -*) cache_bin_dir="./${cache_bin_dir}" ;;
  esac

  local cache_bin="${cache_bin_dir}/${BINARY_NAME}"
  local version_file="${cache_bin_dir}/.version"

  mkdir -p -- "${cache_bin_dir}" || {
    log_err "failed to create cache directory: ${cache_bin_dir}"
    return 1
  }

  STAGED_BIN="$(mktemp -- "${cache_bin_dir}/.${BINARY_NAME}.tmp.XXXXXX")" || {
    STAGED_BIN=""
    log_err "failed to create temporary staging file in ${cache_bin_dir}"
    return 1
  }
  if [[ -z "${STAGED_BIN}" || ! -f "${STAGED_BIN}" ]]; then
    STAGED_BIN=""
    log_err "temporary staging file was not created in ${cache_bin_dir}"
    return 1
  fi
  cp -- "${extracted_bin}" "${STAGED_BIN}" || {
    log_err "failed to copy binary into staging file: ${STAGED_BIN}"
    return 1
  }
  chmod -- 0755 "${STAGED_BIN}" || {
    log_err "failed to mark staged binary executable: ${STAGED_BIN}"
    return 1
  }
  mv -f -- "${STAGED_BIN}" "${cache_bin}" || {
    log_err "failed to install binary atomically to ${cache_bin}"
    return 1
  }
  STAGED_BIN=""

  STAGED_VER="$(mktemp -- "${cache_bin_dir}/.version.tmp.XXXXXX")" || {
    STAGED_VER=""
    log_err "failed to create temporary version file in ${cache_bin_dir}"
    return 1
  }
  if [[ -z "${STAGED_VER}" || ! -f "${STAGED_VER}" ]]; then
    STAGED_VER=""
    log_err "temporary version file was not created in ${cache_bin_dir}"
    return 1
  fi
  printf '%s\n' "${version}" > "${STAGED_VER}" || {
    log_err "failed to write version metadata to ${STAGED_VER}"
    return 1
  }
  chmod -- 0644 "${STAGED_VER}" || {
    log_err "failed to set permissions on version metadata file: ${STAGED_VER}"
    return 1
  }
  mv -f -- "${STAGED_VER}" "${version_file}" || {
    log_err "failed to install version metadata atomically to ${version_file}"
    return 1
  }
  STAGED_VER=""
}

download_and_install() {
  local plugin_dir="$1"
  local cache_bin_dir="$2"

  if ! command -v curl >/dev/null 2>&1; then
    log_err "curl is required to download ${BINARY_NAME} (or install it on PATH / set AGY_COMPANION_BIN)"
    return 1
  fi

  local version target archive_name base_url tarball_url checksums_url
  version="$(parse_plugin_version "${plugin_dir}/plugin.json")" || return 1
  target="$(detect_target)" || return 1
  archive_name="${BINARY_NAME}-v${version}-${target}.tar.gz"
  base_url="https://github.com/${GITHUB_REPO}/releases/download/v${version}"
  tarball_url="${base_url}/${archive_name}"
  checksums_url="${base_url}/checksums.txt"

  trap 'cleanup; exit 130' INT
  trap 'cleanup; exit 143' TERM
  trap 'cleanup; exit 129' HUP
  trap cleanup EXIT

  local tmp_base="${TMPDIR:-/tmp}"
  tmp_base="${tmp_base%/}"
  case "${tmp_base}" in
    -*) tmp_base="./${tmp_base}" ;;
  esac
  if [[ -z "${tmp_base}" || "${tmp_base}" == "/" ]]; then
    TMP_DIR="$(mktemp -d -- "/${BINARY_NAME}.XXXXXX")" || {
      TMP_DIR=""
      log_err "failed to create temporary directory in /"
      return 1
    }
  else
    TMP_DIR="$(mktemp -d -- "${tmp_base}/${BINARY_NAME}.XXXXXX")" || {
      TMP_DIR=""
      log_err "failed to create temporary directory in ${tmp_base}"
      return 1
    }
  fi
  if [[ -z "${TMP_DIR}" || ! -d "${TMP_DIR}" ]]; then
    TMP_DIR=""
    log_err "temporary directory was not created in ${tmp_base:-/}"
    return 1
  fi
  chmod -- 0700 "${TMP_DIR}" || {
    log_err "failed to set permissions on temporary directory: ${TMP_DIR}"
    return 1
  }

  local tarball_path="${TMP_DIR}/${archive_name}"
  local checksums_path="${TMP_DIR}/checksums.txt"

  log_info "downloading ${archive_name} from GitHub Releases (v${version})..."
  if ! curl -fsSL \
    --proto '=https' \
    --proto-redir '=https' \
    --tlsv1.2 \
    --connect-timeout 15 \
    --max-time 120 \
    -o "${checksums_path}" \
    "${checksums_url}" >/dev/null; then
    log_err "failed to download checksums from ${checksums_url} (tip: for local dev run 'cargo build -p companion-cli' or set AGY_COMPANION_BIN)"
    return 1
  fi

  if ! curl -fsSL \
    --proto '=https' \
    --proto-redir '=https' \
    --tlsv1.2 \
    --connect-timeout 15 \
    --max-time 120 \
    -o "${tarball_path}" \
    "${tarball_url}" >/dev/null; then
    log_err "failed to download release archive from ${tarball_url} (tip: for local dev run 'cargo build -p companion-cli' or set AGY_COMPANION_BIN)"
    return 1
  fi

  verify_sha256 "${tarball_path}" "${checksums_path}" "${archive_name}" || return 1

  local extracted_bin=""
  extracted_bin="$(extract_verified_binary "${tarball_path}" "${TMP_DIR}/extract")" || return 1

  install_binary_atomically "${extracted_bin}" "${cache_bin_dir}" "${version}" || return 1
  cleanup
}

resolve_xdg_cache_bin_dir() {
  local xdg="${XDG_DATA_HOME:-}"
  local cache_root=""
  if [[ -n "${xdg}" ]]; then
    case "/${xdg}/" in
      */../*)
        log_err "XDG_DATA_HOME must not contain '..' traversal: ${xdg}"
        return 1
        ;;
    esac
    case "${xdg}" in
      /*) cache_root="${xdg%/}/${BINARY_NAME}" ;;
      *)  cache_root="" ;; # Relative XDG_DATA_HOME falls back to $HOME per S6.
    esac
  fi
  if [[ -z "${cache_root}" ]]; then
    if [[ -n "${HOME:-}" ]]; then
      cache_root="${HOME%/}/.local/share/${BINARY_NAME}"
    else
      return 1
    fi
  fi
  case "${cache_root}" in
    -*) cache_root="./${cache_root}" ;;
  esac
  printf '%s/bin\n' "${cache_root}"
}

resolve_cache_bin_dir() {
  local cache_root=""
  if [[ -n "${ANTIGRAVITY_EXECUTABLE_DATA_DIR:-}" ]]; then
    case "/${ANTIGRAVITY_EXECUTABLE_DATA_DIR}/" in
      */../*)
        log_err "ANTIGRAVITY_EXECUTABLE_DATA_DIR must not contain '..' traversal: ${ANTIGRAVITY_EXECUTABLE_DATA_DIR}"
        return 1
        ;;
    esac
    cache_root="${ANTIGRAVITY_EXECUTABLE_DATA_DIR%/}"
    case "${cache_root}" in
      -*) cache_root="./${cache_root}" ;;
    esac
    if [[ -z "${cache_root}" ]]; then
      printf '/bin\n'
    else
      printf '%s/bin\n' "${cache_root}"
    fi
    return 0
  fi
  if resolve_xdg_cache_bin_dir; then
    return 0
  fi
  log_err "cannot determine cache directory: neither ANTIGRAVITY_EXECUTABLE_DATA_DIR, valid XDG_DATA_HOME, nor HOME is set"
  return 1
}

try_exec_cached_bin() {
  local candidate_dir="$1"
  local plugin_dir="$2"
  local self_path="$3"
  shift 3

  local cand_bin="${candidate_dir}/${BINARY_NAME}"
  local cand_ver="${candidate_dir}/.version"

  if ! is_usable_binary "${cand_bin}" "${self_path}"; then
    return 1
  fi
  if [[ -L "${cand_ver}" || ! -f "${cand_ver}" || ! -r "${cand_ver}" ]]; then
    log_info "cached version metadata missing, non-regular, or unreadable (${cand_ver}); skipping cache at ${candidate_dir}"
    return 1
  fi
  local cached_version="" expected_version=""
  cached_version="$(tr -d '\r\n[:space:]' < "${cand_ver}")"
  expected_version="$(parse_plugin_version "${plugin_dir}/plugin.json")" || exit 1
  if [[ "${cached_version}" == "${expected_version}" ]]; then
    exec_clean "${cand_bin}" ${1+"$@"}
  fi
  log_info "cached binary version (${cached_version}) differs from plugin manifest (${expected_version}); skipping cache at ${candidate_dir}"
  return 1
}

main() {
  local script_path script_dir plugin_dir repo_root repo_prefix
  script_path="$(resolve_script_path "${BASH_SOURCE[0]:-$0}")" || exit 1
  script_dir="$(dirname -- "${script_path}")"
  plugin_dir="$(cd -P -- "${script_dir}/.." >/dev/null 2>&1 && pwd -P)" || {
    log_err "failed to resolve plugin directory from: ${script_dir}"
    exit 1
  }
  export AGY_COMPANION_PLUGIN_DIR="${plugin_dir}"
  repo_root="$(cd -P -- "${plugin_dir}/.." >/dev/null 2>&1 && pwd -P)" || {
    log_err "failed to resolve parent directory from: ${plugin_dir}"
    exit 1
  }
  repo_prefix="${repo_root%/}"

  # Step 1: Explicit AGY_COMPANION_BIN override (fail hard if set & invalid).
  if [[ -n "${AGY_COMPANION_BIN:-}" ]]; then
    local override_bin="${AGY_COMPANION_BIN}"
    case "${override_bin}" in
      */*) ;;
      *)   override_bin="./${override_bin}" ;;
    esac
    if ! is_executable_file "${override_bin}"; then
      log_err "AGY_COMPANION_BIN is set to '${AGY_COMPANION_BIN}', which is not an executable regular file"
      exit 1
    fi
    if [[ "${override_bin}" -ef "${script_path}" ]]; then
      log_err "AGY_COMPANION_BIN points to launcher script itself; refusing recursive exec"
      exit 1
    fi
    exec_clean "${override_bin}" ${1+"$@"}
  fi

  # Step 2: Symlink-aware linked dev checkout auto-detection (target/{debug,release}).
  local debug_bin="${repo_prefix}/target/debug/${BINARY_NAME}"
  local release_bin="${repo_prefix}/target/release/${BINARY_NAME}"
  local has_debug=0
  local has_release=0
  if is_usable_binary "${debug_bin}" "${script_path}"; then
    has_debug=1
  fi
  if is_usable_binary "${release_bin}" "${script_path}"; then
    has_release=1
  fi
  if [[ ${has_debug} -eq 1 && ${has_release} -eq 1 ]]; then
    if [[ "${release_bin}" -nt "${debug_bin}" ]]; then
      exec_clean "${release_bin}" ${1+"$@"}
    else
      exec_clean "${debug_bin}" ${1+"$@"}
    fi
  elif [[ ${has_debug} -eq 1 ]]; then
    exec_clean "${debug_bin}" ${1+"$@"}
  elif [[ ${has_release} -eq 1 ]]; then
    exec_clean "${release_bin}" ${1+"$@"}
  fi

  # Step 3: Version-matched installed/cached binary (checked BEFORE unversioned $PATH).
  # Prefer the explicit XDG install dir over ANTIGRAVITY_EXECUTABLE_DATA_DIR auto-download cache
  # when both match plugin.json version; keep cache_bin_dir as the Step 5 download target.
  local cache_bin_dir cache_bin
  cache_bin_dir="$(resolve_cache_bin_dir)" || exit 1
  cache_bin="${cache_bin_dir}/${BINARY_NAME}"

  if [[ -n "${ANTIGRAVITY_EXECUTABLE_DATA_DIR:-}" ]]; then
    local xdg_bin_dir=""
    xdg_bin_dir="$(resolve_xdg_cache_bin_dir 2>/dev/null || true)"
    if [[ -n "${xdg_bin_dir}" && "${xdg_bin_dir}" != "${cache_bin_dir}" ]]; then
      try_exec_cached_bin "${xdg_bin_dir}" "${plugin_dir}" "${script_path}" ${1+"$@"} || true
    fi
  fi
  try_exec_cached_bin "${cache_bin_dir}" "${plugin_dir}" "${script_path}" ${1+"$@"} || true

  # Step 4: Installed binary on $PATH (with self-exec guard).
  local path_bin=""
  path_bin="$(command -v "${BINARY_NAME}" 2>/dev/null || true)"
  if [[ -n "${path_bin}" ]]; then
    if is_usable_binary "${path_bin}" "${script_path}"; then
      exec_clean "${path_bin}" ${1+"$@"}
    elif [[ "${path_bin}" -ef "${script_path}" ]]; then
      log_info "skipping PATH entry '${path_bin}' because it resolves to launcher script itself"
    fi
  fi

  # Step 5: HTTPS + SHA-256 verified GitHub Releases download & atomic cache install.
  download_and_install "${plugin_dir}" "${cache_bin_dir}" || exit 1

  if ! is_usable_binary "${cache_bin}" "${script_path}"; then
    log_err "installed binary is not executable: ${cache_bin}"
    exit 1
  fi
  cleanup
  exec_clean "${cache_bin}" ${1+"$@"}
}

if [[ "${BASH_SOURCE[0]}" == "$0" ]]; then
  main ${1+"$@"}
fi
