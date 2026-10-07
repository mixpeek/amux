#!/usr/bin/env python3
"""Exercise the shipped CLI with exact scope, executor moves and bad snapshots."""
import json
import os
from pathlib import Path
import subprocess
import tempfile
import unittest

ROOT = Path(__file__).resolve().parent.parent

class ScopeTests(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory()
        self.root = Path(self.tmp.name)
        self.addCleanup(self.tmp.cleanup)
        self.cards = [
            {"id": "P-1", "session": "lane-a", "title": "GS12 proof one", "status": "verified", "closed_at": 1791410000},
            {"id": "P-2", "session": "hub", "title": "Renamed required proof", "status": "todo"},
            {"id": "NEW", "session": "hub", "title": "GS12 proof added later", "status": "todo"},
            {"id": "PLAN", "session": "lane-b", "title": "GS12 plan item 1.1", "status": "verified", "closed_at": 1791410000},
        ]
        self.line = {"epic": "EPIC", "measured": True, "version": 7, "line": [{"id": "P-1"}, {"id": "P-2"}]}

    def run_cli(self, frozen=True):
        board = self.root / 'board.json'
        scope = self.root / 'scope.json'
        board.write_text(json.dumps(self.cards))
        scope.write_text(json.dumps(self.line))
        args = ['python3', str(ROOT / 'scripts/orch-pace.py'), '--orchestrator', 'hub', '--lane-prefix', 'lane-',
                '--deadline', '2026-10-09T12:00:00+00:00', '--now', '2026-10-07T22:00:00+00:00',
                '--proof-prefix', 'GS12 proof', '--plan-regex', r'^GS12 plan item', '--board-file', str(board), '--json']
        if frozen:
            args += ['--epic', 'EPIC', '--done-line-file', str(scope)]
        result = subprocess.run(args, env={**os.environ, 'HOME': str(self.root)}, capture_output=True, text=True)
        self.assertTrue(result.stdout, result.stderr)
        return result.returncode, json.loads(result.stdout)

    def test_frozen_ids_survive_renames_and_executor_moves(self):
        _, before = self.run_cli()
        self.assertEqual((before['proof_total'], before['proof_verified']), (2, 1))
        self.assertEqual(before['done_line']['version'], 7)
        self.assertEqual(before['plan']['items'], 1)
        self.cards[0]['session'] = 'outside-the-old-prefix'
        self.cards[0]['title'] = 'Renamed proof'
        _, after = self.run_cli()
        self.assertEqual((after['proof_total'], after['proof_verified']), (2, 1))
        self.assertEqual(after['done_line'], before['done_line'])

    def test_missing_archived_deleted_or_discarded_scope_never_shrinks(self):
        for field, value in [('status', 'discarded'), ('archived', True), ('deleted', 1)]:
            with self.subTest(field=field):
                self.cards[1][field] = value
                code, out = self.run_cli()
                self.assertEqual(code, 1)
                self.assertEqual(out['verdict'], 'UNMEASURED')
                self.assertFalse(out['measured'])
                del self.cards[1][field]
                self.cards[1]['status'] = 'todo'
        self.cards = [c for c in self.cards if c['id'] != 'P-2']
        code, out = self.run_cli()
        self.assertEqual(code, 1)
        self.assertIn('P-2', out['why_unmeasured'])

    def test_invalid_frozen_snapshot_is_not_a_zero_target(self):
        for line in [[], [{'id': 'P-1'}, {'id': 'P-1'}]]:
            self.line['line'] = line
            code, out = self.run_cli()
            self.assertEqual(code, 1)
            self.assertEqual(out['verdict'], 'UNMEASURED')

    def test_legacy_population_includes_proofs_assigned_to_workers(self):
        _, out = self.run_cli(False)
        self.assertEqual((out['proof_total'], out['proof_verified']), (2, 1))
        self.assertEqual(out['plan']['verified'], 1)

if __name__ == '__main__':
    unittest.main()
