#!/usr/bin/env python3
"""Settings > Updates checks GitHub's desktop-latest release and installs it
(Ethan, 2026-10-22: "add an update button in the settings of mac os").

    desktop/test/update-e2e.py [path/to/Amux.app]

Runs a copy of the app from a temp folder (its own config, AMUX_DESKTOP_TEST=1)
against a local stand-in for GitHub: the release tag, the commit compare and
the zip. The buttons are clicked in the Settings page itself
(amux://test/settings-click runs the button's click()), so the page, the
message to the app, the check, the download, the bundle checks, the swap and
the relaunch through `open` are all the shipped code. Only GitHub is a fake.
"""
import http.server, json, os, plistlib, shutil, subprocess, sys, tempfile, threading, time

HERE = os.path.dirname(os.path.abspath(__file__))
SRC = sys.argv[1] if len(sys.argv) > 1 else os.path.join(HERE, "..", "..", "Amux.app")
# realpath: ps reports /private/var/..., where mkdtemp says /var/...
TMP = os.path.realpath(tempfile.mkdtemp(prefix="amux-update-e2e-"))
CFG, LOG, STATE = (os.path.join(TMP, n) for n in ("config.json", "app.log", "state.json"))
INST = os.path.join(TMP, "Applications", "Amux.app")
BIN = os.path.join(INST, "Contents", "MacOS", "amux")
OLD, NEW = "1111111111111111111111111111111111111111", "2222222222222222222222222222222222222222"
fails = 0


def sh(*a):
    return subprocess.run(a, capture_output=True, text=True)


def check(label, ok, detail=""):
    global fails
    print(("  ok   " if ok else "  FAIL ") + label + ("" if ok else f": {detail}"))
    fails += 0 if ok else 1


def stamped(dst, commit, bundle_id=None):
    """A copy of the built app that says it came from `commit`."""
    shutil.rmtree(dst, ignore_errors=True)
    shutil.copytree(SRC, dst, symlinks=True)
    p = os.path.join(dst, "Contents", "Info.plist")
    info = plistlib.load(open(p, "rb"))
    info["AmuxBuildCommit"] = commit
    if bundle_id:
        info["CFBundleIdentifier"] = bundle_id
    plistlib.dump(info, open(p, "wb"))
    assert sh("codesign", "--force", "--deep", "--sign", "-", dst).returncode == 0


def zipped(commit, bundle_id=None):
    d = tempfile.mkdtemp(dir=TMP)
    stamped(os.path.join(d, "Amux.app"), commit, bundle_id)
    z = os.path.join(d, "Amux-macOS.zip")
    assert sh("ditto", "-c", "-k", "--keepParent", os.path.join(d, "Amux.app"), z).returncode == 0
    return z


# ── the stand-in for GitHub ──
fake = {"tag": NEW, "compare": "behind", "zip": None}
seen = []


class GitHub(http.server.BaseHTTPRequestHandler):
    def log_message(self, *a):
        pass

    def do_GET(self):
        seen.append(self.path)
        if self.path == "/repo/git/ref/tags/desktop-latest":
            body, kind = json.dumps({"object": {"sha": fake["tag"]}}).encode(), "application/json"
        elif self.path.startswith("/repo/compare/"):
            body, kind = json.dumps({"status": fake["compare"], "behind_by": 3}).encode(), "application/json"
        elif self.path == "/Amux-macOS.zip" and fake["zip"]:
            body, kind = open(fake["zip"], "rb").read(), "application/zip"
        else:
            self.send_response(404)
            self.end_headers()
            return
        self.send_response(200)
        self.send_header("Content-Type", kind)
        self.send_header("Content-Length", str(len(body)))
        self.end_headers()
        self.wfile.write(body)


srv = http.server.ThreadingHTTPServer(("127.0.0.1", 0), GitHub)
threading.Thread(target=srv.serve_forever, daemon=True).start()
BASE = f"http://127.0.0.1:{srv.server_address[1]}"
assert sh("xcrun", "swiftc", "-O", "-o", os.path.join(TMP, "wsctl"), os.path.join(HERE, "wsctl.swift")).returncode == 0
url = sh("amux", "url").stdout.strip() or "https://localhost:8824"


def count(s):
    try:
        return open(LOG, errors="replace").read().count(s)
    except FileNotFoundError:
        return 0


def pids():
    """Processes running this test's copy of the app (by its full path)."""
    out = sh("ps", "-axo", "pid=,command=").stdout.splitlines()
    return [int(l.split(None, 1)[0]) for l in out if l.split(None, 1)[1:] and l.split(None, 1)[1].startswith(BIN)]


class App:
    def __init__(self, commit):
        stamped(INST, commit)
        with open(CFG, "w") as f:
            json.dump({"connections": [{"name": "localhost", "url": url}], "lastUrl": url, "cleanExit": True,
                       "windows": [{"url": url + "/#view=board", "frame": "{{200, 200}, {700, 500}}"}]}, f)
        n = count("verdict=windows_restored")
        env = dict(os.environ, AMUX_DESKTOP_CONFIG=CFG, AMUX_DESKTOP_TEST="1", AMUX_DESKTOP_TEST_STATE=STATE,
                   AMUX_DESKTOP_UPDATE_API=BASE + "/repo", AMUX_DESKTOP_UPDATE_ZIP=BASE + "/Amux-macOS.zip")
        self.p = subprocess.Popen([BIN], env=env, stdout=open(LOG, "a"), stderr=subprocess.STDOUT)
        self.pid = self.p.pid
        self.wait(lambda: count("verdict=windows_restored") > n, 20)

    @staticmethod
    def wait(cond, t):
        end = time.time() + t
        while not cond() and time.time() < end:
            time.sleep(0.3)
        return cond()

    def send(self, u, settle=0.5):
        sh(os.path.join(TMP, "wsctl"), "send", str(self.pid), "amux://" + u)
        time.sleep(settle)

    def state(self):
        before = os.path.getmtime(STATE) if os.path.exists(STATE) else 0
        self.send("test/state", 0)
        self.wait(lambda: os.path.exists(STATE) and os.path.getmtime(STATE) != before, 5)
        return json.load(open(STATE))["update"]

    def click(self, id, settle=1):
        self.send("test/settings-click?id=" + id, settle)

    def until(self, want, t=20):
        self.wait(lambda: self.state()["state"] in want, t)
        return self.state()

    def stop(self):
        for pid in pids():
            sh(os.path.join(TMP, "wsctl"), "send", str(pid), "amux://quit")
        self.wait(lambda: not pids(), 10)
        if self.p.poll() is None:
            self.p.terminate()


def plist_commit():
    return plistlib.load(open(os.path.join(INST, "Contents", "Info.plist"), "rb")).get("AmuxBuildCommit")


try:
    print("1. a newer release: Check shows it, Update and Relaunch installs it and the windows come back")
    fake.update(tag=NEW, compare="behind", zip=zipped(NEW))
    app = App(OLD)
    try:
        app.send("settings")
        app.until({"available"})
        seen.clear()
        app.click("upd-check")
        st = app.until({"available"})
        check("check finds the newer build", st["state"] == "available" and st["latest"] == NEW, str(st))
        check("and says how far ahead it is", "3 commits ahead" in st["detail"], st["detail"])
        check("it asked GitHub where the release tag points", "/repo/git/ref/tags/desktop-latest" in seen, str(seen))
        # Opening Settings checks by itself, so prove the button was what was pressed.
        check("the Check for Updates button was clicked", count("test_settings_click id=upd-check result=clicked") == 1)
        restored = count("verdict=windows_restored")
        app.click("upd-install", 0)
        gone = app.wait(lambda: app.p.poll() is not None, 30)
        check("the Update and Relaunch button was clicked", count("test_settings_click id=upd-install result=clicked") == 1)
        check("the old app quits", gone, f"pid {app.pid} still running")
        check("the update is logged", count("verdict=desktop_update_installed from=111111111 to=222222222") == 1,
              open(LOG, errors="replace").read()[-1500:])
        check("the app on disk is the new build", plist_commit() == NEW, str(plist_commit()))
        back = app.wait(lambda: bool(pids()) and count("verdict=windows_restored") > restored, 30)
        check("the new build starts from the same place and reopens the windows", back, f"pids={pids()}")
        if back:
            app.pid = pids()[0]
            st = app.state()
            check("and it reports the new commit", st["commit"] == NEW, str(st))
            app.click("upd-check")
            st = app.until({"upToDate"})
            check("checking again says it is up to date", st["state"] == "upToDate", str(st))
    finally:
        app.stop()
        check("nothing of this test is left running", not pids(), str(pids()))

    print("2. this app is the release: up to date, nothing downloaded")
    seen.clear()
    fake.update(tag=OLD)
    app = App(OLD)
    try:
        app.click("upd-check")
        st = app.until({"upToDate", "available", "failed"})
        check("up to date", st["state"] == "upToDate", str(st))
        check("no download", "/Amux-macOS.zip" not in seen, str(seen))
    finally:
        app.stop()

    print("3. a local build newer than the release is not offered an older one")
    fake.update(tag=NEW, compare="ahead")
    app = App(OLD)
    try:
        app.click("upd-check")
        st = app.until({"upToDate", "available", "failed"})
        check("newer than the release counts as up to date", st["state"] == "upToDate" and "newer" in st["detail"], str(st))
    finally:
        app.stop()

    print("4. a download that is not this app is refused and nothing changes")
    fake.update(tag=NEW, compare="behind", zip=zipped(NEW, bundle_id="com.example.other"))
    app = App(OLD)
    try:
        app.click("upd-check")
        app.until({"available"})
        app.click("upd-install", 0)
        st = app.until({"failed"}, 30)
        check("it says the update failed and why", st["state"] == "failed" and "not this app" in st["detail"], str(st))
        check("the app on disk is unchanged", plist_commit() == OLD, str(plist_commit()))
        check("and it keeps running", app.p.poll() is None)
        check("the failure is logged", count("verdict=desktop_update_failed why=the download is not this app") >= 1)
    finally:
        app.stop()
finally:
    srv.shutdown()

print(f"\n{'PASS' if not fails else 'FAIL'}: update-e2e ({fails} failed) log={LOG}")
sys.exit(1 if fails else 0)
