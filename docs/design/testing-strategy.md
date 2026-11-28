# Antigravity Companion — Testing Strategy & CUJ Verification Guide

> **Status:** Canonical evergreen testing contract for `agy-companion`.
> **Companion Skill:** [`.agents/skills/cuj-testing/SKILL.md`](../../.agents/skills/cuj-testing/SKILL.md) (load whenever designing, writing, reviewing, or refactoring tests).
> **Completed Overhaul Record:** [`testing-infra-improvement-plan.md`](testing-infra-improvement-plan.md).

---

## 1. Core Philosophy — The CUJ-First Testing Trophy

Tests exist to guarantee **critical user journeys (CUJs)**, **public boundary contracts**, and **security invariants (`S1–S8`)** — not to pad counts or mirror private implementation details. Every test must earn its maintenance cost and wall-clock budget.

```text
               /\
              /  \      Layer 4: Lean Opaque-Box Binary E2E (e2e/)
             /CUJs\     Real multi-process lifecycles + shell/npm/Python shims
            /------\    Zero repo-source/doc greps, zero agent_check recursion
           /        \
          /  UI State \ Layer 3: UI Behavioral & Contract Tests (ui/tests/, node --test)
         /  & Contracts\ Pure protocol/state/format logic + Rust<->TS golden JSON
        /----------------\
       /   Rust Domain &  \ Layer 2: Rust Unit & Boundary Integration (cargo test)
      /  Interface Surface \ S1-S4/S6-S7 primitives, proxy HTTP, CloudCode, Sidecar UI API, CLI
     /----------------------\
    /  scripts/agent_check.sh\ Layer 1: Static Hygiene, Secrets, Versions, SVG & S5 Gate
   /__________________________\ (Single source of truth for static checks; never run by tests)
```

### The 5 Iron Rules

1. **Single-Owner (One Home per Invariant):** Every invariant or behavior has exactly **one** authoritative testing layer. Never re-test static manifests, `S5` sink scans, or `fs_safe` path matrices in downstream layers.
2. **Opaque-Box E2E Means Opaque-Box:** Tests in `e2e/` interact **only** with the compiled `agy-companion` binary, HTTP wires, isolated XDG/`$HOME` filesystem state, mock upstreams, and real shell/packaging shims. Reading `crates/`, `ui/src/`, `README.md`, `AGENTS.md`, or `docs/` inside `e2e/` is prohibited.
3. **Boundary-Heavy, Internal-Light:** Test public interfaces (HTTP routes, Sidecar `/_api/*` endpoints, CLI subcommands/exit codes/JSON, on-disk schemas, UI state transitions) thoroughly. Keep internal unit tests strictly for non-trivial pure logic and security-critical primitives (`fs_safe`, URL/SSRF parser, SSE tee, CloudCode routing precedence, SQLite rollups, redaction).
4. **Table-Driven Over Copy-Paste:** Parameterize boundary/negative matrices (invalid hosts, URLs, model IDs, symlink chains, error codes) into compact table-driven loops rather than repeating near-identical 80-line test functions.
5. **Zero Hardcoded Test Counts in Docs:** `AGENTS.md`, `README.md`, `e2e/README.md`, and evergreen design specs describe *what* each gate verifies and *how* to run it — never brittle test counts or bundle byte counts that drift on every PR.

---

## 2. Single-Owner Layer Matrix

| Concern / Invariant | Authoritative Owning Layer | Target Runtime | Never Duplicate Here |
| --- | --- | --- | --- |
| Governance (no `GEMINI.md`), tracked-file hygiene, `AIza...` secret scan, zero AI attribution | **Layer 1:** `scripts/check.py --profile repo` (via `scripts/agent_check.sh`) | `< 1s` | `e2e/`, Rust tests |
| Shell syntax (`bash -n`), `-x` bits, `dev-install.sh` safety, 4-way version sync, npm/Python syntax | **Layer 1:** `scripts/check.py --profile repo` (via `scripts/agent_check.sh`) | `< 2s` | `e2e/` static readers |
| `ui/src/` & `ui/dist/index.html` S5 XSS/CSP sink scan + `< 512,000` byte bundle limit + 5 static hermeticity guards | **Layer 1:** `scripts/check.py --profile repo` (via `scripts/agent_check.sh`) | `< 1s` | `companion-ui`, `e2e/` |
| Plugin JSON manifests, strict fail-closed Expat SVG logo allowlist (`scripts/check_plugin_logo.py` + `scripts/test_check_plugin_logo.py`), `agy plugin validate --strict ./plugin` | **Layer 1:** `scripts/check.py` (`repo` + `os`, via `scripts/agent_check.sh`) | `< 1s` | Multi-fork bash `e2e/` loops |
| Pure domain & security primitives (`fs_safe`, `redaction`, `validation`, `models`, XDG/Windows path precedence, SQLite WAL `0600` & `usage/query.rs` range rollups, `integration/{auto_enable,lock}`) | **Layer 2:** `cargo test -p companion-core` (`--lib`) | `< 1.5s` | `e2e/` subprocess loops, consumer crate copy-paste |
| Upstream Gemini HTTP adapter (`S2`/`S3`/`S4` raw zero-compression, `x-goog-api-key`, no redirects) & `LoopbackProxyClient` | **Layer 2:** `cargo test -p companion-providers` (`--lib`) | `< 1.5s` | `doctor.rs` probe copies, `e2e/` T1/T2 |
| Loopback proxy HTTP wire (`S1`/`S3`/`S4`/`S7`), SSE `usageMetadata` tee, `thoughtSignature` retry, CloudCode auto/403/LRU/slots, `rss.rs` process RAM telemetry, hot-reload, idle shutdown | **Layer 2:** `cargo test -p companion-proxy` (`--lib`) | `< 1.5s` | `e2e/` CloudCode/header duplication |
| Sidecar UI API handler contracts (`get_settings`, `save_settings` `deny_unknown_fields`, `test_connection`, `set_launcher_integration`, `get_usage`), concurrency locks, Rust↔TS golden JSON fixtures | **Layer 2:** `cargo test -p companion-ui` (`--lib`) | `< 1.5s` | `e2e/` key-length / redaction matrices |
| CLI subcommands (`install`/`uninstall`, `integration`, `hook-env`, `serve`, `doctor`, `models`, `launch`), Minimum Symlink Wiring Matrix, Windows `cfg(windows)` parity | **Layer 2:** `cargo test -p companion-cli` (`--lib` & unified `cli_integration` + submodules) + Windows CI | `< 1.5s` lib / `< 2.5s` int | `e2e/` `.rs` source grepping |
| UI wire parsing (`SettingsSnapshot`, `UsageSummary`, `TestConnectionPayload`), banner priority, Day-0/Day-N tab routing, `enabled_models` toggles/`[]` pause, `snapshotEpoch` & write lockout, polling backoff, Usage `$`/`Tokens`, footer RAM & health-only popover, Light/Dark WCAG contrast, open-source copy | **Layer 3:** `npm --prefix ui test` (`node --test tests/*.test.ts` in **`ui/tests/`**) | `< 1.0s` | `e2e/` regexes against `ui/src/main.ts` |
| Multi-process CUJ lifecycles (core + UX v2), real binary `S1–S7` wire canaries, real `sh`/`zsh` `env.sh` execution, `run-companion.sh`, `dev-install.sh`, npm & Python runtime shims, and lean headless Chrome CDP UI smoke (`--suite ui-smoke`) | **Layer 4:** `e2e/` (`--suite cuj\|boundary\|shims\|ui-smoke\|all`, `--workers N`) | `< 12.0s` (`--workers 4`) / `< 30.0s` serial | Static repo lints, pure function fuzzing |

### Non-Negotiable Security Red Lines (`S1–S8`)

- **Symlink-Ancestor Wiring Rule (`S2`/`S6`):** Exhaustive symlink/FIFO/TOCTOU matrix tests belong **once** in `companion-core::fs_safe`. Every file-writing or deleting entry point must delegate to `companion_core::fs_safe` **and** retain a compact behavioral wiring test asserting **post-conditions** (victim sentinel file untouched, fail-closed error/status returned) — never verbatim warning prose. Never delete a consumer's wiring test until its symlink guards delegate to `fs_safe` and at least one behavioral wiring test remains.

| Entry Point | Wiring Risk Caught | Minimum Must-Keep Behavioral Test |
| --- | --- | --- |
| `install` (`companion-cli`) | Staging binary/plugin/data through symlinked plugins root, `bin`, or runtime data dir | 1 table-driven test (symlinked plugins root, `~/.local/bin`, runtime data dir) → refuses traversal, victim untouched |
| `uninstall` & `--purge` (`companion-cli`) | `remove_dir_all` following symlinked plugins root, dev-symlink, or config/data dir | 1 table-driven test (`--dry-run` + real mode) → victim sentinel (`DO_NOT_DELETE`) preserved, unrelated safe cleanup succeeds |
| `integration enable/disable` (`companion-core`) | Mutating symlinked Nix/Stow RC without `--follow-rc-symlink`, or traversing symlinked `.local`/`.local/share` | 1 table-driven test → symlinked RC returns `manual_required` with target untouched; symlinked ancestor fails closed |
| `serve` (`daemon`, `config`, `auth`, `hot_reload`) | Starting on symlinked `config.json`, `auth/`, or `auth/gemini.json`, writing `proxy.pid`/`proxy.log` through symlink, or hot-reload symlink swap | 1 combined test → symlinked `config.json`/`auth/`/`auth/gemini.json`/`proxy.pid`/`proxy.log` refused; hot-reload retains previous active config/auth |
| `usage.db` (`companion-core::usage::sqlite`) | Opening symlink/FIFO `usage_dir`, `usage.db`, or `-wal`/`-shm`/`-journal` | Compact `sqlite` guard test verifying `0600` sidecars and fail-closed rejection |
| `sidecar_ui` (`ui_resource`, `tools`) | Blocking on FIFO `AGY_COMPANION_UI_HTML`, or `set_launcher_integration` following RC symlink | 2 focused tests (non-blocking FIFO rejection; `follow_rc_symlink = false` enforced) |
| `doctor` (`companion-cli`) | Reporting a symlinked `config.json` as healthy | 1 focused unit test verifying fail-closed error status |

- **SVG Logo Validation Rule (`S5`/`S8`):** `plugin/assets/logo.svg` is a static vector graphic using only static SVG geometry/gradient/filter tags (`0 <use>`, `0 href`/`xlink:href`, `0 style`, `0 transform`). Validate it in Gate 7 with the fail-closed Expat XML allowlist (`scripts/check_plugin_logo.py`, banning `<use>`, `href`/`xlink:href`, `style`, `on*`, DTD/XXE/PI/CDATA, and intermediate manifest symlinks/FIFOs outright) — never weaken to regexes or `xml.etree.ElementTree`, and verify it with the fast in-process Python test suite (`scripts/test_check_plugin_logo.py`, `< 0.05s`) rather than spawning dozens of bash `agent_check.sh` subprocesses.

---

## 3. Canonical CUJ 0–6 & Boundary Interface Contract

### A. Product CUJs (End-to-End User Journeys)

| CUJ | User Goal | Primary Boundary Path | Must Verify at Boundary | Do NOT Over-Test Internally |
| --- | --- | --- | --- | --- |
| **CUJ 0 — Install, Upgrade & Clean Uninstall** | Install plugin + binary + launcher hook in one command, upgrade cleanly, uninstall safely | `install` / `uninstall [--purge]` → `plugin/` + `bin/` + `env.sh` + RC marker + PID | Fresh install (`24141..24150` port probe), upgrade version-skew proxy handoff, dev-symlink protection, race-free uninstall order (plugin removed before proxy stop), preserve vs `--purge`, never mutating `~/.gemini/config/config.json` | Dozens of repetitive symlink-chain subprocesses once `fs_safe` + compact wiring matrix cover it |
| **CUJ 1 — Day-0 First Run & Dock Auto-Start** | Open Antigravity from Dock/Spotlight, paste Gemini key, immediately get default Gemini 4 `(AI Studio)` models without touching native plans | `env.sh` → `hook-env --ensure-running` → `serve --idle-shutdown-secs` → `serve --adopt-running` → `get_settings`/`save_settings` → `/v1internal:fetchAvailableModels` | Day-0 untouched native catalog (zero `(AI Studio)` models, placeholder key quarantined); first-run auto-enable (`None → Some(true)`, honoring symlinked RC & `Some(false)` opt-out); save key → redacted response + `0600` atomic write + hot-reload; `JETSKI_MCP_ENV_RESTORE_*` & terminal scrub; `hook-env` fail-open exit `0` | Line-by-line shell formatting strings (execute real `sh`/`zsh` instead) |
| **CUJ 2 — Model Selection & Gemini Execution** | Toggle enabled `(AI Studio)` models (`4 Carbon`, `4 Neon`, `4 Hydrogen` default) and call Gemini with implicit caching preserved | `save_settings({ enabled_models })` → `/v1internal:fetchAvailableModels` + `/v1beta/models*` + `agy-companion models --json` | `enabled_models` filtering (incl. `[]` 0-models pause), deterministic placeholder slots (`M334..=M365`) + `XHigh`, byte-for-byte uncompressed SSE pass-through (`?key=` stripped, `x-goog-api-key` set), foreign `thoughtSignature` strip-retry, actionable `400` on unsupported non-3.8+/4+ models | Internal `GeminiModelInfo` struct layout or sort tie-breaker helpers |
| **CUJ 3 — Live Session Monitoring & Health** | Monitor active model, cache hit rate, call health/breaks, session cost, and proxy health/RAM while coding | SSE `usageMetadata` tee → `GET /api/stats` (`proxy_rss_bytes`), `GET /health` → `get_settings` (`SettingsSnapshot`) → UI **Monitor** tab & `● Live` footer/popover | Streaming & unary `usageMetadata` into rolling stats + recent-calls ring buffer + `proxy_rss_bytes` on `GET /api/stats` and `SettingsSnapshot`, `/health` skew detection, Day-N Monitor landing, visibility-aware polling + `snapshotEpoch` stale-drop + write lockout, `doctor --json` | Regex-matching private `ui/src/main.ts` variable names; CSS pixel metrics |
| **CUJ 4 — Multi-Day Usage & Cost Analytics** | Inspect spend and token trends across `Today` / `7 days` / `30 days` by Model or Provider | `SqliteUsageSink` (`0600` WAL `usage.db`) → SQL range rollups (`get_usage`) → UI **Usage** tab | Durable WAL writes across restarts, zero prompt/response body text stored, accurate UTC day/model/provider rollups & range filters, symlink/FIFO rejection on `usage.db` & sidecars, UI `$` vs `Tokens` & `Model` vs `Provider` state | SQLite internal page layout beyond schema version & `0600` sidecars |
| **CUJ 5 — Multi-Backend Expansion** | Add future non-Google providers cleanly without redesigning config or UI | Nested v1 `config.json` `providers.*` → `ProviderAdapter` → UI `.provider-stack` | Nested v1 config round-trip & unknown-field rejection, `ProviderAdapter` auth/error contract, SSRF URL validation (`https://` remote, `http://` loopback-only, no redirects), UI dynamic provider rendering | Speculative mocks for unimplemented future providers |
| **CUJ 6 — Safe Opt-Out, Recovery & Plan Override** | Disable/re-enable proxy hook, handle Nix/Stow symlinked dotfiles, or override Antigravity plan (`Auto`/`Personal`/`Organization`) | `set_launcher_integration` / `integration` / `save_settings({ account_plan })` → CloudCode routing | Opt-out persists `Some(false)` permanently; symlinked RC returns `manual_required` without mutating target; CloudCode `auto`/`personal`/`org` routing & metadata-only fallback (never retry generation RPCs); priority banner queue ordering | Re-testing the full CloudCode routing matrix in Python E2E |

### B. The 6 Public Boundary Interfaces

| # | Interface | Contract Surface | Authoritative Layer + E2E Canary |
| --- | --- | --- | --- |
| 1 | **Proxy HTTP Wire API** | `/health`, `/api/stats`, `/v1beta/models*`, `/v1internal:fetchAvailableModels`, `/v1internal:*` (`S1`/`S3`/`S4`/`S7`) | `companion-proxy` loopback tests + 1-per-invariant binary E2E canaries |
| 2 | **Sidecar Web UI HTTP API** | `GET /` (`AGY_COMPANION_UI_HTML`), `X-Sidecar-Token`-gated `POST /_api/{get_settings,save_settings,test_connection,set_launcher_integration,get_usage}` | `companion-ui` handler/lock/golden-fixture tests + `sidecar_ui` token/CSP/method E2E |
| 3 | **CLI Subcommands** | `install`, `uninstall`, `integration`, `hook-env`, `serve`, `launch`, `doctor`, `models`, `--version` | `companion-cli` integration suites + cross-process daemon/install E2E |
| 4 | **Persistence & Host Contracts** | `config.json` (nested v1 non-secret + XDG/Windows), sibling `0700`/`0600` `auth/gemini.json` (`AuthStore`), `usage.db` (WAL `0600`), `env.sh` (zero-`eval`), RC markers, `proxy.pid`, `integration.lock` | `companion-core` unit tests + real `sh`/`zsh` execution of `env.sh` |
| 5 | **Embedded UI** | S5 CSP/XSS/bundle gate + modular `ui/src/` state/protocol/pricing/view behavior + lean multi-viewport CDP UI smoke | `agent_check.sh` Gate 6b (S5) + `ui/tests/*.test.ts` (`node --test`) + `--suite ui-smoke` |
| 6 | **Plugin Bundle & Packaging Shims** | `plugin/*`, `plugin/scripts/run-companion.sh`, `scripts/dev-install.sh`, `packages/npm/*`, `packages/python/*` | `agent_check.sh` (manifests/versions/SVG/`agy plugin validate`) + runtime shim E2E |

---

## 4. Anti-Slop Catalog (`Before → After` from This Repo)

### Pattern 1 — No Recursive Gate Invocation Inside Tests
- **Before (Slop):** `e2e/test_tier1_features.py:1686` and `:1696` (plus `T2-F12`, `T3-11`, `T4-06`) spawn `./scripts/agent_check.sh` as a subprocess, re-running `cargo fmt`, `clippy`, `cargo test`, and `--self-test` five full times plus dozens of synthetic forks inside E2E.
- **After (CUJ Boundary):** `./scripts/agent_check.sh` runs **once** as Layer 1. `e2e/` never invokes `agent_check.sh`.

### Pattern 2 — No Hardcoded Test-Count Quotas
- **Before (Slop):** `e2e/test_self.py:614-640` (`SELF-16`) enforces `T1 >= 72`, `T2 >= 80`, and `>= 6` T1 + `>= 6` T2 tests per feature `F1..F12`, coercing filler tests like two separate checks for one `.npmrc` line or three `serve` spawns for two HTTP headers.
- **After (CUJ Boundary):** Zero count-floor assertions anywhere. Runner self-tests check only safety guardrails (`IsolatedEnv`, `PidfileReaper`, `SidecarUiClient`) and `--list` uniqueness.

### Pattern 3 — No White-Box Source/Doc Grepping in `e2e/`
- **Before (Slop):**
  ```python
  # e2e/test_tier1_features.py:1213-1226 — breaks whenever UI controls are renamed/removed
  self.assertRegex(src, r"refs\.refreshBtn\.disabled\s*=\s*!canCall\s*\|\|\s*state\.refreshing\s*\|\|\s*anyWrite")
  # e2e/test_tier1_rollout.py:2252-2258 — breaks whenever Rust modules are split into directories
  install_rs = (self.repo_root / "crates" / "companion-cli" / "src" / "install.rs").read_text()
  self.assertIn("cfg(windows)", install_rs)
  ```
- **After (CUJ Boundary):**
  ```ts
  // ui/tests/state_and_theme.test.ts — tests real write-lockout & stale-epoch behavior via `node --test`
  import test from "node:test";
  import assert from "node:assert/strict";
  import { shouldApplyRefresh } from "../src/state.ts";

  test("drops stale get_settings response after snapshotEpoch advances or while writing", () => {
    assert.equal(shouldApplyRefresh(3, 4, false), false);
    assert.equal(shouldApplyRefresh(4, 4, true), false);
    assert.equal(shouldApplyRefresh(4, 4, false), true);
  });
  ```
  And Windows `cfg(windows)` behavior is verified by `cargo test --workspace` on `windows-latest` CI (never by grepping `.rs` files on macOS).

### Pattern 4 — No Multi-Layer Primitive Duplication or Prose-Coupled Assertions
- **Before (Slop):** Copy-pasting 16+ 140-line symlink-ancestor subprocess tests asserting exact stderr warning prose (`"refusing to traverse symlinked/non-directory runtime data directory"`).
- **After (CUJ Boundary):** Exhaustive matrix lives once in `companion-core::fs_safe`. Consumer suites (`install`, `uninstall`, `integration`, `serve`) keep 1–2 table-driven wiring tests asserting behavioral post-conditions:
  ```rust
  assert!(victim_sentinel.exists(), "uninstall must never traverse symlinked root");
  assert_eq!(fs::read_to_string(&victim_sentinel)?, "DO_NOT_DELETE");
  assert!(!report.warnings.is_empty()); // structural signal, not brittle prose
  ```

### Pattern 5 — No Subprocess Spawns for Pure Functions
- **Before (Slop):** Spawning a companion subprocess 13 separate times in `e2e/` to test `mask_api_key` length edge cases (`len = 1, 11, 12, 21, 64, ""`).
- **After (CUJ Boundary):** One table-driven test in `companion-core::redaction` (`< 1ms`) covers every length boundary; `e2e/` verifies redaction once inside a full CUJ flow.

### Pattern 6 — No Implementation-Detail, Meta-Slop, or Single-Thread Flaky Async Tests
- **Before (Slop):** Asserting `OnceLock` allocation identity (`std::ptr::eq`), `Arc::ptr_eq`, `clap` derive-macro mirrors, a 200-LOC custom bash heredoc parser plus a 119-LOC unit test for that parser, or spawning `axum::serve` inside a single-threaded `#[tokio::test]` with a `200ms` `probe_timeout` (starves the accept loop under parallel `cargo test` CPU load, as seen in `crates/companion-cli/src/daemon.rs:1057` and `:1099`).
- **After (CUJ Boundary):** Assert observable behavior only, and for any Tokio test that spawns an in-process HTTP server alongside a client probe, always use:
  ```rust
  #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
  async fn wait_for_daemon_healthy_tracks_version_deterministically() {
      let handle = tokio::spawn(async move { let _ = axum::serve(listener, router).await; });
      tokio::task::yield_now().await;
      // probe_timeout >= 1000ms avoids cold-CPU starvation while adding 0ms on the happy path
  }
  ```

---

## 5. UI & Rust↔TS Contract Testing Standard

### Directory Layout & S5 Gate Boundary

`scripts/agent_check.sh` Gate 6b recursively scans **every file under `ui/src/`** for forbidden S5 sinks (`innerHTML`, `confirm(`, `localStorage`, unauthorized `fetch(` outside `ui/src/transport.ts`, etc.), and Vite bundles `ui/src/`. Therefore **all UI tests MUST live in `ui/tests/` (outside `ui/src/`)**:

```text
ui/
├── src/                      # S5-scanned & Vite-bundled production code only (modular UX v2)
│   ├── types.ts              # Wire DTOs + parseSettingsSnapshot / parseUsageSummary / validators
│   ├── state.ts              # Pure helpers: chooseInitialTab, shouldApplyRefresh, nextPollDelayMs,
│   │                         #   toggleEnabledModel, needsSettingsAttention
│   ├── pricing.ts            # Token cost estimation & savings math
│   ├── dom.ts                # Safe DOM helpers + token/cost/RAM/time/platform formatters
│   ├── banners.ts            # 5-level priority banner queue
│   ├── footer.ts             # Health-only `● Live` popover & right-side `XX MB RAM` telemetry
│   ├── monitor.ts            # Monitor tab view (empty states & active mixed built-in + BYOM calls)
│   ├── usage.ts              # Usage tab view (Today / 7d / 30d, $ / Tokens, Model / Provider)
│   ├── settings.ts           # Settings tab view (write-only API key, enabled_models, Advanced)
│   ├── transport.ts          # Same-origin `/_api/*` fetch transport with `X-Sidecar-Token`
│   ├── main.ts               # Thin DOM mount + Sidecar Web UI transport wiring
│   └── style.css             # Dark & Light theme tokens (WCAG AA contrast)
└── tests/                    # OUTSIDE ui/src/ — never trips S5 scan, never bundled by Vite
    ├── fake_dom.ts           # Zero-dep fake DOM + timer harness for view tests
    ├── fixtures/
    │   ├── settings_snapshot_active.json       # Verified by companion-ui Rust test
    │   ├── settings_snapshot_day0_offline.json # Verified by companion-ui Rust test
    │   ├── usage_summary_7d.json               # Verified by companion-ui Rust test
    │   └── test_connection_ok.json             # Verified by companion-ui Rust test
    ├── protocol.test.ts            # Parses Rust golden fixtures + malformed payload fallbacks
    ├── pricing_and_format.test.ts  # Cost/savings math + pure formatters
    ├── state_and_theme.test.ts     # State transitions, Light/Dark WCAG contrast, UX v2 copy hygiene
    └── views.test.ts               # Banner queue, health-only footer popover, Monitor/Usage/Settings views
```

### Runtime & TypeScript Rules (Zero-Dep Node 24 Default)

- Run via `npm --prefix ui test` executing `node --test tests/*.test.ts` on Node 24+ (`< 1.0s`, zero new npm dependencies, zero Vite 8 / TypeScript 7 peer-dependency coupling).
- Because Node 24's built-in TypeScript type-stripping erases types without emitting runtime code, pure modules imported by `ui/tests/` must:
  1. Use `import type { ... }` for type-only imports (compatible with `isolatedModules: true` and `allowImportingTsExtensions: true` already enabled in `ui/tsconfig.json`).
  2. Avoid TypeScript `enum`, `namespace`, and constructor parameter properties (use string union types and explicit fields).
- View interaction tests run against zero-dependency `ui/tests/fake_dom.ts`, complemented by lean multi-viewport headless Chrome CDP rendering smoke checks (`python3 -B e2e/run_e2e.py --suite ui-smoke --repo-root . -v`).

### Rust↔TS Golden JSON Snapshot Contract

To prevent silent wire drift between `companion-ui` and `ui/src/types.ts`:
1. `golden_wire_fixtures_match_rust_serialization_verbatim` in `crates/companion-ui/src/tests.rs` constructs canonical `SettingsSnapshot`, `UsageSummary`, and `TestConnectionPayload` structs, serializes them with `serde_json::to_value`, and asserts exact equality against all 4 fixtures in `ui/tests/fixtures/*.json`.
2. `ui/tests/protocol.test.ts` loads those exact fixture files and passes them through `parseSettingsSnapshot` / `parseUsageSummary` / tool extractors, asserting every field used by the UI parses with zero fallback errors.

```rust
// crates/companion-ui/src/tests.rs — verifies wire schema matches shared TS fixtures
#[test]
fn golden_wire_fixtures_match_rust_serialization_verbatim() {
    let actual = serde_json::to_value(sample_settings_snapshot_active()).expect("serialize");
    let expected: serde_json::Value = serde_json::from_str(include_str!(
        "../../../ui/tests/fixtures/settings_snapshot_active.json"
    ))
    .expect("golden json");
    assert_eq!(actual, expected);
}
```

```ts
// ui/tests/protocol.test.ts — parses the exact Rust-verified golden payload
import test from "node:test";
import assert from "node:assert/strict";
import { readFileSync } from "node:fs";
import { parseSettingsSnapshot } from "../src/types.ts";

test("parseSettingsSnapshot accepts canonical active Rust golden fixture", () => {
  const raw = JSON.parse(readFileSync(new URL("./fixtures/settings_snapshot_active.json", import.meta.url), "utf8"));
  const parsed = parseSettingsSnapshot(raw);
  assert.ok(parsed, "golden snapshot must parse without fallback");
  assert.equal(parsed.config.providers.gemini.api_key_configured, true);
});
```

---

## 6. UX v2 Implemented Testing Contract

UX v2 (merged onto `main` across PRs #40–#46) enforces Single-Owner test coverage across all four layers:

| Slice | Layer & Location | Enforced High-Signal Tests | Explicit Do-Not-Do |
| --- | --- | --- | --- |
| **Core, Proxy & CLI** (`core`, `proxy`, `cli`) | Rust `--lib` & `cli_integration` | `enabled_models` (default `4 Carbon`, `4 Neon`, `4 Hydrogen`; subset filter; **`enabled_models: []` 0-models pause** injecting zero `(AI Studio)` models while keeping native catalog untouched). `launcher_auto_start` 4-state table (`None` w/ `--auto-enable-launcher` vs standalone `serve`, `Some(false)` permanent opt-out, `Some(true)` idempotent, symlinked RC `manual_required` persisting `Some(true)` once without mutating target). **`<data_dir>/integration.lock`** concurrency test proving concurrent `serve --auto-enable-launcher` and `sidecar_ui` serialize cleanly on `0600` lockfile with zero config corruption or duplicate RC markers. | Do not add static `.rs` grep tests or duplicate `fs_safe` matrices. |
| **Usage, RAM RSS & Sidecar API** (`core`, `proxy`, `companion-ui`) | Rust `--lib` + golden JSON fixtures | SQLite `usage.db` range rollups (`Today`/`7d`/`30d`: empty DB, UTC day-boundary edges, multi-model/provider grouping, cached-token savings math, zero-token/malformed rows). Proxy session ring-buffer eviction, cache-break detection, and `proxy_rss_bytes` on `GET /api/stats` and `SettingsSnapshot` (with graceful fallback). Semantic `account_plan` (`auto \| personal \| organization`) mapping with zero `*-pa.googleapis.com` or internal codename leaks. Bidirectional Rust↔TS parity against all 4 `ui/tests/fixtures/*.json` files. | Do not spawn Python E2E subprocesses to test SQLite rollup math. |
| **UI & Multi-Viewport Smoke** (`ui/`, `e2e/`) | `ui/tests/*.test.ts` (`node --test`) + `e2e/test_ui_smoke.py` (`--suite ui-smoke`) | Zero-dep Node 24 behavioral tests covering golden snapshot parsing, 5-level priority banner queue (macOS/Linux/Windows), Day-0 (`Settings`) vs Day-N (`Monitor`) landing + attention dot, `enabled_models` toggles + `[]` pause + `Restore defaults`, `snapshotEpoch` stale drop + write lockout, visibility-aware polling backoff, Usage `$` vs `Tokens` & `Model` vs `Provider`, **health-only `● Live` popover (no `Enabled models`/`Session`/`Platform`) + right-side `XX MB RAM` footer**, Light/Dark WCAG AA contrast, and UX v2 copy hygiene — paired with lean headless Chrome CDP UI smoke (`360px`/`460px`/`760px` Dark & Light). | Never put tests inside `ui/src/`; never add regex checks on `ui/src/main.ts` to `e2e/`. |
| **Cross-Process UX v2 CUJs** (`e2e/`) | `e2e/test_cuj_ux_v2.py` (`--suite cuj`) | Lean cross-process opaque-box CUJs verifying Day-0 passive `/v1internal:*` built-in `usageMetadata` tee + S4 gzip skip → `usage.db` → `get_usage` + `proxy_rss_bytes`, `enabled_models` hot-reload & `[]` pause, concurrent `--auto-enable-launcher` + `integration.lock`, opt-out parity + symlinked RC fail-closed, and semantic `account_plan` persistence. | Do not add doc-grep or static repo checks to `e2e/`. |

---

## 7. Strict Local Config & Environment Isolation (Hermeticity)

Every test layer MUST be 100% hermetic — running the suite on a developer workstation with an active `agy-companion` proxy on `127.0.0.1:24141`, real `~/.config/agy-companion`, `~/.local/share/agy-companion`, `~/.gemini/`, live shell RC hooks, or exported proxy/API env vars must produce **zero cross-contamination** and **zero mutations** to host state.

### The 6-Point Hermeticity Contract

1. **Zero Real `$HOME` / XDG / `~/.gemini` / Shell RC Access:** Every test touching config, data, usage, plugins, or shell integration MUST construct an isolated temporary root (`TempDir` in Rust, `IsolatedEnv` in Python) and explicitly override `HOME`, `XDG_CONFIG_HOME`, `XDG_DATA_HOME`, `XDG_CACHE_HOME`, `TMPDIR`, and Windows `APPDATA`/`LOCALAPPDATA`/`USERPROFILE` — or pass explicit dependency-injected options (`IntegrationOptions`, `--config`, `--data-dir`, `--plugins-dir`). Never call bare `CompanionUiServer::new(...)` in tests without `integration_options_override`, `ui_html_source: UiHtmlSource::Fixed(None)`, `env_api_key_fallback: false`, and `hybrid_active_override: Some(false)`; always use `isolated_ui_server(...)`.
2. **Allowlist-Only Child Environments (`env_clear()`):** Every subprocess test helper (`isolated_cli_command` / `isolated_tokio_cli_command` in `crates/companion-cli/tests/common/mod.rs`, `IsolatedEnv.build_env()` in `e2e/harness.py`) MUST start from an **empty** environment (`env_clear()` / `{}`), copy only a minimal safe allowlist (`PATH`, `LANG`, `LC_ALL`, `SSL_CERT_FILE`, `SSL_CERT_DIR`, and Windows `SystemRoot`/`COMSPEC`), and inject isolated temp dirs. Never rely on a partial denylist, raw `Command::new(BIN)`, `CARGO_BIN_EXE_agy-companion` outside `tests/common/mod.rs`, or `run_cli(...)` without an explicit non-`None` `env=` dict.
3. **Zero Hardcoded Default Ports (`24141..=24150`):** Tests MUST NEVER bind or probe `24141..=24150` on loopback. In-process servers bind `127.0.0.1:0`; subprocess servers and `IsolatedEnv.make_config_dict()` MUST auto-allocate ephemeral ports via `find_free_port()` / `free_loopback_port()` (explicitly excluding `24140..=24160`). Only pure JSON serialization/constant unit tests (zero network I/O) may assert `DEFAULT_LISTEN_PORT == 24141`.
4. **Zero Process-Wide Env Mutation:** Rust `#![forbid(unsafe_code)]` already forbids `std::env::set_var` in Edition 2024 — preserve that via explicit DI structs (`UiServerOptions`, `DoctorEnv`, `ProxyStateOptions`, `GeminiProvider::with_env_api_key`) or isolated child re-execs. In Python E2E, never mutate `os.environ` in place; pass `env=` dicts to subprocesses and parallelize across processes (`ProcessPoolExecutor`), never threads.
5. **Guaranteed RAII Teardown on Failure/Panic:** Every spawned child process or detached daemon MUST be guarded **before** any assertion (`ChildGuard` / `KillOnDrop` / `DetachedPidFileGuard` in Rust; `CompanionProcess` / `SidecarUiClient` / `register_pidfile_cleanup` / `ExitStack` callback in Python). Zero unguarded `.spawn()` or raw `subprocess.Popen(...)`.
6. **Self-Verifying "Hostile Developer Shell" Smoke Verification:** Before merging test-infra changes, verify both Rust and E2E suites pass cleanly while a live listener holds `127.0.0.1:24141` **and** hostile ambient env vars are exported (always using a short non-secret placeholder key — never a 39-char `AIza...` string):

```bash
# Run while a real or dummy listener occupies 127.0.0.1:24141
GEMINI_API_KEY=hostile-test-key \
GOOGLE_GEMINI_BASE_URL=http://127.0.0.1:1 \
AGY_COMPANION_CONFIG=/nonexistent/hostile.json \
AGY_COMPANION_UI_HTML=/nonexistent/hostile.html \
HTTP_PROXY=http://127.0.0.1:1 \
RUST_LOG=trace \
cargo test --workspace
```

### Automated Static Hermeticity Checks (`scripts/check.py --profile repo`, `< 0.1s`)

Five fast static guards in `scripts/check.py` (invoked by `scripts/agent_check.sh` and CI `repo-checks`) block hermeticity regressions automatically on every run (compatible with both stock macOS Python 3.9+ and Python 3.12+):

| # | Automated Check | Enforced Rule |
| --- | --- | --- |
| 1 | No raw `Command::new(BIN)` or `CARGO_BIN_EXE_agy-companion` in `crates/companion-cli/tests/` outside `tests/common/mod.rs` | All CLI integration spawns MUST go through `isolated_cli_command` / `isolated_tokio_cli_command` (`env_clear()` + allowlist). |
| 2 | No bare `CompanionUiServer::new(` or unisolated `with_options(` in `crates/companion-ui/src/tests.rs` | Require `isolated_ui_server*` setting `integration_options_override`, `ui_html_source: Fixed(None)`, `env_api_key_fallback: false`, and `hybrid_active_override: Some(false)` (allowlisting only `isolated_ui_server_with` and `ui_resource_reads_process_env_var_dynamically`). |
| 3 | No direct `24141` bind or live port literal in tests | Fail on `bind(...24141)`, `port = 24141`, `listen_port = 24141`, or `:24141` across `crates/` tests and `e2e/` (outside pure constant/serde unit tests). |
| 4 | No `e2e/` white-box repo reads or `agent_check.sh` / `scripts/check.py` recursion | Fail if `e2e/test_*.py` reads `crates/`, `ui/src/`, `README.md`, `AGENTS.md`, or `docs/` via `repo_root` or string literals, or if any `e2e/*.py` references `agent_check.sh` or `scripts/check.py`. |
| 5 | No `run_cli(` without explicit non-`None` `env=` in `e2e/test_*.py` | Python AST check requiring explicit non-`None` `env=` on every `run_cli(...)` call. |

---

## 8. Fast Test Execution & Latency SLOs

Fast feedback loops keep TDD practical and prevent agents from skipping verification. Every suite layer has a hard warm latency budget and a soft per-test cap:

| Execution Scope | Command | Hard Warm SLO | Soft Per-Test Cap | Breach Rule (Action When Exceeded) |
| --- | --- | ---: | ---: | --- |
| **Inner Loop — Single Rust Crate (`--lib`)** | `cargo test -p <crate> --lib` | **`< 1.5s`** | **`< 100ms`** *(pure unit)* / **`< 300ms`** *(in-proc HTTP)* | Never use whole-second `Duration::from_secs(1+)` timeouts in unit tests; inject `#[cfg(test)] Duration::from_millis(150)` or `tokio::time::pause()`. |
| **Inner Loop — `cli_integration`** | `cargo test -p companion-cli --test cli_integration` | **`< 2.5s`** | **`< 1.0s`** *(exception: 1 idle-SSE `< 3.0s`)* | Limit real-sleep daemon tests to 1 canonical case; parameterize symlink checks into table-driven loops. |
| **Inner Loop — UI Pure Logic & Golden Contracts** | `npm --prefix ui test` (`node --test`) | **`< 1.0s`** | **`< 50ms`** | Keep pure logic in `ui/src/{types,state,pricing,dom}.ts` and DOM views tested via `ui/tests/fake_dom.ts`; zero real network/timers; inject fake clocks for backoff. |
| **Full Rust Workspace (Warm)** | `cargo test --workspace` | **`< 6.0s`** | See crate caps | Profile per-crate timings; eliminate `>= 1s` real sleeps before adding new tests. |
| **Pre-Commit / Pre-PR Gate (Warm)** | `./scripts/agent_check.sh` | **`< 12.0s`** | — | Never run `e2e/run_e2e.py` (including `--self-test`) or `npm run build` inside `agent_check.sh`; keep static checks `< 2s`. |
| **Lean Opaque-Box E2E (Serial)** | `python3 -B e2e/run_e2e.py --suite all --workers 1` | **`< 30.0s`** | **`< 3.0s`** *(exception: 1 idle-SSE CUJ `< 5.5s`)* | Never invoke `agent_check.sh`; never spawn subprocesses for pure functions; consolidate duplicate sleep tests. |
| **Lean Opaque-Box E2E (Parallel Default)** | `python3 -B e2e/run_e2e.py --suite all --workers 4` | **`< 12.0s`** | Same per-test cap | Use stdlib `ProcessPoolExecutor` (never threads) with hermetic `IsolatedEnv` + ephemeral ports; reject any PR whose test requires `--workers 1` due to shared state. |
| **Headless Chrome CDP UI Smoke** | `python3 -B e2e/run_e2e.py --suite ui-smoke --repo-root .` | **`< 5.0s`** | **`< 3.0s`** | Keep pure-stdlib CDP against compiled `ui/dist/index.html` with temp `--user-data-dir` and clean skip when Chrome is absent. |

### Speed Engineering Rules

1. **Sub-Second Test Timeouts (`Duration`, Never Whole-Second `u64`):** Expose `#[cfg(test)]` `Duration` hooks (`100–250ms`, e.g., `Duration::from_millis(150)`) for request timeouts and health/connection probes rather than sleeping `1s+` in Rust unit tests.
2. **Keep `--self-test` Out of `agent_check.sh`:** Run slim E2E harness self-tests only when `e2e/harness.py` changes, never on every pre-commit check.
3. **Process-Isolated Parallel E2E (`ProcessPoolExecutor`):** Default `e2e/run_e2e.py` to `--workers 4` (`min(4, os.cpu_count() or 1)`) using pure stdlib `concurrent.futures.ProcessPoolExecutor` (avoiding `os.environ`/`sys.path` thread hazards while staying zero-dependency), keeping `--workers 1` strictly as a debugging flag.
4. **Avoid External Fork Loops in Shell Shims:** Prefer pure bash parameter expansion (`${target%/*}`, `${target##*/}`) over calling external `/usr/bin/dirname` inside symlink-resolution loops so shell shim E2E tests finish in sub-second time.

---

## 9. Fast Inner-Loop & Pre-PR Verification Commands

Climb gates from fastest to heaviest while iterating; always run `./scripts/agent_check.sh` before committing or opening a PR.

| Changed Area | Fastest Inner-Loop Command | Warm SLO |
| --- | --- | ---: |
| `crates/companion-core/` | `cargo test -p companion-core --all-features` | `< 1.5s` |
| `crates/companion-providers/` | `cargo test -p companion-providers` | `< 1.5s` |
| `crates/companion-proxy/` | `cargo test -p companion-proxy` | `< 1.5s` |
| `crates/companion-ui/` | `cargo test -p companion-ui` | `< 1.5s` |
| `crates/companion-cli/` | `cargo test -p companion-cli` | `< 1.5s` lib / `< 2.5s` int |
| `ui/` | `npm --prefix ui test && npm --prefix ui run typecheck && npm --prefix ui run build` | `< 1.0s` tests |
| `plugin/` | `python3 -B scripts/check_plugin_logo.py . && python3 -B scripts/test_check_plugin_logo.py -v && agy plugin validate --strict ./plugin` | `< 1.0s` |
| Cross-process / binary CUJs | `cargo build -p companion-cli && python3 -B e2e/run_e2e.py --suite all --workers 4 --binary target/debug/agy-companion --repo-root . -v` | `< 12.0s` parallel / `< 30.0s` serial |
| Headless Chrome CDP UI smoke | `python3 -B e2e/run_e2e.py --suite ui-smoke --repo-root . -v` *(clean skip if Chrome absent)* | `< 5.0s` |
| Dev-only E2E harness guardrails | `python3 -B e2e/run_e2e.py --self-test -v` | `< 3.0s` |
| **Pre-PR Gate (Mandatory)** | `./scripts/agent_check.sh` | `< 12.0s` |
