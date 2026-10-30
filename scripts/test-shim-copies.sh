#!/usr/bin/env bash
# Two copies of an amux shim on one PATH must reach the real binary, not each
# other (cpu RCA 20261030-065821: a pgrep shim exec'd between ~/.amux/shims and
# a demo home's copy for 7h44m). Each shim is installed twice, in two
# directories ahead of a fake real tool, and must answer from the real tool
# within a few seconds. Exit 0 = all pass. Wired into checks.yml.
set -uo pipefail
cd "$(dirname "$0")/.."
ROOT=$(pwd)
pass=0; fail=0
ok()  { pass=$((pass+1)); echo "ok   $1"; }
bad() { fail=$((fail+1)); echo "FAIL $1"; }
T=$(mktemp -d "${TMPDIR:-/tmp}/shim-copies.XXXXXX"); trap 'rm -rf "$T"' EXIT
mkdir -p "$T/a" "$T/b" "$T/real" "$T/home"
# Fake real tools: plain scripts that do not mention the harness.
for tool in pgrep pkill tmux docker; do
  printf '#!/bin/sh\necho REAL-%s "$@"\n' "$tool" > "$T/real/$tool"; chmod +x "$T/real/$tool"
done
install_copy() { # <shim file> <tool name>
  for d in a b; do cp "$ROOT/scripts/shims/$1" "$T/$d/$2"; chmod +x "$T/$d/$2"; done
}
install_copy pkill-guard-shim.sh pgrep
install_copy pkill-guard-shim.sh pkill
install_copy tmux-fleet-shim.sh tmux
install_copy docker-vm-shim.sh docker
P="$T/a:$T/b:$T/real:/usr/bin:/bin"
# A shim that loops keeps exec'ing in the same pid, and alarm() does not
# survive that chain on macOS, so a watchdog polls the pid and kills it.
bounded() { # <PATH> <tool> <args...>; prints output, then "rc=N" or "LOOP"
  local path=$1 tool=$2; shift 2
  ( cd "$T" && exec env -i HOME="$T/home" AMUX_HOME="$T/home/.amux" PATH="$path" "$T/a/$tool" "$@" ) > "$T/out.txt" 2>&1 &
  local pid=$! i
  # An exited child stays a zombie until waited for, and kill -0 still
  # succeeds on a zombie, so ask ps whether it is still running.
  alive() { case "$(ps -o stat= -p "$pid" 2>/dev/null)" in ''|Z*) return 1 ;; esac; }
  # 60 s: a loop never ends, and the first launch of a freshly written script
  # took 13 s on a loaded Mac (load 32) while a second run took 0.9 s.
  for i in $(seq 1 600); do alive || break; sleep 0.1; done
  if alive; then kill -9 "$pid" 2>/dev/null; wait "$pid" 2>/dev/null; cat "$T/out.txt"; echo LOOP; return; fi
  wait "$pid"; local rc=$?; cat "$T/out.txt"; echo "rc=$rc"
}
run() { bounded "$P" "$@"; }
out=$(run pgrep -x nothing-by-this-name); [[ "$out" == *REAL-pgrep* ]] && ok "pgrep: two shim copies reach the real pgrep" || bad "pgrep looped or failed: $out"
out=$(run tmux -V); [[ "$out" == *REAL-tmux* ]] && ok "tmux: two shim copies reach the real tmux" || bad "tmux looped or failed: $out"
out=$(run docker version); [[ "$out" == *REAL-docker* ]] && ok "docker: two shim copies reach the real docker" || bad "docker looped or failed: $out"
# With only shims on PATH there is no real binary: a clear error, not a loop.
# The utilities the shims call, without tmux: a Linux runner has a real tmux
# in /usr/bin, which the shim rightly found ("tmux 3.4", run 41047171299).
mkdir -p "$T/util"
for u in bash sh env dirname basename grep id cat mkdir printf date; do
  w=$(command -v "$u") && ln -sf "$w" "$T/util/$u"
done
out=$(bounded "$T/a:$T/b:$T/util" tmux -V)
[[ "$out" == *rc=127* ]] && ok "shims and no real tmux: exit 127, no loop" || bad "shims only: $out"
echo "test-shim-copies: $pass passed, $fail failed"
[ "$fail" = 0 ]
