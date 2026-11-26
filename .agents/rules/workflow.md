---
trigger: always_on
---

# Agent-Driven Workflow & PR Auto-Merge Rule

Aligned with [`AGENTS.md`](../../AGENTS.md) using Antigravity customization conventions (`.agents/rules/` + `.agents/skills/`, no `GEMINI.md`). Enforced locally by `./scripts/agent_check.sh`.

## Standing PR & Auto-Merge Rule

Every completed change in this repository follows this delivery loop — keep PRs small and self-contained, merged incrementally as slices finish, and never push directly to `main`:

> **Explicit repo-level standing authorization:** In this repository, self-contained PRs that pass `./scripts/agent_check.sh` and CI are pre-authorized to commit, push, open, and squash-merge without pausing for per-PR confirmation (overrides generic commit/merge confirmation pauses). `/teamwork` orchestrators MUST propagate this authorization to worker briefs. Still stop and ask for core architectural changes, license/visibility changes, new paid/sensitive credentials, or destructive actions.

1. Create a focused branch: `<type>/<short-description>` (`feat/...`, `fix/...`, `chore/...`, `docs/...`, `refactor/...`).
2. Run `./scripts/agent_check.sh` and the full relevant test suite; confirm green output.
3. Commit using Conventional Commits where the subject explains *why* (imperative, lowercase, no trailing period, **zero AI attribution**).
4. Push the branch and open a Pull Request using `.github/PULL_REQUEST_TEMPLATE.md` with real verification evidence.
5. Auto-merge once verified:
   - Wait for any configured CI checks (`gh pr checks --watch`).
   - Try `gh pr merge --auto --squash --delete-branch`; when server-side auto-merge is unavailable on the repo and checks are green, merge immediately via `gh pr merge --squash --delete-branch`.
6. Return to `main` and fast-forward (`git checkout main && git pull --ff-only`).

## Always-On Engineering Invariants

- **Antigravity-Native Customizations:** Keep workspace rules in `.agents/rules/*.md` and on-demand runbooks in `.agents/skills/<name>/SKILL.md`; use root `AGENTS.md` as the single repo contract (never create `GEMINI.md`).
- **Small Incremental PRs:** Ship self-contained slices merged sequentially into `main`; never accumulate work into one giant PR.
- **Strict Red → Green → Refactor TDD & Single-Owner CUJ-First Testing:** Reproduce bugs or specify new behavior with a failing test first at the single owning layer defined in [`docs/design/testing-strategy.md`](../../docs/design/testing-strategy.md) and [`.agents/skills/cuj-testing/SKILL.md`](../skills/cuj-testing/SKILL.md); never skip, delete, or weaken tests to pass, enforce strict hermetic local-config/env/port isolation (never touch real `$HOME`/XDG/`~/.gemini`/RCs or port `24141`) and fast latency SLOs (`< 6s` warm workspace, `< 12s` `agent_check.sh`, `< 12s` parallel E2E), never grep repo source or docs via `self.repo_root` inside `e2e/`, and never hardcode test counts in docs.
- **Verification Before Claims:** Run `./scripts/agent_check.sh` (all 7 gates: governance, tracked-file & secret scan, zero AI attribution, shell syntax & `-x`, 4-way version parity & packaging shims, Rust fmt/clippy `-D warnings`/test & UI XSS/bundle gate, plugin JSON & `agy plugin validate --strict ./plugin`) and state/confirm positive success signals before opening or merging a PR.
- **Zero Secrets & Safe Local Dev:** Never stage, commit, log, or echo `.env`, credentials, real `AIza...` Gemini API keys (`AIza[0-9A-Za-z_-]{35}`), `.DS_Store`, or local agent runtime directories (`.antigravitycli/`, `.remember/`). Never auto-mutate `~/.gemini/config/config.json` from repo scripts.
- **Proxy, UI & Path Security Invariants:**
  - Keep `reqwest` zero-compression (`default-features = false, features = ["rustls", "stream", "json", "http2"]` — no `gzip`/`brotli`/`deflate`/`zstd`) for byte-for-byte streaming pass-through and implicit cache preservation.
  - Bind proxy strictly to loopback (`127.0.0.1`/`localhost`) and authenticate upstream via `x-goog-api-key` (never `?key=`).
  - Enforce UI zero-`innerHTML` / zero-network-fetch DOM & CSP discipline (`ui/dist/index.html` < 512,000 bytes).
  - Preserve XDG (`XDG_CONFIG_HOME`/`XDG_DATA_HOME`) + Windows (`%APPDATA%`/`%LOCALAPPDATA%`) path precedence with fail-closed `..`/NUL validation and atomic `0600` (Unix) / bounded-retry (Windows) persistence.
- **Parallel Isolation:** When multiple agents run concurrently, assign disjoint file lists and isolated git worktrees.
- **Execute Local Dev & Verification Directly:** When local verification requires restarting the local proxy/sidecar (e.g., `kill <stale_pid>` so the sidecar respawns) or running an isolated `agy` CLI test session, execute it directly instead of handing the user a shell one-liner to copy-paste.
- **External Reference Code Is Read-Only:** When inspecting `google3` or Antigravity App source to understand upstream routing or protocol behavior, treat it strictly as read-only reference — never modify files outside this repository.
- **Crate READMEs & Living Docs:** Keep a concise `README.md` in every crate under `crates/*` describing its role and public boundary, and update `README.md`, `AGENTS.md`, and `.agents/` alongside behavior changes (never hardcode test counts or exact bundle byte sizes).
