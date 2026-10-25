#!/usr/bin/env bash
# Runs scripts/tmux-watchdog.sh against fake tmux/pgrep/ps/amux binaries and a
# fake ~/.amux, and checks what Mode 2 restarts:
#   - a Claude worker whose pane is a bare shell: restarted
#   - a Claude worker with a live claude process: left alone
#   - a codex worker (never has a claude process): left alone
#   - an isolated Claude worker with a bare shell: left alone
#   - a worker that keeps dying: restarted WORKER_RESTART_MAX times, then held
#     with one HOLD line, not restarted every scan
set -euo pipefail

HERE="$(cd "$(dirname "$0")" && pwd)"
T="$(mktemp -d "${TMPDIR:-/tmp}/watchdog-test.XXXXXX")"
trap 'rm -rf -- "${T:?}"' EXIT
mkdir -p "$T/bin" "$T/home/sessions" "$T/home/logs"
CALLS="$T/calls"
: > "$CALLS"

w() { printf '%s\n' "${@:2}" > "$T/home/sessions/$1.env"; }
w dead     'CC_PROVIDER="claude"'
w alive    'CC_PROVIDER=claude'
w codexer  'CC_PROVIDER="codex"'
w iso      'CC_PROVIDER="claude"' 'CC_ISOLATED="1"'

# Every worker has a tmux session. Pane pids: dead=101 alive=102 codexer=103 iso=104.
cat > "$T/bin/tmux" <<'EOF'
#!/usr/bin/env bash
case "$1" in
  list-sessions) echo "amux-x: 1 windows"; exit 0 ;;
  has-session) exit 0 ;;
  list-panes) case "$3" in =amux-dead) echo 101;; =amux-alive) echo 102;; =amux-codexer) echo 103;; =amux-iso) echo 104;; esac ;;
  kill-session) echo "kill $3" >> "$CALLS" ;;
esac
EOF
cat > "$T/bin/pgrep" <<'EOF'
#!/usr/bin/env bash
[[ "$2" == 102 ]] && echo 2002
exit 0
EOF
cat > "$T/bin/ps" <<'EOF'
#!/usr/bin/env bash
[[ "$2" == 2002 ]] && echo "/usr/local/bin/claude --resume"
exit 0
EOF
cat > "$T/bin/amux" <<'EOF'
#!/usr/bin/env bash
case "$1" in url) echo "https://localhost:1";; start) echo "start $2" >> "$CALLS";; esac
EOF
printf '#!/usr/bin/env bash\nexit 0\n' > "$T/bin/curl"
printf '#!/usr/bin/env bash\nexit 0\n' > "$T/bin/sleep"
printf '#!/usr/bin/env bash\necho "System-wide memory free percentage: 50%%"\n' > "$T/bin/memory_pressure"
chmod +x "$T/bin/"*

run() {
  CALLS="$CALLS" PATH="$T/bin:$PATH" AMUX_WATCHDOG_HOME="$T/home" AMUX_BIN="$T/bin/amux" \
    AMUX_WATCHDOG_WORKER_COOLDOWN=0 AMUX_START_ALL_STAGGER=0 bash "$HERE/tmux-watchdog.sh"
}

fail=0
check() { if eval "$2"; then echo "PASS $1"; else echo "FAIL $1"; fail=1; fi; }
starts() { grep -c "^start $1\$" "$CALLS"; }

run
check "a Claude worker with a bare shell is restarted" '[[ $(starts dead) == 1 ]]'
check "a Claude worker with a live claude is left alone" '[[ $(starts alive) == 0 ]]'
check "a codex worker is not judged by the claude check" '[[ $(starts codexer) == 0 ]]'
check "an isolated worker is left to its owner" '[[ $(starts iso) == 0 ]]'
check "the detection line names what was not judged" 'grep -q "not judged: 1 non-claude, 1 isolated" "$T/home/logs/tmux-watchdog.log"'

run; run; run; run
check "a worker that keeps dying is restarted 3 times, then held" '[[ $(starts dead) == 3 ]]'
check "the hold is logged once, not every scan" '[[ $(grep -c "HOLD dead:" "$T/home/logs/tmux-watchdog.log") == 1 ]]'

echo "kill/start calls:"; sed 's/^/  /' "$CALLS"
if (( fail )); then echo "test-tmux-watchdog: FAILED"; exit 1; fi
echo "test-tmux-watchdog: all 7 checks passed"
