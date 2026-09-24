# Project UI: progressive disclosure

The first view should let someone who has never used Amux answer: **Is my requested result done? What is happening now? Do I need to act? Where can I inspect the proof?** Task attempts, models, tokens, checkouts, and raw receipts remain accessible, but should not compete with those answers.

## Information hierarchy

1. **Overview (first glance):** show the current state, verified outcomes, unfinished tasks, one next step, the whole-project verdict, four relevant tasks, and a route to evidence. Distinguish a held task from a failed whole-project check. Show the publish/closeout result prominently after approval.
2. **Overview (on demand):** keep adding or refining the desired outcome in a disclosure. Keep the timeline and checkout/configuration behind a separate disclosure. Reopen a saved unsent draft so it cannot be lost from sight.
3. **Tasks:** retain the full kanban and task inspector for a person investigating one item. The overview task links open the corresponding inspector.
4. **Evidence:** show whole-project acceptance and its decision controls, then group retained files by task. The standard Files viewer opens each artifact. Full criteria and individual files expand on request; no evidence is silently omitted.
5. **Workers, Dependencies, Settings:** show worker lifecycle and a compact task count first, expanding checkout/model/history per worker. Keep explicit dependency details and telemetry here rather than on the landing view.
6. **Fleet Workers:** put project-owned workers in a dedicated bottom accordion above review-held workers. They do not also appear in the normal, paused, review, or archived lists. Each bottom accordion has a right-edge actions menu whose membership and eligible actions come from its own visible group. Project worker history remains accessible after expiration.

## State and interaction rules

- A task hold is a task-level condition. The overview must not say that whole-project verification failed unless an acceptance check actually failed.
- Acceptance awaiting a person presents a direct path to the evidence and decision controls. No approval button appears before review.
- Live refreshes preserve open disclosures, focused controls, the unsent outcome draft, and the selected task. A changed count updates without resetting the user's reading position.
- Group actions show the exact count of eligible workers, use the existing confirmation flow, and act on the members of that group after the current search. Project workers are never included in the ordinary “All shown” sweep.
- On a narrow screen, cards stack, controls have touch-sized targets, long paths wrap, and menus stay within the viewport. The project selector replaces the desktop sidebar without hiding status or review.

## Verification scenarios

- Active project with many tasks and artifacts: the first screen stays short; current work, holds, and the route to all tasks/evidence are visible.
- Accepted project: approval/publish state and retained proof are distinguishable from mere task completion.
- Human-review candidate: reviewing evidence reveals every criterion and decision control before approval.
- Project worker fleet: project workers appear in exactly one group, above “Retained for review”; a group action count matches eligible workers and does not include other groups.
- Mobile viewport: no horizontal overflow or overlapping controls; Overview, Evidence, Tasks, and group menus remain usable.
