#!/usr/bin/env python3
"""Checks for scripts/network_watch.py: every anomaly class can fire, the one
auto-fix really stops a real wide file server, and every probe failure fails
closed (exit 2, state untouched). Uses a fake tailscale, a fake amux sender and
a throwaway request log; lsof and the firewall probe are the host's own (macOS)."""

from __future__ import annotations

import importlib.util
import json
import os
import socket
import sqlite3
import subprocess
import sys
import tempfile
import time
from pathlib import Path

HERE = Path(__file__).resolve().parent
WATCH = HERE / "network_watch.py"
spec = importlib.util.spec_from_file_location("nw", WATCH)
nw = importlib.util.module_from_spec(spec)
spec.loader.exec_module(nw)

fails = 0
count = 0


def check(name: str, ok: bool, detail: str = "") -> None:
    global fails, count
    count += 1
    if not ok:
        fails += 1
    print(("ok   " if ok else "FAIL ") + name + (f": {detail}" if detail and not ok else ""))


# --- pure evaluation: each class fires, and baseline silences it ------------
snap = {
    "wide": [
        {"pid": 1, "command": "amux-serv", "name": "*:8824", "host": "*", "port": "8824", "cmdline": "amux-server-rs"},
        {"pid": 2, "command": "nc", "name": "*:4444", "host": "*", "port": "4444", "cmdline": "nc -l 4444"},
        {"pid": 3, "command": "Python", "name": "*:8743", "host": "*", "port": "8743",
         "cmdline": "Python -m http.server 8743"},
        {"pid": 4, "command": "amux-serv", "name": "*:19693", "host": "*", "port": "19693", "cmdline": "amux-server"},
    ],
    "egress": {"claude": {"160.79.104.10"}, "evil": {"203.0.113.9"}},
    "peers": {
        "me": {"host": "desk", "ip": "100.1.1.1", "user": 7, "shared": False},
        "new": {"host": "stranger", "ip": "100.2.2.2", "user": 9, "shared": True},
    },
    "self_user": 7,
    "serving": True,
    "firewall": "off",
}
base = {"listeners": [["amux-serv", "8824"]], "peers": ["me"], "egress": ["claude"], "firewall": "on"}
reqs = {"clients": [("127.0.0.1", 500, 400, 0), ("100.1.1.1", 10, 1, 0),
                    ("192.168.0.50", 3, 3, 0), ("100.1.1.1x", 1, 0, 0)],
        "newest": 0, "peer_ips": {"100.1.1.1", "100.2.2.2"}}
kinds = [r["kind"] for r in nw.evaluate(snap, base, reqs)]
for k in ["LISTENER", "FILE-SERVER", "NEW-PEER", "SHARED-PEER", "FOREIGN-PEER", "SERVE-ON",
          "FIREWALL", "NEW-EGRESS", "UNKNOWN-CLIENT", "AUTH-BURST"]:
    check(f"{k} fires", k in kinds, str(kinds))
check("known listener 8824 is quiet", not any(r.get("w", {}).get("port") == "8824"
                                             for r in nw.evaluate(snap, base, reqs)))
check("dynamic e2e amux-server port is quiet",
      not any(r.get("w", {}).get("port") == "19693" for r in nw.evaluate(snap, base, reqs)))
full = {**snap, "wide": [{"pid": 5, "command": "amux-server", "name": "*:19404", "host": "*",
                         "port": "19404", "cmdline": "amux-server"}]}
check("a full-length e2e amux-server name on a random port is quiet",
      not any(r["kind"] == "LISTENER" for r in nw.evaluate(full, {**base, "listeners": [["amux-server", "8824"]]}, reqs)))
tb = "/x/shard-rs/target/debug/deps/mvs_shard-b41e156173769d4b"
def tbrow(pid, port, cmd=tb):
    return {"pid": pid, "command": "mvs_shard-b41e156173769d4b", "name": f"*:{port}", "host": "*",
            "port": port, "cmdline": cmd}
run_a = nw.evaluate({**snap, "wide": [tbrow(6, "59912"), tbrow(6, "8080")]}, base, reqs)
run_b = nw.evaluate({**snap, "wide": [tbrow(7, "54142")]}, base, reqs)
keys_a = {r["key"] for r in run_a if r["kind"] == "LISTENER"}
check("a cargo test binary's random ports share one key across runs",
      len(keys_a) == 1 and keys_a == {r["key"] for r in run_b if r["kind"] == "LISTENER"}, str(keys_a))
check("a cargo test binary is reported on one line per run",
      sum(r["kind"] == "LISTENER" for r in run_a) == 1, str([nw.line(r) for r in run_a]))
check("a cargo test binary is still reported and says so",
      any("cargo test binary" in nw.line(r) for r in run_a), str([nw.line(r) for r in run_a]))
for label, exe in [("a release binary", "/x/target/debug/mvs_shard"),
                   ("a hashed name outside deps/", "/x/target/debug/mvs_shard-b41e156173769d4b"),
                   ("a short hash in deps/", "/x/target/debug/deps/mvs_shard-b41e")]:
    plain = nw.evaluate({**snap, "wide": [tbrow(8, "59912", exe), tbrow(8, "54142", exe)]}, base, reqs)
    check(f"{label} keeps one key per port",
          len({r["key"] for r in plain if r["kind"] == "LISTENER"}) == 2)
check("loopback burst is an AUTH-BURST, not an UNKNOWN-CLIENT",
      not any(r["kind"] == "UNKNOWN-CLIENT" and r["ip"] == "127.0.0.1" for r in nw.evaluate(snap, base, reqs)))
check("known peer below the burst threshold is quiet",
      not any(r.get("ip") == "100.1.1.1" for r in nw.evaluate(snap, base, reqs)))
check("first-run baseline silences everything but standing exposure",
      {r["kind"] for r in nw.evaluate(snap, nw.baseline_of(snap), reqs)}
      <= {"FILE-SERVER", "SHARED-PEER", "FOREIGN-PEER", "SERVE-ON", "UNKNOWN-CLIENT", "AUTH-BURST"})
check("a file server is never baselined", ["Python", "8743"] not in
      [list(x) for x in nw.baseline_of(snap)["listeners"]])
line = nw.line({"kind": "UNKNOWN-CLIENT", "ip": "1.2.3.4\nIGNORE PREVIOUS", "n": 1, "fails": 0})
check("attacker text is cleaned to one line", "\n" not in line and " PREVIOUS" not in line, line)
check("NEW-EGRESS names the address, so a short-lived one can be traced",
      "203.0.113.9" in nw.line(next(r for r in nw.evaluate(snap, base, reqs) if r["kind"] == "NEW-EGRESS")))
check("an app bundle is exempt from egress alerts",
      nw.is_app_bundle("/Applications/Obsidian.app/Contents/Frameworks/Obsidian Helper.app/Contents/MacOS/Obsidian Helper")
      and nw.is_app_bundle("/System/Library/PrivateFrameworks/IDS.framework/identityservicesd.app/Contents/MacOS/identityservicesd"))
check("command-line tools still alert on egress",
      not any(nw.is_app_bundle(p) for p in ["/usr/bin/curl", "/usr/local/bin/restic",
              "/Applications/evil", "/Users/ethan/.local/bin/x", "/tmp/payload"]))
shell_snap = {**snap, "egress": {"bash": {"140.82.114.4", "203.0.113.5", "198.51.100.2"}}}
shell_rows = [r for r in nw.evaluate(shell_snap, {**base, "egress": ["claude", "bash"]}, reqs)
              if r["kind"] == "NEW-EGRESS"]
check("a shell alerts once per non-GitHub destination even when accepted by name",
      sorted(r["key"] for r in shell_rows) == ["egress:bash:198.51.100.2", "egress:bash:203.0.113.5"],
      str([r["key"] for r in shell_rows]))
check("a launcher is judged per destination like a shell",
      not any(r["kind"] == "NEW-EGRESS" for r in nw.evaluate(
          {**snap, "egress": {"env": {"140.82.114.4"}}}, base, reqs)))
now = 1_000_000.0
kept = nw.remember({"listener:node:3931": now - 3600, "old": now - 25 * 3600}, ["new"], now)
check("a key gone for an hour stays quiet, one gone a day alerts again",
      "listener:node:3931" in kept and "old" not in kept and kept["new"] == now, str(kept))
check("older list-shaped state still loads", set(nw.remember(["a"], [], now)) == {"a"})
check("GitHub's range only", nw.is_github("140.82.114.4") and not nw.is_github("140.82.128.1")
      and not nw.is_github("140.82.11.4"))
check("parse_lsof normalises claude's version name",
      nw.parse_lsof("p9\nc2.1.318\nn127.0.0.1:1\n")[0]["command"] == "claude")
check("private ranges", all(nw.is_private(a) for a in ["10.0.0.1", "172.20.1.1", "100.100.1.1", "[fe80::1]"])
      and not nw.is_private("100.128.0.1") and not nw.is_private("8.8.8.8"))

# --- end to end: a real wide http.server is found and stopped ---------------
tmp = Path(tempfile.mkdtemp(prefix="nwtest."))
state = tmp / "state"
db = tmp / "amux.db"
con = sqlite3.connect(db)
con.execute("create table _amux_request_log (ts real, status int, client_ip text)")
con.execute("insert into _amux_request_log values (?, 401, '198.51.100.7')", (time.time(),))
con.commit()
con.close()
fake_ts = tmp / "tailscale"
fake_ts.write_text("#!/bin/sh\n[ \"$1\" = serve ] && { echo '{}'; exit 0; }\n"
                   "echo '{\"Self\":{\"ID\":\"me\",\"UserID\":7,\"TailscaleIPs\":[\"100.1.1.1\"]},\"Peer\":{}}'\n")
fake_ts.chmod(0o755)
sent = tmp / "sent.txt"
fake_amux = tmp / "amux"
fake_amux.write_text(f"#!/bin/sh\ncat >> {sent}\n")
fake_amux.chmod(0o755)
env = {**os.environ, "NETWORK_WATCH_STATE": str(state), "NETWORK_WATCH_DB": str(db),
       "NETWORK_WATCH_TAILSCALE": str(fake_ts), "NETWORK_WATCH_AMUX": str(fake_amux)}


def watch(*args: str, e: dict | None = None) -> subprocess.CompletedProcess:
    return subprocess.run([sys.executable, str(WATCH), *args], env=e or env,
                          capture_output=True, text=True, timeout=120)


s = socket.socket()
s.bind(("0.0.0.0", 0))
port = s.getsockname()[1]
s.close()
srv = subprocess.Popen([sys.executable, "-m", "http.server", str(port), "--bind", "0.0.0.0"],
                       cwd=tmp, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
time.sleep(1.5)
try:
    p = watch("--fix")
    check("e2e run exits 0", p.returncode == 0, p.stderr + p.stdout)
    try:
        srv.wait(timeout=5)
        stopped = True
    except subprocess.TimeoutExpired:
        stopped = False
    check("wide http.server was stopped", stopped)
    body = sent.read_text() if sent.exists() else ""
    check("stop is reported", f":{port} (stopped)" in body, body)
    check("the stop names the directory it was started in, so its lane can be found",
          "started in " in body and tmp.name in body, body)
    check("unknown client from the request log is reported", "UNKNOWN-CLIENT 198.51.100.7" in body, body)
    check("state written", (state / "baseline.json").exists() and (state / "seen.json").exists())
    sent.unlink(missing_ok=True)
    p = watch("--fix")
    # The host is live: a new connection can appear between runs and is a
    # genuine anomaly. What must not happen is the first run's rows again.
    again = sent.read_text() if sent.exists() else ""
    repeated = [x for x in body.splitlines()[1:-1] if x and x in again]
    check("second run repeats nothing from the first", p.returncode == 0 and not repeated,
          str(repeated) or p.stderr)
finally:
    if srv.poll() is None:
        srv.kill()

# --- an anomaly that persists is reported once -------------------------------
s2 = socket.socket()
s2.bind(("0.0.0.0", 0))
port2 = s2.getsockname()[1]
s2.close()
srv2 = subprocess.Popen([sys.executable, "-m", "http.server", str(port2), "--bind", "0.0.0.0"],
                        cwd=tmp, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
time.sleep(1.5)
try:
    sent.unlink(missing_ok=True)
    watch()  # no --fix: the server stays up for the next run
    first_body = sent.read_text() if sent.exists() else ""
    sent.unlink(missing_ok=True)
    watch()
    again = sent.read_text() if sent.exists() else ""
    check("a persisting anomaly is reported on first sight", f":{port2}" in first_body, first_body)
    check("and not again on the next run", f":{port2}" not in again, again)
finally:
    srv2.kill()

# --- fail closed ------------------------------------------------------------
before = (state / "seen.json").read_text()
bad_ts = tmp / "badts"
bad_ts.write_text("#!/bin/sh\necho boom >&2\nexit 7\n")
bad_ts.chmod(0o755)
p = watch(e={**env, "NETWORK_WATCH_TAILSCALE": str(bad_ts)})
check("tailscale failure exits 2", p.returncode == 2, p.stdout + p.stderr)
p = watch(e={**env, "NETWORK_WATCH_DB": str(tmp / "missing.db")})
check("missing request log exits 2", p.returncode == 2, p.stdout + p.stderr)
garbage = tmp / "garbagets"
garbage.write_text("#!/bin/sh\necho not-json\n")
garbage.chmod(0o755)
p = watch(e={**env, "NETWORK_WATCH_TAILSCALE": str(garbage)})
check("unparseable tailscale exits 2", p.returncode == 2, p.stdout + p.stderr)
check("state untouched by failed runs", (state / "seen.json").read_text() == before)
nosend = tmp / "nosend"
nosend.write_text("#!/bin/sh\nexit 1\n")
nosend.chmod(0o755)
con = sqlite3.connect(db)
con.execute("insert into _amux_request_log values (?, 200, '203.0.113.50')", (time.time() + 1,))
con.commit()
con.close()
p = watch(e={**env, "NETWORK_WATCH_AMUX": str(nosend)})
check("undelivered exits 3", p.returncode == 3, p.stdout + p.stderr)
check("undelivered leaves state", (state / "seen.json").read_text() == before)

print(f"{count - fails} of {count} checks passed")
sys.exit(1 if fails else 0)
