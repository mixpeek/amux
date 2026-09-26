# September 26: load at 167% of 28 cores, from a process tree running under Rosetta

Card MO-3622. The mac-cleanup tick escalated a CPU constraint three times in 30 minutes: 15-minute load 32.0, 49.5, then 46.6 on a 28-core Apple Silicon Mac. Host history showed load near 7 for hours, stepping up from 08:20 and peaking at 64 to 67 around 09:30.

## What the load looked like

No single process explained it. `top` showed 46% user, 34% sys and 21% idle in one sample, and 66% user, 33% sys and 0.3% idle in another. Only 10 to 20 processes were runnable. Ranking by process found a few large consumers (a Virtualization guest, a release `rustc`, pytest workers) that together accounted for about half of the busy CPU. The rest was invisible to a ranking because it belonged to processes that lived for milliseconds.

The process-id counter said how many: **240 to 320 new processes per second**, measured five times over 20 to 30 second windows (a sysctl sampler caught 77% of them by name).

## Cause

**The whole pane tree runs under Rosetta.** The tmux server is `/usr/local/bin/tmux`, an Intel-only build (there is no `/opt/homebrew`). Every pane it starts inherits x86_64, and every universal binary below it (bash, git, grep, awk, sed, python3, sleep) runs translated. The `claude` binary is arm64 and changes nothing, because the preference is inherited down the tree. A thin arm64 test program started from a translated shell also spawned a translated `/bin/bash`, while `arch -arm64` at any point flips everything below it. 88% of the sampled spawns were translated (5193 of 5874 in one window).

A translated spawn costs about 12 times the CPU of a native one. 400 spawns of `/usr/bin/true`: 7.7 CPU-seconds translated, 0.65 native. Sixty `python3 -c pass`: 3.3 against 1.0. That cost is mostly sys time, which matches the 33%, and it wakes `oahd`, `trustd`, `syspolicyd` and XProtect on every exec, all of which showed as busy.

**The largest spawner was amux's own hook script.** `scripts/hooks/hook-report.sh` runs on every tool call of every lane. It ran 8.0 times per second across the fleet and forked about 20 processes per run (a tracer caught 16: 13 bash, 2 python3, cat). Translated, one run cost 0.6 s of CPU and 0.7 s of wall time. That is about 4.7 cores, and 0.7 s added after every tool call in every lane.

**The server probed every tmux session twice every 2 seconds.** `TmuxBackend::reconcile` ran `list-sessions`, then `has-session` and `list-panes` per session, and the bootstrap loop calls it every 2 s. At 28 sessions that was 26 spawns per second (13.0 of each verb), half of what `amux-server-rs` forks, and the sweep that calls it reads only the session name.

**Other work on the box in the same window** (owners are named so the asks below have a target; none of it was touched):

- The `celery-retirement` lane ran two `pytest -n 8` suites at once, with workers at 43 to 87% CPU each, plus a `git` process at 681% during a repo inventory scan. That lane's Python is an Intel Homebrew 3.11 build (the interpreter binary has no arm64 slice), so its tests are translated whatever the tree does.
- Three `graft-push.sh` pre-push gates ran at once for 6 to 9 minutes each, about 13 spawns per second apiece.
- A 32 GB Lima guest, started three and a half days earlier by another project's `limactl`, held its host process at 68 to 257% CPU while the guest itself sat at load 0.8 to 1.3. The host cost with an idle guest is unexplained; the vCPU threads cannot be sampled from outside.
- The amux builder compiled a release `amux-server` for 20 minutes at 09:18 and 11 minutes at 09:42 (about 180% CPU, 6 GB RSS). `origin/main` took 13 commits in the two hours before.
- The desktop lane's mutation tests of the tick script ran at 34 spawns per second while they lasted.

## What changed

- `hook-report.sh` re-execs once under `arch -arm64 -x86_64`. Over 20 runs each: 0.56 to 0.65 s CPU and 0.61 to 0.75 s wall translated, 0.14 s CPU and 0.17 s wall native. It falls back to the old behaviour where no native slice exists, keeps stdin and argv, cannot loop, and `AMUX_NATIVE_ARCH=0` opts out. The existing 29 durability cells are unchanged and green, and a new cell fails if the children stay translated.
- `TmuxBackend::reconcile` is one `list-panes -a` census. A dead pane with no recorded status (AMUX-4636) and a session with no active-window pane still take the per-session path. The regression test drives a fake tmux: 41 sessions cost exactly one spawn, and putting per-session probing back turns it red with the spawn list in the message.
- Log signals for the next occurrence. `TmuxBackend` spawns are counted in 60 s windows with a per-verb tally: WARN `tmux_spawn_rate_high` above 15 per second, and `GET /api/debug/tmux` reports `tmux_spawns`. The mac-health tick logs `translated_procs`, `translated_of` and `translated_measured`, and WARNs `rosetta_translated_tree` when at least 40 processes and 10% of the table are translated, at most once per 6 hours.

Expected, and unmeasured on the live fleet until the deploy: the hook change takes the hook from about 4.8 cores to about 1.1, and reconcile drops from 26 spawns per second to under one. A before and after spawn sample follows the deploy.

## Not done, and why

**Moving the tree itself native** is the larger lever, and it is the owner's call because it changes the execution environment of every lane. Two routes:

1. Install an arm64 tmux (there is no arm64 Homebrew prefix on this Mac) and restart the tmux server. Every lane restarts.
2. Launch each lane's provider command under `arch -arm64 -x86_64` for new starts, with no restart. Verified for `claude`, universal system tools, an Intel-only Python (falls back) and an Intel-only node (falls back). A translated Python still hands translation to its own children, so Intel-Python test suites stay translated.

A canary before either: start one lane's shell under `arch -arm64 -x86_64 bash -l` and confirm `sysctl -n sysctl.proc_translated` prints 0 in it.

**Workloads that belong to other lanes.** These are asks, and nothing was changed on their behalf: cap concurrent `pytest -n` runs in the `celery-retirement` lane; the owner of that guest decides whether it should keep running; the builder's release compile could run at lower priority, at the cost of slower deploys.

## Limits

The spawn sampler saw 77% of spawns, so per-source counts are lower bounds. The 12x figure was measured on a loaded box and the ratio varies with load. The hook rate is one 60 second window. The share of load caused by translation is estimated from spawn cost (about 4.5 core-equivalents at 260 translated spawns per second), and the load average counts runnable threads, so it will not fall by a matching amount.
