#!/usr/bin/env bash
# Run a cargo test command and say whether a BUILD WAS IN FLIGHT while it ran.
#
# WHY (AMUX-3853). On 2026-08-28 a full `cargo test -p amux-server --lib` came
# back with 8 failures in `opencode::structured`, in code nobody had touched.
# Re-run in isolation: 15 pass, 0 fail. The failures were contention — those
# tests spawn a binary out of the shared CARGO_TARGET_DIR while the auto-builder
# is rewriting it, the ETXTBSY family `2618b7d3` already added a retry for. The
# retry is not enough under the load this box actually carries (the fleet, a
# builder rebuilding on every commit, and any peer running clippy).
#
# The cost is not the wasted run. It is that "1530 pass, 0 failed" and "8 failed"
# are both produced by the same command against the same code, and NOTHING in
# cargo's output says which kind of run you got. Every green suite here silently
# means "green, AND nothing was building" — the second clause is invisible, so
# nobody states it, and a red one gets read as a regression.
#
# The wrong lesson from that is "ignore red suites". This exists so you do not
# have to: it prints the missing clause beside the result.
#
#   scripts/test-contended.sh -p amux-server --lib autofix
#
# Exit status is the test command's, untouched — this reports, it never decides.
set -uo pipefail

# RUN FROM A SNAPSHOT OF THIS FILE (AF-368, found by `amux`).
#
# bash reads a script INCREMENTALLY, by byte offset, not into memory up front. On
# a shared checkout that makes every long-running .sh a moving target: a peer
# commits to it, the file grows underneath the running shell, the offsets shift,
# and bash resumes mid-token. It then fails on whatever byte now sits at its saved
# position, which is usually not where the edit was.
#
# Measured live 2026-08-31, and the surface is maximally misleading:
#
#   1888 passed, 0 failed, no `test result: FAILED` line anywhere
#   no contention verdict printed at all
#   ./scripts/test-contended.sh: line 53: syntax error near unexpected token `('
#   exit 2
#
# Line 53 was a bare `#`, and the file was `bash -n` clean the whole time. Two
# commits of mine landed inside that run. Every test had already passed. Anyone
# reading exit 2 reports a red suite; what caught it was that "0 failed" and
# "exit 2" cannot both be a test result.
#
# THIS IS THE THIRD CAUSE, after the builder and the dirty worktree, and it is the
# one this script structurally CANNOT report: it dies before reaching any echo, so
# its verdict is not wrong, it is absent. The instrument's blind spot is the
# instrument. Snapshotting is the only fix at the right layer — a report cannot
# describe a run that stopped existing.
#
# `exec` replaces this process, so there is exactly one shell and the exit status
# still belongs to cargo. The snapshot is removed by the EXIT trap below, which the
# re-executed copy installs.
if [ -z "${_TC_SNAPSHOT:-}" ]; then
  _snap=$(mktemp) || exit 1
  cat "$0" > "$_snap" || { rm -f "$_snap"; exit 1; }
  export _TC_SNAPSHOT="$_snap"
  # CARRY THE REAL PATH ACROSS THE RE-EXEC (AF-346). After this line `$0` is a
  # temp file, so anything downstream that locates the repo from the running
  # script's own path resolves to /var/folders and gets nothing. The target
  # clause below did exactly that and failed SILENTLY, which is the same
  # not-printing-is-not-passing shape it exists to announce.
  export _TC_ORIGIN="$(cd "$(dirname "$0")" && pwd)/$(basename "$0")"
  exec bash "$_snap" "$@"
fi

LOCK="${AMUX_RS_BUILD_LOCK:-$HOME/.amux/rust-build.lock}"
: "${CARGO_TARGET_DIR:=$HOME/.amux/rust-build-target}"
export CARGO_TARGET_DIR

# Sampled, not checked once at the start and once at the end. A build that
# starts AND finishes inside a two-minute suite is invisible to the endpoints
# and is exactly the run that produces a confusing red.
SEEN=0
OWNERS=""
sample() {
  while :; do
    if [ -d "$LOCK" ]; then
      SEEN=1
      p=$(cat "$LOCK/pid" 2>/dev/null || echo "?")
      case " $OWNERS " in *" $p "*) ;; *) OWNERS="$OWNERS $p" ;; esac
      printf '%s\n' "$p" >> "$FLAG"
    fi
    sleep 2
  done
}

FLAG=$(mktemp)
trap 'kill "$SAMPLER" 2>/dev/null; rm -f "$FLAG" "${_TC_SNAPSHOT:-}" "${_TC_SOURCE_SNAPSHOT:-}"' EXIT INT TERM

sample & SAMPLER=$!

# THE SECOND WAY A PEER REDDENS YOUR SUITE (AF-356).
#
# The builder is one of TWO causes and this wrapper only ever measured that one,
# so its clean verdict ("NOT build contention") read as "therefore your bug". The
# other cause is a peer's UNCOMMITTED SOURCE sitting in the shared worktree: cargo
# compiles the tree, not your commit, so their half-finished edit fails a test in a
# module you never opened, and it passes on a rerun after they finish. Identical
# symptom to ETXTBSY, and nothing distinguished them.
#
# Measured live 2026-08-31: `amux` ran the suite and got one failure in
# `gate_table_matches_python`, which they had not touched. They read this
# wrapper's clean verdict, concluded ETXTBSY, and carried that into a
# verification request as its stated weakest evidence line. The real cause was my
# uncommitted `ItemType::Decision` in the shared tree. Both of us were reasoning
# from an instrument that answered a narrower question than the sentence it printed.
#
# Captured BEFORE and AFTER, because a tree that CHANGED under the compile is the
# strongest form of the signal and a single snapshot cannot see it.
#
# Deliberately NOT attributed to a lane. Owner-by-mtime is the inference that has
# been wrong repeatedly on this checkout (AF-179, AMUX-3662, where a lane's own
# writes read as a phantom co-editor), and a confident wrong owner is worse than a
# named file with no owner. The file names are what a reader needs; they can tell
# in one second whether a path is theirs.
dirty_now() { git status --porcelain --untracked-files=no 2>/dev/null | awk '{print $NF}' | sort; }

# WHICH CRATE IS UNDER TEST (AF-336). The clause below lists every dirty file,
# which is the right default: any of them can redden any module, and the entry
# that asked for this said so. What it could not do is answer the narrower
# question a reader actually has -- is a peer's draft inside the code THIS
# COMMAND COMPILED? On this checkout the two come apart constantly, because the
# usual dirty set is a peer editing a crate you are not testing, and five paths
# read as five reasons to doubt a red when the honest count is zero.
#
# Scoped by PACKAGE PATH, never by guessing from the file name. Resolved from
# `cargo metadata` when it answers and from the conventional crates/<name>
# layout when it does not, so a package whose directory does not match its name
# still scopes correctly instead of silently going unscoped.
_pkg=""
_want=0
for _a in "$@"; do
  if [ "$_want" = 1 ]; then _pkg="$_a"; _want=0; continue; fi
  case "$_a" in -p|--package) _want=1 ;; -p=*|--package=*) _pkg="${_a#*=}" ;; --) break ;; esac
done
_pkg_dir=""
if [ -n "$_pkg" ]; then
  _pkg_dir=$(cargo metadata --no-deps --format-version 1 2>/dev/null \
    | python3 -c "import json,sys,os
try: d=json.load(sys.stdin)
except Exception: sys.exit(0)
for p in d.get('packages',[]):
    if p.get('name')==sys.argv[1]:
        print(os.path.relpath(os.path.dirname(p['manifest_path']), d.get('workspace_root','.')));break" "$_pkg" 2>/dev/null)
  [ -n "$_pkg_dir" ] || { [ -d "crates/$_pkg" ] && _pkg_dir="crates/$_pkg"; }
fi
_safe="$(dirname "${_TC_ORIGIN:-$0}")/safe-cargo.sh"

# AF-791: detect shared-target stale artifacts when source content changes but
# mtime does not. On this repo's shared CARGO_TARGET_DIR, mtime-only freshness
# can miss real source edits and reuse stale rlibs. Store a stable source digest
# per package and, when it changes, proactively clear that package's cache before
# running tests so the compile result cannot be stale.
_source_fingerprint() {
  python3 - "$1" <<'PY'
import hashlib
import os
import sys
from pathlib import Path

root = Path(sys.argv[1]).resolve()
h = hashlib.sha256()
paths = []
for rel in sorted(root.rglob('*.rs')):
    if rel.is_file():
        paths.append(rel)
cargo_toml = root / 'Cargo.toml'
if cargo_toml.is_file():
    paths.append(cargo_toml)

for path in paths:
    rel = path.relative_to(root)
    h.update(str(rel).encode())
    with path.open('rb') as handle:
        while True:
            chunk = handle.read(10240)
            if not chunk:
                break
            h.update(chunk)
print(h.hexdigest())
PY
}

_freshen_shared_package_cache() {
  if [ -z "$_pkg_dir" ] || [ -z "$_pkg" ]; then
    return 0
  fi

  _fp_root="$CARGO_TARGET_DIR/.amux-cargo-fingerprint"
  _fp_file="$_fp_root/${_pkg}.sha256"
  mkdir -p "$_fp_root"

  _current_fp=$(_source_fingerprint "$_pkg_dir")
  _previous_fp=""
  if [ -f "$_fp_file" ]; then
    _previous_fp="$(cat "$_fp_file")"
  fi

  if [ "$_previous_fp" = "$_current_fp" ]; then
    return 0
  fi

  if [ -n "$_previous_fp" ]; then
    echo "staleness: shared target cache for package $_pkg differs from source digest;"
    echo "staleness: cleaning package cache before this test run to avoid stale artifacts."
    if [ -x "$_safe" ]; then
      "$_safe" clean --manifest-path "$(cd "$_pkg_dir" && pwd)/Cargo.toml" -p "$_pkg" --quiet
    else
      (cd "$_pkg_dir" && cargo clean --manifest-path Cargo.toml -p "$_pkg" --quiet)
    fi
  fi

  printf '%s' "$_current_fp" > "$_fp_file"
}

DIRTY_BEFORE=$(dirty_now)

_freshen_shared_package_cache

# ── WHICH TARGETS DID THIS NOT RUN? (AF-346) ────────────────────────────────
#
# `cargo test -p amux-server --lib` reports "1827 passed" and SKIPS every
# `tests/*.rs` target — 50 files here. That is not a footnote: the a99955f7
# dashboard regression was caught by a guard that ALREADY EXISTED, was correct,
# and would have blocked the commit. It did not run, because the author verified
# with `--lib` and read a four-digit pass count as the suite.
#
# The number is the trap. A run that says "1827 passed" and a run that says
# "1827 passed, and 50 integration targets were not built" are the same command
# with the same exit status, and only the second one lets you decide whether you
# care. This is the same rule the contention and worktree clauses below already
# follow: say what was NOT measured, in the same breath as the result.
#
# Counted from disk rather than from a constant, so a new integration file is
# included the day it lands rather than when someone remembers to bump a number.
# WHAT ELSE NARROWS A RUN (AF-346, second pass). The clause below starts at
# --lib because that is the flag that cost us a regression, and the entry's own
# closing sentence is that `--lib` is not the only such flag. Three more, each
# printing a number that reads like a total:
#
#   --test <name>   runs ONE integration target and skips the LIB ENTIRELY. On
#                   this crate that is the larger half by a wide margin, and the
#                   number it prints is the smallest of any selector here, so it
#                   is the one most likely to be read as a clean suite.
#   --tests/--benches/--examples  select a target KIND and skip the others.
#   a bare FILTER   (`-p amux-server --lib autofix`) runs only the tests whose
#                   name matches. cargo DOES print "N filtered out", which is
#                   why this one gets a shorter note: the denominator is already
#                   on screen, it is just easy to skim past when N is the number
#                   you came for.
#
# Read off the ARGUMENTS, not off cargo's output, so a run that dies before
# printing anything is still describable. That is the lesson of the snapshot
# block at the top of this file.
_filter_args=""
_sel_kind=""
_want_value=0
for _a in "$@"; do
  if [ "$_want_value" = 1 ]; then _want_value=0; continue; fi
  case "$_a" in
    --) break ;;
    -p|--package|--test|--bin|--example|--bench|--features|--manifest-path|--target|--target-dir|-j|--jobs|--exclude)
      _want_value=1 ;;
    -*) ;;
    *) _filter_args="$_filter_args $_a" ;;
  esac
done
case " $* " in
  *" --test "*)   _sel_kind="one integration target; THE LIB WAS NOT RUN" ;;
  *" --tests "*)  _sel_kind="integration targets only; the lib was not run" ;;
  *" --benches "*|*" --bench "*) _sel_kind="bench targets only" ;;
  *" --examples "*|*" --example "*) _sel_kind="example targets only" ;;
esac

_skipped_targets=""
case " $* " in
  *" --lib "*|*" --bins "*|*" --bin "*|*" --doc "*)
    # FROM GIT, NOT FROM BASH_SOURCE. This script snapshots itself to a temp
    # file and re-execs (the AF-368 self-edit protection above), so inside the
    # re-exec BASH_SOURCE is /var/folders/.../tmp.XXXX and `dirname/..` resolves
    # to nothing. The first version of this block did that and the clause simply
    # never printed — a missing warning is indistinguishable from nothing to
    # warn about, which is the exact failure this clause exists to announce,
    # committed inside the fix for it.
    # THREE SOURCES, most reliable first, because each fails in a different
    # place: _TC_ORIGIN survives the re-exec, git works from anywhere inside a
    # checkout, and cwd is the last resort. The first version used only
    # BASH_SOURCE (a temp file post-re-exec) and the second only git (empty when
    # invoked from outside a repo) — both went silent rather than wrong, which
    # is why a control cell that runs from another cwd is in the suite.
    _root="${_TC_ORIGIN:+$(dirname "$(dirname "$_TC_ORIGIN")")}"
    [ -n "$_root" ] || _root=$(git rev-parse --show-toplevel 2>/dev/null || true)
    _tdir="${_root:-.}/crates/amux-server/tests"
    if [ -d "$_tdir" ]; then
      _skipped_targets=$(find "$_tdir" -maxdepth 1 -name '*.rs' | wc -l | tr -d ' ')
    fi
    ;;
esac

# Through safe-cargo.sh, not bare cargo (AF-478). CLAUDE.md tells you to run
# tests with THIS script and to put any local cargo run through safe-cargo.sh
# for the systemd-scope isolation AMUX-70 exists for; those two instructions
# were in conflict because this line was bare. On a systemd host an OOM-killed
# `cargo test` here failed the pane's whole scope and took the interactive
# session down with it, which is the exact hazard the wrapper prevents. On a
# host with no systemd the wrapper execs cargo directly and this is a no-op.
# TEE, so this script can answer "did cargo start at all" from what the budget
# guard ACTUALLY EMITTED rather than inferring it from an exit code (AMUX-4689).
# 75 is not a clean discriminator: cargo-budget.py returns it both for a refusal
# BEFORE the run and for a failed disk probe AFTER one, while its mid-run kills
# return 124 or 128+signal. Reading 75 as "nothing ran" would be the same
# inference-instead-of-measurement this script exists to stop.
#
# ONE CALL SITE PER RUNNER, BEHIND A FUNCTION. The tee and the mktemp-failure
# fallback are a second axis, and writing them out inline multiplied two choices
# into four copies of the run line. `scripts/test-test-receipt.sh` cell n counts
# occurrences of the safe-cargo invocation and expects one; the duplicate broke
# it, and CI stayed red for five commits before anyone read the step. A count
# assertion is the right check here precisely because a second run line is how
# an unprotected or receipt-less path gets added.
_rcpt_writer="$(dirname "${_TC_ORIGIN:-$0}")/write-test-receipt.sh"
_TC_SOURCE_SNAPSHOT=$(mktemp) || _TC_SOURCE_SNAPSHOT=""
if [ -n "$_TC_SOURCE_SNAPSHOT" ] && [ -x "$_rcpt_writer" ]; then
  "$_rcpt_writer" --snapshot "$_TC_SOURCE_SNAPSHOT"
fi
export AMUX_TEST_SOURCE_SNAPSHOT="$_TC_SOURCE_SNAPSHOT"

_run_under_test() {
  if [ -x "$_safe" ]; then
    # It writes its own receipt for a `test` run; this script writes one at the
    # end, so tell it not to. Two identical receipts would be harmless and
    # confusing. The outer wrapper owns the pre-run source snapshot.
    _TC_RECEIPT=1 "$_safe" test "$@"
  else
    cargo test "$@"
  fi
}
_RUN_LOG=$(mktemp) || _RUN_LOG=""
if [ -n "$_RUN_LOG" ]; then
  _run_under_test "$@" 2>&1 | tee "$_RUN_LOG"
  RC=${PIPESTATUS[0]}
else
  _run_under_test "$@"
  RC=$?
fi
# ${PIPESTATUS[0]} rather than $?, and it is NOT load-bearing here: `set -o
# pipefail` on line 24 already makes $? the rightmost non-zero status, so both
# read 75 on a refusal. Confirmed by mutation, which is why this says so instead
# of claiming a fix it does not make: swapping in $? left all 11 cells green.
#
# Kept because the two answer different questions. pipefail gives "something in
# the pipeline failed"; PIPESTATUS[0] gives "the status of the command whose
# result this is". Those diverge if `tee` itself fails, on a full disk or an
# unwritable TMPDIR, where pipefail would report a passing suite as a failure.
#
# Worth stating plainly, because the ORIGINAL bug report on AMUX-4689 was
# exactly this confusion one level out: `test-contended.sh ... | tail` in an
# interactive shell with no pipefail reports tail's 0, and that was filed as the
# script exiting 0 on a refusal. The script was always right; the measurement
# was not.

DIRTY_AFTER=$(dirty_now)

kill "$SAMPLER" 2>/dev/null
wait "$SAMPLER" 2>/dev/null

# NOTHING RAN, SO CERTIFY NOTHING (AMUX-4689).
#
# Measured 2026-09-15 with the budget forced to refuse: this script printed its
# contention, targets and worktree clauses anyway. Every one of them describes a
# run, and there had been none:
#
#   contention: the auto-builder was NOT rebuilding during this run, so the shared
#   contention: binary was stable under it. A failure here is NOT build contention.
#   worktree:  clean at start and end, so no peer's uncommitted source was in this build.
#
# VERIFY.md's contract is to paste the command AND ITS RESULT LINE as evidence,
# and that block IS the result line. A lane pasting it is pasting reassurance
# about work that did not occur, which is this script's own subject one level up.
#
# The test is the EMITTED EVENT, not the exit code. `cargo_budget_started` is
# printed immediately before the child is spawned, so its absence means cargo
# never started. The whole clause is skipped when the log could not be captured,
# because "no marker found" and "nothing was looked at" are different facts and
# only one of them justifies suppressing the report.
if [ -n "$_RUN_LOG" ] && [ -s "$_RUN_LOG" ] \
   && ! grep -q '"event": "cargo_budget_started"' "$_RUN_LOG" \
   && grep -q '"event": "cargo_budget_refused"' "$_RUN_LOG"; then
  _tc_target="${CARGO_TARGET_DIR:-$HOME/.amux/rust-build-target}"
  echo ""
  echo "refused:   NO TEST RAN. The budget guard refused before cargo started, so the"
  echo "refused:   contention, targets and worktree clauses are SUPPRESSED: each one"
  echo "refused:   describes a run, and there was none. This is not a pass, and not a"
  echo "refused:   failure of the code under test. Exit status is $RC."
  echo "refused:   The refusal above names target_bytes/free_bytes/reason but no remedy."
  echo "refused:   Reclaim the shared target dir:"
  echo "refused:     python3 scripts/cargo-target-guard.py clear --target \"$_tc_target\""
  echo "refused:   Inspect first with --dry-run. To raise the ceiling for ONE run instead:"
  echo "refused:     AMUX_CARGO_MAX_TARGET_GB=<n> scripts/test-contended.sh $*"
  echo "refused:   (the default is 40; \`du -sh\` the target dir to pick a number)"
  rm -f "$_RUN_LOG"
  exit "$RC"
fi
rm -f "$_RUN_LOG"

if [ -s "$FLAG" ]; then
  builds=$(sort -u "$FLAG" | tr '\n' ' ' | sed 's/ *$//')
  echo ""
  echo "contention: A BUILD WAS IN FLIGHT during this run (builder pid(s): $builds)."
  echo "contention: a failure here may be ETXTBSY on the shared target dir rather than"
  echo "contention: a regression. Re-run the failing module alone before believing it"
  echo "contention: (AMUX-3853)."
else
  # SAID EXPLICITLY, not left as silence. "No line printed" would be
  # indistinguishable from "this script did not run", which is the same
  # absent-versus-measured confusion the whole entry is about.
  #
  # NAMES THE AUTO-BUILDER, not "a build" (2026-08-29). This arm used to read
  # "no build was in flight", and the first real run of this script printed it
  # directly under cargo's own "Compiling amux-server ... Finished in 1m 04s".
  # Both were true and the sentence still read as false, because what is
  # sampled is $LOCK, the AUTO-BUILDER's lock: the hazard is another process
  # rewriting the shared binary underneath a test that spawns it, not the
  # compile this very command is doing. A probe has to say what it measured,
  # or the one line that exists to settle "real or contention?" becomes the
  # thing you have to go and check.
  # SAYS WHAT IT RULED OUT, not "a failure here is real" (2026-08-29, second
  # pass). That phrasing was fixed once already this morning for naming "a
  # build" when it samples the AUTO-BUILDER, and it was still overclaiming in a
  # second dimension: a reader takes "real" to mean "a code regression", and
  # this script only ever knew about ONE environmental cause.
  #
  # The specimen arrived the same day. A full lib suite came back 1552 passed /
  # 6 failed under this exact clean verdict, and all six were host memory
  # pressure — swap at 8700MB over the 8192MB AMUX_MEM_SWAP_DENY_MB threshold,
  # so worker start was refused 503 where the tests expect 202. Real failures,
  # nothing to do with the code under test, and this line called them real.
  #
  # An instrument that rules out one cause has to say WHICH, or the next reader
  # generalises it to all of them. Which is the whole argument the top of this
  # file makes about plain `cargo test`, arriving one level up.
  #
  # 2026-09-14: the in-process routers that start workers now pin admission with
  # AdmissionOverride, so that specimen no longer depends on the host. The hint
  # below used to say "check for a 503 admission refusal", which after the pin
  # would steer a reader to blame the host for a harness that forgot to pin.
  echo ""
  echo "contention: the auto-builder was NOT rebuilding during this run, so the shared"
  echo "contention: binary was stable under it. A failure here is NOT build contention."
  echo "contention: (Cargo's own compile for this command is not the hazard; a peer's is.)"
  echo "contention: THAT IS THE ONLY THING RULED OUT. Test routers pin worker admission, so"
  echo "contention: a 503 admission refusal carrying admission_source=host means a test"
  echo "contention: harness skipped the pin."
fi

# THE TARGET CLAUSE (AF-346). Printed regardless of colour, for the same reason
# the worktree clause below is: a caveat about what the RUN covered belongs beside
# the result, not inside the failure branch.
if [ -n "$_sel_kind" ]; then
  echo ""
  echo "targets:   this invocation selected $_sel_kind."
  echo "targets:   Whatever number cargo printed above counts only what it selected."
  echo "targets:   AF-346: a suite-shaped command that silently covers a subset is the"
  echo "targets:   same instrument failure as a probe reporting zero when it never ran."
fi

if [ -n "${_filter_args# }" ]; then
  echo ""
  echo "targets:   a name FILTER was passed ($(echo "$_filter_args" | sed 's/^ //')), so cargo ran a subset."
  echo "targets:   Its own \"filtered out\" count above is the denominator; read it."
fi

if [ -n "$_skipped_targets" ] && [ "$_skipped_targets" != "0" ]; then
  echo ""
  echo "targets:   this invocation selected a subset — $_skipped_targets integration target(s)"
  echo "targets:   under crates/amux-server/tests/ were NOT built or run. Whatever number"
  echo "targets:   cargo printed above counts the lib only."
  echo "targets:   AF-346: the a99955f7 dashboard regression was caught by a guard that"
  echo "targets:   already existed and was correct; it did not run because the author"
  echo "targets:   verified with --lib and read the pass count as the suite."
  echo "targets:   Drop the selector, or name the file:  scripts/test-contended.sh -p amux-server --test <name>"
fi

# THE WORKTREE CLAUSE — printed on BOTH arms, never only the clean one.
#
# A caveat that lives inside one branch is a caveat the other branch's reader
# never sees (ethos rule 1: a statement about a whole set belongs at the top
# level, not inside one arm). A dirty tree explains a red just as well when the
# builder WAS running, and reading only the ETXTBSY note would stop the search
# one cause short.
if [ -n "$DIRTY_BEFORE" ] || [ -n "$DIRTY_AFTER" ]; then
  n_before=$(printf '%s\n' "$DIRTY_BEFORE" | grep -c . || true)
  n_after=$(printf '%s\n' "$DIRTY_AFTER" | grep -c . || true)
  echo "worktree:  $n_before uncommitted file(s) at start, $n_after at end."
  # THE SCOPED COUNT, and it is allowed to be zero out loud (AF-336). "0 of 5 are
  # in the crate you tested" is the sentence that turns a dirty tree from a
  # reason to doubt a red into a reason to stop suspecting it. Printed only when
  # the package directory actually resolved: a scoped-LOOKING count over an
  # unknown scope is the defect this clause would otherwise add.
  if [ -n "$_pkg_dir" ]; then
    n_in=$(printf '%s\n' "$DIRTY_AFTER" | grep -c "^$_pkg_dir/" || true)
    echo "worktree:  $n_in of $n_after are under $_pkg_dir/, the package this command selected"
    echo "worktree:  with -p $_pkg. Only those were compiled into this run; the rest are a"
    echo "worktree:  peer working elsewhere in the workspace and cannot explain a red here."
  fi
  if [ "$DIRTY_BEFORE" != "$DIRTY_AFTER" ]; then
    echo "worktree:  THE TREE CHANGED DURING THIS RUN. cargo compiled the worktree, not"
    echo "worktree:  your commit, so a file that moved under the compile can fail a test"
    echo "worktree:  in a module you never opened. This is the strongest form of the signal."
    printf '%s\n%s\n' "$DIRTY_BEFORE" "$DIRTY_AFTER" | sort -u | sed 's/^/worktree:    /'
  else
    printf '%s\n' "$DIRTY_AFTER" | sed 's/^/worktree:    /'
  fi
  echo "worktree:  Any of these — yours OR a peer's — can redden a module you did not"
  echo "worktree:  touch. Owner is NOT inferred here: mtime-based attribution has been"
  echo "worktree:  wrong on this checkout before (AF-179), and a confident wrong owner is"
  echo "worktree:  worse than a named file with none. You can tell which are yours."
else
  echo "worktree:  clean at start and end, so no peer's uncommitted source was in this"
  echo "worktree:  build. Stated because a silent probe and a clean tree look identical."
fi

# THE RECEIPT (AF-195, extracted to its own script by AF-478 so that
# `safe-cargo.sh test` writes one too — the receipt is a property of running
# tests, not of whichever wrapper you reached for).
_rcpt_writer="$(dirname "${_TC_ORIGIN:-$0}")/write-test-receipt.sh"
[ -x "$_rcpt_writer" ] && "$_rcpt_writer" "$RC" "$@"

exit "$RC"
