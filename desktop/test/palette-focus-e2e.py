#!/usr/bin/env python3
"""⌘K on a place already open brings that tab or pane forward (owner,
2026-10-29, MSG-100765: "if i do cmd + k and select something for which a tab
is already open it goes to that tab").

    desktop/test/palette-focus-e2e.py [path/to/Amux.app]

A second copy of the app with its own config opens a window split in two,
Board | Calendar. From the Board pane, the palette's pick of Calendar (the
same call _cmdkRun makes) must focus the Calendar pane and leave the Board
pane where it is; a pick of Messages, open nowhere, must open in place.
"""
import json, os, re, signal, subprocess, sys, tempfile, time

HERE = os.path.dirname(os.path.abspath(__file__))
APP = sys.argv[1] if len(sys.argv) > 1 else os.path.join(HERE, "..", "..", "Amux.app")
BIN = os.path.join(APP, "Contents", "MacOS", "amux")
TMP = tempfile.mkdtemp(prefix="amux-palette-e2e-")
CFG, LOG, ST = (os.path.join(TMP, n) for n in ("config.json", "app.log", "state.json"))
fails = 0
p = None


def sh(*a):
    return subprocess.run(a, capture_output=True, text=True)


def check(label, ok, detail=""):
    global fails
    print(("  ok   " if ok else "  FAIL ") + label + ("" if ok else f": {detail}"))
    fails += 0 if ok else 1


assert sh("xcrun", "swiftc", "-O", "-o", os.path.join(TMP, "wsctl"), os.path.join(HERE, "wsctl.swift")).returncode == 0
# A private server serving this checkout's dashboard (the page side of the
# change), with its own home and tmux.
import socket, ssl, urllib.request
REPO = os.path.abspath(os.path.join(HERE, "..", ".."))
SERVER = os.environ.get("AMUX_E2E_SERVER_BIN") or os.path.expanduser("~/.local/bin/amux-server-rs")
_s = socket.socket(); _s.bind(("127.0.0.1", 0)); PORT = _s.getsockname()[1]; _s.close()
url = f"https://localhost:{PORT}"
SHOME = os.path.join(TMP, "home"); os.makedirs(os.path.join(SHOME, "tmux"))
_env = {k: v for k, v in os.environ.items() if k not in ("TMUX", "TMUX_PANE")}
_env.update(AMUX_HOME=SHOME, AMUX_RS_PORT=str(PORT), AMUX_ISOLATED="1", AMUX_NO_SELF_ADOPT="1", TMUX_TMPDIR=os.path.join(SHOME, "tmux"),
            AMUX_DASHBOARD_DIR=os.path.join(REPO, "crates", "amux-dashboard", "static"))
server = subprocess.Popen([SERVER], env=_env, stdout=open(os.path.join(TMP, "server.log"), "w"), stderr=subprocess.STDOUT)
_ctx = ssl.create_default_context(); _ctx.check_hostname, _ctx.verify_mode = False, ssl.CERT_NONE
for _ in range(120):
    try:
        if urllib.request.urlopen(url + "/health", context=_ctx, timeout=2).status == 200:
            break
    except OSError:
        pass
    time.sleep(0.5)


def log():
    return open(LOG, errors="replace").read() if os.path.exists(LOG) else ""


def start():
    global p
    n = log().count("verdict=windows_restored")
    p = subprocess.Popen([BIN], env=dict(os.environ, AMUX_DESKTOP_CONFIG=CFG, AMUX_DESKTOP_TEST="1", AMUX_DESKTOP_TEST_STATE=ST),
                         stdout=open(LOG, "a"), stderr=subprocess.STDOUT)
    end = time.time() + 30
    while time.time() < end and log().count("verdict=windows_restored") <= n:
        time.sleep(0.3)
    settle()


def settle(secs=30):
    end = time.time() + secs
    while time.time() < end:
        try:
            w = win()
            if w and "amux" not in [t for tabs in w.get("pane_tabs", []) for t in tabs]:
                return
        except (OSError, ValueError, KeyError):
            pass
        time.sleep(0.5)


def stop():
    p.send_signal(signal.SIGTERM)
    try:
        p.wait(10)
    except subprocess.TimeoutExpired:
        p.kill()


def send(u, settle_s=1.5):
    sh(os.path.join(TMP, "wsctl"), "send", str(p.pid), u)
    time.sleep(settle_s)


def state():
    before = os.path.getmtime(ST) if os.path.exists(ST) else 0
    sh(os.path.join(TMP, "wsctl"), "send", str(p.pid), "amux://test/state")
    end = time.time() + 5
    while time.time() < end and (not os.path.exists(ST) or os.path.getmtime(ST) == before):
        time.sleep(0.05)
    return json.load(open(ST))


def win():
    return next((w for w in state()["windows"] if w.get("grid")), None)



def js(i, code, timeout=15):
    import urllib.parse
    out = ST + ".js"
    if os.path.exists(out):
        os.remove(out)
    send("amux://test/js-pane?i=%d&code=%s" % (i, urllib.parse.quote(code)), 0)
    end = time.time() + timeout
    while time.time() < end and not os.path.exists(out):
        time.sleep(0.1)
    return open(out).read() if os.path.exists(out) else "no answer"


PICK = ("window.__opened = 0; const used = _cmdkFocusElsewhere(%s, () => { window.__opened = 1; });"
        " await new Promise(r => setTimeout(r, 1200)); return JSON.stringify({used, opened: window.__opened, hash: location.hash})")

json.dump({"connections": [{"name": "localhost", "url": url}], "lastUrl": url, "cleanExit": True, "launch": "last",
           "windows": [{"url": url + "/#view=board", "frame": "{{60, 60}, {1300, 820}}",
                        "grid": {"tree": "r0.500(0,1)", "panes": [{"url": url + "/#view=calendar", "title": "Calendar"}]}}]}, open(CFG, "w"))
try:
    start()
    time.sleep(2)
    send("amux://test/pane-focus?i=0", 1)
    r = json.loads(js(0, PICK % "'#view=calendar'") or "{}")
    check("picking Calendar from the Board pane does not open it there", r.get("used") is True and r.get("opened") == 0, r)
    check("the Board pane stays on the Board", "view=board" in r.get("hash", ""), r)
    check("the Calendar pane is focused", win()["focused"] == 1, win().get("focused"))
    check("and it is logged", "verdict=palette_focus_existing place=view=calendar pane=1" in log(), log()[-400:])
    send("amux://test/pane-focus?i=0", 1)
    r = json.loads(js(0, PICK % "'#view=messages'") or "{}")
    check("a place open nowhere opens where you are", r.get("opened") == 1, r)
    check("and that is logged", "verdict=palette_focus_none place=view=messages" in log(), log()[-400:])
finally:
    if p and p.poll() is None:
        stop()
    server.terminate()
print("PASS: palette-focus, all checks passed" if fails == 0 else f"palette-focus: {fails} check(s) FAILED (log {LOG})")
sys.exit(1 if fails else 0)
