"""The door must take a page load's burst of requests without dropping them.

Every dashboard request passes through the door. With socketserver's default
listen backlog of 5, a page load's ~100 parallel requests overflowed it; each
dropped connection was retried a second later (2026-10-28, sandbox).
"""
import os
import socket
import subprocess
import sys
import tempfile
import time
import unittest

HERE = os.path.dirname(os.path.abspath(__file__))


def free_port():
    with socket.socket() as s:
        s.bind(("127.0.0.1", 0))
        return s.getsockname()[1]


class DoorBacklog(unittest.TestCase):
    @classmethod
    def setUpClass(cls):
        cls.tmp = tempfile.mkdtemp(prefix="door-backlog.")
        cls.port = free_port()
        etc = os.path.join(cls.tmp, "etc")
        os.makedirs(etc)
        with open(os.path.join(etc, "secret"), "w") as f:
            f.write("door-test-secret")
        for name in ("users", "members", "joined"):
            open(os.path.join(etc, name), "w").close()
        cls.proc = subprocess.Popen(
            [sys.executable, os.path.join(HERE, "door.py"), "serve"],
            env=dict(os.environ, DOOR_PORT=str(cls.port), DOOR_USERS=os.path.join(etc, "users"),
                     DOOR_SECRET=os.path.join(etc, "secret"), DOOR_MEMBERS=os.path.join(etc, "members"),
                     DOOR_JOINED=os.path.join(etc, "joined"), DOOR_FLEET_ENV=os.path.join(etc, "none"),
                     DOOR_ENTRY=os.path.join(etc, "none"), DOOR_UPDATE_STATUS=os.path.join(etc, "status.json")),
            stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
        for _ in range(100):
            try:
                socket.create_connection(("127.0.0.1", cls.port), timeout=0.2).close()
                break
            except OSError:
                time.sleep(0.1)

    @classmethod
    def tearDownClass(cls):
        cls.proc.terminate()
        cls.proc.wait(timeout=10)

    @unittest.skipUnless(sys.platform.startswith("linux"), "ss reads the listen backlog on Linux")
    def test_the_listening_socket_queues_a_page_loads_burst(self):
        out = subprocess.run(["ss", "-Hltn", "sport = :%d" % self.port], capture_output=True, text=True).stdout
        # LISTEN  Recv-Q  Send-Q(=backlog)  local  peer
        backlog = int(out.split()[2])
        self.assertGreaterEqual(backlog, 128, out)

    def test_a_hundred_connections_at_once_are_all_accepted_quickly(self):
        start = time.monotonic()
        socks = []
        try:
            for _ in range(100):
                socks.append(socket.create_connection(("127.0.0.1", self.port), timeout=5))
            for s in socks:
                s.sendall(b"GET /_door/login HTTP/1.0\r\nHost: x\r\n\r\n")
            for s in socks:
                self.assertTrue(s.recv(16).startswith(b"HTTP/1."))
        finally:
            for s in socks:
                s.close()
        # A dropped connection costs a 1 s SYN retry; all 100 must land well inside that.
        self.assertLess(time.monotonic() - start, 0.9)


if __name__ == "__main__":
    unittest.main()
