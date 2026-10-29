#!/usr/bin/env python3
"""A split's own tab can always be closed (owner, 2026-10-29: "this tab is
stuck i can't move or delete it").

    desktop/test/pane-nest-collapse-e2e.py [path/to/Amux.app]

A second copy of the app with its own config opens a window split in two
(Board | Calendar). Calendar's tab is split (level 3), then the second pane of
that split is split again, so the level-3 split holds its host's page and one
page that is a split of its own. Closing the host's tab used to be refused
(verdict=pane_split_handover_refused, nothing on screen). Now the split
collapses: the other page takes the host's place in the window's pane, with
its own split, and the host's page closes.
"""
import json, os, re, signal, subprocess, sys, tempfile, time

HERE = os.path.dirname(os.path.abspath(__file__))
APP = sys.argv[1] if len(sys.argv) > 1 else os.path.join(HERE, "..", "..", "Amux.app")
BIN = os.path.join(APP, "Contents", "MacOS", "amux")
TMP = tempfile.mkdtemp(prefix="amux-collapse-e2e-")
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
url = sh("amux", "url").stdout.strip() or "https://localhost:8824"


def log():
    return open(LOG, errors="replace").read() if os.path.exists(LOG) else ""


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


json.dump({"connections": [{"name": "localhost", "url": url}], "lastUrl": url, "cleanExit": True, "launch": "last",
           "windows": [{"url": url + "/#view=board", "frame": "{{40, 40}, {2440, 1300}}",
                        "grid": {"tree": "r0.500(0,1)", "panes": [{"url": url + "/#view=calendar", "title": "Calendar"}]}}]}, open(CFG, "w"))
try:
    p = subprocess.Popen([BIN], env=dict(os.environ, AMUX_DESKTOP_CONFIG=CFG, AMUX_DESKTOP_TEST="1", AMUX_DESKTOP_TEST_STATE=ST),
                         stdout=open(LOG, "a"), stderr=subprocess.STDOUT)
    end = time.time() + 30
    while time.time() < end and "verdict=windows_restored" not in log():
        time.sleep(0.3)
    time.sleep(4)
    send("amux://test/pane-nest?at=1", 4)      # Calendar's tab: a split (level 3)
    send("amux://test/pane-nest?at=1.1", 4)    # that split's second pane: a split too
    w = win()
    n1 = (w.get("pane_nested") or [None, None])[1] or {}
    inner = (n1.get("nested") or [None, None])[1]
    check("the level-3 split holds its host's page and one split page", bool(inner) and len(n1.get("pane_tabs") or []) == 2,
          json.dumps(n1)[:400])
    pages = len(state()["pages"])
    refused = log().count("verdict=pane_split_handover_refused")
    send("amux://test/pane-tab-close?at=1&i=0&t=0", 3)   # close the level-3 split's own tab
    w = win()
    n1 = (w.get("pane_nested") or [None, None])[1] or {}
    check("closing the split's own tab is not refused", log().count("verdict=pane_split_handover_refused") == refused, log()[-400:])
    check("the split collapses into the page that was a split", "verdict=pane_split_collapsed_to_split" in log(), log()[-400:])
    check("the window's pane now shows that page's split", bool(n1) and re.match(r"[rc]0\.\d+\(0,1\)$", n1.get("grid", "")) and n1.get("depth") == 1,
          json.dumps(n1)[:400])
    check("one page closed", len(state()["pages"]) == pages - 1, f"pages {pages} -> {len(state()['pages'])}")
    check("the window is still split in two", w and w["grid"] == "r0.500(0,1)", json.dumps(w and w.get("grid")))
finally:
    if p and p.poll() is None:
        p.send_signal(signal.SIGTERM)
        try:
            p.wait(10)
        except subprocess.TimeoutExpired:
            p.kill()
print("PASS: pane-nest-collapse, all checks passed" if fails == 0 else f"pane-nest-collapse: {fails} check(s) FAILED (log {LOG})")
sys.exit(1 if fails else 0)
