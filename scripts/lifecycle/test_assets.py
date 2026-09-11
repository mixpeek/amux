import hashlib
import json
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
import threading
import unittest
from check_assets import check


class AssetProvenanceTest(unittest.TestCase):
    def test_actual_served_bytes_must_match_the_measured_source(self):
        class Handler(BaseHTTPRequestHandler):
            def do_GET(self):
                body = json.dumps({'build': 'fixture-process'}).encode() if self.path == '/health' else b'actual embedded script'
                self.send_response(200)
                self.end_headers()
                self.wfile.write(body)
            def log_message(self, *args):
                pass
        server = ThreadingHTTPServer(('127.0.0.1', 0), Handler)
        thread = threading.Thread(target=server.serve_forever, daemon=True)
        thread.start()
        try:
            base = 'http://127.0.0.1:' + str(server.server_port)
            digest = hashlib.sha256(b'actual embedded script').hexdigest()
            matched = check(base, {'/app.js': digest})
            self.assertEqual(matched['verdict'], 'assets_match')
            self.assertEqual(matched['health']['build'], 'fixture-process')
            stale = check(base, {'/app.js': hashlib.sha256(b'new source').hexdigest()})
            self.assertEqual(stale['verdict'], 'asset_mismatch')
            self.assertEqual(stale['mismatches'], ['/app.js'])
            self.assertEqual(stale['actual']['/app.js'], digest)
        finally:
            server.shutdown()
            server.server_close()
            thread.join()
