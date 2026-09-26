#!/bin/bash
# Safety and decision properties of scripts/mac-cleanup-tick.sh.
#
# The tick runs as root for one command and restarts a launchd agent, so the
# properties worth pinning are the ones that keep it from acting when it should
# not: the thresholds, the label guard, and dry run. Each cell fails if its
# guard is removed — the point is that it CAN go red (ethos rule 7).
set -euo pipefail
HERE=$(cd "$(dirname "$0")" && pwd)
TICK="$HERE/mac-cleanup-tick.sh"
FIX=$(mktemp -d)                      # never a fixed name: /tmp is shared
trap 'rm -rf -- "$FIX"' EXIT
fails=0
[ -x "$TICK" ] || { echo "FAIL: $TICK missing or not executable — no cell below ran"; exit 1; }
check() { if [ "$2" = "$3" ]; then echo "  ok   $1"; else echo "  FAIL $1: expected '$2', got '$3'"; fails=$((fails+1)); fi; }

# The cargo-target arm scans real roots and DELETES real idle targets (DESKT-51). Nothing in this
# file tests it (scripts/test-mac-cleanup-targets.sh does), and its end-to-end cells run the whole
# tick for real, so point the arm at a root that does not exist: a test of the purge arm must
# never reap a build directory on the machine running it, or spend minutes scanning ~/Dev.
export AMUX_CLEANUP_TARGET_ROOTS="$FIX/no-such-root"

# Library mode must define the decisions without running a single probe.
AMUX_CLEANUP_LIB_ONLY=1 . "$TICK"
check "library mode defines should_purge" "yes" "$(type should_purge >/dev/null 2>&1 && echo yes || echo no)"

echo "1. purge triggers on either signal, and on neither it stays put"
check "healthy machine is left alone"      "no"  "$(should_purge 1 20 2 4 && echo yes || echo no)"
check "kernel pressure alone triggers"     "yes" "$(should_purge 2 20 2 4 && echo yes || echo no)"
check "low free alone triggers"            "yes" "$(should_purge 1 2 2 4 && echo yes || echo no)"
# -1 is this script's "probe failed" value. Acting on it would purge on every
# tick of a machine whose sysctl is missing, which is the AMUX-4661 shape.
# -1 vs a trigger of -1: without the "pressure was actually measured" guard,
# a failed probe compares equal and purges on every tick. With the default
# trigger of 2 this cell would pass either way, which is a check that cannot
# fail (ethos rule 7).
check "an unmeasured pressure does not trigger by itself" "no" "$(should_purge -1 20 -1 -1 && echo yes || echo no)"
check "a measured pressure at the trigger still fires"    "yes" "$(should_purge 2 20 2 -1 && echo yes || echo no)"

echo "2. an agent is restarted only past the leak floor"
check "small agent left alone" "no"  "$(should_restart_agent 0.05 2 && echo yes || echo no)"
check "leaked agent restarted" "yes" "$(should_restart_agent 2.5 2 && echo yes || echo no)"
check "a zero floor never restarts" "no" "$(should_restart_agent 99 0 && echo yes || echo no)"

echo "3. only a plain com.* label may reach launchctl"
check "ordinary label accepted"   "yes" "$(is_safe_label com.procwarden.menubar && echo yes || echo no)"
check "shell metacharacters refused" "no" "$(is_safe_label 'com.evil;rm -rf /' && echo yes || echo no)"
check "non-com label refused"     "no"  "$(is_safe_label 'evil' && echo yes || echo no)"

echo "4. footprint strings convert to GB"
check "gigabytes" "27.00" "$(to_gb 27G)"
check "megabytes" "1.27"  "$(to_gb 1296M)"
check "kilobytes" "0.0005" "$(to_gb 512K)"

echo "5. the report names who can act"
case "$(classify_owner ethan /private/tmp/claude-501/-Users-ethan-Dev-ai-for-smbs/abc/scratchpad/lima-native/bin/limactl)" in
  *"lane scratch (-Users-ethan-Dev-ai-for-smbs)"*) echo "  ok   a lane's process names its lane" ;;
  *) echo "  FAIL a lane's process is not attributed: $(classify_owner ethan /private/tmp/claude-501/-Users-ethan-Dev-ai-for-smbs/abc/x)"; fails=$((fails+1)) ;;
esac
case "$(classify_owner root /System/Library/Frameworks/Virtualization.framework/x)" in
  *"macOS daemon"*) echo "  ok   a system daemon is named as reboot-only" ;;
  *) echo "  FAIL a system daemon is misattributed"; fails=$((fails+1)) ;;
esac
# DESKT-44. Apple's VM helper lives under /System/Library/Frameworks, so the
# /System/* arm above claimed it was SIP-protected and reboot-only. It runs as
# the invoking user and stops from userspace. Measured 2026-09-19: pid 9701,
# user ethan, 31 GB resident, reported as reboot-only on a box that runs 24/7.
# The argv below is that process's real one, copied from `ps -axo command`.
VMXPC=/System/Library/Frameworks/Virtualization.framework/Versions/A/XPCServices/com.apple.Virtualization.VirtualMachine.xpc/Contents/MacOS/com.apple.Virtualization.VirtualMachine
case "$(classify_owner ethan "$VMXPC")" in
  *"guest VM"*) echo "  ok   a user's guest VM is named as a VM, not a daemon" ;;
  *) echo "  FAIL a user's guest VM is misattributed: $(classify_owner ethan "$VMXPC")"; fails=$((fails+1)) ;;
esac
# THE HALF THAT COST THE REBOOT, asserted separately: naming it correctly is
# worth nothing if the line still sends the owner to a restart.
#
# Matched on "reboot only", the daemon arm's own phrasing, NOT on the bare word
# "reboot". The first version of this cell used `*reboot*` and went red against
# a correct fix, because the remedy says "no reboot" and a substring cannot tell
# a instruction to reboot from a statement that none is needed.
case "$(classify_owner ethan "$VMXPC")" in
  *"reboot only"*) echo "  FAIL a stoppable VM still asks for a reboot: $(classify_owner ethan "$VMXPC")"; fails=$((fails+1)) ;;
  *) echo "  ok   a stoppable VM is not called reboot-only" ;;
esac
# AND IT MUST CARRY A COMMAND, or the cell above passes on a line that names no
# remedy at all, which is the same dead end in a politer sentence.
case "$(classify_owner ethan "$VMXPC")" in
  *"stop"*) echo "  ok   a stoppable VM carries the command that stops it" ;;
  *) echo "  FAIL a stoppable VM names no remedy: $(classify_owner ethan "$VMXPC")"; fails=$((fails+1)) ;;
esac
# AND THE BOUNDARY: the narrow case must not swallow the arm above it. A real
# SIP-protected binary under the SAME framework stays reboot-only, so this
# cell goes red if the new pattern is widened to /System/*Virtualization*.
case "$(classify_owner root /System/Library/Frameworks/Virtualization.framework/Versions/A/Resources/vmnetd)" in
  *"macOS daemon"*) echo "  ok   a real daemon under the same framework is untouched" ;;
  *) echo "  FAIL the VM case swallowed a genuine SIP daemon"; fails=$((fails+1)) ;;
esac
check "a user's own app" "user process" "$(classify_owner ethan /Applications/Google Chrome.app/Contents/MacOS/Chrome)"
case "$(classify_owner ethan /Applications/Ollama.app/Contents/Resources/llama-server)" in
  *"ollama stop"*) echo "  ok   an ollama model server carries its unload command" ;;
  *) echo "  FAIL an ollama model server is not named with its remedy"; fails=$((fails+1)) ;; esac

echo "5b. the label comes from the FULL command line of a live process, not a truncated one"
# The lima VM helper is 173 characters and its name starts after column 80. A
# fixture that hands classify_owner the whole path passes while the shipped call
# sites cut the command first, so this spawns a REAL process and asks about its pid.
LONGD="$FIX/$(printf 'd%.0s' $(seq 1 50))"; mkdir -p "$LONGD"
cat > "$LONGD/com.apple.Virtualization.VirtualMachine.xpc" <<'VM'
#!/bin/bash
sleep 30
VM
chmod +x "$LONGD/com.apple.Virtualization.VirtualMachine.xpc"
"$LONGD/com.apple.Virtualization.VirtualMachine.xpc" &
VPID=$!
sleep 1
VCMD=$(ps -o command= -p "$VPID" 2>/dev/null)
VPOS=$(awk -v s="$VCMD" 'BEGIN{ print index(s, "Virtualization.VirtualMachine.xpc") }')
# Positive control: without this the cell proves nothing, because a short path
# would match even with the truncation still in place.
check "fixture puts the VM name past column 80" "yes" "$([ "${VPOS:-0}" -gt 80 ] && echo yes || echo no)"
case "$(owner_label_for_pid "$VPID")" in
  *"guest VM"*) echo "  ok   a live VM helper is labelled a user-owned guest VM" ;;
  *) echo "  FAIL live VM helper mislabelled: $(owner_label_for_pid "$VPID")"; fails=$((fails+1)) ;;
esac
kill "$VPID" 2>/dev/null || true; wait "$VPID" 2>/dev/null || true
# Source-level pin of the defect CLASS, because the two call sites are inline and
# a behavioural cell cannot reach them: no `ps -o command=` may feed a cut.
check "no call site truncates the command it classifies" "0" \
  "$(grep -c -E 'ps -o command=.*\| *cut ' "$TICK" || true)"

echo "6. the reboot line appears only when fseventsd is actually large"
check "small fseventsd asks for no reboot" "no"  "$(needs_reboot 8 20 && echo yes || echo no)"
check "large fseventsd asks for a reboot"  "yes" "$(needs_reboot 96 20 && echo yes || echo no)"

# Cells 7 and 8 drive the real script, whose probes are macOS-only (vm_stat,
# kern.memorystatus_vm_pressure_level). On Linux the tick reports measured=false
# and correctly refuses to act, so these cells would fail for a reason that is
# not a defect. Skipped LOUDLY: a silent skip is how a suite reads green while
# testing nothing.
if [ "$(uname)" != "Darwin" ]; then
  echo "7-8. skipped on $(uname): the end-to-end cells need macOS probes (vm_stat, kern.memorystatus_vm_pressure_level)"
  echo
  if [ "$fails" -eq 0 ]; then echo "PASS: mac-cleanup-tick — decision cells passed; end-to-end cells skipped off macOS"; exit 0; fi
  echo "mac-cleanup-tick: $fails check(s) FAILED"; exit 1
fi

echo "6b. snapshots are thinned only when the disk is actually tight"
check "plenty of space leaves snapshots alone" "no"  "$(should_thin 500 100 && echo yes || echo no)"
check "a tight disk thins"                     "yes" "$(should_thin 40 100 && echo yes || echo no)"
# -1 is the "df failed" value. Thinning on it would delete the owner's restore
# window on every tick of a machine whose probe is broken.
check "an unmeasured disk never thins"         "no"  "$(should_thin -1 100 && echo yes || echo no)"

echo "6c. the family rule sees what per-process ranking cannot"
check "hours:minutes:seconds" "144753" "$(etime_secs 40:12:33)"
check "days-hours"            "183845" "$(etime_secs 2-03:04:05)"
check "minutes:seconds"       "449"    "$(etime_secs 07:29)"
# Fixture: pid ppid rss_kb oldest_secs. Parent 999 holds three children summing
# 31 GB; parent 888 holds one 20 GB child. Per-process ranking picks 888's child
# and misses the bigger family, which is the DESKT-31 failure exactly.
FAM=$(printf '%s\n' "101 999 10485760 01:00:00" "102 999 10485760 1-16:00:00" "103 999 11534336 01:00" "201 888 20971520 00:30" "301 1 99999999 00:10" | top_family)
check "the largest FAMILY is picked, not the largest process" "999" "$(echo "$FAM" | awk '{print $1}')"
check "children are counted"                                   "3"   "$(echo "$FAM" | awk '{print $3}')"
check "the oldest child's age is carried"                      "144000" "$(echo "$FAM" | awk '{print $4}')"
check "descendants of launchd are not one giant family"        "999" "$(echo "$FAM" | awk '{print $1}')"
check "a family over the share fires"      "yes" "$(family_exceeds 46514176 100663296 15 && echo yes || echo no)"
check "a small family does not"            "no"  "$(family_exceeds 1048576 100663296 15 && echo yes || echo no)"
check "unknown physical RAM never fires"   "no"  "$(family_exceeds 46514176 0 15 && echo yes || echo no)"
# The return value alone cannot see this guard: without it awk dies on a
# division by zero, which also returns non-zero and reads as "did not fire".
# The observable difference is the error on stderr, so assert THAT.
check "unknown physical RAM produces no awk error" "" "$(family_exceeds 46514176 0 15 2>&1 >/dev/null)"
check "a 40h family is past a 12h ceiling" "yes" "$(family_too_old 144000 12 && echo yes || echo no)"
check "a young family is not"              "no"  "$(family_too_old 600 12 && echo yes || echo no)"

echo "7. end to end: the action runs when triggered, and dry run performs none"
REC="$FIX/purge-calls"
cat > "$FIX/fake-purge.sh" <<EOF
#!/bin/bash
echo called >> "$REC"
EOF
chmod +x "$FIX/fake-purge.sh"
: > "$REC"
# Force the trigger with the free floor so the cell does not depend on the
# machine it runs on being under pressure.
AMUX_CLEANUP_PURGE_CMD="$FIX/fake-purge.sh" AMUX_CLEANUP_FREE_FLOOR_GB=99999 \
  AMUX_CLEANUP_AGENTS="" AMUX_CLEANUP_REPORT_GB=99999 "$TICK" >/dev/null 2>&1
check "purge ran when triggered" "1" "$(wc -l < "$REC" | tr -d ' ')"
: > "$REC"
AMUX_CLEANUP_PURGE_CMD="$FIX/fake-purge.sh" AMUX_CLEANUP_FREE_FLOOR_GB=99999 \
  AMUX_CLEANUP_AGENTS="" AMUX_CLEANUP_REPORT_GB=99999 "$TICK" --dry-run >/dev/null 2>&1
check "dry run performed no purge" "0" "$(wc -l < "$REC" | tr -d ' ')"
: > "$REC"
AMUX_CLEANUP_PURGE_CMD="$FIX/fake-purge.sh" AMUX_CLEANUP_FREE_FLOOR_GB=0 AMUX_CLEANUP_PRESSURE_PURGE=99 \
  AMUX_CLEANUP_AGENTS="" AMUX_CLEANUP_REPORT_GB=99999 "$TICK" >/dev/null 2>&1
check "no purge when neither trigger fires" "0" "$(wc -l < "$REC" | tr -d ' ')"

echo "7b. end to end: thinning goes through the knob, honours dry run, and stays bounded"
TREC="$FIX/thin-calls"
cat > "$FIX/fake-thin.sh" <<EOF
#!/bin/bash
echo "\$@" >> "$TREC"
EOF
chmod +x "$FIX/fake-thin.sh"
: > "$TREC"
AMUX_CLEANUP_THIN_CMD="$FIX/fake-thin.sh BYTES URGENCY" AMUX_CLEANUP_SNAPSHOT_FLOOR_GB=999999 \
  AMUX_CLEANUP_SNAPSHOT_RECLAIM_GB=7 AMUX_CLEANUP_PURGE_CMD=true AMUX_CLEANUP_FREE_FLOOR_GB=0 \
  AMUX_CLEANUP_PRESSURE_PURGE=99 AMUX_CLEANUP_AGENTS="" AMUX_CLEANUP_REPORT_GB=99999 "$TICK" >/dev/null 2>&1
check "thin ran when the disk was under the floor" "1" "$(wc -l < "$TREC" | tr -d ' ')"
case "$(cat "$TREC")" in *7516192768*) echo "  ok   the thin is bounded to the requested bytes" ;;
  *) echo "  FAIL the thin did not carry a bounded byte target: $(cat "$TREC")"; fails=$((fails+1)) ;; esac
: > "$TREC"
AMUX_CLEANUP_THIN_CMD="$FIX/fake-thin.sh BYTES URGENCY" AMUX_CLEANUP_SNAPSHOT_FLOOR_GB=999999 \
  AMUX_CLEANUP_PURGE_CMD=true AMUX_CLEANUP_FREE_FLOOR_GB=0 AMUX_CLEANUP_PRESSURE_PURGE=99 \
  AMUX_CLEANUP_AGENTS="" AMUX_CLEANUP_REPORT_GB=99999 "$TICK" --dry-run >/dev/null 2>&1
check "dry run thinned nothing" "0" "$(wc -l < "$TREC" | tr -d ' ')"
: > "$TREC"
AMUX_CLEANUP_THIN_CMD="$FIX/fake-thin.sh BYTES URGENCY" AMUX_CLEANUP_SNAPSHOT_FLOOR_GB=0 \
  AMUX_CLEANUP_PURGE_CMD=true AMUX_CLEANUP_FREE_FLOOR_GB=0 AMUX_CLEANUP_PRESSURE_PURGE=99 \
  AMUX_CLEANUP_AGENTS="" AMUX_CLEANUP_REPORT_GB=99999 "$TICK" >/dev/null 2>&1
check "no thin when the disk has room" "0" "$(wc -l < "$TREC" | tr -d ' ')"

echo "7c. end to end: the family line names a parent and its child count"
out=$(AMUX_CLEANUP_PURGE_CMD=true AMUX_CLEANUP_FREE_FLOOR_GB=0 AMUX_CLEANUP_PRESSURE_PURGE=99 \
      AMUX_CLEANUP_SNAPSHOT_FLOOR_GB=0 AMUX_CLEANUP_AGENTS="" AMUX_CLEANUP_REPORT_GB=99999 \
      AMUX_CLEANUP_FAMILY_SHARE_PCT=0.0001 "$TICK" 2>&1)
case "$out" in *"FAMILY "*"children of pid "*) echo "  ok   a family over the share is named with its parent" ;;
  *) echo "  FAIL no family line when the share threshold is effectively zero: $(printf '%s' "$out" | tail -3)"; fails=$((fails+1)) ;; esac
out=$(AMUX_CLEANUP_PURGE_CMD=true AMUX_CLEANUP_FREE_FLOOR_GB=0 AMUX_CLEANUP_PRESSURE_PURGE=99 \
      AMUX_CLEANUP_SNAPSHOT_FLOOR_GB=0 AMUX_CLEANUP_AGENTS="" AMUX_CLEANUP_REPORT_GB=99999 \
      AMUX_CLEANUP_FAMILY_SHARE_PCT=99.9 AMUX_CLEANUP_FAMILY_AGE_H=99999 "$TICK" 2>&1)
case "$out" in *"largest family"*"under the"*) echo "  ok   under both thresholds it reports without firing" ;;
  *) echo "  FAIL no under-threshold family line: $(printf '%s' "$out" | tail -3)"; fails=$((fails+1)) ;; esac

echo "7c2. this suite never lets the cargo-target arm near the real tree"
out=$(AMUX_CLEANUP_PURGE_CMD=true AMUX_CLEANUP_FREE_FLOOR_GB=0 AMUX_CLEANUP_PRESSURE_PURGE=99 \
      AMUX_CLEANUP_SNAPSHOT_FLOOR_GB=999999 AMUX_CLEANUP_AGENTS="" AMUX_CLEANUP_REPORT_GB=99999 "$TICK" --dry-run 2>&1)
check "the arm scanned no roots under this suite" "yes" "$(printf '%s' "$out" | grep -q 'cargo targets: found 0 under 0 root(s)' && echo yes || echo no)"

echo "7d. the stale-process arm parses 08 and 09 in elapsed times (bash reads them as octal)"
# 2026-09-24: SCHED-465 printed "value too great for base" because this arm did its
# own bash arithmetic on fields like 08 and 09, then reported found=0. Fixture times
# below all contain one, and two are past the 6h floor.
cat > "$FIX/fake-ps.sh" <<'FPS'
#!/bin/bash
echo "12345 08:09:07 /bin/bash -c source /x/.claude/shell-snapshots/snapshot-bash-1.sh"
echo "12346 1-09:08:09 /bin/bash -c source /x/.claude/shell-snapshots/snapshot-bash-2.sh"
echo "12347 00:09 /bin/bash -c source /x/.claude/shell-snapshots/snapshot-bash-3.sh"
FPS
chmod +x "$FIX/fake-ps.sh"
out=$(AMUX_CLEANUP_STALE_PS_CMD="$FIX/fake-ps.sh" AMUX_CLEANUP_PURGE_CMD=true AMUX_CLEANUP_FREE_FLOOR_GB=0 \
      AMUX_CLEANUP_PRESSURE_PURGE=99 AMUX_CLEANUP_SNAPSHOT_FLOOR_GB=0 AMUX_CLEANUP_AGENTS="" \
      AMUX_CLEANUP_REPORT_GB=99999 "$TICK" --dry-run 2>&1)
check "no octal error on 08/09 fields" "0" "$(printf '%s\n' "$out" | grep -c 'value too great')"
check "both stale times are found, the 9-second one is not" "2" \
  "$(printf '%s\n' "$out" | sed -n 's/.*shell-snapshots found=\([0-9]*\) .*/\1/p')"
case "$out" in *"pid=12345 age=08:09:07"*"pid=12346 age=1-09:08:09"*) echo "  ok   each stale process is named with its age" ;;
  *) echo "  FAIL stale processes not named with their ages: $(printf '%s' "$out" | grep -E 'stale shell|shell-snapshots' | head -3)"; fails=$((fails+1)) ;; esac
check "dry run killed nothing" "0" "$(printf '%s\n' "$out" | sed -n 's/.*shell-snapshots found=[0-9]* killed=\([0-9]*\) .*/\1/p')"

echo "8. end to end: an agent restart goes through the knob, and only past the floor"
AREC="$FIX/restart-calls"
cat > "$FIX/fake-restart.sh" <<EOF
#!/bin/bash
echo "\$@" >> "$AREC"
EOF
chmod +x "$FIX/fake-restart.sh"
: > "$AREC"
# A label that IS loaded on any Mac, with a 0.0001 GB floor so the footprint
# cannot keep the cell from firing. The positive control is the call itself.
LIVE_LABEL=$(launchctl list 2>/dev/null | awk 'NR>1 && $1 ~ /^[0-9]+$/ && $3 ~ /^com\./ {print $3; exit}')
if [ -n "$LIVE_LABEL" ]; then
  AMUX_CLEANUP_RESTART_CMD="$FIX/fake-restart.sh UID LABEL" AMUX_CLEANUP_AGENTS="$LIVE_LABEL" \
    AMUX_CLEANUP_AGENT_LEAK_GB=0.0001 AMUX_CLEANUP_FREE_FLOOR_GB=0 AMUX_CLEANUP_PRESSURE_PURGE=99 \
    AMUX_CLEANUP_REPORT_GB=99999 "$TICK" >/dev/null 2>&1
  check "leaked agent restart invoked the knob" "1" "$(wc -l < "$AREC" | tr -d ' ')"
  case "$(cat "$AREC")" in *"$LIVE_LABEL"*) echo "  ok   the restart names the agent" ;;
    *) echo "  FAIL the restart did not name the agent: $(cat "$AREC")"; fails=$((fails+1)) ;; esac
  : > "$AREC"
  AMUX_CLEANUP_RESTART_CMD="$FIX/fake-restart.sh UID LABEL" AMUX_CLEANUP_AGENTS="$LIVE_LABEL" \
    AMUX_CLEANUP_AGENT_LEAK_GB=9999 AMUX_CLEANUP_FREE_FLOOR_GB=0 AMUX_CLEANUP_PRESSURE_PURGE=99 \
    AMUX_CLEANUP_REPORT_GB=99999 "$TICK" >/dev/null 2>&1
  check "agent under the floor is left alone" "0" "$(wc -l < "$AREC" | tr -d ' ')"
  : > "$AREC"
  AMUX_CLEANUP_RESTART_CMD="$FIX/fake-restart.sh UID LABEL" AMUX_CLEANUP_AGENTS='com.evil;touch /tmp/pwned' \
    AMUX_CLEANUP_AGENT_LEAK_GB=0.0001 AMUX_CLEANUP_FREE_FLOOR_GB=0 AMUX_CLEANUP_PRESSURE_PURGE=99 \
    AMUX_CLEANUP_REPORT_GB=99999 "$TICK" >/dev/null 2>&1
  check "an unsafe label never reaches the restart" "0" "$(wc -l < "$AREC" | tr -d ' ')"
else
  echo "  FAIL no loaded com.* launchd label found, so cells 8 could not run"
  fails=$((fails+1))
fi

echo
if [ "$fails" -eq 0 ]; then echo "PASS: mac-cleanup-tick — all checks passed"; exit 0; fi
echo "mac-cleanup-tick: $fails check(s) FAILED"; exit 1
