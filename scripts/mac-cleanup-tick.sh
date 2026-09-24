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
    -h|--help) sed -n '2,32p' "$0"; exit 0 ;;
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
# A process FAMILY is what actually took this box down: on 2026-08-29 local Ray
# held 194 worker processes and 44.35 GB under one parent for 40 hours, and the
# watcher of the day only had a rule for zombies, so it reported 79 harmless
# reaped-parent entries and never once mentioned Ray (DESKT-31). Per-process
# ranking misses it too, because no single child is large.
FAMILY_SHARE_PCT=${AMUX_CLEANUP_FAMILY_SHARE_PCT:-15}
FAMILY_AGE_H=${AMUX_CLEANUP_FAMILY_AGE_H:-12}
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

echo "mac-cleanup: done purge=${purged%% *} agents_restarted=${agents_restarted}/${agents_checked} stale_shells=${stale_killed} reported=${reported}"
exit 0
