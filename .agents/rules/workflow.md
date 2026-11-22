---
trigger: always_on
---

# Agent-Driven Workflow & PR Auto-Merge Rule

Aligned with [`AGENTS.md`](../../AGENTS.md) using Antigravity customization conventions (`.agents/rules/` + `.agents/skills/`, no `GEMINI.md`). Enforced locally by `./scripts/agent_check.sh`.

## Standing PR & Auto-Merge Rule

Every completed change in this repository follows this delivery loop — never push directly to `main`:

1. Create a focused branch: `<type>/<short-description>` (`feat/...`, `fix/...`, `chore/...`, `docs/...`, `refactor/...`).
2. Run `./scripts/agent_check.sh` and the full relevant test suite; confirm green output.
3. Commit using Conventional Commits where the subject explains *why* (imperative, lowercase, no trailing period, **zero AI attribution**).
4. Push the branch and open a Pull Request using `.github/PULL_REQUEST_TEMPLATE.md` with real verification evidence.
5. Auto-merge once verified:
   - Wait for any configured CI checks (`gh pr checks --watch`).
   - Try `gh pr merge --auto --squash --delete-branch`; when server-side auto-merge is unavailable on the repo and checks are green, merge immediately via `gh pr merge --squash --delete-branch`.
6. Return to `main` and fast-forward (`git checkout main && git pull --ff-only`).

## Always-On Engineering Invariants

- **Antigravity-Native Customizations:** Keep workspace rules in `.agents/rules/*.md` and on-demand runbooks in `.agents/skills/<name>/SKILL.md`; use root `AGENTS.md` as the single repo contract (do not add `GEMINI.md`).
- **Strict Red → Green → Refactor TDD:** Reproduce bugs or specify new behavior with a failing test first; never skip, delete, or weaken tests to pass.
- **Verification Before Claims:** State the expected positive success signal and confirm it in actual command output before opening or merging a PR.
- **Zero Secrets & Clean Tree:** Never stage or commit `.env`, credentials, `.DS_Store`, or local agent runtime directories (`.antigravitycli/`, `.remember/`).
- **Parallel Isolation:** When multiple agents run concurrently, assign disjoint file lists and isolated git worktrees.
