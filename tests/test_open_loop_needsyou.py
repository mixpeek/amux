"""Open-loop detection — a worker that stops on an unanswered question.

Regression cover for the 2026-08-05 loss: a design was approved, the session
asked "Does this look right before I write it up as a spec?", the conversation
moved to another topic, and the workstream became invisible. The advance loop
DID notice the card was stuck — five times — and wrote 'SKIPPED' into a card
log nobody opens.

Like tests/test_peek_parity.py, this loads the REAL functions out of
amux-server.py via AST rather than replicating them. Replication is how the
shipped text and the tested text drift apart, and the whole point here is that
the tested thing is the thing that runs.

Run:
    python3 tests/test_open_loop_needsyou.py
"""

import ast
import json
import os
import re
import tempfile
from pathlib import Path

_SERVER = os.path.join(os.path.dirname(__file__), "..", "amux-server.py")
_WANTED = {"_closing_question", "_iter_jsonl_tail"}


def _load():
    ns = {"re": re, "json": json, "Path": Path, "os": os}
    tree = ast.parse(open(_SERVER, encoding="utf-8").read())
    found = set()
    for node in tree.body:
        if isinstance(node, ast.FunctionDef) and node.name in _WANTED:
            exec(compile(ast.Module([node], []), _SERVER, "exec"), ns)
            found.add(node.name)
    missing = _WANTED - found
    if missing:
        raise AssertionError(f"could not load from amux-server.py: {missing} "
                             "(renamed? update this test rather than deleting it)")
    return ns


NS = _load()


def _jsonl(tmpdir, entries):
    p = Path(tmpdir) / "session.jsonl"
    with open(p, "w") as fh:
        for e in entries:
            fh.write(json.dumps(e) + "\n")
    return p


def _msg(role, text):
    return {"type": role, "message": {"role": role, "content": [{"type": "text", "text": text}]}}


def _closing(entries):
    """Run the REAL _closing_question against a fixture transcript."""
    with tempfile.TemporaryDirectory() as td:
        p = _jsonl(td, entries)
        NS["_session_jsonl_path"] = lambda name: p
        return NS["_closing_question"]("fixture")


CASES = [
    (
        "assistant ends on a question -> the ask is captured",
        [_msg("user", "build the summary tab"),
         _msg("assistant", "Here is the design.\n\nDoes this look right before I write it up as a spec?")],
        "Does this look right before I write it up as a spec?",
    ),
    (
        "assistant ends on a statement -> no open loop",
        [_msg("user", "ship it"),
         _msg("assistant", "Done and merged as PR #82.")],
        "",
    ),
    (
        "human spoke last -> the question is already answered",
        [_msg("assistant", "Does this look right?"),
         _msg("user", "yes, go ahead")],
        "",
    ),
    (
        "a system-reminder is not the human answering",
        [_msg("assistant", "Which account should I use?"),
         _msg("user", "<system-reminder>background task finished</system-reminder>")],
        "Which account should I use?",
    ),
    (
        "a '?' inside an unterminated code fence is not an ask",
        [_msg("user", "show me"),
         _msg("assistant", "Run this:\n\n```bash\ngrep 'what?' file\n")],
        "",
    ),
    (
        "markdown emphasis is stripped from the recorded ask",
        [_msg("user", "go"),
         _msg("assistant", "**Which one do you want?**")],
        "Which one do you want?",
    ),
    (
        "a tool-only final turn does not erase the preceding question",
        [_msg("user", "go"),
         _msg("assistant", "Which environment?"),
         {"type": "assistant", "message": {"role": "assistant",
          "content": [{"type": "tool_use", "id": "x", "name": "Bash", "input": {}}]}}],
        "Which environment?",
    ),
]


def main():
    failed = 0
    for label, entries, expect in CASES:
        got = _closing(entries)
        ok = got == expect
        if not ok:
            failed += 1
        print(f"{'PASS' if ok else 'FAIL'}  {label}")
        if not ok:
            print(f"      expected: {expect!r}")
            print(f"      got:      {got!r}")
    print(f"\n{failed} test(s) FAILED" if failed else "\nAll tests passed")
    raise SystemExit(1 if failed else 0)


if __name__ == "__main__":
    main()
