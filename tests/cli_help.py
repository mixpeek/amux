"""`amux help` names every top-level verb the CLI dispatches, and nothing else.

Reads the shipped dispatch table and the shipped help output, so a verb added
without a help line, or a help line left behind for a removed verb, fails here.
"""
import os
from pathlib import Path
import re
import subprocess
import tempfile
import unittest

ROOT = Path(__file__).resolve().parents[1]
CLI = ROOT / "amux"

# Dispatched on purpose but kept out of `amux help`. Each needs a reason; an
# entry that is no longer dispatched fails test_hidden_verbs_are_still_dispatched.
HIDDEN = {
    "tunnel": "retired: prints the `amux publish` replacement and exits 2",
    "order": "owner-configured harness capability for agents (.claude/rules/harness-order.md)",
    "cdp": "agent browser rung below `amux browser`; driven through the chrome-cdp skill",
}


def dispatched_verbs():
    """{primary verb: aliases} from the top-level `case "${1:-}" in` dispatch."""
    text = CLI.read_text()
    guard = text.index('if [[ "${BASH_SOURCE[0]:-$0}" == "${0}" ]]; then')
    start = text.index('  case "${1:-}" in', guard)
    end = text.index("\n  esac", start)
    verbs = {}
    for m in re.finditer(r"^    ([a-z][a-z_-]*(?:\|[-a-z]+)*)\)", text[start:end], re.M):
        names = m.group(1).split("|")
        verbs[names[0]] = [n for n in names[1:] if not n.startswith("-")]
    return verbs


def help_verbs():
    with tempfile.TemporaryDirectory(prefix="amux-cli-help-") as home:
        env = {"PATH": os.environ.get("PATH", "/usr/bin:/bin"), "HOME": home,
               "AMUX_HOME": str(Path(home) / "data"), "NO_COLOR": "1"}
        out = subprocess.run([str(CLI), "help"], env=env, text=True,
                             capture_output=True, timeout=15, check=True).stdout
    out = re.sub(r"\x1b\[[0-9;]*m", "", out)
    return set(re.findall(r"^\s+amux ([a-z][a-z-]*)", out, re.M))


class HelpCoversDispatch(unittest.TestCase):
    def setUp(self):
        self.dispatched = dispatched_verbs()
        self.helped = help_verbs()

    def test_dispatch_table_was_read(self):
        # A parser that finds nothing would make every other check vacuous.
        self.assertGreater(len(self.dispatched), 50, self.dispatched)
        for verb in ("send", "board", "help", "version"):
            self.assertIn(verb, self.dispatched)

    def test_every_dispatched_verb_is_in_help(self):
        missing = sorted(v for v in self.dispatched
                         if v not in self.helped and v not in HIDDEN and v != "help")
        self.assertEqual(missing, [], "dispatched verbs with no `amux help` line; "
                         "document them or add them to HIDDEN with a reason")

    def test_every_help_verb_is_dispatched(self):
        names = {n for primary, aliases in self.dispatched.items() for n in [primary, *aliases]}
        stale = sorted(v for v in self.helped if v not in names)
        self.assertEqual(stale, [], "`amux help` names verbs the CLI does not dispatch")

    def test_hidden_verbs_are_still_dispatched(self):
        self.assertEqual(sorted(set(HIDDEN) - set(self.dispatched)), [])

    def test_hidden_verbs_stay_out_of_help(self):
        self.assertEqual(sorted(set(HIDDEN) & self.helped), [])


if __name__ == "__main__":
    unittest.main(verbosity=2)
