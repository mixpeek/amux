# amux CI/CD in under two minutes

Ethan, 2026-11-12: "iterate until full amux cicd is less than 2 mins make sure u exhaust all
options profiling every step metrics rollback fast checks whatever". The same ask on 2026-11-01
produced `gate.yml` (the deploy gate) and prebuilds.

Two numbers, kept apart because they have different floors:

- **Push to live**: a push to main until the server runs it. Gated by `gate` (fast checks).
- **Full verdict**: until `rust`, `checks` and `harness recovery` have all finished.

## Baseline (2026-11-12, measured)

### Push to live, Mac (195 deploys, from the push: the `gate` run's created_at)

| stage | p50 | p90 | max |
|---|---|---|---|
| total | 35 s | 222 s | 1357 s |
| `gate` workflow (queue + run) | 23 s | 137 s | 1269 s |
| builder pickup after green | 9 s | 82 s | 530 s |
| compile (Rust changes only) | 81 s | 167 s | 582 s |
| health and adoption | 4 s | 41 s | 461 s |

The builder's own `push_to_live_s` counts from the COMMIT time, which is before the land gate
ran, so it read p50 342 s for the same deploys. Measure from the push.

Tail causes, each seen in the builder log:

1. **Shell-run hold.** A deploy waits up to 900 s for an in-flight shell schedule, because a
   restart kills the run (its pipes belong to the server). 16 of 109 deploys held; 530 s and
   298 s on two of the last ten.
2. **Runner queue.** `gate` takes 20 s but queues behind rust and checks: the repo's plan runs
   20 jobs at once (peak measured: 20) and one push starts about 75.
3. **Compile.** One 46k-line crate; the deploy profile already skips LTO and builds
   incrementally. 81 s median under a load average of 28 to 77.
4. **Cooldown.** 120 s between adoptions on the Mac.

### Sandbox (Linux, 4 vCPU, 7 GB)

Gates on `checks` (median 10.7 min), waits `AMUX_RS_BUILD_COOLDOWN=1800` between adoptions, then
compiles with `AMUX_DEPLOY_JOBS=1` (over 15 min). About an hour per deploy.

### Full verdict

| workflow | median | notes |
|---|---|---|
| rust | 26.6 min | red on 12 of the last 15 pushes: scheduler-redesign.spec.ts (fixed af50045f7) |
| harness recovery | 13 min | |
| cloud image (main) | 11 min | |
| checks | 10.7 min | 1643 s of work in 30 shards; one check is 434 s |

checks' long poles: `test-land-guards.sh` 434 s (280 s of it is the land lock's cross-machine
yield, which a single machine relanding pays every time), land precheck part 5 165 s, the long
hook keepalive test 140 s. rust's: the 2.8 GB nextest archive each shard downloads (30 to 270 s),
16 e2e shards of 4 to 9 min.

**Floor for the full verdict on this plan:** one push needs about 170 runner-minutes; 20
concurrent runners make that 8.5 min even perfectly packed. Under two minutes for the full suite
needs more runners or larger ones, which costs money and is Ethan's call.

## Plan

Push to live under two minutes at p90, on every host; the full suite as fast as the plan allows,
with a red full verdict rolling back what it let through.

| # | change | targets |
|---|---|---|
| 1 | shell schedule runs survive a server restart (detached, adopted on startup); drop the deploy hold | tail 1 |
| 2 | the builder runs the fast gate itself on the commit and does not wait for GitHub's queue | tail 2 |
| 3 | push_to_live measured from the push and published by the server | metrics |
| 4 | sandbox: gate on `gate`, cooldown like the Mac, more build jobs | sandbox |
| 5 | checks: no cross-machine yield for same-machine relands; split the long poles | full verdict |
| 6 | a red full verdict on a live commit rolls back (revert on main, forward deploy) | rollback |
