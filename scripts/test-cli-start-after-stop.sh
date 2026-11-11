#!/usr/bin/env bash
# `amux start` after a stop. The server's stop ends the agent and keeps its tmux
# pane (a bare shell), so `tmux has-session` stays true. `amux start` read that
# as "already running" and refused until the session was killed by hand, and
# `amux stop` waited its full 30 seconds for a session that never goes away.
#
# Runs the real CLI against a PRIVATE tmux server (its own TMUX_TMPDIR, $TMUX
# dropped) and a throwaway CC_HOME, with a stub `claude` that just sleeps, so no
# real worker and not the fleet's tmux server is touched.
#
# Cases, each checked on its outcome:
#   A  pane holds only a shell        -> start replaces it and launches
#   B  pane shell has a live child    -> "already running", pane untouched
#   C  pane process is the agent      -> "already running" (an exec'd launch has
#      itself, not a shell               no children and must not look empty)
#   D  agent_alive (what stop waits on) is false for A's shape, true for B's
# plus a negative control: the CLI with the new check disabled fails case A.
set -euo pipefail
cd "$(dirname "$0")/.." || exit 1
AMUX_BIN="${AMUX_BIN:-./amux}"
command -v tmux >/dev/null || { echo "tmux is required for this test" >&2; exit 1; }
PASS=0; FAIL=0
ok()  { echo "  ok   $1"; PASS=$((PASS+1)); }
bad() { echo "  FAIL $1"; [ -n "${2:-}" ] && echo "       $2"; FAIL=$((FAIL+1)); }

WORK=$(mktemp -d)
export TMUX_TMPDIR="$WORK/tmux"; mkdir -p "$TMUX_TMPDIR"
unset TMUX TMUX_PANE
# The CLI runs bare `tmux`, which TMUX_TMPDIR points at this socket. The test's
# own calls name the socket outright, so none of them can reach another server.
SOCK="$TMUX_TMPDIR/tmux-$(id -u)/default"
mkdir -p -m 700 "${SOCK%/*}"
t() { tmux -S "$SOCK" "$@"; }
cleanup() { t kill-server >/dev/null 2>&1 || true; rm -rf "$WORK"; }
trap cleanup EXIT

FAKE_HOME="$WORK/home"; CC="$FAKE_HOME/.amux"
mkdir -p "$CC/sessions" "$WORK/workdir" "$WORK/bin"
cat > "$CC/sessions/sas.env" <<ENVEOF
CC_DIR="$WORK/workdir"
CC_PROVIDER="claude"
ENVEOF
# The agent: a process that stays up, so the launched pane has a live child.
printf '#!/usr/bin/env bash\nexec sleep 300\n' > "$WORK/bin/claude"
chmod +x "$WORK/bin/claude"

# AMUX_API names a port nothing listens on: with a server up, `amux start`
# hands a claude launch to it, and a test must never reach the live one. The
# empty-pane check runs before that hand-off, so the local path covers it.
run_start() {
  env -u AMUX_URL AMUX_API=https://127.0.0.1:9 PATH="$WORK/bin:$PATH" HOME="$FAKE_HOME" CC_HOME="$CC" \
    AMUX_SESSION=sas-test AMUX_WORKER=sas-test \
    bash "$1" start sas --detach >"$WORK/out" 2>&1
}
pane_pid() { t list-panes -t "=amux-sas:" -F '#{pane_pid}' 2>/dev/null | head -1; }
fresh_session() {
  t kill-session -t "=amux-sas" >/dev/null 2>&1 || true
  t new-session -d -s amux-sas -c "$WORK/workdir" "$@"
  sleep 0.5
}
has_live_child() {
  local kids; kids=$(pgrep -P "$1" | paste -sd, -) || return 1
  [ -n "$kids" ] && ps -p "$kids" -o stat= | grep -qv '^[[:space:]]*Z'
}

# ── A: the shape a server stop leaves ─────────────────────────────────────────
case_a() {
  fresh_session bash --noprofile --norc
  if run_start "$1"; then RC=0; else RC=$?; fi
  OUT=$(cat "$WORK/out"); AFTER=$(pane_pid)
}
case_a "$AMUX_BIN"
if [ "$RC" -eq 0 ] && grep -q "started" <<<"$OUT" && ! grep -q "already running" <<<"$OUT" \
   && [ -n "$AFTER" ] && has_live_child "$AFTER"; then
  ok "A: a shell-only session is replaced and the worker launches"
else
  bad "A: start after stop did not launch (rc=$RC)" "$OUT"
fi
if grep -aq "verdict=empty_pane_replaced session=sas" "$CC/logs/server-rs.log" 2>/dev/null; then
  ok "A: the replacement is logged (verdict=empty_pane_replaced)"
else
  bad "A: no empty_pane_replaced line in server-rs.log"
fi

# ── B: a live worker (shell with a running child) ─────────────────────────────
fresh_session bash --noprofile --norc -c 'sleep 300; true'
BEFORE=$(pane_pid)
if run_start "$AMUX_BIN"; then RC=0; else RC=$?; fi; OUT=$(cat "$WORK/out")
if grep -q "already running" <<<"$OUT" && [ "$(pane_pid)" = "$BEFORE" ] && has_live_child "$BEFORE"; then
  ok "B: a live worker is left alone (already running)"
else
  bad "B: a live worker was not reported as running, or its pane changed" "$OUT"
fi

# ── C: the agent is the pane process itself, with no children ─────────────────
fresh_session sleep 300
BEFORE=$(pane_pid)
if run_start "$AMUX_BIN"; then RC=0; else RC=$?; fi; OUT=$(cat "$WORK/out")
if grep -q "already running" <<<"$OUT" && [ "$(pane_pid)" = "$BEFORE" ]; then
  ok "C: an exec'd agent with no children is not mistaken for an empty pane"
else
  bad "C: an exec'd agent was replaced" "$OUT"
fi

# ── D: the predicate stop waits on ────────────────────────────────────────────
probe() {
  env HOME="$FAKE_HOME" CC_HOME="$CC" bash -c \
    'source "$1" >/dev/null 2>&1; set +e; agent_alive sas; echo "rc=$?"' _ "$AMUX_BIN"
}
fresh_session bash --noprofile --norc
D_EMPTY=$(probe)
fresh_session bash --noprofile --norc -c 'sleep 300; true'
D_LIVE=$(probe)
if [ "$D_EMPTY" = "rc=1" ] && [ "$D_LIVE" = "rc=0" ]; then
  ok "D: agent_alive is false for a shell-only pane and true for a live one"
else
  bad "D: agent_alive gave empty=$D_EMPTY live=$D_LIVE (want rc=1 and rc=0)"
fi

# ── negative control: without the replacement, case A must fail ───────────────
BROKEN="$WORK/amux-broken"
sed 's/if \[\[ "\$_pane_rc" == 1 \]\]; then/if false; then/' "$AMUX_BIN" > "$BROKEN"
if cmp -s "$AMUX_BIN" "$BROKEN"; then
  bad "negative control: the fixture is identical to the CLI (has the specimen drifted?)"
else
  case_a "$BROKEN"
  if grep -q "already running" <<<"$OUT"; then
    ok "negative control: the CLI without the fix reports a stopped worker as already running"
  else
    bad "negative control: the broken CLI still launched; this test cannot fail" "$OUT"
  fi
fi

echo
echo "  $PASS passed, $FAIL failed"
[ "$FAIL" -eq 0 ]
