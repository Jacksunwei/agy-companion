# Contributing to Antigravity Companion

Thank you for your interest in contributing to `agy-companion`. We value focused, well-tested pull requests with a clear thesis over sprawling multi-concern diffs.

## Prerequisites

- **Rust toolchain:** stable Rust supporting `edition = "2024"` (`cargo`, `rustc`, `rustfmt`, `clippy`).
- **Node.js:** `>= 18` (for building the embedded TypeScript MCP App UI in `ui/`).
- **Python 3:** `>= 3.9` (for running the pure-stdlib opaque-box E2E suite in `e2e/`; no `pip` dependencies required).
- **Antigravity CLI (`agy`):** optional, used for `agy plugin validate --strict ./plugin` and live end-to-end testing (skipped cleanly by `agent_check.sh` when absent).

## Repository Layout

| Path | Role |
| --- | --- |
| [`crates/companion-core/`](crates/companion-core/README.md) | Config schema, atomic/TOCTOU-safe file I/O (`fs_safe`), validation, model catalog, redaction, SQLite WAL `UsageSink`, shell RC integration |
| [`crates/companion-providers/`](crates/companion-providers/README.md) | `ProviderAdapter` trait, zero-compression `GeminiProvider`, shared loopback probe client |
| [`crates/companion-proxy/`](crates/companion-proxy/README.md) | `127.0.0.1` Axum HTTP proxy, Hybrid Mode bridge, SSE usage tee, hot-reload, idle shutdown |
| [`crates/companion-ui-mcp/`](crates/companion-ui-mcp/README.md) | Stdio MCP App server serving the embedded settings UI and configuration/health tools |
| [`crates/companion-cli/`](crates/companion-cli/README.md) | Unified `agy-companion` binary (`install`, `uninstall`, `serve`, `ui-mcp`, `doctor`, `models`, `launch`, `hook-env`, `integration`) |
| `ui/` | Vite + TypeScript MCP App frontend compiled to committed `ui/dist/index.html` |
| [`plugin/`](plugin/README.md) | 7-file Antigravity plugin bundle embedded into `companion-cli` at compile time |
| `packages/` | Cross-platform npm and Python (`maturin`) binary distribution shims |
| [`e2e/`](e2e/README.md) | Pure Python 3 stdlib 4-tier opaque-box E2E suite |
| [`docs/`](docs/README.md) | Architecture and feature design specs |

## Adding a Provider Adapter

One of the highest-leverage contributions is implementing a new BYOM backend (Ollama Cloud, OpenAI-compatible endpoints, OpenRouter, Codex, or Claude).

1. **Propose or claim an adapter** using the [Provider Adapter issue form](https://github.com/Jacksunwei/agy-companion/issues/new?template=provider_adapter.yml) so we can align on config schema and streaming/cache behavior upfront.
2. **Implement [`ProviderAdapter`](crates/companion-providers/src/lib.rs)** inside `crates/companion-providers/src/`:
   - **Direct 1-hop translation:** Antigravity speaks Gemini API inbound; translate directly between Gemini payloads and the upstream provider without chaining a second local proxy process.
   - **Zero-compression `reqwest` invariant (S4):** Never enable `gzip`, `brotli`, `deflate`, or `zstd` on `reqwest` in `Cargo.toml` — byte-for-byte streaming pass-through is required to preserve implicit prompt caching.
   - **SSRF & auth safety (S3):** Require `https://` for remote upstream `base_url` (allowing `http://` only for literal `127.0.0.1` / `localhost`), disable HTTP redirects (`Policy::none()`), authenticate via headers (never query params), and redact credentials from logs and `Debug` impls.
   - **Zero `unsafe`:** All workspace crates enforce `#![forbid(unsafe_code)]`.
3. **Add unit and proxy integration tests** covering streaming, error mapping, auth redaction, and invalid `base_url` rejection.

## Local Development Loop

See [`.agents/skills/local-dev/SKILL.md`](.agents/skills/local-dev/SKILL.md) for the full runbook.

```bash
# Build UI + Rust debug binary and symlink ./plugin into ~/.gemini/config/plugins/agy-companion
./scripts/dev-install.sh

# Instant MCP App UI iteration without rebuilding Rust:
export AGY_COMPANION_UI_HTML="$(pwd)/ui/dist/index.html"
npm --prefix ui run build
# Then close and reopen the Antigravity Companion side panel.
```

> **Never** commit real API keys (`AIza...`), `.env` files, `.DS_Store`, or Python `__pycache__` artifacts. Always invoke Python test commands with `python3 -B`.

## Mandatory Verification Before Opening a PR

Run the 7-gate local verification script before every commit and pull request:

```bash
# Primary gate: governance, hygiene/secret scan, commit checks, shell syntax,
# 4-way version parity, Rust fmt/clippy -D warnings/tests, UI CSP/size, plugin validation
./scripts/agent_check.sh
```

When touching proxy, CLI, installer, plugin, or UI behavior, also run the opaque-box E2E suite:

```bash
cargo build -p companion-cli
python3 -B e2e/run_e2e.py --tier all --binary target/debug/agy-companion --repo-root . -v
```

If you edit `ui/src/`, rebuild and verify determinism of the committed single-file bundle:

```bash
npm --prefix ui ci && npm --prefix ui run typecheck && npm --prefix ui run build
```

## Branches, Commits & Pull Requests

1. **Branch off `main`:** Use `<type>/<short-description>` (e.g., `feat/ollama-adapter`, `fix/proxy-timeout`, `docs/readme-quickstart`). Never push directly to `main`.
2. **Conventional Commits (zero AI attribution):**
   - Format: `<type>(<scope>): <imperative lowercase summary without trailing period>`
   - Explain *why* the change exists rather than restating the diff.
   - **No AI attribution** in commit messages (no `Co-Authored-By:` lines naming an assistant or `Generated with ...` footers — enforced by `./scripts/agent_check.sh`).
3. **Pull Request:** Fill out [`.github/PULL_REQUEST_TEMPLATE.md`](.github/PULL_REQUEST_TEMPLATE.md) with a clear summary and pasted local verification output. Update relevant specs in `docs/` alongside behavioral changes.

## Using Coding Agents?

If you are using an AI coding agent in this repository, point it at [`AGENTS.md`](AGENTS.md) — it is the single canonical engineering contract covering repo layout, verification gates, and the S1–S8 architecture and security invariants. Do not create `GEMINI.md`.
