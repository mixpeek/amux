#!/usr/bin/env python3
"""A pane drag knows which level of panes it lands in (owner, 2026-10-29: "i
want the snap to grid thing to be able to recognize and understand where im
dragging and what level in the panes /layers im dragigng to").

    desktop/test/pane-level-e2e.py [path/to/Amux.app]

A second copy of the app with its own config opens a window of three panes,
Board | Calendar | Messages, and Calendar's tab is split into a level of its
own. Holding Messages' tab:
  - in the inner pane's body, the outline is half of that inner pane and says
    "Level 3";
  - in the band right at an edge the inner pane shares with its parent, the
    outline is half of the whole parent pane and says "Level 2";
  - let go there, Messages becomes a pane of the window beside the split,
    which keeps both its panes; the drop logs its level.
"""
import json, os, re, signal, subprocess, sys, tempfile, time

HERE = os.path.dirname(os.path.abspath(__file__))
APP = sys.argv[1] if len(sys.argv) > 1 else os.path.join(HERE, "..", "..", "Amux.app")
BIN = os.path.join(APP, "Contents", "MacOS", "amux")
TMP = tempfile.mkdtemp(prefix="amux-level-e2e-")
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


def start():
    global p
    n = log().count("verdict=windows_restored")
    p = subprocess.Popen([BIN], env=dict(os.environ, AMUX_DESKTOP_CONFIG=CFG, AMUX_DESKTOP_TEST="1", AMUX_DESKTOP_TEST_STATE=ST),
                         stdout=open(LOG, "a"), stderr=subprocess.STDOUT)
    end = time.time() + 30
    while time.time() < end and log().count("verdict=windows_restored") <= n:
        time.sleep(0.3)
    settle()


def settle(secs=30):
    end = time.time() + secs
    while time.time() < end:
        try:
            w = win()
            if w and "amux" not in [t for tabs in w.get("pane_tabs", []) for t in tabs]:
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



def frame(s):
    return [float(x) for x in s.replace("{", "").replace("}", "").split(",")]


json.dump({"connections": [{"name": "localhost", "url": url}], "lastUrl": url, "cleanExit": True, "launch": "last",
           "windows": [{"url": url + "/#view=board", "frame": "{{60, 60}, {1500, 860}}",
                        "grid": {"tree": "r0.333(0,r0.500(1,2))", "panes": [{"url": url + "/#view=calendar", "title": "Calendar"},
                                                                             {"url": url + "/#view=messages", "title": "Messages"}]}}]}, open(CFG, "w"))
try:
    start()
    w = win()
    check("the window opens in three panes", w and len(w["pane_tabs"]) == 3, json.dumps(w and w.get("pane_tabs")))
    send("amux://test/pane-nest?at=1", 4)
    settle()
    w = win()
    n1 = (w.get("pane_nested") or [None, None, None])[1]
    check("Calendar's tab is a split of its own", bool(n1) and n1["grid"].startswith("r"), json.dumps(n1))
    x, y, fw, fh = frame(w["pane_frames"][1])
    mid = y + fh / 2

    # The inner pane's body, near its left edge but past the level bands.
    send(f"amux://test/pane-tab-hover?i=2&t=0&x={x + 40}&y={mid}", 1)
    st = state()
    pf = frame(st["preview_frame"])
    check("in the inner pane's body the outline says level 3", st["preview"] and st["preview_label"].strip().startswith("Level 3"), st["preview_label"])
    check("and covers half of that inner pane", abs(pf[2] - fw / 4) < 12, f"preview {pf}, parent pane width {fw}")

    # The band at the shared left edge: the parent pane.
    send(f"amux://test/pane-tab-hover?i=2&t=0&x={x + 4}&y={mid}", 1)
    st = state()
    pf = frame(st["preview_frame"])
    check("at the shared edge the outline says level 2 and names the split", st["preview"] and st["preview_label"].strip().startswith("Level 2")
          and "Calendar | Calendar" in st["preview_label"], st["preview_label"])
    check("and covers half of the whole parent pane", abs(pf[2] - fw / 2) < 12 and abs(pf[0] - x) < 6, f"preview {pf}, parent {x} {fw}")
    if os.environ.get("LEVEL_SHOT"):
        # The test window's rectangle only (with the outline over it), not the rest of the screen.
        wx, wy, ww, wh = frame(win()["frame"])
        top = frame(st["screen_frame"])[3]
        sh("/usr/sbin/screencapture", "-x", "-R", f"{int(wx)},{int(top - wy - wh)},{int(ww)},{int(wh)}", os.environ["LEVEL_SHOT"])

    # Let go there.
    before = log().count("verdict=pane_drop_level level=2")
    send(f"amux://test/pane-tab-drag?i=2&t=0&x={x + 4}&y={mid}", 3)
    settle()
    w = win()
    check("let go, Messages is a pane of the window beside the split", w["pane_tabs"] == [["Board"], ["Messages"], ["Calendar | Calendar"]], json.dumps(w["pane_tabs"]))
    nested = w.get("pane_nested") or []
    check("and the split keeps both its panes", len(nested) == 3 and bool(nested[2]) and nested[2]["pane_tabs"] == [["Calendar"], ["Calendar"]], json.dumps(nested)[:300])
    check("the drop logged its level", log().count("verdict=pane_drop_level level=2") == before + 1, log()[-500:])
finally:
    if p and p.poll() is None:
        stop()
print("PASS: pane-level, all checks passed" if fails == 0 else f"pane-level: {fails} check(s) FAILED (log {LOG})")
sys.exit(1 if fails else 0)
