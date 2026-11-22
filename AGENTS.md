# AGENTS.md: How Coding Agents Work in This Repo

This repository uses **Antigravity customization conventions** as the default agent-driven setup (`AGENTS.md` + `.agents/rules/` + `.agents/skills/`). Do not create redundant tool-specific root files such as `GEMINI.md`.

## Repo Layout

| Path | Role | Rule |
| --- | --- | --- |
| `AGENTS.md` | Canonical engineering contract | Single root entrypoint for coding agents. |
| `.agents/rules/` | Always-on & contextual workspace rules | Antigravity-native rules (`trigger: always_on` or `model_decision`). |
| `.agents/skills/` | On-demand project skills | Add `<name>/SKILL.md` runbooks here as recurring workflows emerge. |
| `.github/PULL_REQUEST_TEMPLATE.md` | PR template | Every PR includes summary + pasted local verification evidence. |
| `.github/workflows/ci.yml` | CI workflow (currently disabled at repo level) | Kept in sync with `scripts/agent_check.sh` so CI works immediately when enabled. |
| `scripts/agent_check.sh` | Fast local verification gate | Run before every commit and PR. |
| `docs/` | Design docs (`docs/design/`) and ADRs (`docs/decisions/`) | Update docs alongside behavior or architecture changes. |

## Verification Gates

> **Local-only verification policy (for now):** `.github/workflows/ci.yml` is checked in and ready, but GitHub Actions is disabled at the repository level while this repo is private without Actions quota. Run all verification gates locally before opening or merging a PR.

```bash
# Gate 1: Fast static & repo invariant check (< 2s)
./scripts/agent_check.sh
```

As the codebase grows, register lint, typecheck, and unit/integration test commands in `scripts/agent_check.sh` (or dedicated test scripts) and climb gates from fastest to heaviest.

**Verification discipline:** State the expected positive success signal before running a check, then verify it in real output before declaring success or merging.

## Git, PR & Auto-Merge Rule

This repository uses a standing **branch → PR → auto-merge** workflow for all changes:

1. **Never push directly to `main`.** Every task lives on its own branch named `<type>/<short-description>` (e.g., `feat/model-router`, `fix/retry-backoff`, `chore/init-agent-dev-setup`).
2. **Conventional Commits, zero AI attribution.**
   - Format: `<type>(<scope>): <description>` (imperative mood, lowercase, no trailing period).
   - Subject must answer *why* the change exists, not merely restate the diff.
   - **Never** include `Co-Authored-By:` lines naming an assistant, "Generated with ...", or any AI branding.
3. **Verify locally before pushing.** Run `./scripts/agent_check.sh` and all relevant tests; fix failures before opening the PR.
4. **Open a PR using `.github/PULL_REQUEST_TEMPLATE.md`.** Include concrete summary bullets and pasted verification evidence.
5. **Auto-merge once verified.**
   - If CI checks are configured on the repo, wait for them (`gh pr checks --watch`).
   - Enable auto-merge (`gh pr merge --auto --squash --delete-branch`), or when server-side auto-merge / required branch protection is unavailable on a private repo and all checks are green, merge immediately via `gh pr merge --squash --delete-branch`.
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
