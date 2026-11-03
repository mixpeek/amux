#!/usr/bin/env bash
# local-deploy.sh — THIS BOX's deploy path: build the amux server from a local
# ref, install it, and prove it took.
#
# Deliberately different from the general rule in CLAUDE.md ("Only main
# deploys", "never copy a build over the binary"). Owner, 2026-10-30: "I want
# deployment to depend on manual build here. mixpeek's build process is his
# own." So on this box:
#
#   - deployment is a manual build from a branch, and
#   - upstream's (mixpeek's) build/deploy pipeline is not driven from here.
#
# The reasons it cannot be accidental: this checkout has no working auto-deploy
# (the CI gate asks mixpeek for a fork commit's verdicts, which never exist
# there, and amux-builder.timer is masked), and a stale auto-deploy would
# replace a manual build with something nobody chose.
#
# What it does, in order:
#   1. refuses to run while the auto-builder could also deploy (one mechanism
#      at a time — two installers racing is how a fork's binary ends up live)
#   2. builds `--profile deploy` from --ref into ~/.amux/rust-build-target
#   3. installs atomically (`.new` + rename), keeping the previous binary
#   4. restarts amux-server and waits for /health
#   5. verifies /health's `commit` equals the built commit AND
#      /proc/<pid>/exe's md5 equals the installed file — a healthy process
#      running the WRONG binary is the failure the md5 check exists for
#   6. on any failure, puts the previous binary back and restarts it
#
# Usage:
#   scripts/local-deploy.sh [--ref REF] [--skip-build] [--dry-run]
#
#   --ref REF      build this ref (default: HEAD). A branch, tag or sha.
#   --skip-build   install the artifact already at the build path (re-activate,
#                  or re-test the install/verify path without a 30-minute build)
#   --dry-run      print the plan and every check's result, touch nothing
#
# Verdict lines are machine-greppable: "local_deploy ok|rollback|failed".

set -euo pipefail

REPO="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$REPO"

# cargo is not on a plain `ssh host cmd` or cron PATH, and a deploy tool that
# only works in an interactive shell is a trap: the first real invocation from
# anywhere else fails with "cargo is not on PATH" AFTER the caller believed the
# deploy had started (measured 2026-10-30, the first run of this script).
if ! command -v cargo >/dev/null 2>&1; then
  for cand in "$HOME/.cargo/bin" "/usr/local/cargo/bin"; do
    if [ -x "$cand/cargo" ]; then PATH="$cand:$PATH"; export PATH; break; fi
  done
fi

TARGET_DIR="${CARGO_TARGET_DIR:-$HOME/.amux/rust-build-target}"
BUILT="$TARGET_DIR/deploy/amux-server"
INSTALL="${AMUX_RS_INSTALL:-$HOME/.local/bin/amux-server-rs}"
ENDPOINT_JSON="$HOME/.amux/endpoint.json"
HEALTH_TIMEOUT_S="${AMUX_LOCAL_DEPLOY_HEALTH_S:-180}"

REF="HEAD"
SKIP_BUILD=0
DRY_RUN=0
while [ $# -gt 0 ]; do
  case "$1" in
    --ref)        REF="${2:?--ref needs a value}"; shift 2 ;;
    --skip-build) SKIP_BUILD=1; shift ;;
    --dry-run)    DRY_RUN=1; shift ;;
    -h|--help)    sed -n '2,40p' "$0" | sed 's/^# \{0,1\}//'; exit 0 ;;
    *)            echo "local-deploy: unknown argument '$1'" >&2; exit 2 ;;
  esac
done

say() { printf '== %s\n' "$*"; }
die() { printf 'local_deploy failed: %s\n' "$*" >&2; exit 1; }
verdict() { printf 'local_deploy %s\n' "$*"; }

# --- 1. one deploy mechanism at a time -----------------------------------
# BOTH states matter: active means it is racing us now, enabled means it would
# race us on the next tick or after a reboot — which is exactly how a manual
# build gets silently replaced a day later.
timer_state="$(systemctl --user is-active amux-builder.timer 2>/dev/null || true)"
timer_enabled="$(systemctl --user is-enabled amux-builder.timer 2>/dev/null || true)"
say "auto-builder timer: active=${timer_state:-unknown} enabled=${timer_enabled:-unknown}"
if [ "$timer_state" = "active" ] || [ "$timer_enabled" = "enabled" ]; then
  die "amux-builder.timer is ACTIVE; it would race this manual deploy.
       Deployment here is manual by owner instruction (2026-10-30). Stop it first:
         systemctl --user stop amux-builder.timer
       and keep it masked: systemctl --user mask amux-builder.timer"
fi

# --- 2. what are we deploying? ------------------------------------------
COMMIT="$(git rev-parse "$REF")"
SHORT="$(git rev-parse --short "$REF")"
say "ref=$REF commit=$SHORT branch=$(git rev-parse --abbrev-ref HEAD)"
if [ -n "$(git status --porcelain --untracked-files=no)" ]; then
  die "working tree has uncommitted changes to tracked files — the binary would
       carry code that is in no commit, and /health's commit could not identify it.
       Commit first, or stash."
fi

PORT="$(python3 - "$ENDPOINT_JSON" <<'PY' 2>/dev/null || echo 8824
import json,sys
print(json.load(open(sys.argv[1]))["canonical_port"])
PY
)"
URL="https://localhost:$PORT"
say "install=$INSTALL url=$URL"

health_json() { curl -sk -m 4 "$URL/health" 2>/dev/null; }
field() { python3 -c 'import json,sys;print(json.load(sys.stdin).get(sys.argv[1],""))' "$1" 2>/dev/null; }

if [ "$DRY_RUN" = "1" ]; then
  say "DRY RUN — would:"
  say "  cargo build --profile deploy -p amux-server   (ref $SHORT)"
  say "  install -m 0755 from $BUILT to $INSTALL.new, then rename"
  say "  systemctl --user restart amux-server, wait <=${HEALTH_TIMEOUT_S}s for health"
  say "  verify health.commit == $SHORT and /proc/<pid>/exe md5 == installed md5"
  say "current server: $(health_json | field commit)"
  verdict "ok action=dry_run commit=$SHORT"
  exit 0
fi

# --- 3. build -------------------------------------------------------------
if [ "$SKIP_BUILD" = "0" ]; then
  command -v cargo >/dev/null || die "cargo is not on PATH"
  build_start="$(date +%s)"
  say "building --profile deploy from $SHORT (this is the long part)"

  # One build, with the incremental setting chosen by the caller. Output is kept
  # so a LINK failure can be recognised rather than merely reported.
  build_once() {
    nice -n 5 env \
      CARGO_TARGET_DIR="$TARGET_DIR" \
      CARGO_INCREMENTAL="$1" \
      CARGO_BUILD_JOBS="${CARGO_BUILD_JOBS:-1}" \
      "$REPO/scripts/safe-cargo.sh" build --profile deploy -p amux-server
  }
  build_log="$(mktemp -t local-deploy-build.XXXXXX)"
  if ! build_once "${CARGO_INCREMENTAL:-1}" > "$build_log" 2>&1; then
    cat "$build_log"
    # AF-1012: a stale incremental cache fails the LINK, not the compile —
    # `rust-lld: error: undefined hidden symbol: … (.llvm.NNN)` for code that
    # compiles fine. rust-auto-build.sh's remedy is to clear the incremental
    # trees; measured 2026-11-03 that is NOT enough, because the already-linked
    # rlib in deps/ stays poisoned and cargo reuses it. One retry with
    # CARGO_INCREMENTAL=0 changes the flags, forces a clean recompile and avoids
    # the whole class. Without this the operator gets a link error and no path.
    if grep -qE 'undefined hidden symbol|undefined reference to|symbol\(s\) not found' "$build_log"; then
      say "link failed on stale incremental symbols (AF-1012); retrying ONCE with CARGO_INCREMENTAL=0"
      tracing_verdict="local_deploy_incremental_link_retry"
      if ! build_once 0 > "$build_log" 2>&1; then
        cat "$build_log"
        die "cargo build failed twice (second attempt without incremental) — nothing installed, the running server is untouched"
      fi
      say "$tracing_verdict ok"
    else
      die "cargo build failed — nothing installed, the running server is untouched"
    fi
  fi
  rm -f "$build_log"
  # A stale artifact from an earlier successful build sits at this same path and
  # would pass every later `[ -x ]` check after a FAILED build. Compare mtime to
  # the moment we started instead of trusting the path.
  [ -f "$BUILT" ] || die "build reported success but $BUILT does not exist"
  built_mtime="$(stat -c %Y "$BUILT" 2>/dev/null || echo 0)"
  [ "$built_mtime" -ge "$build_start" ] || die "artifact at $BUILT is older than this build (mtime $built_mtime < $build_start) — refusing to install a stale binary"
else
  [ -f "$BUILT" ] || die "--skip-build: no artifact at $BUILT"
  say "skipping the build; installing the existing artifact"
fi

built_md5="$(md5sum "$BUILT" | cut -d' ' -f1)"
say "built md5=$built_md5"

# --- 4. install atomically, keeping the previous binary -------------------
PREV="$INSTALL.prev"
had_prev=0
if [ -f "$INSTALL" ]; then
  cp -p "$INSTALL" "$PREV"
  had_prev=1
  say "previous binary saved for rollback: $PREV"
fi
install -m 0755 "$BUILT" "$INSTALL.new"
mv -f "$INSTALL.new" "$INSTALL"
say "installed $INSTALL"

# --- 5. restart + verify --------------------------------------------------
restart_and_wait() {
  systemctl --user restart amux-server || return 1
  local waited=0
  while [ "$waited" -lt "$HEALTH_TIMEOUT_S" ]; do
    if [ -n "$(health_json)" ]; then return 0; fi
    sleep 3; waited=$((waited + 3))
  done
  return 1
}

verify() {
  local h commit build pid exe_md5
  h="$(health_json)"
  [ -n "$h" ] || { say "health: no response"; return 1; }
  commit="$(printf '%s' "$h" | field commit)"
  build="$(printf '%s' "$h" | field build)"
  pid="$(printf '%s' "$h" | field pid)"
  say "health commit=$commit build=$build pid=$pid"
  # The commit check applies only when THIS run built the artifact: then health
  # proving it runs $SHORT proves the build really was $REF (and not a stale
  # artifact that survived a failed build). With --skip-build the artifact was
  # built earlier, possibly from a different commit — a script-only commit moves
  # HEAD without changing the binary — so the md5 check is the honest one and a
  # commit mismatch is reported, not failed.
  if [ "$SKIP_BUILD" = "0" ]; then
    [ "${commit:0:${#SHORT}}" = "$SHORT" ] || {
      say "health reports commit '$commit', expected '$SHORT'"; return 1; }
  elif [ "${commit:0:${#SHORT}}" != "$SHORT" ]; then
    say "note: live commit '$commit' differs from HEAD '$SHORT' — expected with --skip-build"
  fi
  [ -n "$pid" ] || { say "health carried no pid"; return 1; }
  exe_md5="$(md5sum "/proc/$pid/exe" 2>/dev/null | cut -d' ' -f1)"
  say "running exe md5=$exe_md5 installed md5=$built_md5"
  [ "$exe_md5" = "$built_md5" ] || {
    say "the healthy process is running a DIFFERENT binary than the one installed"; return 1; }
  return 0
}

if restart_and_wait && verify; then
  # Report what the SERVER says it is running, not what we asked for: in
  # --skip-build mode those differ, and a verdict line that names the wrong one
  # is the kind of log that sends the next reader after the wrong commit.
  live_commit="$(health_json | field commit)"
  live_pid="$(health_json | field pid)"
  say "verified: live commit=$live_commit md5=$built_md5"
  if [ "${live_commit:0:${#SHORT}}" = "$SHORT" ]; then
    verdict "ok commit=$live_commit md5=$built_md5 pid=$live_pid"
  else
    verdict "ok commit=$live_commit built_from=$SHORT md5=$built_md5 pid=$live_pid"
  fi
  exit 0
fi

# --- 6. roll back --------------------------------------------------------
say "activation did not verify — rolling back"
if [ "$had_prev" = "1" ]; then
  install -m 0755 "$PREV" "$INSTALL.new" && mv -f "$INSTALL.new" "$INSTALL"
  if restart_and_wait; then
    say "rolled back to the previous binary: $(health_json | field commit)"
    verdict "rollback commit=$SHORT (previous binary restored and healthy)"
  else
    verdict "rollback commit=$SHORT (previous binary restored but the server did NOT come back healthy — needs a human)"
    exit 1
  fi
else
  verdict "rollback commit=$SHORT (no previous binary existed to restore)"
fi
exit 1
