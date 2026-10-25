#!/usr/bin/env bash
# post-commit names the lane's open owner requests when a commit cites none
# (velocity audit 2026-10-25). Runs the real hook in a scratch repo against a
# stub `amux` on PATH, so no server is needed.
set -euo pipefail
here="$(cd "$(dirname "$0")" && pwd)"
hook="$here/git-hooks/post-commit"
work="$(mktemp -d "${TMPDIR:-/tmp}/pc-requests.XXXXXX")"
trap 'rm -rf "$work"' EXIT
fail=0
ok()  { echo "  ok   $1"; }
bad() { echo "  FAIL $1"; fail=1; }

mkdir -p "$work/bin" "$work/repo"
cat > "$work/bin/amux" <<'EOF'
#!/bin/sh
# Stub: answers `amux requests <lane> --open --json` from $STUB_REQUESTS.
echo "$*" >> "$STUB_LOG"
[ "$1" = "requests" ] || exit 0
cat "$STUB_REQUESTS"
EOF
chmod +x "$work/bin/amux"
export PATH="$work/bin:$PATH" STUB_LOG="$work/calls.log"
export STUB_REQUESTS="$work/requests.json"
cat > "$STUB_REQUESTS" <<'EOF'
{"measured": true, "requests": [
 {"id": "MSG-101", "age_h": 30.0, "status": "open", "text": "older ask"},
 {"id": "MSG-202", "age_h": 2.5, "status": "open", "text": "newest ask about the vault picker"}]}
EOF

cd "$work/repo" || exit 1
git init -q . && git config user.email t@example.com && git config user.name t
unset AMUX_SESSION

commit() {  # commit <message>; prints the hook's stderr
  echo "$RANDOM" >> f && git add f
  git -c core.hooksPath=/dev/null commit -q -F - <<<"$1"
  sh "$hook" 2>&1 >/dev/null
}

echo "1. a lane commit citing no request lists the open ones"
out=$(commit $'fix: something\n\nAmux-Session: lane-a')
grep -q 'cites no owner request; lane-a has 2 open' <<<"$out" && ok "names the lane and the count" || bad "count line missing: $out"
grep -q 'verdict=commit_cites_no_request open=2' <<<"$out" && ok "verdict field" || bad "verdict missing"
grep -q 'MSG-202' <<<"$out" && grep -q 'MSG-101' <<<"$out" && ok "lists the ids" || bad "ids missing"
[ "$(grep -n 'MSG-' <<<"$out" | head -1 | grep -c 'MSG-202')" = 1 ] && ok "newest first" || bad "order"
grep -q "amend --no-edit --trailer 'Amux-Request: MSG-<id>'" <<<"$out" && ok "amend remedy" || bad "amend remedy missing"
grep -q 'requests close --sha' <<<"$out" && ok "close remedy" || bad "close remedy missing"
grep -q '^requests lane-a --open --json' "$STUB_LOG" && ok "asked for that lane" || bad "stub not called for lane-a: $(cat "$STUB_LOG")"

echo "2. a commit that cites a request is silent"
: > "$STUB_LOG"
out=$(commit $'fix: x\n\nAmux-Session: lane-a\nAmux-Request: MSG-202')
[ -z "$(grep 'cites no owner request' <<<"$out")" ] && ok "silent" || bad "spoke anyway: $out"
[ ! -s "$STUB_LOG" ] && ok "no lookup" || bad "looked up anyway"

echo "3. MSG-<id> in the body counts too (same rule as amux requests)"
out=$(commit $'fix: y\n\nLands MSG-101.\n\nAmux-Session: lane-a')
[ -z "$(grep 'cites no owner request' <<<"$out")" ] && ok "silent" || bad "spoke anyway"

echo "4. a human commit is silent"
out=$(commit $'fix: z\n\nAmux-Session: (human)')
[ -z "$(grep 'cites no owner request' <<<"$out")" ] && ok "silent" || bad "spoke for (human)"

echo "5. no open requests is silent"
echo '{"measured": true, "requests": []}' > "$STUB_REQUESTS"
out=$(commit $'fix: w\n\nAmux-Session: lane-a')
[ -z "$(grep 'cites no owner request' <<<"$out")" ] && ok "silent" || bad "spoke with nothing open"

echo "6. an unreadable answer fails open"
echo 'not json' > "$STUB_REQUESTS"
out=$(commit $'fix: v\n\nAmux-Session: lane-a'); rc=$?
[ -z "$(grep 'cites no owner request' <<<"$out")" ] && ok "silent" || bad "spoke on bad json"
sh "$hook" >/dev/null 2>&1; [ $? = 0 ] && ok "hook exits 0" || bad "hook exit non-zero"

[ $fail = 0 ] && echo "PASS: post-commit open requests" || { echo "FAIL: post-commit open requests"; exit 1; }
