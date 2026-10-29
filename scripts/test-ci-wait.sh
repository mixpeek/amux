#!/usr/bin/env bash
# test-ci-wait.sh [amux]: `amux ci wait` shares ONE poller per run across every
# waiter (mxp-gs12 review, 2026-10-18: one lane's waiters rate-limited the
# fleet's GitHub App). A stub `gh` counts its calls under a temp HOME.
#   1. three concurrent waiters on one run all see success, and gh is called by
#      one poller only (calls <= polls a single waiter would make, not 3x)
#   2. a run that concludes failure exits 1
#   3. GitHub unreadable (gh exits 1 with a 401 body) exits 3 UNMEASURED, never 0
#   4. a "completed failure" cached before the wait began (an earlier attempt of
#      a re-run, 2026-10-29) is not the verdict: the wait polls and gets success
set -uo pipefail
AM="${1:-$(cd "$(dirname "$0")/.." && pwd)/amux}"; AM="$(cd "$(dirname "$AM")" && pwd)/$(basename "$AM")"
T="$(mktemp -d "${TMPDIR:-/tmp}/ciw.XXXXXX")"; trap 'rm -rf -- "${T:?}"' EXIT
export HOME="$T/home" AMUX_HOME="$T/home/.amux"; mkdir -p "$AMUX_HOME" "$T/bin"
unset AMUX_SESSION GH_TOKEN GITHUB_TOKEN
cat > "$T/bin/gh" <<'EOF'
#!/bin/sh
n=$(cat "$CIW_COUNT" 2>/dev/null || echo 0); n=$((n + 1)); echo "$n" > "$CIW_COUNT"
case "$CIW_MODE" in
  unreadable) echo '{"message":"Bad credentials","status":"401"}'; exit 1 ;;
esac
if [ "$n" -lt 3 ]; then echo '{"status":"in_progress","conclusion":null,"url":"u","head_sha":"s","name":"n"}'
else echo "{\"status\":\"completed\",\"conclusion\":\"$CIW_CONCLUSION\",\"url\":\"u\",\"head_sha\":\"s\",\"name\":\"n\"}"; fi
EOF
chmod +x "$T/bin/gh"; export PATH="$T/bin:$PATH"
fail=0
ok() { echo "ok   $*"; }; bad() { fail=1; echo "FAIL $*"; }

export CIW_COUNT="$T/c1" CIW_MODE=normal CIW_CONCLUSION=success
pids=""; for i in 1 2 3; do bash "$AM" ci wait 101 --repo o/r --poll 1 --timeout 60 > "$T/w$i" 2>&1 & pids="$pids $!"; done
rcs=""; for p in $pids; do wait "$p"; rcs="$rcs $?"; done
calls="$(cat "$T/c1")"
if [ "$rcs" = " 0 0 0" ] && [ "$calls" -le 4 ]; then ok "three waiters on one run: all success, $calls gh call(s) in total (one poller)"
else bad "concurrent waiters: rcs=$rcs calls=$calls: $(cat "$T/w1")"; fi
np="$(grep -c "run=101 verdict=success role=poller" "$AMUX_HOME/logs/ci-watch.log")"; nr="$(grep -c "run=101 verdict=success role=reader" "$AMUX_HOME/logs/ci-watch.log")"
[ "$np" = 1 ] && [ "$nr" = 2 ] && ok "exactly one waiter polled GitHub, two only read the shared state" || bad "roles: $np poller(s), $nr reader(s)"

export CIW_COUNT="$T/c2" CIW_CONCLUSION=failure
bash "$AM" ci wait 102 --repo o/r --poll 1 --timeout 60 >/dev/null 2>&1; rc=$?
[ "$rc" = 1 ] && ok "a failed run exits 1" || bad "failed run rc=$rc"

export CIW_COUNT="$T/c3" CIW_MODE=unreadable
out="$(bash "$AM" ci wait 103 --repo o/r --poll 1 --timeout 60 --json 2>&1)"; rc=$?
if [ "$rc" = 3 ] && printf '%s' "$out" | grep -q '"measured": false'; then ok "an unreadable GitHub exits 3 and says measured false"
else bad "unreadable: rc=$rc out=$out"; fi
export CIW_COUNT="$T/c4" CIW_MODE=normal CIW_CONCLUSION=success
mkdir -p "$AMUX_HOME/ci-watch/o_r-104"
printf '{"polled_at": %s, "rc": 0, "measured": true, "status": "completed", "conclusion": "failure", "url": "u", "run_attempt": 1}' \
  "$(( $(date +%s) - 300 ))" > "$AMUX_HOME/ci-watch/o_r-104/state.json"
bash "$AM" ci wait 104 --repo o/r --poll 1 --timeout 60 >/dev/null 2>&1; rc=$?
[ "$rc" = 0 ] && ok "a conclusion cached before the wait began is re-read, not reported" || bad "stale cached attempt: rc=$rc"
exit "$fail"
