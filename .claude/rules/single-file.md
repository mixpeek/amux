---
description: When editing amux-server.py — the single-file constraint
globs: ["amux-server.py"]
---

amux-server.py is one file containing Python server + inline HTML/CSS/JS. Never split it into multiple files or create separate modules. Always verify syntax after edits:

```bash
python3 -c "import ast; ast.parse(open('amux-server.py').read())"
```

The PostToolUse hook validates this automatically, but if you're making batch edits, run it manually before committing.
