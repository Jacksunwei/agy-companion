# `e2e/` — Opaque-Box 4-Tier E2E Test Suite for `agy-companion`

Pure Python 3 standard-library opaque-box E2E test suite verifying all Milestone 1 user-facing features (**F1–F16**), the full CloudCode transparency, Dock/Launcher integration, and one-command install/uninstall rollout (**R01–R25**), and all 8 Security Invariants (**S1–S8**).

- **Zero external dependencies**: Uses only Python 3 stdlib (`unittest`, `http.server`, `http.client`, `subprocess`, `socket`, `threading`, `tempfile`, `tomllib`, etc.).
- **Strict test isolation**: Every test runs in a fresh `IsolatedEnv` (`tempfile.TemporaryDirectory`) with ambient secrets, shell hook vars, and proxy variables stripped (`STRIPPED_ENV_VARS` covers `GEMINI_API_KEY`, `GOOGLE_API_KEY`, `GOOGLE_GEMINI_BASE_URL`, `CLOUD_CODE_URL`, `HTTP_PROXY`, `HTTPS_PROXY`, `ALL_PROXY`, `NO_PROXY`, `ANTIGRAVITY_RESOLVING_ENVIRONMENT`, `AGY_COMPANION_*`, `JETSKI_MCP_ENV_RESTORE_*`, `SSH_CONNECTION`, `SSH_CLIENT`, `ZDOTDIR`, etc.). All child processes, mock servers, and detached daemons are torn down via `ExitStack` + 5-layer OS-cmdline-verified `reap_pidfile_daemon` + `atexit` PID guards.

## Layout (`15` files)

| File | Purpose |
| --- | --- |
| `run_e2e.py` | CLI runner (`--tier`, `--binary`, `--repo-root`, `--self-test`, `--list`, `--filter`, `--json`, `--fail-fast`, `--timeout`, `--verbose`) supporting multi-module tier tuples and module-name filtering |
| `harness.py` | `E2ETestCase`, `TestRuntimeConfig`, `IsolatedEnv`, `CompanionProcess`, `MockConnectProxy`, 5-layer `reap_pidfile_daemon` / `register_pidfile_cleanup`, `run_cli`, `http_raw_request`, `http_stream_request`, `poll_usage_events`, `poll_stats`, `find_key_recursive`, `resolve_binary` |
| `mock_gemini.py` | Ephemeral HTTP/1.1 `MockGeminiServer` (exact request capture, chunked SSE, split UTF-8/CRLF, FIFO fault queues, `SO_LINGER` TCP RST, `EchoChecksumAction`) & `MockHealthServer` |
| `mcp_client.py` | Strict newline-delimited JSON-RPC 2.0 `McpStdioClient` (stdout purity auditing, fast EOF detection, thread-safe stdin writes, `save_settings_smart` schema introspection) |
| `test_tier1_features.py` | **Tier 1 (M1 + PR 1/2)** — Core happy-path feature coverage (`89` tests) |
| `test_tier1_rollout.py` | **Tier 1 (Rollout)** — Happy-path coverage for `R01`–`R25` (`50` tests; combined Tier 1 = `139`) |
| `test_tier2_boundaries.py` | **Tier 2 (M1)** — Boundaries, corner cases, negative inputs & S1–S8 security tests (`94` tests) |
| `test_tier2_rollout.py` | **Tier 2 (Rollout)** — Boundary, negative & S1–S8 coverage for `R01`–`R25` (`51` tests; combined Tier 2 = `145`) |
| `test_tier3_combinations.py` | **Tier 3 (M1)** — Pairwise cross-feature combinations (`16` tests) |
| `test_tier3_rollout.py` | **Tier 3 (Rollout)** — Pairwise cross-PR combinations (`15` tests; combined Tier 3 = `31`) |
| `test_tier4_scenarios.py` | **Tier 4 (M1)** — Multi-step real-world end-to-end application workloads (`7` tests) |
| `test_tier4_rollout.py` | **Tier 4 (Rollout)** — End-to-end isolated-`$HOME` rollout lifecycles (`6` tests; combined Tier 4 = `13`) |
| `test_self.py` | **Self-Tests** — `25` pure-Python harness self-tests (`SELF-01`..`SELF-25`), runnable without the Rust binary |
| `__init__.py` | Package marker |
| `README.md` | This guide |

Combined totals: **`328` E2E tests** (`T1=139, T2=145, T3=31, T4=13`) + **`25` harness self-tests**.

## Quick Usage

Always invoke with `python3 -B` so no `__pycache__` or `*.pyc` files are created in the working tree:

```bash
# 1. Verify harness integrity without needing a compiled agy-companion binary (25 self-tests)
python3 -B e2e/run_e2e.py --self-test -v

# 2. List all 328 tests (plain text or JSON) without needing a binary
python3 -B e2e/run_e2e.py --list
python3 -B e2e/run_e2e.py --list --json

# 3. Run specific tier(s) or the entire 4-tier suite against a built binary
cargo build -p companion-cli
python3 -B e2e/run_e2e.py --tier 1 --binary target/debug/agy-companion
python3 -B e2e/run_e2e.py --tier all --binary target/debug/agy-companion --repo-root . -v

# 4. Filter by substring or module name (e.g. rollout-only), or emit JSON report
python3 -B e2e/run_e2e.py --tier all --filter rollout --binary target/debug/agy-companion --repo-root . -v
python3 -B e2e/run_e2e.py --tier 2 --filter f5 --binary target/debug/agy-companion -v
python3 -B e2e/run_e2e.py --tier all --binary target/debug/agy-companion --json
```

## Binary Resolution & Exit Codes

Binary lookup precedence for `--tier` runs:
1. `--binary <path>`
2. `AGY_COMPANION_E2E_BIN` environment variable
3. `<repo-root>/target/debug/agy-companion`

Exit codes:
- `0`: All selected tests passed (or `--list` succeeded).
- `1`: One or more tests failed or errored.
- `2`: Invalid CLI arguments or missing/non-executable `agy-companion` binary.
