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

usage() { echo "usage: $0 <apply|revert> <file> <old-string> <new-string>" >&2; exit 2; }
[[ $# -eq 4 ]] || usage
op="$1"; file="$2"; old="$3"; new="$4"
[[ -f "$file" ]] || { echo "mutate: no such file: $file" >&2; exit 1; }

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
