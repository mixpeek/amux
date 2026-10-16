#!/usr/bin/env bash
# The launchd authority launcher must choose origin/main's detached source even
# when its configured repository is currently on a stale local branch. This
# exercises the shipped launcher and tiny fake builders rather than copying its
# Git selection into a unit test.
set -euo pipefail

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
# CI verdicts come from a file, one "sha verdict" per line; unlisted = green.
VERDICTS="$TMP/verdicts"; : > "$VERDICTS"
printf '#!/usr/bin/env bash\nv=$(awk -v s="$1" '"'"'$1==s {print $2}'"'"' "%s" | tail -1); echo "${v:-green}"\n' "$VERDICTS" > "$TMP/verdict"
chmod +x "$TMP/verdict"

ok() { PASS=$((PASS + 1)); }
bad() { FAIL=$((FAIL + 1)); echo "FAIL: $1"; }
expect() { if "$@"; then ok; else bad "$*"; fi; }

git init -q -b main "$AUTH"
mkdir -p "$AUTH/scripts"
cat > "$AUTH/scripts/rust-auto-build.sh" <<'EOF'
#!/usr/bin/env bash
set -euo pipefail
printf 'repo=%s sha=%s authority=%s\n' "$AMUX_REPO" "$(git -C "$AMUX_REPO" rev-parse HEAD)" "$(git -C "$AMUX_REPO" rev-parse "$AMUX_RS_ACTIVATION_REF")" >> "$TRACE"
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
  AMUX_CI_VERDICT_CMD="$TMP/verdict" \
  AMUX_RS_HEALTH_URL="file://$TMP/health.json" \
    bash "$LAUNCHER"
}
commit_next() {
  ( cd "$AUTH"; printf '%s\n' "$1" > "$1.txt"; git add -A
    git -c user.name=test -c user.email=test@example.com commit -qm "$1" )
  git -C "$STALE" fetch -q origin main
  git -C "$AUTH" rev-parse HEAD
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

# CI GATE (Ethan, 2026-10-15: "make sure all the amux shit goes thru cicd").
# A pending tip while the live image IS the newest green: the pending tip is
# never handed to the builder (it re-elects the green one, which it skips).
GREEN=$NEXT
PENDING=$(commit_next pending)
echo "$PENDING pending" >> "$VERDICTS"
echo "{\"commit_full\":\"$GREEN\"}" > "$TMP/health.json"
: > "$TRACE"
run_launcher
expect grep -q "repo=$WORK sha=$GREEN" "$TRACE"
expect test "$(grep -c "sha=$PENDING" "$TRACE")" = 0
: > "$TRACE"
# Live is OLDER than the newest green: activate that green commit, not the tip.
echo "{\"commit_full\":\"$ELECTED\"}" > "$TMP/health.json"
run_launcher
expect grep -q "repo=$WORK sha=$GREEN" "$TRACE"
expect grep -q "CI-GATE tip $PENDING is pending; activating newest green $GREEN .*verdict=ci_gate_held_back" "$LOG"
# The builder must be told the ELECTED sha, or its own authority check refuses it.
expect grep -q "sha=$GREEN authority=$GREEN" "$TRACE"
# Once the tip goes green it is activated.
echo "$PENDING green" >> "$VERDICTS"
: > "$TRACE"
run_launcher
expect grep -q "repo=$WORK sha=$PENDING" "$TRACE"
# A red tip whose newest green is older than live: hold, never roll back.
RED=$(commit_next red)
echo "$RED red" >> "$VERDICTS"
echo "{\"commit_full\":\"$RED\"}" > "$TMP/health.json"
: > "$TRACE"
run_launcher
expect test ! -s "$TRACE"
expect grep -q "newest green $PENDING is older than the live image $RED.*verdict=ci_gate_no_rollback" "$LOG"
# Nothing green at all: refuse and keep the live image.
for s in $(git -C "$STALE" rev-list origin/main); do echo "$s red" >> "$VERDICTS"; done
echo "{}" > "$TMP/health.json"
run_launcher
expect test ! -s "$TRACE"
expect grep -q "verdict=ci_gate_no_green" "$LOG"
# CI that cannot be read fails closed, even with a green commit behind it.
echo "$RED unmeasured" >> "$VERDICTS"
echo "$ELECTED green" >> "$VERDICTS"
run_launcher
expect test ! -s "$TRACE"
expect grep -q "verdict=ci_gate_unmeasured" "$LOG"

# A RED VERDICT IS RE-ASKED (2026-10-16). A re-run can turn a red commit green;
# the cache used to keep "red" forever and held a green commit back. This path
# runs the real cache with only the GitHub call replaced.
CACHE="$TMP/ci-cache"; FETCHED="$TMP/fetched"; : > "$FETCHED"
printf '#!/usr/bin/env bash\nv=$(awk -v s="$1" '"'"'$1==s {print $2}'"'"' "%s" | tail -1); echo "${v:-green}"\n' "$FETCHED" > "$TMP/fetch"
chmod +x "$TMP/fetch"
run_cached() {
  AMUX_AUTHORITY_REPO="$STALE" AMUX_AUTHORITY_WORKTREE="$WORK" AMUX_AUTHORITY_MARKER="$MARKER" \
  AMUX_AUTHORITY_LOG="$LOG" AMUX_RS_ACTIVATION_REF=origin/main AMUX_CI_VERDICT_CACHE="$CACHE" \
  AMUX_CI_FETCH_CMD="$TMP/fetch" AMUX_RS_HEALTH_URL="file://$TMP/health.json" "$@" bash "$LAUNCHER"
}
RERUN=$(commit_next rerun)
echo "{\"commit_full\":\"$ELECTED\"}" > "$TMP/health.json"
echo "$RERUN red" >> "$FETCHED"
: > "$TRACE"
run_cached env
expect test "$(cat "$CACHE/$RERUN")" = red
expect test "$(grep -c "sha=$RERUN" "$TRACE")" = 0
echo "$RERUN green" >> "$FETCHED"
# Inside the re-check window the cached red stands.
run_cached env AMUX_CI_RED_RECHECK_S=3600
expect test "$(grep -c "sha=$RERUN" "$TRACE")" = 0
# Past it, the re-run's green is seen, cached, and activated.
run_cached env AMUX_CI_RED_RECHECK_S=0
expect test "$(cat "$CACHE/$RERUN")" = green
expect grep -q "repo=$WORK sha=$RERUN" "$TRACE"

echo "test-build-activation-launcher: $PASS passed, $FAIL failed"
[ "$FAIL" -eq 0 ]
