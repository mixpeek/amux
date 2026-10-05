# Orchestration contract

Adopted by Ethan on 2026-10-05 ("adopt them all"), after a week of goal spec 12
showed where amux coordinates well and where it does not measure, protect or
offload. The harness enforces these rules through board transitions, git, launch
configuration and the steering queue. Prompts may describe a rule; they never
enforce one. Owner of the harness work: `amux-helper`.

Each rule is a boundary (gets more valuable as models improve) or a scaffold
(compensates for today's models; ships behind a preference and a counter, and is
retired when its counter says it no longer fires usefully).

Design constraint for every change: fewer moving parts and fewer injected
tokens. A rule that adds a mechanism must delete the mechanisms it replaces in
the same phase, and say which.

## Rules

| # | Rule | Kind | Enforced at | Replaces / deletes |
|---|---|---|---|---|
| 1 | A card cannot enter `doing` without frozen acceptance criteria and a typed verification kind (`code`, `deploy`, `proof`, `artifact`); changing them after needs the owner | boundary | board transition | free-text gate lists the worker writes itself |
| 2 | `done` is granted by the server: it measures the commit and runs or observes the check for the card's kind; tests are read-only during the card; a typed `cannot_satisfy` exit replaces worker `force` | boundary | board transition | the evidence text heuristic, duplicate evidence and gate-ack blocks, worker `force` |
| 3 | `verified` needs a reviewer the harness spawns (fresh, read-only, different model where available, not the author); at most three fix rounds, then the owner | boundary + scaffold (cap) | board transition | prose reviewer matching, self-verification |
| 4 | One writer per worktree by default; a failed worktree stops the launch | boundary | launch | the shared-checkout fallback, staged-guard, git-shared-guard, freshness hook |
| 5 | No worker pushes to main; a server queue composes, verifies the composed tree, merges or returns the output | boundary | git + server | client-side `amux land` queue, push-consent, attribution and append-only push guards |
| 6 | Every card carries turn, time and spend budgets and a repeat breaker; exhaustion moves it to `needs-split`. Provider limits, crashes and stalls are told apart | scaffold (numbers) | steering + board | per-producer nudge cooldowns |
| 7 | A card boundary is a fresh session seeded by a harness-built brief; mid-card compaction stays with the provider | scaffold | launch | manual `amux fresh` as the only reset |
| 8 | Closing a card records what was left undone; each item becomes a card or a dismissal with a reason | boundary | board transition | "Structure request" capture cards and their reconcilers |
| 9 | Hub and spoke: peer messages are data, never approvals; no peer delegation; signals are the only clearance channel | boundary | steering | pane peeking as a clearance channel |
| 10 | Bypass flags run only inside a filesystem and network sandbox; credentials stay with the server; each worker gets a minted token instead of a self-asserted header | boundary | launch + API | header-only identity, credentials in worker env |
| 11 | Human gates sit at plan approval and boundary crossings (money, outside parties, production data, credentials), bind to an exact artifact, are single use, and fail closed. Auto-approve never covers scope, money or production data | boundary | board + policy | keyword-matched standing approvals for those categories |
| 12 | Supervision is deterministic first; no model holds kill, merge or push authority | boundary | runtime jobs | (keep) |
| 13 | Injected instructions and memory have byte budgets; a binding rule links the failure that earned it | boundary | scope + memory writer | unbounded composed memory, always-loaded long files |
| 14 | Every rule counts blocks, overrides and correct overrides; a rule that never fires or is always overridden is retired. Providers without status hooks and cost coverage run observe-only | boundary | counters | per-feature env flags nobody reads |

Additions from goal spec 12:

| # | Rule | Enforced at |
|---|---|---|
| A1 | Planned production changes (restarts, migrations) run as a server procedure: notice to the owner 30+ minutes ahead, blue/green, a positive control before traffic, automatic hold on failure | server procedure + board |
| A2 | A deterministic runner under the orchestrator dispatches cards, routes idle lanes, starts proof runs when their inputs verify and closes roll-ups; the orchestrator model decides what is in the queue | runtime job |
| A3 | A project's done line (its proof cards) is frozen and measured from day one; scope changes are versioned decisions with the owner's name | board |
| A4 | Stateful systems get a production-scale test bed; restore, replay and scale tests run there before production | environment |
| A5 | Capacity is a scheduling input: provider usage caps, host pressure and the land queue gate dispatch | runner |

## Phases

| Phase | Rules | Exit, measured |
|---|---|---|
| 0, stabilise | consolidation, counters honest | deploy success > 80% over 24 h; open gs12 cards < 200; no orchestrator stall > 30 min |
| 1, make done real | 1, 2, 3, 14 | < 5% of verified cards reopened on audit |
| 2, take mechanical load off the model | 5, A2, 6, 7, 8, 4 | orchestrator messages halve; land wait p95 < 15 min; no lane idle with work available |
| 3, boundaries | A1, 11, 10, 13, 9 | zero unannounced production restarts; zero scope changes without a bound owner approval |

## Acceptance: measured on goal spec 12

Ethan, 2026-10-05: "make sure you monitor gs12 work as the acceptance criteria
for these rules". A rule is accepted when GS-12 shows its effect, not when its
code merges. Rollout per rule: a short dogfood on the `amux` lanes, then the
`gs12-platform` group, then the fleet. The hourly GS-12 check (SCHED-543)
reports each measure that applies, with its baseline from 2026-10-05.

| Rule | GS-12 measure | Baseline (10-05) | Target |
|---|---|---|---|
| 1 | gs12 code cards entering `doing` with a frozen contract | not measured | 100% |
| 2 | gs12 `done` granted by a server check; verified cards reopened on audit | 0%; 15% (65 of 438) | 100%; under 5% |
| 3 | gs12 `verified` with a harness reviewer | 0 of 373 | 100% of code cards over the size threshold |
| 4 | shared-checkout incidents in gs12 lanes | gs12 lanes already isolated | 0 |
| 5 | land wait p95; worker pushes to main; batches refused for another lane's change | 45 min; all; 15 min of refusals | under 15 min; 0; 0 |
| 6 | nudges per gs12 lane per day; cards moved to needs-split | about 8 KB/lane/day | down; counted |
| 7 | median compaction generations per gs12 lane | lanes run for days | under 3 |
| 8 | open gs12 cards; closes with a left-undone list | about 675 | under 200; 100% |
| 9 | peer messages with no ask | 82% (09-14 sample) | under 20% |
| 10 | gs12 lanes with credentials in env; bypass lanes outside a sandbox | 20 of 20; 20 of 20 | 0; 0 |
| 11 | scope, money or production-data auto-approvals | 7 deferrals on 10-05 | 0 |
| 12 | kills, merges or pushes decided by a model | 0 | 0 |
| 13 | shared MEMORY.md bytes in ~/Dev/mixpeek; launch bytes per lane | 61,012 B; about 125 KB | under 12,000 B; down |
| 14 | rules with live counters | 0 of 19 | 19 of 19 |
| A1 | unannounced production restarts; bad restores that took traffic | 1; 1 (about 7 h) | 0; 0 |
| A2 | orchestrator queue stalls per day; lanes idle with work available | 81; 2 | under 5; 0 |
| A3 | proof cards verified per day | about 1.4 | rising to finish |
| A4 | MVS restore, replay and crash tests run on the test bed before production | 0 | all |
| A5 | lanes dispatched into a usage cap or host pressure | 7 to 15 lanes, 5 times on 10-04/05 | 0 |

Progress and evidence for each rule live on its card on `amux-helper`'s board,
tagged `contract`. This file changes only when a rule changes.
