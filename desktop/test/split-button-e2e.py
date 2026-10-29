#!/usr/bin/env python3
"""The Split button splits the window into panes, the page again on the right
half, as a tab dropped on a pane's right edge does (Ethan, 2026-10-26: "make it
a quick button which takes the current pressed active window and splits it").
The new pane opens on the same deep link and signed in ("make sure u test deep
linking and it opening up with logged in session").

    desktop/test/split-button-e2e.py [path/to/Amux.app]

Real: a private amux server with an owner token that treats this Mac's own
browser as a remote one (AMUX_RS_NO_LOOPBACK_BYPASS for the API, as the browser
e2e server does, and AMUX_RS_SHELL_AS_REMOTE for the page), so "signed in"
means the owner session cookie the page's sign-in sets, as on a phone or
another computer, and not locality; the dashboard from this checkout; the
page's own sign-in; the page's Split click reaching the app over the amuxPane
bridge; the grid the app builds; a quit and relaunch.

Simulated: the click itself (amux://test/split runs the button's click() in the
focused pane), because macOS refuses this agent synthetic mouse events.
"""
import json, os, secrets, signal, socket, subprocess, sys, tempfile, time, urllib.request, ssl

HERE = os.path.dirname(os.path.abspath(__file__))
REPO = os.path.abspath(os.path.join(HERE, "..", ".."))
APP = sys.argv[1] if len(sys.argv) > 1 else os.path.join(REPO, "Amux.app")
BIN = os.path.join(APP, "Contents", "MacOS", "amux")
# A server new enough to know AMUX_RS_SHELL_AS_REMOTE: the deployed one, or a build.
SERVER = os.environ.get("AMUX_E2E_SERVER_BIN") or os.path.expanduser("~/.local/bin/amux-server-rs")
TMP = tempfile.mkdtemp(prefix="amux-split-e2e-")
CFG, LOG, STATE = (os.path.join(TMP, n) for n in ("config.json", "app.log", "state.json"))
HOME = os.path.join(TMP, "home")
fails = 0


def sh(*a):
    return subprocess.run(a, capture_output=True, text=True)


def check(label, ok, detail=""):
    global fails
    print(("  ok   " if ok else "  FAIL ") + label + ("" if ok else f": {detail}"))
    fails += 0 if ok else 1


def free_port():
    s = socket.socket()
    s.bind(("127.0.0.1", 0))
    p = s.getsockname()[1]
    s.close()
    return p


PORT, TOKEN = free_port(), secrets.token_hex(16)
URL = f"https://localhost:{PORT}"
CTX = ssl.create_default_context()
CTX.check_hostname, CTX.verify_mode = False, ssl.CERT_NONE


def api(path, body=None, auth=True):
    req = urllib.request.Request(URL + path, data=json.dumps(body).encode() if body is not None else None,
                                 headers={"Content-Type": "application/json", **({"Authorization": "Bearer " + TOKEN} if auth else {})})
    try:
        with urllib.request.urlopen(req, context=CTX, timeout=10) as r:
            return r.status, r.read().decode()
    except urllib.error.HTTPError as e:
        return e.code, e.read().decode()


# ── A private server: its own home, tmux and token ──
os.makedirs(os.path.join(HOME, "tmux"))
env = {k: v for k, v in os.environ.items() if k not in ("TMUX", "TMUX_PANE")}
env.update(AMUX_HOME=HOME, AMUX_RS_PORT=str(PORT), AMUX_AUTH_TOKEN=TOKEN, AMUX_RS_NO_LOOPBACK_BYPASS="1", AMUX_RS_SHELL_AS_REMOTE="1",
           AMUX_ISOLATED="1", AMUX_NO_SELF_ADOPT="1", TMUX_TMPDIR=os.path.join(HOME, "tmux"),
           AMUX_DASHBOARD_DIR=os.path.join(REPO, "crates", "amux-dashboard", "static"))
server = subprocess.Popen([SERVER], env=env, stdout=open(os.path.join(TMP, "server.log"), "w"), stderr=subprocess.STDOUT)
end = time.time() + 60
while time.time() < end:
    try:
        if api("/health", auth=False)[0] == 200:
            break
    except OSError:
        pass
    time.sleep(0.5)
status, body = api("/api/sessions", {"name": "splitw", "dir": TMP})
check("a worker on the private server", status in (200, 201), f"{status} {body[:200]}")

assert sh("xcrun", "swiftc", "-O", "-o", os.path.join(TMP, "wsctl"), os.path.join(HERE, "wsctl.swift")).returncode == 0
DEEP = f"{URL}/#peek=splitw&tab=chat"


def log():
    try:
        return open(LOG, errors="replace").read()
    except FileNotFoundError:
        return ""


class App:
    def __init__(self, fresh=True):
        if fresh:
            with open(CFG, "w") as f:
                json.dump({"connections": [{"name": "split-e2e", "url": URL}], "lastUrl": URL, "cleanExit": True,
                           "windows": [{"url": DEEP, "frame": "{{120, 120}, {1300, 820}}", "selected": True}]}, f)
        n = log().count("verdict=windows_restored")
        self.p = subprocess.Popen([BIN], env=dict(os.environ, AMUX_DESKTOP_CONFIG=CFG, AMUX_DESKTOP_TEST="1",
                                  AMUX_DESKTOP_TEST_STATE=STATE), stdout=open(LOG, "a"), stderr=subprocess.STDOUT)
        end = time.time() + 25
        while log().count("verdict=windows_restored") <= n and time.time() < end:
            time.sleep(0.3)
        time.sleep(2)
        self.send("activate", 0.5)

    def send(self, u, settle=0.3):
        sh(os.path.join(TMP, "wsctl"), "send", str(self.p.pid), "amux://test/" + u)
        time.sleep(settle)

    def state(self):
        before = os.path.getmtime(STATE) if os.path.exists(STATE) else 0
        self.send("state", 0)
        end = time.time() + 5
        while time.time() < end and (not os.path.exists(STATE) or os.path.getmtime(STATE) == before):
            time.sleep(0.05)
        return json.load(open(STATE))

    def js(self, i, code, timeout=15):
        out = STATE + ".js"
        if os.path.exists(out):
            os.remove(out)
        self.send("js-pane?i=%d&code=%s" % (i, urllib.parse.quote(code)), 0)
        end = time.time() + timeout
        while time.time() < end and not os.path.exists(out):
            time.sleep(0.1)
        return open(out).read() if os.path.exists(out) else "no answer"

    def page(self, i):
        """What pane i shows and whether its server takes it as signed in."""
        r = self.js(i, "const s = await (await fetch('/api/connection/security', {cache: 'no-store'})).json();"
                       "const w = await fetch('/api/sessions', {cache: 'no-store'});"
                       "return JSON.stringify({auth: s.auth, hash: location.hash, peek: typeof peekSession === 'string' ? peekSession : null,"
                       " sessions: w.status, split: !!document.querySelector('body.native-panes')})")
        try:
            return json.loads(r)
        except ValueError:
            return {"error": r}

    def wait_page(self, i, pred, timeout=30):
        end, last = time.time() + timeout, {}
        while time.time() < end:
            last = self.page(i)
            if pred(last):
                return last
            time.sleep(1)
        return last

    def stop(self):
        self.p.send_signal(signal.SIGTERM)
        try:
            self.p.wait(15)
        except subprocess.TimeoutExpired:
            self.p.kill()


def grid(st):
    w = next((w for w in st["windows"] if w.get("grid")), None) or (st["windows"] or [{}])[0]
    return w


import urllib.parse  # noqa: E402

try:
    print("signed out, then signed in on the deep link")
    a = App()
    p0 = a.wait_page(0, lambda p: p.get("auth") in ("sign_in_required", "owner"))
    check("before sign-in the page reads as signed out (the check can fail)", p0.get("auth") == "sign_in_required" and p0.get("sessions") == 401, p0)
    check("the page is on the deep link", p0.get("hash") == "#peek=splitw&tab=chat", p0)
    a.js(0, "const r = await fetch('/api/connection/session', {method: 'POST', headers: {'Content-Type': 'application/json'},"
            " body: JSON.stringify({token: %s})});"
            # What the page's own Sign in does next (app.js _connectionSignIn).
            " location.assign('/api/_clear_sw' + location.hash); return r.status" % json.dumps(TOKEN))
    p0 = a.wait_page(0, lambda p: p.get("auth") == "owner" and p.get("peek") == "splitw")
    check("signed in through the page, the worker open", p0.get("auth") == "owner" and p0.get("peek") == "splitw" and p0.get("sessions") == 200, p0)
    check("the page knows the app can split", p0.get("split") is True, p0)

    print("Split: the page again, on the right half")
    a.send("split", 2)
    st = a.state()
    w = grid(st)
    check("two panes", len(w.get("panes") or []) == 2, w)
    if len(w.get("pane_frames") or []) == 2:
        f = [[float(x) for x in s.replace("{", "").replace("}", "").split(",")] for s in w["pane_frames"]]
        check("side by side, halves", abs(f[0][1] - f[1][1]) < 4 and f[1][0] > f[0][0] and abs(f[0][2] - f[1][2]) < 24, f)
    check("the app logged the split", "verdict=split_button_pane panes=2" in log(), log()[-600:])
    p1 = a.wait_page(1, lambda p: p.get("peek") == "splitw")
    check("the new pane is on the same deep link", p1.get("hash") == "#peek=splitw&tab=chat" and p1.get("peek") == "splitw", p1)
    check("the new pane is signed in", p1.get("auth") == "owner" and p1.get("sessions") == 200, p1)

    print("quit and relaunch")
    a.stop()
    a = App(fresh=False)
    w = grid(a.state())
    check("both panes come back", len(w.get("panes") or []) == 2, w)
    for i in (0, 1):
        p = a.wait_page(i, lambda p: p.get("peek") == "splitw")
        check(f"pane {i} reopens on the deep link, signed in", p.get("auth") == "owner" and p.get("peek") == "splitw" and p.get("sessions") == 200, p)

    # Owner, 2026-10-29: "if i do a split button in a worker it should make the
    # split tab in that tab's pane then rather than the neighboring pane".
    print("Split in a pane splits that pane, down to the deepest level, then beside")
    nested0 = log().count("verdict=split_button_nested")
    for _ in range(4):
        a.send("split", 3)
    w = grid(a.state())
    check("the window keeps its two panes", len(w.get("panes") or []) == 2, w)
    check("each Split nested the pane it was pressed in", log().count("verdict=split_button_nested") - nested0 == 4,
          log().count("verdict=split_button_nested") - nested0)
    check("that pane is now a split of its own", any(" | " in t[0] for t in w.get("pane_tabs") or []), w.get("pane_tabs"))
    a.send("split", 3)
    check("past the deepest level, Split adds a pane beside", "verdict=split_button_sibling reason=too_deep" in log(), log()[-400:])
    a.stop()
finally:
    try:
        a.stop()
    except Exception:
        pass
    server.send_signal(signal.SIGTERM)
    try:
        server.wait(15)
    except subprocess.TimeoutExpired:
        server.kill()
    # The private server by its socket, never `tmux kill-server` bare: inside a
    # pane $TMUX names the FLEET's server and wins over TMUX_TMPDIR.
    sh("env", "-u", "TMUX", "tmux", "-S", os.path.join(HOME, "tmux", f"tmux-{os.getuid()}", "default"), "kill-server")

print(("PASS" if not fails else f"FAIL ({fails})") + f"  log: {LOG}")
sys.exit(1 if fails else 0)
