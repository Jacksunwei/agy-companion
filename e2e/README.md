# `e2e/` — Opaque-Box Intent-Named Parallel E2E Suite

Pure Python 3 standard-library opaque-box E2E suite for `agy-companion`. Exercises the compiled `agy-companion` binary, runtime packaging/launcher shims, and compiled `ui/dist/index.html` without reading repo source/docs or invoking `scripts/agent_check.sh`.

- **Zero external dependencies:** Uses only Python 3 stdlib (`unittest`, `concurrent.futures.ProcessPoolExecutor`, `http.server`, `http.client`, `sqlite3`, `subprocess`, `socket`, `threading`, `tempfile`, etc.). Always run with `python3 -B` so no `__pycache__` or `.pyc` files are created.
- **Strict hermeticity:** Every test runs inside an `IsolatedEnv` (`HOME`, `XDG_CONFIG_HOME`, `XDG_DATA_HOME`, `XDG_CACHE_HOME`, `TMPDIR`, plus Windows profile vars), uses allowlist-scrubbed child envs with explicit `env=` on every `run_cli(...)` call, allocates ephemeral loopback ports outside the default companion port range, and tears down spawned processes/daemons via `ExitStack`, `reap_pidfile_daemon`, and `atexit` guards.

## Layout

| File | Role |
| --- | --- |
| `run_e2e.py` | Parallel intent-named CLI runner (`--suite`, `--workers`, `--binary`, `--repo-root`, `--filter`, `--list`, `--json`, `-x/--fail-fast`, `--timeout`, `-v/--verbose`, `--self-test`, deprecated `--tier` alias) |
| `harness.py` | `E2ETestCase`, `IsolatedEnv`, `CompanionProcess`, `MockConnectProxy`, `reap_pidfile_daemon`, `run_cli`, `http_raw_request`, `http_stream_request`, `poll_usage_events`, `poll_stats`, `find_free_port` |
| `mock_gemini.py` | Ephemeral loopback HTTP/1.1 `MockGeminiServer` (request capture, chunked SSE, raw bytes, fault queues, TCP RST) & `MockHealthServer` |
| `mcp_client.py` | Strict newline-delimited JSON-RPC 2.0 `McpStdioClient` (stdout purity auditing, `save_settings_smart` schema introspection) |
| `cdp_smoke.py` | Pure-stdlib headless Chrome CDP harness (RFC 6455 WebSocket, WCAG 2.1 contrast math, hermetic `ChromeBrowser`, loopback mock MCP App host) |
| `test_cuj_core.py` | `--suite cuj` — Core binary CUJs (proxy routing, SSE pass-through, `ui-mcp` lifecycle, SQLite usage, daemon adoption, install/uninstall, CloudCode catalog) |
| `test_cuj_ux_v2.py` | `--suite cuj` — UX v2 cross-process CUJs (Day-0 passive built-in usage tee + S4 gzip skip + `proxy_rss_bytes`, `enabled_models` hot-reload & `[]` pause, concurrent `--auto-enable-launcher` lock, opt-out parity & symlink fail-closed, semantic `account_plan`) |
| `test_boundary_security.py` | `--suite boundary` — Opaque-box S1–S7 security & error boundary canaries |
| `test_shims_packaging.py` | `--suite shims` — Runtime launcher/dev-install/npm/Python packaging shims |
| `test_ui_smoke.py` | `--suite ui-smoke` — Multi-viewport Dark/Light overflow, WCAG AA contrast, health-only `Live` footer popover, and RAM telemetry against compiled `ui/dist/index.html` |
| `test_self.py` | `--self-test` — Dev-only harness safety guardrails (`IsolatedEnv`, `PidfileReaper`, `McpStdioClient`) |
| `__init__.py` | Package marker |
| `README.md` | This guide |

## Quick Usage

Always invoke with `python3 -B`:

```bash
# Build the binary once
cargo build -p companion-cli

# Run default opaque-box suite (cuj + boundary + shims) in parallel or serial
python3 -B e2e/run_e2e.py --suite all --workers 4 --binary target/debug/agy-companion --repo-root . -v
python3 -B e2e/run_e2e.py --suite all --workers 1 --binary target/debug/agy-companion --repo-root . -v

# Run a single intent suite
python3 -B e2e/run_e2e.py --suite cuj --binary target/debug/agy-companion --repo-root . -v
python3 -B e2e/run_e2e.py --suite boundary --binary target/debug/agy-companion --repo-root . -v
python3 -B e2e/run_e2e.py --suite shims --binary target/debug/agy-companion --repo-root . -v

# List tests, filter by substring, fail fast, or emit JSON
python3 -B e2e/run_e2e.py --suite all --list
python3 -B e2e/run_e2e.py --suite cuj --filter test_cuj_ux_ --binary target/debug/agy-companion -x -v
python3 -B e2e/run_e2e.py --suite all --binary target/debug/agy-companion --json

# Backwards-compatible --tier alias (1/3/4 -> cuj, 2 -> boundary, all -> all)
python3 -B e2e/run_e2e.py --tier all --binary target/debug/agy-companion --repo-root . -v

# Dev-only harness self-test guardrails (no Rust binary needed)
python3 -B e2e/run_e2e.py --self-test -v
```

## Headless Chrome CDP UI Smoke (`--suite ui-smoke`)

`--suite ui-smoke` renders compiled `ui/dist/index.html` directly inside an isolated loopback mock MCP App host (no compiled Rust binary required) and is excluded from `--suite all`:

```bash
python3 -B e2e/run_e2e.py --suite ui-smoke --repo-root . -v
```

Environment controls:
- `AGY_E2E_CHROME_BIN`: Authoritative Chrome/Chromium executable override. When set to a non-executable or empty path (e.g. `AGY_E2E_CHROME_BIN=/nonexistent`), discovery does **not** fall through to system Chrome and tests cleanly skip (exit `0`).
- `CHROME_BIN`: Secondary executable hint used when `AGY_E2E_CHROME_BIN` is unset.
- `AGY_E2E_REQUIRE_CHROME=1`: Fail instead of skipping when no Chrome binary is available.

## Binary Resolution & Exit Codes

Binary lookup precedence (skipped for `--self-test` and `--suite ui-smoke`):
1. `--binary <path>`
2. `AGY_COMPANION_E2E_BIN` environment variable
3. `<repo-root>/target/debug/agy-companion`

Exit codes:
- `0`: All selected tests passed or cleanly skipped (or `--list` succeeded).
- `1`: One or more tests failed or errored.
- `2`: Invalid CLI arguments, zero filter matches, or missing/non-executable binary.
