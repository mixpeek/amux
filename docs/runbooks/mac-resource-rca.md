# Mac resource RCA (escalated by the cleanup tick)

`scripts/mac-cleanup-tick.sh` (SCHED-465, hourly, shell, no model cost) fixes
the symptoms it can compute: purge, restarting a leaking agent, killing stale
shell snapshots, reaping idle cargo targets, thinning local snapshots. After
that it re-measures. Any class still constrained (disk, memory, cpu, family,
and the health classes below) gets an RCA bundle and one message to the
escalation target, at most once per class per cooldown (6h by default). This
page is what that lane does with the message.

The whole tick runs inside one deadline, `AMUX_CLEANUP_TICK_BUDGET_S` (540s),
because the scheduler kills it at 600s. Each cleanup arm gets at most what is
left after `AMUX_CLEANUP_TICK_RESERVE_S` (120s), which is kept for the verdicts
and the escalation. An arm with no time left prints `<arm> skipped: ...
(verdict=tick_out_of_time)` and is named in the done line's `skipped=` field.
A tick that skips arms on a loaded machine has still measured and escalated;
`skipped=none` is the normal reading.

Every tick also checks itself (`check_self_runs`, in the system health
section). It reads SCHED-465's run rows in `schedule_runs`, its run logs under
`~/.amux/shell-runs/SCHED-465/`, and the launchd fallback's log, over the last
24h, and prints one `mac-cleanup: self:` line. Each rule below that fails adds
a `self ...` constraint, which escalates like any other class:

| Rule | Default | Knob |
|---|---|---|
| runs in a row not ok (timed out, killed by a server restart, error) | 2 | `AMUX_CLEANUP_SELF_FAIL_STREAK` |
| runs not ok in 24h | 4 | `AMUX_CLEANUP_SELF_FAIL_24H` |
| slowest run over the tick's warning | 480s | `AMUX_CLEANUP_TICK_WARN_S` |
| runs that skipped arms for lack of time | 3 | `AMUX_CLEANUP_SELF_SKIP_24H` |
| fires that met a running tick and did no work (recorded ok) | 2 | `AMUX_CLEANUP_SELF_COLLIDE_24H` |
| times the launchd fallback had to run it | 3 | `AMUX_CLEANUP_SELF_FALLBACK_24H` |
| SCHED-465 disabled or missing | always | none |

An unreadable history prints `verdict=self_unmeasured` and judges nothing.

The fallback (`scripts/mac-cleanup-fallback.sh`, launchd every 15 minutes) runs
the tick when its output is over 80 minutes old or unfinished. The window must
stay longer than the hourly gap: at 45 minutes it ran a duplicate tick before
most fires, and each scheduled fire then met the lock and did nothing.

## The system health section

Beside disk and memory, every tick checks the rest of the machine. Each probe
is cheap (the section takes about 8s) and goes through a seam;
`AMUX_CLEANUP_HEALTH_PROBES=off` turns them all off.

| Probe | Fixes on its own | Escalates (class) |
|---|---|---|
| CPU hogs: at or over 80% on two ticks in a row | sends TERM to a listed macOS helper that launchd starts again (`AMUX_CLEANUP_CPU_RESTART_NAMES`, default ReportCrash, ecosystemanalyticsd, mediaanalysisd, photoanalysisd, mdworker_shared, spotlightknowledged), only when this user owns it. Log: `cpu-restarts.log` | `hog`: any other process over 300% on two ticks |
| Leftover test processes | stops a process older than 2h whose parent has exited (ppid 1) and whose command runs out of a pytest temp dir. Log: `test-orphans.log` | none |
| Kernel limits | nothing | `limits`: open files, this user's processes or inodes at 85% of the limit. `zombies`: one parent holding 200 unreaped children |
| Crash reports | prunes `.ips/.crash/.diag/.hang/.spin` older than 30 days in `~/Library/Logs/DiagnosticReports` | `crashes`: one app crashing 10 times in 24h |
| Who is using the CPU (`GET /api/resources`: workers, and the rest by kind: `vm`, `detached`, `apps`, `system`, `amux_server`) | with the CPU at 85% busy (`AMUX_CLEANUP_UTIL_ACT_SHARE`), lowers each detached tree at 100%+ (`AMUX_CLEANUP_UTIL_RENICE_MIN_PCT`) to nice 10, never killing it. Never touches tmux (new worker panes would inherit the priority) or gs12 (`AMUX_CLEANUP_UTIL_RENICE_EXCLUDE`). Log: `util-renice.log` | none of its own: it adds `(who: ...)` to the `cpu` constraint so that escalation names the consumers |
| launchd jobs, network, thermal | nothing (report only) | none |

The logs named above live in `~/.amux/logs/mac-cleanup/`. A `zombies`
escalation names a parent that is not waiting on its children; the fix is in
that parent's code, and the zombies go away when it reaps them or exits.

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
  or a database.
- Never touch anything under `/private/tmp/claude-*`. Those are live Claude
  session scratchpads. No process standing in a directory at the moment you
  look does not mean it is unused: on 2026-09-26 a worktree sweep removed the
  desktop lane's active worktree between two of its commands.
  The tick's claude-tmp arm owns that tree (DESKT-77): it removes a session dir
  only when its transcript and files are idle for `AMUX_CLEANUP_CLAUDE_TMP_IDLE_H`
  (48h, 24h on a tight disk), nothing holds it open and no worktree in it is
  dirty, and it holds live sessions to `AMUX_CLEANUP_CLAUDE_TMP_QUOTA_GB` (20G) by
  removing their idle scratchpad entries. Every removal is in
  `~/.amux/logs/mac-cleanup/claude-tmp-reaps.log`. Tune the knobs in the Scope tab
  instead of deleting by hand. Prove regenerability (build output, a clone whose every commit
  is in the real repo) or ask the owner.
- Never kill a live lane's workload to reclaim memory. Report it to its owner.
- Spending money, and anything outside the company, needs Ethan.

## 5. Record it, and hand the next escalation your card

Put a card on your board with the constraint, the cause, the owner, the fix and
its commit, and the constraint re-measured after the fix. Then write just the
card id into the `.card` file the message names. The tick reads it back: if the
same class is constrained again after the cooldown, the next message says
"this RECURRED", quotes your card and the numbers measured then, and asks why
the fix did not hold. Without the id, that message can only say none was written.

## If the tick itself breaks

- Two failed sends in a row to mac-ops file one card on the desktop board per
  24h, with the send error.
- If the amux scheduler stops firing SCHED-465, launchd runs
  `scripts/mac-cleanup-fallback.sh` every 15 minutes. It runs the committed tick
  when the last output is over 45 minutes old, and files one card per 24h
  saying the scheduler went quiet. Log: `~/.amux/logs/mac-cleanup-fallback.log`.

## fseventsd: a volume for agent scratch

fseventsd records every file change on a volume and grows over days of agent
work (1.43 GB at 109% CPU on 2026-10-14). It is SIP-protected, so the tick can
only report it. The remedy is to keep the churn off a volume fseventsd tracks.
Measured 2026-10-16 over 30 minutes: about 110k changed files under
`~/.amux/tmp` and `/private/tmp/claude-501` (40,670 from one lane's parallel
build checkouts), 26 under `~/Dev/amux`. Agent scratch is the churn, not the
project folders.

`scripts/mac-agent-volume.sh` manages an APFS volume, `AgentScratch`, in the
boot container. It shares the container's free space and opts out of both
indexers: Spotlight with `mdutil -i off`, fseventsd with `.fseventsd/no_log`
read at mount. `status` prints one line, `create` adds the volume and turns
Spotlight off, `verify` exits 0 only when every opt-out is read back in place.
Each tick prints that line as `mac-cleanup: agent volume:`.

`.fseventsd` is created by macOS as root and readable only by root, so the
`no_log` step is the owner's, once; `status` prints the exact command. It also
makes `.fseventsd` searchable, so the marker is read rather than assumed.

Pointing scratch at the volume comes after `verify` passes, and switches over
each worker at its next restart: `CLAUDE_CODE_TMPDIR` for Claude Code's
scratchpads, and the lane temp root for `~/.amux/tmp`. The cleanup tick has to
reap the new location before either is switched. Moving `~/Dev` takes every
lane down at once, so it is a planned-downtime decision, not a cleanup step.

