#!/usr/bin/env bash
# AMUX-3789 — the gh shim must inject the App token, defer to an explicit one,
# never recurse into itself, and fail OPEN when the minter is broken.
#
# It runs against a FAKE gh and a FAKE get-token.sh in a throwaway PATH, so no
# cell reaches GitHub, mints a real token, or depends on network. The fake gh
# prints the identity it was handed, which is the only thing these cells assert.
#
# CELL 5 IS THE CONTROL. Every other cell asserts the shim DID something; cell 5
# asserts it delegates untouched when the caller already chose an identity. A
# shim that always injected would pass 1-4 and silently replace a CI token.
#
# Exit 0 = pass, 1 = failure.
set -uo pipefail
cd "$(dirname "$0")/.."
SHIM="$(pwd)/scripts/gh-shim/gh"
[ -x "$SHIM" ] || { echo "FAIL: $SHIM missing or not executable"; exit 1; }

D=$(mktemp -d) || exit 1
trap 'rm -rf "$D"' EXIT
PASS=0; FAIL=0
ok()  { PASS=$((PASS+1)); echo "  ok   — $1"; }
bad() { FAIL=$((FAIL+1)); echo "  FAIL — $1"; }

# A fake real gh, in its own dir, that reports the token it received.
# AMUX_HOME IS ~/.amux, so the fixture root is the amux home itself — not a
# home directory containing one. The first draft nested it and cells 1 and 4
# failed against a correct shim, which is the fixture lying rather than the
# code being wrong.
mkdir -p "$D/realbin" "$D/shimbin" "$D/amuxhome/github-app"
cat > "$D/realbin/gh" <<'EOF'
#!/usr/bin/env bash
echo "GH_TOKEN=${GH_TOKEN:-<unset>} args=$*"
EOF
chmod +x "$D/realbin/gh"
ln -s "$SHIM" "$D/shimbin/gh"

cat > "$D/amuxhome/github-app/get-token.sh" <<'EOF'
#!/usr/bin/env bash
[ "${1:-}" = "--raw" ] && echo "app-token-xyz"
EOF
chmod +x "$D/amuxhome/github-app/get-token.sh"

run() { PATH="$D/shimbin:$D/realbin:/usr/bin:/bin" AMUX_HOME="$D/amuxhome" "$@" gh api /rate_limit 2>&1; }

echo "gh shim — App token in front of every fleet gh call"

# 1 — injects when nothing is set
out=$(run env -u GH_TOKEN -u GITHUB_TOKEN)
case "$out" in
  *"GH_TOKEN=app-token-xyz"*) ok "1: mints and injects the App token" ;;
  *) bad "1: got [$out]" ;;
esac

# 2 — the real gh actually receives the args
case "$out" in
  *"args=api /rate_limit"*) ok "2: arguments pass through unchanged" ;;
  *) bad "2: args lost — got [$out]" ;;
esac

# 3 — no recursion: the shim found the real gh, not itself
case "$out" in
  *"GH_TOKEN="*) ok "3: resolved the REAL gh rather than re-entering the shim" ;;
  *) bad "3: shim did not reach a real gh — got [$out]" ;;
esac

# 4 — FAILS OPEN when the minter is broken, and says so on stderr
cat > "$D/amuxhome/github-app/get-token.sh" <<'EOF'
#!/usr/bin/env bash
exit 1
EOF
chmod +x "$D/amuxhome/github-app/get-token.sh"
out4=$(run env -u GH_TOKEN -u GITHUB_TOKEN)
if [[ "$out4" == *"GH_TOKEN=<unset>"* && "$out4" == *"mint FAILED"* ]]; then
  ok "4: broken minter still runs gh, and announces the ambient identity"
else
  bad "4: expected fail-open + a loud line, got [$out4]"
fi
cat > "$D/amuxhome/github-app/get-token.sh" <<'EOF'
#!/usr/bin/env bash
[ "${1:-}" = "--raw" ] && echo "app-token-xyz"
EOF
chmod +x "$D/amuxhome/github-app/get-token.sh"

# 5 — CONTROL: an explicit token is left alone
out5=$(run env GH_TOKEN=caller-chose-this)
case "$out5" in
  *"GH_TOKEN=caller-chose-this"*) ok "5: control — an explicit GH_TOKEN is never replaced" ;;
  *) bad "5: shim overrode the caller's token — got [$out5]" ;;
esac

# 6 — GITHUB_TOKEN counts as an explicit choice too
out6=$(run env -u GH_TOKEN GITHUB_TOKEN=ci-token)
case "$out6" in
  *"GH_TOKEN=<unset>"*) ok "6: GITHUB_TOKEN also suppresses injection" ;;
  *) bad "6: injected over GITHUB_TOKEN — got [$out6]" ;;
esac

# 7 — the documented escape hatch works
out7=$(run env -u GH_TOKEN -u GITHUB_TOKEN AMUX_GH_SHIM=0)
case "$out7" in
  *"GH_TOKEN=<unset>"*) ok "7: AMUX_GH_SHIM=0 restores the ambient identity" ;;
  *) bad "7: escape hatch did nothing — got [$out7]" ;;
esac

# 8 — the token never appears on stderr, even when things go wrong
if [[ "$out" == *"app-token-xyz"* ]]; then
  # it may appear via the fake gh's own stdout; stderr must stay clean
  err=$(PATH="$D/shimbin:$D/realbin:/usr/bin:/bin" AMUX_HOME="$D/amuxhome" \
        env -u GH_TOKEN -u GITHUB_TOKEN gh api /rate_limit 2>&1 >/dev/null)
  case "$err" in
    *"app-token-xyz"*) bad "8: the token leaked onto stderr" ;;
    *) ok "8: nothing the shim writes to stderr contains the token" ;;
  esac
fi

echo
echo "pass=$PASS fail=$FAIL"
[ "$FAIL" = 0 ]
