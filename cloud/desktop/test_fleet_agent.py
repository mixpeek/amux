"""Fleet agent release install and rollback with real ssh-keygen signatures (T-OPS-1, T-OPS-2, T-OPS-4 shape).

Runs anywhere with ssh-keygen; systemctl is replaced by a shim on PATH and the
server's /health by a small local HTTP server whose answer the test controls.
"""
import hashlib, http.server, importlib, json, os, socket, subprocess, sys, tempfile, threading, unittest

HERE = os.path.dirname(os.path.abspath(__file__))


def free_port():
    s = socket.socket(); s.bind(("127.0.0.1", 0)); p = s.getsockname()[1]; s.close(); return p


class Health(http.server.BaseHTTPRequestHandler):
    ok = True
    version = "v1"

    def log_message(self, *a):
        pass

    def do_GET(self):
        if self.path == "/health" and Health.ok:
            body = json.dumps({"version": Health.version}).encode()
            self.send_response(200); self.send_header("Content-Length", str(len(body))); self.end_headers(); self.wfile.write(body)
        elif self.path.startswith("/files/"):
            p = os.path.join(self.server.files, self.path[len("/files/"):])
            data = open(p, "rb").read()
            self.send_response(200); self.send_header("Content-Length", str(len(data))); self.end_headers(); self.wfile.write(data)
        else:
            self.send_response(500); self.send_header("Content-Length", "0"); self.end_headers()


class ReleaseInstall(unittest.TestCase):
    @classmethod
    def setUpClass(cls):
        cls.t = tempfile.mkdtemp()
        cls.files = os.path.join(cls.t, "files"); os.makedirs(cls.files)
        for name in ("release", "intruder"):
            subprocess.run(["ssh-keygen", "-q", "-t", "ed25519", "-N", "", "-f", os.path.join(cls.t, name)], check=True)
        pub = open(os.path.join(cls.t, "release.pub")).read().strip()
        cls.signers = os.path.join(cls.t, "signers")
        open(cls.signers, "w").write(f'release@amux.io namespaces="amux-release" {pub}\n')
        bindir = os.path.join(cls.t, "bin"); os.makedirs(bindir)
        cls.restarts = os.path.join(cls.t, "restarts")
        shim = os.path.join(bindir, "systemctl")
        open(shim, "w").write(f'#!/bin/sh\necho "$@" >> {cls.restarts}\n'); os.chmod(shim, 0o755)
        cls.ufw_log = os.path.join(cls.t, "ufw.log")
        cls.ts_state = os.path.join(cls.t, "ts.json")
        for name, body in (("ufw", f'echo "$@" >> {cls.ufw_log}'), ("tailscale", f'cat {cls.ts_state} 2>/dev/null || exit 1')):
            p = os.path.join(bindir, name); open(p, "w").write("#!/bin/sh\n" + body + "\n"); os.chmod(p, 0o755)
        cls.port = free_port()
        srv = http.server.ThreadingHTTPServer(("127.0.0.1", cls.port), Health); srv.files = cls.files
        threading.Thread(target=srv.serve_forever, daemon=True).start()
        cls.server_bin = os.path.join(cls.t, "amux-server")
        os.environ.update(FLEET_STATE=os.path.join(cls.t, "state"), FLEET_ETC=cls.t, FLEET_SERVER_BIN=cls.server_bin,
                          FLEET_RELEASE_SIGNERS=cls.signers, FLEET_HEALTH=f"http://127.0.0.1:{cls.port}/health",
                          FLEET_HEALTH_SOAK="1", FLEET_RESTART_CMD="systemctl --user restart amux-server.service", PATH=bindir + os.pathsep + os.environ["PATH"])
        sys.path.insert(0, HERE)
        cls.fa = importlib.import_module("fleet_agent")

    def setUp(self):
        open(self.server_bin, "w").write("OLD BINARY")
        Health.ok = True

    def release(self, content, key="release", sha=None):
        name = hashlib.sha256(content.encode()).hexdigest()[:8]
        p = os.path.join(self.files, name)
        open(p, "w").write(content)
        subprocess.run(["ssh-keygen", "-q", "-Y", "sign", "-f", os.path.join(self.t, key), "-n", "amux-release", p],
                       check=True, capture_output=True)
        base = f"http://127.0.0.1:{self.port}/files/{name}"
        return {"version": name, "url": base, "sig_url": base + ".sig",
                "sha256": sha or hashlib.sha256(content.encode()).hexdigest()}

    def test_good_release_installs(self):
        self.assertEqual(self.fa.install_release(self.release("NEW BINARY")), "installed")
        self.assertEqual(open(self.server_bin).read(), "NEW BINARY")
        self.assertIn("restart", open(self.restarts).read())

    def test_unhealthy_release_rolls_back(self):
        rel = self.release("BROKEN BINARY")
        Health.ok = False
        self.assertEqual(self.fa.install_release(rel), "rolled_back")
        self.assertEqual(open(self.server_bin).read(), "OLD BINARY")

    def test_wrong_key_is_refused_before_anything_changes(self):
        with self.assertRaisesRegex(RuntimeError, "signature did not verify"):
            self.fa.install_release(self.release("EVIL BINARY", key="intruder"))
        self.assertEqual(open(self.server_bin).read(), "OLD BINARY")

    def test_checksum_mismatch_is_refused(self):
        with self.assertRaisesRegex(RuntimeError, "does not match"):
            self.fa.install_release(self.release("NEW BINARY 2", sha="0" * 64))
        self.assertEqual(open(self.server_bin).read(), "OLD BINARY")

    def test_fingerprint_counts_rows_and_hashes_the_file_list(self):
        import sqlite3
        db = os.path.join(self.t, "fp.db")
        c = sqlite3.connect(db); c.execute("CREATE TABLE board_items(x)"); c.executemany("INSERT INTO board_items VALUES(?)", [(1,), (2,)]); c.commit(); c.close()
        tree = os.path.join(self.t, "tree"); os.makedirs(os.path.join(tree, "mdai"), exist_ok=True)
        open(os.path.join(tree, "mdai", "a.mdai"), "w").write("x")
        fp = self.fa.fingerprint(db, None, tree_root=tree)
        self.assertEqual(fp["tables"], {"board_items": 2})
        self.assertEqual(fp["files"], 1)
        open(os.path.join(tree, "mdai", "b.mdai"), "w").write("yy")
        self.assertNotEqual(self.fa.fingerprint(db, None, tree_root=tree)["files_sha256"], fp["files_sha256"])

    def test_backup_config_is_private_and_scoped(self):
        self.fa.write_backup_config({"endpoint": "https://e", "bucket": "amux-a-1", "access_key_id": "ak",
                                     "secret_access_key": "sk", "restic_password": "pw"})
        for f in ("litestream.yml", "restic.env"):
            p = os.path.join(self.t, f)
            self.assertEqual(oct(os.stat(p).st_mode & 0o777), "0o600")
        self.assertIn("sync-interval: 60s", open(os.path.join(self.t, "litestream.yml")).read())


    def test_tailnet_only_refuses_without_a_tailnet_then_applies_on_one(self):
        if os.path.exists(self.ufw_log):
            os.remove(self.ufw_log)
        open(self.ts_state, "w").write(json.dumps({"BackendState": "NeedsLogin"}))
        self.assertFalse(self.fa.apply_tailnet_only(True))
        self.assertFalse(os.path.exists(self.ufw_log), "no firewall change without a tailnet to fall back on")
        open(self.ts_state, "w").write(json.dumps({"BackendState": "Running", "Self": {"DNSName": "amux-a.tail1.ts.net."}}))
        self.assertEqual(self.fa.tailnet_url(), "https://amux-a.tail1.ts.net")
        self.assertTrue(self.fa.apply_tailnet_only(True))
        log = open(self.ufw_log).read()
        self.assertIn("allow in on tailscale0", log)
        self.assertIn("delete allow 443/tcp", log)
        self.assertTrue(self.fa.apply_tailnet_only(False))
        self.assertTrue(open(self.ufw_log).read().strip().endswith("allow 443/tcp"))


if __name__ == "__main__":
    unittest.main()
