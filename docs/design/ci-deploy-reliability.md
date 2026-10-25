# Keeping the Mac on main: CI/CD reliability plan

Owner request (Ethan, 2026-10-24): "the mac needs to stay on main", then
"come up with a plan to fix this and /iterate score by speed of cicd, ability
to catch issues, and kiss".

## What was measured (last 100 main runs, 2026-10-24)

The builder activates the newest main commit whose `rust` AND `checks`
workflows both passed (`scripts/rust-auto-build-authority.sh`).

| Measure | Value |
|---|---|
| `rust` outcomes | 24 success, 53 failure, 21 cancelled (72 h) |
| `rust` passing duration | min 7, median 16 min |
| Red `rust` runs with an e2e failure | 47 of 49 classified |
| Red `rust` runs that failed only e2e | 22 of 49 |
| `checks` outcomes | 55 success, 16 failure, 28 cancelled (52 h) |
| Why `checks` was "cancelled" | its 10-minute job timeout; ~90 serial steps, passing median 9 min |
| Commits green on both | **7 of 100** |
| Server on 2026-10-24 13:00 | 08c8c6973, 79 commits behind main |

Three causes, in order of size:

1. Flaky e2e specs. Each of four red runs in a row failed a different spec.
   Playwright runs with `retries: 0` on purpose and the evidence checker
   (`scripts/check-e2e-evidence.mjs`) rejects retries, so one flake reds the run.
2. `checks` at its time limit. A quarter of its runs timed out.
3. Re-runs could not land. Nothing re-ran a red run; when one was re-run by
   hand, the concurrency queue replaced it with the next push's run (68ade32c3
   and 31351128f re-runs cancelled after 27 minutes), and the gate cached
   "cancelled" as final so a later green re-run was never read.

## Rubric

Speed: how soon a good commit reaches the Mac (push to activation).
Catch: does a real regression still stop a deploy, and is a flake visible.
KISS: fewest moving parts, no new systems, no new cost.
Score = mean of the three, 1 to 10.

## Iterations

| # | Score | Plan | Why it was not the answer |
|---|---|---|---|
| 0 | 3.7 | Baseline: speed 2 (7/100 deployable, hours behind), catch 5 (a red that is always red is ignored), KISS 4 | |
| 1 | 5.0 | Stop cancelling main runs; raise `checks` timeout; a workflow that re-runs e2e-only failures once | Un-cancelling main runs multiplies CI minutes (e2e was $222/mo in September): a cost decision, and it does not touch the main cause, flakes. Speed 5, catch 6, KISS 4 |
| 2 | 5.7 | Drop un-cancelling. Retry failed specs inside the e2e job (`--last-failed`) instead of a workflow re-run | Fights two deliberate designs (`retries: 0`, the evidence checker's "unexpected retry"). Faster, but hides flakes inside a green job. Speed 8, catch 4, KISS 5 |
| 3 | 7.0 | Back to "Re-run failed jobs", which the evidence upload already supports (attempt in the artifact name). Give a re-run attempt its own concurrency group so the queue cannot replace it. Gate re-reads cancelled verdicts. Split `checks` into parallel jobs | Splitting `checks` buys no deploy speed: it runs beside `rust`, which is longer. Speed 7, catch 7, KISS 7 |
| 4 | 7.7 | Replace the split with one line: `checks` budget 10 to 20 minutes, plus a warning at 15 so it is fixed before it times out again | A re-run that passes leaves no trace where a sweep looks. Speed 8, catch 7, KISS 8 |
| 5 | 8.3 | Add the log signal: the gate writes `verdict=ci_green_after_rerun` once per commit to the builder log; the re-run workflow leaves a `flake_rerun` annotation. Keep fixing flaky specs at the cause as they show | Speed 8, catch 8, KISS 9 |

Final plan (iteration 5), what each piece costs, and where it lives:

| Piece | Where | Cost |
|---|---|---|
| Re-run a red main `rust` run's failed jobs once, only when every failed job is e2e | `.github/workflows/rust-flake-rerun.yml` | one or two shards per flake |
| A re-run attempt is never replaced by the queue | `rust.yml` concurrency group | none |
| `checks` has a 20-minute budget and warns at 15 | `checks.yml` | none |
| The gate re-reads a cached "cancelled" like a red | `rust-auto-build-authority.sh` | a few API calls |
| The builder log names every commit that went green only on a re-run | `rust-auto-build-authority.sh` | none |
| Flaky specs fixed at the cause | e2e specs | ongoing |

Not done, on purpose: un-cancelling main runs (cost; the owner's call),
retries inside Playwright (hides flakes and contradicts the evidence checker),
splitting `checks` (no deploy speed gained).

Why speed is 8 and not 10: a passing `rust` run is 16 to 25 minutes, and a
flaky one adds a re-run of one shard (about 10 minutes). Making `rust` itself
faster is separate work.

## How to tell it is working

- `grep -a 'ci_gate_' ~/.amux/logs/rust-auto-build.log | tail` shows
  `ci_gate_clear` or `ci_gate_held_back`, not `ci_gate_no_green`.
- `GET /health` `commit` is within a few commits of `origin/main`.
- `grep -ac ci_green_after_rerun ~/.amux/logs/rust-auto-build.log` counts
  commits that needed a re-run; the specs behind them are the next flakes to fix.
- `verdict=checks_duration` in each `checks` run, and a `checks_near_timeout`
  warning past 15 minutes.
