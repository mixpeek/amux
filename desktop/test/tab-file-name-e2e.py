#!/usr/bin/env python3
"""A tab showing a file is named by the file (owner, 2026-10-29: "files should
have a name based on the file thats open"); before, it kept the view's name
underneath ("Files"). A folder open in Files is named by the folder.

    desktop/test/tab-file-name-e2e.py [path/to/Amux.app]
"""
import json, os, signal, subprocess, sys, tempfile, time, urllib.parse

HERE = os.path.dirname(os.path.abspath(__file__))
APP = sys.argv[1] if len(sys.argv) > 1 else os.path.join(HERE, "..", "..", "Amux.app")
BIN = os.path.join(APP, "Contents", "MacOS", "amux")
TMP = tempfile.mkdtemp(prefix="amux-fname-e2e-")
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
folder = os.path.join(TMP, "iconik-sync")
os.makedirs(folder)
doc = os.path.join(folder, "warm-intros.html")
open(doc, "w").write("<h1>Warm intros</h1>")
json.dump({"connections": [{"name": "localhost", "url": url}], "lastUrl": url, "cleanExit": True, "launch": "last",
           "windows": [{"url": url + "/#view=files", "frame": "{{80, 80}, {1000, 700}}"}]}, open(CFG, "w"))
p = subprocess.Popen([BIN], env=dict(os.environ, AMUX_DESKTOP_CONFIG=CFG, AMUX_DESKTOP_TEST="1", AMUX_DESKTOP_TEST_STATE=ST),
                     stdout=open(LOG, "a"), stderr=subprocess.STDOUT)


def send(u, settle=0.5):
    sh(os.path.join(TMP, "wsctl"), "send", str(p.pid), u)
    time.sleep(settle)


def title(want, secs=15):
    end = time.time() + secs
    t = ""
    while time.time() < end:
        before = os.path.getmtime(ST) if os.path.exists(ST) else 0
        send("amux://test/state", 0)
        e2 = time.time() + 5
        while time.time() < e2 and (not os.path.exists(ST) or os.path.getmtime(ST) == before):
            time.sleep(0.05)
        try:
            t = json.load(open(ST))["windows"][0]["title"]
        except (OSError, ValueError, KeyError, IndexError):
            t = ""
        if t == want:
            return t
        time.sleep(0.5)
    return t


def js(code, settle=2.5):
    send("amux://test/js?code=" + urllib.parse.quote(code), settle)


try:
    end = time.time() + 30
    while time.time() < end and "verdict=windows_restored" not in open(LOG, errors="replace").read():
        time.sleep(0.3)
    title("", 25)   # let the first page load and name its tab
    js("openFilePreview(%s)" % json.dumps(doc))
    t = title("warm-intros.html")
    check("a file open in it names the tab after the file", t == "warm-intros.html", t)
    js("closeFilePreview()")
    js("openExplore(%s)" % json.dumps(folder))
    t = title("iconik-sync")
    check("a folder open in Files names the tab after the folder", t == "iconik-sync", t)
finally:
    p.send_signal(signal.SIGTERM)
    try:
        p.wait(10)
    except subprocess.TimeoutExpired:
        p.kill()
print("PASS: tab-file-name, all checks passed" if fails == 0 else f"tab-file-name: {fails} check(s) FAILED (log {LOG})")
sys.exit(1 if fails else 0)
