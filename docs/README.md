# Documentation Index

Keep design intent and architectural decisions close to the code:

## Design Specs (`docs/design/`)

- [Architecture Specification](design/architecture.md) — Canonical v1 architecture (Antigravity plugin + sidecar proxy + Sidecar Web UI + Rust workspace + npm/PyPI distribution).
- [UI & CUJ Design Spec (v2)](design/ui-ux-v2.md) — Locked v2 single-column 3-tab UX (**Monitor** | **Usage** | **Settings**), zero-friction first-run defaults, multi-select enabled models + official tooltips, multi-backend provider stack, SQLite-backed usage analytics, and open-source naming.
- [Dock/Launcher Integration & One-Command Install](design/dock-launcher-and-install.md) — CloudCode Pro/PayGo/Enterprise transparency, Managed On-Demand Daemon, zero-eval login-shell integration, one-command `install`/`uninstall`, and Windows parity.
- [OpenAI Codex Provider & Unified Auth Storage](design/openai-codex-provider.md) — Unified secret-free `config.json` + sibling `0700`/`0600` `auth/` `AuthStore` architecture, OpenAI Codex OAuth 2.0 PKCE + Sidecar Web UI card, direct 1-hop Gemini ↔ Codex Responses API streaming translation, and `M351..=M364` model-picker slots.
- [Milestone 1 Implementation Plan](design/m1-implementation-plan.md) — Bite-sized TDD execution plan for Milestone 1.
- [Usage & Cost Tracking (TODO)](design/usage-tracking-todo.md) — Post-M1 usage/cost aggregation & `ccusage`-style reporting design notes + day-1 `UsageSink` seam.
- [User Version Upgrades (TODO)](design/upgrade-lifecycle-todo.md) — Post-M1 binary + plugin upgrade design notes + day-1 version/skew seams.
- [Testing Strategy & CUJ Verification Guide](design/testing-strategy.md) — Enduring CUJ-first Testing Trophy, Single-Owner layer matrix, strict hermetic local-config/port isolation, fast latency SLOs, anti-slop rules, UI & Rust↔TS contract testing, and UX v2 testing standards.
- [Testing Infrastructure Improvement Plan](design/testing-infra-improvement-plan.md) — Implemented Phase 0–3 overhaul record (`PRs #47–#51` + final doc sync): `ISO-1..ISO-11` hermeticity remediations, S1–S8 red lines, `WS1–WS6` deliverables, and achieved warm SLO verification posture.

## Architecture Decision Records (`docs/decisions/`)

Record significant trade-offs and why chosen approaches beat alternatives as the codebase evolves.
