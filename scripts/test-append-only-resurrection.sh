#!/usr/bin/env bash
# test-append-only-resurrection.sh — MG-1485: the append-only guard's
# RESURRECTION half. Rebuilds the incident shape in a scratch repo: an archive
# campaign moves an entry into FRUSTRATIONS_ARCHIVE.md, then a stale republish
# re-adds it to FRUSTRATIONS.md with the archive untouched. The union (loss)
# rule passes that by construction; the resurrection check must refuse it —
# while the campaign itself, an innocent append over inherited duplication,
# and the deliberate re-open path all still pass, and the deletion half keeps
# refusing what it always refused.
set -u

GUARD="$(cd "$(dirname "$0")" && pwd)/git-hooks/append-only-push-guard"
T=$(mktemp -d) || exit 2
trap 'rm -rf "$T"' EXIT
FAILS=0
fail() { echo "FAIL: $1" >&2; FAILS=$((FAILS + 1)); }

cd "$T" || exit 2
git init -q .
git config user.email t@test && git config user.name tester

printf 'FRUSTRATIONS.md\nFRUSTRATIONS_ARCHIVE.md\n' > .append-only-files
printf -- '- [ ] 2026-08-01 alpha entry\n- [ ] 2026-08-02 beta entry\n' > FRUSTRATIONS.md
printf -- '# archive\n' > FRUSTRATIONS_ARCHIVE.md
git add -A && git commit -qm base
BASE=$(git rev-parse HEAD)

# The archive campaign: alpha flips [x] and MOVES to the archive.
printf -- '- [ ] 2026-08-02 beta entry\n' > FRUSTRATIONS.md
printf -- '# archive\n- [x] 2026-08-01 alpha entry\n' > FRUSTRATIONS_ARCHIVE.md
git add -A && git commit -qm campaign
CAMPAIGN=$(git rev-parse HEAD)

# 1) The campaign passes: a move is not a loss (the union rule's whole point),
#    and moving INTO the archive is not a resurrection.
"$GUARD" --check "$BASE" "$CAMPAIGN" >/dev/null 2>&1 \
  || fail "the archive campaign (a legitimate move) was refused"

# The stale republish: alpha comes BACK into the active file, still [x],
# archive untouched. Nothing is removed, so the loss rule sees nothing.
printf -- '- [ ] 2026-08-02 beta entry\n- [x] 2026-08-01 alpha entry\n- [ ] 2026-08-21 gamma new\n' > FRUSTRATIONS.md
git add -A && git commit -qm stale-republish
STALE=$(git rev-parse HEAD)

# 2) Resurrection refused, naming the direction the union rule cannot see.
if "$GUARD" --check "$CAMPAIGN" "$STALE" >/dev/null 2>err.txt; then
  fail "the stale republish (resurrection) PASSED — the union rule's blind half is open"
else
  grep -q "RE-ADDS" err.txt || fail "the refusal does not name the resurrection"
fi

# 3) The path-scoped override still works (a deliberate rewrite stays the
#    author's to make, out loud).
AMUX_ALLOW_SHARED_REWRITE=FRUSTRATIONS.md "$GUARD" --check "$CAMPAIGN" "$STALE" >/dev/null 2>&1 \
  || fail "the scoped override did not allow the acknowledged resurrection"

# 4) Inherited duplication never blocks an innocent pusher: with the dup
#    already on the remote (base=STALE), an ordinary append passes with a WARN.
printf -- '- [ ] 2026-08-02 beta entry\n- [x] 2026-08-01 alpha entry\n- [ ] 2026-08-21 gamma new\n- [ ] 2026-08-21 delta newer\n' > FRUSTRATIONS.md
git add -A && git commit -qm innocent-append
INNOCENT=$(git rev-parse HEAD)
if "$GUARD" --check "$STALE" "$INNOCENT" >/dev/null 2>warn.txt; then
  grep -q "inherited duplication" warn.txt \
    || fail "inherited duplication passed silently — the WARN is the visibility"
else
  fail "an innocent append was refused for duplication it did not introduce"
fi

# 5) The deliberate RE-OPEN path: re-add to active AND remove from the archive
#    in the same push. The union rule reads that as the move it is; the
#    resurrection check sees no duplication. No override needed.
printf -- '- [ ] 2026-08-02 beta entry\n- [ ] 2026-08-01 alpha entry\n' > FRUSTRATIONS.md
printf -- '# archive\n' > FRUSTRATIONS_ARCHIVE.md
git add -A && git commit -qm reopen
REOPEN=$(git rev-parse HEAD)
"$GUARD" --check "$CAMPAIGN" "$REOPEN" >/dev/null 2>&1 \
  || fail "a clean re-open (active gains, archive loses, no duplication) was refused"

# 6) The deletion half still refuses what it always refused: drop an entry
#    that exists in neither file afterwards.
git checkout -q "$STALE" -- FRUSTRATIONS.md FRUSTRATIONS_ARCHIVE.md
printf -- '- [x] 2026-08-01 alpha entry\n' > FRUSTRATIONS.md
git add -A && git commit -qm clobber
CLOBBER=$(git rev-parse HEAD)
"$GUARD" --check "$STALE" "$CLOBBER" >/dev/null 2>&1 \
  && fail "a real line loss passed — the deletion half regressed"

if [ "$FAILS" -eq 0 ]; then
  echo "ok: resurrection guard — all 6 cases pass"
  exit 0
fi
echo "$FAILS case(s) failed" >&2
exit 1
