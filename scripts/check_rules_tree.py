#!/usr/bin/env python3
"""Keep the rule tree that VISION.md roots true (VISION.md, "The rule tree" in CLAUDE.md).

    scripts/check_rules_tree.py [--root DIR]     check, exit 1 naming each problem
    scripts/check_rules_tree.py --map            print the tree from the headers

Every tracked CLAUDE.md outside templates/ (those are copied into other projects) must:
  - start with `<!-- rules: parent=<path> owner=<worker> enforces=<A1,A5|all> -->`;
  - name as parent the nearest CLAUDE.md in a folder above it (VISION.md for the root one);
  - cite only invariant ids VISION.md defines (`### A<n>.` headings);
  - have AGENTS.md beside it as a symlink to CLAUDE.md, so other agents read the same rules.
VISION.md itself starts with `<!-- rules: root owner=<worker> -->` and defines each id once.

The map is computed from the headers on every run: there is no committed copy to go stale
(A10). Reads the working tree (or --root), or with --ref the committed bytes at that ref: the
pre-push hook checks each pushed head that way, so uncommitted files never decide a push.
"""
import argparse
import os
import re
import subprocess
import sys

HEADER = re.compile(r"^<!-- rules: parent=(\S+) owner=(\S+) enforces=(\S+) -->\s*$")
ROOT_HEADER = re.compile(r"^<!-- rules: root owner=(\S+) -->\s*$")
INVARIANT = re.compile(r"^### (A\d+)\. ", re.M)
SKIP = ("templates/", "node_modules/", ".worktrees/")


class Tree:
    """The files to check: a working tree (`root`), or the committed bytes at `ref`, which is
    what the pre-push hook checks so a shared checkout's uncommitted files never decide a push."""

    def __init__(self, root, ref=None):
        self.root, self.ref = root, ref
        if ref:
            out = subprocess.run(["git", "-C", root, "ls-tree", "-r", "--full-tree", ref],
                                 capture_output=True, text=True).stdout.splitlines()
            self.entries = {}
            for line in out:
                meta, path = line.split("\t", 1)
                mode, _, sha = meta.split()
                self.entries[path] = (mode, sha)
            self.paths = list(self.entries)
        else:
            self.paths = [p for p in subprocess.run(
                ["git", "-C", root, "ls-files", "--cached", "--others", "--exclude-standard"],
                capture_output=True, text=True).stdout.split("\n") if p and os.path.lexists(os.path.join(root, p))]

    def exists(self, rel):
        return rel in self.entries if self.ref else os.path.exists(os.path.join(self.root, rel))

    def read(self, rel):
        if self.ref:
            return subprocess.run(["git", "-C", self.root, "cat-file", "-p", self.entries[rel][1]],
                                  capture_output=True, text=True).stdout
        with open(os.path.join(self.root, rel), encoding="utf-8") as f:
            return f.read()

    def symlink_to(self, rel):
        """The target `rel` links to, or None when it is not a symlink."""
        if self.ref:
            e = self.entries.get(rel)
            return self.read(rel).strip() if e and e[0] == "120000" else None
        path = os.path.join(self.root, rel)
        return os.readlink(path) if os.path.islink(path) else None


def rule_files(tree):
    return sorted(p for p in tree.paths if os.path.basename(p) == "CLAUDE.md" and not p.startswith(SKIP))


def first_line(text):
    return text.split("\n", 1)[0].rstrip("\n")


def expected_parent(rel, files):
    """The nearest CLAUDE.md in a folder above `rel`'s folder, as a path relative to `rel`."""
    d = os.path.dirname(rel)
    if d == "":
        return "VISION.md"
    while True:
        d = os.path.dirname(d)
        cand = os.path.join(d, "CLAUDE.md") if d else "CLAUDE.md"
        if cand in files:
            return os.path.relpath(cand, os.path.dirname(rel))
        if d == "":
            return os.path.relpath("CLAUDE.md", os.path.dirname(rel))


def check(tree):
    problems = []
    if not tree.exists("VISION.md"):
        return ["VISION.md: missing; it is the root of the rule tree"], {}, []
    vision = tree.read("VISION.md")
    if not ROOT_HEADER.match(first_line(vision)):
        problems.append("VISION.md: first line must be `<!-- rules: root owner=<worker> -->`")
    ids = INVARIANT.findall(vision)
    dup = sorted({i for i in ids if ids.count(i) > 1})
    if dup:
        problems.append("VISION.md: invariant defined twice: " + ", ".join(dup))
    known = set(ids)
    files = rule_files(tree)
    found = {}
    for rel in files:
        line = first_line(tree.read(rel))
        m = HEADER.match(line)
        if not m:
            problems.append("%s: first line must be `<!-- rules: parent=<path> owner=<worker> enforces=<ids|all> -->` (found %r)"
                            % (rel, line[:80]))
            continue
        parent, owner, enforces = m.groups()
        want = expected_parent(rel, set(files))
        if parent != want:
            problems.append("%s: parent=%s, but the nearest rule file above it is %s" % (rel, parent, want))
        if enforces != "all":
            for i in enforces.split(","):
                if i not in known:
                    problems.append("%s: enforces %s, which VISION.md does not define (it has %s)"
                                    % (rel, i, ", ".join(ids) or "none"))
        agents = os.path.join(os.path.dirname(rel), "AGENTS.md")
        if tree.symlink_to(agents) != "CLAUDE.md":
            problems.append("%s: AGENTS.md beside it must be a symlink to CLAUDE.md (ln -s CLAUDE.md %s)"
                            % (rel, agents))
        found[rel] = (parent, owner, enforces)
    return problems, found, ids


def print_map(tree, ids):
    print("VISION.md  (invariants: %s)" % ", ".join(ids))
    def kids(of):
        return sorted(r for r, (p, _, _) in tree.items()
                      if os.path.normpath(os.path.join(os.path.dirname(r), p)) == of)
    def walk(node, depth):
        for r in kids(node):
            _, owner, enf = tree[r]
            print("%s%s  owner=%s enforces=%s" % ("  " * depth, r, owner, enf))
            walk(r, depth + 1)
    walk("VISION.md", 1)


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--root", default=None)
    ap.add_argument("--ref", default=None, help="check the committed bytes at this ref, not the working tree")
    ap.add_argument("--map", action="store_true")
    a = ap.parse_args()
    root = a.root or subprocess.run(["git", "rev-parse", "--show-toplevel"], capture_output=True, text=True).stdout.strip()
    problems, tree, ids = check(Tree(root, a.ref))
    if a.map:
        print_map(tree, ids)
    if problems:
        print("rules-tree: %d problem(s) in %d rule file(s) (verdict=rules_tree_broken)" % (len(problems), len(tree)))
        for p in problems:
            print("  " + p)
        return 1
    if not a.map:
        print("rules-tree: ok, %d rule file(s) under VISION.md, invariants %s (verdict=rules_tree_ok)"
              % (len(tree), ", ".join(ids)))
    return 0


if __name__ == "__main__":
    sys.exit(main())
