#!/usr/bin/env bash
set -euo pipefail

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "${REPO_ROOT}"

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
    fail "[1/4] Governance files: missing ${missing[*]}"
  elif [[ -f "${REPO_ROOT}/GEMINI.md" ]]; then
    fail "[1/4] Governance files: redundant GEMINI.md present (use AGENTS.md + .agents/rules/)"
  else
    pass "[1/4] Governance files: all ${#required[@]} required files present (Antigravity-native layout)"
  fi
}

# ---------------------------------------------------------------------------
# Check 2: No forbidden local/secret files tracked in git
# ---------------------------------------------------------------------------
check_no_forbidden_tracked_files() {
  local tracked=""
  tracked="$(git ls-files | grep -E '(^|/)\.DS_Store$|(^|/)\.env($|\.)|(^|/)\.antigravitycli/|(^|/)\.remember/|\.(pem|key)$' | grep -vE '(^|/)\.env\.example$' || true)"
  if [[ -n "${tracked}" ]]; then
    fail "[2/4] Tracked file hygiene: forbidden files tracked in git"
    printf '%s\n' "${tracked}" | sed 's/^/  - /'
  else
    pass "[2/4] Tracked file hygiene: 0 forbidden or secret files tracked"
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
    fail "[3/4] Commit hygiene: AI attribution detected in commit messages"
    printf '%s\n' "${hits}" | sed 's/^/  - /'
  else
    pass "[3/4] Commit hygiene: 0 AI attribution lines found"
  fi
}

# ---------------------------------------------------------------------------
# Check 4: Shell scripts pass bash syntax check
# ---------------------------------------------------------------------------
check_shell_scripts() {
  local bad=()
  while IFS= read -r script; do
    if ! bash -n "${script}"; then
      bad+=("${script}")
    fi
  done < <(find "${REPO_ROOT}/scripts" -type f -name '*.sh' | sort)

  if [[ ${#bad[@]} -gt 0 ]]; then
    fail "[4/4] Shell syntax: failed bash -n on ${bad[*]}"
  else
    pass "[4/4] Shell syntax: all scripts/*.sh valid"
  fi
}

check_governance_files
check_no_forbidden_tracked_files
check_zero_ai_attribution
check_shell_scripts

if [[ ${FAILURES} -gt 0 ]]; then
  printf '\nagent_check failed (%d failure(s))\n' "${FAILURES}"
  exit 1
fi

printf '\nAll agent_check gates passed.\n'
