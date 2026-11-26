# Testing Infrastructure Audit & Improvement Plan (`2026-11-25`)

> **Status:** Point-in-time audit and phased execution plan (`HEAD dac80c6`; `origin/main` 1 commit ahead at merged PR #40 `f4bd797`).
> **Evergreen Strategy Guide:** [`testing-strategy.md`](testing-strategy.md) · **Reusable Agent Skill:** [`.agents/skills/cuj-testing/SKILL.md`](../../.agents/skills/cuj-testing/SKILL.md).

---

## 1. Executive Diagnosis & Canonical Baseline (`HEAD dac80c6`)

`agy-companion` currently exhibits **inverted test investment**: **46,607 LOC of test and verification code** against **24,188 LOC of production code** (**1.93× full-repo**, **2.04× core app**), with **693 automated checks** taking **~216s warm** — yet **`ui/src/` (4,623 LOC of stateful TypeScript) has 0 behavioral tests**, CI runs **0 E2E tiers** and **0 Windows jobs**, a Rust daemon test is **flaky under cold CPU load**, **70+ existing tests leak into or depend on live developer state** (`127.0.0.1:24141`, real `$HOME` shell RCs, ambient host env vars, and unguarded child spawns), and **130.20s (65.7%) of E2E wall-clock time** is wasted recursively re-running `./scripts/agent_check.sh` alongside serial E2E execution and whole-second Rust/shim sleeps.

### Canonical Verified Metrics Table

| Layer / Component | Files | Prod LOC | Test / Gate LOC | Ratio | Checks / Tests | Measured Warm Runtime |
| --- | ---: | ---: | ---: | ---: | ---: | ---: |
| `crates/companion-core` | 10 `.rs` | 5,749 | 7,399 | 1.29× | 107 (97 default + 10 `sqlite`) | 0.88s |
| `crates/companion-providers` | 5 `.rs` | 1,212 | 2,484 | 2.05× | 27 | 1.01s |
| `crates/companion-proxy` | 21 `.rs` | 4,109 | 6,496 | 1.58× | 77 | 0.12s |
| `crates/companion-ui-mcp` | 6 `.rs` | 940 | 2,846 | **3.03×** | 30 | 1.12s |
| `crates/companion-cli` | 11 `.rs` | 5,506 | 8,289 | 1.51× | 93 (55 lib + 2 bin + 36 `cli_integration.rs`) | 3.88s |
| **Rust Workspace Subtotal** | **53 `.rs`** | **17,516** | **27,514** | **1.57×** (`61.1%`) | **334** *(335 on `f4bd797`)* | **12.65s** *(cold: 47.98s + flake)* |
| `ui/src/` (`main.ts` 3,185 + `style.css` 1,437 + `vite-env.d.ts` 1) | 3 | 4,623 | **0** | **0.00×** | **0** | typecheck/build only |
| **Core App Subtotal (`crates/` + `ui/src/`)** | **56** | **22,139** | **45,244** | **2.04×** | **693** *(incl. `e2e/`)* | — |
| `plugin/` (bundle + `run-companion.sh`) | 7 | 1,220 | 0 | — | *(in E2E + `agent_check`)* | — |
| `packages/` (npm + Python shims) | 5 | 561 | 0 | — | *(in E2E + `agent_check`)* | — |
| `scripts/dev-install.sh` | 1 | 268 | 0 | — | *(in E2E + `agent_check`)* | — |
| `e2e/` (Python opaque-box suite) | 14 `.py` | 0 | **17,730** | — | **359** (`333` tier + `26` self) | **203.33s** (`198.21s` T1–T4 + `5.12s` self) |
| `scripts/agent_check.sh` | 1 `.sh` | 0 | **1,295** | — | 7 gates (`608` LOC / `47%` SVG parser) | ~16–18s warm |
| `.github/workflows/ci.yml` | 1 `.yml` | 0 | 68 | — | duplicates `agent_check` + 2× self-test, **0** E2E | — |
| **Full Repo Grand Total** | **85** | **24,188** | **46,607** | **1.93×** (`65.8%`) | **693** | **~216s** (warm Rust + full E2E) |

*Notes:* Core App test LOC (`45,244`) = `27,514` Rust test + `17,730` `e2e/` (excludes `ui/dist/index.html` [`302,701` bytes], lockfiles, 63-LOC `ui/` build configs, and `docs/`). In `e2e/test_self.py` (`1,483` LOC), **`26`** top-level self-tests execute (`5.12s`); the other `8` `def test_*` lines are nested fixture methods in `SELF-19` and `SELF-24` (so `26` in docs is accurate, whereas `README.md`'s `320` vs `334`/`335` Rust tests is genuine doc drift).

### Key Empirical Breakdowns & Live Sibling State

- **Recursive `agent_check.sh` Tax Inside E2E (`130.20s / 198.21s` = `65.7%`):** `5` full repo runs (`T1-F12` ×2, `T2-F12`, `T3-11`, `T4-06`) + `69` synthetic repo forks (`48` logo + `12` AI-attribution + `9` UI external-tag) dominate every tier (`T1: 54.26s / 80.64s`, `T2: 41.67s / 63.87s`, `T3: 17.03s / 26.10s`, `T4: 17.24s / 27.61s`). All other `322` E2E tests combined finish in **`68.01s`**.
- **Non-Binary E2E Tests (`79 / 333` = `23.7%`, `1,893` LOC):** `254` tier tests invoke the Rust binary; `79` never execute it = **`64` zero-value static/`agent_check` tests** (`58` pure static/lint/manifest/doc/source-grep [`809` LOC] + `6` `agent_check.sh` forks [`357` LOC]) + **`15` real shell/npm/Python runtime shim tests** (`727` LOC). Total touching `self.repo_root` = `90` (`79` non-binary + `11` binary, `5` of which embed static checks).
- **Verified Live Rust Flake (`crates/companion-cli/src/daemon.rs:1057, 1099`):** `daemon.rs:1099-1184` (`wait_for_daemon_healthy_requires_exact_version_match_and_tracks_stale_version`) failed on cold `cargo test` (`exit=101`, `TimedOut { last_observed_version: None }`) because single-threaded `#[tokio::test]` starves `axum::serve` against a `200ms` `probe_timeout` without `yield_now()`. Sibling `daemon.rs:1057` (`250ms` `probe_timeout`) shares the exact pattern. Fix **both** with `#[tokio::test(flavor = "multi_thread", worker_threads = 2)]` + `tokio::task::yield_now().await` + `1000ms` `probe_timeout`.
- **Live Sibling Context:** PR #40 (`f4bd797`, Rust `install`/`integration`/`cli_integration` split) is **already squash-merged** to `origin/main` (touching only `AGENTS.md` `L14, L36, L66` and forcing 3× copy-paste of `_read_rust_module_source` in `e2e/test_tier{1..3}_rollout.py`). UX v2 **PR A–D** (including the health-only `● Live` popover & footer RAM telemetry update) is launching imminently — and `e2e/test_tier1_features.py:1207-1232` (`refs.refreshBtn.disabled`, `refs.endpointTag`) + `e2e/test_tier1_rollout.py:1882-1902` (`"cloudcode"`) are guaranteed to collide with UX v2 PR C.

---

## 2. Four-Bucket Audit & Multi-Layer Overlap Matrix

| Bucket | Summary | Key Evidence (`file:line`) | Action |
| --- | --- | --- | --- |
| **A — Keep (High-Value CUJ & S1–S8)** | Fast Rust domain/protocol/routing suites, Tier 4 multi-process CUJs, real runtime shims | `companion-proxy` (77 tests in `0.12s`), `companion-core::{fs_safe,usage::sqlite,validation,models,redaction,config}` (`0.88s`), `companion-providers`, `companion-ui-mcp` tool contracts, `T4-01..04, 07`, `T4-R01..R05`, real `run-companion.sh` / `dev-install.sh` / npm / Python shims | **Preserve & protect** |
| **B — Redundant (4×–7× Multi-Layer Overlap)** | Same invariant re-tested across `agent_check.sh`, Rust unit, `cli_integration`, and E2E T1–T4 | `agent_check.sh` 5× in E2E (`130.20s`), S5 UI sink scan 7× (`ui-mcp/tests.rs:329-360` + 5 E2E + `agent_check`), `GEMINI.md` absent 6×, 4-way version sync 5–6×, `mask_api_key` via 13 `ui-mcp` spawns, `/health` & `/api/stats` 6× `serve` spawns, placeholder key 16 tests across 5 layers, CloudCode 55+ tests (`0.05s` Rust + 24+ E2E), `doctor.rs:684-825` duplicating `probe_client.rs`, `ci.yml` running `--self-test` 2× | **Enforce Single-Owner** |
| **C — Coupled / Brittle / Flaky** | White-box source greps, verbatim prose checks, internal pointers, single-thread async flake | `T1-F7:1207-1232` (`refs.refreshBtn`), `T1-R19:1882-1902` (`"cloudcode"`), `T1/T2/T3-R24` `.rs` greps, `cli_integration.rs:2433-4689` verbatim warning strings, `OnceLock` `std::ptr::eq` (`ui-mcp/tests.rs:2764`), `Arc::ptr_eq` (`proxy/tests.rs:833`), `cli.rs` `clap` mirrors (`452L` vs `224L`), `daemon.rs:1057,1099` flake | **Replace with behavioral contracts & fix flake** |
| **D — AI Slop, Quotas & Meta-Tests** | Artificial per-feature count floors and test-only parsers | `e2e/test_self.py:614-653` (`SELF-16` enforcing `T1 >= 72`, `T2 >= 80`, `>= 6` per `F1..F12`), 200-LOC bash heredoc parser (`T2-F15:2616-2831`) + 119-LOC `SELF-21` unit test for the parser (`test_self.py:908-1027`), 608-LOC SVG Expat+DFS parser (`agent_check.sh:653-1260`) + 234-LOC / 48-fork fuzz test (`T2-F12:2361-2595`, `15.32s`) | **Delete quotas & meta-slop; replace SVG validator cleanly** |

### Multi-Layer Overlap Matrix (Bucket B Hotspots)

| Behavior / Invariant | Rust Unit (`--lib`) | `cli_integration` | E2E T1/T2 | E2E T3/T4 | `agent_check.sh` / `ci.yml` | Single-Owner Verdict |
| --- | --- | --- | --- | --- | --- | --- |
| **`scripts/agent_check.sh` pass** | — | — | **3 full** (`T1:1686,1696`, `T2:2251`) + **69 forks** (`T2:2259,2314,2361`) | **2 full** (`T3:370`, `T4:556`) | IS `agent_check.sh` + `ci.yml` | Delete all from E2E (**`-130.20s`**) |
| **S5 `ui/src` forbidden sink scan** | `ui-mcp/tests.rs:329-360` | — | `T2:1325,1343`, `T2R:1453` | `T3R:698`, `T4:421` | `agent_check.sh:585` | Keep **only** in `agent_check.sh` |
| **`GEMINI.md` absent** | — | — | `T1:1723,1756`, `T2:2232,2601`, `T1R:2336` | — | `agent_check.sh:54` | Keep **only** in `agent_check.sh` |
| **4-way version sync + manifests** | — | — | `T1:1562-1680` (6), `T2:2019-2106` (6) | `T3:370`, `T4:556` | `agent_check.sh:263-532`, `ci.yml:36-49` | Keep in `agent_check.sh` + runtime shims |
| **Symlinked ancestor dir refusal** | `fs_safe.rs` (786L) + `integration.rs` (1,280L) + `install.rs` (187L) | 16 tests (`2,256L`) | `T2R:1158,1423,1668,1772` | `T4R:538` | — | Migrate to `fs_safe` + `~400L` wiring matrix |
| **API key redaction length bounds** | `redaction.rs` (`< 1ms`) + `ui-mcp` | `L520,916` | **13 `ui-mcp` spawns** (`T1:225-321`, `T2:261-445`) | `T3:262`, `T4:421` | — | Keep `redaction.rs` + 1 E2E canary |
| **`/health` & `/api/stats` headers** | `routes/tests.rs`, `stats.rs` | `L446` | **6 `serve` spawns** (`T1:967-1015`, `T2:1250-1279`) | `T3:80`, `T4:41` | — | Keep Rust unit + `T4-01` + 1 canary |
| **Placeholder key quarantine (`R07`)** | 7 Rust test sites | `L1790` | 6 E2E tests (`T1:2496`, `T1R`, `T2R`) | `T3R:327`, `T4R:677` | — | Core unit + 1 proxy/MCP/doctor canary |
| **CloudCode auto/403/LRU/slots** | `cloudcode/tests.rs` (26, `< 0.05s`) | — | 25+ E2E tests (`T1/T2R:R01-R06`) | `T3R`, `T4R:263` | — | Rust unit suite + `T4-R02` + 1 canary |
| **Idle shutdown & `--adopt-running`** | `proxy/tests.rs`, `daemon.rs` | 3 tests | 10+ real-sleep E2E tests | `T3R:441`, `T4R:441` | — | Rust unit + `T4-R03` + `T3-R06` + 1 |
| **`doctor` probe errors** | `probe_client.rs` (518L) + `doctor.rs` (142L) | `L520,577` | `T1:1429`, `T2:1628-1683` | `T3:129,489`, `T4:630` | — | Keep `probe_client.rs` + 1 `doctor` check |
| **`e2e --self-test` & `py_compile`** | — | — | — | — | `ci.yml:58-66` AND `agent_check.sh:404,537` | Drop duplicate CI & `agent_check` run |

### Critical Coverage Gaps (`P0–P2`)

| Priority | Gap | Impact | Target Remedy |
| --- | --- | --- | --- |
| **P0** | **Zero UI behavioral tests** (`ui/src/`: `4,623` LOC, `58` top-level fns — `39` already pure) | UX v2 PR C introduces 3 tabs, 5-level priority banners, `enabled_models` toggles/`[]` pause, `snapshotEpoch` concurrency, Usage rollups, health-only popover & RAM footer with zero runtime safety net | Extract pure `ui/src/{protocol,state,format}.ts` + add `ui/tests/*.test.ts` via Node 24 `node --test` (`< 0.5s`, `0` new deps) |
| **P0** | **No Rust↔TS wire schema contract test** | Silent field/nullability drift between `companion-ui-mcp` (`SettingsSnapshot`/`UsageSummary`) and `ui/src/protocol.ts` | Golden JSON fixtures in `ui/tests/fixtures/` asserted by Rust `serde_json::to_value` AND parsed by `ui/tests/protocol.test.ts` |
| **P1** | **CI runs 0 E2E tests & 0 Windows jobs** | `.github/workflows/ci.yml` duplicates `agent_check` + 2× `--self-test`, skips real binary E2E, and leaves `cfg(windows)` uncompiled | Run lean E2E (`< 30s`), `npm --prefix ui test`, and `windows-latest` `cargo test --workspace` in `ci.yml` |
| **P1** | **Verified Rust daemon flake (`daemon.rs:1057, 1099`)** | Cold parallel `cargo test` fails intermittently with `TimedOut { last_observed_version: None }` | Switch both tests to `multi_thread` (`worker_threads = 2`) + `yield_now()` + `1000ms` `probe_timeout` |
| **P2** | **UX v2 concurrency & rollup seams** | `<data_dir>/integration.lock` race, `enabled_models: []` pause, SQLite `Today`/`7d`/`30d` UTC rollups | Covered explicitly in [`testing-strategy.md`](testing-strategy.md) §6 before PR A–C land |

---

## 3. Hermeticity Audit (`ISO-1..ISO-11` — 70+ Affected Tests Today)

A full static audit across all 5 Rust crates (`53 .rs`), `e2e/` (`14 .py`), and `scripts/agent_check.sh` on `HEAD dac80c6` identified **11 concrete isolation violations (`ISO-1..ISO-11`)** affecting **70+ existing tests** (`44` E2E + `12` `ui-mcp` + `16` `cli_integration` + cross-cutting gaps) whenever a developer runs tests on a machine with a live proxy on `127.0.0.1:24141`, real `$HOME` shell RCs, or exported proxy/API env vars:

| ID | Sev | Exact Location (`file:line`) | Violation / Host-State Leak | Concrete Remediation |
| --- | --- | --- | --- | --- |
| **ISO-1** | **P0** | `e2e/harness.py:489` + **43 E2E tests** (incl. flagship `T4-01` at `e2e/test_tier4_scenarios.py:52` and `test_tier2_boundaries.py:1547`) | `IsolatedEnv.make_config_dict()` defaults `listen_port=24141`; 43 `start_mcp()` tests omit overriding `listen_port`, sending live `GET /health` + `GET /api/stats` to the developer's running `127.0.0.1:24141` proxy. | Default `make_config_dict(listen_port=None)` to auto-allocate `find_free_port()` and pre-seed isolated `listen_port` before `start_mcp()` (WS2 / Phase 1). |
| **ISO-2** | **P0** | `e2e/test_tier2_rollout.py:1724-1765` (`test_t2_r22_fresh_install_port_24141_collision_falls_back_to_24142_through_24150`) | Directly binds `127.0.0.1:24141` (`probe.bind` at `L1729`, `MockGeminiServer(port=24141)` at `L1735`), skipping when user proxy runs and racing parallel workers — duplicating hermetic `crates/companion-cli/src/install.rs:2918`. | **Delete** `test_t2_r22` from `e2e/` (Single-Owner: `install.rs:2918` already covers fresh-install port fallback hermetically) (WS2 / Phase 1). |
| **ISO-3** | **P0** | `crates/companion-ui-mcp/src/tests.rs`: **12 tests** (`L363, 404, 477, 568, 708, 822, 1116, 1445, 1513, 1694, 1774, 2498`) via `server.rs:141-174` | Call bare `CompanionUiServer::new(cfg_path)` with `integration_options_override: None`, reading the developer's real `$HOME` shell RC dotfiles (`.zlogin`/`.zshrc`/`.bashrc`/fish) & `~/.local/share/agy-companion/env.sh`. | Add `isolated_ui_server(tmp, cfg_path)` defaulting `integration_options_override: Some(...)`, `ui_html_source: Fixed(None)`, `env_api_key_fallback: false`; ban bare `CompanionUiServer::new(` in `tests.rs` (WS4 / Phase 1). |
| **ISO-4** | **P0** | `crates/companion-cli/tests/cli_integration.rs`: **16 of 36 tests** (`L382..1870`, incl. `L585, 612, 629, 654, 926, 1085, 1400, 1797`) | Spawn raw `Command::new(BIN)` / `tokio::process::Command::new(BIN)` bypassing `isolated_cli_command()`, inheriting host `HOME`, `XDG_*`, `GEMINI_API_KEY`, `GOOGLE_GEMINI_BASE_URL`, `AGY_COMPANION_UI_HTML`, `HTTP_PROXY`, `RUST_LOG` (`L926` `save_settings` reads real `$HOME` RCs; `L585/612/629/1400/1797` `doctor` reads ambient env). | Route 100% of `cli_integration.rs` spawns through `isolated_cli_command()` / `isolated_tokio_cli_command()` (WS4 / Phase 1). |
| **ISO-5** | **P1** | `crates/companion-cli/tests/cli_integration.rs:117-142` (`isolated_cli_command`) | Uses an incomplete 15-var `env_remove` denylist missing `AGY_COMPANION_UI_HTML`, `HTTP_PROXY`/`HTTPS_PROXY`/`ALL_PROXY`/`NO_PROXY`, `RUST_LOG`, `APPDATA`, `LOCALAPPDATA`, `USERPROFILE`, `XDG_CACHE_HOME`, `TMPDIR`. | Switch to `cmd.env_clear()` + minimal safe allowlist + isolated temp dirs (matching `e2e/harness.py`) (WS4 / Phase 1). |
| **ISO-6** | **P1** | `crates/companion-cli/tests/cli_integration.rs:654, 926, 1085, 4197` & `e2e/test_tier3_rollout.py:759-778` | **5 orphan-process leaks on panic/timeout:** 4 `ui-mcp` spawns in `cli_integration.rs` omit `ChildGuard`; 3 concurrent `subprocess.Popen` calls in `test_tier3_rollout.py:759-778` omit `ExitStack` / `try..finally`. | Wrap all 4 Rust spawns in `ChildGuard`; register `self.stack.callback(...)` / `try..finally` reaper on `procs` in `test_tier3_rollout.py` (WS2/WS4 / Phase 1). |
| **ISO-7** | **P1** | `crates/companion-ui-mcp/src/tests.rs:2544` & `L1116` | `L2544` leaves `env_api_key_fallback: true`; `L1116` omits `integration_options_override`. | Resolved automatically by `isolated_ui_server()` default helper (WS4 / Phase 1). |
| **ISO-8** | **P1** | `crates/companion-cli/tests/cli_integration.rs:156-158` (`free_loopback_port`) | Allocates test ports in `20_000..=32_000`, overlapping default production port span `24141..=24150`. | Exclude `24140..=24160` in `free_loopback_port()` and bind `127.0.0.1:0` in-process (WS4 / Phase 1). |
| **ISO-9** | **P2** | `e2e/harness.py:454-483`, `e2e/test_self.py:520-544`, `e2e/test_tier2_boundaries.py:2151-2158` | `IsolatedEnv.build_env()` omits `XDG_CACHE_HOME`, `APPDATA`, `LOCALAPPDATA`, `USERPROFILE`; `test_self.py` & `test_tier2_boundaries.py` mutate global `os.environ` in place. | Add `XDG_CACHE_HOME` + Windows env vars to `IsolatedEnv.build_env()`; use `ProcessPoolExecutor` (not threads) and subprocess/`patch.dict` isolation (WS2 / Phase 2). |
| **ISO-10** | **P2** | `scripts/agent_check.sh:568` & 18 `e2e/` `run_cli(...)` calls omitting `env=` | `agent_check.sh` invokes `cargo test` with ambient host env (amplifying `ISO-3`/`ISO-4`); 18 `run_cli` calls omit `env=` (15 static/`agent_check` tests slated for deletion + 3 `agy plugin validate` calls). | Make Rust tests 100% self-hermetic (`ISO-3..5`), pass `env=env.build_env()` to `agy plugin validate`, and add the **5 static hermeticity guards** (`< 0.1s`) to `scripts/agent_check.sh` (WS1/WS5 / Phases 1–3). |
| **ISO-11** | **P2** | `crates/companion-cli/src/daemon.rs:757-762` | Calls ambient `companion_core::default_data_dir()` reading host `XDG_DATA_HOME`/`HOME` (panics if host exports malformed `../bad` `XDG_DATA_HOME`). | Test `resolve_runtime_data_dir` with explicit `usage_dir = Some(...)` or injected default resolver; keep `default_data_dir()` env precedence inside pure `companion-core/src/config.rs` helpers (WS4 / Phase 3). |

**Verified-Clean Suites:** `companion-core` (107 tests, incl. `.env_clear()` shell matrix at `integration.rs:3562`), `companion-providers` (27 tests), `companion-proxy` (77 tests, `0.12s`, `ProxyStateOptions { env_fallback: false, .. }`), `companion-cli/src/{doctor,install,launch,hook_env}.rs` unit tests, `daemon.rs:976` `KillOnDrop`, and `e2e/harness.py` RAII helpers (`E2ETestCase`, `CompanionProcess`, `McpStdioClient`, `reap_pidfile_daemon`).

---

## 4. Fast Execution Bottleneck Audit & Latency SLO Targets

Beyond the **`130.20s` (`65.7%`) recursive `agent_check.sh` tax**, profiling `explorer_survey_1/{timings,e2e_tier_*}.json` reveals five concrete speed bottlenecks across E2E, Rust, and `agent_check.sh`:

1. **Strictly Serial E2E Runner (`e2e/run_e2e.py:395-425`):** Executes all `322` non-`agent_check` tests (`67.99s`) on a single core. Once `ISO-1`/`ISO-2` remove port-`24141` contention, adding pure-stdlib `concurrent.futures.ProcessPoolExecutor` (`--workers 4` default, `--workers 1` for debugging) drops all `322` tests from **`67.99s → 17.00s`**, and drops the pruned ~60-test lean suite from **`28.67s` serial → `7.17s` (`< 12.0s` parallel SLO)**.
2. **`T2-F10` Bash Fork-Bomb (`5.18s` — Single Slowest Non-`agent_check` E2E Test):** `e2e/test_tier2_boundaries.py:1974` runs `plugin/scripts/run-companion.sh:98-116`, which forks external `/usr/bin/dirname` + `/usr/bin/readlink` + subshells 41 times (~164 external execs on macOS). Replacing external `/usr/bin/dirname` with pure-bash parameter expansion (`${target%/*}` / `${target##*/}`) drops `T2-F10` from **`5.18s → ~0.3s`**.
3. **`14` Overlapping Real-Sleep Idle/Adopt E2E Tests (`18.44s` / `27.1%` of Non-`agent_check` E2E):** `T4-R03` (`4.93s`), `T1-R12` (`4.61s` + `2.07s`), `T3-R06` (`2.12s`), `T3-R15` (`1.56s`), `T1-PR1` (`1.04s`), plus parked `0.4–0.5s` sleeps consolidate cleanly to `T4-R03` + `T3-R06` + 1 canary.
4. **Whole-Second `1s` Sleeps in Rust Tests (`~5.0s` of `7.01s` Pure Rust Test Execution):**
   - `crates/companion-providers/src/gemini/tests.rs:1321-1356`: **`1.00s` of `1.01s`** crate total (integer `timeout_secs: u64` → inject `#[cfg(test)] Duration::from_millis(150)`).
   - `crates/companion-ui-mcp/src/tests.rs:968-984`: **`1.01s` of `1.12s`** crate total (`Option<u64>` seconds → `Option<Duration>` `150ms`).
   - `crates/companion-cli/tests/cli_integration.rs:1591` (`~2.7s`: `1.6s` SSE sleep + `1.0s` idle wait), `L2573+2600` (`1.3s`), `L1720` (`~1.1s`), plus 16 copy-pasted symlink subprocesses (`3.56s` total → `< 2.0s`).
5. **`scripts/agent_check.sh:537` `--self-test` Tax (`5.12s` / ~30% of Warm `agent_check.sh`):** Running `e2e/run_e2e.py --self-test` spawns 12 Python subprocesses + `SIGTERM` poll loops on every local pre-commit check (`~16–18s` warm). Dropping `--self-test` from `agent_check.sh` (`D2-A`) plus trimming Rust `1s` sleeps brings warm `agent_check.sh` to **`~8–10s` (`< 12.0s` SLO)**.

### Enforceable Latency SLO Summary (Codified in [`testing-strategy.md`](testing-strategy.md) §8)

| Execution Scope | Current Warm (`dac80c6`) | Hard Warm SLO | Soft Per-Test Cap |
| --- | ---: | ---: | ---: |
| Single Rust crate `--lib` (`cargo test -p <crate> --lib`) | `0.12s – 1.12s` | **`< 1.5s`** | `< 100ms` pure / `< 300ms` in-proc HTTP |
| `companion-cli --test cli_integration` | `3.56s` | **`< 2.5s`** | `< 1.0s` *(1 idle-SSE `< 3.0s`)* |
| UI `npm --prefix ui test` (`node --test`) | *(0 tests)* | **`< 1.0s`** | `< 50ms` |
| Full Rust workspace (`cargo test --workspace`) | `12.65s` | **`< 6.0s`** | See crate caps |
| Pre-commit gate (`./scripts/agent_check.sh`) | `~16–18s` | **`< 12.0s`** | — |
| Lean E2E serial (`--suite all --workers 1`) | `198.21s` (`67.99s` non-`agent_check`) | **`< 30.0s`** | `< 3.0s` *(1 idle-SSE CUJ `< 5.5s`)* |
| Lean E2E parallel default (`--suite all`, `--workers 4`) | *(serial only)* | **`< 12.0s`** | Same per-test cap |

---

## 5. Non-Negotiable S1–S8 Security Red Lines

Naive pruning would introduce real security regressions. All cleanup PRs MUST enforce these four red lines:

### Red Line 1 — Symlink-Ancestor Wiring (`CRITICAL`, S2/S6)
Codebase inspection on `dac80c6` proves that while `config.rs`, `integration.rs`, `usage/sqlite.rs`, `hot_reload.rs`, and `ui_resource.rs` delegate to `companion_core::fs_safe`, **`companion-cli/src/install.rs` (`126` hand-rolled symlink guard sites), `daemon.rs` (`18`), `doctor.rs` (`4`), and `lib.rs` (`5`) still use hand-rolled symlink guards instead of `fs_safe`**. Therefore:
1. **WS4 Prerequisite:** Migrate `install.rs` and `daemon.rs` custom symlink helpers onto `companion_core::fs_safe` BEFORE shrinking `cli_integration` symlink tests.
2. **Minimum Symlink Wiring Matrix (MUST KEEP — 8–10 compact behavioral tests, `~400` LOC replacing `3,536` LOC):**

| Entry Point | Wiring Risk Caught | Minimum Must-Keep Test *(Assert Victim Integrity, NOT Verbatim Warning Prose)* |
| --- | --- | --- |
| `install` (`companion-cli`) | Staging binary/plugin/data through symlinked plugins root, `~/.local/bin`, or runtime data dir | 1 table-driven test → refuses traversal, victim target untouched |
| `uninstall` & `--purge` (`companion-cli`) | `remove_dir_all` following symlinked plugins root, dev-symlink, or config/data dir | 1 table-driven test (`--dry-run` + real mode) → victim sentinel (`DO_NOT_DELETE`) preserved, unrelated safe cleanup completes |
| `integration enable/disable` (`companion-core`) | Mutating symlinked Nix/Stow RC without `--follow-rc-symlink`, or traversing symlinked `.local`/`.local/share` | 1 table-driven test (`~150–200` LOC replacing `1,280` LOC) → symlinked RC returns `manual_required` with target untouched; symlinked ancestor fails closed |
| `serve` (`daemon.rs`, `config.rs`, `hot_reload.rs`) | Starting on symlinked `config.json`, writing `proxy.pid`/`proxy.log` through symlink, or hot-reload swap | 1 combined test → symlinked `config.json`/`proxy.pid`/`proxy.log` refused; hot-reload retains previous active config |
| `usage.db` (`companion-core::usage::sqlite`) | Opening symlink/FIFO `usage_dir`, `usage.db`, or `-wal`/`-shm`/`-journal` | Keep existing compact `sqlite.rs` guard test (`~120` LOC) |
| `ui-mcp` (`ui_resource.rs`, `tools.rs`) | Blocking stdio on FIFO `AGY_COMPANION_UI_HTML`, or `set_launcher_integration` following RC symlink | Keep 2 focused unit tests (non-blocking FIFO rejection; `follow_rc_symlink = false` enforced) |
| `doctor` (`doctor.rs`) | Reporting a symlinked `config.json` as healthy | Keep 1 focused unit test (fail-closed error status) |

### Red Line 2 — Strict Expat SVG Validator (`HIGH`, S5/S8)
Direct inspection of `plugin/assets/logo.svg` (`183` LOC) confirms it uses **only 13 static SVG tags** (`svg, defs, clipPath, rect, path, radialGradient, linearGradient, stop, filter, feGaussianBlur, g, ellipse, circle`) with **0 `<use>`, 0 `href`/`xlink:href`, 0 `style`, 0 `transform`**.
- **Reject** a ~35-LOC regex or `xml.etree.ElementTree` downgrade (`ElementTree` does not block DTD/XXE/PI/CDATA and denylists miss `<animate>`/`<foreignObject>`/namespace smuggling).
- **Safe target (`D3-A`):** Extract a **strictly stronger `~75–85` LOC Expat fail-closed allowlist** (`scripts/check_plugin_logo.py`) preserving path/symlink/size guards (`~20` LOC), Expat DTD/entity/PI/CDATA + `MAX_DEPTH = 64` / `MAX_ELEMENTS = 10_000` guards (`~15` LOC), strict `http://www.w3.org/2000/svg` static geometry/gradient/filter allowlist (`~25` LOC), and attribute/URI checks (`~20` LOC) that **ban `<use>`, `href`/`xlink:href`, `style`, and `on*` outright** (making `<use>` cycle DoS structurally impossible without the 120-LOC DFS cycle detector!). Replace the 48 bash forks (`15.32s`) with **1 fast `< 0.05s` in-process Python test** covering XXE/DOCTYPE, `<script>`/`on*`/`<foreignObject>`, external `url()`/`href`, `<use>`, and symlink/size limits.

### Red Line 3 — E2E Pruning Partition (`333 → ~55–65` Tests, `~4–5k` LOC, `< 30s` Serial / `< 12s` Parallel)

| Partition | Count | Exact Scope |
| --- | ---: | --- |
| **A. DO NOT PRUNE (Red-Line Must-Keep)** | **`45–55`** | **11 Tier-4 CUJs:** `T4-01..04, 07`, `T4-R01..R05`, plus `T4-05`/`T4-R06` live binary S1–S7/zero-leak checks (strip static repo greps). **4 cross-process handoffs:** `T3-05`/`T3-15` combined `run-companion.sh` symlink exec, `T3-R06` idle-to-adopt handoff, `T3-R07` real `zsh` `env.sh` + terminal scrub, `T3-R15` concurrent cold-start → `uninstall --purge`. **18–22 binary S1–S7/stdio/install canaries** (1 per invariant). **8–10 non-Rust runtime shims:** `run-companion.sh`, `dev-install.sh`, npm shim, Python `find_binary()`. |
| **B. Delete Immediately (Day 1: `-130.20s` / `-65.7%`)** | **`64–70`** | All `5` full + `69` synthetic `agent_check.sh` E2E runs (`T1-F12` ×2, `T2-F12` ×4, `T3-11`, `T4-06` `agent_check` step); host-`24141`-binding `test_t2_r22` (`ISO-2`); all UI private-identifier regexes (`T1-F7:1137,1176,1207`, `T1-R19:1888`, `T2-R19:1453,1466`); all Rust `.rs` source greps (`T1-R24:2250,2283`, `T2-R24:1881,1889`, `T3-R14:720`); ~45 static manifest/doc/`GEMINI.md`/S5/`bash -n`/`node --check`/`git ls-files` tests; 320-LOC bash heredoc parser + `SELF-21`. |
| **C. Consolidate / Merge** | **`~205 → 20–25`** | `F2` API key redaction (`13 → 1`), `F6` `/health` & `/api/stats` (`6 → 1`), `R01–R06` CloudCode (`24+ → T4-R02 + 1`), `R07` placeholder key (`6 → 1`), `R11/R12` idle/adopt (`10+ → T4-R03 + T3-R06 + 1`), `R13–R18` `env.sh`/RC/hook-env/launch (`20+ → ~6`). |

### Red Line 4 — `ui/tests/` Outside `ui/src/` (S5 Gate Purity)
Because `scripts/agent_check.sh:576-596` recursively greps `ui/src/` for S5 forbidden sinks, all UI unit/contract tests **must** live in `ui/tests/` (never inside `ui/src/`).

---

## 6. Six Workstreams (`WS1–WS6`) & Quantified Targets

| Workstream | Scope | Baseline (`dac80c6`) | Target | Net Gain |
| --- | --- | --- | --- | --- |
| **WS1 — Kill `agent_check` Recursion, `--self-test` Tax & Quotas** | Delete `SELF-16` quotas, delete 5 full + 69 synthetic `agent_check.sh` E2E runs, remove `--self-test` (`5.12s`) from `agent_check.sh:537` & `ci.yml` (`D2-A`), pass isolated `env=` to `agy plugin validate` (`ISO-10`) | `198.21s` E2E (`130.20s` recursion); `agent_check.sh` `~16–18s` | `< 68s` E2E immediately; `agent_check.sh` **`< 12.0s`** | **`-130.20s` E2E (`-65.7%`)**, **`-5.12s` pre-commit** |
| **WS2 — E2E De-Slop, Hermeticity & Parallel `--workers 4`** | Fix `ISO-1` (`make_config_dict` ephemeral port default), `ISO-2` (delete `test_t2_r22`), `ISO-6` (`ExitStack` in `test_tier3_rollout.py:759`), `ISO-9` (`XDG_CACHE_HOME` + Windows env vars + zero `os.environ` mutation); delete 64–70 static/UI-regex/Rust-grep/heredoc tests; consolidate ~205 T1/T2 duplicates (incl. 14 sleep tests); replace `/usr/bin/dirname` in `run-companion.sh:98-116` with pure bash (`T2-F10` `5.18s → ~0.3s`); add stdlib `ProcessPoolExecutor` `--workers 4` to `e2e/run_e2e.py`; slim `test_self.py` (`1,483L → ~140L`, 3 guardrails) & shift CLI to `--suite cuj\|boundary\|shims\|all` | `359` tests, `17,730` LOC, `203.33s` serial, 44 port-`24141` leaks | `~55–65` E2E + `3` self-tests, `~4–5k` LOC, **`< 30.0s` serial / `< 12.0s` `--workers 4`** (`~7.2s` sim), `0` leaks | **`-80%` count, `-75%` LOC, `-94%` parallel runtime**, 100% hermetic |
| **WS3 — UI Behavioral & Rust↔TS Contract Tests** | Extract pure `ui/src/{protocol,state,format}.ts`; add `ui/tests/*.test.ts` (`node --test`, Node 24) + Rust↔TS golden JSON fixtures; wire `npm --prefix ui test` into Gate 6 | `0` UI tests (`4,623` LOC uncovered) | `20–30` UI tests, **`< 1.0s`** (`~0.3–0.5s`), `0` new deps | Closes **#1 CUJ blind spot** for UX v2 PR C |
| **WS4 — Rust Flake Fix, Hermeticity, `150ms` Timeouts & Symlink De-Bloat** | Fix `daemon.rs:1057,1099` (`multi_thread` + `yield_now` + `1000ms`); fix `ISO-3..ISO-8, ISO-11` (`isolated_ui_server()` for 12 `ui-mcp` tests, `env_clear()` allowlist for all 36 `cli_integration.rs` spawns, `ChildGuard` on 4 spawns, skip `24140..=24160` in `free_loopback_port`, pure `daemon.rs:757`); replace whole-second `1s` test timeouts in `gemini/tests.rs:1321` & `ui-mcp/tests.rs:968` with `#[cfg(test)] 150ms` `Duration`; migrate `install.rs`/`daemon.rs` onto `fs_safe` + `~400L` Minimum Wiring Matrix; drop duplicate `ui-mcp` S5 scan, `doctor` probe copies, `OnceLock`/`Arc::ptr_eq`, `clap` mirrors | `334/335` tests, `27,514` test LOC (`1.57×`), `12.65s` warm, 1 flake, 28+ host leaks | `~210–230` tests, `~14–16k` LOC (`~0.85×`), **`< 6.0s` warm** (`< 1.5s` / crate, `< 2.5s` `cli_integration`), `0` flakes, `0` leaks | **`-42%` Rust test LOC, `-53%` warm runtime**, 100% hermetic |
| **WS5 — SVG Gate Hardening, Static Hermeticity Guards & CI Alignment** | Extract `~75–85` LOC strict Expat allowlist `scripts/check_plugin_logo.py` + 1 `< 0.05s` in-process test; add the **5 automated static hermeticity guards (`< 0.1s`)** to `scripts/agent_check.sh` (no raw `Command::new(BIN)`, no bare `CompanionUiServer::new(`, no `24141` bind/port, no `e2e/` white-box repo reads/`agent_check.sh`, no `run_cli` without `env=`); update `ci.yml` to run all 4 trophy layers once + `windows-latest` `cargo test` | `agent_check.sh` `1,295` LOC (`~16–18s`); `ci.yml` 0 E2E / 0 Windows | `agent_check.sh` `~500` LOC + `80` LOC logo script (**`< 12.0s`**); full 4-layer + Windows CI | **`-55%` gate LOC**, automated hermeticity enforcement, real Windows CI |
| **WS6 — Docs, Rules & Reusable Agent Skill** | Author `testing-strategy.md` (incl. 6-Point Hermeticity Contract & Latency SLOs), `testing-infra-improvement-plan.md`, `.agents/skills/cuj-testing/SKILL.md` (8-point review checklist); update `docs/README.md`, `workflow.md`, surgical `AGENTS.md` | No strategy doc, SLOs, hermeticity contract, or testing skill | Canonical guide + always-on rules + skill | Prevents future AI test bloat, leaks & slowdowns permanently |

**Projected Repo-Wide Net Result:** Test+gate LOC **`46,607 → ~20,000` (`-57%`)** · Warm Rust + UI + full E2E runtime **`~216s → ~15–20s` parallel (`-91%`) / `~30–35s` serial (`-85%`)** · Warm `agent_check.sh` **`~16–18s → < 12.0s`** · Warm `cargo test --workspace` **`12.65s → < 6.0s`** · E2E tests **`359 → ~55–65`** · UI behavioral tests **`0 → 20–30`** · Hermeticity violations **`70+ → 0`**.

---

## 7. Zero-Conflict Phased Execution Roadmap

Sequenced explicitly around merged PR #40 (`f4bd797`) and Conv #2's imminent UX v2 PRs A–D:

```text
Conv #1 (Rust Health):  [PR #36..#39] -> [PR #40 f4bd797 MERGED] ------------------------->
Conv #2 (UX v2):                         [PR A: core/proxy/cli] -> [PR B: usage/mcp] -> [PR C: ui/e2e] -> [PR D: plugin]
Testing Infra:          [Phase 0 NOW: docs/skill, 0 conflicts] -> [Phase 1: E2E de-slop + P0 isolation + flake] -> [Phase 2: ui/tests + --workers 4 + E2E shrink] -> [Phase 3: Rust/150ms/SVG/guards/CI]
```

1. **Phase 0 (Materialized Now — WS6, Zero Code Conflict):** Repo docs (`testing-strategy.md` incl. Hermeticity + Latency SLOs, `testing-infra-improvement-plan.md` incl. `ISO-1..ISO-11` + speed audit), `cuj-testing` skill, `docs/README.md`, `.agents/rules/workflow.md`, and surgical `AGENTS.md` edits (`L12, L18, L23, L87, L111` only — leaving PR #40's `L14, L36, L66` and immediately adjacent `L35` untouched for a 0-conflict 3-way rebase onto `f4bd797`, deferring `L35` & `L36` together to post-rebase Phase 1). Immediately equips UX v2 PR A–D agents.
2. **Phase 1 (PR 1 — WS1 + P0 Hermeticity `ISO-1..ISO-8` + Flake Fix, Before or Alongside UX v2 PR C):** Delete `SELF-16`, `SELF-21`, heredoc parser, `test_t2_r22` (`ISO-2`), 5 full + 69 synthetic `agent_check.sh` E2E runs (`-130.20s`), and all 64–70 static/UI-regex/Rust-grep E2E tests (eliminating the `refreshBtn`/`"cloudcode"` PR C collision and the 3× `_read_rust_module_source` workaround); fix `ISO-1` (`make_config_dict` ephemeral port default), `ISO-3`/`ISO-7` (`isolated_ui_server()` for 12 `ui-mcp` tests), `ISO-4`/`ISO-5`/`ISO-6`/`ISO-8` (`env_clear()` allowlist for all 36 `cli_integration.rs` spawns + `ChildGuard` + `ExitStack` + skip `24140..=24160`); remove `--self-test` (`-5.12s`) from `agent_check.sh:537` & `ci.yml`; fix `daemon.rs:1057,1099` flake; slim `test_self.py` to 3 guardrails; fix stale `README.md` `320` count & `AGENTS.md` post-rebase `L35`/`L36` counts.
3. **Phase 2 (PR 2 — WS3 + Remainder of WS2 + Parallel E2E `--workers 4`, Alongside/After UX v2 PR C):** Land `ui/tests/*.test.ts` (`node --test`, `< 1.0s`) + Rust↔TS golden JSON fixtures + Gate 6 wiring; fix `ISO-9`/`ISO-10` (`XDG_CACHE_HOME` + Windows env vars in `IsolatedEnv`, isolated `env=` on `agy plugin validate`); optimize `run-companion.sh:98-116` with pure bash (`T2-F10` `5.18s → ~0.3s`); add stdlib `ProcessPoolExecutor` `--workers 4` to `e2e/run_e2e.py`; consolidate quota-padded T1/T2 binary & sleep E2E tests to `~55–65` CUJ/boundary/shim tests (`< 30.0s` serial / `< 12.0s` parallel) and update `e2e/README.md`.
4. **Phase 3 (PR 3 — WS4 Rust Consolidation + `150ms` Timeouts + WS5 SVG/Hermeticity Guards & CI Alignment):** Replace `1s` test timeouts in `gemini/tests.rs:1321` & `ui-mcp/tests.rs:968` with `#[cfg(test)] 150ms` `Duration` (`cargo test --workspace < 6.0s`); fix `ISO-11` (`daemon.rs:757`); migrate `install.rs`/`daemon.rs` symlink guards to `fs_safe` and collapse `integration/tests.rs` & `cli_install_symlink_safety.rs` to the ~400-LOC Minimum Wiring Matrix; drop implementation-detail Rust tests; deploy `scripts/check_plugin_logo.py` (`~75–85` LOC Expat allowlist + 1 `< 0.05s` test) and the **5 automated static hermeticity guards** (`< 0.1s`) in `scripts/agent_check.sh`; verify full Hostile Developer Shell smoke check; and align `.github/workflows/ci.yml` (all 4 trophy layers + `windows-latest`).

---

## 8. Sign-Off Decisions (`D1–D4`)

| ID | Decision | Options & Trade-Offs | Recommended Default |
| --- | --- | --- | --- |
| **`D1`** | **UI Behavioral Testing Stack** *(for UX v2 PR C)* | **D1-A:** Zero-dep Node 24 `node --test tests/*.test.ts` in `ui/tests/` (outside `ui/src/`) on extracted pure `ui/src/{protocol,state,format}.ts` + Rust↔TS golden JSON fixtures (`< 0.5s`, `0` deps, `0` Vite `8.3.8` / TS `7.0.2` peer-dep risk; local Node `v24.16.0` & `ci.yml:22` already run Node 24).<br>**D1-B:** `node --test` + standalone `happy-dom` (`1` devDep, no `vitest`, `~1–2s`, adds real DOM click/ARIA/popover tests).<br>**D1-C:** `vitest` + `happy-dom` (~50–100 transitive deps, Vite 8 / TS 7 peer-dep risk). | **`D1-A`** *(with `D1-B` as clean 1-dep upgrade if DOM regressions appear)* |
| **`D2`** | **E2E Runner Interface & `test_self.py`** | **D2-A:** Shift primary CLI to `--suite cuj\|boundary\|shims\|all` (keep `--tier` as deprecated Phase-1 alias); delete `SELF-16` & `SELF-21`; slim `test_self.py` (`1,483L → ~140L`, 3 guardrails: `IsolatedEnv`, `PidfileReaper`, `McpStdioClient`); remove `--self-test` from `agent_check.sh` & `ci.yml` (`-5.12s` local / `-10.24s` CI).<br>**D2-B:** Keep `--tier 1..4` names + slim `--self-test` in `agent_check.sh` only (still delete `SELF-16` & `SELF-21`). | **`D2-A`** |
| **`D3`** | **`scripts/agent_check.sh` 608-LOC SVG Validator** | **D3-A:** Replace with strictly stronger `~75–85` LOC Expat fail-closed allowlist (`scripts/check_plugin_logo.py`, banning `<use>`/`href`/`style`/`on*`/DTD/XXE/PI/CDATA) + `1` `< 0.05s` in-process Python test (replacing 48 bash forks / `15.32s`).<br>**D3-B:** Move existing 608-LOC parser into `scripts/check_plugin_logo.py`, deleting only the 48-fork bash test (`-15.32s`). | **`D3-A`** |
| **`D4`** | **Phase 1 Pruning Timing vs UX v2 PR A–D** | **D4-A:** Land Phase 0 (docs + skill) now; land Phase 1 immediately before or alongside UX v2 PR C so PR C never hits `refreshBtn`/`"cloudcode"` collisions.<br>**D4-B:** Defer Phase 1 until after all UX v2 PRs A–D merge (PR C patches/deletes the 2 colliding UI regex tests manually). | **`D4-A`** |
