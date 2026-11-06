#!/usr/bin/env bash
# claude-rotate-refresh.sh brings an installed rotator up to the checkout, leaves
# a host with none installed alone, and says which happened.
set -u
here="$(cd "$(dirname "$0")" && pwd)"
d="$(mktemp -d)"; trap 'rm -rf "$d"' EXIT
pass=0; fail=0
check() { if [[ "$1" == "$2" ]]; then pass=$((pass+1)); else fail=$((fail+1)); echo "FAIL: $3 (got: $1)"; fi; }
verdict() { sed -n 's/^verdict=\([a-z_]*\).*/\1/p'; }
printf 'new\n' > "$d/src.py"
mkdir -p "$d/inst"

check "$(bash "$here/claude-rotate-refresh.sh" "$d/src.py" "$d/inst/claude-rotate.py" | verdict)" \
  claude_rotate_not_installed "no installed rotator: nothing installed"
check "$([[ -e "$d/inst/claude-rotate.py" ]] && echo present || echo absent)" absent "and no file appears"

printf 'old\n' > "$d/inst/claude-rotate.py"
ino_before="$(ls -i "$d/inst/claude-rotate.py" | awk '{print $1}')"
check "$(bash "$here/claude-rotate-refresh.sh" "$d/src.py" "$d/inst/claude-rotate.py" | verdict)" \
  claude_rotate_refreshed "an older copy is refreshed"
check "$(cat "$d/inst/claude-rotate.py")" new "with the checkout's bytes"
check "$([[ -x "$d/inst/claude-rotate.py" ]] && echo exec || echo noexec)" exec "and stays executable"
ino_after="$(ls -i "$d/inst/claude-rotate.py" | awk '{print $1}')"
check "$([[ "$ino_before" != "$ino_after" ]] && echo renamed || echo in_place)" renamed "by rename, never in place"

check "$(bash "$here/claude-rotate-refresh.sh" "$d/src.py" "$d/inst/claude-rotate.py" | verdict)" \
  claude_rotate_current "an equal copy is left alone"

check "$(bash "$here/claude-rotate-refresh.sh" "$d/missing.py" "$d/inst/claude-rotate.py" | verdict)" \
  claude_rotate_refresh_failed "a missing source says so"

echo "test-claude-rotate-refresh: $pass passed, $fail failed"
[[ $fail -eq 0 ]]
