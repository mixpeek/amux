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

# AF-95 changed what the DIVERGED banner says, and this MARK was not updated with
# it — so `says` (case b) broke while the two `lacks` (c, d) went VACUOUS, unable to
# fail against a string the hook no longer prints. Two assertions became theatre and
# one broke, from one edit that was verified by RENDERING the branches rather than by
# running this suite.
#
# The replacement is the REMEDY line, which appears only inside the diverged block
# (session-freshness.sh:111) and nowhere in the behind-only or ahead-only paths — so
# (c) and (d) can genuinely fail again if that gating is ever broken.
MARK="RECONCILE IT: git merge origin/main"

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
# The advice AF-95 removed must not come back: on the canonical checkout there is no
# other clone to go log in, so this line sent a reader somewhere that does not exist.
# Pinning its ABSENCE is worth more than the old tense-fragile rationale check, which
# passed only because the replacement text says "reaches" where it said "reached".
lacks "log friction in a clone that is current" "$out"

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
#
# The lag commits land under crates/ because the hook counts only commits the
# BUILDER would act on (`-- crates/ Cargo.toml Cargo.lock`, copied from
# rust-auto-build.sh's trigger). The first cut of that pathspec fix (ac6f8b3)
# broke this suite for two pushes (AMUX-3494): these fixtures wrote n$i.txt,
# which the builder ignores, so the hook was correctly silent and every lag
# assertion read <empty>. The fixture must share the predicate of the
# mechanism it exercises — the same rule the hook fix itself was about.
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
      mkdir -p crates
      for i in $(seq 1 "$1"); do echo "n$i" > "crates/n$i.rs"; git add -A; git commit -qm "n$i"; done
      git push -q origin main ) >/dev/null 2>&1
  fi
  local pf="$d/prov.json"
  printf '{"sha":"%s","ref":"main","on_main":"yes","built_at":"x"}\n' "$built" > "$pf"
  # SELF-CHECK before consulting the hook: if the repo is not in the shape the
  # case claims, a setup bug would read as a hook bug. Counted with the hook's
  # own pathspec, so "the shape the case claims" is the shape the hook sees.
  ( cd "$d/work"
    git fetch -q origin 2>/dev/null
    got=$(git rev-list --count "${built}..origin/main" -- crates/ Cargo.toml Cargo.lock 2>/dev/null || echo x)
    if [ "$got" != "$1" ]; then echo "SETUP BROKEN for lag$1: wanted build-relevant behind=$1, got $got"; exit 0; fi
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

# (g2) NEGATIVE CONTROL for the pathspec itself (ac6f8b3) — the repo IS behind,
#      but every undeployed commit touches only files the builder ignores
#      (scripts/, docs). The builder will never rebuild for these, so the server
#      is NOT behind and the banner must stay dark. Before ac6f8b3 the hook
#      counted these and cried "1 behind" on every scripts-only push — the
#      false positive that made the banner ignorable. Reverting the pathspec
#      turns this cell red.
lag_nonbuild_run() {
  local d="$TMP/lagnb"; rm -rf "$d"
  git init -q --bare -b main "$d/origin.git"
  git clone -q "$d/origin.git" "$d/work" 2>/dev/null
  local built=""
  ( cd "$d/work"
    git checkout -q -B main
    mkdir -p .claude; cp "$HOOK" .claude/session-freshness.sh
    echo seed > seed.txt; git add -A; git commit -qm seed
    git push -q -u origin main ) >/dev/null 2>&1
  built=$(cd "$d/work" && git rev-parse HEAD)
  ( cd "$d/work"
    mkdir -p scripts docs
    echo s > scripts/tool.sh;  git add -A; git commit -qm "scripts-only"
    echo d > docs/note.md;     git add -A; git commit -qm "docs-only"
    git push -q origin main ) >/dev/null 2>&1
  local pf="$d/prov.json"
  printf '{"sha":"%s","ref":"main","on_main":"yes","built_at":"x"}\n' "$built" > "$pf"
  # Both halves of the shape must hold or the silence proves nothing: the repo
  # genuinely behind (plain count 2), and none of it build-relevant (pathspec 0).
  ( cd "$d/work"
    git fetch -q origin 2>/dev/null
    plain=$(git rev-list --count "${built}..origin/main" 2>/dev/null || echo x)
    build=$(git rev-list --count "${built}..origin/main" -- crates/ Cargo.toml Cargo.lock 2>/dev/null || echo x)
    if [ "$plain" != "2" ] || [ "$build" != "0" ]; then
      echo "SETUP BROKEN for lagnb: wanted plain=2 build-relevant=0, got plain=$plain build=$build"; exit 0
    fi
    AMUX_RS_BUILD_PROVENANCE="$pf" bash .claude/session-freshness.sh 2>&1 )
}
out=$(lag_nonbuild_run)
if setup_ok "lagnb" "$out"; then
  lacks "$BEHINDMARK" "$out"
fi

# (i2) AEAB-32 follow-up — the AGE must be the OLDEST undeployed commit, not the
#      newest. Shipped as `git log -1 ... | tail -1`, where `-1` limits git to one
#      commit so `tail` never sees a second line: it printed "3 minutes ago" while
#      the oldest undeployed commit was 23 HOURS old. The line exists to separate
#      "just merged, the builder is about to pick it up" from "a fix has sat
#      undeployed overnight", and only the second is worth waking up for — so an
#      age that is always small turns the whole line into reassuring noise.
#
#      Every case above passes with it broken; they assert the COUNT. This asserts
#      the age, using commits with deliberately different dates.
lag_age_run() {
  local d="$TMP/lagage"; rm -rf "$d"
  git init -q --bare -b main "$d/origin.git"
  git clone -q "$d/origin.git" "$d/work" 2>/dev/null
  local built=""
  ( cd "$d/work"
    git checkout -q -B main
    mkdir -p .claude; cp "$HOOK" .claude/session-freshness.sh
    echo seed > seed.txt; git add -A; git commit -qm seed
    git push -q -u origin main ) >/dev/null 2>&1
  built=$(cd "$d/work" && git rev-parse HEAD)
  # OLD commit first, then a RECENT one. If the hook reports the newest, it will
  # say "seconds/minutes"; if it reports the oldest it must say years.
  ( cd "$d/work"
    mkdir -p crates
    echo a > crates/a.rs; git add -A
    GIT_AUTHOR_DATE="2020-01-01T00:00:00" GIT_COMMITTER_DATE="2020-01-01T00:00:00" git commit -qm old
    echo b > crates/b.rs; git add -A; git commit -qm recent
    git push -q origin main ) >/dev/null 2>&1
  local pf="$d/prov.json"
  printf '{"sha":"%s","ref":"main","on_main":"yes","built_at":"x"}\n' "$built" > "$pf"
  ( cd "$d/work"
    git fetch -q origin 2>/dev/null
    n=$(git rev-list --count "${built}..origin/main" -- crates/ Cargo.toml Cargo.lock 2>/dev/null || echo x)
    [ "$n" = "2" ] || { echo "SETUP BROKEN for lagage: wanted 2 build-relevant behind, got $n"; exit 0; }
    AMUX_RS_BUILD_PROVENANCE="$pf" bash .claude/session-freshness.sh 2>&1 )
}
out=$(lag_age_run)
if setup_ok "lagage" "$out"; then
  says "2 commit(s) behind" "$out"
  # The oldest is dated 2020, so a correct reader says "years ago". A reader
  # taking the newest says seconds/minutes — the failure this pins.
  if printf '%s\n' "$out" | grep -qE "oldest undeployed commit landed [0-9]+ years ago"; then PASS=$((PASS+1));
  else FAIL=$((FAIL+1)); echo "FAIL: (i2) the age must be the OLDEST undeployed commit, not the newest"; echo "  got: $out"; fi
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

# ── Axis 2b: installed git hooks vs the checkout (2026-08-23) ───────────────
#
# The axis exists because amux already detected this and told nobody: the server
# logged "OUTDATED HOOK ... Reinstall: scripts/install-hooks.sh" 128 times over 8
# days, naming 9 session/repo pairs, into server-rs.log. Meanwhile every checkout
# on the machine had .git/hooks/pre-commit dated Aug 5 and the append-only
# data-loss guard was not installed at all.
#
# Case (i) is the load-bearing control. An axis that fires whenever it can see a
# hooks directory would "pass" every positive case here and be pure noise in
# practice — and noise in a SessionStart banner is worse than silence, because
# the banner's whole value is that speaking is rare.
HOOKMARK="installed git hooks differ from this checkout"

# A repo with scripts/git-hooks and a real .git/hooks, both populated.
hooks_repo() { # $1 name
  local d="$TMP/$1"; rm -rf "$d"
  git init -q -b main "$d/work"
  ( cd "$d/work"
    mkdir -p .claude scripts/git-hooks
    cp "$HOOK" .claude/session-freshness.sh
    for h in pre-commit pre-push prepare-commit-msg amux-staged-guard; do
      printf '#!/bin/sh\n# v1 %s\n' "$h" > "scripts/git-hooks/$h"
      chmod +x "scripts/git-hooks/$h"
      cp "scripts/git-hooks/$h" ".git/hooks/$h"
    done
    echo seed > seed.txt; git add -A; git commit -qm seed ) >/dev/null 2>&1
  echo "$d/work"
}
hooks_run() { ( cd "$1"; AMUX_RS_BUILD_PROVENANCE="$TMP/no-such-provenance.json" \
                        bash .claude/session-freshness.sh 2>&1 ); }

# (i) CONTROL — installed hooks identical to the checkout: say NOTHING.
w=$(hooks_repo hk_same)
out=$(hooks_run "$w")
lacks "$HOOKMARK" "$out"

# (j) one installed hook DIFFERS — named, and named specifically.
w=$(hooks_repo hk_diff)
printf '#!/bin/sh\n# v0 stale\n' > "$w/.git/hooks/pre-commit"
out=$(hooks_run "$w")
says "$HOOKMARK" "$out"
says "pre-commit" "$out"

# (k) an installed hook is MISSING ENTIRELY. This is the append-only-push-guard
#     case and it is the one a mtime or version comparison cannot express: there
#     is no file to be old, and "absent" reads identically to "fine" unless it is
#     said out loud.
w=$(hooks_repo hk_missing)
rm -f "$w/.git/hooks/pre-push"
out=$(hooks_run "$w")
says "MISSING" "$out"

# (l) a file in scripts/git-hooks that install-hooks.sh does NOT install must be
#     IGNORED. Without this the axis nags forever about something whose printed
#     remedy would not fix it — a warning with no honest exit, which is rule 3.
w=$(hooks_repo hk_extra)
printf 'not installed anywhere\n' > "$w/scripts/git-hooks/git-shared-guard.py"
out=$(hooks_run "$w")
lacks "$HOOKMARK" "$out"

# (m) IN A WORKTREE. `.git` is a FILE there, so "$REPO/.git/hooks" does not
#     exist and a hardcoded path reports nothing — silent in exactly the
#     checkouts AEAB-26 says the guard is already blind in. `git rev-parse
#     --git-path hooks` resolves to the MAIN repo's hooks dir, which is the one
#     a worktree actually executes.
w=$(hooks_repo hk_wt)
( cd "$w" && git worktree add -q "$TMP/hk_wt_linked" -b wtbranch ) >/dev/null 2>&1
printf '#!/bin/sh\n# v0 stale\n' > "$w/.git/hooks/pre-commit"
mkdir -p "$TMP/hk_wt_linked/.claude"
cp "$HOOK" "$TMP/hk_wt_linked/.claude/session-freshness.sh"
if [ -f "$TMP/hk_wt_linked/.git" ]; then
  out=$(hooks_run "$TMP/hk_wt_linked")
  says "$HOOKMARK" "$out"
else
  FAIL=$((FAIL+1)); echo "SETUP BROKEN for (m): .git is not a file, so this is not a worktree"
fi

# ── Axis 1e: provenance vs what the RUNNING SERVER answers (AEAB-50 follow-up) ─
#
# Everything else about the running build reads rust-build-provenance.json, which
# is a CLAIM. /health's `commit` is compiled into the binary that answered and
# cannot be stale by construction. This axis exists so the reader does not depend
# on the builder being honest.
#
# Driven through AMUX_HEALTH_URL against `file://` fixtures rather than whatever
# is listening on the machine running the suite — a test whose verdict depends on
# the host's state is not testing the hook, which this file has already learned
# once the hard way.
XMARK="disagree about what is deployed"

xcheck_run() { # $1 prov sha  $2 health json
  local d="$TMP/xcheck"; rm -rf "$d"; mkdir -p "$d"
  git init -q --bare -b main "$d/origin.git"
  git clone -q "$d/origin.git" "$d/work" 2>/dev/null
  ( cd "$d/work"
    git checkout -q -B main
    mkdir -p .claude; cp "$HOOK" .claude/session-freshness.sh
    echo seed > seed.txt; git add -A; git commit -qm seed
    git push -q -u origin main ) >/dev/null 2>&1
  printf '{"sha":"%s","ref":"main","on_main":"yes","built_at":"x"}\n' "$1" > "$d/prov.json"
  printf '%s\n' "$2" > "$d/health.json"
  ( cd "$d/work"
    AMUX_RS_BUILD_PROVENANCE="$d/prov.json" AMUX_HEALTH_URL="file://$d/health.json" \
      bash .claude/session-freshness.sh 2>&1 )
}

FULLSHA=0123456789abcdef0123456789abcdef01234567

# (n) THE CONTROL, and the one a wrong comparison fails. /health returns 12 hex
#     and provenance 40, so an EQUALITY test would fire here — on every real call,
#     forever. A detector that always fires is the same defect as one that never
#     can, so this case is the reason the match is a prefix.
out=$(xcheck_run "$FULLSHA" '{"commit":"0123456789ab","server":"amux-rust"}')
lacks "$XMARK" "$out"

# (o) genuine disagreement — say so, and name BOTH values. Naming only one leaves
#     the reader to go find the other, which is the trip this axis exists to save.
out=$(xcheck_run "$FULLSHA" '{"commit":"deadbeef1234","server":"amux-rust"}')
says "$XMARK" "$out"
says "deadbeef1234" "$out"
says "0123456789ab" "$out"

# (p) no server answering: SILENT. Fails open like everything else here — a
#     freshness check that shouts because the machine is offline gets deleted.
out=$(xcheck_run "$FULLSHA" '')
lacks "$XMARK" "$out"

# (q) WAS an assertion that an absent `commit` field stays silent. REMOVED, and
#     the reason is worth more than the assertion was: with a prefix match an
#     empty health value expands to the pattern `*`, which matches everything, so
#     that case is silent BY CONSTRUCTION and no wrong implementation reachable
#     from here can make it fire. Mutation-checked — dropping the `-n
#     "$health_commit"` guard changed nothing, 39 passed. It read as coverage and
#     was theatre, which rule 7 says is worse than no check because it confers
#     false confidence. The guard stays in the hook because it states the intent;
#     the assertion goes because it could not fail. Case (n) already covers the
#     comparison being wrong.

# (r) a SHORTER health value must still match — the length comes from the health
#     value, not a hardcoded 12, so the day the server widens or narrows that
#     field this keeps working instead of failing silently.
out=$(xcheck_run "$FULLSHA" '{"commit":"0123456","server":"amux-rust"}')
lacks "$XMARK" "$out"

# ── Axis 3: does the hook notice that IT is not the current one? (AEAB-46) ────
#
# Every other axis lives inside this file, so when the file is old a missing axis
# is indistinguishable from an axis that found nothing. On 2026-08-22 a session
# ran the Aug 5 copy — `grep -c "RUNNING SERVER is"` returns 0 there — and was
# never told the fleet was 23 commits behind.
SELFMARK="THIS FRESHNESS HOOK is not the one on origin/main"
FOOTMARK="(from "

# A clone whose hook is on origin/main, optionally modified locally afterwards.
self_run() { # $1 name  $2 "modify" to diverge the local copy
  local d="$TMP/$1"; rm -rf "$d"
  git init -q --bare -b main "$d/origin.git"
  git clone -q "$d/origin.git" "$d/work" 2>/dev/null
  ( cd "$d/work"
    git checkout -q -B main
    mkdir -p .claude; cp "$HOOK" .claude/session-freshness.sh
    echo seed > seed.txt; git add -A; git commit -qm seed
    git push -q -u origin main ) >/dev/null 2>&1
  if [ "${2:-}" = "modify" ]; then
    printf '\n# a local edit that origin does not have\n' >> "$d/work/.claude/session-freshness.sh"
  fi
  ( cd "$d/work"
    AMUX_RS_BUILD_PROVENANCE="$TMP/no-such-provenance.json" \
    AMUX_HEALTH_URL="file://$TMP/no-such-health.json" \
      bash .claude/session-freshness.sh 2>&1 )
}

# (s) THE CONTROL. Hook identical to origin's: say nothing about itself. An axis
#     that fires on a current checkout is noise in a banner whose entire value is
#     that speaking is rare — and it would fire on every session, forever.
out=$(self_run self_same)
lacks "$SELFMARK" "$out"

# (t) local copy differs from origin's: say so. This is the Aug-5-copy case, and
#     the wording has to say that the ABSENCE of axes is the danger, not just
#     that the file differs.
out=$(self_run self_diff modify)
says "$SELFMARK" "$out"

# (u) THE SILENCE PROPERTY, and the one the provenance footer could most easily
#     break. When nothing is wrong the hook must print NOTHING AT ALL — not even
#     a footer saying which copy stayed quiet. A SessionStart banner that always
#     prints one line is a banner people stop reading, which costs every axis
#     above it.
out=$(self_run self_silent)
lacks "$FOOTMARK" "$out"
if [ -z "$out" ]; then PASS=$((PASS+1)); else
  FAIL=$((FAIL+1)); echo "FAIL: a fully current checkout must print nothing; got: $out"
fi

# (v) when the banner DOES speak, it names which copy spoke — so a reader seeing
#     three axes where the current hook has six can tell the difference between
#     "all clear" and "this copy has nothing to say about that".
out=$(self_run self_foot modify)
says "$FOOTMARK" "$out"

echo
echo "test-session-freshness: $PASS passed, $FAIL failed"
[ "$FAIL" -eq 0 ] || exit 1
