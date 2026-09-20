#!/usr/bin/env bash
# The launchd authority launcher must choose origin/main's detached source even
# when its configured repository is currently on a stale local branch. This
# exercises the shipped launcher and tiny fake builders rather than copying its
# Git selection into a unit test.
set -euo pipefail
# Fixtures are HERMETIC: no template from the developer's own git config.
# `git init` copies $HOME's init.templatedir into every new repo, so a global
# commit-msg hook (a Conventional Commits enforcer, say) lands in the throwaway
# repos below and rejects their fixture commits. 17 of this repo's test scripts
# failed that way on a machine that had one, while CI stayed green because the
# runner has no template — a test that passes only on machines configured like
# the author's. Empty means "no template", and git then creates no .git/hooks,
# so a test that installs a hook makes that directory itself.
export GIT_TEMPLATE_DIR=


ROOT=$(cd "$(dirname "$0")/.." && pwd)
LAUNCHER="$ROOT/scripts/rust-auto-build-authority.sh"
TMP=$(mktemp -d)
trap 'rm -rf "$TMP"' EXIT
AUTH="$TMP/authority"
STALE="$TMP/stale"
WORK="$TMP/activation-source"
MARKER="$TMP/activation-source.authority"
LOG="$TMP/authority.log"
TRACE="$TMP/trace"
PASS=0; FAIL=0
export TRACE

ok() { PASS=$((PASS + 1)); }
bad() { FAIL=$((FAIL + 1)); echo "FAIL: $1"; }
expect() { if "$@"; then ok; else bad "$*"; fi; }

git init -q -b main "$AUTH"
mkdir -p "$AUTH/scripts"
cat > "$AUTH/scripts/rust-auto-build.sh" <<'EOF'
#!/usr/bin/env bash
set -euo pipefail
printf 'repo=%s sha=%s\n' "$AMUX_REPO" "$(git -C "$AMUX_REPO" rev-parse HEAD)" >> "$TRACE"
EOF
chmod +x "$AUTH/scripts/rust-auto-build.sh"
(
  cd "$AUTH"
  printf 'elected\n' > source.txt
  git add -A
  git -c user.name=test -c user.email=test@example.com commit -qm elected
)
ELECTED=$(git -C "$AUTH" rev-parse HEAD)
git -C "$AUTH" update-ref refs/remotes/origin/main "$ELECTED"
git clone -q "$AUTH" "$STALE"
(
  cd "$STALE"
  git checkout -qb stale/local
  printf 'stale\n' > local.txt
  git add -A
  git -c user.name=test -c user.email=test@example.com commit -qm stale
)
STALE_SHA=$(git -C "$STALE" rev-parse HEAD)

run_launcher() {
  AMUX_AUTHORITY_REPO="$STALE" \
  AMUX_AUTHORITY_WORKTREE="$WORK" \
  AMUX_AUTHORITY_MARKER="$MARKER" \
  AMUX_AUTHORITY_LOG="$LOG" \
  AMUX_RS_ACTIVATION_REF=origin/main \
    bash "$LAUNCHER"
}

run_launcher
expect test -f "$MARKER"
expect test "$(cat "$MARKER")" = "$STALE"
expect grep -q "repo=$WORK sha=$ELECTED" "$TRACE"
expect test "$(git -C "$WORK" rev-parse HEAD)" = "$ELECTED"
expect grep -q "BUILD $ELECTED from detached origin/main" "$LOG"

# Move only the remote-tracking authority ref. The configured source remains
# on its stale local branch, so a launcher that runs its local HEAD would still
# choose STALE_SHA rather than the newly elected source.
(
  cd "$AUTH"
  printf 'next elected\n' > next.txt
  git add -A
  git -c user.name=test -c user.email=test@example.com commit -qm next
)
NEXT=$(git -C "$AUTH" rev-parse HEAD)
git -C "$STALE" fetch -q origin main
run_launcher
expect grep -q "repo=$WORK sha=$NEXT" "$TRACE"
expect test "$(git -C "$WORK" rev-parse HEAD)" = "$NEXT"
expect test "$(git -C "$STALE" rev-parse HEAD)" = "$STALE_SHA"

echo "test-build-activation-launcher: $PASS passed, $FAIL failed"
[ "$FAIL" -eq 0 ]
