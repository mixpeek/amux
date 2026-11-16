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

Rule 10's minted token reaches every lane, not only contract lanes (2026-11-09):
each launch of a Claude, Codex, Gemini or isolated worker mints one unless the lane
lists 10 in `AMUX_CONTRACT_RULES_OFF`. Contract lanes are refused without it as
before. Other lanes follow `AMUX_WORKER_IDENTITY`: `report` (default, logs
`worker_identity_would_refuse` and lets the request through), `enforce` (refuses
403) or `off` (the old opt-in behaviour). `GET /api/contract/counters` shows the
counts under `worker_identity`; switch to `enforce` once `would_refuse` stays 0.

Locality is no longer a credential either (2026-11-09, `api/loopback_trust.rs`).
A request from this machine proves itself like any other: the owner bearer, a
member cookie, a lane's minted token, or, on a host behind a sign-in door with no
owner token, the front-door owner secret (`~/.amux/front-door-owner`, which the
door returns to Caddy for the owner and the owner's own CLI reads). What an
uncredentialed local request costs is `AMUX_LOOPBACK_TRUST`: `report` (default,
admitted, counted and named by caller, `loopback_would_be_refused`), `enforce`
(401 `loopback_not_trusted`; `AMUX_RS_NO_LOOPBACK_BYPASS=1` means this) or `off`.
`GET /api/contract/counters` shows it under `loopback`, with
`uncredentialed_callers` naming who still relies on locality.

A connector token is minted for whoever the request proves, not for whoever a
header names or omits (2026-11-09, `api/connector_scope.rs`). A lane's token
holds the bearer to that worker's connectors scope; the owner, a member or an
in-process job (its `org::policy_headers("job:<name>")` mark) prove themselves
as for any other route; a request that proves nobody is anonymous. A worker
header alone still narrows the bearer to that worker's scope, since a name can
only restrict. `AMUX_CONNECTOR_TOKEN_SCOPE`: `report` (default, minted, counted,
`connector_token_would_be_refused`), `enforce` (403 `connector_token_refused`
with how to prove; a lane not yet minted a token is still served and counted) or
`off`. Every mint writes a receipt (`GET /api/policy/receipts`, action
`connector_token_mint`) with actor, on-behalf-of, account, scopes and verdict,
never the bearer. Counts are under `connector_token` in
`GET /api/contract/counters`.

A published app is its own principal with no amux access unless a person gave
it some (2026-11-09, `api/app_identity.rs`). Publishing a port mints an app
credential, returned once and stored only as its hash; the app sends it as
`X-Amux-App-Token` and is attributed `app:<id>` on behalf of its publisher,
held to the calls a person allowed (`api_allow`, `METHOD /api/path` rules; a
worker can only ask, `api_requested`, and approving the link allows exactly
that). It can never be given its own access controls, members, sign-in,
connectors, the vault, grants or scopes. On Linux a loopback call from the
app's process tree without its token is caught by process, including one that
carries the worker token it inherited or the owner token it read from disk.
`AMUX_PUBLISHED_APP_API`: `report` (default, allowed, counted per app and kind,
`published_app_api_would_be_refused`), `enforce` (403 `published_app_api_refused`
with what to do) or `off`. Counts are under `published_app` in
`GET /api/contract/counters`. Still open: a process the app detaches from its
tree, a non-Linux host, or a non-loopback call carrying the owner token is not
caught; governance PRs 16-17 close that.

A held external email is seen and decided only by who may (2026-11-09,
`api/email_scope.rs`). The pending list is scoped to the caller the server
proved: the owner, an owner policy and a Global member see every draft, a
scoped member the drafts of the workers in its scope, a worker only its own,
an app or an unproven caller none. Approve and reject record the proven
identity with `approver_verified: true`, never the name `X-Amux-Approver`
claims; a worker may withdraw only its own draft and never approve; an app can
do neither; a scoped member decides only inside its scope, in every mode but
`off`. Every external send that `AMUX_EMAIL_EXTERNAL_ALLOW` or
`AMUX_EMAIL_EXTERNAL_EXEMPT` lets skip the hold leaves a ledger row and a
policy receipt (`email_external_bypass`) naming the rule and its layer.
`AMUX_EMAIL_APPROVAL_SCOPE`: `report` (default, allowed and counted,
`email_scope_would_refuse`), `enforce` (403 with the next step) or `off`.
Counts are under `email_gate` in `GET /api/contract/counters`.

**The ledger** (`api/ledger.rs`, governance PR 7) is the record every gate
above writes to. A finished command (from `_amux_interactions`) and every
policy receipt land once in `_amux_ledger`, append-only (triggers refuse
UPDATE and DELETE) and hash-chained. The actor is what the server proved
(`owner`, `member:<email>`, `policy:<name>`, `worker:<name>` with its token,
`app:<id>` with its token); a name only asserted goes in `claimed` and the
actor is `anonymous`. Presence, status reports, typing and read markers are
not recorded. `GET /api/ledger?actor=&resource=&kind=&trace_id=&since_h=`
pages it newest first; `GET /api/ledger/verify` walks the chain and names
the first changed or missing row; `/api/audit` reads it as `kind=ledger`.
`AMUX_LEDGER_RETAIN_DAYS` (default 365) prunes a prefix and moves the anchor,
so the rest still verifies; deleting a worker keeps its rows. `AMUX_LEDGER=off`
stops appends. A failed append never fails the request: it logs
`ledger_append_failed`, and verify and `ledger` in `GET
/api/contract/counters` count it.

**One trace per unit of work** (`api/trace.rs`, governance PR 8). A request
without a W3C `traceparent` is an origin and the server mints a trace there;
one with a valid header continues it. Every response names its trace in
`traceparent`. A schedule firing on the cron path mints its own trace and is
recorded in the ledger as its root (`kind=schedule`). Delivering a message to
a worker records a child span on the `cmd_history` row (`trace_id`, `span_id`,
`parent_span_id`, `trace_source`) and writes `<home>/trace/<worker>.current`;
the message text is never changed. The `amux` CLI and the git
`prepare-commit-msg` hook read that file (or `$TRACEPARENT`, or
`$AMUX_TRACE_ID`), so a worker's calls and commits (`Amux-Trace:` trailer)
descend from the last message delivered to it. That is all a worker's
"current" trace means; `trace_source` (`minted`, `inherited`, `delivered`)
says how each record got its trace. Ledger rows, policy receipts and the
sent-email log carry it. `GET /api/trace/{id}` returns the tree, measured, with
the sources it consulted. `AMUX_TRACE=off` turns it off; a failure to write a
worker's file logs `trace_propagation_failed` and is counted under `trace` in
`GET /api/contract/counters`.

Additions from goal spec 12:

| # | Rule | Enforced at |
|---|---|---|
| A1 | Planned production changes (restarts, migrations) run as a server procedure: notice to the owner 30+ minutes ahead, blue/green, a positive control before traffic, automatic hold on failure | server procedure + board |
| A2 | A deterministic runner under the orchestrator dispatches cards, routes idle lanes, starts proof runs when their inputs verify and closes roll-ups; the orchestrator model decides what is in the queue | runtime job |
| A3 | A project's done line (its proof cards) is frozen and measured from day one; scope changes are versioned decisions with the owner's name | board |
| A4 | Stateful systems get a production-scale test bed; restore, replay and scale tests run there before production | environment |
| A5 | Capacity is a scheduling input: provider usage caps, host pressure and the land queue gate dispatch | runner |

## Rule 5 by repository declaration

A repository opts every land into the server queue with one committed key in
`.amux/land.conf`, read from origin/main, so a change cannot weaken its own gate:

```
[amux]
	queueGate = scripts/ci-fast-gate.sh "$AMUX_LAND_BASE" "$AMUX_LAND_TIP"
```

The gate runs on origin/main plus everything ahead of the entry, with the
program the command runs restored to origin/main's bytes. `AMUX_LAND_BASE` is
that main and `AMUX_LAND_TIP` the composed commit. `queueGateTimeout` overrides
the 900 s budget. Red refuses the entry and bisects a batch; a gate that cannot
start or times out refuses without blame, because that is no verdict. Declared
repositories also get the client's `landRegenerate` and `landRegenerable`
handling and `landFastGate` on the server push, and the repository's
`scripts/land_gate.py` owner and receipt legs run per entry.

`amux land` is then the one path, for every lane, and it waits for the outcome.
No declaration means the old path, and every land says so
(`verdict=land_gate_undeclared`). `AMUX_LAND_QUEUE_DECLARED=0` in `server.env`
turns declarations off for the server; `AMUX_LAND_CLIENT=1
AMUX_LAND_CLIENT_REASON=<why>` skips the queue once, logged, which is how a fix
to a broken declaration or a broken server lands.

## A red main after a land

A declaring repository names what green means and what a red gets:

```
[amux]
	queueChecks = rust
	queueChecks = checks
	queueOnRed = revert
```

`queueOnRed` is `fix` (the default), `revert` or `off`; with no `queueChecks`
nothing is watched. `api/land_red.rs` judges each queue push once, oldest
first. A red is attributed only when the push's base was green, no newer main is
green, and the failure survives one re-run of the failed jobs. A flaky,
unmeasured or unattributed red changes nothing and stays with
`scripts/main-ci-watch.sh`. An attributed red from one entry gets a revert
commit, which joins the queue at priority and passes the same gate as any land;
everything else gets a card on each lane (a batch of several entries, a revert
that conflicts or is refused, a revert budget of 2 a day and 1 at a time). A
revert is never reverted, and each repository and sha gets one episode. The
lane is told. `GET /api/land` lists them under `red`. `AMUX_LAND_RED=0` in
`server.env` stops the watch.

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
