#!/usr/bin/env bash
# Cells for the staged-guard hook's unaccounted-lines RENDERING (AF-342).
#
# WHY THIS EXISTS. The server-side decision has unit tests in git_guard.rs, but
# what a lane actually experiences is what this hook PRINTS, and that half had
# no coverage at all. The defect being fixed was entirely a printing volume
# problem: 93 lines of "matching nothing you edited firsthand" on commit
# 40fa0ce0, across four files written start to finish by one session with no
# peer involved. Nothing was wrong with the verdict; the noise was the bug.
#
# A warning that fires on the normal path is one people learn to scroll past,
# and it takes the real signal with it, so "stays silent when it should" is a
# behaviour worth pinning rather than trusting to a future reader's restraint.
#
# The cells exec the SHIPPED rendering block lifted out of the hook file, not a
# retyped copy: the strings under test are the strings that ship (ethos rule 7).
set -uo pipefail
cd "$(dirname "$0")/.."
HOOK="${STAGED_GUARD_HOOK:-$(pwd)/scripts/git-hooks/amux-staged-guard}"
PASS=0; FAIL=0
ok()  { PASS=$((PASS+1)); echo "  ok   $1"; }
bad() { FAIL=$((FAIL+1)); echo "  FAIL $1"; }

echo "== staged-guard render cells =="

run_cell() {
  # $1 = cell name, $2 = python assertions with `render(d)` available
  if python3 - "$HOOK" <<PY 2>&1
import io, sys
src = open(sys.argv[1]).read()
start = src.index('    for _ua in d.get("unaccounted") or []:')
end   = src.index('    _render_split_risk(d, w)')
block = "\n".join(l[4:] if l.startswith("    ") else l
                  for l in src[start:end].splitlines())

def render(d):
    buf = io.StringIO()
    exec(block, {"d": d, "w": buf.write})
    return buf.getvalue()

$2
PY
  then ok "$1"; else bad "$1"; fi
}

# The case that motivated the fix: every staged path was shell-written by the
# committer, so nothing is unaccounted and three paths could not be checked.
# Silence is the whole point. If this cell ever fails, the noise is back.
run_cell "silent when only undecidable paths exist (the 93-line noise case)" '
out = render({"unaccounted": [], "unaccounted_undecidable": [
    {"path": "scripts/friction_themes.py"},
    {"path": "docs/friction-themes.md"},
    {"path": ".github/workflows/checks.yml"}]})
assert out == "", "expected silence, got:\n" + out
'

# A real finding BESIDE an undecidable path. The list the reader is looking at
# is partial, and a path the check could not run on must not read as one that
# passed. This is the measured/n_considered contract (AF-320) at the point of
# display rather than in the payload.
run_cell "partial result is labelled partial when a real finding is shown" '
out = render({"unaccounted": [{"path": "src/routes.rs", "count": 2,
                               "lines": ["RouteEntry { path: \"/api/x\" },"]}],
              "unaccounted_undecidable": [{"path": "docs/notes.md"}]})
assert "src/routes.rs" in out, out
assert "could not run on 1 other path" in out, out
assert "docs/notes.md" in out, "the undecidable path must be NAMED: " + out
'

# No partiality note when there is nothing to be partial about.
run_cell "no partiality note when nothing was skipped" '
out = render({"unaccounted": [{"path": "src/routes.rs", "count": 1, "lines": []}],
              "unaccounted_undecidable": []})
assert "could not run" not in out, out
assert "src/routes.rs" in out, out
'

# An older server does not send the key. The hook must degrade to its previous
# output rather than raising: hooks and server versions move independently, and
# this one runs on every commit in every checkout on the machine.
run_cell "missing key from an older server degrades to the old output" '
out = render({"unaccounted": [{"path": "a.rs", "count": 1, "lines": []}]})
assert "could not run" not in out, out
assert "a.rs" in out, out
'

# The real-peer-hunk path must still print in full. This is the cell that stops
# the fix from being a hollowing-out: a hook that printed NOTHING, ever, would
# pass all four cells above.
run_cell "a genuine unaccounted line still prints its content and the remedy" '
out = render({"unaccounted": [{"path": "src/routes.rs", "count": 1,
                               "lines": ["RouteEntry { path: \"/api/reclaim\" },"]}],
              "unaccounted_undecidable": []})
assert "/api/reclaim" in out, "the offending line itself must be shown: " + out
assert "git add -p" in out, "the remedy must be named: " + out
'

echo
echo "  $PASS passed, $FAIL failed"
[ "$FAIL" -eq 0 ]
