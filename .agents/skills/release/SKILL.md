---
name: release
description: Manage agy-companion SemVer dev/release versioning (X.Y.0-dev.N vs final X.Y.Z), cut and tag public releases across GitHub Releases, PyPI, and npm, verify OIDC Trusted Publishing and GitHub Environments, monitor release.yml runs, and run post-release live verification. Use when preparing a release PR, curating CHANGELOG.md, promoting -dev.N to X.Y.Z, running dry-run release builds, tagging vX.Y.Z, troubleshooting release.yml failures, or verifying published artifacts. Do NOT use for local plugin linking or Sidecar UI dev (use local-dev) or writing automated tests (use cuj-testing).
---

# Release Lifecycle & Publishing Runbook

Use this skill when managing `agy-companion` dev/release SemVer versions, running release dry-runs, preparing a release PR, checking OIDC Trusted Publishing & GitHub Environments, cutting and monitoring a `vX.Y.Z` tag, or verifying live published artifacts.

Related references:
- Architecture & one-time setup: [`docs/design/release-pipeline.md`](../../../docs/design/release-pipeline.md)
- Release workflow: [`.github/workflows/release.yml`](../../../.github/workflows/release.yml)
- Changelog: [`CHANGELOG.md`](../../../CHANGELOG.md)
- PR & workflow policy: [`.agents/rules/workflow.md`](../../rules/workflow.md)

## 1. Versioning Model (Daily Dev vs. Public Release)

All four synced manifests (`Cargo.toml`, `plugin/plugin.json`, `packages/npm/agy-companion/package.json` + its 5 `@agy-companion/*` `optionalDependencies`, `packages/python/pyproject.toml`) and `Cargo.lock` hold the exact same SemVer string, parsed and ordered by the shared stdlib helper [`scripts/versioning.py`](../../../scripts/versioning.py) (`0.0.12 < 0.1.0-dev.0 < 0.1.0-dev.1 < 0.1.0 < 0.2.0-dev.0`).

| Scenario | Transition | Command |
| --- | --- | --- |
| First dev PR after a public release | `X.Y.Z` → `X.(Y+1).0-dev.0` (e.g., `0.1.0` → `0.2.0-dev.0`) | `python3 -B scripts/bump_version.py` |
| Subsequent daily dev PR | `X.Y.Z-dev.N` → `X.Y.Z-dev.(N+1)` (e.g., `0.2.0-dev.0` → `0.2.0-dev.1`) | `python3 -B scripts/bump_version.py` |
| Release-prep PR promotion | `X.Y.Z-dev.N` → `X.Y.Z` (fails closed if already final) | `python3 -B scripts/bump_version.py --release` |
| Explicit manual overrides | Final SemVer component or explicit `X.Y.Z[-dev.N]` | `python3 -B scripts/bump_version.py --part patch\|minor\|major` or `--set <ver>` |

Key invariants:
- **PEP 440 wheel normalization:** `packages/python/pyproject.toml` keeps exact 4-way SemVer parity (e.g., `"0.2.0-dev.0"`). `maturin` normalizes `-dev.N` to PEP 440 `.devN` inside built wheel filenames, `METADATA`, and `.data/scripts/` paths (`agy_companion-0.2.0.dev0.data/scripts/agy-companion`), while the compiled Rust binary prints `agy-companion 0.2.0-dev.0`. `scripts/package_release_asset.py` uses `versioning.to_pep440()` for wheel extraction and raw SemVer for `--version` smoke tests and archive names.
- **Fail-closed live publish guard:** `.github/workflows/release.yml` allows `X.Y.Z-dev.N` on `dry_run: true` runs from `main`, and **strictly refuses** live publishing (`publish == true`) for any `-dev.N` version (enforced both in `scripts/package_release_asset.py resolve-release` and via job-level `!contains(needs.verify.outputs.version, '-')` guards on `github-release`, `publish-npm`, and `publish-pypi`).
- **Authorization boundary:** Regular PRs to `main` and `dry_run=true` workflow dispatches are covered by repo standing authorization (`.agents/rules/workflow.md`). **Pushing a live `v*` tag or publishing live to GitHub Releases, npm, and PyPI requires explicit human authorization.**

## 2. Optional Pre-Release Dry Run

Validate the full 5-target build and packaging pipeline on `main` (works for both `X.Y.Z-dev.N` and final `X.Y.Z`) without publishing:

```bash
gh workflow run release.yml --ref main -f dry_run=true
gh run list --workflow=release.yml --limit 5
gh run watch <run_id>
```

Expected positive signal:
- `verify`, all 5 `build` matrix jobs (`darwin-arm64`, `darwin-x64`, `linux-x64`, `linux-arm64`, `win32-x64`), and `package-verify` (`checksums.txt`, `run-companion.sh` extraction, win32 zip verification, `npm pack` + offline install smoke, `twine check --strict` + venv wheel smoke) succeed.
- All 3 publish jobs (`github-release`, `publish-npm`, `publish-pypi`) are **skipped**.

## 3. Release-Prep PR Checklist

1. Start from an up-to-date `main` branch:
   ```bash
   git checkout main && git pull --ff-only
   git checkout -b chore/release-vX.Y.Z
   ```
2. Promote the prerelease dev version to the final release version:
   ```bash
   python3 -B scripts/bump_version.py --release
   ```
   Confirm all 4 manifests (`Cargo.toml`, `plugin/plugin.json`, `packages/npm/agy-companion/package.json` + its 5 `optionalDependencies`, `packages/python/pyproject.toml`) and `Cargo.lock` moved cleanly from `X.Y.Z-dev.N` to `X.Y.Z`.
3. Curate `CHANGELOG.md`:
   - Review commits since the previous tag:
     ```bash
     git log "$(git describe --tags --abbrev=0)..HEAD" --oneline
     ```
   - Move `## [Unreleased]` items into a new `## [X.Y.Z] - YYYY-MM-DD` section (Keep a Changelog categories: `Added`, `Changed`, `Fixed`, `Security`, `Removed`), leaving `## [Unreleased]` empty above it.
   - Update the bottom compare/release links:
     - `[Unreleased]: https://github.com/Jacksunwei/agy-companion/compare/vX.Y.Z...HEAD`
     - `[X.Y.Z]: https://github.com/Jacksunwei/agy-companion/compare/vPREV...vX.Y.Z`
4. Run full local verification gates:
   ```bash
   ./scripts/agent_check.sh
   python3 -B scripts/check.py --profile repo --verify-ui-build
   ```
5. Commit (`chore(release): prepare vX.Y.Z release`, zero AI attribution), push, open the PR, confirm all CI checks (`repo-checks` + `ubuntu`/`macos`/`windows`) are green, squash-merge, and fast-forward `main`:
   ```bash
   git checkout main && git pull --ff-only
   ```

## 4. Pre-Tag Registry & OIDC Verification

Confirm every preflight item below before pushing a live `vX.Y.Z` tag:

| Surface | Requirement |
| --- | --- |
| **GitHub Environments** (`Jacksunwei/agy-companion` → Settings → Environments) | Both `pypi` and `npm` environments exist and restrict deployments to protected tags `v*.*.*`. |
| **Zero Static `NPM_TOKEN`** | Confirm `NPM_TOKEN` is **not** set in the `npm` GitHub Environment (all 6 npm packages were bootstrapped in `v0.1.0` and publish via tokenless OIDC; `release.yml` fails closed if bundled `npm < 11.5.1`). |
| **PyPI Trusted Publisher** (`pypi.org/manage/project/agy-companion/settings/publishing/`) | Package `agy-companion` configured with Owner `Jacksunwei`, Repo `agy-companion`, Workflow `release.yml`, Environment `pypi`. |
| **npm Trusted Publishers (all 6 packages)** | `agy-companion`, `@agy-companion/darwin-arm64`, `@agy-companion/darwin-x64`, `@agy-companion/linux-x64`, `@agy-companion/linux-arm64`, and `@agy-companion/win32-x64` each list Owner `Jacksunwei`, Repo `agy-companion`, Workflow `release.yml`, Environment `npm`, with **`Allow npm publish`** enabled (and **"Require two-factor authentication and disallow tokens"** enabled once verified). |
| **npm 48-hour unused expiry rule** | npm expires a newly configured Trusted Publisher if no successful OIDC publish completes within **48 hours** of adding it. If any Trusted Publisher was recently added or rotated, attach/re-verify it right before cutting the tag. |

## 5. Tagging, Monitoring & Idempotent Re-Runs

After explicit human authorization to publish live:

```bash
git checkout main && git pull --ff-only
ver="$(python3 -B -c "import tomllib, pathlib; print(tomllib.loads(pathlib.Path('Cargo.toml').read_text(encoding='utf-8'))['workspace']['package']['version'])")"
git tag -a "v${ver}" -m "Release v${ver}"
git push origin "v${ver}"
```

Monitor the triggered workflow run:

```bash
gh run list --workflow=release.yml --limit 5
gh run watch <run_id>
```

Idempotent failure recovery:
- `github-release` checks `gh release view` and uses `--clobber` if the release already exists.
- `publish-npm` checks `npm view "${pkg_name}@${VERSION}" version` and skips packages already published at that version.
- `publish-pypi` sets `skip-existing: true`.
- Recover transient runner or registry failures safely with:
  ```bash
  gh run rerun <run_id> --failed
  ```

## 6. Post-Release Live Verification

Verify all three live channels after `release.yml` completes:

1. **GitHub Release assets & SLSA Build Provenance:**
   ```bash
   verify_dir="$(mktemp -d)"
   gh release view "v${ver}" --repo Jacksunwei/agy-companion
   gh release download "v${ver}" --repo Jacksunwei/agy-companion \
     -p "agy-companion-v${ver}-darwin-arm64.tar.gz" \
     -p "checksums.txt" \
     -D "${verify_dir}"
   gh attestation verify "${verify_dir}/agy-companion-v${ver}-darwin-arm64.tar.gz" --repo Jacksunwei/agy-companion
   gh attestation verify "${verify_dir}/checksums.txt" --repo Jacksunwei/agy-companion
   rm -rf "${verify_dir}"
   ```
   Expected positive signal: 6 assets present (5 platform archives + `checksums.txt`) and `Verification succeeded!` on both provenance checks.

2. **PyPI live install & version check:**
   ```bash
   uvx --refresh --default-index https://pypi.org/simple "agy-companion==${ver}" --version
   ```
   Expected positive signal: `agy-companion X.Y.Z`.

3. **npm live registry & `npx` check:**
   ```bash
   for pkg in \
     agy-companion \
     @agy-companion/darwin-arm64 \
     @agy-companion/darwin-x64 \
     @agy-companion/linux-x64 \
     @agy-companion/linux-arm64 \
     @agy-companion/win32-x64; do
     echo "${pkg}: $(npm view "${pkg}" dist-tags.latest --registry=https://registry.npmjs.org/)"
   done
   npx -y --registry=https://registry.npmjs.org/ agy-companion@latest --version
   ```
   Expected positive signal: all 6 packages report `X.Y.Z` for `dist-tags.latest`, and `npx` prints `agy-companion X.Y.Z`.
   - **CDN cache lag note:** The `https://www.npmjs.com/package/agy-companion` web UI and badge endpoints can lag behind `https://registry.npmjs.org/` due to CDN caching; treat `npm view ... --registry=https://registry.npmjs.org/` and `npx` as authoritative.

## 7. Post-Release Next Dev Cycle

No manual version-reset commit is required after tagging `vX.Y.Z`. On the very next feature, fix, or chore PR, running:

```bash
python3 -B scripts/bump_version.py
```

automatically transitions `X.Y.Z` → `X.(Y+1).0-dev.0` across all 4 synced manifests and `Cargo.lock`.
