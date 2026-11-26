#!/usr/bin/env bash
set -euo pipefail
unset CDPATH

REPO_ROOT="$(cd -P -- "$(dirname -- "${BASH_SOURCE[0]}")/.." >/dev/null 2>&1 && pwd -P)"
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

# Resolve branch commit range safely across main, feature branches,
# detached HEAD (CI pull_request), and shallow clones.
resolve_commit_range() {
  local range="HEAD"
  if git rev-parse --verify origin/main >/dev/null 2>&1; then
    local base=""
    base="$(git merge-base origin/main HEAD 2>/dev/null || true)"
    if [[ -n "${base}" && "${base}" != "$(git rev-parse HEAD)" ]]; then
      range="${base}..HEAD"
    fi
  fi
  printf '%s\n' "${range}"
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
  local f=""
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
# Check 2: Tracked file hygiene & Google API key secret scan (fail-closed)
# ---------------------------------------------------------------------------
check_no_forbidden_tracked_files() {
  local hygiene_failed=0

  # 2a. Forbidden tracked filenames (incl. __pycache__/ and *.py[cod])
  local tracked_list=""
  if ! tracked_list="$(git ls-files)"; then
    fail "[2/${TOTAL_CHECKS}] Tracked file hygiene & secret scan: git ls-files failed"
    return
  fi

  local forbidden_pattern='(^|/)\.DS_Store$|(^|/)\.env($|\.)|(^|/)\.antigravitycli/|(^|/)\.remember/|^target/|(^|/)node_modules/|(^|/)__pycache__/|\.py[cod]$|\.(pem|key)$'
  local matched=""
  local match_rc=0
  matched="$(printf '%s\n' "${tracked_list}" | LC_ALL=C grep -E -- "${forbidden_pattern}")" || match_rc=$?
  if [[ ${match_rc} -gt 1 ]]; then
    fail "[2/${TOTAL_CHECKS}] Tracked file hygiene & secret scan: grep failed scanning tracked filenames (exit ${match_rc})"
    hygiene_failed=1
  elif [[ ${match_rc} -eq 0 && -n "${matched}" ]]; then
    local filtered=""
    local filt_rc=0
    filtered="$(printf '%s\n' "${matched}" | LC_ALL=C grep -vE -- '(^|/)\.env\.example$')" || filt_rc=$?
    if [[ ${filt_rc} -gt 1 ]]; then
      fail "[2/${TOTAL_CHECKS}] Tracked file hygiene & secret scan: grep failed filtering .env.example (exit ${filt_rc})"
      hygiene_failed=1
    elif [[ ${filt_rc} -eq 0 && -n "${filtered}" ]]; then
      fail "[2/${TOTAL_CHECKS}] Tracked file hygiene & secret scan: forbidden files tracked in git"
      printf '%s\n' "${filtered}" | sed 's/^/  - /'
      hygiene_failed=1
    fi
  fi

  # 2b. Real Google API key secret scan (39-char AIza...) — NEVER echo matched key values!
  local secret_pattern='AIza[0-9A-Za-z_-]{35}'

  # (i) Tracked working-tree files (-I skips binary, -l prints filenames only)
  local wt_hits=""
  local wt_rc=0
  wt_hits="$(LC_ALL=C git grep -I -l -E -- "${secret_pattern}")" || wt_rc=$?
  if [[ ${wt_rc} -gt 1 ]]; then
    fail "[2/${TOTAL_CHECKS}] Tracked file hygiene & secret scan: git grep failed (exit ${wt_rc})"
    hygiene_failed=1
  elif [[ ${wt_rc} -eq 0 && -n "${wt_hits}" ]]; then
    fail "[2/${TOTAL_CHECKS}] Tracked file hygiene & secret scan: potential Google API key detected in tracked file(s)"
    printf '%s\n' "${wt_hits}" | sed 's/^/  - /'
    hygiene_failed=1
  fi

  # (ii) Staged index (--cached catches keys staged even if edited out of working tree)
  local idx_hits=""
  local idx_rc=0
  idx_hits="$(LC_ALL=C git grep --cached -I -l -E -- "${secret_pattern}")" || idx_rc=$?
  if [[ ${idx_rc} -gt 1 ]]; then
    fail "[2/${TOTAL_CHECKS}] Tracked file hygiene & secret scan: git grep --cached failed (exit ${idx_rc})"
    hygiene_failed=1
  elif [[ ${idx_rc} -eq 0 && -n "${idx_hits}" ]]; then
    fail "[2/${TOTAL_CHECKS}] Tracked file hygiene & secret scan: potential Google API key detected in staged file(s)"
    printf '%s\n' "${idx_hits}" | sed 's/^/  - /'
    hygiene_failed=1
  fi

  # (iii) Commit diffs (-m inspects merges too) & commit messages (--format='%h' prints ONLY hashes)
  if git rev-parse --verify HEAD >/dev/null 2>&1; then
    local range=""
    range="$(resolve_commit_range)"

    local diff_commits=""
    local diff_rc=0
    diff_commits="$(LC_ALL=C git log "${range}" -m -G"${secret_pattern}" --format='%h')" || diff_rc=$?
    if [[ ${diff_rc} -ne 0 ]]; then
      fail "[2/${TOTAL_CHECKS}] Tracked file hygiene & secret scan: git log -G failed (exit ${diff_rc})"
      hygiene_failed=1
    elif [[ -n "${diff_commits}" ]]; then
      fail "[2/${TOTAL_CHECKS}] Tracked file hygiene & secret scan: potential Google API key detected in commit diff(s)"
      printf '%s\n' "${diff_commits}" | awk '!seen[$0]++' | sed 's/^/  - commit /'
      hygiene_failed=1
    fi

    local msg_commits=""
    local msg_rc=0
    msg_commits="$(LC_ALL=C git log "${range}" --extended-regexp --grep="${secret_pattern}" --format='%h')" || msg_rc=$?
    if [[ ${msg_rc} -ne 0 ]]; then
      fail "[2/${TOTAL_CHECKS}] Tracked file hygiene & secret scan: git log --grep failed (exit ${msg_rc})"
      hygiene_failed=1
    elif [[ -n "${msg_commits}" ]]; then
      fail "[2/${TOTAL_CHECKS}] Tracked file hygiene & secret scan: potential Google API key detected in commit message(s)"
      printf '%s\n' "${msg_commits}" | awk '!seen[$0]++' | sed 's/^/  - commit /'
      hygiene_failed=1
    fi
  fi

  if [[ ${hygiene_failed} -eq 0 ]]; then
    pass "[2/${TOTAL_CHECKS}] Tracked file hygiene & secret scan: 0 forbidden files and 0 Google API key matches (tracked, staged, commits)"
  fi
}

# ---------------------------------------------------------------------------
# Check 3: Zero AI attribution in commit history
# ---------------------------------------------------------------------------
check_zero_ai_attribution() {
  if ! git rev-parse --verify HEAD >/dev/null 2>&1; then
    pass "[3/${TOTAL_CHECKS}] Commit hygiene: skipped (no commits yet)"
    return
  fi

  local range=""
  range="$(resolve_commit_range)"

  local log_output=""
  if ! log_output="$(git log "${range}" --format='%h %B')"; then
    fail "[3/${TOTAL_CHECKS}] Commit hygiene: git log failed for range ${range}"
    return
  fi

  local hits=""
  local grep_rc=0
  hits="$(printf '%s\n' "${log_output}" | LC_ALL=C grep -iE -- 'co-authored-by:.*((claude|gemini|copilot|openai|chatgpt|anthropic|antigravity|cursor)|(^|[^[:alnum:]])(ai|bot)([^[:alnum:]]|$))|generated with[[:space:]]+((claude|gemini|copilot)|ai([^[:alnum:]]|$))')" || grep_rc=$?
  if [[ ${grep_rc} -gt 1 ]]; then
    fail "[3/${TOTAL_CHECKS}] Commit hygiene: grep failed scanning commit messages (exit ${grep_rc})"
  elif [[ ${grep_rc} -eq 0 && -n "${hits}" ]]; then
    fail "[3/${TOTAL_CHECKS}] Commit hygiene: AI attribution detected in commit messages"
    printf '%s\n' "${hits}" | sed 's/^/  - /'
  else
    pass "[3/${TOTAL_CHECKS}] Commit hygiene: 0 AI attribution lines found"
  fi
}

# ---------------------------------------------------------------------------
# Check 4: Shell scripts pass bash syntax check, executable bit (-x), & dev-install safety
# ---------------------------------------------------------------------------
check_shell_scripts() {
  local shell_failed=0
  local required_scripts=(
    "scripts/agent_check.sh"
    "scripts/dev-install.sh"
    "plugin/scripts/run-companion.sh"
  )
  local req=""
  for req in "${required_scripts[@]}"; do
    if [[ ! -f "${REPO_ROOT}/${req}" ]]; then
      fail "[4/${TOTAL_CHECKS}] Shell syntax: missing required script ${req}"
      shell_failed=1
    fi
  done

  local search_dirs=("${REPO_ROOT}/scripts")
  if [[ -d "${REPO_ROOT}/plugin/scripts" ]]; then
    search_dirs+=("${REPO_ROOT}/plugin/scripts")
  fi

  local bad_syntax=()
  local non_exec=()
  local count=0
  local script=""
  while IFS= read -r -d '' script; do
    count=$((count + 1))
    if [[ ! -x "${script}" ]]; then
      non_exec+=("${script#${REPO_ROOT}/}")
    fi
    if ! bash -n -- "${script}"; then
      bad_syntax+=("${script#${REPO_ROOT}/}")
    elif [[ -x /bin/bash ]] && ! /bin/bash -n -- "${script}"; then
      bad_syntax+=("${script#${REPO_ROOT}/}")
    fi
  done < <(find "${search_dirs[@]}" -type f -name '*.sh' -print0)

  if [[ ${count} -eq 0 ]]; then
    fail "[4/${TOTAL_CHECKS}] Shell syntax: no .sh files found under scripts/ or plugin/scripts/"
    shell_failed=1
  fi
  if [[ ${#non_exec[@]} -gt 0 ]]; then
    fail "[4/${TOTAL_CHECKS}] Shell syntax: missing executable bit (-x) on ${non_exec[*]}"
    shell_failed=1
  fi
  if [[ ${#bad_syntax[@]} -gt 0 ]]; then
    fail "[4/${TOTAL_CHECKS}] Shell syntax: failed bash -n on ${bad_syntax[*]}"
    shell_failed=1
  fi

  local dev_install="${REPO_ROOT}/scripts/dev-install.sh"
  if [[ -f "${dev_install}" ]]; then
    local cfg_rc=0
    LC_ALL=C grep -iq 'config\.json' "${dev_install}" || cfg_rc=$?
    if [[ ${cfg_rc} -eq 0 ]]; then
      fail "[4/${TOTAL_CHECKS}] Shell syntax: scripts/dev-install.sh must not reference config.json"
      shell_failed=1
    elif [[ ${cfg_rc} -gt 1 ]]; then
      fail "[4/${TOTAL_CHECKS}] Shell syntax: grep failed checking scripts/dev-install.sh (exit ${cfg_rc})"
      shell_failed=1
    fi
  fi

  if [[ ${shell_failed} -eq 0 ]]; then
    pass "[4/${TOTAL_CHECKS}] Shell syntax: all ${count} scripts/*.sh and plugin/scripts/*.sh valid & executable (dev-install.sh zero config.json refs)"
  fi
}

# ---------------------------------------------------------------------------
# Check 5: 4-way version sync & npm/PyPI packaging shims
# ---------------------------------------------------------------------------
check_version_sync_and_packaging() {
  local pkg_failed=0

  if ! command -v python3 >/dev/null 2>&1; then
    fail "[5/${TOTAL_CHECKS}] Version sync & packaging: python3 not found on PATH"
    return
  fi
  if ! command -v node >/dev/null 2>&1; then
    fail "[5/${TOTAL_CHECKS}] Version sync & packaging: node not found on PATH"
    return
  fi

  # 5a. 4-way TOML/JSON version sync + npm/PyPI manifest assertions + py_compile (zero __pycache__)
  local py_out=""
  if ! py_out="$(python3 -B - "${REPO_ROOT}" <<'PYEOF'
import json
import os
import pathlib
import py_compile
import re
import sys
import tempfile

root = pathlib.Path(sys.argv[1])

def parse_toml_value(path: pathlib.Path, section: str, key: str) -> str:
    text = path.read_text(encoding="utf-8")
    try:
        import tomllib  # type: ignore[import-not-found]
        data = tomllib.loads(text)
        node = data
        for part in section.split("."):
            if not isinstance(node, dict) or part not in node:
                raise KeyError(f"missing [{section}]")
            node = node[part]
        val = node[key]
        if not isinstance(val, str) or not val:
            raise ValueError(f"[{section}].{key} must be a non-empty string")
        return val
    except ImportError:
        in_sec = False
        sec_re = re.compile(r"^\s*\[" + re.escape(section) + r"\]\s*(?:#.*)?$")
        any_sec_re = re.compile(r"^\s*\[")
        key_re = re.compile(r"^\s*" + re.escape(key) + r'\s*=\s*"([^"]+)"\s*(?:#.*)?$')
        for line in text.splitlines():
            if sec_re.match(line):
                in_sec = True
                continue
            if any_sec_re.match(line):
                in_sec = False
                continue
            if in_sec:
                m = key_re.match(line)
                if m:
                    return m.group(1)
        raise ValueError(f"could not locate [{section}].{key} in {path}")

errors = []
cargo_toml = root / "Cargo.toml"
plugin_json = root / "plugin" / "plugin.json"
npm_pkg_json = root / "packages" / "npm" / "agy-companion" / "package.json"
pyproject_toml = root / "packages" / "python" / "pyproject.toml"
py_src_root = root / "packages" / "python" / "python"

for required in (cargo_toml, plugin_json, npm_pkg_json, pyproject_toml):
    if not required.is_file():
        errors.append(f"missing required manifest: {required.relative_to(root)}")

if errors:
    for err in errors:
        print(err, file=sys.stderr)
    sys.exit(1)

cargo_ver = parse_toml_value(cargo_toml, "workspace.package", "version")
plugin_data = json.loads(plugin_json.read_text(encoding="utf-8"))
plugin_ver = plugin_data.get("version")
npm_data = json.loads(npm_pkg_json.read_text(encoding="utf-8"))
npm_ver = npm_data.get("version")
py_ver = parse_toml_value(pyproject_toml, "project", "version")
maturin_bindings = parse_toml_value(pyproject_toml, "tool.maturin", "bindings")
maturin_manifest = parse_toml_value(pyproject_toml, "tool.maturin", "manifest-path")

if not (cargo_ver == plugin_ver == npm_ver == py_ver):
    errors.append(
        f"4-way version mismatch: Cargo.toml={cargo_ver!r}, "
        f"plugin/plugin.json={plugin_ver!r}, "
        f"packages/npm/agy-companion/package.json={npm_ver!r}, "
        f"packages/python/pyproject.toml={py_ver!r}"
    )

expected_publish = {"registry": "https://registry.npmjs.org/", "access": "public"}
if npm_data.get("publishConfig") != expected_publish:
    errors.append(
        f"packages/npm/agy-companion/package.json publishConfig must be {expected_publish!r}, "
        f"got {npm_data.get('publishConfig')!r}"
    )

expected_bin = {
    "agy-companion": "bin/agy-companion.js",
    "agyc": "bin/agy-companion.js",
}
if npm_data.get("bin") != expected_bin:
    errors.append(
        f"packages/npm/agy-companion/package.json bin must be {expected_bin!r}, "
        f"got {npm_data.get('bin')!r}"
    )

expected_targets = {
    "@agy-companion/darwin-arm64",
    "@agy-companion/darwin-x64",
    "@agy-companion/linux-x64",
    "@agy-companion/linux-arm64",
    "@agy-companion/win32-x64",
}
opt_deps = npm_data.get("optionalDependencies")
if not isinstance(opt_deps, dict):
    errors.append("packages/npm/agy-companion/package.json missing optionalDependencies object")
else:
    missing_targets = sorted(expected_targets - set(opt_deps.keys()))
    if missing_targets:
        errors.append(f"optionalDependencies missing targets: {missing_targets}")
    for dep_name, dep_ver in sorted(opt_deps.items()):
        if dep_name.startswith("@agy-companion/") and dep_ver != cargo_ver:
            errors.append(
                f"optionalDependencies[{dep_name!r}]={dep_ver!r} != {cargo_ver!r}"
            )

if maturin_bindings != "bin":
    errors.append(f"[tool.maturin].bindings must be 'bin', got {maturin_bindings!r}")

resolved_manifest = (pyproject_toml.parent / maturin_manifest).resolve()
if not resolved_manifest.is_file():
    errors.append(f"[tool.maturin].manifest-path does not exist: {maturin_manifest}")

py_init = py_src_root / "agy_companion" / "__init__.py"
if not py_init.is_file():
    errors.append("missing packages/python/python/agy_companion/__init__.py")
else:
    py_files = sorted(py_src_root.rglob("*.py"))
    e2e_runner = root / "e2e" / "run_e2e.py"
    if e2e_runner.is_file():
        py_files.extend(sorted((root / "e2e").glob("*.py")))
    scripts_dir = root / "scripts"
    if scripts_dir.is_dir():
        py_files.extend(sorted(scripts_dir.glob("*.py")))
    with tempfile.TemporaryDirectory() as td:
        for idx, py_file in enumerate(py_files):
            py_compile.compile(
                str(py_file),
                cfile=os.path.join(td, f"mod_{idx}.pyc"),
                doraise=True,
            )

if errors:
    for err in errors:
        print(err, file=sys.stderr)
    sys.exit(1)

print(cargo_ver)
PYEOF
  )"; then
    fail "[5/${TOTAL_CHECKS}] Version sync & packaging: 4-way manifest/Python validation failed"
    pkg_failed=1
  fi

  # 5b. JS shim existence, executable bit, node --check, and execFileSync allow-list hygiene
  local npm_shim="${REPO_ROOT}/packages/npm/agy-companion/bin/agy-companion.js"
  local npm_gen="${REPO_ROOT}/packages/npm/generate-platform-pkgs.mjs"

  if [[ ! -f "${npm_shim}" ]]; then
    fail "[5/${TOTAL_CHECKS}] Version sync & packaging: missing packages/npm/agy-companion/bin/agy-companion.js"
    pkg_failed=1
  else
    if [[ ! -x "${npm_shim}" ]]; then
      fail "[5/${TOTAL_CHECKS}] Version sync & packaging: packages/npm/agy-companion/bin/agy-companion.js is not executable (expected 0755)"
      pkg_failed=1
    fi
    if ! node --check "${npm_shim}"; then
      fail "[5/${TOTAL_CHECKS}] Version sync & packaging: node --check failed on bin/agy-companion.js"
      pkg_failed=1
    fi

    local has_efs_rc=0
    LC_ALL=C grep -q 'execFileSync' "${npm_shim}" || has_efs_rc=$?
    if [[ ${has_efs_rc} -ne 0 ]]; then
      fail "[5/${TOTAL_CHECKS}] Version sync & packaging: bin/agy-companion.js must use execFileSync"
      pkg_failed=1
    fi

    local unsafe_js_pattern='(^|[^[:alnum:]_$])(exec|execSync)[[:space:]]*\(|shell[[:space:]]*:[[:space:]]*(true|['"'"'"])'
    local unsafe_hits=""
    local unsafe_rc=0
    unsafe_hits="$(LC_ALL=C grep -nE -- "${unsafe_js_pattern}" "${npm_shim}")" || unsafe_rc=$?
    if [[ ${unsafe_rc} -gt 1 ]]; then
      fail "[5/${TOTAL_CHECKS}] Version sync & packaging: grep failed scanning bin/agy-companion.js (exit ${unsafe_rc})"
      pkg_failed=1
    elif [[ ${unsafe_rc} -eq 0 && -n "${unsafe_hits}" ]]; then
      fail "[5/${TOTAL_CHECKS}] Version sync & packaging: unsafe exec/shell pattern detected in bin/agy-companion.js"
      printf '%s\n' "${unsafe_hits}" | sed 's/^/  - /'
      pkg_failed=1
    fi
  fi

  if [[ ! -f "${npm_gen}" ]]; then
    fail "[5/${TOTAL_CHECKS}] Version sync & packaging: missing packages/npm/generate-platform-pkgs.mjs"
    pkg_failed=1
  else
    if [[ ! -x "${npm_gen}" ]]; then
      fail "[5/${TOTAL_CHECKS}] Version sync & packaging: packages/npm/generate-platform-pkgs.mjs is not executable (expected 0755)"
      pkg_failed=1
    fi
    if ! node --check "${npm_gen}"; then
      fail "[5/${TOTAL_CHECKS}] Version sync & packaging: node --check failed on generate-platform-pkgs.mjs"
      pkg_failed=1
    fi

    # 5c. Dry-run generate-platform-pkgs.mjs into a trap-cleaned temp directory
    if [[ -n "${py_out}" ]]; then
      local gen_rc=0
      (
        tmp_out="$(mktemp -d)"
        trap 'rm -rf -- "${tmp_out}"' EXIT
        node "${npm_gen}" --out-dir "${tmp_out}" >/dev/null &&
          python3 -B - "${tmp_out}" "${py_out}" <<'PYEOF'
import json
import pathlib
import sys

out_dir = pathlib.Path(sys.argv[1])
expected_ver = sys.argv[2]
expected_publish = {"registry": "https://registry.npmjs.org/", "access": "public"}
targets = {
    "darwin-arm64": (["darwin"], ["arm64"]),
    "darwin-x64": (["darwin"], ["x64"]),
    "linux-x64": (["linux"], ["x64"]),
    "linux-arm64": (["linux"], ["arm64"]),
    "win32-x64": (["win32"], ["x64"]),
}

errors = []
for target, (exp_os, exp_cpu) in targets.items():
    manifest_path = out_dir / target / "package.json"
    if not manifest_path.is_file():
        errors.append(f"missing generated manifest: {target}/package.json")
        continue
    data = json.loads(manifest_path.read_text(encoding="utf-8"))
    if data.get("name") != f"@agy-companion/{target}":
        errors.append(f"{target}: unexpected name {data.get('name')!r}")
    if data.get("version") != expected_ver:
        errors.append(f"{target}: version {data.get('version')!r} != {expected_ver!r}")
    if data.get("publishConfig") != expected_publish:
        errors.append(f"{target}: unexpected publishConfig {data.get('publishConfig')!r}")
    if data.get("os") != exp_os or data.get("cpu") != exp_cpu:
        errors.append(f"{target}: unexpected os/cpu {data.get('os')!r}/{data.get('cpu')!r}")
    if data.get("files") != ["bin/"]:
        errors.append(f"{target}: unexpected files {data.get('files')!r}")
    if data.get("preferUnplugged") is not True:
        errors.append(f"{target}: preferUnplugged must be true")
    if "exports" in data:
        errors.append(f"{target}: must omit 'exports' field")

if errors:
    for err in errors:
        print(err, file=sys.stderr)
    sys.exit(1)
PYEOF
      ) || gen_rc=$?
      if [[ ${gen_rc} -ne 0 ]]; then
        fail "[5/${TOTAL_CHECKS}] Version sync & packaging: generate-platform-pkgs.mjs dry-run validation failed"
        pkg_failed=1
      fi
    fi
  fi

  if [[ ${pkg_failed} -eq 0 ]]; then
    pass "[5/${TOTAL_CHECKS}] Version sync & packaging: 4-way version ${py_out}, publishConfig, 5 platform manifests, node --check, execFileSync hygiene & py_compile passed"
  fi
}

# ---------------------------------------------------------------------------
# Check 6: Rust workspace (fmt, clippy, test) & UI XSS hygiene / bundle gate
# ---------------------------------------------------------------------------
check_rust_and_ui() {
  local check6_failed=0

  # 6a. Rust workspace
  if [[ -f "${REPO_ROOT}/Cargo.toml" ]]; then
    if ! command -v cargo >/dev/null 2>&1; then
      fail "[6/${TOTAL_CHECKS}] Rust & UI: Cargo.toml present but cargo not found on PATH"
      check6_failed=1
    else
      if ! cargo fmt --all -- --check; then
        fail "[6/${TOTAL_CHECKS}] Rust & UI: cargo fmt --all -- --check failed"
        check6_failed=1
      fi
      if ! cargo clippy --workspace --all-targets -- -D warnings; then
        fail "[6/${TOTAL_CHECKS}] Rust & UI: cargo clippy --workspace --all-targets -- -D warnings failed"
        check6_failed=1
      fi
      if ! cargo test --workspace; then
        fail "[6/${TOTAL_CHECKS}] Rust & UI: cargo test --workspace failed"
        check6_failed=1
      fi
    fi
  fi

  # 6b. UI static XSS hygiene & single-file bundle size gate
  local ui_src="${REPO_ROOT}/ui/src"
  local ui_bundle="${REPO_ROOT}/ui/dist/index.html"
  local max_bytes=512000
  local bundle_bytes="0"

  if [[ ! -d "${ui_src}" ]]; then
    fail "[6/${TOTAL_CHECKS}] Rust & UI: missing ui/src directory"
    check6_failed=1
  else
    local src_sink_pattern='innerHTML|outerHTML|insertAdjacentHTML|document\.write|dangerouslySetInnerHTML|localStorage|sessionStorage|document\.cookie|new[[:space:]]+Function|XMLHttpRequest|(^|[^[:alnum:]_$])(eval|alert|confirm|prompt|fetch)[[:space:]]*\('
    local src_hits=""
    local src_rc=0
    src_hits="$(LC_ALL=C grep -rnE -- "${src_sink_pattern}" "${ui_src}")" || src_rc=$?
    if [[ ${src_rc} -gt 1 ]]; then
      fail "[6/${TOTAL_CHECKS}] Rust & UI: grep failed scanning ui/src/ (exit ${src_rc})"
      check6_failed=1
    elif [[ ${src_rc} -eq 0 && -n "${src_hits}" ]]; then
      fail "[6/${TOTAL_CHECKS}] Rust & UI: forbidden DOM/JS/network sink(s) detected in ui/src/"
      printf '%s\n' "${src_hits}" | sed 's/^/  - /'
      check6_failed=1
    fi

    if ! command -v npm >/dev/null 2>&1; then
      fail "[6/${TOTAL_CHECKS}] Rust & UI: npm not found on PATH for UI behavioral tests"
      check6_failed=1
    elif ! npm --prefix "${REPO_ROOT}/ui" test; then
      fail "[6/${TOTAL_CHECKS}] Rust & UI: npm --prefix ui test failed"
      check6_failed=1
    fi

    if [[ ! -f "${ui_bundle}" || ! -s "${ui_bundle}" ]]; then
      fail "[6/${TOTAL_CHECKS}] Rust & UI: ui/dist/index.html missing or empty"
      check6_failed=1
    else
      bundle_bytes="$(wc -c < "${ui_bundle}" | tr -d '[:space:]')"
      if [[ ! "${bundle_bytes}" =~ ^[0-9]+$ || "${bundle_bytes}" -le 0 || "${bundle_bytes}" -ge "${max_bytes}" ]]; then
        fail "[6/${TOTAL_CHECKS}] Rust & UI: ui/dist/index.html size ${bundle_bytes} bytes must be > 0 and < ${max_bytes} bytes"
        check6_failed=1
      fi

      local ext_tag_pattern='<script[^>]*[[:space:]]src[[:space:]]*=|<link[^>]*[[:space:]]href[[:space:]]*=|<img[^>]*[[:space:]]src[[:space:]]*=|<iframe|@import'
      local ext_hits=""
      local ext_rc=0
      ext_hits="$(LC_ALL=C grep -inE -- "${ext_tag_pattern}" "${ui_bundle}")" || ext_rc=$?
      if [[ ${ext_rc} -gt 1 ]]; then
        fail "[6/${TOTAL_CHECKS}] Rust & UI: grep failed scanning ui/dist/index.html for external tags (exit ${ext_rc})"
        check6_failed=1
      elif [[ ${ext_rc} -eq 0 && -n "${ext_hits}" ]]; then
        fail "[6/${TOTAL_CHECKS}] Rust & UI: external resource tag(s) detected in ui/dist/index.html"
        printf '%s\n' "${ext_hits}" | sed 's/^/  - /'
        check6_failed=1
      fi

      local bundle_sink_pattern="${src_sink_pattern}"
      local bundle_hits=""
      local bundle_rc=0
      bundle_hits="$(LC_ALL=C grep -nE -- "${bundle_sink_pattern}" "${ui_bundle}")" || bundle_rc=$?
      if [[ ${bundle_rc} -gt 1 ]]; then
        fail "[6/${TOTAL_CHECKS}] Rust & UI: grep failed scanning ui/dist/index.html for forbidden sinks (exit ${bundle_rc})"
        check6_failed=1
      elif [[ ${bundle_rc} -eq 0 && -n "${bundle_hits}" ]]; then
        fail "[6/${TOTAL_CHECKS}] Rust & UI: forbidden sink(s) detected in ui/dist/index.html"
        printf '%s\n' "${bundle_hits}" | sed 's/^/  - /'
        check6_failed=1
      fi
    fi
  fi

  # 6c. 5 Automated Static Hermeticity Guards
  if ! command -v python3 >/dev/null 2>&1; then
    fail "[6/${TOTAL_CHECKS}] Rust & UI: python3 not found on PATH for static hermeticity guards"
    check6_failed=1
  elif ! python3 -B - "${REPO_ROOT}" <<'PYEOF'
from __future__ import annotations

import ast
import io
import pathlib
import re
import sys
import tokenize

root = pathlib.Path(sys.argv[1])
errors: list[str] = []

_RUST_TOKEN_RE = re.compile(
    r'b?r(#*)"[\s\S]*?"\1'
    r"|b?'(?:\\(?:x[0-9A-Fa-f]{2}|u\{[0-9A-Fa-f_]+\}|.)|[^'\\])'"
    r'|b?"(?:\\[\s\S]|[^"\\])*"'
    r"|//[^\n]*"
    r"|/\*[\s\S]*?\*/"
)


def strip_rust_comments(text: str) -> str:
    def _repl(match: re.Match[str]) -> str:
        tok = match.group(0)
        if tok.startswith("//"):
            return ""
        if tok.startswith("/*"):
            return "\n" * tok.count("\n")
        return tok

    return _RUST_TOKEN_RE.sub(_repl, text)


def strip_py_comments(text: str) -> str:
    lines = text.splitlines(keepends=True)
    try:
        tokens = list(tokenize.generate_tokens(io.StringIO(text).readline))
    except tokenize.TokenError:
        return text
    comment_tokens = [tok for tok in tokens if tok.type == tokenize.COMMENT]
    for tok in reversed(comment_tokens):
        row = tok.start[0] - 1
        lines[row] = lines[row][: tok.start[1]] + lines[row][tok.end[1] :]
    return "".join(lines)


# Guard 1: Zero raw Command::new(BIN) / tokio::process::Command::new(BIN) or
# CARGO_BIN_EXE_agy-companion in crates/companion-cli/tests/**/*.rs outside tests/common/mod.rs.
cli_tests_dir = root / "crates" / "companion-cli" / "tests"
common_mod = cli_tests_dir / "common" / "mod.rs"
raw_bin_re = re.compile(
    r"\b(?:tokio::process::|std::process::)?Command::new\s*\(\s*(?:[\w:]+::)?BIN\b"
    r"|\bCARGO_BIN_EXE_agy-companion\b"
)
if not cli_tests_dir.is_dir():
    errors.append("Guard 1: missing crates/companion-cli/tests directory")
else:
    for rs_file in sorted(cli_tests_dir.rglob("*.rs")):
        if rs_file == common_mod:
            continue
        cleaned = strip_rust_comments(rs_file.read_text(encoding="utf-8"))
        for match in raw_bin_re.finditer(cleaned):
            line_no = cleaned.count("\n", 0, match.start()) + 1
            rel = rs_file.relative_to(root)
            errors.append(
                f"Guard 1 ({rel}:{line_no}): raw Command::new(BIN) or CARGO_BIN_EXE_agy-companion outside tests/common/mod.rs"
            )

# Guard 2: Zero bare CompanionUiServer::new( or unisolated CompanionUiServer::with_options(
# in crates/companion-ui-mcp/src/tests.rs outside isolated_ui_server_with and
# ui_resource_reads_process_env_var_dynamically.
ui_mcp_tests = root / "crates" / "companion-ui-mcp" / "src" / "tests.rs"
if not ui_mcp_tests.is_file():
    errors.append("Guard 2: missing crates/companion-ui-mcp/src/tests.rs")
else:
    cleaned = strip_rust_comments(ui_mcp_tests.read_text(encoding="utf-8"))
    fn_re = re.compile(r"(?m)^[ \t]*(?:pub(?:\([^)]*\))?\s+)?(?:async\s+)?fn\s+([A-Za-z_][A-Za-z0-9_]*)\b")
    ctor_re = re.compile(r"\bCompanionUiServer\s*::\s*(new|with_options)\s*\(")
    fn_matches = list(fn_re.finditer(cleaned))
    fn_names = {m.group(1) for m in fn_matches}
    if "isolated_ui_server_with" not in fn_names:
        errors.append("Guard 2: isolated_ui_server_with helper not found in companion-ui-mcp/src/tests.rs")

    slices: list[tuple[str, int, str]] = []
    if fn_matches:
        slices.append(("<module>", 0, cleaned[: fn_matches[0].start()]))
        for idx, m in enumerate(fn_matches):
            end = fn_matches[idx + 1].start() if idx + 1 < len(fn_matches) else len(cleaned)
            slices.append((m.group(1), m.start(), cleaned[m.start() : end]))
    else:
        slices.append(("<module>", 0, cleaned))

    for fn_name, offset, body in slices:
        for match in ctor_re.finditer(body):
            ctor = match.group(1)
            allowed = (
                (ctor == "with_options" and fn_name == "isolated_ui_server_with")
                or (ctor == "new" and fn_name == "ui_resource_reads_process_env_var_dynamically")
            )
            if not allowed:
                line_no = cleaned.count("\n", 0, offset + match.start()) + 1
                errors.append(
                    f"Guard 2 (crates/companion-ui-mcp/src/tests.rs:{line_no} in {fn_name}): "
                    f"disallowed CompanionUiServer::{ctor}("
                )

# Guard 3: Zero direct 24141 binds or live port literals in crates/ tests or e2e/.
rust_port_patterns = (
    re.compile(r"\bbind\s*\([^;{}]{0,200}24141\b"),
    re.compile(r"(?<![:\w])(?:listen_)?port\s*=\s*24141\b"),
    re.compile(r"\b(?:NonZeroU16::new|ProxyPort::new)\s*\(\s*24141\s*\)"),
)
crates_dir = root / "crates"
for rs_file in sorted(crates_dir.rglob("*.rs")):
    rel = rs_file.relative_to(root)
    raw_text = rs_file.read_text(encoding="utf-8")
    is_test_file = "tests" in rel.parts or rs_file.name == "tests.rs"
    if is_test_file:
        test_offset = 0
        test_text = strip_rust_comments(raw_text)
    else:
        cfg_idx = raw_text.find("#[cfg(test)]")
        if cfg_idx == -1:
            continue
        test_offset = cfg_idx
        test_text = strip_rust_comments(raw_text[cfg_idx:])
    for pat in rust_port_patterns:
        for match in pat.finditer(test_text):
            line_no = raw_text.count("\n", 0, test_offset) + test_text.count("\n", 0, match.start()) + 1
            errors.append(f"Guard 3 ({rel}:{line_no}): direct 24141 bind/port literal in Rust test")

py_port_patterns = (
    re.compile(r"\bbind\s*\([^;\n]{0,200}24141\b"),
    re.compile(r"\b(?:listen_)?port\s*=\s*24141\b"),
    re.compile(r"""["'][^"']*:24141\b"""),
)
e2e_dir = root / "e2e"
if e2e_dir.is_dir():
    for py_file in sorted(e2e_dir.rglob("*.py")):
        if "__pycache__" in py_file.parts:
            continue
        rel = py_file.relative_to(root)
        cleaned = strip_py_comments(py_file.read_text(encoding="utf-8"))
        for pat in py_port_patterns:
            for match in pat.finditer(cleaned):
                line_no = cleaned.count("\n", 0, match.start()) + 1
                errors.append(f"Guard 3 ({rel}:{line_no}): direct 24141 bind/port literal in e2e")

# Guard 4 & Guard 5: e2e opaque-box hygiene & explicit non-None env= on run_cli().
forbidden_repo_substr = re.compile(r"(?:^|/)(?:crates|docs|ui/src)(?:/|$)|(?:^|/)(?:README\.md|AGENTS\.md)$")
forbidden_repo_segments = frozenset({"crates", "docs", "README.md", "AGENTS.md"})


def extract_repo_root_segments(node: ast.AST) -> list[str] | None:
    if isinstance(node, ast.Name) and node.id == "repo_root":
        return []
    if isinstance(node, ast.Attribute) and node.attr == "repo_root":
        return []
    if isinstance(node, ast.BinOp) and isinstance(node.op, ast.Div):
        base = extract_repo_root_segments(node.left)
        if base is not None:
            if isinstance(node.right, ast.Constant) and isinstance(node.right.value, str):
                return base + [p for p in node.right.value.split("/") if p]
            return base
    if isinstance(node, ast.Call):
        if isinstance(node.func, ast.Attribute) and node.func.attr == "joinpath":
            base = extract_repo_root_segments(node.func.value)
            if base is not None:
                segs = list(base)
                for arg in node.args:
                    if isinstance(arg, ast.Constant) and isinstance(arg.value, str):
                        segs.extend(p for p in arg.value.split("/") if p)
                return segs
        if (
            (isinstance(node.func, ast.Attribute) and node.func.attr == "join")
            or (isinstance(node.func, ast.Name) and node.func.id == "Path")
        ) and node.args:
            base = extract_repo_root_segments(node.args[0])
            if base is not None:
                segs = list(base)
                for arg in node.args[1:]:
                    if isinstance(arg, ast.Constant) and isinstance(arg.value, str):
                        segs.extend(p for p in arg.value.split("/") if p)
                return segs
    return None


if e2e_dir.is_dir():
    for py_file in sorted(e2e_dir.rglob("*.py")):
        if "__pycache__" in py_file.parts:
            continue
        rel = py_file.relative_to(root)
        src = py_file.read_text(encoding="utf-8")
        tree = ast.parse(src, filename=str(rel))
        cleaned = strip_py_comments(src)
        if "agent_check.sh" in cleaned:
            errors.append(f"Guard 4 ({rel}): forbidden agent_check.sh reference in e2e")
        for node in ast.walk(tree):
            if isinstance(node, ast.Constant) and isinstance(node.value, str):
                if "agent_check.sh" in node.value:
                    errors.append(f"Guard 4 ({rel}:{node.lineno}): forbidden agent_check.sh string in e2e")

        if not py_file.name.startswith("test_"):
            continue

        for node in ast.walk(tree):
            if isinstance(node, ast.Constant) and isinstance(node.value, str):
                if forbidden_repo_substr.search(node.value):
                    errors.append(
                        f"Guard 4 ({rel}:{node.lineno}): white-box repo path string {node.value!r}"
                    )
            segs = extract_repo_root_segments(node)
            if segs:
                if segs[0] in forbidden_repo_segments or segs[:2] == ["ui", "src"]:
                    errors.append(
                        f"Guard 4 ({rel}:{getattr(node, 'lineno', 0)}): white-box repo_root read {'/'.join(segs)!r}"
                    )
            if isinstance(node, ast.Call):
                is_run_cli = (
                    (isinstance(node.func, ast.Name) and node.func.id == "run_cli")
                    or (isinstance(node.func, ast.Attribute) and node.func.attr == "run_cli")
                )
                if is_run_cli:
                    env_kw = next((kw for kw in node.keywords if kw.arg == "env"), None)
                    if env_kw is None:
                        errors.append(
                            f"Guard 5 ({rel}:{node.lineno}): run_cli() missing explicit env= keyword"
                        )
                    elif isinstance(env_kw.value, ast.Constant) and env_kw.value.value is None:
                        errors.append(
                            f"Guard 5 ({rel}:{node.lineno}): run_cli() passed env=None"
                        )

if errors:
    for err in errors:
        print(err, file=sys.stderr)
    sys.exit(1)
PYEOF
  then
    fail "[6/${TOTAL_CHECKS}] Rust & UI: static hermeticity guard(s) failed"
    check6_failed=1
  fi

  if [[ ${check6_failed} -eq 0 ]]; then
    pass "[6/${TOTAL_CHECKS}] Rust & UI: fmt/clippy/test, UI tests, 5 static hermeticity guards, 0 forbidden sinks/external tags, ui/dist/index.html is ${bundle_bytes} bytes (< ${max_bytes})"
  fi
}

# ---------------------------------------------------------------------------
# Check 7: Plugin JSON manifests & agy plugin validate --strict ./plugin
# ---------------------------------------------------------------------------
check_plugin_bundle() {
  local plugin_failed=0

  if ! command -v python3 >/dev/null 2>&1; then
    fail "[7/${TOTAL_CHECKS}] Plugin bundle: python3 not found on PATH for JSON manifest validation"
    return
  fi

  local logo_validator="${REPO_ROOT}/scripts/check_plugin_logo.py"
  local logo_tests="${REPO_ROOT}/scripts/test_check_plugin_logo.py"

  if [[ ! -f "${logo_validator}" ]]; then
    fail "[7/${TOTAL_CHECKS}] Plugin bundle: missing scripts/check_plugin_logo.py"
    plugin_failed=1
  elif ! python3 -B "${logo_validator}" "${REPO_ROOT}"; then
    fail "[7/${TOTAL_CHECKS}] Plugin bundle: JSON manifest and Expat SVG logo validation failed"
    plugin_failed=1
  fi

  if [[ ! -f "${logo_tests}" ]]; then
    fail "[7/${TOTAL_CHECKS}] Plugin bundle: missing scripts/test_check_plugin_logo.py"
    plugin_failed=1
  else
    local logo_test_out=""
    if ! logo_test_out="$(python3 -B "${logo_tests}" 2>&1)"; then
      fail "[7/${TOTAL_CHECKS}] Plugin bundle: scripts/test_check_plugin_logo.py failed"
      printf '%s\n' "${logo_test_out}" | sed 's/^/  /'
      plugin_failed=1
    fi
  fi

  if [[ ${plugin_failed} -ne 0 ]]; then
    return
  fi

  if ! command -v agy >/dev/null 2>&1; then
    pass "[7/${TOTAL_CHECKS}] Plugin bundle: all 3 plugin JSON manifests and logo valid; skipped agy plugin validate --strict ./plugin (agy CLI not found on PATH)"
    return
  fi

  if agy plugin validate --strict "${REPO_ROOT}/plugin"; then
    pass "[7/${TOTAL_CHECKS}] Plugin bundle: all 3 plugin JSON manifests, logo, and agy plugin validate --strict ./plugin passed"
  else
    fail "[7/${TOTAL_CHECKS}] Plugin bundle: agy plugin validate --strict ./plugin failed"
  fi
}

check_governance_files
check_no_forbidden_tracked_files
check_zero_ai_attribution
check_shell_scripts
check_version_sync_and_packaging
check_rust_and_ui
check_plugin_bundle

if [[ ${FAILURES} -gt 0 ]]; then
  printf '\nagent_check failed (%d failure(s))\n' "${FAILURES}"
  exit 1
fi

printf '\nAll agent_check gates passed.\n'
