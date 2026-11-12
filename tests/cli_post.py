"""`amux post` sends the text it was given, from a file or stdin, and refuses a flag it does not know.

Ran `amux post p:owner --file /path/report.txt`, which posted the literal text
"--file /path/report.txt" and answered "posted to ...". 21 of
mixpeek-meta-worker's posts to the owner (11-09 to 11-10) arrived as bare
paths. Runs the shipped CLI against a stub server that records each POST.
"""
import http.server
import os
from pathlib import Path
import subprocess
import tempfile
import threading
import unittest

ROOT = Path(__file__).resolve().parents[1]
CLI = ROOT / "amux"


class Stub(http.server.BaseHTTPRequestHandler):
    posts = []

    def do_POST(self):
        body = self.rfile.read(int(self.headers.get("Content-Length") or 0)).decode()
        Stub.posts.append({"path": self.path, "body": body, "thread": self.headers.get("X-Amux-Thread")})
        out = b'{"conv":"d:t|owner","author":"t"}'
        self.send_response(200)
        self.send_header("Content-Type", "application/json")
        self.send_header("Content-Length", str(len(out)))
        self.end_headers()
        self.wfile.write(out)

    def log_message(self, *a):
        pass


class PostTest(unittest.TestCase):
    @classmethod
    def setUpClass(cls):
        cls.srv = http.server.HTTPServer(("127.0.0.1", 0), Stub)
        threading.Thread(target=cls.srv.serve_forever, daemon=True).start()
        cls.tmp = tempfile.TemporaryDirectory(prefix="amux-cli-post-")
        cls.report = Path(cls.tmp.name) / "report.txt"
        cls.report.write_text("Daily report\n\n- two items\n")

    @classmethod
    def tearDownClass(cls):
        cls.srv.shutdown()
        cls.tmp.cleanup()

    def post(self, *args, stdin=""):
        Stub.posts.clear()
        env = {"PATH": os.environ.get("PATH", "/usr/bin:/bin"), "HOME": self.tmp.name,
               "AMUX_API": "http://127.0.0.1:%d" % self.srv.server_port, "AMUX_SESSION": "t"}
        r = subprocess.run(["bash", str(CLI), "post", "p:owner", *args], input=stdin,
                           capture_output=True, text=True, env=env, timeout=60)
        return r, list(Stub.posts)

    def test_file_posts_the_files_text(self):
        r, posts = self.post("--file", str(self.report))
        self.assertEqual(r.returncode, 0, r.stderr)
        self.assertEqual([p["body"] for p in posts], ["Daily report\n\n- two items"])

    def test_file_with_a_thread(self):
        r, posts = self.post("--thread", "42", "--file", str(self.report))
        self.assertEqual(r.returncode, 0, r.stderr)
        self.assertEqual([(p["thread"], p["body"]) for p in posts], [("42", "Daily report\n\n- two items")])

    def test_stdin_and_no_text_read_stdin(self):
        for args in (["--stdin"], ["-"], []):
            r, posts = self.post(*args, stdin="from stdin\n")
            self.assertEqual(r.returncode, 0, (args, r.stderr))
            self.assertEqual([p["body"] for p in posts], ["from stdin"], args)

    def test_plain_text(self):
        r, posts = self.post("hello", "there")
        self.assertEqual(r.returncode, 0, r.stderr)
        self.assertEqual([p["body"] for p in posts], ["hello there"])

    def test_an_unknown_flag_is_refused_and_nothing_is_posted(self):
        r, posts = self.post("--flie", str(self.report))
        self.assertNotEqual(r.returncode, 0)
        self.assertIn("unknown option '--flie'", r.stderr)
        self.assertEqual(posts, [])

    def test_a_missing_file_is_refused_and_nothing_is_posted(self):
        r, posts = self.post("--file", str(Path(self.tmp.name) / "nope.txt"))
        self.assertNotEqual(r.returncode, 0)
        self.assertIn("--file needs a readable file", r.stderr)
        self.assertEqual(posts, [])

    def test_file_and_text_together_is_refused(self):
        r, posts = self.post("--file", str(self.report), "extra")
        self.assertNotEqual(r.returncode, 0)
        self.assertEqual(posts, [])

    def test_double_dash_posts_text_that_starts_with_dashes(self):
        r, posts = self.post("--", "--- not a flag")
        self.assertEqual(r.returncode, 0, r.stderr)
        self.assertEqual([p["body"] for p in posts], ["--- not a flag"])


if __name__ == "__main__":
    unittest.main()
