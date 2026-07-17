# procwarden

Inspect and **selectively kill** macOS processes — without nuking the ones that
crash your machine. CLI + menu bar + a scheduled, config-driven cleanup sweep so
junk (runaways, zombies, memory hogs) doesn't pile up.

Built for an Apple Silicon Mac Studio where Spotlight/`ecosystemd`-class daemons
periodically peg cores; procwarden surfaces exactly what's grinding and lets you
deal with it safely.

## Why not just `top`/Activity Monitor?
- **Accurate per-process CPU & RAM even for root daemons.** psutil under-reports
  per-process CPU on recent macOS, so procwarden measures CPU from kernel `ps`
  cumulative-time deltas and falls back to `ps` RSS where psutil is denied.
- **A safety engine.** Every process is rated `LOW / MEDIUM / HIGH / FATAL`.
  Killing is gated: FATAL is *never* killable; HIGH/MEDIUM need an explicit
  `--force`; LOW (your own apps) is fair game.
- **Scheduled auto-cleanup** you configure with rules, defaulting to **dry-run**.

## Install
```bash
python3 -m pip install psutil rumps
# run via the launcher (no install step needed):
/Users/ethan/Dev/procwarden/bin/procwarden --help
# optional: put it on PATH
ln -s /Users/ethan/Dev/procwarden/bin/procwarden /usr/local/bin/procwarden
```

## CLI
```bash
procwarden list                 # top processes by CPU (default)
procwarden list -s mem -n 20    # by memory, 20 rows
procwarden list -s net --net    # by network (runs nettop; slower)
procwarden list -s time         # longest-running first
procwarden list --author        # resolve code-sign author/source (slower)
procwarden list --killable      # only what's safe to kill
procwarden list --json          # machine-readable

procwarden top                  # live-refreshing view
procwarden inspect <pid>        # full detail + safety verdict for one process

procwarden kill <pid> [pid…]    # SIGTERM with safety gate + confirm
procwarden kill <pid> -y        #   skip confirm
procwarden kill <pid> --escalate#   SIGKILL if it survives SIGTERM 2s
procwarden kill <pid> --force   #   override HIGH/MEDIUM (never FATAL)
procwarden kill <pid> --signal KILL
```

Columns: PID, USER, CPU%, MEM, %MEM, [NET-IN/OUT], UPTIME, THR, **RISK**, NAME,
[AUTHOR]. RISK colors: green=safe, yellow=verify, magenta=system, red=never.

### Dimensions captured
CPU, memory (RSS + %), network in/out (cumulative, via `nettop`), uptime,
thread count, owner, code-sign author/source, parent pid, status (incl. zombie),
and the kill-safety verdict.

> **Disk I/O per process** is intentionally *not* shown: macOS doesn't expose it
> without `sudo` + dtrace (`fs_usage`/`iotop`). System-wide disk usage is shown
> in the header instead, rather than a fabricated column.

## Scheduled cleanup (the anti-junk loop)
```bash
procwarden config init          # write ~/.procwarden/config.json (DRY-RUN default)
procwarden config show
procwarden sweep                # run one sweep now (honors config enforce flag)
procwarden sweep --dry-run      # force log-only even if enforce=true

procwarden schedule install     # launchd agent, every 30 min, survives reboot
procwarden schedule install -i 15
procwarden schedule status
procwarden schedule uninstall
```

### Config (`~/.procwarden/config.json`)
```jsonc
{
  "enforce": false,             // false = DRY RUN (log only). Flip to true to act.
                                //   A RULE may set "enforce": true to act on its own
                                //   even while this global flag keeps the rest dry-run
                                //   (explicit --dry-run still overrides everything).
  "log_file": "~/.procwarden/sweep.log",
  "exclude_names": ["^ssh$", "^tmux", "^procwarden"],
  "rules": [
    { "name": "runaway-cpu",
      "match": { "name_regex": ".*", "owner": "me" },
      "when":  { "cpu_above": 90 },
      "for_sweeps": 3,          // must hold 3 sweeps in a row -> not a momentary spike
      "action": "terminate", "signal": "TERM", "force": false }
  ]
}
```
Rule fields:
- `match`: `name_regex`, `cmdline_regex`, `owner` (`me`/`root`/`any`/user), `status`
- `when` (all must hold): `cpu_above`, `cpu_below`, `mem_above_mb`,
  `uptime_above_hours`, `net_above_mb`
- `for_sweeps`: consecutive matching sweeps before firing (sustained detection)
- `action`: `terminate` or `report`; `force` allows HIGH/MEDIUM (never FATAL)

The **safety gate always applies on top of rules** — a rule can never terminate a
FATAL process, and won't touch HIGH/MEDIUM unless that rule sets `force: true`.

**Recommended rollout:** leave `enforce:false`, let it run a day, read
`~/.procwarden/sweep.log` (each line is JSON: what *would* have been killed), then
flip `enforce:true` once the rules look right.

## Nightly maintenance (keep a 24/7 box snappy without rebooting)
A sweep only *kills* processes. Keeping a long-uptime server fast needs a few
non-process levers too — so `procwarden maintain` runs them in one nightly pass:

```bash
procwarden maintain               # run the pass now
procwarden maintain --dry-run     # preview: purge/spotlight/reap all log-only
procwarden maintain --json        # machine-readable summary
procwarden schedule nightly       # launchd agent, daily at 03:00, survives reboot
procwarden schedule nightly --at 04:30
procwarden schedule status        # shows both the sweep and the nightly agent
```

What the pass does each night:
1. **purge** — reclaims inactive/cached memory, relieving swap thrashing.
2. **spotlight** — drops a `.metadata_never_index` sentinel in each dir of
   `maintenance.spotlight_exclude` (per-directory, no root; `mdutil -i off` only
   works at *volume* granularity). Starves `mds`/`mdworker` of index churn on hot
   dirs — useful when *indexing* is the load. Note: it does not quiet `fseventsd`
   itself (SIP-protected, logs events regardless); if your churn is real workload
   (e.g. a `git`/build loop), fix that at the source instead.
3. **health** — watches memory %, swap, and indexing daemons (`fseventsd`,
   `ecosystemd`, `mds`, …); on a **new** concern it posts to the amux board
   (deduped against the last run, so a persistent issue never spams nightly).
4. **reap** — runs the normal config sweep; obeys `enforce` (dry-run until you
   trust it), so it never kills your live sessions without opt-in.

Separation by risk: purge/spotlight/health are non-destructive and run whenever
their config flag is on; only the reap is gated behind `enforce`.

### Config (`maintenance` block)
```jsonc
"maintenance": {
  "enabled": true,
  "purge_memory": true,
  "run_sweep": true,
  "spotlight_exclude": [],        // e.g. ["~/.amux", "/path/to/vm-images"]
  "watch_daemons": ["fseventsd", "ecosystemd", "mds", "mds_stores", "mdworker"],
  "health": { "mem_used_pct_above": 90, "swap_used_mb_above": 6000,
              "daemon_cpu_above": 80 },
  "alert":  { "post_board": true, "urgent_owner_alert": false,
              "amux_base": "https://localhost:8822",
              "session": "procwarden-maintain" }
}
```

### Privilege (purge + mdutil need root)
The pass runs unprivileged as you; only `purge` and `mdutil` are elevated, via a
narrow passwordless-sudo drop-in (least privilege — nothing else is granted):

```bash
procwarden maintain --print-sudoers    # shows the exact drop-in
# install it (validates with visudo before writing):
procwarden maintain --print-sudoers | sudo bash -c \
  'f=$(mktemp); cat > "$f"; visudo -cf "$f" && \
   install -m 440 -o root -g wheel "$f" /etc/sudoers.d/procwarden && echo OK; rm -f "$f"'
```
Without it, `purge`/`mdutil` degrade to `needs-privilege` and the rest of the pass
still runs (memory just isn't reclaimed). Logs: `~/.procwarden/maintain.log`
(JSON, one pass per line) and `maintain.out.log`/`maintain.err.log` (launchd).

## Menu bar
```bash
procwarden menubar              # 🛡 in the menu bar
```
Shows the heaviest processes (toggle CPU/Memory/Network), inspect any of them,
and **kill only safe (LOW) ones** from the menu. System/root/FATAL processes show
🔒 Protected — overriding those is a deliberate CLI `--force` action.

## Safety model
| RISK   | what it is                                   | killable?            |
|--------|----------------------------------------------|----------------------|
| FATAL  | kernel_task, launchd, WindowServer, mds, …   | never                |
| HIGH   | root Apple system daemon (`/System`, …)      | CLI `--force` only   |
| MEDIUM | another user's / root non-system process     | CLI `--force` only   |
| LOW    | your own app, or an auto-relaunching agent    | yes                  |
| NONE   | zombie (already dead; reaped by parent)       | n/a                  |

State for sustained-threshold detection lives in `~/.procwarden/state.json`.
