#!/usr/bin/env python3
"""Fail when runtime code hardcodes a specific person, machine or deployment.

amux is public and anyone can run install.sh. Names, email addresses, home
directories, hostnames and LAN addresses of whoever wrote a line must come
from configuration (server.env, the environment, git config, the OS), never
from source. See .claude/rules/no-hardcoded-specifics.md.

Scans tracked runtime files (plus the Markdown under skills/ and templates/,
which lanes read), skipping comments by each file's language, Rust
`#[cfg(test)]` modules and test files.

Known violations live in scripts/fixtures/hardcoded-specifics-baseline.txt as
`path<TAB>snippet`, one row per hit, where the snippet is the hit's code with
whitespace collapsed. The baseline is a ratchet by IDENTITY, not by count:
- a hit with no matching row fails (NEW), even if another hit in the same
  file was fixed in the same change;
- a row with no matching hit fails (STALE) until it is removed, so a fix
  cannot leave slack behind;
- with --base <ref>, a row that the base revision's baseline does not have
  fails (ADDED): rows can only ever be removed.

Run: python3 scripts/test-no-hardcoded-specifics.py [--base <git-ref>]
     python3 scripts/test-no-hardcoded-specifics.py --write-baseline
"""
import collections
import os
import re
import subprocess
import sys

ROOT = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
BASELINE_REL = "scripts/fixtures/hardcoded-specifics-baseline.txt"
BASELINE = os.path.join(ROOT, BASELINE_REL)

PATTERNS = [
    ("person name", re.compile(r"\bethan\b", re.I)),
    ("personal email", re.compile(r"\b[\w.+-]+@(mixpeek|trymixpeek)\.com\b", re.I)),
    ("upstream domain", re.compile(r"\b(try)?mixpeek\.com\b", re.I)),
    ("user home path", re.compile(r"/Users/[a-z][\w.-]*|/home/(?!\$|\{)[a-z][\w.-]*/")),
    ("github handle", re.compile(r"\besteininger\b", re.I)),
    ("LAN address", re.compile(r"\b(10\.\d{1,3}|192\.168|172\.(1[6-9]|2\d|3[01]))\.\d{1,3}\.\d{1,3}\b")),
]

RUNTIME_ROOTS = ("crates/", "scripts/", "templates/", "skills/")
RUNTIME_FILES = {"amux", "install.sh", "uninstall.sh", "amux-remote"}
MD_ROOTS = ("skills/", "templates/")
SKIP_PARTS = ("/tests/", "/test_", "/fixtures/", "/node_modules/", "/testdata/")
SKIP_NAME = re.compile(r"^(test[-_].*|.*[-_]test\.\w+|.*\.test\.\w+|.*_tests?\.rs)$")

C_LIKE = {".rs", ".js", ".mjs", ".cjs", ".ts", ".css"}
HASH = {".py", ".sh", ".bash", ".toml", ".env", ".service", ".timer", ".template", ""}
MARKUP = {".html", ".md"}
PLAIN = {".json"}


def lang(path):
    ext = os.path.splitext(path)[1]
    if ext in C_LIKE:
        return "c"
    if ext in HASH:
        return "hash"
    if ext in MARKUP:
        return "markup"
    if ext in PLAIN:
        return "plain"
    return None


def tracked():
    out = subprocess.run(["git", "-C", ROOT, "ls-files", "-z"], capture_output=True, check=True).stdout
    for p in out.decode().split("\0"):
        if not p:
            continue
        if not (p.startswith(RUNTIME_ROOTS) or p in RUNTIME_FILES):
            continue
        if any(s in "/" + p for s in SKIP_PARTS) or SKIP_NAME.match(os.path.basename(p)):
            continue
        if p.endswith(".md") and not p.startswith(MD_ROOTS):
            continue
        if lang(p) is None:
            continue
        yield p


def _strip_rust_literals(line, state):
    """Return `line` with string/char literal contents blanked, carrying an
    open raw-string terminator in state['raw'] across lines."""
    out = []
    i, n = 0, len(line)
    while i < n:
        if state["raw"] is not None:
            end = line.find(state["raw"], i)
            if end < 0:
                return "".join(out)
            i = end + len(state["raw"])
            state["raw"] = None
            continue
        if state["str"]:
            if line[i] == "\\":
                i += 2
                continue
            if line[i] == '"':
                state["str"] = False
            i += 1
            continue
        m = re.match(r'b?r(#*)"', line[i:])
        if m and (i == 0 or not (line[i - 1].isalnum() or line[i - 1] == "_")):
            state["raw"] = '"' + m.group(1)
            i += m.end()
            continue
        c = line[i]
        if c == "/" and line[i:i + 2] == "//":
            break
        if c == '"':
            state["str"] = True
            i += 1
            continue
        if c == "'":
            m = re.match(r"'(\\.|[^\\'])'", line[i:]) or re.match(r"'\\u\{[0-9a-fA-F]+\}'", line[i:])
            if m:
                i += m.end()
                continue
        out.append(c)
        i += 1
    return "".join(out)


def runtime_lines(path):
    try:
        with open(os.path.join(ROOT, path), encoding="utf-8") as f:
            lines = f.read().split("\n")
    except (UnicodeDecodeError, FileNotFoundError, IsADirectoryError):
        return
    kind = lang(path)
    is_rs = path.endswith(".rs")
    in_block = False
    skip_depth = None      # brace depth while inside a #[cfg(test)] module
    pending_test = False   # saw #[cfg(test)]; the next item decides
    lit = {"raw": None, "str": False}
    for i, line in enumerate(lines, 1):
        s = line.strip()
        if is_rs:
            code_only = _strip_rust_literals(line, lit)
            if skip_depth is not None:
                skip_depth += code_only.count("{") - code_only.count("}")
                if skip_depth <= 0:
                    skip_depth = None
                continue
            if pending_test:
                pending_test = False
                if re.match(r"(pub(\([\w:]+\))?\s+)?mod\s+\w+\s*\{", s):
                    depth = code_only.count("{") - code_only.count("}")
                    skip_depth = depth if depth > 0 else None
                    continue
            if s == "#[cfg(test)]":
                pending_test = True
                continue
        if kind == "c":
            if in_block:
                if "*/" in s:
                    in_block = False
                continue
            if s.startswith("/*"):
                if "*/" not in s:
                    in_block = True
                continue
            if s.startswith("//") or s.startswith("*"):
                continue
            code = re.sub(r"(?<![:\"'\w])//\s.*$", "", line)
        elif kind == "hash":
            if s.startswith("#") and not s.startswith("#!"):
                continue
            code = re.sub(r"\s#\s.*$", "", line)
        elif kind == "markup":
            if in_block:
                if "-->" in s:
                    in_block = False
                continue
            if s.startswith("<!--"):
                if "-->" not in s:
                    in_block = True
                continue
            code = line
        else:
            code = line
        yield i, code


def snippet(code):
    return " ".join(code.split())[:200]


def scan():
    hits = collections.defaultdict(list)
    for p in tracked():
        for n, code in runtime_lines(p):
            for label, rx in PATTERNS:
                if rx.search(code):
                    hits[p].append((n, label, snippet(code)))
                    break
    return hits


def parse_baseline(text):
    rows = collections.Counter()
    for raw in text.splitlines():
        if not raw.strip() or raw.startswith("#"):
            continue
        path, _, snip = raw.partition("\t")
        rows[(path, snip)] += 1
    return rows


def load_baseline():
    if not os.path.exists(BASELINE):
        return collections.Counter()
    with open(BASELINE, encoding="utf-8") as f:
        return parse_baseline(f.read())


def base_baseline(ref):
    r = subprocess.run(["git", "-C", ROOT, "show", f"{ref}:{BASELINE_REL}"],
                       capture_output=True, text=True)
    if r.returncode != 0:
        return None
    return parse_baseline(r.stdout)


def main(argv):
    hits = scan()
    if "--write-baseline" in argv:
        with open(BASELINE, "w", encoding="utf-8") as f:
            f.write("# path<TAB>snippet: one row per known hit. Remove rows as they are fixed; never add one.\n")
            for p in sorted(hits):
                for _, _, snip in sorted(hits[p], key=lambda h: h[2]):
                    f.write(f"{p}\t{snip}\n")
        print(f"wrote {sum(len(v) for v in hits.values())} rows to {BASELINE_REL}")
        return 0

    base = load_baseline()
    have = collections.Counter()
    where = collections.defaultdict(list)
    for p, rows in hits.items():
        for n, label, snip in rows:
            have[(p, snip)] += 1
            where[(p, snip)].append((n, label))
    fails = []
    for key in sorted(have):
        extra = have[key] - base.get(key, 0)
        if extra > 0:
            for n, label in where[key][-extra:]:
                fails.append(f"NEW   {key[0]}:{n} [{label}] {key[1]}")
    for key in sorted(base):
        missing = base[key] - have.get(key, 0)
        if missing > 0:
            fails.append(f"STALE {key[0]}: baseline row no longer matches; remove it ({missing}x): {key[1]}")

    if "--base" in argv:
        ref = argv[argv.index("--base") + 1]
        trusted = base_baseline(ref)
        if trusted is None:
            print(f"note  {BASELINE_REL} does not exist at {ref}; skipping the no-new-rows check (first introduction)")
        else:
            for key in sorted(base):
                added = base[key] - trusted.get(key, 0)
                if added > 0:
                    fails.append(f"ADDED {key[0]}: baseline row not in {ref}; rows may only be removed: {key[1]}")

    total = sum(have.values())
    print(f"hardcoded-specifics: {total} runtime hit(s) in {len(hits)} file(s); baseline has {sum(base.values())} row(s)")
    if fails:
        print("\n".join(fails))
        print("FAIL  move the value into configuration (see .claude/rules/no-hardcoded-specifics.md)")
        return 1
    print("ok    no hardcoded person, machine or deployment beyond the recorded baseline")
    return 0


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
