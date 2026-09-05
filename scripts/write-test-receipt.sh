#!/usr/bin/env bash
# Record which BYTES a test run compiled, so pre-commit can say whether a green
# result describes the commit in front of it.
#
# Usage: scripts/write-test-receipt.sh <exit-code> <the cargo args that ran...>
#
# WHY THIS IS ITS OWN FILE (AF-478). It used to live inline in
# `test-contended.sh`, which made the receipt a property of ONE wrapper rather
# than of running tests. CLAUDE.md names two sanctioned local paths and they
# disagreed: it says to run tests with `test-contended.sh`, and it also says to
# put any local cargo run through `safe-cargo.sh` for the systemd-scope
# isolation (AMUX-70). Only the first wrote a receipt, so following the safety
# instruction produced a commit whose hook reported the bytes as untested.
#
# Measured 2026-09-04 on AF-475: three green runs through `safe-cargo.sh`
# minutes before the commit, and pre-commit answered "1 of 1 staged crate
# file(s) DIFFER from the bytes your last run compiled (`-p amux-server --lib
# board_drive`, 72140s ago)". It was naming a run from twenty hours earlier
# because nothing that day had written a receipt. The hook was right about what
# it could see and wrong about the world, and no sequence of sanctioned commands
# could have made it right.
#
# THE RECEIPT ITSELF (AF-195). A green result describes the bytes cargo
# compiled, and the commit ships the bytes in the INDEX. On a shared checkout
# those come apart between the end of a run and the `git commit` that cites it:
# a peer stages a change to a file you tested, and your commit carries a version
# no test ever saw. c971756b shipped RED under a message asserting the opposite
# of its own diff, and both the passing run and the failing rerun were TRUE when
# taken.
#
# NOT a pre-commit test gate. The pre-commit hook's own comments record why it
# compiles tests but does not run them: ~40s on every commit across ~50 lanes,
# and a failing assertion blocks only its author. That trade is measured and it
# stands. This costs nothing at commit time; the hook compares two blob shas it
# already has.
#
# WHAT IS RECORDED: the blob sha of every tracked file under crates/ as cargo
# saw it. Clean files come from HEAD in one `ls-tree` call; only the dirty ones
# are hashed, so this is a handful of hashes rather than several hundred.
#
# Never fails its caller. A receipt that could break a test run would be traded
# away the first time it did, and an absent receipt is a state pre-commit
# already reports honestly.

rc="${1:-0}"
shift 2>/dev/null || true

{
  receipt_dir="${AMUX_HOME:-$HOME/.amux}/test-receipts"
  mkdir -p "$receipt_dir" 2>/dev/null || true
  receipt="$receipt_dir/${AMUX_SESSION:-unknown}.tsv"
  {
    # Header carries what the body cannot: which run this was and how it ended.
    # A receipt with no verdict would let a RED run vouch for a commit.
    echo "# repo	$(git rev-parse --show-toplevel 2>/dev/null)"
    echo "# head	$(git rev-parse HEAD 2>/dev/null)"
    echo "# rc	$rc"
    echo "# at	$(date -u +%s)"
    echo "# args	$*"
    # HEAD's blobs for every tracked file under crates/, then override with the
    # worktree hash of each dirty one. Order matters: the second write wins.
    git ls-tree -r HEAD --format='%(objectname)	%(path)' -- crates 2>/dev/null
    # UNTRACKED FILES TOO. cargo compiles a new .rs the moment it exists, and
    # `git diff` cannot see it, so omitting them made a brand-new module report
    # "not in the tested set at all" on the very commit that adds it — a false
    # alarm on the most ordinary case there is.
    for f in $(git diff --name-only -- crates 2>/dev/null
               git diff --cached --name-only -- crates 2>/dev/null
               git ls-files --others --exclude-standard -- crates 2>/dev/null); do
      [ -f "$f" ] || continue
      printf '%s\t%s\n' "$(git hash-object "$f" 2>/dev/null)" "$f"
    done
  } > "$receipt.tmp" 2>/dev/null && mv "$receipt.tmp" "$receipt" 2>/dev/null
} || true

exit 0
