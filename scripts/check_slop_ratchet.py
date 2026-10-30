#!/usr/bin/env python3
"""The slop ratchet: a push may not make a file it touches sloppier (VISION.md A4, A10).

    scripts/check_slop_ratchet.py [--base REF] [--head REF]   check the range, exit 1 on a raise
    scripts/check_slop_ratchet.py --report [--head REF]       totals across the repo

For each server Rust file and dashboard script the range changes, it counts these at the head
and at the base, and refuses when any count went up:
  - Rust (crates/*/src, before the first #[cfg(test)]):
      discarded_result  `let _ = ...`: a Result thrown away without a word
      unwrap / expect   `.unwrap()` / `.expect(`: a panic where an error belongs
  - Dashboard (crates/amux-dashboard/static/*.js, state/*.mjs):
      empty_catch       `catch (e) {}`: an error swallowed where no sweep can see it
      inline_style      `style="` / `style='`: a style that skips the shared classes and tokens
A file that grows past the god-file size (5,000 lines of Rust, 20,000 of JavaScript) is
reported, not refused: nearly every dashboard change adds to app.js until it is split
(docs/design/god-file-method.md).

A new file starts at zero and a rename keeps its counts. Each file is compared with ITS OWN
base, so slop another lane landed meanwhile never blocks an unrelated push. A justified raise
carries `Slop-Raise: <reason>` in a commit message of the range and is let through, logged.
There is no committed baseline: --report computes progress from the code (A10).
"""
import argparse
import re
import subprocess
import sys

RS_ROOT = "crates/"
JS_PREFIX = "crates/amux-dashboard/static/"
GOD = {".rs": 5000, ".js": 20000, ".mjs": 20000}
PATTERNS = {
    ".rs": {
        "discarded_result": re.compile(r"^\s*let _ = ", re.M),
        "unwrap": re.compile(r"\.unwrap\(\)"),
        "expect": re.compile(r"\.expect\("),
    },
    ".js": {
        "empty_catch": re.compile(r"catch\s*(?:\(\s*[\w$]*\s*\))?\s*\{\s*\}"),
        "inline_style": re.compile(r"style=[\"']"),
    },
}
PATTERNS[".mjs"] = PATTERNS[".js"]


def git(*args):
    return subprocess.run(["git"] + list(args), capture_output=True, text=True)


def in_scope(path):
    if path.endswith(".rs"):
        return path.startswith(RS_ROOT) and "/src/" in path and "/tests/" not in path
    if path.endswith((".js", ".mjs")):
        return path.startswith(JS_PREFIX) and not path.endswith(".min.js")
    return False


def ext(path):
    return ".mjs" if path.endswith(".mjs") else ".js" if path.endswith(".js") else ".rs"


def counts(path, text):
    if text is None:
        return {}, 0
    e = ext(path)
    body = text
    if e == ".rs":
        cut = body.find("#[cfg(test)]")
        if cut >= 0:
            body = body[:cut]
    c = {k: len(p.findall(body)) for k, p in PATTERNS[e].items()}
    return c, text.count("\n")


def show(ref, path):
    r = git("show", "%s:%s" % (ref, path))
    return r.stdout if r.returncode == 0 else None


def changed(base, head):
    """(path at head, path at base or None) for each file the range adds, edits or renames."""
    r = git("diff", "--name-status", "-M", base, head)
    out = []
    for line in r.stdout.splitlines():
        parts = line.split("\t")
        st = parts[0]
        if st.startswith("D"):
            continue
        if st.startswith("R"):
            old, new = parts[1], parts[2]
        elif st.startswith("A"):
            old, new = None, parts[1]
        else:
            old, new = parts[1], parts[1]
        if in_scope(new):
            out.append((new, old))
    return out


def raise_reason(base, head):
    msgs = git("log", "--format=%B", "%s..%s" % (base, head)).stdout
    m = re.search(r"^Slop-Raise:\s*(\S.*)$", msgs, re.M)
    return m.group(1).strip() if m else None


def check(base, head):
    raises, warns = [], []
    files = changed(base, head)
    for new, old in files:
        after, n_after = counts(new, show(head, new))
        before, n_before = counts(new, show(base, old)) if old else ({}, 0)
        for k, v in after.items():
            if v > before.get(k, 0):
                raises.append((new, k, before.get(k, 0), v))
        limit = GOD[ext(new)]
        if n_after > limit and n_after > n_before:
            warns.append((new, n_before, n_after, limit))
    return files, raises, warns


FIX = {
    "discarded_result": "handle the Result, or log it with a verdict (A4)",
    "unwrap": "return or log the error instead of panicking",
    "expect": "return or log the error instead of panicking",
    "empty_catch": "log the error (a beacon or console.warn with context), or let it propagate",
    "inline_style": "use a class in app.css with theme and spacing tokens",
}


def report(head):
    files = [p for p in git("ls-tree", "-r", "--name-only", head).stdout.splitlines() if in_scope(p)]
    total = {}
    gods = []
    for p in files:
        c, n = counts(p, show(head, p))
        for k, v in c.items():
            total[k] = total.get(k, 0) + v
        if n > GOD[ext(p)]:
            gods.append((n, p))
    print("slop report at %s: %d file(s) considered (verdict=slop_report measured=true n_considered=%d)"
          % (head, len(files), len(files)))
    for k in sorted(total):
        print("  %-17s %d" % (k, total[k]))
    print("  god files (%d):" % len(gods))
    for n, p in sorted(gods, reverse=True):
        print("    %7d  %s" % (n, p))
    return 0


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--base")
    ap.add_argument("--head", default="HEAD")
    ap.add_argument("--report", action="store_true")
    a = ap.parse_args()
    if a.report:
        return report(a.head)
    base = a.base or git("merge-base", "origin/main", a.head).stdout.strip()
    if not base:
        print("slop-ratchet: no base to compare with (verdict=slop_ratchet_unmeasured measured=false)")
        return 0
    files, raises, warns = check(base, a.head)
    for p, nb, na, lim in warns:
        print("slop-ratchet: WARN %s grew %d -> %d lines, past the god-file size %d (verdict=slop_god_file_grew); "
              "see docs/design/god-file-method.md" % (p, nb, na, lim))
    if not raises:
        print("slop-ratchet: ok, %d file(s) checked, no indicator went up (verdict=slop_ratchet_ok measured=true n_considered=%d)"
              % (len(files), len(files)))
        return 0
    why = raise_reason(base, a.head)
    print("slop-ratchet: %d indicator(s) went up in %d file(s) checked:" % (len(raises), len(files)))
    for p, k, b, v in raises:
        print("  %s: %s %d -> %d. Fix: %s" % (p, k, b, v, FIX[k]))
    if why:
        print("slop-ratchet: let through by Slop-Raise: %s (verdict=slop_raise_allowed)" % why)
        return 0
    print("Refused (verdict=slop_ratchet_raised). Remove the new slop, or add a `Slop-Raise: <reason>`\n"
          "trailer to a commit in the range when the raise is deliberate.")
    return 1


if __name__ == "__main__":
    sys.exit(main())
