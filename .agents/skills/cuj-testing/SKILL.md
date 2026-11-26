---
name: cuj-testing
description: Design, write, review, refactor, or prune automated tests in agy-companion using the CUJ-first Testing Trophy and Single-Owner layer rules. Use whenever adding tests for a new feature or bug fix (Rust crates, MCP tools, proxy routes, CLI, UI state/protocol, or Python E2E), refactoring tests, reviewing test quality/coverage in a PR, fixing flaky tests, or auditing test bloat/duplication — even if the user just asks to "add tests" or "verify this change". Do NOT use for local plugin linking, AGY_COMPANION_UI_HTML live reload, sidecar log debugging, or manual live agy CLI runs (use local-dev instead).
---

# CUJ-First Testing & Anti-Slop Skill (`agy-companion`)

Use this skill whenever designing, writing, reviewing, or pruning automated tests in `agy-companion`. For the full CUJ 0–6 matrix and UX v2 testing contract, read [`docs/design/testing-strategy.md`](../../../docs/design/testing-strategy.md). For the point-in-time audit and `WS1–WS6` pruning roadmap, see [`docs/design/testing-infra-improvement-plan.md`](../../../docs/design/testing-infra-improvement-plan.md).

## 1. Core Stance: The One-Home Rule

Every invariant or user behavior belongs in **exactly one** authoritative layer:
- **Test public boundaries and CUJs thoroughly** (HTTP routes, MCP JSON-RPC tools, CLI exit/JSON/side effects, on-disk schemas, UI state transitions, cross-process binary lifecycles).
- **Keep internal unit tests focused** on non-trivial pure logic and security primitives (`fs_safe`, SSRF/URL validation, SSE tee, CloudCode routing, SQLite rollups, redaction).
- **Why:** Duplicate checks across Rust, `agent_check.sh`, and Python `e2e/` previously bloated tests to nearly 2× production LOC, broke internal Rust module refactors (source-file greps in `e2e/`), collided with UX v2 UI changes (regexes on private JS vars), and wasted 65%+ of E2E runtime re-running `agent_check.sh`.

## 2. Layer Ownership Decision Tree

Before writing a test, place it in the **single** owning row below and stay within its warm SLO and soft per-test cap:

| Layer | Owns (PUT HERE) | Never Put Here | Hard Warm SLO & Per-Test Soft Cap |
| --- | --- | --- | --- |
| **1. `scripts/agent_check.sh`** | Repo hygiene, no `GEMINI.md`, `AIza...` secret scan, zero AI attribution, `bash -n`, 4-way version sync, `ui/src/` + `ui/dist/` S5 XSS/CSP/size scan, static hermeticity guards, plugin JSON + strict Expat SVG allowlist + `agy plugin validate` | Never invoke `agent_check.sh` from tests; never run `e2e --self-test` or `npm run build` inside `agent_check.sh` | **`< 12.0s` warm** *(static checks `< 2s`)* |
| **2. Rust `--lib`** (`crates/*/src/`) | Pure domain & security primitives (`fs_safe`, `redaction`, `validation`, `models`, XDG/Windows path precedence, SQLite WAL `0600` & range rollups, SSE parser, CloudCode routing, `LoopbackProxyClient`, MCP tool semantics & golden JSON parity) | `OnceLock` pointer identity, `Arc::ptr_eq`, `clap` derive mirrors, duplicate `ui/src/` S5 greps, `1s+` unit sleeps | **`< 1.5s` / crate** (`< 100ms` pure / `< 300ms` HTTP; workspace **`< 6.0s`**) |
| **3. Rust `--test`** (`crates/companion-cli/tests/`) | CLI subcommand contracts (`install`/`uninstall`, `integration`, `hook-env`, `serve`, `doctor`, `models`, `launch`), compact behavioral symlink wiring tests (post-conditions, not prose) | Copy-pasted 100-LOC symlink permutations already covered by `fs_safe`; raw `Command::new(BIN)` | **`< 2.5s`** (`< 1.0s` / test; 1 idle-SSE `< 3.0s`) |
| **4. UI Tests** (`ui/tests/*.test.ts`, `node --test`) | Pure `ui/src/{protocol,state,format}.ts` tests: Rust↔TS golden JSON parsing, banner priority queue, Day-0/N tab landing, `enabled_models` toggles/`[]` pause, `snapshotEpoch` & write lockout, polling backoff, Usage `$`/`Tokens`, footer RAM & health-only popover, open-source copy | Never place tests inside `ui/src/` (`agent_check.sh:576-596` greps `ui/src/` for S5 sinks and Vite bundles it!) | **`< 1.0s`** (`< 50ms` / test) |
| **5. Opaque-Box E2E** (`e2e/`) | Real multi-process binary CUJs (CUJ 0–6), 1-per-invariant binary `S1–S7`/stdio canaries, real `sh`/`zsh` `env.sh`, `run-companion.sh`, `dev-install.sh`, npm & Python runtime shims | Zero `self.repo_root` reads of `crates/`, `ui/src/`, `README.md`, `AGENTS.md`, or `docs/`; zero `agent_check.sh` calls; zero port-`24141` binds/probes | **`< 12.0s`** (`--workers 4`) / **`< 30.0s`** serial (`< 3.0s` / test; 1 idle-SSE `< 5.5s`) |

## 3. Strict Hermeticity (6-Point Contract)

Tests MUST NEVER read, depend on, or mutate real developer state (`~/.config/agy-companion`, `~/.local/share/agy-companion`, `~/.gemini/`, shell RC files, port `24141`, or exported host env vars):

1. **Isolated Temp Roots & Explicit DI:** Always use `TempDir` / `IsolatedEnv` and explicit DI (`isolated_ui_server(...)` setting `integration_options_override`, `ui_html_source: Fixed(None)`, `env_api_key_fallback: false`; `DoctorEnv`; `ProxyStateOptions`). Never call bare `CompanionUiServer::new(...)` in tests.
2. **Allowlist-Only Child Envs (`env_clear()`):** Spawn CLI/E2E subprocesses strictly via `isolated_cli_command` (`env_clear()` + allowlist) or `IsolatedEnv.build_env()` (incl. `XDG_CACHE_HOME` & Windows `APPDATA`/`LOCALAPPDATA`/`USERPROFILE`). Never raw `Command::new(BIN)` or `run_cli(...)` without `env=`.
3. **Zero Hardcoded `24141..=24150` Ports:** Bind `127.0.0.1:0` in-process and auto-allocate ephemeral ports (excluding `24140..=24160`) for subprocesses and `make_config_dict()`.
4. **Zero Process-Wide Env Mutation:** Rust `#![forbid(unsafe_code)]` forbids `set_var`; in Python never mutate `os.environ` in place — pass `env=` dicts and parallelize via stdlib `ProcessPoolExecutor`.
5. **Guaranteed RAII Teardown:** Guard every child/daemon before asserting (`ChildGuard` / `KillOnDrop` in Rust; `CompanionProcess` / `McpStdioClient` / `ExitStack` in Python). Zero unguarded `.spawn()` or raw `subprocess.Popen`.
6. **Hostile Developer Shell Check:** Before merging test-infra PRs, verify tests pass while a listener holds `127.0.0.1:24141` (always use a short fake key — NEVER a 39-char `AIza...` string!):
   ```bash
   GEMINI_API_KEY=hostile-test-key GOOGLE_GEMINI_BASE_URL=http://127.0.0.1:1 \
   AGY_COMPANION_CONFIG=/nonexistent/hostile.json AGY_COMPANION_UI_HTML=/nonexistent/hostile.html \
   HTTP_PROXY=http://127.0.0.1:1 RUST_LOG=trace cargo test --workspace
   ```

## 4. Six Anti-Slop Red Flags (`Bad → Good`)

1. **Recursive gate calls:**
   - *Bad:* Running `./scripts/agent_check.sh` inside `e2e/` (re-runs `fmt`/`clippy`/`cargo test` repeatedly).
   - *Good:* Run `./scripts/agent_check.sh` once as Layer 1; never call it from tests.
2. **White-box source/doc grepping in `e2e/`:**
   - *Bad:* `re.search(r"refs\.refreshBtn\.disabled", main_ts)` or `(self.repo_root / "crates/.../install.rs").read_text()`.
   - *Good:* Behavioral TS tests in `ui/tests/*.test.ts` (`node --test`), Rust unit tests, and Windows CI for `cfg(windows)`.
3. **Quota padding & subprocess loops for pure functions:**
   - *Bad:* Adding filler tests to satisfy `>= N` per feature, or spawning `ui-mcp` 13 times to test `mask_api_key` length cutoffs.
   - *Good:* One table-driven Rust unit test in `companion-core::redaction` (`< 1ms`) + 1 redaction assertion in a real E2E CUJ.
4. **Symlink copy-paste & verbatim prose assertions:**
   - *Bad:* Duplicating full `fs_safe` directory-chain permutations across consumers and asserting exact stderr warning sentences.
   - *Good:* Exhaustive matrix once in `companion-core::fs_safe`. Every file-mutating consumer (`install`, `uninstall`, `integration`, `serve`, `usage.db`, `ui-mcp`, `doctor`) delegates to `fs_safe` and keeps 1–2 compact behavioral wiring tests asserting victim sentinel integrity and structured error/warning state.
5. **Implementation-detail, slow sleeps, or single-thread flaky async tests:**
   - *Bad:* `std::ptr::eq` on `OnceLock`, `Arc::ptr_eq`, test-only bash parsers, whole-second `Duration::from_secs(1)` unit timeouts, or single-threaded `#[tokio::test]` spawning `axum::serve` with `200ms` `probe_timeout`.
   - *Good:* Assert observable behavior only; inject `#[cfg(test)] Duration::from_millis(150)` for timeout tests; for in-process `axum::serve` Tokio tests use `#[tokio::test(flavor = "multi_thread", worker_threads = 2)]` + `tokio::task::yield_now().await` + `probe_timeout >= 1000ms`.
6. **Hardcoded test counts in docs:**
   - *Bad:* Writing `"335 Rust tests"` or `"26 self-tests"` into `AGENTS.md` or `README.md`.
   - *Good:* Describe *what* each gate verifies and *how* to run it.

## 5. Adding Tests for New Interfaces & UX v2

- **Config / Proxy / CLI feature (e.g., `enabled_models: []` pause, `launcher_auto_start`, `integration.lock`):** Write table-driven Rust unit/proxy tests covering state transitions and 1 focused concurrency test for `0600` lock serialization.
- **SQLite / Telemetry (e.g., `Today`/`7d`/`30d` rollups, session ring buffer, proxy RAM):** Write fast in-memory/tempdir Rust tests covering empty DB, UTC day boundaries, cache-savings math, and malformed/zero-token rows.
- **MCP ↔ UI Wire Change (`SettingsSnapshot`, `UsageSummary`, `account_plan`):** Update canonical JSON fixtures in `ui/tests/fixtures/*.golden.json`, assert `serde_json::to_value` parity in `companion-ui-mcp`, and parse those exact fixtures in `ui/tests/protocol.test.ts`.
- **UI Logic:** Extract pure functions into `ui/src/{protocol,state,format}.ts` (using `import type`, no TS `enum` or constructor parameter properties so Node 24 `ts-strip` runs natively) and test in `ui/tests/*.test.ts` (outside `ui/src/`).
- **SVG / Security Gates:** Never weaken `plugin/assets/logo.svg` validation to regex or `ElementTree` — keep strict Expat allowlisting banning `<use>`, `href`/`xlink:href`, `style`, `on*`, DTD/XXE/PI/CDATA, verified by a fast in-process Python test.

## 6. Fast Inner-Loop Commands by Change Scope

| Changed Path | Fastest Targeted Verification | Warm SLO |
| --- | --- | ---: |
| `crates/companion-core/` | `cargo test -p companion-core --all-features` | `< 1.5s` |
| `crates/companion-providers/` | `cargo test -p companion-providers` | `< 1.5s` |
| `crates/companion-proxy/` | `cargo test -p companion-proxy` | `< 1.5s` |
| `crates/companion-ui-mcp/` | `cargo test -p companion-ui-mcp` | `< 1.5s` |
| `crates/companion-cli/` | `cargo test -p companion-cli` | `< 1.5s` lib / `< 2.5s` int |
| `ui/` | `npm --prefix ui test && npm --prefix ui run typecheck && npm --prefix ui run build` | `< 1.0s` tests |
| `plugin/` | `agy plugin validate --strict ./plugin` | `< 1.0s` |
| Cross-process / binary E2E | `cargo build -p companion-cli && python3 -B e2e/run_e2e.py --tier all --binary target/debug/agy-companion --repo-root . -v` *(switches to `--suite all --workers 4` post-Phase 2)* | `< 12.0s` parallel |
| **Mandatory Pre-PR Gate** | `./scripts/agent_check.sh` | `< 12.0s` |

## 7. Eight-Point Pre-PR Test Review Checklist

1. **One Home:** Does every new test live in the single owning layer (zero overlap with `agent_check.sh` or lower layers)?
2. **Opaque-Box E2E:** Does any `e2e/` diff read `crates/`, `ui/src/`, `README.md`, `AGENTS.md`, or `docs/`, or invoke `agent_check.sh`? If yes, reject and move it.
3. **Behavioral, Not Fragile:** Do assertions verify public outputs, wire contracts, or victim-file integrity rather than private variable names, pointers, or exact warning prose?
4. **S5 & Secret Hygiene:** Are all UI tests in `ui/tests/` (outside `ui/src/`), and are all example keys synthetic (never a 39-char `AIza...` key)?
5. **Determinism:** Do async Tokio tests spawning local servers use `multi_thread` + `yield_now()` + generous timeouts?
6. **Zero Count Drift:** Did the PR avoid adding hardcoded test counts or bundle byte literals to docs?
7. **Strict Hermetic Isolation:** Zero real `$HOME`/XDG/`~/.gemini`/RC reads, allowlist-only `env_clear()` helpers (`isolated_cli_command`, `isolated_ui_server`, `IsolatedEnv`), zero `24141..=24150` binds/probes, RAII child teardown, and passes the Hostile Developer Shell smoke check?
8. **Fast Speed SLO Compliance:** Within warm SLOs & per-test caps (`< 100ms` unit, `< 1s` integration, `< 3s` E2E, `< 6s` workspace, `< 12s` `agent_check.sh`, `< 12s` `--workers 4` E2E); no `1s+` unit sleeps (inject `150ms` `Duration`), no `--self-test` in `agent_check.sh`, and parallel-safe under `ProcessPoolExecutor`?
