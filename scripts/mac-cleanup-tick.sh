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
# The census walks files, and on 2026-10-20 it could not finish: ~/Dev holds run
# directories of 40k-140k body files each, a directory-only walk of it ran past
# 300s, and the 60s budget went to /private/tmp first. So the escalation said "no
# directory over 2000 modified files (scan complete=no)" while one recorder wrote
# 7,000 files a minute into a single directory there (RCA 20261020-095236). When
# fswatch is installed the tick asks FSEvents itself instead: a CHURN_SAMPLE_S
# subscription sees exactly the stream fseventsd is processing, deletions
# included, in fixed time, whatever the size of the tree. The census stays as the
# fallback. CHURN_SAMPLE_CMD prints one changed path per line for <seconds>;
# AMUX_CLEANUP_CHURN_SAMPLE_CMD=off forces the census.
CHURN_SAMPLE_S=${AMUX_CLEANUP_CHURN_SAMPLE_S:-20}
CHURN_SAMPLE_ROOTS=${AMUX_CLEANUP_CHURN_SAMPLE_ROOTS:-$HOME:/private/tmp:/private/var/folders}
CHURN_FSWATCH=$(command -v fswatch 2>/dev/null || { [ -x /opt/homebrew/bin/fswatch ] && echo /opt/homebrew/bin/fswatch; } || { [ -x /usr/local/bin/fswatch ] && echo /usr/local/bin/fswatch; } || true)
CHURN_SAMPLE_CMD=${AMUX_CLEANUP_CHURN_SAMPLE_CMD:-${CHURN_FSWATCH:+churn_fswatch}}
[ "$CHURN_SAMPLE_CMD" = off ] && CHURN_SAMPLE_CMD=""
# The sample reads FSEvents from fseventsd, the daemon it is measuring, so a
# wedged fseventsd reads as a quiet disk (RCA 20261028-133155: pid 347 at 100%
# CPU holding 51G delivered 0 of 500 files written during a sample). A control:
# write CHURN_CONTROL_N files under the state dir while sampling and require the
# sample to see one. auto = on for the real fswatch sampler; 1 forces it on.
CHURN_CONTROL=${AMUX_CLEANUP_CHURN_CONTROL:-auto}
CHURN_CONTROL_N=20
# Seam: prints "<gb> <cpu_pct>" for fseventsd, or nothing when it is not running.
# The suites stub it so the live daemon cannot decide a test (as PRESSURE_CMD does).
FSEVENTSD_CMD=${AMUX_CLEANUP_FSEVENTSD_CMD:-fseventsd_reading}
# RESTART A BLOATED fseventsd ITSELF (Mac memory RCA 20261027-180101). fseventsd
# keeps every byte it grows for its whole life: 7.8G -> 30G in one day on this
# Mac, 21G of it compressed, and the only way back is a restart (launchd brings
# it straight back; no reboot). Three escalations in a row asked the owner to
# run it by hand. Over FSEVENTSD_RESTART_GB the tick runs it, at most once per
# FSEVENTSD_RESTART_COOLDOWN_H, through `sudo -n`, so it needs one sudoers line
# from the owner (printed when missing) and never prompts. Off with
# AMUX_CLEANUP_FSEVENTSD_RESTART_GB=0.
FSEVENTSD_RESTART_GB=${AMUX_CLEANUP_FSEVENTSD_RESTART_GB:-12}
FSEVENTSD_RESTART_COOLDOWN_H=${AMUX_CLEANUP_FSEVENTSD_RESTART_COOLDOWN_H:-6}
FSEVENTSD_RESTART_CMD=${AMUX_CLEANUP_FSEVENTSD_RESTART_CMD:-sudo -n /usr/bin/killall fseventsd}
FSEVENTSD_SUDOERS_LINE="$(id -un 2>/dev/null || echo "$USER") ALL=(root) NOPASSWD: /usr/bin/killall fseventsd"
# A spin is escalated once per fseventsd process. The same pid still spinning
# is the owner's restart still pending, so the next escalation waits for a new
# pid or this many hours (RCA fseventsd 20261021-195930: pid 34409 re-asked 6h
# after 20261021-135901 with nothing new and nothing a lane could do).
FSEVENTSD_PID_CMD=${AMUX_CLEANUP_FSEVENTSD_PID_CMD:-pgrep -x fseventsd}
FSEVENTSD_SPIN_REMIND_H=${AMUX_CLEANUP_FSEVENTSD_SPIN_REMIND_H:-24}
# A memory constraint whose holder is an fseventsd already escalated and waiting
# on the owner's restart is the same ask again: memory RCAs 20261028-081710,
# 20261029-003047 and 20261029-063250 each named fseventsd (43G, 65G, 68G) and
# the one owner step, six hours apart, while the fseventsd class itself said
# "owner pending, not escalated again". At or over this size, fseventsd counts
# as the holder and memory waits with it.
MEM_FSE_HOLDER_GB=${AMUX_CLEANUP_MEM_FSE_HOLDER_GB:-8}
# Churn this many times the floor on an already-escalated pid is a new cause and
# escalates; less is ordinary work on top of the same stuck process (RCA
# fseventsd 20261022-075928: 228 events in 20s from cargo builds, floor 134,
# re-escalated pid 34409 as "stop the churn" while it was the stuck thread).
FSEVENTSD_CHURN_NEW_X=${AMUX_CLEANUP_FSEVENTSD_CHURN_NEW_X:-5}
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
# $HOME/.amux@2 (disk RCA 20261011-174748): every side target a lane or a subagent
# builds into lives there as ~/.amux/rust-build-target-<name> (CLAUDE.md names the
# shared one; lanes, e2e harnesses and subagents add their own). The roots never
# covered it, so 106G sat there unseen while one afternoon added 58G of new ones.
# Depth 2 reaches <name>/CACHEDIR.TAG and nothing deeper; the shared target is in
# TARGET_KEEP, and a target being built holds open files, which the lsof guard keeps.
# $HOME/.amux/tmp@2, FIRST (disk RCA 2026-10-13 20:16): per-agent targets land at
# ~/.amux/tmp/<name>/CACHEDIR.TAG (desktop-target 34.8G, target-drawzoom/rt/
# schedspec/drawfix/schedfail 30G more, all built that day). $HOME/.amux@2 stops one
# level short of them, and the tick's report read "found 2 ... scan complete=no":
# the 310G scratchpad root listed next spends the whole scan budget, so a cheap
# root placed after it is never reached. Shallow and high-yield goes first.
# @9, not @2 (disk RCA 2026-10-14 02:23, 3.7G free): a worker's TMPDIR,
# ~/.amux/tmp/<worker>/, holds its checkouts, and their targets sit at
# <worker>/<run>/<tree>/server/mvs/shard-rs/target, eight levels down. mxp-gs12 had
# 183.1G in seven of them, 155G idle past 6h, none visible at @2. The depth-9 scan
# of the whole tree takes about 8s.
# @3 AND @9, @3 first (disk RCA 20261018-075226): the @9 walk no longer finishes
# in its share of the scan budget (complete=no), and find walks depth-first, so
# it timed out inside one checkout before reaching mxp-gs12/_target (55G, two
# levels down, growing ~20G/h). The @3 pass finds every per-worker target in
# well under a second (maxdepth counts the CACHEDIR.TAG file itself, so a
# target two directories down needs 3); sort -u drops the duplicates the @9 pass adds.
# /private/tmp/claude-<uid>@8 (disk RCA 2026-10-12): a session that keeps its clones under
# scratchpad/work/<clone>/ puts the tag at <proj>/<session>/scratchpad/work/<clone>/target/CACHEDIR.TAG,
# seven levels down. At @6 the scan never saw them: one session held 47G and 23G of targets idle
# over 6h with 21G free on the disk, and the summary read "eligible 0".
# $HOME@3, last (disk RCA 2026-10-16, 207G free): a lane's own dot-folder,
# ~/.mvs-infra-ops/target{,-mi5852,-base}, held 263G of build output idle since
# 2026-09-28. The tag sits at ~/.<lane>/target/CACHEDIR.TAG, three levels down,
# and no root above reached it, so every tick reported "consumers above 10G:
# none". Last, so the high-yield roots spend the budget first.
TARGET_ROOTS=${AMUX_CLEANUP_TARGET_ROOTS:-$HOME/.amux/tmp@3:$HOME/.amux/tmp@9:/private/tmp/claude-$(id -u)@8:/private/tmp@3:${USER_TMP%/}@3:$HOME/Documents/Codex@8:$HOME/Dev@8:$HOME/.amux/worktrees@6:$HOME/.amux@2:$HOME/.ao/data/worktrees@8:$HOME@3}
TARGET_IDLE_H=${AMUX_CLEANUP_TARGET_IDLE_H:-24}
# Superseded build output INSIDE an active target (disk RCA 20261018-075226).
# The idle reaper above needs the whole target quiet; a lane that builds one
# crate from many trees never is. mxp-gs12's target held 33 debug + 31 release
# copies of one test binary (one per source path, each a new hash) and 8937
# orphaned *.rcgu.o from compiles killed by `timeout`, growing ~20G/h. Kept per
# stem: the newest N executables; anything younger than the age floor or open
# is never touched. 0 disables the arm.
SUPERSEDED_KEEP=${AMUX_CLEANUP_SUPERSEDED_KEEP:-3}
SUPERSEDED_MIN_AGE_MIN=${AMUX_CLEANUP_SUPERSEDED_MIN_AGE_MIN:-60}
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
# `builder prune --max-used-space` reclaimed 0B on goal-shared with 87.8G of
# cache (buildx 0.24 against Docker 29, containerd store) while an age prune
# freed 21.8G, so the cap measures `builder du` itself and, over the cap,
# prunes with the short window VM_PRUNE_CAP_AGE.
VM_PRUNE_CAP_AGE=${AMUX_CLEANUP_VM_PRUNE_CAP_AGE:-1h}
VM_PRUNE_CAP_CMD=${AMUX_CLEANUP_VM_PRUNE_CAP_CMD:-vm_build_cache_cap colima-PROFILE CAP}
# How full the VM's OWN data filesystem is (Mac disk RCA 20261021-175921). The
# host reads a VM's disk file as allocated bytes, so goal-shared at 98% inside
# (178 of 196 G used, 5 G left beside 2.6 G MVS snapshot writes) escalated as a
# host "disk burn" while the 40gb cache cap logged "26.1GB is within 40gb" and
# freed nothing. Over VM_GUEST_TIGHT_PCT the cap drops to 0 and keeps only cache
# used in the last VM_GUEST_TIGHT_AGE; at VM_GUEST_FULL_PCT it is a constraint.
VM_GUEST_DF_CMD=${AMUX_CLEANUP_VM_GUEST_DF_CMD:-colima ssh -p PROFILE -- df -kP /mnt/lima-colima-PROFILE}
VM_GUEST_TIGHT_PCT=${AMUX_CLEANUP_VM_GUEST_TIGHT_PCT:-85}
VM_GUEST_FULL_PCT=${AMUX_CLEANUP_VM_GUEST_FULL_PCT:-90}
VM_GUEST_TIGHT_AGE=${AMUX_CLEANUP_VM_GUEST_TIGHT_AGE:-10m}
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
# The same dir holds the Rust tempfile crate's .tmpXXXXXX dirs. 57 amux test
# helpers leaked theirs (std::mem::forget) until test_tempdirs::keep: 11,735
# dirs, 29 GB, ~400 more an hour (disk RCA 20261029-103955). Tests that are
# killed still leave them. One whose holder-lock names a live pid is kept, and
# at most this many go per tick.
USER_TMP_MAX=${AMUX_CLEANUP_USER_TMP_MAX:-3000}
# Workers with workspace isolation get TMPDIR=~/.amux/tmp/<worker>, outside the
# per-user temp dir above, so nothing reaped them: 101 GB in 13,336 entries
# idle over 12 h on 2026-10-04 with the disk at 57 GB free.
LANE_TMP_ROOT=${AMUX_CLEANUP_LANE_TMP_ROOT:-$HOME/.amux/tmp}
LANE_TMP_IDLE_MIN=${AMUX_CLEANUP_LANE_TMP_IDLE_MIN:-1440}
# At most this many entries per tick. Deleting 11,781 entries (millions of
# files) in one pass on 2026-10-04 drove fseventsd from 9 to 57 GB resident
# and filled 52 GB of swap; a capped pass leaves the rest for the next tick.
LANE_TMP_MAX=${AMUX_CLEANUP_LANE_TMP_MAX:-300}
# Time for the whole lane tmp pass, and for one entry's idle walk or size. An
# unbounded find over mxp-gs12's 23 checkouts (cargo targets inside) ran past the
# scheduler's 600s kill six ticks running on 2026-10-14 at load 133. An entry
# whose walk runs out of time is kept; the pass resumes at a cursor next tick.
LANE_TMP_BUDGET_S=${AMUX_CLEANUP_LANE_TMP_BUDGET_S:-90}
LANE_TMP_WALK_S=${AMUX_CLEANUP_LANE_TMP_WALK_S:-20}
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
# Every running container in a colima VM gets a CPU cap if it has none (Mac hog
# RCA 20261020-115243). colima goal-shared has 12 vCPUs, and an uncapped
# container's Ray and MVS shard reached ~10 of them: the guest's own load was
# 10-12 and the VM process read 500-1580% on the Mac at load15 36/28. A one-off
# `docker update --cpus 3` (the 10-19 hog card) was lost when the owning lane's
# launcher recreated the container without --cpus, so the cap has to be reapplied
# every tick. A container that already has a cap is left alone, whatever it is.
# 0 turns the arm off; names in VM_CPU_CAP_EXEMPT (space separated) are skipped.
VM_CPU_CAP=${AMUX_CLEANUP_VM_CPU_CAP:-4}
VM_CPU_CAP_EXEMPT=${AMUX_CLEANUP_VM_CPU_CAP_EXEMPT:-}
# A VM process is the sum of its guest's work, so the single-process hog line
# does not fit it (Mac hog RCA 20261020-225151). With every container capped,
# goal-shared read 380-530% while its only busy container sat at its 3-CPU cap:
# granted work plus 90-230% of virtualization overhead, never under 300%. A VM
# escalates as a hog only when its guest escapes the caps: a busy container
# with no cap, or the VM over the busy containers' caps plus VM_HOG_OVERHEAD_PCT.
# A container is busy at VM_BUSY_PCT or more of one CPU.
VM_PROC_NAMES=${AMUX_CLEANUP_VM_PROC_NAMES:-com.apple.Virtualization.VirtualMachine qemu-system-aarch64 qemu-system-x86_64}
VM_HOG_OVERHEAD_PCT=${AMUX_CLEANUP_VM_HOG_OVERHEAD_PCT:-250}
VM_BUSY_PCT=${AMUX_CLEANUP_VM_BUSY_PCT:-25}
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
# check_utilization: who is using the CPU (GET /api/resources), and when the CPU
# is UTIL_ACT_SHARE busy, detached trees at UTIL_RENICE_MIN_PCT+ lowered to nice 10.
UTIL_CMD=${AMUX_CLEANUP_UTIL_CMD:-curl -sk --max-time 20 -H X-Amux-Session:desktop $(amux url 2>/dev/null || echo https://localhost:8824)/api/resources}
UTIL_ACT_SHARE=${AMUX_CLEANUP_UTIL_ACT_SHARE:-0.85}
# The census sums per-process samples, so a job that starts a fresh short-lived
# child every few seconds falls between them. RCA 20261024-170152: a lane's
# video job ran one ffmpeg after another at ~950% each; the census read the CPU
# 71% busy while top read 88% and load15 was 123% of the cores, so the cpu
# constraint escalated and this arm reported the tree without lowering it.
# Busy is now the higher of the census and top's own idle figure.
UTIL_IDLE_CMD=${AMUX_CLEANUP_UTIL_IDLE_CMD:-top -l 2 -s 1 -n 0}
UTIL_RENICE_MIN_PCT=${AMUX_CLEANUP_UTIL_RENICE_MIN_PCT:-100}
UTIL_RENICE_EXCLUDE=${AMUX_CLEANUP_UTIL_RENICE_EXCLUDE:-(^|/)tmux( |$)|gs12}
UTIL_TREE_PS_CMD=${AMUX_CLEANUP_UTIL_TREE_PS_CMD:-ps -axo pid=,ppid=,uid=,nice=,args=}
# Apple's photo and video analysis daemons run at normal priority under the
# user's uid and take whole cores: mediaanalysisd measured 270% at PRI 58 on
# 2026-10-21. Background policy (taskpolicy -b) drops it to PRI 4, where it only
# gets cores nothing else wants, so it costs nothing on an idle Mac and yields
# to workers on a busy one. Root-owned analytics daemons are already at
# background priority and are not ours to change.
DEMOTE_NAMES=${AMUX_CLEANUP_DEMOTE_NAMES:-mediaanalysisd photoanalysisd}
DEMOTE_MIN_PCT=${AMUX_CLEANUP_DEMOTE_MIN_PCT:-50}
DEMOTE_PS_CMD=${AMUX_CLEANUP_DEMOTE_PS_CMD:-ps -Ao pid=,uid=,pri=,pcpu=,comm=}
TASKPOLICY_CMD=${AMUX_CLEANUP_TASKPOLICY_CMD:-/usr/sbin/taskpolicy -b -p}
RENICE_CMD=${AMUX_CLEANUP_RENICE_CMD:-renice}
VM_EVENTS_CMD=${AMUX_CLEANUP_VM_EVENTS_CMD:-docker --context colima-PROFILE events --since MINm --until 0s --format \{\{.Action\}\}}
# Running containers: any one not in VM_IDLE_IGNORE makes the VM busy. A proof
# stack waiting between legs only runs healthchecks, and on 2026-10-04 at
# 18:06Z this tick stopped goal-shared under memory pressure with
# gs12-restore's gr98 stack in it, blocking every lane's local proofs.
VM_PS_CMD=${AMUX_CLEANUP_VM_PS_CMD:-docker --context colima-PROFILE ps --format \{\{.Names\}\}}
# Which container inside a VM writes its disk (the RCA bundle's writers list).
# Prints "S<TAB>name<TAB>blockio" and "I<TAB>/name<TAB>startedAt" lines.
VM_WRITERS_CMD=${AMUX_CLEANUP_VM_WRITERS_CMD:-vm_writers_raw PROFILE}
OPEN_WRITERS_CMD=${AMUX_CLEANUP_OPEN_WRITERS_CMD:-open_writers_raw}
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
# The binary the tmp reapers ask "which directories are a process's cwd" (DESKT-95).
# By absolute path: lsof lives in /usr/sbin, which a shell's PATH can lack, and
# both reapers read an empty answer as "nothing is in use" and deleted in-use
# directories (test-mac-cleanup-user-tmp and -lane-tmp failed exactly so).
LSOF_BIN=${AMUX_CLEANUP_LSOF_BIN:-$(command -v lsof 2>/dev/null || echo /usr/sbin/lsof)}
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
# ...but only when macOS cannot add swap. The pager creates swapfiles on
# demand while the disk has room, so free swap is always a small fraction just
# after it grows: 2026-10-13 (AMUX memory RCA 19:17) swap_total went 10240M ->
# 12288M at 18:48, then read 385MB free with pressure 1 (normal), 83% memory
# free and 61G of disk. Under this much free disk the low fraction is trusted.
SWAP_GROW_DISK_GB=${AMUX_CLEANUP_SWAP_GROW_DISK_GB:-20}
CPU_SHARE=${AMUX_CLEANUP_CPU_SHARE:-0.9}
CPU_IDLE_MAX_PCT=${AMUX_CLEANUP_CPU_IDLE_MAX_PCT:-25}   # high load only counts as CPU when idle is under this
STATE_DIR=${AMUX_CLEANUP_STATE_DIR:-$HOME/.amux/logs/mac-cleanup}
# ONE deadline for the whole tick. The scheduler kills a shell schedule at 600s and
# the per-arm budgets alone add up past that, so under load every arm spent its full
# share and the kill landed before the verdicts: six ticks in a row on 2026-10-14,
# load 133, swap 12 of 13 GB, and the health check said nothing. Each cleanup arm
# gets at most what is left of TICK_BUDGET_S after TICK_RESERVE_S, which is kept for
# the verdicts and escalation; an arm with no time left is skipped and named.
# Under TICK_WARN_S too (RCA self 20261016-012157): at 540 the deadline sat above
# the 480s warning, so a run that kept every budget could still cross it (497s on
# 2026-10-16 at load 36/28) and escalate itself. The clamp below enforces it.
TICK_BUDGET_S=${AMUX_CLEANUP_TICK_BUDGET_S:-450}
TICK_RESERVE_S=${AMUX_CLEANUP_TICK_RESERVE_S:-120}
TICK_SKIPPED=""
# The budgeted arms in the order they run, each with a weight (Mac self RCA
# 20261020-005306). Capping an arm at "what the tick has left" let the first
# arms take everything under load: whichever arm ran LAST got nothing. Before
# 60cb904b1 that was claude tmp; after it, worktrees, skipped by 6 of the next 7
# ticks while needing 3-13s. tick_arm now caps an arm in this list at its
# weighted share of the time left, so every later arm keeps its share too. The
# weights are what each arm took on the loaded ticks of 2026-10-19 (cargo 54-95s,
# superseded 56-85s, claude tmp 63-110s, worktrees 3-13s); lane tmp resumes
# from a cursor, so it gets less than the 90s it will always spend. The declared
# budgets are ceilings and add up to ~800s, so they cannot be the weights.
TICK_ARM_PLAN=${AMUX_CLEANUP_TICK_ARM_PLAN:-"cargo_targets:75 superseded_artifacts:75 claude_tmp:100 lane_tmp:30 worktrees:20"}
TICK_ARM_SHARE=0     # the running arm's cap, for an arm that sizes its own phases

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
# The quota pass gets its own share of that budget. Before 2026-10-12 the
# session pass spent all 150s on 92 sessions (53 never reached), so the quota
# pass reached mxp-gs12's 391G with no time left and judged nothing (AMUX-5904).
CLAUDE_TMP_QUOTA_BUDGET_S=${AMUX_CLEANUP_CLAUDE_TMP_QUOTA_BUDGET_S:-90}
# A whole session or a container folder that cannot be sized in this long is big
# by definition; deciding that with a full 45s walk each spent the quota pass's
# whole budget before it reached a single copy.
CLAUDE_TMP_PROBE_S=${AMUX_CLEANUP_CLAUDE_TMP_PROBE_S:-10}
# Not the desktop lane: it is ISOLATED, and amux refuses automated sends into an
# isolated worker by design (round 1 of DESKT-57 measured exactly that refusal).
# mac-ops is a non-isolated sonnet worker whose brief is the runbook below.
ESCALATE_TO=${AMUX_CLEANUP_ESCALATE_TO:-mac-ops}
ESCALATE_COOLDOWN_H=${AMUX_CLEANUP_ESCALATE_COOLDOWN_H:-6}
# Seam: FILE is replaced with the message path. The test points this at a recorder.
ESCALATE_CMD=${AMUX_CLEANUP_ESCALATE_CMD:-amux send TARGET --file FILE}
# A disk constraint STARTS the scheduled reclaim (SCHED-783, scripts/disk-reclaim-tick.sh)
# instead of only reporting it. 2026-10-20 the tick logged "disk free 90.2G is
# under the 150G floor — escalation suppressed" hour after hour and changed
# nothing. The reclaim runs on its own scheduler slot, so this tick's 600s budget
# is not spent on it; it tightens its own idle and retention rules when the disk
# is low. One start per RECLAIM_TRIGGER_COOLDOWN_H.
RECLAIM_SCHEDULE=${AMUX_CLEANUP_RECLAIM_SCHEDULE:-SCHED-783}
reclaim_trigger_default() { # prints the HTTP status of the run request
  curl -sk --max-time 20 -o /dev/null -w '%{http_code}' -X POST -H "X-Amux-Session: desktop" \
    "$(amux url 2>/dev/null || echo https://localhost:8824)/api/schedules/$RECLAIM_SCHEDULE/run"
}
RECLAIM_TRIGGER_CMD=${AMUX_CLEANUP_RECLAIM_TRIGGER_CMD:-reclaim_trigger_default}
RECLAIM_TRIGGER_COOLDOWN_H=${AMUX_CLEANUP_RECLAIM_TRIGGER_COOLDOWN_H:-3}
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
if [ "$TICK_BUDGET_S" -ge "$TICK_WARN_S" ]; then
  echo "mac-cleanup: tick budget ${TICK_BUDGET_S}s is not under the ${TICK_WARN_S}s warning; using $(( TICK_WARN_S - 30 ))s (verdict=tick_budget_clamped)"
  TICK_BUDGET_S=$(( TICK_WARN_S - 30 ))
fi
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
# The parent of a candidate shell. Only an orphan (parent 1: its Claude session
# has ended) is stale; a shell whose claude process is still its parent is a
# live background task, however long it has run.
STALE_PPID_CMD=${AMUX_CLEANUP_STALE_PPID_CMD:-ps -o ppid= -p}
# Orphaned `sqlite3 <amux db> .backup` copies (DESKT-94). SQLite's online backup
# restarts whenever another connection writes the source, so a .backup of the
# live amux.db never finishes (AMUX-3491 measured it; VACUUM INTO is the fix).
# When the lane's shell times out the sqlite3 child is reparented to launchd and
# spins on: on 2026-10-10 one ran 2h52m at 96% CPU holding a 5G partial copy.
# Only an ORPHAN (parent pid 1) is stopped: nobody is waiting on it, and it only
# reads the database. One whose caller is alive is reported, never killed.
SQLITE_ORPHAN_MIN=${AMUX_CLEANUP_SQLITE_ORPHAN_MIN:-20}
SQLITE_PS_CMD=${AMUX_CLEANUP_SQLITE_PS_CMD:-ps -Ao pid=,ppid=,etime=,args=}
SQLITE_DB_ROOT=${AMUX_CLEANUP_SQLITE_DB_ROOT:-${AMUX_HOME:-$HOME/.amux}}
# ── system health beyond disk and memory (2026-10-13) ─────────────────────────
# The tick grew up around disk and memory. Ethan asked for a general health check
# with the power to fix what it safely can. Each probe below is cheap (the whole
# section stays under ~15s) and goes through a seam. AMUX_CLEANUP_HEALTH_PROBES=off
# turns every live probe off at once, which is what the suites that run the
# whole tick use so a test can never stop a real process.
HEALTH_PROBES=${AMUX_CLEANUP_HEALTH_PROBES:-on}
# CPU: a process at or over CPU_HOG_PCT on two ticks in a row is a hog. A hog on
# the restart list (user-owned macOS helpers that launchd starts again on demand)
# is sent TERM; ReportCrash sat at 98% for four days on 2026-10-12. Anything else
# is reported, and escalated once over CPU_HOG_ESCALATE_PCT (launchservicesd ran
# at ~1450% on 2026-10-08 with nobody told).
CPU_HOG_PCT=${AMUX_CLEANUP_CPU_HOG_PCT:-80}
CPU_HOG_ESCALATE_PCT=${AMUX_CLEANUP_CPU_HOG_ESCALATE_PCT:-300}
CPU_RESTART_NAMES=${AMUX_CLEANUP_CPU_RESTART_NAMES:-ReportCrash ecosystemanalyticsd mediaanalysisd photoanalysisd mdworker_shared spotlightknowledged}
HEALTH_PS_CMD=${AMUX_CLEANUP_HEALTH_PS_CMD:-ps -Ao pid=,ppid=,uid=,pcpu=,etime=,comm=}
# Leftover test helpers: a process whose command runs out of a pytest temp dir,
# whose parent has exited (ppid 1), and that is older than TEST_ORPHAN_MIN. On
# 2026-10-12, 99 fake kubectl stubs from mixpeek tests had sat 4-22h this way.
TEST_ORPHAN_MIN=${AMUX_CLEANUP_TEST_ORPHAN_MIN:-120}
ORPHAN_PS_CMD=${AMUX_CLEANUP_ORPHAN_PS_CMD:-ps -Ao pid=,ppid=,uid=,etime=,args=}
# Python multiprocessing workers whose parent has exited (ppid 1) and that are
# older than MP_ORPHAN_MIN. A spawn worker reports only to its parent, so with
# the parent gone it can never deliver a result; it only holds memory. Mac
# memory RCA 20261019-205313: four pgimp workers sat 5 days this way, ~6.6G.
MP_ORPHAN_MIN=${AMUX_CLEANUP_MP_ORPHAN_MIN:-360}
# Kernel limits: open files, this user's processes, inodes, zombies. Over
# LIMITS_PCT of a limit is a constraint and escalates; zombies are reported.
LIMITS_PCT=${AMUX_CLEANUP_LIMITS_PCT:-85}
# One parent holding this many zombies is not reaping its children, which is a
# bug in that parent: on 2026-10-13 amux-server-rs held 590 after 4.8 days.
ZOMBIE_PARENT_N=${AMUX_CLEANUP_ZOMBIE_PARENT_N:-200}
LIMITS_CMD=${AMUX_CLEANUP_LIMITS_CMD:-limits_reading}
# Crash reports: an app that crashed CRASH_LOOP_N times in 24h is crash-looping
# and escalates. Reports older than CRASH_KEEP_DAYS are pruned (they are the
# user's own diagnostic copies; macOS keeps its own submitted ones elsewhere).
CRASH_DIR=${AMUX_CLEANUP_CRASH_DIR:-$HOME/Library/Logs/DiagnosticReports}
# The escalation state, read by the crash verdict to tell a new crash from one already handed over.
CRASH_STATE=${AMUX_CLEANUP_CRASH_STATE:-$STATE_DIR/state}
CRASH_LOOP_N=${AMUX_CLEANUP_CRASH_LOOP_N:-10}
CRASH_KEEP_DAYS=${AMUX_CLEANUP_CRASH_KEEP_DAYS:-30}
# A loop with no crash in the last CRASH_RECENT_MIN has stopped: it is reported,
# not escalated. Over the whole 24h a fixed loop kept escalating every 6h until
# its reports aged out (RCA crashes 20261017-022150: git x30, the last at 16:57
# the day before, none after the 19:50 fix). Same 6h as the escalation cooldown.
CRASH_RECENT_MIN=${AMUX_CLEANUP_CRASH_RECENT_MIN:-360}
# A process macOS relaunches over and over: launchd logs every short-lived one
# it reaps. ReportCrash writes about 25 reports per burst and then stops, so a
# crash handler relaunched once a second for 13 hours (Muse's, 2026-10-21, about
# 47,000 launches) read as three small bursts and once as "the loop has stopped"
# (Mac crashes RCA 20261021-235935). This counts launches, not reports.
RESPAWN_CMD=${AMUX_CLEANUP_RESPAWN_CMD:-respawn_reading}
RESPAWN_WINDOW_MIN=${AMUX_CLEANUP_RESPAWN_WINDOW_MIN:-5}
RESPAWN_N=${AMUX_CLEANUP_RESPAWN_N:-100}
RESPAWN_REMIND_H=${AMUX_CLEANUP_RESPAWN_REMIND_H:-24}
RESPAWN_PID_CMD=${AMUX_CLEANUP_RESPAWN_PID_CMD:-pgrep -xo}
# Report-only probes: launchd jobs outside com.apple that last exited non-zero
# and are not running, the default gateway's packet loss with DNS lookup time,
# and macOS thermal/CPU-speed limits.
LAUNCHD_LIST_CMD=${AMUX_CLEANUP_LAUNCHD_LIST_CMD:-launchctl list}
NET_CMD=${AMUX_CLEANUP_NET_CMD:-net_reading}
THERMAL_CMD=${AMUX_CLEANUP_THERMAL_CMD:-pmset -g therm}
# The check reads its OWN record each tick: the scheduler's run rows, the full
# run logs it keeps, and the launchd fallback's log. A check that only reports
# the machine says nothing on the day it is the thing that broke (2026-10-14:
# six runs in a row killed at 600s, and nothing anywhere said so).
# The agent-scratch volume fseventsd and Spotlight skip (scripts/mac-agent-volume.sh).
# Reported each tick beside the fseventsd reading, so a reader sees whether the
# remedy is in place without running anything. Committed bytes, as for the tick.
AGENT_VOLUME_CMD=${AMUX_CLEANUP_AGENT_VOLUME_CMD:-agent_volume_status}
SELF_SCHED_ID=${AMUX_CLEANUP_SELF_SCHED_ID:-SCHED-465}
SELF_DB=${AMUX_CLEANUP_SELF_DB:-${AMUX_HOME:-$HOME/.amux}/amux.db}
SELF_RUNS_DIR=${AMUX_CLEANUP_SELF_RUNS_DIR:-${AMUX_HOME:-$HOME/.amux}/shell-runs/$SELF_SCHED_ID}
SELF_FALLBACK_LOG=${AMUX_CLEANUP_SELF_FALLBACK_LOG:-$HOME/.amux/logs/mac-cleanup-fallback.log}
SELF_FAIL_STREAK=${AMUX_CLEANUP_SELF_FAIL_STREAK:-2}   # runs in a row not ok
SELF_FAIL_24H=${AMUX_CLEANUP_SELF_FAIL_24H:-4}         # runs not ok in 24h
SELF_SKIP_24H=${AMUX_CLEANUP_SELF_SKIP_24H:-3}         # runs that skipped arms for lack of time
SELF_FALLBACK_24H=${AMUX_CLEANUP_SELF_FALLBACK_24H:-3} # times launchd had to run it instead
# The speed rule judges only recent runs. Over the whole 24h, one slow run kept
# escalating for a day after the fix that sped the tick up (RCA self
# 20261016-082126: a 497s run from 09:25 the day before, re-escalated at 6h
# cadence while every run after the fix took 402s or less). Same 6h as the
# escalation cooldown, so each escalation is judged on runs the last one did not see.
SELF_SLOW_RECENT_MIN=${AMUX_CLEANUP_SELF_SLOW_RECENT_MIN:-360}
SELF_COLLIDE_24H=${AMUX_CLEANUP_SELF_COLLIDE_24H:-2}   # fires that found another tick running

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

# Restart fseventsd when it holds over FSEVENTSD_RESTART_GB. Prints one verdict
# line and returns 0 only when the restart ran. Sets FSE_RESTART_NEEDS_SUDOERS
# when sudo refused, so the escalation can carry the one line that fixes it.
fseventsd_try_restart() { # <gb> <dry:0|1>
  FSE_RESTART_NEEDS_SUDOERS=0
  [ "${FSEVENTSD_RESTART_GB:-0}" != 0 ] || return 1
  needs_reboot "$1" "$FSEVENTSD_RESTART_GB" || return 1
  local last now; now=$(date +%s); last=$(state_get "$STATE_DIR/state" fseventsd_restarted_at || true)
  if [ -n "$last" ] && [ $(( now - last )) -lt $(( FSEVENTSD_RESTART_COOLDOWN_H * 3600 )) ]; then
    echo "mac-cleanup:   fseventsd ${1}G: restarted $(( (now - last) / 60 ))m ago, next restart allowed after ${FSEVENTSD_RESTART_COOLDOWN_H}h (verdict=fseventsd_restart_cooldown)"
    return 1
  fi
  # Suites that run the whole tick turn the live probes off so a test can never
  # stop a real process; the real restart honours that (a test's fake runs).
  if [ "${HEALTH_PROBES:-on}" = off ] && [ "$FSEVENTSD_RESTART_CMD" = "sudo -n /usr/bin/killall fseventsd" ]; then
    echo "mac-cleanup:   fseventsd ${1}G: live probes are off, the real restart is not run (verdict=fseventsd_restart_probes_off)"
    return 1
  fi
  if [ "${2:-0}" = 1 ]; then
    echo "mac-cleanup:   fseventsd ${1}G is over ${FSEVENTSD_RESTART_GB}G: would restart it (dry run) (verdict=fseventsd_restart_dry)"
    return 1
  fi
  local out rc=0
  out=$($FSEVENTSD_RESTART_CMD 2>&1) || rc=$?
  if [ "$rc" = 0 ]; then
    state_put "$STATE_DIR/state" "fseventsd_restarted_at=$now"
    sleep "${AMUX_CLEANUP_FSEVENTSD_RESTART_SETTLE_S:-3}"
    local after; after=$($FSEVENTSD_CMD 2>/dev/null); after=${after%% *}
    echo "mac-cleanup:   fseventsd held ${1}G (over ${FSEVENTSD_RESTART_GB}G): restarted it, launchd brought it back at ${after:-?}G (verdict=fseventsd_restarted measured=true)"
    return 0
  fi
  if printf '%s' "$out" | grep -qi 'password\|sudo\|not allowed\|a terminal is required'; then
    FSE_RESTART_NEEDS_SUDOERS=1
    echo "mac-cleanup:   fseventsd ${1}G is over ${FSEVENTSD_RESTART_GB}G but the tick may not restart it: sudo refused. One line from the owner lets it (sudo visudo -f /etc/sudoers.d/amux-fseventsd): $FSEVENTSD_SUDOERS_LINE (verdict=fseventsd_restart_needs_sudoers)"
  else
    echo "mac-cleanup:   fseventsd ${1}G: restart failed rc=$rc: ${out:0:200} (verdict=fseventsd_restart_failed)"
  fi
  return 1
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
  local roots=$1 depth=$2 budget=$3 deadline left rc tag d r tmp rdepth slice todo=0
  local -a rootarr
  TARGET_SCAN_COMPLETE=yes
  TARGET_ROOTS_SCANNED=0
  deadline=$(( $(date +%s) + budget ))
  IFS=':' read -r -a rootarr <<< "$roots"
  # ${arr[@]+"${arr[@]}"}: bash 3.2 (macOS /bin/bash) treats an EMPTY array as unset under
  # `set -u`, so a bare "${arr[@]}" aborts the tick when the list is empty.
  # Each root gets an equal share of what is left, so one slow root cannot spend the
  # whole budget and leave every root after it unscanned (2026-10-12: 5 of 8 reached,
  # tick after tick, always the same 5). Time a fast root does not use goes to the rest.
  for r in ${rootarr[@]+"${rootarr[@]}"}; do r=${r%@*}; if [ -n "$r" ] && [ -d "$r" ]; then todo=$((todo+1)); fi; done
  for r in ${rootarr[@]+"${rootarr[@]}"}; do
    rdepth=$depth
    case "$r" in *@[0-9]*) rdepth=${r##*@}; r=${r%@*} ;; esac
    [ -n "$r" ] && [ -d "$r" ] || continue
    left=$(( deadline - $(date +%s) ))
    slice=$(( left / todo )); todo=$((todo-1))
    if [ "$slice" -le 0 ]; then TARGET_SCAN_COMPLETE=no; continue; fi
    left=$slice
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
churn_fswatch() { # <seconds>: every path FSEvents reports under CHURN_SAMPLE_ROOTS
  local -a roots
  IFS=':' read -r -a roots <<< "$CHURN_SAMPLE_ROOTS"
  # Stopped with SIGINT, not SIGALRM: fswatch writes through a block buffer when
  # its output is not a terminal, and only a clean exit flushes it, so an alarm
  # dropped the tail of every sample.
  "$CHURN_FSWATCH" -r -m fsevents_monitor ${roots[@]+"${roots[@]}"} 2>/dev/null &
  local p=$!
  sleep "$1"; kill -INT "$p" 2>/dev/null; wait "$p" 2>/dev/null || true
}

# Print "<count> <dir>" for the directories with at least <min_events> FSEvents in
# a <seconds> sample, grouped by the changed path's parent, busiest first, at most
# three. Same contract as churn_census; a sample always covers every root, so it
# sets CHURN_COMPLETE=yes. With the control on it also sets CHURN_DELIVERED to
# yes or no: whether fseventsd delivered any of the control files written during
# the sample. no means the empty or short list is fseventsd's silence, not a
# quiet disk. The control's own events are not counted.
churn_sample() { # <seconds> <min_events>
  CHURN_COMPLETE=yes; CHURN_DELIVERED=unknown
  local ctl="" w="" raw
  if [ "$CHURN_CONTROL" = 1 ] || { [ "$CHURN_CONTROL" = auto ] && [ "$CHURN_SAMPLE_CMD" = churn_fswatch ]; }; then
    ctl="$STATE_DIR/fsevents-control"; mkdir -p "$ctl" 2>/dev/null || ctl=""
  fi
  raw=$(mktemp "${TMPDIR:-/tmp}/churn-raw.XXXXXX")
  if [ -n "$ctl" ]; then
    ( sleep 2; i=0; while [ "$i" -lt "$CHURN_CONTROL_N" ]; do echo "$i" > "$ctl/c$i"; i=$((i+1)); done ) &
    w=$!
  fi
  CHURN_CONTROL_DIR="$ctl" $CHURN_SAMPLE_CMD "$1" > "$raw" 2>/dev/null
  [ -n "$w" ] && wait "$w" 2>/dev/null
  if [ -n "$ctl" ]; then
    if grep -qF "$ctl/" "$raw"; then CHURN_DELIVERED=yes; else CHURN_DELIVERED=no; fi
    rm -rf -- "${ctl:?}"
  fi
  { grep -vF "${ctl:-/nonexistent-churn-control}/" "$raw" || true; } \
    | awk '{ sub(/\/[^\/]*$/, ""); if ($0 != "") c[$0]++ } END { for (k in c) print c[k], k }' \
    | awk -v m="$2" '$1 >= m' | sort -rn | head -3
  rm -f -- "$raw"
}

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

# Stop orphaned `sqlite3 <amux db> .backup` copies (DESKT-94; knobs above).
# Sets sqlite_found / sqlite_stopped / sqlite_live.
reap_orphan_sqlite_backups() { # <dry:0|1>
  local DRY=${1:-0} line spid sppid setime sargs ss
  sqlite_found=0; sqlite_stopped=0; sqlite_live=0
  while IFS= read -r line; do
    [ -n "$line" ] || continue
    # pid ppid etime args...
    read -r spid sppid setime sargs <<< "$line"
    case "$sargs" in sqlite3\ *) ;; *) continue ;; esac
    case "$sargs" in *.backup*) ;; *) continue ;; esac
    case "$sargs" in *"$SQLITE_DB_ROOT/"*) ;; *) continue ;; esac
    ss=$(etime_secs "$setime")
    case "$ss" in ''|*[!0-9]*) continue ;; esac
    [ "$ss" -ge $(( SQLITE_ORPHAN_MIN * 60 )) ] || continue
    sqlite_found=$((sqlite_found+1))
    if [ "$sppid" != 1 ]; then
      sqlite_live=$((sqlite_live+1))
      echo "mac-cleanup: sqlite .backup pid=$spid age=$setime has a live caller (ppid $sppid), left alone: $sargs"
      continue
    fi
    if [ "$DRY" = "1" ]; then
      echo "mac-cleanup: orphaned sqlite .backup pid=$spid age=$setime would be stopped (dry run): $sargs"
      continue
    fi
    kill -TERM "$spid" 2>/dev/null || true
    sleep 2
    kill -0 "$spid" 2>/dev/null && kill -KILL "$spid" 2>/dev/null
    if kill -0 "$spid" 2>/dev/null; then
      echo "mac-cleanup: FAILED to stop orphaned sqlite .backup pid=$spid"
    else
      sqlite_stopped=$((sqlite_stopped+1))
      mkdir -p "$STATE_DIR" 2>/dev/null
      printf '%s\tstopped\t%s\t%s\t%s\n' "$(date '+%F %T')" "$spid" "$setime" "$sargs" >> "$STATE_DIR/sqlite-orphans.log"
      echo "mac-cleanup: stopped orphaned sqlite .backup pid=$spid age=$setime (a .backup of a live db never finishes; use VACUUM INTO): $sargs"
    fi
  done < <($SQLITE_PS_CMD 2>/dev/null)   # not a heredoc: this body is indented, and an indented EOF never closes one
  echo "mac-cleanup: sqlite .backup: found=${sqlite_found} stopped=${sqlite_stopped} live_caller=${sqlite_live} (floor ${SQLITE_ORPHAN_MIN}m)"
}

# ── system health: CPU, leftover processes, limits, crashes, launchd, net ─────
# Every function prints its own "mac-cleanup: ..." line and appends any
# constraint to HEALTH_VERDICTS (one per line, first word = escalation class).

# May this arm still run? Shrinks each named budget variable to what the tick has
# left before its verdict reserve, or skips the arm, says so, and records it.
# Where the tick's time went: each tick_arm call (and the verdict markers) closes
# the previous section, and the done line names the slowest, so a slow run says
# which part was slow instead of only its total.
TICK_SECTION="" TICK_SECTION_T0=0 TICK_SLOWEST="" TICK_SLOWEST_S=-1
tick_mark() { # <label> (or "end")
  if [ -n "$TICK_SECTION" ]; then
    local d=$(( SECONDS - TICK_SECTION_T0 ))
    if [ "$d" -gt "$TICK_SLOWEST_S" ]; then TICK_SLOWEST_S=$d; TICK_SLOWEST="${TICK_SECTION// /_}:${d}s"; fi
    [ "$d" -gt 0 ] && TICK_SECTIONS="${TICK_SECTIONS:+$TICK_SECTIONS,}${TICK_SECTION// /_}:${d}"
  fi
  TICK_SECTION=$1; TICK_SECTION_T0=$SECONDS
  [ "$1" = end ] && TICK_SECTION=""
  return 0
}

tick_arm() { # <label> [budget var]...
  local label=$1 key v left w=0 rest=0 seen=0 e sum=0 share; shift
  key=${label// /_}
  tick_mark "$label"
  left=$(( TICK_BUDGET_S - TICK_RESERVE_S - SECONDS ))
  if [ "$left" -le 0 ]; then
    echo "mac-cleanup: $label skipped: the tick has used ${SECONDS}s of its ${TICK_BUDGET_S}s budget and keeps ${TICK_RESERVE_S}s for the verdicts (verdict=tick_out_of_time)"
    TICK_SKIPPED="${TICK_SKIPPED:+$TICK_SKIPPED,}$key"
    return 1
  fi
  for e in $TICK_ARM_PLAN; do
    if [ "${e%%:*}" = "$key" ]; then seen=1; w=${e#*:}; fi
    [ "$seen" = 1 ] && rest=$(( rest + ${e#*:} ))
  done
  share=$left
  if [ "$w" -gt 0 ] && [ "$rest" -gt "$w" ]; then
    share=$(( left * w / rest )); [ "$share" -ge 1 ] || share=1
  fi
  TICK_ARM_SHARE=$share
  for v in "$@"; do sum=$(( sum + ${!v} )); done
  if [ "$share" -lt "$left" ] && [ "$sum" -gt "$share" ]; then
    # Phases run one after another, so they split the share in proportion.
    for v in "$@"; do printf -v "$v" '%s' "$(( ${!v} * share / sum > 0 ? ${!v} * share / sum : 1 ))"; done
    echo "mac-cleanup: $label capped at ${share}s of the ${left}s left, keeping the rest for the arms after it (verdict=tick_arm_share)"
  else
    for v in "$@"; do if [ "${!v}" -gt "$left" ]; then printf -v "$v" '%s' "$left"; fi; done
  fi
  return 0
}

in_list() { # <word> <space-separated list>
  case " $2 " in *" $1 "*) return 0 ;; esac; return 1
}

# What the running colima VMs' containers may use: "budget=<pct> uncapped=<n>
# busy=<name>:<cpu%>/<cap>,..." over containers at VM_BUSY_PCT or more. budget
# is the sum of the busy containers' caps; a busy uncapped container counts in
# uncapped. "unmeasured" when no VM answered, which never clears a hog.
vm_cpu_budget() {
  local p profiles out="" ctx line
  profiles=$(running_vm_profiles)
  [ -n "$profiles" ] || { echo "unmeasured: no running colima VM"; return 0; }
  while IFS= read -r p; do
    [ -n "$p" ] || continue
    ctx="colima-$p"
    line=$(perl -e 'alarm shift; exec @ARGV' "$VM_STEP_S" "$VM_DOCKER" --context "$ctx" stats --no-stream --format '{{.Name}} {{.CPUPerc}}' 2>/dev/null) || true
    [ -n "$line" ] || continue
    while read -r name cpu; do
      [ -n "$name" ] || continue
      out+="$name ${cpu%\%} $("$VM_DOCKER" --context "$ctx" inspect -f '{{.HostConfig.NanoCpus}}' "$name" 2>/dev/null || echo 0)"$'\n'
    done <<< "$line"
  done <<< "$profiles"
  [ -n "$out" ] || { echo "unmeasured: docker stats answered for no VM"; return 0; }
  printf '%s' "$out" | awk -v busy="$VM_BUSY_PCT" '
    $2+0 >= busy+0 { cap=$3/1e9; if (cap > 0) budget += cap*100; else unc++;
      list = list (list ? "," : "") $1 ":" int($2+0.5) "%/" (cap > 0 ? cap : "none") }
    END { printf "budget=%d uncapped=%d busy=%s\n", budget, unc, (list ? list : "none") }'
}
# True when <pcpu> is within <budget line>'s caps plus the overhead allowance.
vm_within_caps() { # <pcpu> <vm_cpu_budget output>
  local budget unc
  budget=$(printf '%s' "$2" | sed -n 's/^budget=\([0-9]*\).*/\1/p')
  unc=$(printf '%s' "$2" | sed -n 's/.* uncapped=\([0-9]*\).*/\1/p')
  [ "${unc:-1}" = 0 ] || return 1   # also "unmeasured", which has no uncapped= field
  awk -v c="$1" -v b="${budget:-0}" -v o="$VM_HOG_OVERHEAD_PCT" 'BEGIN{exit !(c+0 < b+o)}'
}
VM_BUDGET=""

check_cpu_hogs() { # <dry:0|1>
  local DRY=${1:-0} prev="$STATE_DIR/cpu-hogs.prev" cur pid ppid puid pcpu et comm name n=0 sus=0 cpu_restart=0 i
  cur=$(mktemp "${TMPDIR:-/tmp}/cpu-hogs.XXXXXX")
  while read -r pid ppid puid pcpu et comm; do
    [ -n "$comm" ] || continue
    awk -v c="$pcpu" -v t="$CPU_HOG_PCT" 'BEGIN{exit !(c+0 >= t+0)}' || continue
    name=$(basename "$comm")
    printf '%s %s\n' "$pid" "$name" >> "$cur"; n=$((n+1))
    if ! grep -qx "$pid $name" "$prev" 2>/dev/null; then continue; fi
    sus=$((sus+1))
    if [ "$puid" = "$(id -u)" ] && in_list "$name" "$CPU_RESTART_NAMES"; then
      if [ "$DRY" = 1 ]; then echo "mac-cleanup: cpu hog $name pid=$pid at ${pcpu}% for two ticks would be restarted (dry run)"; continue; fi
      kill -TERM "$pid" 2>/dev/null || true
      for i in 1 2 3; do kill -0 "$pid" 2>/dev/null || break; sleep 1; done
      if kill -0 "$pid" 2>/dev/null; then echo "mac-cleanup: FAILED to restart cpu hog $name pid=$pid (still running after TERM)"; continue; fi
      cpu_restart=$((cpu_restart+1)); mkdir -p "$STATE_DIR"
      printf '%s\trestarted\t%s\t%s\t%s%%\n' "$(date '+%F %T')" "$pid" "$name" "$pcpu" >> "$STATE_DIR/cpu-restarts.log"
      echo "mac-cleanup: cpu hog $name pid=$pid at ${pcpu}% for two ticks: restarted (a macOS helper launchd starts again on demand)"
    else
      echo "mac-cleanup: cpu hog $name pid=$pid at ${pcpu}% for two ticks, reported, not touched ($(owner_label_for_pid "$pid"))"
      if awk -v c="$pcpu" -v t="$CPU_HOG_ESCALATE_PCT" 'BEGIN{exit !(c+0 >= t+0)}'; then
        local guest=""
        if in_list "$name" "$VM_PROC_NAMES"; then
          [ -n "$VM_BUDGET" ] || VM_BUDGET=$(vm_cpu_budget)
          guest=" (guest: $VM_BUDGET)"
          if vm_within_caps "$pcpu" "$VM_BUDGET"; then
            echo "mac-cleanup: cpu hog $name pid=$pid at ${pcpu}% is its guest's capped work, not escalated: $VM_BUDGET, line = budget + ${VM_HOG_OVERHEAD_PCT}% overhead (verdict=vm_hog_within_caps)"
            continue
          fi
        fi
        HEALTH_VERDICTS=$(printf '%s\nhog %s pid %s at %s%% CPU on two ticks in a row (over %s%%)%s\n' "$HEALTH_VERDICTS" "$name" "$pid" "$pcpu" "$CPU_HOG_ESCALATE_PCT" "$guest")
      fi
    fi
  done < <($HEALTH_PS_CMD 2>/dev/null)
  if [ "$DRY" != 1 ]; then mkdir -p "$STATE_DIR" && mv -f "$cur" "$prev"; else rm -f -- "${cur:?}"; fi
  CPU_RESTARTED=$cpu_restart
  echo "mac-cleanup: cpu hogs: ${n} at or over ${CPU_HOG_PCT}% now, ${sus} also last tick, restarted ${cpu_restart}"
}

reap_test_orphans() { # <dry:0|1>
  local DRY=${1:-0} pid ppid puid et args s found=0 stopped=0 ex=""
  while read -r pid ppid puid et args; do
    [ "$ppid" = 1 ] && [ "$puid" = "$(id -u)" ] || continue
    # A cargo test whose caller exited is the same leftover as a pytest one, and
    # costs more: 2026-10-21 one sat 10h50m at 0% CPU holding the builder's cargo
    # lease (cargo_reclaim_deferred streak 103, wedged=true), so the shared 62G
    # target could not be reclaimed. Its test binary is a separate ppid-1 process
    # once cargo dies, and matches the deps/ arm on the next tick.
    case "$args" in
      *"/pytest-of-"*) ;;
      *"cargo test"*|*"cargo nextest"*|*/target/debug/deps/*|*/target/release/deps/*) ;;
      # A browser an amux server launched for a THROWAWAY home (a test or probe
      # server under a temp amux-*/home). Browsers outlive their server on
      # purpose (9ef5d12fe) so a restart can adopt them, but a temp home's
      # server never comes back. CPU RCA 20261027-122513: four Chromes from a
      # failed reaper probe sat 14.5h, one spawning renderers at ~140% CPU.
      # The owner's real profiles live under ~/.amux/playwright-auth and have
      # no /home/ component, so they cannot match.
      *"--user-data-dir="*"/amux-"*"/home/playwright-auth/profiles/"*) ;;
      *) continue ;;
    esac
    s=$(etime_secs "$et"); case "$s" in ''|*[!0-9]*) continue ;; esac
    [ "$s" -ge $(( TEST_ORPHAN_MIN * 60 )) ] || continue
    found=$((found+1))
    [ "$found" -le 3 ] && ex="${ex}${ex:+; }pid $pid age $et: ...${args: -110}"
    if [ "$DRY" = 1 ]; then continue; fi
    kill -TERM "$pid" 2>/dev/null || true
    sleep 0.2; kill -0 "$pid" 2>/dev/null && { sleep 1; kill -KILL "$pid" 2>/dev/null; }
    if kill -0 "$pid" 2>/dev/null; then echo "mac-cleanup: FAILED to stop leftover test process pid=$pid"; continue; fi
    stopped=$((stopped+1)); mkdir -p "$STATE_DIR"
    printf '%s\tstopped\t%s\t%s\t%s\n' "$(date '+%F %T')" "$pid" "$et" "${args:0:300}" >> "$STATE_DIR/test-orphans.log"
  done < <($ORPHAN_PS_CMD 2>/dev/null)
  TEST_ORPHANS_STOPPED=$stopped
  echo "mac-cleanup: leftover test processes (ppid 1, pytest temp dir, cargo test / test binary or a temp-home browser, over ${TEST_ORPHAN_MIN}m): found ${found}, stopped ${stopped}$( [ "$DRY" = 1 ] && [ "$found" -gt 0 ] && echo " (dry run)")"
  [ -z "$ex" ] || echo "mac-cleanup:   e.g. $ex (full list in $STATE_DIR/test-orphans.log)"
}

reap_orphan_mp_workers() { # <dry:0|1>
  local DRY=${1:-0} pid ppid puid et args sec found=0 stopped=0
  while read -r pid ppid puid et args; do
    [ "$ppid" = 1 ] && [ "$puid" = "$(id -u)" ] || continue
    case "$args" in *"from multiprocessing.spawn import spawn_main"*"--multiprocessing-fork"*) ;; *) continue ;; esac
    sec=$(etime_secs "$et"); case "$sec" in ''|*[!0-9]*) continue ;; esac
    [ "$sec" -ge $(( MP_ORPHAN_MIN * 60 )) ] || continue
    found=$((found+1))
    echo "mac-cleanup:   orphaned multiprocessing worker pid $pid age $et, lane $(lane_for_pid "$pid")"
    [ "$DRY" = 1 ] && continue
    [ "$(ps -o args= -p "$pid" 2>/dev/null)" = "$args" ] || continue   # still the same process
    kill -TERM "$pid" 2>/dev/null || true
    sleep 0.3; kill -0 "$pid" 2>/dev/null && { sleep 1; kill -KILL "$pid" 2>/dev/null; }
    if kill -0 "$pid" 2>/dev/null; then echo "mac-cleanup: FAILED to stop orphaned multiprocessing worker pid=$pid"; continue; fi
    stopped=$((stopped+1)); mkdir -p "$STATE_DIR"
    printf '%s\tstopped\t%s\t%s\t%s\n' "$(date '+%F %T')" "$pid" "$et" "${args:0:300}" >> "$STATE_DIR/mp-orphans.log"
  done < <($ORPHAN_PS_CMD 2>/dev/null)
  MP_ORPHANS_STOPPED=$stopped
  echo "mac-cleanup: orphaned multiprocessing workers (ppid 1, over ${MP_ORPHAN_MIN}m): found ${found}, stopped ${stopped}$( [ "$DRY" = 1 ] && [ "$found" -gt 0 ] && echo " (dry run)") (verdict=mp_orphans)"
}

# An app's crash handler left behind by an update: parent gone (ppid 1) and its
# own executable no longer on disk. It keeps forking children that exec that
# missing path and die before exec (SIGTRAP), one crash report each. Mac crashes
# RCA 20261019-195257: ChatGPT's v154 browser_crashpad_handler, orphaned since
# 10-08 after the app moved to v155, crash-looped 8 times in 3 minutes. Stopping
# it loses nothing: the app starts a fresh handler from its current version.
reap_orphan_crash_handlers() { # <dry:0|1>
  local DRY=${1:-0} pid ppid puid et args exe comm found=0 stopped=0
  while read -r pid ppid puid et args; do
    [ "$ppid" = 1 ] && [ "$puid" = "$(id -u)" ] || continue
    case "$args" in *[Cc]rashpad*[Hh]andler*) ;; *) continue ;; esac
    # The executable is the argv prefix whose basename is a crash handler's
    # name, and the kernel's own short name for the process (ps -c: the
    # executable's basename, cut to 15 characters) must begin that basename.
    # So a script or a tool that merely names a handler in its args is never
    # matched, and neither is any other process whose binary an update deleted.
    exe=$(printf '%s' "$args" | sed -nE 's#^(/.*/[A-Za-z0-9_.-]*([Cc]rashpad_[Hh]andler|CrashpadHandler))( .*)?$#\1#p')
    [ -n "$exe" ] || continue
    comm=$(ps -c -o comm= -p "$pid" 2>/dev/null)
    case "${exe##*/}" in "${comm:-//}"*) ;; *) continue ;; esac   # "//" never begins a basename: no name, no match
    [ ! -e "$exe" ] || continue
    found=$((found+1))
    echo "mac-cleanup:   orphaned crash handler pid $pid age $et: its executable is gone ($exe)"
    [ "$DRY" = 1 ] && continue
    # The pid must still be this process: re-read its args right before the signal.
    [ "$(ps -o args= -p "$pid" 2>/dev/null)" = "$args" ] || continue
    kill -TERM "$pid" 2>/dev/null || true
    sleep 0.3; kill -0 "$pid" 2>/dev/null && { sleep 1; kill -KILL "$pid" 2>/dev/null; }
    if kill -0 "$pid" 2>/dev/null; then echo "mac-cleanup: FAILED to stop orphaned crash handler pid=$pid"; continue; fi
    stopped=$((stopped+1))
  done < <($ORPHAN_PS_CMD 2>/dev/null)
  ORPHAN_HANDLERS_STOPPED=$stopped
  echo "mac-cleanup: orphaned crash handlers (ppid 1, executable deleted by an app update): found ${found}, stopped ${stopped}$( [ "$DRY" = 1 ] && [ "$found" -gt 0 ] && echo " (dry run)") (verdict=orphan_crash_handlers)"
}

# Prints "<open_files> <max_files> <user_procs> <max_user_procs> <inode_pct> <zombies> <top_zombie_ppid> <its_zombies>".
limits_reading() {
  local f mf p mp ip z zp
  f=$(/usr/sbin/sysctl -n kern.num_files 2>/dev/null); mf=$(/usr/sbin/sysctl -n kern.maxfiles 2>/dev/null)
  p=$(ps -U "$(id -u)" -o pid= 2>/dev/null | grep -c .); mp=$(/usr/sbin/sysctl -n kern.maxprocperuid 2>/dev/null)
  ip=$(df -i "$HOME" 2>/dev/null | awk 'NR==2{for(i=1;i<=NF;i++) if($i ~ /%$/) v=$i} END{gsub("%","",v); print v+0}')
  z=$(ps -Ao stat= 2>/dev/null | grep -c '^Z' || true)
  zp=$(ps -Ao ppid=,stat= 2>/dev/null | awk '$2 ~ /^Z/{c[$1]++} END{for(p in c) if(c[p]>m){m=c[p]; t=p} if(m) print t, m}')
  echo "${f:--} ${mf:--} ${p:--} ${mp:--} ${ip:--} ${z:-0} ${zp:-- 0}"
}

check_limits() {
  local f mf p mp ip z zpid zn zname out
  read -r f mf p mp ip z zpid zn <<< "$($LIMITS_CMD 2>/dev/null)"
  if [ -z "${f:-}" ] || [ "$f" = - ] || [ "$mf" = - ]; then echo "mac-cleanup: limits: unmeasured (the limits probe printed nothing)"; return 0; fi
  out=$(awk -v f="$f" -v mf="$mf" -v p="$p" -v mp="$mp" -v ip="$ip" -v lim="$LIMITS_PCT" 'BEGIN{
    if (mf > 0 && f/mf*100 >= lim) printf "limits open files %d is %.0f%% of kern.maxfiles %d (over %d%%)\n", f, f/mf*100, mf, lim
    if (p != "-" && mp > 0 && p/mp*100 >= lim) printf "limits this user runs %d processes, %.0f%% of kern.maxprocperuid %d (over %d%%)\n", p, p/mp*100, mp, lim
    if (ip != "-" && ip+0 >= lim) printf "limits inodes %d%% used on the data volume (over %d%%)\n", ip, lim
  }')
  zname=""
  if [ -n "${zpid:-}" ] && [ "$zpid" != - ] && [ "${zn:-0}" -gt 0 ]; then
    zname=$(ps -o comm= -p "$zpid" 2>/dev/null); zname=$(basename "${zname:-pid $zpid}")
    if [ "$zn" -ge "$ZOMBIE_PARENT_N" ]; then
      out=$(printf '%s\nzombies %s (pid %s) holds %s unreaped children (at or over %s): it is not waiting on what it spawns\n' "$out" "$zname" "$zpid" "$zn" "$ZOMBIE_PARENT_N" | grep . || true)
    fi
  fi
  [ -z "$out" ] || HEALTH_VERDICTS=$(printf '%s\n%s\n' "$HEALTH_VERDICTS" "$out")
  echo "mac-cleanup: limits: open files ${f}/${mf}, user processes ${p}/${mp}, inodes ${ip}% used, zombies ${z}$( [ -n "$zname" ] && echo " (most under $zname pid $zpid: $zn)")$( [ -z "$out" ] && echo ", none at a limit" || echo ", $(printf '%s\n' "$out" | grep -c .) constraint(s)")"
}

# Reports of a fault signal the process sent ITSELF, not a fault: the crashing
# thread called kill() (a __kill frame) with no signal handler on the stack
# (no _sigtramp), for SIGSEGV, SIGBUS, SIGILL or SIGFPE, and macOS says the
# process raised it. Mac crashes RCA 20261026-225543: "bash crashed 14 times"
# was a test fixture, `kill -SEGV $$` in a fake psql that a lane's test suite
# runs (customers/tubescience/dropin/tests/test_tspg_conn_retry.py, "crash"
# mode), once per run, in each of its worktrees. A real fault is
# EXC_BAD_ACCESS, and a handler re-raising one carries _sigtramp, so neither
# is matched. A report without thread frames is NOT matched: no evidence, it
# stays a crash. Prints one report name per line.
crash_simulated() { # <dir>
  python3 - "$1" <<'PY' 2>/dev/null
import json, os, sys, time
d = sys.argv[1]
cut = time.time() - 2 * 86400
for n in os.listdir(d):
    p = os.path.join(d, n)
    if not n.endswith(".ips") or not os.path.isfile(p) or os.stat(p).st_mtime < cut:
        continue
    try:
        b = json.loads(open(p, errors="replace").read().split("\n", 1)[1])
    except Exception:
        continue
    if (b.get("exception") or {}).get("signal") not in ("SIGSEGV", "SIGBUS", "SIGILL", "SIGFPE"):
        continue
    t = b.get("termination") or {}
    if t.get("byPid") is None or t.get("byPid") != b.get("pid"):
        continue
    th = [x for x in b.get("threads") or [] if x.get("triggered")]
    syms = [f.get("symbol") or "" for f in (th[0].get("frames") or [])] if th else []
    if "__kill" in syms and not any("sigtramp" in s for s in syms):
        print(n)
PY
}

# Crash counts per app over the last <minutes>, as "<count> <app>" lines, most
# first. A report's crash time is the EARLIER of the stamp macOS puts in its
# name (-YYYY-MM-DD-HHMMSS, local) and its mtime: macOS rewrites a report long
# after the crash (seen: a 12:02:37 crash rewritten at 19:55), so mtime alone
# kept a loop that ended at 12:05 "live" and re-escalated it 8h later (Mac
# crashes RCA 20261019-195257). A rewrite only moves mtime later, so the
# earlier of the two is never newer than the crash.
crash_counts() { # <dir> <minutes> [app]
  python3 - "$1" "$2" "${3:-}" <<'PY' 2>/dev/null
import collections, os, re, sys, time
SKIP = set(os.environ.get("CRASH_SKIP", "").split())
d, mins, only = sys.argv[1], float(sys.argv[2]), sys.argv[3]
cut = time.time() - mins * 60
c = collections.Counter()
for n in os.listdir(d):
    if n in SKIP:
        continue
    p = os.path.join(d, n)
    if not os.path.isfile(p):
        continue
    app = re.sub(r"[-_][0-9]{4}-[0-9]{2}-[0-9]{2}.*", "", n)
    if only and app != only:
        continue
    t = os.stat(p).st_mtime
    m = re.search(r"[-_]([0-9]{4}-[0-9]{2}-[0-9]{2})-([0-9]{6})", n)
    if m:
        try:
            t = min(t, time.mktime(time.strptime(m.group(1) + m.group(2), "%Y-%m-%d%H%M%S")))
        except ValueError:
            pass
    if t >= cut:
        c[app] += 1
for app, k in c.most_common():
    print(k, app)
PY
}

# What the reports themselves say about who crashed, appended to the crashes
# line. Mac crashes RCA 20261025-060129: "bash crashed 10 times" took an hour
# to place. The .ips files already held it: all 10 were /bin/bash translated
# by Rosetta, under coalition com.amux.tmux-watchdog (the watchdog restarted
# the tmux server on 10-23, so EVERY worker shell carries that coalition and
# runs under Rosetta with the Intel tmux), parent bash, SIGSEGV re-raised by
# the process itself, each alive about 0.2s. Prints "" when no report parses.
crash_context() { # <dir> <app>
  python3 - "$1" "$2" <<'PY' 2>/dev/null
import collections, json, os, re, sys, time
SKIP = set(os.environ.get("CRASH_SKIP", "").split())
d, app = sys.argv[1], sys.argv[2]
cut = time.time() - 86400
coal, par, sig = collections.Counter(), collections.Counter(), collections.Counter()
n = tr = selfk = short = 0
for name in os.listdir(d):
    if name in SKIP:
        continue
    if re.sub(r"[-_][0-9]{4}-[0-9]{2}-[0-9]{2}.*", "", name) != app or not name.endswith(".ips"):
        continue
    p = os.path.join(d, name)
    if os.stat(p).st_mtime < cut:
        continue
    try:
        b = json.loads(open(p, errors="replace").read().split("\n", 1)[1])
    except Exception:
        continue
    n += 1
    coal[b.get("coalitionName") or "?"] += 1
    par[b.get("parentProc") or "?"] += 1
    sig[(b.get("exception") or {}).get("signal") or "?"] += 1
    tr += 1 if b.get("translated") else 0
    t = b.get("termination") or {}
    selfk += 1 if t.get("byPid") is not None and t.get("byPid") == b.get("pid") else 0
    try:
        # mach absolute time ticks at 24MHz on Apple silicon (125/3 ns a tick)
        short += 1 if (b["procExitAbsTime"] - b["procStartAbsTime"]) * 125 / 3 / 1e9 < 1 else 0
    except Exception:
        pass
if n:
    top = lambda c: ", ".join("%s x%d" % kv for kv in c.most_common(2))
    out = ["coalition " + top(coal), "parent " + top(par), "signal " + top(sig), "%d/%d under Rosetta" % (tr, n)]
    if selfk: out.append("%d raised by the process itself" % selfk)
    if short: out.append("%d alive under 1s" % short)
    note = " (the watchdog started the tmux server: this means a process under tmux, a worker's shell)" if coal.most_common(1)[0][0] == "com.amux.tmux-watchdog" else ""
    print("reports: " + "; ".join(out) + note)
PY
}

crash_newest() { # <dir> <app>: epoch of the app's newest crash, dated as crash_counts dates it
  python3 - "$1" "$2" <<'PY' 2>/dev/null
import os, re, sys, time
SKIP = set(os.environ.get("CRASH_SKIP", "").split())
d, only, best = sys.argv[1], sys.argv[2], 0
for n in os.listdir(d):
    if n in SKIP:
        continue
    p = os.path.join(d, n)
    if not os.path.isfile(p) or re.sub(r"[-_][0-9]{4}-[0-9]{2}-[0-9]{2}.*", "", n) != only:
        continue
    t = os.stat(p).st_mtime
    m = re.search(r"[-_]([0-9]{4}-[0-9]{2}-[0-9]{2})-([0-9]{6})", n)
    if m:
        try:
            t = min(t, time.mktime(time.strptime(m.group(1) + m.group(2), "%Y-%m-%d%H%M%S")))
        except ValueError:
            pass
    best = max(best, t)
print(int(best))
PY
}

crash_episode() { # <dir> <app> <gap minutes>: crashes since the app's last quiet gap of at least <gap>
  python3 - "$1" "$2" "$3" <<'PY' 2>/dev/null
import os, re, sys, time
SKIP = set(os.environ.get("CRASH_SKIP", "").split())
d, only, gap = sys.argv[1], sys.argv[2], float(sys.argv[3]) * 60
ts = []
for n in os.listdir(d):
    if n in SKIP:
        continue
    p = os.path.join(d, n)
    if not os.path.isfile(p) or re.sub(r"[-_][0-9]{4}-[0-9]{2}-[0-9]{2}.*", "", n) != only:
        continue
    t = os.stat(p).st_mtime
    m = re.search(r"[-_]([0-9]{4}-[0-9]{2}-[0-9]{2})-([0-9]{6})", n)
    if m:
        try:
            t = min(t, time.mktime(time.strptime(m.group(1) + m.group(2), "%Y-%m-%d%H%M%S")))
        except ValueError:
            pass
    ts.append(t)
ts.sort(reverse=True)
k = 1 if ts else 0
while k < len(ts) and ts[k - 1] - ts[k] < gap:
    k += 1
print(k)
PY
}

crash_since() { # <dir> <app> <epoch>: the app's crashes dated after <epoch>, dated as crash_counts dates them
  python3 - "$1" "$2" "$3" <<'PY' 2>/dev/null
import os, re, sys, time
SKIP = set(os.environ.get("CRASH_SKIP", "").split())
d, only, since, n_after = sys.argv[1], sys.argv[2], float(sys.argv[3]), 0
for n in os.listdir(d):
    if n in SKIP:
        continue
    p = os.path.join(d, n)
    if not os.path.isfile(p) or re.sub(r"[-_][0-9]{4}-[0-9]{2}-[0-9]{2}.*", "", n) != only:
        continue
    t = os.stat(p).st_mtime
    m = re.search(r"[-_]([0-9]{4}-[0-9]{2}-[0-9]{2})-([0-9]{6})", n)
    if m:
        try:
            t = min(t, time.mktime(time.strptime(m.group(1) + m.group(2), "%Y-%m-%d%H%M%S")))
        except ValueError:
            pass
    if t > since:
        n_after += 1
print(n_after)
PY
}

# Prints "<launches> <name>" per process launchd reaped in the window, busiest
# first. The name is launchd's, cut to 15 characters ("EndoCrashpadHan").
respawn_reading() {
  log show --last "${RESPAWN_WINDOW_MIN}m" --style compact \
    --predicate 'process == "launchd" AND eventMessage BEGINSWITH "removing inactive unmanaged service"' 2>/dev/null \
    | sed -nE 's/.*unmanaged service: com\.apple\.xpc\.launchd\.unmanaged\.(.*)\.[0-9]+$/\1/p' | sort | uniq -c | sort -rn
}

crash_responsible() { # <dir> <name prefix>: the app macOS holds responsible in that process's newest crash report
  python3 - "$1" "$2" <<'PY' 2>/dev/null
import json, os, re, sys
d, pre = sys.argv[1], sys.argv[2]
best = None
for n in os.listdir(d):
    if not n.endswith(".ips") or not re.sub(r"[-_][0-9]{4}-[0-9]{2}-[0-9]{2}.*", "", n).startswith(pre):
        continue
    t = os.stat(os.path.join(d, n)).st_mtime
    if best is None or t > best[0]:
        best = (t, n)
if best:
    try:
        body = open(os.path.join(d, best[1]), errors="replace").read().split("\n", 1)[1]
        print(json.loads(body).get("responsibleProc") or "")
    except Exception:
        pass
PY
}

check_respawn_loops() {
  local rows cnt name resp rpid key esc_key esc_at n=0
  RESPAWN_LOOPS=""; RESPAWN_ESC_KEY=""
  export CRASH_SKIP; CRASH_SKIP=$( [ -d "$CRASH_DIR" ] && crash_simulated "$CRASH_DIR" )
  rows=$($RESPAWN_CMD 2>/dev/null || true)
  while read -r cnt name; do
    [ -n "${name:-}" ] && [ "${cnt:-0}" -ge "$RESPAWN_N" ] 2>/dev/null || continue
    # A loop is launches that CRASH. Busy launchers are reaped just as often:
    # lanes start background jobs through nohup, 111 in 5m on 2026-10-22, and
    # that escalated as a crash loop "held by an unknown app" (RCA crashes
    # 20261022-105614). Without a crash report for the name it is not one.
    if ! crash_counts "$CRASH_DIR" 1440 | awk -v p="$name" '{c=$1; $1=""; sub(/^ /,""); if (index($0, p) == 1 && c > 0) f=1} END{exit !f}'; then
      echo "mac-cleanup: respawn check: $name launched $cnt times in ${RESPAWN_WINDOW_MIN}m with no crash report in 24h: a busy launcher, not a crash loop (verdict=respawn_busy_no_crashes)"
      continue
    fi
    n=$((n+1)); RESPAWN_LOOPS="$RESPAWN_LOOPS $name"
    resp=$(crash_responsible "$CRASH_DIR" "$name" || true)
    rpid=""; [ -z "$resp" ] || rpid=$($RESPAWN_PID_CMD "$resp" 2>/dev/null | awk 'NR==1{print $1}' || true)
    key="$name:${resp:-unknown}:${rpid:-none}"
    esc_key=$(state_get "$CRASH_STATE" esc_respawn_key 2>/dev/null || true)
    esc_at=$(state_get "$CRASH_STATE" esc_crashes 2>/dev/null || true); case "$esc_at" in ''|*[!0-9]*) esc_at=0 ;; esac
    if [ "$key" = "$esc_key" ] && [ $(( $(date +%s) - esc_at )) -lt $(( RESPAWN_REMIND_H * 3600 )) ]; then
      echo "mac-cleanup: respawn loop: $name relaunched $cnt times in ${RESPAWN_WINDOW_MIN}m, the loop the last crashes escalation named $(( ($(date +%s) - esc_at) / 60 ))m ago; ${resp:-its app} restarting it is pending; not escalated again until its pid changes or ${RESPAWN_REMIND_H}h (verdict=respawn_loop_owner_pending)"
      continue
    fi
    echo "mac-cleanup: respawn loop: $name relaunched $cnt times in ${RESPAWN_WINDOW_MIN}m, responsible app ${resp:-unknown}${rpid:+ pid $rpid} (verdict=respawn_loop)"
    HEALTH_VERDICTS=$(printf '%s\ncrashes %s relaunched %s times in %sm (over %s), each launch dying at once: a respawn loop held by %s; crash reports undercount it (ReportCrash stops after about 25); the owner quits and reopens %s\n' \
      "$HEALTH_VERDICTS" "$name" "$cnt" "$RESPAWN_WINDOW_MIN" "$RESPAWN_N" "${resp:-an unknown app}${rpid:+ (pid $rpid)}" "${resp:-that app}")
    RESPAWN_ESC_KEY=$key
  done <<< "$rows"
  echo "mac-cleanup: respawn loops (over ${RESPAWN_N} launches in ${RESPAWN_WINDOW_MIN}m): ${n}${RESPAWN_LOOPS:+:$RESPAWN_LOOPS}"
}

check_crash_reports() { # <dry:0|1>
  local DRY=${1:-0} top name cnt pruned=0 old newest esc episode since_esc
  if [ ! -d "$CRASH_DIR" ]; then echo "mac-cleanup: crash reports: no directory at $CRASH_DIR"; return 0; fi
  # A process that sends itself SIGSEGV with kill() did not crash (see
  # crash_simulated): every count below leaves those reports out, and this
  # line says how many and whose, so a test fixture never reads as a crash loop.
  export CRASH_SKIP; CRASH_SKIP=$(crash_simulated "$CRASH_DIR")
  if [ -n "$CRASH_SKIP" ]; then
    echo "mac-cleanup: crash reports: $(printf '%s\n' "$CRASH_SKIP" | grep -c .) in 48h are a fault signal the process sent itself with kill(), not a crash, and are not counted ($(printf '%s\n' "$CRASH_SKIP" | sed -E 's/[-_][0-9]{4}-[0-9]{2}-[0-9]{2}.*//' | sort | uniq -c | sort -rn | awk '{printf "%s%s x%s", (NR>1 ? ", " : ""), $2, $1}')) (verdict=crash_reports_self_signaled)"
  fi
  # The app name is the file name up to the "-YYYY-MM-DD" stamp macOS appends.
  top=$(crash_counts "$CRASH_DIR" 1440 | head -3)
  local recent
  while read -r cnt name; do
    [ -n "${cnt:-}" ] || continue
    local r covered=""
    for r in ${RESPAWN_LOOPS:-}; do case "$name" in "$r"*) covered=$r ;; esac; done
    if [ -n "$covered" ] && [ "$cnt" -ge "$CRASH_LOOP_N" ]; then
      echo "mac-cleanup: crash reports: $name crashed $cnt times in 24h, reports of the live respawn loop $covered (counted above by launches, not again here) (verdict=crash_reports_from_respawn_loop)"
    elif [ "$cnt" -ge "$CRASH_LOOP_N" ]; then
      recent=$(crash_counts "$CRASH_DIR" "$CRASH_RECENT_MIN" "$name" | awk '{print $1}'); recent=${recent:-0}
      # A loop with no crash since the last crashes escalation was already
      # handed over, with its card (Mac crashes RCA 20261020-015312): the 6h
      # cooldown ran out 10 minutes before the 6h quiet window would have called
      # the ChatGPT loop ended, and it re-escalated with zero new crashes.
      newest=$(crash_newest "$CRASH_DIR" "$name"); newest=${newest:-0}
      esc=$(state_get "$CRASH_STATE" esc_crashes 2>/dev/null || true); case "$esc" in ''|*[!0-9]*) esc=0 ;; esac
      if [ "$recent" -gt 0 ] && [ "$esc" -gt 0 ] && [ "$newest" -le "$esc" ]; then
        echo "mac-cleanup: crash reports: $name crashed $cnt times in 24h, the newest $(( ($(date +%s) - newest) / 60 ))m ago, before the last crashes escalation $(( ($(date +%s) - esc) / 60 ))m ago; nothing new to hand over (verdict=crash_loop_already_escalated)"
      elif [ "$recent" -gt 0 ] && [ "$esc" -gt 0 ] && since_esc=$(crash_since "$CRASH_DIR" "$name" "$esc") \
           && [ "${since_esc:-0}" -lt "$CRASH_LOOP_N" ]; then
        # One crash after the escalation must not hand the whole answered burst
        # over again (Mac crashes RCA 20261021-165921: Muse's crash handler
        # crashed once at 11:07, 8 minutes after the 10:59 escalation of the
        # 10:54 pkill burst, and the 25 crashes re-escalated 6h later).
        echo "mac-cleanup: crash reports: $name crashed $cnt times in 24h, $since_esc since the last crashes escalation $(( ($(date +%s) - esc) / 60 ))m ago (under ${CRASH_LOOP_N}); the rest were already handed over (verdict=crash_loop_already_escalated_new_under_threshold)"
      elif [ "$recent" -gt 0 ] && episode=$(crash_episode "$CRASH_DIR" "$name" "$CRASH_RECENT_MIN") && [ "${episode:-0}" -lt "$CRASH_LOOP_N" ]; then
        # Crashes from before a quiet gap belong to a loop that already ended.
        # Mac crashes RCA 20261020-085229: one Chrome relaunch onto an update
        # (2 reports at 08:06) revived 11 reports of a burst that ended 20h
        # earlier, and the 24h total of 13 escalated a loop that was not there.
        echo "mac-cleanup: crash reports: $name crashed $cnt times in 24h, but only $episode since its last $(( CRASH_RECENT_MIN / 60 ))h without a crash; the rest is a loop that ended (verdict=crash_new_episode_under_threshold)"
      elif [ "$recent" -gt 0 ]; then
        local ctx; ctx=$(crash_context "$CRASH_DIR" "$name")
        HEALTH_VERDICTS=$(printf '%s\ncrashes %s crashed %s times in 24h (at or over %s)%s\n' "$HEALTH_VERDICTS" "$name" "$cnt" "$CRASH_LOOP_N" "${ctx:+ ($ctx)}")
      else
        echo "mac-cleanup: crash reports: $name crashed $cnt times in 24h but none in the last $(( CRASH_RECENT_MIN / 60 ))h, so the loop has stopped; not a constraint (verdict=crash_loop_ended)"
      fi
    fi
  done <<< "$top"
  old=$(find "$CRASH_DIR" -maxdepth 1 -type f \( -name '*.ips' -o -name '*.crash' -o -name '*.diag' -o -name '*.hang' -o -name '*.spin' \) -mtime +"$CRASH_KEEP_DAYS" 2>/dev/null | grep -c . || true)
  if [ "$old" -gt 0 ] && [ "$DRY" != 1 ]; then
    find "$CRASH_DIR" -maxdepth 1 -type f \( -name '*.ips' -o -name '*.crash' -o -name '*.diag' -o -name '*.hang' -o -name '*.spin' \) -mtime +"$CRASH_KEEP_DAYS" -delete 2>/dev/null
    pruned=$old
  fi
  CRASH_PRUNED=$pruned
  echo "mac-cleanup: crash reports: last 24h $( [ -n "$top" ] && printf '%s' "$top" | awk '{c=$1; $1=""; sub(/^ /,""); printf "%s%s x%d", s, $0, c; s=", "}' || echo none), pruned ${pruned} older than ${CRASH_KEEP_DAYS}d$( [ "$DRY" = 1 ] && [ "$old" -gt 0 ] && echo " (would prune ${old}, dry run)")"
  # Mac crashes RCA 20261018-105230: the Python crashes are grpc's fork handler
  # in interpreters reading a shared site-packages; a missing .pth there is the
  # next thing to check, so say whether each one carries it.
  local gsp gn=0 gh=0 gmiss=""
  for gsp in /usr/local/lib/python3.*/site-packages /opt/homebrew/lib/python3.*/site-packages; do
    [ -d "$gsp/grpc" ] || continue
    gn=$((gn+1))
    if [ -f "$gsp/amux-grpc-fork-safe.pth" ]; then gh=$((gh+1)); else gmiss="$gmiss $gsp"; fi
  done
  echo "mac-cleanup: python grpc fork-safe defaults: ${gh} of ${gn} site-packages with grpc carry them${gmiss:+; missing:${gmiss} (install: scripts/install-python-grpc-defaults.sh)}"
}

check_launchd_failing() {
  local rows
  rows=$($LAUNCHD_LIST_CMD 2>/dev/null | awk 'NR>1 && $1=="-" && $2!="0" && $3 !~ /^com\.apple\./ {printf "%s%s (last exit %s)", s, $3, $2; s=", "}')
  echo "mac-cleanup: launchd jobs not running after a non-zero exit (outside com.apple): ${rows:-none}"
}

# Prints "<gateway> <loss_pct> <dns_ms>", or nothing when there is no default route.
net_reading() {
  local gw loss t0 t1
  gw=$(route -n get default 2>/dev/null | awk '/gateway:/{print $2}')
  [ -n "$gw" ] || return 0
  loss=$(ping -c 3 -i 0.2 -t 3 "$gw" 2>/dev/null | awk -F', ' '/packet loss/{for(i=1;i<=NF;i++) if($i ~ /packet loss/){sub("%.*","",$i); print $i+0}}')
  t0=$(perl -MTime::HiRes=time -e 'printf "%d", time*1000'); dscacheutil -q host -a name github.com >/dev/null 2>&1; t1=$(perl -MTime::HiRes=time -e 'printf "%d", time*1000')
  echo "$gw ${loss:--} $((t1 - t0))"
}

check_network() {
  local gw loss dns
  read -r gw loss dns <<< "$($NET_CMD 2>/dev/null)"
  if [ -z "${gw:-}" ]; then echo "mac-cleanup: network: no default route (offline, or the probe printed nothing)"; return 0; fi
  echo "mac-cleanup: network: gateway $gw loss ${loss}% over 3 pings, DNS lookup ${dns}ms (reported, never changed)"
}

check_thermal() {
  local out lim
  out=$($THERMAL_CMD 2>/dev/null)
  lim=$(printf '%s\n' "$out" | awk -F= '/CPU_Speed_Limit/{gsub(/[ \t]/,"",$2); print $2}')
  if [ -n "$lim" ]; then echo "mac-cleanup: thermal: CPU speed limit ${lim}%$( [ "$lim" -lt 100 ] 2>/dev/null && echo " (throttled)")"
  elif printf '%s' "$out" | grep -q 'No thermal warning'; then echo "mac-cleanup: thermal: no thermal warning recorded"
  else echo "mac-cleanup: thermal: unmeasured"; fi
}

agent_volume_status() {
  local f; f=$(mktemp "${TMPDIR:-/tmp}/agent-volume.XXXXXX") || return 1
  git -C "${AMUX_REPO_DIR:-$HOME/Dev/amux}" show origin/main:scripts/mac-agent-volume.sh > "$f" 2>/dev/null \
    && perl -e 'alarm 30; exec @ARGV' bash "$f" status; local rc=$?
  rm -f -- "${f:?}"; return $rc
}
check_agent_volume() {
  local out; out=$($AGENT_VOLUME_CMD 2>/dev/null)
  if [ -z "$out" ]; then echo "mac-cleanup: agent volume: unmeasured (scripts/mac-agent-volume.sh gave nothing)"; return 0; fi
  printf '%s\n' "$out" | sed 's/^agent-volume: /mac-cleanup: agent volume: /; s/^next/mac-cleanup:   agent volume next/'
}

self_verdict() { HEALTH_VERDICTS=$(printf '%s\nself %s\n' "$HEALTH_VERDICTS" "$1"); }

# Is this check itself working? Run rows (status, cause), run logs (finished,
# elapsed, skipped arms) and fallback runs over the last 24h, plus whether the
# schedule is still enabled. Each failing rule adds a "self ..." constraint, so it
# escalates like any other class, with its own cooldown.
# A fire that met a running tick lost nothing when the holder is another run of
# this same schedule, started earlier and still writing its log: that run is the
# hour's check, and its own outcome is counted above. Run logs are named after
# their start in ms, so "started before and still writing at" is measurable.
# Mac self RCA 20261023-145529: the desktop lane ran SCHED-465 by hand at
# 12:47, 13:50 and 14:50 (source manual:desktop), the :55 cron fire met each one,
# and all three read as lost work. A holder outside the scheduler (the launchd
# fallback, a hand-run script, the 2026-10-15 duplicate ticks) has no run log
# here, so it still counts. A log with a non-numeric name cannot be placed in
# time and also still counts.
self_coll_covered() { # <collision log> -> 0 if a tracked earlier run held the lock
  local f=$1 t g b gs
  t=$(stat -c %Y "$f" 2>/dev/null || stat -f %m "$f" 2>/dev/null) || return 1
  for g in "$SELF_RUNS_DIR"/*.log; do
    [ "$g" = "$f" ] && continue
    b=${g##*/}; b=${b%.log}
    case "$b" in ''|*[!0-9]*) continue ;; esac
    gs=$((b / 1000))
    [ "$gs" -lt "$t" ] || continue
    grep -aq '^mac-cleanup: previous tick still running' "$g" 2>/dev/null && continue
    [ "$(stat -c %Y "$g" 2>/dev/null || stat -f %m "$g" 2>/dev/null || echo 0)" -ge "$((t - 5))" ] && return 0
  done
  return 1
}

check_self_runs() {
  local en rows st note ok=0 bad=0 streak=0 instreak=1 causes="" f line el slowest=0 slow_recent=0 slowest_age=0 fage n_logs=0 n_unfin=0 n_skip=0 n_skip_new=0 skip_esc=0 n_coll=0 n_coll_cov=0 bad_new=0 ran since fb sum
  if ! en=$(sqlite3 -readonly -cmd '.timeout 5000' "$SELF_DB" \
      "SELECT enabled FROM schedules WHERE id='$SELF_SCHED_ID' AND deleted IS NULL" 2>/dev/null); then
    echo "mac-cleanup: self: run history UNMEASURED ($SELF_DB unreadable), judged nothing about this check (verdict=self_unmeasured)"
    return 0
  fi
  if [ "$en" != 1 ]; then
    self_verdict "$SELF_SCHED_ID is $([ -z "$en" ] && echo missing || echo disabled): only the 15-minute launchd fallback runs this check now"
  fi
  rows=$(sqlite3 -readonly -separator '|' -cmd '.timeout 5000' "$SELF_DB" \
    "SELECT ran_at, status, replace(replace(COALESCE(note,''),'|',' '),char(10),' ') FROM schedule_runs WHERE schedule_id='$SELF_SCHED_ID' AND status NOT IN ('running','pending') AND ran_at >= CAST(strftime('%s','now') AS INTEGER) - 86400 ORDER BY ran_at DESC" 2>/dev/null)
  skip_esc=$(state_get "$STATE_DIR/state" esc_self 2>/dev/null || true); case "$skip_esc" in ''|*[!0-9]*) skip_esc=0 ;; esac
  while IFS='|' read -r ran st note; do
    [ -n "$st" ] || continue
    if [ "$st" = ok ]; then ok=$((ok+1)); instreak=0; continue; fi
    bad=$((bad+1)); [ "$instreak" = 1 ] && streak=$((streak+1))
    case "$ran" in ''|*[!0-9]*) bad_new=$((bad_new+1)) ;; *) [ "$ran" -gt "$skip_esc" ] && bad_new=$((bad_new+1)) ;; esac
    case "$note" in
      *"timed out"*) note="killed at the scheduler's time limit" ;;
      *"server restarted"*) note="killed by an amux server restart" ;;
      *) note="${st}: ${note:0:60}" ;;
    esac
    causes=$(printf '%s\n%s' "$causes" "$note")
  done <<< "$rows"
  sum=$(printf '%s\n' "$causes" | grep . | sort | uniq -c | sort -rn | awk '{c=$1; $1=""; sub(/^ /,""); printf "%s%s x%d", s, $0, c; s="; "}')
  # A log still being written belongs to this run (or a live one): skip the last 10 minutes.
  while IFS= read -r f; do
    [ -n "$f" ] || continue
    n_logs=$((n_logs+1))
    # The scheduler records a fire that met a running tick as ok, and it did no work
    # (2026-10-15: 5 of 19 "ok" runs, each blocked by a duplicate fallback tick).
    if grep -aq '^mac-cleanup: previous tick still running' "$f" 2>/dev/null; then
      if self_coll_covered "$f"; then n_coll_cov=$((n_coll_cov+1)); else n_coll=$((n_coll+1)); fi
      continue
    fi
    line=$(grep -a '^mac-cleanup: done ' "$f" 2>/dev/null | tail -1)
    if [ -z "$line" ]; then n_unfin=$((n_unfin+1)); continue; fi
    el=${line#*elapsed=}; el=${el%%s *}
    case "$el" in ''|*[!0-9]*) ;; *)
      fage=$(( $(date +%s) - $(stat -c %Y "$f" 2>/dev/null || stat -f %m "$f" 2>/dev/null || date +%s) ))   # GNU first (see the note above, line ~740)
      if [ "$el" -gt "$slowest" ]; then slowest=$el; slowest_age=$fage; fi
      if [ "$fage" -lt $(( SELF_SLOW_RECENT_MIN * 60 )) ] && [ "$el" -gt "$slow_recent" ]; then slow_recent=$el; fi ;;
    esac
    case "$line" in *" skipped=none "*) ;; *" skipped="*) n_skip=$((n_skip+1))
      [ "$(stat -c %Y "$f" 2>/dev/null || stat -f %m "$f" 2>/dev/null || echo 0)" -gt "$skip_esc" ] && n_skip_new=$((n_skip_new+1)) ;; esac
  done < <(find "$SELF_RUNS_DIR" -maxdepth 1 -type f -name '*.log' -mmin -1440 ! -mmin -10 2>/dev/null)
  since=$(date -v-24H '+%F %T' 2>/dev/null || date -d '24 hours ago' '+%F %T' 2>/dev/null)
  fb=$(awk -v s="$since" '/so launchd is$/ { if ($2" "$3 >= s) n++ } END { print n+0 }' "$SELF_FALLBACK_LOG" 2>/dev/null); fb=${fb:-0}
  echo "mac-cleanup: self: $SELF_SCHED_ID last 24h: $((ok+bad)) finished runs, $ok ok, $bad not ok${sum:+ ($sum)}; newest $streak in a row not ok; $n_logs run log(s), $n_unfin killed before the done line, $n_coll found another tick running and did nothing, $n_coll_cov met an earlier run of this schedule still working, slowest ${slowest}s $(( slowest_age / 3600 ))h ago, slowest in the last $(( SELF_SLOW_RECENT_MIN / 60 ))h ${slow_recent}s (warn ${TICK_WARN_S}s, killed at 600s), $n_skip skipped arms for lack of time; the launchd fallback ran it ${fb}x"
  [ "$streak" -ge "$SELF_FAIL_STREAK" ] && self_verdict "$streak runs in a row of $SELF_SCHED_ID did not finish ok ($sum)"
  # Raised again only when a failed run is NEWER than the last self escalation
  # (Mac self RCA 20261021-125823): the 06:52-09:54Z restart-loop failures, already
  # answered with 666533ab0 at 06:58Z, re-escalated at 12:58Z with 8 ok runs since.
  if [ "$bad" -ge "$SELF_FAIL_24H" ] && [ "$bad_new" -gt 0 ]; then
    self_verdict "$bad of $((ok+bad)) runs of $SELF_SCHED_ID in 24h did not finish ok ($sum)"
  elif [ "$bad" -ge "$SELF_FAIL_24H" ]; then
    echo "mac-cleanup: self: all $bad failed runs of $SELF_SCHED_ID in 24h predate the last self escalation, which already answered them; not raised again until a new one (verdict=self_failures_already_escalated)"
  fi
  [ "$slow_recent" -ge "$TICK_WARN_S" ] && self_verdict "the slowest run of $SELF_SCHED_ID in the last $(( SELF_SLOW_RECENT_MIN / 60 ))h took ${slow_recent}s, over the ${TICK_WARN_S}s warning (the scheduler kills it at 600s)"
  if [ "$slowest" -ge "$TICK_WARN_S" ] && [ "$slow_recent" -lt "$TICK_WARN_S" ]; then
    echo "mac-cleanup: self: the ${slowest}s run is $(( slowest_age / 3600 ))h old, outside the last $(( SELF_SLOW_RECENT_MIN / 60 ))h; recent runs are under the ${TICK_WARN_S}s warning, so it is not a constraint (verdict=self_slow_run_aged_out)"
  fi
  # Raised again only when a run NEWER than the last self escalation skipped
  # (Mac self RCA 20261020-005306): the 24h count keeps runs from before a fix
  # for a day, and re-escalated them every 6h with nothing new to look at.
  if [ "$n_skip" -ge "$SELF_SKIP_24H" ] && [ "$n_skip_new" -gt 0 ]; then
    self_verdict "$n_skip runs of $SELF_SCHED_ID in 24h skipped cleanup arms for lack of time"
  elif [ "$n_skip" -ge "$SELF_SKIP_24H" ]; then
    echo "mac-cleanup: self: all $n_skip runs that skipped arms in 24h predate the last self escalation, which already answered them; not raised again until a new one (verdict=self_skips_already_escalated)"
  fi
  [ "$n_coll_cov" -gt 0 ] && echo "mac-cleanup: self: $n_coll_cov fire(s) of $SELF_SCHED_ID met an earlier run of the same schedule still working (a manual run shortly before the cron fire); that run did the hour's check, so they are not lost work (verdict=self_collision_covered)"
  [ "$n_coll" -ge "$SELF_COLLIDE_24H" ] && self_verdict "$n_coll fires of $SELF_SCHED_ID in 24h found another tick holding the lock and did no work (the scheduler records them as ok)"
  # Raised again only for a fallback run NEWER than the last self escalation.
  # Self RCA 20261018-165251: the same three runs (10-17 17:30 and 18:19, 10-18
  # 10:25), each already escalated with its fix landed, re-escalated at 16:52
  # because the 6h cooldown ran out while they were still inside the 24h window.
  if [ "$fb" -ge "$SELF_FALLBACK_24H" ]; then
    local esc_at esc_str fb_new
    esc_at=$(state_get "$STATE_DIR/state" esc_self 2>/dev/null); case "$esc_at" in ''|*[!0-9]*) esc_at=0 ;; esac
    esc_str=$(date -r "$esc_at" '+%F %T' 2>/dev/null || date -d "@$esc_at" '+%F %T' 2>/dev/null)
    fb_new=$(awk -v s="${esc_str:-0}" '/so launchd is$/ { if ($2" "$3 > s) n++ } END { print n+0 }' "$SELF_FALLBACK_LOG" 2>/dev/null); fb_new=${fb_new:-0}
    if [ "$esc_at" -eq 0 ] || [ "$fb_new" -gt 0 ]; then
      self_verdict "the launchd fallback ran this check ${fb} times in 24h; it runs only when the scheduled run's output is stale or unfinished, so $SELF_SCHED_ID is not finishing on schedule"
    else
      echo "mac-cleanup: self: all ${fb} fallback runs in 24h predate the last self escalation (${esc_str}), which already answered them; not raised again until a new one (verdict=self_fallback_already_escalated)"
    fi
  fi
  return 0
}

# Who is using the CPU, from the server's census (GET /api/resources: workers,
# and the CPU no worker owns by kind and root tree), and the one reversible
# thing this tick does about it. host_guard renices runaway tool processes
# under a WORKER'S pane; a tree whose shell or worker exited is reparented to
# launchd and escapes it. 2026-10-17, load 87 on 28 cores: two `amux land`
# trees, a pytest and a backfill were among them, and finding them took a
# dozen hand-written ps walks. Busy over UTIL_ACT_SHARE of the cores, each such
# detached tree at or over UTIL_RENICE_MIN_PCT is lowered to nice 10 (never
# killed; the work only runs slower). UTIL_RENICE_EXCLUDE (an extended regex on
# the root's command line) is never touched: tmux, because every new worker
# pane would inherit the priority, and gs12 by the owner's standing priority.
# UTIL_SUMMARY names the heaviest consumers for the cpu constraint.
UTIL_SUMMARY=""; UTIL_RENICED=0
check_utilization() { # <dry:0|1>
  local DRY=${1:-0} json parsed busy cap total line root pct cmd pids p n_trees=0 n_pids=0 log="$STATE_DIR/util-renice.log"
  UTIL_SUMMARY=""; UTIL_RENICED=0
  json=$($UTIL_CMD 2>/dev/null) || json=""
  parsed=$(printf '%s' "$json" | python3 -c '
import json, sys
try: d = json.load(sys.stdin)
except Exception: print("unmeasured the server did not answer"); sys.exit()
m = d.get("machine") or {}
if not m.get("measured"): print("unmeasured " + str(m.get("why_unmeasured") or "no machine block (server older than the census)")); sys.exit()
pc = lambda x: "%d%%" % round(x)
top = ", ".join("%s %s" % (w["name"], pc(w["cpu_percent"])) for w in (d.get("top_by_cpu") or [])[:3] if w["cpu_percent"] >= 50)
rest = ", ".join("%s %s" % (k, pc(v)) for k, v in sorted((m.get("by_kind") or {}).items(), key=lambda kv: -kv[1]) if v >= 50)
print("busy %s %s %s %s" % (m["cpu_percent_total"], m["capacity_percent"], m["workers_cpu_percent"], m["unattributed_cpu_percent"]))
print("who workers " + (top or "none over 50%") + "; rest " + (rest or "none over 50%"))
for o in m.get("top_unattributed") or []:
    if o["kind"] == "detached": print("tree %d %s %s" % (o["root_pid"], o["cpu_percent"], o["command"].rsplit("/", 1)[-1][:60]))
' 2>/dev/null)
  case "$parsed" in
    busy*) ;;
    *) parsed=${parsed#unmeasured }; echo "mac-cleanup: utilization: unmeasured: ${parsed:-no reading} (verdict=util_unmeasured)"; return 0 ;;
  esac
  read -r _ total cap _ _ <<< "$(printf '%s\n' "$parsed" | sed -n 1p)"
  UTIL_SUMMARY=$(printf '%s\n' "$parsed" | sed -n 's/^who //p')
  echo "mac-cleanup: utilization: $(printf '%s\n' "$parsed" | awk 'NR==1{printf "%d%% of %d%% busy, workers %d%%, everything else %d%%", $2, $3, $4, $5}'); $UTIL_SUMMARY"
  local idle share
  idle=$($UTIL_IDLE_CMD 2>/dev/null | awk '/^CPU usage/ {for (i=1;i<=NF;i++) if ($i ~ /idle/) v=$(i-1)} END{sub(/%/,"",v); print v}')
  case "$idle" in ''|*[!0-9.]*) idle=-1 ;; esac
  share=$(awk -v t="$total" -v c="$cap" -v i="$idle" 'BEGIN{ s = (c>0) ? t/c : 0; if (i >= 0 && (100-i)/100 > s) s = (100-i)/100; printf "%.2f", s }')
  # The bar is never stricter than the cpu constraint's own (busy once idle is
  # under CPU_IDLE_MAX_PCT): with the arm at 0.85 and the constraint at 0.75,
  # a machine 79% busy escalated every tick and this arm never acted (same RCA).
  local act; act=$(awk -v s="$UTIL_ACT_SHARE" -v m="${CPU_IDLE_MAX_PCT:-25}" 'BEGIN{ b = 1 - m/100; printf "%.2f", (b < s) ? b : s }')
  busy=$(awk -v x="$share" -v s="$act" 'BEGIN{print (x >= s) ? 1 : 0}')
  if awk -v i="$idle" -v t="$total" -v c="$cap" 'BEGIN{exit !(i >= 0 && c > 0 && (100-i)/100 > t/c + 0.05)}'; then
    echo "mac-cleanup: utilization: top reads the CPU $(awk -v i="$idle" 'BEGIN{printf "%d", 100-i}')% busy, above the census; judging busy by top (verdict=util_census_under_top)"
  fi
  local snap; snap=$($UTIL_TREE_PS_CMD 2>/dev/null)
  while read -r _ root pct cmd; do
    [ -n "$root" ] || continue
    awk -v p="$pct" -v m="$UTIL_RENICE_MIN_PCT" 'BEGIN{exit !(p+0 >= m+0)}' || continue
    line=$(printf '%s\n' "$snap" | awk -v r="$root" '$1==r' | head -1)
    if [ -z "$line" ]; then echo "mac-cleanup:   detached tree pid $root ($cmd) at ${pct}%: gone before it was read"; continue; fi
    if printf '%s\n' "$line" | awk '{$1=$2=$3=$4=""; print}' | grep -Eq -- "$UTIL_RENICE_EXCLUDE"; then
      echo "mac-cleanup:   detached tree pid $root ($cmd) at ${pct}%: excluded, not touched"; continue
    fi
    if [ "$busy" != 1 ]; then echo "mac-cleanup:   detached tree pid $root ($cmd) at ${pct}%: reported (CPU under ${act} busy)"; continue; fi
    # The tree: the root and every descendant, own uid, not already low.
    pids=$(printf '%s\n' "$snap" | awk -v r="$root" -v me="$(id -u)" '
      { pp[$1]=$2; uid[$1]=$3; ni[$1]=$4; order[NR]=$1 }
      END { for (i=1;i<=NR;i++) { p=order[i]; q=p; for (k=0;k<64 && q!="" && q!=r && q>1;k++) q=pp[q]
              if (q==r && uid[p]==me && ni[p]+0 < 10) printf "%s ", p } }')
    if [ -z "$pids" ]; then echo "mac-cleanup:   detached tree pid $root ($cmd) at ${pct}%: already at low priority"; continue; fi
    if [ "$DRY" = 1 ]; then echo "mac-cleanup:   detached tree pid $root ($cmd) at ${pct}%: would lower $(echo $pids | wc -w | tr -d ' ') process(es) to nice 10 (dry run)"; continue; fi
    n=0; for p in $pids; do $RENICE_CMD -n 10 -p "$p" >/dev/null 2>&1 && n=$((n+1)); done
    n_trees=$((n_trees+1)); n_pids=$((n_pids+n))
    mkdir -p "$STATE_DIR"; printf '%s\treniced\t%s\t%s%%\t%s procs\t%s\n' "$(date '+%F %T')" "$root" "$pct" "$n" "$cmd" >> "$log"
    echo "mac-cleanup:   detached tree pid $root ($cmd) at ${pct}%: lowered $n process(es) to nice 10 (verdict=util_detached_reniced)"
  done <<< "$(printf '%s\n' "$parsed" | grep '^tree ' || true)"
  UTIL_RENICED=$n_pids
  echo "mac-cleanup: utilization: detached trees lowered ${n_trees} (${n_pids} processes) at or over ${UTIL_RENICE_MIN_PCT}% while the CPU is ${act}+ busy; log $log"
}

DEMOTED=0
demote_background_daemons() { # <dry:0|1>
  local DRY=${1:-0} pid uid pri pcpu comm name n=0 found=0 log="$STATE_DIR/daemon-demote.log"
  DEMOTED=0
  while read -r pid uid pri pcpu comm; do
    [ -n "$comm" ] || continue
    name=${comm##*/}
    in_list "$name" "$DEMOTE_NAMES" || continue
    [ "$uid" = "$(id -u)" ] || continue
    awk -v p="$pcpu" -v m="$DEMOTE_MIN_PCT" 'BEGIN{exit !(p+0 >= m+0)}' || continue
    [ "${pri:-0}" -gt 20 ] || { echo "mac-cleanup:   $name pid $pid at ${pcpu}%: already background (pri $pri)"; continue; }
    found=$((found+1))
    if [ "$DRY" = 1 ]; then echo "mac-cleanup:   $name pid $pid at ${pcpu}% pri $pri: would set background policy (dry run)"; continue; fi
    if $TASKPOLICY_CMD "$pid" >/dev/null 2>&1; then
      n=$((n+1)); mkdir -p "$STATE_DIR"; printf '%s\tbackground\t%s\t%s\t%s%%\tpri %s\n' "$(date '+%F %T')" "$pid" "$name" "$pcpu" "$pri" >> "$log"
      echo "mac-cleanup:   $name pid $pid at ${pcpu}% pri $pri: set to background policy (verdict=daemon_demoted)"
    else
      echo "mac-cleanup:   $name pid $pid: taskpolicy FAILED (verdict=daemon_demote_failed)"
    fi
  done < <($DEMOTE_PS_CMD 2>/dev/null)
  DEMOTED=$n
  echo "mac-cleanup: background daemons [$DEMOTE_NAMES] at or over ${DEMOTE_MIN_PCT}% at normal priority: found $found, demoted $n"
}

system_health() { # <dry:0|1>
  HEALTH_VERDICTS=""; CPU_RESTARTED=0; TEST_ORPHANS_STOPPED=0; CRASH_PRUNED=0
  if [ "$HEALTH_PROBES" = off ]; then echo "mac-cleanup: system health: probes off (AMUX_CLEANUP_HEALTH_PROBES=off)"; return 0; fi
  check_cpu_hogs "$1"; reap_test_orphans "$1"; reap_orphan_mp_workers "$1"; reap_orphan_crash_handlers "$1"; check_limits; check_respawn_loops; check_crash_reports "$1"
  check_launchd_failing; check_network; check_thermal; check_agent_volume; check_utilization "$1"; demote_background_daemons "$1"; check_self_runs
  HEALTH_VERDICTS=$(printf '%s\n' "$HEALTH_VERDICTS" | grep . || true)
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

# Prune superseded artifacts inside cargo targets, active ones included
# (disk RCA 20261018-075226). Two kinds, both regenerable and neither an input
# to any later build:
#  - test/bin executables deps/<stem>-<16 hex> beyond the newest N per stem
#    (each source path or flag set hashes to a new copy; cargo only ever runs
#    the one its current fingerprint names), with their .d and .dSYM;
#  - *.rcgu.o codegen units, which rustc deletes after a link it finishes, so
#    one older than the age floor was left by a compile that was killed.
# Never a file younger than the floor, and never one any process holds open.
prune_superseded_artifacts() { # <roots> <keep_n> <min_age_min> <dry:0|1> [budget_s]
  # One pass per stage, never a process per file (self RCA 20261018-103045): the
  # first version ran has_open_handle (sed + a regex grep over the whole lsof
  # snapshot, ~55ms idle and far more under load) once per candidate. Its first
  # live run had 5830 candidates, ran ~30 min, outlived SCHED-465's 600s limit and
  # was killed before the done line, so the launchd fallback reran the tick. Now:
  # stat in batches, one awk pass that drops every candidate lsof shows open, and
  # a deadline (TARGET_BUDGET_S) checked between targets.
  local roots=$1 keep=$2 age=$3 dry=${4:-0} budget=${5:-$TARGET_BUDGET_S}
  local cands lsofsnap stats picks open_out t d now cutoff deadline f n_targets=0 n_fail=0 k_open=0 k_recent=0 n_exe=0 n_obj=0 kb=0 complete=yes
  SUPERSEDED_PRUNED=0; SUPERSEDED_PRUNED_KB=0
  if [ "${keep:-0}" -le 0 ]; then echo "mac-cleanup: superseded artifacts: off (AMUX_CLEANUP_SUPERSEDED_KEEP=0)"; return 0; fi
  deadline=$(( $(date +%s) + budget ))
  cands=$(mktemp "${TMPDIR:-/tmp}/sup-cands.XXXXXX"); lsofsnap=$(mktemp "${TMPDIR:-/tmp}/sup-lsof.XXXXXX")
  stats=$(mktemp "${TMPDIR:-/tmp}/sup-stats.XXXXXX"); picks=$(mktemp "${TMPDIR:-/tmp}/sup-picks.XXXXXX"); open_out=$(mktemp "${TMPDIR:-/tmp}/sup-open.XXXXXX")
  find_cargo_targets "$roots" "$TARGET_DEPTH" "$TARGET_SCAN_S" | sort -u > "$cands"
  # THE SHARED TARGETS, WHETHER OR NOT THE SCAN FINDS THEM (disk RCA
  # 20261025-190143). find_cargo_targets keys on CACHEDIR.TAG, and
  # ~/.amux/rust-build-target has none, so this arm never saw it: 31.4G of
  # orphaned amux-crate .rcgu.o sat in its debug/deps, the profile passed
  # cargo-budget's 40G cap and every lane's cargo and pre-commit was refused.
  # TARGET_KEEP protects a target from being REAPED whole; pruning what it
  # holds that no build will read is this arm's job, kept targets included.
  local kept_added=0 k
  local IFS_OLD=$IFS; IFS=':'
  for k in $TARGET_KEEP; do
    IFS=$IFS_OLD
    [ -n "$k" ] && [ -d "$k" ] || continue
    grep -qxF -- "$k" "$cands" || { printf '%s\n' "$k" >> "$cands"; kept_added=$((kept_added+1)); }
  done
  IFS=$IFS_OLD
  if ! { $LSOF_CMD > "$lsofsnap" 2>/dev/null && [ -s "$lsofsnap" ]; }; then
    echo "mac-cleanup: superseded artifacts: open handles UNMEASURED (lsof produced nothing): pruned 0, nothing is deleted without it"
    rm -f -- "${cands:?}" "${lsofsnap:?}" "${stats:?}" "${picks:?}" "${open_out:?}"; return 0
  fi
  now=$(date +%s); cutoff=$(( now - age * 60 ))
  while IFS= read -r t; do
    [ -d "$t" ] || continue
    if [ "$(date +%s)" -ge "$deadline" ]; then complete=no; break; fi
    n_targets=$((n_targets+1))
    for d in "$t"/*/deps "$t"/*/*/deps; do
      [ -d "$d" ] || continue
      # "<kind>\t<mtime>\t<size>\t<path>", every file in one stat batch per deps dir.
      find "$d" -maxdepth 1 -type f \( -perm -u+x -name '*-????????????????' -o -name '*.rcgu.o' \) \
        -exec sh -c 'stat -c "%Y %s %n" "$@" 2>/dev/null || stat -f "%m %z %N" "$@" 2>/dev/null' sh {} + 2>/dev/null >> "$stats"
    done
  done < "$cands"
  # Executables: group by stem, newest first; past the first N and older than the
  # cutoff is a pick. Codegen units: older than the cutoff is a pick. Too-young
  # superseded copies are counted, not picked.
  awk -v keep="$keep" -v cutoff="$cutoff" '
    { f=$0; sub(/^[0-9]+ [0-9]+ /,"",f); mt=$1; sz=$2
      if (f ~ /\.rcgu\.o$/) { if (mt < cutoff) print "obj\t" sz "\t" f; next }
      if (f !~ /-[0-9a-f]{16}$/) next
      stem=f; sub(/-[0-9a-f]{16}$/,"",stem); print "exe\t" stem "\t" mt "\t" sz "\t" f }' "$stats" \
    | sort -t$'\t' -k1,1 -k2,2 -k3,3nr \
    | awk -F'\t' -v keep="$keep" -v cutoff="$cutoff" '
      $1=="obj" { print "obj\t" $2 "\t" $3; next }
      { if ($2!=last) { last=$2; n=0 } n++; if (n<=keep) next
        if ($3 >= cutoff) { print "young\t0\t" $5; next }
        print "exe\t" $4 "\t" $5 }' > "$picks"
  k_recent=$(awk -F'\t' '$1=="young"' "$picks" | wc -l | tr -d ' ')
  # One pass over lsof: a pick is open when any whitespace token, or the NAME
  # column, equals its path.
  awk -v pf="$picks" 'BEGIN { while ((getline l < pf) > 0) { split(l, a, "\t"); if (a[1]!="young") want[a[3]]=1 } }
    { for (i=1;i<=NF;i++) if ($i in want) open[$i]=1
      if (NF>=9) { name=$0; for (i=1;i<9;i++) sub(/^[^[:space:]]+[[:space:]]+/,"",name); sub(/ \(deleted\)$/,"",name); if (name in want) open[name]=1 } }
    END { for (p in open) print p }' "$lsofsnap" > "$open_out"
  k_open=$(wc -l < "$open_out" | tr -d ' ')
  # Drop the open ones in one awk pass, then delete in xargs batches.
  # getline in BEGIN, never NR==FNR: with an empty first file NR==FNR holds for
  # the whole second file and every pick would read as open.
  awk -F'\t' -v of="$open_out" 'BEGIN { while ((getline l < of) > 0) open[l]=1 } $1!="young" && !($3 in open)' "$picks" > "$stats"
  n_exe=$(awk -F'\t' '$1=="exe"' "$stats" | wc -l | tr -d ' ')
  n_obj=$(awk -F'\t' '$1=="obj"' "$stats" | wc -l | tr -d ' ')
  kb=$(awk -F'\t' '{ s += int($2/1024) } END { print s+0 }' "$stats")
  if [ "$dry" != 1 ] && [ -s "$stats" ]; then
    awk -F'\t' '{ print $3; if ($1=="exe") { print $3 ".d"; print $3 ".dSYM" } }' "$stats" | tr '\n' '\0' | xargs -0 rm -rf -- 2>/dev/null
    while IFS=$'\t' read -r _ _ f; do [ -e "$f" ] && n_fail=$((n_fail+1)); done < "$stats"
  fi
  SUPERSEDED_PRUNED=$((n_exe+n_obj)); SUPERSEDED_PRUNED_KB=$kb
  echo "mac-cleanup: superseded artifacts: targets ${n_targets} (${kept_added} shared without CACHEDIR.TAG), complete=${complete}, $( [ "$dry" = 1 ] && echo 'would prune' || echo pruned ) ${n_exe} superseded executable(s) + ${n_obj} orphaned .rcgu.o ($(fmt_kb "$kb")), kept: newest ${keep} per stem, younger than ${age}m ${k_recent}, open ${k_open}, failed ${n_fail}, elapsed $(( $(date +%s) - now ))s"
  rm -f -- "${cands:?}" "${lsofsnap:?}" "${stats:?}" "${picks:?}" "${open_out:?}"
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

# Is there a git checkout within three levels of <dir> that may hold somebody's only
# copy of something? A worktree (.git FILE) or a full clone (.git DIR) counts when it
# has uncommitted or untracked content, a stash, or a local commit no remote has. A
# git that cannot answer in time, or at all, counts as dirty: this keeps on doubt.
has_dirty_worktree() { # <dir>
  local g w out ahead st
  while IFS= read -r g; do
    w=$(dirname "$g")
    out=$(perl -e 'alarm shift; exec @ARGV' "$CLAUDE_TMP_WALK_S" git -C "$w" status --porcelain --untracked-files=normal 2>/dev/null) || return 0
    if [ -n "$out" ]; then return 0; fi
    [ -d "$g" ] || continue
    ahead=$(git -C "$w" log --branches --not --remotes -1 --format=%H 2>/dev/null) || return 0
    st=$(git -C "$w" stash list 2>/dev/null | head -1)
    if [ -n "$ahead" ] || [ -n "$st" ]; then return 0; fi
  done < <(find "$1" -maxdepth 3 -name .git \( -type f -o -type d \) -prune 2>/dev/null)
  return 1
}

du_kb() { # <path> <budget_s>; prints KB, or nothing when the walk ran out of time
  perl -e 'alarm shift; exec @ARGV' "$2" du -sk "$1" 2>/dev/null | awk '{print $1+0}'
}

claude_tmp_ledger() { # <reason> <kb> <path>
  mkdir -p "$STATE_DIR" 2>/dev/null
  printf '%s\t%s\t%s\t%s\n' "$(date '+%F %T')" "$1" "${2:-?}" "$3" >> "$STATE_DIR/claude-tmp-reaps.log"
}

# Judge one quota candidate and remove it when it is idle, closed and holds no
# local work. Uses (and updates) the caller's counters. 0 = removed, 1 = kept.
quota_try_remove() { # <kb> <path> <dry>
  local ekb=$1 e=$2 dry=$3 rc=0
  if [ -f "$e/.git" ]; then n_qkept=$((n_qkept+1)); q_local=$((q_local+1)); return 1; fi
  any_recent "$CLAUDE_TMP_ENTRY_IDLE_H" "$CLAUDE_TMP_WALK_S" "$e" || rc=$?
  if [ "$rc" != 1 ]; then n_qkept=$((n_qkept+1)); q_recent=$((q_recent+1)); return 1; fi
  if has_open_handle "$e" "$lsofsnap" || has_dirty_worktree "$e"; then n_qkept=$((n_qkept+1)); q_local=$((q_local+1)); return 1; fi
  if [ "$dry" = 1 ]; then echo "mac-cleanup:   would remove $(fmt_kb "$ekb") $e (session over ${CLAUDE_TMP_QUOTA_GB}G quota, entry idle >= ${CLAUDE_TMP_ENTRY_IDLE_H}h, dry run)"; return 0; fi
  rm -rf -- "${e:?}"
  if [ -e "$e" ]; then n_fail=$((n_fail+1)); echo "mac-cleanup:   FAILED to remove $e (still present after rm)"; return 1; fi
  CLAUDE_TMP_REAPED=$((CLAUDE_TMP_REAPED+1)); CLAUDE_TMP_REAPED_KB=$((CLAUDE_TMP_REAPED_KB+ekb))
  claude_tmp_ledger "over ${CLAUDE_TMP_QUOTA_GB}G quota, entry idle >= ${CLAUDE_TMP_ENTRY_IDLE_H}h" "$ekb" "$e"
  echo "mac-cleanup:   removed $(fmt_kb "$ekb") $e (session over ${CLAUDE_TMP_QUOTA_GB}G quota, entry idle >= ${CLAUDE_TMP_ENTRY_IDLE_H}h)"
  return 0
}

# Sets CLAUDE_TMP_REAPED / CLAUDE_TMP_REAPED_KB. Fails closed like the arms above:
# no lsof snapshot means nothing is removed, and a walk that runs out of time keeps.
reap_claude_tmp() { # <root> <idle_hours> <dry:0|1>
  local root=$1 idle_h=$2 dry=${3:-0}
  local t0 lsofsnap live sess proj uuid rc kb q n_sess=0 n_live=0 n_open=0 n_dirty=0 n_unk=0 n_over=0 n_fail=0 n_quota=0 n_qkept=0 handles=measured
  local sess_budget=$(( CLAUDE_TMP_BUDGET_S - CLAUDE_TMP_QUOTA_BUDGET_S )) qt0
  [ "$sess_budget" -gt 0 ] || sess_budget=1
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
  # The pass resumes where the last one ran out of time (2026-10-13: 38 of 136 session
  # dirs reached inside the share, the same alphabetical 98 skipped every tick, so an
  # idle session late in the list was never judged). The cursor is the first dir the
  # last pass did not reach; a dir that has since gone is replaced by the next one
  # after it in order, and a pass that reaches every dir clears the cursor.
  local -a sessions=(); local cursor_file="$STATE_DIR/claude-tmp.cursor" cursor="" start=0 nall k stopped=""
  for sess in "$root"/*/*/; do sess=${sess%/}; [ -d "$sess" ] && sessions+=("$sess"); done
  nall=${#sessions[@]}
  if [ "$nall" -gt 0 ] && [ -s "$cursor_file" ]; then
    cursor=$(cat "$cursor_file")
    for ((k=0; k<nall; k++)); do if ! [[ "${sessions[$k]}" < "$cursor" ]]; then start=$k; break; fi; done
  fi
  for ((k=0; k<nall; k++)); do
    sess=${sessions[$(( (start + k) % nall ))]}
    [ -d "$sess" ] || continue
    n_sess=$((n_sess+1))
    uuid=${sess##*/}; proj=${sess%/*}; proj=${proj##*/}
    if [ $(( $(date +%s) - t0 )) -ge "$sess_budget" ]; then n_over=$((n_over+1)); [ -n "$stopped" ] || stopped=$sess; continue; fi
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
  if [ "$dry" != 1 ]; then
    if [ -n "$stopped" ]; then mkdir -p "$STATE_DIR" && printf '%s\n' "$stopped" > "$cursor_file"
      echo "mac-cleanup:   claude tmp: session pass ran out of its ${sess_budget}s share; the next tick starts at $stopped"
    else rm -f -- "$cursor_file"; fi
  fi
  # Rule 2: the quota on live sessions, biggest idle entries first.
  if [ "$quota_kb" -gt 0 ]; then
    qt0=$(date +%s)
    while IFS= read -r sess; do
      [ -d "$sess" ] || continue
      if [ $(( $(date +%s) - qt0 )) -ge "$CLAUDE_TMP_QUOTA_BUDGET_S" ]; then n_over=$((n_over+1)); continue; fi
      # A session too big to sum inside the walk budget is measured by its entries
      # instead: skipping it would exempt exactly the sessions the quota is for
      # (gs12-cicd's 45.8G did not finish a 15s walk on 2026-10-02).
      local total ents ekb e sum=0
      kb=$(du_kb "$sess" "$CLAUDE_TMP_PROBE_S")
      if [ -n "$kb" ] && [ "$kb" -le "$quota_kb" ]; then continue; fi
      # A session too big to size has no known total, so the entries cannot say
      # when it is under quota: every eligible idle entry goes, judged as soon as
      # it is sized (sizing everything first spent the budget before any judging).
      local qall=0; [ -n "$kb" ] || qall=1
      ents=$(mktemp "${TMPDIR:-/tmp}/ctmp-ents.XXXXXX")
      # A plain folder that is too big to size in time, or over quota while still
      # holding recent files (so it can never go whole), is a container (gs12 keeps every repo copy under scratchpad/work): its children
      # are the candidates. Before 2026-10-12 such a folder was dropped from the
      # count, so a 451G session read 26.1G and none of its 405G was ever considered.
      local qunk=0 c ckb q_recent=0 q_local=0 q_small=0
      for e in "$sess"/scratchpad/* "$sess"/scratchpad/.[!.]*; do
        [ -e "$e" ] || continue
        if [ $(( $(date +%s) - qt0 )) -ge "$CLAUDE_TMP_QUOTA_BUDGET_S" ]; then qunk=$((qunk+1)); break; fi
        ekb=$(du_kb "$e" "$CLAUDE_TMP_PROBE_S")
        # A file, or a checkout, that the probe could not size gets the full walk.
        if [ -z "$ekb" ] && { [ ! -d "$e" ] || [ -e "$e/.git" ]; }; then ekb=$(du_kb "$e" "$CLAUDE_TMP_WALK_S"); fi
        if [ -d "$e" ] && [ ! -e "$e/.git" ] && { [ -z "$ekb" ] || { [ "$ekb" -gt "$quota_kb" ] && any_recent "$CLAUDE_TMP_ENTRY_IDLE_H" "$CLAUDE_TMP_WALK_S" "$e"; }; }; then
          for c in "$e"/* "$e"/.[!.]*; do
            [ -e "$c" ] || continue
            if [ $(( $(date +%s) - qt0 )) -ge "$CLAUDE_TMP_QUOTA_BUDGET_S" ]; then qunk=$((qunk+1)); break; fi
            # A copy in use cannot go, so it is not worth a size walk: judging
            # recency first is what lets the budget reach the idle ones.
            rc=0; any_recent "$CLAUDE_TMP_ENTRY_IDLE_H" "$CLAUDE_TMP_PROBE_S" "$c" || rc=$?
            if [ "$rc" = 0 ]; then n_qkept=$((n_qkept+1)); q_recent=$((q_recent+1)); continue; fi
            if [ "$rc" = 2 ]; then qunk=$((qunk+1)); continue; fi
            ckb=$(du_kb "$c" "$CLAUDE_TMP_WALK_S")
            if [ -z "$ckb" ]; then qunk=$((qunk+1)); continue; fi
            sum=$((sum+ckb))
            if [ "$qall" = 1 ] && [ "$ckb" -ge $(( CLAUDE_TMP_ENTRY_MIN_MB * 1024 )) ]; then quota_try_remove "$ckb" "$c" "$dry" || true; continue; fi
            printf '%s\t%s\n' "$ckb" "$c" >> "$ents"
          done
          if [ $(( $(date +%s) - qt0 )) -ge "$CLAUDE_TMP_QUOTA_BUDGET_S" ]; then break; fi
          continue
        fi
        if [ -n "$ekb" ]; then printf '%s\t%s\n' "$ekb" "$e" >> "$ents"; sum=$((sum+ekb)); else qunk=$((qunk+1)); fi
      done
      total=${kb:-$sum}
      if [ "$total" -le "$quota_kb" ] && [ "$qunk" = 0 ]; then rm -f -- "${ents:?}"; [ -n "$kb" ] || n_unk=$((n_unk+1)); continue; fi
      n_quota=$((n_quota+1))
      sort -rn "$ents" -o "$ents"
      while IFS=$'\t' read -r ekb e; do
        [ "$qall" = 1 ] || [ "$total" -gt "$quota_kb" ] || break
        if [ "$ekb" -lt $(( CLAUDE_TMP_ENTRY_MIN_MB * 1024 )) ]; then q_small=$((q_small+1)); break; fi   # sorted biggest first: everything after is smaller
        # Each candidate can cost a find and a git status per checkout (up to the
        # walk budget each); without this the arm could outlast the tick's 600s kill.
        if [ $(( $(date +%s) - qt0 )) -ge "$CLAUDE_TMP_QUOTA_BUDGET_S" ]; then qunk=$((qunk+1)); break; fi
        if quota_try_remove "$ekb" "$e" "$dry"; then total=$((total-ekb)); fi
      done < "$ents"
      rm -f -- "${ents:?}"
      if [ "$total" -gt "$quota_kb" ] || [ "$qunk" -gt 0 ]; then
        echo "mac-cleanup:   quota kept in $sess: ${q_recent} in use within ${CLAUDE_TMP_ENTRY_IDLE_H}h, ${q_local} open or holding local work, ${qunk} unsized or past the budget, ${q_small} under ${CLAUDE_TMP_ENTRY_MIN_MB}M (verdict=claude_tmp_quota_kept)"
        if [ "$qunk" -gt 0 ]; then
          echo "mac-cleanup:   OVER QUOTA at least $(fmt_kb "$total") $sess (quota ${CLAUDE_TMP_QUOTA_GB}G): ${qunk} entr(ies) too big to size in ${CLAUDE_TMP_WALK_S}s, or past the ${CLAUDE_TMP_QUOTA_BUDGET_S}s quota budget, are NOT counted and NOT considered; the counted rest is recent, open or a checkout with local work, so it stays"
        else
          echo "mac-cleanup:   OVER QUOTA $(fmt_kb "$total") $sess (quota ${CLAUDE_TMP_QUOTA_GB}G): the rest is recent, open or a checkout with local work, so it stays"
        fi
      fi
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
# CPU needs the CPU to be busy, not just a high load (rca 20261015-032259):
# macOS load counts threads blocked in the kernel, so the nightly restic
# backup (a 14G amux.db dump, then hashing) and an fseventsd backlog held
# load15 at 35-42 on 28 cores while top read the CPU 65% idle. Arg 10 is the
# measured idle percent (-1 = unmeasured, which keeps the load-only rule).
# The disk writer that no file search sees: macOS swap. Disk RCA
# 20261022-105614 burned 28.9G/h with no writer named, while swap grew from 7G
# to 59G under memory pressure (two VMs holding 64G, fseventsd 17G). When swap
# growth since the last run explains at least half the burn, the disk verdict
# says so and points at memory. Prints the suffix, or nothing.
swap_burn_note() { # <burn_gbh> <swap_total_prev_mb> <swap_total_now_mb> <hours_between>
  awk -v b="$1" -v p="$2" -v n="$3" -v h="$4" 'BEGIN{
    if (b+0 <= 0 || p+0 <= 0 || n+0 <= 0 || h+0 <= 0) exit
    g = (n - p) / 1024; r = g / h
    if (g >= 1 && r >= b * 0.5) printf "; the writer is macOS swap, grown %.1fG to %.1fG since the last run (%.1fG/h) under memory pressure, so the cause is memory (see the memory class)", p/1024, n/1024, r
  }'
}

classify_constraints() { # <disk_free_gb> <burn> <hours_to_full> <pressure> <swap_free_mb> <load15> <ncpu> <family_exceeds:0|1> <swap_total_mb> [cpu_idle_pct]
  awk -v df="$1" -v b="$2" -v h="$3" -v pr="$4" -v sw="$5" -v l="$6" -v n="$7" -v fam="$8" -v swt="$9" -v idle="${10:--1}" -v imax="$CPU_IDLE_MAX_PCT" \
      -v floor="$DISK_FLOOR_GB" -v htf="$HOURS_TO_FULL" -v swf="$SWAP_FREE_FLOOR_MB" -v cs="$CPU_SHARE" -v swcp="$SWAP_FREE_CRITICAL_PCT" -v sgd="$SWAP_GROW_DISK_GB" -v famd="${FAMILY_DESC:-}" 'BEGIN{
    if (df >= 0 && df < floor) printf "disk free %.1fG is under the %dG floor\n", df, floor
    else if (h != "-" && h+0 < htf) printf "disk burning %.1fG/h, full in %.1fh (under %dh)\n", b, h, htf
    if (pr >= 2) printf "memory kernel pressure %d after the purge arm\n", pr
    else if (swt > 0 && sw >= 0 && sw < swf && (pr != 1 || (sw/swt < swcp && (df < 0 || df < sgd)))) printf "memory swap has %dMB free (under %dMB)\n", sw, swf
    if (l >= 0 && n > 0 && l/n > cs && (idle < 0 || idle < imax)) printf "cpu 15-min load %.1f is %.0f%% of %d cores (over %.0f%%)\n", l, l/n*100, n, cs*100
    # Which family (RCA 20261026-010147): the line used to say only "a process
    # family", so the RCA started by re-running the scan to learn what it was.
    if (fam == 1) print "family a process family is over its share of RAM" (famd != "" ? ": " famd : "")
  }'
}

# THE VM DISK IS THE WRITER THE HOST SEES; THE WRITER IS A CONTAINER INSIDE IT.
# RCA 20261022-200009: the bundle named colima-goal-shared's datadisk and no
# lane, and it took an hour to find ts-dropin-mx rewriting a 2.6G MVS snapshot
# every ~6 min (4.3G/min, 1102G in 28h). Each container's block writes over
# its uptime name it at once.
vm_writers_raw() { # <profile>
  local ctx="colima-$1" ids
  ids=$(perl -e 'alarm 20; exec @ARGV' docker --context "$ctx" ps -q 2>/dev/null) || return 0
  [ -n "$ids" ] || return 0
  perl -e 'alarm 40; exec @ARGV' docker --context "$ctx" stats --no-stream --format '{{.Name}}	{{.BlockIO}}' 2>/dev/null | sed 's/^/S	/'
  # shellcheck disable=SC2086  # ids is a word list
  perl -e 'alarm 20; exec @ARGV' docker --context "$ctx" inspect -f '{{.Name}}	{{.State.StartedAt}}' $ids 2>/dev/null | sed 's/^/I	/'
}
vm_container_writers() { # <profile> [now_epoch]: the top 3 by write rate, one bundle line each
  local p=$1 now=${2:-$(date +%s)}
  ${VM_WRITERS_CMD//PROFILE/$p} 2>/dev/null | python3 -c '
import sys, re, datetime
prof, now = sys.argv[1], float(sys.argv[2])
U = {"B": 1, "kB": 1e3, "KB": 1e3, "MB": 1e6, "GB": 1e9, "TB": 1e12}
def size(t):
    m = re.match(r"\s*([\d.]+)\s*([kKMGT]?B)", t)
    return float(m.group(1)) * U[m.group(2)] if m else 0.0
wrote, started = {}, {}
for line in sys.stdin:
    f = line.rstrip("\n").split("\t")
    if len(f) == 3 and f[0] == "S" and "/" in f[2]:
        wrote[f[1]] = size(f[2].split("/")[1])
    elif len(f) == 3 and f[0] == "I":
        try:
            t = datetime.datetime.fromisoformat(re.sub(r"(\.\d{6})\d*", r"\1", f[2]).replace("Z", "+00:00"))
            started[f[1].lstrip("/")] = t.timestamp()
        except ValueError:
            pass
rows = []
for n, w in wrote.items():
    up_h = (now - started[n]) / 3600 if n in started else 0
    if up_h > 0 and w > 0:
        rows.append((w / 1e9 / up_h, w / 1e9, up_h, n))
rows.sort(reverse=True)
for i, (rate, gb, up, n) in enumerate(rows[:3]):
    tag = " (verdict=vm_container_writer)" if i == 0 else ""
    print(f"- inside colima-{prof}: {n} wrote {gb:.0f}G in {up:.1f}h ({rate:.1f}G/h){tag}")
' "$p" "$now"
}

# A WRITER OUTSIDE THE SEARCHED ROOTS IS STILL HOLDING ITS FILE OPEN. RCA
# 20261025-100211: the find above ran out of budget, never looked in
# ~/.config, and the bundle blamed ts-dropin-mx (its VM disk was flat) while a
# pg_dump wrote ss_s1.dir/5040.dat.gz at 34G/h. lsof found it in 1s. Two
# samples of each open-for-write file's ALLOCATED bytes (a sparse VM disk's
# apparent size never moves) give its growth rate wherever it lives.
open_writers_raw() { # one sample: pid<TAB>command<TAB>allocated bytes<TAB>path, files over 500M open for write
  perl -e 'alarm 20; exec @ARGV' lsof -nP -w 2>/dev/null \
    | awk '$5=="REG" && $4 ~ /^[0-9]+[wu]/ && $7+0>536870912 {n=$9; for(i=10;i<=NF;i++) n=n" "$i; print $2"\t"$1"\t"n}' \
    | sort -u | while IFS=$'\t' read -r pid cmd path; do
        b=$(stat -f %b "$path" 2>/dev/null) && printf '%s\t%s\t%s\t%s\n' "$pid" "$cmd" "$((b * 512))" "$path"
      done
}
open_file_writers() { # [interval_s] [pane map text]: the top 3 growing files, one bundle line each
  local iv=${1:-10} pmap=${2:-} a b
  a=$($OPEN_WRITERS_CMD 2>/dev/null) || a=""
  [ -n "$a" ] || return 0
  sleep "$iv"
  b=$($OPEN_WRITERS_CMD 2>/dev/null) || b=""
  python3 -c '
import sys
iv, a, b, floor = float(sys.argv[1]), sys.argv[2], sys.argv[3], float(sys.argv[4])
def rows(t):
    d = {}
    for l in t.splitlines():
        f = l.split("\t")
        if len(f) == 4 and f[2].isdigit():
            d[(f[0], f[3])] = (f[1], int(f[2]))
    return d
A, B = rows(a), rows(b)
out = []
for k, (cmd, nb) in B.items():
    rate = (nb - A[k][1]) / 2**30 * 3600 / iv if k in A else 0
    if rate >= floor:   # a trickle (a VM disk at 0.0G/h) is not the writer
        out.append((rate, nb / 2**30, k[0], cmd, k[1]))
out.sort(reverse=True)
for r in out[:3]:
    print("%.1f\t%.1f\t%s\t%s\t%s" % r)
' "$iv" "$a" "$b" "${OPEN_WRITERS_MIN_GBH:-1}" | { i=0; while IFS=$'\t' read -r rate gb pid cmd path; do
    tag=""; [ "$i" -eq 0 ] && tag=" (verdict=open_file_writer)"; i=$((i+1))
    echo "- open for write: $path growing ${rate}G/h (${gb}G now) by pid=$pid $cmd, lane: $(lane_for_pid "$pid" "$pmap")$tag"
  done; }
}

# Burn rate from the host-metrics history (5-minute samples), by least squares
# over the window. Two readings 15 minutes apart are noise: round 2 of DESKT-57
# read 65 G/h off exactly that while a peer's delete sat in the window. Prints
# "<burn_gbh> <hours|-> <n> <span_h> <run|->", or "- - <n> <span_h> <run|->" when there are fewer
# than 6 samples or under an hour of span, so the caller falls back and says so.
# The burn is capped at the window's NET loss (oldest minus newest over the
# span): a dip that has already recovered is not burning. On 2026-10-05 the
# disk fell 46 G at 05:00Z and was back to 269.7 G by 05:30Z, yet the slope
# across that dip read 12.1 G/h, "full in 22h" (escalation 20261005-013827).
# The optional <now_ts> <now_free_gb> is this run's own reading, taken AFTER its
# cleanup arms. The host history's newest sample predates them, so without it the
# space this run just reclaimed is invisible: disk RCA 20261018-135254 fitted
# 233G down to 206G (13.3 G/h, "full in 15.5h" off the pre-prune 206.2G) while
# the same run had just pruned 21.9G and the disk read 228.4G. The writer was
# real and bounded: the hourly prune takes back what it builds, a sawtooth.
burn_from_history() { # <history json on stdin> <min_gbh> [now_ts now_free_gb]
  python3 -c '
import json,sys
m=float(sys.argv[1])
try:
    d=json.load(sys.stdin); rows=[(r["ts"],r["disk_free_gb"]) for r in d.get("samples",[]) if r.get("measured") and r.get("disk_free_gb") is not None]
except Exception:
    rows=[]
hn=len(rows); ran="-"
try:
    nt,nf=float(sys.argv[2]),float(sys.argv[3])
    # Only a reading just after the history: a stale history is not stretched to now.
    if rows and nf>=0 and 0<nt-max(r[0] for r in rows)<=3600: rows.append((nt,nf)); ran="run"
except (IndexError,ValueError):
    pass
n=len(rows); span=(max(r[0] for r in rows)-min(r[0] for r in rows))/3600 if n else 0
if n<6 or span<1: print("- - %d %.1f %s"%(hn,span,ran)); sys.exit()
xs=[(t-rows[0][0])/3600 for t,_ in rows]; ys=[f for _,f in rows]
mx=sum(xs)/n; my=sum(ys)/n
slope=sum((x-mx)*(y-my) for x,y in zip(xs,ys))/max(1e-9,sum((x-mx)**2 for x in xs))
o=sorted(zip(xs,ys)); net=(o[0][1]-o[-1][1])/max(1e-9,o[-1][0]-o[0][0])
burn=min(-slope,net); last=ys[-1] if xs[-1]==max(xs) else ys[xs.index(max(xs))]
print(("%.1f -" if burn<m else "%.1f %.1f")%((burn,) if burn<m else (burn,last/burn)), hn, "%.1f"%span, ran)
' "$1" "${2:-}" "${3:-}" 2>/dev/null || echo "- - 0 0.0"
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
  lane_by_path "$1" || printf 'no lane'
}
# An orphan's lane from where it runs. A Claude Code background command is
# reparented to launchd when its shell exits, so the parent walk finds no pane,
# yet its argv or cwd still sits in the worker's own scratch: LANE_TMP_ROOT/<w>
# (the workspace-isolation TMPDIR) or <repo>/.worktrees/<w>. Mac RCA
# 20261019-105318: mxp-gs12's prep.py held 954% CPU and 7.7G as "no lane".
# Printed as "<w> (orphan, by path)" so a reader knows it was inferred.
# "<session-uuid> <worker>" for every live Claude worker, from its own argv
# (--resume <uuid> ... --name <worker>). Seam: AMUX_CLEANUP_SESSION_MAP.
claude_session_map() {
  if [ "${AMUX_CLEANUP_SESSION_MAP+set}" = set ]; then printf '%s\n' "$AMUX_CLEANUP_SESSION_MAP"; return 0; fi
  ps -Ao args= 2>/dev/null | sed -nE 's/.*--resume ([0-9a-f-]{36}).* --name ([A-Za-z0-9._-]+).*/\1 \2/p'
}
lane_by_path() { # <pid>
  local cmd cwd w
  cmd=$(ps -o command= -p "$1" 2>/dev/null) || return 1
  cwd=$(lsof -a -p "$1" -d cwd -Fn 2>/dev/null | sed -n 's/^n//p' | head -1)
  # lsof reports the resolved cwd (/private/var for /var), so match both spellings.
  # A Claude session scratchpad (claude-<uid>/<project>/<session-uuid>/) names
  # the worker whose live session it is (Mac memory RCA 20261019-205313: four
  # 5-day-old orphans under mxp-gs12's scratchpad read "no lane").
  w=$(printf '%s\n%s\n' "$cwd" "$cmd" | SESSMAP="$(claude_session_map)" ROOT="${LANE_TMP_ROOT%/}" ROOTP="$(cd -P "$LANE_TMP_ROOT" 2>/dev/null && pwd)" python3 -c '
import os, re, sys
root = "(?:%s)" % "|".join(re.escape(r) for r in {os.environ["ROOT"], os.environ.get("ROOTP") or os.environ["ROOT"]})
sess = dict(l.split()[:2] for l in os.environ.get("SESSMAP", "").splitlines() if len(l.split()) >= 2)
for line in sys.stdin:
    m = re.search(root + r"/([A-Za-z0-9._-]+)(/|\s|$)", line) or re.search(r"/\.worktrees/([A-Za-z0-9._-]+)(/|\s|$)", line)
    if m: print(m.group(1)); break
    m = re.search(r"/claude-[0-9]+/[^/\s]+/([0-9a-f-]{36})(/|\s|$)", line)
    if m and m.group(1) in sess: print(sess[m.group(1)]); break
' 2>/dev/null)
  [ -n "$w" ] || return 1
  printf '%s (orphan, by path)' "${w#amux-}"
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

# Hold one VM's build cache under VM_PRUNE_MAX_USED. Runs on EVERY tick, not
# only when disk is tight (disk RCA 20261008-124643: the cap lived inside the
# tight-disk branch, so a normal tick never ran it and goal-shared's cache
# reached 64G against a 40G cap while the host burned 34G/h).
vm_guest_used() { # <profile>: "<pct> <avail_gb>" for the VM's data filesystem, or nothing
  local cmd=${VM_GUEST_DF_CMD//PROFILE/$1}
  perl -e 'alarm shift; exec @ARGV' "$VM_STEP_S" $cmd 2>/dev/null \
    | awk 'NR>1 && $2+0>0 {printf "%d %.1f\n", ($3*100+$2-1)/$2, $4/1048576; exit}'
}

cap_vm_build_cache() { # <profile>
  local p=$1 cmd out rc=0 cap=$VM_PRUNE_MAX_USED age=$VM_PRUNE_CAP_AGE g pct avail
  [ "$VM_PRUNE_MAX_USED" != 0 ] || return 0
  g=$(vm_guest_used "$p" || true); pct=${g%% *}; avail=${g#* }
  if [ -z "$g" ]; then
    echo "mac-cleanup:   vm $p data filesystem: unmeasured (df in the guest gave nothing) (verdict=vm_guest_disk_unmeasured)"
  elif [ "$pct" -ge "$VM_GUEST_TIGHT_PCT" ]; then
    cap=0b; age=$VM_GUEST_TIGHT_AGE
    echo "mac-cleanup:   vm $p data filesystem ${pct}% full (${avail}G free, over ${VM_GUEST_TIGHT_PCT}%): build cache unused for ${age} goes whatever its size (verdict=vm_guest_disk_tight)"
  else
    echo "mac-cleanup:   vm $p data filesystem ${pct}% full (${avail}G free)"
  fi
  cmd=${VM_PRUNE_CAP_CMD//PROFILE/$p}; cmd=${cmd//CAP/$cap}
  case "$cmd" in
    vm_build_cache_cap\ *)
      # The window rides on this one command: a tight VM must not shorten it for the next.
      export -f vm_build_cache_cap; export VM_DOCKER
      out=$(VM_PRUNE_CAP_AGE=$age perl -e 'alarm shift; exec @ARGV' "$VM_STEP_S" bash -c "$cmd" 2>&1) || rc=$? ;;
    *) out=$(VM_PRUNE_CAP_AGE=$age perl -e 'alarm shift; exec @ARGV' "$VM_STEP_S" $cmd 2>&1) || rc=$? ;;
  esac
  if [ -n "$g" ]; then
    # Read again after the prune: a cap that freed enough is not a constraint.
    g=$(vm_guest_used "$p" || true); [ -z "$g" ] || { pct=${g%% *}; avail=${g#* }; }
    if [ "$pct" -ge "$VM_GUEST_FULL_PCT" ]; then
      HEALTH_VERDICTS=$(printf '%s\ndisk VM %s data filesystem %s%% full (%sG free, at or over %s%%): its containers fail on write when it fills; the owner frees images or volumes inside it\n' "$HEALTH_VERDICTS" "$p" "$pct" "$avail" "$VM_GUEST_FULL_PCT")
    fi
  fi
  if [ "$rc" = 0 ]; then
    echo "mac-cleanup:   vm $p build cache capped at $cap: $(printf '%s' "$out" | grep -iE 'total|build cache' | head -1 | tr -s ' \t' ' ' | cut -c1-60); $(printf '%s' "$out" | grep -i 'total' | tail -1 | tr -s ' \t' ' ' | cut -c1-60) (verdict=vm_build_cache_capped)"
  else
    echo "mac-cleanup:   vm $p build-cache cap FAILED (rc $rc): $(printf '%s' "$out" | tail -1 | cut -c1-120) (verdict=vm_build_cache_cap_failed)"
  fi
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
    cap_vm_build_cache "$p"
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
  # A directory of a few small files is kept like a small file (mxp-gs12,
  # 2026-10-17): ~/.amux/tmp/mxp-gs12/mvs-drill/standby_ok, a 1 KB manual gate a
  # production MVS promote waits on, was reaped twice (10-14, 10-16) because
  # only a TOP-LEVEL file was protected. Kept: at most LANE_TMP_KEEP_DIR_FILES
  # files, none over LANE_TMP_KEEP_FILE_KB, LANE_TMP_KEEP_DIR_KB in all. Build
  # trees, clones and dependency dirs still go: they hold big files or many.
  local keep_dir_files=${LANE_TMP_KEEP_DIR_FILES:-64} keep_dir_kb=${LANE_TMP_KEEP_DIR_KB:-4096} n_small_dir=0 nf
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
  # Resolved here, not only at the top: the suites run this function on its own.
  local lsof_bin=${LSOF_BIN:-${AMUX_CLEANUP_LSOF_BIN:-$(command -v lsof 2>/dev/null || echo /usr/sbin/lsof)}}
  inuse=$("$lsof_bin" -a -d cwd -Fn 2>/dev/null | sed -n 's/^n//p' | sed 's|^/private||')
  # Fail closed: an empty answer means lsof did not run, never that nothing is in use.
  if [ -z "$inuse" ]; then
    echo "mac-cleanup: lane tmp: WARN in-use directories UNMEASURED ($lsof_bin gave nothing), reaped nothing this tick (verdict=lane_tmp_cwd_unmeasured)"
    return 0
  fi
  # Resumes where the last pass ran out of time or hit the cap, as reap_claude_tmp
  # does, so the same front entries cannot use up every tick's budget.
  local -a ents=(); local j nall start=0 stopped="" t0=$SECONDS rc out n_unk=0 n_over=0
  local walk=${LANE_TMP_WALK_S:-20} budget=${LANE_TMP_BUDGET_S:-90} cursor_file=""
  [ -n "${STATE_DIR:-}" ] && cursor_file="$STATE_DIR/lane-tmp.cursor"
  for e in "$root"/*/*; do [ -e "$e" ] && ents+=("$e"); done
  nall=${#ents[@]}
  if [ "$nall" -gt 0 ] && [ -n "$cursor_file" ] && [ -s "$cursor_file" ]; then
    local cursor; cursor=$(cat "$cursor_file")
    for ((j=0; j<nall; j++)); do if ! [[ "${ents[$j]}" < "$cursor" ]]; then start=$j; break; fi; done
  fi
  for ((j=0; j<nall; j++)); do
    e=${ents[$(( (start + j) % nall ))]}
    [ -e "$e" ] || continue
    if [ "$n" -ge "${LANE_TMP_MAX:-300}" ]; then capped=1; stopped=$e; break; fi
    if [ $(( SECONDS - t0 )) -ge "$budget" ]; then n_over=$((n_over+1)); [ -n "$stopped" ] || stopped=$e; continue; fi
    rc=0; out=$(perl -e 'alarm shift; exec @ARGV' "$walk" find "$e" -mmin -"$LANE_TMP_IDLE_MIN" -print -quit 2>/dev/null) || rc=$?
    if [ "$rc" = 142 ]; then n_unk=$((n_unk+1)); continue; fi
    [ -n "$out" ] && continue
    if printf '%s\n' "$named" | grep -qxF "${e#"$root"/}"; then n_named=$((n_named+1)); continue; fi
    if [ -f "$e" ] && [ ! -L "$e" ] && [ "$(du -sk "$e" 2>/dev/null | cut -f1)" -lt "$small_kb" ] 2>/dev/null; then
      n_small=$((n_small+1)); continue
    fi
    if [ -d "$e" ] && [ ! -L "$e" ] \
       && [ -z "$(perl -e 'alarm shift; exec @ARGV' "$walk" find "$e" -type f -size +"${small_kb}"k -print -quit 2>/dev/null)" ]; then
      nf=$(perl -e 'alarm shift; exec @ARGV' "$walk" find "$e" -type f 2>/dev/null | head -n $((keep_dir_files + 1)) | grep -c . || true)
      if [ "$nf" -ge 1 ] && [ "$nf" -le "$keep_dir_files" ] && [ "$(du -sk "$e" 2>/dev/null | cut -f1)" -lt "$keep_dir_kb" ] 2>/dev/null; then
        n_small_dir=$((n_small_dir+1)); continue
      fi
    fi
    ce=$( (cd "$e" 2>/dev/null && pwd -P) || printf '%s' "$e"); ce=${ce#/private}
    printf '%s\n' "$inuse" | awk -v p="$ce" '$0==p || index($0, p "/")==1 {f=1} END{exit !f}' && continue
    k=$(perl -e 'alarm shift; exec @ARGV' "$walk" du -sk "$e" 2>/dev/null | cut -f1)
    if [ "$dry" = 1 ]; then n=$((n+1)); kb=$((kb+${k:-0})); continue; fi
    rm -rf -- "${e:?}" 2>/dev/null && { n=$((n+1)); kb=$((kb+${k:-0})); }
  done
  if [ "$dry" != 1 ] && [ -n "$cursor_file" ]; then
    if [ -n "$stopped" ]; then mkdir -p "$STATE_DIR" && printf '%s\n' "$stopped" > "$cursor_file"; else rm -f "$cursor_file"; fi
  fi
  echo "mac-cleanup: lane tmp: $([ "$dry" = 1 ] && echo 'would remove' || echo removed) $n entr$([ "$n" = 1 ] && echo y || echo ies) idle over ${LANE_TMP_IDLE_MIN} min, $(awk -v k="$kb" 'BEGIN{printf "%.1fG", k/1048576}') ($root); kept $n_named named by an enabled schedule, $n_small small file(s) under ${small_kb}K, $n_small_dir small dir(s) (<= ${keep_dir_files} files, none over ${small_kb}K)$([ "$capped" = 1 ] && echo "; capped at $LANE_TMP_MAX this tick, the rest next tick")$([ "$n_unk" -gt 0 ] && echo "; kept $n_unk whose idle walk ran past ${walk}s (idleness unknown)")$([ "$n_over" -gt 0 ] && echo "; $n_over not reached inside the ${budget}s budget")$([ -n "$stopped" ] && [ "$dry" != 1 ] && echo ", the next tick starts at ${stopped#"$root"/}")"
}

reap_user_tmp() { # <dry:0|1>
  local dry=$1 root="${USER_TMP_ROOT%/}" d b n=0 kb=0 k inuse nr=0 krs=0 nlive=0 capped=0 pid lk max=${USER_TMP_MAX:-3000}
  [ -d "$root" ] || { echo "mac-cleanup: user tmp: no $root"; return 0; }
  # Resolved here, not only at the top: the suites run this function on its own.
  local raw lsof_bin=${LSOF_BIN:-${AMUX_CLEANUP_LSOF_BIN:-$(command -v lsof 2>/dev/null || echo /usr/sbin/lsof)}}
  raw=$("$lsof_bin" -a -d cwd -Fn 2>/dev/null | sed -n 's/^n//p' | sed 's|^/private||')
  # Fail closed: an empty answer means lsof did not run, never that nothing is in use.
  if [ -z "$raw" ]; then
    echo "mac-cleanup: user tmp: WARN in-use directories UNMEASURED ($lsof_bin gave nothing), reaped nothing this tick (verdict=user_tmp_cwd_unmeasured)"
    return 0
  fi
  inuse=$(printf '%s\n' "$raw" | grep -oE '/\.?tmp[.A-Za-z0-9]+' | sort -u)
  while IFS= read -r d; do
    [ -n "$d" ] || continue
    b=${d##*/}
    printf '%s\n' "$inuse" | grep -qx "/$b" && continue
    if [ "$((n + nr))" -ge "$max" ]; then capped=1; break; fi
    case $b in
      .tmp*)
        # A test still running holds its store: keep the dir while that pid lives.
        pid=
        for lk in "$d"/*.holder-lock; do
          [ -f "$lk" ] || continue
          pid=$(sed -n 's/.*"pid":\([0-9][0-9]*\).*/\1/p' "$lk" 2>/dev/null | head -1)
          [ -n "$pid" ] && kill -0 "$pid" 2>/dev/null && break
          pid=
        done
        if [ -n "$pid" ]; then nlive=$((nlive+1)); continue; fi
        k=$(du -sk "$d" 2>/dev/null | cut -f1)
        if [ "$dry" = 1 ]; then nr=$((nr+1)); krs=$((krs+${k:-0})); continue; fi
        rm -rf -- "${root:?}/${b:?}" 2>/dev/null && { nr=$((nr+1)); krs=$((krs+${k:-0})); }
        continue ;;
    esac
    k=$(du -sk "$d" 2>/dev/null | cut -f1)
    if [ "$dry" = 1 ]; then n=$((n+1)); kb=$((kb+${k:-0})); continue; fi
    rm -rf -- "${root:?}/${b:?}" 2>/dev/null && { n=$((n+1)); kb=$((kb+${k:-0})); }
  done <<EOF
$(find "$root" -maxdepth 1 \( -name 'tmp.*' -o -name '.tmp*' \) -type d -mmin +"$USER_TMP_IDLE_MIN" 2>/dev/null)
EOF
  echo "mac-cleanup: user tmp: $([ "$dry" = 1 ] && echo 'would remove' || echo removed) $n tmp.* dir(s) idle over ${USER_TMP_IDLE_MIN} min, $(awk -v k="$kb" 'BEGIN{printf "%.1fG", k/1048576}'), and $nr .tmp* test dir(s), $(awk -v k="$krs" 'BEGIN{printf "%.1fG", k/1048576}') ($root)$([ "$nlive" -gt 0 ] && echo "; kept $nlive whose holder-lock pid is alive")$([ "$capped" = 1 ] && echo "; capped at $max this tick, the rest next tick") (verdict=user_tmp_reaped)"
}

# Over <cap> of build cache in docker context <ctx>, prune cache unused for
# VM_PRUNE_CAP_AGE. Prints the measured total and a Total: line like prune.
# Cap every running, uncapped container in docker context <ctx> at <cpus>.
# Prints one line per container it capped or failed, then a summary line.
vm_cap_container_cpus() { # <ctx> <cpus>
  local ctx=$1 cpus=$2 name nano capped=0 failed=0 kept=0 exempt=0
  while read -r name nano; do
    [ -n "$name" ] || continue
    name=${name#/}
    if in_list "$name" "$VM_CPU_CAP_EXEMPT"; then exempt=$((exempt+1)); continue; fi
    if [ "${nano:-0}" != 0 ]; then kept=$((kept+1)); continue; fi
    if "$VM_DOCKER" --context "$ctx" update --cpus "$cpus" "$name" >/dev/null 2>&1; then
      capped=$((capped+1)); echo "capped $name at $cpus CPUs (it had none; undo: docker --context $ctx update --cpus 0 $name) verdict=vm_container_cpu_capped"
    else
      failed=$((failed+1)); echo "FAILED to cap $name at $cpus CPUs verdict=vm_container_cpu_cap_failed"
    fi
  done < <("$VM_DOCKER" --context "$ctx" ps -q 2>/dev/null | xargs "$VM_DOCKER" --context "$ctx" inspect --format '{{.Name}} {{.HostConfig.NanoCpus}}' 2>/dev/null)
  echo "summary capped=$capped already_capped=$kept exempt=$exempt failed=$failed"
}

cap_vm_container_cpus() { # <dry:0|1>
  local dry=$1 p out
  [ "$VM_CPU_CAP" != 0 ] || { echo "mac-cleanup: vm cpu caps: off (AMUX_CLEANUP_VM_CPU_CAP=0)"; return 0; }
  local profiles; profiles=$(running_vm_profiles)
  [ -n "$profiles" ] || { echo "mac-cleanup: vm cpu caps: no running colima VM"; return 0; }
  while IFS= read -r p; do
    [ -n "$p" ] || continue
    if [ "$dry" = 1 ]; then echo "mac-cleanup:   would cap uncapped containers in colima VM $p at $VM_CPU_CAP CPUs (dry run)"; continue; fi
    out=$(export -f vm_cap_container_cpus in_list; export VM_DOCKER VM_CPU_CAP_EXEMPT; perl -e 'alarm shift; exec @ARGV' "$VM_STEP_S" bash -c 'vm_cap_container_cpus "$@"' _ "colima-$p" "$VM_CPU_CAP" 2>&1) || true
    printf '%s\n' "$out" | grep -v '^summary ' | sed "s/^/mac-cleanup:   vm $p /"
    echo "mac-cleanup: vm cpu caps: $p $(printf '%s\n' "$out" | grep '^summary ' | tail -1 | sed 's/^summary //') at $VM_CPU_CAP CPUs (verdict=vm_cpu_caps)"
  done <<EOF2
$profiles
EOF2
}

vm_build_cache_cap() { # <ctx> <cap: NNgb>
  local ctx=$1 cap=$2 total
  total=$("$VM_DOCKER" --context "$ctx" builder du 2>/dev/null | awk '/^Total:/{print $2}' | tail -1)
  local over; over=$(awk -v t="${total:-0B}" -v c="$cap" 'function b(x,  n,u){n=x+0; u=toupper(x); sub(/^[0-9.]+/,"",u); return n*(u~/^T/?1e12:u~/^G/?1e9:u~/^M/?1e6:u~/^K/?1e3:1)} BEGIN{print (b(t) > b(c)) ? 1 : 0}')
  if [ "$over" != 1 ]; then echo "build cache ${total:-0B} is within $cap"; echo "Total: 0B"; return 0; fi
  echo "build cache ${total} is over $cap, pruning cache unused for $VM_PRUNE_CAP_AGE"
  "$VM_DOCKER" --context "$ctx" builder prune -af --filter "until=$VM_PRUNE_CAP_AGE" 2>&1 | grep -i 'total' | tail -1
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
  # SUPERSEDED TAGS GO TOO (Mac disk RCA 20261010-162636). mxp-gs12 builds a
  # 9.84 GB mixpeek/standalone image per commit (10:41, 12:53, 13:21, 14:12 on
  # 2026-10-10) and each stayed until it was 6 h old, so four unused copies
  # (39 GB) sat in the VM and the host burned ~17 G/h for the afternoon. An
  # unused image older than a NEWER image of the same repository is a
  # superseded build: it goes once it is VM_IMAGE_SUPERSEDE_MIN old (30 min,
  # so a build still tagging is never raced). In-use images are always kept.
  plan=$("$VM_DOCKER" --context "$ctx" image inspect $ids | USED="$used" SECS="$secs" SUPERSEDE="$(( ${VM_IMAGE_SUPERSEDE_MIN:-30} * 60 ))" python3 -c '
import json, os, sys, time
import calendar
used = set(os.environ["USED"].split()); cutoff = time.time() - int(os.environ["SECS"])
floor = time.time() - int(os.environ["SUPERSEDE"])
def utc(v):
    v = (v or "").rstrip("Z")
    if not v or v.startswith("0001-"): return None
    off = 0
    for sign in "+-":
        i = v.rfind(sign, 19)
        if i > 0:
            h, m = v[i+1:].split(":"); off = (int(h) * 3600 + int(m) * 60) * (1 if sign == "+" else -1); v = v[:i]; break
    return calendar.timegm(time.strptime(v[:19], "%Y-%m-%dT%H:%M:%S")) - off
images = json.load(sys.stdin)
def when(im): return utc((im.get("Metadata") or {}).get("LastTagTime")) or utc(im.get("Created"))
def repos(im): return {t.rsplit(":", 1)[0] for t in (im.get("RepoTags") or []) if ":" in t and not t.startswith("<none>")}
newest = {}
for im in images:
    t = when(im)
    if t is None: continue
    for r in repos(im): newest[r] = max(newest.get(r, 0), t)
for im in images:
    if im["Id"] in used: continue
    t = when(im)
    if t is None: continue
    if t <= cutoff: print(im["Id"], im.get("Size", 0), "age")
    elif t <= floor and any(newest.get(r, 0) > t for r in repos(im)): print(im["Id"], im.get("Size", 0), "superseded")
') || return 1
  local n=0 bytes=0 sz why sup=0
  while read -r id sz why; do
    [ -n "$id" ] || continue
    if "$VM_DOCKER" --context "$ctx" rmi "$id" >/dev/null 2>&1; then
      echo "deleted: $id${why:+ ($why)}"; n=$((n+1)); bytes=$((bytes + sz))
      [ "$why" = superseded ] && sup=$((sup+1))
    fi
  done <<EOF
$plan
EOF
  if [ "$sup" -gt 0 ]; then
    echo "Total reclaimed space: $(awk -v b="$bytes" 'BEGIN{printf "%.2fGB", b/1e9}') ($n image(s), $sup superseded, verdict=vm_images_pruned_by_tag_age)"
  else
    echo "Total reclaimed space: $(awk -v b="$bytes" 'BEGIN{printf "%.2fGB", b/1e9}') ($n image(s), verdict=vm_images_pruned_by_tag_age)"
  fi
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
    if [ "$dry" = 1 ]; then echo "mac-cleanup:   would cap the build cache of and trim colima VM $p (dry run)"; continue; fi
    cap_vm_build_cache "$p"
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

# Counters the cleanup arms set. Zero here, because tick_arm can skip any arm and
# the report and done line below read them under set -u.
TARGETS_REAPED=0; TARGETS_REAPED_KB=0; WORKTREES_REAPED=0; CLAUDE_TMP_REAPED=0; CLAUDE_TMP_REAPED_KB=0
VMS_PRUNED=0; VMS_TRIMMED=0; VMS_STOPPED=0; DD_STOPPED=0

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

fseventsd_reading() {
  local pid; pid=$(pgrep -x fseventsd 2>/dev/null | head -1)
  [ -n "$pid" ] || return 0
  echo "$(to_gb "$(top -l 1 -pid "$pid" -stats mem 2>/dev/null | tail -1 | tr -d ' ')") $(ps -o pcpu= -p "$pid" 2>/dev/null | tr -d ' ')"
}
# Baseline BEFORE any arm deletes a file. The reap arms below can remove a
# cargo target or a scratch tree (hundreds of thousands of files), and that
# deletion is fseventsd churn this tick made itself. Reading CPU only after them
# escalated the tick's own aftermath as a root cause (RCA 20261012-161555: 83% CPU
# right after 2 targets / 7.8G were reaped, 5-15% a few minutes later).
fse_pre=$($FSEVENTSD_CMD 2>/dev/null)

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
#
# ONLY ORPHANS (2026-10-28). This arm used to kill every shell past the age
# floor, so a live lane's long-running background loop died at the first :57
# tick after 6h: mxp-gs12 lost fallback-alarm (6h39m), restore-watch (6h05m) and
# export-watch (06:29:07, "killed stale shell-snapshot pid=57216") to it. A shell
# whose parent is still its claude process is that session's work; only a shell
# reparented to pid 1 has outlived its session. An unreadable parent is kept.
stale_killed=0; stale_found=0; stale_live=0
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
  ppid=$($STALE_PPID_CMD "$pid" 2>/dev/null | tr -d ' ')
  if [ "$ppid" != 1 ]; then
    stale_live=$((stale_live+1))
    echo "mac-cleanup: kept shell-snapshot pid=$pid age=${elapsed_raw} parent=${ppid:-unreadable}: its session is alive (verdict=stale_shell_kept_live_session)"
    continue
  fi
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
echo "mac-cleanup: shell-snapshots found=${stale_found} killed=${stale_killed} kept_live=${stale_live} (floor ${STALE_SHELL_SNAPSHOT_H}h, orphans only)"

# ── act: stop orphaned sqlite .backup copies of amux's databases ─────────────
reap_orphan_sqlite_backups "$DRY"
system_health "$DRY"

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
tick_arm "cargo targets" TARGET_SCAN_S TARGET_BUDGET_S && reap_idle_cargo_targets "$TARGET_ROOTS" "$tgt_idle" "$DRY"
tick_arm "superseded artifacts" TARGET_SCAN_S TARGET_BUDGET_S && prune_superseded_artifacts "$TARGET_ROOTS" "$SUPERSEDED_KEEP" "$SUPERSEDED_MIN_AGE_MIN" "$DRY" "$TARGET_BUDGET_S"
VMS_PRUNED=0
# Images BEFORE the trim (Mac disk RCA 20261010-162636): the trim is what hands
# freed VM blocks back to the host, and with images swept after it, the 2-4
# superseded 9.84 GB builds a tick removed stayed allocated on the host until
# the next hourly trim.
[ "${AMUX_CLEANUP_VM_IMAGE_PRUNE:-1}" = 1 ] && tick_arm "vm images" && prune_vm_images "$DRY"
[ "$HEALTH_PROBES" != off ] && tick_arm "vm cpu caps" && cap_vm_container_cpus "$DRY"
if tick_arm "vm trim"; then
  if [ "$tgt_idle" != "$TARGET_IDLE_H" ]; then prune_vm_build_caches "$DRY" "$tgt_free"; else trim_vms "$DRY"; fi
fi
# Session scratch BEFORE user/lane tmp (Mac self RCA 20261019-175342): it ran
# last, after lane tmp, which spends its whole 90s budget every run and resumes
# from a cursor next time. Under load the tick reached claude tmp with nothing
# left 4 of 9 runs, so the arm over the largest tree (101G) was the one skipped.
ctmp_idle=$CLAUDE_TMP_IDLE_H
if [ "$tgt_idle" != "$TARGET_IDLE_H" ] && [ "$ctmp_idle" -gt 0 ] && [ "$CLAUDE_TMP_TIGHT_IDLE_H" -lt "$ctmp_idle" ]; then ctmp_idle=$CLAUDE_TMP_TIGHT_IDLE_H; fi
if tick_arm "claude tmp"; then
  # The quota pass is a share of the arm's budget, so shrink both in proportion.
  ct_left=$TICK_ARM_SHARE
  if [ "$CLAUDE_TMP_BUDGET_S" -gt "$ct_left" ]; then
    CLAUDE_TMP_QUOTA_BUDGET_S=$(( CLAUDE_TMP_QUOTA_BUDGET_S * ct_left / CLAUDE_TMP_BUDGET_S )); CLAUDE_TMP_BUDGET_S=$ct_left
  fi
  reap_claude_tmp "$CLAUDE_TMP_ROOT" "$ctmp_idle" "$DRY"
fi
[ "${AMUX_CLEANUP_USER_TMP:-1}" = 1 ] && tick_arm "user tmp" && reap_user_tmp "$DRY"
[ "${AMUX_CLEANUP_LANE_TMP:-1}" = 1 ] && tick_arm "lane tmp" LANE_TMP_BUDGET_S && reap_lane_tmp "$DRY"
VMS_STOPPED=0
vm_level=$($PRESSURE_CMD 2>/dev/null); case "$vm_level" in ''|*[!0-9]*) vm_level=-1 ;; esac
if [ "$vm_level" -ge 2 ]; then stop_idle_vms "$DRY" "memory pressure $vm_level"; stop_idle_docker_desktop "$DRY" "memory pressure $vm_level"
elif [ "$tgt_idle" != "$TARGET_IDLE_H" ]; then stop_idle_vms "$DRY" "disk under ${TARGET_TIGHT_FREE_GB}G"; fi
tick_arm "worktrees" WORKTREE_BUDGET_S && reap_idle_worktrees "$WORKTREE_ROOTS" "$WORKTREE_IDLE_H" "$DRY"

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
    # The full command is read, then shortened for the line: a cut on ps output
    # is the truncation class test-mac-cleanup-tick.sh pins (AMUX-4686).
    fam_cmd=$(ps -o command= -p "$fam_ppid" 2>/dev/null)
    FAMILY_DESC="${fam_gb}G (${fam_pct}% of RAM, share ${FAMILY_SHARE_PCT}%) in ${fam_n} children of pid ${fam_ppid}, lane $(lane_for_pid "$fam_ppid" "$(pane_map)"): ${fam_cmd:0:120}"
    echo "mac-cleanup: FAMILY ${fam_gb}G (${fam_pct}% of RAM) in ${fam_n} children of pid ${fam_ppid}, oldest ${fam_hours}h — $(owner_label_for_pid "$fam_ppid") — reported, never killed"
  elif family_too_old "$fam_age" "$FAMILY_AGE_H"; then
    echo "mac-cleanup: FAMILY ${fam_gb}G (${fam_pct}% of RAM) in ${fam_n} children of pid ${fam_ppid} has run ${fam_hours}h, past the ${FAMILY_AGE_H}h ceiling — $(owner_label_for_pid "$fam_ppid")"
  else
    echo "mac-cleanup: largest family ${fam_gb}G (${fam_pct}% of RAM) in ${fam_n} children of pid ${fam_ppid}, under the ${FAMILY_SHARE_PCT}% share and ${FAMILY_AGE_H}h ceiling"
  fi
else
  echo "mac-cleanup: family scan produced no rows (ps unavailable?)"
fi

fse_reading=$($FSEVENTSD_CMD 2>/dev/null)
fse_verdict=""
if [ -n "$fse_reading" ]; then
  fse_gb=${fse_reading%% *}; fse_cpu=${fse_reading#* }
  fse_cpu_int=${fse_cpu%%.*}
  fse_hot=0
  if needs_reboot "$fse_gb" "$FSEVENTSD_REBOOT_GB"; then fse_hot=1; fi
  if [ "${fse_cpu_int:-0}" -ge "$FSEVENTSD_CPU_PCT" ] 2>/dev/null; then fse_hot=1; fi
  # CPU counts only when it was already hot before this tick deleted anything.
  # Memory is not reset by a burst, so the GB threshold stands on its own.
  fse_pre_cpu=${fse_pre#* }; fse_pre_cpu_int=${fse_pre_cpu%%.*}
  if [ "$fse_hot" = 1 ] && [ -n "$fse_pre" ] && ! needs_reboot "$fse_gb" "$FSEVENTSD_REBOOT_GB" \
     && [ "${fse_pre_cpu_int:-0}" -lt "$FSEVENTSD_CPU_PCT" ] 2>/dev/null; then
    fse_hot=0
    echo "mac-cleanup: fseventsd ${fse_cpu:-?}% CPU after this tick's own deletions, ${fse_pre_cpu:-?}% before them: the burst is this tick's, not a constraint (verdict=fseventsd_burst_self_inflicted)"
  fi
  if [ "$fse_hot" = 1 ] && fseventsd_try_restart "$fse_gb" "$DRY"; then
    fse_hot=0
    reported=$((reported+1))
  fi
  if [ "$fse_hot" = 1 ]; then
    echo "mac-cleanup: fseventsd holds ${fse_gb}G at ${fse_cpu:-?}% CPU and is SIP-protected; the feed and the owner's remedy follow"
    # In this shell, not $(...): churn_census sets CHURN_COMPLETE, which a subshell would lose.
    churn_f=$(mktemp "${TMPDIR:-/tmp}/churn-out.XXXXXX")
    churn_unit="files modified in ${CHURN_MIN}m"; churn_top_unit="files in ${CHURN_MIN}m"; churn_floor=$CHURN_FILES
    churn_measured=1
    if ! tick_arm "fseventsd churn census" CHURN_BUDGET_S; then CHURN_COMPLETE=no; churn_measured=0; : > "$churn_f"
    elif [ -n "$CHURN_SAMPLE_CMD" ]; then
      churn_s=$CHURN_SAMPLE_S; [ "$churn_s" -le "$CHURN_BUDGET_S" ] 2>/dev/null || churn_s=$CHURN_BUDGET_S
      # The census floor is per CHURN_MIN minutes; scale it to the sample window.
      churn_floor=$(( (CHURN_FILES * churn_s + CHURN_MIN * 60 - 1) / (CHURN_MIN * 60) )); [ "$churn_floor" -ge 1 ] || churn_floor=1
      churn_unit="FSEvents in ${churn_s}s"; churn_top_unit=$churn_unit
      churn_sample "$churn_s" "$churn_floor" > "$churn_f"
      echo "mac-cleanup:   fseventsd feed: sampled FSEvents for ${churn_s}s under $CHURN_SAMPLE_ROOTS, deletions included (verdict=fseventsd_churn_sampled)"
    else
      churn_census "$CHURN_ROOTS" "$CHURN_MIN" "$CHURN_BUDGET_S" "$CHURN_FILES" > "$churn_f"
    fi
    churn=$(cat "$churn_f"); rm -f -- "${churn_f:?}"
    if [ "${CHURN_DELIVERED:-unknown}" = no ]; then
      # The sample cannot see the feed: fseventsd delivered none of the control
      # files, so whatever it did report is a fraction of the churn and an empty
      # list is its silence. The daemon is wedged or far behind either way, and
      # a restart is the remedy that needs no measurement of the feed.
      fse_top="not measured: fseventsd delivered 0 of ${CHURN_CONTROL_N} control files written during the ${churn_s}s sample, so it is wedged or far behind and the sample cannot see the feed"
      echo "mac-cleanup:   fseventsd feed: $fse_top (verdict=fseventsd_not_delivering)"
      fse_spin=1; fse_wedged=1
    elif [ -n "$churn" ]; then
      printf '%s\n' "$churn" | while read -r n d; do echo "mac-cleanup:   fseventsd feed: $n $churn_unit under $d"; done
      fse_top=$(printf '%s\n' "$churn" | head -1 | { read -r n d; echo "$n $churn_top_unit under $d"; })
    elif [ "$churn_measured" = 0 ]; then
      # Nothing was sampled, so there is no "no directory over N" to report: that
      # line used to print here and reached the escalation as the measured feed
      # (Mac fseventsd RCA 20261026-130357, with the census skipped out of time).
      fse_top="not measured: the churn census was skipped because the tick ran out of time"
      echo "mac-cleanup:   fseventsd feed: $fse_top (verdict=fseventsd_feed_unmeasured)"
    elif [ -n "$CHURN_SAMPLE_CMD" ]; then
      fse_top="no directory over ${churn_floor} ${churn_unit} (FSEvents sample, deletions included)"
      echo "mac-cleanup:   fseventsd feed: $fse_top"
      # Hot on CPU alone with nothing to process is a stuck fseventsd thread
      # (Mac fseventsd RCA 20261021-135901: one core for 40+ min at ~20 FSEvents/s
      # disk-wide, after one thread logged 6,200 group-list lookups in 43s).
      # There is no churn to stop; restarting the daemon clears it, and its memory
      # with it: the stuck pid 34409 grew 0.81G -> 4.0G in five hours with no
      # churn, and past the memory threshold this used to flip back to "stop the
      # file churn, then reboot" (RCA fseventsd 20261021-195930).
      if [ "${CHURN_COMPLETE:-yes}" != no ]; then
        fse_spin=1
        echo "mac-cleanup:   fseventsd is spinning with no file churn to feed it; owner runs: sudo killall fseventsd (launchd restarts it; no reboot needed) (verdict=fseventsd_spin_no_churn)"
      fi
    else
      fse_top="no directory over ${CHURN_FILES} modified files in ${CHURN_MIN}m (scan complete=${CHURN_COMPLETE}; deletions are not counted)"
      echo "mac-cleanup:   fseventsd feed: $fse_top"
    fi
    fse_pid=$($FSEVENTSD_PID_CMD 2>/dev/null | head -1 || true)
    fse_esc_pid=$(state_get "$STATE_DIR/state" esc_fseventsd_pid || true)
    fse_esc_at=$(state_get "$STATE_DIR/state" esc_fseventsd || true)
    fse_feed_n=${fse_top%% *}; case "$fse_feed_n" in ''|*[!0-9]*) fse_feed_n=0 ;; esac
    if [ -n "$fse_pid" ] && [ "$fse_pid" = "$fse_esc_pid" ] \
       && [ $(( $(date +%s) - ${fse_esc_at:-0} )) -lt $(( FSEVENTSD_SPIN_REMIND_H * 3600 )) ] \
       && { [ "${fse_spin:-0}" = 1 ] || [ "$fse_feed_n" -lt $(( churn_floor * FSEVENTSD_CHURN_NEW_X )) ]; }; then
      echo "mac-cleanup:   fseventsd pid $fse_pid (${fse_gb}G) is the process the last fseventsd escalation named $(( ($(date +%s) - fse_esc_at) / 60 ))m ago and it is still hot$( [ "${fse_spin:-0}" = 1 ] || echo ", with churn under ${FSEVENTSD_CHURN_NEW_X}x the floor"): the owner's restart is pending (sudo killall fseventsd, which also returns its memory); not escalated again until a new pid, churn over ${FSEVENTSD_CHURN_NEW_X}x the floor, or ${FSEVENTSD_SPIN_REMIND_H}h (verdict=fseventsd_spin_owner_pending)"
      reported=$((reported+1)); fse_owner_pending=1
    elif [ "${fse_wedged:-0}" = 1 ]; then
      fse_verdict="fseventsd at ${fse_cpu:-?}% CPU holding ${fse_gb}G is not delivering file events ($fse_top): the owner restarts it with sudo killall fseventsd (launchd brings it back, no reboot), then the next tick can measure the feed"
      [ "${FSE_RESTART_NEEDS_SUDOERS:-0}" = 1 ] && fse_verdict="$fse_verdict. The tick restarts it itself once the owner adds: $FSEVENTSD_SUDOERS_LINE"
    elif [ "${fse_spin:-0}" = 1 ]; then
      fse_verdict="fseventsd at ${fse_cpu:-?}% CPU (over ${FSEVENTSD_CPU_PCT}%) holding ${fse_gb}G with no file churn to feed it ($fse_top): a stuck fseventsd thread, not a writer; the owner restarts it with sudo killall fseventsd (launchd brings it back, no reboot)"
    else
      echo "mac-cleanup:   fseventsd remedy: stop the churn, then the owner runs sudo killall fseventsd to return its memory (launchd restarts it; no reboot)"
      fse_verdict="fseventsd holds ${fse_gb}G at ${fse_cpu:-?}% CPU (over ${FSEVENTSD_REBOOT_GB}G or ${FSEVENTSD_CPU_PCT}%); stop the file churn feeding it, then sudo killall fseventsd returns its memory (launchd restarts it, no reboot). Top feed: $fse_top"
      [ "${FSE_RESTART_NEEDS_SUDOERS:-0}" = 1 ] && fse_verdict="$fse_verdict. The tick restarts it itself once the owner adds: $FSEVENTSD_SUDOERS_LINE"
    fi
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
CPU_IDLE_CMD=${AMUX_CLEANUP_CPU_IDLE_CMD:-top -l 2 -s 1 -n 0}
cpu_idle=$($CPU_IDLE_CMD 2>/dev/null | awk '/^CPU usage/ {for (i=1;i<=NF;i++) if ($i ~ /idle/) v=$(i-1)} END{sub(/%/,"",v); print v}'); case "$cpu_idle" in ''|*[!0-9.]*) cpu_idle=-1 ;; esac
ncpu=$(sysctl -n hw.ncpu 2>/dev/null || echo 0)
STATE="$STATE_DIR/state"
prev_ts=$(state_get "$STATE" last_ts); prev_free=$(state_get "$STATE" last_disk_free_gb)
prev_swt=$(state_get "$STATE" last_swap_total_mb || true)   # read before this run's state write below
HISTORY_CMD=${AMUX_CLEANUP_HISTORY_CMD:-curl -sk --max-time 15 $(amux url 2>/dev/null || echo https://localhost:8824)/api/metrics/host/history?since_h=2}
read -r burn htf hist_n hist_span hist_run <<EOF
$($HISTORY_CMD 2>/dev/null | burn_from_history "$BURN_MIN_GBH" "$(date +%s)" "$disk_now")
EOF
burn_src="history ${hist_n} samples over ${hist_span}h"
[ "${hist_run:-}" = "run" ] && burn_src="$burn_src + this run's post-cleanup ${disk_now}G"
if [ "$htf" = "-" ] && [ "$burn" = "-" ]; then
read -r burn htf <<EOF
$(disk_trend "${prev_ts:-0}" "${prev_free:-0}" "$now" "$disk_now" "$BURN_MIN_GBH")
EOF
  burn_src="2 readings (history had ${hist_n} samples over ${hist_span}h)"
fi
if [ "$burn" = "-" ]; then burn_txt="unmeasured (no previous reading)"; else burn_txt="${burn}G/h from ${burn_src}"; fi
if [ "$htf" = "-" ]; then htf_txt="not filling"; else htf_txt="full in ${htf}h"; fi
[ "$DRY" = "1" ] || state_put "$STATE" "last_ts=$now" "last_disk_free_gb=$disk_now" "last_swap_total_mb=$swap_total_now"
tick_mark verdicts
verdicts=$(classify_constraints "$disk_now" "$burn" "$htf" "$level_now" "$swap_free_now" "$load15" "$ncpu" "$fam_over" "$swap_total_now" "$cpu_idle")
swap_note=$(swap_burn_note "$burn" "${prev_swt:-0}" "$swap_total_now" "$(awk -v a="$now" -v b="${prev_ts:-0}" 'BEGIN{ if (b > 0) printf "%.3f", (a-b)/3600; else print 0 }')")
if [ -n "$swap_note" ] && printf '%s\n' "$verdicts" | grep -q '^disk burning'; then
  verdicts=$(printf '%s\n' "$verdicts" | N="$swap_note" awk '/^disk burning/{$0=$0 ENVIRON["N"]} {print}')
  echo "mac-cleanup: disk burn is swap growth${swap_note#;} (verdict=disk_burn_is_swap)"
fi
# Say so when a high load was judged not to be CPU, so a sweep can tell
# "load was high but the CPU idle" from "load was fine".
if awk -v l="$load15" -v n="$ncpu" -v cs="$CPU_SHARE" -v i="$cpu_idle" -v m="$CPU_IDLE_MAX_PCT" 'BEGIN{exit !(l>=0 && n>0 && l/n>cs && i>=m)}'; then
  echo "mac-cleanup: load15=${load15}/${ncpu} is over ${CPU_SHARE} of cores but the CPU is ${cpu_idle}% idle: threads blocked on IO, not CPU (verdict=load_not_cpu)"
fi
# Say so when a low swap reading was let through because swap can still grow,
# so a sweep can tell "judged growable" from "never measured".
if [ "$level_now" = 1 ] && printf '%s' "$swap_free_now" | grep -qE '^[0-9.]+$' && \
   awk -v sw="$swap_free_now" -v swf="$SWAP_FREE_FLOOR_MB" -v df="$disk_now" -v sgd="$SWAP_GROW_DISK_GB" \
     'BEGIN{exit !(sw < swf && df >= sgd)}'; then
  echo "mac-cleanup: swap ${swap_free_now}MB free of ${swap_total_now}MB but pressure is normal and ${disk_now}G of disk lets macOS add swap: not a constraint (verdict=swap_low_growable)"
fi
if [ -n "${fse_verdict:-}" ]; then verdicts=$(printf '%s\n%s\n' "$verdicts" "$fse_verdict" | grep . || true); fi
if [ -n "${HEALTH_VERDICTS:-}" ]; then verdicts=$(printf '%s\n%s\n' "$verdicts" "$HEALTH_VERDICTS" | grep . || true); fi
# A cpu constraint names who is using the CPU, so the escalation starts from
# the consumers rather than from a load number (check_utilization).
if [ -n "${UTIL_SUMMARY:-}" ]; then verdicts=$(printf '%s\n' "$verdicts" | U="$UTIL_SUMMARY" awk '/^cpu /{$0=$0 " (who: " ENVIRON["U"] ")"} {print}'); fi
echo "mac-cleanup: assess disk=${disk_now}G burn=${burn_txt} ${htf_txt} pressure=${level_now} swap_free=${swap_free_now}MB swap_total=${swap_total_now}MB load15=${load15}/${ncpu}"
reclaim_started=0
if printf '%s\n' "$verdicts" | grep -q '^disk '; then
  if [ "$DRY" = 1 ]; then
    echo "mac-cleanup: disk constraint: would start the disk reclaim (dry run)"
  elif ! escalation_due "$STATE" reclaim "$now" "$RECLAIM_TRIGGER_COOLDOWN_H"; then
    echo "mac-cleanup: disk constraint: disk reclaim already started under ${RECLAIM_TRIGGER_COOLDOWN_H}h ago (verdict=disk_reclaim_cooldown)"
  else
    rcode=$($RECLAIM_TRIGGER_CMD 2>/dev/null); case "$rcode" in
      2??) state_put "$STATE" "esc_reclaim=$now"; reclaim_started=1
           echo "mac-cleanup: disk constraint: started the disk reclaim ($RECLAIM_SCHEDULE) (verdict=disk_reclaim_triggered)" ;;
      *)   echo "mac-cleanup: disk constraint: could not start the disk reclaim (http ${rcode:-none}) (verdict=disk_reclaim_trigger_failed)" ;;
    esac
  fi
fi
# A cargo test binary (target/<profile>/deps/<crate>-<hash>) runs one test
# thread per core unless RUST_TEST_THREADS caps it, so one plain `cargo test`
# takes all 28 cores of this shared Mac (2026-10-28: amux at 1088% CPU from one
# test binary). The repo's .cargo/config.toml sets the cap; this names a binary
# running without one, so the next escalation shows the cause, not just a pid.
test_binary_note() { # pid command-path
  case "$2" in */deps/*-[0-9a-f][0-9a-f][0-9a-f]*) ;; *) return 0 ;; esac
  local thr cap
  thr=$(ps -M -p "$1" 2>/dev/null | tail -n +2 | wc -l | tr -d ' ')
  cap=$(ps -E -o command= -p "$1" 2>/dev/null | grep -o 'RUST_TEST_THREADS=[0-9]*' | head -1)
  if [ -n "$cap" ]; then
    echo " [cargo test binary, $thr threads, $cap]"
  else
    echo " [cargo test binary, $thr threads, RUST_TEST_THREADS unset: one test thread per core (verdict=cpu_test_threads_unbounded)]"
  fi
}
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
      echo "- ${cpu}% pid=$pid $(basename "$cmd") — $(owner_label_for_pid "$pid"), lane: $(lane_for_pid "$pid" "$pmap")$(test_binary_note "$pid" "$cmd")"; done
    if printf '%s\n' "$verdicts" | grep -q '^disk '; then
      echo; echo "## Files over 500M written in the last hour (the writers)"
      # Open files first: wherever they live, and only seconds of the budget.
      open_file_writers "${AMUX_CLEANUP_OPEN_WRITERS_IV_S:-10}" "$pmap"
      IFS=':' read -r -a wroots <<< "$TARGET_ROOTS"
      # ONE budget for the whole search, not one per root: 9 roots at 45s each
      # was 405s on its own and pushed the tick past the scheduler's 600s kill.
      # Few-file roots first (VM disks, the amux home), and each remaining root
      # gets an equal share of what is left, so one tree full of build files
      # cannot spend it all: the first cut spent 60s in the scratchpads and
      # searched 1 root of 9. Cargo's internals are pruned for the same reason.
      wlist=("$HOME/.colima" "$HOME/.amux"); for r in ${wroots[@]+"${wroots[@]}"}; do wlist+=("${r%@*}"); done
      # Inside the tick's deadline too, leaving 30s to write and send the escalation.
      w_left=$(( TICK_BUDGET_S - SECONDS - 30 )); [ "$w_left" -ge 0 ] || w_left=0
      [ "$WRITERS_BUDGET_S" -le "$w_left" ] || WRITERS_BUDGET_S=$w_left
      wdeadline=$(( $(date +%s) + WRITERS_BUDGET_S )); wn=${#wlist[@]}; wi=0
      for r in "${wlist[@]}"; do
        wi=$((wi+1)); [ -d "$r" ] || continue
        wleft=$(( wdeadline - $(date +%s) )); if [ "$wleft" -le 0 ]; then echo "SEARCH-CUT $r" >&2; continue; fi
        wslice=$(( wleft / (wn - wi + 1) )); [ "$wslice" -ge 3 ] || wslice=3
        { perl -e 'alarm shift; exec @ARGV' "$wslice" find "$r" -xdev \( -name node_modules -o -name .git -o -name deps -o -name incremental -o -name .fingerprint -o -name build \) -prune -o -type f -size +500M -mmin -60 -exec stat -f '%b %z %N' {} + 2>/dev/null; } 2>/dev/null || echo "SEARCH-CUT $r" >&2
      done 2>"$STATE_DIR/writers.cut" | sort -u | sort -rn | head -15 | awk '{a=$1*512/2^30; z=$2/2^30; $1=$2=""; sub(/^  /,""); printf "- %.1fG allocated (%.1fG apparent) %s\n", a, z, $0}' > "$STATE_DIR/writers.list"
      cat "$STATE_DIR/writers.list"
      for vp in $(sed -n 's#.*/_disks/colima-\([^/ ]*\)/datadisk.*#\1#p' "$STATE_DIR/writers.list" | sort -u); do
        vw=$(vm_container_writers "$vp")
        if [ -n "$vw" ]; then echo "Inside the colima-$vp VM disk, by write rate since each container started:"; echo "$vw"; fi
      done
      if [ -s "$STATE_DIR/writers.cut" ]; then echo "(search budget of ${WRITERS_BUDGET_S}s: $(grep -c . "$STATE_DIR/writers.cut") of ${#wlist[@]} root(s) not fully searched: $(sed 's/^SEARCH-CUT //' "$STATE_DIR/writers.cut" | tr '\n' ' '))"; fi
      echo "(allocated is what the disk actually holds; a sparse VM disk's apparent size is its ceiling, not its use)"
    fi
    echo; echo "## Full tick output"; echo "~/.amux/logs/mac-cleanup-tick.last"
  } > "$bundle"
  grep -a 'verdict=vm_container_writer\|verdict=open_file_writer' "$bundle" | sed 's/^- /mac-cleanup: disk writer /'
  while IFS= read -r v; do
    cls=${v%% *}
    if [ "$DRY" = "1" ]; then echo "mac-cleanup: constraint $v — would escalate (dry run)"; continue; fi
    if [ "$cls" = memory ] && [ "${fse_owner_pending:-0}" = 1 ] \
       && awk -v g="${fse_gb:-0}" -v t="$MEM_FSE_HOLDER_GB" 'BEGIN{exit !(g+0 >= t+0)}'; then
      echo "mac-cleanup: constraint $v — not escalated: macOS fseventsd holds ${fse_gb}G and its restart is already with the owner (sudo killall fseventsd, or the sudoers line that lets this tick do it); escalating memory again names the same holder and the same step (verdict=memory_owner_pending_fseventsd)"
      continue
    fi
    if ! escalation_due "$STATE" "$cls" "$now" "$ESCALATE_COOLDOWN_H"; then
      echo "mac-cleanup: constraint $v — escalation suppressed (last $cls escalation under ${ESCALATE_COOLDOWN_H}h ago)"; continue
    fi
    msg="$STATE_DIR/rca/$stamp.$cls.msg"; cardf="$STATE_DIR/rca/$stamp.$cls.card"
    nowsnap="disk=${disk_now}G burn=${burn_txt} load15=${load15}/${ncpu} pressure=${level_now} swap_free=${swap_free_now}MB"
    plast=$(state_get "$STATE" "esc_$cls"); pstamp=$(state_get "$STATE" "esc_${cls}_stamp"); psnap=$(state_get "$STATE" "esc_${cls}_snap")
    page=-; pcard=-
    if [ -n "$plast" ]; then
      page=$(awk -v a="$now" -v b="$plast" 'BEGIN{printf "%.1f", (a-b)/3600}')
      # Squeeze, not delete: a lane with no board writes its card as a sentence
      # (none:board_off. ...), and deleting whitespace quoted it back as one
      # unreadable word (memory RCA 20261029-063250).
      pcard=$(tr -s '[:space:]' ' ' < "$STATE_DIR/rca/$pstamp.$cls.card" 2>/dev/null | sed 's/^ //; s/ $//'); [ -n "$pcard" ] || pcard="none written (the previous turn did not record one)"
      [ -n "$psnap" ] || psnap="not recorded"
    fi
    escalation_message "$cls" "$v" "$bundle" "$cardf" "$nowsnap" "$page" "$pcard" "$psnap" > "$msg"
    cmd=${ESCALATE_CMD//TARGET/$ESCALATE_TO}; cmd=${cmd//FILE/$msg}
    if sendout=$($cmd 2>&1); then
      state_put "$STATE" "esc_$cls=$now" "esc_${cls}_stamp=$stamp" "esc_${cls}_snap=$nowsnap" "sendfail_$cls=0"; escalated=$((escalated+1))
      [ "$cls" = fseventsd ] && [ -n "${fse_pid:-}" ] && state_put "$STATE" "esc_fseventsd_pid=$fse_pid"
      [ "$cls" = crashes ] && [ -n "${RESPAWN_ESC_KEY:-}" ] && state_put "$STATE" "esc_respawn_key=$RESPAWN_ESC_KEY"
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
tick_mark end
echo "mac-cleanup: done elapsed=${SECONDS}s slowest=${TICK_SLOWEST:-none} purge=${purged%% *} agents_restarted=${agents_restarted}/${agents_checked} stale_shells=${stale_killed} sqlite_orphans=${sqlite_stopped} targets_reaped=${TARGETS_REAPED} claude_tmp_reaped=${CLAUDE_TMP_REAPED:-0} cpu_restarted=${CPU_RESTARTED:-0} test_orphans=${TEST_ORPHANS_STOPPED:-0} crash_pruned=${CRASH_PRUNED:-0} util_reniced=${UTIL_RENICED:-0} daemons_demoted=${DEMOTED:-0} reclaim_started=${reclaim_started:-0} skipped=${TICK_SKIPPED:-none} sections=${TICK_SECTIONS:-none} constraints=$(printf '%s' "$verdicts" | grep -c . || true) escalated=${escalated} reported=${reported}"
exit 0
