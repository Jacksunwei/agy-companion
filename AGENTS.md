# AGENTS.md: How Coding Agents Work in This Repo

This file is the canonical engineering contract for coding agents working in `agy-byom`. Read it before editing code, opening a PR, or modifying workflows.

## Repo Layout

| Path | Role | Rule |
| --- | --- | --- |
| `AGENTS.md` | Canonical agent engineering contract | Single source of truth; keep `CLAUDE.md` and `GEMINI.md` aligned as thin entrypoints. |
| `.agents/rules/` | Always-on workspace rules | Loaded automatically by Antigravity / compatible coding agents. |
| `.github/` | PR template & CI workflows | Every PR uses `PULL_REQUEST_TEMPLATE.md` and passes CI. |
| `scripts/agent_check.sh` | Fast local verification gate | Run before every commit and PR; CI runs the exact same script. |
| `docs/` | Design docs (`docs/design/`) and ADRs (`docs/decisions/`) | Update docs alongside behavior or architecture changes. |

## Verification Gates

Run checks from fastest to heaviest before committing or opening a PR:

```bash
# Gate 1: Fast static & repo invariant check (< 2s)
./scripts/agent_check.sh
```

As the codebase grows, register lint, typecheck, and unit/integration test commands in `scripts/agent_check.sh` and `.github/workflows/ci.yml` so local checks and CI stay identical.

**Verification discipline:** Before running a check, state the positive success signal you expect, then verify it in real output before declaring success or merging.

## Git, PR & Auto-Merge Rule

This repository uses a standing **branch → PR → auto-merge** workflow for all changes:

1. **Never push directly to `main`.** Every task lives on its own branch named `<type>/<short-description>` (e.g., `feat/model-router`, `fix/retry-backoff`, `chore/init-agent-dev-setup`).
2. **Conventional Commits, zero AI attribution.**
   - Format: `<type>(<scope>): <description>` (imperative mood, lowercase, no trailing period).
   - Subject must answer *why* the change exists, not merely restate the diff.
   - **Never** include `Co-Authored-By:` lines naming an assistant, "Generated with ...", or any AI branding.
3. **Verify locally before pushing.** Run `./scripts/agent_check.sh` and all relevant tests; fix failures before opening the PR.
4. **Open a PR using `.github/PULL_REQUEST_TEMPLATE.md`.** Include concrete summary bullets and pasted verification evidence.
5. **Auto-merge once green.**
   - Wait for CI checks to pass (`gh pr checks --watch`, if checks are configured).
   - Enable auto-merge (`gh pr merge --auto --squash --delete-branch`). If server-side auto-merge is not enabled on the repository (e.g., private repo without GitHub Pro auto-merge or required branch protection) and all checks are green, merge immediately via `gh pr merge --squash --delete-branch`.
6. **Keep local `main` clean.** After merge, switch back and fast-forward:
   ```bash
   git checkout main && git pull --ff-only
   ```

## Engineering Principles

1. **Design first.** For non-trivial changes, document intent and boundaries in `docs/design/` and significant trade-offs in `docs/decisions/` alongside the code.
2. **Strict Red → Green → Refactor TDD.** Write a failing test reproducing the bug or specifying the new behavior first, implement the minimal fix to go green, then refactor. Never skip, delete, or weaken tests to get green.
3. **Small, reversible changes.** Prefer focused PRs with a clear thesis over sprawling multi-concern diffs. Keep pure domain logic decoupled from I/O and external adapters.
4. **No secrets or local junk.** Never commit `.env`, credentials, tokens, OS metadata (`.DS_Store`), or agent runtime directories (`.antigravitycli/`, `.remember/`).
5. **Parallel agent hygiene.** When running multiple agents concurrently, give each authoring agent an isolated git worktree and disjoint file ownership.

## When to Stop and Ask the Human

- The task requires changing a core architectural decision, license, or external public contract.
- Adding a new paid/sensitive external service, credential flow, or unapproved major dependency.
- A test cannot pass without weakening assertions, or requirements are genuinely ambiguous across materially different designs.
