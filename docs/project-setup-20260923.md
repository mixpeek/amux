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
