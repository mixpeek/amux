#!/usr/bin/env bash
# Cells for the commit-message stamp (AMUX-3916).
#
# WHY THIS EXISTS. `Amux-Session` is read from $AMUX_SESSION, which is an
# ENVIRONMENT VARIABLE and therefore travels to every child process. Any process
# that inherits it — a subagent, a script, a session that wandered into this
# checkout — writes commits indistinguishable from that lane's. `Amux-Conversation`
# is a LOOKUP of that same variable, so it cannot corroborate it: a wrong stamp
# produces a wrong conversation id identically and the pair reads as doubly
# confirmed. Measured on 2026-08-30: four commits stamped to a lane that did not
# make them, and two agents citing the two fields to each other as agreeing
# sources.
#
# `Amux-Agent` is walked from the hook's own PROCESS ANCESTRY, which no env var
# can move. The property below is the whole point and it is stated as an
# invariance: change $AMUX_SESSION to anything you like and the agent field does
# not move.
#
# Runs the SHIPPED hook, not a retyped copy.
set -uo pipefail
cd "$(dirname "$0")/.."
HOOK="${COMMIT_STAMP_HOOK:-$(pwd)/scripts/git-hooks/prepare-commit-msg}"
PASS=0; FAIL=0
ok(){ PASS=$((PASS+1)); printf '  ok   %s\n' "$1"; }
no(){ FAIL=$((FAIL+1)); printf '  FAIL %s\n     %s\n' "$1" "${2:-}"; }

TMP="$(mktemp -d)"; trap 'rm -rf "$TMP"' EXIT
mkdir -p "$TMP/home/sessions"

# A CONTROLLED ANCESTOR, because the ambient one is not a test input.
#
# Cells 1 and 2 used to run the hook directly and read whatever `claude` process
# happened to be above the test. On a dev box that is the session running it, so
# they passed; in CI there is no claude anywhere in the tree, `Amux-Agent` is
# correctly omitted, and both cells failed on an empty string. They had never
# been green in CI: the first CI run that reached them (33396997200, 2026-08-31)
# went red on exactly these two, and stayed red for the fleet until this fix. A
# cell whose verdict depends on who launched the test measures the launcher, not
# the hook.
#
# A symlink named `claude` gives ps an argv[0] whose basename matches the hook's
# binary test, which is the same "match the binary, not the substring" rule the
# hook documents. A copied binary does not work on macOS (code signing kills
# it), which is why this is a symlink and not a cp.
mkdir -p "$TMP/bin"
ln -sf /bin/sh "$TMP/bin/claude"

run(){ # run <session> -> the trailer block, with NO claude ancestor
  printf 'subject\n' > "$TMP/msg"
  AMUX_SESSION="$1" AMUX_HOME="$TMP/home" bash "$HOOK" "$TMP/msg" >/dev/null 2>&1
  grep '^Amux-' "$TMP/msg" 2>/dev/null
}

# ONE shim, TWO claimed lanes. Both hook runs share a single claude ancestor, so
# the pid the trailer names is the same process in both. That is what makes the
# invariance below a comparison rather than a coincidence: two separate shims
# would have two pids and the cell could never pass, whatever the hook did.
cat > "$TMP/ancestry.sh" <<EOS
printf 'subject\n' > "$TMP/msg_a"
AMUX_SESSION=lane-alpha AMUX_HOME="$TMP/home" bash "$HOOK" "$TMP/msg_a" >/dev/null 2>&1
printf 'subject\n' > "$TMP/msg_b"
AMUX_SESSION=lane-beta  AMUX_HOME="$TMP/home" bash "$HOOK" "$TMP/msg_b" >/dev/null 2>&1
printf '%s' "\$\$" > "$TMP/shim.pid"
EOS
"$TMP/bin/claude" "$TMP/ancestry.sh"
shim="$(cat "$TMP/shim.pid" 2>/dev/null)"

echo "commit-stamp cells (AMUX-3916)"

a="$(grep  '^Amux-Agent:'   "$TMP/msg_a" 2>/dev/null)"
b="$(grep  '^Amux-Agent:'   "$TMP/msg_b" 2>/dev/null)"
sa="$(grep '^Amux-Session:' "$TMP/msg_a" 2>/dev/null)"
sb="$(grep '^Amux-Session:' "$TMP/msg_b" 2>/dev/null)"

# 1. THE INVARIANCE. Two different claimed lanes, one real committing process.
[ "$sa" != "$sb" ] \
  && ok "Amux-Session follows \$AMUX_SESSION (it is the claim)" \
  || no "Amux-Session invariant" "both runs said '$sa'; the test is not exercising the spoof"
if [ -n "$a" ] && [ "$a" = "$b" ]; then
  ok "Amux-Agent does NOT move with \$AMUX_SESSION ($a)"
else
  no "Amux-Agent must be invariant under \$AMUX_SESSION" "alpha='$a' beta='$b'"
fi

# 2. IT NAMES THE REAL ANCESTOR, not a placeholder and not the hook itself. A
#    field that is merely PRESENT looks identical to one that discriminates,
#    which is the failure this whole card is about. `ps -p <pid>` liveness was
#    the old proxy for that, and it cannot tell the RIGHT process from any live
#    one: the hook's own pid would have satisfied it. The shim's pid is known
#    here, so assert equality with it instead.
pid="$(printf '%s' "$a" | sed -n 's/.*pid=\([0-9]\{1,\}\).*/\1/p')"
if [ -n "$pid" ] && [ -n "$shim" ] && [ "$pid" = "$shim" ]; then
  ok "Amux-Agent pid=$pid is the claude ancestor, not the hook's own process"
else
  no "Amux-Agent must name the claude ancestor" "trailer='$a' shim pid='$shim'"
fi

# 3. REGRESSION: A PATH IS NOT A PROGRAM. The first draft matched `*claude*`
#    against the whole command line and picked up a shell whose cwd was
#    /private/tmp/claude-501/..., reporting that shell as the agent with
#    model=unspecified. Match the first token's basename.
if grep -q 'case "${_exe##\*/}" in' "$HOOK" || grep -q '_exe##' "$HOOK"; then
  ok "matches the executable's basename, not a substring of the command line"
else
  no "the agent walk must not glob \*claude\* over the whole command line" \
     "a cwd containing 'claude' would be reported as the agent"
fi

# 4. A CONVERSATION ID THE LANE HAS NOT CONFIRMED IS OMITTED (AMUX-3897).
#    An absent field reads as unknown; a wrong one reads as fact.
printf '{"cc_conversation_id":"11111111-2222-4333-8444-555555555555"}' \
  > "$TMP/home/sessions/unconfirmed.meta.json"
if run unconfirmed | grep -q '^Amux-Conversation:'; then
  no "an unconfirmed conv id must not be stamped" "$(run unconfirmed)"
else
  ok "unconfirmed conversation id is omitted, not guessed"
fi
#    CONTROL: a freshly confirmed one IS stamped, or cell 4 passes by the field
#    never being emitted at all.
python3 - "$TMP/home/sessions/confirmed.meta.json" <<'PY'
import json,sys,time
json.dump({"cc_conversation_id":"11111111-2222-4333-8444-555555555555",
           "cc_conversation_confirmed_at":int(time.time())}, open(sys.argv[1],"w"))
PY
if run confirmed | grep -q '^Amux-Conversation:'; then
  ok "a freshly confirmed conversation id IS stamped (cell 4 is not vacuous)"
else
  no "a confirmed conv id must still be stamped" "$(run confirmed)"
fi

# 5. AMUX-3939: the MODEL fallback must say what was measured.
#
# `unspecified` read as "the field was not populated", i.e. the measurement did
# not run. The measurement DID run: the walk found the right process, and that
# process was launched as a bare `claude` with no `--model`. ts-gke's specimen
# was 9fd67b10, stamped model=unspecified while its own Co-Authored-By said
# Sonnet 4.6; its agent pid 5559 had the single-word command line `claude`,
# while every other live claude process on the box carried --model.
#
# Driving this needs a CONTROLLED ANCESTRY, since the walk reads real parents:
# the `claude` shim built at the top of this file, whose rationale is there.
run_under() { # run_under <extra claude argv...> -> the Amux-Agent trailer
  printf 'subject\n' > "$TMP/msg2"
  AMUX_SESSION=lane-model AMUX_HOME="$TMP/home" \
    "$TMP/bin/claude" -c "bash '$HOOK' '$TMP/msg2' >/dev/null 2>&1" "$@"
  grep '^Amux-Agent:' "$TMP/msg2" 2>/dev/null
}

m_absent="$(run_under)"
case "$m_absent" in
  *"model=argv-absent"*)
    ok "a bare \`claude\` reports model=argv-absent (measured, not missing)" ;;
  *"model=unspecified"*)
    no "the fallback must not say 'unspecified'" \
       "that word means the probe did not run; it did — got '$m_absent'" ;;
  *) no "a claude-named ancestor must produce an Amux-Agent trailer" "got '$m_absent'" ;;
esac

# CONTROL, and the card named it: "an explicitly-launched lane still reports its
# real model, or the fix has just replaced one empty value with another."
m_real="$(run_under --model claude-opus-5)"
case "$m_real" in
  *"model=claude-opus-5"*)
    ok "an explicit --model is still read off argv (the fix is not a blanket string)" ;;
  *) no "a launched-with--model lane must report its real model" "got '$m_real'" ;;
esac

# CONTROL 2: the invariance this field exists for is untouched. The model now
# has a second possible value, and a fix that reached for the transcript to get
# a nicer string would have coupled this field to \$AMUX_SESSION — which is
# exactly what cell 1 forbids. Asserted again HERE, against the fallback path
# specifically, because cell 1 runs under an ancestry that has a real --model
# and so never exercises this branch.
# Compare the MODEL field only: each run_under spawns its own shim, so the pids
# differ by construction and comparing whole trailers would fail for a reason
# that is not the property under test. (It did, on the first draft of this cell.)
_model_of() { printf '%s' "$1" | sed -n 's/.*model=\([^ ]*\).*/\1/p'; }
x="$(_model_of "$(AMUX_SESSION=aaa run_under)")"
y="$(_model_of "$(AMUX_SESSION=bbb run_under)")"
if [ -n "$x" ] && [ "$x" = "$y" ]; then
  ok "the fallback path is ALSO invariant under \$AMUX_SESSION (model=$x)"
else
  no "model=argv-absent must not move with \$AMUX_SESSION" "aaa='$x' bbb='$y'"
fi

# A DECLARED TRAILER THAT DISAGREES WITH THE LANE (AF-479).
#
# `--if-exists doNothing` means a value already in the message wins over the one
# the hook knows is true. Correct for a cherry-pick, and it also let a hand-typed
# `Amux-Session: amux` land on ac550324 from a lane called amux-frustrations,
# silently. These cells pin all three arms: disagreement is SAID and recorded,
# agreement is silent, and a message with no declaration is untouched.
declare_run(){ # declare_run <declared-or-empty> <session> ; sets $D_MSG and $D_ERR
  if [ -n "$1" ]; then
    printf 'subject\n\nbody\n\nAmux-Session: %s\n' "$1" > "$TMP/dmsg"
  else
    printf 'subject\n\nbody\n' > "$TMP/dmsg"
  fi
  AMUX_SESSION="$2" AMUX_HOME="$TMP/home" bash "$HOOK" "$TMP/dmsg" 2>"$TMP/derr" >/dev/null
  D_MSG="$(grep '^Amux-' "$TMP/dmsg" 2>/dev/null)"
  D_ERR="$(cat "$TMP/derr" 2>/dev/null)"
}

declare_run peer-lane my-lane
case "$D_MSG" in
  *"Amux-Session: peer-lane"*) ok "a declared session survives (a cherry-picked author is real provenance)" ;;
  *) no "the declared trailer must be KEPT, never overwritten" "got '$D_MSG'" ;;
esac
case "$D_MSG" in
  *"Amux-Committer: my-lane"*) ok "the committing lane is recorded beside it" ;;
  *) no "a disagreeing stamp must add Amux-Committer" "got '$D_MSG'" ;;
esac
case "$D_ERR" in
  *"peer-lane"*"my-lane"*) ok "the disagreement is SAID at commit time, naming both lanes" ;;
  *) no "the mismatch must warn on stderr, naming both values" "got '$D_ERR'" ;;
esac

# NEGATIVE CONTROL, the one that decides whether the cells above test anything.
# If Amux-Committer were stamped unconditionally, all three would still pass and
# the field would carry no signal at all: its PRESENCE is what a reader and the
# push guard key on.
declare_run my-lane my-lane
case "$D_MSG" in
  *"Amux-Committer"*) no "an AGREEING stamp must not add Amux-Committer" "got '$D_MSG'" ;;
  *) ok "no Amux-Committer when the declared stamp agrees (presence is the signal)" ;;
esac
if [ -z "$D_ERR" ]; then
  ok "an agreeing stamp is silent"
else
  no "an agreeing stamp must not warn" "got '$D_ERR'"
fi

# NEGATIVE CONTROL 2: the ordinary path, which is every commit on this box.
declare_run "" my-lane
case "$D_MSG" in
  *"Amux-Committer"*) no "an undeclared message must not grow Amux-Committer" "got '$D_MSG'" ;;
  *"Amux-Session: my-lane"*) ok "an undeclared message is stamped exactly as before" ;;
  *) no "the ordinary stamp path must be unchanged" "got '$D_MSG'" ;;
esac

printf '\n%d passed, %d failed\n' "$PASS" "$FAIL"
[ "$FAIL" -eq 0 ]
