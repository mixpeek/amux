#!/usr/bin/env bash
# AF-78 (follow-up to AF-74 / AMUX-3145): runtime smoke for the local-scope
# unbound-variable shape that shellcheck STRUCTURALLY cannot see.
#
# THE GAP, established by measurement. The AF-74 shellcheck gate (rust.yml) catches the
# classes shellcheck can express. SC2154, its unbound-var check, fires ONLY for a
# name assigned NOWHERE in the file. AMUX-3145 was the opposite: `AMUX_API` was USED
# in cmd_start's tmux inject but its `local` declaration there was missing, while the
# SAME name is `local`-assigned in five OTHER functions. shellcheck sees those
# assignments, does no per-function `local` scope analysis, and exits 0. Under the
# CLI's own `set -euo pipefail` that out-of-scope use is a hard "AMUX_API: unbound
# variable" at launch, and launch is the least-tested path precisely because a real
# one spawns a session.
#
# WHAT THIS DOES. It runs the REAL non-dry-run launch path far enough to execute the
# inject at `amux:650` (`-e "AMUX_URL=$AMUX_API"`), with AMUX_API UNSET, but with tmux
# STUBBED so nothing is spawned and the PRODUCTION fleet is never touched. Dry-run
# returns BEFORE line 650, which is exactly why dry-run could not catch AMUX-3145; the
# whole point of this smoke is to reach the inject. amux-cloud validated AMUX-3145 with
# this recipe BY HAND; this makes it a CI test.
#
# DISCRIMINATION (ethos rule 7). The self-check builds a deliberately-broken copy of
# the CLI with cmd_start's `local AMUX_API=...` declaration replaced by an unset.
# The CLI now initializes AMUX_API globally too, so deleting only the local line
# no longer creates the original unset-variable precondition. The mutant must
# remove that inherited value and FAIL at the real inject with "unbound variable". The
# unmodified CLI must PASS. A smoke that cannot fail on the broken copy is theatre, so
# the copy is verified to actually differ before it is trusted as a negative control.
#
# Follows scripts/test-cli-quoting.sh / test-amux-url.sh: an isolated CC_HOME plus a
# stub tmux on PATH, so a run can neither reach a real worker nor touch the real tmux
# server. No network, no fleet state, deterministic.
set -euo pipefail
cd "$(dirname "$0")/.." || exit 1
AMUX_BIN="${AMUX_BIN:-./amux}"   # override to run against a fixture
PASS=0; FAIL=0
ok()  { echo "  ok   $1"; PASS=$((PASS+1)); }
bad() { echo "  FAIL $1"; [ -n "${2:-}" ] && echo "       $2"; FAIL=$((FAIL+1)); }

# The declaration site where the mutant restores AMUX-3145's unset state. Scope the mutation to cmd_start:
# other commands legitimately resolve AMUX_API through cmd_url too, and adding one of
# those must not make this negative control impossible to construct. Deleting the
# declaration only inside cmd_start and unsetting its inherited value leaves the
# real inject unbound without touching another function's declaration.
SPECIMEN='local AMUX_API="${AMUX_API:-$(cmd_url)}"'

count_cmd_start_specimen() {
  awk -v needle="$SPECIMEN" '
    /^cmd_start\(\)[[:space:]]*\{/ { in_cmd_start=1 }
    in_cmd_start && index($0, needle) { count++ }
    in_cmd_start && /^}/ { in_cmd_start=0 }
    END { print count + 0 }
  ' "$1"
}

# ── Isolated fleet ───────────────────────────────────────────────────────────
# CC_HOME is overridable (amux:35), so the worker roster is a throwaway dir. A stub
# `tmux` on PATH answers has-session/new-session WITHOUT a real pane, so the launch
# path runs without spawning anything on the production socket, the hazard the card
# calls out by name.
WORK=$(mktemp -d)
trap 'rm -rf "$WORK"' EXIT
FAKE_HOME="$WORK/home"; CC="$WORK/home/.amux"
mkdir -p "$CC/sessions" "$WORK/workdir"
# One throwaway worker. CC_DIR must be absolute and exist (amux:507-511). CC_PROVIDER
# claude keeps the launch on the local tmux path. codex/ollama DELEGATE to the server
# (amux:513) and never reach the inject this test is about.
cat > "$CC/sessions/af78smoke.env" <<ENVEOF
CC_DIR="$WORK/workdir"
CC_PROVIDER="claude"
ENVEOF
# Stub tmux. has-session => "not running" (exit 1) so cmd_start proceeds past the
# already-running early return into the real launch; new-session/set-option succeed
# without spawning. NOTHING is forwarded to a real tmux server.
STUB="$WORK/stub"; mkdir -p "$STUB"
cat > "$STUB/tmux" <<'TMUXEOF'
#!/usr/bin/env bash
case "${1:-}" in
  has-session) exit 1 ;;
  *)           exit 0 ;;
esac
TMUXEOF
chmod +x "$STUB/tmux"

# Run `amux start af78smoke --detach` against $1 with AMUX_API UNSET and tmux stubbed.
# --detach returns right after the inject at amux:650 WITHOUT attaching. Output
# (stdout+stderr) lands in $WORK/out; the caller reads $? for the exit code. Run
# DIRECTLY (not in $(...)) so the exit code is the parent's, not a lost subshell's.
run_launch() {
  env -u AMUX_API -u AMUX_URL \
    PATH="$STUB:$PATH" HOME="$FAKE_HOME" CC_HOME="$CC" \
    AMUX_SESSION=af78smoke-test AMUX_WORKER=af78smoke-test \
    bash "$1" start af78smoke --detach >"$WORK/out" 2>&1
}

# ── 1. current CLI: the inject resolves AMUX_API; launch reaches "started" ─────
# The launch's exit status IS the assertion in both cells below (case 1 must
# succeed, case 2 must fail on the broken CLI). `if` keeps the status readable
# under `set -e`; `|| true` would erase the thing being tested (AF-562).
if run_launch "$AMUX_BIN"; then RC=0; else RC=$?; fi; OUT=$(cat "$WORK/out")
if [ "$RC" -eq 0 ] && printf '%s' "$OUT" | grep -q "started" \
   && ! printf '%s' "$OUT" | grep -q "unbound variable"; then
  ok "current CLI reaches the launch inject with AMUX_API unset (no unbound var)"
else
  bad "current CLI failed the launch smoke (rc=$RC)" "$OUT"
fi

# ── 2. self-check / negative control: the AMUX-3145 specimen must be caught ────
# Build the broken copy by replacing the declaration with an explicit unset,
# including any inherited global initialization. Do not use a
# file-wide grep deletion: cmd_open_browser now carries the same legitimate spelling.
BROKEN="$WORK/amux-broken"
awk -v needle="$SPECIMEN" '
  /^cmd_start\(\)[[:space:]]*\{/ { in_cmd_start=1 }
  in_cmd_start && index($0, needle) { print "  unset AMUX_API # AF-78 negative control: inherited initialization removed"; next }
  { print }
  in_cmd_start && /^}/ { in_cmd_start=0 }
' "$AMUX_BIN" > "$BROKEN"
# Prove the fixture is ACTUALLY broken before trusting its failure. A self-check that
# silently no-ops (the specimen line drifted, so nothing was deleted) is the exact
# theatre rule 7 warns about: build a broken fixture, verify it is broken.
n_orig=$(count_cmd_start_specimen "$AMUX_BIN")
n_brk=$(count_cmd_start_specimen "$BROKEN")
if [ "$n_orig" -eq 1 ] && [ "$n_brk" -eq 0 ]; then
  ok "broken fixture built: cmd_start AMUX_API forced unset (declarations: $n_orig -> $n_brk)"
else
  bad "could not build the broken fixture; has the specimen line drifted?" \
      "expected exactly 1 cmd_start occurrence in $AMUX_BIN and 0 in the copy; got $n_orig and $n_brk"
fi
if run_launch "$BROKEN"; then RC=0; else RC=$?; fi; OUT=$(cat "$WORK/out")
if [ "$RC" -ne 0 ] && printf '%s' "$OUT" | grep -q "unbound variable"; then
  ok "broken copy fails at runtime with an unbound-variable error (rc=$RC)"
else
  bad "broken copy did NOT surface the unbound var; the smoke cannot discriminate" \
      "rc=$RC out=$OUT"
fi

echo
echo "  $PASS passed, $FAIL failed"
[ "$FAIL" -eq 0 ]
