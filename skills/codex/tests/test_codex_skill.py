#!/usr/bin/env python3
"""Tests for codex-skill.py.

    python3 skills/codex/tests/test_codex_skill.py            # unit + live
    CODEX_SKILL_LIVE=0 python3 skills/codex/tests/test_codex_skill.py   # unit only

The unit tests run on a synthetic rollout. The live tests run against this machine's real Codex
sessions through the real app-server and skip (with the reason) when no codex binary or no thread
history is present. Nothing here starts, resumes or messages a thread; the browser round trip that
does is skills/codex/tests/e2e-browser-roundtrip.sh, run by hand because it spends model tokens.
"""
import importlib.util, json, os, subprocess, sys, tempfile, unittest
from pathlib import Path

SCRIPT = Path(__file__).resolve().parents[1] / "scripts" / "codex-skill.py"
spec = importlib.util.spec_from_file_location("codex_skill", SCRIPT)
cs = importlib.util.module_from_spec(spec)
spec.loader.exec_module(cs)
LIVE = os.environ.get("CODEX_SKILL_LIVE", "1") != "0"


def run(*args):
    r = subprocess.run([sys.executable, str(SCRIPT), *args], capture_output=True, text=True, timeout=300)
    return r.returncode, json.loads(r.stdout.splitlines()[0]) if r.stdout else None


def rollout_fixture(path):
    """Two turns: one completed with a user message, an agent message and a failing command; one left open."""
    T, U1, U2 = "th-1", "turn-a", "turn-b"
    lines = [
        {"ordinal": 0, "type": "session_meta", "payload": {"id": T}},
        {"ordinal": 1, "type": "event_msg", "payload": {"type": "task_started", "turn_id": U1, "started_at": 1000}},
        {"ordinal": 2, "type": "event_msg", "payload": {"type": "item_completed", "turn_id": U1, "started_at_ms": 1000000,
         "completed_at_ms": 1000000, "item": {"type": "UserMessage", "id": "u1", "content": [{"type": "text", "text": "hello"}]}}},
        {"ordinal": 3, "type": "response_item", "payload": {"type": "message", "role": "assistant"}},
        {"ordinal": 4, "type": "event_msg", "payload": {"type": "item_completed", "turn_id": U1, "item": {
            "type": "CommandExecution", "id": "c1", "command": ["/bin/bash", "-lc", "false"], "cwd": "file:///tmp/x",
            "status": "failed", "exit_code": 1, "aggregated_output": "boom", "duration": {"secs": 1, "nanos": 500000000}}}},
        {"ordinal": 5, "type": "event_msg", "payload": {"type": "item_completed", "turn_id": U1, "item": {
            "type": "AgentMessage", "id": "a1", "content": [{"type": "Text", "text": "done"}], "phase": "final"}}},
        {"ordinal": 6, "type": "event_msg", "payload": {"type": "task_complete", "turn_id": U1, "completed_at": 1010, "duration_ms": 10000}},
        {"ordinal": 7, "type": "event_msg", "payload": {"type": "task_started", "turn_id": U2, "started_at": 2000}},
        {"ordinal": 8, "type": "event_msg", "payload": {"type": "item_completed", "turn_id": U2, "item": {
            "type": "UserMessage", "id": "u2", "content": [{"type": "text", "text": "again"}]}}},
    ]
    path.write_text("".join(json.dumps(x) + "\n" for x in lines))


class Unit(unittest.TestCase):
    def setUp(self):
        self.dir = tempfile.TemporaryDirectory()
        self.path = Path(self.dir.name) / "rollout.jsonl"
        rollout_fixture(self.path)

    def tearDown(self):
        self.dir.cleanup()

    def test_rollout_items_in_api_shape(self):
        items, ck, more = cs.rollout_items(str(self.path))
        self.assertEqual([i["item"]["id"] for i in items], ["u1", "c1", "a1", "u2"])
        self.assertEqual(ck, 8)
        self.assertFalse(more)
        cmd = cs.norm_item(items[1], 0)
        self.assertEqual(cmd["kind"], "commandExecution")
        self.assertEqual(cmd["command"], "/bin/bash -lc false")
        self.assertEqual(cmd["cwd"], "/tmp/x")
        self.assertEqual(cmd["exit_code"], 1)
        self.assertEqual(cmd["duration_ms"], 1500)
        self.assertEqual(cs.norm_item(items[2], 0)["text"], "done")

    def test_checkpoints_never_skip_or_repeat(self):
        seen, ck = [], None
        for _ in range(20):           # bounded: a checkpoint that does not advance must fail, not hang
            items, ck2, more = cs.rollout_items(str(self.path), ck, 1)
            if not items:
                break
            seen += [i["item"]["id"] for i in items]
            ck = ck2
        self.assertEqual(seen, ["u1", "c1", "a1", "u2"])
        self.assertEqual(cs.rollout_items(str(self.path), ck, 10)[0], [])

    def test_rollout_turns(self):
        turns = cs.rollout_turns(str(self.path))
        self.assertEqual([(t["id"], t["status"]) for t in turns], [("turn-a", "completed"), ("turn-b", "inProgress")])
        self.assertIsNone(turns[1]["completedAt"])

    def test_redaction(self):
        r = cs.Redactor()
        samples = {
            "key sk-proj-abcdefghijklmnopqrstuvwx1234": "sk-proj-",
            "token ghp_" + "a" * 36: "ghp_",
            "Authorization: Bearer abcdefghijklmnopqrstuvwxyz012345": "abcdefghijklmnop",
            "OPENAI_API_KEY=supersecretvalue123": "supersecretvalue123",
            "jwt eyJhbGciOiJIUzI1NiJ9.eyJzdWIiOiIxMjM0NTY3ODkwIn0.dozjgNryP4J3jVmNHl0w5N_XgL0n3I9PlFUP0THsR8U": "eyJhbGci",
        }
        for text, secret in samples.items():
            out = r.text(text)
            self.assertNotIn(secret, out, text)
            self.assertIn("[REDACTED]", out)
        self.assertEqual(r.text("an ordinary sentence about tokens"), "an ordinary sentence about tokens")

    def test_clip_marks_truncation(self):
        out = cs.clip("x" * 50, 10)
        self.assertTrue(out.startswith("x" * 10))
        self.assertIn("50 chars total", out)

    def test_only_read_methods_reach_app_server(self):
        fake = cs.AppServer.__new__(cs.AppServer)
        for m in ("turn/start", "thread/resume", "thread/archive", "turn/interrupt", "thread/name/set"):
            with self.assertRaises(cs.Fail) as e:
                cs.AppServer.call(fake, m, {})
            self.assertEqual(e.exception.code, "method_not_allowed")


@unittest.skipUnless(LIVE, "CODEX_SKILL_LIVE=0")
class Live(unittest.TestCase):
    @classmethod
    def setUpClass(cls):
        exe, _ = cs.codex_bin()
        if not exe:
            raise unittest.SkipTest("no runnable codex binary on this machine")
        code, cls.lst = run("list", "--limit", "5000", "--status-days", "0")
        if code != 0 or not cls.lst["threads"]:
            raise unittest.SkipTest(f"no Codex threads to test against: {cls.lst}")

    def test_capabilities(self):
        code, d = run("capabilities", "--no-cloud")
        self.assertEqual(code, 0)
        self.assertTrue(d["capabilities"]["app_server"]["available"], d)
        self.assertEqual(d["capabilities"]["local_artifacts"]["never_opened"], ["auth.json", "config.toml"])

    def test_list_covers_state_db(self):
        con = cs.ro_db("state_5.sqlite")
        if not con:
            self.skipTest("state_5.sqlite unreadable")
        n_db = con.execute("select count(*) from threads where archived = 0").fetchone()[0]
        self.assertEqual(self.lst["n_scanned"], n_db, self.lst["n_listed_by"])
        ids = [t["id"] for t in self.lst["threads"]]
        self.assertEqual(len(ids), len(set(ids)))

    def test_desktop_threads_discovered(self):
        desk = [t for t in self.lst["threads"] if t["source"] == "desktop"]
        if not desk:
            self.skipTest("no thread on this machine has originator 'Codex Desktop'")
        self.assertTrue(all(t["originator"] == "Codex Desktop" for t in desk))

    def test_status_never_unexplained(self):
        for t in self.lst["threads"]:
            st = t["status"]
            self.assertIn(st["basis"], ("observed", "inferred", "unavailable", "skipped"), t["id"])
            self.assertTrue(st["detail"], t["id"])
            if st["state"] in ("running", "waiting", "abandoned"):
                self.assertEqual(st["basis"], "inferred")
                self.assertIsNone(st["last_turn"]["completed_at"])

    def _readable(self, source):
        for t in self.lst["threads"]:
            ev = t["status"].get("evidence") or {}
            if ev.get("history_source") == source and t["status"]["state"] not in ("running", "waiting"):
                return t["id"]
        self.skipTest(f"no settled thread whose history comes from {source}")

    def _chunked_equals_full(self, tid):
        code, full = run("read", tid, "--limit", "100000", "--max-chars", "20")
        self.assertEqual(code, 0, full)
        ids = [i["id"] for i in full["items"]]
        got, ck = [], None
        for _ in range(10000):
            args = ["read", tid, "--limit", "9", "--max-chars", "20"] + (["--since", str(ck)] if ck is not None else [])
            code, page = run(*args)
            self.assertEqual(code, 0, page)
            got += [i["id"] for i in page["items"]]
            if not page["items"]:
                break
            ck = page["checkpoint"]
        self.assertEqual(got, ids)
        self.assertEqual(len(got), len(set(got)))
        return full

    def test_read_app_server_history(self):
        full = self._chunked_equals_full(self._readable("app_server"))
        self.assertEqual(full["history_source"], "app_server")

    def test_read_rollout_history(self):
        full = self._chunked_equals_full(self._readable("rollout"))
        self.assertEqual(full["history_source"], "rollout")
        kinds = {i["kind"] for i in full["items"]}
        self.assertIn("userMessage", kinds)

    def test_export_keeps_messages_whole(self):
        tid = self._readable("rollout")
        code, full = run("read", tid, "--limit", "100000", "--full", "--types", "userMessage,agentMessage")
        self.assertEqual(code, 0, full)
        r = subprocess.run([sys.executable, str(SCRIPT), "export", tid, "--output-chars", "50"],
                           capture_output=True, text=True, timeout=300)
        self.assertEqual(r.returncode, 0, r.stderr)
        self.assertIn(f"`{tid}`", r.stdout)
        for item in full["items"]:
            self.assertIn(item["text"].strip()[:2000], r.stdout, item["id"])

    def test_unknown_thread_is_an_error_not_empty(self):
        code, d = run("read", "00000000-0000-0000-0000-000000000000")
        self.assertEqual(code, 2)
        self.assertFalse(d["ok"])
        self.assertEqual(d["error"], "thread_not_found")


if __name__ == "__main__":
    unittest.main(verbosity=2)
