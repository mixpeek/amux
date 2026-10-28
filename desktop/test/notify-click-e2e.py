#!/usr/bin/env python3
"""A notification click opens what it is about in a new tab of the window
you have, not a new window (owner, 2026-10-28: "when i click a notification
on the mac, it opens up a brand new amux window instead of the current one
with the worker open (in a new tab)").

    desktop/test/notify-click-e2e.py [path/to/Amux.app]

Runs a copy of the app (its own config, AMUX_DESKTOP_TEST=1) with one window
on the board, hides the app the way clicking a notification from another app
finds it, and runs the click handler (amux://test/notification-click calls
the same openNotificationTab a real click does). The page is the real
dashboard from this Mac's amux server.
"""
import json, os, signal, subprocess, sys, tempfile, time

HERE = os.path.dirname(os.path.abspath(__file__))
APP = sys.argv[1] if len(sys.argv) > 1 else os.path.join(HERE, "..", "..", "Amux.app")
BIN = os.path.join(APP, "Contents", "MacOS", "amux")
TMP = tempfile.mkdtemp(prefix="amux-notifyclick-e2e-")
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
worker = os.environ.get("AMUX_SESSION") or "amux"
with open(CFG, "w") as f:
    json.dump({"connections": [{"name": "localhost", "url": url}], "lastUrl": url, "cleanExit": True,
               "windows": [{"url": f"{url}/#view=board", "frame": "{{200, 200}, {900, 600}}"}]}, f)
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
    ws = until(lambda ws: any(w["view"] == "#view=board" for w in ws))
    check("starts with one window on the board", len(ws) == 1 and ws[0]["view"] == "#view=board", str(ws))
    first = ws[0]["number"] if ws else None

    print("amux is hidden (you are in another app) and a worker's notification is clicked")
    send("hide", 1)
    send("notification-click?worker=" + worker)
    ws = until(lambda ws: any(w["selected"] and w["view"].startswith("#peek=" + worker) for w in ws))
    sel = [w for w in ws if w["selected"]]
    check("the worker opens, in the selected tab", len(sel) == 1 and sel[0]["view"].startswith("#peek=" + worker), str(ws))
    check("as a tab of the same window, not a new window", len(ws) == 2 and all(w["tabs"] == 2 for w in ws)
          and any(w["number"] == first for w in ws), str(ws))
    check("the board is still in its tab", any(not w["selected"] and w["view"] == "#view=board" for w in ws), str(ws))

    print("a Messages notification, the same way")
    send("hide", 1)
    send("notification-click?hash=%23messages%3Dw%3A" + worker)
    ws = until(lambda ws: len(ws) == 3 and any(w["selected"] and "messages" in w["view"] for w in ws))
    check("it opens in a third tab of the same window", len(ws) == 3 and all(w["tabs"] == 3 for w in ws), str(ws))
    log = open(LOG, errors="replace").read()
    check("each click is logged with where it opened", log.count("verdict=notification_opened where=new_tab") == 2, log[-600:])
finally:
    p.send_signal(signal.SIGTERM)
    try:
        p.wait(15)
    except subprocess.TimeoutExpired:
        p.kill()
print("PASS" if fails == 0 else f"FAIL ({fails})")
sys.exit(1 if fails else 0)
