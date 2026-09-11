#!/usr/bin/env python3
"""The real CLI must expose durable assignment text beyond a card preview."""
import json
import os
from pathlib import Path
import subprocess
import tempfile
import unittest


class BoardMessageTests(unittest.TestCase):
    def test_preview_and_explicit_full_assignment(self):
        repo = Path(__file__).resolve().parents[1]
        cli = os.environ.get('AMUX_BIN', str(repo / 'amux'))
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            (root / 'bin').mkdir()
            fixture = {'id': 'TEST-1', 'status': 'doing', 'type': 'chore',
                       'desc': '**Prompt:** ' + 'preview ' * 40,
                       'messages': [{'id': 79, 'type': 'user', 'origin': '',
                                     'text': 'preview ' * 40 + 'Required row_count and @/tmp/uploads/fruit.csv'}]}
            (root / 'response.json').write_text(json.dumps(fixture))
            curl = root / 'bin/curl'
            curl.write_text('#!/bin/sh\ncat "$BOARD_MESSAGE_FIXTURE"\n')
            curl.chmod(0o755)
            env = dict(os.environ, PATH=str(root / 'bin') + ':' + os.environ['PATH'],
                       AMUX_API='http://127.0.0.1:9', AMUX_URL='http://127.0.0.1:9',
                       AMUX_SESSION='board-message-fixture', AMUX_HOME=str(root / 'home'),
                       BOARD_MESSAGE_FIXTURE=str(root / 'response.json'))
            def run(*args):
                return subprocess.run(['bash', cli, 'board', 'show', 'TEST-1', *args],
                                      env=env, capture_output=True, text=True, timeout=10)
            preview = run()
            self.assertEqual(preview.returncode, 0, preview.stderr)
            self.assertIn('MSG-79', preview.stdout)
            self.assertIn('amux board show TEST-1 --messages', preview.stdout)
            self.assertNotIn('Required row_count', preview.stdout)
            full = run('--messages')
            self.assertEqual(full.returncode, 0, full.stderr)
            self.assertIn('Required row_count and @/tmp/uploads/fruit.csv', full.stdout)
            invalid = run('--mesages')
            self.assertNotEqual(invalid.returncode, 0, 'misspelled flags must not silently show a preview')
            self.assertIn('Usage:', invalid.stderr)


if __name__ == '__main__':
    unittest.main()
