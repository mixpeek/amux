#!/usr/bin/env bash
# scripts/check_slop_ratchet.py on a scratch repo: a raise is refused, a justified raise is let
# through, slop already on the base never blocks an unrelated edit, a new file starts at zero, a
# rename keeps its counts, test code is not counted, and god-file growth only warns.
set -euo pipefail
ROOT=$(cd "$(dirname "$0")/.." && pwd)
CHK="$ROOT/scripts/check_slop_ratchet.py"
T=$(mktemp -d)
trap 'rm -rf "$T"' EXIT
fails=0
ok() { echo "ok   $1"; }
bad() { echo "FAIL $1"; fails=$((fails + 1)); }

cd "$T" && git init -q -b main r && cd r
g() { git -c user.name=t -c user.email=t@example.com "$@"; }
mkdir -p crates/x/src crates/amux-dashboard/static
printf 'fn a() {\n    let _ = f();\n}\n' > crates/x/src/lib.rs
printf 'function a() { try { f(); } catch (e) {} }\n' > crates/amux-dashboard/static/app.js
g add -A && g commit -qm base
BASE=$(git rev-parse HEAD)
run() { python3 "$CHK" --base "$1" --head HEAD > "$T/out" 2>&1; }

printf 'fn a() {\n    let _ = f();\n    let _ = g();\n}\n' > crates/x/src/lib.rs
g commit -qam raise
run "$BASE" && bad "a new discarded result passed" || { grep -q "discarded_result 1 -> 2" "$T/out" && ok "a raise is refused, naming the indicator" || bad "raise: $(cat "$T/out")"; }

g commit -q --allow-empty -m "why" -m "Slop-Raise: a channel send whose receiver may be gone"
run "$BASE" && grep -q "verdict=slop_raise_allowed" "$T/out" && ok "Slop-Raise lets a deliberate raise through, logged" || bad "slop-raise: $(cat "$T/out")"

g reset -q --hard "$BASE"
printf 'function a() { try { f(); } catch (e) {} }\nfunction b() { return 1; }\n' > crates/amux-dashboard/static/app.js
g commit -qam unrelated
run "$BASE" && ok "slop already on the base does not block an unrelated edit" || bad "unrelated: $(cat "$T/out")"

g reset -q --hard "$BASE"
printf 'function n() { try { f(); } catch {} return `<b style="x">`; }\n' > crates/amux-dashboard/static/new.js
g add -A && g commit -qm new
run "$BASE" && bad "a new file with slop passed" || { grep -q "empty_catch 0 -> 1" "$T/out" && grep -q "inline_style 0 -> 1" "$T/out" && ok "a new file starts at zero" || bad "new file: $(cat "$T/out")"; }

g reset -q --hard "$BASE"
g mv crates/x/src/lib.rs crates/x/src/moved.rs && g commit -qm rename
run "$BASE" && ok "a rename keeps its counts" || bad "rename: $(cat "$T/out")"

g reset -q --hard "$BASE"
printf 'fn a() {\n    let _ = f();\n}\n#[cfg(test)]\nmod tests {\n    fn t() { x().unwrap(); }\n}\n' > crates/x/src/lib.rs
g commit -qam tests
run "$BASE" && ok "code after #[cfg(test)] is not counted" || bad "test code: $(cat "$T/out")"

g reset -q --hard "$BASE"
python3 - <<'PY'
open('crates/x/src/lib.rs','a').write('// line\n' * 5100)
PY
g commit -qam grow
run "$BASE" && grep -q "verdict=slop_god_file_grew" "$T/out" && ok "a god file growing warns and does not refuse" || bad "god file: $(cat "$T/out")"

python3 "$CHK" --report --head HEAD | grep -q "verdict=slop_report measured=true" && ok "--report computes totals from the code" || bad "report"

echo "test-slop-ratchet: $fails failed"
exit "$fails"
