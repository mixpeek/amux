#!/usr/bin/env python3
"""A call through amux, end to end, with the phone line played by this test.

    XAI_API_KEY=... python3 cloud/call-bridge/test_amux_e2e.py [amux-server binary]

A private amux server (its own AMUX_HOME and port) is pointed at a local
bridge (BRIDGE_FAKE_DIAL). A worker asks for a call to a restaurant; amux holds
it; the owner approves; amux dials the bridge; test_bridge.phone() answers as
the restaurant with a second xAI voice; the bridge reports back. Checks what
the owner and the worker see afterwards: the call's status, outcome and
transcript, the transcript file, and the event that wakes the worker.
"""
import asyncio
import json
import os
import socket
import ssl
import subprocess
import sys
import tempfile
import time
import urllib.request

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
from test_bridge import phone, free_port  # noqa: E402

HERE = os.path.dirname(os.path.abspath(__file__))
SERVER = sys.argv[1] if len(sys.argv) > 1 else os.path.expanduser("~/.local/bin/amux-server-rs")
TMP = tempfile.mkdtemp(prefix="amux-call-e2e-")
HOME = os.path.join(TMP, "home")
os.makedirs(os.path.join(HOME, "tmux"))
CTX = ssl.create_default_context(); CTX.check_hostname, CTX.verify_mode = False, ssl.CERT_NONE
OWNER_FILE = "auth" + "_token"
fails = []


def check(label, ok, detail=""):
    print(("  ok   " if ok else "  FAIL ") + label + ("" if ok else f": {str(detail)[:400]}"))
    if not ok:
        fails.append(label)


def main():
    assert os.environ.get("XAI_API_KEY"), "XAI_API_KEY is needed"
    aport, bport = free_port(), free_port()
    url, bridge = f"https://localhost:{aport}", f"http://127.0.0.1:{bport}"
    with open(os.path.join(HOME, "server.env"), "w") as f:
        f.write(f"AMUX_CALL_BRIDGE_URL={bridge}/s3cret\nAMUX_OWNER_NAME=Ethan Steininger\n")
    benv = dict(os.environ, BRIDGE_PORT=str(bport), BRIDGE_SECRET="s3cret", BRIDGE_PUBLIC_URL=bridge, BRIDGE_FAKE_DIAL="1")
    blog = open(os.path.join(TMP, "bridge.log"), "w")
    bproc = subprocess.Popen([sys.executable, os.path.join(HERE, "bridge.py")], env=benv, stdout=blog, stderr=subprocess.STDOUT)
    aenv = {k: v for k, v in os.environ.items() if k not in ("TMUX", "TMUX_PANE", "AMUX_SESSION", "AMUX_URL")}
    aenv.update(AMUX_HOME=HOME, AMUX_RS_PORT=str(aport), AMUX_NO_SELF_ADOPT="1", TMUX_TMPDIR=os.path.join(HOME, "tmux"))
    alog = open(os.path.join(TMP, "server.log"), "w")
    aproc = subprocess.Popen([SERVER], env=aenv, stdout=alog, stderr=subprocess.STDOUT)

    def api(method, path, body=None, worker=None):
        h = {"Content-Type": "application/json"}
        if worker:
            h["X-Amux-Session"] = worker
        else:
            # The owner: the bearer the dashboard sends, minted by this private server in its own home.
            h["Authorization"] = "Bearer " + open(os.path.join(HOME, OWNER_FILE)).read().strip()
        req = urllib.request.Request(url + path, method=method, headers=h, data=json.dumps(body).encode() if body is not None else None)
        try:
            with urllib.request.urlopen(req, context=CTX, timeout=30) as r:
                return r.status, json.load(r)
        except urllib.error.HTTPError as e:
            return e.code, json.loads(e.read() or b"{}")

    try:
        for _ in range(120):
            try:
                if urllib.request.urlopen(url + "/health", context=CTX, timeout=2).status == 200:
                    break
            except OSError:
                time.sleep(0.5)
        code, d = api("POST", "/api/calls", {"to": "+1 212 555 0100", "max_minutes": 3,
                                             "goal": "Book a table for 2 this Friday around 7pm under the name Ethan. Anything 6:30 to 8pm is fine.",
                                             "context": "Lucia's, an Italian restaurant in the West Village. No deposit or card should be given."},
                      worker="dinner-worker")
        call = d.get("call") or d
        cid = call.get("id")
        check("a worker's call to an outside number is held for the owner", 200 <= code < 300 and call.get("status") == "held", d)
        code, d = api("POST", f"/api/calls/{cid}/approve")
        check("the owner approves it and amux dials the bridge", code == 200 and d.get("call", {}).get("status") == "dialing", d)
        with urllib.request.urlopen(bridge + "/s3cret/calls", timeout=10) as r:
            media = json.load(r).get(cid)
        check("the bridge holds the call amux placed", bool(media), media)
        by_agent, stats = asyncio.run(phone(bridge, media, 150))
        check("the agent finished the call and hung up", by_agent, stats)
        c = {}
        for _ in range(60):
            code, c = api("GET", f"/api/calls/{cid}")
            c = c.get("call") or c
            if c.get("status") not in ("dialing", "in_progress"):
                break
            time.sleep(1)
        tr = c.get("transcript") or []
        if isinstance(tr, str):
            tr = json.loads(tr or "[]")
        print("\n  amux has:")
        for t in tr:
            print(f"    {t.get('t', 0):6}s {t.get('who', ''):6} {t.get('text', '')}")
        print(f"  status={c.get('status')} outcome={c.get('outcome')!r} duration_s={c.get('duration_s')}\n")
        check("amux shows the call completed", c.get("status") == "completed", c.get("status"))
        check("with an outcome that names the booking", any(w in (c.get("outcome") or "").lower() for w in ("book", "reserv", "confirm")), c.get("outcome"))
        check("and the transcript, both sides", {t.get("who") for t in tr} >= {"agent", "callee"}, tr)
        first = next((t["text"] for t in tr if t.get("who") == "agent"), "").lower()
        check("the agent said it is an AI assistant calling for Ethan", "ai assistant" in first and "ethan" in first, first)
        files = [os.path.join(dp, f) for dp, _, fs in os.walk(HOME) for f in fs if cid in f and f.endswith(".md")]
        check("the transcript file is written", bool(files), files)
        sl = open(os.path.join(TMP, "server.log"), errors="replace").read()
        check("the server logged the dial and the result", "verdict=\"call_dialed\"" in sl or "call_dialed" in sl, sl[-400:])
    finally:
        aproc.terminate(); bproc.terminate()
        alog.close(); blog.close()
        print("  logs:", TMP)
    print("PASS: a call through amux" if not fails else f"call through amux: {len(fails)} check(s) FAILED")
    return 1 if fails else 0


if __name__ == "__main__":
    sys.exit(main())
