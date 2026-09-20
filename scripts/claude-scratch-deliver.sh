#!/usr/bin/env bash
# Deliver each lane its own claude-scratch-report findings AS A BOARD CARD.
#
# WHY A CARD AND NOT A MESSAGE (AMUX-4732). On 2026-09-14 three lanes were asked
# by peer message to clean up their scratchpads:
#
#   mixpeek-homepage    running      accepted    40.3 G -> 21.59 G   -18.7 G
#   mixpeek-ops-server  not running  ask lost    36.8 G -> 43.18 G    +6.4 G
#   mixpeek-cicd        paused       ask lost     9.3 G -> 15.60 G    +6.3 G
#
# A peer message needs the recipient to be UP. Both lanes that were down kept
# growing, and the delivery had no way to report that it had reached nobody. A
# board card sits on the lane's board until it is picked up, and
# `amux board request` arms a callback when it reaches a terminal status.
#
# THIS SCRIPT NEVER DELETES ANYTHING, and neither does the card it files. Same
# promise as the report it reads: the lane that owns the bytes decides.
#
# Usage:
#   scripts/claude-scratch-deliver.sh                 # PLAN ONLY, files nothing
#   scripts/claude-scratch-deliver.sh --apply         # actually file/update
#   scripts/claude-scratch-deliver.sh --from-tsv FILE # use a captured report
#
# DRY RUN IS THE DEFAULT, deliberately. Filing on another lane's board is
# outbound, and the report's own worst case is an ambiguous row naming 19
# candidate lanes. The plan is printed so a human or a later run can read what
# would be sent before anything is.
set -uo pipefail
cd "$(dirname "$0")/.."

APPLY=0
MIN_GB=1.0
FROM_TSV=""
# The idempotency key. It goes in the TITLE because a title is what a duplicate
# check can see cheaply on another lane's board, and it must not change between
# runs or every tick files a new card (constraint 2: the board already has
# 12,745 rows).
MARKER="[scratch]"

while [ $# -gt 0 ]; do
  case "$1" in
    --apply)    APPLY=1; shift ;;
    --min-gb)   MIN_GB=$2; shift 2 ;;
    --from-tsv) FROM_TSV=$2; shift 2 ;;
    -h|--help)  sed -n '1,30p' "$0"; exit 0 ;;
    *) echo "unknown argument: $1" >&2; exit 2 ;;
  esac
done

API="${AMUX_URL:-$(amux url 2>/dev/null || echo https://localhost:8824)}"
ME="${AMUX_SESSION:-amux}"

TSV=$(mktemp "${TMPDIR:-/tmp}/csd.XXXXXX")
trap 'rm -f "$TSV"' EXIT
if [ -n "$FROM_TSV" ]; then
  cat "$FROM_TSV" > "$TSV"
else
  ./scripts/claude-scratch-report.sh --tsv --min-gb "$MIN_GB" > "$TSV" 2>/dev/null
fi

# ---------------------------------------------------------------------------
# Constraint 1: ONLY DELIVER WHAT IS ATTRIBUTABLE.
#
# The report emits `AMBIGUOUS:19-candidates` for /Users/ethan/Dev/mixpeek and
# `AMBIGUOUS:10-candidates` for /Users/ethan/Dev/amux, because that many lanes
# share the CC_DIR. Filing "your scratchpad is 15.6 GB" on 19 boards is 18 wrong
# cards, and a wrong card costs the recipient a read and a judgement.
#
# So: a row is deliverable only when the OWNER column is a single lane name and
# the REACH column says plainly `reachable`. `reachable-via-candidates` is the
# ambiguous case wearing a reassuring word, and it is excluded here on purpose.
# Everything else is counted and reported, never silently dropped.
# ---------------------------------------------------------------------------
# TWO THINGS THIS PREDICATE LEARNED FROM ITS FIRST LIVE RUN, both of which a
# hermetic fixture had not shown:
#
# 1. THE REPORT PRINTS A PROSE FOOTER EVEN UNDER --tsv. Its summary block
#    ("276 conversation(s), 118.9 GB total", the BY LIVENESS / BY REACH tables)
#    is not inside the `if TSV != 1` guard, so those lines arrived as rows. The
#    first run counted 287 rows against 276 real conversations and listed two
#    footer lines as findings. A row is now required to have SIX tab-separated
#    fields with a NUMERIC size, which prose cannot satisfy.
#
# 2. REACH IS A COMMA-SEPARATED FLAG SET, not a single word. The live value for
#    the biggest finding was `reachable,escalated-09-14`, and an exact match on
#    "reachable" dropped it. That lane was holding 29.12 GB and is one of the
#    two that never received the 2026-09-14 message, so the flag recording the
#    escalation is exactly what suppressed the next one. Tokenise and look for
#    `reachable` as a WHOLE token: `reachable-via-candidates` is a different
#    token and must still be excluded, which a substring test would not do.
_awk_reach='
  function deliverable_row(  i, n, parts) {
    if (NF != 6) return 0
    if ($1 !~ /^[0-9]+(\.[0-9]+)?$/) return 0
    if ($1 + 0 < min + 0) return 0
    if ($5 ~ /^AMBIGUOUS:/ || $5 == "unattributed" || $5 == "") return 0
    n = split($4, parts, ",")
    for (i = 1; i <= n; i++) if (parts[i] == "reachable") return 1
    return 0
  }
  function is_row() { return NF == 6 && $1 ~ /^[0-9]+(\.[0-9]+)?$/ && $1 + 0 >= min + 0 }
'
deliverable=$(awk -F'\t' -v min="$MIN_GB" "$_awk_reach"' deliverable_row() { print }' "$TSV")
skipped=$(awk -F'\t' -v min="$MIN_GB" "$_awk_reach"' is_row() && !deliverable_row() { print }' "$TSV")

# Count only real rows, for the same reason: a denominator that includes the
# footer is a number that measures the parser rather than the tree.
n_rows=$(awk -F'\t' -v min="$MIN_GB" "$_awk_reach"' is_row() { n++ } END { print n + 0 }' "$TSV")
n_deliver=$(printf '%s' "$deliverable" | grep -c . || true)
n_skip=$(printf '%s' "$skipped" | grep -c . || true)

echo "claude-scratch-deliver: $n_rows row(s) at >= ${MIN_GB}GB; $n_deliver deliverable, $n_skip not attributable"
[ "$APPLY" = 1 ] || echo "PLAN ONLY. Nothing will be filed. Re-run with --apply to deliver."
echo

# Report the skips WITH THEIR REASON rather than dropping them. An ambiguous row
# is still 15 GB somebody has to deal with; it just cannot be addressed to one
# lane, and a delivery that stayed silent about it would read as "nothing else
# to do".
if [ -n "$skipped" ]; then
  echo "NOT DELIVERED (no single owner to address):"
  printf '%s\n' "$skipped" | while IFS=$'\t' read -r gb tr cls reach owner conv; do
    printf '  %-7s %-28s %s\n' "${gb}GB" "$(printf '%s' "$owner" | cut -c1-28)" "$conv"
  done
  echo
fi

created=0; updated=0; refused=0
verdicts=""

lanes=$(printf '%s\n' "$deliverable" | awk -F'\t' '{print $5}' | sort -u | grep -c . || true)
echo "DELIVERY ($lanes lane(s) with a unique owner):"

for lane in $(printf '%s\n' "$deliverable" | awk -F'\t' '{print $5}' | sort -u); do
  [ -n "$lane" ] || continue
  rows=$(printf '%s\n' "$deliverable" | awk -F'\t' -v l="$lane" '$5 == l')
  total=$(printf '%s\n' "$rows" | awk -F'\t' '{s += $1} END {printf "%.2f", s}')
  body=$(printf '%s\n' "$rows" | awk -F'\t' '{printf "  %s GB  %s  last write %s  %s\n", $1, $3, $2, $6}')

  title="$MARKER $lane is holding ${total} GB in ${ROOT:-its scratch root}"

  # Constraint 2: FIND AN EXISTING CARD BEFORE FILING ONE. Keyed on the marker
  # plus the lane, over that lane's own non-terminal cards.
  existing=$(curl -sk --max-time 60 "$API/api/board?all=1&session=$lane" 2>/dev/null | python3 -c '
import json, sys
try:
    rows = json.load(sys.stdin)
except Exception:
    sys.exit(0)
if not isinstance(rows, list):
    sys.exit(0)
TERMINAL = {"done", "verified", "discarded", "quarantined", "cancelled"}
for r in rows:
    if not isinstance(r, dict):
        continue
    if (r.get("status") or "") in TERMINAL:
        continue
    if str(r.get("title") or "").startswith(sys.argv[1]):
        print(r.get("id") or "")
        break
' "$MARKER" 2>/dev/null)

  if [ "$APPLY" != 1 ]; then
    if [ -n "$existing" ]; then
      printf '  %-26s would UPDATE %s (%s GB)\n' "$lane" "$existing" "$total"
    else
      printf '  %-26s would CREATE (%s GB)\n' "$lane" "$total"
    fi
    continue
  fi

  note="Your scratchpad under ${ROOT:-your scratch root} is holding ${total} GB.

$body
Reported by scripts/claude-scratch-report.sh, run from $ME. NOTHING HAS BEEN
DELETED and nothing will be: the lane that owns the bytes decides, which is why
this is a card and not a cleanup.

Why a card rather than a message: on 2026-09-14 the same ask went out as peer
messages and the two lanes that were DOWN never received it. Both grew ~6 GB
more while the one that was up reclaimed 18.7 GB.

A conversation whose transcript has been silent for days is the safe place to
start; the report's STALE SUBFOLDERS section names those. Close this card when
you have decided, even if the decision is to keep it all."

  if [ -n "$existing" ]; then
    if printf '%s' "$note" | amux board progress "$existing" --stdin >/dev/null 2>&1; then
      updated=$((updated + 1)); verdicts="$verdicts\n  $lane\tupdated\t$existing"
      printf '  %-26s UPDATED %s\n' "$lane" "$existing"
    else
      refused=$((refused + 1)); verdicts="$verdicts\n  $lane\trefused\tdesc_append to $existing"
      printf '  %-26s REFUSED (desc_append to %s)\n' "$lane" "$existing"
    fi
  else
    # Constraint 4: A REFUSAL MUST BE REPORTED AS A REFUSAL. `curl` exits 0 on a
    # 403, and cross_board_create_forbidden is a real refusal a worker can hit,
    # so this goes through `amux board request`, the sanctioned cross-board
    # verb, and reads its exit status rather than assuming it worked.
    # `if out=$(...)` rather than assigning and then reading `$?`: the two are
    # equivalent here and only one of them stays correct if a `local` or another
    # command is ever inserted between them.
    if out=$(printf '%s' "$note" | amux board request "$lane" "$title" --desc-stdin --type chore 2>&1); then
      created=$((created + 1)); verdicts="$verdicts\n  $lane\tcreated\t$(printf '%s' "$out" | tr '\n' ' ' | cut -c1-60)"
      printf '  %-26s CREATED %s\n' "$lane" "$(printf '%s' "$out" | head -1)"
    else
      refused=$((refused + 1)); verdicts="$verdicts\n  $lane\trefused\t$(printf '%s' "$out" | tr '\n' ' ' | cut -c1-60)"
      printf '  %-26s REFUSED %s\n' "$lane" "$(printf '%s' "$out" | tr '\n' ' ' | cut -c1-70)"
    fi
  fi
done

echo
# THE SUMMARY IS COMPUTED, NEVER WRITTEN. A hardcoded "all delivered" cannot
# disagree with the run, which is the whole reason the 09-14 attempt is only
# reconstructable because a human wrote it on a card by hand.
echo "delivered=$created updated=$updated refused=$refused not_attributable=$n_skip over $n_rows row(s)"
[ "$refused" -eq 0 ]
