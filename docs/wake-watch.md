# Wake watches: wake your lane when a file lands or a CI run concludes

Stop writing watcher loops (`ci_watch.sh`, `rb_boot_watch.sh`,
`chain_done_notify*.sh`, ...). Register a watch instead. amux checks it, sends
your lane exactly one message when it fires, and tells you if it expires first.

```bash
# A file appears (glob allowed)
amux watch add file --path '/Users/ethan/.amux/tmp/mxp-gs12/handoffs/ledger-*.md' --when exists

# A line matching a regex is appended (lines already in the file do not count)
amux watch add file --path /path/run.log --regex 'gRPC server listening' --expires 3h \
  --note 'rollback pod bound: run rb_finish next'

# Any change to a file or to the set a glob matches (created, modified, removed)
amux watch add file --path /path/state.json --when changed

# A CI run concludes: by run id, by commit sha (every run on it, optionally one
# workflow), or the next run of a workflow started after you registered
amux watch add ci --repo mixpeek/mixpeek --run 40217171871
amux watch add ci --repo mixpeek/amux --sha 2cdf04110 --workflow rust
amux watch add ci --repo mixpeek/mixpeek --workflow zb-platform-pipeline.yml

amux watch ls            # your watches: state, measured, n_considered, why_unmeasured
amux watch ls --json     # same, machine-readable; omit nothing
amux watch rm W-1a2b3c
```

The message you receive looks like:

```
wake-watch: 1 event(s) for mxp-gs12

- W-1a2b3c fired: ci mixpeek/mixpeek run 40217171871, run 40217171871 (C10) completed failure https://github.com/mixpeek/mixpeek/actions/runs/40217171871 (note: then land C10)
```

## Replacing ci_watch.sh

`ci_watch.sh` waited up to 3 hours for a ledger file change, a tracked run to
conclude, or a new run in five workflows, polling `gh` every 60 s from inside
one lane. The same intent, with no loop in the lane:

```bash
amux watch add file --path '/Users/ethan/.amux/tmp/mxp-gs12/handoffs/ledger-*.md' --when changed --expires 3h
amux watch add ci --repo mixpeek/mixpeek --run 40217171871 --expires 3h
for wf in zb-platform-pipeline.yml ephemeral-plane.yml standalone-proof-stack.yml mvs-tests.yml; do
  amux watch add ci --repo mixpeek/mixpeek --workflow "$wf" --expires 3h
done
```

Then end your turn. Each watch fires once; register again if you want the next one.

## How it works

Two existing primitives, configured; nothing new to learn:

- **Filesystem.** Each watch is a JSON file at
  `~/.amux/wake-watch/<lane>/<id>.json`. `amux watch ls` reads them.
- **Scheduler.** `amux watch add` makes sure your lane has one shell schedule,
  `wake-watch: <lane>`, `every 1m`, with `exit_actions {"10": "wake"}`, and
  extends its `until` to cover your latest watch. Each run checks your watches.
  When one fires or expires it prints the message and exits 10, and the
  scheduler's `wake` exit action delivers that output to the schedule's own
  lane (`scheduler.rs` `shell_wake`). A shell run costs no model turn.

Because delivery is your lane's own schedule, it reaches an **isolated** lane:
a schedule is the owner-configured exception in CLAUDE.md "Isolated workers"
(see `deliver_automated`, guard `sched:<id>`). A paused, archived or
unregistered lane is refused, and the refusal is recorded on the run row.

**CI is read by one shared poller per target**, never one loop per lane. The
state lives in `~/.amux/ci-watch/<repo>-<key>/state.json`, the same file
`amux ci wait` uses. Whichever reader finds it older than the CI floor takes the
`poller` lock, makes one GitHub request with the GitHub App token
(`get-token.sh --min-remaining 600`), and writes it; everyone else reads it.
Two lanes watching the same run cost one request a minute.

## Bounds

| bound | default | env |
|---|---|---|
| live watches per lane | 8 | `AMUX_WAKE_WATCH_MAX` |
| expiry (every watch has one) | 6h, at most 7 days | `--expires`, `AMUX_WAKE_WATCH_MAX_TTL_S` |
| poll floor, files | 30 s | `AMUX_WAKE_WATCH_FILE_FLOOR_S` |
| poll floor, CI | 60 s | `AMUX_WAKE_WATCH_CI_FLOOR_S` |
| closed watches kept | 24 h, then pruned | `AMUX_WAKE_WATCH_KEEP_CLOSED_S` |
| delivery attempts per wake | 3, 5 s apart | `AMUX_SHELL_WAKE_ATTEMPTS`, `AMUX_SHELL_WAKE_BACKOFF_S` |

`--every` below the floor is raised to it, and `add` says so.

## Honest negatives

A watch that cannot read its input is **unmeasured**: it never fires and never
reads as "nothing happened". `amux watch ls` shows `UNMEASURED` and the reason
(`cannot list <dir>`, `cannot read <file>`, `http 401: Bad credentials`). A
directory you cannot list is unmeasured even though `glob()` would quietly
return nothing for it. Registering a path you cannot read is refused. A sha
with no runs yet is pending, never success.

## Where to look when it goes wrong

- `~/.amux/logs/wake-watch.log`: one line per registration, check, fire,
  expiry, prune, GitHub poll and failure, each with `verdict=`; failures start
  with `WARN` (`watch_unmeasured`, `watch_cap_refused`, `watch_schedule_failed`,
  `ci_polled ... measured=False`).
- `~/.amux/logs/server-rs.log`: the delivery, `verdict=shell_wake_delivered`,
  `shell_wake_queued`, `shell_wake_retry`, `shell_wake_refused`,
  `shell_wake_failed`, with attempts and elapsed time.
- The schedule's run history (`GET /api/schedules/<id>`, Scheduler page):
  every wake run's delivery status, and the schedule-failure notice when one
  could not be delivered.

## What is not covered

- A watch fires once. There is no repeating watch; register again.
- A wake that the scheduler could not deliver after its attempts is recorded
  as failed on the run row and in the log, and the watch stays closed. It is
  not re-sent on the next tick, so a lane paused for an hour misses that event
  and the owner is told through the schedule-failure notice.
- The check runs from `origin/main` of `AMUX_REPO_DIR` (`~/Dev/amux`), so the
  schedule fails until this script is on main there.
