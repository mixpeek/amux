# Mac resource RCA (escalated by the cleanup tick)

`scripts/mac-cleanup-tick.sh` (SCHED-465, every 30 minutes, shell, no model
cost) fixes the symptoms it can compute: purge, restarting a leaking agent,
killing stale shell snapshots, reaping idle cargo targets, thinning local
snapshots. After that it re-measures. Any class still constrained (disk,
memory, cpu, family) gets an RCA bundle and one message to the desktop lane,
at most once per class per cooldown (6h by default). This page is what that
lane does with the message.

## 1. Read the evidence before measuring anything new

- The bundle named in the message: `~/.amux/logs/mac-cleanup/rca/<stamp>.md`.
- The full tick output: `~/.amux/logs/mac-cleanup-tick.last`.
- The trend: `GET $(amux url)/api/metrics/host/history?since_h=12`.

## 2. Find the cause, not the biggest number

Ask what changed, not only what is large.

- **Disk:** which directory grew since the previous tick? `find <root> -xdev
  -type f -size +500M -mmin -60` over `/private/tmp`, the per-user temp dir,
  `~/.amux`, `~/Dev` and `~/Documents/Codex` names the writer. If `df` does not
  move after a delete, run `tmutil listlocalsnapshots /System/Volumes/Data`
  first.
- **Memory:** rank by footprint plus compressed (`top -o mem -stats
  pid,mem,cmprs`), then walk the parent chain to the owning lane (tmux pane or
  `AMUX_SESSION` in the process environment). A process family (many small
  children under one parent) is invisible to a per-process ranking.
- **CPU:** `ps -Ao pid,pcpu,etime,command` sorted by CPU. A long-lived process
  at high CPU with no matching lane work is the usual cause (a busy-loop
  watcher, a crash-restart loop, a VM running a workload nobody is using).
- **CPU, when no process explains the load:** high sys time with idle cores
  means process churn. Measure the spawn rate (`sh -c 'echo $$'` twice, N
  seconds apart; pids are sequential) and what share of it is translated:
  `sysctl -n sysctl.proc_translated` in a lane shell prints 1 when the tree runs
  under Rosetta, where each spawn costs about 12x a native one. The mac-health
  tick logs `translated_procs` and WARNs `rosetta_translated_tree`. Then rank
  spawners by parent program. Worked example:
  `docs/incidents/2026-09-26-mac-cpu-rosetta-spawn-storm.md`.

Write down the owner. Every consumer on this box belongs to a lane, a launchd
agent, an app, or macOS.

## 3. Fix the symptom now, then the cause

The symptom fix buys time. The cause fix is what stops the next message:

- a leak in amux or a script: fix it at the root, add a test and a log signal
  (amux CLAUDE.md two-fix rule), commit and push;
- another lane's workload: message that lane with the evidence and one ask;
- a new class of regenerable waste: add an arm to the tick with a test, the way
  the cargo-target arm was added (DESKT-51);
- a macOS or app process: report it, since a reboot or an app setting is the
  owner's call.

## 4. Stay inside the boundary

- Never delete another lane's uncommitted work, a repo, `.git`, credentials,
  or a database. Prove regenerability (build output, a clone whose every commit
  is in the real repo) or ask the owner.
- Never kill a live lane's workload to reclaim memory. Report it to its owner.
- Spending money, and anything outside the company, needs Ethan.

## 5. Record it

Put a card on the desktop board with the constraint, the cause, the owner, the
fix and its commit, so the next escalation for the same class starts from it.
