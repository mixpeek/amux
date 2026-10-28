---
description: Review, improve, validate, and merge a PR into main following repository best practices.
allowed-tools: Bash, Read, Edit
argument-hint: [PR number or URL]
amux-managed: true
---

# Instructions

The user's request is: **$ARGUMENTS**

Own the PR from review through verified merge into `main`.

1. **Understand** — Read the PR, diff, linked issues, repository instructions (`AGENTS.md`, `CLAUDE.md`), conventions, and acceptance criteria.
2. **Review** — Identify correctness, architecture, security, performance, maintainability, compatibility, and test coverage issues.
3. **Improve** — Fix defects, simplify code, address root causes, and enhance implementations where appropriate. Follow existing patterns and avoid unnecessary complexity or scope creep.
4. **Validate** — Run relevant tests, linting, type checks, builds, and integration checks. Add missing tests, fix failures, and repeat until passing.
5. **Reconcile** — Ensure compatibility with current `main`, resolve conflicts, and preserve unrelated changes.
6. **Merge** — Commit and push improvements to the PR branch. Once required checks and reviews pass, merge into `main` following repository policy. Verify merge status and post-merge CI.

## Principles

- Follow repository-specific best practices and established abstractions.
- Prefer minimal, robust, production-ready solutions.
- Fix issues directly rather than merely reporting them.
- Preserve backward compatibility unless explicitly authorized otherwise.
- Never bypass branch protections, required reviews, or failing checks.
- Operate autonomously and escalate only genuine blockers.
- Do not claim success without verification.

## Completion

Report the PR, improvements made, validation results, merge commit, and remaining risks. The task is complete only after the merge is verified.
