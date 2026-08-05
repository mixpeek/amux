#!/usr/bin/env python3
"""Syntax-check the dashboard's inline <script> blocks.

The check this replaces was:

    m = re.search(r'<script>\\s*\\n(.*?)</script>', content, re.DOTALL)

`re.search` returns the FIRST match, so it validated 2,380 of the file's
~1.27M characters of client code. Injecting a deliberate syntax error into 10
of the 11 inline blocks — including the 1MB main dashboard script — left it
green. A gate that cannot see the thing it guards is worse than no gate.

Scanning raw text turns up two things that look like a script block and are not:

  1. A Python regex literal:  re.sub(r"<script\\b.*?</script>", "", frag)
     Excluded by requiring a well-formed opening tag, so `<script\\b` misses.

  2. An f-string that BUILDS a script at runtime:
     f'<script>window._AMUX_AUTH_TOKEN={_json.dumps(AUTH_TOKEN)};'
     Only valid JS after Python formats it. Checked with each interpolation
     stubbed as `0`, which validates the template's SHAPE (a dropped brace or
     paren still fails) without pretending to know the values.

So the walk is over the AST, not the text. Anything the AST walk does not
reach is listed as an explicit coverage gap rather than passing in silence —
silence is how the original gap survived.
"""
import ast
import os
import re
import subprocess
import sys
import tempfile

SCRIPT_RE = re.compile(r"<script(?![^>]*\bsrc=)(?:\s[^>]*)?>(.*?)</script>",
                       re.DOTALL | re.IGNORECASE)

path = sys.argv[1] if len(sys.argv) > 1 else "amux-server.py"
src = open(path).read()
tree = ast.parse(src)


def blocks_in(text, lineno, kind):
    return [(lineno, kind, m.group(1)) for m in SCRIPT_RE.finditer(text)
            if m.group(1).strip()]


found = []
for node in ast.walk(tree):
    if isinstance(node, ast.Constant) and isinstance(node.value, str):
        found += blocks_in(node.value, node.lineno, "literal")
    elif isinstance(node, ast.JoinedStr):
        flat = "".join(v.value if isinstance(v, ast.Constant) and isinstance(v.value, str)
                       else "0" for v in node.values)
        found += blocks_in(flat, node.lineno, "runtime-built")

if not found:
    print("ERROR: no inline <script> blocks found — the extractor is broken, "
          "which is exactly the failure this check exists to catch", file=sys.stderr)
    sys.exit(1)

failed = 0
for lineno, kind, body in found:
    fd, tmp = tempfile.mkstemp(suffix=".js")
    os.write(fd, body.encode())
    os.close(fd)
    r = subprocess.run(["node", "--check", tmp], capture_output=True, text=True)
    os.unlink(tmp)
    if r.returncode != 0:
        failed += 1
        print(f"--- {kind} <script> in the string starting at {path}:{lineno} ---",
              file=sys.stderr)
        print(r.stderr.replace(tmp, "<script block>"), file=sys.stderr)

# Coverage report. A block whose <script> and </script> live in DIFFERENT
# string literals (concatenation, or an f-string chain) is not reachable
# verbatim by the AST walk, so it may be unchecked or checked only in its
# stubbed form. Naming these keeps the remaining gap discoverable instead of
# silent — silence is exactly how the original gap survived.
seen = {b for _, _, b in found}
gaps = [(src.count("\n", 0, m.start(1)) + 1, len(m.group(1)))
        for m in SCRIPT_RE.finditer(src)
        if m.group(1).strip() and m.group(1) not in seen]

total = sum(len(b) for _, _, b in found)
if failed:
    print(f"JS FAILED: {failed}/{len(found)} inline script block(s) do not parse",
          file=sys.stderr)
    sys.exit(1)
print(f"JS OK — {len(found)} inline script blocks, {total:,} chars checked")
for line, size in gaps:
    print(f"  note: {size} chars at {path}:{line} span string boundaries — "
          f"not verified verbatim (see the coverage comment in this script)")
