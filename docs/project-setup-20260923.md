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
