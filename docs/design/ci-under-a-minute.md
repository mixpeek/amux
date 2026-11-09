# Getting amux CI/CD under one minute

Owner goal, 2026-11-02: "iterate until the full amux cicd is under 1 min".

## Where it stands

Measured on main, 2026-11-02. `rust` is first job start to last job end; `checks`
is run created to run finished, so it includes queueing.

| Job or workflow | Before | Now | What changed |
|---|---|---|---|
| `checks` | 845 to 986s | 160 to 648s, median 457s | parallel shards; 750s of checks, median shard 44s |
| `rust`, Rust commit | 1,570 to 1,930s | 805 to 1,895s | no separate `cargo check`, shards use `/mnt`, 3 e2e workers |
| `rust`, no Rust change | same as above | 722 to 1,915s | restores binaries by exact key |
| `check` job, no Rust change | 621s | 45 to 60s | a pass is keyed on the Rust inputs |
| `test-build` job, no Rust change | 509s | 207 to 246s | restores the archive |
| nextest archive | 2,827 MB | 2,425 MB | stripped test binaries |
| e2e shard, median | about 520s | 448s (one run) | 3 workers; watch whether flakes rise |
| `harness recovery` | 540 to 760s | 636 to 1,202s | unchanged, and it runs beside the queue |
| `cloud image` | 820s | unchanged | outside the deploy gate; its CLI install is unpinned, so caching would freeze versions |

Spans overlap heavily with queueing, and every `rust` run above ended red or was
measured while flaky e2e specs still failed, so read them as ranges.

**The queue is now a floor of its own.** One `checks` run waited 500s for a runner
while its jobs took a median of 68s. Adding shards past this point adds waiting.

## What sets the floor

Tuning cannot reach 60s, because three costs each exceed it.

1. **e2e is 8,002s of browser time** (`e2e/durations.json`): 926 tests on each of
   three browsers, ios-safari alone 3,790s. In 60s that needs about 133 browser lanes
   at once, on an account whose jobs already queue for up to 217s.
2. **A Rust change compiles for about 650s before a test can run**
   (test-build, run 41458289010). amux-server has 118 integration test binaries, and
   each links the whole server library, which also makes the 2.8 GB archive every
   shard downloads.
3. **The nextest suite is about 16 minutes of runner time** across 8 shards.

## The three decisions that remain

Each one belongs to the owner.

**A. What has to pass before a deploy.** Recommended, and the only one that reaches
60s by itself. The deploy gate waits for a fast tier: the `amux land` receipt, `checks`
and the affected tests. The full suite runs right after and holds or reverts on red.
This changes what a green deploy promises: a regression only the full suite catches
reaches the server until that run finishes, about 20 minutes later.

**B. Hardware.** Warm build state is what removes the compile. Options: a self-hosted
runner with a persistent target directory (this repo is public, so it must never run
pull-request code), or larger hosted runners, which bill. A local dashboard edit
already rebuilds in 14s and a large code change in 63s on the `deploy` profile.

**C. Folding the 118 test binaries into one.** nextest already gives each test its own
process, so isolation holds. It should cut the compile and most of the archive. It
changes what `binary(=name)` selects in `scripts/land_gate.py`, an `[owner]` tier file,
so the owner has to approve that diff. A Rust commit would still take minutes.

Running ios-safari on a schedule instead of on every push would remove 47% of e2e time.
A WebKit-only break would then surface at the next scheduled run.

## Push to live, 2026-11-08

Owner ask, 2026-11-08: "get our cicd below 2 min". The deploy waits only for
`gate` (decision A above, taken 2026-11-01), so the number that matters is
push to live: from the push to the server answering with the commit.

Measured over the 25 deploys before 2026-11-08 (`gate` run created to
`ACTIVATION HEALTHY` in `~/.amux/logs/rust-auto-build.log`): median 193 s,
p90 412 s, 2 of 25 under 120 s.

| Step | Median | p90 |
|---|---|---|
| `gate` | 31 s | 78 s |
| builder picks it up | 10 s | 77 s |
| deploy build | 96 s | 195 s |

The builder's own `push_to_live_s` starts at the committer time, which for an
`amux land` commit is when the land gate rebased it, so it also counts the land
gate and reads higher (median 363 s).

**The build ran for commits that change no compiled input.** The server
compiles its commit in, so a docs commit read as "STAMP DRIFT" and rebuilt
amux-server: 80 to 185 s for bytes identical to the running ones. In the week
to 2026-11-07, 182 of 269 commits changed no Rust source.

**Fix.** The server takes its commit from the receipt beside its binary when
the receipt names its own bytes (`activation::served`), and re-executes when
the receipt changes. The builder re-stamps the receipt instead of compiling
when nothing the compile reads changed since the bytes were compiled: crates/,
Cargo.*, the toolchain files and every include target outside crates/
(`ci-rust-key.mjs`). `/health` keeps `commit` as the commit served and adds
`build_commit`. `scripts/test-build-restamp.sh` covers it. Such a commit should
go live in about `gate` plus one builder tick plus a re-exec.

**Still over 2 minutes:**
- Commits that change Rust: `gate` plus a 90 to 200 s incremental build of
  amux-server.
- Commits that change only the dashboard: it is compiled into the binary.
  Serving it from a deployed snapshot (`AMUX_DASHBOARD_DIR`, already supported)
  would put those 44 commits a week on the re-stamp path. That is the next step.

**Measured after fa39bb928 (2026-11-08).** The first two non-Rust commits both
re-stamped (`verdict=activation_restamped`, 31 includes outside crates/
checked). Times run from the `gate` run's `created_at` until the server served
the new commit (`served_commit_measured source="receipt"`):

| commit | gate | builder pickup | adoption | push to live |
|---|---|---|---|---|
| d36f6d0b1 (scripts) | 78 s | 10 s | 121 s, held by a running shell schedule (`adoption_deferred_shell_run`, SCHED-465) | 209 s |
| 69abc30b6 (e2e spec) | 13 s | 5 s | 2 s | 20 s |

A re-stamp with nothing holding adoption is live in about 20 s. The 900 s
shell-run hold still applies, so a long shell schedule can delay one.

**Dashboard commits, 2026-11-08.** rust-embed compiles `static/` into the
binary, so a dashboard commit compiled: 89c3d6149 reported
`push_to_live_s=430`. When the dashboard is the only compile input that
changed, the builder now copies that commit's `static/` to
`~/.amux/dashboard-snapshots/<git tree id>` (`AMUX_RS_DASHBOARD_SNAPSHOTS`)
and names it in the receipt as `dashboard_dir`. The server serves that
directory in place of its embedded copy (`amux_dashboard::use_snapshot`,
log `dashboard_snapshot_served`). `/health` reports `dashboard_source` and
`dashboard_version`. A server that cannot serve the snapshot logs
`dashboard_snapshot_refused` and reports its compiled commit, so the builder
restores the receipt and compiles in the same run. The builder keeps the
served snapshot and three others. `scripts/test-build-restamp.sh` covers the
dashboard-only, dashboard-plus-Rust, unsupported-server and refused cases.

## Push to live, 2026-11-09: the land gate sets the floor

The builder's `push_to_live_s` counts from the commit's committer time, which
`amux land` sets when it replays the change onto origin/main, before the gate
runs. So the number includes the land gate. After the push, the deploy is
already fast: activation waits only for `gate` (`AMUX_CI_REQUIRED_WORKFLOWS=gate`
in server.env, about 16 s), then a re-stamp or a compile.

Measured over 35 pushes from 2026-11-08 08:00 to 11-09 08:00, from committer
time to the `gate` run's `created_at`: 22 reached GitHub within 34 s, and 13
waited 167 to 1,605 s. All 13 were held by the land gate. Its receipts show
three costs, in this order of size:

| cost | what it is | measured |
|---|---|---|
| Rust leg | nextest compiles and runs the changed modules' tests on the composed tree | 95 to 1,034 s; 4c1bf9485 1,034 s at load 0.58 per core |
| Load wait | `wait_for_load` holds every change until load is at most 2.0 per core | 270 s (adfe6b6e5) and 331 s (ecc956210), both changes with no Rust leg |
| e2e leg | the affected Playwright specs, run after the Rust leg | 75 to 164 s |

The review runs beside the Rust leg (27 to 342 s), so it adds time only when
it is the longest leg. Across 146 gate runs since 11-08 the median was 216 s
and the 90th percentile 1,250 s.

**So the 2026-11-09 audit's B1 named the wrong mechanism.** It said activation
waits for the `rust` workflow's e2e shards. It does not. The fix belongs in
`scripts/land_gate.py`, an `[owner]` file, so each change below needs the
owner's approval of its exact diff, filed by the gate itself.

**Changes, smallest first:**

1. **No compile, and so no load wait, for a change with no Rust input.**
   The e2e leg compiles too: `e2e/serve-head.sh` builds the server from the
   composed tree, and a dashboard change recompiles because `static/` is
   embedded. That compile is what the load wait protects. When the change's
   compile inputs (`scripts/ci-rust-key.mjs`) match the live server's
   receipt apart from `crates/amux-dashboard/static/`, the gate can hand the
   e2e leg the live binary (`AMUX_E2E_SERVER_BIN`, which CI already uses)
   and the composed tree's `static/` (`AMUX_DASHBOARD_DIR`, which
   serve-head already honors), and skip `wait_for_load`. That saves the
   270 to 331 s wait and the e2e build on those commits. If the inputs
   differ, the gate compiles and waits as today, and logs which inputs
   differed.
2. **e2e beside the Rust leg.** Today it waits for nextest to finish. The two
   share the build directory, so the overlap is only the browser time
   (75 to 164 s), and only if the e2e server build does not wait on the
   same cargo lock. Measure that before relying on it.
3. **Land on the fast legs and run the Rust leg after the push (SA-9, "fast
   checks with revert-on-red").** The receipt passes on owner, revert,
   review and e2e. The Rust leg keeps running on the composed tree. If it
   goes red, the gate pushes `git revert` of the change with the failing
   tests named, and logs `land_gate_reverted`. That takes up to 1,034 s off
   a Rust change. The cost: a Rust regression is live for the length of the
   leg, which the builder's boot health check and the `rust` workflow also
   catch.

Change 1 reuses two hooks that exist and changes no verdict. Change 3 is the one that
reaches the 2-minute goal for Rust commits.
