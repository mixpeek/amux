#!/usr/bin/env python3
"""land_gate.rank_specs: the local e2e run keeps the specs that name the change.

Built on a scratch repo: one spec names an identifier the change adds, the
others name only words every spec shares. Run: python3 scripts/test-land-gate-rank.py
"""
import os, subprocess, sys, tempfile

sys.path.insert(0, os.path.join(os.path.dirname(os.path.abspath(__file__))))
import land_gate as g

failed = 0


def check(name, ok, detail=""):
    global failed
    print(("ok   " if ok else "FAIL ") + name + ("" if ok else ": " + detail))
    failed += not ok


def run(repo, *a):
    subprocess.run(["git", "-C", repo, *a], check=True, capture_output=True)


with tempfile.TemporaryDirectory() as repo:
    run(repo, "init", "-q")
    run(repo, "config", "user.email", "t@example.invalid")
    run(repo, "config", "user.name", "t")
    os.makedirs(os.path.join(repo, "e2e"))
    os.makedirs(os.path.join(repo, "crates", "s"))
    specs = {f"s{i}.spec.ts": "await request.post('/api/board', { data: { title } });\n" for i in range(8)}
    specs["s5.spec.ts"] += "expect(body.code).toBe('cross_board_create_unproven');\n"
    specs["s2.spec.ts"] += "await page.locator('.settings-tab-btn').click();\n"
    for n, t in specs.items():
        open(os.path.join(repo, "e2e", n), "w").write(t)
    open(os.path.join(repo, "crates", "s", "board.rs"), "w").write("fn create() {}\n")
    run(repo, "add", "-A")
    run(repo, "commit", "-qm", "base")
    open(os.path.join(repo, "crates", "s", "board.rs"), "w").write(
        "fn create() {\n    // a comment naming settings-tab-btn is not the change\n"
        "    return err(\"cross_board_create_unproven\", \"/api/board\");\n}\n")
    run(repo, "commit", "-qam", "change")
    names = sorted(specs)
    ranked = g.rank_specs(repo, "HEAD^", names)
    check("the spec naming the changed literal ranks first", ranked[:1] == ["s5.spec.ts"], str(ranked))
    check("every spec is kept without min_score", sorted(ranked) == names, str(ranked))
    kept = g.rank_specs(repo, "HEAD^", names, min_score=1)
    check("min_score=1 keeps only specs naming the change", kept == ["s5.spec.ts"], str(kept))
    check("a token in a comment line is not counted", "s2.spec.ts" not in kept, str(kept))

print("%d failed" % failed)
sys.exit(1 if failed else 0)
