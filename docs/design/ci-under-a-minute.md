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
