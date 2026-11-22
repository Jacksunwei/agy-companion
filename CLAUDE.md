# CLAUDE.md

This repository's canonical engineering contract is [`AGENTS.md`](AGENTS.md). Read it in full before editing code.

## Quick Commands

```bash
# Fast local invariant & verification gate (also run in CI)
./scripts/agent_check.sh
```

## Non-Negotiable Rules

- **Branch → PR → Auto-Merge:** Never push directly to `main`. Work on `<type>/<short-description>`, pass `./scripts/agent_check.sh` and tests, open a PR via `.github/PULL_REQUEST_TEMPLATE.md`, wait for CI green, squash-merge (`gh pr merge --auto --squash --delete-branch` or `gh pr merge --squash --delete-branch`), and fast-forward local `main`.
- **Clean Commits:** Conventional Commits answering *why*, with **zero AI attribution**.
- **Strict TDD:** Red → Green → Refactor; never skip, delete, or loosen tests to get green.
- **Design & Docs:** Keep `docs/design/` and `docs/decisions/` in sync with behavior changes.
- **Repo Hygiene:** Never commit secrets, `.env` files, OS artifacts, or local agent caches.
