#!/usr/bin/env python3
"""The amux icon in the menu bar and its menu (owner, 2026-11-22: "make a menu
bar icon and put the most appropriate dropdown items", "Use the amux icon for
the menu bar").

    desktop/test/status-menu-e2e.py [path/to/Amux.app]

A quiet test copy makes the item but keeps it off the owner's menu bar, so the
menu is read through amux://test/state and its items are chosen through
amux://test/status-menu?title=, which performs the item's action as a click
does. Checks: the icon is the app icon, the menu has the expected items in
order, the needs-you section lists each waiting worker and its count shows
beside the icon, choosing a worker opens it, New Window opens a window, and the
Show in menu bar switch (Settings > This Mac) removes and restores the item.
"""
import json, os, signal, subprocess, sys, tempfile, time

HERE = os.path.dirname(os.path.abspath(__file__))
APP = sys.argv[1] if len(sys.argv) > 1 else os.path.join(HERE, "..", "..", "Amux.app")
BIN = os.path.join(APP, "Contents", "MacOS", "amux")
TMP = tempfile.mkdtemp(prefix="amux-status-menu-e2e-")
CFG, LOG, ST = (os.path.join(TMP, n) for n in ("config.json", "app.log", "state.json"))
WSCTL = os.path.join(TMP, "wsctl")
fails = 0


def sh(*a):
    return subprocess.run(a, capture_output=True, text=True)


def check(label, ok, detail=""):
    global fails
    print(("  ok   " if ok else "  FAIL ") + label + ("" if ok else f": {str(detail)[:500]}"))
    fails += 0 if ok else 1
    return ok


def log():
    try:
        return open(LOG, errors="replace").read()
    except FileNotFoundError:
        return ""


def send(pid, url):
    sh(WSCTL, "send", str(pid), url)


def state(pid):
    before = os.path.getmtime(ST) if os.path.exists(ST) else 0
    send(pid, "amux://test/state")
    end = time.time() + 5
    while time.time() < end and (not os.path.exists(ST) or os.path.getmtime(ST) == before):
        time.sleep(0.05)
    return json.load(open(ST))


def wait(pred, secs=10):
    end = time.time() + secs
    while time.time() < end:
        v = pred()
        if v:
            return v
        time.sleep(0.3)
    return pred()


assert sh("xcrun", "swiftc", "-O", "-o", WSCTL, os.path.join(HERE, "wsctl.swift")).returncode == 0
url = sh("amux", "url").stdout.strip() or "https://localhost:8824"
json.dump({"connections": [{"name": "localhost", "url": url}], "lastUrl": url, "cleanExit": True, "launch": "last",
           "windows": [{"url": url + "/#view=board", "frame": "{{80, 80}, {900, 640}}", "tabGroup": 1, "selected": True}]},
          open(CFG, "w"))
env = dict(os.environ, AMUX_DESKTOP_CONFIG=CFG, AMUX_DESKTOP_TEST="1", AMUX_DESKTOP_TEST_STATE=ST)
p = subprocess.Popen([BIN], env=env, stdout=open(LOG, "a"), stderr=subprocess.STDOUT)
try:
    wait(lambda: "verdict=windows_restored" in log(), 30)
    s = state(p.pid)
    m = s.get("status_menu") or {}
    print("    menu:", m.get("items"))
    check("the app makes its menu bar item", bool(m), s.keys())
    check("with the amux icon on it", m.get("has_icon") is True, m)
    check("kept off the owner's menu bar in a quiet test copy", m.get("visible") is False, m)
    check("it says so in its log", "verdict=status_menu_shown visible=0" in log())
    items = m.get("items", [])
    want = ["Open amux", "New Window", "New Tab", "Start Recording", "Recordings", "Computer Use", "Pins",
            "Workspaces", "Connected to localhost", "Connect to Server…", "Settings…", "Check for Updates…", "Quit amux"]
    check("the menu has every item", all(w in items for w in want), [w for w in want if w not in items])
    check("in order", [i for i in items if i in want] == want, items)
    check("with this Mac's CPU and memory", any(i.startswith("This Mac: CPU ") and "Memory" in i for i in items), items)
    check("and the computer-use state, measured", bool(m.get("computer") or m.get("computers")), m)

    # Two workers wait for the owner.
    send(p.pid, "amux://test/status-menu?needs=2")
    m = state(p.pid)["status_menu"]
    print("    with needs:", m.get("items")[:4], "title:", repr(m.get("title")))
    check("the count shows beside the icon", m.get("title", "").strip() == "2", m.get("title"))
    check("the menu opens with who needs you", m["items"][:3] == ["2 need you", "test-w1", "test-w2"], m["items"][:4])
    check("and no 'Nothing needs you' line", "Nothing needs you" not in m["items"], m["items"])

    n0 = len(state(p.pid)["windows"])
    send(p.pid, "amux://test/status-menu?title=test-w1")
    # The page then shows the worker list, since no worker test-w1 exists; what
    # the app asked for is in its log, and the tab it added is in its state.
    got = wait(lambda: "notification_opened where=new_tab why=status_menu hash=#peek=test-w1" in log())
    check("choosing a worker opens it in a new tab", got and len(state(p.pid)["windows"]) > n0,
          (n0, [w["view"] for w in state(p.pid)["windows"]]))
    check("and logs which", "verdict=status_menu_need_opened worker=test-w1" in log())

    n1 = sum(w["tabs"] for w in state(p.pid)["windows"] if w["selected"])
    send(p.pid, "amux://test/status-menu?title=New Window")
    more = wait(lambda: sum(w["tabs"] for w in state(p.pid)["windows"] if w["selected"]) > n1)
    check("New Window opens a window", bool(more), (n0, n1, len(state(p.pid)["windows"])))

    send(p.pid, "amux://test/status-menu?needs=0")
    m = state(p.pid)["status_menu"]
    check("with nobody waiting the count goes away", m.get("title", "") == "" and m["items"][0] == "Nothing needs you", m)

    # The Show in Menu Bar switch in Settings > This Mac.
    send(p.pid, "amux://test/settings-action?action=toggle&name=menuBar&value=0")
    off = wait(lambda: state(p.pid).get("status_menu") is None)
    check("Show in Menu Bar off removes it", off, state(p.pid).get("status_menu"))
    send(p.pid, "amux://test/settings-action?action=toggle&name=menuBar&value=1")
    on = wait(lambda: state(p.pid).get("status_menu") is not None)
    check("and on brings it back", on)
finally:
    p.send_signal(signal.SIGTERM)
    try:
        p.wait(15)
    except subprocess.TimeoutExpired:
        p.kill()

print(f"{'PASS' if fails == 0 else 'FAIL'}: {fails} failing check(s) (log {LOG})")
sys.exit(1 if fails else 0)
