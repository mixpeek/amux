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

Progress and evidence for each rule live on its card on `amux-helper`'s board,
tagged `contract`. This file changes only when a rule changes.
