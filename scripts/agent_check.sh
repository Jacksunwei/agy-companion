#!/usr/bin/env bash
set -euo pipefail
unset CDPATH

REPO_ROOT="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." >/dev/null 2>&1 && pwd -P)"
cd -- "${REPO_ROOT}" >/dev/null

TOTAL_CHECKS=7
FAILURES=0

pass() {
  printf 'PASS %s\n' "$1"
}

fail() {
  printf 'FAIL %s\n' "$1"
  FAILURES=$((FAILURES + 1))
}

# ---------------------------------------------------------------------------
# Check 1: Required Antigravity-native governance files exist (and no GEMINI.md)
# ---------------------------------------------------------------------------
check_governance_files() {
  local required=(
    "LICENSE"
    "AGENTS.md"
    ".agents/rules/workflow.md"
    ".github/PULL_REQUEST_TEMPLATE.md"
  )
  local missing=()
  for f in "${required[@]}"; do
    if [[ ! -f "${REPO_ROOT}/${f}" ]]; then
      missing+=("${f}")
    fi
  done

  if [[ ${#missing[@]} -gt 0 ]]; then
    fail "[1/${TOTAL_CHECKS}] Governance files: missing ${missing[*]}"
  elif [[ -f "${REPO_ROOT}/GEMINI.md" ]]; then
    fail "[1/${TOTAL_CHECKS}] Governance files: redundant GEMINI.md present (use AGENTS.md + .agents/rules/)"
  else
    pass "[1/${TOTAL_CHECKS}] Governance files: all ${#required[@]} required files present (Antigravity-native layout)"
  fi
}

# ---------------------------------------------------------------------------
# Check 2: No forbidden local/secret files tracked in git
# ---------------------------------------------------------------------------
check_no_forbidden_tracked_files() {
  local tracked=""
  tracked="$(git ls-files | grep -E '(^|/)\.DS_Store$|(^|/)\.env($|\.)|(^|/)\.antigravitycli/|(^|/)\.remember/|^target/|(^|/)node_modules/|\.(pem|key)$' | grep -vE '(^|/)\.env\.example$' || true)"
  if [[ -n "${tracked}" ]]; then
    fail "[2/${TOTAL_CHECKS}] Tracked file hygiene: forbidden files tracked in git"
    printf '%s\n' "${tracked}" | sed 's/^/  - /'
  else
    pass "[2/${TOTAL_CHECKS}] Tracked file hygiene: 0 forbidden or secret files tracked"
  fi
}

# ---------------------------------------------------------------------------
# Check 3: Zero AI attribution in commit history
# ---------------------------------------------------------------------------
check_zero_ai_attribution() {
  local range="HEAD"
  if git rev-parse --verify origin/main >/dev/null 2>&1; then
    local base
    base="$(git merge-base origin/main HEAD 2>/dev/null || true)"
    if [[ -n "${base}" && "${base}" != "$(git rev-parse HEAD)" ]]; then
      range="${base}..HEAD"
    fi
  fi

  local hits=""
  hits="$(git log "${range}" --format='%h %B' 2>/dev/null | grep -iE 'co-authored-by:.*(claude|gemini|copilot|openai|chatgpt|anthropic|antigravity|cursor|ai|bot)|generated with (claude|gemini|ai|copilot)' || true)"
  if [[ -n "${hits}" ]]; then
    fail "[3/${TOTAL_CHECKS}] Commit hygiene: AI attribution detected in commit messages"
    printf '%s\n' "${hits}" | sed 's/^/  - /'
  else
    pass "[3/${TOTAL_CHECKS}] Commit hygiene: 0 AI attribution lines found"
  fi
}

# ---------------------------------------------------------------------------
# Check 4: Shell scripts pass bash syntax check and launcher is executable
# ---------------------------------------------------------------------------
check_shell_scripts() {
  local shell_failed=0
  local launcher="${REPO_ROOT}/plugin/scripts/run-companion.sh"

  if [[ ! -d "${REPO_ROOT}/plugin/scripts" ]]; then
    fail "[4/${TOTAL_CHECKS}] Shell syntax: missing plugin/scripts directory"
    shell_failed=1
  elif [[ ! -f "${launcher}" ]]; then
    fail "[4/${TOTAL_CHECKS}] Shell syntax: missing plugin/scripts/run-companion.sh"
    shell_failed=1
  elif [[ ! -x "${launcher}" ]]; then
    fail "[4/${TOTAL_CHECKS}] Shell syntax: plugin/scripts/run-companion.sh is not executable (expected 0755)"
    shell_failed=1
  fi

  local search_dirs=("${REPO_ROOT}/scripts")
  if [[ -d "${REPO_ROOT}/plugin/scripts" ]]; then
    search_dirs+=("${REPO_ROOT}/plugin/scripts")
  fi

  local bad=()
  local count=0
  while IFS= read -r script; do
    count=$((count + 1))
    if ! bash -n "${script}"; then
      bad+=("${script}")
    elif [[ -x /bin/bash ]] && ! /bin/bash -n "${script}"; then
      bad+=("${script}")
    fi
  done < <(find "${search_dirs[@]}" -type f -name '*.sh' | sort)

  if [[ ${count} -eq 0 ]]; then
    fail "[4/${TOTAL_CHECKS}] Shell syntax: no .sh files found under scripts/ or plugin/scripts/"
    shell_failed=1
  elif [[ ${#bad[@]} -gt 0 ]]; then
    fail "[4/${TOTAL_CHECKS}] Shell syntax: failed bash -n on ${bad[*]}"
    shell_failed=1
  fi

  if [[ ${shell_failed} -eq 0 ]]; then
    pass "[4/${TOTAL_CHECKS}] Shell syntax: all ${count} scripts/*.sh and plugin/scripts/*.sh valid (run-companion.sh executable)"
  fi
}

# ---------------------------------------------------------------------------
# Check 5: Rust workspace format, clippy, and unit/integration tests
# ---------------------------------------------------------------------------
check_rust_workspace() {
  if [[ ! -f "${REPO_ROOT}/Cargo.toml" ]]; then
    pass "[5/${TOTAL_CHECKS}] Rust workspace: skipped (no Cargo.toml)"
    return
  fi

  if ! command -v cargo >/dev/null 2>&1; then
    fail "[5/${TOTAL_CHECKS}] Rust workspace: Cargo.toml present but cargo not found on PATH"
    return
  fi

  local rust_failed=0
  if ! cargo fmt --all -- --check; then
    fail "[5/${TOTAL_CHECKS}] Rust workspace: cargo fmt --all -- --check failed"
    rust_failed=1
  fi
  if ! cargo clippy --workspace --all-targets -- -D warnings; then
    fail "[5/${TOTAL_CHECKS}] Rust workspace: cargo clippy --workspace --all-targets -- -D warnings failed"
    rust_failed=1
  fi
  if ! cargo test --workspace; then
    fail "[5/${TOTAL_CHECKS}] Rust workspace: cargo test --workspace failed"
    rust_failed=1
  fi

  if [[ ${rust_failed} -eq 0 ]]; then
    pass "[5/${TOTAL_CHECKS}] Rust workspace: fmt, clippy (-D warnings), and cargo test passed"
  fi
}

# ---------------------------------------------------------------------------
# Check 6: UI static XSS hygiene & single-file bundle size gate
# ---------------------------------------------------------------------------
check_ui_hygiene_and_bundle() {
  local ui_src="${REPO_ROOT}/ui/src"
  local ui_bundle="${REPO_ROOT}/ui/dist/index.html"
  local max_bytes=512000

  if [[ ! -d "${ui_src}" ]]; then
    fail "[6/${TOTAL_CHECKS}] UI hygiene & bundle: missing ui/src directory"
    return
  fi

  local ui_failed=0
  local src_sink_pattern='innerHTML|outerHTML|insertAdjacentHTML|document\.write|dangerouslySetInnerHTML|localStorage|sessionStorage|document\.cookie|new Function|XMLHttpRequest|(^|[^[:alnum:]_$])(eval|alert|confirm|prompt|fetch)\('
  local src_hits=""
  src_hits="$(grep -rnE -- "${src_sink_pattern}" "${ui_src}" || true)"
  if [[ -n "${src_hits}" ]]; then
    fail "[6/${TOTAL_CHECKS}] UI hygiene & bundle: forbidden DOM/JS/network sink(s) detected in ui/src/"
    printf '%s\n' "${src_hits}" | sed 's/^/  - /'
    ui_failed=1
  fi

  local bundle_bytes="0"
  if [[ ! -f "${ui_bundle}" || ! -s "${ui_bundle}" ]]; then
    fail "[6/${TOTAL_CHECKS}] UI hygiene & bundle: ui/dist/index.html missing or empty"
    ui_failed=1
  else
    bundle_bytes="$(wc -c < "${ui_bundle}" | tr -d '[:space:]')"
    if [[ ! "${bundle_bytes}" =~ ^[0-9]+$ || "${bundle_bytes}" -le 0 || "${bundle_bytes}" -ge "${max_bytes}" ]]; then
      fail "[6/${TOTAL_CHECKS}] UI hygiene & bundle: ui/dist/index.html size ${bundle_bytes} bytes must be > 0 and < ${max_bytes} bytes"
      ui_failed=1
    fi

    local ext_tag_pattern='<script[^>]+src=|<link[^>]+href=|<img[^>]+src=|<iframe|@import'
    local ext_hits=""
    ext_hits="$(grep -nE -- "${ext_tag_pattern}" "${ui_bundle}" || true)"
    if [[ -n "${ext_hits}" ]]; then
      fail "[6/${TOTAL_CHECKS}] UI hygiene & bundle: external resource tag(s) detected in ui/dist/index.html"
      printf '%s\n' "${ext_hits}" | sed 's/^/  - /'
      ui_failed=1
    fi

    local bundle_sink_pattern='innerHTML|outerHTML|document\.write|localStorage|sessionStorage|XMLHttpRequest|(^|[^[:alnum:]_$])(eval|fetch)\('
    local bundle_hits=""
    bundle_hits="$(grep -nE -- "${bundle_sink_pattern}" "${ui_bundle}" || true)"
    if [[ -n "${bundle_hits}" ]]; then
      fail "[6/${TOTAL_CHECKS}] UI hygiene & bundle: forbidden sink(s) detected in ui/dist/index.html"
      printf '%s\n' "${bundle_hits}" | sed 's/^/  - /'
      ui_failed=1
    fi
  fi

  if [[ ${ui_failed} -eq 0 ]]; then
    pass "[6/${TOTAL_CHECKS}] UI hygiene & bundle: 0 forbidden sinks/external tags, ui/dist/index.html is ${bundle_bytes} bytes (< ${max_bytes})"
  fi
}

# ---------------------------------------------------------------------------
# Check 7: Antigravity plugin bundle version parity & strict validation
# ---------------------------------------------------------------------------
check_plugin_bundle() {
  local manifest="${REPO_ROOT}/plugin/plugin.json"
  local cargo_toml="${REPO_ROOT}/Cargo.toml"

  if [[ ! -f "${manifest}" ]]; then
    fail "[7/${TOTAL_CHECKS}] Plugin bundle: missing plugin/plugin.json"
    return
  fi

  if [[ ! -f "${cargo_toml}" ]]; then
    fail "[7/${TOTAL_CHECKS}] Plugin bundle: missing Cargo.toml for version parity check"
    return
  fi

  local cargo_version=""
  cargo_version="$(awk '
    /^\[workspace\.package\]/ { in_sec = 1; next }
    /^\[/ { in_sec = 0 }
    in_sec && /^version[[:space:]]*=/ {
      sub(/^version[[:space:]]*=[[:space:]]*"/, "")
      sub(/".*$/, "")
      print
      exit
    }
  ' "${cargo_toml}")"

  local plugin_version=""
  plugin_version="$(
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
  )"

  if [[ -z "${cargo_version}" || -z "${plugin_version}" || "${cargo_version}" != "${plugin_version}" ]]; then
    fail "[7/${TOTAL_CHECKS}] Plugin bundle: version mismatch (Cargo.toml='${cargo_version}', plugin/plugin.json='${plugin_version}')"
    return
  fi

  if ! command -v agy >/dev/null 2>&1; then
    pass "[7/${TOTAL_CHECKS}] Plugin bundle: version ${plugin_version} matches Cargo.toml; skipped agy plugin validate --strict ./plugin (agy CLI not found on PATH)"
    return
  fi

  if agy plugin validate --strict "${REPO_ROOT}/plugin"; then
    pass "[7/${TOTAL_CHECKS}] Plugin bundle: version ${plugin_version} matches Cargo.toml and agy plugin validate --strict ./plugin passed"
  else
    fail "[7/${TOTAL_CHECKS}] Plugin bundle: agy plugin validate --strict ./plugin failed"
  fi
}

check_governance_files
check_no_forbidden_tracked_files
check_zero_ai_attribution
check_shell_scripts
check_rust_workspace
check_ui_hygiene_and_bundle
check_plugin_bundle

if [[ ${FAILURES} -gt 0 ]]; then
  printf '\nagent_check failed (%d failure(s))\n' "${FAILURES}"
  exit 1
fi

printf '\nAll agent_check gates passed.\n'
