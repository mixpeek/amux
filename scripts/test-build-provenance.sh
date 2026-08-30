#!/usr/bin/env bash
# AEAB-12 — the auto-builder must SAY when it is about to install a revision that
# is not on main.
#
# It rebuilds whenever the build source's local HEAD moves, on any branch, and the
# server self-adopts within ~5s. That permissiveness is deliberate and stays — this
# machine has survived weeks pinned to an unmerged fix branch. The defect is that a
# deliberate pin and an ACCIDENTAL feature branch were byte-identical to the
# builder. On 2026-08-17 a commit made on a branch inside the build source served
# the whole fleet for 9h42m with no CI and no review, while the same condition
# stopped the machine tracking upstream. Nothing looked wrong anywhere.
#
# Runs the SHIPPED script (via its provenance-only seam, which stops before the
# cargo build) against REAL git repos, so this exercises the real predicate rather
# than a restatement of it.
#
# The on-main cases are the load-bearing ones: a builder that shouted on every
# build would be ignored within a day, and the whole value is that the line only
# appears when something is actually off.
set -uo pipefail
cd "$(dirname "$0")/.."
BUILDER="$(pwd)/scripts/rust-auto-build.sh"
PASS=0; FAIL=0
TMP=$(mktemp -d); trap 'rm -rf "$TMP"' EXIT
export GIT_AUTHOR_NAME=t GIT_AUTHOR_EMAIL=t@t GIT_COMMITTER_NAME=t GIT_COMMITTER_EMAIL=t@t
# Pin the default branch — CI has none and falls back to `master`, which silently
# changed what these repos looked like (the mistake test-session-freshness.sh made).
export GIT_CONFIG_COUNT=1 GIT_CONFIG_KEY_0=init.defaultBranch GIT_CONFIG_VALUE_0=main

# $1 = case name, $2 = shell to run inside the repo after the base commit.
run_case() {
  local d="$TMP/$1"
  git init -q -b main "$d"
  ( cd "$d"
    # Cargo.toml matters: the builder keys its rebuild stamp on the last commit
    # touching crates/ | Cargo.toml | Cargo.lock, and exits early when there is
    # none. A fixture without it exercises nothing (the first cut of this file
    # did exactly that and reported 7 failures against working code).
    mkdir -p crates; echo '[workspace]' > Cargo.toml; echo x > crates/f.rs
    git add -A; git commit -qm base
    eval "$2"
  ) >/dev/null 2>&1
  AMUX_RS_BUILD_PROVENANCE_ONLY=1 \
  AMUX_REPO="$d" \
  AMUX_RS_BUILD_STAMP="$d/.stamp" \
  AMUX_RS_BUILD_LOG="$d/.log" \
  AMUX_RS_BUILD_LOCK="$d/.lock" \
  AMUX_RS_BUILD_PROVENANCE="$d/prov.json" \
  bash "$BUILDER" >"$d/out" 2>&1
  cat "$d/prov.json" 2>/dev/null
}

field() { sed -n "s/.*\"$1\":\"\([^\"]*\)\".*/\1/p" <<<"$2"; }
is() { if [ "$2" = "$3" ]; then PASS=$((PASS+1)); else FAIL=$((FAIL+1)); echo "FAIL: $1 — wanted '$3', got '$2'"; fi; }

# (a) CONTROL — on main. Must record on_main=yes and say nothing.
p=$(run_case onmain 'true')
is "(a) on main -> on_main"  "$(field on_main "$p")" "yes"
is "(a) on main -> ref"      "$(field ref "$p")"     "main"

# (b) THE INCIDENT — a feature branch in the build source.
p=$(run_case feature 'git checkout -q -b fix/my-thing; echo y > crates/g.rs; git add -A; git commit -qm work')
is "(b) feature branch -> on_main" "$(field on_main "$p")" "no"
is "(b) feature branch -> ref"     "$(field ref "$p")"     "fix/my-thing"

# (c) A branch whose commits are ALREADY on main (e.g. just merged) is NOT off-main.
#     Warning here would train people to ignore the line.
p=$(run_case merged 'git checkout -q -b spur; git checkout -q main; git merge -q --ff-only spur; git checkout -q spur')
is "(c) branch contained in main -> on_main" "$(field on_main "$p")" "yes"

# (d) Detached HEAD at a commit on main — the deliberate-pin shape after a
#     fast-forward — must also read as on main.
p=$(run_case detached 'git checkout -q --detach HEAD')
is "(d) detached at a main commit -> on_main" "$(field on_main "$p")" "yes"

# (e) The human-facing line appears only in the off-main case.
grep -q 'OFF-MAIN' "$TMP/feature/out" && PASS=$((PASS+1)) || { FAIL=$((FAIL+1)); echo "FAIL: (e) off-main case printed no OFF-MAIN line"; }
grep -q 'OFF-MAIN' "$TMP/onmain/out"  && { FAIL=$((FAIL+1)); echo "FAIL: (e) on-main case must stay quiet"; } || PASS=$((PASS+1))

# ── AEAB-50: the file must describe what is INSTALLED, not what was attempted ──
#
# It used to be written before the build and nowhere else, so a FAILED build left
# it permanently asserting a deploy that never happened — and the freshness hook
# quotes it as "the RUNNING SERVER is N behind; built <sha>", which is the line a
# session uses to decide whether its merge deployed.
#
# This runs the REAL failure path rather than restating it: a repo with no cargo
# manifest, so `cargo build` fails instantly and cheaply. Every destructive seam
# is pinned away from real paths (DRYRUN for the disk clear, temp INSTALL/STAMP/
# LOG/PROVENANCE), which is also why this can run in CI.
prov_failbuild() {
  local d="$TMP/failbuild"; rm -rf "$d"; mkdir -p "$d"
  git init -q -b main "$d/repo"
  ( cd "$d/repo"
    mkdir -p scripts crates
    echo x > crates/keep.txt
    git add -A; git commit -qm "no cargo manifest here" ) >/dev/null 2>&1
  printf '%s\n' "$1" > "$d/prov.json"
  AMUX_REPO="$d/repo" \
  AMUX_RS_INSTALL="$d/install-bin" \
  AMUX_RS_BUILD_STAMP="$d/stamp" \
  AMUX_RS_BUILD_LOG="$d/build.log" \
  AMUX_RS_BUILD_PROVENANCE="$d/prov.json" \
  AMUX_RS_DISK_CLEAR_DRYRUN=1 \
    timeout 120 bash "$BUILDER" >/dev/null 2>&1
  cat "$d/prov.json"
}

SENTINEL='{"sha":"deadbeefdeadbeefdeadbeefdeadbeefdeadbeef","ref":"main","on_main":"yes","built_at":"LAST GOOD"}'
got="$(prov_failbuild "$SENTINEL")"
if [ "$got" = "$SENTINEL" ]; then
  PASS=$((PASS+1)); echo "  ok   — a FAILED build leaves provenance naming the last good install"
else
  FAIL=$((FAIL+1))
  echo "  FAIL — a failed build overwrote provenance; it now claims a deploy that never happened"
  echo "         wanted: $SENTINEL"
  echo "         got:    ${got:-<empty>}"
fi

echo
echo "test-build-provenance: $PASS passed, $FAIL failed"
[ "$FAIL" -eq 0 ] || exit 1
