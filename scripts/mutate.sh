#!/usr/bin/env bash
# Apply and revert a single mutation SAFELY on a shared checkout.
#
# WHY THIS EXISTS (AMUX-3670, 2026-08-24). This repo asks for mutation testing
# on every check — "confirm the mutation LANDED before reading the suite's
# colour" — and the obvious harness is:
#
#     cp $F /tmp/orig ; <mutate> ; <run tests> ; cp /tmp/orig $F
#
# That restore is a WHOLE-FILE WRITE, and on a shared checkout it is
# indistinguishable from `git checkout -- $F` as far as a concurrent peer is
# concerned. Measured: at 15:45 it reverted mixpeek-research's in-flight
# `fn chrome_launch_args` out of browser.rs while keeping the call site that had
# arrived inside the mutate/restore window, breaking `cargo check` for both
# lanes. Twice in a row, because the harness ran twice. The same harness had run
# roughly a dozen times that day across five files; every one of them was a
# chance to do the same to somebody.
#
# So: mutate by EXACT STRING, revert by the inverse exact string. Only the bytes
# being mutated are ever written, so a peer editing any other part of the file is
# untouched.
#
# The uniqueness check is not politeness, it is the discipline: a mutation that
# matched 0 places and a suite that stayed green look identical, and "the tests
# have no coverage here" is the wrong conclusion you reach from it.
#
#   scripts/mutate.sh apply  <file> <old> <new>
#   scripts/mutate.sh revert <file> <old> <new>   # same args, swapped internally
#
# Exit 0 on success, 1 if the target is absent or not unique.
set -uo pipefail

# ── THIS SCRIPT MUST NOT MUTATE ITSELF (AF-440) ─────────────────────────────
#
# Measured 2026-09-03, twice in five minutes, on this file. `run` applies the
# mutation, executes the command, and reverts from a trap — but bash reads a
# script by BYTE OFFSET as it executes, so rewriting the file underneath the
# running interpreter shifts every offset after the edit. The revert never
# happened. Both times the file was left MUTATED, `bash -n` passed, and the
# damage surfaced only as the NEXT invocation refusing with "the replacement
# already occurs 1 time(s)" — which reads as a bad argument, not as a corrupted
# file.
#
# That is AF-368's mechanism arriving inside the tool whose entire purpose is to
# make mutation safe on a shared checkout. Refuse it: a self-mutation cannot be
# made safe from within the process being edited, and the tool that leaves a
# mutation behind is worse than no tool, because its failure is silent and its
# next refusal blames the caller.
refuse_self_mutation() {
  local target="$1" self_path
  self_path=$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)/$(basename "${BASH_SOURCE[0]}")
  local abs
  abs=$(cd "$(dirname "$target")" 2>/dev/null && pwd)/$(basename "$target") || return 0
  [[ "$abs" == "$self_path" ]] || return 0
  echo "mutate: REFUSING to mutate $target — that is this script (AF-440)." >&2
  echo "  bash reads a script by byte offset as it runs, so rewriting this file" >&2
  echo "  underneath the running interpreter loses the revert and leaves the" >&2
  echo "  mutation applied. Measured twice on 2026-09-03; both times \`bash -n\`" >&2
  echo "  passed and the only symptom was the NEXT run refusing with an argument" >&2
  echo "  error." >&2
  echo "  To test a change to this script, copy it and mutate the copy:" >&2
  echo "    cp $target /tmp/mutate-under-test.sh && $0 run /tmp/mutate-under-test.sh ..." >&2
  exit 2
}

usage() {
  echo "usage: $0 <apply|revert> <file> <old-string> <new-string>" >&2
  echo "       $0 run <file> <old-string> <new-string> -- <command...>" >&2
  echo "       $0 survey <file> [--limit N] [--stop-at REGEX] -- <command...>" >&2
  echo "       $0 seams <file> [--limit N] [--build <cmd>] -- <test-command...>" >&2
  exit 2
}

# ── `run`: APPLY, TEST, REVERT IN ONE PROCESS, WITH A TRAP (AF-284) ──────────
#
# `apply` and `revert` are two invocations, so the revert is conditional on the
# CALLER surviving to make it. That is the half of the hazard this script did not
# close, and it bit on 2026-08-28: amux-frustrations ran apply, then a
# `cargo test` that hit a 10-minute tool timeout, and the revert on the next line
# of the same shell block never ran. `or(Some(0))` sat in a peer's git_guard.rs
# for ten minutes on the shared checkout. No commit took it, but only because
# nobody ran `git add -A` in the window.
#
# The byte-scoped apply/revert this script already does bounds the BLAST RADIUS.
# It cannot bound the DURATION, because a two-call API hands cleanup back to a
# caller that may die. `run` keeps both calls in one process and reverts from a
# trap, so a timeout, a Ctrl-C, a failing test or a `set -e` abort all restore.
#
# The command's exit status is preserved and returned, because the whole point is
# to read the suite's colour under the mutation.
# ── `seams`: IS ANYTHING HOLDING THESE TWO ARGUMENTS APART? (AF-438) ─────────
#
# A DIFFERENT CLASS FROM `survey`, and `survey` cannot find it. Survey asks
# whether a LINE's value matters. This asks whether two things that must AGREE
# are held together by anything but the fact that they were written together.
#
# Seven instances in one night, four of them mvs-pitr's from a different repo:
#   AF-429  a writer and a detector, each with its own green test, and a fixture
#           hand-typing the writer's output so nothing pinned the pair
#   AF-437  a deriver and four readers of the same env var name
#   AF-438  a resolver and a message, both tested, the call site between them not
#   MP-100  two checks that fired on every fixture, so either could be deleted
#   MP-125  two roots that agreed on a name, so reading the wrong one survived
#   + two more where a fixture agreed with the reader and neither with the writer
#
# mvs-pitr's diagnosis is the one that made this buildable: every instance was a
# missing DIRECTION rather than a missing assertion, and none was visible from
# either side alone. A test per component passes exactly as well when the seam
# between them is broken.
#
#   scripts/mutate.sh seams <file> [--limit N] [--build <cmd>] -- <test-cmd>
#
# THE PROBE IS AN ARGUMENT SWAP. At each call site with two or more bare
# identifier arguments, exchange the first two and see what objects. Three
# outcomes, and all three are worth knowing:
#
#   HELD-BY-TYPES  the swap does not compile. The type system is the assertion,
#                  which is the best possible answer and needs no test.
#   KILLED         it compiles and a test fails. Something observes the pair.
#   SURVIVED       it compiles and every test passes. NOTHING holds these two
#                  apart — the seam is real, and this is the AF-438 shape
#                  exactly: `build(dir, ...)` and `build(&label, ...)` are both
#                  `&str`, both valid, and one of them is wrong.
#
# Pass `--build` to separate the first outcome from the second. Without it a
# compile failure and a test failure both read as KILLED, which understates the
# seams that only the compiler is holding — those are safe today and become
# unheld the moment someone widens a type.
if [[ "${1:-}" == "seams" ]]; then
  shift
  file="${1:-}"; shift || usage
  [[ -f "$file" ]] || { echo "mutate: no such file: $file" >&2; exit 1; }
  limit=15; build_cmd=""
  while [[ $# -gt 0 && "${1:-}" != "--" ]]; do
    case "$1" in
      --limit) limit="${2:-15}"; shift 2 ;;
      --build) build_cmd="${2:-}"; shift 2 ;;
      *) usage ;;
    esac
  done
  [[ "${1:-}" == "--" ]] || usage
  shift
  [[ $# -ge 1 ]] || usage
  self="${BASH_SOURCE[0]}"
  before=$(python3 -c 'import hashlib,sys;print(hashlib.sha256(open(sys.argv[1],"rb").read()).hexdigest())' "$file")

  cands=$(python3 - "$file" "$limit" <<'PY'
import re, sys
path, limit = sys.argv[1], int(sys.argv[2])
lines = open(path, encoding='utf-8').read().split('\n')
counts = {}
for l in lines:
    counts[l] = counts.get(l, 0) + 1
# A call with >=2 arguments that are bare identifiers, possibly &-borrowed.
# Deliberately NOT expressions: swapping `foo()` with `bar.baz()` changes far
# more than a direction and its failure says nothing about a seam.
CALL = re.compile(r'\b([a-z_][a-z0-9_]*)\(\s*(&?[a-z_][a-z0-9_]*)\s*,\s*(&?[a-z_][a-z0-9_]*)\s*[,)]')
SKIP = {'assert_eq', 'assert_ne', 'min', 'max', 'swap', 'zip', 'format', 'write', 'writeln',
        'push', 'insert', 'replace', 'splitn', 'saturating_sub', 'checked_sub', 'eq', 'ne'}
out, skipped_dup, skipped_same, skipped_nocall, skipped_text = [], 0, 0, 0, 0
for i, l in enumerate(lines):
    st = l.strip()
    if not st or st.startswith(('//', '#', '*', '/*')):
        skipped_text += 1
        continue
    m = CALL.search(l)
    if not m:
        skipped_nocall += 1
        continue
    fn, a, b = m.group(1), m.group(2), m.group(3)
    # Identical arguments cannot be swapped into a different meaning, and a
    # SKIP-listed callee is symmetric by definition — reporting either would be
    # noise that teaches skipping the real ones.
    if a == b or fn in SKIP:
        skipped_same += 1
        continue
    if counts[l] != 1:
        skipped_dup += 1
        continue
    lo, hi = m.span(2), m.span(3)
    new = l[:lo[0]] + b + l[lo[1]:hi[0]] + a + l[hi[1]:]
    out.append((i + 1, l, new, f"{fn}({a},{b}) -> ({b},{a})"))
assert len(out) + skipped_dup + skipped_same + skipped_nocall + skipped_text == len(lines), \
    "seams accounting lost a line: some exclusion is not counted"
print(f"#META\t{len(out)}\t{skipped_dup}\t{skipped_same}\t{skipped_nocall}\t{skipped_text}\t{len(lines)}")
for ln, old, new, label in out[:limit]:
    print(f"{ln}\t{old}\t{new}\t{label}")
PY
  ) || { echo "mutate seams: could not enumerate call sites" >&2; exit 1; }

  meta=$(printf '%s\n' "$cands" | head -1)
  n_all=$(printf '%s' "$meta" | cut -f2)
  n_dup=$(printf '%s' "$meta" | cut -f3)
  n_same=$(printf '%s' "$meta" | cut -f4)
  n_nocall=$(printf '%s' "$meta" | cut -f5)
  n_text=$(printf '%s' "$meta" | cut -f6)
  n_lines=$(printf '%s' "$meta" | cut -f7)
  body=$(printf '%s\n' "$cands" | tail -n +2)
  n_run=$(printf '%s\n' "$body" | grep -c . || true)
  echo "mutate seams: $file — $n_all swappable call site(s), running $n_run (limit $limit)."
  echo "mutate seams: of $n_lines lines: $n_all swappable, $n_dup non-unique, $n_same symmetric or"
  echo "mutate seams: identical args, $n_nocall no qualifying call, $n_text comment or blank."
  [ -n "$build_cmd" ] || echo "mutate seams: no --build given, so HELD-BY-TYPES cannot be told from KILLED."
  echo ""
  held=0; killed=0; survived=0
  while IFS=$'\t' read -r ln old new label; do
    [ -n "$ln" ] || continue
    verdict=""
    if [ -n "$build_cmd" ]; then
      if ! "$self" run "$file" "$old" "$new" -- bash -c "$build_cmd" >/dev/null 2>&1; then
        verdict="held-by-types"; held=$((held + 1))
      fi
    fi
    if [ -z "$verdict" ]; then
      if "$self" run "$file" "$old" "$new" -- "$@" >/dev/null 2>&1; then
        verdict="SURVIVED"; survived=$((survived + 1))
      else
        verdict="killed"; killed=$((killed + 1))
      fi
    fi
    printf '  %-14s L%-5s %s\n' "$verdict" "$ln" "$label"
    now=$(python3 -c 'import hashlib,sys;print(hashlib.sha256(open(sys.argv[1],"rb").read()).hexdigest())' "$file")
    if [ "$now" != "$before" ]; then
      echo "" >&2
      echo "mutate seams: ABORTING — $file did not return to its starting bytes after L$ln." >&2
      exit 3
    fi
  done <<< "$body"
  echo ""
  echo "mutate seams: $held held-by-types, $killed killed, $survived SURVIVED."
  if [ "$survived" -gt 0 ]; then
    echo "mutate seams: a SURVIVED swap compiled and passed every test. Two arguments that"
    echo "mutate seams: must mean different things, and nothing anywhere says so. Some are"
    echo "mutate seams: harmless (genuinely interchangeable); each is a question about which"
    echo "mutate seams: DIRECTION is authoritative, which is the thing no per-component test"
    echo "mutate seams: can answer (AF-438, and mvs-pitr's four)."
  fi
  if [ "$n_all" -gt "$n_run" ]; then
    echo "mutate seams: $((n_all - n_run)) call site(s) NOT run (--limit $limit); this result"
    echo "mutate seams: describes the $n_run that were."
  fi
  exit 0
fi

# ── `survey`: WHICH LINES DOES THIS COMMAND ACTUALLY DEPEND ON? (AF-422) ─────
#
# `run` answers "can THIS check fail", one mutation at a time, and it only gets
# used when you already suspect the answer. The eight-instance cluster on AF-422
# is what happens when you do not suspect it: six of the eight were caught by a
# compiler, a peer, or an unrelated second look, on checks their authors had
# just written and believed. Rule 7 already names the class and names this
# script. The gap was never the knowledge, it was that running it cost enough
# thought to skip.
#
# So: point it at a file and a command, and it tells you which lines the
# command's outcome does not depend on.
#
#   scripts/mutate.sh survey <file> [--limit N] [--stop-at REGEX] -- <command...>
#
# A SURVIVOR IS NOT AUTOMATICALLY A BUG. Log strings, error messages and
# defensive branches survive honestly, and a survey that demanded zero survivors
# would be the gate with no truthful path that ethos rule 3 forbids. It is a
# reading list: for each survivor, either the check does not cover it or it did
# not need covering, and only you can say which.
#
# Mutations are line-scoped and syntax-preserving, chosen so the file still
# compiles: comparison and boolean operators flip, boolean literals flip, shell
# emptiness tests invert. Each one goes through the same apply/trap-revert path
# as `run`, so the blast radius and the duration bound are identical.
#
# It SKIPS what it cannot do safely and says how many, because a survey that
# silently examined 9 of 30 lines and reported "all killed" is the exact shape
# this file exists to stop. Non-unique lines are skipped (mutate needs exactly
# one occurrence) and so is everything at or after `--stop-at`, which defaults
# to the first `#[cfg(test)]`: mutating the tests instead of the code under test
# measures nothing.
if [[ "${1:-}" == "survey" ]]; then
  shift
  file="${1:-}"; shift || usage
  [[ -f "$file" ]] || { echo "mutate: no such file: $file" >&2; exit 1; }
  limit=20; stop_at='^#\[cfg\(test\)\]'
  while [[ $# -gt 0 && "${1:-}" != "--" ]]; do
    case "$1" in
      --limit)   limit="${2:-20}"; shift 2 ;;
      --stop-at) stop_at="${2:-}"; shift 2 ;;
      *) usage ;;
    esac
  done
  [[ "${1:-}" == "--" ]] || usage
  shift
  [[ $# -ge 1 ]] || usage
  self="${BASH_SOURCE[0]}"
  before=$(python3 -c 'import hashlib,sys;print(hashlib.sha256(open(sys.argv[1],"rb").read()).hexdigest())' "$file")

  cands=$(python3 - "$file" "$stop_at" "$limit" <<'PY'
import re, sys
path, stop_at, limit = sys.argv[1], sys.argv[2], int(sys.argv[3])
lines = open(path, encoding='utf-8').read().split('\n')
stop = len(lines)
if stop_at:
    for i, l in enumerate(lines):
        if re.search(stop_at, l):
            stop = i
            break
# (pattern, replacement) applied to the FIRST occurrence in a line. Ordered so
# the more meaningful flips are tried first when a line admits several.
RULES = [
    (' == ', ' != '), (' != ', ' == '),
    (' && ', ' || '), (' || ', ' && '),
    (' < ', ' <= '), (' > ', ' >= '),
    ('-lt ', '-le '), ('-gt ', '-ge '),
    ('-ne ', '-eq '), ('-eq ', '-ne '),
    ('[ -n ', '[ -z '), ('[ -z ', '[ -n '),
    ('true', 'false'), ('false', 'true'),
]
counts = {}
for l in lines:
    counts[l] = counts.get(l, 0) + 1
# EVERY EXCLUSION IS COUNTED, AND THEY MUST SUM (ts-gke, 2026-09-03: apply the
# positive control to a filter's EXCLUSIONS, not only to its matches).
# `skipped_nomut` was computed here and never printed, and comment/blank lines
# were dropped with no counter at all — so "84 mutable lines found" could not be
# told from "84 found out of 1391 scanned, 900 of which I silently ignored".
# That is precisely the property this tool's own docstring claims to have.
out, skipped_dup, skipped_nomut, skipped_text = [], 0, 0, 0
for i, l in enumerate(lines[:stop]):
    st = l.strip()
    if not st or st.startswith(('//', '#', '*', '/*')):
        skipped_text += 1
        continue
    hit = None
    for a, b in RULES:
        if a in l:
            hit = (a, b)
            break
    if hit is None:
        skipped_nomut += 1
        continue
    if counts[l] != 1:
        skipped_dup += 1
        continue
    a, b = hit
    out.append((i + 1, l, l.replace(a, b, 1), f"{a.strip()} -> {b.strip()}"))
# The identity that makes a silent drop impossible: every scanned line is in
# exactly one bucket. A future rule that `continue`s without a counter breaks
# this loudly instead of quietly shrinking the survey.
assert len(out) + skipped_dup + skipped_nomut + skipped_text == stop, (
    f"survey accounting lost {stop - (len(out) + skipped_dup + skipped_nomut + skipped_text)} "
    f"line(s): some exclusion is not counted")
print(f"#META\t{len(out)}\t{skipped_dup}\t{stop}\t{len(lines)}\t{skipped_nomut}\t{skipped_text}")
for ln, old, new, label in out[:limit]:
    print(f"{ln}\t{old}\t{new}\t{label}")
PY
  ) || { echo "mutate survey: could not enumerate candidates" >&2; exit 1; }

  meta=$(printf '%s\n' "$cands" | head -1)
  n_all=$(printf '%s' "$meta" | cut -f2)
  n_dup=$(printf '%s' "$meta" | cut -f3)
  stop_line=$(printf '%s' "$meta" | cut -f4)
  n_lines=$(printf '%s' "$meta" | cut -f5)
  n_norule=$(printf '%s' "$meta" | cut -f6)
  n_text=$(printf '%s' "$meta" | cut -f7)
  body=$(printf '%s\n' "$cands" | tail -n +2)
  n_run=$(printf '%s\n' "$body" | grep -c . || true)
  echo "mutate survey: $file — $n_all mutable line(s) found, running $n_run (limit $limit)."
  echo "mutate survey: scope: lines 1-$stop_line of $n_lines (--stop-at '$stop_at')."
  echo "mutate survey: of those $stop_line: $n_all mutable, $n_dup non-unique, $n_norule with no"
  echo "mutate survey: applicable rule, $n_text comment or blank. Every scanned line is in exactly"
  echo "mutate survey: one bucket and the four are asserted to sum, so no exclusion is silent."
  echo ""
  killed=0; survived=0; survivors=""
  while IFS=$'\t' read -r ln old new label; do
    [ -n "$ln" ] || continue
    if "$self" run "$file" "$old" "$new" -- "$@" >/dev/null 2>&1; then
      survived=$((survived + 1))
      survivors="$survivors
  SURVIVED L$ln  ($label)  $(printf '%s' "$old" | sed 's/^[[:space:]]*//' | cut -c1-72)"
      printf '  SURVIVED L%-5s %-14s %s\n' "$ln" "$label" "$(printf '%s' "$old" | sed 's/^[[:space:]]*//' | cut -c1-64)"
    else
      killed=$((killed + 1))
      printf '  killed   L%-5s %-14s %s\n' "$ln" "$label" "$(printf '%s' "$old" | sed 's/^[[:space:]]*//' | cut -c1-64)"
    fi
    now=$(python3 -c 'import hashlib,sys;print(hashlib.sha256(open(sys.argv[1],"rb").read()).hexdigest())' "$file")
    if [ "$now" != "$before" ]; then
      echo "" >&2
      echo "mutate survey: ABORTING — $file did not return to its starting bytes after L$ln." >&2
      echo "mutate survey: A survey that keeps going from here mutates a file it no longer" >&2
      echo "mutate survey: understands, on a shared checkout. Inspect with: git diff -- $file" >&2
      exit 3
    fi
  done <<< "$body"
  echo ""
  echo "mutate survey: $killed killed, $survived SURVIVED."
  if [ "$survived" -gt 0 ]; then
    echo "mutate survey: a survivor is a line whose value the command's outcome does not"
    echo "mutate survey: depend on. Some are honest — log strings, error text, defensive"
    echo "mutate survey: branches. Each one is a question, not a verdict: is this uncovered,"
    echo "mutate survey: or did it not need covering? Only you can answer that."
  fi
  if [ "$n_all" -gt "$n_run" ]; then
    echo "mutate survey: $((n_all - n_run)) mutable line(s) were NOT run (--limit $limit). This"
    echo "mutate survey: result describes the $n_run that were, and says nothing about the rest."
  fi
  exit 0
fi

if [[ "${1:-}" == "run" ]]; then
  shift
  [[ $# -ge 5 ]] || usage
  file="$1"; old="$2"; new="$3"; shift 3
  [[ "${1:-}" == "--" ]] || usage
  shift
  [[ -f "$file" ]] || { echo "mutate: no such file: $file" >&2; exit 1; }
  refuse_self_mutation "$file"
  self="${BASH_SOURCE[0]}"
  "$self" apply "$file" "$old" "$new" || exit 1
  # Armed only AFTER a successful apply: a trap set earlier would "revert" a
  # mutation that never landed, and on a shared checkout that writes bytes
  # nobody asked for.
  # DISARM FIRST, THEN REVERT. A killed command fires TERM and then EXIT, so a
  # naive trap reverts twice — and the second pass finds 0 occurrences and
  # prints "NOT applied", which reads as a FAILED restore immediately after a
  # successful one. The file was fine both times; only the message lied. That is
  # the shape this repo keeps filing, so it does not ship in the tool that exists
  # to prevent it.
  trap 'trap - EXIT INT TERM; "$self" revert "$file" "$old" "$new" >&2' EXIT INT TERM
  "$@"
  rc=$?
  echo "mutate run: command exited $rc; reverting" >&2
  exit "$rc"
fi

[[ $# -eq 4 ]] || usage
op="$1"; file="$2"; old="$3"; new="$4"
[[ -f "$file" ]] || { echo "mutate: no such file: $file" >&2; exit 1; }
refuse_self_mutation "$file"

case "$op" in
  apply)  from="$old"; to="$new" ;;
  revert) from="$new"; to="$old" ;;
  *) usage ;;
esac

python3 - "$file" "$from" "$to" "$op" <<'PY'
import sys
path, frm, to, op = sys.argv[1:5]
s = open(path, encoding='utf-8').read()

# APPLY MUST NOT CREATE AN AMBIGUOUS REVERT (AMUX-3682, hit while using this).
#
# `apply` replaced a line with `cp "$SCRIPT_DIR/$rel" "$dest"`, a string the file
# ALREADY contained in a fallback branch. So the file then held two copies, and
# `revert` correctly refused as ambiguous — printing to stderr and LEAVING THE
# FILE MUTATED. A later `bash -n` passed, the suite was re-run, and only a diff
# against git showed the installer was still carrying the mutation.
#
# That is this tool's own failure mode: the refusal was right, but it fired at
# revert time when the damage was already done and the operator had moved on.
# Checked here instead, where refusing costs nothing — and it is the same
# principle the ethos file states about a gate whose refusal destroys the
# evidence needed to satisfy it.
if op == 'apply' and s.count(to) > 0:
    print(f"mutate apply: the replacement already occurs {s.count(to)} time(s) in {path} — "
          f"revert would be ambiguous and would leave the file mutated. "
          f"Pick a replacement unique to this file. NOT applied.", file=sys.stderr)
    sys.exit(1)

n = s.count(frm)
if n != 1:
    # Both directions are failures worth stopping on. 0 means the mutation never
    # landed, and a green suite after that says nothing about the tests. >1 means
    # it would land in places you did not choose.
    print(f"mutate {op}: target occurs {n} times in {path} — need exactly 1. NOT applied.",
          file=sys.stderr)
    sys.exit(1)
open(path, 'w', encoding='utf-8').write(s.replace(frm, to, 1))
print(f"mutate {op}: LANDED in {path}")
PY
