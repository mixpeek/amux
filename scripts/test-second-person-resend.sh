#!/usr/bin/env bash
# AF-468 — one second-person body reaching a SECOND lane silently misattributes work.
#
# WHY THIS FILE EXISTS. The check shipped verified only by hand, and the four cases
# lived in a commit message. A fixture in a commit message does not run. ts-gke, who
# reported the bug, made the point that decided it: knowing WHICH cases are real and
# which are constructed is part of what a fixture is worth, and that provenance had
# nowhere to live either.
#
# EXTRACTS the shipped function rather than reimplementing it, and pulls its callees
# transitively — the lesson of scripts/test-unstamped-ledger.sh, where AMUX-40 swapped
# a bare `curl` for `_curl`, the harness did not follow, and three assertions blamed
# the ledger for a missing dependency.
#
# Exit 0 = pass, 1 = failure.
set -uo pipefail
cd "$(dirname "$0")/.."

PASS=0; FAIL=0
# POSITIVE CONTROL COUNTER (ts-gke). Four of the six cases assert SILENCE, and a
# check that is dead is silent — so a totally broken check still passes them. They
# proved it: neutering the pronoun regex on a copy of the CLI scored 5 of 8, which
# READS as a partial regression and IS total failure. The suite's whole
# discriminating power lives in the FIRE cases, so the run refuses to report a
# verdict unless at least one of them actually fired.
FIRED=0
ok()  { PASS=$((PASS+1)); echo "  ok   — $1"; }
bad() { FAIL=$((FAIL+1)); echo "  FAIL — $1"; }

_fn_range() {
  awk -v want="$1" '
    $0 ~ "^"want"\\(\\) *\\{" { start=NR; depth=0 }
    start && /\{/ { depth += gsub(/\{/,"{") }
    start && /\}/ { depth -= gsub(/\}/,"}"); if (depth<=0) { print start","NR; exit } }
  ' amux
}
r=$(_fn_range _warn_second_person_resend)
if [ -z "$r" ]; then
  bad "could not locate _warn_second_person_resend in ./amux — the CLI moved and this test is blind"
  echo; echo "$PASS passed, $FAIL failed"; exit 1
fi
eval "$(sed -n "${r}p" amux)"
type -t _warn_second_person_resend >/dev/null || {
  bad "the function did not survive extraction — every assertion below would be vacuous"
  echo; echo "$PASS passed, $FAIL failed"; exit 1; }
ok "the shipped function extracted from ./amux"

TMP=$(mktemp -d); trap 'rm -rf "$TMP"' EXIT
export CC_HOME="$TMP"
fresh() { rm -f "$TMP/recent-sends.jsonl"; }
# $1=session $2=target $3=body  -> prints the warning, or nothing
fire() { AMUX_SESSION="$1" _warn_second_person_resend "$2" "$3" 2>&1; }

# ---------------------------------------------------------------------------
# CASE 1 — REAL. ts-gke's own 01:32 traffic, the day they reported this: two
# DISTINCT bodies, each naming its lane inline, both carrying second person. This
# is the true-negative a constructed case cannot give you, because it is what
# careful multi-lane correspondence actually looks like.
# ---------------------------------------------------------------------------
fresh
fire ts-gke mixpeek-cicd        "mixpeek-cicd: your caveat stands." >/dev/null
out=$(fire ts-gke mixpeek-frustrations "mixpeek-frustrations: your six messages.")
[ -z "$out" ] && ok "REAL: two distinct bodies, each naming its lane -> silent" \
              || bad "REAL: fired on correct multi-lane correspondence: $out"

# ---------------------------------------------------------------------------
# CASE 2 — REAL. ts-gke -> tubescience, then the SAME file to mvs-infra. mvs-infra
# read "YOUR 01:00-03:00 peak is confirmed" as theirs and flagged it; they had never
# run an executions-by-hour scan. The analysis landed on their record and in a card.
# ---------------------------------------------------------------------------
fresh
B="Your 01:00-03:00 peak is confirmed, and you wrote that the scan was clean."
fire ts-gke tubescience "$B" >/dev/null
out=$(fire ts-gke mvs-infra "$B")
case "$out" in
  *"you already sent this body to tubescience"*) FIRED=$((FIRED+1)); ok "REAL: same body to a NEW lane -> warns, naming the prior lane" ;;
  "") bad "REAL: the reported incident produced NO warning" ;;
  *)  bad "REAL: warned with unexpected wording: $out" ;;
esac
case "$out" in *"2 second-person reference"*) ok "counts the references so the author can judge scale" ;;
  *) bad "did not count second-person references: $out" ;; esac

# ---------------------------------------------------------------------------
# CASE 3 — CONSTRUCTED, and ts-gke was explicit that it must be: their ledger holds
# four rows and no retry, so no real specimen exists. This is the silence chosen ON
# PURPOSE and therefore the one most worth pinning — a warning the author has already
# acted on is how a check gets tuned out (ethos rule 5).
# ---------------------------------------------------------------------------
out=$(fire ts-gke tubescience "$B")
[ -z "$out" ] && ok "CONSTRUCTED: retry to a lane that already has it -> silent" \
              || bad "CONSTRUCTED: warned on a retry the author already acted on: $out"

# ---------------------------------------------------------------------------
# CASE 4 — CONSTRUCTED. The RELAY: a different lane forwards a body verbatim. The
# FIRE is correct (a forwarded "you" meant the forwarder); the PROSE is what broke,
# because "this body was already sent to X" is false in the forwarder's mouth.
# ---------------------------------------------------------------------------
fresh
fire mixpeek-cicd mvs-infra "$B" >/dev/null
out=$(fire amux-frustrations backend "$B")
case "$out" in
  *"already sent to mvs-infra by mixpeek-cicd"*) FIRED=$((FIRED+1)); ok "CONSTRUCTED: a relay names the ORIGINAL sender, not the forwarder" ;;
  *"you already sent"*) bad "a relay claims the forwarder sent it — the pre-df33f271 wording: $out" ;;
  *) bad "relay produced unexpected output: $out" ;;
esac

# ---------------------------------------------------------------------------
# CASE 5 — CONSTRUCTED. The fleet-send seam. mixpeek's scripts/fleet-send.sh refuses
# a second-person body at N>1 and offers --allow-second-person; it then loops
# `amux send`, so recipients 2..N would warn AFTER the author already decided. It
# exports the marker only when N>1 (ts-gke, e273c2c3bc) — at N=1 that script is the
# authority on nothing and the warning is exactly what you want to see.
# ---------------------------------------------------------------------------
fresh
fire ts-gke a "$B" >/dev/null
out=$(AMUX_SEND_MULTI_ACK=fleet-send fire ts-gke b "$B")
[ -z "$out" ] && ok "CONSTRUCTED: AMUX_SEND_MULTI_ACK suppresses an already-decided send" \
              || bad "CONSTRUCTED: the marker did not suppress: $out"

# ---------------------------------------------------------------------------
# CASE 6 — CONSTRUCTED. No second person: repetition alone is not the defect.
# ---------------------------------------------------------------------------
fresh
fire ts-gke a "The build is green; the scan found nothing." >/dev/null
out=$(fire ts-gke b "The build is green; the scan found nothing.")
[ -z "$out" ] && ok "CONSTRUCTED: a repeated body with NO second person -> silent" \
              || bad "CONSTRUCTED: fired without second person: $out"

echo
# THE POSITIVE CONTROL. Report NOTHING normal if the check never fired: a pass
# count is a poor summary of a suite whose silence cases cannot distinguish a
# working check from a dead one.
if [ "$FIRED" -eq 0 ]; then
  echo "VERDICT WITHHELD — the check NEVER FIRED in any case that should have."
  echo "  $PASS of the assertions above are SILENCE assertions, and silence is"
  echo "  exactly what a dead check produces. This run cannot tell a working"
  echo "  check from one that can no longer fire at all. Treat it as BROKEN,"
  echo "  not as $PASS passed."
  exit 1
fi
echo "$PASS passed, $FAIL failed   (2 cases REAL, 4 CONSTRUCTED — see comments)"
echo "positive control: $FIRED fire case(s) actually fired, so the silences mean something"
[ "$FAIL" -eq 0 ] || exit 1
