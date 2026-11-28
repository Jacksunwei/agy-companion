# AGENTS.md: How Coding Agents Work in This Repo

This repository uses **Antigravity customization conventions** (`AGENTS.md` + `.agents/rules/` + `.agents/skills/`). Never create redundant tool-specific root files such as `GEMINI.md`.

## Repo Layout

| Path | Role | Details |
| --- | --- | --- |
| `AGENTS.md` | Canonical repo contract | Single root entrypoint (never add `GEMINI.md`). |
| `README.md` | Project documentation | Overview, architecture, quickstarts, XDG/Windows paths, S1–S8, roadmap. |
| `.agents/rules/` | Workspace rules | `workflow.md` (`always_on`: PR auto-merge, TDD, local-dev & doc invariants); `product-ux.md` (`model_decision`: UX, zero-AI-slop & model-catalog guardrails). |
| `.agents/skills/` | On-demand skills | `local-dev/SKILL.md` (build, deploy, Sidecar Web UI, reload, stale-PID takeover, live E2E); `cuj-testing/SKILL.md` (CUJ-first Testing Trophy, Single-Owner layers, hermeticity); `release/SKILL.md` (dev/release SemVer lifecycle, changelog curation, OIDC/Environment preflight, tagging & live verification). |
| `Cargo.toml`, `Cargo.lock` | Rust workspace root | `edition = "2024"`, single version source of truth (bumped every PR via `scripts/bump_version.py`: `X.Y.Z-dev.N` daily bumping patch only, `--release` / `--part minor` for final `X.Y.Z`), Apache-2.0. |
| `crates/` | 5-crate Rust workspace (`#![forbid(unsafe_code)]`) | `companion-core`, `companion-providers`, `companion-proxy`, `companion-ui`, `companion-cli`. See each crate's `crates/*/README.md` for module boundaries and public APIs. |
| `ui/` | Build-time Aux Panel UI | Zero-runtime-dependency Vite 8 + TypeScript + `vite-plugin-singlefile` (`ui/.npmrc` pins `registry.npmjs.org`). Tests in `ui/tests/*.test.ts` + Rust↔TS golden fixtures in `ui/tests/fixtures/*.json`; builds committed `ui/dist/index.html` (`< 512,000` bytes, embedded via `include_str!`). |
| `plugin/` | Antigravity plugin bundle | 6 files: `plugin.json`, `assets/logo.svg`, `sidecars/proxy/sidecar.json` (`has_web_ui: true`), `scripts/run-companion.sh`, `skills/companion/SKILL.md`, `README.md`; validated with `agy plugin validate --strict ./plugin` and embedded into `companion-cli`. |
| `packages/` | Distribution shims | `packages/npm/agy-companion/` (`agy-companion` & `agyc`, 5 platform `optionalDependencies`), `packages/npm/generate-platform-pkgs.mjs`, and `packages/python/` (`maturin` `bindings = "bin"`). |
| `e2e/` | Opaque-box Python 3 E2E suite | Pure stdlib parallel CUJ, boundary, shim, and Chrome CDP UI smoke (`e2e/run_e2e.py --suite all --workers 4`). Never grep repo source/docs inside `e2e/`; always run with `python3 -B`. |
| `scripts/` | Verification & dev scripts | `check.py` (`--profile repo\|os\|all`, `--verify-ui-build`, `--with-e2e`), `versioning.py`, `bump_version.py`, `test_version_bump.py`, `agent_check.sh` (POSIX shim), `check_plugin_logo.py`, `package_release_asset.py`, `test_release_packaging.py`, `dev-install.sh`. |
| `.github/` | PR template, CI & release | `PULL_REQUEST_TEMPLATE.md`, `release.yml`, `dependabot.yml`, `workflows/ci.yml` (`repo-checks` on `ubuntu-latest` + `test` matrix on `[ubuntu, macos, windows]`), and `workflows/release.yml` (5-target release pipeline for GitHub Releases, npm & PyPI). |
| `docs/` | Design specs & ADRs | `docs/design/` (index at `docs/README.md`) and `docs/decisions/`. Update alongside behavior or architecture changes. |

## Verification Gates

> GitHub Actions CI (`.github/workflows/ci.yml`) runs on PRs and pushes to `main`. Run local gates before pushing and confirm CI is green before merging.

```bash
# Primary gate (all 7 repo checks + clippy + workspace tests + UI tests + SVG validator tests)
./scripts/agent_check.sh
# Cross-platform / CI-equivalent profiles:
python3 -B scripts/check.py --profile repo --verify-ui-build
python3 -B scripts/check.py --profile os --with-e2e

# Targeted fast checks during iteration
cargo fmt --all -- --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace

# UI typecheck, behavioral tests, bundle rebuild & determinism check
npm --prefix ui ci && npm --prefix ui run typecheck && npm --prefix ui test && npm --prefix ui run build
git diff --exit-code -- ui/dist/index.html

# Plugin & packaging shim checks
agy plugin validate --strict ./plugin
node --check packages/npm/agy-companion/bin/agy-companion.js
node --check packages/npm/generate-platform-pkgs.mjs

# Opaque-box E2E suite (-B prevents __pycache__)
cargo build -p companion-cli
python3 -B e2e/run_e2e.py --suite all --workers 4 --binary target/debug/agy-companion --repo-root . -v
python3 -B e2e/run_e2e.py --suite ui-smoke --repo-root . -v
python3 -B e2e/run_e2e.py --self-test -v  # only when modifying e2e/harness.py
```

**Verification discipline:** State the expected positive success signal before running a check, then verify it in real output before declaring success or merging.

## Architecture & Security Invariants

All 5 Rust crates (`crates/*/src/lib.rs`), `crates/companion-cli/src/main.rs`, and `crates/companion-cli/tests/cli_integration.rs` enforce `#![forbid(unsafe_code)]`. See [`docs/design/architecture.md`](docs/design/architecture.md) for full details.

| ID | Invariant | Enforced Contract |
| --- | --- | --- |
| **S1** | Loopback-only proxy | Bind exclusively to `127.0.0.1` / `localhost`; reject `0.0.0.0`, `::`, and non-loopback hosts; re-verify `local_addr.ip() == LOCALHOST`; expose no `--host` CLI flag. |
| **S2** | Secret redaction & atomic `0600` persistence | Return `RedactedConfig` only; quarantine `PROXY_PLACEHOLDER_API_KEY`; persist provider secrets outside `config.json` in sibling `0700` `auth/` (`0600` `auth/gemini.json` via `AuthStore`); enforce `#[serde(deny_unknown_fields)]` and mutex-serialized writes on `save_settings`; gate `/_api/*` with `X-Sidecar-Token` (`401`) and `application/json` (`415`); atomic `0600` writes (Unix) / bounded retry (Windows) with symlink/FIFO guards on config, `auth/`, and `usage.db` (`-wal`/`-shm`/`-journal`); cap `hyper`/`hyper_util`/`h2`/`reqwest` logs at `INFO`; zero real `AIza...` keys anywhere. |
| **S3** | Upstream auth & SSRF defense | Authenticate via `x-goog-api-key` (strip `?key=`, keep `?alt=sse`); require `https://` for remote `base_url` (`http://` only for `127.0.0.1`/`localhost`); reject credentials, backslashes, `..`, and redirects (`Policy::none()`); restrict CloudCode fallback/header-strip retries to read-only metadata RPCs. |
| **S4** | Byte-for-byte pass-through & zero compression | Pin `reqwest` with `default-features = false, features = ["rustls", "stream", "json", "http2"]` — **never** enable `gzip`/`brotli`/`deflate`/`zstd`. Forward request bodies untouched (preserving Gemini implicit caching) and tee `usageMetadata` without mutating stream bytes. |
| **S5** | UI zero-`innerHTML` & strict CSP | Use safe DOM APIs only (`createElement`, `textContent`, `replaceChildren`); no `innerHTML`/`eval`/`localStorage`/external tags; `fetch(` allowed only in `ui/src/transport.ts` to same-origin `/_api/` with `X-Sidecar-Token`. Keep `ui/dist/index.html` `< 512,000` bytes; place UI tests in `ui/tests/` (outside `ui/src/`). |
| **S6** | XDG + Windows path precedence & fail-closed I/O | Config: `--config` → `AGY_COMPANION_CONFIG` → `$XDG_CONFIG_HOME/agy-companion/config.json` → `~/.config/agy-companion/config.json` (Windows: `%APPDATA%`), with sibling `auth/` (`auth/gemini.json`). Usage dir: `config.usage_dir` → `$XDG_DATA_HOME/agy-companion/usage` → `~/.local/share/agy-companion/usage` (Windows: `%LOCALAPPDATA%`). Reject `..`, NUL, oversized (`> 1 MiB` config / `> 64 KiB` auth), and symlink/FIFO/non-regular files via `fs_safe` post-open `fstat` + `(dev, ino)` checks. |
| **S7** | Minimal HTTP surface & upfront body limits | Permit `GET`/`POST` only (`405` otherwise); reject malformed/oversized `Content-Length > 64 MiB` upfront with `400`/`413` JSON plus streaming `64 MiB` cap; emit `X-Content-Type-Options: nosniff` and `Cache-Control: no-store`. |
| **S8** | Launcher integrity & packaging allow-list | `run-companion.sh` scrubs inherited proxy envs, checks version-matched cache before `$PATH`, uses physical `cd -P --`, HTTPS + SHA-256 verification, and tarball traversal checks. npm shim uses `execFileSync` + 5-target allow-list and public registry `publishConfig`. Keep all 4 manifests version-synced. |

**UI Browser Support Policy:** The Aux Panel UI targets the modern Chromium/Electron webview shipped with Antigravity App and Baseline Newly Available web platform features. Newer CSS and DOM APIs are used strictly as progressive enhancements with zero runtime polyfills and zero external CDN dependencies (per **S5**).

## Workflows & Runbooks

- **Git, PR & Auto-Merge:** Follow [`.agents/rules/workflow.md`](.agents/rules/workflow.md) (focused branch → `./scripts/agent_check.sh` → Conventional Commit with zero AI attribution → PR via `.github/PULL_REQUEST_TEMPLATE.md` → squash auto-merge once green → fast-forward `main`).
- **Local Antigravity App Dev & Live E2E:** Follow [`.agents/skills/local-dev/SKILL.md`](.agents/skills/local-dev/SKILL.md) (`./scripts/dev-install.sh`, Sidecar Web UI `sidecar://agy-companion/proxy/`, `AGY_COMPANION_UI_HTML` reload, stale-PID takeover, `proxy.log`, and isolated `agy` CLI E2E).
- **Testing & Anti-Slop:** Follow [`.agents/skills/cuj-testing/SKILL.md`](.agents/skills/cuj-testing/SKILL.md) and [`docs/design/testing-strategy.md`](docs/design/testing-strategy.md).
- **Release Lifecycle & Publishing:** Follow [`.agents/skills/release/SKILL.md`](.agents/skills/release/SKILL.md) and [`docs/design/release-pipeline.md`](docs/design/release-pipeline.md) (`X.Y.Z-dev.N` daily PRs bumping patch only, `--release` / `--part minor` promotion, `CHANGELOG.md`, OIDC Trusted Publishing, tagging `vX.Y.Z`, and live verification).
- **Product, UX & Model Catalogs:** Follow [`.agents/rules/product-ux.md`](.agents/rules/product-ux.md).

## Engineering Principles

1. **Design first.** Document non-trivial intent/boundaries in `docs/design/` and trade-offs in `docs/decisions/` alongside code.
2. **Strict Red → Green → Refactor TDD.** Write a failing test at the single owning layer first, implement the minimal fix, then refactor. Never skip, delete, or weaken tests.
3. **Small, reversible changes.** Prefer focused PRs with a clear thesis; keep pure domain logic decoupled from I/O and adapters.
4. **No secrets or local junk.** Never commit `.env`, credentials, real `AIza...` keys, `.DS_Store`, or `.antigravitycli/`/`.remember/`.
5. **Parallel agent hygiene.** Give concurrent authoring agents isolated git worktrees and disjoint file ownership.

## When to Stop and Ask the Human

- The task requires changing a core architectural decision, license, repo visibility, or external public contract.
- Adding a new paid/sensitive external service, credential flow, or unapproved major dependency.
- A test cannot pass without weakening assertions, or requirements are genuinely ambiguous across materially different designs.
