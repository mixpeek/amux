"""Rig for ios/AmuxShareUITests/OfflineDashboardUITests.swift.

A throwaway amux server (its own AMUX_HOME, port 18961, private tmux, a stub
worker CLI that only echoes) plus a controller the UI test drives, because
the simulator has no airplane mode: stopping this server is how the test
takes the app offline.

  python3 scripts/ios-offline-rig.py <simulator-udid>
  # then, in ios/: see the test file's header for the xcodebuild line

Controller on http://127.0.0.1:18990:
  /start  /stop  /state  /shot?name=x (simulator screenshot)
  /peek?w=<worker>       the worker's pane
  /history?token=<s>     messages the server recorded containing s
Scratch, screenshots and logs go to $IOS_OFFLINE_RIG_DIR (default: a temp dir).
"""
import http.server, json, os, ssl, subprocess, sys, threading, time, urllib.request, urllib.parse

REPO = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
ROOT = os.environ.get("IOS_OFFLINE_RIG_DIR") or __import__("tempfile").mkdtemp(prefix="ios-offline-rig-")
HOME = os.path.join(ROOT, "home")
PORT = 18961
URL = f"https://127.0.0.1:{PORT}"
SERVER = os.path.expanduser("~/.local/bin/amux-server-rs")
STATIC = os.path.join(REPO, "crates", "amux-dashboard", "static")
UDID = sys.argv[1] if len(sys.argv) > 1 else "booted"
SHOTS = os.path.join(ROOT, "shots")
os.makedirs(HOME, exist_ok=True); os.makedirs(SHOTS, exist_ok=True)
STUB = os.path.join(ROOT, "stub-claude.sh")
with open(STUB, "w") as f:
    f.write('#!/bin/bash\n# Stand-in for the Claude CLI: echoes whatever it is sent.\necho "stub worker ready"\nexec cat\n')
os.chmod(STUB, 0o755)
CTX = ssl.create_default_context(); CTX.check_hostname = False; CTX.verify_mode = ssl.CERT_NONE
proc = None
log = open(os.path.join(ROOT, "rig.log"), "a")

def say(*a):
    print(time.strftime("%H:%M:%S"), *a, file=log, flush=True)

def env():
    e = {k: v for k, v in os.environ.items() if k not in ("TMUX", "TMUX_PANE", "AMUX_SESSION", "AMUX_WORKER", "AMUX_URL")}
    e.update(AMUX_HOME=HOME, AMUX_RS_PORT=str(PORT), AMUX_NO_SELF_ADOPT="1",
             AMUX_ALLOW_TMUX_SPAWN_FROM_TEST_HOME="1", TMUX_TMPDIR=os.path.join(ROOT, "tmux"),
             AMUX_DASHBOARD_DIR=STATIC, AMUX_CLAUDE_CMD=STUB,
             CLAUDE_CONFIG_DIR=os.path.join(HOME, "claude-config"), AMUX_VAULT_KEYSTORE="file",
             AMUX_GEMINI_BASE="http://127.0.0.1:9", AMUX_APP_PORTS=f"{PORT+2}-{PORT+9}")
    os.makedirs(e["TMUX_TMPDIR"], exist_ok=True)
    return e

def call(method, path, body=None):
    data = json.dumps(body).encode() if body is not None else None
    req = urllib.request.Request(URL + path, data=data, method=method, headers={"Content-Type": "application/json"})
    with urllib.request.urlopen(req, context=CTX, timeout=20) as r:
        t = r.read()
        try: return r.status, json.loads(t or b"{}")
        except ValueError: return r.status, t.decode(errors="replace")

def up():
    try: return call("GET", "/health")[0] == 200
    except Exception: return False

def start():
    global proc
    if proc and proc.poll() is None: return "already"
    proc = subprocess.Popen([SERVER], env=env(), stdout=open(os.path.join(ROOT, "server.log"), "a"), stderr=subprocess.STDOUT)
    for _ in range(120):
        if up(): say("server up"); return "up"
        time.sleep(0.5)
    return "timeout"

def stop():
    global proc
    if not proc: return "none"
    proc.terminate()
    try: proc.wait(10)
    except subprocess.TimeoutExpired: proc.kill(); proc.wait()
    proc = None
    for _ in range(40):
        if not up(): break
        time.sleep(0.25)
    say("server stopped"); return "stopped"

class H(http.server.BaseHTTPRequestHandler):
    def log_message(self, *a): pass
    def reply(self, obj):
        b = json.dumps(obj).encode()
        self.send_response(200); self.send_header("Content-Type", "application/json")
        self.send_header("Content-Length", str(len(b))); self.end_headers(); self.wfile.write(b)
    def do_GET(self):
        u = urllib.parse.urlparse(self.path); q = urllib.parse.parse_qs(u.query)
        say("ctl", self.path)
        if u.path == "/start": return self.reply({"result": start()})
        if u.path == "/stop": return self.reply({"result": stop()})
        if u.path == "/state": return self.reply({"up": up()})
        if u.path == "/shot":
            p = os.path.join(SHOTS, q.get("name", ["shot"])[0] + ".png")
            r = subprocess.run(["xcrun", "simctl", "io", UDID, "screenshot", p], capture_output=True, text=True)
            return self.reply({"path": p, "rc": r.returncode, "err": r.stderr[-300:]})
        if u.path == "/peek":
            try: s, v = call("GET", "/api/sessions/" + q.get("w", ["offline-demo"])[0] + "/peek?lines=200")
            except Exception as e: return self.reply({"error": str(e)})
            return self.reply({"status": s, "text": (v.get("history") or v.get("output") or "") if isinstance(v, dict) else str(v)})
        if u.path == "/history":
            import sqlite3
            tok = q.get("token", [""])[0]
            try:
                db = sqlite3.connect("file:" + os.path.join(HOME, "amux.db") + "?mode=ro", uri=True, timeout=10)
                rows = db.execute("select id, session, text, delivery from cmd_history where text like ?", ("%" + tok + "%",)).fetchall()
                db.close()
            except Exception as e:
                return self.reply({"error": str(e)})
            return self.reply({"count": len(rows), "rows": [list(r) for r in rows]})
        self.reply({"error": "no route"})

def seed():
    """Two workers and two cards, once per home."""
    try:
        if any(s.get("name") == "offline-demo" for s in call("GET", "/api/sessions")[1]):
            return
        for name, desc in (("offline-demo", "Stub worker for the iOS offline test"), ("plane-notes", "Second stub worker")):
            call("POST", "/api/sessions", {"name": name, "dir": ROOT, "desc": desc})
        for t in ("Cached card: review the offline copy", "Cached card: catch up after landing"):
            call("POST", "/api/board", {"title": t, "status": "todo", "session": "offline-demo"})
    except Exception as e:
        say("seed failed", e)

if __name__ == "__main__":
    print(start(), ROOT, flush=True)
    seed()
    srv = http.server.ThreadingHTTPServer(("127.0.0.1", 18990), H)
    try: srv.serve_forever()
    finally: stop()
