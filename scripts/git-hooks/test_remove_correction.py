#!/usr/bin/env python3
"""Exercise the shipped PreToolUse protocol without executing proposed commands."""
import hashlib
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

# Studio's recurring control flow, with fixture paths. Existence in origin was
# incorrectly treated as evidence that the local untracked copy could be removed.
STUDIO_RESYNC = '''WT=/workspace/.worktrees/studio-plg-backlog
for f in server/api/lineage/models.py server/tests/test_contract.py; do
  cd /workspace/mixpeek && git cat-file -e "origin/main:$f" 2>/dev/null && rm -f "$WT/$f" && echo "removed: $f" || echo "KEPT: $f"
done
git -C "$WT" checkout --detach origin/main
'''


class RemoveCorrectionTests(unittest.TestCase):
    def test_actual_invocations(self):
        cases = [
            STUDIO_RESYNC,
            'rm -f "$WT/$f"',
            'rm -- $WT/$f',
            'rm -rf "${WT}/"*',
            'rmdir "$WT/$f"',
            '/bin/rm -f "$WT"/"$f"',
            '/usr/bin/rm -f "$WT/$f"',
            'command rm -f "$WT/$f"',
            'env TAG=test rm -f "$WT/$f"',
            'exec rm -f "$WT/$f"',
            "'rm' -f \"$WT/$f\"",
            'if test -f "$WT/$f"; then rm -f "$WT/$f"; fi',
            'while true; do rm -f "$WT/$f"; break; done',
            '{ rm -f "$WT/$f"; }',
            'rm -f \\\n"$WT/$f"',
            'rm "${WT:-}/file"',
            'rm "${WT?defined but possibly empty}/file"',
            'rm "${1}/file"',
            'rm "$1/file"',
            'echo before; (rm "$WT/$f")',
            'echo $(rm "$WT/$f")',
            'bash <<\'SH\'\nrm -f "$WT/$f"\nSH',
            'rm /tmp/literal "$WT/$f"',
            '# earlier comment\nrm "$WT/$f"',
        ]
        for command in cases:
            with self.subTest(command=command):
                self.assertTrue(guard._dynamic_remove_operands(command))

    def test_inert_or_explicit_operands(self):
        cases = [
            'rm -- /tmp/owned/exact-file.py',
            'rm -f ./owned/file.py',
            'rm \'$WT/$f\'',
            'rm "\\$WT/file"',
            'rm \\$WT/file',
            'rm /tmp/owned/"$f"',
            'rm "${WT:?missing root}/$f"',
            'printf "%s" \'rm -f "$WT/$f"\'',
            'echo "rm -f $WT/$f"',
            'echo "example; rm -f $WT/$f"',
            'printf "%s" $(date) rm "$WT/$f"',
            'git commit -m \'fix rm "$WT/$f"\'',
            'python3 -c \'print("rm $WT/$f")\'',
            'cat <<\'EOF\'\nrm -f "$WT/$f"\nEOF',
            'cat <<EOF\nrm -f "$WT/$f"\nEOF',
            '# rm "$WT/$f"\necho done',
            'echo done # rm "$WT/$f"',
            'for rm in "$WT/$f"; do echo "$rm"; done',
            'command -v rm "$WT/$f"',
            'rm /tmp/file > "$WT/log"',
            'grep rm "$WT/$f"',
            '',
        ]
        for command in cases:
            with self.subTest(command=command):
                self.assertEqual(guard._dynamic_remove_operands(command), [])

    def run_hook(self, home, command, **overrides):
        env = dict(os.environ, HOME=str(home), AMUX_HOME=str(home / ".amux"),
                   AMUX_SESSION="studio-fixture", AMUX_WORKER="",
                   AMUX_SHARED_CHECKOUTS=str(home / "unrelated"),
                   AMUX_URL="http://127.0.0.1:9")
        env.update(overrides)
        return subprocess.run([sys.executable, str(HOOK)], text=True, capture_output=True,
                              input=json.dumps({"tool_name": "Bash", "tool_input": {"command": command},
                                                "cwd": str(home)}), env=env, timeout=10)

    def test_protocol_denies_before_execution_and_records_measured_signal(self):
        with tempfile.TemporaryDirectory() as tmp:
            home = Path(tmp)
            sentinel = home / "must-not-exist"
            command = f'touch "{sentinel}";\n' + STUDIO_RESYNC
            result = self.run_hook(home, command)
            self.assertEqual(result.returncode, 0, result.stderr)
            decision = json.loads(result.stdout)["hookSpecificOutput"]
            self.assertEqual(decision["hookEventName"], "PreToolUse")
            self.assertEqual(decision["permissionDecision"], "deny")
            self.assertIn("none of its commands ran", decision["permissionDecisionReason"])
            self.assertIn("existence, not equality", decision["permissionDecisionReason"])
            self.assertFalse(sentinel.exists())
            log = home / ".amux/logs/tool-corrections.jsonl"
            events = [json.loads(line) for line in log.read_text().splitlines()]
            self.assertEqual(len(events), 1)
            self.assertEqual(events[0]["event"], "unsafe_remove_operand")
            self.assertEqual(events[0]["session"], "studio-fixture")
            self.assertEqual(events[0]["verdict"], "deny")
            self.assertIs(events[0]["measured"], True)
            self.assertEqual(events[0]["n_considered"], 1)
            self.assertEqual(events[0]["command_sha256"], hashlib.sha256(command.encode()).hexdigest())
            self.assertNotIn("$WT", log.read_text())

    def test_refusal_names_each_operand_and_a_form_the_guard_accepts(self):
        # gs12 review: 306 whole calls rejected; the reason never said which word.
        cases = {
            '"$WT/$f"': '"${WT:?}/$f"',
            '$WT/$f': '${WT:?}/$f',
            '"${WT}/"*': '"${WT:?}/"*',
            '"${WT:-}/file"': '"${WT:?}/file"',
            '"$1/file"': '"${1:?}/file"',
            '"$WT"/"$f"': '"${WT:?}"/"$f"',
        }
        for raw, fixed in cases.items():
            with self.subTest(raw=raw):
                self.assertEqual(guard._guarded_operand(raw), fixed)
                self.assertTrue(guard._dynamic_remove_operands('rm -f ' + raw))
                self.assertEqual(guard._dynamic_remove_operands('rm -f ' + fixed), [],
                                 'the suggested form must pass the same scanner')
        with tempfile.TemporaryDirectory() as tmp:
            result = self.run_hook(Path(tmp), 'echo a; rm -f "$WT/$f"; rm "$D/x"')
            reason = json.loads(result.stdout)["hookSpecificOutput"]["permissionDecisionReason"]
            self.assertIn('Rejected operands: "$WT/$f" -> "${WT:?}/$f"; "$D/x" -> "${D:?}/x".', reason)

    def test_corrected_command_is_not_approved_or_executed(self):
        with tempfile.TemporaryDirectory() as tmp:
            home = Path(tmp)
            owned = home / "owned-file"
            owned.write_text("must survive a hook replay")
            result = self.run_hook(home, f'rm -f -- "{owned}"')
            self.assertEqual(result.returncode, 0, result.stderr)
            self.assertEqual(result.stdout, "")  # no allow decision: native checks still run
            self.assertTrue(owned.exists())
            self.assertFalse((home / ".amux/logs/tool-corrections.jsonl").exists())

    def test_unmanaged_claude_is_unchanged(self):
        with tempfile.TemporaryDirectory() as tmp:
            result = self.run_hook(Path(tmp), 'rm "$WT/$f"', AMUX_SESSION="", AMUX_WORKER="")
            self.assertEqual(result.returncode, 0, result.stderr)
            self.assertEqual(result.stdout, "")

    def test_worker_alias_and_isolated_workers_get_same_correction(self):
        with tempfile.TemporaryDirectory() as tmp:
            result = self.run_hook(Path(tmp), 'rm "$WT/$f"', AMUX_SESSION="",
                                   AMUX_WORKER="isolated-fixture", CC_ISOLATED="1")
            self.assertEqual(json.loads(result.stdout)["hookSpecificOutput"]["permissionDecision"], "deny")

    def test_unwritable_audit_does_not_defeat_correction(self):
        with tempfile.TemporaryDirectory() as tmp:
            home = Path(tmp)
            invalid_home = home / "file"
            invalid_home.write_text("not a directory")
            result = self.run_hook(home, 'rm "$WT/$f"', AMUX_HOME=str(invalid_home))
            self.assertEqual(json.loads(result.stdout)["hookSpecificOutput"]["permissionDecision"], "deny")

    def test_audit_rotates(self):
        with tempfile.TemporaryDirectory() as tmp:
            home = Path(tmp)
            log = home / ".amux/logs/tool-corrections.jsonl"
            log.parent.mkdir(parents=True)
            with log.open("wb") as stream:
                stream.truncate(4 * 1024 * 1024 + 1)
            result = self.run_hook(home, 'rm "$WT/$f"')
            self.assertEqual(result.returncode, 0, result.stderr)
            self.assertEqual(log.with_suffix(".jsonl.1").stat().st_size, 4 * 1024 * 1024 + 1)
            self.assertEqual(json.loads(log.read_text())["n_considered"], 1)


if __name__ == "__main__":
    unittest.main(verbosity=2)
