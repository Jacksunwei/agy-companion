# AGENTS.md: How Coding Agents Work in This Repo

This repository uses **Antigravity customization conventions** as the default agent-driven setup (`AGENTS.md` + `.agents/rules/` + `.agents/skills/`). Do not create redundant tool-specific root files such as `GEMINI.md`.

## Repo Layout

| Path | Role | Rule |
| --- | --- | --- |
| `AGENTS.md` | Canonical engineering contract | Single root entrypoint for coding agents (never add `GEMINI.md`). |
| `README.md` | Root project documentation | Overview, architecture diagram, quickstarts (`plugin`, `agy` CLI, `npx`/`uvx`/`cargo`), XDG/Windows paths, S1–S8 invariants, roadmap. |
| `.agents/rules/` | Always-on & contextual workspace rules | Antigravity-native rules (`workflow.md` with `trigger: always_on`). |
| `.agents/skills/` | On-demand project skills | Project runbooks (`local-dev/SKILL.md` for local build/link/reload/E2E workflow). |
| `Cargo.toml`, `Cargo.lock` | Rust workspace root | `edition = "2024"`, single version source of truth (`0.1.0`), Apache-2.0. |
| `crates/` | 5-crate Rust workspace (`#![forbid(unsafe_code)]`) | `companion-core` (config, XDG/Windows paths, redaction, `PROXY_PLACEHOLDER_API_KEY`, usage seam, zero-`eval` `integration` `env.sh`/RC hooks), `companion-providers` (`ProviderAdapter` + raw `reqwest` `GeminiProvider`), `companion-proxy` (loopback Axum proxy, CloudCode per-account 128-bit SipHash LRU, Day-0 untouched catalog, `IdleTracker`, SSE usage tee), `companion-ui-mcp` (`rmcp` stdio MCP App server with 5 tools incl. `set_launcher_integration`), `companion-cli` (unified `agy-companion` binary: `serve`, `ui-mcp`, `doctor`, `models`, `launch`, `hook-env`, `integration`, `install`, `uninstall`). |
| `ui/` | Build-time MCP App UI frontend | Vite 8 + TypeScript + `@modelcontextprotocol/ext-apps 2` + `vite-plugin-singlefile 2.3` (`ui/.npmrc` pins `https://registry.npmjs.org/`), compiling to committed `ui/dist/index.html` (`283,627` bytes, `< 512,000` bytes / 500 KiB, embedded via `include_str!`). |
| `plugin/` | Distributable Antigravity plugin bundle | `plugin.json`, `assets/logo.svg`, `mcp_config.json`, `sidecars/proxy/sidecar.json`, `scripts/run-companion.sh`, `skills/companion/SKILL.md`, `README.md`; validated with `agy plugin validate --strict ./plugin` and embedded byte-for-byte into `companion-cli` via `include_str!`. |
| `packages/` | Cross-platform distribution shims | `packages/npm/agy-companion/` (`agy-companion` & `agyc`, 5 `@agy-companion/<os>-<arch>` `optionalDependencies`, `publishConfig` -> public npm), `packages/npm/generate-platform-pkgs.mjs`, and `packages/python/` (`maturin` `bindings = "bin"`). |
| `e2e/` | Pure Python 3 stdlib opaque-box E2E suite | `15` files (`e2e/run_e2e.py`, `e2e/README.md`, `test_tier{1..4}_*.py`, `test_tier{1..4}_rollout.py`); 4-tier suite (**328** E2E tests: `T1=139, T2=145, T3=31, T4=13`) + **25** `--self-test` harness checks; always run with `python3 -B` so no `__pycache__` is created. |
| `scripts/agent_check.sh` | Fast 7-gate local verification script | Run before every commit and PR. |
| `scripts/dev-install.sh` | Local dev build & plugin symlink helper | Supports `--no-build`, `--release`, `--dry-run`, `--uninstall`, `--plugins-dir <dir>`, `-h/--help`; physical `cd -P --` symlink safety; never modifies user Antigravity settings. |
| `.github/PULL_REQUEST_TEMPLATE.md` | PR template | Every PR includes summary + pasted local verification evidence. |
| `.github/workflows/ci.yml` | CI workflow (currently disabled at repo level) | Kept in sync with `scripts/agent_check.sh` so CI works immediately when enabled. |
| `docs/` | Design specs (`docs/design/architecture.md`, `docs/design/dock-launcher-and-install.md`, etc.; index at `docs/README.md`) and reserved ADRs (`docs/decisions/`) | Update docs alongside behavior or architecture changes. |

## Verification Gates

> **Local-only verification policy (for now):** `.github/workflows/ci.yml` is checked in and ready, but GitHub Actions is disabled at the repository level while this repo is private without Actions quota. Run all verification gates locally before opening or merging a PR.

```bash
# Gate 1 (Primary): Full 7-gate verification script
#   [1/7] Antigravity-native governance files (no GEMINI.md)
#   [2/7] Tracked file hygiene (incl. __pycache__/*.pyc) & Google API key secret scan (tracked, staged, commits)
#   [3/7] Zero AI attribution in commits
#   [4/7] Shell syntax (bash -n & /bin/bash -n), executable bit (-x) on scripts/*.sh + plugin/scripts/*.sh, and dev-install safety
#   [5/7] 4-way version sync (Cargo.toml, plugin.json, npm package.json + 5 optionalDeps, pyproject.toml), generate-platform-pkgs.mjs dry-run, JS/Python shim checks, temp-dir e2e/*.py py_compile (zero __pycache__) & e2e/run_e2e.py --self-test (25 self-tests)
#   [6/7] Rust workspace (fmt, clippy -D warnings, cargo test — 265 tests) & unified full S5 sink + whitespace-aware external-tag scanning across ui/src/ & ui/dist/index.html (< 512,000 bytes / 500 KiB, currently 283,627 bytes)
#   [7/7] Plugin JSON manifests, bundled logo asset validation (canonical relative path, non-symlink regular file, strict static SVG allowlist) & agy plugin validate --strict ./plugin (clean skip if agy absent)
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

# Opaque-box E2E suite (pure Python 3 stdlib; -B prevents __pycache__)
python3 -B e2e/run_e2e.py --self-test -v
cargo build -p companion-cli
python3 -B e2e/run_e2e.py --tier all --binary target/debug/agy-companion --repo-root . -v
```

Climb gates from fastest to heaviest during iteration, and always run `./scripts/agent_check.sh` cleanly before committing or opening a PR.

**Verification discipline:** State the expected positive success signal before running a check, then verify it in real output before declaring success or merging.

## Architecture & Security Invariants

All 5 Rust crates (`crates/*/src/lib.rs`), `crates/companion-cli/src/main.rs`, and `crates/companion-cli/tests/cli_integration.rs` enforce `#![forbid(unsafe_code)]`.

| ID | Invariant | Enforced Contract |
| --- | --- | --- |
| **S1** | Loopback-only proxy | `companion-proxy` binds exclusively to `127.0.0.1` / `localhost` (`validate_listen_host` rejects `0.0.0.0`, `::`, and non-loopback hosts; `serve_on_listener` re-verifies `local_addr.ip() == Ipv4Addr::LOCALHOST`; CLI exposes no `--host` flag). |
| **S2** | Secret redaction, atomic persistence & zero leaks | `get_settings` and `open_settings` return `RedactedConfig` only (`api_key_configured` + masked `api_key_hint`, never plaintext). `PROXY_PLACEHOLDER_API_KEY` (`agy-companion-local`) is quarantined across all crates so it never leaks upstream or false-passes `doctor`. `Config::validate()` enforces printable-ASCII header safety on `gemini.api_key` (full `doctor`/`serve`/`GeminiProvider` parity). `companion-ui-mcp` serializes concurrent `save_settings` and `set_launcher_integration` writes with async mutexes (released before `/health` probes). `Config::save_secure`, `env.sh`, `proxy.pid`, and rotating `5 MiB` `proxy.log` write atomically / securely (Unix mode `0600` / parent dir `0700`; Windows user-profile ACLs + bounded 3-attempt `25ms` → `50ms` retry on transient `ErrorKind::PermissionDenied`). `JsonlUsageSink` and `run_serve` reject symlink/non-dir `usage_dir` and symlink/FIFO `YYYY-MM.jsonl` before `open(2)` and verify post-open `fstat` `is_file()` + Unix `(dev, ino)` before `chmod 0600` or `write_all`. `companion-cli` caps `rmcp` tracing at `INFO` so raw JSON-RPC `save_settings` payloads are never logged. Zero real `AIza...` keys in repo files, commits, logs, or JSONL usage files (`agent_check.sh` scans for `AIza[0-9A-Za-z_-]{35}`). |
| **S3** | Upstream auth & SSRF defense | Authenticate upstream via `x-goog-api-key` header (strip `?key=` query params, preserve `?alt=sse`). Remote `base_url` MUST use `https://`; `http://` is permitted ONLY for literal `127.0.0.1` or `localhost`. Reject embedded credentials, backslashes, `..` traversal, and HTTP redirects (`Policy::none()`). CloudCode uses conditional `.no_proxy()` (loopback only) and restricts fallback / Enterprise `403` `x-goog-user-project` strip retries strictly to read-only metadata RPCs (`fetchAvailableModels`, `loadCodeAssist`). |
| **S4** | Raw byte-for-byte pass-through & `reqwest` zero-compression | `Cargo.toml` pins `reqwest = { version = "0.13", default-features = false, features = ["rustls", "stream", "json", "http2"] }` with **zero compression features** — NEVER enable `gzip`, `brotli`, `deflate`, or `zstd`. Request bodies forward untouched (preserving Gemini implicit context caching, 900s timeout / `>=600s` CloudCode read timeout) and uncompressed SSE/JSON streams tee `usageMetadata` without buffering or mutating payload bytes; CloudCode preserves upstream `Content-Encoding` on raw/Day-0 fallback. |
| **S5** | UI zero-`innerHTML` & strict CSP discipline | `ui/src/` uses ONLY safe DOM APIs (`document.createElement`, `textContent`, `replaceChildren`) — zero `innerHTML`, `outerHTML`, `insertAdjacentHTML`, `document.write`, `dangerouslySetInnerHTML`, `eval(`, `new Function`, `localStorage`, `sessionStorage`, `document.cookie`, `alert(`, `confirm(`, `prompt(`, `XMLHttpRequest`, or `fetch(`. Register MCP App handlers (`ontoolinput`, `ontoolresult`, `onhostcontextchanged`) before `await app.connect()`. `ui/src/main.ts` enforces monotonic `snapshotEpoch` stale-refresh guards, `refreshBtn` write lockout, dynamic `listen_host` rendering, and `portChangedSinceSave` reset on reachable refresh. Committed `ui/dist/index.html` stays `< 512,000` bytes (500 KiB; currently `283,627` bytes) and `agent_check.sh` scans both `ui/src/` and `ui/dist/index.html` with the unified full S5 sink regex plus whitespace-aware external `<script src>`, `<link href>`, `<img src>`, `<iframe>`, and `@import` rejection. `companion-ui-mcp`'s `load_ui_html_override` (`AGY_COMPANION_UI_HTML`) checks `fs::metadata` (`is_file()` and `<= MAX_UI_HTML_BYTES` = 10 MiB) before `File::open()` to prevent FIFO hangs. |
| **S6** | XDG + Windows path precedence & fail-closed file validation | **Config:** `--config <path>` → `AGY_COMPANION_CONFIG` → `$XDG_CONFIG_HOME/agy-companion/config.json` *(when set, non-empty, absolute)* → platform default (macOS/Linux: `~/.config/agy-companion/config.json`, no `~/Library/Application Support/` drift; Windows: `%APPDATA%\agy-companion\config.json` via `dirs::config_dir()`, fallback `%USERPROFILE%\.config\agy-companion\config.json`). **Usage dir:** `config.usage_dir` → `$XDG_DATA_HOME/agy-companion/usage` *(when set, non-empty, absolute)* → platform default (macOS/Linux: `~/.local/share/agy-companion/usage`; Windows: `%LOCALAPPDATA%\agy-companion\usage` via `dirs::data_local_dir()`, fallback `%USERPROFILE%\.local\share\agy-companion\usage`). Enforce `MAX_CONFIG_FILE_BYTES` (`1 MiB`), reject `..` traversal and NUL bytes fail-closed, reject symlink/FIFO/non-regular config files with post-open `fstat` `is_file()` + Unix `(dev, ino)` checks across `Config::load_or_default`, `doctor`, and proxy startup/hot-reload (`FileStamp::Rejected(Symlink \| NotRegularFile)` retains previous active config without blocking `RwLock`), and reject symlink/non-dir `usage_dir` or symlink/FIFO `YYYY-MM.jsonl`; unset, empty/whitespace, or benign relative XDG values fall back to platform defaults. |
| **S7** | Minimal HTTP surface, upfront body limit & security headers | `companion-proxy` permits only `GET` and `POST` (`405` otherwise), enforces outermost `enforce_content_length_limit` middleware rejecting malformed/conflicting `Content-Length` with `400` and oversized/`u64`-overflow `Content-Length > 64 MiB` upfront with `413` JSON before reading body bytes alongside `DefaultBodyLimit::max(64 MiB)` on `POST /v1beta/models/{*rest}`, and sets `X-Content-Type-Options: nosniff` + `Cache-Control: no-store` on `/health`, `/api/stats`, and JSON error responses. |
| **S8** | Launcher integrity & packaging allow-list | `plugin/scripts/run-companion.sh` scrubs inherited proxy env vars via `exec_clean`, honors `XDG_DATA_HOME`, checks version-matched cache BEFORE unversioned `$PATH`, uses physical `cd -P --`, HTTPS-only downloads, SHA-256 verification (`sha256sum`/`shasum -a 256` with `< "${checksums_file}"` redirection), `tar -f - < "${tarball}"` stdin traversal checks, and `INT`/`TERM`/`HUP`/`EXIT` cleanup traps; `scripts/dev-install.sh` uses physical `cd -P --` symlink safety; `packages/npm/agy-companion/bin/agy-companion.js` uses `execFileSync` with a hardcoded 5-target `(platform, arch)` allow-list; npm manifests pin `"publishConfig": { "registry": "https://registry.npmjs.org/", "access": "public" }`; `packages/python/python/agy_companion/__init__.py` `find_binary()` rejects `..`/NUL and normalizes `AGY_COMPANION_BIN`; all 4 manifests (`Cargo.toml`, `plugin/plugin.json`, `packages/npm/agy-companion/package.json` + 5 `optionalDependencies`, `packages/python/pyproject.toml`) stay version-synced. |

## Local Antigravity App Development & Testing Workflow

See [`.agents/skills/local-dev/SKILL.md`](.agents/skills/local-dev/SKILL.md) for the complete step-by-step runbook. Summary:

- **Build & link plugin:** Run `./scripts/dev-install.sh` (builds `ui/` + `cargo build -p companion-cli` and symlinks `./plugin` into `~/.gemini/config/plugins/agy-companion` without modifying user Antigravity settings). `plugin/scripts/run-companion.sh` resolves symlinks on `$0` and automatically runs `<repo_root>/target/{debug,release}/agy-companion` so local dev never downloads from GitHub Releases.
- **Instant UI iteration (zero Rust rebuild):** From the repo root, export `AGY_COMPANION_UI_HTML="$(pwd)/ui/dist/index.html"`, run `npm --prefix ui run build`, and close/reopen the **Antigravity Companion** aux pane.
- **Live MCP reload (no App restart):** Restart **`agy-companion_companion`** via **Settings > MCP Servers > Restart** or invoke the `RefreshMcpServers` RPC with `{"serverName": "agy-companion_companion"}`.
- **Logs & live E2E:** Inspect sidecar logs at `~/.gemini/antigravity/sidecar_data/agy-companion/proxy/logs/`, run standalone `cargo run -p companion-cli -- serve`, or run isolated `agy` CLI E2E with `GOOGLE_GEMINI_BASE_URL="http://127.0.0.1:4141"`, `agy-companion doctor`, `agy-companion models --json`, and isolated `XDG_CONFIG_HOME` / `XDG_DATA_HOME`.
- **Deterministic opaque-box E2E:** See [`e2e/README.md`](e2e/README.md) and run `python3 -B e2e/run_e2e.py --tier all --binary target/debug/agy-companion --repo-root . -v` (`328` tests across Tiers 1–4; `--self-test -v` runs `25` harness self-tests).

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
