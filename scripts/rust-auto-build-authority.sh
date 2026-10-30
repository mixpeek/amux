#!/usr/bin/env bash
# The launchd builder must not execute whichever revision happens to be checked
# out in the shared developer tree. A second checkout can be locally ahead (or
# simply stale) while still pointing its timer at this path; that was the ATE-93
# takeover loop. This launcher is the single normal activation authority: it
# runs the committed builder only from a clean detached worktree at origin/main.
set -euo pipefail

REPO="${AMUX_AUTHORITY_REPO:-}"
REF="${AMUX_RS_ACTIVATION_REF:-origin/main}"
WORK="${AMUX_AUTHORITY_WORKTREE:-$HOME/.amux/activation-source}"
MARKER="${AMUX_AUTHORITY_MARKER:-${WORK}.authority}"
LOG="${AMUX_AUTHORITY_LOG:-$HOME/.amux/logs/rust-auto-build.log}"
mkdir -p "$(dirname "$LOG")"

say() { echo "== !! ACTIVATION AUTHORITY $*" >> "$LOG"; }
refuse() { say "REFUSED — $*"; exit 0; }
# The gate's last verdict, as JSON, for readers outside this log (`amux
# requests` says why a landed fix is not live). Written by rename so a reader
# never sees half a file; a failed write costs the reader its explanation only.
GATE_STATE="${AMUX_ACTIVATION_GATE_STATE:-$HOME/.amux/activation-gate.json}"
# target_since is when the gate first named this target. It is carried over
# while the target is unchanged, so the server can judge the builder by how
# long a GREEN commit has waited for it, not by how long CI took (AF-1055).
gate_state() { # verdict tip tip_state target held
  local t now since prev
  t=$(mktemp "$GATE_STATE.XXXXXX" 2>/dev/null) || return 0
  now=$(date +%s); since=$now
  prev=$(cat "$GATE_STATE" 2>/dev/null || true)
  if [ -n "$4" ] && [ "$(printf '%s' "$prev" | sed -n 's/.*"target":"\([^"]*\)".*/\1/p')" = "$4" ]; then
    since=$(printf '%s' "$prev" | sed -n 's/.*"target_since":\([0-9][0-9]*\).*/\1/p')
    [ -n "$since" ] || since=$now
  fi
  printf '{"at":%s,"verdict":"%s","tip":"%s","tip_state":"%s","target":"%s","target_since":%s,"held":%s}\n' \
    "$now" "$1" "$2" "$3" "$4" "$since" "${5:-0}" > "$t" && mv -f "$t" "$GATE_STATE" || rm -f "$t"
}

# The deploy settings live in ~/.amux/server.env like every other amux
# setting, but launchd hands this job only its plist's environment, so a gate
# set there was read by nothing (2026-10-29: AMUX_CI_REQUIRED_WORKFLOWS=rust sat
# in server.env while the gate ran its default). Read the deploy keys from it,
# never overriding one the environment already sets, and name what was read.
ENV_FILE="${AMUX_SERVER_ENV:-$HOME/.amux/server.env}"
if [ -r "$ENV_FILE" ]; then
  read_keys=""
  while IFS='=' read -r k v; do
    case "$k" in
      AMUX_CI_REQUIRED_WORKFLOWS|AMUX_DEPLOY_CI_GATE|AMUX_DEPLOY_PREBUILD|AMUX_ACTIVATION_ROLLBACK|AMUX_ACTIVATION_HEALTH_S) ;;
      *) continue ;;
    esac
    [ -z "${!k+x}" ] || continue
    v="${v%\"}"; v="${v#\"}"; v="${v%\'}"; v="${v#\'}"
    export "$k=$v"
    read_keys="$read_keys $k"
  done < <(grep -E '^[A-Z_]+=' "$ENV_FILE" 2>/dev/null || true)
  [ -z "$read_keys" ] || [ "$(cat "${GATE_STATE}.env-keys" 2>/dev/null)" = "$read_keys" ] \
    || { say "deploy settings read from $ENV_FILE:$read_keys verdict=authority_env_read"; echo "$read_keys" > "${GATE_STATE}.env-keys" 2>/dev/null || true; }
fi

[ -n "$REPO" ] || refuse "AMUX_AUTHORITY_REPO is unset; no checkout may choose the live image"
tip=$(git -C "$REPO" rev-parse --verify -q "${REF}^{commit}" 2>/dev/null) \
  || refuse "cannot resolve $REF in $REPO; refusing an unmeasured activation"

# ONLY CI-GREEN COMMITS GO LIVE (Ethan, 2026-10-15: "make sure all the amux
# shit goes thru cicd"). Until then this activated $REF's tip about a minute
# after any push, while its CI was still running or already red. Now the target
# is the newest first-parent commit of $REF whose required push workflows
# (AMUX_CI_REQUIRED_WORKFLOWS, default checks) all completed with success.
# The default was rust,checks until 2026-10-29: rust runs the whole e2e suite,
# about 50 minutes, and each push cancelled the queued run, so on a busy main
# nothing went green for hours while fixes sat undeployed. The owner chose the
# fast gate ("ok do that", best practice: a fast required gate, the slow suite
# after deploy); scripts/main-ci-watch.sh (SCHED-713) still cards a red rust
# run, and a deploy that does not come up healthy is rolled back
# (AMUX_ACTIVATION_ROLLBACK in rust-auto-build.sh, on by default since then). A tip that is pending or red leaves the last green image
# running; a commit that never had CI (an inner commit of a multi-commit push)
# is passed over. Verdicts come from GitHub check runs through the App token;
# final ones are cached per sha so a 60s timer costs about one API call while a
# run is in flight. Opt out with AMUX_DEPLOY_CI_GATE=0.
CI_CACHE="${AMUX_CI_VERDICT_CACHE:-$HOME/.amux/ci-verdicts-v2}"  # v2: workflow-run verdicts; v1 judged check runs
# One cache per set of required workflows: a verdict is about that set, and
# with one shared cache a commit judged green under `checks` stayed green after
# the gate went back to `rust,checks` (found 2026-10-29, changing the gate).
CI_CACHE="$CI_CACHE/$(printf %s "${AMUX_CI_REQUIRED_WORKFLOWS:-checks}" | tr -c 'A-Za-z0-9' '_')"
# How far back the walk looks for a green commit. It was 40, and on 2026-10-24
# main had 56 commits after its newest green one: the gate refused every tick
# (ci_gate_no_green), the builder never ran, and a hand-installed build of a
# 2026-10-07 fork stayed live for hours. Verdicts are cached per sha, so a deep
# walk costs API calls once. A green found deeper than CI_DEEP_WARN is still
# activated and logged as ci_gate_green_deep: main has been red that long.
CI_DEPTH="${AMUX_CI_GATE_DEPTH:-300}"
CI_DEEP_WARN="${AMUX_CI_GATE_DEEP_WARN:-40}"
# The push workflow runs for one sha, as GitHub's JSON. AMUX_CI_FETCH_CMD
# stands in for the network in tests; everything after it (the judging below
# and the verdict cache) is the shipped path.
ci_runs_json() {
  local sha="$1" token
  if [ -n "${AMUX_CI_FETCH_CMD:-}" ]; then "$AMUX_CI_FETCH_CMD" "$sha"; return; fi
  token=$(GH_TOKEN= "$HOME/.amux/github-app/get-token.sh" --min-remaining 300 2>/dev/null \
    | sed -n 's/^export GH_TOKEN=//p' | tr -d "'\"") || true
  local url="https://api.github.com/repos/${AMUX_CI_REPO:-mixpeek/amux}/actions/runs?head_sha=$sha&event=push&per_page=100"
  if [ -z "$token" ]; then
    # No App token (an amux Cloud box has none): a public repo's runs read without
    # one, 60 requests an hour per address. Without this the sandbox logged
    # ci_gate_unmeasured on every pass and ran a build hours behind a green main
    # (2026-10-26). AMUX_CI_ANON=0 keeps the old refusal.
    [ "${AMUX_CI_ANON:-1}" = 1 ] || return 1
    echo "ci gate: no App token, reading $sha's runs unauthenticated verdict=ci_gate_anonymous_read" >> "$LOG"
    curl -sf --max-time 15 -H "Accept: application/vnd.github+json" "$url"
    return
  fi
  curl -sf --max-time 15 -H "Authorization: Bearer $token" -H "Accept: application/vnd.github+json" "$url"
}
# Judge by WORKFLOW RUNS, not check runs (2026-10-15): 6325024c7 went live
# with its rust run cancelled by the next push before any job started, so it
# had no failing check run and "every check run passed" was true of the
# checks workflow alone. Each required workflow must have completed with
# success; a cancelled or skipped one leaves the commit without a verdict
# (none), so the walk passes it over to an older, fully tested commit.
# Kept at top level: the Python below is indentation-sensitive, and indenting
# it inside a block made every verdict "unmeasured" (2026-10-16, 162e78e03).
ci_judge_runs() {
  AMUX_CI_REQUIRED="${AMUX_CI_REQUIRED_WORKFLOWS:-checks}" python3 -c '
import json,os,sys
runs=json.load(sys.stdin)["workflow_runs"]
need=[w.strip() for w in os.environ["AMUX_CI_REQUIRED"].split(",") if w.strip()]
if not runs: print("none"); raise SystemExit
states=[]
for w in need:
    mine=sorted([r for r in runs if r["name"]==w], key=lambda r: r["created_at"])
    if not mine: states.append("none"); continue
    r=mine[-1]
    if r["status"]!="completed": states.append("pending")
    elif r["conclusion"]=="success": states.append("green")
    elif r["conclusion"] in ("cancelled","skipped","stale"): states.append("none")
    else: states.append("red")
for v in ("red","pending","none"):
    if v in states: print(v); raise SystemExit
# Green only on a re-run means a flaky test let it through: say so where a
# sweep looks (the builder log), once per commit, since green is cached.
again=[w+" attempt "+str(r.get("run_attempt",1)) for w in need for r in [sorted([x for x in runs if x["name"]==w], key=lambda x: x["created_at"])[-1]] if r.get("run_attempt",1)>1]
if again: print("WARN CI-GATE "+runs[0].get("head_sha","?")[:9]+" green only after a re-run ("+", ".join(again)+") verdict=ci_green_after_rerun", file=sys.stderr)
print("green")'
}
ci_verdict() {   # green | red | pending | none | unmeasured
  local sha="$1" cached
  if [ -n "${AMUX_CI_VERDICT_CMD:-}" ]; then "$AMUX_CI_VERDICT_CMD" "$sha"; return; fi
  cached=$(cat "$CI_CACHE/$sha" 2>/dev/null || true)
  case "$cached" in
    green) echo "$cached"; return ;;
    red|none)
      # A re-run can turn red green (2026-10-16: 3067a22da's rust run passed on
      # attempt 2 at 11:36, the cache had said red since 11:13, and the gate
      # held a server fix back for an hour while the fleet ran out of quota).
      # A cancelled run can be re-run too: on 2026-10-24 f83afc792's checks run
      # was cancelled, its re-run was never read because "none" was cached as
      # final, and the Mac stayed 79 commits behind main.
      # Ask again every AMUX_CI_RED_RECHECK_S until the commit is a day old.
      local cache_age commit_age
      cache_age=$(python3 -c 'import os,sys,time; print(int(time.time()-os.path.getmtime(sys.argv[1])))' "$CI_CACHE/$sha" 2>/dev/null || echo 0)
      commit_age=$(( $(date +%s) - $(git -C "$REPO" log -1 --format=%ct "$sha") ))
      if [ "$cache_age" -lt "${AMUX_CI_RED_RECHECK_S:-600}" ] || [ "$commit_age" -gt 86400 ]; then
        echo "$cached"; return
      fi ;;
  esac
  local token v
  v=$(ci_runs_json "$sha" | ci_judge_runs 2>>"$LOG") || v=unmeasured
  # "none" is only final once the commit is old enough that CI would have started.
  if [ "$v" = green ] || [ "$v" = red ] || { [ "$v" = none ] && \
     [ $(( $(date +%s) - $(git -C "$REPO" log -1 --format=%ct "$sha") )) -gt 3600 ]; }; then
    mkdir -p "$CI_CACHE" && echo "$v" > "$CI_CACHE/$sha"
  fi
  echo "$v"
}

# A gate that cannot measure cannot vouch for its own replacement either: the
# installed launcher is refreshed only after an activation, so a launcher whose
# CI judge broke could never be replaced by its fix (2026-10-16: 162e78e03
# indented the judge's Python, every pass logged ci_gate_unmeasured for about
# 2.5h, and the fix in 29fa4caf9 was installed by hand). When unmeasured, adopt
# $REF's launcher if it differs from this file, parses, and passes the launcher
# test from that same commit, run here with self-heal off so it cannot recurse.
# One attempt per tip, so an unhealable state does not rerun the test every pass.
self_heal_launcher() {
  local self="${AMUX_AUTHORITY_SELF:-$0}" tried tmp tdir
  [ "${AMUX_AUTHORITY_SELF_HEAL:-1}" = 1 ] || return 0
  [ -f "$self" ] && [ ! -L "$self" ] && [ -w "$(dirname "$self")" ] || return 0
  tried="${self}.heal-tried"
  [ "$(cat "$tried" 2>/dev/null)" = "$tip" ] && return 0
  tmp=$(mktemp "$(dirname "$self")/.authority-heal.XXXXXX") || return 0
  if ! git -C "$REPO" show "$tip:scripts/rust-auto-build-authority.sh" > "$tmp" 2>/dev/null || ! bash -n "$tmp" 2>/dev/null \
     || cmp -s "$tmp" "$self"; then
    rm -f "$tmp"; return 0
  fi
  echo "$tip" > "$tried" 2>/dev/null || true
  tdir=$(mktemp -d)
  if git -C "$REPO" archive "$tip" scripts | tar -x -C "$tdir" 2>/dev/null \
     && (cd "$tdir" && AMUX_AUTHORITY_SELF_HEAL=0 ${AMUX_AUTHORITY_HEAL_TEST_CMD:-bash scripts/test-build-activation-launcher.sh} >/dev/null 2>&1) \
     && chmod 0755 "$tmp" && mv -f "$tmp" "$self"; then
    say "LAUNCHER SELF-HEALED from $tip: the gate could not measure CI, and that commit's launcher passed its own test; the next pass runs it verdict=authority_launcher_self_healed"
  else
    rm -f "$tmp"
    say "LAUNCHER SELF-HEAL REFUSED for $tip: its launcher did not pass its own test, kept this one verdict=authority_launcher_self_heal_refused"
  fi
  rm -rf "$tdir"
}

target="$tip"
if [ "${AMUX_DEPLOY_CI_GATE:-1}" != "0" ]; then
  target=""; tip_state=""; passed=0
  for sha in $(git -C "$REPO" rev-list --first-parent -n "$CI_DEPTH" "$tip"); do
    v=$(ci_verdict "$sha")
    [ -n "$tip_state" ] || tip_state="$v"
    if [ "$v" = green ]; then target="$sha"; break; fi
    if [ "$v" = unmeasured ]; then
      gate_state ci_gate_unmeasured "$tip" unmeasured "" "$passed"
      self_heal_launcher
      refuse "CI-GATE unmeasured for $sha (no App token and no anonymous read, GitHub unreachable or rate-limited); keeping the live image verdict=ci_gate_unmeasured"
    fi
    passed=$((passed + 1))
  done
  if [ -n "$target" ] && [ "$passed" -ge "$CI_DEEP_WARN" ]; then
    say "WARN CI-GATE newest green $target is $passed first-parent commits behind $REF tip $tip; main has been red or pending that long verdict=ci_gate_green_deep"
  fi
  [ -n "$target" ] || gate_state ci_gate_no_green "$tip" "$tip_state" "" "$passed"
  [ -n "$target" ] || refuse "CI-GATE no green commit in the last $CI_DEPTH of $REF (tip $tip is $tip_state); keeping the live image verdict=ci_gate_no_green"
  if [ "$target" != "$tip" ]; then
    # NEVER ROLL BACK. If the live image already contains the newest green
    # commit, holding it is the gate's whole effect: activating the older green
    # commit would run an old binary over a newer schema (migrations only go
    # forward) and undo work nobody asked to undo.
    live=$(curl -sk --max-time 4 "${AMUX_RS_HEALTH_URL:-https://localhost:${AMUX_RS_PORT:-8824}/api/health}" 2>/dev/null \
      | python3 -c 'import json,sys; d=json.load(sys.stdin); print((d.get("commit_full") or d.get("commit") or "").replace("-dirty",""))' 2>/dev/null || true)
    if [ -n "$live" ] && live_full=$(git -C "$REPO" rev-parse --verify -q "${live}^{commit}" 2>/dev/null) \
       && [ "$live_full" != "$target" ] && git -C "$REPO" merge-base --is-ancestor "$target" "$live_full"; then
      gate_state ci_gate_no_rollback "$tip" "$tip_state" "$target" "$passed"
      refuse "CI-GATE tip $tip is $tip_state; newest green $target is older than the live image $live_full, holding it rather than rolling back verdict=ci_gate_no_rollback"
    fi
    gate_state ci_gate_held_back "$tip" "$tip_state" "$target" "$passed"
    say "CI-GATE tip $tip is $tip_state; activating newest green $target ($passed newer commit(s) not yet green) verdict=ci_gate_held_back"
  else
    gate_state ci_gate_clear "$tip" green "$tip" 0
  fi
fi

if [ -e "$WORK" ]; then
  [ -f "$MARKER" ] || refuse "$WORK exists without this launcher's marker"
  [ "$(cat "$MARKER")" = "$REPO" ] || refuse "$WORK marker names a different source repository"
  git -C "$WORK" rev-parse --is-inside-work-tree >/dev/null 2>&1 \
    || refuse "$WORK is not a Git worktree"
  [ -z "$(git -C "$WORK" status --porcelain --untracked-files=all)" ] \
    || refuse "$WORK is dirty; preserving it instead of activating unknown bytes"
  git -C "$WORK" checkout --detach -q "$target" \
    || refuse "could not advance clean authority worktree to $target"
else
  mkdir -p "$(dirname "$WORK")"
  git -C "$REPO" worktree add --detach "$WORK" "$target" >/dev/null \
    || refuse "could not create authority worktree at $WORK"
  printf '%s\n' "$REPO" > "$MARKER"
fi

say "BUILD $target from detached $REF worktree=$WORK (source checkout=$REPO)"
# The builder's own authority check compares against this ref, so hand it the
# ELECTED sha: passing $REF made it refuse every held-back green commit.
rc=0
AMUX_REPO="$WORK" AMUX_RS_ACTIVATION_REF="$target" "$WORK/scripts/rust-auto-build.sh" || rc=$?

# BUILD WHILE CI RUNS (owner, 2026-10-29: "we need to speed up how this
# works"). With the tip still pending, compile it now in the builder's stable
# directory, so when it goes green its activation finds nothing to compile.
# From a copy of the target's script, in the background: this worktree moves
# on the next pass, and bash reads a running script by offset.
# AMUX_DEPLOY_PREBUILD=0 turns it off.
if [ "${AMUX_DEPLOY_PREBUILD:-1}" = 1 ] && [ "$target" != "$tip" ] && [ "${tip_state:-}" = pending ] \
   && [ -f "$WORK/scripts/rust-prebuild.sh" ] && [ "$(cat "$HOME/.amux/prebuild-done" 2>/dev/null)" != "$tip" ] \
   && ! { [ "$(cat "$HOME/.amux/prebuild-done.failed" 2>/dev/null)" = "$tip" ] \
          && [ -n "$(find "$HOME/.amux/prebuild-done.failed" -mmin -10 2>/dev/null)" ]; } \
   && ! git -C "$REPO" diff --quiet "$target" "$tip" -- crates Cargo.toml Cargo.lock 2>/dev/null; then
  pre=$(mktemp "${TMPDIR:-/tmp}/amux-prebuild-sh.XXXXXX")
  cp "$WORK/scripts/rust-prebuild.sh" "$pre"
  ( nohup bash "$pre" "$REPO" "$tip" >/dev/null 2>&1; rm -f "$pre" ) &
  disown 2>/dev/null || true
  say "PREBUILD tip $tip while its CI runs (pid $!) verdict=prebuild_launched"
fi
exit "$rc"
