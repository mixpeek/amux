#!/bin/bash
# mac-cleanup-tick.sh — the scheduled machine-cleanup tick.
#
# WHAT IT ACTS ON, and it is a short list on purpose:
#   1. `purge`, when the kernel says the machine is under memory pressure or
#      free memory is under a floor. On 2026-09-15 at 10:00 one purge took free
#      memory from 3.6 GB to 14.0 GB. It drops caches; it cannot lose work.
#   2. Claude Code shell-snapshot bash processes older than 6 hours. These are
#      background watchers (build polls, dev servers, CI loops) that outlive
#      their session. One held a busy-loop at 100% CPU for 21 hours.
#   3. A launchd agent named in AMUX_CLEANUP_AGENTS whose footprint has grown
#      past AMUX_CLEANUP_AGENT_LEAK_GB. The prompting case is
#      com.procwarden.menubar, which leaked to 27 GB over 15 days and again to
#      1.3 GB within 13 hours of a restart. KeepAlive brings it straight back,
#      so a restart costs nothing but the leak.
#   4. Cargo `target/` directories nothing has written to for 24 hours and no
#      process holds open. Build output is regenerable by definition and was the
#      largest single class of waste on this box: 127 cache-signed directories
#      held 233 GB on 2026-09-26 and one hand pass over the idle ones freed
#      66.8 GiB (DESKT-51). The scan is bounded, every guard fails closed (an
#      unmeasurable directory is kept and the line says so), and the shared
#      target is never a candidate.
#
#   5. ASSESS after acting (DESKT-57). Re-measure, name each resource that is
#      still constrained or trending toward it (disk floor or hours-to-full at
#      the measured burn rate, memory pressure or swap, CPU share, a runaway
#      process family), write an RCA bundle with the evidence, and hand every
#      still-constrained class to a model turn on the desktop lane, at most
#      once per cooldown per class. The shell fixes symptoms because that is
#      computable; finding and fixing the underlying cause is judgment, so it
#      goes to a model with the evidence already gathered (ethos rule 2).
#
# WHAT IT ONLY REPORTS, however large it gets:
#   Everything else. fseventsd held 96 GB on this box and macOS protects it; a
#   peer lane's colima VM held 16 GB plus 31 GB compressed and that lane was
#   using it; Chrome and Docker are the human's. Killing another party's work to
#   reclaim memory is a decision for its owner (ethos rule 8), so this names the
#   consumer, its owner and the remedy, and stops there.
#
# WHY sudo appears at all: on this box passwordless sudo is limited to
# /usr/sbin/purge and /usr/bin/mdutil. `purge` is therefore the ONE reclaim this
# script can perform as root, and a reboot (the only remedy for fseventsd) needs
# a password nobody can type for it.
#
# Usage:
#   scripts/mac-cleanup-tick.sh             # measure, act where warranted
#   scripts/mac-cleanup-tick.sh --dry-run   # measure, act on nothing
set -uo pipefail

# launchd's PATH has no /usr/sbin, and sysctl/purge/vm_stat live there. Its
# sibling mac-pressure-tripwire.sh reported measured=false for exactly this
# reason (AMUX-4661), so this one exports the PATH before any probe runs.
PATH="/usr/local/bin:/opt/homebrew/bin:/usr/bin:/bin:/usr/sbin:/sbin:${PATH:-}"
export PATH

# Every AMUX_CLEANUP_* knob below is configurable in amux's GLOBAL scope (the
# Scope tab, ~/.amux/amux.env), DESKT-72: a value set there applies to every
# tick, scheduled or fallback, without editing SCHED-465's command. An explicit
# environment value (the schedule's own, a test's) still wins.
scope_cleanup_knobs() { # <scope file>
  local f=$1 line k v
  [ -f "$f" ] || return 0
  while IFS= read -r line || [ -n "$line" ]; do
    case "$line" in AMUX_CLEANUP_[A-Z0-9_]*=*) ;; *) continue ;; esac
    k=${line%%=*}; v=${line#*=}
    v=${v#\"}; v=${v%\"}; v=${v#\'}; v=${v%\'}
    [ -n "${!k+x}" ] && continue
    export "$k=$v"
  done < "$f"
}
scope_cleanup_knobs "${AMUX_CLEANUP_SCOPE_FILE:-${AMUX_HOME:-$HOME/.amux}/amux.env}"

DRY=0
for a in "$@"; do
  case "$a" in
    --dry-run) DRY=1 ;;
    -h|--help) sed -n '2,/^set -uo pipefail$/p' "$0" | sed '$d'; exit 0 ;;
    *) echo "unknown argument: $a" >&2; exit 2 ;;
  esac
done

# One tick at a time. A run the scheduler timed out keeps going, and the next fire
# started beside it (two ticks were measured running at once on 2026-09-26).
# mkdir is atomic, so it is the lock, and it holds the owner's PID. A lock whose
# PID is not alive belongs to a dead run and is taken over at once: the server
# restarts on every deploy and SIGKILLs a tick in progress, which never runs its
# exit trap, and an age-only rule would then block every tick for the whole
# stale window. The age rule stays as the backstop for a lock with no PID.
lock_live() { # <lock_dir> <stale_min> -> 0 if a live tick holds it
  local pid
  [ -d "$1" ] || return 1
  pid=$(cat "$1/pid" 2>/dev/null)
  if [ -n "$pid" ]; then kill -0 "$pid" 2>/dev/null; return; fi
  [ -z "$(find "$1" -maxdepth 0 -mmin "+$2" 2>/dev/null)" ]
}
tick_lock() { # <lock_dir> <stale_min>
  local d=${1:?}
  if ! mkdir "$d" 2>/dev/null; then
    lock_live "$d" "$2" && return 1
    rm -f -- "${d:?}/pid"; rmdir "${d:?}" 2>/dev/null
    mkdir "$d" 2>/dev/null || return 1
  fi
  echo $$ > "$d/pid"
}

# ── knobs ────────────────────────────────────────────────────────────────────
# Pressure >= this purges. 2 is the kernel's "warn"; it self-clears often, which
# is why the PAGING tripwire ignores 2 and this one does not: dropping caches is
# free, so acting early is cheap and acting late is not.
PRESSURE_PURGE=${AMUX_CLEANUP_PRESSURE_PURGE:-2}
FREE_FLOOR_GB=${AMUX_CLEANUP_FREE_FLOOR_GB:-4}
AGENT_LEAK_GB=${AMUX_CLEANUP_AGENT_LEAK_GB:-2}
AGENTS=${AMUX_CLEANUP_AGENTS:-com.procwarden.menubar}
REPORT_GB=${AMUX_CLEANUP_REPORT_GB:-10}
FSEVENTSD_REBOOT_GB=${AMUX_CLEANUP_FSEVENTSD_REBOOT_GB:-5}
FSEVENTSD_CPU_PCT=${AMUX_CLEANUP_FSEVENTSD_CPU_PCT:-80}
# When fseventsd is hot, name what is feeding it (DESKT-81). On 2026-10-03 it grew
# from under 10G to 20G between 03:00 and 10:00, and to 76G before the owner
# rebooted on 10-05. The tick printed the size every half hour and never the
# cause; a manual census found one lane scratchpad writing 14,708 files in five
# minutes by re-extracting a source tree before every check. The census counts
# files modified in the last CHURN_MIN minutes under each root, groups them three
# directory levels down, and names the groups over CHURN_FILES. It cannot see
# deletions, which feed fseventsd too (the 10-04 lane-tmp pass), so it says so.
CHURN_ROOTS=${AMUX_CLEANUP_CHURN_ROOTS:-/private/tmp/claude-$(id -u):$HOME/Dev:$HOME/.amux}   # not overlapping: an overlap counts a file twice
CHURN_MIN=${AMUX_CLEANUP_CHURN_MIN:-5}
CHURN_FILES=${AMUX_CLEANUP_CHURN_FILES:-2000}
CHURN_BUDGET_S=${AMUX_CLEANUP_CHURN_BUDGET_S:-60}
# Seam: prints "<gb> <cpu_pct>" for fseventsd, or nothing when it is not running.
# The suites stub it so the live daemon cannot decide a test (as PRESSURE_CMD does).
FSEVENTSD_CMD=${AMUX_CLEANUP_FSEVENTSD_CMD:-fseventsd_reading}
# APFS local snapshots pin deleted blocks, so a reaper can delete 50 GB and free
# nothing until they expire (DESKT-26 measured exactly that: a thin returned
# 68.7 GB). Thinning is BOUNDED and disk-triggered because these snapshots are
# the owner's short-window restore path: DESKT-13 thinned by hand and the space
# was gone again within days, which is why this is an arm and not a one-shot.
SNAPSHOT_FLOOR_GB=${AMUX_CLEANUP_SNAPSHOT_FLOOR_GB:-100}
SNAPSHOT_RECLAIM_GB=${AMUX_CLEANUP_SNAPSHOT_RECLAIM_GB:-50}
SNAPSHOT_URGENCY=${AMUX_CLEANUP_SNAPSHOT_URGENCY:-2}
# Lima/colima VM data disks. On 2026-09-24 ~/.colima held 330.9 GB in nine of them
# while the disk sat at 1.8 GB free, and nothing in amux named it: disk_watch's
# named list does not include it and this tick reported only processes. Six of the
# nine (162.7 GB) belonged to no registered VM at all (DESKT-47).
LIMA_ROOT=${AMUX_CLEANUP_LIMA_ROOT:-$HOME/.colima/_lima}
LIMA_SHOW_KB=${AMUX_CLEANUP_LIMA_SHOW_KB:-10485760}
# A process FAMILY is what actually took this box down: on 2026-08-29 local Ray
# held 194 worker processes and 44.35 GB under one parent for 40 hours, and the
# watcher of the day only had a rule for zombies, so it reported 79 harmless
# reaped-parent entries and never once mentioned Ray (DESKT-31). Per-process
# ranking misses it too, because no single child is large.
FAMILY_SHARE_PCT=${AMUX_CLEANUP_FAMILY_SHARE_PCT:-15}
FAMILY_AGE_H=${AMUX_CLEANUP_FAMILY_AGE_H:-12}
# Idle cargo target dirs (DESKT-51). The roots are where lanes actually leave them:
# per-session Claude scratchpads, the per-user temp dir, Codex work dirs, the repos,
# and the amux/ao worktree stores. 24h idle matches scripts/reap-amux-debris.sh's
# side-target floor: a lane's build touches its target constantly, so a full idle
# day is a strong signal and being wrong costs one rebuild. The shared target is
# protected by KEEP, not by luck. The scan and the deletions each carry a time
# budget so one slow disk cannot stretch a 30-minute tick past its schedule.
# A root may carry a scan depth as path@N. The per-user temp dir holds only shallow targets but is
# full of leaked clones, and scanning it to the default depth measured 40s of a 110s scan.
# TMPDIR may be unset under the scheduler, and `/tmp` is a symlink that find does not follow, so the
# per-user temp dir comes from getconf when TMPDIR is empty rather than from a symlink that scans nothing.
USER_TMP=${TMPDIR:-$(getconf DARWIN_USER_TEMP_DIR 2>/dev/null || echo /tmp)}
TARGET_ROOTS=${AMUX_CLEANUP_TARGET_ROOTS:-/private/tmp/claude-$(id -u)@6:/private/tmp@3:${USER_TMP%/}@3:$HOME/Documents/Codex@8:$HOME/Dev@8:$HOME/.amux/worktrees@6:$HOME/.ao/data/worktrees@8}
TARGET_IDLE_H=${AMUX_CLEANUP_TARGET_IDLE_H:-24}
# Under disk pressure the idle floor drops (DESKT-62). On 2026-10-01 the disk fell
# from 574G to 155G in five days, and about 60G of build output sat 21-23h idle in
# one lane's scratchpad, just inside the 24h floor, on every tick that measured it.
# Build output is regenerable, every other guard still applies (open handles,
# dry run, the shared targets), and being wrong costs one rebuild.
TARGET_TIGHT_FREE_GB=${AMUX_CLEANUP_TARGET_TIGHT_FREE_GB:-250}
TARGET_TIGHT_IDLE_H=${AMUX_CLEANUP_TARGET_TIGHT_IDLE_H:-6}
# Docker build cache inside running colima VMs (DESKT-69). On 2026-10-01 the disk
# reached 10G free with five gs12 VMs running; four held 4-13G of build cache
# each, and pruning it plus an fstrim gave back 66G in minutes. Build cache is
# regenerable by definition. Images, containers and volumes are never touched:
# an image may be the only copy of a local build, a volume may be data.
# Runs only under the same disk threshold as the tight build-output floor.
# Seams: the test points these at recorders. PROFILE is the colima profile.
VM_LIST_CMD=${AMUX_CLEANUP_VM_LIST_CMD:-colima list --json}
# -a with an age filter (2026-10-04): `builder prune -f` removes only dangling
# cache, so the leases BuildKit keeps for finished builds pinned their
# snapshots: goal-shared held 366 leases and 369 snapshots for 10 images, 93 GB
# in containerd's overlay store, and the host disk burned 26 G/h. `-af --filter
# until=6h` released 78.8 GB there (leases 366 -> 130) and keeps the last 6 h
# of cache warm for builds in flight.
VM_PRUNE_CMD=${AMUX_CLEANUP_VM_PRUNE_CMD:-docker --context colima-PROFILE builder prune -af --filter until=AGE}
# Build cache unused this long is pruned when the disk is tight. Under the
# urgent floor the window shortens: 2026-10-06 the gs12 shared VM wrote build
# cache at ~78G/h, so almost nothing was 6h old and the host fell toward 40G
# free while 31G of 2h-old cache sat reclaimable.
VM_PRUNE_AGE=${AMUX_CLEANUP_VM_PRUNE_AGE:-6h}
VM_PRUNE_URGENT_AGE=${AMUX_CLEANUP_VM_PRUNE_URGENT_AGE:-2h}
VM_PRUNE_URGENT_FREE_GB=${AMUX_CLEANUP_VM_PRUNE_URGENT_FREE_GB:-100}
# A SIZE cap on top of the age window (disk RCA 20261007-081932). gs12-restore
# builds a full mixpeek/standalone image per commit, ~10G of fresh cache each
# (pip install 3.5G, site-packages copy 3.5G, model downloads 2G), so the 6 h
# window alone let goal-shared hold 78G of cache and the host burned 14.5G/h.
# BuildKit evicts least-recently-used records first and never one a running
# build holds, so the cap keeps the newest builds warm. 0 disables it.
VM_PRUNE_MAX_USED=${AMUX_CLEANUP_VM_PRUNE_MAX_USED:-40gb}
VM_PRUNE_CAP_CMD=${AMUX_CLEANUP_VM_PRUNE_CAP_CMD:-docker --context colima-PROFILE builder prune -af --max-used-space CAP}
VM_TRIM_CMD=${AMUX_CLEANUP_VM_TRIM_CMD:-colima ssh -p PROFILE -- sudo fstrim -a}
# Unused IMAGES in running colima VMs, past an age, every tick (MF-4043):
# goal-shared reached 39 images / 148 GB (137 GB unused) on 2026-10-03 and the
# host fell from 96 to 51 GB free in ~25 min. An image a container uses, or
# one built within the age, is kept; the rest is rebuildable cache.
VM_IMAGE_PRUNE_AGE=${AMUX_CLEANUP_VM_IMAGE_PRUNE_AGE:-6h}
# The per-user temp dir ($TMPDIR, /var/folders/.../T): mktemp -d extracts that
# a killed process never removed. 2026-10-03: 5,561 tmp.* dirs, 1,674 of them
# over a day old holding 49 GB (full Mixpeek repo extracts left by hook runs a
# land precheck had to kill), while the host fell toward 100 GB free.
USER_TMP_ROOT=${AMUX_CLEANUP_USER_TMP_ROOT:-$(getconf DARWIN_USER_TEMP_DIR 2>/dev/null || printf '%s' "${TMPDIR:-/tmp}")}
USER_TMP_IDLE_MIN=${AMUX_CLEANUP_USER_TMP_IDLE_MIN:-1440}
# Workers with workspace isolation get TMPDIR=~/.amux/tmp/<worker>, outside the
# per-user temp dir above, so nothing reaped them: 101 GB in 13,336 entries
# idle over 12 h on 2026-10-04 with the disk at 57 GB free.
LANE_TMP_ROOT=${AMUX_CLEANUP_LANE_TMP_ROOT:-$HOME/.amux/tmp}
LANE_TMP_IDLE_MIN=${AMUX_CLEANUP_LANE_TMP_IDLE_MIN:-1440}
# At most this many entries per tick. Deleting 11,781 entries (millions of
# files) in one pass on 2026-10-04 drove fseventsd from 9 to 57 GB resident
# and filled 52 GB of swap; a capped pass leaves the rest for the next tick.
LANE_TMP_MAX=${AMUX_CLEANUP_LANE_TMP_MAX:-300}
# AGE IS MEASURED FROM THE LAST TAG, not from the image's creation (disk RCA
# 20261007-081932). `image prune --filter until=` reads Created, and a build
# that hits the cache tags an image whose Created can be days old, so a tag
# made a minute ago was pruned on the next tick. gs12-deputy saw tags vanish
# within an hour or two, could not find the pruner, and pinned every build
# with a never-started container; nothing retired the pins, and each ~10G
# standalone image stayed for good. vm_image_prune_by_tag keeps any image a
# container uses or that was tagged (or, never tagged, created) within AGE.
VM_IMAGE_PRUNE_CMD=${AMUX_CLEANUP_VM_IMAGE_PRUNE_CMD:-vm_image_prune_by_tag colima-PROFILE AGE}
VM_DOCKER=${AMUX_CLEANUP_VM_DOCKER:-docker}
VM_STEP_S=${AMUX_CLEANUP_VM_STEP_S:-120}
# Idle colima VMs under pressure (DESKT-70). On 2026-10-01 eight goal-spec lanes
# had each started a private VM (16-32G apiece); with five running the Mac hit
# load 116 and memory pressure 2, and the one idle VM stopped by hand freed ~80G
# of footprint. A VM is IDLE when its guest 15-minute load is under
# VM_IDLE_LOAD and docker recorded no event other than a healthcheck exec in the
# last VM_IDLE_MIN minutes. Stopping is reversible (`colima start -p <p>`;
# images, containers and volumes stay on disk), it runs only under memory
# pressure or the tight-disk threshold, and every stop is recorded with its
# restore command in $STATE_DIR/vm-stops.log.
VM_IDLE_LOAD=${AMUX_CLEANUP_VM_IDLE_LOAD:-0.5}
# Seam for the kernel pressure reading the assess stage and the idle-VM arm act on,
# so a suite on a loaded Mac does not inherit the machine's live pressure (DESKT-70).
PRESSURE_CMD=${AMUX_CLEANUP_PRESSURE_CMD:-sysctl -n kern.memorystatus_vm_pressure_level}
VM_IDLE_MIN=${AMUX_CLEANUP_VM_IDLE_MIN:-60}
# Seams. PROFILE is the colima profile. The load probe prints /proc/loadavg; the
# events probe prints one docker event action per line.
VM_LOAD_CMD=${AMUX_CLEANUP_VM_LOAD_CMD:-colima ssh -p PROFILE -- cat /proc/loadavg /proc/uptime}
VM_EVENTS_CMD=${AMUX_CLEANUP_VM_EVENTS_CMD:-docker --context colima-PROFILE events --since MINm --until 0s --format \{\{.Action\}\}}
# Running containers: any one not in VM_IDLE_IGNORE makes the VM busy. A proof
# stack waiting between legs only runs healthchecks, and on 2026-10-04 at
# 18:06Z this tick stopped goal-shared under memory pressure with
# gs12-restore's gr98 stack in it, blocking every lane's local proofs.
VM_PS_CMD=${AMUX_CLEANUP_VM_PS_CMD:-docker --context colima-PROFILE ps --format \{\{.Names\}\}}
VM_IDLE_IGNORE=${AMUX_CLEANUP_VM_IDLE_IGNORE:-gs12-pypi-cache}
VM_STOP_CMD=${AMUX_CLEANUP_VM_STOP_CMD:-colima stop -p PROFILE}
# Docker Desktop runs its own VM that `colima list` never shows. On 2026-10-05
# it held 16 GB resident plus 30 GB compressed under memory pressure 2, three
# days after its last container (escalation 20261004-213936). Same rule as a
# colima VM: quit it only when it has been up the whole window with no running
# container and no docker event but healthcheck execs; unreadable is busy.
# Its images and volumes stay; it starts again on demand.
DD_UP_CMD=${AMUX_CLEANUP_DD_UP_CMD:-ps -o etime= -p PIDOF}
# -f on the path: macOS truncates a process name to 16 characters, so
# `pgrep -x com.docker.backend` never matches and would read "not running".
DD_PID_CMD=${AMUX_CLEANUP_DD_PID_CMD:-pgrep -f /Docker.app/Contents/MacOS/com.docker.backend}
DD_PS_CMD=${AMUX_CLEANUP_DD_PS_CMD:-docker --context desktop-linux ps --format \{\{.Names\}\}}
DD_EVENTS_CMD=${AMUX_CLEANUP_DD_EVENTS_CMD:-docker --context desktop-linux events --since MINm --until 0s --format \{\{.Action\}\}}
# Empty means the built-in quit below: the app name has a space, which an
# unquoted seam would split.
DD_STOP_CMD=${AMUX_CLEANUP_DD_STOP_CMD:-}
TARGET_KEEP=${AMUX_CLEANUP_TARGET_KEEP:-$HOME/.amux/rust-build-target:$HOME/.ao/data/cargo-target-shared:${CARGO_TARGET_DIR:-}}
TARGET_DEPTH=${AMUX_CLEANUP_TARGET_DEPTH:-8}
TARGET_SCAN_S=${AMUX_CLEANUP_TARGET_SCAN_S:-90}
TARGET_WALK_S=${AMUX_CLEANUP_TARGET_WALK_S:-60}
TARGET_BUDGET_S=${AMUX_CLEANUP_TARGET_BUDGET_S:-200}
LSOF_CMD=${AMUX_CLEANUP_LSOF_CMD:-lsof -nP}
# Idle detached-worktree scratch checkouts (MO-3631/MO-3633, recurred 3 times in
# one day at up to 24GB per incident: gs-10-zero-base-cicd alone, an isolated
# worker that cannot be messaged, created a fresh ~4GB detached mixpeek checkout
# under the shared per-user temp dir every 10-90 minutes and never removed the
# previous one). The cargo-target arm above is structurally blind to this class:
# it prunes `.git` on purpose, since a target dir's OWN safety never depended on
# git history. A worktree's does, so this arm is deliberately narrower and does
# NOT reuse TARGET_ROOTS: it never scans /private/tmp/claude-* (live session
# scratchpads, DESKT-58 -- MO-3627 already cost a real mistake sweeping one) or
# ~/Dev or Documents/Codex (a real clone or a Codex checkout can legitimately
# hold a worktree that IS the point of the directory, not scratch beside it).
# Roots are exactly the shared, generic scratch locations observed to actually
# accumulate this: the per-user temp dir, and the two committed worktree stores.
WORKTREE_ROOTS=${AMUX_CLEANUP_WORKTREE_ROOTS:-${USER_TMP%/}@2:$HOME/Dev/amux-worktrees@1:$HOME/.amux/worktrees@1:$HOME/.ao/data/worktrees@1}
# Short idle floor on purpose: the tick runs every 30 minutes and this class
# accumulates on a 10-90 minute cadence, so a 24h floor (right for a cargo
# target, rebuilt in minutes) would let dozens pile up before the first one
# qualifies. 2h is long enough that the generator's OWN current round is
# essentially never mistaken for a superseded one (MO-3631/3633 measured
# 10-90 min between rounds by hand). A whole number on purpose: dir_idle's
# hours*60 is integer bash arithmetic, and a fractional hour there is a
# syntax error, not a rounding one.
WORKTREE_IDLE_H=${AMUX_CLEANUP_WORKTREE_IDLE_H:-2}
WORKTREE_SCAN_S=${AMUX_CLEANUP_WORKTREE_SCAN_S:-60}
WORKTREE_WALK_S=${AMUX_CLEANUP_WORKTREE_WALK_S:-30}
WORKTREE_BUDGET_S=${AMUX_CLEANUP_WORKTREE_BUDGET_S:-120}
WORKTREE_DEPTH=${AMUX_CLEANUP_WORKTREE_DEPTH:-2}
# Assessment (DESKT-57). A disk is constrained under DISK_FLOOR_GB, or when the
# burn since the previous tick would fill it within HOURS_TO_FULL. Memory is
# constrained when the kernel still reports pressure after the purge arm ran,
# or swap has under SWAP_FREE_FLOOR_MB left. CPU is constrained when the 15-min
# load average exceeds CPU_SHARE of the cores.
DISK_FLOOR_GB=${AMUX_CLEANUP_DISK_FLOOR_GB:-150}
HOURS_TO_FULL=${AMUX_CLEANUP_HOURS_TO_FULL:-24}
BURN_MIN_GBH=${AMUX_CLEANUP_BURN_MIN_GBH:-1}
SWAP_FREE_FLOOR_MB=${AMUX_CLEANUP_SWAP_FREE_FLOOR_MB:-512}
# macOS never proactively reclaims swap: once a page is written out, it sits
# in the swapfile until the owning process touches it again, long after the
# pressure spike that caused it has passed. MO-3655/MO-3657 measured
# swap_free PINNED at the same 438.06MB across multiple hours while
# kern.memorystatus_vm_pressure_level stayed 1 (normal) and load/free% both
# visibly improved around it -- a floor read alone cannot tell "actively
# low" from "low once, years ago." Below this fraction of swap_total free is
# still trusted even with pr==1, since the kernel's own pressure read can
# lag a genuine fast-moving emergency.
SWAP_FREE_CRITICAL_PCT=${AMUX_CLEANUP_SWAP_FREE_CRITICAL_PCT:-0.10}
CPU_SHARE=${AMUX_CLEANUP_CPU_SHARE:-0.9}
STATE_DIR=${AMUX_CLEANUP_STATE_DIR:-$HOME/.amux/logs/mac-cleanup}

# Claude Code session temp dirs (DESKT-77). Claude Code gives every session
# /private/tmp/claude-<uid>/<project>/<session-uuid>/{scratchpad,tasks} and never
# removes it. On 2026-10-02 that tree held 214G in 258 session dirs, while the disk
# sat at 35G free: 84G in sessions whose transcript had not been written for two
# days, and one live session at 45.8G of leftover 2.1G repo tarballs. Two rules:
#  1. A DEAD session dir is removed: its transcript (~/.claude/projects/<project>/
#     <uuid>.jsonl, or the <uuid>/ subagent dir) and every file inside it are idle
#     for CLAUDE_TMP_IDLE_H (CLAUDE_TMP_TIGHT_IDLE_H when the disk is tight), no
#     process holds anything in it open, and no git worktree inside it is dirty.
#  2. A LIVE session over CLAUDE_TMP_QUOTA_GB loses its biggest top-level scratchpad
#     entries that are idle for CLAUDE_TMP_ENTRY_IDLE_H and not open, until it is
#     under quota. A git worktree entry is never removed by this rule.
# Every removal is appended to $STATE_DIR/claude-tmp-reaps.log. CLAUDE_TMP_QUOTA_GB=0
# turns rule 2 off; CLAUDE_TMP_IDLE_H=0 turns rule 1 off.
CLAUDE_TMP_ROOT=${AMUX_CLEANUP_CLAUDE_TMP_ROOT:-/private/tmp/claude-$(id -u)}
CLAUDE_PROJECTS=${AMUX_CLEANUP_CLAUDE_PROJECTS:-$HOME/.claude/projects}
CLAUDE_TMP_IDLE_H=${AMUX_CLEANUP_CLAUDE_TMP_IDLE_H:-48}
CLAUDE_TMP_TIGHT_IDLE_H=${AMUX_CLEANUP_CLAUDE_TMP_TIGHT_IDLE_H:-24}
CLAUDE_TMP_QUOTA_GB=${AMUX_CLEANUP_CLAUDE_TMP_QUOTA_GB:-20}
CLAUDE_TMP_ENTRY_IDLE_H=${AMUX_CLEANUP_CLAUDE_TMP_ENTRY_IDLE_H:-6}
# An entry under this size is never removed by the quota rule. Without it the first
# live run (2026-10-02) removed 194 small scratch files (scripts, logs, card JSON)
# from three live sessions while chasing a quota their size could not affect;
# the 10 removals that mattered were all over 1G.
CLAUDE_TMP_ENTRY_MIN_MB=${AMUX_CLEANUP_CLAUDE_TMP_ENTRY_MIN_MB:-256}
CLAUDE_TMP_WALK_S=${AMUX_CLEANUP_CLAUDE_TMP_WALK_S:-45}   # 15s measured too short: a 16G idle session walks in 8-10s unloaded, longer under load
CLAUDE_TMP_BUDGET_S=${AMUX_CLEANUP_CLAUDE_TMP_BUDGET_S:-150}
# Not the desktop lane: it is ISOLATED, and amux refuses automated sends into an
# isolated worker by design (round 1 of DESKT-57 measured exactly that refusal).
# mac-ops is a non-isolated sonnet worker whose brief is the runbook below.
ESCALATE_TO=${AMUX_CLEANUP_ESCALATE_TO:-mac-ops}
ESCALATE_COOLDOWN_H=${AMUX_CLEANUP_ESCALATE_COOLDOWN_H:-6}
# Seam: FILE is replaced with the message path. The test points this at a recorder.
ESCALATE_CMD=${AMUX_CLEANUP_ESCALATE_CMD:-amux send TARGET --file FILE}
RUNBOOK=docs/runbooks/mac-resource-rca.md
# Seam: FILE is replaced with a JSON body. The test points this at a recorder.
# Not written as ${AMUX_CLEANUP_CARD_CMD:-curl ...}: the `}` in curl's %{http_code}
# would close that expansion early and run the rest of the line as a command.
if [ -n "${AMUX_CLEANUP_CARD_CMD:-}" ]; then CARD_CMD=$AMUX_CLEANUP_CARD_CMD
else CARD_CMD="curl -sk --max-time 20 -o /dev/null -w %{http_code} -X POST -H Content-Type:application/json -H X-Amux-Session:desktop --data @FILE $(amux url 2>/dev/null || echo https://localhost:8824)/api/board"; fi
SENDFAIL_CARD_AFTER=${AMUX_CLEANUP_SENDFAIL_CARD_AFTER:-2}
# The scheduler kills a shell schedule at 600s ("delivery failed: timed out after
# 600s", twice on 2026-09-26 once the disk-writer search switched on under load
# 48). The budgets are sized to stay under it, and the done line prints elapsed
# time with a WARN past TICK_WARN_S so the next regression announces itself.
TICK_WARN_S=${AMUX_CLEANUP_TICK_WARN_S:-480}
WRITERS_BUDGET_S=${AMUX_CLEANUP_WRITERS_BUDGET_S:-150}
LOCK_STALE_MIN=${AMUX_CLEANUP_LOCK_STALE_MIN:-30}
# Seams: the tests point these at a recorder so an action can be observed
# without running it. Defaults are what the scheduler actually runs.
PURGE_CMD=${AMUX_CLEANUP_PURGE_CMD:-sudo -n /usr/sbin/purge}
THIN_CMD=${AMUX_CLEANUP_THIN_CMD:-tmutil thinlocalsnapshots / BYTES URGENCY}
SNAP_LIST_CMD=${AMUX_CLEANUP_SNAP_LIST_CMD:-tmutil listlocalsnapshots /}
RESTART_CMD=${AMUX_CLEANUP_RESTART_CMD:-launchctl kickstart -k gui/UID/LABEL}
# Claude Code shell-snapshots older than this many hours are killed. These are
# background bash processes that Claude Code leaves behind: `until` loops
# waiting for builds, dev servers, CI watchers. One of them held a tight
# busy-loop (`until [ -s /dev/null ]`) at 100% CPU for 21 hours (2026-09-19).
STALE_SHELL_SNAPSHOT_H=${AMUX_CLEANUP_STALE_SHELL_SNAPSHOT_H:-6}
# Seam: the test points this at a fixture so the arm can be fed elapsed times
# without a real process. The default is what the scheduler actually runs.
STALE_PS_CMD=${AMUX_CLEANUP_STALE_PS_CMD:-ps -eo pid=,etime=,args=}

# ── pure decisions (no side effects, so the tests can exercise them) ──────────

# Purge when the kernel reports pressure at or above the trigger, OR free memory
# is under the floor. Two triggers because either alone misses a real case: a
# machine can sit at pressure 1 with free memory near zero, and it can report
# pressure 2 with free memory that looks fine.
should_purge() { # <pressure_level> <free_gb> <pressure_trigger> <free_floor>
  awk -v p="$1" -v f="$2" -v pt="$3" -v ff="$4" \
    'BEGIN{ exit !((p+0 >= pt+0 && p+0 > 0) || f+0 < ff+0) }'
}

should_restart_agent() { # <footprint_gb> <leak_gb>
  awk -v f="$1" -v t="$2" 'BEGIN{ exit !(f+0 >= t+0 && t+0 > 0) }'
}

# A label this script may hand to launchctl. Anything else is refused rather
# than interpolated into a command line.
is_safe_label() { # <label>
  case "$1" in
    *[!A-Za-z0-9._-]*) return 1 ;;
    com.*) return 0 ;;
    *) return 1 ;;
  esac
}

# Who owns a process, so the report says who can act on it rather than leaving
# a reader to guess. Deliberately coarse: the three answers differ in WHO acts.
classify_owner() { # <user> <command>
  case "$2" in
    *llama-server*|*/Ollama.app/*)
      # Named specially because the remedy is one command and nobody guesses it
      # from "user process": Ollama held qwen3-coder:30b at a 256k context for
      # 54 GB on 2026-09-15, and it unloads on its own keep_alive anyway.
      echo "ollama model server (unload now: ollama stop <model>; it also unloads when idle)" ;;
    */private/tmp/claude-501/*)
      p=${2#*/private/tmp/claude-501/}; p=${p%%/*}
      echo "lane scratch (${p})" ;;
    *Virtualization.VirtualMachine.xpc*)
      # BEFORE the /System/* arm on purpose, because it lives there and is not
      # what that arm describes. Apple's VM helper is
      # /System/Library/Frameworks/Virtualization.framework/.../XPCServices/
      # com.apple.Virtualization.VirtualMachine.xpc, so the fallback below calls
      # it SIP-protected and reboot-only. It is neither: it runs as the invoking
      # user and is a guest VM that some userspace tool started, so it stops
      # from userspace. Measured 2026-09-19: pid 9701, user ethan, 31 GB
      # resident, reported as "reboot only" while `colima stop` would have
      # freed it. Telling the owner to reboot a 24/7 box for something a
      # command can stop is the expensive half of this mistake.
      #
      # The owning tool is deliberately NOT named here, because it is not
      # derivable at this point: the argv is the framework helper's own path
      # and carries no VM or lane name, and the process is reparented to
      # launchd (ppid 1), so neither the command nor the process tree says who
      # started it. Naming a guess would send the owner to the wrong lane.
      echo "user-owned guest VM (stop from userspace, no reboot: colima stop / limactl stop / quit Docker Desktop)" ;;
    /System/*|/usr/libexec/*|/usr/sbin/*)
      echo "macOS daemon (SIP-protected, reboot only)" ;;
    *)
      if [ "$1" = "root" ]; then echo "root process"; else echo "user process"; fi ;;
  esac
}

# Thin only when free disk is under the floor. Same unmeasured guard as the
# purge decision: df failing must not read as "no space left" and thin on every
# tick of a machine whose probe is broken.
should_thin() { # <free_disk_gb> <floor_gb>
  awk -v f="$1" -v t="$2" 'BEGIN{ exit !(f+0 >= 0 && f+0 < t+0) }'
}

# `ps` elapsed time ("40:12:33", "2-03:04:05", "07:29") into seconds.
etime_secs() { # <etime>
  awk -v v="$1" 'BEGIN{
    d=0; if (index(v,"-")) { split(v,a,"-"); d=a[1]+0; v=a[2] }
    n=split(v,t,":");
    if (n==3) s=t[1]*3600+t[2]*60+t[3]; else if (n==2) s=t[1]*60+t[2]; else s=t[1]+0;
    printf "%d", d*86400+s
  }'
}

# Sum resident memory by PARENT pid over `ps -Ao pid=,ppid=,rss=,etime=` on
# stdin, printing "<ppid> <total_kb> <children> <oldest_secs>" for the largest
# family. Parents 0 and 1 are excluded: everything on the machine descends from
# launchd, so including them would always report one enormous family and name
# nobody (the instrument would be unable to express the failure again).
top_family() {
  # ONE awk over every row, including the elapsed-time parse. A shell loop
  # calling etime_secs per process forks ~950 awks and takes minutes, which on a
  # 30-minute tick is a cleanup job that becomes its own load problem.
  awk '
    function secs(v,   d,n,t,s) {
      d=0; if (index(v,"-")) { split(v,a,"-"); d=a[1]+0; v=a[2] }
      n=split(v,t,":");
      if (n==3) s=t[1]*3600+t[2]*60+t[3]; else if (n==2) s=t[1]*60+t[2]; else s=t[1]+0;
      return d*86400+s
    }
    { ppid=$2+0; if (ppid<=1) next; age=secs($4); kb[ppid]+=$3+0; n[ppid]++; if (age>old[ppid]) old[ppid]=age }
    END { best=0; for (p in kb) if (kb[p]>kb[best]) best=p;
          if (best) printf "%d %d %d %d", best, kb[best], n[best], old[best] }'
}

family_exceeds() { # <family_kb> <phys_kb> <share_pct>
  awk -v f="$1" -v p="$2" -v s="$3" 'BEGIN{ exit !(p+0 > 0 && f+0 > 0 && (f+0)/(p+0)*100 >= s+0) }'
}

family_too_old() { # <oldest_secs> <ceiling_hours>
  awk -v s="$1" -v h="$2" 'BEGIN{ exit !(h+0 > 0 && s+0 >= (h+0) * 3600) }'
}

# The owner label for a LIVE pid, classified from its FULL command line.
# classify_owner matches on substrings, and the substring that names a process
# can sit past any fixed column: Apple's VM helper is 173 characters and
# "Virtualization.VirtualMachine.xpc" starts after character 80. Both call sites
# used to cut the command to 80 or 70 characters BEFORE classifying, so the case
# added for exactly this process could never match, and the live report went on
# calling a stoppable 31 GB guest VM "SIP-protected, reboot only". Its unit test
# passed the whole path in by hand, which is not the value the caller passed.
# Truncate for DISPLAY if you must; never for classification.
owner_label_for_pid() { # <pid>
  local c u
  c=$(ps -o command= -p "$1" 2>/dev/null)
  u=$(ps -o user= -p "$1" 2>/dev/null | tr -d ' ')
  classify_owner "${u:-?}" "${c:-unknown}"
}

# Whole GB with one decimal from 1 GB up, whole MB below, so a real 78.7 GB disk
# reads naturally and a few-MB test fixture is exact.
fmt_kb() { # <kb>
  awk -v k="$1" 'BEGIN{ if (k >= 1048576) printf "%.1fG", k/1048576; else printf "%dM", k/1024 }'
}

# Names of the lima instances whose hostagent is running, from `ps` command lines
# on stdin. Split from the ps call so the pattern can be tested on a real-shaped
# line: the instance name is the directory in --pidfile .../_lima/<name>/ha.pid.
lima_names_from_ps() {
  sed -n 's|.*limactl hostagent.*/_lima/\([^/ ]*\)/ha\.pid.*|\1|p'
}

lima_running_names() {
  # Seam: the test sets AMUX_CLEANUP_LIMA_RUNNING (newline-separated, may be empty).
  if [ "${AMUX_CLEANUP_LIMA_RUNNING+set}" = set ]; then printf '%s\n' "$AMUX_CLEANUP_LIMA_RUNNING"; return 0; fi
  ps -Ao command= 2>/dev/null | lima_names_from_ps
}

# Running VMs that nothing in the fleet references. A reference is a session whose
# name or desc mentions the VM, or an AMUX_DOCKER_VM value in any scope file (the
# shared VM is named only there, by design). Until 2026-10-02 the session list
# was fetched only when $AMUX_URL was set; the scheduled shell does not set it,
# so every running VM, the shared one included, read UNREFERENCED. A list that
# cannot be read now says so (ethos rule 4) instead of reading as "nobody".
# Seams: AMUX_CLEANUP_SESSIONS_CMD prints "name desc" lines; AMUX_CLEANUP_SCOPE_ROOT
# is the ~/.amux holding amux.env, env/*.env and sessions/*.env.
vm_reference_report() {
  local vms; vms=$(lima_running_names)
  [ -n "$vms" ] || return 0
  local sessions rc
  if [ -n "${AMUX_CLEANUP_SESSIONS_CMD:-}" ]; then sessions=$(eval "$AMUX_CLEANUP_SESSIONS_CMD" 2>/dev/null); rc=$?
  else
    sessions=$(curl -sk --max-time 10 "$(amux url 2>/dev/null || echo https://localhost:8824)/api/sessions" 2>/dev/null \
      | python3 -c "import json,sys;[print(s.get('name',''),s.get('desc','')) for s in json.load(sys.stdin)]" 2>/dev/null); rc=$?
  fi
  if [ "$rc" -ne 0 ] || [ -z "$sessions" ]; then
    echo "mac-cleanup: VM references: unmeasured (session list unreadable), $(printf '%s\n' "$vms" | grep -c .) running VM(s) not judged"
    return 0
  fi
  local root="${AMUX_CLEANUP_SCOPE_ROOT:-${AMUX_HOME:-$HOME/.amux}}" shared
  shared=$(cat "$root/amux.env" "$root"/env/*.env "$root"/sessions/*.env 2>/dev/null \
    | sed -n 's/^[[:space:]]*\(export[[:space:]]*\)\{0,1\}AMUX_DOCKER_VM=["'"'"']\{0,1\}\([A-Za-z0-9._-]*\).*/\2/p' | sort -u)
  local vz_gb
  vz_gb=$(ps -eo rss=,command= 2>/dev/null | grep 'Virtualization.VirtualMachine' | grep -v grep | awk '{s+=$1}END{printf "%.1f", s/1048576}')
  local vm short n_un=0 n_ref=0
  while IFS= read -r vm; do
    [ -n "$vm" ] || continue
    short=${vm#colima-}
    if printf '%s\n' "$shared" | grep -qxF "$short" || printf '%s\n' "$sessions" | grep -qiF "$short"; then
      n_ref=$((n_ref+1)); continue
    fi
    n_un=$((n_un+1))
    echo "mac-cleanup: UNREFERENCED VM '$vm' running, no fleet session mentions '$short' and no scope sets AMUX_DOCKER_VM=$short — stop with: limactl stop $vm"
  done <<< "$vms"
  echo "mac-cleanup: VM references: $n_ref referenced, $n_un unreferenced (all VMs together ${vz_gb:-0}G resident)"
}

# Report the lima data disks: allocated size (ls -lsk, instant, no du), whether a
# VM is registered for each, whether it is running. REPORT ONLY. These are other
# lanes' VM data, so this never deletes: deletion is the owner's call (ethos rule 8).
# A disk under _disks/ with no ~/.colima/_lima/<name> instance directory cannot be
# booted by any VM unless a profile of that exact name is recreated, so it reads
# ORPHANED. A missing root says so instead of reading as clean.
lima_disks_report() { # <lima_root> [show_kb]
  local root=$1 show=${2:-10485760} d name kb reg state age running
  local n=0 total=0 orph=0 runn=0 detail=""
  if [ ! -d "$root" ]; then
    echo "mac-cleanup: lima disks: no lima root at $root (not present)"; return 0
  fi
  running=$(lima_running_names)
  for d in "$root"/_disks/*/datadisk; do
    [ -e "$d" ] || continue
    name=$(basename "$(dirname "$d")")
    kb=$(ls -lsk "$d" 2>/dev/null | awk '{print $1+0}')
    kb=${kb:-0}
    n=$((n+1)); total=$((total+kb))
    if printf '%s\n' "$running" | grep -qx -- "$name"; then state=running; runn=$((runn+kb))
    elif [ -d "$root/$name" ]; then state=stopped
    else state=ORPHANED; orph=$((orph+kb)); fi
    if [ "$state" != stopped ] || [ "$kb" -ge "$show" ]; then
      # GNU first: GNU `stat -f` means --file-system, so `stat -f %m` prints a
      # multi-line "File: ..." block on Linux instead of failing, and the
      # arithmetic below dies on "File: unbound variable". BSD refuses -c.
      age=$(( ( $(date +%s) - $(stat -c %Y "$d" 2>/dev/null || stat -f %m "$d" 2>/dev/null || echo 0) ) / 86400 ))
      detail="${detail}mac-cleanup:   $(fmt_kb "$kb") ${name} ${state} (last written ${age}d ago)"$'\n'
    fi
  done
  if [ "$n" = 0 ]; then echo "mac-cleanup: lima disks: none under $root/_disks"; return 0; fi
  echo "mac-cleanup: lima disks: ${n} ($(fmt_kb "$total") allocated, $(fmt_kb "$orph") ORPHANED with no registered VM, $(fmt_kb "$runn") in running VMs)"
  printf '%s' "$detail"
}

needs_reboot() { # <fseventsd_gb> <threshold>
  awk -v f="$1" -v t="$2" 'BEGIN{ exit !(f+0 >= t+0) }'
}

# Footprint strings from `top` ("1296M", "27G", "512K") into GB.
to_gb() { # <top-mem-string>
  awk -v v="$1" 'BEGIN{
    u=substr(v,length(v)); n=v+0;
    if (u=="G") printf "%.2f", n;
    else if (u=="M") printf "%.2f", n/1024;
    else if (u=="K") printf "%.4f", n/1048576;
    else printf "%.2f", n/1073741824;
  }'
}

# Library mode: the test sources this file for the functions above and must not
# trip a single probe or action doing it.
# ── idle cargo target dirs (DESKT-51) ────────────────────────────────────────
# The cache-directory tagging signature (bford.info/cachedir). CACHEDIR.TAG marks
# ANY cache, not just cargo's: ruff, pytest and uv virtualenvs all write it, and a
# .venv is not disposable. So the tag alone never qualifies a directory; it must
# also carry .rustc_info.json, which only a cargo target root has.
CARGO_TAG_SIG=8a477f597d28d172789f06886806bc55

# Print every cargo target root under the colon-separated roots. Runs in the
# CALLER'S shell (redirect its stdout, do not $(...) it) because it sets
# TARGET_SCAN_COMPLETE and TARGET_ROOTS_SCANNED, and a caller that reads only the
# list cannot tell a finished scan from one cut off by the budget (ethos rule 4).
# Pruned names never contain a target root and are where the file count lives:
# node_modules, .git, virtualenvs, and cargo's own build internals.
find_cargo_targets() { # <colon-roots> <maxdepth> <budget_s>
  local roots=$1 depth=$2 budget=$3 deadline left rc tag d r tmp rdepth
  local -a rootarr
  TARGET_SCAN_COMPLETE=yes
  TARGET_ROOTS_SCANNED=0
  deadline=$(( $(date +%s) + budget ))
  IFS=':' read -r -a rootarr <<< "$roots"
  # ${arr[@]+"${arr[@]}"}: bash 3.2 (macOS /bin/bash) treats an EMPTY array as unset under
  # `set -u`, so a bare "${arr[@]}" aborts the tick when the list is empty.
  for r in ${rootarr[@]+"${rootarr[@]}"}; do
    rdepth=$depth
    case "$r" in *@[0-9]*) rdepth=${r##*@}; r=${r%@*} ;; esac
    [ -n "$r" ] && [ -d "$r" ] || continue
    left=$(( deadline - $(date +%s) ))
    if [ "$left" -le 0 ]; then TARGET_SCAN_COMPLETE=no; continue; fi
    TARGET_ROOTS_SCANNED=$((TARGET_ROOTS_SCANNED+1))
    tmp=$(mktemp "${TMPDIR:-/tmp}/cargo-scan.XXXXXX")
    # `|| rc=$?`, never `cmd; rc=$?`: the tests source this under `set -e`, where a
    # non-zero find (an unreadable directory) would end the shell before rc was read.
    # The braces are for the shell's own "Alarm clock: 14" job message, which it prints
    # to ITS stderr when the alarm kills find and would otherwise land in the tick output.
    rc=0
    { perl -e 'alarm shift; exec @ARGV' "$left" find "$r" -maxdepth "$rdepth" \
      \( -name node_modules -o -name .git -o -name .venv -o -name venv -o -name deps \
         -o -name incremental -o -name build -o -name .fingerprint \) -prune \
      -o -name CACHEDIR.TAG -type f -print > "$tmp" 2>/dev/null; } 2>/dev/null || rc=$?
    # 128+SIGALRM: what was found before the alarm is still in the file
    if [ "$rc" = 142 ]; then TARGET_SCAN_COMPLETE=no; fi
    while IFS= read -r tag; do
      d=$(dirname "$tag")
      grep -q -- "$CARGO_TAG_SIG" "$tag" 2>/dev/null || continue
      [ -f "$d/.rustc_info.json" ] || continue
      printf '%s\n' "$d"
    done < "$tmp"
    rm -f -- "${tmp:?}"
  done
}

# Print "<count> <dir>" for the busiest directories (three levels under a root)
# with at least <min_files> files modified in the last <minutes>, busiest first, at
# most three. Call it in the caller's shell with stdout redirected, never $(...):
# it sets CHURN_COMPLETE=no when the budget cut a root short, so a short
# list is not read as a quiet disk.
churn_census() { # <colon-roots> <minutes> <budget_s> <min_files>
  local roots=$1 mins=$2 budget=$3 minf=$4 deadline left r rc out
  local -a rootarr
  CHURN_COMPLETE=yes
  deadline=$(( $(date +%s) + budget ))
  out=$(mktemp "${TMPDIR:-/tmp}/churn.XXXXXX")
  IFS=':' read -r -a rootarr <<< "$roots"
  for r in ${rootarr[@]+"${rootarr[@]}"}; do
    [ -n "$r" ] && [ -d "$r" ] || continue
    left=$(( deadline - $(date +%s) ))
    if [ "$left" -le 0 ]; then CHURN_COMPLETE=no; continue; fi
    rc=0
    { perl -e 'alarm shift; exec @ARGV' "$left" find "$r" -xdev \( -name .git -o -name node_modules \) -prune -o -type f -mmin "-$mins" -print 2>/dev/null; } 2>/dev/null \
      | awk -v r="${r%/}" '{ p=substr($0, length(r)+2); n=split(p, a, "/"); k=r; for (i=1; i<=3 && i<n; i++) k=k "/" a[i]; c[k]++ } END { for (k in c) print c[k], k }' >> "$out" || rc=$?
    if [ "$rc" = 142 ]; then CHURN_COMPLETE=no; fi
  done
  awk -v m="$minf" '$1 >= m' "$out" | sort -rn | head -3
  rm -f -- "${out:?}"
}

# 0 = nothing inside was written in the last <hours>; 1 = something was; 2 = the
# walk did not finish inside its budget, so idleness is UNKNOWN and the caller must
# keep the directory. `find -print -quit` stops at the first recent file, so an
# active target costs almost nothing and only a truly idle one is walked in full.
dir_idle() { # <dir> <hours> <walk_budget_s>
  local d=$1 mins=$(( $2 * 60 )) budget=$3 out rc=0
  out=$(perl -e 'alarm shift; exec @ARGV' "$budget" find "$d" -type f -mmin "-$mins" -print -quit 2>/dev/null) || rc=$?
  if [ "$rc" = 142 ]; then return 2; fi
  [ -z "$out" ]
}

# Does any row of an lsof snapshot name this directory or something inside it?
# The boundary matters: a handle on .../target-mr283/x must NOT protect
# .../target, and a handle on .../target/x must. Matching on the bare prefix gets
# the first wrong; matching the whole path column gets both right.
has_open_handle() { # <dir> <lsof_snapshot_file>
  local pat
  pat=$(printf '%s' "$1" | sed 's/[][\.*^$/+?(){}|]/\\&/g')
  grep -Eq -- "(^|[[:space:]])${pat}(/|[[:space:]]|\$)" "$2"
}

# Is <dir> equal to, or inside, a protected path from the colon-separated list?
is_kept_target() { # <dir> <colon-list>
  local d=$1 k
  local -a keep
  IFS=':' read -r -a keep <<< "$2"
  for k in ${keep[@]+"${keep[@]}"}; do
    [ -n "$k" ] || continue
    k=${k%/}
    case "$d" in "$k"|"$k"/*) return 0 ;; esac
  done
  return 1
}

# Reap idle cargo targets. Sets TARGETS_FOUND / _ELIGIBLE / _REAPED / _REAPED_KB.
# WHY EVERY GUARD FAILS CLOSED: this deletes tens of GB in other lanes' trees, so
# "could not tell" has to mean "kept". lsof unavailable, a walk cut off by its
# budget, or a scan cut off by its budget each keep the directory and say so in the
# line, because a summary that read the same for "nothing idle" and "could not
# look" would be the defect this file already has a rule against.
reap_idle_cargo_targets() { # <roots> <idle_hours> <dry:0|1>
  local roots=$1 idle_h=$2 dry=${3:-0}
  local cands eligible lsofsnap t0 d kb rc n_active=0 n_open=0 n_unk=0 n_shared=0 n_over=0 n_fail=0 n_elig_kb=0 handles=measured
  TARGETS_FOUND=0; TARGETS_ELIGIBLE=0; TARGETS_REAPED=0; TARGETS_REAPED_KB=0
  t0=$(date +%s)
  cands=$(mktemp "${TMPDIR:-/tmp}/cargo-cands.XXXXXX"); eligible=$(mktemp "${TMPDIR:-/tmp}/cargo-elig.XXXXXX"); lsofsnap=$(mktemp "${TMPDIR:-/tmp}/cargo-lsof.XXXXXX")
  find_cargo_targets "$roots" "$TARGET_DEPTH" "$TARGET_SCAN_S" > "$cands"
  # Overlapping roots list a target twice. `find` on a path that is already gone prints nothing,
  # which dir_idle reads as IDLE, so a duplicate would be "reaped" a second time at size zero.
  sort -u "$cands" -o "$cands"
  TARGETS_FOUND=$(grep -c . "$cands" || true)
  # One snapshot for selection. Empty or failed means we cannot see open files at
  # all, and an empty snapshot would read as "nothing is open" for every directory.
  if $LSOF_CMD > "$lsofsnap" 2>/dev/null && [ -s "$lsofsnap" ]; then :; else handles=UNMEASURED; fi
  if [ "$handles" = UNMEASURED ]; then
    echo "mac-cleanup: cargo targets: found ${TARGETS_FOUND} under ${TARGET_ROOTS_SCANNED} root(s), scan complete=${TARGET_SCAN_COMPLETE}, open handles UNMEASURED (lsof produced nothing): reaped 0, nothing is deleted without it"
    rm -f -- "${cands:?}" "${eligible:?}" "${lsofsnap:?}"; return 0
  fi
  while IFS= read -r d; do
    [ -d "$d" ] || continue     # gone since the scan (a lane removed it, or an overlapping root already did)
    if is_kept_target "$d" "$TARGET_KEEP"; then n_shared=$((n_shared+1)); continue; fi
    # The walk phase needs its own guard: a hundred candidates at the per-walk budget each
    # would outlast the tick's schedule even though every single walk is bounded.
    if [ $(( $(date +%s) - t0 )) -ge "$TARGET_BUDGET_S" ]; then n_over=$((n_over+1)); continue; fi
    rc=0; dir_idle "$d" "$idle_h" "$TARGET_WALK_S" || rc=$?
    if [ "$rc" = 1 ]; then n_active=$((n_active+1)); continue; fi
    if [ "$rc" = 2 ]; then n_unk=$((n_unk+1)); continue; fi
    if has_open_handle "$d" "$lsofsnap"; then n_open=$((n_open+1)); continue; fi
    kb=$(du -sk "$d" 2>/dev/null | awk '{print $1+0}')
    printf '%s\t%s\n' "${kb:-0}" "$d" >> "$eligible"
    TARGETS_ELIGIBLE=$((TARGETS_ELIGIBLE+1)); n_elig_kb=$((n_elig_kb+${kb:-0}))
  done < "$cands"
  # Biggest first, so a spent budget has already taken the wins that matter.
  sort -rn "$eligible" -o "$eligible"
  while IFS=$'\t' read -r kb d; do
    if [ $(( $(date +%s) - t0 )) -ge "$TARGET_BUDGET_S" ]; then n_over=$((n_over+1)); continue; fi
    if [ "$dry" = 1 ]; then
      echo "mac-cleanup:   would reap $(fmt_kb "$kb") $d (idle >= ${idle_h}h, dry run)"
      continue
    fi
    # A second look right before the delete: a build may have started since the
    # selection snapshot, and it will hold the directory open.
    $LSOF_CMD > "$lsofsnap" 2>/dev/null || true
    if [ ! -s "$lsofsnap" ] || has_open_handle "$d" "$lsofsnap"; then n_open=$((n_open+1)); continue; fi
    rm -rf -- "${d:?}"
    if [ -e "$d" ]; then
      n_fail=$((n_fail+1)); echo "mac-cleanup:   FAILED to remove $(fmt_kb "$kb") $d (still present after rm)"
    else
      TARGETS_REAPED=$((TARGETS_REAPED+1)); TARGETS_REAPED_KB=$((TARGETS_REAPED_KB+kb))
      echo "mac-cleanup:   reaped $(fmt_kb "$kb") $d (idle >= ${idle_h}h)"
    fi
  done < "$eligible"
  echo "mac-cleanup: cargo targets: found ${TARGETS_FOUND} under ${TARGET_ROOTS_SCANNED} root(s), scan complete=${TARGET_SCAN_COMPLETE}, eligible ${TARGETS_ELIGIBLE} ($(fmt_kb "$n_elig_kb") idle >= ${idle_h}h), reaped ${TARGETS_REAPED} ($(fmt_kb "$TARGETS_REAPED_KB")), kept: active ${n_active}, open ${n_open}, unmeasured ${n_unk}, shared ${n_shared}, over budget ${n_over}, failed ${n_fail}"
  rm -f -- "${cands:?}" "${eligible:?}" "${lsofsnap:?}"
}

# Find candidate detached worktree scratch checkouts (MO-3631/3633): a directory
# whose top level IS `.git`-as-a-FILE (not a directory). `git worktree add`
# writes a one-line `gitdir: <path>` file there; a real clone's `.git` is a
# directory. That distinction is the whole safety boundary: this function can
# never select a real repository, only something `git worktree` itself created,
# because nothing else on this box writes a `.git` file at a directory's root.
find_detached_worktrees() { # <colon-roots> <maxdepth> <budget_s>
  local roots=$1 depth=$2 budget=$3 deadline left rc tag d r rdepth
  local -a rootarr
  WORKTREE_SCAN_COMPLETE=yes
  WORKTREE_ROOTS_SCANNED=0
  deadline=$(( $(date +%s) + budget ))
  IFS=':' read -r -a rootarr <<< "$roots"
  for r in ${rootarr[@]+"${rootarr[@]}"}; do
    rdepth=$depth
    case "$r" in *@[0-9]*) rdepth=${r##*@}; r=${r%@*} ;; esac
    [ -n "$r" ] && [ -d "$r" ] || continue
    # Never a claude-<uid> session scratchpad, however a caller's root list is
    # built or wherever TMPDIR actually resolves to: those are live Claude
    # session directories (DESKT-58) and no config knob may widen this arm onto
    # them. Matched as a path COMPONENT, not a literal /private/tmp/ prefix, so
    # a symlinked or remapped temp dir cannot slip past a prefix check that
    # assumed one specific mount.
    case "/$r/" in */claude-[0-9]*/*) continue ;; esac
    left=$(( deadline - $(date +%s) ))
    if [ "$left" -le 0 ]; then WORKTREE_SCAN_COMPLETE=no; continue; fi
    WORKTREE_ROOTS_SCANNED=$((WORKTREE_ROOTS_SCANNED+1))
    rc=0
    { perl -e 'alarm shift; exec @ARGV' "$left" find "$r" -maxdepth "$rdepth" \
      -name .git -type f -print 2>/dev/null; } 2>/dev/null | while IFS= read -r tag; do
        dirname "$tag"
      done || rc=$?
    if [ "$rc" = 142 ]; then WORKTREE_SCAN_COMPLETE=no; fi
  done
}

# Reap idle detached worktree scratch checkouts. Sets WORKTREES_FOUND / _ELIGIBLE
# / _REAPED. Fails closed the same way reap_idle_cargo_targets does: lsof
# unavailable, a merge-base check that cannot run, or a dirty tree each KEEP the
# worktree, because "could not tell" must never look like "nothing to reap".
#
# Safety, in the order actually checked (any failure keeps it):
#  1. still a worktree git recognizes (`git worktree list` on its own admin
#     file), so a half-removed one from a race is left for its own owner.
#  2. fully clean (`git status --porcelain --untracked-files=all` empty) --
#     no partial-credit for "only trivial-looking diffs": that judgment call is
#     exactly what a human did by hand three times today, and it does not
#     belong in unattended code. A worktree with ANY uncommitted content is
#     kept, full stop.
#  3. HEAD reachable from some remote-tracking ref of the SAME repo it belongs
#     to (`git for-each-ref refs/remotes --contains`) -- not just origin/main,
#     because a worktree can point at a fork remote or a differently-named
#     default branch. No unique commit is ever at risk: removing a worktree
#     never deletes the branch or the commit, only the checked-out copy, but
#     this still refuses to guess about a commit that exists NOWHERE else.
#  4. idle >= WORKTREE_IDLE_H (dir_idle, the same helper and the same
#     "unknown means keep" contract the cargo-target arm already relies on).
#  5. no open file handle (same lsof snapshot convention).
# Removal is `git worktree remove`, never `rm -rf`: git itself re-checks
# cleanliness at removal time and refuses on anything this scan raced past.
reap_idle_worktrees() { # <roots> <idle_hours> <dry:0|1>
  local roots=$1 idle_h=$2 dry=${3:-0}
  local cands eligible lsofsnap t0 d rc n_notreal=0 n_dirty=0 n_unmerged=0 n_active=0 n_open=0 n_unk=0 n_over=0 n_fail=0 handles=measured
  local gitdir toplevel head remote_hit real_d wt_line wt_path real_wt found_self
  WORKTREES_FOUND=0; WORKTREES_ELIGIBLE=0; WORKTREES_REAPED=0
  t0=$(date +%s)
  cands=$(mktemp "${TMPDIR:-/tmp}/wt-cands.XXXXXX"); eligible=$(mktemp "${TMPDIR:-/tmp}/wt-elig.XXXXXX"); lsofsnap=$(mktemp "${TMPDIR:-/tmp}/wt-lsof.XXXXXX")
  find_detached_worktrees "$roots" "$WORKTREE_DEPTH" "$WORKTREE_SCAN_S" > "$cands"
  sort -u "$cands" -o "$cands"
  WORKTREES_FOUND=$(grep -c . "$cands" || true)
  if $LSOF_CMD > "$lsofsnap" 2>/dev/null && [ -s "$lsofsnap" ]; then :; else handles=UNMEASURED; fi
  if [ "$handles" = UNMEASURED ]; then
    echo "mac-cleanup: scratch worktrees: found ${WORKTREES_FOUND} under ${WORKTREE_ROOTS_SCANNED} root(s), scan complete=${WORKTREE_SCAN_COMPLETE}, open handles UNMEASURED (lsof produced nothing): reaped 0, nothing is deleted without it"
    rm -f -- "${cands:?}" "${eligible:?}" "${lsofsnap:?}"; return 0
  fi
  while IFS= read -r d; do
    [ -d "$d" ] || continue
    if [ $(( $(date +%s) - t0 )) -ge "$WORKTREE_BUDGET_S" ]; then n_over=$((n_over+1)); continue; fi
    gitdir=$(sed -n 's/^gitdir: //p' "$d/.git" 2>/dev/null)
    [ -n "$gitdir" ] && [ -d "$gitdir" ] || { n_notreal=$((n_notreal+1)); continue; }
    # The worktree's own admin dir names the real repo toplevel two levels up
    # (.../.git/worktrees/<name>). Resolving it there, not by trusting $d, means
    # a worktree pointed at by a stale or hand-edited .git file cannot walk this
    # into removing something it does not actually belong to.
    toplevel=$(git -C "$gitdir" rev-parse --path-format=absolute --git-common-dir 2>/dev/null)
    [ -n "$toplevel" ] || { n_notreal=$((n_notreal+1)); continue; }
    toplevel=$(dirname "$toplevel")
    # NOT a literal string match against `git worktree list`'s own path: macOS
    # resolves /tmp to /private/tmp (and /var/folders/... has similar aliasing),
    # so `find`'s root-as-given and git's OWN recorded, already-resolved path
    # can name the identical directory in two different spellings. Canonicalize
    # both sides with `cd && pwd -P` before comparing, or a perfectly valid
    # worktree is kept forever as "not-a-real-worktree" on every single tick.
    real_d=$(cd "$d" 2>/dev/null && pwd -P) || { n_notreal=$((n_notreal+1)); continue; }
    found_self=no
    while IFS= read -r wt_line; do
      case "$wt_line" in worktree\ *) wt_path=${wt_line#worktree } ;; *) continue ;; esac
      real_wt=$(cd "$wt_path" 2>/dev/null && pwd -P) || continue
      if [ "$real_wt" = "$real_d" ]; then found_self=yes; break; fi
    done < <(git -C "$toplevel" worktree list --porcelain 2>/dev/null)
    [ "$found_self" = yes ] || { n_notreal=$((n_notreal+1)); continue; }
    if [ -n "$(git -C "$d" status --porcelain --untracked-files=all 2>/dev/null)" ]; then n_dirty=$((n_dirty+1)); continue; fi
    head=$(git -C "$d" rev-parse HEAD 2>/dev/null)
    [ -n "$head" ] || { n_dirty=$((n_dirty+1)); continue; }
    remote_hit=$(git -C "$toplevel" for-each-ref refs/remotes --contains "$head" 2>/dev/null)
    [ -n "$remote_hit" ] || { n_unmerged=$((n_unmerged+1)); continue; }
    rc=0; dir_idle "$d" "$idle_h" "$WORKTREE_WALK_S" || rc=$?
    if [ "$rc" = 1 ]; then n_active=$((n_active+1)); continue; fi
    if [ "$rc" = 2 ]; then n_unk=$((n_unk+1)); continue; fi
    if has_open_handle "$d" "$lsofsnap"; then n_open=$((n_open+1)); continue; fi
    printf '%s\t%s\n' "$toplevel" "$d" >> "$eligible"
    WORKTREES_ELIGIBLE=$((WORKTREES_ELIGIBLE+1))
  done < "$cands"
  while IFS=$'\t' read -r toplevel d; do
    if [ $(( $(date +%s) - t0 )) -ge "$WORKTREE_BUDGET_S" ]; then n_over=$((n_over+1)); continue; fi
    if [ "$dry" = 1 ]; then
      echo "mac-cleanup:   would reap worktree $d (idle >= ${idle_h}h, merged, clean, dry run)"
      continue
    fi
    $LSOF_CMD > "$lsofsnap" 2>/dev/null || true
    if [ ! -s "$lsofsnap" ] || has_open_handle "$d" "$lsofsnap"; then n_open=$((n_open+1)); continue; fi
    if git -C "$toplevel" worktree remove "$d" >/dev/null 2>&1; then
      WORKTREES_REAPED=$((WORKTREES_REAPED+1))
      echo "mac-cleanup:   reaped worktree $d (idle >= ${idle_h}h, merged, clean)"
    else
      n_fail=$((n_fail+1)); echo "mac-cleanup:   FAILED to remove worktree $d (git worktree remove refused -- left in place)"
    fi
  done < "$eligible"
  echo "mac-cleanup: scratch worktrees: found ${WORKTREES_FOUND} under ${WORKTREE_ROOTS_SCANNED} root(s), scan complete=${WORKTREE_SCAN_COMPLETE}, eligible ${WORKTREES_ELIGIBLE}, reaped ${WORKTREES_REAPED}, kept: not-a-real-worktree ${n_notreal}, dirty ${n_dirty}, unmerged ${n_unmerged}, active ${n_active}, open ${n_open}, unmeasured ${n_unk}, over budget ${n_over}, failed ${n_fail}"
  rm -f -- "${cands:?}" "${eligible:?}" "${lsofsnap:?}"
}

# ── Claude Code session temp dirs (DESKT-77) ────────────────────────────────
# 0 = some file under one of the paths was written in the last <hours>; 1 = none
# was (or none exists); 2 = the walk ran out of budget, so the caller must keep.
any_recent() { # <hours> <budget_s> <path>...
  local mins=$(( $1 * 60 )) budget=$2 out rc=0; shift 2
  local -a ex=(); local q
  for q in "$@"; do [ -e "$q" ] && ex+=("$q"); done
  [ ${#ex[@]} -gt 0 ] || return 1
  out=$(perl -e 'alarm shift; exec @ARGV' "$budget" find "${ex[@]}" -mmin "-$mins" -print -quit 2>/dev/null) || rc=$?
  if [ "$rc" = 142 ]; then return 2; fi
  [ -n "$out" ]
}

# Is there a git worktree (a .git FILE) within three levels of <dir> with uncommitted
# or untracked content? A dirty worktree is the one thing in a scratchpad most
# likely to be somebody's only copy, so it keeps the whole session dir.
has_dirty_worktree() { # <dir>
  local g w
  while IFS= read -r g; do
    w=$(dirname "$g")
    if [ -n "$(git -C "$w" status --porcelain --untracked-files=normal 2>/dev/null | head -1)" ]; then return 0; fi
  done < <(find "$1" -maxdepth 3 -name .git -type f 2>/dev/null)
  return 1
}

du_kb() { # <path> <budget_s>; prints KB, or nothing when the walk ran out of time
  perl -e 'alarm shift; exec @ARGV' "$2" du -sk "$1" 2>/dev/null | awk '{print $1+0}'
}

claude_tmp_ledger() { # <reason> <kb> <path>
  mkdir -p "$STATE_DIR" 2>/dev/null
  printf '%s\t%s\t%s\t%s\n' "$(date '+%F %T')" "$1" "${2:-?}" "$3" >> "$STATE_DIR/claude-tmp-reaps.log"
}

# Sets CLAUDE_TMP_REAPED / CLAUDE_TMP_REAPED_KB. Fails closed like the arms above:
# no lsof snapshot means nothing is removed, and a walk that runs out of time keeps.
reap_claude_tmp() { # <root> <idle_hours> <dry:0|1>
  local root=$1 idle_h=$2 dry=${3:-0}
  local t0 lsofsnap live sess proj uuid rc kb q n_sess=0 n_live=0 n_open=0 n_dirty=0 n_unk=0 n_over=0 n_fail=0 n_quota=0 n_qkept=0 handles=measured
  local quota_kb=${CLAUDE_TMP_QUOTA_KB:-$(( CLAUDE_TMP_QUOTA_GB * 1048576 ))}   # KB override: the suite's fixtures are megabytes
  CLAUDE_TMP_REAPED=0; CLAUDE_TMP_REAPED_KB=0
  if [ ! -d "$root" ]; then echo "mac-cleanup: claude tmp: no root at $root (not present)"; return 0; fi
  t0=$(date +%s)
  lsofsnap=$(mktemp "${TMPDIR:-/tmp}/ctmp-lsof.XXXXXX"); live=$(mktemp "${TMPDIR:-/tmp}/ctmp-live.XXXXXX")
  if $LSOF_CMD > "$lsofsnap" 2>/dev/null && [ -s "$lsofsnap" ]; then :; else handles=UNMEASURED; fi
  if [ "$handles" = UNMEASURED ]; then
    echo "mac-cleanup: claude tmp: open handles UNMEASURED (lsof produced nothing): reaped 0, nothing is deleted without it"
    rm -f -- "${lsofsnap:?}" "${live:?}"; return 0
  fi
  for sess in "$root"/*/*/; do
    sess=${sess%/}
    [ -d "$sess" ] || continue
    n_sess=$((n_sess+1))
    uuid=${sess##*/}; proj=${sess%/*}; proj=${proj##*/}
    if [ $(( $(date +%s) - t0 )) -ge "$CLAUDE_TMP_BUDGET_S" ]; then n_over=$((n_over+1)); continue; fi
    if [ "$idle_h" -le 0 ]; then printf '%s\n' "$sess" >> "$live"; n_live=$((n_live+1)); continue; fi
    rc=0; any_recent "$idle_h" "$CLAUDE_TMP_WALK_S" "$CLAUDE_PROJECTS/$proj/$uuid.jsonl" "$CLAUDE_PROJECTS/$proj/$uuid" "$sess" || rc=$?
    if [ "$rc" = 0 ]; then printf '%s\n' "$sess" >> "$live"; n_live=$((n_live+1)); continue; fi
    if [ "$rc" = 2 ]; then n_unk=$((n_unk+1)); continue; fi
    if has_open_handle "$sess" "$lsofsnap"; then n_open=$((n_open+1)); printf '%s\n' "$sess" >> "$live"; continue; fi
    if has_dirty_worktree "$sess"; then n_dirty=$((n_dirty+1)); continue; fi
    kb=$(du_kb "$sess" "$CLAUDE_TMP_WALK_S")
    if [ "$dry" = 1 ]; then echo "mac-cleanup:   would remove $(fmt_kb "${kb:-0}") $sess (session idle >= ${idle_h}h, dry run)"; continue; fi
    rm -rf -- "${sess:?}"
    if [ -e "$sess" ]; then n_fail=$((n_fail+1)); echo "mac-cleanup:   FAILED to remove $sess (still present after rm)"; continue; fi
    CLAUDE_TMP_REAPED=$((CLAUDE_TMP_REAPED+1)); CLAUDE_TMP_REAPED_KB=$((CLAUDE_TMP_REAPED_KB+${kb:-0}))
    claude_tmp_ledger "session idle >= ${idle_h}h" "$kb" "$sess"
    echo "mac-cleanup:   removed $(fmt_kb "${kb:-0}") $sess (session idle >= ${idle_h}h)"
  done
  # Rule 2: the quota on live sessions, biggest idle entries first.
  if [ "$quota_kb" -gt 0 ]; then
    while IFS= read -r sess; do
      [ -d "$sess" ] || continue
      if [ $(( $(date +%s) - t0 )) -ge "$CLAUDE_TMP_BUDGET_S" ]; then n_over=$((n_over+1)); continue; fi
      # A session too big to sum inside the walk budget is measured by its entries
      # instead: skipping it would exempt exactly the sessions the quota is for
      # (gs12-cicd's 45.8G did not finish a 15s walk on 2026-10-02).
      local total ents ekb e sum=0
      kb=$(du_kb "$sess" "$CLAUDE_TMP_WALK_S")
      if [ -n "$kb" ] && [ "$kb" -le "$quota_kb" ]; then continue; fi
      ents=$(mktemp "${TMPDIR:-/tmp}/ctmp-ents.XXXXXX")
      for e in "$sess"/scratchpad/* "$sess"/scratchpad/.[!.]*; do
        [ -e "$e" ] || continue
        if [ $(( $(date +%s) - t0 )) -ge "$CLAUDE_TMP_BUDGET_S" ]; then break; fi
        ekb=$(du_kb "$e" "$CLAUDE_TMP_WALK_S")
        if [ -n "$ekb" ]; then printf '%s\t%s\n' "$ekb" "$e" >> "$ents"; sum=$((sum+ekb)); fi
      done
      total=${kb:-$sum}
      if [ "$total" -le "$quota_kb" ]; then rm -f -- "${ents:?}"; [ -n "$kb" ] || n_unk=$((n_unk+1)); continue; fi
      n_quota=$((n_quota+1))
      sort -rn "$ents" -o "$ents"
      while IFS=$'\t' read -r ekb e; do
        [ "$total" -gt "$quota_kb" ] || break
        if [ "$ekb" -lt $(( CLAUDE_TMP_ENTRY_MIN_MB * 1024 )) ]; then break; fi   # sorted biggest first: everything after is smaller
        if [ -f "$e/.git" ]; then n_qkept=$((n_qkept+1)); continue; fi
        rc=0; any_recent "$CLAUDE_TMP_ENTRY_IDLE_H" "$CLAUDE_TMP_WALK_S" "$e" || rc=$?
        if [ "$rc" != 1 ]; then n_qkept=$((n_qkept+1)); continue; fi
        if has_open_handle "$e" "$lsofsnap" || has_dirty_worktree "$e"; then n_qkept=$((n_qkept+1)); continue; fi
        if [ "$dry" = 1 ]; then echo "mac-cleanup:   would remove $(fmt_kb "$ekb") $e (session over ${CLAUDE_TMP_QUOTA_GB}G quota, entry idle >= ${CLAUDE_TMP_ENTRY_IDLE_H}h, dry run)"; total=$((total-ekb)); continue; fi
        rm -rf -- "${e:?}"
        if [ -e "$e" ]; then n_fail=$((n_fail+1)); echo "mac-cleanup:   FAILED to remove $e (still present after rm)"; continue; fi
        total=$((total-ekb)); CLAUDE_TMP_REAPED=$((CLAUDE_TMP_REAPED+1)); CLAUDE_TMP_REAPED_KB=$((CLAUDE_TMP_REAPED_KB+ekb))
        claude_tmp_ledger "over ${CLAUDE_TMP_QUOTA_GB}G quota, entry idle >= ${CLAUDE_TMP_ENTRY_IDLE_H}h" "$ekb" "$e"
        echo "mac-cleanup:   removed $(fmt_kb "$ekb") $e (session over ${CLAUDE_TMP_QUOTA_GB}G quota, entry idle >= ${CLAUDE_TMP_ENTRY_IDLE_H}h)"
      done < "$ents"
      rm -f -- "${ents:?}"
      if [ "$total" -gt "$quota_kb" ]; then echo "mac-cleanup:   OVER QUOTA $(fmt_kb "$total") $sess (quota ${CLAUDE_TMP_QUOTA_GB}G): the rest is recent, open or a worktree, so it stays"; fi
    done < "$live"
  fi
  echo "mac-cleanup: claude tmp: ${n_sess} session dir(s) under $root, removed ${CLAUDE_TMP_REAPED} ($(fmt_kb "$CLAUDE_TMP_REAPED_KB")), idle >= ${idle_h}h, quota ${CLAUDE_TMP_QUOTA_GB}G; kept: live ${n_live}, open ${n_open}, dirty worktree ${n_dirty}, unmeasured ${n_unk}, over budget ${n_over}, failed ${n_fail}; over quota ${n_quota} (entries kept ${n_qkept})"
  rm -f -- "${lsofsnap:?}" "${live:?}"
}

# ── assessment (DESKT-57) ────────────────────────────────────────────────────
# Burn rate and hours to full from two readings. Prints "<burn_gbh> <hours|->".
# A disk that is not losing space (or lost less than BURN_MIN_GBH) has no ETA,
# printed as "-" rather than a large number that reads as measured.
disk_trend() { # <prev_ts> <prev_free_gb> <now_ts> <now_free_gb> <min_gbh>
  awk -v pt="$1" -v pf="$2" -v nt="$3" -v nf="$4" -v m="$5" 'BEGIN{
    dt=(nt-pt)/3600; if (pt<=0 || dt<=0.01) { print "- -"; exit }
    b=(pf-nf)/dt
    if (b < m) printf "%.1f -\n", b; else printf "%.1f %.1f\n", b, nf/b }'
}

# One line per constrained class, "<class> <reason>". Nothing printed means
# nothing is constrained. -1 inputs mean "not measured" and never trip a class.
# swap_total_mb==0 means macOS has never had to create a swapfile since boot
# (dynamic_pager allocates it lazily, on first real pressure) -- that reads as
# free=0 on a perfectly healthy machine, indistinguishable from free=0 on a
# machine with a large swapfile that is actually full, unless total is also
# checked. MO-3629/MO-3638/MO-3629(again): three escalations in one day,
# swap_free=0.00MB each time, kern.memorystatus_vm_pressure_level=1 (normal)
# and >=90% memory free every time -- a swapfile that was never created is not
# a constraint, so this class only trips when a swapfile actually exists.
# A low swap_free can ALSO be stale rather than never-created: MO-3655/3657
# measured the identical 438.06MB free across several hours while pressure
# stayed 1 (normal) and load/free% both improved -- macOS does not walk swap
# back down once pages are written, so a floor-only check conflates "low
# right now" with "low once, now stale." pr==1 (kernel-confirmed normal) is
# trusted over a stale swap_free UNLESS swap itself is critically low as a
# fraction of its own total, which still trips even if pressure hasn't
# caught up to a genuinely fast-moving emergency.
classify_constraints() { # <disk_free_gb> <burn> <hours_to_full> <pressure> <swap_free_mb> <load15> <ncpu> <family_exceeds:0|1> <swap_total_mb>
  awk -v df="$1" -v b="$2" -v h="$3" -v pr="$4" -v sw="$5" -v l="$6" -v n="$7" -v fam="$8" -v swt="$9" \
      -v floor="$DISK_FLOOR_GB" -v htf="$HOURS_TO_FULL" -v swf="$SWAP_FREE_FLOOR_MB" -v cs="$CPU_SHARE" -v swcp="$SWAP_FREE_CRITICAL_PCT" 'BEGIN{
    if (df >= 0 && df < floor) printf "disk free %.1fG is under the %dG floor\n", df, floor
    else if (h != "-" && h+0 < htf) printf "disk burning %.1fG/h, full in %.1fh (under %dh)\n", b, h, htf
    if (pr >= 2) printf "memory kernel pressure %d after the purge arm\n", pr
    else if (swt > 0 && sw >= 0 && sw < swf && (pr != 1 || sw/swt < swcp)) printf "memory swap has %dMB free (under %dMB)\n", sw, swf
    if (l >= 0 && n > 0 && l/n > cs) printf "cpu 15-min load %.1f is %.0f%% of %d cores (over %.0f%%)\n", l, l/n*100, n, cs*100
    if (fam == 1) print "family a process family is over its share of RAM"
  }'
}

# Burn rate from the host-metrics history (5-minute samples), by least squares
# over the window. Two readings 15 minutes apart are noise: round 2 of DESKT-57
# read 65 G/h off exactly that while a peer's delete sat in the window. Prints
# "<burn_gbh> <hours|-> <n> <span_h>", or "- - <n> <span_h>" when there are fewer
# than 6 samples or under an hour of span, so the caller falls back and says so.
# The burn is capped at the window's NET loss (oldest minus newest over the
# span): a dip that has already recovered is not burning. On 2026-10-05 the
# disk fell 46 G at 05:00Z and was back to 269.7 G by 05:30Z, yet the slope
# across that dip read 12.1 G/h, "full in 22h" (escalation 20261005-013827).
burn_from_history() { # <history json on stdin> <min_gbh>
  python3 -c '
import json,sys
m=float(sys.argv[1])
try:
    d=json.load(sys.stdin); rows=[(r["ts"],r["disk_free_gb"]) for r in d.get("samples",[]) if r.get("measured") and r.get("disk_free_gb") is not None]
except Exception:
    rows=[]
n=len(rows); span=(max(r[0] for r in rows)-min(r[0] for r in rows))/3600 if n else 0
if n<6 or span<1: print("- - %d %.1f"%(n,span)); sys.exit()
xs=[(t-rows[0][0])/3600 for t,_ in rows]; ys=[f for _,f in rows]
mx=sum(xs)/n; my=sum(ys)/n
slope=sum((x-mx)*(y-my) for x,y in zip(xs,ys))/max(1e-9,sum((x-mx)**2 for x in xs))
o=sorted(zip(xs,ys)); net=(o[0][1]-o[-1][1])/max(1e-9,o[-1][0]-o[0][0])
burn=min(-slope,net); last=ys[-1] if xs[-1]==max(xs) else ys[xs.index(max(xs))]
print(("%.1f -" if burn<m else "%.1f %.1f")%((burn,) if burn<m else (burn,last/burn)), n, "%.1f"%span)
' "$1" 2>/dev/null || echo "- - 0 0.0"
}

# Read a key from the state file; empty if absent.
state_get() { # <file> <key>
  [ -f "$1" ] && sed -n "s/^$2=//p" "$1" | tail -1
}
# Write keys to the state file atomically (rename), keeping the other keys.
state_put() { # <file> <key=value>...
  local f=$1 tmp k; shift
  mkdir -p "$(dirname "$f")"
  tmp=$(mktemp "$f.XXXXXX")
  [ -f "$f" ] && cp "$f" "$tmp"
  for kv in "$@"; do k=${kv%%=*}; grep -v "^$k=" "$tmp" > "$tmp.n" || true; mv "$tmp.n" "$tmp"; printf '%s\n' "$kv" >> "$tmp"; done
  mv "$tmp" "$f"
}
# 0 if <class> may escalate now: never escalated, or the last one is older than the cooldown.
escalation_due() { # <state_file> <class> <now> <cooldown_h>
  local last; last=$(state_get "$1" "esc_$2")
  [ -z "$last" ] && return 0
  [ $(( $3 - last )) -ge $(( $4 * 3600 )) ]
}

# The amux lane a process belongs to: walk its parent chain to a tmux PANE and
# name that pane's session. "user process" says nothing about who can act; a
# lane name does. Two tempting shortcuts are both wrong on this Mac: macOS does
# not expose another process's environment to `ps eww`, so AMUX_SESSION cannot
# be read, and walking past the pane reaches the ONE tmux server whose own
# argv names whichever lane happened to start it (round 2 of DESKT-57 got
# "amux-chat-worker" for every process that way).
# Seam: AMUX_CLEANUP_PANE_MAP ("<pane_pid> <session>" lines) replaces tmux.
pane_map() {
  if [ "${AMUX_CLEANUP_PANE_MAP+set}" = set ]; then printf '%s\n' "$AMUX_CLEANUP_PANE_MAP"; return 0; fi
  tmux list-panes -a -F '#{pane_pid} #{session_name}' 2>/dev/null
  # launchd agents too, so the builder, the server and app helpers are named
  # instead of reading "no lane" (round 2 left 8 of the top 10 CPU unowned).
  launchctl list 2>/dev/null | awk 'NR>1 && $1 ~ /^[0-9]+$/ {print $1, "launchd:" $3}'
}
lane_for_pid() { # <pid> [pane map text]
  local p=$1 map=${2:-} i=0 hit
  [ -n "$map" ] || map=$(pane_map)
  while [ -n "$p" ] && [ "$p" -gt 1 ] 2>/dev/null && [ $i -lt 30 ]; do
    hit=$(printf '%s\n' "$map" | awk -v p="$p" '$1==p{print $2; exit}')
    if [ -n "$hit" ]; then printf '%s' "${hit#amux-}"; return 0; fi
    p=$(ps -o ppid= -p "$p" 2>/dev/null | tr -d ' '); i=$((i+1))
  done
  printf 'no lane'
}

# The hand-off prompt (DESKT-58). Rounds 1-3 sent a four-line pointer, and the
# model did good work with it, but every escalation started cold: nothing said
# what the previous one for this class concluded, whether its fix held, or what
# "done" is. This carries all three, and a card-file handshake so the NEXT
# escalation can quote the card this one produces.
escalation_message() { # <cls> <verdict> <bundle> <card_file> <now_snapshot> <prev_age_h|-> <prev_card|-> <prev_snapshot|->
  local cls=$1 v=$2 bundle=$3 cardf=$4 nowsnap=$5 page=$6 pcard=$7 psnap=$8
  echo "Ask: find and fix the ROOT CAUSE of this Mac $cls constraint, then prove it cleared. The cleanup tick already applied every safe symptom fix, so a symptom fix alone is not done."
  echo
  echo "Constraint: $v"
  echo "Measured now: $nowsnap"
  if [ "$page" = "-" ]; then
    echo "History: first $cls escalation on record."
  else
    echo "History: this RECURRED. The previous $cls escalation was ${page}h ago (card: $pcard; measured then: $psnap). Start from that card: say whether its fix landed, why it did not hold, or what new cause this is."
  fi
  echo "Evidence: $bundle (top memory and CPU with owner and lane, what the tick did, disk writers)"
  echo "Runbook: git -C ${AMUX_REPO_DIR:-$HOME/Dev/amux} show origin/main:$RUNBOOK"
  echo
  echo "Done means all four:"
  echo "1. The cause named with its owner (lane, launchd agent, app, or macOS) and the measurement that shows it."
  echo "2. A fix at the cause: a commit with a test and a log signal, a config change, or one message to the owning lane with the evidence and one ask. Say which."
  echo "3. The constraint re-measured after the fix, cleared or not, with the number."
  echo "4. A card on your board with the above, and its id written to $cardf (just the id, e.g. MO-1234), so the next escalation for $cls can start from it."
  echo
  echo "Boundary: never delete another lane's uncommitted work, a repo, .git, credentials or a database; never touch anything under /private/tmp/claude-* by hand (live Claude session scratchpads: no process standing in a directory does not mean it is unused; this tick's claude-tmp arm owns that tree, with its liveness rules and its ledger); never kill a live lane's workload; spending money or anything outside the company needs Ethan."
}

# One board card for a failure nobody would otherwise see. Deduped by key for 24h
# in the state file, so a broken path files one card, not one per tick.
file_card() { # <state_file> <key> <title> <desc>
  local st=$1 key=$2 last now body cmd code
  now=$(date +%s); last=$(state_get "$st" "card_$key")
  if [ -n "$last" ] && [ $(( now - last )) -lt 86400 ]; then echo "already filed within 24h"; return 0; fi
  body=$(mktemp "${TMPDIR:-/tmp}/mac-card.XXXXXX")
  python3 -c 'import json,sys; print(json.dumps({"title":sys.argv[1],"desc":sys.argv[2],"session":"desktop","status":"todo","type":"escalation","next_action":"Read the tick output named in the description and restore the broken path.","acceptance_criteria":"The next scheduled tick runs and its escalation path reports no failure."}))' "$3" "$4" > "$body"
  cmd=${CARD_CMD//FILE/$body}
  code=$($cmd 2>/dev/null) || code=failed
  rm -f -- "${body:?}"
  case "$code" in 2??|ok) state_put "$st" "card_$key=$now"; echo "filed ($code)" ;; *) echo "card POST failed ($code)" ;; esac
}

# The idle floor for this tick: the normal one, or the tight one when free disk is
# under the threshold. An unmeasured disk (-1) keeps the normal floor.
# Running colima profiles, one per line, from `colima list --json` (one JSON
# object per line: {"name":"gs12-mvs","status":"Running",...}).
running_vm_profiles() {
  $VM_LIST_CMD 2>/dev/null | python3 -c '
import json,sys
for line in sys.stdin:
    line=line.strip()
    if not line: continue
    try: o=json.loads(line)
    except Exception: continue
    if str(o.get("status","")).lower()=="running" and o.get("name"): print(o["name"])' 2>/dev/null
}

# Prune build cache in every running VM and trim its disk so the host gets the
# blocks back. Sets VMS_PRUNED / VMS_FAILED. Each step is time-boxed.
prune_vm_build_caches() { # <dry:0|1> [free_gb]
  local dry=$1 free=${2:--1} p cmd out rc age=$VM_PRUNE_AGE
  VMS_PRUNED=0; VMS_FAILED=0
  if awk -v f="$free" -v u="$VM_PRUNE_URGENT_FREE_GB" 'BEGIN{exit !(f >= 0 && f < u)}'; then
    age=$VM_PRUNE_URGENT_AGE
    echo "mac-cleanup: vm build cache: ${free}G free is under ${VM_PRUNE_URGENT_FREE_GB}G, pruning cache unused for ${age} (not ${VM_PRUNE_AGE})"
  fi
  local profiles; profiles=$(running_vm_profiles)
  if [ -z "$profiles" ]; then echo "mac-cleanup: vm build cache: no running colima VM"; return 0; fi
  while IFS= read -r p; do
    [ -n "$p" ] || continue
    if [ "$dry" = 1 ]; then echo "mac-cleanup:   would prune build cache and trim colima VM $p (dry run)"; continue; fi
    cmd=${VM_PRUNE_CMD//PROFILE/$p}; cmd=${cmd//AGE/$age}; rc=0
    out=$(perl -e 'alarm shift; exec @ARGV' "$VM_STEP_S" $cmd 2>&1) || rc=$?
    if [ "$rc" != 0 ]; then
      VMS_FAILED=$((VMS_FAILED+1)); echo "mac-cleanup:   vm $p build-cache prune FAILED (rc $rc): $(printf '%s' "$out" | tail -1 | cut -c1-120)"; continue
    fi
    if [ "$VM_PRUNE_MAX_USED" != 0 ]; then
      cmd=${VM_PRUNE_CAP_CMD//PROFILE/$p}; cmd=${cmd//CAP/$VM_PRUNE_MAX_USED}; rc=0
      out=$(perl -e 'alarm shift; exec @ARGV' "$VM_STEP_S" $cmd 2>&1) || rc=$?
      if [ "$rc" = 0 ]; then
        echo "mac-cleanup:   vm $p build cache capped at $VM_PRUNE_MAX_USED: $(printf '%s' "$out" | grep -i 'total' | tail -1 | tr -s ' \t' ' ' | cut -c1-60) (verdict=vm_build_cache_capped)"
      else
        echo "mac-cleanup:   vm $p build-cache cap FAILED (rc $rc): $(printf '%s' "$out" | tail -1 | cut -c1-120) (verdict=vm_build_cache_cap_failed)"
      fi
    fi
    cmd=${VM_TRIM_CMD//PROFILE/$p}
    perl -e 'alarm shift; exec @ARGV' "$VM_STEP_S" $cmd >/dev/null 2>&1 || echo "mac-cleanup:   vm $p fstrim did not complete (build cache was still pruned)"
    VMS_PRUNED=$((VMS_PRUNED+1))
    echo "mac-cleanup:   vm $p build cache: $(printf '%s' "$out" | grep -i 'total' | tail -1 | tr -s ' \t' ' ' | cut -c1-60)"
  done <<EOF
$profiles
EOF
  echo "mac-cleanup: vm build cache: pruned ${VMS_PRUNED} running VM(s), failed ${VMS_FAILED} (images, containers and volumes untouched)"
}

# 0 if a VM is idle: guest 15-min load under the threshold AND no docker event
# but healthcheck execs in the window. 1 if busy. 2 if either probe could not be
# read, which is BUSY for every purpose: an unmeasured VM is never stopped.
vm_is_idle() { # <profile>
  local p=$1 cmd load ev rc=0
  cmd=${VM_LOAD_CMD//PROFILE/$p}
  local probe up
  probe=$(perl -e 'alarm 30; exec @ARGV' $cmd 2>/dev/null) || rc=$?
  load=$(printf '%s\n' "$probe" | awk 'NR==1{print $3}'); up=$(printf '%s\n' "$probe" | awk 'NR==2{print int($1)}')
  case "$load" in ''|*[!0-9.]*) return 2 ;; esac
  case "$up" in ''|*[!0-9]*) return 2 ;; esac
  # A VM younger than the window has had no chance to show activity: on
  # 2026-10-01 gs12-obs read idle 25 minutes after a lane created it.
  [ "$up" -ge $(( VM_IDLE_MIN * 60 )) ] || return 1
  cmd=${VM_EVENTS_CMD//PROFILE/$p}; cmd=${cmd//MIN/$VM_IDLE_MIN}
  ev=$(perl -e 'alarm 30; exec @ARGV' $cmd 2>/dev/null) || return 2
  awk -v l="$load" -v t="$VM_IDLE_LOAD" 'BEGIN{ exit !(l+0 < t+0) }' || return 1
  if printf '%s\n' "$ev" | grep -v -E '^(exec_create|exec_start|exec_die)' | grep -q .; then return 1; fi
  local ps name
  cmd=${VM_PS_CMD//PROFILE/$p}
  ps=$(perl -e 'alarm 30; exec @ARGV' $cmd 2>/dev/null) || return 2
  while IFS= read -r name; do
    [ -n "$name" ] || continue
    case " $VM_IDLE_IGNORE " in *" $name "*) continue ;; esac
    VM_BUSY_CONTAINER="$name"
    return 1
  done <<PSEOF
$ps
PSEOF
  return 0
}

# `colima stop` deletes the profile's docker context and only `colima start`
# recreates it, so a VM brought back any other way left every lane's
# `docker --context colima-<p>` failing "context not found" (gs12-tiering,
# 2026-10-03: goal-shared stopped here at 04:09, running again by 11:5x with no
# context; gs12-restore's context gone too). Keep the context, pointing at the
# VM's socket: while stopped a lane gets "cannot connect" (the truth), and the
# context works again however the VM is restarted.
keep_docker_context() { # <profile>
  local p=$1 ctx="colima-$1" sock="${COLIMA_HOME:-$HOME/.colima}/$1/docker.sock"
  command -v docker >/dev/null 2>&1 || return 0
  docker context inspect "$ctx" >/dev/null 2>&1 && return 0
  if docker context create "$ctx" --docker "host=unix://$sock" >/dev/null 2>&1; then
    echo "mac-cleanup:   kept docker context $ctx (colima stop removes it) -> $sock"
    echo "$(date '+%F %T') kept docker context $ctx after stopping $p" >> "$STATE_DIR/vm-stops.log"
  else
    echo "mac-cleanup:   WARN could not keep docker context $ctx after stopping $p"
  fi
}

# Remove tmp.* directories in the per-user temp dir untouched for
# USER_TMP_IDLE_MIN minutes that no running process uses as its cwd.
# reap_lane_tmp <dry>: remove ~/.amux/tmp/<worker>/<entry> when NOTHING inside
# it changed for LANE_TMP_IDLE_MIN minutes (the newest file decides, so a cargo
# target dir in use is kept even if its top-level mtime is old) and no running
# process has its cwd at or under it.
#
# Two more keeps (gs12-data, 2026-10-06): the 12 h sweep deleted a private
# kubeconfig and four scripts, one of them run by enabled schedules SCHED-598
# and SCHED-599. A lane's TMPDIR is also where its shell schedules live.
#  - An entry an enabled schedule's command names is kept. If the schedules
#    cannot be read, nothing is reaped this tick: a sweep that cannot see what
#    is in use must not guess.
#  - A plain file under LANE_TMP_KEEP_FILE_KB is kept. Those are hand-written
#    scripts and configs; the space this reaper exists for (101 GB on
#    2026-10-04) was build trees and extracts, never small files.
reap_lane_tmp() { # <dry:0|1>
  local dry=$1 root="${LANE_TMP_ROOT%/}" e ce n=0 kb=0 k inuse capped=0 named db n_named=0 n_small=0
  local small_kb=${LANE_TMP_KEEP_FILE_KB:-1024}
  [ -d "$root" ] || { echo "mac-cleanup: lane tmp: no $root"; return 0; }
  db=${LANE_TMP_SCHED_DB:-${AMUX_HOME:-$HOME/.amux}/amux.db}
  # Every ~/.amux/tmp/<worker>/<entry> an enabled schedule names, as
  # "<worker>/<entry>" lines. `~`, $HOME and the literal home all count.
  if ! named=$(sqlite3 -readonly -cmd '.timeout 5000' "$db" \
      "SELECT command FROM schedules WHERE enabled=1 AND deleted IS NULL" 2>/dev/null); then
    echo "mac-cleanup: lane tmp: WARN schedules unreadable ($db), reaped nothing this tick (verdict=lane_tmp_schedules_unmeasured)"
    return 0
  fi
  named=$(printf '%s\n' "$named" | grep -oE '(~|\$HOME|\$\{HOME\}|'"$HOME"')/\.amux/tmp/[^/[:space:]"'"'"';,`)|&<>]+/[^/[:space:]"'"'"';,`)|&<>]+' \
    | sed -E 's#^.*/\.amux/tmp/##' | sort -u)
  inuse=$(lsof -a -d cwd -Fn 2>/dev/null | sed -n 's/^n//p' | sed 's|^/private||')
  for e in "$root"/*/*; do
    [ -e "$e" ] || continue
    if [ "$n" -ge "${LANE_TMP_MAX:-300}" ]; then capped=1; break; fi
    [ -n "$(find "$e" -mmin -"$LANE_TMP_IDLE_MIN" -print -quit 2>/dev/null)" ] && continue
    if printf '%s\n' "$named" | grep -qxF "${e#"$root"/}"; then n_named=$((n_named+1)); continue; fi
    if [ -f "$e" ] && [ ! -L "$e" ] && [ "$(du -sk "$e" 2>/dev/null | cut -f1)" -lt "$small_kb" ] 2>/dev/null; then
      n_small=$((n_small+1)); continue
    fi
    ce=$( (cd "$e" 2>/dev/null && pwd -P) || printf '%s' "$e"); ce=${ce#/private}
    printf '%s\n' "$inuse" | awk -v p="$ce" '$0==p || index($0, p "/")==1 {f=1} END{exit !f}' && continue
    k=$(du -sk "$e" 2>/dev/null | cut -f1)
    if [ "$dry" = 1 ]; then n=$((n+1)); kb=$((kb+${k:-0})); continue; fi
    rm -rf -- "${e:?}" 2>/dev/null && { n=$((n+1)); kb=$((kb+${k:-0})); }
  done
  echo "mac-cleanup: lane tmp: $([ "$dry" = 1 ] && echo 'would remove' || echo removed) $n entr$([ "$n" = 1 ] && echo y || echo ies) idle over ${LANE_TMP_IDLE_MIN} min, $(awk -v k="$kb" 'BEGIN{printf "%.1fG", k/1048576}') ($root); kept $n_named named by an enabled schedule, $n_small small file(s) under ${small_kb}K$([ "$capped" = 1 ] && echo "; capped at $LANE_TMP_MAX this tick, the rest next tick")"
}

reap_user_tmp() { # <dry:0|1>
  local dry=$1 root="${USER_TMP_ROOT%/}" d b n=0 kb=0 k inuse
  [ -d "$root" ] || { echo "mac-cleanup: user tmp: no $root"; return 0; }
  inuse=$(lsof -a -d cwd -Fn 2>/dev/null | sed -n 's/^n//p' | sed 's|^/private||' | grep -oE '/tmp\.[A-Za-z0-9]+' | sort -u)
  while IFS= read -r d; do
    [ -n "$d" ] || continue
    b=${d##*/}
    printf '%s\n' "$inuse" | grep -qx "/$b" && continue
    k=$(du -sk "$d" 2>/dev/null | cut -f1)
    if [ "$dry" = 1 ]; then n=$((n+1)); kb=$((kb+${k:-0})); continue; fi
    rm -rf -- "${root:?}/${b:?}" 2>/dev/null && { n=$((n+1)); kb=$((kb+${k:-0})); }
  done <<EOF
$(find "$root" -maxdepth 1 -name 'tmp.*' -type d -mmin +"$USER_TMP_IDLE_MIN" 2>/dev/null)
EOF
  echo "mac-cleanup: user tmp: $([ "$dry" = 1 ] && echo 'would remove' || echo removed) $n tmp.* dir(s) idle over ${USER_TMP_IDLE_MIN} min, $(awk -v k="$kb" 'BEGIN{printf "%.1fG", k/1048576}') ($root)"
}

# Remove images in docker context <ctx> that no container uses and that were
# last tagged (or, never tagged, created) more than <age> ago. Prints one
# "deleted" line per image and a "Total reclaimed space" line like
# `image prune`, so the caller's summary reads the same.
vm_image_prune_by_tag() { # <ctx> <age: Nh | Nm | seconds>
  local ctx=$1 age=$2 secs ids used plan id
  case "$age" in *h) secs=$(( ${age%h} * 3600 )) ;; *m) secs=$(( ${age%m} * 60 )) ;; *) secs=$age ;; esac
  ids=$("$VM_DOCKER" --context "$ctx" images -q --no-trunc | sort -u) || return 1
  [ -n "$ids" ] || { echo "Total reclaimed space: 0B"; return 0; }
  used=$("$VM_DOCKER" --context "$ctx" ps -aq --no-trunc) || return 1
  [ -z "$used" ] || used=$("$VM_DOCKER" --context "$ctx" inspect -f '{{.Image}}' $used) || return 1
  # shellcheck disable=SC2086
  plan=$("$VM_DOCKER" --context "$ctx" image inspect $ids | USED="$used" SECS="$secs" python3 -c '
import json, os, sys, time
import calendar
used = set(os.environ["USED"].split()); cutoff = time.time() - int(os.environ["SECS"])
def utc(v):
    v = (v or "").rstrip("Z")
    if not v or v.startswith("0001-"): return None
    off = 0
    for sign in "+-":
        i = v.rfind(sign, 19)
        if i > 0:
            h, m = v[i+1:].split(":"); off = (int(h) * 3600 + int(m) * 60) * (1 if sign == "+" else -1); v = v[:i]; break
    return calendar.timegm(time.strptime(v[:19], "%Y-%m-%dT%H:%M:%S")) - off
for im in json.load(sys.stdin):
    if im["Id"] in used: continue
    t = utc(im.get("Created"))
    if t is None or t > cutoff: continue
    print(im["Id"], im.get("Size", 0))
') || return 1
  local n=0 bytes=0 sz
  while read -r id sz; do
    [ -n "$id" ] || continue
    if "$VM_DOCKER" --context "$ctx" rmi "$id" >/dev/null 2>&1; then
      echo "deleted: $id"; n=$((n+1)); bytes=$((bytes + sz))
    fi
  done <<EOF
$plan
EOF
  echo "Total reclaimed space: $(awk -v b="$bytes" 'BEGIN{printf "%.2fGB", b/1e9}') ($n image(s), verdict=vm_images_pruned_by_tag_age)"
}

# Prune unused images older than VM_IMAGE_PRUNE_AGE in every running VM.
prune_vm_images() { # <dry:0|1>
  local dry=$1 p cmd out rc n=0 total=""
  local profiles; profiles=$(running_vm_profiles)
  [ -n "$profiles" ] || { echo "mac-cleanup: vm images: no running colima VM"; return 0; }
  while IFS= read -r p; do
    [ -n "$p" ] || continue
    if [ "$dry" = 1 ]; then echo "mac-cleanup:   would prune unused images older than $VM_IMAGE_PRUNE_AGE in colima VM $p (dry run)"; continue; fi
    cmd=${VM_IMAGE_PRUNE_CMD//PROFILE/$p}; cmd=${cmd//AGE/$VM_IMAGE_PRUNE_AGE}; rc=0
    case "$cmd" in
      vm_image_prune_by_tag\ *)
        # A shell function cannot be exec'd: run it in a child bash under the
        # same alarm, with the function and its docker seam exported.
        export -f vm_image_prune_by_tag; export VM_DOCKER
        out=$(perl -e 'alarm shift; exec @ARGV' "$VM_STEP_S" bash -c "$cmd" 2>&1) || rc=$? ;;
      *) out=$(perl -e 'alarm shift; exec @ARGV' "$VM_STEP_S" $cmd 2>&1) || rc=$? ;;
    esac
    if [ "$rc" != 0 ]; then
      echo "mac-cleanup:   vm $p image prune FAILED (rc $rc): $(printf '%s' "$out" | tail -1 | cut -c1-120)"; continue
    fi
    n=$((n+1)); total=$(printf '%s' "$out" | grep -i 'total' | tail -1 | tr -s ' \t' ' ' | cut -c1-60)
    echo "mac-cleanup:   vm $p unused images older than $VM_IMAGE_PRUNE_AGE: ${total:-nothing to prune}"
  done <<EOF
$profiles
EOF
  echo "mac-cleanup: vm images: swept $n running VM(s) (in-use images and anything newer than $VM_IMAGE_PRUNE_AGE kept)"
}

# Stop every idle running VM. Sets VMS_STOPPED. Records each stop.
stop_idle_vms() { # <dry:0|1> <why>
  local dry=$1 why=$2 p rc n_busy=0 n_unk=0 profiles cmd
  VMS_STOPPED=0
  profiles=$(running_vm_profiles)
  if [ -z "$profiles" ]; then echo "mac-cleanup: idle VMs: no running colima VM"; return 0; fi
  while IFS= read -r p; do
    [ -n "$p" ] || continue
    rc=0; vm_is_idle "$p" || rc=$?
    if [ "$rc" = 1 ]; then n_busy=$((n_busy+1)); continue; fi
    if [ "$rc" = 2 ]; then n_unk=$((n_unk+1)); continue; fi
    if [ "$dry" = 1 ]; then echo "mac-cleanup:   would stop idle colima VM $p (dry run)"; continue; fi
    cmd=${VM_STOP_CMD//PROFILE/$p}
    if perl -e 'alarm 180; exec @ARGV' $cmd >/dev/null 2>&1; then
      VMS_STOPPED=$((VMS_STOPPED+1))
      mkdir -p "$STATE_DIR"
      echo "$(date '+%F %T') stopped colima VM $p ($why; guest load under $VM_IDLE_LOAD, no docker activity but healthchecks for ${VM_IDLE_MIN}m). Restore: colima start -p $p" >> "$STATE_DIR/vm-stops.log"
      echo "mac-cleanup:   stopped idle colima VM $p (restore: colima start -p $p)"
      keep_docker_context "$p"
    else
      echo "mac-cleanup:   FAILED to stop idle colima VM $p"
    fi
  done <<EOF
$profiles
EOF
  echo "mac-cleanup: idle VMs: stopped ${VMS_STOPPED}, kept busy ${n_busy}, kept unmeasured ${n_unk} (only under $why)"
}

# Quit Docker Desktop when idle. Sets DD_STOPPED (0/1).
stop_idle_docker_desktop() { # <dry:0|1> <why>
  local dry=$1 why=$2 pid up cmd ev ps
  DD_STOPPED=0
  pid=$($DD_PID_CMD 2>/dev/null | head -1)
  if [ -z "$pid" ]; then echo "mac-cleanup: docker desktop: not running"; return 0; fi
  cmd=${DD_UP_CMD//PIDOF/$pid}
  up=$(etime_secs "$($cmd 2>/dev/null | tr -d ' ')")
  case "$up" in ''|*[!0-9]*) echo "mac-cleanup: docker desktop: kept, uptime unmeasured"; return 0 ;; esac
  if [ "$up" -lt $(( VM_IDLE_MIN * 60 )) ]; then echo "mac-cleanup: docker desktop: kept, up ${up}s (under ${VM_IDLE_MIN}m)"; return 0; fi
  ps=$(perl -e 'alarm 30; exec @ARGV' $DD_PS_CMD 2>/dev/null) || { echo "mac-cleanup: docker desktop: kept, containers unmeasured"; return 0; }
  if printf '%s\n' "$ps" | grep -q .; then echo "mac-cleanup: docker desktop: kept, running $(printf '%s\n' "$ps" | head -1)"; return 0; fi
  cmd=${DD_EVENTS_CMD//MIN/$VM_IDLE_MIN}
  ev=$(perl -e 'alarm 30; exec @ARGV' $cmd 2>/dev/null) || { echo "mac-cleanup: docker desktop: kept, events unmeasured"; return 0; }
  if printf '%s\n' "$ev" | grep -v -E '^(exec_create|exec_start|exec_die)' | grep -q .; then echo "mac-cleanup: docker desktop: kept, docker activity in the last ${VM_IDLE_MIN}m"; return 0; fi
  if [ "$dry" = 1 ]; then echo "mac-cleanup: docker desktop: would quit idle Docker Desktop (dry run)"; return 0; fi
  local ok=0
  if [ -n "$DD_STOP_CMD" ]; then perl -e 'alarm 120; exec @ARGV' $DD_STOP_CMD >/dev/null 2>&1 && ok=1
  else perl -e 'alarm 120; exec @ARGV' osascript -e 'quit app "Docker Desktop"' >/dev/null 2>&1 && ok=1; fi
  if [ "$ok" = 1 ]; then
    DD_STOPPED=1; mkdir -p "$STATE_DIR"
    echo "$(date '+%F %T') quit Docker Desktop ($why; no container and no docker activity but healthchecks for ${VM_IDLE_MIN}m). Restore: open -a 'Docker Desktop'" >> "$STATE_DIR/vm-stops.log"
    echo "mac-cleanup: docker desktop: quit idle Docker Desktop (restore: open -a 'Docker Desktop')"
  else
    echo "mac-cleanup: docker desktop: FAILED to quit"
  fi
}

# Trim every running VM's filesystems every tick, whatever the disk reads.
# A colima data disk (/dev/vdb1) is mounted without discard and is not in the
# guest's fstab, so its weekly fstrim.timer never trims it: blocks a build or
# prune frees inside the VM stay allocated on the Mac until something trims.
# Gated on a tight disk, that left goal-shared's disk file swinging 40 to 50 G
# (2026-10-05: 269 -> 223 G at 05:00Z, back to 270 G only when the tight arm
# trimmed at 05:26Z), and the dip read as a 12 G/h burn (escalation
# 20261005-013827). fstrim only discards unused blocks. Sets VMS_TRIMMED.
trim_vms() { # <dry:0|1>
  local dry=$1 p cmd n_fail=0
  VMS_TRIMMED=0
  local profiles; profiles=$(running_vm_profiles)
  [ -n "$profiles" ] || { echo "mac-cleanup: vm trim: no running colima VM"; return 0; }
  while IFS= read -r p; do
    [ -n "$p" ] || continue
    if [ "$dry" = 1 ]; then echo "mac-cleanup:   would trim colima VM $p (dry run)"; continue; fi
    cmd=${VM_TRIM_CMD//PROFILE/$p}
    if perl -e 'alarm shift; exec @ARGV' "$VM_STEP_S" $cmd >/dev/null 2>&1; then VMS_TRIMMED=$((VMS_TRIMMED+1)); else n_fail=$((n_fail+1)); fi
  done <<EOF
$profiles
EOF
  echo "mac-cleanup: vm trim: trimmed ${VMS_TRIMMED} running VM(s), failed ${n_fail}"
}

effective_target_idle_h() { # <disk_free_gb> <normal_h> <tight_free_gb> <tight_h>
  awk -v f="$1" -v n="$2" -v t="$3" -v h="$4" 'BEGIN{ if (f >= 0 && f < t && h < n) print h; else print n }'
}

[ "${AMUX_CLEANUP_LIB_ONLY:-0}" = "1" ] && return 0 2>/dev/null

# ── measure ──────────────────────────────────────────────────────────────────
LOCK="$STATE_DIR/tick.lock"; mkdir -p "$STATE_DIR"
if ! tick_lock "$LOCK" "$LOCK_STALE_MIN"; then
  echo "mac-cleanup: previous tick still running (pid $(cat "$LOCK/pid" 2>/dev/null || echo unknown) holds $LOCK), not starting a second one"
  exit 0
fi
trap 'rm -f -- "${LOCK:?}/pid"; rmdir "${LOCK:?}" 2>/dev/null' EXIT
measured=true
level=$(sysctl -n kern.memorystatus_vm_pressure_level 2>/dev/null)
case "$level" in ''|*[!0-9]*) level=-1; measured=false ;; esac
read -r free_gb inactive_gb compressor_gb <<EOF
$(vm_stat 2>/dev/null | awk '
  NR==1{ match($0,/[0-9]+ bytes/); ps=substr($0,RSTART,RLENGTH)+0 }
  /Pages free/{f=$3} /Pages inactive/{i=$3} /occupied by compressor/{c=$5}
  END{ gsub(/\./,"",f); gsub(/\./,"",i); gsub(/\./,"",c);
       printf "%.2f %.2f %.2f", f*ps/2^30, i*ps/2^30, c*ps/2^30 }')
EOF
[ -n "${free_gb:-}" ] || { free_gb=-1; inactive_gb=-1; compressor_gb=-1; measured=false; }
swap_line=$(sysctl -n vm.swapusage 2>/dev/null)
swap_used=$(printf '%s' "$swap_line" | sed -E 's/.*used = ([0-9.]+)M.*/\1/'); case "$swap_used" in ''|*[!0-9.]*) swap_used=-1 ;; esac
swap_free=$(printf '%s' "$swap_line" | sed -E 's/.*free = ([0-9.]+)M.*/\1/'); case "$swap_free" in ''|*[!0-9.]*) swap_free=-1 ;; esac

echo "mac-cleanup: measured=$measured pressure=$level free=${free_gb}G inactive=${inactive_gb}G compressor=${compressor_gb}G swap_used=${swap_used}MB swap_free=${swap_free}MB dry_run=$DRY"
# Right after the reading, because the schedule keeps only the head of this output:
# the disk consumer is the line a reader needs during an emergency.
lima_disks_report "$LIMA_ROOT" "$LIMA_SHOW_KB"
# amux computer-use sandboxes (AMUX-5300): 4 GB Docker desktops, one per lane.
# The server's computer-sandbox-reaper stops idle ones; this line only NAMES
# them, so a reader of this tick sees them beside the VM that hosts them.
computer_sandboxes_report() {
  local out
  if ! command -v docker >/dev/null 2>&1; then
    echo "mac-cleanup: computer sandboxes: unmeasured (no docker CLI)"; return 0
  fi
  # A test seam, eval'd; the default is a plain call so its --format keeps its quotes.
  if [ -n "${AMUX_CLEANUP_COMPUTER_CMD:-}" ]; then out=$(eval "$AMUX_CLEANUP_COMPUTER_CMD" 2>/dev/null)
  else out=$(docker ps --filter label=amux-computer --format '{{.Label "amux-computer"}} {{.Status}}' 2>/dev/null); fi
  if [ $? -ne 0 ]; then
    echo "mac-cleanup: computer sandboxes: unmeasured (docker not answering)"; return 0
  fi
  local n; n=$(printf '%s' "$out" | grep -c . || true)
  if [ "$n" = 0 ]; then echo "mac-cleanup: computer sandboxes: 0 running"; return 0; fi
  echo "mac-cleanup: computer sandboxes: ${n} running ($(printf '%s' "$out" | awk '{print $1}' | paste -sd, -)); idle ones are stopped by amux's computer-sandbox-reaper, GET /api/computer/status"
}
computer_sandboxes_report

# ── report: running VMs with no fleet session referencing them ───────────────
# A VM eating 20+GB RAM with no session using it is the single most common
# resource leak on this box (measured: gs7-e ran 5 days at 96% CPU / 22GB RAM
# with zero fleet references, 2026-09-27). The tick cannot stop it (ethos rule 8)
# but it CAN name it loudly rather than burying it in the disk report.
vm_reference_report

# ── act: purge ───────────────────────────────────────────────────────────────
purged=no
if [ "$measured" = true ] && should_purge "$level" "$free_gb" "$PRESSURE_PURGE" "$FREE_FLOOR_GB"; then
  if [ "$DRY" = "1" ]; then
    purged="would (dry run)"
  else
    if $PURGE_CMD >/dev/null 2>&1; then
      sleep 3
      after=$(vm_stat 2>/dev/null | awk '
        NR==1{ match($0,/[0-9]+ bytes/); ps=substr($0,RSTART,RLENGTH)+0 }
        /Pages free/{f=$3} END{ gsub(/\./,"",f); printf "%.2f", f*ps/2^30 }')
      purged="yes (free ${free_gb}G -> ${after}G)"
    else
      # Passwordless sudo is limited to purge and mdutil here; anything else
      # means the grant changed, and saying so beats a silent no-op.
      purged="FAILED (is passwordless sudo for /usr/sbin/purge still granted?)"
    fi
  fi
else
  purged="not needed (trigger: pressure >= $PRESSURE_PURGE or free < ${FREE_FLOOR_GB}G)"
fi
echo "mac-cleanup: purge $purged"

# ── act: restart leaked agents named in the knob ─────────────────────────────
agents_restarted=0; agents_checked=0
uid=$(id -u)
for label in $AGENTS; do
  is_safe_label "$label" || { echo "mac-cleanup: refused agent label '$label' (not a plain com.* label)"; continue; }
  pid=$(launchctl list 2>/dev/null | awk -v l="$label" '$3==l {print $1}')
  case "$pid" in ''|*[!0-9]*) echo "mac-cleanup: agent $label not loaded"; continue ;; esac
  agents_checked=$((agents_checked+1))
  mem=$(top -l 1 -pid "$pid" -stats mem 2>/dev/null | tail -1 | tr -d ' ')
  gb=$(to_gb "${mem:-0}")
  if should_restart_agent "$gb" "$AGENT_LEAK_GB"; then
    if [ "$DRY" = "1" ]; then
      echo "mac-cleanup: agent $label at ${gb}G would be restarted (floor ${AGENT_LEAK_GB}G, dry run)"
    else
      cmd=${RESTART_CMD//UID/$uid}; cmd=${cmd//LABEL/$label}
      if $cmd >/dev/null 2>&1; then
        agents_restarted=$((agents_restarted+1))
        echo "mac-cleanup: agent $label restarted at ${gb}G (floor ${AGENT_LEAK_GB}G)"
      else
        echo "mac-cleanup: agent $label restart FAILED at ${gb}G"
      fi
    fi
  else
    echo "mac-cleanup: agent $label ${gb}G under the ${AGENT_LEAK_GB}G floor, left alone"
  fi
done

# ── act: kill stale Claude Code shell-snapshot processes ──────────────────────
# Claude Code spawns bash processes for background commands (build watchers, dev
# servers, CI polls). They persist after the session that created them ends, and
# occasionally hold busy-loops that burn a full core. Safe to kill: they are
# abandoned scaffolding, not user work.
stale_killed=0; stale_found=0
stale_cutoff_s=$((STALE_SHELL_SNAPSHOT_H * 3600))
while IFS= read -r line; do
  [ -n "$line" ] || continue
  pid=$(printf '%s' "$line" | awk '{print $1}')
  elapsed_raw=$(printf '%s' "$line" | awk '{print $2}')
  # Reuse etime_secs, the parser the family scan already trusts and tests. This
  # arm used to carry its OWN bash-arithmetic copy, and bash reads a leading-zero
  # field such as 08 or 09 as invalid octal: SCHED-465 printed "line 303: 09:
  # value too great for base" at 17:21 and 17:51 on 2026-09-24, then reported
  # found=0 while it could not have counted the very processes it exists for.
  # Two parsers for one format is how one of them stays broken. An unparseable
  # time is skipped and never guessed, since this arm KILLS what it matches.
  elapsed_s=$(etime_secs "$elapsed_raw")
  case "$elapsed_s" in ''|*[!0-9]*) continue ;; esac
  [ "$elapsed_s" -ge "$stale_cutoff_s" ] || continue
  stale_found=$((stale_found+1))
  cpu=$(ps -o pcpu= -p "$pid" 2>/dev/null | tr -d ' ')
  if [ "$DRY" = "1" ]; then
    echo "mac-cleanup: stale shell-snapshot pid=$pid age=${elapsed_raw} cpu=${cpu}% would be killed (dry run)"
  else
    kill "$pid" 2>/dev/null && stale_killed=$((stale_killed+1))
    echo "mac-cleanup: killed stale shell-snapshot pid=$pid age=${elapsed_raw} cpu=${cpu}%"
  fi
done <<EOF
$($STALE_PS_CMD 2>/dev/null | grep 'shell-snapshots/snapshot-bash' | grep -v grep)
EOF
echo "mac-cleanup: shell-snapshots found=${stale_found} killed=${stale_killed} (floor ${STALE_SHELL_SNAPSHOT_H}h)"

# ── report: the consumers this script must not touch ─────────────────────────
# Named with an owner, because the useful half of a hog report is who can act.
echo "mac-cleanup: consumers above ${REPORT_GB}G (reported, never killed):"
reported=0
while IFS= read -r line; do
  [ -n "$line" ] || continue
  pid=$(printf '%s' "$line" | awk '{print $1}'); mem=$(printf '%s' "$line" | awk '{print $2}')
  gb=$(to_gb "$mem")
  awk -v g="$gb" -v t="$REPORT_GB" 'BEGIN{ exit !(g+0 >= t+0) }' || continue
  cmd=$(ps -o command= -p "$pid" 2>/dev/null)
  echo "mac-cleanup:   ${gb}G pid=$pid $(owner_label_for_pid "$pid") — $(printf '%s' "$cmd" | awk '{print $1}' | sed 's|.*/||')"
  reported=$((reported+1))
done <<EOF
$(top -l 1 -o mem -n 12 -stats pid,mem 2>/dev/null | awk 'f{print} /^PID/{f=1}' | sed 's/\*//')
EOF
[ "$reported" = 0 ] && echo "mac-cleanup:   none"

# ── act: reap idle cargo target dirs ─────────────────────────────────────────
# Before the snapshot arm on purpose: freed blocks stay pinned by a local snapshot
# until it is thinned, so the reap comes first and the thin below sees the result.
tgt_free=$(df -k /System/Volumes/Data 2>/dev/null | awk 'NR==2{ printf "%.1f", $4/1048576 }'); case "$tgt_free" in ''|*[!0-9.]*) tgt_free=-1 ;; esac
tgt_idle=$(effective_target_idle_h "$tgt_free" "$TARGET_IDLE_H" "$TARGET_TIGHT_FREE_GB" "$TARGET_TIGHT_IDLE_H")
if [ "$tgt_idle" != "$TARGET_IDLE_H" ]; then
  echo "mac-cleanup: disk tight (${tgt_free}G free, under ${TARGET_TIGHT_FREE_GB}G): build output idle ${tgt_idle}h is reclaimable this tick, not ${TARGET_IDLE_H}h"
fi
reap_idle_cargo_targets "$TARGET_ROOTS" "$tgt_idle" "$DRY"
VMS_PRUNED=0
if [ "$tgt_idle" != "$TARGET_IDLE_H" ]; then prune_vm_build_caches "$DRY" "$tgt_free"; else trim_vms "$DRY"; fi
[ "${AMUX_CLEANUP_VM_IMAGE_PRUNE:-1}" = 1 ] && prune_vm_images "$DRY"
[ "${AMUX_CLEANUP_USER_TMP:-1}" = 1 ] && reap_user_tmp "$DRY"
[ "${AMUX_CLEANUP_LANE_TMP:-1}" = 1 ] && reap_lane_tmp "$DRY"
VMS_STOPPED=0
vm_level=$($PRESSURE_CMD 2>/dev/null); case "$vm_level" in ''|*[!0-9]*) vm_level=-1 ;; esac
if [ "$vm_level" -ge 2 ]; then stop_idle_vms "$DRY" "memory pressure $vm_level"; stop_idle_docker_desktop "$DRY" "memory pressure $vm_level"
elif [ "$tgt_idle" != "$TARGET_IDLE_H" ]; then stop_idle_vms "$DRY" "disk under ${TARGET_TIGHT_FREE_GB}G"; fi
reap_idle_worktrees "$WORKTREE_ROOTS" "$WORKTREE_IDLE_H" "$DRY"
ctmp_idle=$CLAUDE_TMP_IDLE_H
if [ "$tgt_idle" != "$TARGET_IDLE_H" ] && [ "$ctmp_idle" -gt 0 ] && [ "$CLAUDE_TMP_TIGHT_IDLE_H" -lt "$ctmp_idle" ]; then ctmp_idle=$CLAUDE_TMP_TIGHT_IDLE_H; fi
reap_claude_tmp "$CLAUDE_TMP_ROOT" "$ctmp_idle" "$DRY"

# ── act: thin APFS local snapshots when the disk is tight ────────────────────
disk_free_gb=$(df -k /System/Volumes/Data 2>/dev/null | awk 'NR==2{ printf "%.1f", $4/1048576 }')
case "$disk_free_gb" in ''|*[!0-9.]*) disk_free_gb=-1 ;; esac
snaps_before=$($SNAP_LIST_CMD 2>/dev/null | tail -n +2 | grep -c . )
if should_thin "$disk_free_gb" "$SNAPSHOT_FLOOR_GB" && [ "$snaps_before" = 0 ]; then
  # DESKT-49: with nothing to thin, running tmutil only produced "thinned 0 -> 0",
  # which reads as an attempt that freed nothing rather than as nothing to do.
  echo "mac-cleanup: snapshots none to thin (free ${disk_free_gb}G)"
elif should_thin "$disk_free_gb" "$SNAPSHOT_FLOOR_GB"; then
  if [ "$DRY" = "1" ]; then
    echo "mac-cleanup: snapshots ${snaps_before}, free ${disk_free_gb}G under the ${SNAPSHOT_FLOOR_GB}G floor — would thin up to ${SNAPSHOT_RECLAIM_GB}G (dry run)"
  else
    bytes=$(awk -v g="$SNAPSHOT_RECLAIM_GB" 'BEGIN{ printf "%d", g*1073741824 }')
    cmd=${THIN_CMD//BYTES/$bytes}; cmd=${cmd//URGENCY/$SNAPSHOT_URGENCY}
    if $cmd >/dev/null 2>&1; then
      snaps_after=$($SNAP_LIST_CMD 2>/dev/null | tail -n +2 | grep -c . )
      free_after=$(df -k /System/Volumes/Data 2>/dev/null | awk 'NR==2{ printf "%.1f", $4/1048576 }')
      echo "mac-cleanup: snapshots thinned ${snaps_before} -> ${snaps_after}, free ${disk_free_gb}G -> ${free_after}G (target ${SNAPSHOT_RECLAIM_GB}G, urgency ${SNAPSHOT_URGENCY})"
    else
      echo "mac-cleanup: snapshot thin FAILED with ${snaps_before} snapshots at ${disk_free_gb}G free"
    fi
  fi
else
  echo "mac-cleanup: snapshots ${snaps_before}, free ${disk_free_gb}G at or above the ${SNAPSHOT_FLOOR_GB}G floor — not thinned"
fi

# ── report: the largest process FAMILY, which per-process ranking cannot see ──
phys_kb=$(awk -v b="$(sysctl -n hw.memsize 2>/dev/null || echo 0)" 'BEGIN{ printf "%d", b/1024 }')
fam=$(ps -Ao pid=,ppid=,rss=,etime= 2>/dev/null | top_family)
if [ -n "$fam" ]; then
  set -- $fam
  fam_ppid=$1; fam_kb=$2; fam_n=$3; fam_age=$4
  fam_gb=$(awk -v k="$fam_kb" 'BEGIN{ printf "%.1f", k/1048576 }')
  fam_pct=$(awk -v f="$fam_kb" -v p="$phys_kb" 'BEGIN{ printf "%.1f", (p>0)? f/p*100 : -1 }')
  fam_hours=$(awk -v s="$fam_age" 'BEGIN{ printf "%.1f", s/3600 }')
  if family_exceeds "$fam_kb" "$phys_kb" "$FAMILY_SHARE_PCT"; then
    echo "mac-cleanup: FAMILY ${fam_gb}G (${fam_pct}% of RAM) in ${fam_n} children of pid ${fam_ppid}, oldest ${fam_hours}h — $(owner_label_for_pid "$fam_ppid") — reported, never killed"
  elif family_too_old "$fam_age" "$FAMILY_AGE_H"; then
    echo "mac-cleanup: FAMILY ${fam_gb}G (${fam_pct}% of RAM) in ${fam_n} children of pid ${fam_ppid} has run ${fam_hours}h, past the ${FAMILY_AGE_H}h ceiling — $(owner_label_for_pid "$fam_ppid")"
  else
    echo "mac-cleanup: largest family ${fam_gb}G (${fam_pct}% of RAM) in ${fam_n} children of pid ${fam_ppid}, under the ${FAMILY_SHARE_PCT}% share and ${FAMILY_AGE_H}h ceiling"
  fi
else
  echo "mac-cleanup: family scan produced no rows (ps unavailable?)"
fi

fseventsd_reading() {
  local pid; pid=$(pgrep -x fseventsd 2>/dev/null | head -1)
  [ -n "$pid" ] || return 0
  echo "$(to_gb "$(top -l 1 -pid "$pid" -stats mem 2>/dev/null | tail -1 | tr -d ' ')") $(ps -o pcpu= -p "$pid" 2>/dev/null | tr -d ' ')"
}
fse_reading=$($FSEVENTSD_CMD 2>/dev/null)
fse_verdict=""
if [ -n "$fse_reading" ]; then
  fse_gb=${fse_reading%% *}; fse_cpu=${fse_reading#* }
  fse_cpu_int=${fse_cpu%%.*}
  fse_hot=0
  if needs_reboot "$fse_gb" "$FSEVENTSD_REBOOT_GB"; then fse_hot=1; fi
  if [ "${fse_cpu_int:-0}" -ge "$FSEVENTSD_CPU_PCT" ] 2>/dev/null; then fse_hot=1; fi
  if [ "$fse_hot" = 1 ]; then
    echo "mac-cleanup: fseventsd holds ${fse_gb}G at ${fse_cpu:-?}% CPU and is SIP-protected — a reboot is the only remedy (owner runs: sudo fdesetup authrestart)"
    # In this shell, not $(...): churn_census sets CHURN_COMPLETE, which a subshell would lose.
    churn_f=$(mktemp "${TMPDIR:-/tmp}/churn-out.XXXXXX")
    churn_census "$CHURN_ROOTS" "$CHURN_MIN" "$CHURN_BUDGET_S" "$CHURN_FILES" > "$churn_f"
    churn=$(cat "$churn_f"); rm -f -- "${churn_f:?}"
    if [ -n "$churn" ]; then
      printf '%s\n' "$churn" | while read -r n d; do echo "mac-cleanup:   fseventsd feed: $n files modified in ${CHURN_MIN}m under $d"; done
      fse_top=$(printf '%s\n' "$churn" | head -1 | awk '{print $1" files in '"${CHURN_MIN}"'m under "$2}')
    else
      fse_top="no directory over ${CHURN_FILES} modified files in ${CHURN_MIN}m (scan complete=${CHURN_COMPLETE}; deletions are not counted)"
      echo "mac-cleanup:   fseventsd feed: $fse_top"
    fi
    fse_verdict="fseventsd holds ${fse_gb}G at ${fse_cpu:-?}% CPU (over ${FSEVENTSD_REBOOT_GB}G or ${FSEVENTSD_CPU_PCT}%); stop the file churn feeding it, then only a reboot returns the memory. Top feed: $fse_top"
  else
    echo "mac-cleanup: fseventsd ${fse_gb}G ${fse_cpu:-?}% CPU, under the ${FSEVENTSD_REBOOT_GB}G/${FSEVENTSD_CPU_PCT}% thresholds"
  fi
fi

# ── assess: what is still constrained, why, and who fixes the cause ──────────
fam_over=0
if [ -n "${fam_kb:-}" ] && family_exceeds "$fam_kb" "$phys_kb" "$FAMILY_SHARE_PCT"; then fam_over=1; fi
now=$(date +%s)
disk_now=$(df -k /System/Volumes/Data 2>/dev/null | awk 'NR==2{ printf "%.1f", $4/1048576 }')
case "$disk_now" in ''|*[!0-9.]*) disk_now=-1 ;; esac
level_now=$($PRESSURE_CMD 2>/dev/null); case "$level_now" in ''|*[!0-9]*) level_now=-1 ;; esac
swap_free_now=$(sysctl -n vm.swapusage 2>/dev/null | sed -E 's/.*free = ([0-9.]+)M.*/\1/'); case "$swap_free_now" in ''|*[!0-9.]*) swap_free_now=-1 ;; esac
swap_total_now=$(sysctl -n vm.swapusage 2>/dev/null | sed -E 's/.*total = ([0-9.]+)M.*/\1/'); case "$swap_total_now" in ''|*[!0-9.]*) swap_total_now=-1 ;; esac
load15=$(sysctl -n vm.loadavg 2>/dev/null | awk '{print $4}'); case "$load15" in ''|*[!0-9.]*) load15=-1 ;; esac
ncpu=$(sysctl -n hw.ncpu 2>/dev/null || echo 0)
STATE="$STATE_DIR/state"
prev_ts=$(state_get "$STATE" last_ts); prev_free=$(state_get "$STATE" last_disk_free_gb)
HISTORY_CMD=${AMUX_CLEANUP_HISTORY_CMD:-curl -sk --max-time 15 $(amux url 2>/dev/null || echo https://localhost:8824)/api/metrics/host/history?since_h=2}
read -r burn htf hist_n hist_span <<EOF
$($HISTORY_CMD 2>/dev/null | burn_from_history "$BURN_MIN_GBH")
EOF
burn_src="history ${hist_n} samples over ${hist_span}h"
if [ "$htf" = "-" ] && [ "$burn" = "-" ]; then
read -r burn htf <<EOF
$(disk_trend "${prev_ts:-0}" "${prev_free:-0}" "$now" "$disk_now" "$BURN_MIN_GBH")
EOF
  burn_src="2 readings (history had ${hist_n} samples over ${hist_span}h)"
fi
if [ "$burn" = "-" ]; then burn_txt="unmeasured (no previous reading)"; else burn_txt="${burn}G/h from ${burn_src}"; fi
if [ "$htf" = "-" ]; then htf_txt="not filling"; else htf_txt="full in ${htf}h"; fi
[ "$DRY" = "1" ] || state_put "$STATE" "last_ts=$now" "last_disk_free_gb=$disk_now"
verdicts=$(classify_constraints "$disk_now" "$burn" "$htf" "$level_now" "$swap_free_now" "$load15" "$ncpu" "$fam_over" "$swap_total_now")
if [ -n "${fse_verdict:-}" ]; then verdicts=$(printf '%s\n%s\n' "$verdicts" "$fse_verdict" | grep . || true); fi
echo "mac-cleanup: assess disk=${disk_now}G burn=${burn_txt} ${htf_txt} pressure=${level_now} swap_free=${swap_free_now}MB swap_total=${swap_total_now}MB load15=${load15}/${ncpu}"
escalated=0
if [ -z "$verdicts" ]; then
  echo "mac-cleanup: constraints none"
else
  stamp=$(date +%Y%m%d-%H%M%S)
  bundle="$STATE_DIR/rca/$stamp.md"
  mkdir -p "$STATE_DIR/rca"
  pmap=$(pane_map)
  {
    echo "# Mac resource RCA bundle $stamp"
    echo; echo "## Still constrained after the tick's symptom fixes"; printf '%s\n' "$verdicts" | sed 's/^/- /'
    echo; echo "## What the tick already did"
    echo "- purge: $purged"; echo "- cargo targets reaped: ${TARGETS_REAPED:-0} ($(fmt_kb "${TARGETS_REAPED_KB:-0}"))"
    echo "- stale shells killed: $stale_killed; agents restarted: $agents_restarted"
    echo; echo "## Top memory (with owner)"
    top -l 1 -o mem -n 10 -stats pid,mem,command 2>/dev/null | awk 'f{print} /^PID/{f=1}' | sed 's/\*//' | while read -r pid mem cmd; do
      echo "- $mem pid=$pid $cmd — $(owner_label_for_pid "$pid"), lane: $(lane_for_pid "$pid" "$pmap")"; done
    echo; echo "## Top CPU (with owner)"
    ps -Ao pid=,pcpu=,comm= 2>/dev/null | sort -k2 -rn | head -10 | while read -r pid cpu cmd; do
      echo "- ${cpu}% pid=$pid $(basename "$cmd") — $(owner_label_for_pid "$pid"), lane: $(lane_for_pid "$pid" "$pmap")"; done
    if printf '%s\n' "$verdicts" | grep -q '^disk '; then
      echo; echo "## Files over 500M written in the last hour (the writers)"
      IFS=':' read -r -a wroots <<< "$TARGET_ROOTS"
      # ONE budget for the whole search, not one per root: 9 roots at 45s each
      # was 405s on its own and pushed the tick past the scheduler's 600s kill.
      # Few-file roots first (VM disks, the amux home), and each remaining root
      # gets an equal share of what is left, so one tree full of build files
      # cannot spend it all: the first cut spent 60s in the scratchpads and
      # searched 1 root of 9. Cargo's internals are pruned for the same reason.
      wlist=("$HOME/.colima" "$HOME/.amux"); for r in ${wroots[@]+"${wroots[@]}"}; do wlist+=("${r%@*}"); done
      wdeadline=$(( $(date +%s) + WRITERS_BUDGET_S )); wn=${#wlist[@]}; wi=0
      for r in "${wlist[@]}"; do
        wi=$((wi+1)); [ -d "$r" ] || continue
        wleft=$(( wdeadline - $(date +%s) )); if [ "$wleft" -le 0 ]; then echo "SEARCH-CUT $r" >&2; continue; fi
        wslice=$(( wleft / (wn - wi + 1) )); [ "$wslice" -ge 3 ] || wslice=3
        { perl -e 'alarm shift; exec @ARGV' "$wslice" find "$r" -xdev \( -name node_modules -o -name .git -o -name deps -o -name incremental -o -name .fingerprint -o -name build \) -prune -o -type f -size +500M -mmin -60 -exec stat -f '%b %z %N' {} + 2>/dev/null; } 2>/dev/null || echo "SEARCH-CUT $r" >&2
      done 2>"$STATE_DIR/writers.cut" | sort -u | sort -rn | head -15 | awk '{a=$1*512/2^30; z=$2/2^30; $1=$2=""; sub(/^  /,""); printf "- %.1fG allocated (%.1fG apparent) %s\n", a, z, $0}'
      if [ -s "$STATE_DIR/writers.cut" ]; then echo "(search budget of ${WRITERS_BUDGET_S}s: $(grep -c . "$STATE_DIR/writers.cut") of ${#wlist[@]} root(s) not fully searched: $(sed 's/^SEARCH-CUT //' "$STATE_DIR/writers.cut" | tr '\n' ' '))"; fi
      echo "(allocated is what the disk actually holds; a sparse VM disk's apparent size is its ceiling, not its use)"
    fi
    echo; echo "## Full tick output"; echo "~/.amux/logs/mac-cleanup-tick.last"
  } > "$bundle"
  while IFS= read -r v; do
    cls=${v%% *}
    if [ "$DRY" = "1" ]; then echo "mac-cleanup: constraint $v — would escalate (dry run)"; continue; fi
    if ! escalation_due "$STATE" "$cls" "$now" "$ESCALATE_COOLDOWN_H"; then
      echo "mac-cleanup: constraint $v — escalation suppressed (last $cls escalation under ${ESCALATE_COOLDOWN_H}h ago)"; continue
    fi
    msg="$STATE_DIR/rca/$stamp.$cls.msg"; cardf="$STATE_DIR/rca/$stamp.$cls.card"
    nowsnap="disk=${disk_now}G burn=${burn_txt} load15=${load15}/${ncpu} pressure=${level_now} swap_free=${swap_free_now}MB"
    plast=$(state_get "$STATE" "esc_$cls"); pstamp=$(state_get "$STATE" "esc_${cls}_stamp"); psnap=$(state_get "$STATE" "esc_${cls}_snap")
    page=-; pcard=-
    if [ -n "$plast" ]; then
      page=$(awk -v a="$now" -v b="$plast" 'BEGIN{printf "%.1f", (a-b)/3600}')
      pcard=$(tr -d '[:space:]' < "$STATE_DIR/rca/$pstamp.$cls.card" 2>/dev/null); [ -n "$pcard" ] || pcard="none written (the previous turn did not record one)"
      [ -n "$psnap" ] || psnap="not recorded"
    fi
    escalation_message "$cls" "$v" "$bundle" "$cardf" "$nowsnap" "$page" "$pcard" "$psnap" > "$msg"
    cmd=${ESCALATE_CMD//TARGET/$ESCALATE_TO}; cmd=${cmd//FILE/$msg}
    if sendout=$($cmd 2>&1); then
      state_put "$STATE" "esc_$cls=$now" "esc_${cls}_stamp=$stamp" "esc_${cls}_snap=$nowsnap" "sendfail_$cls=0"; escalated=$((escalated+1))
      echo "mac-cleanup: constraint $v — escalated to $ESCALATE_TO$( [ "$page" = - ] || echo " (recurrence, previous ${page}h ago, card $pcard)") (bundle $bundle)"
    else
      # The reason, not just the word: round 1 printed FAILED and the cause (an
      # isolated target) was only findable by re-running the send by hand.
      reason=$(printf '%s' "$sendout" | tail -1 | cut -c1-160)
      fails=$(( $(state_get "$STATE" "sendfail_$cls" || true) + 1 )); state_put "$STATE" "sendfail_$cls=$fails"
      echo "mac-cleanup: constraint $v — escalation to $ESCALATE_TO FAILED ($fails in a row): $reason (bundle $bundle)"
      # A failed send is otherwise visible only in a file nobody reads (DESKT-58).
      if [ "$fails" -ge "$SENDFAIL_CARD_AFTER" ]; then
        echo "mac-cleanup:   send-failure card: $(file_card "$STATE" "sendfail" "Mac cleanup tick cannot reach $ESCALATE_TO: $cls escalations failing" "SCHED-465 failed to send $fails $cls escalation(s) in a row to $ESCALATE_TO. Last error: $reason. Constraint: $v. Bundle: $bundle. Full output: ~/.amux/logs/mac-cleanup-tick.last")"
      fi
    fi
  done <<EOF
$verdicts
EOF
fi

if [ "$SECONDS" -ge "$TICK_WARN_S" ]; then
  echo "mac-cleanup: WARN tick took ${SECONDS}s, over ${TICK_WARN_S}s: the scheduler kills it at 600s, so a slower run is lost; tighten the budgets"
fi
echo "mac-cleanup: done elapsed=${SECONDS}s purge=${purged%% *} agents_restarted=${agents_restarted}/${agents_checked} stale_shells=${stale_killed} targets_reaped=${TARGETS_REAPED} claude_tmp_reaped=${CLAUDE_TMP_REAPED:-0} constraints=$(printf '%s' "$verdicts" | grep -c . || true) escalated=${escalated} reported=${reported}"
exit 0
