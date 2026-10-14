"""Export from one real amux server and import into another (T-MIG-1, T-MIG-2, T-MIG-3, T-MIG-5, T-MIG-7).

  AMUX_SERVER_BIN=~/.local/bin/amux-server-rs python3 -m unittest scripts/test_amux_portable.py

Without AMUX_SERVER_BIN the class errors with "not run" rather than passing.
"""
import io, json, os, socket, subprocess, sys, tarfile, tempfile, time, unittest, urllib.request, ssl

HERE = os.path.dirname(os.path.abspath(__file__))
sys.path.insert(0, HERE)
import amux_portable as ap  # noqa: E402

BIN = os.path.expanduser(os.environ.get("AMUX_SERVER_BIN", ""))
CTX = ssl.create_default_context(); CTX.check_hostname, CTX.verify_mode = False, ssl.CERT_NONE


def free_port():
    s = socket.socket(); s.bind(("127.0.0.1", 0)); p = s.getsockname()[1]; s.close(); return p


def start_server(tmp, name):
    port = free_port()
    home = os.path.join(tmp, name, "home"); os.makedirs(home)
    log = open(os.path.join(tmp, name, "server.log"), "w")
    p = subprocess.Popen([BIN], env={"HOME": home, "PATH": os.environ["PATH"], "AMUX_HOME": os.path.join(tmp, name, "amux"),
                                     "AMUX_RS_PORT": str(port), "AMUX_NO_SELF_ADOPT": "1",
                                     "AMUX_BOARD_SEMANTIC_INTAKE": "0"}, stdout=log, stderr=subprocess.STDOUT)
    for _ in range(120):
        try:
            urllib.request.urlopen(f"https://127.0.0.1:{port}/health", context=CTX, timeout=2); break
        except Exception:
            time.sleep(0.5)
    return p, f"https://127.0.0.1:{port}", home


class Portable(unittest.TestCase):
    @classmethod
    def setUpClass(cls):
        if not BIN or not os.path.exists(BIN):
            raise RuntimeError("not run: set AMUX_SERVER_BIN to an amux-server binary")
        cls.tmp = tempfile.mkdtemp()
        cls.pa, cls.a_url, cls.a_home = start_server(cls.tmp, "a")
        cls.pb, cls.b_url, cls.b_home = start_server(cls.tmp, "b")
        cls.a, cls.b = ap.Api(cls.a_url), ap.Api(cls.b_url)
        a = cls.a
        os.makedirs(os.path.join(cls.a_home, "work", "notes"))
        a.call("POST", "/api/sessions", {"name": "researcher", "dir": os.path.join(cls.a_home, "work", "notes"),
                                         "desc": "Reads and summarises", "start": False}, ok=(200, 201))
        a.call("POST", "/api/schedules", {"title": "nightly digest", "session": "researcher", "schedule_expr": "daily at 6am",
                                          "command": "summarise the day", "kind": "tmux"})
        a.call("POST", "/api/memories", {"scope": {"level": "global"}, "name": "tone", "content": "Write plainly.",
                                         "memory_type": "feedback"})
        st, card = a.call("POST", "/api/board", {"title": "Draft the launch post", "desc": "Short and plain.", "status": "todo"})
        cls.card_id = card["id"]
        a.call("POST", "/api/crm/contacts", {"name": "Dana Lee", "email": "dana@example.com", "company": "Acme"})
        a.call("POST", "/api/skills/digest", {"content": "# Digest\nSummarise the day in five lines."})
        a.call("POST", "/api/saved-messages", {"text": "Ship it after the tests pass", "label": "ship"})
        a.call("POST", "/api/journal", {"date": "2026-10-14", "text": "Moved to the cloud today."})
        a.call("POST", "/api/files/upload?path=" + "mdai/Daily.mdai", raw=b"---\nsources: []\n---\n# Daily\n")
        cls.bundle = os.path.join(cls.tmp, "a.amuxmove")
        cls.manifest = ap.export(cls.a, cls.bundle, ["mdai"])

    @classmethod
    def tearDownClass(cls):
        for p in (getattr(cls, "pa", None), getattr(cls, "pb", None)):
            if p:
                p.terminate(); p.wait(timeout=20)

    def test_1_export_lists_every_kind_and_no_secret(self):
        c = self.manifest["counts"]
        for k in ("workers", "schedules", "memories", "board", "crm", "skills", "saved_messages", "journal"):
            self.assertGreaterEqual(c.get(k, 0), 1, k)
        self.assertEqual(self.manifest["files"], 1)

    def test_2_dry_run_changes_nothing_then_apply_creates_everything(self):
        _, data, files = ap.read_bundle(self.bundle)
        before = len(ap.rows(self.b.get("/api/board?all=1"), "items"))
        plan = ap.plan_import(self.b, data, files)
        self.assertTrue(any(p[2] == "create" for p in plan))
        self.assertEqual(len(ap.rows(self.b.get("/api/board?all=1"), "items")), before, "a dry run wrote something")
        result = ap.apply_import(self.b, data, files, plan)
        kinds = {k for k, _, _ in result}
        for k in ("worker", "schedule", "memory", "card", "contact", "skill", "saved message", "journal entry", "file"):
            self.assertIn(k, kinds)
        b = self.b
        self.assertIn("researcher", {w["name"] for w in ap.rows(b.get("/api/sessions"), "sessions")})
        sched = [s for s in ap.rows(b.get("/api/schedules"), "schedules") if s["title"] == "nightly digest"]
        self.assertEqual(len(sched), 1)
        self.assertFalse(sched[0].get("enabled"), "an imported schedule must arrive disabled")
        self.assertIn("Draft the launch post", {c["title"] for c in ap.rows(b.get("/api/board?all=1"), "items")})
        self.assertIn("Dana Lee", {c["name"] for c in ap.rows(b.get("/api/crm/contacts"), "contacts")})
        self.assertIn("digest", {s["name"] for s in ap.rows(b.get("/api/skills"), "skills")})
        with urllib.request.urlopen(self.b_url + "/api/files/download?path=mdai/Daily.mdai", context=CTX) as r:
            self.assertIn(b"# Daily", r.read())
        workers = {w["name"]: w for w in ap.rows(b.get("/api/sessions"), "sessions")}
        self.assertNotIn(workers["researcher"].get("status"), ("running", "active"), "an imported worker must not start")

    def test_3_second_import_changes_nothing_and_says_so(self):
        _, data, files = ap.read_bundle(self.bundle)
        plan = ap.plan_import(self.b, data, files)
        creates = [p for p in plan if p[2] == "create"]
        self.assertEqual(creates, [], creates)

    def test_4_secret_shaped_values_are_refused_both_ways(self):
        self.a.call("POST", "/api/board", {"title": "key", "desc": "sk-ant-api03-" + "x" * 40})
        with self.assertRaises(SystemExit):
            ap.export(self.a, os.path.join(self.tmp, "leaky.amuxmove"))
        bad = os.path.join(self.tmp, "bad.amuxmove")
        with tarfile.open(bad, "w:gz") as tar:
            for name, body in (("manifest.json", {"format": "amuxmove", "version": 1, "exported_at": 0, "source": "x"}),
                               ("data.json", {"board": [{"title": "t", "desc": "ghp_" + "A" * 36}]})):
                b = json.dumps(body).encode(); ti = tarfile.TarInfo(name); ti.size = len(b); tar.addfile(ti, io.BytesIO(b))
        with self.assertRaises(SystemExit):
            ap.read_bundle(bad)

    def test_5_unknown_format_and_escaping_paths_are_refused(self):
        for name, files in (("future.amuxmove", {"manifest.json": {"format": "amuxmove", "version": 99}}),
                            ("escape.amuxmove", {"manifest.json": {"format": "amuxmove", "version": 1, "exported_at": 0,
                                                                   "source": "x"}, "data.json": {}, "files/../../etc/x": "x"})):
            p = os.path.join(self.tmp, name)
            with tarfile.open(p, "w:gz") as tar:
                for n, body in files.items():
                    b = (json.dumps(body) if not isinstance(body, str) else body).encode()
                    ti = tarfile.TarInfo(n); ti.size = len(b); tar.addfile(ti, io.BytesIO(b))
            with self.assertRaises(SystemExit):
                ap.read_bundle(p)


if __name__ == "__main__":
    unittest.main()
