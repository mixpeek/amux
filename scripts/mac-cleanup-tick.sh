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

DRY=0
for a in "$@"; do
  case "$a" in
    --dry-run) DRY=1 ;;
    -h|--help) sed -n '2,/^set -uo pipefail$/p' "$0" | sed '$d'; exit 0 ;;
    *) echo "unknown argument: $a" >&2; exit 2 ;;
  esac
done

# ── knobs ────────────────────────────────────────────────────────────────────
# Pressure >= this purges. 2 is the kernel's "warn"; it self-clears often, which
# is why the PAGING tripwire ignores 2 and this one does not: dropping caches is
# free, so acting early is cheap and acting late is not.
PRESSURE_PURGE=${AMUX_CLEANUP_PRESSURE_PURGE:-2}
FREE_FLOOR_GB=${AMUX_CLEANUP_FREE_FLOOR_GB:-4}
AGENT_LEAK_GB=${AMUX_CLEANUP_AGENT_LEAK_GB:-2}
AGENTS=${AMUX_CLEANUP_AGENTS:-com.procwarden.menubar}
REPORT_GB=${AMUX_CLEANUP_REPORT_GB:-10}
FSEVENTSD_REBOOT_GB=${AMUX_CLEANUP_FSEVENTSD_REBOOT_GB:-20}
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
TARGET_KEEP=${AMUX_CLEANUP_TARGET_KEEP:-$HOME/.amux/rust-build-target:$HOME/.ao/data/cargo-target-shared:${CARGO_TARGET_DIR:-}}
TARGET_DEPTH=${AMUX_CLEANUP_TARGET_DEPTH:-8}
TARGET_SCAN_S=${AMUX_CLEANUP_TARGET_SCAN_S:-120}
TARGET_WALK_S=${AMUX_CLEANUP_TARGET_WALK_S:-60}
TARGET_BUDGET_S=${AMUX_CLEANUP_TARGET_BUDGET_S:-300}
LSOF_CMD=${AMUX_CLEANUP_LSOF_CMD:-lsof -nP}
# Assessment (DESKT-57). A disk is constrained under DISK_FLOOR_GB, or when the
# burn since the previous tick would fill it within HOURS_TO_FULL. Memory is
# constrained when the kernel still reports pressure after the purge arm ran,
# or swap has under SWAP_FREE_FLOOR_MB left. CPU is constrained when the 15-min
# load average exceeds CPU_SHARE of the cores.
DISK_FLOOR_GB=${AMUX_CLEANUP_DISK_FLOOR_GB:-150}
HOURS_TO_FULL=${AMUX_CLEANUP_HOURS_TO_FULL:-24}
BURN_MIN_GBH=${AMUX_CLEANUP_BURN_MIN_GBH:-1}
SWAP_FREE_FLOOR_MB=${AMUX_CLEANUP_SWAP_FREE_FLOOR_MB:-512}
CPU_SHARE=${AMUX_CLEANUP_CPU_SHARE:-0.9}
STATE_DIR=${AMUX_CLEANUP_STATE_DIR:-$HOME/.amux/logs/mac-cleanup}
# Not the desktop lane: it is ISOLATED, and amux refuses automated sends into an
# isolated worker by design (round 1 of DESKT-57 measured exactly that refusal).
# mac-ops is a non-isolated sonnet worker whose brief is the runbook below.
ESCALATE_TO=${AMUX_CLEANUP_ESCALATE_TO:-mac-ops}
ESCALATE_COOLDOWN_H=${AMUX_CLEANUP_ESCALATE_COOLDOWN_H:-6}
# Seam: FILE is replaced with the message path. The test points this at a recorder.
ESCALATE_CMD=${AMUX_CLEANUP_ESCALATE_CMD:-amux send TARGET --file FILE}
RUNBOOK=docs/runbooks/mac-resource-rca.md
# Seams: the tests point these at a recorder so an action can be observed
# without running it. Defaults are what the scheduler actually runs.
PURGE_CMD=${AMUX_CLEANUP_PURGE_CMD:-sudo -n /usr/sbin/purge}
THIN_CMD=${AMUX_CLEANUP_THIN_CMD:-tmutil thinlocalsnapshots / BYTES URGENCY}
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
      age=$(( ( $(date +%s) - $(stat -f %m "$d" 2>/dev/null || stat -c %Y "$d" 2>/dev/null || echo 0) ) / 86400 ))
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
classify_constraints() { # <disk_free_gb> <burn> <hours_to_full> <pressure> <swap_free_mb> <load15> <ncpu> <family_exceeds:0|1>
  awk -v df="$1" -v b="$2" -v h="$3" -v pr="$4" -v sw="$5" -v l="$6" -v n="$7" -v fam="$8" \
      -v floor="$DISK_FLOOR_GB" -v htf="$HOURS_TO_FULL" -v swf="$SWAP_FREE_FLOOR_MB" -v cs="$CPU_SHARE" 'BEGIN{
    if (df >= 0 && df < floor) printf "disk free %.1fG is under the %dG floor\n", df, floor
    else if (h != "-" && h+0 < htf) printf "disk burning %.1fG/h, full in %.1fh (under %dh)\n", b, h, htf
    if (pr >= 2) printf "memory kernel pressure %d after the purge arm\n", pr
    else if (sw >= 0 && sw < swf) printf "memory swap has %dMB free (under %dMB)\n", sw, swf
    if (l >= 0 && n > 0 && l/n > cs) printf "cpu 15-min load %.1f is %.0f%% of %d cores (over %.0f%%)\n", l, l/n*100, n, cs*100
    if (fam == 1) print "family a process family is over its share of RAM"
  }'
}

# Burn rate from the host-metrics history (5-minute samples), by least squares
# over the window. Two readings 15 minutes apart are noise: round 2 of DESKT-57
# read 65 G/h off exactly that while a peer's delete sat in the window. Prints
# "<burn_gbh> <hours|-> <n> <span_h>", or "- - <n> <span_h>" when there are fewer
# than 6 samples or under an hour of span, so the caller falls back and says so.
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
burn=-slope; last=ys[-1] if xs[-1]==max(xs) else ys[xs.index(max(xs))]
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

[ "${AMUX_CLEANUP_LIB_ONLY:-0}" = "1" ] && return 0 2>/dev/null

# ── measure ──────────────────────────────────────────────────────────────────
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
reap_idle_cargo_targets "$TARGET_ROOTS" "$TARGET_IDLE_H" "$DRY"

# ── act: thin APFS local snapshots when the disk is tight ────────────────────
disk_free_gb=$(df -k /System/Volumes/Data 2>/dev/null | awk 'NR==2{ printf "%.1f", $4/1048576 }')
case "$disk_free_gb" in ''|*[!0-9.]*) disk_free_gb=-1 ;; esac
snaps_before=$(tmutil listlocalsnapshots / 2>/dev/null | tail -n +2 | grep -c . )
if should_thin "$disk_free_gb" "$SNAPSHOT_FLOOR_GB"; then
  if [ "$DRY" = "1" ]; then
    echo "mac-cleanup: snapshots ${snaps_before}, free ${disk_free_gb}G under the ${SNAPSHOT_FLOOR_GB}G floor — would thin up to ${SNAPSHOT_RECLAIM_GB}G (dry run)"
  else
    bytes=$(awk -v g="$SNAPSHOT_RECLAIM_GB" 'BEGIN{ printf "%d", g*1073741824 }')
    cmd=${THIN_CMD//BYTES/$bytes}; cmd=${cmd//URGENCY/$SNAPSHOT_URGENCY}
    if $cmd >/dev/null 2>&1; then
      snaps_after=$(tmutil listlocalsnapshots / 2>/dev/null | tail -n +2 | grep -c . )
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

fse_pid=$(pgrep -x fseventsd 2>/dev/null | head -1)
if [ -n "$fse_pid" ]; then
  fse_gb=$(to_gb "$(top -l 1 -pid "$fse_pid" -stats mem 2>/dev/null | tail -1 | tr -d ' ')")
  if needs_reboot "$fse_gb" "$FSEVENTSD_REBOOT_GB"; then
    echo "mac-cleanup: fseventsd holds ${fse_gb}G and is SIP-protected — a reboot is the only remedy (owner runs: sudo fdesetup authrestart)"
  else
    echo "mac-cleanup: fseventsd ${fse_gb}G, under the ${FSEVENTSD_REBOOT_GB}G reboot threshold"
  fi
fi

# ── assess: what is still constrained, why, and who fixes the cause ──────────
fam_over=0
if [ -n "${fam_kb:-}" ] && family_exceeds "$fam_kb" "$phys_kb" "$FAMILY_SHARE_PCT"; then fam_over=1; fi
now=$(date +%s)
disk_now=$(df -k /System/Volumes/Data 2>/dev/null | awk 'NR==2{ printf "%.1f", $4/1048576 }')
case "$disk_now" in ''|*[!0-9.]*) disk_now=-1 ;; esac
level_now=$(sysctl -n kern.memorystatus_vm_pressure_level 2>/dev/null); case "$level_now" in ''|*[!0-9]*) level_now=-1 ;; esac
swap_free_now=$(sysctl -n vm.swapusage 2>/dev/null | sed -E 's/.*free = ([0-9.]+)M.*/\1/'); case "$swap_free_now" in ''|*[!0-9.]*) swap_free_now=-1 ;; esac
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
verdicts=$(classify_constraints "$disk_now" "$burn" "$htf" "$level_now" "$swap_free_now" "$load15" "$ncpu" "$fam_over")
echo "mac-cleanup: assess disk=${disk_now}G burn=${burn_txt} ${htf_txt} pressure=${level_now} swap_free=${swap_free_now}MB load15=${load15}/${ncpu}"
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
      for r in ${wroots[@]+"${wroots[@]}"} "$HOME/.amux" "$HOME/.colima"; do
        r=${r%@*}; [ -d "$r" ] || continue
        perl -e 'alarm 45; exec @ARGV' find "$r" -xdev -type f -size +500M -mmin -60 -exec stat -f '%b %z %N' {} + 2>/dev/null
      done | sort -u | sort -rn | head -15 | awk '{a=$1*512/2^30; z=$2/2^30; $1=$2=""; sub(/^  /,""); printf "- %.1fG allocated (%.1fG apparent) %s\n", a, z, $0}'
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
    msg="$STATE_DIR/rca/$stamp.$cls.msg"
    {
      echo "Ask: RCA and fix the root cause of this Mac $cls constraint; the tick has already fixed what it can."
      echo "Constraint: $v"
      echo "Evidence: $bundle"
      echo "Runbook: git -C ${AMUX_REPO_DIR:-$HOME/Dev/amux} show origin/main:$RUNBOOK (the committed copy; a checkout may be behind)"
    } > "$msg"
    cmd=${ESCALATE_CMD//TARGET/$ESCALATE_TO}; cmd=${cmd//FILE/$msg}
    if sendout=$($cmd 2>&1); then
      state_put "$STATE" "esc_$cls=$now"; escalated=$((escalated+1))
      echo "mac-cleanup: constraint $v — escalated to $ESCALATE_TO (bundle $bundle)"
    else
      # The reason, not just the word: round 1 printed FAILED and the cause (an
      # isolated target) was only findable by re-running the send by hand.
      echo "mac-cleanup: constraint $v — escalation to $ESCALATE_TO FAILED: $(printf '%s' "$sendout" | tail -1 | cut -c1-160) (bundle $bundle)"
    fi
  done <<EOF
$verdicts
EOF
fi

echo "mac-cleanup: done purge=${purged%% *} agents_restarted=${agents_restarted}/${agents_checked} stale_shells=${stale_killed} targets_reaped=${TARGETS_REAPED} constraints=$(printf '%s' "$verdicts" | grep -c . || true) escalated=${escalated} reported=${reported}"
exit 0
