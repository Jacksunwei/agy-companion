---
trigger: always_on
---

# Agent-Driven Workflow & PR Auto-Merge Rule

Aligned with [`AGENTS.md`](../../AGENTS.md) using Antigravity customization conventions (`.agents/rules/` + `.agents/skills/`, no `GEMINI.md`). Enforced locally by `./scripts/agent_check.sh`.

## Standing PR & Auto-Merge Rule

Every completed change in this repository follows this delivery loop — keep PRs small and self-contained, merged incrementally as slices finish, and never push directly to `main`:

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
- **Strict Red → Green → Refactor TDD:** Reproduce bugs or specify new behavior with a failing test first; never skip, delete, or weaken tests to pass.
- **Verification Before Claims:** Run `./scripts/agent_check.sh` (all 7 gates: governance, tracked-file & secret scan, zero AI attribution, shell syntax & `-x`, 4-way version parity & packaging shims, Rust fmt/clippy `-D warnings`/test & UI XSS/bundle gate, plugin JSON & `agy plugin validate --strict ./plugin`) and state/confirm positive success signals before opening or merging a PR.
- **Zero Secrets & Safe Local Dev:** Never stage, commit, log, or echo `.env`, credentials, real `AIza...` Gemini API keys (`AIza[0-9A-Za-z_-]{35}`), `.DS_Store`, or local agent runtime directories (`.antigravitycli/`, `.remember/`). Never auto-mutate `~/.gemini/config/config.json` from repo scripts.
- **Proxy, UI & Path Security Invariants:**
  - Keep `reqwest` zero-compression (`default-features = false, features = ["rustls", "stream", "json", "http2"]` — no `gzip`/`brotli`/`deflate`/`zstd`) for byte-for-byte streaming pass-through and implicit cache preservation.
  - Bind proxy strictly to loopback (`127.0.0.1`/`localhost`) and authenticate upstream via `x-goog-api-key` (never `?key=`).
  - Enforce UI zero-`innerHTML` / zero-network-fetch DOM & CSP discipline (`ui/dist/index.html` < 512,000 bytes).
  - Preserve XDG (`XDG_CONFIG_HOME`/`XDG_DATA_HOME`) + Windows (`%APPDATA%`/`%LOCALAPPDATA%`) path precedence with fail-closed `..`/NUL validation and atomic `0600` (Unix) / bounded-retry (Windows) persistence.
- **Parallel Isolation:** When multiple agents run concurrently, assign disjoint file lists and isolated git worktrees.
