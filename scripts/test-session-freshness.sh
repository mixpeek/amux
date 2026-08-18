#!/usr/bin/env bash
# AEAB-18 — the SessionStart freshness hook must warn when THIS checkout has
# diverged, because an append to a shared append-only file here reaches nobody.
#
# The failure being guarded is silent and write-shaped. A merely-stale checkout
# announces itself the moment you pull. A DIVERGED one does not: appending to
# frustrations.md succeeds, prints nothing, and never reaches origin, because the
# hourly sync job refuses to fast-forward (correctly — it must not rewrite a
# shared tree). On 2026-08-17 four entries went in that way; that copy held 25
# entries while origin held 124, and it was noticed by chance days later.
#
# Every case builds REAL git repos and runs the SHIPPED hook as a subprocess, so
# this exercises the actual dispatch path rather than a paraphrase of its logic.
#
# The (a) and (c)/(d) cases are the load-bearing ones: a hook that warned
# unconditionally would satisfy (b) while being pure noise, and noise is how a
# banner gets ignored — which the hook's own header calls out as the reason it
# stays silent when everything is current.
#
# Exit 0 = all pass, 1 = a failure. Wired into .github/workflows/checks.yml.
set -uo pipefail
cd "$(dirname "$0")/.."
HOOK="$(pwd)/.claude/session-freshness.sh"
PASS=0; FAIL=0
TMP=$(mktemp -d); trap 'rm -rf "$TMP"' EXIT
export GIT_AUTHOR_NAME=t GIT_AUTHOR_EMAIL=t@t GIT_COMMITTER_NAME=t GIT_COMMITTER_EMAIL=t@t
# Pin the default branch instead of inheriting it. The first cut of this file did
# not, and passed locally (init.defaultBranch=main) while failing 4 assertions in
# CI, where git has no default set and falls back to `master`: the bare repo's
# HEAD was master, the pushes created main, and the clone ended up tracking
# nothing — so the hook found no upstream and printed NOTHING. The failure then
# read as "the hook is broken" when the harness was.
export GIT_CONFIG_COUNT=1 GIT_CONFIG_KEY_0=init.defaultBranch GIT_CONFIG_VALUE_0=main

# Build: a bare origin, a clone, and the hook installed inside the clone.
# `n_behind` commits land on origin after the clone; `n_ahead` land locally.
mk() { # $1 name  $2 n_ahead  $3 n_behind
  local d="$TMP/$1"
  git init -q --bare -b main "$d/origin.git"
  git clone -q "$d/origin.git" "$d/work" 2>/dev/null
  ( cd "$d/work"
    git checkout -q -B main
    mkdir -p .claude
    cp "$HOOK" .claude/session-freshness.sh
    echo seed > seed.txt; git add -A; git commit -qm seed
    git push -q -u origin main
  )
  if [ "$3" -gt 0 ]; then   # commits that exist ONLY on origin
    git clone -q "$d/origin.git" "$d/other" 2>/dev/null
    ( cd "$d/other"; git checkout -q -B main origin/main
      for i in $(seq 1 "$3"); do echo "up$i" > "up$i.txt"; git add -A; git commit -qm "up$i"; done
      git push -q origin main )
  fi
  if [ "$2" -gt 0 ]; then   # commits that exist ONLY here
    ( cd "$d/work"; for i in $(seq 1 "$2"); do echo "loc$i" > "loc$i.txt"; git add -A; git commit -qm "loc$i"; done )
  fi
  # HARNESS SELF-CHECK, before the hook is consulted at all. Without it a setup
  # bug is indistinguishable from a hook bug — which is exactly how the CI
  # failure above read. If the repo is not in the shape the case name claims,
  # say SETUP and fail loudly rather than blaming the thing under test.
  ( cd "$d/work"
    git fetch -q origin 2>/dev/null
    a=$(git rev-list --count origin/main..HEAD 2>/dev/null || echo x)
    b=$(git rev-list --count HEAD..origin/main 2>/dev/null || echo x)
    if [ "$a" != "$2" ] || [ "$b" != "$3" ]; then
      echo "SETUP BROKEN for '$1': wanted ahead=$2 behind=$3, got ahead=$a behind=$b"
      exit 0
    fi
    # ISOLATION. Point provenance at a path that does not exist, so these cases
    # test the git axes ONLY. Without this the hook reads the developer's real
    # ~/.amux/rust-build-provenance.json — whose sha a synthetic repo has never
    # heard of — and case (a) fails on this machine while passing in CI, where
    # no such file exists. A test whose verdict depends on the host's state is
    # not testing the hook.
    AMUX_RS_BUILD_PROVENANCE="$TMP/no-such-provenance.json" \
      bash .claude/session-freshness.sh 2>&1
  )
}

# A harness failure is not a test result. Surface it as its own failure so it can
# never be mistaken for the hook misbehaving.
setup_ok() { case "$2" in *"SETUP BROKEN"*) FAIL=$((FAIL+1)); echo "$2" | head -1; return 1;; esac; return 0; }
says()  { case "$2" in *"$1"*) PASS=$((PASS+1));; *) FAIL=$((FAIL+1)); echo "FAIL: expected output to mention '$1'"; echo "  got: ${2:-<empty>}";; esac; }
lacks() { case "$2" in *"$1"*) FAIL=$((FAIL+1)); echo "FAIL: output should NOT mention '$1'"; echo "  got: ${2:-<empty>}";; *) PASS=$((PASS+1));; esac; }

MARK="do NOT append to frustrations.md here"

# (a) CONTROL — current checkout: the hook must stay SILENT. Without this, a hook
#     that printed the warning unconditionally would pass every other case.
out=$(mk clean 0 0)
if [ -z "$(printf '%s' "$out" | tr -d '[:space:]')" ]; then PASS=$((PASS+1));
else FAIL=$((FAIL+1)); echo "FAIL: (a) a current checkout must produce NO output; got: $out"; fi

# (b) THE INCIDENT — diverged (unpushed AND behind): must warn, and must say why.
out=$(mk diverged 2 3)
says "DIVERGED" "$out"
says "$MARK" "$out"
says "2 unpushed" "$out"
lacks "reached nobody" "$out"   # the rationale belongs in the rule, not the banner

# (c) Behind ONLY — recoverable by a pull, so the strand warning must NOT fire.
#     It should still report the ordinary staleness line.
out=$(mk behind 0 3)
lacks "$MARK" "$out"
lacks "DIVERGED" "$out"
says "commit(s) behind" "$out"

# (d) Ahead ONLY — ordinary in-flight work; a push still reaches origin.
out=$(mk ahead 2 0)
lacks "$MARK" "$out"
lacks "DIVERGED" "$out"

# ---------------------------------------------------------------------------
# (e) AEAB-12 — the running server's BUILD PROVENANCE. The builder records what
#     it installed; the hook reports it. A session should not be able to be
#     looking at a fleet-wide deploy of somebody's scratch branch without being
#     told, which is what happened for 9h42m on 2026-08-17.
#
#     The on_main=yes and missing-file cases are the load-bearing ones: this line
#     is only worth having if it stays absent in the normal case.
# ---------------------------------------------------------------------------
PROVMARK="was built from an unmerged revision"
prov_run() { # $1 = file contents, or "" for no file
  local d="$TMP/prov"; rm -rf "$d"; mkdir -p "$d/.claude"
  cp "$HOOK" "$d/.claude/session-freshness.sh"
  ( cd "$d"; git init -q -b main .; echo x > f; git add -A; git commit -qm x ) >/dev/null 2>&1
  local pf="$d/prov.json"
  if [ -n "$1" ]; then printf '%s\n' "$1" > "$pf"; else rm -f "$pf"; fi
  ( cd "$d"; AMUX_RS_BUILD_PROVENANCE="$pf" bash .claude/session-freshness.sh 2>&1 )
}

out=$(prov_run '{"sha":"deadbeef1234","ref":"fix/my-thing","on_main":"no","built_at":"x"}')
says "$PROVMARK" "$out"
says "fix/my-thing" "$out"

out=$(prov_run '{"sha":"deadbeef1234","ref":"main","on_main":"yes","built_at":"x"}')
lacks "$PROVMARK" "$out"

out=$(prov_run "")                       # no file at all — fail open, stay silent
lacks "$PROVMARK" "$out"

out=$(prov_run 'not json at all')        # garbage — must not warn on a bad parse
lacks "$PROVMARK" "$out"

# ---------------------------------------------------------------------------
# (f) AEAB-32 — the RUNNING BUILD's lag behind origin/main. `on_main` cannot
#     express this: a build sitting on main while main moves on reads `yes`.
#     Measured cost of not saying it: a worker-panic fix merged 2026-08-13 22:15
#     did not reach the fleet until 09:11 the next morning, and 39 panics fired
#     in between.
#
#     The CONTROL (h) is the load-bearing case here. A probe wired to the wrong
#     ref would report a lag on every session, and a banner that always fires is
#     one nobody reads — which this hook's own header names as the reason it
#     stays silent when things are current.
# ---------------------------------------------------------------------------
BEHINDMARK="behind origin/main"
NEVERMARK="not a commit this checkout knows"

# Builds an origin+clone, then writes a provenance file naming a commit that is
# `$1` commits back from origin/main. Real repos and the shipped hook, so this
# cannot pass against a paraphrase of the logic.
lag_run() { # $1 = how many commits the BUILT sha is behind origin/main
  local d="$TMP/lag$1"; rm -rf "$d"
  git init -q --bare -b main "$d/origin.git"
  git clone -q "$d/origin.git" "$d/work" 2>/dev/null
  local built=""
  ( cd "$d/work"
    git checkout -q -B main
    mkdir -p .claude; cp "$HOOK" .claude/session-freshness.sh
    echo seed > seed.txt; git add -A; git commit -qm seed
    git push -q -u origin main ) >/dev/null 2>&1
  built=$(cd "$d/work" && git rev-parse HEAD)
  if [ "$1" -gt 0 ]; then
    ( cd "$d/work"
      for i in $(seq 1 "$1"); do echo "n$i" > "n$i.txt"; git add -A; git commit -qm "n$i"; done
      git push -q origin main ) >/dev/null 2>&1
  fi
  local pf="$d/prov.json"
  printf '{"sha":"%s","ref":"main","on_main":"yes","built_at":"x"}\n' "$built" > "$pf"
  # SELF-CHECK before consulting the hook: if the repo is not in the shape the
  # case claims, a setup bug would read as a hook bug.
  ( cd "$d/work"
    git fetch -q origin 2>/dev/null
    got=$(git rev-list --count "${built}..origin/main" 2>/dev/null || echo x)
    if [ "$got" != "$1" ]; then echo "SETUP BROKEN for lag$1: wanted behind=$1, got $got"; exit 0; fi
    AMUX_RS_BUILD_PROVENANCE="$pf" bash .claude/session-freshness.sh 2>&1 )
}

# (f) the running build is 3 commits stale — must say so, with the count.
out=$(lag_run 3)
if setup_ok "lag3" "$out"; then
  says "$BEHINDMARK" "$out"
  says "3 commit(s) behind" "$out"
  says "merging does NOT deploy" "$out"
  lacks "$PROVMARK" "$out"        # it IS on main; only the LAG is the complaint
fi

# (g) CONTROL — the running build IS origin/main. Silence, or the banner is noise.
out=$(lag_run 0)
if setup_ok "lag0" "$out"; then
  lacks "$BEHINDMARK" "$out"
  lacks "$NEVERMARK" "$out"
fi

# (h) A sha this checkout has never heard of: the fleet is running something that
#     never reached origin. That is informative, not a parse error to swallow —
#     and it must NOT be reported as an ordinary lag, which would understate it.
#     Needs a REAL origin: with no origin/main the hook cannot judge any sha and
#     correctly stays silent, so reusing prov_run here would have asserted
#     nothing (it is how this case failed on first run).
unknown_run() {
  local d="$TMP/unknown"; rm -rf "$d"
  git init -q --bare -b main "$d/origin.git"
  git clone -q "$d/origin.git" "$d/work" 2>/dev/null
  ( cd "$d/work"
    git checkout -q -B main
    mkdir -p .claude; cp "$HOOK" .claude/session-freshness.sh
    echo seed > seed.txt; git add -A; git commit -qm seed
    git push -q -u origin main ) >/dev/null 2>&1
  local pf="$d/prov.json"
  printf '{"sha":"%s","ref":"main","on_main":"yes","built_at":"x"}\n' \
    "0123456789abcdef0123456789abcdef01234567" > "$pf"
  ( cd "$d/work"; AMUX_RS_BUILD_PROVENANCE="$pf" bash .claude/session-freshness.sh 2>&1 )
}
out=$(unknown_run)
says "$NEVERMARK" "$out"
lacks "$BEHINDMARK" "$out"

echo
echo "test-session-freshness: $PASS passed, $FAIL failed"
[ "$FAIL" -eq 0 ] || exit 1
