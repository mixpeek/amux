#!/usr/bin/env python3
"""A tab dragged into an amux window's content splits THAT window into panes
(Ethan, 2026-10-23: "me dragging a tab into a section in the amux mac os app
window should create the grid").

    desktop/test/pane-grid-e2e.py [path/to/Amux.app]

Runs a second copy of the app (its own config, AMUX_DESKTOP_TEST=1) against
the local amux server, with one window of five tabs. One run walks the grid
through its life: two, three, four panes, a full grid refusing a fifth, a
divider moved, a quit and relaunch, a pane back to a tab, a pane dragged out,
a pane closed, the window's own page closed.

Real: the tab-bar press reaching tabPressed, the window under the pointer
(the window server's own z-order), the drop preview panel, finding the window
the tab became, the web views moving into split views, and the frames the
window server reports for each pane.

Simulated, as in tab-snap-e2e.py: the pointer (amux://test/pointer) and the
tear (moveTabToNewWindow, what AppKit does with a tab dropped outside every
tab bar), because macOS refuses this agent synthetic mouse events. A pane's
header drag and its buttons go through the same handlers a click reaches
(amux://test/pane-*), not through a real mouse.
"""
import json, os, re, signal, subprocess, sys, tempfile, time, urllib.parse

HERE = os.path.dirname(os.path.abspath(__file__))
APP = sys.argv[1] if len(sys.argv) > 1 else os.path.join(HERE, "..", "..", "Amux.app")
BIN = os.path.join(APP, "Contents", "MacOS", "amux")
TMP = tempfile.mkdtemp(prefix="amux-panegrid-e2e-")
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


def near(a, b, tol=4):
    return all(abs(p - q) <= tol for p, q in zip(a, b))


def log():
    try:
        return open(LOG, errors="replace").read()
    except FileNotFoundError:
        return ""


assert sh("xcrun", "swiftc", "-O", "-o", os.path.join(TMP, "wsctl"), os.path.join(HERE, "wsctl.swift")).returncode == 0
url = sh("amux", "url").stdout.strip() or "https://localhost:8824"


def displays():
    return [json.loads(l) for l in sh(os.path.join(TMP, "wsctl"), "displays").stdout.splitlines() if l.strip()]


# The test copy's windows float above everything, so on the owner's screen a
# click of theirs lands on them and moves them mid-case (2026-10-24 07:4x). A
# virtual display (vdisplay.m, as workspace-e2e.py uses) keeps them out of
# reach. PANE_E2E_MAIN_DISPLAY=1 runs on the main display instead.
vdisp = None
before = displays()
d0 = before[0]
if os.environ.get("PANE_E2E_MAIN_DISPLAY") != "1":
    assert sh("clang", "-fobjc-arc", "-o", os.path.join(TMP, "vdisplay"), os.path.join(HERE, "vdisplay.m"),
              "-framework", "Foundation", "-framework", "CoreGraphics").returncode == 0
    vdisp = subprocess.Popen([os.path.join(TMP, "vdisplay"), "amux-pane-e2e", "4343", "1920", "1080"], stdout=subprocess.DEVNULL)
    end = time.time() + 15
    new = []
    while not new and time.time() < end:
        time.sleep(0.5)
        new = [d for d in displays() if d["uuid"] not in {x["uuid"] for x in before}]
    assert new, "the virtual display did not connect"
    d0 = new[0]
    time.sleep(2)
vx, vy, vw, vh = rect(d0["visible"])
print(f"display: {d0.get('name')} {d0['visible']}")
src = (vx + vw * 0.08, vy + vh * 0.08, vw * 0.7, vh * 0.8)
src_s = "{{%d, %d}, {%d, %d}}" % src
TABS = ["board", "calendar", "map", "sessions", "messages"]


class App:
    def __init__(self, fresh=True, extra=()):
        if fresh:
            # extra: hashes of more tabs; the last one is the tab shown.
            with open(CFG, "w") as f:
                json.dump({"connections": [{"name": "localhost", "url": url}], "lastUrl": url, "cleanExit": True,
                           "windows": [{"url": f"{url}/#view={v}", "frame": src_s, "tabGroup": 1, "selected": v == "messages" and not extra}
                                       for v in TABS]
                           + [{"url": f"{url}/{h}", "frame": src_s, "tabGroup": 1, "selected": i == len(extra) - 1}
                              for i, h in enumerate(extra)]}, f)
        n = log().count("verdict=windows_restored")
        self.p = subprocess.Popen([BIN], env=dict(os.environ, AMUX_DESKTOP_CONFIG=CFG, AMUX_DESKTOP_TEST="1",
                                  AMUX_DESKTOP_TEST_STATE=STATE), stdout=open(LOG, "a"), stderr=subprocess.STDOUT)
        end = time.time() + 25
        while log().count("verdict=windows_restored") <= n and time.time() < end:
            time.sleep(0.3)
        # Pages load and post their names; drop-tab?title= finds a tab by that
        # name, so wait for every one (a fixed 3s lost the race under load).
        end = time.time() + 30
        while time.time() < end:
            time.sleep(0.5)
            try:
                names = [w["title"] for w in self.state()["windows"]]
            except (OSError, ValueError, KeyError):
                continue
            if names and "amux" not in names and "" not in names:
                break
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

    def stop(self):
        self.p.send_signal(signal.SIGTERM)
        try:
            self.p.wait(15)
        except subprocess.TimeoutExpired:
            self.p.kill()


SHOTS = os.environ.get("PANE_E2E_SHOTS") or os.path.join(TMP, "shots")


def shot(name, g):
    """The grid window as the window server draws it, for a human to judge."""
    if not g or "number" not in g:
        return
    os.makedirs(SHOTS, exist_ok=True)
    out = os.path.join(SHOTS, name + ".png")
    try:
        r = sh("/usr/sbin/screencapture", "-x", "-o", f"-l{g['number']}", out)
        print(f"  shot {out}" if r.returncode == 0 and os.path.exists(out) else f"  shot {name} not taken: {r.stderr.strip()}")
    except OSError as e:
        print(f"  shot {name} not taken: {e}")


def grid_window(st):
    g = [w for w in st["windows"] if "grid" in w]
    if not g:
        return None
    # Fractions to two places: a divider lands on a whole point, so an odd
    # height gives 0.501 for an even split.
    g[0]["grid"] = re.sub(r"([rc])(\d\.\d+)", lambda m: m.group(1) + "%.3f" % round(float(m.group(2)), 2), g[0]["grid"])
    return g[0]


def drag_tab(app, title, x, y):
    """Press in the tab bar, move to (x, y) with the button down, drop the tab
    called `title` there. Returns the state seen mid-drag."""
    app.send("activate", 0.3)
    app.send(f"pointer?x={src[0] + src[2] / 2}&y={src[1] + src[3] - 10}&down=1", 0.1)
    app.send("press-tab", 0.1)
    app.send(f"pointer?x={x}&y={y}&down=1", 0.5)
    mid = app.state()
    n = log().count("verdict=test_drop_tab_title_missing")
    app.send(f"drop-tab?title={title}", 1.5)
    app.send("pointer-clear", 0.2)
    if log().count("verdict=test_drop_tab_title_missing") > n:
        print(f"  note: no tab named {title} at the drop; the showing tab moved instead")
    return mid


app = App()
try:
    st = app.state()
    titles = sorted(w["title"] for w in st["windows"])
    check("starts as one window of five tabs", len(st["windows"]) == 5 and all(w["tabs"] == 5 for w in st["windows"]), str(st["windows"]))
    cx, cy, cw, ch = rect(st["content_frame"])

    print("1. the showing tab dragged into its own window's right half: two panes")
    mid = drag_tab(app, "Messages", cx + cw * 0.9, cy + ch * 0.5)
    want = (cx + cw / 2, cy, cw / 2, ch)
    check("the preview shows the right half of the content", mid["preview"] and near(rect(mid["preview_frame"]), want), f"{mid['preview_frame']} want {want}")
    st = app.state()
    g = grid_window(st)
    check("one window holds a grid of two", g is not None and g["grid"] == "r0.500(0,1)", str(st["windows"]))
    check("Messages is the right pane", g is not None and g["panes"][1] == "Messages", str(g))
    check("the right pane fills the right half", g is not None and near(rect(g["pane_frames"][1]), want, 6), str(g and g["pane_frames"]))
    check("the tab left the tab bar (4 tabs, no window for it)", g is not None and g["tabs"] == 4 and len(st["windows"]) == 4, str(st["windows"]))
    check("its own window is hidden, its page kept", st["hidden_panes"] == 1, str(st["hidden_panes"]))
    check("no preview after the drop", not st["preview"])
    check("the dock is logged", "verdict=tab_docked_as_pane panes=2 edge=right" in log())

    print("2. a tab dropped in the right pane's top right corner: that pane splits, new one on top")
    right = rect(g["pane_frames"][1])
    drag_tab(app, "Calendar", right[0] + right[2] * 0.92, right[1] + right[3] * 0.92)
    st = app.state()
    g = grid_window(st)
    check("three panes, the right column split", g is not None and g["grid"] == "r0.500(0,c0.500(1,2))", str(g))
    check("Calendar on top of Messages", g is not None and g["panes"][1:] == ["Calendar", "Messages"], str(g and g["panes"]))
    check("top right quarter", g is not None and near(rect(g["pane_frames"][1]), (cx + cw / 2, cy + ch / 2, cw / 2, ch / 2), 30), str(g and g["pane_frames"]))

    print("3. a tab dropped on the left pane's lower part: four panes")
    left = rect(g["pane_frames"][0])
    drag_tab(app, "Map", left[0] + left[2] * 0.5, left[1] + left[3] * 0.1)
    st = app.state()
    g = grid_window(st)
    check("four panes, two columns of two", g is not None and g["grid"] == "r0.500(c0.500(0,1),c0.500(2,3))", str(g))

    print("4. a full grid: a fifth tab dropped near a pane's edge joins that pane's tabs")
    before = g["grid"] if g else ""
    # The one tab still in the tab bar (AppKit showed the next tab after each tear, so which it is varies).
    rest = [w["title"] for w in st["windows"] if "grid" not in w and w["tabs"] > 1]
    fifth = rest[0] if rest else "Board"
    top_left = rect(g["pane_frames"][0]) if g else (0, 0, 0, 0)
    mid = drag_tab(app, fifth, top_left[0] + top_left[2] * 0.06, top_left[1] + top_left[3] * 0.5)
    st = app.state()
    g = grid_window(st)
    inset = (top_left[0] + min(24, top_left[2] * 0.06), top_left[1] + min(24, top_left[3] * 0.06),
             top_left[2] - 2 * min(24, top_left[2] * 0.06), top_left[3] - 2 * min(24, top_left[3] * 0.06))
    check("the preview is the whole pane (a join), not its half", mid["preview"] and near(rect(mid["preview_frame"]), inset, 6), f"{mid['preview_frame']} want {inset}")
    check("the grid is unchanged: still four panes", g is not None and g["grid"] == before, str(g))
    check("the fifth tab is the top left pane's second tab, shown", g is not None and g["pane_tabs"][0][-1] == fifth and len(g["pane_tabs"][0]) == 2 and g["pane_active"][0] == 1, str(g and g["pane_tabs"]))
    check("the join is logged", "verdict=tab_joined_pane panes=4 pane_tabs=2" in log())
    shot("4-four-panes-fifth-joined", g)

    print("5. a divider moved, then a quit and relaunch: the grid comes back as left")
    app.send("pane-divider?f=0.3", 1.5)
    st = app.state()
    g = grid_window(st)
    check("the split moved to 0.3", g is not None and g["grid"].startswith("r0.300("), str(g))
    panes_before = g["panes"] if g else []
    tabs_before = g["pane_tabs"] if g else []
    active_before = g["pane_active"] if g else []
    grid_before = g["grid"] if g else ""
    time.sleep(2)
finally:
    app.stop()

saved = json.load(open(CFG))
sg = [w for w in saved.get("windows", []) if w.get("grid")]
check("the grid is saved with the window", len(sg) == 1 and sg[0]["grid"]["tree"].startswith("r0.300(") and len(sg[0]["grid"]["panes"]) == 3, str(sg))
check("the top left pane's two tabs are saved", len(sg) == 1 and len(sg[0]["grid"].get("hostTabs") or []) + sum(len(p.get("tabs") or []) for p in sg[0]["grid"]["panes"]) == 1, str(sg))

app = App(fresh=False)
try:
    st = app.state()
    g = grid_window(st)
    print("    after relaunch:", [(w["title"], w["tabs"], w.get("grid")) for w in st["windows"]])
    check("relaunched with the same grid", g is not None and g["grid"] == grid_before, f"{g and g['grid']} want {grid_before}")
    check("the same pages in the same panes", g is not None and g["panes"] == panes_before, f"{g and g['panes']} want {panes_before}")
    check("the same tabs in each pane, the same one shown", g is not None and g["pane_tabs"] == tabs_before and g["pane_active"] == active_before, f"{g and g['pane_tabs']} want {tabs_before}")
    check("the panes' own windows stay hidden", st["hidden_panes"] == 4, str(st["hidden_panes"]))
    check("the restore is logged", "verdict=pane_grid_restored panes=4" in log())

    app.send("activate", 0.3)
    print("6. a pane's tab button: back to a tab")
    tabs0 = g["tabs"] if g else 0
    app.send("pane-tab?i=1", 1)
    st = app.state()
    g = grid_window(st)
    check("three panes left", g is not None and len(g["panes"]) == 3, str(g))
    check("one more tab in the window", any(w["tabs"] == tabs0 + 1 for w in st["windows"]), str(st["windows"]))

    print("7. a pane's header dragged out of every window: a tab again")
    x0, y0, w0, h0 = rect(st["content_frame"])
    app.send(f"pane-drag?i=1&x={vx + vw - 5}&y={vy + 5}", 1)
    st = app.state()
    g = grid_window(st)
    check("two panes left", g is not None and len(g["panes"]) == 2, str(g))

    print("8. a pane's header dragged onto the other pane's top: the grid rearranges")
    g0 = rect(g["pane_frames"][0]) if g else (0, 0, 0, 0)
    app.send(f"pane-drag?i=1&x={g0[0] + g0[2] * 0.5}&y={g0[1] + g0[3] * 0.92}", 1)
    st = app.state()
    g = grid_window(st)
    check("still two panes, now one above the other", g is not None and g["grid"].startswith("c0.500("), str(g))

    print("9. a pane closed: one pane left with its two tabs; its last extra tab closed: a plain window again")
    n_ctrl = len(st["windows"])
    # Close the pane with one tab; case 8 may have put the two-tab pane at either index.
    one = next((i for i, t in enumerate(g["pane_tabs"]) if len(t) == 1), 1) if g else 1
    app.send(f"pane-close?i={one}", 1)
    st = app.state()
    g = grid_window(st)
    check("one pane left, keeping its own two tabs", g is not None and g["grid"] == "0" and len(g["pane_tabs"][0]) == 2, str(g))
    app.send("pane-tab-close?i=0&t=1", 1)
    st = app.state()
    check("no grid left", grid_window(st) is None, str(st["windows"]))
    check("the closed pane's page is gone, not a tab", len(st["windows"]) == n_ctrl, str(st["windows"]))
    check("the close is logged", "verdict=pane_closed panes_left=1" in log())
finally:
    app.stop()

print("11. the press never reaches the app and the tear is seen after the drop: still a pane")
app = App()
try:
    st = app.state()
    cx, cy, cw, ch = rect(st["content_frame"])
    app.send("activate", 0.3)
    # A real tab drag: AppKit's drag session gives the app no events, so all it
    # sees is the button already up, the pointer where the tab was dropped, and
    # a new window there.
    x, y = cx + cw * 0.1, cy + ch * 0.5
    app.send(f"pointer?x={x}&y={y}&down=0", 0.1)
    app.send(f"tear-tab?x={x}&y={y}", 1.5)
    st = app.state()
    g = grid_window(st)
    # Which tab /tear-tab takes is the window in front, which varies when the
    # test copy cannot become the active app; the drop point decides the side.
    check("the dropped tab became the left pane", g is not None and g["grid"] == "r0.500(0,1)" and st["hidden_panes"] == 1, str(st["windows"]))
    check("it says it found the pane at the release", "verdict=tab_pane_drop_found_at_release zone=left" in log())
finally:
    app.stop()

print("13. system overlays over the window (as in a real drag): the pane is still found")
# Another process's windows above amux: one at the Dock's layer covering the
# display, one at the dragging level under the pointer (the tab's drag image).
assert sh("xcrun", "swiftc", "-O", "-o", os.path.join(TMP, "overlay"), os.path.join(HERE, "overlay.swift")).returncode == 0
overlays = []
app = App()
try:
    st = app.state()
    cx, cy, cw, ch = rect(st["content_frame"])
    x, y = cx + cw * 0.9, cy + ch * 0.5
    app.send("activate", 0.3)
    app.send(f"pointer?x={src[0] + src[2] / 2}&y={src[1] + src[3] - 10}&down=1", 0.1)
    app.send("press-tab", 0.1)
    for ox, oy, ow, oh, lvl in [(vx, vy, vw, vh, 20), (x - 40, y - 30, 80, 60, 500)]:
        o = subprocess.Popen([os.path.join(TMP, "overlay"), str(ox), str(oy), str(ow), str(oh), str(lvl)], stdout=subprocess.PIPE)
        o.stdout.readline()
        overlays.append(o)
    time.sleep(0.5)
    app.send(f"pointer?x={x}&y={y}&down=1", 0.5)
    mid = app.state()
    check("the preview shows the right half under the drag image", mid["preview"] and near(rect(mid["preview_frame"]), (cx + cw / 2, cy, cw / 2, ch)), mid["preview_frame"])
    app.send("drop-tab?title=Messages", 1.5)
    app.send("pointer-clear", 0.2)
    g = grid_window(app.state())
    check("the tab became the right pane", g is not None and g["grid"] == "r0.500(0,1)" and g["panes"][1] == "Messages", str(g))
finally:
    app.stop()
    for o in overlays:
        o.terminate()

for case, title, fx, want_first in [("14. AppKit cancels a drop on the page (no tear): the pressed hidden tab becomes the pane", "Calendar", 0.9, False),
                                    ("15. the showing tab, dropped on its own page with no tear: it becomes the pane, the next tab keeps the window", "Messages", 0.1, True)]:
    print(case)
    app = App()
    try:
        st = app.state()
        cx, cy, cw, ch = rect(st["content_frame"])
        app.send("activate", 0.3)
        app.send(f"pointer?x={src[0] + src[2] / 2}&y={src[1] + src[3] - 10}&down=1", 0.1)
        app.send(f"press-tab?title={title}", 0.1)
        app.send(f"pointer?x={cx + cw * fx}&y={cy + ch * 0.5}&down=1", 0.5)
        mid = app.state()
        check("the preview shows during the drag", mid["preview"], mid["preview_frame"])
        # The release, and no tear: AppKit put the tab back in the bar.
        app.send(f"pointer?x={cx + cw * fx}&y={cy + ch * 0.5}&down=0", 1.5)
        app.send("pointer-clear", 0.2)
        st = app.state()
        g = grid_window(st)
        side = 0 if want_first else 1
        check(f"{title} is the {'left' if want_first else 'right'} pane", g is not None and g["grid"] == "r0.500(0,1)" and g["panes"][side] == title, str(st["windows"]))
        check("the press named the tab from AppKit's tab buttons", f"test_press_tab title={title} buttons=5 tabs=5" in log(), "")
        check("it says the drop came from the bar", "verdict=tab_pane_drop_from_bar" in log())
    finally:
        app.stop()

print("12. a tab moved to a new window by the menu, pointer over a page: not docked")
app = App()
try:
    st = app.state()
    cx, cy, cw, ch = rect(st["content_frame"])
    app.send("activate", 0.3)
    app.send(f"pointer?x={cx + cw * 0.5}&y={cy + ch * 0.4}&down=0", 0.1)
    app.send("tear-tab", 1.5)
    st = app.state()
    check("no grid: the moved tab is its own window", grid_window(st) is None and any(w["tabs"] == 1 for w in st["windows"]), str(st["windows"]))
finally:
    app.stop()

print("10. the window's own page closed: the next pane takes the window")
app = App()
try:
    st = app.state()
    cx, cy, cw, ch = rect(st["content_frame"])
    drag_tab(app, "Messages", cx + cw * 0.9, cy + ch * 0.5)
    st = app.state()
    g = grid_window(st)
    frame0 = g["frame"] if g else ""
    host_page = g["panes"][0] if g else ""
    app.send("pane-close?i=0", 1)
    st = app.state()
    w = [w for w in st["windows"] if w["title"] == "Messages"]
    check("Messages now has the window, at the same place", len(w) == 1 and near(rect(w[0]["frame"]), rect(frame0)), f"{st['windows']} frame {frame0}")
    check(f"{host_page} is gone", not any(x["title"] == host_page for x in st["windows"]), str(st["windows"]))
    check("the takeover is logged", "verdict=pane_grid_rehosted" in log())
finally:
    app.stop()

print("16. each pane has its own tabs: join, switch, move between panes, close, new, zoom, focus")
app = App()
try:
    st = app.state()
    cx, cy, cw, ch = rect(st["content_frame"])
    drag_tab(app, "Messages", cx + cw * 0.9, cy + ch * 0.5)
    g = grid_window(app.state())
    right = rect(g["pane_frames"][1]) if g else (0, 0, 0, 0)
    mid = drag_tab(app, "Calendar", right[0] + right[2] * 0.5, right[1] + right[3] * 0.5)
    st = app.state()
    g = grid_window(st)
    check("a drop on a pane's middle joins its tabs: still two panes", g is not None and g["grid"] == "r0.500(0,1)", str(g))
    check("the right pane holds Messages then Calendar, Calendar shown", g is not None and g["pane_tabs"][1] == ["Messages", "Calendar"] and g["pane_active"][1] == 1, str(g and g["pane_tabs"]))
    check("the joined pane has focus", g is not None and g["focused"] == 1, str(g and g["focused"]))

    app.send("pane-tab-select?i=1&t=0", 0.8)
    g = grid_window(app.state())
    check("a click on a strip tab shows it", g is not None and g["pane_active"][1] == 0 and g["panes"][1] == "Messages", str(g))
    app.send("pane-action?a=next", 0.8)
    g = grid_window(app.state())
    check("Next Tab in Pane cycles within the pane", g is not None and g["pane_active"][1] == 1, str(g and g["pane_active"]))
    app.send("pane-action?a=tab&n=1", 0.8)
    g = grid_window(app.state())
    check("⌘1 is the focused pane's first tab", g is not None and g["pane_active"][1] == 0, str(g and g["pane_active"]))

    app.send("pane-action?a=left", 0.8)
    g = grid_window(app.state())
    check("⌘⌥← moves focus to the left pane", g is not None and g["focused"] == 0, str(g and g["focused"]))
    app.send("pane-action?a=right", 0.8)

    left = rect(g["pane_frames"][0]) if g else (0, 0, 0, 0)
    app.send(f"pane-tab-drag?i=1&t=1&x={left[0] + left[2] * 0.5}&y={left[1] + left[3] * 0.5}", 1.2)
    g = grid_window(app.state())
    check("a strip tab dragged to the other pane's middle joins it there", g is not None and g["pane_tabs"][1] == ["Messages"] and g["pane_tabs"][0][-1] == "Calendar", str(g and g["pane_tabs"]))

    app.send("pane-focus?i=0", 0.3)
    app.send("pane-action?a=new", 2.5)
    g = grid_window(app.state())
    shot("16-pane-with-three-tabs", g)
    check("New Tab in Pane adds a tab to the focused pane", g is not None and len(g["pane_tabs"][0]) == 3 and g["pane_active"][0] == 2, str(g and g["pane_tabs"]))

    app.send("pane-action?a=close", 1)
    g = grid_window(app.state())
    check("Close Tab in Pane closes only that tab", g is not None and len(g["pane_tabs"][0]) == 2 and g["grid"] == "r0.500(0,1)", str(g and g["pane_tabs"]))

    app.send("pane-tab-close?i=0&t=1", 1)
    g = grid_window(app.state())
    check("a strip tab's close button closes that tab", g is not None and len(g["pane_tabs"][0]) == 1, str(g and g["pane_tabs"]))

    app.send("pane-focus?i=1", 0.3)
    app.send("pane-action?a=zoom", 1)
    st = app.state()
    g = grid_window(st)
    zr = rect(g["pane_frames"][1]) if g else (0, 0, 0, 0)
    shot("16-zoomed", g)
    check("Zoom Pane shows the focused pane alone over the view", g is not None and g["zoomed"] == 1 and g["pane_shown"] == [False, True] and zr[2] > cw * 0.9, str(g))
    app.send("pane-action?a=zoom", 1)
    g = grid_window(app.state())
    check("zooming again puts the layout back", g is not None and g["zoomed"] == -1 and g["pane_shown"] == [True, True] and g["grid"] == "r0.500(0,1)", str(g))
finally:
    app.stop()

print("17. panes with their own tabs survive a workspace save and reopen, and a quit and relaunch")
app = App()
try:
    st = app.state()
    cx, cy, cw, ch = rect(st["content_frame"])
    drag_tab(app, "Messages", cx + cw * 0.9, cy + ch * 0.5)
    g = grid_window(app.state())
    right = rect(g["pane_frames"][1]) if g else (0, 0, 0, 0)
    drag_tab(app, "Calendar", right[0] + right[2] * 0.5, right[1] + right[3] * 0.5)
    g0 = grid_window(app.state())
    want = (g0["grid"], g0["pane_tabs"], g0["pane_active"]) if g0 else None
    check("set up: two panes, the right one with two tabs", g0 is not None and len(g0["pane_tabs"][1]) == 2, str(g0))
    n = log().count("verdict=layout_saved")
    sh(os.path.join(TMP, "wsctl"), "send", str(app.p.pid), "amux://save-workspace/pane-ws")
    end = time.time() + 5
    while log().count("verdict=layout_saved") <= n and time.time() < end:
        time.sleep(0.1)
    saved = json.load(open(CFG))
    ws = [l for l in saved.get("layouts", []) if l["name"] == "pane-ws"]
    check("the workspace records the grid", bool(ws) and any(w.get("grid") for w in ws[0]["windows"]), str(ws)[:400])
    # Change the layout: close the right pane.
    app.send("pane-close?i=1", 1)
    g1 = grid_window(app.state())
    check("changed: the grid is gone", g1 is None, str(g1))
    sh(os.path.join(TMP, "wsctl"), "send", str(app.p.pid), "amux://workspace/pane-ws")
    time.sleep(4)
    g2 = grid_window(app.state())
    got = (g2["grid"], g2["pane_tabs"], g2["pane_active"]) if g2 else None
    check("opening the workspace brings the panes and their tabs back", got == want, f"{got} vs {want}")
    app.stop()
    app = App(fresh=False)
    g3 = grid_window(app.state())
    got = (g3["grid"], g3["pane_tabs"], g3["pane_active"]) if g3 else None
    check("a quit and relaunch keeps the panes and their tabs", got == want, f"{got} vs {want}")
    pages = [w["title"] for w in app.state()["windows"]]
    check("no page is open twice", len(pages) == len(set(pages)), str(pages))
    # Opening the workspace again while its panes are showing (the Workspaces
    # menu, 2026-10-25 09:54:01 in Ethan's log): the same panes, no page twice.
    sh(os.path.join(TMP, "wsctl"), "send", str(app.p.pid), "amux://workspace/pane-ws")
    time.sleep(4)
    st = app.state()
    g4 = grid_window(st)
    got = (g4["grid"], g4["pane_tabs"], g4["pane_active"]) if g4 else None
    check("reopening the shown workspace keeps the panes and their tabs", got == want, f"{got} vs {want}")
    tabs = [t for w in st["windows"] for t in ([w["title"]] if "grid" not in w else [x for pt in w["pane_tabs"] for x in pt])]
    check("reopening opens no page twice", len(tabs) == len(set(tabs)), str(tabs))
    saved = json.load(open(CFG))
    dup = [w for w in saved.get("windows", []) if w.get("grid")]
    urls = [u for w in dup for u in [w["url"]] + [t["url"] for t in (w["grid"].get("hostTabs") or [])]
            + [x for p in w["grid"]["panes"] for x in [p["url"]] + [t["url"] for t in (p.get("tabs") or [])]]]
    check("the saved grid names no page twice", len(urls) == len(set(urls)), str(urls))
finally:
    app.stop()

print("18. a tab named by its owner keeps the name, across a relaunch, until cleared")
app = App()
try:
    app.send("rename-tab?view=view=board&name=Planning", 1)
    titles = [w["title"] for w in app.state()["windows"]]
    check("the tab shows its new name", "Planning" in titles and "Board" not in titles, str(titles))
    check("the rename is logged", "verdict=tab_renamed named=1" in log())
    time.sleep(2)   # the debounced save
    app.stop()
    app = App(fresh=False)
    titles = [w["title"] for w in app.state()["windows"]]
    check("a relaunch keeps the name", "Planning" in titles, str(titles))
    app.send("rename-tab?view=view=board&name=", 1)
    titles = [w["title"] for w in app.state()["windows"]]
    check("clearing it brings the page's own name back", "Board" in titles and "Planning" not in titles, str(titles))
finally:
    app.stop()

print("19. an open file, drawing or MDAI node stays open across a relaunch (Ethan, 2026-10-25)")
FILES = os.path.join(os.path.expanduser("~"), ".amux", "tmp", "pane-grid-e2e-files")
os.makedirs(FILES, exist_ok=True)
md = os.path.join(FILES, "notes-file.md")
with open(md, "w") as f:
    f.write("# A heading that is not the file name\n\nbody\n")   # the title shows the heading
dr = os.path.join(FILES, "sketch.excalidraw")
with open(dr, "w") as f:
    json.dump({"type": "excalidraw", "version": 2, "elements": [], "appState": {}}, f)
enc = lambda p: "#path=" + urllib.parse.quote(p, safe="")
app = App(extra=[enc(dr), enc(md)])
try:
    def views():
        return {w.get("view") for w in app.state()["windows"]}
    end = time.time() + 20
    while time.time() < end and enc(md) not in views():
        time.sleep(0.5)
    check("the shown tab reports the open file by its path", enc(md) in views(), str(views()))
    app.send("select-view?view=" + enc(dr)[1:], 1)   # a #path= tab still deferred is found by its URL
    end = time.time() + 20
    while time.time() < end and enc(dr) not in views():
        time.sleep(0.5)
    check("the drawing tab reports its drawing", enc(dr) in views(), str(views()))
    time.sleep(2)   # the debounced save
    app.stop()
    saved = [w["url"] for w in json.load(open(CFG))["windows"]]
    check("the saved tabs name both files", any(u.endswith(enc(md)) for u in saved) and any(u.endswith(enc(dr)) for u in saved), str(saved))
    app = App(fresh=False)
    end = time.time() + 60   # hidden tabs load one at a time after the shown one
    while time.time() < end and not {enc(md), enc(dr)} <= views():
        time.sleep(0.5)
    check("after a relaunch the file is open again", enc(md) in views(), str(views()))
    check("and the drawing", enc(dr) in views(), str(views()))
finally:
    app.stop()

if vdisp:
    vdisp.terminate()
    vdisp.wait(10)
print(f"screenshots: {SHOTS}")
print(f"PASS: pane-grid, all checks passed" if fails == 0 else f"pane-grid: {fails} check(s) FAILED (log {LOG})")
sys.exit(1 if fails else 0)
