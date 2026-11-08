# Stranded todo cards, 2026-10-30 (AF-1048)

`board.todo_is_reachable_by_dispatch` fails: **49 of 141 managed todo cards (34%)
belong to workers that are not registered**, so dispatch can never offer them.
The count is the invariant's own (`/api/health/invariants`, 19:30Z), and the
card list below matches it: 49 cards (status `todo`, not archived) whose
`session` is one of the 22 names it lists.

Nothing here was moved. Each card's fate is a choice about someone's work, so
this is a recommendation per card for the owner (ethos rule 8). The invariant's
own instruction is the rule applied: restore the worker, reassign to the
intended worker, or move to backlog, and never bulk-assign into one queue.

## How to read the recommendations

- **Reassign → X**: the work is live and X is the registered worker that owns
  that surface now.
- **Discard**: a test fixture, a chat capture that was answered in its chat, or a
  card whose own title says "no action needed".
- **Owner**: the card is an ask of the owner and dispatch should not reach it
  (see the `ETHAN` note at the end).

## By former worker

### ETHAN (12): the owner's queue

| card | type | title | recommendation |
|---|---|---|---|
| ETHAN-75 | escalation | Set MIXPEEK_PRIVATE_TOKEN as a GitHub repo secret | Owner |
| ETHAN-76 | code | DECISION: do pushdown Phases 2 and 3 still happen? | Owner (retype `decision`) |
| ETHAN-77 | code | kubectl apply the stateless-guard RBAC | Owner (retype `ops`) |
| ETHAN-79 | escalation | logo detection is marketed, 0 of 12 extractors ship it | Owner |
| ETHAN-83 | chore | 5 cards in ETHAN's todo are no longer dispatched | Discard once this report is read: it describes this same fault |
| ETHAN-84 | escalation | grant amux-ops logging.configWriter | Owner |
| MF-725 | escalation | TEAM_INVITE_ENABLED live on the worker, absent from the chart | Owner |
| MF-727 | escalation | Free-tier activation stopped on 2026-07-21 | Owner |
| AMUX-3463 | code | owner SMS/iMessage sends still fail | Reassign → amux |
| BACKE-3377 | code | agentic_enrich reason absent unless output_schema declares it | Reassign → mixpeek-general |
| MR-28 | code | alerting-rules.yaml has no CI/CD deploy path | Reassign → mixpeek-cicd |
| MR-6 | code | MVS Scroll drops sort_field, sorts in Python behind a 10k cap | Reassign → mvs-infra |

### Chat captures (13): chats that no longer exist

| card | worker | title | recommendation |
|---|---|---|---|
| AC-433 | amux-chat | Structure request: put this in the ellipse at the top worker details | Reassign → amux, if the ask is still wanted; else discard |
| AC-434 | amux-chat | Structure request: send that scope to this worker | Discard (a relay instruction, done or moot) |
| ACW-12 | amux-chat-worker | Structure request: wdym it goes to live to workers | Discard (a question, answered in chat) |
| ACW-16 | amux-chat-worker | Structure request: wdym it goes to live to workers | Discard (duplicate of ACW-12) |
| ACW-14 | amux-chat-worker | Merge chat worker changes to main | Reassign → amux, after checking the changes still exist |
| AMHC-1 | amux-meta-helper-chat | unblock all goal-specs asks | Discard (a one-time permission relay from 2026-10) |
| AMHC-2 | amux-meta-helper-chat | Structure request: unblock everything yourself | Discard (same relay) |
| MOC-1 | mixpeek-override-chat | Structure request: questions about the md | Discard (questions, answered in chat) |
| MOC-2 | mixpeek-override-chat | Structure request: monthly min/max | Discard (question) |
| TPC-2 | tubescience-parity-chat | Audit existing Mixpeek resources and data import strategy | Reassign → tubescience-parity |
| TPC-3 | tubescience-parity-chat | Design and document collection/namespace schema | Reassign → tubescience-parity |
| CD-106 | chat-demo | Verify message sent to @amux regarding chat rendering | Discard (demo) |
| CD-107 | chat-demo | Create task for chat output rendering in amux | Discard (demo) |

### Test and probe workers (6)

| card | worker | title | recommendation |
|---|---|---|---|
| RPC-1 | restart-probe-chat | Write 600-word ocean tides explanation | Discard (probe payload) |
| VLBW-1 | verify-launch-bar-works | verify launch bar works | Discard (probe) |
| VOT-1 | verify-orchestrations-tab | verify orchestrations tab | Discard (probe) |
| TFPET-1 | test-fanout-parent-eph-TFP-15 | test_worker_consumes_every_routed_queue cannot follow -Q "$QUEUES" | Reassign → mixpeek-cicd: a real CI defect filed by an ephemeral fanout child |
| TFPET-2 | test-fanout-parent-eph-TFP-18 | AO worktrees run the MAIN checkout's pre-push via core.hooksPath | Reassign → mixpeek-cicd (same) |
| TFPET-3 | test-fanout-parent-eph-TFP-18 | tree-guards isolated path refuses on any failure with no delta rule | Reassign → mixpeek-cicd (same) |

### Retired helper and side lanes (18)

| card | worker | title | recommendation |
|---|---|---|---|
| AGH-5 | amux-gs12-helper | Benchmark orchestrate against Factory Missions and Cognition Devin | Backlog on amux |
| AGH-7 | amux-gs12-helper | Remove a proven duplicate or dead Amux execution path | Reassign → amux |
| AGH-8 | amux-gs12-helper | Make Amux CI/CD gates faster and more trustworthy | Reassign → amux |
| AGH-9 | amux-gs12-helper | Structure request: make CI/CD for amux your initiative | Discard (duplicate of AGH-8) |
| MO-3868 | amux-gs12-helper | GS12 WP18 namespace backup and restore, PITR for MVS | Reassign → mxp-gs12 (GS-12 work package) |
| MO-3881 | gs12-amux-helper | GS12 WP19 the consolidation pass | Reassign → mxp-gs12 |
| ATE-130 | amux-testing-e2e | Cold card deep links and History can lose to onboarding in E2E | Reassign → amux |
| ATE-79 | amux-testing-e2e | Local multiplayer secure across Tailscale HTTP/2 and PWA reloads | Reassign → amux |
| ATE-84 | amux-testing-e2e | Persist terminal board outcome summary durably | Reassign → amux |
| AC-416 | amux-codex | Diagnose three-user cloud tenant wake and realtime stalls | Reassign → amux |
| AG4GM-1 | amux-gs-4-gke-minimization | macOS taskgated-helper unloaded: KeePassXC, Tailscale CLI, Granola killed at launch | Reassign → security (needs sudo, so likely an owner step) |
| M1-2 | mixpeek-16 | Prove a Mongo restore: the DR drill has no Mongo leg | Reassign → mvs-infra |
| M2-1 | mixpeek-20 | cancelled-orphan reaper has never run in production | Reassign → mixpeek-general (was addressed to `backend`) |
| MB-29 | mixpeek-backend | GET /clusters/{id}/groups fabricates groups for runs that never executed | Reassign → mixpeek-general |
| MB-31 | mixpeek-backend | 1 card in mixpeek-backend's todo is no longer dispatched | Discard (describes MB-29's stranding) |
| MHC-548 | mixpeek-studio-codex | staged set held a stale revert; disarmed, no action needed | Discard (title says no action needed) |
| MSC-2 | mixpeek-studio-codex | server/openapi.json is yours not mine, no action needed | Discard (same) |
| MA-122 | mvs-23 | Local MVS: text_extractor vectors not persisting/searchable | Reassign → mvs-infra |

## Totals

| recommendation | cards |
|---|---|
| Reassign | 24 |
| Discard | 17 |
| Owner | 7 |
| Backlog | 1 |
| total | 49 |

## The `ETHAN` question

The invariant counts `ETHAN` as an unregistered worker, so the owner's own
queue reads as stranded work. Seven of those 12 cards are asks of the owner,
which dispatch should never hand a worker. Four (AMUX-3463, BACKE-3377, MR-28,
MR-6) are worker code that landed in the owner's queue, and one (ETHAN-83) is a
stale report of this same fault. Excluding `ETHAN` wholesale would hide the
four. Recommended: keep counting `ETHAN` cards whose type is `code`, and retype
ETHAN-76 and ETHAN-77, which are owner asks filed as `code`, so the check sees
them correctly. Asks and escalations waiting on the owner are the queue working
as intended.
