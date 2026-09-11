#!/usr/bin/env bash
# Cells for the co-edit warning's mtime CORROBORATION (AF-391).
#
# WHY THIS EXISTS. The warning says a peer may have uncommitted work in a file
# you are about to commit whole. It is mtime-derived and says so, and on a
# checkout ~125 lanes share that makes the BUSIEST lane the default suspect for
# any file whose mtime moves. mixpeek-general was named as the editor of a file
# they had never opened; clearing it cost them and mixpeek-cicd a verification
# each. Their first check was the one wired here: worktree bytes already
# committed, so there is no uncommitted content to be in dispute.
#
# THE PROPERTY UNDER TEST IS THE ASYMMETRY, not the quiet. Downgrade only when
# the claim is mtime-derived AND nothing is in dispute; a transcript-backed
# co-edit, or a file with real uncommitted content, must still print in full.
# A test that only checked "it got quieter" would pass a guard that had been
# hollowed out, which is the failure this whole file family exists to prevent.
#
# Runs the SHIPPED loop body against a real git fixture, not a paraphrase.
set -euo pipefail
cd "$(dirname "$0")/.."
HOOK="${STAGED_GUARD_HOOK:-$(pwd)/scripts/git-hooks/amux-staged-guard}"
PASS=0; FAIL=0
ok()  { PASS=$((PASS+1)); echo "  ok   $1"; }
bad() { FAIL=$((FAIL+1)); echo "  FAIL $1"; echo "       got: $2"; }

TMP="$(mktemp -d)" || exit 1
trap 'rm -rf "$TMP"' EXIT
export GIT_AUTHOR_NAME=t GIT_AUTHOR_EMAIL=t@t GIT_COMMITTER_NAME=t GIT_COMMITTER_EMAIL=t@t
export GIT_CONFIG_COUNT=1 GIT_CONFIG_KEY_0=init.defaultBranch GIT_CONFIG_VALUE_0=main

# AMUX-3954: same path `run_case` points MIRROR_NOTICE_LOG at. Cleared up
# front so the LAST line after any one run_case call is that call's own row,
# with nothing to clean up between cells.
LEDGER="${TMPDIR:-/tmp}/test-mirror-notices.jsonl"
rm -f "$LEDGER"

echo "== staged-guard co-edit corroboration cells =="

# A repo with two files: one whose worktree bytes are committed (nothing in
# dispute) and one carrying real uncommitted content.
git init -q "$TMP/r"
( cd "$TMP/r"
  printf 'committed\n' > settled.txt
  printf 'committed\n' > dirty.txt
  git add settled.txt dirty.txt; git commit -qm base
  printf 'uncommitted edit\n' >> dirty.txt ) >/dev/null 2>&1

run_case() { # $1 = python dict for one shared-file entry
  python3 - "$HOOK" "$TMP/r" "$1" <<'PY' 2>&1
import io, json, os, subprocess, sys, time, ast
src = open(sys.argv[1]).read()
os.chdir(sys.argv[2])
# Take the SHIPPED helper and the SHIPPED loop body, never a retyped copy.
# Start at the FIRST helper the loop body calls, not the second: cell (b)
# failed with NameError when _never_wrote was added above it, which is the
# harness telling the truth about an incomplete extraction.
h0 = src.index("def _never_wrote(path, session):")
h1 = src.index("\n\n", src.index("        return None, \"the check errored\""))
helper = src[h0:h1]
# AMUX-3954: same rule applies to the ledger call the loop body now makes.
# Extracted rather than stubbed, so a change to its signature breaks THIS test
# instead of silently exec'ing a paraphrase.
n0 = src.index("def _record_mirror_notice(")
n1 = src.index("\ndef _derive_session_from_tmux")
notice_helper = src[n0:n1]
b0 = src.index('    for f in (d.get("shared") or []):')
b1 = src.index("    # PRE-COMMIT MISATTRIBUTES A PEER'S WRITE TO AN INNOCENT HOOK")
body = "\n".join(l[4:] if l.startswith("    ") else l for l in src[b0:b1].splitlines())
buf = io.StringIO()
g = {
    "subprocess": subprocess, "json": json, "time": time, "w": buf.write,
    "d": {"shared": [ast.literal_eval(sys.argv[3])]},
    # `sess` is a `main()` local the loop body closes over; the extraction
    # runs at module scope, so it needs a stand-in like the others below.
    "sess": "test-session",
    # Real names the extracted call site reads as globals, kept out of the
    # asserted output by pointing the ledger at a path this test owns.
    "MIRROR_NOTICE_LOG": os.path.join(os.environ.get("TMPDIR", "/tmp"), "test-mirror-notices.jsonl"),
    "GUARD_VERSION": 0,
}
exec(helper, g)
exec(notice_helper, g)
exec(body, g)
sys.stdout.write(buf.getvalue())
PY
}

# (a) THE REPORTED CASE: mtime-derived claim on a file with nothing in dispute.
out=$(run_case "{'path':'settled.txt','owner':'peer','peer':True,'age_secs':600,'has_unstaged_changes':False,'mine_provenance':'observed','their_provenance':'transcript'}")
if printf '%s' "$out" | grep -q "UNCORROBORATED co-edit claim"; then ok "(a) an uncorroborated mtime claim on settled content is downgraded"
else bad "(a) expected the downgrade line" "$out"; fi
if printf '%s' "$out" | grep -q "settled.txt"; then ok "(a) the file is still NAMED, so nothing actionable disappears"
else bad "(a) the downgraded line must still name the file" "$out"; fi
if printf '%s' "$out" | grep -q "git apply --cached"; then bad "(a) the eight-line remedy must not print for a dispute that does not exist" "$out"
else ok "(a) the remedy block is gone, which is the cost being removed"; fi
# AMUX-3954: the downgrade is LOGGED, not just printed — that is the whole
# point of the ledger (measuring how often a fired claim would have been
# right requires keeping the downgraded ones too).
row=$(tail -1 "$LEDGER" 2>/dev/null || true)
if printf '%s' "$row" | python3 -c "import json,sys; d=json.load(sys.stdin); assert d['disposition']=='downgraded' and d['path']=='settled.txt' and d['owner']=='peer'" 2>/dev/null
then ok "(a) the downgrade is recorded in the mirror-notice ledger"
else bad "(a) expected a downgraded ledger row for settled.txt/peer" "$row"; fi

# (b) CONTROL, the one that stops this being a hollowing-out: REAL uncommitted
#     content still prints the full warning even though the claim is mtime-derived.
out=$(run_case "{'path':'dirty.txt','owner':'peer','peer':True,'age_secs':600,'has_unstaged_changes':True,'mine_provenance':'observed','their_provenance':'transcript'}")
if printf '%s' "$out" | grep -q "git apply --cached"; then ok "(b) a file with real uncommitted content still gets the full warning"
else bad "(b) the full warning must survive when content IS in dispute" "$out"; fi
if printf '%s' "$out" | grep -q "UNCORROBORATED co-edit claim"; then bad "(b) must not downgrade a file that has uncommitted content" "$out"
else ok "(b) no downgrade when there is something to dispute"; fi

# (c) CONTROL: a transcript-backed claim on BOTH sides is not the weak claim and
#     must never be downgraded, even on settled content.
out=$(run_case "{'path':'settled.txt','owner':'peer','peer':True,'age_secs':600,'has_unstaged_changes':False,'mine_provenance':'transcript','their_provenance':'transcript'}")
if printf '%s' "$out" | grep -q "UNCORROBORATED co-edit claim"; then bad "(c) a two-sided transcript claim is not mtime-derived and must stand" "$out"
else ok "(c) only an mtime-derived claim is eligible for downgrade"; fi
# AMUX-3954: a claim that FIRED (named a peer in full) is logged too, with the
# opposite disposition from (a) — the ledger has to carry both to be useful.
row=$(tail -1 "$LEDGER" 2>/dev/null || true)
if printf '%s' "$row" | python3 -c "import json,sys; d=json.load(sys.stdin); assert d['disposition']=='fired' and d['path']=='settled.txt' and d['owner']=='peer'" 2>/dev/null
then ok "(c) a fired claim is recorded in the mirror-notice ledger too"
else bad "(c) expected a fired ledger row for settled.txt/peer" "$row"; fi

# (d) An unknown path: the check cannot run, and that must not read as settled.
out=$(run_case "{'path':'no-such-file.txt','owner':'peer','peer':True,'age_secs':600,'has_unstaged_changes':True,'mine_provenance':'observed','their_provenance':'transcript'}")
if printf '%s' "$out" | grep -q "UNCORROBORATED co-edit claim"; then bad "(d) an unrunnable check must not be reported as 'nothing in dispute'" "$out"
else ok "(d) a check that could not run leaves the warning standing"; fi

# (e) NO PEER NAMED: this is YOUR OWN dirty file, not a co-edit claim. Must
# print the solo form and must NOT add a mirror-notice row — there is no
# co-editor to have a claim about. Distinguishes the ledger from "log every
# shared-loop iteration", which would make it noise instead of a claim record.
before=$(tail -1 "$LEDGER" 2>/dev/null || true)
out=$(run_case "{'path':'settled.txt','owner':None,'peer':False,'age_secs':600,'has_unstaged_changes':False,'mine_provenance':'transcript'}")
if printf '%s' "$out" | grep -q "is yours and has uncommitted changes"; then ok "(e) a solo dirty file prints the no-peer form"
else bad "(e) expected the solo form with no peer named" "$out"; fi
after=$(tail -1 "$LEDGER" 2>/dev/null || true)
if [ "$before" = "$after" ]; then ok "(e) a solo entry with no peer adds nothing to the mirror-notice ledger"
else bad "(e) a claim naming no co-editor must not become a ledger row" "$after"; fi

# -- (t) READER vs WRITER: trailer history settles it (MC-1561) ---------------
#
# The complement to (a). AF-391 asks whether anything is in DISPUTE; this asks
# whether the named session is a WRITER of the path at all. A hot shared file has
# uncommitted content, so (a) correctly declines to downgrade it, while the lane
# being named has never written a line. mixpeek-cicd was named three times in one
# session for files it had never opened, because it had PARSED one of them dozens
# of times in a shared cwd. Measured there: 9 commits that day, 9 with trailers,
# 0 naming them.
#
# THE `None` ARM IS THE LOAD-BEARING ONE. History with no trailers at all must not
# read as "they never wrote it", or every repo that does not use trailers would
# silently suppress every notice.
git init -q "$TMP/w"
( cd "$TMP/w"
  git config user.email t@t; git config user.name t
  printf 'a\n' > hot.txt;   git add hot.txt;   git commit -qm "one

Amux-Session: writer-lane"
  printf 'b\n' >> hot.txt;  git add hot.txt;   git commit -qm "two

Amux-Session: writer-lane"
  printf 'x\n' > plain.txt; git add plain.txt; git commit -qm "no trailer here"
  printf 'dirty\n' >> hot.txt ) >/dev/null 2>&1

if python3 - "$HOOK" "$TMP/w" <<'NWPY'
import importlib.util, os, subprocess, sys
src = open(sys.argv[1]).read()
os.chdir(sys.argv[2])
h0 = src.index("def _never_wrote(path, session):")
h1 = src.index("def _nothing_in_dispute(path):")
g = {"subprocess": subprocess}
exec(src[h0:h1], g)
nw = g["_never_wrote"]

assert nw("hot.txt", "reader-lane") is True,  "a lane with no trailer on the path must read as never-wrote"
assert nw("hot.txt", "writer-lane") is False, "a lane WITH a trailer on the path must not be downgraded"
assert nw("plain.txt", "reader-lane") is None, "history with no trailers cannot answer, and must not say True"
assert nw("no-such-path.txt", "reader-lane") is None, "no history cannot answer"
assert nw("hot.txt", "") is None and nw("", "reader-lane") is None, "missing inputs cannot answer"
NWPY
then ok "(t) trailer history separates reader from writer, and says None when it cannot"
else bad "(t) the writer check either misread a writer, or claimed an answer it could not have" ""
fi

echo
echo "  $PASS passed, $FAIL failed"
[ "$FAIL" -eq 0 ]
