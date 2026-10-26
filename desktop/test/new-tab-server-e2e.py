#!/usr/bin/env python3
"""A new tab opens on the server of the tab it was opened from (Ethan,
2026-10-26: "if i open a new tab from an active one that has a certain
connection (like sandbox) it should open the tab in that").

    desktop/test/new-tab-server-e2e.py [path/to/Amux.app]

Two connections to this Mac's amux server under two names, localhost (the
default) and 127.0.0.1, are two servers to the app (different origins). The
window is on 127.0.0.1; its new tab must be too, and so must the address the
tab saves for a relaunch.

Runs a copy of the app (its own config, AMUX_DESKTOP_TEST=1) with one window
on a worker, presses + (amux://test/new-tab calls the button's action,
newWindowForTab), and reads back what the new tab's page reports it shows.
The page is the real dashboard from this Mac's amux server.
"""
import json, os, signal, subprocess, sys, tempfile, time

HERE = os.path.dirname(os.path.abspath(__file__))
APP = sys.argv[1] if len(sys.argv) > 1 else os.path.join(HERE, "..", "..", "Amux.app")
BIN = os.path.join(APP, "Contents", "MacOS", "amux")
TMP = tempfile.mkdtemp(prefix="amux-newtab-server-e2e-")
CFG, LOG, STATE = (os.path.join(TMP, n) for n in ("config.json", "app.log", "state.json"))
fails = 0


def sh(*a):
    return subprocess.run(a, capture_output=True, text=True)


def check(label, ok, detail=""):
    global fails
    print(("  ok   " if ok else "  FAIL ") + label + ("" if ok else f": {detail}"))
    fails += 0 if ok else 1


assert sh("xcrun", "swiftc", "-O", "-o", os.path.join(TMP, "wsctl"), os.path.join(HERE, "wsctl.swift")).returncode == 0
url = sh("amux", "url").stdout.strip() or "https://localhost:8824"
port = url.rsplit(":", 1)[-1].strip("/")
default, other = f"https://localhost:{port}", f"https://127.0.0.1:{port}"
worker = os.environ.get("AMUX_SESSION") or "amux"
with open(CFG, "w") as f:
    json.dump({"connections": [{"name": "localhost", "url": default}, {"name": "second", "url": other}],
               "lastUrl": default, "cleanExit": True,
               "windows": [{"url": f"{other}/#peek={worker}", "frame": "{{200, 200}, {900, 600}}"}]}, f)
p = subprocess.Popen([BIN], env=dict(os.environ, AMUX_DESKTOP_CONFIG=CFG, AMUX_DESKTOP_TEST="1", AMUX_DESKTOP_TEST_STATE=STATE),
                     stdout=open(LOG, "a"), stderr=subprocess.STDOUT)
end = time.time() + 20
while time.time() < end and "verdict=windows_restored" not in open(LOG, errors="replace").read():
    time.sleep(0.3)


def send(u, settle=0.3):
    sh(os.path.join(TMP, "wsctl"), "send", str(p.pid), "amux://test/" + u)
    time.sleep(settle)


def state():
    before = os.path.getmtime(STATE) if os.path.exists(STATE) else 0
    send("state", 0)
    end = time.time() + 5
    while time.time() < end and (not os.path.exists(STATE) or os.path.getmtime(STATE) == before):
        time.sleep(0.05)
    return json.load(open(STATE))["windows"]


def until(cond, t=30):
    end = time.time() + t
    while time.time() < end:
        ws = state()
        if cond(ws):
            return ws
        time.sleep(0.5)
    return state()


try:
    print("a new tab opens on the server of the tab it was opened from")
    ws = until(lambda ws: any(w.get("server") == other for w in ws))
    check("starts with the window on the second server", any(w.get("server") == other for w in ws), str(ws))
    send("new-tab")
    ws = until(lambda ws: any(w["selected"] and w["view"] == "#view=sessions" for w in ws))
    sel = [w for w in ws if w["selected"]]
    check("the new tab is selected and shows the worker list", len(sel) == 1 and sel[0]["view"] == "#view=sessions", str(ws))
    check("the new tab is on the second server, not the default", len(sel) == 1 and sel[0].get("server") == other, str(sel))
    check("what it saves for a relaunch is on the second server", len(sel) == 1 and sel[0].get("saved", "").startswith(other), str(sel))
    check("the first tab still saves its own server", any(not w["selected"] and w.get("saved", "").startswith(other) for w in ws), str(ws))
finally:
    p.send_signal(signal.SIGTERM)
    try:
        p.wait(10)
    except subprocess.TimeoutExpired:
        p.kill()

print(f"\n{'PASS' if not fails else 'FAIL'}: new-tab-server-e2e ({fails} failed) log={LOG}")
sys.exit(1 if fails else 0)
