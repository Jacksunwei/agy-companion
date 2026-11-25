# Documentation Index

Keep design intent and architectural decisions close to the code:

## Design Specs (`docs/design/`)

- [Architecture Specification](design/architecture.md) — Canonical v1 architecture (Antigravity plugin + sidecar proxy + MCP App UI + Rust workspace + npm/PyPI distribution).
- [UI & CUJ Design Spec (v2)](design/ui-ux-v2.md) — Locked v2 single-column 3-tab UX (**Monitor** | **Usage** | **Settings**), zero-friction first-run defaults, multi-select enabled models + official tooltips, multi-backend provider stack, SQLite-backed usage analytics, and open-source naming.
- [Dock/Launcher Integration & One-Command Install](design/dock-launcher-and-install.md) — CloudCode Pro/PayGo/Enterprise transparency, Managed On-Demand Daemon, zero-eval login-shell integration, one-command `install`/`uninstall`, and Windows parity.
- [Milestone 1 Implementation Plan](design/m1-implementation-plan.md) — Bite-sized TDD execution plan for Milestone 1.
- [Usage & Cost Tracking (TODO)](design/usage-tracking-todo.md) — Post-M1 usage/cost aggregation & `ccusage`-style reporting design notes + day-1 `UsageSink` seam.
- [User Version Upgrades (TODO)](design/upgrade-lifecycle-todo.md) — Post-M1 binary + plugin upgrade design notes + day-1 version/skew seams.

## Architecture Decision Records (`docs/decisions/`)

Record significant trade-offs and why chosen approaches beat alternatives as the codebase evolves.
