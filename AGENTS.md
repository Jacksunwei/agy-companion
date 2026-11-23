# AGENTS.md: How Coding Agents Work in This Repo

This repository uses **Antigravity customization conventions** as the default agent-driven setup (`AGENTS.md` + `.agents/rules/` + `.agents/skills/`). Do not create redundant tool-specific root files such as `GEMINI.md`.

## Repo Layout

| Path | Role | Rule |
| --- | --- | --- |
| `AGENTS.md` | Canonical engineering contract | Single root entrypoint for coding agents (never add `GEMINI.md`). |
| `.agents/rules/` | Always-on & contextual workspace rules | Antigravity-native rules (`workflow.md` with `trigger: always_on`). |
| `.agents/skills/` | On-demand project skills | Project runbooks (`local-dev/SKILL.md` for local build/link/reload/E2E workflow). |
| `Cargo.toml`, `Cargo.lock` | Rust workspace root | `edition = "2024"`, single version source of truth (`0.1.0`), Apache-2.0. |
| `crates/` | 5-crate Rust workspace | `companion-core` (config, XDG/Windows paths, redaction, usage seam), `companion-providers` (`ProviderAdapter` + raw `reqwest` `GeminiProvider`), `companion-proxy` (loopback Axum proxy + SSE usage tee), `companion-ui-mcp` (`rmcp` stdio MCP App server), `companion-cli` (unified `agy-companion` binary). |
| `ui/` | Build-time MCP App UI frontend | Vite 8 + TypeScript + `@modelcontextprotocol/ext-apps 2` + `vite-plugin-singlefile 2.3` (`ui/.npmrc` pins `https://registry.npmjs.org/`), compiling to committed `ui/dist/index.html` (`< 512,000` bytes, embedded via `include_str!`). |
| `plugin/` | Distributable Antigravity plugin bundle | `plugin.json`, `mcp_config.json`, `sidecars/proxy/sidecar.json`, `scripts/run-companion.sh`, `skills/companion/SKILL.md`, `README.md`; validated with `agy plugin validate --strict ./plugin`. |
| `packages/` | Cross-platform distribution shims | `packages/npm/agy-companion/` (`agy-companion` & `agyc`, 5 `@agy-companion/<os>-<arch>` `optionalDependencies`, `publishConfig` -> public npm), `packages/npm/generate-platform-pkgs.mjs`, and `packages/python/` (`maturin` `bindings = "bin"`). |
| `scripts/agent_check.sh` | Fast 7-gate local verification script | Run before every commit and PR. |
| `scripts/dev-install.sh` | Local dev build & plugin symlink helper | Supports `--no-build`, `--release`, `--dry-run`, `--uninstall`, `--plugins-dir`; never modifies user Antigravity settings. |
| `.github/PULL_REQUEST_TEMPLATE.md` | PR template | Every PR includes summary + pasted local verification evidence. |
| `.github/workflows/ci.yml` | CI workflow (currently disabled at repo level) | Kept in sync with `scripts/agent_check.sh` so CI works immediately when enabled. |
| `docs/` | Design docs (`docs/design/`) and ADRs (`docs/decisions/`) | Update docs alongside behavior or architecture changes. |

## Verification Gates

> **Local-only verification policy (for now):** `.github/workflows/ci.yml` is checked in and ready, but GitHub Actions is disabled at the repository level while this repo is private without Actions quota. Run all verification gates locally before opening or merging a PR.

```bash
# Gate 1 (Primary): Full 7-gate verification script
#   [1/7] Antigravity-native governance files (no GEMINI.md)
#   [2/7] Tracked file hygiene (incl. __pycache__/*.pyc) & Google API key secret scan (tracked, staged, commits)
#   [3/7] Zero AI attribution in commits
#   [4/7] Shell syntax (bash -n & /bin/bash -n), executable bit (-x) on scripts/*.sh + plugin/scripts/*.sh, and dev-install safety
#   [5/7] 4-way version sync (Cargo.toml, plugin.json, npm package.json + 5 optionalDeps, pyproject.toml) & JS/Python shim checks
#   [6/7] Rust workspace (fmt, clippy -D warnings, cargo test) & UI XSS hygiene / single-file bundle gate (< 512000 bytes)
#   [7/7] Plugin JSON manifests & agy plugin validate --strict ./plugin (clean skip if agy absent)
./scripts/agent_check.sh

# Targeted workspace checks (when iterating on a slice)
cargo fmt --all -- --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace

# UI single-file bundle rebuild & determinism check (ui/.npmrc pins https://registry.npmjs.org/)
npm --prefix ui ci && npm --prefix ui run typecheck && npm --prefix ui run build
git diff --exit-code -- ui/dist/index.html

# Plugin & packaging shim checks
agy plugin validate --strict ./plugin
node --check packages/npm/agy-companion/bin/agy-companion.js
node --check packages/npm/generate-platform-pkgs.mjs
```

Climb gates from fastest to heaviest during iteration, and always run `./scripts/agent_check.sh` cleanly before committing or opening a PR.

**Verification discipline:** State the expected positive success signal before running a check, then verify it in real output before declaring success or merging.

## Architecture & Security Invariants

| ID | Invariant | Enforced Contract |
| --- | --- | --- |
| **S1** | Loopback-only proxy | `companion-proxy` binds exclusively to `127.0.0.1` / `localhost` (`validate_listen_host` rejects `0.0.0.0`, `::`, and non-loopback hosts; CLI exposes no `--host` flag). |
| **S2** | Secret redaction, atomic persistence & zero leaks | `get_settings` and `open_settings` return `RedactedConfig` only (`api_key_configured` + masked `api_key_hint`, never plaintext). `Config::save_secure` writes atomically (temp file + `sync_all()` + `fs::rename`; Unix mode `0600` / parent dir `0700`; Windows user-profile ACLs + bounded 3-attempt `25ms` → `50ms` retry on transient `ErrorKind::PermissionDenied`). `companion-cli` caps `rmcp` tracing at `INFO` so raw JSON-RPC `save_settings` payloads are never logged. Zero real `AIza...` keys in repo files, commits, logs, or JSONL usage files (`agent_check.sh` scans for `AIza[0-9A-Za-z_-]{35}`). |
| **S3** | Upstream auth & SSRF defense | Authenticate upstream via `x-goog-api-key` header (strip `?key=` query params, preserve `?alt=sse`). Remote `base_url` MUST use `https://`; `http://` is permitted ONLY for literal `127.0.0.1` or `localhost`. Reject embedded credentials, backslashes, `..` traversal, and HTTP redirects (`Policy::none()`). |
| **S4** | Raw byte-for-byte pass-through & `reqwest` zero-compression | `Cargo.toml` pins `reqwest = { version = "0.13", default-features = false, features = ["rustls", "stream", "json", "http2"] }` with **zero compression features** — NEVER enable `gzip`, `brotli`, `deflate`, or `zstd`. Request bodies forward untouched (preserving Gemini implicit context caching, 900s timeout) and uncompressed SSE/JSON streams tee `usageMetadata` without buffering or mutating payload bytes. |
| **S5** | UI zero-`innerHTML` & strict CSP discipline | `ui/src/` uses ONLY safe DOM APIs (`document.createElement`, `textContent`, `replaceChildren`) — zero `innerHTML`, `outerHTML`, `insertAdjacentHTML`, `document.write`, `dangerouslySetInnerHTML`, `eval(`, `new Function`, `localStorage`, `sessionStorage`, `document.cookie`, `alert(`, `confirm(`, `prompt(`, `XMLHttpRequest`, or `fetch(`. Register MCP App handlers (`ontoolinput`, `ontoolresult`, `onhostcontextchanged`) before `await app.connect()`. Committed `ui/dist/index.html` stays `< 512,000` bytes with zero external `<script src>`, `<link href>`, `<img src>`, `<iframe>`, or `@import`. |
| **S6** | XDG + Windows path precedence & traversal rejection | **Config:** `--config <path>` → `AGY_COMPANION_CONFIG` → `$XDG_CONFIG_HOME/agy-companion/config.json` *(when set, non-empty, absolute)* → platform default (macOS/Linux: `~/.config/agy-companion/config.json`, no `~/Library/Application Support/` drift; Windows: `%APPDATA%\agy-companion\config.json` via `dirs::config_dir()`, fallback `%USERPROFILE%\.config\agy-companion\config.json`). **Usage dir:** `config.usage_dir` → `$XDG_DATA_HOME/agy-companion/usage` *(when set, non-empty, absolute)* → platform default (macOS/Linux: `~/.local/share/agy-companion/usage`; Windows: `%LOCALAPPDATA%\agy-companion\usage` via `dirs::data_local_dir()`, fallback `%USERPROFILE%\.local\share\agy-companion\usage`). Reject symlinks for config files and reject `..` traversal or NUL bytes fail-closed; unset, empty/whitespace, or benign relative XDG values fall back to platform defaults. |
| **S7** | Minimal HTTP surface & security headers | `companion-proxy` permits only `GET` and `POST` (`405` otherwise), caps `POST /v1beta/models/{*rest}` at 64 MiB (`DefaultBodyLimit`), and sets `X-Content-Type-Options: nosniff` + `Cache-Control: no-store` on `/health` and `/api/stats`. |
| **S8** | Launcher integrity & packaging allow-list | `plugin/scripts/run-companion.sh` verifies SHA-256 (`sha256sum`/`shasum -a 256`) and archive traversal safety before executing downloaded binaries; `packages/npm/agy-companion/bin/agy-companion.js` uses `execFileSync` with a hardcoded `(platform, arch)` allow-list; npm manifests pin `"publishConfig": { "registry": "https://registry.npmjs.org/", "access": "public" }`; all 4 manifests (`Cargo.toml`, `plugin/plugin.json`, `packages/npm/agy-companion/package.json`, `packages/python/pyproject.toml`) stay version-synced. |

## Local Antigravity App Development & Testing Workflow

See [`.agents/skills/local-dev/SKILL.md`](.agents/skills/local-dev/SKILL.md) for the complete step-by-step runbook. Summary:

- **Build & link plugin:** Run `./scripts/dev-install.sh` (builds `ui/` + `cargo build -p companion-cli` and symlinks `./plugin` into `~/.gemini/config/plugins/agy-companion` without modifying user Antigravity settings). `plugin/scripts/run-companion.sh` resolves symlinks on `$0` and automatically runs `<repo_root>/target/{debug,release}/agy-companion` so local dev never downloads from GitHub Releases.
- **Instant UI iteration (zero Rust rebuild):** From the repo root, export `AGY_COMPANION_UI_HTML="$(pwd)/ui/dist/index.html"`, run `npm --prefix ui run build`, and close/reopen the **AGY Companion** aux pane.
- **Live MCP reload (no App restart):** Restart **`agy-companion_companion`** via **Settings > MCP Servers > Restart** or invoke the `RefreshMcpServers` RPC with `{"serverName": "agy-companion_companion"}`.
- **Logs & live E2E:** Inspect sidecar logs at `~/.gemini/antigravity/sidecar_data/agy-companion/proxy/logs/`, run standalone `cargo run -p companion-cli -- serve`, or run isolated `agy` CLI E2E with `GOOGLE_GEMINI_BASE_URL="http://127.0.0.1:4141"`, `agy-companion doctor`, `agy-companion models --json`, and isolated `XDG_CONFIG_HOME` / `XDG_DATA_HOME`.

## Git, PR & Auto-Merge Rule

This repository uses a standing **branch → PR → auto-merge** workflow for all changes, keeping PRs small, self-contained, and merged incrementally as slices complete (never accumulating into one giant PR):

1. **Never push directly to `main`.** Every task lives on its own branch named `<type>/<short-description>` (e.g., `feat/model-router`, `fix/retry-backoff`, `chore/packaging-shims-and-m1-gates`).
2. **Conventional Commits, zero AI attribution.**
   - Format: `<type>(<scope>): <description>` (imperative mood, lowercase, no trailing period).
   - Subject must answer *why* the change exists, not merely restate the diff.
   - **Never** include `Co-Authored-By:` lines naming an assistant, "Generated with ...", or any AI branding.
3. **Verify locally before pushing.** Run `./scripts/agent_check.sh` and all relevant tests; fix failures before opening the PR.
4. **Open a PR using `.github/PULL_REQUEST_TEMPLATE.md`.** Include concrete summary bullets and pasted verification evidence.
5. **Auto-merge once verified.**
   - If CI checks are configured on the repo, wait for them (`gh pr checks --watch`).
   - Enable auto-merge (`gh pr merge --auto --squash --delete-branch`), or when server-side auto-merge / required branch protection is unavailable on this repo (`delete_branch_on_merge: true` is active on GitHub) and all checks are green, merge immediately via `gh pr merge --squash --delete-branch`.
6. **Keep local `main` clean.** After merge, switch back and fast-forward:
   ```bash
   git checkout main && git pull --ff-only
   ```

## Engineering Principles

1. **Design first.** For non-trivial changes, document intent and boundaries in `docs/design/` and significant trade-offs in `docs/decisions/` alongside the code.
2. **Strict Red → Green → Refactor TDD.** Write a failing test reproducing the bug or specifying the new behavior first, implement the minimal fix to go green, then refactor. Never skip, delete, or weaken tests to get green.
3. **Small, reversible changes.** Prefer focused PRs with a clear thesis over sprawling multi-concern diffs. Keep pure domain logic decoupled from I/O and external adapters.
4. **No secrets or local junk.** Never commit `.env`, credentials, real API keys, OS metadata (`.DS_Store`), or agent runtime directories (`.antigravitycli/`, `.remember/`).
5. **Parallel agent hygiene.** When running multiple agents concurrently, give each authoring agent an isolated git worktree and disjoint file ownership.

## When to Stop and Ask the Human

- The task requires changing a core architectural decision, license, or external public contract.
- Adding a new paid/sensitive external service, credential flow, or unapproved major dependency.
- A test cannot pass without weakening assertions, or requirements are genuinely ambiguous across materially different designs.
