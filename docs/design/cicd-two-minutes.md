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

## Results (2026-11-12, measured)

| # | status | commit | measured |
|---|---|---|---|
| 1 | in progress (subagent) | | |
| 2 | done | 375d3f20c, 2b82ca690 | b8d48aeb5 (no Rust change): local gate 35 s, push to live **49 s**. The first live run failed on launchd's PATH (no GNU `timeout`, BSD `xargs -I` limit); fixed in 2b82ca690. |
| 3 | done in the builder log | 375d3f20c | `from_push_s` on every ACTIVATION HEALTHY line, from the commit's first workflow run. Not yet published by the server. |
| 4 | done (host config, not in the repo) | | sandbox compile 3 min 53 s with 3 jobs (was 9 to 16 min with 1); cooldown 120 s (was 1800); its CI gate was OFF (`AMUX_DEPLOY_CI_GATE=0`), now on, with the local gate. |
| 5 | done | 3efdcdbd9 | the lease yields only to a waiter that announced itself; long tests split; planned longest checks shard 65 s (was 434 s). |
| 6 | not built | | Automatic revert of other lanes' commits is the human's call (ethos rule 8). A red main still files its card (SCHED-713). |

Also landed:

- **af50045f7** main was red on every push: scheduler-redesign.spec.ts still expected New to open the
  form directly.
- **b8d48aeb5** a push to main runs the e2e its diff selects (16 shards to 1 on recent commits);
  every spec runs on main's tip every two hours (rust.yml `schedule`).
- **0d5a19ab6** the deploy compile can use its own target directory (`AMUX_DEPLOY_TARGET_DIR`, set on
  the Mac): de0fc8719 waited 423 s instead of 130 s for a lane's `cargo test` lock. Lock waits log
  `deploy_build_lock_wait`.

### Where the time goes now

- **A push that changes no Rust: under a minute** to live on the Mac.
- **A push that changes Rust:** pickup about 20 s, then the compile, 106 to 150 s on this Mac at load
  25 to 36. The compile is the floor: one 46k-line crate, already incremental with no LTO. Splitting
  the crate is the remaining lever.
- **Full verdict:** bounded by runners, and shared with pull requests. On 2026-11-12 at 20:00 UTC, 47
  amux runs were queued: about 20 `contrib/gov-*` PRs, each a rust run, ahead of main's checks (one
  waited 72 min). Deploys no longer wait on that queue.

### Needs the owner

- More or larger runners (money): the only way the full suite finishes in two minutes.
- Pull request CI policy: run PR CI when a PR is ready or labelled, or merge the stacked PRs, so
  PRs stop starving main's verdicts.

## The zero-spend plan (Ethan, 2026-11-12: "a plan without increasing any $ spend until cicd e2e is sub 2 min")

### The target, defined

For every push to main, p90 under 120 s from the push to BOTH:

1. **Live**: every amux host this team runs (the Mac, sandbox, mixpeek-dev) serves the commit.
2. **Verified**: a per-push verdict is in: the build, the Rust tests the diff selects, and the e2e
   specs the diff selects (scripts/ci-affected.mjs), all green.

Every test still runs: the full sweep runs on main's tip every two hours (rust.yml `schedule`).
A sweep that goes red names the commits since the last green one. That is the trade for two
minutes: a regression the selector misses shows up within two hours rather than on its push.

### The resource nobody is using: mixpeek-dev

An n2d-highmem-48 already paid for: 48 vCPU, 377 GB RAM, 887 GB free disk, load about 10.
Ubuntu 24.04, glibc 2.39, the same as sandbox, so what it builds runs there.

Measured on it (2026-11-12, throwaway containers from a fresh clone, nice 10, 40 CPUs):

| step | seconds |
|---|---|
| every workspace test binary, cold | 315 |
| every workspace test binary after a one-file change | 81 |
| amux-server lib test binaries after a one-file change | 87 |
| amux-server's 4655 lib tests, 40 threads | 248 to 305, of which ONE test is 227 to 280 |
| deploy binary (`--profile deploy`), cold | 469 (stable), 321 to 396 (nightly) |
| deploy binary, FIRST rebuild after the cold build | 126 to 154 |
| deploy binary, a later one-file rebuild (warm incremental cache) | **43 to 46** |
| the same with rustc's parallel front end (nightly `-Z threads=16`) | 46: no gain |

Two findings change the plan:

- **A warm cache is the lever, not more cores.** The first rebuild after a cold build is three
  times a later one, and 40 cores do not shorten either: one 46k-line crate's front end runs on one
  thread. A builder that keeps its target directory between pushes rebuilds in about 45 s. The
  parallel front end gives nothing here, so stable stays.
- **One test is the whole tail.** `request_log::over_cap_stats_samples_the_window_while_analyze_
  truncates_and_both_report_it` inserts 260k rows to cross a 200k scan cap; every other lib test
  finishes inside 20 s on 40 threads. Its time is the inserts (five indexes each, one a JSON
  expression), not optimisation: compiling dependencies at opt-level 2 did not shorten it.

GitHub's free plan runs 20 jobs at once across the org, shared with about 20 stacked PRs today.
Moving main's per-push work off that pool is what makes two minutes reachable at no cost.

### Steps

| # | change | expected | how it is verified |
|---|---|---|---|
| Z1 | A self-hosted runner on mixpeek-dev, labelled `amux-main`, in a container per job, nice 10, capped at 24 CPUs. Used only by `push` to main and `schedule`; never by `pull_request`, so no fork can reach it. | no queue for main | a push's jobs start within 5 s (job created_at to started_at) |
| Z2 | Per-push verdict on Z1: a target directory that persists between jobs (never cold), test binaries the diff selects, nextest at 40 threads, the selected e2e specs with Playwright workers in parallel. | test build 45 to 90 s warm, tests about 20 s once Z8 lands, e2e in parallel: under 120 s | push_to_verdict in /api/pull, p90 over 50 pushes |
| Z3 | checks.yml for a push to main on Z1: 30 shards at once on 48 cores, planned longest 65 s | about 70 s | the checks run's wall time |
| Z4 | Sandbox stops compiling: Z1 builds the deploy binary from its warm directory (43 to 46 s for a one-file change; same glibc, 2.39) and sandbox installs it. Sandbox cannot build main at all today: since 393951d4a one rustc peaks at 6.8 GB against its 6.5 GB limit, at 1, 2 or 3 jobs. | sandbox live about 60-90 s after the push, from about an hour | push_to_live on sandbox |
| Z5 | The Mac: the gate runs locally (done), shell runs no longer hold deploys (done, 393951d4a), the compile starts at pickup (prebuild, done), the deploy compile gets its own target directory once every lane's tests unset AMUX_DEPLOY_TARGET_DIR (3283eeb11) and the builder refuses a binary under 1 MB, and runs at nice 0 if the benchmark shows it helps. | p90 compile from 284 s to under 100 s | push_to_live on the Mac (/api/pull, d461fb10a) |
| Z6 | PRs stay on GitHub's free runners. They no longer delay main, so their policy stops being urgent. | no main run waits for a PR | queue time of main's jobs |
| Z8 | The 227 s test crosses its cap with a test-only cap: the scan cap becomes a parameter the test sets to a few thousand, so the same edges are pinned at 1% of the rows. | lib tests from about 250 s to about 20 s | nextest's slowest test |
| Z9 | A check in checks.yml (scripts/ci-self-hosted-guard.py) fails when any job that can run on `amux-main` is reachable from `pull_request`, `pull_request_target` or `workflow_run`, so the runner's safety is enforced, not remembered. | none on time | a mutation that adds `pull_request` to such a workflow turns checks red |
| Z7 | Rollback stays as it is: an unhealthy build rolls back on the host (AMUX_ACTIVATION_ROLLBACK); a red verdict files the CI-red card. A revert of someone's commit stays a human call. | | |

### The Mac: the one path the plan does not yet bring under two minutes

A push that changes no Rust is live on the Mac in under a minute today (b8d48aeb5: 49 s). A push
that changes Rust is pickup (about 20 s) plus the compile, and the compile is the Mac's own:
nothing else here can build a macOS binary at no cost (a GitHub macOS runner starts cold and takes
several minutes). Measured over 114 Mac deploys: compile p50 106 s, p90 284 s, minimum 72 s.

What moves it, each measured after it lands rather than assumed:

- **Lock contention.** de0fc8719 compiled for 423 s against a usual 130 s, behind a lane's `cargo
  test` holding the shared target directory; the builder now logs `deploy_build_lock_wait`. A deploy
  directory of its own (Z5) removes it once every lane runs tests that unset
  AMUX_DEPLOY_TARGET_DIR (3283eeb11), and the builder refuses to install a binary under 1 MB.
- **Load.** The Mac runs at load 25 to 95 on 28 cores and the deploy compile runs at nice 10. One
  sample at nice 10 under load 94: 129 s for a first rebuild after a copy. The nice 0 comparison was
  stopped because it would have taken CPU from the fleet at load 94; it is to be run when the Mac is
  quiet, from the builder's own log (compile time by `nice=` in its settings line).
- **The crate.** The floor under any load is the single-threaded front end of amux-server (43 to
  46 s on mixpeek-dev for a one-file change). Splitting the crate would lower it for every host and
  every test build; it is the largest change in this plan and is left for after Z1 to Z9.

Expected after Z5: p50 about 90 s, p90 about 150 s from the push. **The Mac's Rust path is the
remaining gap to two minutes at p90**, and this plan says so rather than counting it as done.

### Risks

- **A public repo with a self-hosted runner.** GitHub warns because a fork's pull request could run
  code on it. Z1 takes only push and schedule events, which only people with write access cause;
  `pull_request` workflows keep `runs-on: ubuntu-latest`, and Z9 fails checks if that ever changes.
  The runner is registered to this repository only, runs as its own unprivileged user, starts a
  fresh container for each job (`--ephemeral` registration, so a job cannot leave anything for the
  next), and holds no credentials beyond the job's own GITHUB_TOKEN.
- **mixpeek-dev is Mixpeek's dev box.** The cap (24 of 48 CPUs, nice 10) keeps its workers
  responsive; the benchmark ran at load 11 to 13 with no effect on them that the box reported.
- **Selection misses a regression.** The two-hourly sweep is the backstop; the land gate also runs
  the selected tests before each push.

## Built and measured (2026-11-13)

What landed after the plan, and what each measured on real pushes (from_push_s, the push to the
server reporting the commit). `GET /api/pull` publishes it as `push_to_live`, with the deploys that
needed a new binary under `built`.

| change | commit | effect |
|---|---|---|
| sandbox installs a binary mixpeek-dev built (Z4) | fb58627c0 | the sandbox deploys Rust commits again (its own compile is OOM-killed) |
| a local gate pass is cached as the verdict; sandbox builder every 15 s | d665e521f | no-Rust commits: sandbox 32 to 84 s |
| `lto = "off"` in the deploy profile | d5dc04716 | a real one-function rebuild 173/81 s -> 63/63 s |
| received binary starts the builder; publisher reuses unchanged binaries | 4aaec163c, 2ad7a78b7 | |
| the Mac compiles alongside its local gate; 30 s gate budget | 55aeaa533 | 73c7a9f32 651 s -> 13b436e0b 171 s |
| publisher drops a build a newer commit supersedes in its first minute | b8929c66a | b96fac4ef waited ~4 min behind an older build |

Profile of a full amux-server compile (nightly -Z time-passes, deploy profile, lto off not yet):
166 s, of which type check 4 s and macro expansion 10 s; LLVM is the rest (IR 65 s, passes 59 s,
ThinLTO 59 s). So splitting the crate helps through codegen, not the front end.

Rebuild of a real one-function change on mixpeek-dev, lto off, 24 CPUs at nice 19, load ~30:

| opt-level | first | again |
|---|---|---|
| 3 (kept) | 105 s | 52 s |
| 2 | 59 s | 52 s |
| 1 | 66 s | 65 s |

Lowering it buys nothing; nightly `-Z threads` bought nothing either (46 vs 43 s).

### Where it stands

| commit kind | sandbox | Mac |
|---|---|---|
| no compile input changed | 32 to 84 s | 52 to 124 s |
| Rust change | 121 to 358 s measured; ~70 to 125 s when mixpeek-dev is at load ~30 | 171 to 251 s with the early prebuild, 651 s before it |

The Rust path is now the compile on a shared, loaded machine: the Mac runs the fleet at load 40 to
115, mixpeek-dev runs Mixpeek at load 20 to 120. The no-spend levers are spent. What would move it
further needs the owner: a quieter or dedicated build machine (an Apple Silicon one for the Macs

## Built and measured (2026-11-13)

What landed after the plan, and what each measured on real pushes (from_push_s, the push to the
server reporting the commit). `GET /api/pull` publishes it as `push_to_live`, with the deploys that
needed a new binary under `built`.

| change | commit | effect |
|---|---|---|
| sandbox installs a binary mixpeek-dev built (Z4) | fb58627c0 | the sandbox deploys Rust commits again (its own compile is OOM-killed) |
| a local gate pass is cached as the verdict; sandbox builder every 15 s | d665e521f | no-Rust commits: sandbox 32 to 84 s |
| `lto = "off"` in the deploy profile | d5dc04716 | a real one-function rebuild 173/81 s -> 63/63 s |
| received binary starts the builder; publisher reuses unchanged binaries | 4aaec163c, 2ad7a78b7 | |
| the Mac compiles alongside its local gate; 30 s gate budget | 55aeaa533 | 73c7a9f32 651 s -> 13b436e0b 171 s |
| publisher drops a build a newer commit supersedes in its first minute | b8929c66a | b96fac4ef waited about 4 min behind an older build |

Profile of a full amux-server compile (nightly -Z time-passes, deploy profile, before lto off):
166 s, of which type check 4 s and macro expansion 10 s; LLVM is the rest (IR 65 s, passes 59 s,
ThinLTO 59 s). Splitting the crate would help through codegen, not the front end.

Rebuild of a real one-function change on mixpeek-dev, lto off, 24 CPUs at nice 19, load about 30:

| opt-level | first | again |
|---|---|---|
| 3 (kept) | 105 s | 52 s |
| 2 | 59 s | 52 s |
| 1 | 66 s | 65 s |

Lowering it buys nothing; nightly `-Z threads` bought nothing either (46 against 43 s).

### Where it stands

| commit kind | sandbox | Mac |
|---|---|---|
| no compile input changed | 32 to 84 s | 52 to 124 s |
| Rust change | 121 to 358 s measured; about 70 to 125 s when mixpeek-dev is at load about 30 | 171 to 251 s with the early prebuild, 651 s before it |

The Rust path is now the compile on a shared, loaded machine: the Mac runs the fleet at load 40 to
115, mixpeek-dev runs Mixpeek at load 20 to 120. The no-spend levers are spent. What would move it
further needs the owner: a quieter or dedicated build machine (an Apple Silicon one for the Mac
binary), or splitting amux-server into crates so a change recompiles less.
