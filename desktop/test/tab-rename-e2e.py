#!/usr/bin/env python3
"""Renaming a tab split into panes renames the whole tab (owner, 2026-10-29:
"in macos when i rename a tab it should rename the entire tab").

    desktop/test/tab-rename-e2e.py [path/to/Amux.app]

A second copy of the app with its own config opens a window tab split into
two panes beside a plain tab. Renaming the split tab from the window's tab bar
names the whole tab, and its panes keep their pages' names. The name survives
a relaunch and is in a saved workspace. Clearing it shows the panes' names
again. Renaming a page from its pane's own tab strip still names only that page.
"""
import json, os, signal, subprocess, sys, tempfile, time

HERE = os.path.dirname(os.path.abspath(__file__))
APP = sys.argv[1] if len(sys.argv) > 1 else os.path.join(HERE, "..", "..", "Amux.app")
BIN = os.path.join(APP, "Contents", "MacOS", "amux")
TMP = tempfile.mkdtemp(prefix="amux-rename-e2e-")
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


def start():
    global p
    n = open(LOG, errors="replace").read().count("verdict=windows_restored") if os.path.exists(LOG) else 0
    p = subprocess.Popen([BIN], env=dict(os.environ, AMUX_DESKTOP_CONFIG=CFG, AMUX_DESKTOP_TEST="1", AMUX_DESKTOP_TEST_STATE=ST),
                         stdout=open(LOG, "a"), stderr=subprocess.STDOUT)
    end = time.time() + 30
    while time.time() < end and open(LOG, errors="replace").read().count("verdict=windows_restored") <= n:
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


def split():
    return next(w for w in state()["windows"] if w.get("grid"))


frame = "{{80, 80}, {1100, 760}}"
json.dump({"connections": [{"name": "localhost", "url": url}], "lastUrl": url, "cleanExit": True, "launch": "last",
           "windows": [{"url": url + "/#view=board", "frame": frame, "tabGroup": 1, "selected": True,
                        "grid": {"tree": "r0.500(0,1)", "panes": [{"url": url + "/#view=calendar", "title": "Calendar"}]}},
                       {"url": url + "/#view=messages", "frame": frame, "tabGroup": 1, "selected": False}]}, open(CFG, "w"))
try:
    start()
    w = split()
    check("the split tab is named by its panes", w["title"] == "Board | Calendar", w["title"])
    send("amux://test/rename-tab?view=view%3Dboard&whole=1&name=Planning", 2)
    w = split()
    check("renaming it names the whole tab", w["title"] == "Planning", w["title"])
    check("its panes keep their pages' names", w["pane_tabs"] == [["Board"], ["Calendar"]], str(w["pane_tabs"]))
    check("the rename is logged", "verdict=tab_renamed whole=1 named=1" in open(LOG, errors="replace").read())
    send("amux://save-workspace/Named", 3)
    c = json.load(open(CFG))
    ws = next((l for l in c.get("layouts") or [] if l["name"] == "Named"), {})
    g = next((x["grid"] for x in ws.get("windows", []) if x.get("grid")), {})
    check("the saved workspace has the tab's name", g.get("name") == "Planning", json.dumps(g)[:300])
    stop()
    start()
    check("the name survives a relaunch", split()["title"] == "Planning", split()["title"])
    send("amux://test/rename-tab?view=view%3Dboard&whole=1&name=", 2)
    check("clearing it shows the panes' names again", split()["title"] == "Board | Calendar", split()["title"])
    check("and its tooltip lists the panes", split().get("tooltip") == "Board | Calendar", str(split().get("tooltip")))
    # A page renamed from its pane's own tab strip: only that page.
    send("amux://test/rename-tab?view=view%3Dcalendar&name=Dates", 2)
    w = split()
    check("a pane's page rename names only that page", w["title"] == "Board | Dates" and w["pane_tabs"] == [["Board"], ["Dates"]],
          f'{w["title"]} {w["pane_tabs"]}')
finally:
    if p and p.poll() is None:
        stop()
print("PASS: tab-rename, all checks passed" if fails == 0 else f"tab-rename: {fails} check(s) FAILED (log {LOG})")
sys.exit(1 if fails else 0)
