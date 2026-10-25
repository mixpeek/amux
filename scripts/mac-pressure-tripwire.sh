#!/bin/bash
# mac-pressure-tripwire.sh — page BEFORE the kernel starts killing processes.
#
# WHY THIS EXISTS. On 2026-09-14 the box sat at kernel memory pressure level 2
# with swap at 85,639 of 86,016 MB, and the only reason anyone knew was that
# Ethan happened to look. It cleared on its own within the hour (level 1, swap
# down 18.4 GB, no reboot), so nothing was lost that time. The next one is a
# coin flip: at level 4 the kernel's jetsam kills largest-first, which on this
# box is fseventsd and then the claude sessions.
#
# WHAT IT WATCHES, and why these three:
#   - kern.memorystatus_vm_pressure_level. The kernel's own verdict. 1 normal,
#     2 warn, 4 critical. Level 2 is NOT alerted: it was observed oscillating
#     1 <-> 2 within ten minutes on 2026-09-14 and clearing without help, so
#     paging on it would train the alarm to be ignored.
#   - fseventsd RSS. SIP-protected and unkillable (csrutil enabled, and the
#     binary carries the `restricted` flag), so a reboot is the only thing that
#     clears it. It sits around 8 GB here and is the largest single process.
#     NOTE, and this is why the reading below is a median: its RSS is extremely
#     volatile. Samples on 2026-09-14 within fifteen seconds: 2.87, 8.13, 8.48,
#     8.57, 12.83 GB. Do NOT infer a growth rate from two samples; I did, and
#     reported "growing several GB per hour" to Ethan on MO-3326, which the very
#     next sample contradicted. A threshold on a smoothed value is honest here;
#     a trend line from spot reads is not.
#   - swap free. The last cushion before the kernel has nowhere to put pages.
#
# WHAT IT DOES AND DOES NOT DO. It kills nothing itself, stops no lane and
# restarts no daemon (those remedies are Ethan's under the 24/7 rule in
# ~/Dev/CLAUDE.md). It does act early: at the warn level it starts the hourly
# cleanup pass (SCHED-465) ahead of its slot, which works only inside that
# pass's own bounded rules, then pages at the critical level.
#
# The alarm is a fire alarm (~/.claude/CLAUDE.md: "USE VERY SPARINGLY"), so it
# fires only on conditions a human must act on, and at most once per cooldown.
#
# Usage:
#   scripts/mac-pressure-tripwire.sh           # check, page if a threshold trips
#   scripts/mac-pressure-tripwire.sh --dry-run # never page, just print
set -uo pipefail

# `sysctl` lives in /usr/sbin, which is NOT on the PATH this runs under, so both
# the pressure level and the swap reading came back empty and two of the three
# thresholds could never trip (AMUX-4661). Measured on this box: the running
# amux-server-rs process, which is what executes SCHED-458, carries
#   PATH=/Users/ethan/.cargo/bin:/Users/ethan/.local/bin:/usr/local/bin:/opt/homebrew/bin:/usr/bin:/bin
# with no /usr/sbin, and the script printed `measured=false level=-1
# swap_free=-1MB swap_used=MB` under it. Its sibling mac-cleanup-tick.sh already
# exports this for the same reason; host-analysis.sh does too.
#
# The script was HONEST about the gap rather than silent (measured=false is in
# every line it prints), which is the only reason this was findable at all. That
# is the property to keep if these probes ever change.
PATH="/usr/local/bin:/opt/homebrew/bin:/usr/bin:/bin:/usr/sbin:/sbin:${PATH:-}"
export PATH

PRESSURE_ALERT=${AMUX_TRIPWIRE_PRESSURE:-3}       # >= this pages. 2 self-clears, so not 2.
FSEVENTSD_GB=${AMUX_TRIPWIRE_FSEVENTSD_GB:-20}    # 12.8 GB seen 09-14; 20 is a real escalation
SWAP_FREE_MB=${AMUX_TRIPWIRE_SWAP_FREE_MB:-512}   # last cushion
COOLDOWN_S=${AMUX_TRIPWIRE_COOLDOWN_S:-10800}     # 3h, so a 15m schedule cannot spam
STATE=${AMUX_TRIPWIRE_STATE:-$HOME/.amux/mac-pressure-tripwire.last}
ACT_PRESSURE=${AMUX_TRIPWIRE_ACT_PRESSURE:-2}     # >= this starts the cleanup; it acts, so 2 is fine
CLEANUP_SCHEDULE=${AMUX_TRIPWIRE_CLEANUP_SCHEDULE:-SCHED-465}
ACT_COOLDOWN_S=${AMUX_TRIPWIRE_ACT_COOLDOWN_S:-3600}
ACT_STATE=${AMUX_TRIPWIRE_ACT_STATE:-$HOME/.amux/mac-pressure-tripwire.act}
ACT_TRIGGER_CMD=${AMUX_TRIPWIRE_ACT_TRIGGER_CMD:-act_trigger_default}
DRY=0
[ "${1:-}" = "--dry-run" ] && DRY=1

# AMUX_TRIPWIRE_LEVEL stands in for the kernel reading (tests only).
level=${AMUX_TRIPWIRE_LEVEL:-$(sysctl -n kern.memorystatus_vm_pressure_level 2>/dev/null)}
[ -n "$level" ] || level=-1
swap_line=$(sysctl -n vm.swapusage 2>/dev/null)
swap_free=$(printf '%s' "$swap_line" | sed -E 's/.*free = ([0-9.]+)M.*/\1/')
swap_used=$(printf '%s' "$swap_line" | sed -E 's/.*used = ([0-9.]+)M.*/\1/')
case "$swap_free" in ''|*[!0-9.]*) swap_free=-1 ;; esac
# RECLAIMABLE, not "free". On macOS `Pages free` is routinely near zero on a
# perfectly healthy box because the OS keeps pages resident until something
# wants them; measured here 0.38 GB free beside 22.76 GB inactive. Reporting
# free alone reads as an emergency during normal operation, and I made exactly
# that mistake on 2026-09-14 before writing this.
read -r free_gb reclaim_gb <<EOF
$(vm_stat 2>/dev/null | awk '/Pages free/{gsub(/\./,"",$3); f=$3} /Pages inactive/{gsub(/\./,"",$3); i=$3} /Pages purgeable/{gsub(/\./,"",$3); p=$3} /Pages speculative/{gsub(/\./,"",$3); s=$3} END{printf "%.2f %.2f", f*16384/1073741824, (f+i+p+s)*16384/1073741824}')
EOF
[ -n "${free_gb:-}" ] || free_gb=-1
[ -n "${reclaim_gb:-}" ] || reclaim_gb=-1

# MEDIAN OF THREE, because a single sample of this is noise. Measured across
# fifteen seconds on 2026-09-14: 2.87, 8.13, 8.48, 8.57 and 12.83 GB for the
# same process. One reading is not a trend and must not page on a spike.
fse_a=$(ps -axo rss,comm 2>/dev/null | awk '$2 ~ /fseventsd$/ {if($1>m)m=$1} END{print m+0}'); sleep 2
fse_b=$(ps -axo rss,comm 2>/dev/null | awk '$2 ~ /fseventsd$/ {if($1>m)m=$1} END{print m+0}'); sleep 2
fse_c=$(ps -axo rss,comm 2>/dev/null | awk '$2 ~ /fseventsd$/ {if($1>m)m=$1} END{print m+0}')
fse_kb=$(printf '%s\n%s\n%s\n' "$fse_a" "$fse_b" "$fse_c" | sort -n | sed -n 2p)
[ -n "$fse_kb" ] || fse_kb=0
fse_gb=$(awk -v k="$fse_kb" 'BEGIN{printf "%.2f", k/1048576}')

# Every field is measured, and says so when it is not (ethos rule 4: an output
# that can read zero must publish whether the measurement ran).
measured=true
[ "$level" = "-1" ] && measured=false
[ "$swap_free" = "-1" ] && measured=false

reasons=""
add() { reasons="${reasons}${reasons:+; }$1"; }
awk_ge() { awk -v a="$1" -v b="$2" 'BEGIN{exit !(a+0 >= b+0)}'; }

[ "$level" != "-1" ] && awk_ge "$level" "$PRESSURE_ALERT" && \
  add "kernel memory pressure level $level (4 = critical, jetsam kills largest-first)"
awk_ge "$fse_gb" "$FSEVENTSD_GB" && \
  add "fseventsd at ${fse_gb} GB (SIP-protected, unkillable; only a reboot clears it)"
[ "$swap_free" != "-1" ] && awk -v a="$swap_free" -v b="$SWAP_FREE_MB" 'BEGIN{exit !(a+0 <= b+0)}' && \
  add "swap free ${swap_free} MB of the ${swap_used} MB in use"

echo "mac-tripwire: measured=$measured level=$level reclaimable=${reclaim_gb}GB free=${free_gb}GB fseventsd=${fse_gb}GB(median of 3) swap_free=${swap_free}MB swap_used=${swap_used}MB"

# ACT BEFORE PAGING (Ethan 2026-10-22: "make sure its proactive"). The page tier
# above waits for level 3, where only a human can help. At the warn level the
# hourly cleanup (SCHED-465: idle VMs, stale shells, test orphans, build output,
# renice) can still free memory, so start it now instead of waiting up to an
# hour for its slot. It is the same bounded pass that runs every hour, so an
# early start costs one extra pass at most, and the cooldown keeps a level that
# oscillates 1 <-> 2 from starting it more than once an hour.
act_trigger_default() { # prints the HTTP status of the run request
  curl -sk --max-time 20 -o /dev/null -w '%{http_code}' -X POST -H "X-Amux-Session: desktop" \
    "$(amux url 2>/dev/null || echo https://localhost:8824)/api/schedules/$CLEANUP_SCHEDULE/run"
}
if [ "$level" != "-1" ] && awk_ge "$level" "$ACT_PRESSURE" || [ -n "$reasons" ]; then
  act_last=0
  [ -f "$ACT_STATE" ] && act_last=$(cat "$ACT_STATE" 2>/dev/null || echo 0)
  case "$act_last" in ''|*[!0-9]*) act_last=0 ;; esac
  act_since=$(( $(date +%s) - act_last ))
  if [ "$DRY" = "1" ]; then
    echo "mac-tripwire: level $level >= $ACT_PRESSURE: would start the cleanup ($CLEANUP_SCHEDULE) (dry run)"
  elif [ "$act_since" -lt "$ACT_COOLDOWN_S" ]; then
    echo "mac-tripwire: level $level >= $ACT_PRESSURE: cleanup started ${act_since}s ago, under ${ACT_COOLDOWN_S}s (verdict=cleanup_cooldown)"
  else
    acode=$($ACT_TRIGGER_CMD 2>/dev/null)
    case "$acode" in
      2??) printf '%s' "$(date +%s)" > "$ACT_STATE" 2>/dev/null
           echo "mac-tripwire: level $level >= $ACT_PRESSURE: started the cleanup ($CLEANUP_SCHEDULE) (verdict=cleanup_triggered)" ;;
      *)   echo "mac-tripwire: level $level >= $ACT_PRESSURE: could not start the cleanup (http ${acode:-none}) (verdict=cleanup_trigger_failed)" ;;
    esac
  fi
fi

if [ -z "$reasons" ]; then
  echo "mac-tripwire: no threshold tripped (pressure>=$PRESSURE_ALERT, fseventsd>=${FSEVENTSD_GB}GB, swap_free<=${SWAP_FREE_MB}MB)"
  exit 0
fi

now=$(date +%s)
last=0
[ -f "$STATE" ] && last=$(cat "$STATE" 2>/dev/null || echo 0)
case "$last" in ''|*[!0-9]*) last=0 ;; esac
since=$(( now - last ))
if [ "$since" -lt "$COOLDOWN_S" ]; then
  echo "mac-tripwire: TRIPPED ($reasons) but within cooldown, ${since}s of ${COOLDOWN_S}s — not paging again"
  exit 0
fi

msg="Mac memory tripwire: $reasons. Reclaimable ${reclaim_gb} GB (free ${free_gb} GB), swap ${swap_used} MB used with ${swap_free} MB free, fseventsd ${fse_gb} GB. Nothing was killed or stopped; every remedy here is yours under the 24/7 rule (MO-3326)."
why="Jetsam kills largest-first at level 4, which on this box is fseventsd then the claude sessions."
if [ "$DRY" = "1" ]; then
  echo "mac-tripwire: DRY RUN, would page:"; echo "  $msg"
else
  amux alert "$msg" "$why" >/dev/null 2>&1 && echo "mac-tripwire: PAGED Ethan — $reasons" \
    || echo "mac-tripwire: TRIPPED ($reasons) but the alert call FAILED — check amux alert"
  printf '%s' "$now" > "$STATE" 2>/dev/null
fi
exit 0
