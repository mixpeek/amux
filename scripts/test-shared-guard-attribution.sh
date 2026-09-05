#!/usr/bin/env bash
# Cells for git-shared-guard.py's DISCARD REFUSAL WORDING (AF-423).
#
# WHY THIS EXISTS. The block itself is right and well covered by use: `git
# checkout --` on a shared checkout is unrecoverable and refusing it is correct.
# What had no coverage is the ATTRIBUTION inside the refusal, and that is where
# it was wrong: the server sends `owner: "(unknown)"` with `peer: false` when
# there is NO peer record, and this guard rendered the placeholder as a name —
# "(recently edited by (unknown))", closing with "or ask (unknown) first".
#
# The sibling guard has branched on `peer` since AF-24. This one had not, which
# is the same shape as AF-410 and AF-420: a fix that reached one emitter of
# several. So the property worth pinning is not "does it block" but "does it
# name somebody it cannot support naming".
#
# The cells exec the SHIPPED renderer lifted out of the hook file, not a retyped
# copy, so the strings under test are the strings that ship (ethos rule 7).
set -uo pipefail
cd "$(dirname "$0")/.."
HOOK="${SHARED_GUARD_HOOK:-$(pwd)/scripts/git-hooks/git-shared-guard.py}"
PASS=0; FAIL=0
ok()  { PASS=$((PASS+1)); echo "  ok   $1"; }
bad() { FAIL=$((FAIL+1)); echo "  FAIL $1"; }

echo "== shared-guard attribution cells =="

cell() {
  # $1 = name, $2 = python assertions with `msg(hits, foreign)` available
  if python3 - "$HOOK" <<PY 2>&1
import re, sys
src = open(sys.argv[1]).read()
# Lift the refusal tail out of _discard_verdict: everything from the AF-423
# marker to the end of that function. Extracting rather than retyping is the
# point — a retyped copy would pass while the shipped strings rotted.
start = src.index("    # AF-423:")
end = src.index("\ndef _has_cotenants", start)
body = src[start:end]
ns = {}
wrapper = "def msg(hits, foreign):\n" + body
exec(compile(wrapper, "<lifted>", "exec"), ns)
msg = ns["msg"]
$2
PY
  then ok "$1"; else bad "$1"; fi
}

# THE SPECIMEN: no peer record at all. The server's placeholder must not be
# rendered as a session, and the remedy must not tell anyone to go ask it.
cell "placeholder owner names nobody, and no ask-them remedy" '
m = msg([{"path": "a.rs", "owner": "(unknown)", "peer": False}], [])
assert "(unknown)" not in m, m
assert "ask (unknown)" not in m, m
assert "ANOTHER SESSION HAS ALSO EDITED" not in m, m
assert "NO other session" in m, m
assert "UNRECOVERABLE" in m, "the block must still be explained: " + m
assert "git stash push" in m, "the recoverable alternative must survive: " + m
'

# CONTROL 1, the half that keeps this honest: a REAL peer must still be named,
# in both slots. Without this, deleting the attribution entirely would pass.
cell "a real peer is still named, and still asked" '
m = msg([{"path": "a.rs", "owner": "backend", "peer": True}], [{"path": "a.rs"}])
assert "backend" in m, m
assert "ask backend first" in m, m
assert "belongs to another session" in m, m
'

# CONTROL 2: an EMPTY owner is the same non-answer as the placeholder. It used
# to render as "an edit record with no session attached", which reads fine in
# the first slot and absurd in "or ask <that> first".
cell "an empty owner names nobody either" '
m = msg([{"path": "a.rs", "owner": "", "peer": False}], [])
assert "NO other session" in m, m
assert "or ask  first" not in m and "ask an edit record" not in m, m
'

# CONTROL 3: an OLD SERVER sends no `peer` key. Absent means "cannot answer",
# not "answer is no" — a real-looking name must still be honoured, or the fix
# silently strips every attribution against an older server.
cell "a named owner with no peer flag is still honoured" '
m = msg([{"path": "a.rs", "owner": "mvs-infra"}], [])
assert "mvs-infra" in m, m
assert "ask mvs-infra first" in m, m
'

# THE CELL A MUTATION FOUND MISSING. Every placeholder cell above also sets
# `peer: False`, so the flag alone decided them and the string check was never
# load-bearing — removing "(unknown)" from the exclusion list passed the whole
# suite. An OLD SERVER sends the placeholder with NO `peer` key, and that is
# exactly where the string check has to work: control 3 says an unflagged name
# is honoured, so without this the placeholder would be honoured too.
cell "an old server's placeholder is still not a name" '
m = msg([{"path": "a.rs", "owner": "(unknown)"}], [])
assert "(unknown)" not in m, m
assert "ask (unknown)" not in m, m
assert "NO other session" in m, m
'

# CONTROL 4: mixed hits. One nameable peer among placeholders must still be
# named — the presence of an unattributed row is not a reason to drop a real one.
cell "a real peer among placeholders is still named" '
m = msg([{"path": "a.rs", "owner": "(unknown)", "peer": False},
         {"path": "b.rs", "owner": "ts-gke", "peer": True}], [])
assert "ts-gke" in m, m
assert "(unknown)" not in m, m
'

echo "  $PASS passed, $FAIL failed"
[ "$FAIL" -eq 0 ]
