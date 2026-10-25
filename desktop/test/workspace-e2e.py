#!/usr/bin/env python3
"""End-to-end check of the Mac app's workspace restore, on this Mac's real
window server, Desktops and (virtual) displays.

    desktop/test/workspace-e2e.py [path/to/Amux.app]

Runs a second copy of the app with its own config (AMUX_DESKTOP_CONFIG), so
the copy you use is not touched. It needs a running amux server (it reads
`amux url`) and at least three Desktops on the main display. Windows appear
on your Desktops while it runs, and a virtual monitor is plugged in and
unplugged; nothing switches the Desktop you are on.

Scenarios, each checked against what the window server reports rather than
what the app says it did:
  restore    15 windows on three Desktops, a tab group, a minimized window
  autosave   Window > Arrange > Tile, saved within seconds without quitting
  crash      kill -9, relaunch: the last autosave comes back
  quit       SIGTERM (logout, reboot) saves and relaunch restores
  workspace  save, change, reopen: same windows reused, frames back
  display    a monitor unplugged and plugged back in: the window goes home,
             and a resolution change scales it
"""
import json, os, signal, subprocess, sys, tempfile, time

HERE = os.path.dirname(os.path.abspath(__file__))
APP = sys.argv[1] if len(sys.argv) > 1 else os.path.join(HERE, "..", "..", "Amux.app")
BIN = os.path.join(APP, "Contents", "MacOS", "amux")
TMP = tempfile.mkdtemp(prefix="amux-ws-e2e-")
CFG = os.path.join(TMP, "config.json")
LOG = os.path.join(TMP, "app.log")
results = []


def sh(*a, **k):
    return subprocess.run(a, capture_output=True, text=True, **k)


def build_tools():
    r = sh("xcrun", "swiftc", "-O", "-o", os.path.join(TMP, "wsctl"), os.path.join(HERE, "wsctl.swift"))
    assert r.returncode == 0, r.stderr
    r = sh("clang", "-fobjc-arc", "-o", os.path.join(TMP, "vdisplay"), os.path.join(HERE, "vdisplay.m"),
           "-framework", "Foundation", "-framework", "CoreGraphics")
    assert r.returncode == 0, r.stderr


def ctl(*a):
    out = sh(os.path.join(TMP, "wsctl"), *map(str, a)).stdout
    return [json.loads(l) for l in out.splitlines() if l.strip()]


def check(name, ok, detail=""):
    results.append((name, bool(ok), detail))
    print(("PASS " if ok else "FAIL ") + name + (f"  ({detail})" if detail else ""), flush=True)
    return ok


def wait(cond, timeout=15, step=0.5):
    end = time.time() + timeout
    while time.time() < end:
        v = cond()
        if v:
            return v
        time.sleep(step)
    return cond()


def cfg():
    with open(CFG) as f:
        return json.load(f)


class App:
    def __init__(self):
        self.p = None

    def start(self):
        env = dict(os.environ, AMUX_DESKTOP_CONFIG=CFG)
        n = self.restores()
        self.p = subprocess.Popen([BIN], env=env, stdout=open(LOG, "a"), stderr=subprocess.STDOUT)
        wait(lambda: self.restores() > n, 20)
        time.sleep(3)

    @staticmethod
    def restores():
        try:
            with open(LOG, errors="replace") as f:
                return f.read().count("verdict=windows_restored")
        except FileNotFoundError:
            return 0

    def windows(self):
        return ctl("windows", self.p.pid)

    def send(self, url):
        ctl("send", self.p.pid, url)

    def stop(self, sig=signal.SIGTERM):
        if self.p and self.p.poll() is None:
            self.p.send_signal(sig)
            try:
                self.p.wait(10)
            except subprocess.TimeoutExpired:
                self.p.kill()
                self.p.wait()


def frame_of(s):
    a = s["frame"].replace("{", "").replace("}", "").split(",")
    return tuple(int(float(v)) for v in a)


def same(live, saved, tol=3):
    """live is the window server's bounds, saved an NSWindow frame. On macOS 26
    the bounds sit 9 to 13 pt inside the frame on each side, so compare centers,
    and sizes within that inset."""
    lx, ly, lw, lh = live
    sx, sy, sw, sh = saved
    return (abs((lx + lw / 2) - (sx + sw / 2)) <= tol and abs((ly + lh / 2) - (sy + sh / 2)) <= tol
            and 0 <= sw - lw <= 32 and 0 <= sh - lh <= 32)


def live_frames(app):
    return [((w["x"], w["y"], w["w"], w["h"]), w) for w in app.windows()]


def matches_saved(app, saved, check_desktop=True):
    """Every saved visible window has a live window at its frame on its Desktop."""
    live = live_frames(app)
    misses = []
    for s in saved:
        if s.get("selected") is False:
            continue
        f = frame_of(s)
        hit = [w for lf, w in live if same(lf, f)]
        if not hit:
            misses.append(f"no window at {f}")
        elif check_desktop and s.get("spaceIndex") and not s.get("minimized") \
                and not any(w["desktop"] == s["spaceIndex"] for w in hit):
            misses.append(f"{f} on Desktop {[w['desktop'] for w in hit]}, saved {s['spaceIndex']}")
    return misses


def main():
    if not os.path.exists(BIN):
        sys.exit(f"no app at {APP}; build it with AMUX_DESKTOP_NO_INSTALL=1 desktop/build-desktop.sh")
    build_tools()
    url = sh("amux", "url").stdout.strip()
    displays = ctl("displays")
    main_disp = displays[0]
    spaces = [s for s in ctl("spaces") if s["display"] in (main_disp["uuid"], "Main")]
    cur = next(s for s in spaces if s["current"])
    others = [s for s in spaces if not s["current"]][:2]
    if len(others) < 2:
        sys.exit("needs at least three Desktops on the main display (Mission Control > +)")
    vis = [int(float(v)) for v in main_disp["visible"].replace("{", "").replace("}", "").split(",")]
    vx, vy, vw, vh = vis
    views = ["#view=board", "#peek=amux&tab=terminal", "#view=sessions", "#view=files", "#view=tasks"]
    wins = []
    for d, sp in enumerate([cur] + others):
        for i, h in enumerate(views):
            w = dict(url=f"{url}/{h}", display=main_disp["uuid"], space=sp["uuid"], spaceIndex=sp["number"],
                     frame="{{%d, %d}, {%d, %d}}" % (vx + 40 + i * 140 + d * 13, vy + 40 + i * 50 + d * 7,
                                                    min(900, vw - 800), min(640, vh - 300)))
            if d == 1 and i < 3:
                # Tabs share one window frame: the group's.
                w["tabGroup"] = 1
                w["selected"] = i == 1
                if wins and wins[-1].get("tabGroup") == 1:
                    w["frame"] = wins[-1]["frame"]
            if d == 2 and i == 4:
                w["minimized"] = True
            wins.append(w)
    with open(CFG, "w") as f:
        json.dump(dict(connections=[dict(name="localhost", url=url)], lastUrl=url, windows=wins,
                       menuBar=False, notifications=False, dockBadge=False, openAtLogin=False), f)
    # A display is only removed while the main display is awake: unplugging
    # a virtual one under a sleeping monitor leaves it connected, with no
    # process behind it, until the monitor wakes. Keep it awake for the run.
    awake = subprocess.Popen(["caffeinate", "-d", "-u"])
    app = App()
    vd = None
    try:
        # ── restore ──
        app.start()
        live = app.windows()
        check("restore: 15 windows exist", len(live) == 15, f"{len(live)} windows")
        m = matches_saved(app, wins)
        check("restore: every window at its frame, on its Desktop", not m, "; ".join(m))
        per = {}
        for w in live:
            per[w["desktop"]] = per.get(w["desktop"], 0) + 1
        tab_frames = {(w["x"], w["y"]) for w in live if w["desktop"] == others[0]["number"]}
        check("restore: the tab group is one window on its Desktop", len(tab_frames) == 3, f"{len(tab_frames)} places; {per}")
        check("restore: no window grew (frame is the whole window)",
              all(w["h"] <= min(640, vh - 300) for w in live), str(sorted({w['h'] for w in live})))

        # ── autosave ──
        before = {frame_of(w) for w in cfg()["windows"] if w.get("spaceIndex") == cur["number"]}
        app.send("amux://arrange/tile")
        def tiled():
            now = [w for w in cfg()["windows"] if w.get("spaceIndex") == cur["number"]]
            return now if len(now) == 5 and not ({frame_of(w) for w in now} & before) else None
        here = wait(tiled, 15)
        m = matches_saved(app, cfg()["windows"])
        check("autosave: tiled frames saved without quitting", here and not m,
              f"{len(here or [])} retiled on Desktop {cur['number']}; " + "; ".join(m))

        # ── crash ──
        saved = cfg()["windows"]
        app.stop(signal.SIGKILL)
        app.start()
        check("crash: unclean exit noticed", "verdict=restore_after_unclean_exit" in open(LOG, errors="replace").read())
        m = matches_saved(app, saved)
        check("crash: last autosave restored", not m, "; ".join(m))

        # ── quit ──
        app.stop(signal.SIGTERM)
        c = cfg()
        check("quit: SIGTERM saved and marked a clean exit", c.get("cleanExit") is True and len(c["windows"]) == 15)
        app.start()
        m = matches_saved(app, c["windows"])
        check("quit: relaunch restored", not m, "; ".join(m))

        # ── workspace ──
        app.send("amux://save-workspace/E2E")
        wait(lambda: any(l["name"] == "E2E" for l in cfg().get("layouts") or []), 10)
        ws = next(l for l in cfg()["layouts"] if l["name"] == "E2E")
        numbers = sorted(w["n"] for w in app.windows())
        app.send("amux://arrange/fill")
        time.sleep(5)
        moved = matches_saved(app, ws["windows"])
        app.send("amux://workspace/E2E")
        time.sleep(4)
        m = matches_saved(app, ws["windows"])
        check("workspace: a change after saving is visible", bool(moved), f"{len(moved)} differ")
        check("workspace: reopening puts every window back", not m, "; ".join(m))
        check("workspace: the same windows were reused, none reopened",
              sorted(w["n"] for w in app.windows()) == numbers)
        check("workspace: marked open now", cfg().get("activeWorkspace") == "E2E")

        # ── display ──
        vd = subprocess.Popen([os.path.join(TMP, "vdisplay"), "amux-e2e", "4242", "1920", "1080"], stdout=subprocess.PIPE)
        new = wait(lambda: [d for d in ctl("displays") if d["uuid"] not in {x["uuid"] for x in displays}], 15)
        if not check("display: virtual monitor connected", new):
            return
        vuuid = new[0]["uuid"]
        vf = [int(float(v)) for v in new[0]["frame"].replace("{", "").replace("}", "").split(",")]
        time.sleep(2)
        app.send("amux://arrange/next-display")
        time.sleep(1)
        on_v = lambda: [w for w in app.windows() if vf[0] <= w["x"] < vf[0] + vf[2] and vf[1] <= w["y"] < vf[1] + vf[3]]
        moved_win = wait(on_v, 10)
        check("display: Move to Next Display", moved_win, f"{len(moved_win or [])} on the new display")
        wait(lambda: any(w.get("display") == vuuid for w in cfg()["windows"]), 10)
        check("display: saved on that display", any(w.get("display") == vuuid for w in cfg()["windows"]))
        vd.terminate(); vd.wait()
        wait(lambda: len(ctl("displays")) == len(displays), 15)
        time.sleep(4)
        check("display: unplugged, its window is on the main display", not on_v() and len(app.windows()) == 15)
        check("display: unplugged, the window still belongs to it",
              any(w.get("display") == vuuid for w in cfg()["windows"]))
        vd = subprocess.Popen([os.path.join(TMP, "vdisplay"), "amux-e2e", "4242", "1920", "1080"], stdout=subprocess.PIPE)
        wait(lambda: len(ctl("displays")) > len(displays), 15)
        back = wait(on_v, 12)
        check("display: plugged back in, the window went home", back)
        # Resolution change on the same monitor: the window keeps its share of the screen.
        vd.terminate(); vd.wait()
        wait(lambda: len(ctl("displays")) == len(displays), 15)
        time.sleep(3)
        vd = subprocess.Popen([os.path.join(TMP, "vdisplay"), "amux-e2e", "4242", "1280", "720"], stdout=subprocess.PIPE)
        nd = wait(lambda: [d for d in ctl("displays") if d["uuid"] == vuuid], 15)
        if nd:
            vf = [int(float(v)) for v in nd[0]["frame"].replace("{", "").replace("}", "").split(",")]
        small = wait(lambda: [w for w in on_v() if w["w"] <= vf[2]], 12)
        check("display: resolution change scales the window into it", nd and small,
              f"display {vf[2]}x{vf[3]}, window {[(w['w'], w['h']) for w in on_v()]}")
        # Quit while the monitor is unplugged, relaunch, plug it back in.
        vd.terminate(); vd.wait()
        wait(lambda: len(ctl("displays")) == len(displays), 15)
        time.sleep(3)
        app.stop(signal.SIGTERM)
        app.start()
        check("display: relaunched without the monitor, all windows open",
              len(app.windows()) == 15 and any(w.get("display") == vuuid for w in cfg()["windows"]))
        vd = subprocess.Popen([os.path.join(TMP, "vdisplay"), "amux-e2e", "4242", "1280", "720"], stdout=subprocess.PIPE)
        wait(lambda: len(ctl("displays")) > len(displays), 15)
        check("display: plugged in after relaunch, the window went home", wait(on_v, 12))
        vd.terminate(); vd.wait()
        wait(lambda: len(ctl("displays")) == len(displays), 15)
    finally:
        app.stop(signal.SIGTERM)
        if vd and vd.poll() is None:
            vd.terminate()
        left = wait(lambda: len(ctl("displays")) == len(displays), 15)
        check("cleanup: no virtual display left connected", left, f"{len(ctl('displays'))} displays")
        awake.terminate()
    bad = [r for r in results if not r[1]]
    print(f"\n{len(results) - len(bad)} passed, {len(bad)} failed (log {LOG})")
    sys.exit(1 if bad else 0)


if __name__ == "__main__":
    main()
