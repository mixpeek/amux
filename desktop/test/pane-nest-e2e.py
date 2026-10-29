#!/usr/bin/env python3
"""Splits inside tabs, six levels of tabs (owner, 2026-10-29: "i want to have
more nested tab capabilities, so i have top level 2nd level i want 3rd and 4th
so i can create panes within panes etc.").

    desktop/test/pane-nest-e2e.py [path/to/Amux.app]

A second copy of the app with its own config opens a window split into two
panes (Board | Calendar). Then, as the pane header's "Split this tab" does:
  - Calendar's tab becomes a split of its own (level 3), then a pane inside
    that is split again, down to level 6; a seventh level is refused and
    logged; each split opens its new pane to the right, and with Settings'
    "Along the pane's longer side" a tall pane splits top and bottom;
  - the window's own first pane (the host's page) is split too;
  - the window and the parent strip are named by the panes inside;
  - it saves into a workspace with every level, and comes back after a relaunch;
  - closing a split tab closes the pages inside it.
"""
import json, os, re, signal, subprocess, sys, tempfile, time

HERE = os.path.dirname(os.path.abspath(__file__))
APP = sys.argv[1] if len(sys.argv) > 1 else os.path.join(HERE, "..", "..", "Amux.app")
BIN = os.path.join(APP, "Contents", "MacOS", "amux")
TMP = tempfile.mkdtemp(prefix="amux-nest-e2e-")
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


json.dump({"connections": [{"name": "localhost", "url": url}], "lastUrl": url, "cleanExit": True, "launch": "last",
           "windows": [{"url": url + "/#view=board", "frame": "{{40, 40}, {2440, 1300}}",
                        "grid": {"tree": "r0.500(0,1)", "panes": [{"url": url + "/#view=calendar", "title": "Calendar"}]}}]}, open(CFG, "w"))
try:
    start()
    w = win()
    check("the window opens split in two", w and w["pane_tabs"] == [["Board"], ["Calendar"]], json.dumps(w and w.get("pane_tabs")))

    # Level 3: Calendar's tab becomes a split of its own.
    send("amux://test/pane-nest?at=1", 4)
    settle()
    w = win()
    n1 = (w.get("pane_nested") or [None, None])[1]
    check("a pane's tab splits into panes of its own (level 3)",
          bool(n1) and re.match(r"[rc]0\.50\d\(0,1\)$", n1["grid"]) and n1["pane_tabs"] == [["Calendar"], ["Calendar"]] and n1["depth"] == 1, json.dumps(n1))
    check("each of its panes is drawn", bool(n1) and all(n1["pane_shown"]), json.dumps(n1))
    check("the window's own panes are still two", w["grid"] == "r0.500(0,1)", w["grid"])
    check("the parent strip names the split tab by its panes", w["pane_tabs"][1] == ["Calendar | Calendar"], json.dumps(w["pane_tabs"]))
    check("the window is named by every pane", w["title"] == "Board | Calendar | Calendar", w["title"])

    # Level 4: a pane inside that split is split again.
    send("amux://test/pane-nest?at=1.1", 4)
    settle()
    w = win()
    n1 = w["pane_nested"][1]
    n11 = (n1.get("nested") or [None, None])[1]
    check("a pane inside a split tab splits again (level 4)",
          bool(n11) and re.match(r"[rc]0\.50\d\(0,1\)$", n11["grid"]) and n11["depth"] == 2 and all(n11["pane_shown"]), json.dumps(n11))
    # Levels 5 and 6.
    send("amux://test/pane-nest?at=1.1.1", 4)
    send("amux://test/pane-nest?at=1.1.1.1", 4)
    settle()
    w = win()
    def at(w, path):
        n = {"nested": w["pane_nested"]}
        for i in path:
            n = (n.get("nested") or [])[i] if n else None
            if n is None:
                return None
        return n
    n5, n6 = at(w, [1, 1, 1]), at(w, [1, 1, 1, 1])
    check("level 5 and level 6 split too", bool(n5) and bool(n6) and n5["depth"] == 3 and n6["depth"] == 4 and all(n6["pane_shown"]),
          json.dumps(w["pane_nested"])[:500])
    # Owner, 2026-10-29: "split button still opens pane below when it should
    # be to the right".
    grids = [n and n["grid"] for n in (n1, n11, n5, n6)]
    check("each split opens its new pane to the right while the pane has room",
          all(grids) and grids[0][0] == "r" and grids[1][0] == "r", str(grids))
    check("a pane too narrow for two side by side splits below, and says so",
          all(grids) and any(g[0] == "c" for g in grids[2:]) and "verdict=split_edge_no_room want=right" in log(), str(grids))
    check("no split is saved past its ends", all(grids) and all(0 < float(re.match(r"[rc]([0-9.]+)", g).group(1)) < 1 for g in grids), str(grids))
    check("level 6 offers no deeper split", bool(n6) and n6["can_nest"] == [False, False], json.dumps(n6))
    before = log().count("verdict=pane_nest_refused")
    send("amux://test/pane-nest?at=1.1.1.1.0", 2)
    check("a seventh level is refused and logged", log().count("verdict=pane_nest_refused") == before + 1 and (at(win(), [1, 1, 1, 1]) or {}).get("nested") == [None, None],
          log()[-400:])
    if os.environ.get("NEST_SHOT"):
        wid = win()["number"]
        sh("/usr/sbin/screencapture", "-x", "-o", "-l", str(wid), os.environ["NEST_SHOT"])

    # The window's own first pane (its host's page) splits too.
    send("amux://test/pane-nest?at=0", 4)
    settle()
    w = win()
    n0 = w["pane_nested"][0]
    check("the window's own first pane splits too", bool(n0) and re.match(r"[rc]0\.50\d\(0,1\)$", n0["grid"]) and n0["pane_tabs"] == [["Board"], ["Board"]],
          json.dumps(w.get("pane_nested"))[:400])
    n1 = (w.get("pane_nested") or [None, None])[1] or {}
    check("and nothing else moved", (n1.get("nested") or [None, None])[1] is not None and w["grid"] == "r0.500(0,1)", json.dumps(w)[:400])
    check("its pane keeps a copy of the page as a tab, to hold the window", w["pane_tabs"][0] == ["Board | Board", "Board"], json.dumps(w["pane_tabs"]))

    # Saved with every level, and back after a relaunch.
    send("amux://save-workspace/Nested", 3)
    ws = next((l for l in json.load(open(CFG)).get("layouts") or [] if l["name"] == "Nested"), {})
    def depth_of(g, d=1):
        subs = [x.get("split") for x in (g.get("panes") or [])] + [x.get("split") for x in (g.get("hostTabs") or [])]
        for p_ in g.get("panes") or []:
            subs += [t.get("split") for t in p_.get("tabs") or []]
        return max([d] + [depth_of(s, d + 1) for s in subs if s])
    g0 = next((x["grid"] for x in ws.get("windows", []) if x.get("grid")), {})
    check("the workspace saves every level", depth_of(g0) == 5, json.dumps(g0)[:600])
    shape = lambda w: (w["grid"], w["pane_tabs"], json.dumps(w["pane_nested"], sort_keys=True))
    want = shape(win())
    stop()
    start()
    time.sleep(3)
    got = shape(win())
    check("a relaunch brings every level back", got == want, f"want={want} got={got}")

    # Settings' "Along the pane's longer side": a tall pane splits top and bottom.
    stop()
    cfg = json.load(open(CFG))
    cfg["splitDirection"] = "longer"
    json.dump(cfg, open(CFG, "w"))
    start()
    time.sleep(3)
    send("amux://test/pane-nest?at=0.1", 4)
    settle()
    n01 = (((win().get("pane_nested") or [None])[0] or {}).get("nested") or [None, None])[1]
    check("with the longer-side setting a tall pane splits top and bottom", bool(n01) and n01["grid"].startswith("c"), json.dumps(n01)[:300])

    # Closing a split tab closes the pages inside it.
    pages = len(state()["pages"])
    send("amux://test/pane-tab-close?i=1&t=0", 3)
    w = win()
    after = len(state()["pages"])
    check("closing a split tab closes its pages", (w is None or len(w["pane_tabs"]) == 1) and after == pages - 5, f"pages {pages} -> {after}, {json.dumps(w and w.get('pane_tabs'))}")
    shot = os.path.join(TMP, "nested.png")
finally:
    if p and p.poll() is None:
        stop()
print("PASS: pane-nest, all checks passed" if fails == 0 else f"pane-nest: {fails} check(s) FAILED (log {LOG})")
sys.exit(1 if fails else 0)
