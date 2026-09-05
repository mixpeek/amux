#!/usr/bin/env bash
# test-append-only-substring-scope.sh — AF-432. The guard's classifier tests
# membership as a SUBSTRING over the pushed union, so a deleted line that some
# LONGER line happens to contain was rescued SILENTLY, and exit 0 was an
# unqualified claim about the file while resting, for that line, on nothing.
#
# The specimen: repairing AF-430 dropped `CARD: AF-10`, a line in no other file,
# and the guard passed without a word because `CARD: AF-106` contains it.
#
# The report must be PRECISE or it is worthless. Cell 2 is the one that decides
# that: an ordinary archive MOVE rescues every moved line by exact whole-line
# match elsewhere in the union, and reporting those too would fire the note on
# every retirement and teach skipping it. Measured on the real 55-commit push
# range that archived 47 entries, this reports exactly one line.
#
# Each cell writes BOTH files outright rather than restoring one from a commit.
# That is not stylistic: this repo's shared-checkout guard reads the tool call,
# cannot see a `cd` into a scratch repo, and refuses a restore naming a path a
# peer has edited — correctly, on the information it has (AF-435).
set -u
GUARD="$(cd "$(dirname "$0")" && pwd)/git-hooks/append-only-push-guard"
T=$(mktemp -d) || exit 2
trap 'rm -rf "$T"' EXIT
FAILS=0
fail() { echo "FAIL: $1" >&2; FAILS=$((FAILS + 1)); }

cd "$T" || exit 2
git init -q .
git config user.email t@test && git config user.name tester

# write <entry-one-card-line-or-empty> <archive-tail>
write() {
  { printf '# frustrations\n\nheader\n\n---\n\n'
    if [ -n "$1" ]; then
      printf '## entry one\nAREA: hooks\n%s\nSYMPTOM: alpha unique prose one.\n\n' "$1"
    fi
    printf '## entry two\nAREA: hooks\nCARD: AF-106\nSYMPTOM: beta unique prose two.\n'
  } > frustrations.md
  # %b, not %s: the archive tail carries \n escapes, and printing them
  # literally made the moved entry ONE long line, so its lines matched only as
  # substrings and cell 2 failed against a fixture that was not an archive.
  { printf '# archive\n\nheader\n\n---\n'; [ -n "$2" ] && printf '%b' "$2"; } > frustrations-archive.md
  git add -A && git commit -qm "$3"
  git rev-parse HEAD
}

BASE=$(write 'CARD: AF-10' '' base)

# 1) THE SPECIMEN. `CARD: AF-10` is dropped; `CARD: AF-106` still contains it.
#    Must NOT refuse (the substring test earns its keep) and must NOT be silent.
SUB=$(write 'CARD: AF-242' '' swap)
out=$("$GUARD" --check "$BASE" "$SUB" 2>&1); rc=$?
[ "$rc" = 0 ] || fail "a substring-only rescue must not refuse (got exit $rc)"
case "$out" in *"matched only as a"*) ;; *) fail "the substring rescue was SILENT — that is the whole defect" ;; esac
case "$out" in *"CARD: AF-10"*) ;; *) fail "the note did not name the line" ;; esac

# 2) THE CONTROL THAT DECIDES WHETHER THIS IS USABLE. An archive MOVE: entry one
#    leaves the ledger and appears verbatim in the archive. Whole-line survival,
#    not a substring rescue. Reporting it would fire on every retirement.
MOVE=$(write '' '\n## entry one\nAREA: hooks\nCARD: AF-10\nSYMPTOM: alpha unique prose one.\n' archive-move)
out=$("$GUARD" --check "$BASE" "$MOVE" 2>&1); rc=$?
[ "$rc" = 0 ] || fail "an ordinary archive move was refused (exit $rc)"
case "$out" in *"matched only as a"*) fail "an archive move fired the substring note — noise on every retirement" ;; esac

# 3) A genuinely lost line is still REFUSED. The new class must not swallow the
#    one thing this guard exists to stop.
DROP=$(write '' '' drop)
"$GUARD" --check "$BASE" "$DROP" >/dev/null 2>&1 \
  && fail "a real line loss passed — the deletion half regressed"

# 4) An in-place EXTENSION. The old line survives as a prefix of a longer one,
#    which is this file's most routine edit. It must not refuse. It IS reported,
#    because the check honestly cannot tell it from case 1 — saying so is the
#    point, and claiming to distinguish them would be the lie.
EXT=$(write 'CARD: AF-10 (repointed 2026-09-02, see NOTE-CARD)' '' extend)
out=$("$GUARD" --check "$BASE" "$EXT" 2>&1); rc=$?
[ "$rc" = 0 ] || fail "an in-place extension was refused (exit $rc)"
case "$out" in *"matched only as a"*) ;; *) fail "an extension should be reported as substring-only, not silently rescued" ;; esac

if [ "$FAILS" -eq 0 ]; then
  echo "ok: substring-scope — all 4 cases pass"
  exit 0
fi
echo "$FAILS case(s) failed" >&2
exit 1
