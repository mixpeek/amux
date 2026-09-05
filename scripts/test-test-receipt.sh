#!/usr/bin/env bash
# Cells for the AF-195 test receipt: does the pre-commit report actually
# discriminate, or does it print a reassuring line no matter what?
#
# Every cell drives the REAL hook block, extracted by line range from the
# shipped file rather than paraphrased, because a check pinning a copy of the
# logic is exactly as green as one pinning the wrong layer (ethos rule 7).
set -u
ROOT_REPO="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
HOOK="$ROOT_REPO/scripts/git-hooks/pre-commit"
TMP=$(mktemp -d); trap 'rm -rf "$TMP"' EXIT
pass=0; fail=0
ok() { if [ "$2" = "$3" ]; then pass=$((pass+1)); echo "  ok   $1"; else
       fail=$((fail+1)); echo "  FAIL $1: want [$3] got [$2]"; fi; }

# The shipped block, from its banner to EOF. Extracted, never retyped.
START=$(grep -n 'DOES YOUR GREEN RESULT DESCRIBE THIS COMMIT' "$HOOK" | cut -d: -f1)
[ -n "$START" ] || { echo "FATAL: the receipt block is gone from $HOOK"; exit 1; }
sed -n "${START},\$p" "$HOOK" > "$TMP/block.sh"

run() {  # run(receipt_file, staged_paths) -> stdout
  # A FRESH home per cell. Sharing one let cell c's receipt survive into cell
  # d's "no receipt" case, which passed for the wrong reason on the first run.
  ( export AMUX_HOME="$TMP/home.$RANDOM$RANDOM" AMUX_SESSION=cell
    mkdir -p "$AMUX_HOME/test-receipts"
    [ -n "$1" ] && cp "$1" "$AMUX_HOME/test-receipts/cell.tsv"
    cd "$ROOT_REPO" || exit
    ROOT="$ROOT_REPO"; STAGED="$2"
    # shellcheck disable=SC1090
    . "$TMP/block.sh" ) 2>&1
}

# A real staged path and its real index sha, so the comparison is against git.
REALP=$(git -C "$ROOT_REPO" ls-files 'crates/*.rs' | head -1)
REALSHA=$(git -C "$ROOT_REPO" ls-files -s -- "$REALP" | awk '{print $2}')
mkr() {  # mkr(rc, sha) -> receipt path
  f="$TMP/r$RANDOM.tsv"
  { printf '# repo\t%s\n' "$ROOT_REPO"
    printf '# head\tdeadbeef\n'
    printf '# rc\t%s\n' "$1"
    printf '# at\t%s\n' "$(date -u +%s)"
    printf '# args\t-p amux-server --lib\n'
    printf '%s\t%s\n' "$2" "$REALP"; } > "$f"
  echo "$f"
}

echo "cell a: staged bytes match the tested bytes -> says so"
o=$(run "$(mkr 0 "$REALSHA")" "$REALP")
ok "reports a match" "$(echo "$o" | grep -c 'match the bytes')" "1"
ok "does not cry change" "$(echo "$o" | grep -c 'DIFFER')" "0"

echo "cell b: THE POSITIVE CONTROL — staged bytes moved since the run"
o=$(run "$(mkr 0 0000000000000000000000000000000000000000)" "$REALP")
ok "reports the drift" "$(echo "$o" | grep -c 'DIFFER')" "1"
ok "names the file" "$(echo "$o" | grep -c "$REALP")" "1"
ok "withholds the reassurance" "$(echo "$o" | grep -c 'match the bytes')" "0"

echo "cell c: the last run was RED — it must not vouch for anything"
o=$(run "$(mkr 101 "$REALSHA")" "$REALP")
ok "says the run failed" "$(echo "$o" | grep -c 'EXITED 101')" "1"
ok "no green claim on a red run" "$(echo "$o" | grep -c 'match the bytes')" "0"

echo "cell d: no receipt at all — silence must not read as coverage"
o=$(run "" "$REALP")
ok "says there is none" "$(echo "$o" | grep -c 'none for session')" "1"

echo "cell e: a receipt from another checkout claims nothing about this one"
f=$(mkr 0 "$REALSHA")
grep -v '^# repo' "$f" > "$f.x"; { printf '# repo\t/somewhere/else\n'; cat "$f.x"; } > "$f"
o=$(run "$f" "$REALP")
ok "names the other checkout" "$(echo "$o" | grep -c 'DIFFERENT checkout')" "1"

echo "cell f: no staged crate files — the block stays silent"
o=$(run "$(mkr 0 "$REALSHA")" "README.md")
ok "silent on a non-crate commit" "$(echo "$o" | grep -c 'test receipt')" "0"

echo "cell g: a staged file the run never saw is called out, not passed"
o=$(run "$(mkr 0 "$REALSHA")" "$REALP
crates/amux-server/src/never_tested_xyz.rs")
ok "flags the unseen path" "$(echo "$o" | grep -c 'not in the tested set')" "1"

echo "cell h: a NEW untracked crate file is recorded by the receipt writer"
# Drives the SHIPPED writer, which is now its own script (AF-478) rather than a
# block inside test-contended.sh. Invoking the real file beats extracting a line
# range: the range trick existed only because the code was not callable.
WRITER="$ROOT_REPO/scripts/write-test-receipt.sh"
[ -x "$WRITER" ] || { echo "FATAL: the receipt writer is gone or not executable"; exit 1; }
NEWREL="crates/amux-core/src/__receipt_cell_$$.rs"
echo "// throwaway" > "$ROOT_REPO/$NEWREL"
WHOME="$TMP/whome"
# rc=101, not 0. With 0 a writer that HARDCODES "# rc 0" is indistinguishable
# from one that reads its argument, and the exit-code assertion below could not
# have failed. Caught by mutating the writer.
( cd "$ROOT_REPO" && AMUX_HOME="$WHOME" AMUX_SESSION=wcell "$WRITER" 101 -p amux-server ) >/dev/null 2>&1
rm -f "$ROOT_REPO/$NEWREL"
ok "the new file is in the receipt" \
   "$(grep -c "$NEWREL" "$WHOME/test-receipts/wcell.tsv" 2>/dev/null || echo 0)" "1"
ok "and a long-tracked file is too" \
   "$(grep -c "	$REALP\$" "$WHOME/test-receipts/wcell.tsv" 2>/dev/null || echo 0)" "1"
ok "the receipt carries the run's REAL exit code" \
   "$(grep -c '^# rc	101$' "$WHOME/test-receipts/wcell.tsv" 2>/dev/null || echo 0)" "1"

echo "cell i: a path listed TWICE resolves to the LAST row, not the first"
# The writer emits HEAD's blob for every tracked file, then overrides the dirty
# ones. Reading the first row takes HEAD's blob and reports every file you
# edited-and-tested as DIFFERS. That is not hypothetical: it is what the block
# said on its first real commit, 27 seconds after a green run with no edit in
# between.
f="$TMP/dup.tsv"
{ printf '# repo\t%s\n' "$ROOT_REPO"
  printf '# head\tdeadbeef\n# rc\t0\n'
  printf '# at\t%s\n' "$(date -u +%s)"
  printf '# args\t-p amux-server\n'
  printf '%s\t%s\n' 0000000000000000000000000000000000000000 "$REALP"   # HEAD row
  printf '%s\t%s\n' "$REALSHA" "$REALP"; } > "$f"                        # worktree row
o=$(run "$f" "$REALP")
ok "the override row wins" "$(echo "$o" | grep -c 'match the bytes')" "1"
ok "no false drift report" "$(echo "$o" | grep -c 'DIFFER')" "0"

# ---------------------------------------------------------------------------
# AF-478: the receipt is a property of RUNNING TESTS, not of one wrapper.
#
# CLAUDE.md names two sanctioned local paths and they were in conflict: run
# tests with `test-contended.sh`, and put any local cargo run through
# `safe-cargo.sh` for the systemd-scope isolation AMUX-70 exists for. Only the
# first wrote a receipt. Measured 2026-09-04: three green runs through
# safe-cargo.sh minutes before a commit, and pre-commit answered "1 of 1 staged
# crate file(s) DIFFER ... (`-p amux-server --lib board_drive`, 72140s ago)" —
# naming a run from twenty hours earlier, because nothing that day had written
# one. No sequence of sanctioned commands could have made the hook right.
#
# The cells below drive the real scripts. `cargo` and `systemd-run` are stubbed
# so a cell costs milliseconds and so the SAME cells run on a systemd host and
# on one without: the shipped script picks its branch from /run/systemd/system,
# and stubbing only cargo would leave the Linux branch untested.
mkdir -p "$TMP/bin"
cat > "$TMP/bin/cargo" <<'STUB'
#!/bin/sh
exit ${FAKE_CARGO_RC:-0}
STUB
cat > "$TMP/bin/systemd-run" <<'STUB'
#!/bin/sh
# Drop this wrapper's own flags, run whatever followed `--`.
while [ $# -gt 0 ]; do [ "$1" = "--" ] && { shift; break; }; shift; done
exec "$@"
STUB
chmod +x "$TMP/bin/cargo" "$TMP/bin/systemd-run"

sc() {  # sc(home_tag, extra_env, args...) -> exit code of safe-cargo.sh
  tag=$1; shift
  ( cd "$ROOT_REPO" || exit
    export PATH="$TMP/bin:$PATH" AMUX_HOME="$TMP/h.$tag" AMUX_SESSION=sc
    mkdir -p "$AMUX_HOME/test-receipts"
    bash "$ROOT_REPO/scripts/safe-cargo.sh" "$@" ) >/dev/null 2>&1
  echo $?
}
rcpt() { echo "$TMP/h.$1/test-receipts/sc.tsv"; }

echo "cell j: safe-cargo.sh test writes a receipt — the whole point of AF-478"
sc j test -p amux-server --lib request_log >/dev/null
ok "a receipt exists after a test run" "$([ -f "$(rcpt j)" ] && echo 1 || echo 0)" "1"
ok "it records the args that ran" \
   "$(grep -c '^# args	test -p amux-server --lib request_log$' "$(rcpt j)" 2>/dev/null || echo 0)" "1"
ok "and a green run is recorded green" \
   "$(grep -c '^# rc	0$' "$(rcpt j)" 2>/dev/null || echo 0)" "1"

echo "cell k: THE NEGATIVE CONTROL — a non-test subcommand writes nothing"
# Without this, a writer that fired on every invocation would pass cell j. It
# also pins the exec path: `check`/`build`/`clippy` still exec, and the auto
# builder runs `build` through this script.
sc k check -p amux-server >/dev/null
ok "no receipt for a check run" "$([ -f "$(rcpt k)" ] && echo 1 || echo 0)" "0"

echo "cell l: test-contended.sh's guard suppresses the duplicate"
o=$( cd "$ROOT_REPO" || exit
     export PATH="$TMP/bin:$PATH" AMUX_HOME="$TMP/h.l" AMUX_SESSION=sc _TC_RECEIPT=1
     mkdir -p "$AMUX_HOME/test-receipts"
     bash "$ROOT_REPO/scripts/safe-cargo.sh" test -p amux-server >/dev/null 2>&1; echo done )
ok "no receipt when the caller writes its own" "$([ -f "$(rcpt l)" ] && echo 1 || echo 0)" "0"

echo "cell m: a RED run keeps its exit code through the non-exec path"
# The receipt forced this script to stop `exec`ing for `test`, and a wrapper
# that swallowed the status would turn every red suite green for its caller.
# The exit code and the recorded rc are separate claims; both are asserted.
code=$( cd "$ROOT_REPO" || exit
        export PATH="$TMP/bin:$PATH" AMUX_HOME="$TMP/h.m" AMUX_SESSION=sc FAKE_CARGO_RC=101
        mkdir -p "$AMUX_HOME/test-receipts"
        bash "$ROOT_REPO/scripts/safe-cargo.sh" test -p amux-server >/dev/null 2>&1; echo $? )
ok "the caller sees the failure" "$code" "101"
ok "and the receipt says the run was red" \
   "$(grep -c '^# rc	101$' "$(rcpt m)" 2>/dev/null || echo 0)" "1"

echo "cell n: test-contended.sh no longer calls bare cargo"
# The other half of the conflict: on a systemd host an OOM-killed cargo test in
# the pane's own scope takes the interactive session down with it, which is the
# hazard safe-cargo.sh exists for. The sanctioned TEST path was the unprotected
# one. Asserted against the shipped file because there is no cheap way to run it
# end to end here, and a wrong answer is loud: the line is there or it is not.
#
# NOTE FOR WHOEVER EDITS A LABEL HERE: no backticks inside these double-quoted
# strings. Bash command-substitutes them, and a label reading "no bare
# `cargo test`" ran a real cargo test and hung this suite until it was killed.
ok "it delegates the run to safe-cargo.sh" \
   "$(grep -cF '"$_safe" test "$@"' "$ROOT_REPO/scripts/test-contended.sh")" "1"
ok "and tells it not to double-write the receipt" \
   "$(grep -cF '_TC_RECEIPT=1' "$ROOT_REPO/scripts/test-contended.sh")" "1"
ok "no bare cargo test at top level" \
   "$(grep -c '^cargo test' "$ROOT_REPO/scripts/test-contended.sh")" "0"

echo ""
echo "test-test-receipt: $pass passed, $fail failed"
[ "$fail" = 0 ]
