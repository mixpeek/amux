#!/usr/bin/env python3
"""Workspaces > Freeze Workspace locks the tabs, names and workers and leaves
sizes free (owner, 2026-11-03: "freeze a workspace, so i can't change the tabs,
names of tabs, workers etc but i can make them bigger/smaller").

    desktop/test/freeze-e2e.py [path/to/Amux.app]

A second copy of the app with its own config opens a window tab split into
Board | Calendar beside a Messages tab. Frozen, every way of adding, closing,
renaming, moving, docking or splitting a tab is refused and logged, the pane
headers keep only Zoom, the window says Frozen and its pages read
window.__amuxFrozen. A divider, pane zoom, page zoom and picking a tab in the
tab bar still work. The freeze survives a relaunch; unfrozen, renaming and new
tabs work again. The page side (a frozen page keeps its worker) is
e2e/desktop-freeze-worker.spec.ts.
"""
import json, os, signal, subprocess, sys, tempfile, time

HERE = os.path.dirname(os.path.abspath(__file__))
APP = sys.argv[1] if len(sys.argv) > 1 else os.path.join(HERE, "..", "..", "Amux.app")
BIN = os.path.join(APP, "Contents", "MacOS", "amux")
TMP = tempfile.mkdtemp(prefix="amux-freeze-e2e-")
CFG, LOG, ST = (os.path.join(TMP, n) for n in ("config.json", "app.log", "state.json"))
fails = 0


def sh(*a):
    return subprocess.run(a, capture_output=True, text=True)


def check(label, ok, detail=""):
    global fails
    print(("  ok   " if ok else "  FAIL ") + label + ("" if ok else f": {detail}"))
    fails += 0 if ok else 1


assert sh("xcrun", "swiftc", "-O", "-o", os.path.join(TMP, "wsctl"), os.path.join(HERE, "wsctl.swift")).returncode == 0
url = sh("amux", "url").stdout.strip() or "https://localhost:8824"
p = None


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
    end = time.time() + 30
    while time.time() < end:
        try:
            ws = state()["windows"]
            if len(ws) >= 2 and all(w["title"] not in ("", "amux") and "amux |" not in w["title"] for w in ws):
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


def send(u, settle=1.0):
    sh(os.path.join(TMP, "wsctl"), "send", str(p.pid), u)
    time.sleep(settle)


def state():
    before = os.path.getmtime(ST) if os.path.exists(ST) else 0
    sh(os.path.join(TMP, "wsctl"), "send", str(p.pid), "amux://test/state")
    end = time.time() + 5
    while time.time() < end and (not os.path.exists(ST) or os.path.getmtime(ST) == before):
        time.sleep(0.05)
    return json.load(open(ST))


def js(code, i=0):
    out = ST + ".js"
    if os.path.exists(out):
        os.remove(out)
    send("amux://test/js-pane?i=%d&code=%s" % (i, __import__("urllib.parse").parse.quote(code)), 0.2)
    end = time.time() + 5
    while time.time() < end and not os.path.exists(out):
        time.sleep(0.05)
    return open(out).read() if os.path.exists(out) else "no answer"


def split():
    return next(w for w in state()["windows"] if w.get("grid"))


def shape():
    """Everything a freeze must keep: windows, tabs, titles and pane tabs."""
    st = state()
    return sorted((w["title"], w["tabs"], json.dumps(w.get("pane_tabs"))) for w in st["windows"]), st["hidden_panes"]


def refused(what):
    return log().count(f"verdict=frozen_refused what={what}")


frame = "{{80, 80}, {1100, 760}}"
json.dump({"connections": [{"name": "localhost", "url": url}], "lastUrl": url, "cleanExit": True, "launch": "last",
           "windows": [{"url": url + "/#view=board", "frame": frame, "tabGroup": 1, "selected": True,
                        "grid": {"tree": "r0.500(0,1)", "panes": [{"url": url + "/#view=calendar", "title": "Calendar"}]}},
                       {"url": url + "/#view=messages", "frame": frame, "tabGroup": 1, "selected": False}]}, open(CFG, "w"))
try:
    start()
    send("amux://test/activate")
    w = split()
    check("unfrozen, the pane headers have all their buttons", all(len(b) >= 4 for b in w["pane_buttons"]), str(w["pane_buttons"]))
    send("amux://test/freeze?on=1")
    st = state()
    check("Freeze Workspace freezes it", st["frozen"] is True, str(st.get("frozen")))
    check("and is logged", "verdict=workspace_frozen workspace=unsaved" in log())
    w = split()
    check("the window says Frozen", w["subtitle"] == "Frozen", repr(w["subtitle"]))
    check("pane headers keep only Zoom", w["pane_buttons"] == [["Zoom"], ["Zoom"]], str(w["pane_buttons"]))
    check("its pages are told", js("return String(window.__amuxFrozen)", 0) == "true" and js("return String(window.__amuxFrozen)", 1) == "true")

    before = shape()
    for hook, what in [("rename-tab?view=view%3Dboard&whole=1&name=Nope", "rename"),
                       ("rename-tab?view=view%3Dcalendar&name=Nope", "rename"),
                       ("new-tab", "new_tab"), ("new-window", "new_window"),
                       ("pane-action?i=0&a=new", "new_tab"), ("pane-action?i=0&a=close", "pane_tab_moved"),
                       ("pane-tab-close?i=1&t=0", "pane_tab_moved"), ("pane-tab?i=1", "pane_tab_moved"),
                       ("pane-close?i=1", "pane_tab_moved"), ("pane-nest?at=1", "split"), ("split", "split"),
                       ("close-tab", "close_tab"), ("menu-move-tab", "tab_moved")]:
        n = refused(what)
        send("amux://test/" + hook, 1.5)
        check(f"{hook.split('?')[0]} is refused", refused(what) > n and shape() == before,
              f"refusals {n}->{refused(what)} shape {shape()} vs {before}")

    # Still free: a divider, pane zoom, page zoom, picking a tab.
    f0 = split()["pane_frames"]
    send("amux://test/pane-divider?f=0.3", 1.5)
    check("a divider still moves", split()["pane_frames"] != f0, str(split()["pane_frames"]))
    send("amux://test/pane-action?i=0&a=zoom", 1)
    z = split()["zoomed"]
    send("amux://test/pane-action?i=0&a=zoom", 1)
    check("a pane still zooms and back", z == 0 and split()["zoomed"] == -1, f"{z} {split()['zoomed']}")
    send("amux://test/zoom-levels", 1)
    z0 = json.load(open(ST + ".zoom"))["levels"]
    send("amux://test/zoom?op=in", 1)
    send("amux://test/zoom-levels", 1)
    z1 = json.load(open(ST + ".zoom"))["levels"]
    send("amux://test/zoom?op=reset", 1)
    check("page zoom still works", z1 != z0, f"{z0} -> {z1}")
    send("amux://test/press-tab?title=Messages", 1.5)
    sel = next((x for x in state()["windows"] if x["view"] == "#view=messages"), {})
    check("a press on a tab still picks it", sel.get("selected") is True and "verdict=test_press_tab_frozen" in log(), str(sel))
    check("and starts no drag", state()["tracking"] is False)
    check("nothing changed the workspace", shape() == before, f"{shape()} vs {before}")

    stop()
    start()
    st = state()
    check("the freeze survives a relaunch", st["frozen"] is True and split()["subtitle"] == "Frozen", str(st.get("frozen")))
    send("amux://test/freeze?on=0")
    check("unfreezing shows every button again", all(len(b) >= 4 for b in split()["pane_buttons"]) and split()["subtitle"] == "",
          str(split()["pane_buttons"]))
    check("and tells the pages", js("return String(window.__amuxFrozen)", 0) == "false")
    send("amux://test/rename-tab?view=view%3Dboard&whole=1&name=Planning", 2)
    check("unfrozen, renaming works again", split()["title"] == "Planning", split()["title"])
    tabs = split()["tabs"]
    send("amux://test/new-tab", 3)
    check("and a new tab opens", split()["tabs"] == tabs + 1, f"{tabs} -> {split()['tabs']}")
finally:
    if p and p.poll() is None:
        stop()
print("PASS: freeze, all checks passed" if fails == 0 else f"freeze: {fails} check(s) FAILED (log {LOG})")
sys.exit(1 if fails else 0)
