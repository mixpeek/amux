#!/usr/bin/env bash
# Tests for host-responder.sh: a host trip wakes the host-health worker first and
# pages the owner only when a worker cannot help (critical, responder down, or
# still tripping ESCALATE_S after the worker started). Both outward calls are
# stubs that record each call, so the cells count real starts and real pages.
set -euo pipefail

HERE=$(cd "$(dirname "$0")" && pwd)
SUT="$HERE/host-responder.sh"
FAIL=0
RAN=0
cell() { # name expected actual
  RAN=$((RAN + 1))
  if [ "$2" = "$3" ]; then echo "PASS $1: $3"; else echo "FAIL $1: expected '$2' got '$3'"; FAIL=$((FAIL + 1)); fi
}
[ -f "$SUT" ] || { echo "FAIL: $SUT not found"; exit 1; }

d=$(mktemp -d "${TMPDIR:-/tmp}/hresp.XXXXXX")
runs="$d/runs"; pages="$d/pages"
printf '#!/bin/sh\necho x >> "%s"\necho "${RUN_CODE:-202}"\n' "$runs" > "$d/run.sh"
printf '#!/bin/sh\necho x >> "%s"\nexit "${PAGE_RC:-0}"\n' "$pages" > "$d/page.sh"
chmod +x "$d/run.sh" "$d/page.sh"
reset() { rm -rf -- "${d:?}/trips"; : > "$runs"; : > "$pages"; }
n() { wc -l < "$1" | tr -d ' '; }
r() { # <now> <severity> [RUN_CODE]
  RUN_CODE=${3:-202} PAGE_RC=${PAGE_RC:-0} AMUX_RESPONDER_COOLDOWN_S=${AMUX_RESPONDER_COOLDOWN_S:-1800} AMUX_RESPONDER_NOW=$1 AMUX_RESPONDER_DIR="$d/trips" \
    AMUX_RESPONDER_RUN_CMD="$d/run.sh" AMUX_RESPONDER_PAGE_CMD="$d/page.sh" \
    bash "$SUT" mac-memory "$2" "swap free 100 MB" "the message" 2>&1
}

# A warn trip starts the worker and does not page.
reset
out=$(r 1000 warn)
cell "warn_starts_worker" "1" "$(n "$runs")"
cell "warn_does_not_page" "0" "$(n "$pages")"
cell "warn_verdict" "yes" "$(printf '%s' "$out" | grep -q 'verdict=responder_started' && echo yes || echo no)"
cell "trip_file_names_reason" "yes" "$(grep -q '^reasons=swap free 100 MB$' "$d/trips/mac-memory" && echo yes || echo no)"

# The same trip 15 minutes later: the worker is on it, no second start, no page.
out=$(r 1900 warn)
cell "running_no_restart" "1" "$(n "$runs")"
cell "running_no_page" "0" "$(n "$pages")"

# Still tripping 30 minutes after the worker started: escalate to the owner.
out=$(r 2800 warn)
cell "unresolved_pages" "1" "$(n "$pages")"
cell "unresolved_names_why" "yes" "$(printf '%s' "$out" | grep -q 'escalation=unresolved' && echo yes || echo no)"

# Cleared, then a new trip: a fresh episode starts the worker again, no page.
AMUX_RESPONDER_DIR="$d/trips" bash "$SUT" clear mac-memory >/dev/null
out=$(r 5000 warn)
cell "fresh_episode_restarts_worker" "2" "$(n "$runs")"
cell "fresh_episode_no_page" "1" "$(n "$pages")"

# Restarting the worker more often than the escalation window cannot postpone
# the page: escalation counts from the FIRST start of the episode.
reset
AMUX_RESPONDER_COOLDOWN_S=600 r 1000 warn >/dev/null
AMUX_RESPONDER_COOLDOWN_S=600 r 1700 warn >/dev/null
out=$(AMUX_RESPONDER_COOLDOWN_S=600 r 2800 warn)
cell "restarts_do_not_postpone_page" "1" "$(n "$pages")"
cell "escalation_minutes_from_first_start" "yes" "$(printf '%s' "$out" | grep -q 'unresolved 30 min' && echo yes || echo no)"

# Critical pages at once, worker or not.
reset
out=$(r 1000 critical)
cell "critical_pages_now" "1" "$(n "$pages")"

# The responder cannot start (no schedule, refused, server down): page.
reset
out=$(r 1000 warn none)
cell "no_responder_pages" "1" "$(n "$pages")"
cell "no_responder_verdict" "yes" "$(printf '%s' "$out" | grep -q 'verdict=responder_failed+paged_owner' && echo yes || echo no)"
reset
out=$(r 1000 warn 409)
cell "refused_run_pages" "1" "$(n "$pages")"

# Page cooldown: a second critical inside the hour does not page again.
reset
r 1000 critical >/dev/null
out=$(r 1600 critical)
cell "page_cooldown_holds" "1" "$(n "$pages")"
cell "page_cooldown_verdict" "yes" "$(printf '%s' "$out" | grep -q 'page_cooldown' && echo yes || echo no)"

# A failed page is named, and not recorded as paged (so the next run retries).
reset
out=$(PAGE_RC=1 r 1000 critical)
cell "page_failure_named" "yes" "$(printf '%s' "$out" | grep -q 'page_failed' && echo yes || echo no)"
cell "page_failure_not_recorded" "yes" "$(grep -q '^paged=0$' "$d/trips/mac-memory" && echo yes || echo no)"

rm -rf -- "${d:?}"
echo
echo "ran=$RAN failed=$FAIL platform=$(uname -s)"
[ "$FAIL" -eq 0 ] || exit 1
