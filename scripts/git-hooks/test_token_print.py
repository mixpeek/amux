#!/usr/bin/env python3
"""The guard refuses a Bash call that would print the GitHub App token."""
import importlib.util
import json
import os
from pathlib import Path
import subprocess
import sys
import tempfile
import unittest

HOOK = Path(os.environ.get("AMUX_GUARD_UNDER_TEST") or Path(__file__).with_name("git-shared-guard.py"))
spec = importlib.util.spec_from_file_location("shared_guard", HOOK)
guard = importlib.util.module_from_spec(spec)
spec.loader.exec_module(guard)

T = "~/.amux/github-app/get-token.sh"

PRINTS = [
    T,
    T + " --raw",
    T + " --min-remaining 1800",
    '"$HOME/.amux/github-app/get-token.sh" --raw',
    "bash " + T,
    "cd /tmp && " + T,
    T + " | cat",
    T + " --raw | tee /tmp/x",
    "echo $(" + T + " --raw)",
    'echo "$(' + T + ')"',
    T + " > /dev/stdout",
    "cat ~/.amux/github-app/.token-cache",
    "tail -n1 /Users/me/.amux/github-app/.token-cache",
    "cd ~/.amux/github-app && cat .token-cache",
]

CONSUMED = [
    'eval "$(' + T + ')"',
    'eval "$(' + T + ' --min-remaining 1800)"',
    "eval $(" + T + ")",
    "GH_TOKEN=$(" + T + " --raw) gh api rate_limit",
    'export GH_TOKEN="$(' + T + ' --raw)"',
    'tok="$("$HOME/.amux/github-app/get-token.sh" --min-remaining 600 --raw 2>/dev/null || true)"',
    T + " | sed -n 's/^export GH_TOKEN=//p' > /tmp/tok",
    T + " --raw > /tmp/tok",
    T + " --expires-in",
    T + " --help",
    # Named, not run.
    "grep -n get-token.sh scripts/pr-watch.sh",
    "sed -n 1,40p " + T,
    "ls -la ~/.amux/github-app/",
    "amux send x --stdin <<'EOF'\nrun " + T + " bare and it prints\nEOF",
    "git grep -n 'get-token.sh' origin/main",
]


class TokenPrintTests(unittest.TestCase):
    def setUp(self):
        self.home = tempfile.TemporaryDirectory()
        os.environ["AMUX_HOME"] = self.home.name

    def tearDown(self):
        self.home.cleanup()

    def test_printing_forms_are_refused(self):
        for command in PRINTS:
            with self.subTest(command=command):
                d = guard._token_print_verdict(command)
                self.assertIsNotNone(d, command)
                self.assertEqual(d["hookSpecificOutput"]["permissionDecision"], "deny")
                self.assertIn("--expires-in", d["hookSpecificOutput"]["permissionDecisionReason"])

    def test_consuming_and_naming_forms_pass(self):
        for command in CONSUMED:
            with self.subTest(command=command):
                self.assertIsNone(guard._token_print_verdict(command), command)

    def test_refusal_is_logged_without_the_command(self):
        guard._token_print_verdict(T + " --raw")
        rows = [json.loads(l) for l in (Path(self.home.name) / "logs/tool-corrections.jsonl").read_text().splitlines()]
        self.assertEqual(rows[-1]["event"], "token_print")
        self.assertEqual(rows[-1]["verdict"], "deny")
        self.assertNotIn("get-token", json.dumps(rows[-1]))

    def test_the_hook_entry_point_denies(self):
        payload = json.dumps({"tool_name": "Bash", "tool_input": {"command": T + " --raw"}, "cwd": "/tmp"})
        env = dict(os.environ, AMUX_HOME=self.home.name, AMUX_SESSION="")
        out = subprocess.run([sys.executable, str(HOOK)], input=payload, capture_output=True, text=True, env=env, timeout=30)
        self.assertEqual(json.loads(out.stdout)["hookSpecificOutput"]["permissionDecision"], "deny")


if __name__ == "__main__":
    unittest.main()
