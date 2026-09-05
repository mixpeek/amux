#!/usr/bin/env bash
# test-frustrations-archive-move.sh — AF-436. Retiring an entry is a MOVE across
# TWO files: a deletion from frustrations.md and an append to
# frustrations-archive.md. The script's summary named neither, so the obvious
# next command is `git add frustrations.md` — the file you were reading, the
# file whose line number you passed — which stages the deletion WITHOUT the
# append and produces a commit holding the entry in neither file.
#
# That is the lost-work state AF-430 was filed about. Self-inflicted 2026-09-03
# in eb552cc1, on MR-44, five hours after AF-430 restored MR-44 from an earlier
# instance of the same shape.
#
# Cell 2 is the control and the reason this is a test rather than a comment: the
# hint must name BOTH files, because a hint naming one is the bug with extra
# words.
set -u
SRC="$(cd "$(dirname "$0")/.." && pwd)"
T=$(mktemp -d) || exit 2
trap 'rm -rf "$T"' EXIT
FAILS=0
fail() { echo "FAIL: $1" >&2; FAILS=$((FAILS + 1)); }

# The script resolves LEDGER/ARCHIVE from its OWN location
# (Path(__file__).resolve().parent.parent), so a scratch run needs the same
# layout. Copy, do not symlink: resolve() follows symlinks straight back to the
# real repo, and this test would then archive a live entry. The copy is made
# here, at test time, from the shipped file — so what runs is the shipped bytes.
mkdir -p "$T/repo/scripts"
cp "$SRC/scripts/frustrations-archive.py" "$T/repo/scripts/"
cd "$T/repo" || exit 2
cat > frustrations.md <<'LEDGER'
# frustrations

Format — fixed fields so this greps
  AREA: x

---

## a retirable entry
AREA: hooks
SEVERITY: annoys
STATUS: fixed
DATE: 2026-09-03
SESSION: tester
CARD: none
SYMPTOM: something happened.
COST: five minutes.
FIX: it was fixed.
LEDGER
printf '# amux frustrations: archive\n\nheader\n\n---\n' > frustrations-archive.md

LN=$(grep -n '^## a retirable entry' frustrations.md | cut -d: -f1)
out=$(AMUX_URL=http://127.0.0.1:1 python3 "$T/repo/scripts/frustrations-archive.py" "$LN" tester \
      --evidence-stdin <<< "evidence line" 2>&1)

# 1) The move happened.
grep -q '^## a retirable entry' frustrations-archive.md \
  || fail "the entry did not reach the archive"
grep -q '^## a retirable entry' frustrations.md \
  && fail "the entry is still in the ledger"

# 2) THE CONTROL, and the point of the cell: the output names BOTH files. A hint
#    naming only the ledger is the defect with extra words.
case "$out" in *frustrations.md*) ;; *) fail "the output does not name the ledger" ;; esac
case "$out" in *frustrations-archive.md*) ;; *) fail "the output does not name the ARCHIVE — this is the whole defect" ;; esac
case "$out" in *"TWO files"*) ;; *) fail "the output does not say it was a two-file move" ;; esac

# 3) It offers a runnable command, and by PATHSPEC — `git add -A` is refused by
#    this repo's own shared-checkout guard, so a hint suggesting it is a hint
#    nobody can follow.
case "$out" in *"git add frustrations.md frustrations-archive.md"*) ;; *) fail "no runnable two-file stage command" ;; esac
case "$out" in *"git add -A"*) fail "suggested git add -A, which the shared-checkout guard refuses" ;; esac

if [ "$FAILS" -eq 0 ]; then
  echo "ok: archive move — all 3 cases pass"
  exit 0
fi
echo "$FAILS case(s) failed" >&2
exit 1
