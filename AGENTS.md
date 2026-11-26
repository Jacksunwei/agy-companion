# AGENTS.md: How Coding Agents Work in This Repo

This repository uses **Antigravity customization conventions** as the default agent-driven setup (`AGENTS.md` + `.agents/rules/` + `.agents/skills/`). Do not create redundant tool-specific root files such as `GEMINI.md`.

## Repo Layout

| Path | Role | Rule |
| --- | --- | --- |
| `AGENTS.md` | Canonical engineering contract | Single root entrypoint for coding agents (never add `GEMINI.md`). |
| `README.md` | Root project documentation | Overview, architecture diagram, quickstarts (`plugin`, `agy` CLI, `npx`/`uvx`/`cargo`), XDG/Windows paths, S1–S8 invariants, roadmap. |
| `.agents/rules/` | Always-on & contextual workspace rules | Antigravity-native rules (`workflow.md` with `trigger: always_on`). |
| `.agents/skills/` | On-demand project skills | Project runbooks (`local-dev/SKILL.md` for local build/link/reload/E2E workflow; `cuj-testing/SKILL.md` for CUJ-first testing, Single-Owner layer placement, and anti-slop reviews). |
| `Cargo.toml`, `Cargo.lock` | Rust workspace root | `edition = "2024"`, single version source of truth (`0.0.2`), Apache-2.0. |
| `crates/` | 5-crate Rust workspace (`#![forbid(unsafe_code)]`) | `companion-core` (nested v1 config, `fs_safe` TOCTOU/atomic-write guards, `protocol` wire DTOs, `validation`, `models` Gemini 3.8+/4+ catalog, redaction, `PROXY_PLACEHOLDER_API_KEY`, usage seam + optional `sqlite` feature + `usage/query.rs` range rollups, modular zero-`eval` `integration/{auto_enable,disable,enable,fs_inspect,lifecycle,lock,markers,mod,strip,templates,tests}` `env.sh`/RC hooks), `companion-providers` (`ProviderAdapter`, raw `reqwest` `gemini/` `GeminiProvider`, shared pooled `probe_client` `LoopbackProxyClient`), `companion-proxy` (loopback Axum proxy, `cloudcode/` per-account 128-bit SipHash LRU & Day-0 untouched catalog, `routes/` modular handlers, non-blocking async `hot_reload`, `ActivityTracker`, `ProxyStateOptions`, `rss.rs` process RAM telemetry, SSE usage tee), `companion-ui-mcp` (modular `rmcp` stdio MCP App server `{types, ui_resource, tools, server}` with `UiServerOptions`, zero direct `reqwest`, `OnceLock` SVG caching, parallel `tokio::join!` snapshots, and 6 tools incl. strict nested `save_settings` w/ `deny_unknown_fields`, `supported_models`, hardened `test_connection`, config-before-mutate `set_launcher_integration`, and `get_usage`), `companion-cli` (unified `agy-companion` binary with `serve.rs`, shared `process.rs`, `daemon::wait_for_daemon_healthy`, modular `install/{mod,uninstall,bundle,safety,proxy,windows,format,tests}.rs` deriving Windows manifests from embedded `plugin/*.json`, `--config` across all config-reading subcommands incl. `install`/`uninstall`, and unified integration test crate `tests/cli_integration.rs` + 6 submodules `tests/cli_integration/{cli_idle_shutdown,daemon_integration,install_lifecycle,install_symlink_safety,serve_doctor,ui_mcp}.rs` + `tests/common/mod.rs`: `serve`, `ui-mcp`, `doctor`, `models`, `launch`, `hook-env`, `integration`, `install`, `uninstall`). |
| `ui/` | Build-time MCP App UI frontend | Vite 8 + TypeScript + `@modelcontextprotocol/ext-apps 2` + `vite-plugin-singlefile 2.3` (`ui/.npmrc` pins `https://registry.npmjs.org/`), modular `ui/src/{types,dom,pricing,state,banners,footer,monitor,usage,settings,main}.ts` + `style.css`, zero-dep Node 24 behavioral tests (`ui/tests/*.test.ts` + Rust↔TS golden wire fixtures in `ui/tests/fixtures/*.json`, run via `npm --prefix ui test` in `< 1.0s`), compiling to committed `ui/dist/index.html` (`< 512,000` bytes / 500 KiB, embedded via `include_str!`). |
| `plugin/` | Distributable Antigravity plugin bundle | `plugin.json`, `assets/logo.svg`, `mcp_config.json`, `sidecars/proxy/sidecar.json`, `scripts/run-companion.sh`, `skills/companion/SKILL.md`, `README.md`; validated with `agy plugin validate --strict ./plugin` and embedded byte-for-byte into `companion-cli` via `include_str!`. |
| `packages/` | Cross-platform distribution shims | `packages/npm/agy-companion/` (`agy-companion` & `agyc`, 5 `@agy-companion/<os>-<arch>` `optionalDependencies`, `publishConfig` -> public npm), `packages/npm/generate-platform-pkgs.mjs`, and `packages/python/` (`maturin` `bindings = "bin"`). |
| `e2e/` | Pure Python 3 stdlib opaque-box E2E suite | Intent-named parallel opaque-box binary CUJ, security boundary, runtime shim, and headless Chrome CDP UI smoke suite (`e2e/run_e2e.py --suite cuj\|boundary\|shims\|ui-smoke\|all --workers 4`, `e2e/README.md`, `docs/design/testing-strategy.md`); never grep repo source/docs inside `e2e/`; always run with `python3 -B` so no `__pycache__` is created. |
| `scripts/check.py`, `scripts/agent_check.sh` | Unified cross-platform verification runner & POSIX shim | Pure Python 3 stdlib `scripts/check.py` (`--profile repo\|os\|all`, `--verify-ui-build`, `--with-e2e`, paired with strict Expat SVG validator `scripts/check_plugin_logo.py` + `scripts/test_check_plugin_logo.py`); `scripts/agent_check.sh` is a thin POSIX shim executing `python3 -B scripts/check.py --profile all "$@"`. Run before every commit and PR. |
| `scripts/dev-install.sh` | Local dev build & plugin symlink helper | Supports `--no-build`, `--release`, `--dry-run`, `--uninstall`, `--plugins-dir <dir>`, `-h/--help`; physical `cd -P --` symlink safety; never modifies user Antigravity settings. |
| `.github/PULL_REQUEST_TEMPLATE.md` | PR template | Every PR includes summary + pasted local verification evidence. |
| `.github/workflows/ci.yml` | GitHub Actions CI workflow | Runs 2 non-overlapping jobs on PRs and pushes to `main`: `repo-checks` on `ubuntu-latest` (`python -B scripts/check.py --profile repo --verify-ui-build`) and `test` matrix across `[ubuntu-latest, macos-latest, windows-latest]` (`python -B scripts/check.py --profile os --with-e2e`). |
| `docs/` | Design specs (`docs/design/architecture.md`, `docs/design/dock-launcher-and-install.md`, `docs/design/testing-strategy.md`, etc.; index at `docs/README.md`) and reserved ADRs (`docs/decisions/`) | Update docs alongside behavior or architecture changes. |

## Verification Gates

> **Verification policy:** GitHub Actions CI (`.github/workflows/ci.yml`) is enabled on PRs and pushes to `main`. Run all verification gates locally before pushing, and confirm CI checks are green before merging.

```bash
# Gate 1 (Primary): Unified cross-platform verification runner (scripts/agent_check.sh -> scripts/check.py --profile all)
#   --profile repo (OS-independent; run once in CI with --verify-ui-build):
#     [repo 1/7] Antigravity-native governance files (no GEMINI.md)
#     [repo 2/7] Tracked file hygiene (incl. __pycache__/*.pyc) & Google API key secret scan (tracked, staged, commits)
#     [repo 3/7] Zero AI attribution in commits
#     [repo 4/7] Shell syntax (bash -n & /bin/bash -n), executable bit (-x) on scripts/*.sh + plugin/scripts/*.sh, and dev-install safety
#     [repo 5/7] 4-way version sync (Cargo.toml, plugin.json, npm package.json + 5 optionalDeps, pyproject.toml), generate-platform-pkgs.mjs dry-run, JS/Python shim checks, temp-dir scripts/*.py & e2e/*.py py_compile (zero __pycache__)
#     [repo 6/7] cargo fmt --check, 5 automated static hermeticity guards (Gate 6c), unified full S5 sink + whitespace-aware external-tag scanning across ui/src/ & ui/dist/index.html (< 512,000 bytes / 500 KiB), plus optional --verify-ui-build
#     [repo 7/7] Plugin JSON manifests, strict fail-closed Expat SVG logo allowlist (scripts/check_plugin_logo.py) & agy plugin validate --strict ./plugin (clean skip if agy absent)
#   --profile os (OS-dependent; run identically across ubuntu-latest, macos-latest, windows-latest in CI with --with-e2e):
#     [os 1/4] cargo clippy --workspace --all-targets -- -D warnings
#     [os 2/4] cargo test --workspace
#     [os 3/4] npm --prefix ui test (zero-dep Node 24 behavioral tests)
#     [os 4/4] python -B scripts/test_check_plugin_logo.py
#     [os 5/5] (--with-e2e) cargo build -p companion-cli + python -B e2e/run_e2e.py --suite all --workers 4
./scripts/agent_check.sh
# Or invoke directly on any OS (incl. Windows):
python3 -B scripts/check.py --profile repo --verify-ui-build
python3 -B scripts/check.py --profile os --with-e2e

# Targeted workspace checks (when iterating on a slice)
cargo fmt --all -- --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace

# UI behavioral tests, typecheck, single-file bundle rebuild & determinism check (ui/.npmrc pins https://registry.npmjs.org/)
npm --prefix ui ci && npm --prefix ui run typecheck && npm --prefix ui test && npm --prefix ui run build
git diff --exit-code -- ui/dist/index.html

# Plugin & packaging shim checks
agy plugin validate --strict ./plugin
node --check packages/npm/agy-companion/bin/agy-companion.js
node --check packages/npm/generate-platform-pkgs.mjs

# Opaque-box E2E suite (pure Python 3 stdlib; -B prevents __pycache__)
cargo build -p companion-cli
python3 -B e2e/run_e2e.py --suite all --workers 4 --binary target/debug/agy-companion --repo-root . -v
python3 -B e2e/run_e2e.py --suite ui-smoke --repo-root . -v
# Dev-only harness safety self-test (run when modifying e2e/harness.py or e2e/mcp_client.py)
python3 -B e2e/run_e2e.py --self-test -v
```

Climb gates from fastest to heaviest during iteration, and always run `./scripts/agent_check.sh` cleanly before committing or opening a PR.

**Verification discipline:** State the expected positive success signal before running a check, then verify it in real output before declaring success or merging.

## Architecture & Security Invariants

All 5 Rust crates (`crates/*/src/lib.rs`), `crates/companion-cli/src/main.rs`, and the unified integration test crate (`crates/companion-cli/tests/cli_integration.rs`) enforce `#![forbid(unsafe_code)]`.

| ID | Invariant | Enforced Contract |
| --- | --- | --- |
| **S1** | Loopback-only proxy | `companion-proxy` binds exclusively to `127.0.0.1` / `localhost` (`LoopbackHost` / `validate_listen_host` on `proxy.host` rejects `0.0.0.0`, `::`, and non-loopback hosts; `serve_on_listener` re-verifies `local_addr.ip() == Ipv4Addr::LOCALHOST`; CLI exposes no `--host` flag). |
| **S2** | Secret redaction, atomic persistence & zero leaks | `get_settings` and `open_settings` return nested `RedactedConfig` only (`providers.gemini.api_key_configured` + masked `providers.gemini.api_key_hint`, never plaintext). `PROXY_PLACEHOLDER_API_KEY` (`agy-companion-local`) is quarantined across all crates so it never leaks upstream or false-passes `doctor`. `Config::validate()` enforces printable-ASCII header safety via `ApiKey` on `providers.gemini.api_key` (full `doctor`/`serve`/`GeminiProvider` parity). `companion-ui-mcp` enforces `#[serde(deny_unknown_fields)]` on `save_settings` and serializes concurrent `save_settings` and `set_launcher_integration` writes with async mutexes (released before `/health` probes). `Config::save_secure`, `env.sh`, `proxy.pid`, and rotating `5 MiB` `proxy.log` write atomically / securely (Unix mode `0600` / parent dir `0700`; Windows user-profile ACLs + bounded 3-attempt `25ms` → `50ms` retry on transient `ErrorKind::PermissionDenied`). `SqliteUsageSink` and `run_serve` reject symlink/non-dir `usage_dir` and symlink/FIFO/non-regular `usage.db` (`-wal`/`-shm`/`-journal`) before opening, pre-create `usage.db` via `OpenOptions` + post-open `fstat` `is_file()` + Unix `(dev, ino)` + `fchmod 0600`, open SQLite without `SQLITE_OPEN_CREATE`/`SQLITE_OPEN_URI`, and enforce fd-verified `0600` on `usage.db` and its sidecars (`-wal`/`-shm`/`-journal`). `companion-cli` caps `rmcp` tracing at `INFO` so raw JSON-RPC `save_settings` payloads are never logged. Zero real `AIza...` keys in repo files, commits, logs, or SQLite usage databases (`agent_check.sh` scans for `AIza[0-9A-Za-z_-]{35}`). |
| **S3** | Upstream auth & SSRF defense | Authenticate upstream via `x-goog-api-key` header (strip `?key=` query params, preserve `?alt=sse`). Remote `base_url` MUST use `https://`; `http://` is permitted ONLY for literal `127.0.0.1` or `localhost`. Reject embedded credentials, backslashes, `..` traversal, and HTTP redirects (`Policy::none()`). CloudCode uses conditional `.no_proxy()` (loopback only) and restricts fallback / Enterprise `403` `x-goog-user-project` strip retries strictly to read-only metadata RPCs (`fetchAvailableModels`, `loadCodeAssist`, `listExperiments`). |
| **S4** | Raw byte-for-byte pass-through & `reqwest` zero-compression | `Cargo.toml` pins `reqwest = { version = "0.13", default-features = false, features = ["rustls", "stream", "json", "http2"] }` with **zero compression features** — NEVER enable `gzip`, `brotli`, `deflate`, or `zstd`. Request bodies forward untouched (preserving Gemini implicit context caching, 900s timeout / `>=600s` CloudCode read timeout) and uncompressed SSE/JSON streams tee `usageMetadata` without buffering or mutating payload bytes; CloudCode preserves upstream `Content-Encoding` on raw/Day-0 fallback. |
| **S5** | UI zero-`innerHTML` & strict CSP discipline | `ui/src/` uses ONLY safe DOM APIs (`document.createElement`, `textContent`, `replaceChildren`) — zero `innerHTML`, `outerHTML`, `insertAdjacentHTML`, `document.write`, `dangerouslySetInnerHTML`, `eval(`, `new Function`, `localStorage`, `sessionStorage`, `document.cookie`, `alert(`, `confirm(`, `prompt(`, or `XMLHttpRequest`; `fetch(` is permitted ONLY inside `ui/src/transport.ts` targeting same-origin relative `/_api/` paths authenticated with `X-Sidecar-Token` and `Content-Type: application/json` when running as an Antigravity Sidecar Web UI. Register MCP App handlers (`ontoolinput`, `ontoolresult`, `onhostcontextchanged`) before `await app.connect()`. `ui/src/state.ts` and `ui/src/main.ts` enforce monotonic `snapshotEpoch` stale-refresh guards (`shouldApplyRefresh`), in-flight write lockout, and dynamic `proxy.host` rendering, covered by zero-dep Node 24 behavioral tests in `ui/tests/*.test.ts` (outside `ui/src/`) plus Rust↔TS golden wire fixtures in `ui/tests/fixtures/*.json`. Committed `ui/dist/index.html` stays `< 512,000` bytes (500 KiB) and `agent_check.sh` scans both `ui/src/` and `ui/dist/index.html` with the unified full S5 sink regex, scoped `/_api/` fetch guard, plus whitespace-aware external `<script src>`, `<link href>`, `<img src>`, `<iframe>`, and `@import` rejection. `companion-ui-mcp`'s `load_ui_html_override` (`AGY_COMPANION_UI_HTML`) delegates to `companion_core::fs_safe::read_regular_file_utf8_bounded(..., SymlinkPolicy::Follow)` (pre-open `fs::metadata` `is_file()` + `<= MAX_UI_HTML_BYTES` = 10 MiB before `File::open()`, plus post-open `fstat` `is_file()` and Unix `(dev, ino)` TOCTOU verification) to prevent FIFO hangs and file-swap races. |
| **S6** | XDG + Windows path precedence & fail-closed file validation | **Config:** `--config <path>` → `AGY_COMPANION_CONFIG` → `$XDG_CONFIG_HOME/agy-companion/config.json` *(when set, non-empty, absolute)* → platform default (macOS/Linux: `~/.config/agy-companion/config.json`, no `~/Library/Application Support/` drift; Windows: `%APPDATA%\agy-companion\config.json` via `dirs::config_dir()`, fallback `%USERPROFILE%\.config\agy-companion\config.json`). **Usage dir:** `config.usage_dir` → `$XDG_DATA_HOME/agy-companion/usage` *(when set, non-empty, absolute)* → platform default (macOS/Linux: `~/.local/share/agy-companion/usage`; Windows: `%LOCALAPPDATA%\agy-companion\usage` via `dirs::data_local_dir()`, fallback `%USERPROFILE%\.local\share\agy-companion\usage`). Enforce `MAX_CONFIG_FILE_BYTES` (`1 MiB`), reject `..` traversal and NUL bytes fail-closed, reject symlink/FIFO/non-regular config files with post-open `fstat` `is_file()` + Unix `(dev, ino)` checks across `Config::load_or_default`, `doctor`, and proxy startup/hot-reload (`FileStamp::Rejected(Symlink \| NotRegularFile)` retains previous active config without blocking `RwLock`), and reject symlink/non-dir `usage_dir` or symlink/FIFO/non-regular `usage.db` (`-wal`/`-shm`/`-journal`); unset, empty/whitespace, or benign relative XDG values fall back to platform defaults. |
| **S7** | Minimal HTTP surface, upfront body limit & security headers | `companion-proxy` permits only `GET` and `POST` (`405` otherwise), enforces outermost `enforce_content_length_limit` middleware rejecting malformed/conflicting `Content-Length` with `400` and oversized/`u64`-overflow `Content-Length > 64 MiB` upfront with `413` JSON before reading body bytes alongside `DefaultBodyLimit::max(64 MiB)` on `POST /v1beta/models/{*rest}` and `/v1internal:fetchAvailableModels` plus streaming `read_bounded_request_body` on `/v1internal:*` (`413` on overflow, `400` on disconnect/read error), and sets `X-Content-Type-Options: nosniff` + `Cache-Control: no-store` on `/health`, `/api/stats`, and JSON error responses. |
| **S8** | Launcher integrity & packaging allow-list | `plugin/scripts/run-companion.sh` scrubs inherited proxy env vars via `exec_clean`, honors `XDG_DATA_HOME`, checks version-matched cache BEFORE unversioned `$PATH`, uses physical `cd -P --`, HTTPS-only downloads, SHA-256 verification (`sha256sum`/`shasum -a 256` with `< "${checksums_file}"` redirection), `tar -f - < "${tarball}"` stdin traversal checks, and `INT`/`TERM`/`HUP`/`EXIT` cleanup traps; `scripts/dev-install.sh` uses physical `cd -P --` symlink safety; `packages/npm/agy-companion/bin/agy-companion.js` uses `execFileSync` with a hardcoded 5-target `(platform, arch)` allow-list; npm manifests pin `"publishConfig": { "registry": "https://registry.npmjs.org/", "access": "public" }`; `packages/python/python/agy_companion/__init__.py` `find_binary()` rejects `..`/NUL and normalizes `AGY_COMPANION_BIN`; all 4 manifests (`Cargo.toml`, `plugin/plugin.json`, `packages/npm/agy-companion/package.json` + 5 `optionalDependencies`, `packages/python/pyproject.toml`) stay version-synced. |

## Local Antigravity App Development & Testing Workflow

See [`.agents/skills/local-dev/SKILL.md`](.agents/skills/local-dev/SKILL.md) for the complete step-by-step runbook. Summary:

- **Build & link plugin:** Run `./scripts/dev-install.sh` (builds `ui/` + `cargo build -p companion-cli` and symlinks `./plugin` into `~/.gemini/config/plugins/agy-companion` without modifying user Antigravity settings). `plugin/scripts/run-companion.sh` resolves symlinks on `$0` and automatically runs `<repo_root>/target/{debug,release}/agy-companion` so local dev never downloads from GitHub Releases.
- **Instant UI iteration (zero Rust rebuild):** From the repo root, export `AGY_COMPANION_UI_HTML="$(pwd)/ui/dist/index.html"`, run `npm --prefix ui run build`, and close/reopen the **Antigravity Companion** aux pane.
- **Live MCP reload (no App restart):** Restart **`agy-companion_hub`** via **Settings > MCP Servers > Restart** or invoke the `RefreshMcpServers` RPC with `{"serverName": "agy-companion_hub"}`.
- **Logs & live E2E:** Inspect sidecar logs at `~/.gemini/antigravity/sidecar_data/agy-companion/proxy/logs/`, run standalone `cargo run -p companion-cli -- serve`, or run isolated `agy` CLI E2E with `GOOGLE_GEMINI_BASE_URL="http://127.0.0.1:24141"`, `agy-companion doctor`, `agy-companion models --json`, and isolated `XDG_CONFIG_HOME` / `XDG_DATA_HOME`.
- **Deterministic opaque-box E2E:** See [`e2e/README.md`](e2e/README.md) and [`docs/design/testing-strategy.md`](docs/design/testing-strategy.md), and run `python3 -B e2e/run_e2e.py --suite all --workers 4 --binary target/debug/agy-companion --repo-root . -v` (plus `--suite ui-smoke --repo-root . -v` for headless Chrome UI smoke and dev-only `--self-test -v` when editing the E2E harness).

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
2. **Strict Red → Green → Refactor TDD.** Write a failing test reproducing the bug or specifying the new behavior first, implement the minimal fix to go green, then refactor. Never skip, delete, or weaken tests to get green. Follow the Single-Owner CUJ-first testing rules in [`docs/design/testing-strategy.md`](docs/design/testing-strategy.md) and [`.agents/skills/cuj-testing/SKILL.md`](.agents/skills/cuj-testing/SKILL.md) (strict hermetic local-config/port isolation, fast latency SLOs, no static repo source/doc grepping in `e2e/`, no hardcoded test counts in docs).
3. **Small, reversible changes.** Prefer focused PRs with a clear thesis over sprawling multi-concern diffs. Keep pure domain logic decoupled from I/O and external adapters.
4. **No secrets or local junk.** Never commit `.env`, credentials, real API keys, OS metadata (`.DS_Store`), or agent runtime directories (`.antigravitycli/`, `.remember/`).
5. **Parallel agent hygiene.** When running multiple agents concurrently, give each authoring agent an isolated git worktree and disjoint file ownership.

## When to Stop and Ask the Human

- The task requires changing a core architectural decision, license, or external public contract.
- Adding a new paid/sensitive external service, credential flow, or unapproved major dependency.
- A test cannot pass without weakening assertions, or requirements are genuinely ambiguous across materially different designs.
