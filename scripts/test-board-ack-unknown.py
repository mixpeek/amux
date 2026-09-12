#!/usr/bin/env python3
"""Exercise the real CLI with writes whose successful response is corrupted."""
import http.server
import json
import os
from pathlib import Path
import subprocess
import tempfile
import threading
import unittest

CLI = os.environ.get('AMUX_CLI_PATH', str(Path(__file__).resolve().parents[1] / 'amux'))

class BoardAckTests(unittest.TestCase):
    def run_case(self, broken):
        row = {'id': 'TEST-1', 'status': 'review', 'desc': 'original'}
        writes, beacons = [], []
        class Handler(http.server.BaseHTTPRequestHandler):
            def do_PATCH(self):
                body = json.loads(self.rfile.read(int(self.headers['Content-Length'])))
                field = next(iter(body)); writes.append(field)
                if 'desc_append' in body: row['desc'] += '\n' + body['desc_append']
                elif 'status' in body and broken == 'refusal':
                    self.reply({'error': 'gate not acknowledged', 'gate': ['reviewed'], 'item': 'TEST-1'}, 409); return
                else: row.update(body)
                # The write LANDED. Missing JSON cannot prove otherwise.
                if field == broken: self.reply(b'<html>response lost</html>')
                elif broken == 'array' and field == 'evidence': self.reply([])
                else: self.reply(dict(row))
            def do_GET(self): self.reply(dict(row))
            def do_POST(self):
                beacons.append(json.loads(self.rfile.read(int(self.headers['Content-Length'])))); self.reply({'ok': True})
            def reply(self, body, status=200):
                data = body if isinstance(body, bytes) else json.dumps(body).encode()
                self.send_response(status); self.send_header('Content-Length', str(len(data)))
                self.end_headers(); self.wfile.write(data)
            def log_message(self, *_): pass
        server = http.server.ThreadingHTTPServer(('127.0.0.1', 0), Handler)
        threading.Thread(target=server.serve_forever, daemon=True).start()
        try:
            with tempfile.TemporaryDirectory() as home:
                url = 'http://127.0.0.1:' + str(server.server_port)
                env = dict(os.environ, AMUX_API=url, AMUX_URL=url, CC_HOME=home,
                           AMUX_SESSION='ack-fixture', AMUX_WORKER='ack-fixture')
                result = subprocess.run(['bash', CLI, 'board', 'done', 'TEST-1', '--checked', 'reviewed', '--outcome-stdin'],
                    input='Outcome retained exactly.', text=True, capture_output=True, env=env, timeout=30)
                spool = Path(home, 'cli-transport.jsonl')
                events = [json.loads(line) for line in spool.read_text().splitlines()] if spool.exists() else []
                if broken not in ('success', 'refusal'):
                    self.assertNotEqual(result.returncode, 0)
                    self.assertIn('write outcome is unknown', result.stderr)
                    self.assertNotIn('NOT recorded', result.stderr)
                    self.assertNotIn('<html>', result.stderr)
                    self.assertEqual(events[-1]['verdict'], 'board_ack_unknown')
                    self.assertEqual(events[-1]['item'], 'TEST-1')
                    self.assertTrue(events[-1]['measured'])
                    self.assertEqual(events[-1]['n_considered'], 1)
                    # Next invocation delivers the durable diagnostic via the existing spool.
                    subprocess.run(['bash', CLI, 'board', 'show', 'TEST-1'], env=env, capture_output=True, timeout=30)
                    self.assertTrue(any(r.get('verdict') == 'board_ack_unknown' for b in beacons for r in b.get('rows', [])))
                return row, writes, result
        finally: server.shutdown(); server.server_close()

    def test_unknown_evidence_stops_before_status(self):
        row, writes, result = self.run_case('evidence')
        self.assertEqual(writes, ['evidence']); self.assertEqual(row['status'], 'review')
        self.assertEqual(row['evidence'], 'Outcome retained exactly.')
        self.assertIn('status transition was not attempted', result.stderr)

    def test_unknown_outcome_stops_before_status(self):
        row, writes, result = self.run_case('desc_append')
        self.assertEqual(writes, ['evidence', 'desc_append']); self.assertEqual(row['status'], 'review')
        self.assertIn('Outcome retained exactly.', row['desc'])
        self.assertIn('status transition was not attempted', result.stderr)

    def test_unknown_transition_does_not_claim_no_write(self):
        row, writes, result = self.run_case('status')
        self.assertEqual(row['status'], 'done'); self.assertEqual(len(writes), 3)
        self.assertNotIn('status transition was not attempted', result.stderr)

    def test_non_object_json_is_not_an_acknowledgement(self):
        row, writes, _ = self.run_case('array')
        self.assertEqual(writes, ['evidence']); self.assertEqual(row['status'], 'review')

    def test_success_and_refusal_remain_distinct(self):
        row, writes, result = self.run_case('success')
        self.assertEqual(row['status'], 'done'); self.assertEqual(result.returncode, 0, result.stderr)
        row, writes, result = self.run_case('refusal')
        self.assertEqual(row['status'], 'review'); self.assertEqual(result.returncode, 3)
        self.assertIn('gate not acknowledged', result.stderr)
        self.assertNotIn('write outcome is unknown', result.stderr)

if __name__ == '__main__': unittest.main()
