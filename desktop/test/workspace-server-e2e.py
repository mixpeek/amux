#!/usr/bin/env python3
"""A saved workspace keeps everything and lives on the server (owner,
2026-10-28: "saved workspaces should understand everything (tabs, subtabs,
pane locations, etc.) i should be able to quickly re-create all of this. also
i want it saved on the server").

    desktop/test/workspace-server-e2e.py [path/to/Amux.app]

Runs a second copy of the app with its own config and its own server key
(AMUX_DESKTOP_WORKSPACES_KEY), so the owner's saved workspaces are untouched.

1. Opens two window tabs: one split into two panes, the right pane holding two
   tabs with the second shown, and saves them as a workspace.
2. The server's prefs hold that workspace.
3. Closes a pane tab, switches window tab and tiles the windows; opening the
   workspace puts back the saved tabs, panes, pane tabs and views. (Before
   this, every change was copied into the open workspace, so Open returned to
   the scrambled state.)
4. Two saves back to back both reach the server.
5. A second copy of the app with an empty config gets the workspaces from the
   server and opens the same windows.
6. Changes made offline (pending at launch) merge into the server's list, and
   a Mac's first sync keeps the workspaces only it had.
7. The open windows reach the server with their tab names, and a Mac with no
   windows of its own opens them.
"""
import json, os, signal, subprocess, sys, tempfile, time

HERE = os.path.dirname(os.path.abspath(__file__))
APP = sys.argv[1] if len(sys.argv) > 1 else os.path.join(HERE, "..", "..", "Amux.app")
BIN = os.path.join(APP, "Contents", "MacOS", "amux")
TMP = tempfile.mkdtemp(prefix="amux-wsserver-e2e-")
KEY = "desktop_workspaces_e2e_%d" % os.getpid()
fails = 0


def sh(*a):
    return subprocess.run(a, capture_output=True, text=True)


def check(label, ok, detail=""):
    global fails
    print(("  ok   " if ok else "  FAIL ") + label + ("" if ok else f": {detail}"))
    fails += 0 if ok else 1


def server_value(key=None):
    out = sh("amux", "get", f"/api/prefs?key={key or KEY}").stdout
    try:
        v = json.loads(out).get("value")
        return json.loads(v) if v else None
    except ValueError:
        return None


assert sh("xcrun", "swiftc", "-O", "-o", os.path.join(TMP, "wsctl"), os.path.join(HERE, "wsctl.swift")).returncode == 0
url = sh("amux", "url").stdout.strip() or "https://localhost:8824"


class App:
    def __init__(self, name, config):
        self.dir = os.path.join(TMP, name)
        os.makedirs(self.dir)
        self.cfg, self.log, self.st = (os.path.join(self.dir, n) for n in ("config.json", "app.log", "state.json"))
        json.dump(config, open(self.cfg, "w"))
        self.p = subprocess.Popen([BIN], env=dict(os.environ, AMUX_DESKTOP_CONFIG=self.cfg, AMUX_DESKTOP_TEST="1",
                                  AMUX_DESKTOP_TEST_STATE=self.st, AMUX_DESKTOP_WORKSPACES_KEY=KEY),
                                  stdout=open(self.log, "a"), stderr=subprocess.STDOUT)

    def text(self):
        return open(self.log, errors="replace").read()

    def wait_log(self, s, secs):
        end = time.time() + secs
        while time.time() < end and s not in self.text():
            time.sleep(0.3)
        return s in self.text()

    def send(self, u, settle=0.5):
        sh(os.path.join(TMP, "wsctl"), "send", str(self.p.pid), u)
        time.sleep(settle)

    def state(self):
        before = os.path.getmtime(self.st) if os.path.exists(self.st) else 0
        self.send("amux://test/state", 0)
        end = time.time() + 5
        while time.time() < end and (not os.path.exists(self.st) or os.path.getmtime(self.st) == before):
            time.sleep(0.05)
        return json.load(open(self.st))

    def shape(self):
        """Per window tab, in tab order: its panes, each pane's tabs and shown tab, and views."""
        out = []
        for w in self.state()["windows"]:
            out.append({"selected": w["selected"], "grid": w.get("grid"), "pane_tabs": w.get("pane_tabs"),
                        "pane_active": w.get("pane_active"), "views": w.get("pane_views") or [w["view"]]})
        return sorted(out, key=lambda d: json.dumps(d["views"]))

    def settle(self, n=3, secs=40):
        end = time.time() + secs
        while time.time() < end:
            try:
                ws = self.state()["windows"]
                if len(ws) >= n and all(w["title"] not in ("", "amux") for w in ws):
                    return True
            except (OSError, ValueError, KeyError):
                pass
            time.sleep(0.5)
        return False

    def stop(self):
        self.p.send_signal(signal.SIGTERM)
        try:
            self.p.wait(10)
        except subprocess.TimeoutExpired:
            self.p.kill()


frame = "{{80, 80}, {1100, 760}}"
grid = {"tree": "r0.500(0,1)", "panes": [{"url": url + "/#view=calendar", "title": "Calendar",
                                         "tabs": [{"url": url + "/#view=pages", "title": "Pages"}], "active": 1}]}
base = {"connections": [{"name": "localhost", "url": url}], "lastUrl": url, "cleanExit": True, "launch": "last"}
apps = []
try:
    a = App("one", dict(base, windows=[
        {"url": url + "/#view=board", "frame": frame, "tabGroup": 1, "selected": True, "grid": grid},
        {"url": url + "/#view=messages", "frame": frame, "tabGroup": 1, "selected": False},
    ]))
    apps.append(a)
    check("the app opened its windows", a.wait_log("verdict=windows_restored", 25) and a.settle(2), a.text()[-800:])
    a.send("amux://save-workspace/E2E", 3)
    # Open it, so it is the open workspace: the old build copied every later
    # change into the open one.
    a.send("amux://workspace/E2E", 6)
    saved = a.shape()
    srv = server_value()
    names = [l["name"] for l in (srv or {}).get("layouts", [])]
    check("the workspace is on the server", names == ["E2E"], f"server={srv and names}")
    w = next((x for x in (srv or {}).get("layouts", [{}])[0].get("windows", []) if x.get("grid")), {})
    check("the server copy has the panes and the pane tabs",
          (w.get("grid") or {}).get("tree") == "r0.500(0,1)" and [t["url"].rsplit("=", 1)[-1] for t in (w["grid"]["panes"][0].get("tabs") or [])] == ["pages"]
          and w["grid"]["panes"][0].get("active") == 1, json.dumps(w)[:600])
    check("the sync is logged", "verdict=workspaces_synced key=" + KEY in a.text(), a.text()[-600:])

    # Scramble: close the shown pane tab, show the other window tab, tile.
    a.send("amux://test/pane-tab-close?i=1&t=1", 1)
    a.send("amux://test/select-view?view=view%3Dmessages", 1)
    a.send("amux://arrange/tile", 3)
    scrambled = a.shape()
    check("the scramble changed the windows", scrambled != saved, json.dumps(scrambled))
    time.sleep(3)   # past the autosave, which used to overwrite the open workspace
    a.send("amux://workspace/E2E", 8)
    reopened = a.shape()
    check("Open puts back the saved tabs, panes, pane tabs and views", reopened == saved,
          f"saved={json.dumps(saved)} reopened={json.dumps(reopened)}")
    check("the saved workspace on the server did not follow the scramble", server_value() == srv)

    # Two saves back to back: the second waits for the first, so the server
    # keeps both (two overlapping syncs read the same list and one save was lost).
    a.send("amux://save-workspace/P1", 0)
    a.send("amux://save-workspace/P2", 6)
    names = sorted(l["name"] for l in (server_value() or {}).get("layouts", []))
    check("two quick saves both reach the server", names == ["E2E", "P1", "P2"], str(names))
    # The open windows go to the server too, tab names included.
    a.send("amux://test/rename-tab?view=view%3Dboard&whole=1&name=Planning", 1)
    a.send("amux://arrange/fill", 1)
    end = time.time() + 30
    win = None
    while time.time() < end:
        win = server_value(KEY + "_windows")
        if win and any((w.get("grid") or {}).get("name") == "Planning" for w in win.get("windows", [])):
            break
        time.sleep(1)
    check("the open windows are on the server, with the tab's name",
          bool(win) and any((w.get("grid") or {}).get("name") == "Planning" for w in win.get("windows", [])), json.dumps(win)[:400])
    a.stop()
    srv = server_value()

    # Another Mac: no workspaces of its own; it gets them from the server.
    b = App("two", dict(base, launch="new"))
    apps.append(b)
    got = b.wait_log("verdict=workspaces_synced key=" + KEY, 40)
    layouts = json.load(open(b.cfg)).get("layouts") or []
    check("a Mac with no workspaces gets them from the server", got and [l["name"] for l in layouts] == ["E2E", "P1", "P2"], b.text()[-800:])
    b.send("amux://workspace/E2E", 3)
    b.settle(2)
    time.sleep(4)
    check("and opens the same windows", b.shape() == saved, f"saved={json.dumps(saved)} got={json.dumps(b.shape())}")
    b.stop()

    # A Mac that saved "Y" and deleted "P1" while offline, then quit: the changes
    # are still pending at its next launch, and replaying them keeps the other
    # Mac's "P2" and "E2E" (the pending list used to replace the server's).
    e2e = next(l for l in srv["layouts"] if l["name"] == "E2E")
    y = dict(e2e, name="Y")
    c = App("three", dict(base, launch="new", layouts=[e2e, y], workspacesSyncedOnce=True,
                          workspacesPending=[{"kind": "save", "name": "Y", "layout": y}, {"kind": "delete", "name": "P1"}]))
    apps.append(c)
    c.wait_log("verdict=workspaces_synced key=" + KEY, 40)
    names = sorted(l["name"] for l in (server_value() or {}).get("layouts", []))
    check("an offline save and delete merge into the server's list", names == ["E2E", "P2", "Y"], f"{names} {c.text()[-500:]}")
    cc = json.load(open(c.cfg))
    check("and are cleared once the server has them", not cc.get("workspacesPending")
          and sorted(l["name"] for l in cc.get("layouts") or []) == ["E2E", "P2", "Y"], json.dumps(cc.get("workspacesPending"))[:300])
    c.stop()

    # A Mac that never synced keeps its own workspace alongside the server's.
    d = App("four", dict(base, launch="new", layouts=[dict(e2e, name="Mine")]))
    apps.append(d)
    d.wait_log("verdict=workspaces_synced key=" + KEY, 40)
    names = sorted(l["name"] for l in (server_value() or {}).get("layouts", []))
    check("a Mac's first sync keeps the workspaces only it had", names == ["E2E", "Mine", "P2", "Y"], str(names))
    d.stop()

    # A Mac with no windows of its own opens the server's, the named tab included.
    e = App("five", dict(base, launch="last"))
    apps.append(e)
    e.wait_log("verdict=windows_adopted_from_server", 40)
    time.sleep(4)
    titles = sorted(w["title"] for w in e.state()["windows"])
    check("a Mac with no windows opens the server's, with the tab's name", "Planning" in titles, f"{titles} {e.text()[-400:]}")
finally:
    for x in apps:
        if x.p.poll() is None:
            x.stop()
    for k in (KEY, KEY + "_windows"):
        sh("curl", "-sk", "-m", "20", "-X", "POST", "-H", "Content-Type: application/json",
           "-H", "X-Amux-Session: " + os.environ.get("AMUX_SESSION", ""), "-d", json.dumps({"key": k, "value": ""}), url + "/api/prefs")
print("PASS: workspace-server, all checks passed" if fails == 0 else f"workspace-server: {fails} check(s) FAILED (dir {TMP})")
sys.exit(1 if fails else 0)
