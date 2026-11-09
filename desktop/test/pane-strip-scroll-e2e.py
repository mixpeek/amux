#!/usr/bin/env python3
"""A pane's tab strip keeps tabs readable and scrolls sideways (owner,
2026-11-09: "Should I be able to scroll tabs horizontally? Or should they be
squeezed like they are"). Tabs share the room down to 120 points each; past
that the strip scrolls, and the shown tab is scrolled into view.

    desktop/test/pane-strip-scroll-e2e.py [path/to/Amux.app]

Runs a copy of the app (its own config, AMUX_DESKTOP_TEST=1) with a window
split in two: one pane with nine tabs, one with two. Reads each strip back
from amux://test/state (pane_strips) and scrolls one with
amux://test/pane-strip-scroll, the path a trackpad swipe takes.
"""
import json, os, signal, subprocess, sys, tempfile, time

HERE = os.path.dirname(os.path.abspath(__file__))
APP = sys.argv[1] if len(sys.argv) > 1 else os.path.join(HERE, "..", "..", "Amux.app")
BIN = os.path.join(APP, "Contents", "MacOS", "amux")
TMP = tempfile.mkdtemp(prefix="amux-strip-e2e-")
CFG, LOG, STATE = (os.path.join(TMP, n) for n in ("config.json", "app.log", "state.json"))
MIN_W, MAX_W = 120, 200
fails = 0


def sh(*a):
    return subprocess.run(a, capture_output=True, text=True)


def check(label, ok, detail=""):
    global fails
    print(("  ok   " if ok else "  FAIL ") + label + ("" if ok else f": {detail}"))
    fails += 0 if ok else 1
    return ok


def log():
    try:
        return open(LOG, errors="replace").read()
    except FileNotFoundError:
        return ""


assert sh("xcrun", "swiftc", "-O", "-o", os.path.join(TMP, "wsctl"), os.path.join(HERE, "wsctl.swift")).returncode == 0
url = sh("amux", "url").stdout.strip() or "https://localhost:8824"
views = ["board", "calendar", "map", "sessions", "messages", "usage", "schedules", "memory", "files"]
many = [{"url": f"{url}/#view={v}", "title": v.title()} for v in views]
json.dump({"connections": [{"name": "localhost", "url": url}], "lastUrl": url, "cleanExit": True, "launch": "last",
           "windows": [{"url": url + "/#view=board", "frame": "{{80, 80}, {1100, 700}}", "selected": True,
                        "grid": {"tree": "r0.500(0,1)",
                                 "hostTabs": [{"url": url + "/#view=calendar", "title": "Calendar"}], "hostActive": 0,
                                 "panes": [dict(many[0], tabs=many[1:], active=len(many) - 1)]}}]}, open(CFG, "w"))
p = subprocess.Popen([BIN], env=dict(os.environ, AMUX_DESKTOP_CONFIG=CFG, AMUX_DESKTOP_TEST="1", AMUX_DESKTOP_TEST_STATE=STATE),
                     stdout=open(LOG, "a"), stderr=subprocess.STDOUT)


def send(u, settle=0.4):
    sh(os.path.join(TMP, "wsctl"), "send", str(p.pid), "amux://test/" + u)
    time.sleep(settle)


def strips():
    before = os.path.getmtime(STATE) if os.path.exists(STATE) else 0
    send("state", 0)
    end = time.time() + 5
    while time.time() < end and (not os.path.exists(STATE) or os.path.getmtime(STATE) == before):
        time.sleep(0.05)
    for w in json.load(open(STATE))["windows"]:
        if w.get("pane_strips"):
            return w
    return None


def visible(s, i):
    t = s["tabs"][i]
    return t["x"] >= -0.5 and t["x"] + t["w"] <= s["room"] + 0.5


try:
    end = time.time() + 30
    while time.time() < end and "verdict=windows_restored" not in log():
        time.sleep(0.3)
    send("activate", 0.5)
    w = None
    end = time.time() + 20
    while time.time() < end:
        w = strips()
        if w and len(w["pane_strips"]) == 2 and len(w["pane_strips"][1].get("tabs", [])) == len(views):
            break
        time.sleep(0.5)
    if not check("a window split in two, the second pane with nine tabs", bool(w) and len(w["pane_strips"]) == 2, w):
        raise SystemExit(1)
    two, nine = w["pane_strips"]
    print(f"    strips: two tabs in {two['room']:.0f}pt, nine tabs in {nine['room']:.0f}pt")
    check("nine tabs do not fit the room", len(views) * MIN_W > nine["room"], nine["room"])
    widths = [t["w"] + 2 for t in nine["tabs"]]
    check(f"no tab is squeezed below {MIN_W}pt", min(widths) >= MIN_W - 0.5, widths)
    check("the shown tab (the last) is scrolled into view", visible(nine, len(views) - 1) and nine["scroll"] > 0, nine)
    check("a pane whose tabs fit does not scroll, and none is wider than 200pt",
          two["scroll"] == 0 and all(t["w"] + 2 <= MAX_W + 0.5 for t in two["tabs"]) and visible(two, len(two["tabs"]) - 1), two)

    send("pane-focus?i=1")
    send("pane-strip-scroll?dx=-5000")
    s = strips()["pane_strips"][1]
    check("scrolled all the way left: the first tab at the start", s["scroll"] == 0 and abs(s["tabs"][0]["x"]) < 0.5, s)
    send("pane-strip-scroll?dx=150")
    s = strips()["pane_strips"][1]
    check("a swipe moves the strip by what it was asked", abs(s["scroll"] - 150) < 0.5, s)
    send("pane-strip-scroll?dx=99999")
    s = strips()["pane_strips"][1]
    last = s["tabs"][-1]
    check("and no further than the last tab's end", abs(last["x"] + last["w"] + 2 - s["room"]) < 1, s)

    send("pane-tab-select?i=1&t=0", 0.8)
    s = strips()["pane_strips"][1]
    check("picking the first tab while scrolled right brings it into view", visible(s, 0), s)
    w = strips()
    check("and it is the tab shown", w["pane_active"][1] == 0, w["pane_active"])
finally:
    p.send_signal(signal.SIGTERM)
    try:
        p.wait(15)
    except subprocess.TimeoutExpired:
        p.kill()

print(f"{'PASS' if fails == 0 else 'FAIL'}: {fails} failing check(s)")
sys.exit(1 if fails else 0)
