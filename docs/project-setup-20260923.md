# Project setup preserves the requested outcome

Found while using Projects on 8824 to submit a full repository goal specification.
The previous setup helper used the global model instead of the selected planner,
proposed human-only review for runtime work, and discarded the original description
when creating the policy. An empty verification field also left the form unable to submit.

Creation now writes policy and the exact initial request in one store transaction.
A stable identity survives uncertain HTTP replies and reloads; replay cannot create
another command or overwrite changed policy. Invalid input rolls back both records.
The description uses the existing local settings draft store.

Drafting uses the selected project provider/model and the same repository-scoped spec
context as command intake. Runtime requests require a validated execution contract.
Proposed whole-project verifier scripts are explicit deliverables, not claimed existing
files. The per-task baseline falls back to `git diff --check`; each task still requires
its own falsifiable criteria checks. Independent whole-project runtime checks and human
artifact review remain separate gates.

A live delivery check also exposed starvation in session refresh: continuous SSE events
reset the debounce indefinitely. The existing coalescing timer now keeps its first
deadline, so status and queue reads continue during busy traffic.

Validation: 17 project API/unit tests; three dashboard function regressions; 25 outbox
regressions. The original connection-recovery evidence remains separate. Live project
execution and its implementation evidence are not implied by these harness tests.

The first live rerun on `f7f9b6a6` proved selection reached Codex, then caught a
service-manager PATH mismatch: a broken npm shim shadowed the working provider
installed in the worker login environment. Helper discovery now matches workers
and the existing account probe. Arguments remain literal, tools/hooks stay disabled,
and explicit CLI overrides remain authoritative. This is one provider invocation,
not a fallback call. Project/outbox/state regression scripts are also wired into CI.

The next live retry exposed a separate transport defect: the generic mutation
outbox aborted the long-running draft after 15s and repeatedly invoked the planner.
Project drafting is now excluded from automatic replay, including already-retained
outbox entries. Actual create/command mutations remain durable. The draft's outer
and browser deadlines exceed the helper's bounded I/O deadline. Pending draft
requests can be dismissed in the Connection modal; their original form stays saved.

Schema rejection on the next single-call run exposed missing evidence-path
constraints in the setup prompt. Those constraints now reach the planner. A
validation error gets one bounded same-model correction with the prior response
and precise diagnostic; provider failures do not trigger retries or fallbacks.

Goal 03 also exceeds the old 16,000-character preview, omitting later Studio,
API-suite and standalone acceptance bodies. The bounded preview now retains up
to 64,000 characters per file (three files maximum), and explicitly marks larger
previews. A regression requires complete later-section details to reach setup;
the existing oversized source still must account for every section exactly once.

The corrected live draft was accepted on its second bounded attempt. The operator
created `bucket-objects-gs3` through the UI, retaining the initial request and 22
acceptance criteria. Review caught a weak Studio gate (one passing capability could
pass a multi-capability requirement); the reviewed contract now also requires zero
failed and zero missing capabilities. Setup guidance now requests this complete
coverage pattern and shared fixture reuse instead of an arbitrary two-command cap.
This remains semantic model guidance plus human review, not a guarantee that a
generated contract is exhaustive. No runtime result is claimed at project creation.

The first real intake retained two 23-section model responses but failed to create
tasks: whole-file keyword scanning imported Ray from another project's background,
and the admin-step guard mistook "commit the contract" inside a requested output
for a separate commit-only task. Scope checks now use indexed requirement bodies,
while the model still sees the complete document. Admin checks inspect outcome
titles instead of punishing normal producing-task instructions. A validator
revision rechecks exhausted retained responses once without another model call;
invalid responses remain bounded and a restart cannot consume valid recovery.

Live recovery on 8ca24c26 spent no model calls but exposed a later wording
rejection: "build and run standalone Docker image" was not among the accepted
Docker run phrases. A local replay reproduced the exact failure. Equivalent run
wording is now accepted, named-service checks use words (not substrings inside
"arrays"), and rejected retained-plan revalidation persists the new diagnostic
rather than showing an old error. Validator revision 3 rechecks saved plans once.
