# Changelog

All notable changes to **Antigravity Companion** (`agy-companion`) are documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html). Daily development PRs bump the prerelease dev version (`X.Y.0-dev.N` via `python3 -B scripts/bump_version.py`) without publishing; public releases promote `X.Y.0-dev.N` to `X.Y.0` (via `--release`) and summarize changes here.

## [Unreleased]

## [0.1.0] - 2026-11-27

First public release of **Antigravity Companion** across GitHub Releases, npm (`agy-companion` / `agyc`), and PyPI (`agy-companion`).

### Added
- **Local Loopback Proxy & Hybrid Mode** — Rust (`#![forbid(unsafe_code)]`) loopback-only (`127.0.0.1`) Axum proxy preserving built-in Antigravity models while routing personal provider models cleanly alongside them, with zero-compression byte-for-byte streaming pass-through to preserve prompt caching.
- **ChatGPT (OpenAI Codex) Provider** — One-click OAuth 2.0 PKCE sign-in, sibling `0700`/`0600` `auth/` credential store, direct 1-hop Gemini ↔ Codex Responses API streaming translation, prompt caching (`session_id`, `prompt_cache_key`, developer system instructions), and `GPT-6.2 Astra`, `GPT-6.2 Sol`, `GPT-6.1 Terra`, and `GPT-6 Luna` (`Low`–`XHigh`) models.
- **Gemini API Provider** — Header-based `x-goog-api-key` authentication, model catalog (`Gemini 4 Carbon`, `Gemini 4 Neon`, `Gemini 4 Hydrogen`, `Gemini 4 Argon`, `Gemini 3.8 Flash`), and inline connection testing.
- **Native Sidecar Web UI (Monitor | Usage | Settings)** — Single-file zero-dependency TypeScript Aux Panel (`ui/dist/index.html`, zero `innerHTML`, strict CSP) featuring live context window fill, prompt cache hit rate & break timeline, per-model spend breakdown, 7-day SQLite (`usage.db` v2) usage charts, and accessible light/dark theming.
- **Live Antigravity Language Server Telemetry** — Loopback-only LS RPC client and per-window telemetry collector providing real-time context window, cache, and Vertex list-price estimates for built-in Antigravity models even before connecting external providers.
- **One-Command Install, Smooth Lifecycle Handoff & Self-Upgrade** — `agy-companion install`, `uninstall [--purge]`, `launch`, `doctor`, and `serve` across macOS, Linux, and Windows, with zero-eval shell hooks, plugin-driven self-upgrade, and graceful proxy handoff.
- **Automated 5-Platform Release Pipeline** — Reproducible prebuilt binaries for `darwin-arm64`, `darwin-x64`, `linux-x64` (`manylinux_2_28`), `linux-arm64` (`manylinux_2_28`), and `win32-x64` with `checksums.txt`, SLSA Build Provenance, npm provenance, and PyPI PEP 740 attestations.

[Unreleased]: https://github.com/Jacksunwei/agy-companion/compare/v0.1.0...HEAD
[0.1.0]: https://github.com/Jacksunwei/agy-companion/releases/tag/v0.1.0
