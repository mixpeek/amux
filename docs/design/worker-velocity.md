# Worker velocity: see your bottlenecks, cut the waste, know when work lands

Owner asks, 2026-10-30 (MSG-101574, -576, -577, -580, -583, and 11:25 / 11:33):
per-task token and time estimates from heuristics that update per worker;
projections of when work finishes; where the waste is; metrics a worker reads
to overcome its own bottlenecks; improve automatically. Reference workers:
`mxp-gs12` and `tubescience-parity`. "The velocity shit is the top priority"
(12:10). Board is off for the `amux` lane, so this file is the tracking record.

## What others do (researched 2026-10-30)

- **Cognition (Devin):** meters work in ACUs (about 15 minutes each), shows
  usage per session after the fact, caps it (a per-session ACU limit that
  pauses Devin; org and per-user limits that stop work), and has the user
  approve a plan before execution. No per-task estimate before a run.
- **Factory (Droids, Missions):** meters tokens per model; Missions plan by
  conversation, split work into milestones and features, show progress in
  Mission Control. Its numbers (median mission about 2 h) are after-the-fact
  aggregates, not predictions.

Neither predicts per task from history. Two ideas worth copying: a cap tied to
the estimate (Devin's session limit), and showing the plan's basis (Factory's
milestones). amux goes further: per-worker, history-based estimates and waste
attribution.

## Data

- `task_episodes`: one row per attempt at a card (worker, start, end, outcome,
  reason). 2,502 attempts in 30 days, 35 workers with enough history.
- `token_ledger`: tokens and cost per model turn, attributed to a card when
  known (37% of turns are; the rest are ambient).
- Gap: a worker that parks a card records no reason. Self-parking is the top
  waste for both reference workers, and we cannot yet say why.

## Reference workers, 30 days (live, 2026-10-30)

| | tubescience-parity | mxp-gs12 |
|---|---|---|
| attempts / cards / finished | 103 / 77 / 38 | 53 / 47 / 9 |
| finished per day | 1.27 | 0.30 |
| time on attempts that finished | 22% | 18% |
| top waste | self-parked 50% (24 h) | self-parked 35%, review 28% |
| per finished card, median | 9 min, 141k tokens | 8 min, 784k tokens |

## Plan

1. **Measure (landed, 4022416b6, live).** `GET /api/velocity/{worker}` and
   `GET /api/velocity`: throughput, time by category, rework, waste split by
   who can fix it (amux, the worker, the board, the owner), the top
   bottleneck, and `?card=ID` estimates from the worker's similar finished
   cards, then all its cards, then the fleet's, each saying its basis and n.
2. **First automatic fix (landed, 07238bd49).** The harness's own top waste:
   the board driver claimed cards for lanes at their usage limit, the prompt
   was held, the claim released (248 times in 30 days). It now asks
   `dispatch_hold` first and leaves the card unclaimed.
3. **Close the "why" gap (landed, d1a5fe2b8).** Record a reason when a worker parks or releases a
   card (from the status change's note), so self-parking splits into waiting
   on a dependency, unclear card, too big, and so on.
4. **Reach the worker by default (landed, 140ff3b8b; reconcile fix d031aa96d).** A short computed section in each worker's
   generated context (its throughput, top bottleneck with evidence, and the
   estimate for its current card), plus `amux velocity [worker]`. Recomputed
   from history, so it updates per worker as it works.
5. **Projections (landed, f3b2aaa87; `amux velocity --projection`).** Per worker, the queue's finish time from the estimates of
   its queued cards (one card at a time); per project or goal, chained across
   workers through `depends_on` and scheduled external events (for example a
   shard build), with a slip factor from how past estimates and builds
   actually went. Every projection is scored against what happened.
6. **Overrun flag (Devin-style cap) (landed, 84d79aaf0; flags past p75 times calibration).** When a card runs past its worker's p90
   for similar cards, say so on the card and to the worker; optionally set
   `_amux_task_budgets` from the estimate.
7. **Owner view (landed, 24928c6df: Metrics > Velocity).** A Velocity section: workers ranked by wasted hours, each
   worker's breakdown, and project projections; one row of controls on a
   phone.

Each step lands with a test, a mutation that fails it, and a log or response
field that says whether it measured anything.
