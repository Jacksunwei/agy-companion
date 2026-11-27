# Release Pipeline (GitHub Releases, npm & PyPI)

This document describes the automated release pipeline in [`.github/workflows/release.yml`](../../.github/workflows/release.yml) and the one-time setup required on GitHub, PyPI, and npm before tagging a release.

## Distribution Channels & Target Matrix

Each release builds every native binary **once per target** via `PyO3/maturin-action` (`packages/python/pyproject.toml`, `bindings = "bin"`, `strip = true`), then reuses that exact stripped binary across all three distribution channels:

| Target | Rust Triple | Runner | Linux libc / macOS min | GitHub Release Asset | npm Package | PyPI Wheel |
| --- | --- | --- | --- | --- | --- | --- |
| `darwin-arm64` | `aarch64-apple-darwin` | `macos-latest` | macOS `>= 11.0` | `agy-companion-vX.Y.Z-darwin-arm64.tar.gz` | `@agy-companion/darwin-arm64` | `py3-none-macosx_11_0_arm64` |
| `darwin-x64` | `x86_64-apple-darwin` | `macos-15-intel` | macOS `>= 11.0` | `agy-companion-vX.Y.Z-darwin-x64.tar.gz` | `@agy-companion/darwin-x64` | `py3-none-macosx_11_0_x86_64` |
| `linux-x64` | `x86_64-unknown-linux-gnu` | `ubuntu-latest` | `manylinux_2_28` (glibc `>= 2.28`) | `agy-companion-vX.Y.Z-linux-x64.tar.gz` | `@agy-companion/linux-x64` | `py3-none-manylinux_2_28_x86_64` |
| `linux-arm64` | `aarch64-unknown-linux-gnu` | `ubuntu-24.04-arm` | `manylinux_2_28` (glibc `>= 2.28`) | `agy-companion-vX.Y.Z-linux-arm64.tar.gz` | `@agy-companion/linux-arm64` | `py3-none-manylinux_2_28_aarch64` |
| `win32-x64` | `x86_64-pc-windows-msvc` | `windows-latest` | MSVC | `agy-companion-vX.Y.Z-win32-x64.zip` | `@agy-companion/win32-x64` | `py3-none-win_amd64` |

Additionally:

- **GitHub Releases** publishes deterministic archives (`gzip.GzipFile(filename="", mtime=0)` + PAX tar / fixed-epoch zip), sorted `checksums.txt` (`<sha256>  <filename>`) verified against `plugin/scripts/run-companion.sh`'s `verify_sha256` and `extract_verified_binary`, categorized release notes (`.github/release.yml`), and **SLSA Build Provenance** attestations via `actions/attest` (`gh release create --draft --verify-tag` → `gh release edit --draft=false`).
- **npm** publishes the 5 `@agy-companion/<target>` binary packages first, then the `agy-companion` meta-package (`packages/npm/agy-companion/`) exposing `agy-companion` and `agyc` via `optionalDependencies`, all with Sigstore `--provenance`.
- **PyPI** publishes the 5 prebuilt `py3-none-*` binary wheels with PEP 740 attestations and `verify-metadata: true` (no source distribution, because `companion-ui` and `companion-cli` embed workspace-root `ui/dist/index.html` and `plugin/*` at compile time).

## Supply-Chain Hardening

- **Immutable action pins:** Every `uses:` step in `.github/workflows/release.yml` is pinned to a 40-character Git commit SHA (kept up to date weekly via `.github/dependabot.yml`).
- **Least-privilege permissions & checkout hygiene:** Workflow-level `permissions: {}` with minimal per-job scopes (`contents: read` for verify/build/package-verify; `contents: write`, `id-token: write`, `attestations: write` for `github-release`; `contents: read`, `id-token: write` for `publish-npm` and `publish-pypi`). Every `actions/checkout` sets `persist-credentials: false`, and `setup-node` in `package-verify` and `publish-npm` sets `package-manager-cache: false` (and never sets `registry-url`).
- **Hermetic release builds:** Cargo caching (`Swatinem/rust-cache`) is intentionally omitted from `release.yml`, and `publish-npm` fails closed if bundled `npm < 11.5.1` (no runtime `npm install -g`).
- **Verifying SLSA Build Provenance:** After a live release, verify any downloaded GitHub Release archive or `checksums.txt` with GitHub CLI:
  ```bash
  gh attestation verify agy-companion-vX.Y.Z-darwin-arm64.tar.gz --repo Jacksunwei/agy-companion
  ```

## Pipeline Stages

```mermaid
flowchart LR
    V["verify<br/>4-way version parity, tag check,<br/>check.py --verify-ui-build, all tests"]
    B["build (5-target matrix)<br/>hermetic maturin build + extract binary +<br/>--version smoke test + deterministic archive"]
    P["package-verify<br/>checksums.txt, run-companion.sh + win32 zip verify,<br/>npm pack --json + install smoke, twine + venv smoke"]
    GH["github-release<br/>actions/attest SLSA provenance +<br/>atomic draft->publish gh release"]
    NPM["publish-npm<br/>env: npm (OIDC or temp 0600 NPM_TOKEN)"]
    PYPI["publish-pypi<br/>env: pypi (OIDC Trusted Publishing + PEP 740)"]

    V --> B --> P
    P -->|"tag push (publish=true)"| GH
    GH --> NPM
    GH --> PYPI
```

- **Dry-run verification:** Trigger `Release` via GitHub Actions `workflow_dispatch` with `dry_run: true` (the default) from `main`. This executes `verify`, all 5 `build` jobs, and `package-verify` (including real `linux-x64` npm and wheel install smoke tests) without creating a GitHub Release, public attestations, or publishing to npm/PyPI.
- **Live release:** Push a matching SemVer tag `vX.Y.Z` (where `X.Y.Z` matches `Cargo.toml`, `plugin/plugin.json`, `packages/npm/agy-companion/package.json`, and `packages/python/pyproject.toml`). `github-release`, `publish-npm`, and `publish-pypi` are idempotent on re-run.

## One-Time Setup (GitHub, PyPI & npm)

### 1. GitHub Environments

Create two GitHub Environments under **Settings -> Environments** on `Jacksunwei/agy-companion`:

- `pypi`
- `npm`

Restrict both environments to protected tags (`v*.*.*`) under **Deployment branches and tags -> Selected branches and tags**.

### 2. PyPI Trusted Publishing (Zero Secrets)

In [PyPI `agy-companion` Publishing Settings](https://pypi.org/manage/project/agy-companion/settings/publishing/), add a **GitHub Actions** Trusted Publisher:

- **Owner:** `Jacksunwei`
- **Repository:** `agy-companion`
- **Workflow name:** `release.yml`
- **Environment name:** `pypi`

No `PYPI_API_TOKEN` secret is needed; PEP 740 attestations are uploaded automatically.

### 3. npm Organization & Trusted Publishing

1. The **`@agy-companion`** npm organization already exists on npmjs.com (verified `HTTP 200` on `registry.npmjs.org/-/org/agy-companion/package`, currently 0 published packages). Confirm under **npm Settings -> Organizations** that your npm account (`jacksunwei`) owns / has publish permissions on both `agy-companion` and `@agy-companion`.
2. **First release bootstrap (needed once for brand-new `@agy-companion/*` packages):**
   npm Trusted Publishing (OIDC) can only be attached to packages that already exist on npm. For the first release that creates `@agy-companion/{darwin-arm64,darwin-x64,linux-x64,linux-arm64,win32-x64}`:
   - Create a short-lived **Granular Access Token** on npmjs.com (e.g., 7-day expiration) scoped to read+write on `agy-companion` and the `@agy-companion` org (bypass 2FA for CI automation).
   - Add it as secret `NPM_TOKEN` inside the **`npm`** GitHub Environment.
   - Cut the first tag (`git tag -a vX.Y.Z -m "Release vX.Y.Z" && git push origin vX.Y.Z`). The workflow writes `NPM_TOKEN` to a temporary `0600` `NPM_CONFIG_USERCONFIG`, unsets `NPM_TOKEN`, and publishes all 5 `@agy-companion/*` packages and `agy-companion` with `--provenance --access public`.
3. **Switch to tokenless OIDC Trusted Publishing & lock down tokens (after the first release):**
   - On npmjs.com, open **Settings -> Trusted Publishers -> GitHub Actions** for all 6 packages (`agy-companion` and the 5 `@agy-companion/*` packages) and configure:
     - **Organization / user:** `Jacksunwei`
     - **Repository:** `agy-companion`
     - **Workflow filename:** `release.yml`
     - **Environment name:** `npm`
   - **48-hour activation rule:** npm expires a newly configured Trusted Publisher connection if no successful OIDC publish completes within **48 hours** of adding it. Delete `NPM_TOKEN` from the `npm` GitHub Environment, revoke the bootstrap token on npmjs.com, and cut the next patch release within 48 hours of adding the Trusted Publishers (or wait to attach them until right before cutting that release).
   - **Post-bootstrap lockdown:** Once OIDC publishing succeeds for all 6 packages, enable **"Require two-factor authentication and disallow tokens"** under each package's **Publishing access** on npmjs.com so static tokens can never be used to publish.

## Cutting a Release

Daily development PRs bump the **patch** version (`python3 -B scripts/bump_version.py`) without triggering a release. When ready to cut a public release:

1. Open a release-prep PR bumping the **minor** version and updating [`CHANGELOG.md`](../../CHANGELOG.md):
   ```bash
   git checkout -b chore/release-vX.Y.0
   python3 -B scripts/bump_version.py --part minor
   # Move [Unreleased] highlights in CHANGELOG.md under [X.Y.0] - YYYY-MM-DD
   ./scripts/agent_check.sh
   ```
   Merge the PR to `main` once CI is green.

2. Tag `main` and push the tag to trigger `.github/workflows/release.yml`:
   ```bash
   git checkout main && git pull --ff-only
   ver="$(python3 -B -c "import tomllib, pathlib; print(tomllib.loads(pathlib.Path('Cargo.toml').read_text())['workspace']['package']['version'])")"
   git tag -a "v${ver}" -m "Release v${ver}"
   git push origin "v${ver}"
   ```
