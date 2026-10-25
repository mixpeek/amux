#!/usr/bin/env python3
"""A grid of windows saved as a workspace keeps each window's tab names and
its zone (Ethan, 2026-10-21: "that grid that i have should be the layout that
can be saved, with the state of each, with the tab names for each").

    desktop/test/grid-workspace-e2e.py [path/to/Amux.app]

Runs a second copy of the app with its own config, as workspace-e2e.py does.
Opens three windows in a grid (left half, top right, bottom right with two
tabs), saves it, scrambles it with Arrange > Fill, reopens it, and checks
against the window server: the same tab names at the same places.
"""
import json, os, signal, subprocess, sys, tempfile, time

HERE = os.path.dirname(os.path.abspath(__file__))
APP = sys.argv[1] if len(sys.argv) > 1 else os.path.join(HERE, "..", "..", "Amux.app")
BIN = os.path.join(APP, "Contents", "MacOS", "amux")
TMP = tempfile.mkdtemp(prefix="amux-grid-e2e-")
CFG, LOG = os.path.join(TMP, "config.json"), os.path.join(TMP, "app.log")
fails = 0


def sh(*a):
    return subprocess.run(a, capture_output=True, text=True)


def check(label, ok, detail=""):
    global fails
    print(("  ok   " if ok else "  FAIL ") + label + ("" if ok else f": {detail}"))
    fails += 0 if ok else 1


def ctl(*a):
    return [json.loads(l) for l in sh(os.path.join(TMP, "wsctl"), *map(str, a)).stdout.splitlines() if l.strip()]


def cfg():
    with open(CFG) as f:
        return json.load(f)


def wait(cond, secs, step=0.5):
    end = time.time() + secs
    while time.time() < end:
        v = cond()
        if v:
            return v
        time.sleep(step)
    return cond()


assert sh("xcrun", "swiftc", "-O", "-o", os.path.join(TMP, "wsctl"), os.path.join(HERE, "wsctl.swift")).returncode == 0
url = sh("amux", "url").stdout.strip() or "https://localhost:8824"
disp = ctl("displays")[0]
vx, vy, vw, vh = (float(v) for v in disp["visible"].replace("{", "").replace("}", "").split(","))


def rect(fx, fy, fw, fh):
    return "{{%d, %d}, {%d, %d}}" % (vx + vw * fx, vy + vh * fy, vw * fw, vh * fh)


grid = [  # url, frame, tab group, selected
    (url + "/#view=board", rect(0, 0, 0.5, 1), None, None),
    (url + "/#view=messages", rect(0.5, 0.5, 0.5, 0.5), None, None),
    (url + "/#view=calendar", rect(0.5, 0, 0.5, 0.5), 1, True),
    (url + "/#view=schedules", rect(0.5, 0, 0.5, 0.5), 1, False),
]
with open(CFG, "w") as f:
    json.dump({"connections": [{"name": "localhost", "url": url}], "lastUrl": url, "cleanExit": True, "launch": "last",
               "windows": [{"url": u, "frame": fr, "tabGroup": g, "selected": s} for u, fr, g, s in grid]}, f)

p = subprocess.Popen([BIN], env=dict(os.environ, AMUX_DESKTOP_CONFIG=CFG), stdout=open(LOG, "a"), stderr=subprocess.STDOUT)
try:
    wait(lambda: "verdict=windows_restored" in open(LOG, errors="replace").read(), 20)
    # The pages load and name their windows ("Board", "Messages", ...).
    titled = wait(lambda: (lambda ws: ws if len(ws) >= 3 and all(w["title"] not in ("", "amux") for w in ws) else None)(ctl("windows", p.pid)), 30)
    check("the grid opened with its pages named", bool(titled), json.dumps(ctl("windows", p.pid)))
    visible = lambda: sorted(w["title"] for w in ctl("windows", p.pid) if w["onscreen"] and w["title"] in ("Calendar", "Workers"))
    check("the tab saved as visible is the one showing", visible() == ["Calendar"], str(visible()))
    ctl("send", p.pid, "amux://save-workspace/Grid")
    ws = wait(lambda: next((l for l in cfg().get("layouts") or [] if l["name"] == "Grid"), None), 10)
    check("the workspace saved", bool(ws))
    titles = [w.get("title") for w in (ws or {}).get("windows", [])]
    check("every saved window has its tab name", len(titles) == 4 and all(titles), str(titles))
    check("the tab group's hidden tab kept its name too", any(w.get("tabGroup") and w.get("selected") is False and w.get("title")
                                                             for w in (ws or {}).get("windows", [])), str(titles))
    before = {w["title"]: (w["x"], w["y"], w["w"], w["h"]) for w in ctl("windows", p.pid)}
    ctl("send", p.pid, "amux://arrange/tile")
    time.sleep(4)
    moved = {w["title"]: (w["x"], w["y"], w["w"], w["h"]) for w in ctl("windows", p.pid)}
    check("scrambling moved the grid", moved != before, str(moved))
    ctl("send", p.pid, "amux://workspace/Grid")
    time.sleep(5)
    after = {w["title"]: (w["x"], w["y"], w["w"], w["h"]) for w in ctl("windows", p.pid)}
    close = lambda a, b: all(abs(x - y) <= 16 for x, y in zip(a, b))
    check("reopening shows the same tab of the group", visible() == ["Calendar"], str(visible()))
    check("reopening puts each named window back in its zone",
          set(after) == set(before) and all(close(after[t], before[t]) for t in before), f"before={before} after={after}")
    log = open(LOG, errors="replace").read()
    check("the save is logged", "verdict=layout_saved name=Grid windows=4" in log)
    # The same lines Window > Workspaces and Manage Workspaces show for it.
    shows = next((l.split("shows=", 1)[1] for l in log.splitlines() if "verdict=layout_saved name=Grid" in l and "shows=" in l), "")
    check("each window is named by its tabs and zone", " (left half)" in shows and " (top right)" in shows
          and " (bottom right)" in shows and shows.count(" | ") == 2, shows)
    check("the tab group lists both its tabs, the visible one first",
          any(", " in part and part.endswith("(bottom right)") for part in shows.split(" | ")), shows)
finally:
    p.send_signal(signal.SIGTERM)
    try:
        p.wait(10)
    except subprocess.TimeoutExpired:
        p.kill()
print(f"PASS: grid-workspace, all checks passed" if fails == 0 else f"grid-workspace: {fails} check(s) FAILED (log {LOG})")
sys.exit(1 if fails else 0)
