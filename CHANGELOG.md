# Changelog

All notable changes to Antigravity Companion are documented in this file.

## [Unreleased]

## [0.1.1] - 2026-11-28

### Added
- **In-panel version badge & one-click upgrade** — Shows the running version in the panel header, checks for updates in the background, and lets you upgrade to newer releases directly from the panel.
- **Main vs. sub-agent context breakdown** — Distinguishes main-agent and sub-agent token usage in a two-tone Monitor context bar and explains per-model early compaction windows inline.

### Changed
- **Clearer provider labels & model rates** — AI Studio Gemini models now appear with the `(AI Studio)` label instead of `(Proxy)`, model badges display provider attribution, and listed model prices and context windows match current provider rates.

### Fixed
- **ChatGPT (OpenAI Codex) reasoning & tool calling** — Streamed reasoning summaries (including final-only summaries) as visible thoughts, preserved encrypted reasoning across turns, advertised full Codex context windows, and fixed multi-turn function responses.
- **AI Studio Gemini long-session reliability** — Applied derived 85% compaction windows, capped `maxOutputTokens` to the 64K streaming limit, and prevented compaction-window fallbacks on long sessions.
- **Monitor, Usage & sidecar polish** — Fixed GPT provider attribution and placeholder model names in Usage, kept the Monitor context-limit info icon aligned, removed asymmetric scrollbar spacing in the side panel, and prevented port conflicts when adopting an already-running proxy.

## [0.1.0] - 2026-11-27

Initial public release.

### Added
- **ChatGPT (OpenAI Codex)** — Sign in with your ChatGPT account to use `GPT-6.2 Astra`, `GPT-6.2 Sol`, `GPT-6.1 Terra`, and `GPT-6 Luna` in Antigravity's model picker.
- **Gemini API** — Connect your Gemini API key to use `Gemini 4 Carbon`, `Gemini 4 Neon`, `Gemini 4 Hydrogen`, `Gemini 4 Argon`, and `Gemini 3.8 Flash` alongside your built-in Antigravity models.
- **Monitor, Usage & Settings panel** — Native side panel showing live context window fill, prompt cache hit rate, session spend, and 7-day usage breakdowns, including automatic tracking for built-in Antigravity models with zero setup.
- **One-command install & uninstall** — Install via `npx -y agy-companion@latest install` or `uvx --refresh agy-companion install` on macOS, Linux, and Windows, with automatic upgrades and clean uninstall (`uninstall [--purge]`).

[Unreleased]: https://github.com/Jacksunwei/agy-companion/compare/v0.1.1...HEAD
[0.1.1]: https://github.com/Jacksunwei/agy-companion/compare/v0.1.0...v0.1.1
[0.1.0]: https://github.com/Jacksunwei/agy-companion/releases/tag/v0.1.0
