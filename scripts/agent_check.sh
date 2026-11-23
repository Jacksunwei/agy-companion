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

  # 5d. Run pure-stdlib E2E harness self-test when e2e/run_e2e.py is present (gracefully skipped in minimal repos)
  local e2e_runner="${REPO_ROOT}/e2e/run_e2e.py"
  if [[ -f "${e2e_runner}" ]]; then
    if ! python3 -B "${e2e_runner}" --self-test >/dev/null 2>&1; then
      fail "[5/${TOTAL_CHECKS}] Version sync & packaging: python3 -B e2e/run_e2e.py --self-test failed"
      pkg_failed=1
    fi
  fi

  if [[ ${pkg_failed} -eq 0 ]]; then
    pass "[5/${TOTAL_CHECKS}] Version sync & packaging: 4-way version ${py_out}, publishConfig, 5 platform manifests, node --check, execFileSync hygiene, py_compile & e2e self-test passed"
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

  if [[ ${check6_failed} -eq 0 ]]; then
    pass "[6/${TOTAL_CHECKS}] Rust & UI: fmt/clippy/test passed, 0 forbidden sinks/external tags, ui/dist/index.html is ${bundle_bytes} bytes (< ${max_bytes})"
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

  if ! python3 -B - "${REPO_ROOT}" <<'PYEOF'
import json
import pathlib
import re
import sys
import xml.parsers.expat as expat

MAX_LOGO_BYTES = 1_048_576
MAX_SVG_DEPTH = 64
MAX_SVG_ELEMENTS = 10_000

SVG_NS = "http://www.w3.org/2000/svg"
XLINK_NS = "http://www.w3.org/1999/xlink"
XML_NS = "http://www.w3.org/XML/1998/namespace"
ALLOWED_NAMESPACES = frozenset({SVG_NS, XLINK_NS, XML_NS})

ALLOWED_SVG_TAGS = frozenset({
    "svg",
    "g",
    "defs",
    "title",
    "desc",
    "symbol",
    "use",
    "rect",
    "circle",
    "ellipse",
    "line",
    "polyline",
    "polygon",
    "path",
    "linearGradient",
    "radialGradient",
    "stop",
    "clipPath",
    "mask",
    "pattern",
    "marker",
    "filter",
    "feBlend",
    "feColorMatrix",
    "feComponentTransfer",
    "feFuncR",
    "feFuncG",
    "feFuncB",
    "feFuncA",
    "feComposite",
    "feDropShadow",
    "feFlood",
    "feGaussianBlur",
    "feMerge",
    "feMergeNode",
    "feMorphology",
    "feOffset",
    "feTurbulence",
})

TEXT_ONLY_TAGS = frozenset({"title", "desc"})
HREF_ALLOWED_TAGS = frozenset({"use", "linearGradient", "radialGradient", "pattern"})
ALLOWED_XLINK_ATTRS = frozenset({"href"})
ALLOWED_XML_ATTRS = frozenset({"id", "lang", "space"})
ALLOWED_UNNS_ATTRS = frozenset({
    "id",
    "role",
    "aria-labelledby",
    "aria-hidden",
    "focusable",
    "lang",
    "version",
    "width",
    "height",
    "viewBox",
    "preserveAspectRatio",
    "x",
    "y",
    "x1",
    "y1",
    "x2",
    "y2",
    "cx",
    "cy",
    "r",
    "rx",
    "ry",
    "fx",
    "fy",
    "fr",
    "d",
    "points",
    "pathLength",
    "transform",
    "transform-origin",
    "gradientUnits",
    "gradientTransform",
    "spreadMethod",
    "patternUnits",
    "patternContentUnits",
    "patternTransform",
    "clipPathUnits",
    "maskUnits",
    "maskContentUnits",
    "filterUnits",
    "primitiveUnits",
    "markerUnits",
    "markerWidth",
    "markerHeight",
    "refX",
    "refY",
    "orient",
    "overflow",
    "fill",
    "fill-opacity",
    "fill-rule",
    "stroke",
    "stroke-opacity",
    "stroke-width",
    "stroke-linecap",
    "stroke-linejoin",
    "stroke-miterlimit",
    "stroke-dasharray",
    "stroke-dashoffset",
    "opacity",
    "color",
    "stop-color",
    "stop-opacity",
    "flood-color",
    "flood-opacity",
    "visibility",
    "display",
    "paint-order",
    "shape-rendering",
    "color-interpolation",
    "color-interpolation-filters",
    "image-rendering",
    "vector-effect",
    "isolation",
    "mix-blend-mode",
    "mask-type",
    "clip-path",
    "clip-rule",
    "mask",
    "filter",
    "marker-start",
    "marker-mid",
    "marker-end",
    "offset",
    "href",
    "in",
    "in2",
    "result",
    "mode",
    "type",
    "values",
    "operator",
    "k1",
    "k2",
    "k3",
    "k4",
    "stdDeviation",
    "edgeMode",
    "radius",
    "dx",
    "dy",
    "baseFrequency",
    "numOctaves",
    "seed",
    "stitchTiles",
    "slope",
    "intercept",
    "amplitude",
    "exponent",
    "tableValues",
})

URL_ATTRS = frozenset({
    "fill",
    "stroke",
    "clip-path",
    "mask",
    "filter",
    "marker-start",
    "marker-mid",
    "marker-end",
})
COLOR_FUNC_ATTRS = frozenset({
    "fill",
    "stroke",
    "stop-color",
    "flood-color",
    "color",
})
TRANSFORM_ATTRS = frozenset({
    "transform",
    "gradientTransform",
    "patternTransform",
})
PAREN_ALLOWED_ATTRS = URL_ATTRS | COLOR_FUNC_ATTRS | TRANSFORM_ATTRS
COLOR_FUNCS = frozenset({"rgb", "rgba", "hsl", "hsla"})
TRANSFORM_FUNCS = frozenset({"matrix", "translate", "scale", "rotate", "skewX", "skewY"})

LOCAL_FRAG_RE = re.compile(r"^#[A-Za-z_][A-Za-z0-9_.:-]*$")
CTRL_CHAR_RE = re.compile(r"[\x00-\x1f\x7f]")
FORBIDDEN_ATTR_TOKENS_RE = re.compile(
    r"(?:\\|/\*|\*/|//|@import|-moz-binding|expression\s*\()",
    re.IGNORECASE,
)
FUNC_CALL_RE = re.compile(r"(?<![A-Za-z0-9_.-])([A-Za-z_-][A-Za-z0-9_-]*)(\s*)\(([^()]*)\)")
NUMERIC_FUNC_ARGS_RE = re.compile(r"^[0-9.,eE% /+-]+$")
RFC3986_SCHEME_CHARS = frozenset(
    "ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+.-"
)


class _SvgAbort(Exception):
    pass


def _has_rfc3986_scheme(text):
    if ":" not in text:
        return False
    for piece in text.split(":")[:-1]:
        head = piece.rstrip()
        idx = len(head)
        while idx > 0 and head[idx - 1] in RFC3986_SCHEME_CHARS:
            idx -= 1
        if any(("A" <= ch <= "Z") or ("a" <= ch <= "z") for ch in head[idx:]):
            return True
    return False


def _split_ns(name):
    if " " in name:
        return name.rsplit(" ", 1)
    return "", name


def _validate_attr_value(local_attr, attr_val):
    if CTRL_CHAR_RE.search(attr_val):
        raise _SvgAbort(f"SVG logo attribute '{local_attr}' contains forbidden control character")
    if FORBIDDEN_ATTR_TOKENS_RE.search(attr_val):
        raise _SvgAbort(f"SVG logo attribute '{local_attr}' contains forbidden CSS/URI token")

    paren_depth = 0
    open_count = 0
    for ch in attr_val:
        if ch == "(":
            paren_depth += 1
            open_count += 1
            if paren_depth > 1:
                raise _SvgAbort(f"SVG logo attribute '{local_attr}' contains nested parentheses")
        elif ch == ")":
            paren_depth -= 1
            if paren_depth < 0:
                raise _SvgAbort(f"SVG logo attribute '{local_attr}' contains unbalanced parentheses")
    if paren_depth != 0:
        raise _SvgAbort(f"SVG logo attribute '{local_attr}' contains unclosed parentheses")

    url_spans = []
    url_targets = []
    if open_count > 0:
        if local_attr not in PAREN_ALLOWED_ATTRS:
            raise _SvgAbort(f"SVG logo attribute '{local_attr}' does not permit functional notation")
        matched_parens = 0
        for m in FUNC_CALL_RE.finditer(attr_val):
            matched_parens += 1
            fn_name = m.group(1)
            fn_ws = m.group(2)
            fn_arg = m.group(3)
            fn_lower = fn_name.lower()
            if fn_lower == "url":
                if local_attr not in URL_ATTRS:
                    raise _SvgAbort(f"SVG logo attribute '{local_attr}' does not permit url()")
                if fn_ws:
                    raise _SvgAbort(
                        f"SVG logo attribute '{local_attr}' must not have whitespace between url and '('"
                    )
                inner = fn_arg.strip(" ")
                if len(inner) >= 2 and inner[0] in ("'", '"') and inner[-1] == inner[0]:
                    inner = inner[1:-1]
                if not LOCAL_FRAG_RE.fullmatch(inner):
                    raise _SvgAbort(
                        f"SVG logo attribute '{local_attr}' contains non-local url() reference"
                    )
                url_spans.append((m.start(), m.end()))
                url_targets.append(inner[1:])
            elif fn_lower in COLOR_FUNCS:
                if local_attr not in COLOR_FUNC_ATTRS:
                    raise _SvgAbort(f"SVG logo attribute '{local_attr}' does not permit {fn_name}()")
                if fn_ws:
                    raise _SvgAbort(
                        f"SVG logo attribute '{local_attr}' must not have whitespace before '(' in {fn_name}()"
                    )
                if not NUMERIC_FUNC_ARGS_RE.fullmatch(fn_arg) or not fn_arg.strip(" "):
                    raise _SvgAbort(
                        f"SVG logo attribute '{local_attr}' contains invalid {fn_name}() arguments"
                    )
            elif fn_name in TRANSFORM_FUNCS:
                if local_attr not in TRANSFORM_ATTRS:
                    raise _SvgAbort(f"SVG logo attribute '{local_attr}' does not permit {fn_name}()")
                if fn_ws and fn_ws.strip(" "):
                    raise _SvgAbort(
                        f"SVG logo attribute '{local_attr}' contains forbidden whitespace before '('"
                    )
                if not NUMERIC_FUNC_ARGS_RE.fullmatch(fn_arg) or not fn_arg.strip(" "):
                    raise _SvgAbort(
                        f"SVG logo attribute '{local_attr}' contains invalid {fn_name}() arguments"
                    )
            else:
                raise _SvgAbort(
                    f"SVG logo attribute '{local_attr}' contains disallowed function '{fn_name}()'"
                )
        if matched_parens != open_count:
            raise _SvgAbort(
                f"SVG logo attribute '{local_attr}' contains invalid parenthesized expression"
            )

    if local_attr not in ("id", "href"):
        if url_spans:
            parts = []
            pos = 0
            for start_idx, end_idx in url_spans:
                parts.append(attr_val[pos:start_idx])
                pos = end_idx
            parts.append(attr_val[pos:])
            remainder = " ".join(parts)
        else:
            remainder = attr_val
        if _has_rfc3986_scheme(remainder):
            raise _SvgAbort(f"SVG logo attribute '{local_attr}' contains forbidden URI scheme")

    return url_targets


def _validate_svg_bytes(raw):
    try:
        raw.decode("utf-8")
    except UnicodeDecodeError as exc:
        return [f"plugin/plugin.json: SVG logo is not valid UTF-8 ({exc})"]

    stack = []
    id_stack = []
    ref_graph = {}
    seen_root = False
    elem_count = 0
    xml_id_key = f"{XML_NS} id"

    def _abort(msg):
        raise _SvgAbort(msg)

    def _on_xml_decl(version, encoding, _standalone):
        if version not in (None, "1.0"):
            _abort(f"SVG logo XML declaration version must be 1.0, got {version!r}")
        if encoding is not None and encoding.lower() != "utf-8":
            _abort(f"SVG logo XML declaration encoding must be UTF-8, got {encoding!r}")

    def _on_start_ns(_prefix, uri):
        if uri not in ALLOWED_NAMESPACES:
            _abort(f"SVG logo declares forbidden XML namespace {uri!r}")

    def _on_start_elem(name, attrs):
        nonlocal seen_root, elem_count
        elem_ns, local_tag = _split_ns(name)
        if not seen_root:
            seen_root = True
            if (elem_ns, local_tag) != (SVG_NS, "svg"):
                _abort(
                    f"SVG logo root element must be {{{SVG_NS}}}svg, got {{{elem_ns}}}{local_tag}"
                )
        else:
            if elem_ns != SVG_NS:
                _abort(f"SVG logo element <{local_tag}> uses forbidden namespace {elem_ns!r}")
            if stack and stack[-1] in TEXT_ONLY_TAGS:
                _abort(f"SVG logo element <{stack[-1]}> must not contain child elements")

        if local_tag not in ALLOWED_SVG_TAGS:
            _abort(f"SVG logo contains disallowed element <{local_tag}>")

        elem_count += 1
        if elem_count > MAX_SVG_ELEMENTS:
            _abort(f"SVG logo exceeds maximum element count ({MAX_SVG_ELEMENTS})")
        if len(stack) + 1 > MAX_SVG_DEPTH:
            _abort(f"SVG logo exceeds maximum nesting depth ({MAX_SVG_DEPTH})")

        parent_ids = id_stack[-1] if id_stack else ()
        elem_ids = []
        raw_id = attrs.get("id")
        if raw_id:
            elem_ids.append(raw_id)
        raw_xml_id = attrs.get(xml_id_key)
        if raw_xml_id and raw_xml_id not in elem_ids:
            elem_ids.append(raw_xml_id)
        for pid in parent_ids:
            for eid in elem_ids:
                ref_graph.setdefault(pid, set()).add(eid)
        effective_ids = tuple(elem_ids) if elem_ids else parent_ids
        owner_ids = elem_ids if elem_ids else list(parent_ids)

        for attr_name, attr_val in attrs.items():
            attr_ns, local_attr = _split_ns(attr_name)
            if local_attr.lower().startswith("on"):
                _abort(f"SVG logo contains forbidden event handler attribute '{local_attr}'")
            if attr_ns == "":
                if local_attr not in ALLOWED_UNNS_ATTRS:
                    _abort(
                        f"SVG logo element <{local_tag}> contains disallowed attribute '{local_attr}'"
                    )
            elif attr_ns == XLINK_NS:
                if local_attr not in ALLOWED_XLINK_ATTRS:
                    _abort(
                        f"SVG logo element <{local_tag}> contains disallowed xlink attribute '{local_attr}'"
                    )
            elif attr_ns == XML_NS:
                if local_attr not in ALLOWED_XML_ATTRS:
                    _abort(
                        f"SVG logo element <{local_tag}> contains disallowed xml attribute '{local_attr}'"
                    )
            else:
                _abort(
                    f"SVG logo element <{local_tag}> contains attribute in forbidden namespace {attr_ns!r}"
                )

            if local_attr == "href" and attr_ns in ("", XLINK_NS):
                if local_tag not in HREF_ALLOWED_TAGS:
                    _abort(
                        f"SVG logo element <{local_tag}> does not permit '{local_attr}' attribute"
                    )
                if not LOCAL_FRAG_RE.fullmatch(attr_val):
                    _abort(
                        f"SVG logo element <{local_tag}> '{local_attr}' must be a local #id fragment"
                    )
                target_id = attr_val[1:]
                for owner in owner_ids:
                    ref_graph.setdefault(owner, set()).add(target_id)

            for target_id in _validate_attr_value(local_attr, attr_val):
                for owner in owner_ids:
                    ref_graph.setdefault(owner, set()).add(target_id)

        stack.append(local_tag)
        id_stack.append(effective_ids)

    def _on_end_elem(_name):
        stack.pop()
        id_stack.pop()

    def _on_char_data(data):
        if not data.strip(" \t\r\n"):
            return
        if not stack or stack[-1] not in TEXT_ONLY_TAGS:
            parent = stack[-1] if stack else "document"
            _abort(f"SVG logo element <{parent}> must not contain text content")

    parser = expat.ParserCreate(namespace_separator=" ")
    parser.XmlDeclHandler = _on_xml_decl
    parser.StartDoctypeDeclHandler = lambda *_a: _abort(
        "SVG logo must not contain DOCTYPE or DTD declarations"
    )
    parser.EntityDeclHandler = lambda *_a: _abort(
        "SVG logo must not contain ENTITY declarations"
    )
    parser.UnparsedEntityDeclHandler = lambda *_a: _abort(
        "SVG logo must not contain unparsed ENTITY declarations"
    )
    parser.NotationDeclHandler = lambda *_a: _abort(
        "SVG logo must not contain NOTATION declarations"
    )
    parser.AttlistDeclHandler = lambda *_a: _abort(
        "SVG logo must not contain ATTLIST declarations"
    )
    parser.ElementDeclHandler = lambda *_a: _abort(
        "SVG logo must not contain ELEMENT declarations"
    )
    parser.ExternalEntityRefHandler = lambda *_a: _abort(
        "SVG logo must not reference external entities"
    )
    parser.ProcessingInstructionHandler = lambda target, _data: _abort(
        f"SVG logo must not contain XML processing instructions (<?{target}?>)"
    )
    parser.StartCdataSectionHandler = lambda: _abort(
        "SVG logo must not contain CDATA sections"
    )
    parser.StartNamespaceDeclHandler = _on_start_ns
    parser.StartElementHandler = _on_start_elem
    parser.EndElementHandler = _on_end_elem
    parser.CharacterDataHandler = _on_char_data

    try:
        parser.Parse(raw, True)
    except _SvgAbort as exc:
        return [f"plugin/plugin.json: {exc}"]
    except expat.ExpatError as exc:
        return [f"plugin/plugin.json: SVG logo is not valid XML ({exc})"]

    if not seen_root:
        return ["plugin/plugin.json: SVG logo has no root element"]

    # Iterative 3-color DFS cycle detection for local fragment references (<use>/gradient/pattern)
    color = {}
    for start in list(ref_graph.keys()):
        if color.get(start, 0) != 0:
            continue
        dfs_stack = [(start, iter(ref_graph.get(start, ())))]
        color[start] = 1
        while dfs_stack:
            node, it = dfs_stack[-1]
            try:
                nxt = next(it)
            except StopIteration:
                color[node] = 2
                dfs_stack.pop()
                continue
            state = color.get(nxt, 0)
            if state == 1:
                return ["plugin/plugin.json: SVG logo contains cyclic local fragment reference"]
            if state == 0:
                color[nxt] = 1
                dfs_stack.append((nxt, iter(ref_graph.get(nxt, ()))))

    return []

root = pathlib.Path(sys.argv[1])
manifests = [
    "plugin/plugin.json",
    "plugin/mcp_config.json",
    "plugin/sidecars/proxy/sidecar.json",
]
errors = []
parsed = {}
for rel in manifests:
    path = root / rel
    if not path.is_file():
        errors.append(f"missing {rel}")
        continue
    try:
        data = json.loads(path.read_text(encoding="utf-8"))
        if not isinstance(data, dict):
            errors.append(f"{rel}: top-level JSON must be an object")
        else:
            parsed[rel] = data
    except Exception as exc:
        errors.append(f"{rel}: invalid JSON ({exc})")

plugin_manifest = parsed.get("plugin/plugin.json")
if isinstance(plugin_manifest, dict):
    logo = plugin_manifest.get("logo")
    if not isinstance(logo, str) or not logo.strip() or logo != logo.strip():
        errors.append("plugin/plugin.json: 'logo' must be a non-empty trimmed string")
    elif "\x00" in logo or logo.startswith("/") or "\\" in logo:
        errors.append(f"plugin/plugin.json: invalid 'logo' path {logo!r}")
    else:
        raw_parts = logo.split("/")
        logo_rel = pathlib.PurePosixPath(logo)
        if (
            not raw_parts
            or any(seg in {"", ".", ".."} for seg in raw_parts)
            or logo_rel.is_absolute()
            or ".." in logo_rel.parts
            or not logo_rel.parts
        ):
            errors.append(
                f"plugin/plugin.json: 'logo' must be a canonical relative path without '.', '..', or empty segments: {logo!r}"
            )
        elif logo_rel.suffix.lower() not in {".svg", ".png", ".jpg", ".jpeg", ".webp"}:
            errors.append(f"plugin/plugin.json: unsupported 'logo' extension {logo_rel.suffix!r}")
        else:
            plugin_dir = root / "plugin"
            plugin_dir_resolved = plugin_dir.resolve()
            logo_path = plugin_dir.joinpath(*logo_rel.parts)
            cur = plugin_dir
            symlink_segment = False
            for part in logo_rel.parts:
                cur = cur / part
                if cur.is_symlink():
                    symlink_segment = True
                    break
            if symlink_segment or logo_path.is_symlink():
                errors.append(f"plugin/plugin.json: 'logo' must not traverse or point to a symlink: {logo!r}")
            elif not logo_path.exists() or not logo_path.is_file():
                errors.append(f"plugin/plugin.json: 'logo' target is missing or not a regular file: {logo!r}")
            else:
                try:
                    logo_resolved = logo_path.resolve()
                    logo_resolved.relative_to(plugin_dir_resolved)
                except Exception:
                    errors.append(f"plugin/plugin.json: 'logo' escapes plugin root: {logo!r}")
                else:
                    logo_size = logo_path.stat().st_size
                    if logo_size == 0:
                        errors.append(f"plugin/plugin.json: 'logo' file is empty: {logo!r}")
                    elif logo_size > MAX_LOGO_BYTES:
                        errors.append(
                            f"plugin/plugin.json: 'logo' file exceeds {MAX_LOGO_BYTES} bytes ({logo_size}): {logo!r}"
                        )
                    else:
                        raw = logo_path.read_bytes()
                        if len(raw) == 0:
                            errors.append(f"plugin/plugin.json: 'logo' file is empty: {logo!r}")
                        elif len(raw) > MAX_LOGO_BYTES:
                            errors.append(
                                f"plugin/plugin.json: 'logo' file exceeds {MAX_LOGO_BYTES} bytes ({len(raw)}): {logo!r}"
                            )
                        elif logo_rel.suffix.lower() == ".svg":
                            errors.extend(_validate_svg_bytes(raw))

if errors:
    for err in errors:
        print(err, file=sys.stderr)
    sys.exit(1)
PYEOF
  then
    fail "[7/${TOTAL_CHECKS}] Plugin bundle: JSON manifest and logo validation failed"
    plugin_failed=1
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
