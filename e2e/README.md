# `e2e/` — Opaque-Box 4-Tier E2E Test Suite for `agy-companion`

Pure Python 3 standard-library opaque-box E2E test suite verifying all Milestone 1 user-facing features (**F1–F12**, plus additive **F14–F16** docs, local-dev workflow & XDG path checks) and all 8 Security Invariants (**S1–S8**).

- **Zero external dependencies**: Uses only Python 3 stdlib (`unittest`, `http.server`, `http.client`, `subprocess`, `socket`, `threading`, `tempfile`, `tomllib`, etc.).
- **Strict test isolation**: Every test runs in a fresh `IsolatedEnv` (`tempfile.TemporaryDirectory`) with ambient secrets and proxy variables stripped (`GEMINI_API_KEY`, `GOOGLE_GEMINI_BASE_URL`, `HTTP_PROXY`, `HTTPS_PROXY`, `ALL_PROXY`, `NO_PROXY`, `AGY_COMPANION_CONFIG`, `AGY_COMPANION_UI_HTML`, `AGY_COMPANION_BIN`, `ANTIGRAVITY_EXECUTABLE_DATA_DIR`). All child processes and mock servers are torn down via `ExitStack` + process-group `SIGTERM`→`SIGKILL` + `atexit` PID guards.

## Layout

| File | Purpose |
| --- | --- |
| `run_e2e.py` | CLI runner (`--tier`, `--binary`, `--repo-root`, `--self-test`, `--list`, `--filter`, `--json`, `--fail-fast`, `--timeout`, `--verbose`) |
| `harness.py` | `E2ETestCase`, `TestRuntimeConfig`, `IsolatedEnv`, `CompanionProcess`, `run_cli`, `http_raw_request`, `http_stream_request`, `poll_usage_events`, `poll_stats`, `find_key_recursive`, `resolve_binary` |
| `mock_gemini.py` | Ephemeral HTTP/1.1 `MockGeminiServer` (exact request capture, chunked SSE, split UTF-8/CRLF, FIFO fault queues, `SO_LINGER` TCP RST, `EchoChecksumAction`) & `MockHealthServer` |
| `mcp_client.py` | Strict newline-delimited JSON-RPC 2.0 `McpStdioClient` (stdout purity auditing, fast EOF detection, thread-safe stdin writes, `save_settings_smart` schema introspection) |
| `test_tier1_features.py` | **Tier 1** — Core happy-path feature coverage (85 tests: 6–7 per F1–F12 + 11 for F14/F15/F16, Gateway & CloudCode Hybrid mode) |
| `test_tier2_boundaries.py` | **Tier 2** — Boundaries, corner cases, negative inputs & S1–S8 security tests (94 tests: 6–9 per F1–F12 + 6 for F14/F15/F16) |
| `test_tier3_combinations.py` | **Tier 3** — Pairwise cross-feature combinations (16 tests, including F15 live UI reload, symlink-aware launcher & F16 XDG coordination) |
| `test_tier4_scenarios.py` | **Tier 4** — Multi-step real-world end-to-end application workloads (7 tests) |
| `test_self.py` | **Self-Tests** — 21 pure-Python harness self-tests (`SELF-01`..`SELF-21`), runnable without the Rust binary |

## Quick Usage

```bash
# 1. Verify harness integrity without needing a compiled agy-companion binary
python3 e2e/run_e2e.py --self-test -v

# 2. List all tests (plain text or JSON) without needing a binary
python3 e2e/run_e2e.py --list
python3 e2e/run_e2e.py --list --json

# 3. Run specific tier(s) or the entire 4-tier suite against a built binary
python3 e2e/run_e2e.py --tier 1 --binary target/debug/agy-companion
python3 e2e/run_e2e.py --tier all --binary target/debug/agy-companion --repo-root .

# 4. Filter by substring or emit JSON execution report
python3 e2e/run_e2e.py --tier 2 --filter f5 --binary target/debug/agy-companion -v
python3 e2e/run_e2e.py --tier all --binary target/debug/agy-companion --json
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
