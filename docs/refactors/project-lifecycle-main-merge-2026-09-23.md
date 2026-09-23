# Project lifecycle merge verification — 2026-09-23

## Integration strategy

Merge `codex/amux-project-lifecycle` at
`b9f8c4971f6c1c58f0c20898f063a78b8704f954` into main at
`8a33c9d147fc9c9e44063e42d704047a22a90cd7`, preserving both histories.
The branches diverged at `7baa322501c97fbf12ca705dd4acfcfec9abbd6e`.
Main had 90 commits and the lifecycle branch had 42 commits beyond that base.

Resolve overlapping behavior in favor of the project lifecycle design. Preserve
main's independent fixes, including browser startup grace, held email edits,
worker lease release, transcript rendering, board completion gates, reminder
identity, runtime diagnostics, read-pool sizing and model catalog refresh.
Audit the result against main as well as the branch: all 177 newly added named
functions/types inspected from main remain present.

The terminal poll implementations had duplicate declarations after textual merge.
Consolidate them into one polling path retaining in-flight exclusion, stale
session generation handling and main's bounded polling cadence.

## Gaps found and corrected during validation

- Resolve worker checkout display from the durable workspace record, so project
  terminals show the repository-local worktree rather than the parent checkout.
- Keep a parent waiting on its own required outputs in progress; distinguish
  that normal wait from an executor failure in the outcome verdict.
- Add the closeout API to the route census.
- Include documentation in the cloud Docker build context because the server
  embeds the project harness contract at compile time.
- Update lifecycle fixtures for repository-local worktrees, the independent
  project driver, committed static verification scripts and publish-gate receipts.
- Load the actual production review-action helper in sliced JavaScript tests.

## Checks

- All-target workspace Clippy passed with warnings denied.

- Workspace library tests: 234 core tests passed; 2,999 server tests initially
  passed, with two outdated fixtures failing. After correcting those fixtures,
  all 119 selected project tests passed, including both failures. Ten server
  tests were ignored in the full unit run; the project rerun ignored one.
- All 74 server integration targets executed: 426 passed, 0 failed, 26 ignored
  across the initial run and the corrected remaining-target runs.
- Browser state tests: 27 passed. Outbox tests: 16 passed. Outage recovery and
  project verdict tests: 49 passed.
- SPA lint: zero errors; 51 existing unused-variable warnings.
- Targeted worker API regression confirms recorded worktree path and active
  state; its ordinary-worker negative control also passes.

## Fresh-server UI lifecycle proof

Run an isolated server on port 18973 with a temporary Git repository, local bare
remote, private tmux socket and deterministic nonbillable provider fixtures.
No real model quality or production deployment is claimed by this fixture.

Through the browser, create `merge-proof-v2`, request parallel alpha/beta reports,
and observe two executors in separate repository-local `.worktrees` checkouts.
Both children complete on attempt one. The parent outcome becomes verified
only after both children. All three tasks reach verified; the composed candidate
waits for human artifact review with its executors stopped and retained.

Open both Markdown reports in the standard Preview/Raw file viewer. At an
approximately 375 CSS-pixel phone viewport, review the artifacts and approve
through the UI. The closeout action publishes candidate
`b31791c6b0e861b85cde3b4294e5189d13c2190b` to the fixture's remote main,
expires both executor records and removes their worktrees. The project Workers
page shows both expired records, integrated heads, removed checkouts and one
retained asset per worker. Worker cards fit the phone viewport without horizontal
content overflow. Restore the browser viewport after testing.

The first fixture attempt is preserved paused as diagnostic history: its obsolete
inline shell verification command was correctly refused by the harness. Updating
the fixture to commit a verification script made the new run pass; no product
verification gate was relaxed.
