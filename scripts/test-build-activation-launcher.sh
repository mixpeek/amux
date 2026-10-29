#!/usr/bin/env bash
# The launchd authority launcher must choose origin/main's detached source even
# when its configured repository is currently on a stale local branch. This
# exercises the shipped launcher and tiny fake builders rather than copying its
# Git selection into a unit test.
set -euo pipefail

ROOT=$(cd "$(dirname "$0")/.." && pwd)
LAUNCHER="$ROOT/scripts/rust-auto-build-authority.sh"
# Never this machine's settings or a real prebuild: the launcher reads deploy
# keys from ~/.amux/server.env and may start a background compile.
export AMUX_SERVER_ENV=/dev/null AMUX_DEPLOY_PREBUILD=0
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
# Self-heal rewrites the launcher that is running; only the cells below that
# point it at a scratch copy may turn it on.
export AMUX_AUTHORITY_SELF_HEAL=0
# CI verdicts come from a file, one "sha verdict" per line; unlisted = green.
VERDICTS="$TMP/verdicts"; : > "$VERDICTS"
# Every cell writes its gate verdict here, never to the live ~/.amux copy.
export AMUX_ACTIVATION_GATE_STATE="$TMP/gate.json"
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
# The verdict also reaches readers outside the log (amux requests).
expect grep -q "\"verdict\":\"ci_gate_held_back\",\"tip\":\"$PENDING\",\"tip_state\":\"pending\",\"target\":\"$GREEN\",\"held\":1}" "$AMUX_ACTIVATION_GATE_STATE"
# The builder must be told the ELECTED sha, or its own authority check refuses it.
expect grep -q "sha=$GREEN authority=$GREEN" "$TRACE"
# Once the tip goes green it is activated.
echo "$PENDING green" >> "$VERDICTS"
: > "$TRACE"
run_launcher
expect grep -q "repo=$WORK sha=$PENDING" "$TRACE"
expect grep -q "\"verdict\":\"ci_gate_clear\",\"tip\":\"$PENDING\"" "$AMUX_ACTIVATION_GATE_STATE"
# A red tip whose newest green is older than live: hold, never roll back.
RED=$(commit_next red)
echo "$RED red" >> "$VERDICTS"
echo "{\"commit_full\":\"$RED\"}" > "$TMP/health.json"
: > "$TRACE"
run_launcher
expect test ! -s "$TRACE"
expect grep -q "newest green $PENDING is older than the live image $RED.*verdict=ci_gate_no_rollback" "$LOG"
expect grep -q "\"verdict\":\"ci_gate_no_rollback\",\"tip\":\"$RED\",\"tip_state\":\"red\"" "$AMUX_ACTIVATION_GATE_STATE"
# A green commit buried under more than 40 red ones is still found and activated
# (2026-10-24: 56 red commits after the newest green held every deploy, and a
# hand build of a fork stayed live). Logged as ci_gate_green_deep.
DEEP_GREEN=$PENDING
for i in $(seq 1 45); do echo "$(commit_next "deep$i") red" >> "$VERDICTS"; done
echo "{\"commit_full\":\"$ELECTED\"}" > "$TMP/health.json"
: > "$TRACE"; : > "$LOG"
run_launcher
expect grep -q "repo=$WORK sha=$DEEP_GREEN" "$TRACE"
expect grep -q "newest green $DEEP_GREEN is 46 first-parent commits behind.*verdict=ci_gate_green_deep" "$LOG"
: > "$TRACE"
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
# The stub answers in GitHub's workflow-runs shape, so the shipped parser runs.
cat > "$TMP/fetch" <<EOF
#!/usr/bin/env bash
v=\$(awk -v s="\$1" '\$1==s {print \$2}' "$FETCHED" | tail -1)
case "\$v" in red) c=failure ;; *) c=success ;; esac
case "\$v" in green2) a=2 ;; *) a=1 ;; esac
printf '{"workflow_runs":[{"name":"rust","status":"completed","conclusion":"%s","created_at":"1","run_attempt":%s,"head_sha":"%s"},{"name":"checks","status":"completed","conclusion":"success","created_at":"1","run_attempt":1}]}' "\$c" "\$a" "\$1"
EOF
chmod +x "$TMP/fetch"
# rust,checks unless REQUIRED=unset, which leaves the setting to the default
# and to AMUX_SERVER_ENV.
run_cached() {
  local req=(AMUX_CI_REQUIRED_WORKFLOWS=rust,checks)
  [ "${REQUIRED:-}" = unset ] && req=()
  env -u AMUX_CI_REQUIRED_WORKFLOWS ${req[@]+"${req[@]}"} \
  AMUX_AUTHORITY_REPO="$STALE" AMUX_AUTHORITY_WORKTREE="$WORK" AMUX_AUTHORITY_MARKER="$MARKER" \
  AMUX_AUTHORITY_LOG="$LOG" AMUX_RS_ACTIVATION_REF=origin/main AMUX_CI_VERDICT_CACHE="$CACHE" \
  AMUX_CI_FETCH_CMD="$TMP/fetch" AMUX_RS_HEALTH_URL="file://$TMP/health.json" "$@" bash "$LAUNCHER"
}
RERUN=$(commit_next rerun)
echo "{\"commit_full\":\"$ELECTED\"}" > "$TMP/health.json"
echo "$RERUN red" >> "$FETCHED"
: > "$TRACE"
run_cached env
expect test "$(cat "$CACHE/rust_checks/$RERUN")" = red
expect test "$(grep -c "sha=$RERUN" "$TRACE")" = 0
echo "$RERUN green" >> "$FETCHED"
# Inside the re-check window the cached red stands.
run_cached env AMUX_CI_RED_RECHECK_S=3600
expect test "$(grep -c "sha=$RERUN" "$TRACE")" = 0
# Past it, the re-run's green is seen, cached, and activated.
run_cached env AMUX_CI_RED_RECHECK_S=0
expect test "$(cat "$CACHE/rust_checks/$RERUN")" = green
expect grep -q "repo=$WORK sha=$RERUN" "$TRACE"

# THE GATE'S DEFAULT IS checks (2026-10-29): a commit whose rust run failed but
# whose checks passed is elected when nothing sets the gate.
DEF=$(commit_next default-gate)
echo "$DEF red" >> "$FETCHED"
: > "$LOG"
REQUIRED=unset run_cached env
expect grep -q "BUILD $DEF" "$LOG"
# And a gate set in server.env is read (launchd hands this job none of it).
ENVF="$TMP/server.env"; printf 'OTHER=1\nAMUX_CI_REQUIRED_WORKFLOWS=rust,checks\n' > "$ENVF"
STRICT=$(commit_next strict-gate)
echo "$STRICT red" >> "$FETCHED"
: > "$LOG"
REQUIRED=unset run_cached env AMUX_SERVER_ENV="$ENVF" AMUX_ACTIVATION_GATE_STATE="$TMP/gate.json"
expect grep -q "verdict=authority_env_read" "$LOG"
expect grep -q "AMUX_CI_REQUIRED_WORKFLOWS" "$LOG"
expect test "$(grep -c "BUILD $STRICT" "$LOG")" = 0
# The environment wins over the file.
: > "$LOG"
REQUIRED=unset run_cached env AMUX_SERVER_ENV="$ENVF" AMUX_CI_REQUIRED_WORKFLOWS=checks
expect grep -q "BUILD $STRICT" "$LOG"

# A CANCELLED VERDICT IS RE-ASKED TOO (2026-10-24). f83afc792's checks run was
# cancelled, so the cache held "none" as final and never read its re-run; the
# Mac stayed 79 commits behind main. A cached none is re-asked like a red.
REVIVED=$(commit_next revived)
echo "{\"commit_full\":\"$RERUN\"}" > "$TMP/health.json"
mkdir -p "$CACHE/rust_checks"; echo none > "$CACHE/rust_checks/$REVIVED"
echo "$REVIVED green2" >> "$FETCHED"
: > "$TRACE"
run_cached env AMUX_CI_RED_RECHECK_S=3600
expect test "$(cat "$CACHE/rust_checks/$REVIVED")" = none
expect test "$(grep -c "sha=$REVIVED" "$TRACE")" = 0
run_cached env AMUX_CI_RED_RECHECK_S=0
expect test "$(cat "$CACHE/rust_checks/$REVIVED")" = green
expect grep -q "repo=$WORK sha=$REVIVED" "$TRACE"
# It went green on a re-run, which the builder log says once, by commit.
expect grep -q "WARN CI-GATE ${REVIVED:0:9} green only after a re-run (rust attempt 2) verdict=ci_green_after_rerun" "$LOG"
expect test "$(grep -c "ci_green_after_rerun" "$LOG")" = 1

# A GATE THAT CANNOT MEASURE HEALS ITS OWN LAUNCHER (2026-10-16): 162e78e03
# broke the CI judge, every pass refused as unmeasured, and the fixed launcher
# could only arrive through an activation it was refusing. With CI unreadable,
# the elected ref's launcher replaces the running one when it passes its test.
SELF="$TMP/bin/amux-build-authority"; mkdir -p "$TMP/bin"
printf '#!/usr/bin/env bash\n# the broken installed launcher\n' > "$SELF"
( cd "$AUTH"; printf '#!/usr/bin/env bash\n# the fixed launcher\n' > scripts/rust-auto-build-authority.sh
  git add -A; git -c user.name=test -c user.email=test@example.com commit -qm fixed-launcher )
git -C "$AUTH" update-ref refs/remotes/origin/main "$(git -C "$AUTH" rev-parse HEAD)"
git -C "$STALE" fetch -q origin main
HEALTIP=$(git -C "$STALE" rev-parse origin/main)
echo "$HEALTIP unmeasured" >> "$VERDICTS"
heal_run() { AMUX_AUTHORITY_SELF_HEAL=1 AMUX_AUTHORITY_SELF="$SELF" AMUX_AUTHORITY_HEAL_TEST_CMD="$1" run_launcher; }
# Its test fails: the running launcher is kept and the refusal is logged.
heal_run false
expect grep -q "the broken installed launcher" "$SELF"
expect grep -q "verdict=authority_launcher_self_heal_refused" "$LOG"
# Same tip again: not retried (one attempt per tip).
: > "$LOG"; heal_run true
expect grep -q "the broken installed launcher" "$SELF"
expect test "$(grep -c 'SELF-HEAL' "$LOG")" = 0
# A new tip whose launcher passes: installed, logged, and the pass still refuses.
rm -f "$SELF.heal-tried"; : > "$LOG"; heal_run true
expect grep -q "the fixed launcher" "$SELF"
expect grep -q "verdict=authority_launcher_self_healed" "$LOG"
expect grep -q "verdict=ci_gate_unmeasured" "$LOG"
# Already identical: nothing to do.
: > "$LOG"; rm -f "$SELF.heal-tried"; heal_run true
expect test "$(grep -c 'SELF-HEAL' "$LOG")" = 0

echo "test-build-activation-launcher: $PASS passed, $FAIL failed"
[ "$FAIL" -eq 0 ]
