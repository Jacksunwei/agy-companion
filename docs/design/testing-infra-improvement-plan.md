# Testing Infrastructure Overhaul — Implementation & Verification Record

> **Status:** **Implemented (Phase 0–3 Complete — PRs #47–#51 + Final Doc Sync)** on top of merged UX v2 (`PRs #40–#46` on `main`).
> **Evergreen Strategy Guide:** [`testing-strategy.md`](testing-strategy.md) · **Reusable Agent Skill:** [`.agents/skills/cuj-testing/SKILL.md`](../../.agents/skills/cuj-testing/SKILL.md).

---

## 1. Executive Summary

Prior to this overhaul, `agy-companion` exhibited inverted test investment: heavy multi-layer duplication across Rust and Python `e2e/`, recursive `./scripts/agent_check.sh` invocations dominating E2E wall-clock time, artificial per-feature quota assertions, white-box source/doc grepping in `e2e/`, zero behavioral tests for the modular UX v2 frontend (`ui/src/`), an intermittent single-threaded Tokio flake in `crates/companion-cli/src/daemon.rs`, `ISO-1..ISO-11` host-state leaks (including live probes against `127.0.0.1:24141`, real `$HOME` shell RCs, and ambient host env vars), and a ~609-LOC embedded bash SVG parser inside `scripts/agent_check.sh`.

All six workstreams (`WS1–WS6`), all eleven hermeticity remediations (`ISO-1..ISO-11`), all four `S1–S8` security red lines, and full UX v2 behavioral/E2E/CDP coverage have now been implemented and merged incrementally onto `main` while meeting every warm latency SLO with zero hardcoded test counts in docs.

---

## 2. Delivered PR Sequence (`Phase 0–3`)

| PR | Branch | Scope (`WS1–WS6`) | Key Deliverables |
| --- | --- | --- | --- |
| **#47** | `docs/testing-strategy-and-cuj-skill` | Phase 0 (`WS6` initial) | Established [`testing-strategy.md`](testing-strategy.md), this plan, `.agents/skills/cuj-testing/SKILL.md`, `docs/README.md`, and always-on rules in `.agents/rules/workflow.md` + `AGENTS.md`. |
| **#48** | `test/ui-node24-suite` | `WS3` (`D1-A`) | Extracted pure state helpers into `ui/src/state.ts`, rebuilt deterministic `ui/dist/index.html` (`< 512,000` bytes), added zero-dep Node 24 `ui/tests/*.test.ts` (`fake_dom.ts`, `protocol.test.ts`, `pricing_and_format.test.ts`, `state_and_theme.test.ts`, `views.test.ts`) strictly outside `ui/src/`, and created the 4 Rust↔TS golden wire fixtures in `ui/tests/fixtures/*.json`. |
| **#49** | `refactor/rust-hermetic-tests-and-fs-safe` | `WS4` + `ISO-3..8, 11` | Enforced `env_clear()` allowlist + `ChildGuard` across `crates/companion-cli/tests/common/mod.rs` and unified `tests/cli_integration.rs` (+ 6 submodules); added `isolated_ui_server*` + `hybrid_active_override` in `companion-ui-mcp`; fixed `daemon.rs` multi-thread Tokio flakes and `companion-proxy` raw-TCP RST flake; replaced `1s` unit timeouts with `150ms` `Duration`; migrated all production `symlink_metadata` call sites in `companion-cli` onto `companion_core::fs_safe`; collapsed symlink suites to the behavioral Minimum Symlink Wiring Matrix; added Rust↔TS `serde_json::to_value` golden wire parity test against `ui/tests/fixtures/*.json`. |
| **#50** | `refactor/e2e-parallel-cuj-and-cdp-smoke` | `WS1` + `WS2` (`D2-A`) + `ISO-1, 2, 6, 9, 10` | Deleted legacy `e2e/test_tier*.py` files (removing all recursive `agent_check.sh` forks, `SELF-16`/`SELF-21` quotas, heredoc parser, `24141` binder, and white-box source/doc greps); slimmed `e2e/test_self.py` to the 3 essential guardrails (`IsolatedEnv`, `PidfileReaper`, `McpStdioClient`); upgraded `e2e/run_e2e.py` to parallel stdlib `ProcessPoolExecutor` with intent-named `--suite cuj\|boundary\|shims\|ui-smoke\|all` (+ `--tier` alias); added `e2e/test_cuj_ux_v2.py` (5 cross-process UX v2 CUJs) and lean pure-stdlib headless Chrome CDP UI smoke (`e2e/cdp_smoke.py`, `e2e/test_ui_smoke.py`). |
| **#51** | `chore/agent-check-expat-svg-and-ci` | `WS5` (`D3-A`) | Replaced the embedded bash SVG parser in `scripts/agent_check.sh` with strict fail-closed Expat allowlist `scripts/check_plugin_logo.py` + fast in-process `scripts/test_check_plugin_logo.py` (Python 3.9+ & 3.14+ compatible); wired `npm --prefix ui test` and Gate `6c` (5 automated static hermeticity guards); removed redundant `--self-test` execution from Gate 5; updated `.github/workflows/ci.yml` to run UI tests, `agent_check.sh`, parallel E2E `--suite all --workers 4`, and `rust-windows`. |
| **PR 6** | `docs/sync-testing-infra-and-verification` | `WS6` (final sync) | Synced `AGENTS.md`, `testing-strategy.md`, this record, `.agents/skills/cuj-testing/SKILL.md`, `docs/README.md`, and `README.md` with zero hardcoded test counts or stale byte sizes, backed by full consolidated live verification including Hostile Developer Shell testing. |

---

## 3. Hermeticity Remediation Status (`ISO-1..ISO-11` → Resolved)

All eleven host-state isolation violations identified in the pre-overhaul audit are resolved and guarded against regression by `scripts/agent_check.sh` Gate `6c` plus live Hostile Developer Shell verification:

| ID | Original Leak | Implemented Remediation | Static Guard |
| --- | --- | --- | --- |
| **ISO-1** | `IsolatedEnv.make_config_dict()` defaulted `listen_port=24141` | Default `listen_port=None` auto-allocates an ephemeral port outside `24140..=24160` via `find_free_port()`. | Gate `6c` Guard 3 |
| **ISO-2** | Legacy `test_t2_r22` bound `127.0.0.1:24141` directly | Deleted from `e2e/`; fresh-install port fallback is owned hermetically by `companion-cli` unit tests. | Gate `6c` Guard 3 |
| **ISO-3** | Bare `CompanionUiServer::new(...)` read developer `$HOME` RCs | Replaced with `isolated_ui_server*` setting `integration_options_override`, `UiHtmlSource::Fixed(None)`, `env_api_key_fallback: false`, and `hybrid_active_override: Some(false)`. | Gate `6c` Guard 2 |
| **ISO-4** | Raw `Command::new(BIN)` spawns inherited host env in CLI tests | Consolidated all spawns through `isolated_cli_command` / `isolated_tokio_cli_command` in `crates/companion-cli/tests/common/mod.rs`. | Gate `6c` Guard 1 |
| **ISO-5** | Partial `env_remove` denylist missed proxy/Windows/UI env vars | Switched to `cmd.env_clear()` + minimal safe allowlist + isolated temp `HOME`/`XDG_*`/`TMPDIR`/`APPDATA`/`LOCALAPPDATA`/`USERPROFILE`. | Gate `6c` Guard 1 |
| **ISO-6** | Unguarded child spawns could leak orphan processes on panic | Wrapped all Rust `.spawn()` sites in `ChildGuard` and all E2E spawns in `CompanionProcess` / `McpStdioClient` / `ChromeBrowser` / `ExitStack` / `atexit`. | Code & runtime check |
| **ISO-7** | Individual `ui-mcp` tests omitted `env_api_key_fallback: false` or integration override | Enforced universally by `isolated_ui_server*` constructor defaults. | Gate `6c` Guard 2 |
| **ISO-8** | `free_loopback_port()` could collide with `24141..=24150` | Excluded `24140..=24160` across both Rust `free_loopback_port()` / `free_closed_port()` and Python `find_free_port()`. | Gate `6c` Guard 3 |
| **ISO-9** | `IsolatedEnv.build_env()` lacked `XDG_CACHE_HOME` & Windows profile vars; `test_self.py` mutated `os.environ` | Added `XDG_CACHE_HOME`, `APPDATA`, `LOCALAPPDATA`, `USERPROFILE`, and `PYTHONDONTWRITEBYTECODE=1`; eliminated in-place `os.environ` mutation and switched runner to `ProcessPoolExecutor`. | Gate `6c` Guard 5 |
| **ISO-10** | `run_cli(...)` calls omitted explicit `env=` | Made `env=` keyword-only and required at runtime in `e2e/harness.py` (`ValueError` on `None`) and enforced via Python AST check. | Gate `6c` Guard 5 |
| **ISO-11** | `daemon.rs` unit test called ambient `default_data_dir()` | Injected `resolve_runtime_data_dir_with_default` for hermetic testing. | Hostile Shell check |

---

## 4. Security Red Lines (`S1–S8`) — Enforced Posture

1. **Red Line 1 — Symlink-Ancestor Wiring (`S2`/`S6`):** All production `fs::symlink_metadata` sites across `crates/companion-cli/src/` were migrated onto `companion_core::fs_safe` / `inspect_leaf_kind` (preserving intentional dev-symlink and `local_bin_link` detection), eager `fs_safe` prechecks were added to `serve.rs` (`usage.db` sidecars, `proxy.pid`, `proxy.log`), and the unified `crates/companion-cli/tests/cli_integration/install_symlink_safety.rs` enforces the behavioral **Minimum Symlink Wiring Matrix** across all seven entry-point surfaces (`install`, `uninstall`/`--purge`, `integration`, `serve`, `usage.db`, `ui-mcp`, `doctor`) asserting victim-file integrity rather than brittle warning prose.
2. **Red Line 2 — Strict Expat SVG Validator (`S5`/`S8`):** `scripts/check_plugin_logo.py` enforces a fail-closed Expat XML allowlist banning `<use>`, `href`, `xlink:href`, `style`, `on*`, `<!DOCTYPE>`, XXE, PIs, CDATA, and intermediate manifest symlinks/FIFOs (verified via `os.lstat` `S_ISREG` + canonical `relative_to`), tested in-process by `scripts/test_check_plugin_logo.py` (`< 0.05s`) under both stock macOS Python 3.9+ and Python 3.14+.
3. **Red Line 3 — Opaque-Box E2E Partition:** All legacy `e2e/test_tier*.py` files were replaced by intent-named modules (`test_cuj_core.py`, `test_cuj_ux_v2.py`, `test_boundary_security.py`, `test_shims_packaging.py`, `test_ui_smoke.py`) plus slim 3-guardrail `test_self.py` (`IsolatedEnv`, `PidfileReaper`, `McpStdioClient`), with zero `agent_check.sh` recursion and zero white-box repo source/doc reads (enforced statically by Gate `6c` Guard 4).
4. **Red Line 4 — `ui/tests/` Outside `ui/src/` (`S5`):** All Node 24 behavioral tests and 4 golden wire JSON fixtures live strictly under `ui/tests/`, keeping `ui/src/` and `ui/dist/index.html` (`< 512,000` bytes) 100% pure for Vite bundling and Gate `6b` S5 sink/external-tag scanning.

---

## 5. Achieved Latency SLO & Verification Posture

Every verification layer meets its hard warm SLO from [`testing-strategy.md`](testing-strategy.md) §8 with zero flakes and zero host-state leaks:

| Verification Layer | Live Command | Hard Warm SLO | Achieved Posture |
| --- | --- | ---: | --- |
| **Pre-Commit / Pre-PR Gate** | `./scripts/agent_check.sh` | **`< 12.0s`** | **PASS (7/7 gates, well within `< 12.0s` warm)** |
| **Rust Workspace (Warm)** | `cargo test --workspace -q` | **`< 6.0s`** | **PASS (`< 6.0s` warm; each crate `--lib` `< 1.5s`; `cli_integration` `< 2.5s`; `0` flakes)** |
| **UI Behavioral & Golden Suite** | `npm --prefix ui test` | **`< 1.0s`** | **PASS (`< 1.0s`, zero new npm deps, Rust↔TS golden parity verified)** |
| **Opaque-Box E2E (Parallel)** | `python3 -B e2e/run_e2e.py --suite all --workers 4 --binary target/debug/agy-companion --repo-root . -v` | **`< 12.0s`** | **PASS (`< 12.0s`, stdlib `ProcessPoolExecutor`)** |
| **Opaque-Box E2E (Serial)** | `python3 -B e2e/run_e2e.py --suite all --workers 1 --binary target/debug/agy-companion --repo-root . -v` | **`< 30.0s`** | **PASS (`< 30.0s`)** |
| **Headless Chrome CDP UI Smoke** | `python3 -B e2e/run_e2e.py --suite ui-smoke --repo-root . -v` | **`< 5.0s`** | **PASS (cleanly skips when Chrome absent; Dark/Light `360/460/760px` WCAG AA + health-only popover + RAM footer)** |
| **Dev-Only Harness Guardrails** | `python3 -B e2e/run_e2e.py --self-test -v` | **`< 3.0s`** | **PASS (decoupled from `agent_check.sh` & CI)** |
| **SVG Validator Unit Suite** | `python3 -B scripts/test_check_plugin_logo.py -v` | **`< 0.05s`** | **PASS (Python 3.9+ and 3.14+)** |
| **Hostile Developer Shell** | Poisoned `HOME`/`XDG_*`/`GEMINI_API_KEY`/`GOOGLE_GEMINI_BASE_URL`/`AGY_COMPANION_*`/`HTTP(S)_PROXY`/`RUST_LOG` + live `127.0.0.1:24141` TCP canary | **`0` leaks** | **PASS across Rust, UI, E2E `--suite all`, and `--suite ui-smoke` (`0` canary hits, `0` host FS mutations)** |

---

## 6. Executed Decisions (`D1–D4`)

| ID | Decision | Implemented Choice |
| --- | --- | --- |
| **`D1`** | **UI Behavioral Testing Stack** | **`D1-A` (Implemented in PR #48):** Zero-dep Node 24 `node --test tests/*.test.ts` in `ui/tests/` (outside `ui/src/`) with zero-dep `fake_dom.ts`, `ui/src/state.ts`, and 4 Rust↔TS golden wire JSON fixtures in `ui/tests/fixtures/*.json`. |
| **`D2`** | **E2E Runner Interface & `test_self.py`** | **`D2-A` (Implemented in PR #50 & PR #51):** Primary `--suite cuj\|boundary\|shims\|ui-smoke\|all` + `--workers N` (`ProcessPoolExecutor`) with deprecated `--tier` alias; `test_self.py` slimmed to 3 essential guardrails and removed from `agent_check.sh` and `ci.yml`. |
| **`D3`** | **`scripts/agent_check.sh` SVG Validator** | **`D3-A` (Implemented in PR #51):** Fail-closed Expat allowlist in `scripts/check_plugin_logo.py` + fast in-process `scripts/test_check_plugin_logo.py`, replacing the embedded bash parser and multi-fork fuzz loops. |
| **`D4`** | **Sequencing Relative to UX v2** | **Completed on merged UX v2 (`PRs #40–#46`):** Overhaul landed cleanly on top of UX v2 with full UX v2 behavioral, golden-fixture, cross-process E2E (`test_cuj_ux_v2.py`), and headless Chrome CDP (`test_ui_smoke.py`) coverage. |
