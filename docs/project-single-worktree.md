# One project, one working checkout

A project with worktree isolation enabled owns one named branch and one checkout
under `<repository>/.worktrees/`. All task workers and retries reference that
checkout. A worker is an execution record, not a Git workspace owner.

Tasks execute one at a time. The durable planner claim also governs direct worker
starts, so a UI Start action cannot create a second writer. A task's submitted
result stops its executor before verification. Committed results accumulate in the
project branch; per-task receipts and retained artifacts stay independently
reviewable. Finished workers are held for review, not left running over the next
task's files.

Project acceptance composes current main into the project branch, verifies the
whole candidate, and binds human review to that exact commit. Verification commands
still use disposable checkouts where an independent immutable test environment is
required; these are not additional worker-owned workspaces. The project page links
to its canonical working checkout, rather than whichever worker happened to run
most recently.

Approval publishes the reviewed result. Cleanup checks current remote main,
terminal tasks, retained assets, stopped workers, and a clean checkout before
removing the project worktree without force. Workers expire with their records,
original branch references, messages, and retained artifacts available for review.
A changed candidate or an unpublished/uncommitted change prevents unsafe cleanup.

Existing clean worker checkouts are consolidated once. Original workspace records,
commit references, and local control receipts are retained in Amux's private
`project-checkout-imports` directory. Clean branches merge deterministically;
conflicts reopen the owning task through ordinary bounded project repair. A source
checkout is removed only after its exact head is contained in the project branch.
Dirty or changed source checkouts are preserved and reported in logs.

Choosing the explicit shared-checkout option still uses the repository directory
instead of creating a worktree. It has the same single-writer scheduling rule.
