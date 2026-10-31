# Usage & Optimization

Status: **doing** (amux lane, 2026-10-30). Owner ask: a Usage & Optimization
tab that answers "where is my intelligence budget going, what value is it
producing, and how can amux automatically improve that allocation", as a
closed loop: observe, attribute, evaluate, optimize, measure, repeat; and
APIs that let workers improve themselves from it. The amux lane's board is
off, so this file is the card.

## Where it goes

The loop already exists in pieces, so this extends them rather than starting a
ninth thing (VISION: eight primitives; reuse existing paradigms).

| Layer | Already there | Added here |
|---|---|---|
| Visibility | `token_ledger.cost_usd` per turn; `/api/usage/attribution` by source | spend per worker and group in `/api/velocity` |
| Attribution | `task_episodes` (attempts per card), `token_ledger.task` (37% of turns) | cost per card and per finished card |
| Efficiency | `/api/velocity`: finished per day, share of time on finished work | cost per success |
| Quality | finished vs parked, abandoned, reworked attempts | success rate (cards finished / cards attempted) |
| Optimization | waste categories with an owner (velocity) | `opportunities[]`: evidence, $/month, who acts |
| Automation | velocity section in each worker's context; board-drive fixes (ee1fd734d) | each opportunity routed to who can act |

API: `GET /api/velocity/optimize?days=30` (fleet) and the per-worker
`/api/velocity/{w}` gain `cost`. One family, so the numbers agree.

UI: a top-level **Usage** tab (the owner wants it prominent) that renders the
Metrics > Velocity view, extended: totals (spend, cards finished, success rate,
cost per success, potential savings), spend by group, opportunities, then the
worker list. One row of controls on a phone.

## Opportunities (computed, never guessed)

Each carries `kind`, `evidence` (the numbers and card or worker ids),
`est_usd_month`, `owner` (amux, worker, owner) and `action`.

- **Background spend**: `/api/usage/attribution` sources marked background
  (amux nudges, steering) with their dollars. Measured 2026-10-30: "amux nudged
  this lane about a card" cost $3,208 in API-equivalent dollars over 24 h, 3,731
  turns. Owner: amux.
- **Rework**: workers with attempts per card over 2, priced at their cost per
  attempt. Owner: the worker.
- **Parked spend**: dollars spent in attempts that ended parked or abandoned.
  Owner: whoever the waste category names.
- **Model routing**: cards finished under 15 minutes on the most expensive
  model, priced at the difference. Owner: the owner (a model choice is theirs).

## Closing the loop

- Worker-owned opportunities join that worker's velocity context section, so
  the worker sees its own top opportunity without asking.
- amux-owned ones are what the amux lane works next (the nudge cost above is
  the first).
- Owner-owned ones are listed in the tab with their evidence.
- The 7-day re-measure (2026-11-06, scheduled) compares spend, success rate
  and cost per success against the baseline in `self-improve-baseline/`.

## Status

- [ ] `/api/velocity/optimize` with totals, by group and opportunities
- [ ] Usage tab
- [ ] Worker context carries its top opportunity
