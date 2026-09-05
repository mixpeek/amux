#!/usr/bin/env bash
# amux fleet cold-start — bring every non-archived worker back up after a reboot.
#
# WHY THIS EXISTS
#
# On 2026-08-29 the machine restarted and 56 of the fleet's 58 non-archived
# workers stayed down until a human noticed and started them by hand. Nothing was
# broken in the sense of an error being raised: nothing had ever been responsible
# for this. launchd brought back the four amux SERVICES it knows about
# (com.amux.server-rs, its builder, the watchdog, cert-renew) and the watchdog
# supervises the server only, by design and with an explicit rationale. The
# WORKERS — the 56 processes that are the actual point of the fleet — had no
# owner at boot at all. The dashboard showed them registered, described, holding
# 69 cards in `doing`, and not running.
#
# That is the gap this file closes. It is deliberately the smallest thing that
# can close it: wait for the server, then call the one bulk-start verb, then say
# what happened.
#
# WHY IT WAITS FOR THE SERVER
#
# A worker's whole value is that it can reach the API — its board cards, its
# memory, its identity. `amux start` injects AMUX_URL into every worker it
# spawns, resolving it through `amux url`, and workers whose provider is
# codex/ollama/gemini have their launch DELEGATED to the server outright. Racing
# the server means those workers launch against a base that is not answering yet.
# So we wait, with a bounded timeout, and we start the fleet either way — a
# claude-provider worker that comes up before the server recovers on its own is
# better than a fleet that stayed down because a health check was slow.
#
# WHY NOT KeepAlive
#
# This is a cold-start, not a supervisor. Restarting a worker that a human
# deliberately stopped would be amux deciding something that is the human's to
# decide (ethos rule 8). It runs once, at login, and exits.

set -uo pipefail

AMUX_BIN="${AMUX_BIN:-/Users/ethan/Dev/amux/amux}"
LOG="${AMUX_FLEET_BOOT_LOG:-$HOME/.amux/logs/fleet-boot.log}"
HEALTH_TIMEOUT="${AMUX_FLEET_BOOT_HEALTH_TIMEOUT:-120}"

mkdir -p "$(dirname "$LOG")"

log() { printf '%s %s\n' "$(date '+%Y-%m-%dT%H:%M:%S%z')" "$*" >> "$LOG"; }

log "=== fleet-boot starting (uptime: $(uptime | sed 's/^ *//')) ==="

# HOW LONG THE FLEET WAS DOWN BEFORE ANYONE LOGGED IN (AF-498).
#
# This agent lives in ~/Library/LaunchAgents, and a LaunchAgent loads at GUI
# LOGIN, not at boot. So an UNATTENDED reboot — a macOS auto-update at 2am is
# the specimen — brings the machine back with every worker down, and nothing
# starts until a human sits down and logs in. Reported live: "an iOS update
# automatically at like 2 a.m. So everything stopped."
#
# Nothing was broken and nothing could have said so: this script's log begins
# when it RUNS, so the hours before it ran left no trace anywhere. The gap is
# computed, never asserted — an unreadable boot time says UNMEASURED rather than
# letting a missing number read as zero (ethos rule 4).
# AMUX_FLEET_BOOT_EPOCH overrides the source so all three arms below are
# reachable in a test. Without it the gap branch is only exercisable by actually
# rebooting the machine, which means it would ship unverified — the same reason
# the LSOF override exists in the git guard.
boot_epoch="${AMUX_FLEET_BOOT_EPOCH:-}"
if [[ -n "$boot_epoch" ]]; then
  :
elif [[ -r /proc/stat ]]; then
  boot_epoch="$(awk '/^btime /{print $2}' /proc/stat 2>/dev/null)"
elif [[ -x /usr/sbin/sysctl ]]; then
  boot_epoch="$(/usr/sbin/sysctl -n kern.boottime 2>/dev/null | sed -n 's/^{ sec = \([0-9]*\).*/\1/p')"
fi
if [[ "$boot_epoch" =~ ^[0-9]+$ ]] && (( boot_epoch > 0 )); then
  _gap=$(( $(date +%s) - boot_epoch ))
  (( _gap < 0 )) && _gap=0
  if (( _gap > 300 )); then
    log "LOGIN GAP: the machine booted $(( _gap / 60 ))m before this agent ran, so the fleet was DOWN for that whole window. A LaunchAgent loads at GUI login, not at boot — an unattended reboot (an OS auto-update overnight) leaves every worker down until a human logs in. Enable automatic login if that window matters more than an unlocked desktop; it is a machine setting, not an amux one."
  else
    log "login gap: ${_gap}s between machine boot and this agent — a login followed the boot promptly"
  fi
else
  log "LOGIN GAP UNMEASURED: could not read the machine's boot time, so the window between boot and this agent is unknown rather than zero"
fi

if [[ ! -x "$AMUX_BIN" ]]; then
  log "FATAL: amux CLI not executable at $AMUX_BIN — fleet NOT started"
  exit 1
fi

# Resolve the API base the same way the CLI does, so a port change cannot strand
# this script the way a hardcoded 8822 stranded so much else.
# AMUX_FLEET_BOOT_BASE exists so the server-down path can actually be exercised
# (ethos rule 7). Point it at a dead port and the WARN branch must fire; if it
# does not, this loop is not a check. Verified 2026-08-29 against a closed port.
base="${AMUX_FLEET_BOOT_BASE:-$("$AMUX_BIN" url 2>/dev/null || echo "https://localhost:8824")}"

# Wait for /health. Report which way it ended — "waited and gave up" and "came up
# in 3s" produce identical fleet outcomes on the happy path and completely
# different ones when a worker fails, so the log has to distinguish them.
# The endpoint is `/health`, NOT `/api/health` — the latter is a 404. This
# mattered because the first version of this loop tested `curl -sk ... >/dev/null`
# and read CURL'S EXIT STATUS, which is 0 for a perfectly-delivered 404. So it
# declared the server up, instantly, against an endpoint that does not exist, and
# would have declared it up just as fast with the server stopped. That is the
# amux-wide "read the BODY, never the exit code" rule (see cmd_fresh) in the one
# place where believing it costs the whole fleet: a false "up" here is what sends
# codex/ollama/gemini workers at a server that is not answering.
#
# So: check the HTTP CODE, and check that the body actually says status ok.
# PORTABLE TEMPLATE, and the `-t` form is why (AMUX-3965 follow-up).
#
# `mktemp -t name` with no trailing X's is accepted by BSD/macOS mktemp, which
# appends its own randomness, and REFUSED by GNU coreutils with "too few X's in
# template". On Linux this command therefore printed nothing, $health_tmp was the
# empty string, and `curl -o ""` failed on every iteration — so the health probe
# could never succeed and the boot logged "server never answered" no matter how
# healthy the server was.
#
# It went unseen because this box is macOS and the failure is silent on the arm
# that works. Caught by test-fleet-boot-divergence.sh running on a Linux CI
# runner: every POSITIVE cell failed and every NEGATIVE cell passed, which is the
# signature of a probe that never ran rather than a behaviour that changed.
health_tmp="$(mktemp "${TMPDIR:-/tmp}/amux-fleet-boot-health.XXXXXX")"
waited=0
server_up=0
while (( waited < HEALTH_TIMEOUT )); do
  code="$(curl -sk --max-time 5 "$base/health" -o "$health_tmp" -w '%{http_code}' 2>/dev/null || true)"
  if [[ "$code" == "200" ]] && grep -q '"status":"ok"' "$health_tmp" 2>/dev/null; then
    server_up=1
    break
  fi
  sleep 3
  waited=$((waited + 3))
done
rm -f "$health_tmp"

if (( server_up == 1 )); then
  log "server answered $base/health with status ok after ${waited}s"
else
  log "WARN: server did NOT return a healthy $base/health within ${HEALTH_TIMEOUT}s — starting the fleet anyway; codex/ollama/gemini workers may fail to launch"
fi

# The stagger matters more here than anywhere else: this runs while launchd is
# still bringing the rest of the machine up.
export AMUX_START_ALL_STAGGER="${AMUX_START_ALL_STAGGER:-2}"

summary="$("$AMUX_BIN" start-all 2>&1)"
rc=$?

# Strip ANSI so the log is greppable.
printf '%s\n' "$summary" | sed 's/\x1b\[[0-9;]*m//g' >> "$LOG"
log "start-all exited rc=$rc"

# Independent verdict. `start-all`'s own count is what IT believes it did; this
# is what the server can actually see, which is the number that matters and the
# one that would have exposed the original silent-abort bug immediately.
if (( server_up == 1 )); then
  # The payload goes to a FILE and python reads the file by name.
  #
  # Two quoting/plumbing traps were hit writing this, both of which produced a
  # verdict line that looked like a measured negative:
  #   * `python3 -c '...'` whose script contained an inner `'` — the shell closed
  #     the string early and the probe emitted nothing.
  #   * `curl ... | python3 - <<'EOF'` — the heredoc IS stdin, so it overrode the
  #     pipe and `json.load(sys.stdin)` got an empty read. curl was healthy the
  #     whole time (200, 173KB, 0.1s); only the plumbing was wrong.
  # Reading a named file has neither edge, and a missing/short file is a
  # distinguishable, reportable state rather than a silent empty parse.
  # Same portability trap as $health_tmp above.
  sess_tmp="$(mktemp "${TMPDIR:-/tmp}/amux-fleet-boot.XXXXXX")"
  http="$(curl -sk --max-time 45 "$base/api/sessions" -o "$sess_tmp" -w '%{http_code}' 2>/dev/null)"
  if [[ "$http" != "200" ]]; then
    verdict="VERDICT UNAVAILABLE: /api/sessions returned HTTP ${http:-<none>}"
  else
    # CLAIMED-UP, parsed from start-all's own summary. It counts `started` +
    # `already running`, which is what start-all asserts is up when it exits.
    #
    # AMUX-3965. `started` is incremented when `cmd_start --detach` returns 0, and
    # `tmux new-session -d` returns immediately — so the count is a claim about the
    # SPAWN CALL, and the verdict below is a claim about the WORLD. On the boot of
    # 2026-08-30 they differed by six: `55 started, 0 failed` while the verdict
    # named 7 workers down, 6 of them in start-all's own started list. Both numbers
    # were already in this script and nothing compared them, so the boot exited 0.
    claimed_up="$(printf '%s\n' "$summary" | sed 's/\x1b\[[0-9;]*m//g' \
      | awk '/^start-all:/ {
            s=0; a=0; seen=0
            for (i=1;i<=NF;i++) {
              if ($i=="started" && $(i-1) ~ /^[0-9]+$/)          { s=$(i-1); seen=1 }
              if ($i=="already" && $(i+1)=="running" && $(i-1) ~ /^[0-9]+$/) { a=$(i-1); seen=1 }
            }
            # Print NOTHING when neither field was found. Printing 0 here would
            # be indistinguishable from a real `0 started`, and the caller would
            # read a failed parse as a satisfied comparison — the same shape this
            # whole check exists to catch, one level down. A genuine zero still
            # prints, because `seen` tracks whether the TOKEN matched, not
            # whether the value was non-zero.
            if (seen) print s+a
            exit
          }')"
    verdict="$(SESS_FILE="$sess_tmp" CLAIMED_UP="${claimed_up:-}" python3 <<'PYEOF' 2>&1
import json, os
try:
    with open(os.environ["SESS_FILE"]) as fh:
        d = json.load(fh)
except Exception as e:
    print("VERDICT UNAVAILABLE: could not parse /api/sessions (%s)" % e)
    raise SystemExit(0)
live = [x for x in d if not x.get("archived")]
run = [x for x in live if x.get("running")]
down = sorted(x["name"] for x in live if not x.get("running"))
tail = (" · still down: " + ", ".join(down)) if down else " · all up"

# The divergence line comes FIRST, because on a successful boot nobody reads
# past the summary, and this is the only line that says the summary was wrong.
claimed = os.environ.get("CLAIMED_UP", "").strip()
if not claimed.isdigit():
    # Say the comparison did not run rather than let its absence read as
    # agreement (ethos rule 4). A parse failure here is not a clean boot.
    print("DIVERGENCE UNMEASURED: could not read start-all's claimed-up count "
          "from its summary, so 'started' was not checked against 'running'")
elif len(run) < int(claimed):
    print("DIVERGENCE: start-all claimed %s up, %d are actually running. "
          "`started` counts spawns that returned, not workers that came up. "
          "Down: %s" % (claimed, len(run), ", ".join(down) or "(none named)"))
    print("EXIT_NONZERO")

print("verdict: %d/%d non-archived workers running%s" % (len(run), len(live), tail))
PYEOF
)"
    # Carry the divergence into the EXIT STATUS, so a boot whose own log knows
    # something is wrong does not report success. The marker is stripped from
    # what gets logged; the human-readable DIVERGENCE line stays.
    if printf '%s\n' "$verdict" | grep -q '^EXIT_NONZERO$'; then
      verdict="$(printf '%s\n' "$verdict" | grep -v '^EXIT_NONZERO$')"
      (( rc == 0 )) && rc=1
    fi
  fi
  rm -f "$sess_tmp"
  log "${verdict:-VERDICT UNAVAILABLE: /api/sessions probe produced nothing}"
else
  log "verdict skipped — server never answered, so 'still down' could not be distinguished from 'unreadable'"
fi

log "=== fleet-boot done ==="
exit "$rc"
