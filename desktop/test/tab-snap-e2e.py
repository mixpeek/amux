#!/usr/bin/env python3
"""Dragging a tab out of the Mac app's tab bar to a display edge lands its new
window in that zone (Ethan, 2026-10-21, MSG-92611).

    desktop/test/tab-snap-e2e.py [path/to/Amux.app]

Runs a second copy of the app (its own config, AMUX_DESKTOP_TEST=1) on this
Mac's window server, with two windows tabbed together. Each case restarts it.

What is real: the tab-bar press reaches tabPressed as an NSEvent in the
window's tab bar, the preview panel, the release, finding the window the tab
became, placing it, and the frames the window server then reports.

What is simulated, and why: the pointer. A real drag needs synthetic mouse
events, which macOS refuses this agent (no Accessibility grant), so the test
sets the position and button state the drag code reads (amux://test/pointer)
and tears the tab with moveTabToNewWindow, which is what AppKit does on a drop
outside every tab bar. So this does NOT prove that AppKit delivers the press
to the app's event monitor or keeps the timer firing during its own drag
session; a person dragging once does.
"""
import json, os, signal, subprocess, sys, tempfile, time

HERE = os.path.dirname(os.path.abspath(__file__))
APP = sys.argv[1] if len(sys.argv) > 1 else os.path.join(HERE, "..", "..", "Amux.app")
BIN = os.path.join(APP, "Contents", "MacOS", "amux")
TMP = tempfile.mkdtemp(prefix="amux-tabsnap-e2e-")
CFG, LOG, STATE = (os.path.join(TMP, n) for n in ("config.json", "app.log", "state.json"))
fails = 0


def sh(*a):
    return subprocess.run(a, capture_output=True, text=True)


def check(label, ok, detail=""):
    global fails
    print(("  ok   " if ok else "  FAIL ") + label + ("" if ok else f": {detail}"))
    fails += 0 if ok else 1


def rect(s):
    x, y, w, h = (float(v) for v in s.replace("{", "").replace("}", "").split(","))
    return x, y, w, h


def near(a, b, tol=3):
    return all(abs(p - q) <= tol for p, q in zip(a, b))


assert sh("xcrun", "swiftc", "-O", "-o", os.path.join(TMP, "wsctl"), os.path.join(HERE, "wsctl.swift")).returncode == 0
url = sh("amux", "url").stdout.strip() or "https://localhost:8824"
d0 = json.loads(sh(os.path.join(TMP, "wsctl"), "displays").stdout.splitlines()[0])
vx, vy, vw, vh = rect(d0["visible"])
# Source window: off the 12x8 grid by 10pt on its left edge, so a grid snap is visible.
col = vw / 12
src = (vx + 2 * col + 10, vy + vh * 0.2, vw * 0.45, vh * 0.55)
src_s = "{{%d, %d}, {%d, %d}}" % src


class App:
    def __init__(self, extra=()):
        with open(CFG, "w") as f:
            json.dump({"connections": [{"name": "localhost", "url": url}], "lastUrl": url, "cleanExit": True,
                       "windows": list(extra) + [{"url": url + "/#view=board", "frame": src_s, "tabGroup": 1, "selected": False},
                                                 {"url": url + "/#view=messages", "frame": src_s, "tabGroup": 1, "selected": True}]}, f)
        n = self.count("verdict=windows_restored")
        self.p = subprocess.Popen([BIN], env=dict(os.environ, AMUX_DESKTOP_CONFIG=CFG, AMUX_DESKTOP_TEST="1",
                                  AMUX_DESKTOP_TEST_STATE=STATE), stdout=open(LOG, "a"), stderr=subprocess.STDOUT)
        end = time.time() + 20
        while self.count("verdict=windows_restored") <= n and time.time() < end:
            time.sleep(0.3)
        time.sleep(2)

    @staticmethod
    def count(s):
        try:
            return open(LOG, errors="replace").read().count(s)
        except FileNotFoundError:
            return 0

    def send(self, u, settle=0.25):
        sh(os.path.join(TMP, "wsctl"), "send", str(self.p.pid), "amux://test/" + u)
        time.sleep(settle)

    def state(self):
        before = os.path.getmtime(STATE) if os.path.exists(STATE) else 0
        self.send("state", 0)
        end = time.time() + 5
        while time.time() < end and (not os.path.exists(STATE) or os.path.getmtime(STATE) == before):
            time.sleep(0.05)
        return json.load(open(STATE))

    def stop(self):
        self.p.send_signal(signal.SIGTERM)
        try:
            self.p.wait(10)
        except subprocess.TimeoutExpired:
            self.p.kill()


def zone(st, fx, fy, fw, fh):
    x, y, w, h = rect(st["visible_frame"])
    return (round(x + w * fx), round(y + h * fy), round(w * fw), round(h * fh))


def drag_tab(name, at, expect, press="press-tab", tear=True, snap=True, extra=(), tear_after_release=False):
    """Press in the tab bar, move to `at` (a function of the state) with the
    button down, drop, and check where the new window lands."""
    print(name)
    app = App(extra)
    try:
        st = app.state()
        check("starts with the two tabs together", sum(1 for w in st["windows"] if w["tabs"] == 2) == 2, str(st["windows"]))
        if not snap:
            app.send("snap?on=0")
        # The button is down where the press lands (the tab bar), as in a real drag.
        app.send(f"pointer?x={src[0] + src[2] / 2}&y={src[1] + src[3] - 10}&down=1", 0.1)
        if press:
            app.send(press)
        x, y = at(st)
        app.send(f"pointer?x={x}&y={y}&down=1", 0.4)
        mid = app.state()
        log_before = App.count("verdict=tab_")
        if tear and not tear_after_release:
            app.send("tear-tab" if tear is True else tear, 0.1)
        if tear and tear_after_release:
            # A real AppKit drag splits the tab off at the drop, with the release.
            app.send("drop-tab", 1.5)
        else:
            app.send(f"pointer?x={x}&y={y}&down=0", 1.2)
        end = app.state()
        return st, mid, end, App.count("verdict=tab_") > log_before
    finally:
        app.stop()


def torn(end):
    one = [w for w in end["windows"] if w["tabs"] == 1]
    return one


edge_left = lambda st: (rect(st["screen_frame"])[0], rect(st["visible_frame"])[1] + rect(st["visible_frame"])[3] / 2)
bottom_mid = lambda st: (rect(st["visible_frame"])[0] + rect(st["visible_frame"])[2] / 2, rect(st["visible_frame"])[1] + 2)
top_right = lambda st: (rect(st["screen_frame"])[0] + rect(st["screen_frame"])[2] - 1, rect(st["screen_frame"])[1] + rect(st["screen_frame"])[3] - 1)
screen_mid = lambda st: (rect(st["visible_frame"])[0] + rect(st["visible_frame"])[2] * 0.8, rect(st["visible_frame"])[1] + rect(st["visible_frame"])[3] * 0.85)

for name, at, z in [("1. tab to the left edge: the left half", edge_left, (0, 0, 0.5, 1)),
                    ("2. tab to the bottom middle: the center third", bottom_mid, (1 / 3, 0, 1 / 3, 1)),
                    ("3. tab to the top right corner: that quarter", top_right, (0.5, 0.5, 0.5, 0.5))]:
    st, mid, end, logged = drag_tab(name, at, z)
    want = zone(st, *z)
    check("the preview shows during the drag, at the zone", mid["preview"] and near(rect(mid["preview_frame"]), want), f"{mid['preview']} {mid['preview_frame']} want {want}")
    t = torn(end)
    check("the tab became its own window", len(t) == 2, str(end["windows"]))
    moved = [w for w in t if near(rect(w["frame"]), want)]
    check("that window lands in the zone", len(moved) == 1, f"{[w['frame'] for w in t]} want {want}")
    stayed = [w for w in t if near(rect(w["frame"]), src)]
    check("the window it left stays where it was", len(stayed) == 1, f"{[w['frame'] for w in t]} src {src}")
    check("the preview is gone after the drop", not end["preview"])
    check("the snap is logged", logged)

st, mid, end, logged = drag_tab("4. tab dropped away from every edge: no zone, the grid places it", screen_mid, None)
t = torn(end)
check("no preview away from the edges", not mid["preview"], mid["preview_frame"])
check("the torn window keeps its size", len(t) == 2 and all(abs(rect(w["frame"])[2] - src[2]) <= 2 and abs(rect(w["frame"])[3] - src[3]) <= 2 for w in t), str(t))
check("and it says the grid placed it", "verdict=tab_torn_off_snapped zone=grid" in open(LOG, errors="replace").read())

st, mid, end, logged = drag_tab("5. a click in the page then a drag is not a tab drag", edge_left, None, press="press-content", tear=False)
check("nothing tracks it and nothing previews", not mid["tracking"] and not mid["preview"], str(mid))
check("no window is moved into a zone", not any(near(rect(w["frame"]), zone(st, 0, 0, 0.5, 1)) for w in end["windows"]), str(end["windows"]))

st, mid, end, logged = drag_tab("6. a tab dropped back into a tab bar is not placed", edge_left, None, tear=False)
check("both tabs stay together where they were", all(w["tabs"] == 2 and near(rect(w["frame"]), src) for w in end["windows"]), str(end["windows"]))
check("it says the tab was not torn off", "verdict=tab_drag_not_torn_off" in open(LOG, errors="replace").read())

st, mid, end, logged = drag_tab("7. snap to grid turned off: the torn tab lands where it was dropped", edge_left, None, snap=False)
check("no preview, no zone", not mid["preview"] and not any(near(rect(w["frame"]), zone(st, 0, 0, 0.5, 1)) for w in end["windows"]), str(end["windows"]))

other = "{{%d, %d}, {%d, %d}}" % (vx + vw * 0.55, vy + vh * 0.1, vw * 0.4, vh * 0.4)
st, mid, end, logged = drag_tab("9. a tab dropped on another window's tab bar joins it and nothing is placed", edge_left, None,
                                tear="join-tab", extra=[{"url": url + "/#view=calendar", "frame": other}])
check("the tab joined the other window", any(w["title"] == "Messages" and w["tabs"] == 2 for w in end["windows"]), str(end["windows"]))
check("the tab it left is not moved", any(w["title"] == "Board" and w["tabs"] == 1 and near(rect(w["frame"]), src) for w in end["windows"]), str(end["windows"]))
check("no window is put in the zone the drag passed over", not any(near(rect(w["frame"]), zone(st, 0, 0, 0.5, 1)) for w in end["windows"]), str(end["windows"]))

# Ethan's real drag (2026-10-22 10:48) got no preview and no zone: the press
# monitor never tracked it. The watcher places a tab that leaves its group while
# the button is down, with no press seen at all.
st, mid, end, logged = drag_tab("10. the press never reaches the app: the tab is still placed in its zone", edge_left, None, press=None)
want = zone(st, 0, 0, 0.5, 1)
check("the watcher sees the tear and previews the zone", mid["preview"] and near(rect(mid["preview_frame"]), want) or "tab_tear_seen_by_watch" in open(LOG, errors="replace").read(), mid["preview_frame"])
check("the torn tab lands in the left half", any(w["tabs"] == 1 and near(rect(w["frame"]), want) for w in end["windows"]), str(end["windows"]))
check("it says the watcher found it", "verdict=tab_tear_seen_by_watch" in open(LOG, errors="replace").read())

n0 = open(LOG, errors="replace").read().count("verdict=tab_torn_off_snapped")
st, mid, end, logged = drag_tab("11. AppKit splits the tab off at the drop: placed once, in the zone", edge_left, None, tear_after_release=True)
want = zone(st, 0, 0, 0.5, 1)
placed = open(LOG, errors="replace").read().count("verdict=tab_torn_off_snapped") - n0
check("the torn tab lands in the left half", any(w["tabs"] == 1 and near(rect(w["frame"]), want) for w in end["windows"]), str(end["windows"]))
check("it is placed once, not again by the watcher", placed == 1, f"{placed} placements")

print("8. a whole window dragged to the right edge: the right half")
app = App()
try:
    st = app.state()
    app.send(f"pointer?x={rect(st['screen_frame'])[0] + rect(st['screen_frame'])[2] - 1}&y={vy + vh / 2}&down=1", 0.1)
    app.send("window-drag", 0.4)
    mid = app.state()
    want = zone(st, 0.5, 0, 0.5, 1)
    check("the preview shows the right half", mid["preview"] and near(rect(mid["preview_frame"]), want), mid["preview_frame"])
    app.send(f"pointer?x={rect(st['screen_frame'])[0] + rect(st['screen_frame'])[2] - 1}&y={vy + vh / 2}&down=0", 1.2)
    end = app.state()
    # The visible tab is the window that moves; AppKit moves its hidden tab with it on the window server.
    check("the window lands in the right half, both tabs still in it", any(near(rect(w["frame"]), want) and w["tabs"] == 2 for w in end["windows"]), str(end["windows"]))
    check("it is logged", "verdict=window_snapped_zone zone=right half" in open(LOG, errors="replace").read())
finally:
    app.stop()

print(f"PASS: tab-snap, all checks passed" if fails == 0 else f"tab-snap: {fails} check(s) FAILED (log {LOG})")
sys.exit(1 if fails else 0)
